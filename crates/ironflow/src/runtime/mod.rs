//! Runtime for executing workflow effects.
//!
//! This module provides the infrastructure for processing effects from the outbox:
//!
//! - [`WorkflowRuntime`] — Main coordinator that runs effect and timer workers
//! - [`WorkflowBuilder`] — Builder for registering workflows and configuring the runtime
//! - [`RuntimeConfig`] — Configuration for polling intervals, timeouts, etc.
//!
//! # Example
//!
//! ```ignore
//! use ironflow::runtime::{RuntimeConfig, WorkflowRuntime};
//!
//! let runtime = WorkflowRuntime::builder(pool, WorkflowServiceConfig::default())
//!     .register::<OrderWorkflow>(order_handler)
//!     .register::<InventoryWorkflow>(inventory_handler)
//!     .config(RuntimeConfig::default())
//!     .build_runtime()?;
//!
//! runtime.run(shutdown_signal).await?;
//! ```

mod config;
mod effect_worker;
pub(crate) mod registry;
mod timer_worker;

pub use config::RuntimeConfig;
pub use registry::{WorkflowBuilder, WorkflowRuntime};
