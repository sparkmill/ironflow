//! Workflow engine bundle.

use std::sync::Arc;

use crate::runtime::WorkflowRuntime;
use crate::service::WorkflowService;
use crate::store::{Store, WorkflowQueryStore};

/// Convenience bundle for a service + runtime pair.
#[derive(Clone)]
pub struct WorkflowEngine<S>
where
    S: Store + WorkflowQueryStore,
{
    pub service: Arc<WorkflowService<S>>,
    pub runtime: WorkflowRuntime<S>,
}
