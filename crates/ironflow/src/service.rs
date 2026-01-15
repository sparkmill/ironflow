//! Workflow service entrypoint.

use std::sync::Arc;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::Workflow;
use crate::error::{Error, Result};
use crate::runtime::registry::{WorkflowEntry, WorkflowRegistry};
use crate::store::{Store, StoredEvent, WorkflowInstanceSummary, WorkflowQueryStore};
use crate::workflow::WorkflowId;

/// Configuration for the workflow service.
#[derive(Debug, Clone, Default)]
pub struct WorkflowServiceConfig {
    /// Record input observations in the store.
    pub record_input_observations: bool,
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
    pub async fn execute<W>(&self, input: &W::Input) -> Result<()>
    where
        W: Workflow + Send + Sync + 'static,
        W::State: Send,
        W::Input: Serialize + DeserializeOwned + Send + Sync,
        W::Effect: DeserializeOwned,
    {
        let Some((_workflow_type, entry)) = self.registry.get(W::TYPE) else {
            return Err(Error::UnknownWorkflowType(W::TYPE.to_string()));
        };

        let input_json = serde_json::to_value(input)?;
        entry.execute(input_json).await
    }

    /// Execute an untyped workflow input by type string.
    pub async fn execute_dynamic(&self, workflow_type: &str, payload: &Value) -> Result<()> {
        let Some((_workflow_type, entry)) = self.registry.get(workflow_type) else {
            return Err(Error::UnknownWorkflowType(workflow_type.to_string()));
        };

        entry.execute(payload.clone()).await
    }

    /// Returns the number of registered workflows.
    pub(crate) fn workflow_count(&self) -> usize {
        self.registry.len()
    }

    /// Look up a workflow entry by type.
    pub(crate) fn get_entry(
        &self,
        workflow_type: &str,
    ) -> Option<(&'static str, &dyn WorkflowEntry)> {
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

    /// Rebuild the latest state for a workflow and return it as JSON.
    pub async fn fetch_latest_state(
        &self,
        workflow_type: &str,
        workflow_id: &WorkflowId,
    ) -> Result<Value> {
        let Some((_workflow_type, entry)) = self.registry.get(workflow_type) else {
            return Err(Error::UnknownWorkflowType(workflow_type.to_string()));
        };

        entry.replay_latest_state(workflow_id).await
    }
}
