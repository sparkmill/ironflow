//! Storage abstraction for workflow events and effects.
//!
//! This module provides the [`Store`] and [`UnitOfWork`] traits that abstract
//! over different storage backends. Two implementations are provided:
//!
//! - [`PgStore`] — PostgreSQL storage for production (requires `postgres` feature)

mod outbox;
#[cfg(feature = "postgres")]
mod postgres;

use std::future::Future;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

pub use outbox::{DeadLetter, DeadLetterQuery, OutboxEffect, OutboxStore};
#[cfg(feature = "postgres")]
pub use postgres::PgStore;

use crate::error::Result;
use crate::workflow::WorkflowId;

/// Recorded input observation for introspection.
#[derive(Debug, Clone)]
pub struct InputObservation {
    /// Workflow type that handled the input.
    pub workflow_type: String,
    /// Workflow instance ID.
    pub workflow_id: WorkflowId,
    /// Best-effort input type name.
    pub input_type: String,
    /// Input payload as JSON.
    pub payload: Value,
}

/// Stored event with global ordering metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub global_sequence: i64,
    pub workflow_type: String,
    pub workflow_id: WorkflowId,
    pub sequence: i64,
    pub payload: Value,
    pub created_at: OffsetDateTime,
}

/// Summary of a workflow instance for listing/monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInstanceSummary {
    pub workflow_type: String,
    pub workflow_id: WorkflowId,
    pub created_at: OffsetDateTime,
    pub event_count: i64,
    pub last_event_at: Option<OffsetDateTime>,
    pub completed_at: Option<OffsetDateTime>,
}

/// Result of beginning a unit of work.
///
/// Indicates whether the workflow is active (can process inputs) or already
/// completed (terminal state reached, should skip processing).
pub enum BeginResult<U> {
    /// Workflow is active and ready to process inputs.
    Active {
        /// Existing events for replay.
        events: Vec<Value>,
        /// Unit of work for appending new events/effects.
        uow: U,
    },
    /// Workflow has already completed (terminal state).
    ///
    /// No events loaded, no lock held. Caller should skip processing.
    Completed,
}

/// Storage backend for workflow events and effects.
///
/// Implementations must provide transactional semantics with per-stream locking.
/// The [`Store::begin`] method acquires an exclusive lock on the workflow instance,
/// preventing concurrent modifications to the same stream.
///
/// Users typically don't interact with this trait directly — use
/// [`Decider`](crate::Decider) which orchestrates the full decision cycle.
///
/// # Implementations
///
/// - [`PgStore`] — PostgreSQL with row-level locking (requires `postgres` feature)
pub trait Store: Send + Sync + Clone + 'static {
    /// The unit of work type returned by this store.
    type UnitOfWork<'a>: UnitOfWork + Send
    where
        Self: 'a;

    /// Begin a unit of work for a workflow instance.
    ///
    /// This method:
    /// 1. Checks if workflow is already completed (returns `Completed` if so)
    /// 2. Acquires an exclusive lock on the workflow instance
    /// 3. Loads all existing events for replay
    /// 4. Returns a unit of work for appending new events/effects
    ///
    /// The lock is held until the unit of work is committed or dropped.
    fn begin<'a>(
        &'a self,
        workflow_type: &'static str,
        workflow_id: &WorkflowId,
    ) -> impl Future<Output = Result<BeginResult<Self::UnitOfWork<'a>>>> + Send;
}

/// A transactional unit of work for a single workflow instance.
///
/// All operations are performed within a transaction that holds an exclusive
/// lock on the workflow instance. Changes are only persisted when [`commit`](Self::commit)
/// is called — dropping the unit of work without committing rolls back all changes.
///
/// This trait is not re-exported from the crate root. Users interact with stores
/// via [`Decider`](crate::Decider), not directly with units of work.
pub trait UnitOfWork: Send {
    /// Append events to the event store.
    ///
    /// Events are serialized to JSON and stored with monotonically increasing
    /// sequence numbers within the stream.
    fn append_events<E, I>(&mut self, events: I) -> impl Future<Output = Result<()>> + Send
    where
        E: Serialize + Send,
        I: IntoIterator<Item = E> + Send;

    /// Enqueue effects to the outbox.
    ///
    /// Effects are serialized to JSON and stored for later processing by
    /// effect handlers.
    fn enqueue_effects<F, I>(&mut self, effects: I) -> impl Future<Output = Result<()>> + Send
    where
        F: Serialize + Send,
        I: IntoIterator<Item = F> + Send;

    /// Schedule timers for future input delivery.
    ///
    /// Timers are stored with their `fire_at` timestamp. When the time arrives,
    /// the timer worker will deliver the embedded input to the workflow.
    ///
    /// If a timer has a `key`, it replaces any existing timer with the same key
    /// for the same workflow instance.
    fn schedule_timers<T>(&mut self, timers: T) -> impl Future<Output = Result<()>> + Send
    where
        T: IntoIterator<Item = crate::Timer<serde_json::Value>> + Send;

    /// Cancel pending timers by key for the current workflow instance.
    fn cancel_timers(&mut self, keys: Vec<String>) -> impl Future<Output = Result<()>> + Send;

    /// Record an input observation for introspection.
    ///
    /// This is optional and can be toggled by the service configuration.
    fn record_input_observation(
        &mut self,
        observation: InputObservation,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Mark the workflow as completed (terminal state reached).
    ///
    /// When committed, the workflow will be marked as completed in the store,
    /// which can be used for cleanup, monitoring, or rejecting further inputs.
    fn mark_completed(&mut self);

    /// Commit the unit of work, persisting all changes and releasing the lock.
    ///
    /// After commit, the events are visible to subsequent reads, the effects
    /// are available for processing, and the timers are scheduled.
    fn commit(self) -> impl Future<Output = Result<()>> + Send;
}

/// Event store operations needed for projection replay.
pub trait EventStore: Send + Sync + Clone + 'static {
    /// Fetch events after the provided global sequence (exclusive).
    ///
    /// Returns events ordered by `global_sequence` ascending.
    fn fetch_events_since(
        &self,
        after: i64,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<StoredEvent>>> + Send;
}

/// Projection position storage for projection workers.
pub trait ProjectionStore: Send + Sync + Clone + 'static {
    /// Load the last processed global sequence for a projection.
    fn load_projection_position(
        &self,
        projection_name: &str,
    ) -> impl Future<Output = Result<i64>> + Send;

    /// Persist the last processed global sequence for a projection.
    fn store_projection_position(
        &self,
        projection_name: &str,
        global_sequence: i64,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// Query-only store operations for inspecting workflow history.
#[async_trait]
pub trait WorkflowQueryStore: Send + Sync + Clone + 'static {
    /// List workflow instances, optionally filtering by type.
    async fn list_workflows(
        &self,
        workflow_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<WorkflowInstanceSummary>>;

    /// Fetch all events for a workflow instance ordered by sequence.
    async fn fetch_workflow_events(
        &self,
        workflow_type: &str,
        workflow_id: &WorkflowId,
    ) -> Result<Vec<StoredEvent>>;
}
