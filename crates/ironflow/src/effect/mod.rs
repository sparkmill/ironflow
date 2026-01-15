//! Effect execution types for workflow side effects.
//!
//! This module provides the core types for executing effects produced by workflows:
//!
//! - [`EffectHandler`] — Trait for per-workflow effect execution
//! - [`EffectContext`] — Metadata for correlation and idempotency
//! - [`RetryPolicy`] — Configuration for exponential backoff

mod context;
pub(crate) mod handler;
mod retry;

pub use context::EffectContext;
pub use handler::EffectHandler;
pub use retry::RetryPolicy;
