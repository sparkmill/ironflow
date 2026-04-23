//! Workflow service entrypoint.

use std::sync::Arc;

use serde_json::Value;

use crate::Workflow;
use crate::error::{Error, Result};
use crate::runtime::registry::{DynamicEntry, WorkflowRegistry};
use crate::store::{Store, StoredEvent, WorkflowInstanceSummary, WorkflowQueryStore};
use crate::workflow::WorkflowId;

/// Configuration for the workflow service.
#[derive(Debug, Clone, Default)]
pub struct WorkflowServiceConfig {
    /// Record input observations in the store. When enabled, accepted,
    /// rejected, and dropped-because-completed inputs are all persisted to
    /// `ironflow.input_observations` with their respective `outcome` and
    /// rejection payloads. Defaults to `false` (audit is opt-in).
    pub record_input_observations: bool,
}

/// Result of [`WorkflowService::execute`] and
/// [`WorkflowService::execute_dynamic`].
///
/// Generic over the rejection payload type `R`:
/// - Typed callers (`execute::<W>`) get `ExecuteOutcome<W::Rejection>` — the
///   concrete domain type.
/// - Dynamic callers (`execute_dynamic`) get `ExecuteOutcome<Value>` — the
///   rejection serialized to JSON, since the type isn't known at compile
///   time.
///
/// Surfaces what happened to an input: events produced (`Accepted`),
/// rejection from the workflow's decide logic (`Rejected`), or silent drop
/// because the target workflow had already completed (`AlreadyCompleted`).
/// Rejection and completion are not errors — they're valid business
/// outcomes. The `Err` side of the surrounding `Result` is reserved for
/// framework failures (DB, serde, unknown workflow type).
#[derive(Debug, Clone)]
pub enum ExecuteOutcome<R> {
    /// Events were appended, effects enqueued, timers scheduled.
    Accepted {
        /// Number of events the decider appended in this call.
        events_appended: usize,
    },
    /// Workflow's decide returned a rejection payload. No state change.
    Rejected(R),
    /// Workflow was already terminal; input was not dispatched to decide.
    AlreadyCompleted,
}

impl<R> ExecuteOutcome<R> {
    /// Transform the rejection payload by `f`, preserving `Accepted` and
    /// `AlreadyCompleted`.
    pub fn map<R2, F>(self, f: F) -> ExecuteOutcome<R2>
    where
        F: FnOnce(R) -> R2,
    {
        match self {
            ExecuteOutcome::Accepted { events_appended } => {
                ExecuteOutcome::Accepted { events_appended }
            }
            ExecuteOutcome::Rejected(r) => ExecuteOutcome::Rejected(f(r)),
            ExecuteOutcome::AlreadyCompleted => ExecuteOutcome::AlreadyCompleted,
        }
    }

    /// Fallibly transform the rejection payload by `f`. Used by the typed
    /// facade to deserialize `ExecuteOutcome<Value>` into
    /// `ExecuteOutcome<W::Rejection>`.
    pub fn try_map<R2, E, F>(self, f: F) -> std::result::Result<ExecuteOutcome<R2>, E>
    where
        F: FnOnce(R) -> std::result::Result<R2, E>,
    {
        Ok(match self {
            ExecuteOutcome::Accepted { events_appended } => {
                ExecuteOutcome::Accepted { events_appended }
            }
            ExecuteOutcome::Rejected(r) => ExecuteOutcome::Rejected(f(r)?),
            ExecuteOutcome::AlreadyCompleted => ExecuteOutcome::AlreadyCompleted,
        })
    }
}

/// App-facing workflow service.
///
/// This is the single entrypoint for executing workflow inputs.
#[derive(Clone)]
pub struct WorkflowService<S>
where
    S: Store,
{
    registry: Arc<WorkflowRegistry>,
    store: S,
    config: WorkflowServiceConfig,
}

impl<S> WorkflowService<S>
where
    S: Store,
{
    /// Create a new workflow service from a registry.
    pub(crate) fn new(
        store: S,
        registry: Arc<WorkflowRegistry>,
        config: WorkflowServiceConfig,
    ) -> Self {
        Self {
            registry,
            store,
            config,
        }
    }

    /// Execute a typed workflow input.
    ///
    /// Goes through the TypeId-keyed typed dispatcher so the rejection
    /// stays as `W::Rejection` end-to-end — no JSON round-trip.
    /// `Err(...)` is reserved for framework failures (DB, unknown workflow
    /// type).
    pub async fn execute<W>(&self, input: &W::Input) -> Result<ExecuteOutcome<W::Rejection>>
    where
        W: Workflow + Send + Sync + 'static,
        W::State: Send,
        W::Input: Send + Sync,
        W::Rejection: Send,
    {
        let Some(entry) = self.registry.get_typed::<W>() else {
            return Err(Error::UnknownWorkflowType(W::TYPE.to_string()));
        };
        entry.execute(input).await
    }

    /// Execute an untyped workflow input by type string.
    ///
    /// Rejection payloads are surfaced as [`serde_json::Value`] since the
    /// caller doesn't know the workflow's `Rejection` type at compile time.
    pub async fn execute_dynamic(
        &self,
        workflow_type: &str,
        payload: &Value,
    ) -> Result<ExecuteOutcome<Value>> {
        let Some((_workflow_type, entry)) = self.registry.get(workflow_type) else {
            return Err(Error::UnknownWorkflowType(workflow_type.to_string()));
        };

        entry.execute_dynamic(payload.clone()).await
    }

    /// Returns the number of registered workflows.
    pub(crate) fn workflow_count(&self) -> usize {
        self.registry.len()
    }

    /// Look up a workflow entry by type.
    pub(crate) fn get_entry(
        &self,
        workflow_type: &str,
    ) -> Option<(&'static str, &dyn DynamicEntry)> {
        self.registry.get(workflow_type)
    }

    /// Returns the service configuration.
    pub fn config(&self) -> &WorkflowServiceConfig {
        &self.config
    }
}

impl<S> WorkflowService<S>
where
    S: Store + WorkflowQueryStore,
{
    /// List workflow instances, optionally filtered by type.
    pub async fn list_workflows(
        &self,
        workflow_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<WorkflowInstanceSummary>> {
        self.store
            .list_workflows(workflow_type, limit, offset)
            .await
    }

    /// Fetch full event history for a workflow instance.
    pub async fn fetch_workflow_events(
        &self,
        workflow_type: &str,
        workflow_id: &WorkflowId,
    ) -> Result<Vec<StoredEvent>> {
        self.store
            .fetch_workflow_events(workflow_type, workflow_id)
            .await
    }

    /// Rebuild the latest state for a workflow, typed as `W::State`.
    ///
    /// Goes through the TypeId-keyed typed dispatcher so the state stays
    /// as `W::State` end-to-end — no JSON round-trip.
    pub async fn fetch_latest_state<W>(&self, workflow_id: &WorkflowId) -> Result<W::State>
    where
        W: Workflow + Send + Sync + 'static,
        W::State: Send,
    {
        let Some(entry) = self.registry.get_typed::<W>() else {
            return Err(Error::UnknownWorkflowType(W::TYPE.to_string()));
        };
        entry.replay_latest_state(workflow_id).await
    }

    /// Rebuild the latest state for a workflow by type string and return it as JSON.
    ///
    /// Use this when the workflow type isn't known at compile time (e.g. HTTP
    /// dashboards). Typed callers should prefer [`Self::fetch_latest_state`].
    pub async fn fetch_latest_state_dynamic(
        &self,
        workflow_type: &str,
        workflow_id: &WorkflowId,
    ) -> Result<Value> {
        let Some((_workflow_type, entry)) = self.registry.get(workflow_type) else {
            return Err(Error::UnknownWorkflowType(workflow_type.to_string()));
        };

        entry.replay_latest_state_dynamic(workflow_id).await
    }
}
