//! Event-sourced workflow engine for durable, long-running processes.
//!
//! Ironflow provides a minimal, type-safe framework for building workflows where:
//!
//! - **Pure functional core** — [`Workflow::evolve`] and [`Workflow::decide`] are
//!   deterministic with no side effects
//! - **Event sourcing** — State is reconstructed by replaying events
//! - **Async effects** — Side effects are queued to an outbox for external processing
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                            Decider<W, S>                                │
//! │                                                                         │
//! │   1. Begin unit of work (acquires lock)                                 │
//! │   2. Replay events → state                                              │
//! │   3. decide(now, state, input) → events + effects                       │
//! │   4. Append events to store                                             │
//! │   5. Enqueue effects to outbox                                          │
//! │   6. Commit unit of work                                                │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use ironflow::{Decision, Decider, HasWorkflowId, PgStore, Workflow, WorkflowId};
//!
//! struct CounterWorkflow;
//!
//! impl Workflow for CounterWorkflow {
//!     type State = i32;
//!     type Input = CounterInput;
//!     type Event = CounterEvent;
//!     type Effect = ();
//!
//!     const TYPE: &'static str = "counter";
//!
//!     fn evolve(state: Self::State, event: Self::Event) -> Self::State {
//!         match event {
//!             CounterEvent::Incremented => state + 1,
//!         }
//!     }
//!
//!     fn decide(_now: time::OffsetDateTime, _state: &Self::State, _input: &Self::Input)
//!         -> Decision<Self::Event, Self::Effect, Self::Input>
//!     {
//!         Decision::event(CounterEvent::Incremented)
//!     }
//! }
//!
//! // Execute a workflow decision
//! let store = PgStore::new(pool);
//! let decider: Decider<CounterWorkflow, _> = Decider::new(store);
//! decider.execute(&CounterInput::Increment { id: "counter-1".into() }).await?;
//! ```
//!
//! # Feature Flags
//!
//! - `postgres` — Enables [`PgStore`] for production use with PostgreSQL
//!
//! # Design Documentation
//!
//! See `DESIGN.md` for architectural decisions and future work.

// Allow the crate to reference itself as `ironflow` for macro-generated code
extern crate self as ironflow;

mod decider;
pub mod effect;
mod engine;
mod error;
mod projection;
pub mod runtime;
mod service;
pub mod store;
mod timer;
pub mod visualization;
mod workflow;

pub use effect::{EffectContext, EffectHandler, RetryPolicy};
pub use engine::WorkflowEngine;
pub use error::{Error, Result};
pub use nonempty::NonEmpty;
pub use projection::{Projection, ProjectionConfig, ProjectionEvent, ProjectionWorker};
pub use runtime::{RuntimeConfig, WorkflowBuilder, WorkflowRuntime};
pub use service::{WorkflowService, WorkflowServiceConfig};
#[cfg(feature = "postgres")]
pub use store::PgStore;
pub use store::{
    BeginResult, DeadLetter, DeadLetterQuery, EventStore, InputObservation, OutboxStore,
    ProjectionStore, Store, StoredEvent,
};
pub use timer::Timer;
pub use visualization::{
    FieldValue, StateDefinition, StateInfo, StateMachineDefinition, StateMachineView,
    TransitionDefinition, TransitionInfo,
};
pub use workflow::{Decision, HasWorkflowId, Workflow, WorkflowId, WorkflowRef};

// Re-export derive macros
pub use ironflow_macros::HasWorkflowId;
