//! Workflow service entrypoint.

use std::sync::Arc;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::Workflow;
use crate::error::{Error, Result};
use crate::runtime::registry::{WorkflowEntry, WorkflowRegistry};

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
pub struct WorkflowService {
    registry: Arc<WorkflowRegistry>,
    config: WorkflowServiceConfig,
}

impl WorkflowService {
    /// Create a new workflow service from a registry.
    pub(crate) fn new(registry: Arc<WorkflowRegistry>, config: WorkflowServiceConfig) -> Self {
        Self { registry, config }
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
