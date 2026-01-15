use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ironflow::{
    DeadLetter, DeadLetterQuery, OutboxStore, PgStore, RetryPolicy, RuntimeConfig, WorkflowBuilder,
    WorkflowRuntime, WorkflowService, WorkflowServiceConfig,
};
use sqlx::PgPool;
use tokio::task::JoinHandle;

use super::db;

/// Initialize tracing for tests. Safe to call multiple times.
///
/// Uses a standard filter for ironflow debugging. The `try_init()` call
/// is idempotent - subsequent calls are no-ops if already initialized.
pub fn init_test_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ironflow=debug")
        .try_init();
}

/// Assert that events match the expected types in order.
pub fn assert_event_types(events: &[serde_json::Value], expected_types: &[&str]) {
    assert_eq!(
        events.len(),
        expected_types.len(),
        "event count mismatch: expected {}, got {}",
        expected_types.len(),
        events.len()
    );
    for (i, expected_type) in expected_types.iter().enumerate() {
        assert_eq!(
            events[i]["type"], *expected_type,
            "event {i} type mismatch: expected {expected_type}, got {}",
            events[i]["type"]
        );
    }
}

pub const TEST_MAX_ATTEMPTS: u32 = 3;
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub const DEFAULT_TEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const TEST_LOCK_DURATION: Duration = Duration::from_secs(30);

/// Tracks maximum concurrent executions for parallelism tests.
#[derive(Default)]
pub struct ConcurrencyTracker {
    current: AtomicUsize,
    max_seen: AtomicUsize,
}

impl ConcurrencyTracker {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn enter(&self) {
        let count = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_seen.fetch_max(count, Ordering::SeqCst);
    }

    pub fn exit(&self) {
        self.current.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_seen.load(Ordering::SeqCst)
    }
}

/// Fast runtime config for tests.
pub fn test_runtime_config() -> RuntimeConfig {
    RuntimeConfig {
        effect_poll_interval: Duration::from_millis(50),
        timer_poll_interval: Duration::from_millis(100),
        shutdown_timeout: Duration::from_secs(5),
        retry_policy: RetryPolicy {
            max_attempts: TEST_MAX_ATTEMPTS,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_millis(200),
        },
        ..Default::default()
    }
}

/// Poll until condition returns Some(T) or timeout expires.
pub async fn wait_until<F, Fut, T>(timeout: Duration, interval: Duration, check: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if let Some(result) = check().await? {
            return Ok(result);
        }

        if tokio::time::Instant::now() > deadline {
            return Err(anyhow!("timeout waiting for condition"));
        }

        tokio::time::sleep(interval).await;
    }
}

/// Manages workflow runtime lifecycle for tests. Drop signals shutdown automatically.
pub struct TestApp {
    pub store: PgStore,
    pub service: Arc<WorkflowService<PgStore>>,
    pool: PgPool,
    max_attempts: u32,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<JoinHandle<anyhow::Result<()>>>,
}

pub struct TestAppBuilder<'a> {
    pool: &'a PgPool,
    builder: WorkflowBuilder<PgStore>,
    runtime_config: RuntimeConfig,
}

impl<'a> TestAppBuilder<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        let store = PgStore::new(pool.clone());
        Self {
            pool,
            builder: WorkflowRuntime::builder(store, WorkflowServiceConfig::default()),
            runtime_config: test_runtime_config(),
        }
    }

    pub fn register<H: ironflow::EffectHandler + Send + Sync + 'static>(
        mut self,
        handler: H,
    ) -> Self
    where
        H::Workflow: ironflow::Workflow + Send + Sync + 'static,
        <H::Workflow as ironflow::Workflow>::State: Default + Send + Sync,
        <H::Workflow as ironflow::Workflow>::Input:
            serde::de::DeserializeOwned + ironflow::HasWorkflowId + Send + Sync,
        <H::Workflow as ironflow::Workflow>::Event: serde::Serialize + Send + Sync,
        <H::Workflow as ironflow::Workflow>::Effect:
            serde::Serialize + serde::de::DeserializeOwned + Send + Sync,
        H::Error: std::fmt::Display + Send + Sync,
    {
        self.builder = self.builder.register(handler);
        self
    }

    pub fn config(mut self, config: RuntimeConfig) -> Self {
        self.runtime_config = config;
        self
    }

    /// Build engine and spawn runtime in background.
    pub async fn build_and_run(self) -> Result<TestApp> {
        let max_attempts = self.runtime_config.retry_policy.max_attempts;
        let engine = self
            .builder
            .config(self.runtime_config)
            .build_engine()
            .map_err(anyhow::Error::from)?;

        let store = PgStore::new(self.pool.clone());

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let runtime = engine.runtime;
        let handle = tokio::spawn(async move {
            runtime
                .run(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .map_err(anyhow::Error::from)
        });

        Ok(TestApp {
            store,
            service: engine.service,
            pool: self.pool.clone(),
            max_attempts,
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
        })
    }
}

impl TestApp {
    pub fn builder(pool: &PgPool) -> TestAppBuilder<'_> {
        TestAppBuilder::new(pool)
    }

    #[allow(dead_code)]
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.await??;
        }
        Ok(())
    }

    pub async fn wait_for_events(
        &self,
        workflow_type: &str,
        workflow_id: &str,
        expected: usize,
        timeout: Duration,
    ) -> Result<Vec<serde_json::Value>> {
        let pool = &self.pool;
        wait_until(timeout, DEFAULT_POLL_INTERVAL, || async {
            let events = db::fetch_events(pool, workflow_type, workflow_id).await?;
            if events.len() >= expected {
                Ok(Some(events))
            } else {
                Ok(None)
            }
        })
        .await
        .with_context(|| format!("waiting for {expected} events on {workflow_type}/{workflow_id}"))
    }

    pub async fn count_pending_timers(
        &self,
        workflow_type: &str,
        workflow_id: &str,
        key: Option<&str>,
    ) -> Result<i64> {
        db::count_timers(&self.pool, workflow_type, workflow_id, true, key)
            .await
            .map_err(Into::into)
    }

    pub async fn fetch_dead_letters(&self, query: DeadLetterQuery) -> Result<Vec<DeadLetter>> {
        let dead_letters = self
            .store
            .fetch_dead_letters(&query, self.max_attempts)
            .await?;
        Ok(dead_letters)
    }

    pub async fn wait_for_dead_letter(
        &self,
        query: DeadLetterQuery,
        timeout: Duration,
    ) -> Result<Vec<DeadLetter>> {
        wait_until(timeout, DEFAULT_POLL_INTERVAL, || async {
            let dead_letters = self.fetch_dead_letters(query.clone()).await?;
            if dead_letters.is_empty() {
                Ok(None)
            } else {
                Ok(Some(dead_letters))
            }
        })
        .await
        .context("waiting for dead letter")
    }

    pub async fn wait_for_effect_processed(
        &self,
        workflow_id: &str,
        timeout: Duration,
    ) -> Result<()> {
        let pool = &self.pool;
        wait_until(timeout, DEFAULT_POLL_INTERVAL, || async {
            if db::is_effect_processed(pool, workflow_id).await? {
                Ok(Some(()))
            } else {
                Ok(None)
            }
        })
        .await
        .with_context(|| format!("waiting for effect to be processed: {workflow_id}"))
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        // Signal shutdown if not already done (e.g., on panic or early return).
        // This is synchronous - we just send the signal; the runtime will
        // shut down gracefully in the background.
        //
        // We intentionally don't abort the task - that causes "closed pool" errors
        // when the runtime is mid-operation. The shutdown signal is sufficient;
        // the runtime will stop polling and exit cleanly.
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
