//! Workflow registry and runtime builder.

use std::any::{Any, TypeId};
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
use crate::service::{ExecuteOutcome, WorkflowService, WorkflowServiceConfig};
use crate::store::{DeadLetter, DeadLetterQuery, OutboxStore, Store, WorkflowQueryStore};

/// Best-effort extraction of a panic message from the `Box<dyn Any>` carried
/// by `JoinError::into_panic`. Panics typically carry `&'static str` or
/// `String` payloads; anything else is reported as a placeholder so ops
/// still see the panic happened, even if the payload type is exotic.
fn extract_panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Log the outcome of a worker task. Called by each worker's supervisor
/// immediately when the worker terminates, so panics/cancellations appear
/// in tracing the moment they happen rather than at shutdown time.
fn log_worker_termination(
    runtime_worker_id: &str,
    worker_name: &str,
    result: std::result::Result<(), tokio::task::JoinError>,
) {
    match result {
        Ok(()) => {}
        Err(e) if e.is_panic() => {
            let panic_msg = match e.try_into_panic() {
                Ok(payload) => extract_panic_message(payload),
                Err(_) => "<join error, not a panic>".to_string(),
            };
            tracing::error!(
                worker_id = %runtime_worker_id,
                worker = %worker_name,
                panic = %panic_msg,
                "Worker task panicked — processing for this worker has stopped"
            );
        }
        Err(e) => {
            tracing::warn!(
                worker_id = %runtime_worker_id,
                worker = %worker_name,
                error = ?e,
                "Worker task ended unexpectedly (cancelled)"
            );
        }
    }
}

/// Typed dispatch for a specific workflow `W`.
///
/// Parallel to [`DynamicEntry`] but preserves `W` at the trait level, so
/// `execute` takes `&W::Input` and returns `ExecuteOutcome<W::Rejection>`
/// without going through JSON. The typed registry stores
/// `Arc<dyn TypedEntry<W>>` keyed by `TypeId::of::<W>()`, letting
/// [`WorkflowService::execute`](crate::WorkflowService::execute) bypass the
/// dyn-erased path that requires serde round-trip.
#[async_trait]
pub(crate) trait TypedEntry<W: Workflow>: Send + Sync + 'static {
    /// Execute a typed input and return the typed outcome.
    async fn execute(&self, input: &W::Input) -> crate::Result<ExecuteOutcome<W::Rejection>>;

    /// Rebuild the latest state for this workflow and return it typed.
    async fn replay_latest_state(
        &self,
        workflow_id: &crate::workflow::WorkflowId,
    ) -> crate::Result<W::State>;
}

/// Type-erased workflow entry for dynamic dispatch.
///
/// This trait allows the registry to store different workflow types
/// in a single HashMap while preserving type-safe execution.
#[async_trait]
pub(crate) trait DynamicEntry: Send + Sync {
    /// Execute a decision for this workflow type.
    ///
    /// Deserializes the input JSON, routes to the typed workflow, and
    /// returns an untyped outcome (Accepted / Rejected with JSON payload /
    /// AlreadyCompleted). Rejection payloads are serialized to JSON
    /// because this method is behind a `dyn` boundary that can't mention
    /// `W::Rejection`.
    async fn execute_dynamic(
        &self,
        input_json: Value,
    ) -> crate::Result<crate::ExecuteOutcome<Value>>;

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
    async fn replay_latest_state_dynamic(
        &self,
        workflow_id: &crate::workflow::WorkflowId,
    ) -> crate::Result<Value>;
}

/// Concrete registry entry. Captures `W`, `H`, and `S` at registration
/// and implements both [`TypedEntry<W>`] and [`DynamicEntry`] — the typed
/// view is used by `execute<W>` / `fetch_latest_state<W>`, the dynamic
/// view by `execute_dynamic` / `fetch_latest_state_dynamic`.
struct WorkflowEntry<W, H, S>
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
impl<W, H, S> DynamicEntry for WorkflowEntry<W, H, S>
where
    W: Workflow + Send + Sync + 'static,
    W::State: Send + Serialize,
    W::Input: Serialize + DeserializeOwned + Send + Sync,
    W::Effect: DeserializeOwned,
    H: EffectHandler<Workflow = W>,
    S: Store + WorkflowQueryStore,
{
    async fn execute_dynamic(
        &self,
        input_json: Value,
    ) -> crate::Result<crate::ExecuteOutcome<Value>> {
        let input: W::Input = serde_json::from_value(input_json)?;
        let outcome = self.execute(&input).await?;

        // Erase rejection to JSON for the dyn boundary. Typed callers
        // re-materialize W::Rejection via `ExecuteOutcome::try_map`.
        outcome
            .try_map(|rejection| serde_json::to_value(&rejection))
            .map_err(crate::Error::from)
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

    async fn replay_latest_state_dynamic(
        &self,
        workflow_id: &crate::workflow::WorkflowId,
    ) -> crate::Result<Value> {
        let state = self.replay_latest_state(workflow_id).await?;
        Ok(serde_json::to_value(state)?)
    }
}

#[async_trait]
impl<W, H, S> TypedEntry<W> for WorkflowEntry<W, H, S>
where
    W: Workflow + Send + Sync + 'static,
    W::State: Send,
    W::Input: Send + Sync,
    W::Rejection: Send,
    H: EffectHandler<Workflow = W>,
    S: Store + WorkflowQueryStore,
{
    async fn execute(&self, input: &W::Input) -> crate::Result<ExecuteOutcome<W::Rejection>> {
        crate::decider::execute::<W, _>(&self.store, self.record_input_observations, input).await
    }

    async fn replay_latest_state(
        &self,
        workflow_id: &crate::workflow::WorkflowId,
    ) -> crate::Result<W::State> {
        let events = self
            .store
            .fetch_workflow_events(W::TYPE, workflow_id)
            .await?;

        let mut state = W::State::default();
        for (sequence, event) in events.into_iter().enumerate() {
            let typed: W::Event = serde_json::from_value(event.payload).map_err(|e| {
                crate::Error::event_deserialization(W::TYPE, workflow_id.as_str(), sequence, e)
            })?;
            state = W::evolve(state, typed);
        }

        Ok(state)
    }
}

/// Registry mapping workflow types to their entries.
///
/// Two parallel views of the same entries:
/// - `entries` (string-keyed) — used by workers claiming timers/effects
///   from the DB and by the public `execute_dynamic` API.
/// - `typed_entries` (TypeId-keyed) — used by typed `execute<W>` to
///   bypass the JSON round-trip. Holds `Arc<dyn TypedEntry<W>>`
///   values boxed as `Box<dyn Any + Send + Sync>`; downcasting is safe
///   because the TypeId key guarantees `W` matches.
pub(crate) struct WorkflowRegistry {
    entries: HashMap<&'static str, Arc<dyn DynamicEntry>>,
    typed_entries: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl WorkflowRegistry {
    /// Create an empty registry.
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            typed_entries: HashMap::new(),
        }
    }

    /// Register a workflow with its handler. Populates both the
    /// string-keyed dynamic map and the TypeId-keyed typed map from the
    /// same shared `Arc<WorkflowEntry<W, H, S>>`.
    fn register<W, H, S>(&mut self, store: S, record_input_observations: bool, handler: H)
    where
        W: Workflow + Send + Sync + 'static,
        W::State: Send + Serialize,
        W::Input: Serialize + DeserializeOwned + Send + Sync,
        W::Effect: DeserializeOwned,
        W::Rejection: Send,
        H: EffectHandler<Workflow = W>,
        S: Store + WorkflowQueryStore,
    {
        let entry = Arc::new(WorkflowEntry {
            store,
            record_input_observations,
            handler,
            _marker: PhantomData,
        });

        // Dynamic (string-keyed) view — used by workers + execute_dynamic.
        let dyn_entry: Arc<dyn DynamicEntry> = entry.clone();
        self.entries.insert(W::TYPE, dyn_entry);

        // Typed (TypeId-keyed) view — used by execute<W>. Boxed through
        // Any because the map value type can't mention W.
        let typed_entry: Arc<dyn TypedEntry<W>> = entry;
        self.typed_entries
            .insert(TypeId::of::<W>(), Box::new(typed_entry));
    }

    /// Look up a workflow entry by type string.
    ///
    /// Returns the static workflow type key and the entry if found.
    /// The static key can be used to create `EffectContext` instances.
    pub(crate) fn get(&self, workflow_type: &str) -> Option<(&'static str, &dyn DynamicEntry)> {
        self.entries
            .get_key_value(workflow_type)
            .map(|(k, v)| (*k, v.as_ref()))
    }

    /// Look up the typed dispatcher for `W`. Returns `None` if `W` wasn't
    /// registered. The downcast is infallible when the TypeId matches,
    /// which is the only way a value ends up at this key.
    pub(crate) fn get_typed<W>(&self) -> Option<Arc<dyn TypedEntry<W>>>
    where
        W: Workflow + 'static,
    {
        self.typed_entries
            .get(&TypeId::of::<W>())
            .and_then(|any| any.downcast_ref::<Arc<dyn TypedEntry<W>>>())
            .cloned()
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
        <H::Workflow as Workflow>::Rejection: Send,
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
        W::Rejection: Send,
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
        // Each entry is a supervisor task that owns the worker task,
        // awaits its termination, and logs panics/cancellations in
        // real time (not at shutdown). This matters for long-running
        // deployments: a worker that dies at t=60s gets logged at
        // t=60s, not at whenever `shutdown` fires.
        let mut supervisors: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        // Spawn effect workers (each under a supervisor)
        for i in 0..effect_worker_count {
            let worker_name = if effect_worker_count == 1 {
                format!("{}-effect", runtime.worker_id)
            } else {
                format!("{}-effect-{}", runtime.worker_id, i)
            };

            let effect_worker = EffectWorker::new(
                Arc::clone(&runtime),
                runtime.store.clone(),
                runtime.config.clone(),
                worker_name.clone(),
            );

            let effect_shutdown_rx = shutdown_rx.clone();
            let runtime_worker_id = runtime.worker_id.clone();
            supervisors.push(tokio::spawn(async move {
                let inner = tokio::spawn(async move {
                    effect_worker.run(effect_shutdown_rx).await;
                });
                log_worker_termination(&runtime_worker_id, &worker_name, inner.await);
            }));
        }

        // Spawn timer workers (each under a supervisor)
        for i in 0..timer_worker_count {
            let worker_name = if timer_worker_count == 1 {
                format!("{}-timer", runtime.worker_id)
            } else {
                format!("{}-timer-{}", runtime.worker_id, i)
            };

            let timer_worker = TimerWorker::new(
                Arc::clone(&runtime),
                runtime.store.clone(),
                runtime.config.clone(),
                worker_name.clone(),
            );

            let timer_shutdown_rx = shutdown_rx.clone();
            let runtime_worker_id = runtime.worker_id.clone();
            supervisors.push(tokio::spawn(async move {
                let inner = tokio::spawn(async move {
                    timer_worker.run(timer_shutdown_rx).await;
                });
                log_worker_termination(&runtime_worker_id, &worker_name, inner.await);
            }));
        }

        // Wait for shutdown signal
        shutdown.await;

        // Signal shutdown to all workers
        let _ = shutdown_tx.send(true);

        // Wait for all supervisors. Each supervisor already logged its
        // worker's termination (panic or cancellation) in real time,
        // so this loop just collects completions. We don't auto-restart
        // — process-level supervision (systemd, k8s) is the expected
        // recovery mechanism. A panicked worker does NOT fail `run()`.
        let shutdown_timeout = runtime.config.shutdown_timeout;
        let all_workers = async move {
            for supervisor in supervisors {
                let _ = supervisor.await;
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
