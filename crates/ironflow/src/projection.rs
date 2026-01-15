//! Projection infrastructure.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{debug, info};

use crate::error::Result;
use crate::store::{EventStore, ProjectionStore, StoredEvent};
use crate::workflow::WorkflowId;

/// Type alias for boxed futures (object-safe async).
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Projection event delivered to projection handlers.
#[derive(Debug, Clone)]
pub struct ProjectionEvent {
    pub global_sequence: i64,
    pub workflow_type: String,
    pub workflow_id: WorkflowId,
    pub sequence: i64,
    pub payload: Value,
    pub created_at: time::OffsetDateTime,
}

impl From<StoredEvent> for ProjectionEvent {
    fn from(event: StoredEvent) -> Self {
        Self {
            global_sequence: event.global_sequence,
            workflow_type: event.workflow_type,
            workflow_id: event.workflow_id,
            sequence: event.sequence,
            payload: event.payload,
            created_at: event.created_at,
        }
    }
}

/// Projection handler trait.
pub trait Projection: Send + Sync + 'static {
    /// Projection identifier used for checkpointing.
    fn name(&self) -> &'static str;

    /// Apply an event to the projection.
    fn handle<'a>(&'a self, event: ProjectionEvent) -> BoxFuture<'a, Result<()>>;
}

/// Configuration for projection workers.
#[derive(Debug, Clone)]
pub struct ProjectionConfig {
    /// How often to poll for new events.
    pub poll_interval: Duration,
    /// Maximum number of events to fetch per batch.
    pub batch_size: u32,
    /// Base delay for retry backoff after projection failures.
    pub error_backoff_base: Duration,
    /// Maximum delay for retry backoff after projection failures.
    pub error_backoff_max: Duration,
}

impl Default for ProjectionConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(200),
            batch_size: 100,
            error_backoff_base: Duration::from_millis(200),
            error_backoff_max: Duration::from_secs(5),
        }
    }
}

impl ProjectionConfig {
    fn error_backoff_duration(&self, failures: u32) -> Duration {
        let multiplier = 2u32.saturating_pow(failures.saturating_sub(1));
        let delay = self.error_backoff_base.saturating_mul(multiplier);
        delay.min(self.error_backoff_max)
    }
}

/// Worker that applies projections using global event ordering.
pub struct ProjectionWorker<S, P>
where
    S: EventStore + ProjectionStore,
    P: Projection,
{
    store: S,
    projection: Arc<P>,
    config: ProjectionConfig,
    worker_id: String,
}

impl<S, P> ProjectionWorker<S, P>
where
    S: EventStore + ProjectionStore,
    P: Projection,
{
    pub fn new(store: S, projection: Arc<P>, config: ProjectionConfig, worker_id: String) -> Self {
        Self {
            store,
            projection,
            config,
            worker_id,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut poll_interval = interval(self.config.poll_interval);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut failures: u32 = 0;

        info!(
            worker_id = %self.worker_id,
            projection = self.projection.name(),
            "Projection worker started"
        );

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    match self.process_batch().await {
                        Ok(()) => {
                            failures = 0;
                        }
                        Err(err) => {
                            failures = failures.saturating_add(1);
                            let backoff = self.config.error_backoff_duration(failures);
                            info!(
                                worker_id = %self.worker_id,
                                projection = self.projection.name(),
                                failures,
                                backoff_ms = backoff.as_millis(),
                                "Projection error, backing off"
                            );
                            tracing::error!(error = %err, "Projection batch failed");
                            tokio::select! {
                                _ = tokio::time::sleep(backoff) => {}
                                _ = shutdown.changed() => {
                                    if *shutdown.borrow() {
                                        info!(
                                            worker_id = %self.worker_id,
                                            projection = self.projection.name(),
                                            "Projection worker shutting down"
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(
                            worker_id = %self.worker_id,
                            projection = self.projection.name(),
                            "Projection worker shutting down"
                        );
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    async fn process_batch(&self) -> Result<()> {
        let position = self
            .store
            .load_projection_position(self.projection.name())
            .await?;

        let events = self
            .store
            .fetch_events_since(position, self.config.batch_size)
            .await?;

        if events.is_empty() {
            return Ok(());
        }

        for event in events {
            let projection_event = ProjectionEvent::from(event);
            let next_position = projection_event.global_sequence;
            self.projection.handle(projection_event).await?;
            self.store
                .store_projection_position(self.projection.name(), next_position)
                .await?;
            debug!(
                projection = self.projection.name(),
                global_sequence = next_position,
                "Projection advanced"
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct MockStore {
        state: Arc<Mutex<MockState>>,
    }

    struct MockState {
        events: Vec<StoredEvent>,
        position: i64,
    }

    impl MockStore {
        fn new(events: Vec<StoredEvent>) -> Self {
            Self {
                state: Arc::new(Mutex::new(MockState {
                    events,
                    position: 0,
                })),
            }
        }
    }

    impl EventStore for MockStore {
        async fn fetch_events_since(&self, after: i64, limit: u32) -> Result<Vec<StoredEvent>> {
            let state = self.state.lock().unwrap();
            let mut events: Vec<_> = state
                .events
                .iter()
                .filter(|event| event.global_sequence > after)
                .cloned()
                .collect();
            events.truncate(limit as usize);
            Ok(events)
        }
    }

    impl ProjectionStore for MockStore {
        async fn load_projection_position(&self, _projection_name: &str) -> Result<i64> {
            let state = self.state.lock().unwrap();
            Ok(state.position)
        }

        async fn store_projection_position(
            &self,
            _projection_name: &str,
            global_sequence: i64,
        ) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.position = global_sequence;
            Ok(())
        }
    }

    struct FlakyProjection {
        calls: Arc<AtomicUsize>,
    }

    impl FlakyProjection {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Projection for FlakyProjection {
        fn name(&self) -> &'static str {
            "flaky"
        }

        fn handle<'a>(&'a self, _event: ProjectionEvent) -> BoxFuture<'a, Result<()>> {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                let attempt = calls.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    return Err(crate::Error::UnknownWorkflowType(
                        "projection-failure".to_string(),
                    ));
                }
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn projection_worker_retries_with_backoff() {
        let event = StoredEvent {
            global_sequence: 1,
            workflow_type: "test".to_string(),
            workflow_id: WorkflowId::new("wf-1"),
            sequence: 1,
            payload: serde_json::json!({"type": "Test"}),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };

        let store = MockStore::new(vec![event]);
        let projection = Arc::new(FlakyProjection::new());
        let config = ProjectionConfig {
            poll_interval: Duration::from_millis(10),
            batch_size: 10,
            error_backoff_base: Duration::from_millis(200),
            error_backoff_max: Duration::from_millis(200),
        };

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker =
            ProjectionWorker::new(store, projection.clone(), config, "worker-1".to_string());
        let handle = tokio::spawn(worker.run(shutdown_rx));

        async fn wait_for_calls(
            projection: &FlakyProjection,
            target: usize,
            timeout: Duration,
        ) -> tokio::time::Instant {
            let start = tokio::time::Instant::now();
            loop {
                if projection.calls() >= target {
                    return tokio::time::Instant::now();
                }
                if start.elapsed() > timeout {
                    panic!("timed out waiting for {} calls", target);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        let first_at = wait_for_calls(&projection, 1, Duration::from_millis(200)).await;
        let second_at = wait_for_calls(&projection, 2, Duration::from_secs(1)).await;
        assert!(second_at.duration_since(first_at) >= Duration::from_millis(150));

        let _ = shutdown_tx.send(true);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = handle.await;
    }
}
