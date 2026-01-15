//! Workflow registry and runtime builder.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::sync::watch;
use tracing::info;
use uuid::Uuid;

use super::config::RuntimeConfig;
use super::effect_worker::EffectWorker;
use super::timer_worker::TimerWorker;
use crate::Workflow;
use crate::effect::{EffectContext, EffectHandler};
use crate::engine::WorkflowEngine;
use crate::error::Error;
use crate::service::{WorkflowService, WorkflowServiceConfig};
use crate::store::{DeadLetter, DeadLetterQuery, OutboxStore, Store, WorkflowQueryStore};

/// Type-erased workflow entry for dynamic dispatch.
///
/// This trait allows the registry to store different workflow types
/// in a single HashMap while preserving type-safe execution.
#[async_trait]
pub(crate) trait WorkflowEntry: Send + Sync {
    /// Execute a decision for this workflow type.
    ///
    /// Deserializes the input JSON and routes to the typed workflow.
    async fn execute(&self, input_json: Value) -> crate::Result<()>;

    /// Handle an effect for this workflow type.
    ///
    /// Deserializes the effect JSON and routes to the typed EffectHandler.
    /// Returns the result input as JSON if the handler returned `Some(input)`.
    /// Errors are returned as strings for dead letter queue storage.
    async fn handle_effect(
        &self,
        effect_json: Value,
        ctx: &EffectContext,
    ) -> Result<Option<Value>, String>;

    /// Rebuild the latest state for this workflow from stored events.
    async fn replay_latest_state(&self, workflow_id: &crate::workflow::WorkflowId)
        -> crate::Result<Value>;
}

/// Typed workflow entry that captures concrete types at registration.
///
/// Wraps a store and `EffectHandler` for a specific workflow type.
struct TypedWorkflowEntry<W, H, S>
where
    W: Workflow,
    H: EffectHandler<Workflow = W>,
    S: Store,
{
    store: S,
    record_input_observations: bool,
    handler: H,
    _marker: PhantomData<W>,
}

#[async_trait]
impl<W, H, S> WorkflowEntry for TypedWorkflowEntry<W, H, S>
where
    W: Workflow + Send + Sync + 'static,
    W::State: Send + Serialize,
    W::Input: Serialize + DeserializeOwned + Send + Sync,
    W::Effect: DeserializeOwned,
    H: EffectHandler<Workflow = W>,
    S: Store + WorkflowQueryStore,
{
    async fn execute(&self, input_json: Value) -> crate::Result<()> {
        let input: W::Input = serde_json::from_value(input_json)?;
        crate::decider::execute::<W, _>(&self.store, self.record_input_observations, &input).await
    }

    async fn handle_effect(
        &self,
        effect_json: Value,
        ctx: &EffectContext,
    ) -> Result<Option<Value>, String> {
        let effect: W::Effect = serde_json::from_value(effect_json).map_err(|e| e.to_string())?;

        let input = self
            .handler
            .handle(&effect, ctx)
            .await
            .map_err(|e| e.to_string())?;

        // Serialize result input to JSON if present
        match input {
            Some(i) => {
                let json = serde_json::to_value(i).map_err(|e| e.to_string())?;
                Ok(Some(json))
            }
            None => Ok(None),
        }
    }

    async fn replay_latest_state(
        &self,
        workflow_id: &crate::workflow::WorkflowId,
    ) -> crate::Result<Value> {
        let events = self.store.fetch_workflow_events(W::TYPE, workflow_id).await?;

        let mut state = W::State::default();
        for event in events {
            let typed: W::Event = serde_json::from_value(event.payload)?;
            state = W::evolve(state, typed);
        }

        Ok(serde_json::to_value(state)?)
    }
}

/// Registry mapping workflow types to their entries.
pub(crate) struct WorkflowRegistry {
    entries: HashMap<&'static str, Box<dyn WorkflowEntry>>,
}

impl WorkflowRegistry {
    /// Create an empty registry.
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register a workflow with its handler.
    fn register<W, H, S>(&mut self, store: S, record_input_observations: bool, handler: H)
    where
        W: Workflow + Send + Sync + 'static,
        W::State: Send + Serialize,
        W::Input: Serialize + DeserializeOwned + Send + Sync,
        W::Effect: DeserializeOwned,
        H: EffectHandler<Workflow = W>,
        S: Store + WorkflowQueryStore,
    {
        let entry = TypedWorkflowEntry {
            store,
            record_input_observations,
            handler,
            _marker: PhantomData,
        };
        self.entries.insert(W::TYPE, Box::new(entry));
    }

    /// Look up a workflow entry by type.
    ///
    /// Returns the static workflow type key and the entry if found.
    /// The static key can be used to create `EffectContext` instances.
    pub(crate) fn get(&self, workflow_type: &str) -> Option<(&'static str, &dyn WorkflowEntry)> {
        self.entries
            .get_key_value(workflow_type)
            .map(|(k, v)| (*k, v.as_ref()))
    }

    /// Returns the number of registered workflows.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Builder for constructing a [`WorkflowRuntime`].
///
/// Use this to register workflows and configure the runtime before starting.
///
/// # Example
///
/// ```ignore
/// let runtime = WorkflowRuntime::builder(store, WorkflowServiceConfig::default())
///     .register::<OrderWorkflow>(order_handler)
///     .register::<InventoryWorkflow>(inventory_handler)
///     .config(RuntimeConfig {
///         effect_poll_interval: Duration::from_millis(50),
///         ..Default::default()
///     })
///     .build_runtime()?;
/// ```
pub struct WorkflowBuilder<S>
where
    S: Store + WorkflowQueryStore,
{
    store: S,
    registry: WorkflowRegistry,
    duplicate_workflow_type: Option<String>,
    config: RuntimeConfig,
    service_config: WorkflowServiceConfig,
}

impl<S> WorkflowBuilder<S>
where
    S: Store + WorkflowQueryStore,
{
    /// Create a new builder with the given store and service configuration.
    fn new(store: S, service_config: WorkflowServiceConfig) -> Self {
        Self {
            store,
            registry: WorkflowRegistry::new(),
            duplicate_workflow_type: None,
            config: RuntimeConfig::default(),
            service_config,
        }
    }

    /// Register a workflow type with its effect handler.
    ///
    /// The workflow type is inferred from the handler's associated type.
    /// The workflow's `TYPE` constant is used as the key for routing.
    /// Each workflow type can only be registered once.
    ///
    /// Defers duplicate workflow type checks until build time.
    pub fn register<H>(mut self, handler: H) -> Self
    where
        H: EffectHandler,
        H::Workflow: Workflow + Send + Sync + 'static,
        <H::Workflow as Workflow>::State: Send + Serialize,
        <H::Workflow as Workflow>::Input: Serialize + DeserializeOwned + Send + Sync,
        <H::Workflow as Workflow>::Effect: DeserializeOwned,
    {
        if self.registry.entries.contains_key(H::Workflow::TYPE) {
            if self.duplicate_workflow_type.is_none() {
                self.duplicate_workflow_type = Some(H::Workflow::TYPE.to_string());
            }
            return self;
        }

        self.registry.register::<H::Workflow, _, _>(
            self.store.clone(),
            self.service_config.record_input_observations,
            handler,
        );
        self
    }

    /// Register a workflow that does not emit effects.
    pub fn register_without_effects<W>(self) -> Self
    where
        W: Workflow<Effect = ()> + Send + Sync + 'static,
        W::State: Send + Serialize,
        W::Input: Serialize + DeserializeOwned + Send + Sync,
        W::Effect: DeserializeOwned,
    {
        self.register(crate::effect::handler::NoopHandler::<W>::default())
    }

    /// Set the runtime configuration.
    ///
    /// If not called, uses [`RuntimeConfig::default()`].
    pub fn config(mut self, config: RuntimeConfig) -> Self {
        self.config = config;
        self
    }

    /// Build the workflow engine (service + runtime).
    pub fn build_engine(self) -> crate::Result<WorkflowEngine<S>> {
        let (service, runtime) = self.build_parts()?;
        Ok(WorkflowEngine { service, runtime })
    }

    /// Build the runtime.
    pub fn build_runtime(self) -> crate::Result<WorkflowRuntime<S>> {
        let (_service, runtime) = self.build_parts()?;
        Ok(runtime)
    }

    /// Build the workflow service without starting workers.
    pub fn build_service(self) -> crate::Result<WorkflowService<S>> {
        if let Some(workflow_type) = self.duplicate_workflow_type {
            return Err(Error::DuplicateWorkflowType(workflow_type));
        }
        let registry = Arc::new(self.registry);
        Ok(WorkflowService::new(
            self.store,
            registry,
            self.service_config,
        ))
    }

    fn build_parts(self) -> crate::Result<(Arc<WorkflowService<S>>, WorkflowRuntime<S>)> {
        if let Some(workflow_type) = self.duplicate_workflow_type {
            return Err(Error::DuplicateWorkflowType(workflow_type));
        }
        let worker_id = self
            .config
            .worker_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let registry = Arc::new(self.registry);
        let service = Arc::new(WorkflowService::new(
            self.store.clone(),
            Arc::clone(&registry),
            self.service_config,
        ));

        let runtime = WorkflowRuntime {
            store: self.store,
            service,
            config: self.config,
            worker_id,
        };

        Ok((Arc::clone(&runtime.service), runtime))
    }
}

/// Effect execution runtime.
///
/// The runtime coordinates effect and timer workers to process
/// effects from the outbox. It routes effects to the appropriate
/// workflow handler based on the `workflow_type`.
///
/// # Lifecycle
///
/// 1. Create with [`WorkflowRuntime::builder(store, WorkflowServiceConfig::default())`]
/// 2. Register workflows with [`WorkflowBuilder::register()`]
/// 3. Configure with [`WorkflowBuilder::config()`]
/// 4. Build with [`WorkflowBuilder::build_runtime()`]
/// 5. Run with [`WorkflowRuntime::run()`] (not yet implemented)
///
/// # Example
///
/// ```ignore
/// let runtime = WorkflowRuntime::builder(store, WorkflowServiceConfig::default())
///     .register::<OrderWorkflow>(order_handler)
///     .build_runtime()?;
///
/// // Run until shutdown signal
/// runtime.run(shutdown_signal).await?;
/// ```
#[derive(Clone)]
pub struct WorkflowRuntime<S>
where
    S: Store + WorkflowQueryStore,
{
    store: S,
    service: Arc<WorkflowService<S>>,
    config: RuntimeConfig,
    worker_id: String,
}

impl<S> WorkflowRuntime<S>
where
    S: Store + WorkflowQueryStore,
{
    /// Create a new runtime builder.
    pub fn builder(store: S, service_config: WorkflowServiceConfig) -> WorkflowBuilder<S> {
        WorkflowBuilder::new(store, service_config)
    }

    /// Returns the runtime configuration.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Returns the worker identifier.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Returns the number of registered workflows.
    pub fn workflow_count(&self) -> usize {
        self.service.workflow_count()
    }

    /// Returns the workflow service handle.
    pub(crate) fn service(&self) -> &WorkflowService<S> {
        &self.service
    }
}

impl<S> WorkflowRuntime<S>
where
    S: Store + WorkflowQueryStore + OutboxStore,
{
    /// Run the effect and timer workers until shutdown signal.
    ///
    /// This method starts workers which poll the outbox:
    /// - Effect workers: process immediate effects via handlers
    /// - Timer workers: process due timers by routing embedded inputs
    ///
    /// The number of workers is controlled by `effect_workers` and
    /// `timer_workers` in [`RuntimeConfig`]. Workers coordinate via
    /// `FOR UPDATE SKIP LOCKED` to avoid processing the same effect twice.
    ///
    /// # Shutdown Behavior
    ///
    /// When the shutdown future completes:
    /// 1. All workers stop claiming new work
    /// 2. Wait for current work (if any) to complete
    /// 3. Return cleanly after timeout
    ///
    /// # Example
    ///
    /// ```ignore
    /// use tokio::signal;
    ///
    /// let runtime = WorkflowRuntime::builder(pg_store, WorkflowServiceConfig::default())
    ///     .register(order_handler)
    ///     .config(RuntimeConfig {
    ///         effect_workers: 4,  // 4 parallel effect workers
    ///         ..Default::default()
    ///     })
    ///     .build_runtime()?;
    ///
    /// // Run until Ctrl+C
    /// runtime.run(async { signal::ctrl_c().await.ok(); }).await?;
    /// ```
    pub async fn run<F>(self, shutdown: F) -> crate::Result<()>
    where
        F: Future<Output = ()> + Send,
    {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let effect_worker_count = self.config.effect_workers.max(1);
        let timer_worker_count = self.config.timer_workers.max(1);

        info!(
            worker_id = %self.worker_id,
            workflows = self.workflow_count(),
            effect_workers = effect_worker_count,
            timer_workers = timer_worker_count,
            "Runtime starting"
        );

        let runtime = Arc::new(self);
        let mut worker_handles = Vec::new();

        // Spawn effect workers
        for i in 0..effect_worker_count {
            let worker_id = if effect_worker_count == 1 {
                format!("{}-effect", runtime.worker_id)
            } else {
                format!("{}-effect-{}", runtime.worker_id, i)
            };

            let effect_worker = EffectWorker::new(
                Arc::clone(&runtime),
                runtime.store.clone(),
                runtime.config.clone(),
                worker_id,
            );

            let effect_shutdown_rx = shutdown_rx.clone();
            let handle = tokio::spawn(async move {
                effect_worker.run(effect_shutdown_rx).await;
            });
            worker_handles.push(handle);
        }

        // Spawn timer workers
        for i in 0..timer_worker_count {
            let worker_id = if timer_worker_count == 1 {
                format!("{}-timer", runtime.worker_id)
            } else {
                format!("{}-timer-{}", runtime.worker_id, i)
            };

            let timer_worker = TimerWorker::new(
                Arc::clone(&runtime),
                runtime.store.clone(),
                runtime.config.clone(),
                worker_id,
            );

            let timer_shutdown_rx = shutdown_rx.clone();
            let handle = tokio::spawn(async move {
                timer_worker.run(timer_shutdown_rx).await;
            });
            worker_handles.push(handle);
        }

        // Wait for shutdown signal
        shutdown.await;

        // Signal shutdown to all workers
        let _ = shutdown_tx.send(true);

        // Wait for all workers with timeout
        let shutdown_timeout = runtime.config.shutdown_timeout;
        let all_workers = async {
            for handle in worker_handles {
                let _ = handle.await;
            }
        };

        match tokio::time::timeout(shutdown_timeout, all_workers).await {
            Ok(()) => {
                info!(worker_id = %runtime.worker_id, "Runtime stopped gracefully");
            }
            Err(_) => {
                tracing::warn!(
                    worker_id = %runtime.worker_id,
                    timeout_secs = shutdown_timeout.as_secs(),
                    "Shutdown timeout exceeded, forcing stop"
                );
            }
        }

        Ok(())
    }

    /// Fetch dead-lettered effects.
    ///
    /// Returns effects that have exceeded the configured `max_attempts`
    /// and are no longer being retried.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use ironflow::runtime::outbox::DeadLetterQuery;
    ///
    /// // Fetch all dead letters
    /// let dead_letters = runtime.fetch_dead_letters(DeadLetterQuery::new()).await?;
    ///
    /// // Fetch dead letters for a specific workflow type
    /// let order_dead_letters = runtime
    ///     .fetch_dead_letters(DeadLetterQuery::new().workflow_type("order"))
    ///     .await?;
    /// ```
    pub async fn fetch_dead_letters(
        &self,
        query: DeadLetterQuery,
    ) -> crate::Result<Vec<DeadLetter>> {
        self.store
            .fetch_dead_letters(&query, self.config.retry_policy.max_attempts)
            .await
    }

    /// Count dead-lettered effects.
    ///
    /// Useful for monitoring and alerting on dead letter queue size.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let count = runtime.count_dead_letters(DeadLetterQuery::new()).await?;
    /// println!("Dead letters: {}", count);
    /// ```
    pub async fn count_dead_letters(&self, query: DeadLetterQuery) -> crate::Result<u64> {
        self.store
            .count_dead_letters(&query, self.config.retry_policy.max_attempts)
            .await
    }

    /// Retry a dead-lettered effect.
    ///
    /// Resets the effect's attempt count to 0, making it available for
    /// processing again by the effect worker.
    ///
    /// Returns `Ok(true)` if the effect was found and reset,
    /// `Ok(false)` if the effect was not found or already processed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let dead_letters = runtime.fetch_dead_letters(DeadLetterQuery::new()).await?;
    /// for dl in dead_letters {
    ///     runtime.retry_dead_letter(dl.id).await?;
    /// }
    /// ```
    pub async fn retry_dead_letter(&self, effect_id: Uuid) -> crate::Result<bool> {
        self.store.retry_dead_letter(effect_id).await
    }

    /// Fetch dead-lettered timers.
    pub async fn fetch_timer_dead_letters(
        &self,
        query: DeadLetterQuery,
    ) -> crate::Result<Vec<DeadLetter>> {
        self.store
            .fetch_timer_dead_letters(&query, self.config.retry_policy.max_attempts)
            .await
    }

    /// Count dead-lettered timers.
    pub async fn count_timer_dead_letters(&self, query: DeadLetterQuery) -> crate::Result<u64> {
        self.store
            .count_timer_dead_letters(&query, self.config.retry_policy.max_attempts)
            .await
    }

    /// Retry a dead-lettered timer.
    pub async fn retry_timer_dead_letter(&self, timer_id: Uuid) -> crate::Result<bool> {
        self.store.retry_timer_dead_letter(timer_id).await
    }
}
