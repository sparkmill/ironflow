//! Outbox storage operations for effect processing.

use std::future::Future;
use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::workflow::{WorkflowId, WorkflowRef};

/// A claimed effect from the outbox, ready for processing.
///
/// Contains all metadata needed to execute the effect and route results.
#[derive(Debug, Clone)]
pub struct OutboxEffect {
    /// Unique identifier for this effect (UUID v7).
    pub id: Uuid,
    /// The workflow this effect belongs to.
    pub workflow: WorkflowRef,
    /// The effect payload as JSON.
    pub payload: Value,
    /// Number of previous attempts (0 for first try).
    pub attempts: u32,
    /// When the effect was created.
    pub created_at: OffsetDateTime,
}

/// A dead-lettered effect that has exceeded maximum retry attempts.
///
/// Dead letters are effects that failed permanently or exceeded the
/// configured `max_attempts`. They remain in the outbox for inspection
/// and manual retry.
#[derive(Debug, Clone)]
pub struct DeadLetter {
    /// Unique identifier for this effect (UUID v7).
    pub id: Uuid,
    /// The workflow this effect belongs to.
    pub workflow: WorkflowRef,
    /// The effect payload as JSON.
    pub payload: Value,
    /// Number of failed attempts.
    pub attempts: u32,
    /// The last error message from the most recent failure.
    pub last_error: Option<String>,
    /// When the effect was created.
    pub created_at: OffsetDateTime,
}

/// Query parameters for fetching dead letters.
///
/// Use the builder methods to filter by workflow type, workflow ID,
/// or limit the number of results.
#[derive(Debug, Clone, Default)]
pub struct DeadLetterQuery {
    /// Filter by workflow type.
    pub workflow_type: Option<String>,
    /// Filter by workflow ID.
    pub workflow_id: Option<WorkflowId>,
    /// Maximum number of results to return.
    pub limit: Option<u32>,
}

impl DeadLetterQuery {
    /// Create a new empty query (matches all dead letters).
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter by workflow type.
    pub fn workflow_type(mut self, workflow_type: impl Into<String>) -> Self {
        self.workflow_type = Some(workflow_type.into());
        self
    }

    /// Filter by workflow ID.
    pub fn workflow_id(mut self, workflow_id: impl Into<WorkflowId>) -> Self {
        self.workflow_id = Some(workflow_id.into());
        self
    }

    /// Limit the number of results.
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// Storage operations for effect processing.
///
/// This trait extends the base storage with outbox-specific operations
/// needed by the effect worker. Only implemented for stores that support
/// persistent effect processing (e.g., PostgreSQL).
///
/// # Locking Protocol
///
/// Effects are claimed using optimistic locking:
/// 1. `claim_effect` atomically selects and locks an effect
/// 2. The effect is locked for `lock_duration`
/// 3. `mark_processed` or `record_failure` must be called before the lock expires
/// 4. If a worker crashes, the lock expires and another worker can claim it
pub trait OutboxStore: Send + Sync + Clone + 'static {
    /// Claim the next available immediate effect for processing.
    ///
    /// Returns `None` if no effects are available. The effect is locked
    /// for `lock_duration` to prevent double-processing.
    ///
    /// Effects where `attempts >= max_attempts` are excluded (dead letters).
    ///
    /// # Arguments
    ///
    /// * `worker_id` - Identifier for this worker (for debugging)
    /// * `lock_duration` - How long to hold the lock
    /// * `max_attempts` - Maximum attempts before an effect becomes dead-lettered
    fn claim_effect(
        &self,
        worker_id: &str,
        lock_duration: Duration,
        max_attempts: u32,
    ) -> impl Future<Output = crate::Result<Option<OutboxEffect>>> + Send;

    /// Mark an effect as successfully processed.
    ///
    /// Sets `processed_at` to the current time, removing it from the
    /// pending queue.
    fn mark_processed(&self, effect_id: Uuid) -> impl Future<Output = crate::Result<()>> + Send;

    /// Record a failure and schedule retry with backoff.
    ///
    /// Increments `attempts`, records the error message, and sets
    /// `locked_until` to `now + backoff_duration` to delay retry.
    ///
    /// The effect worker checks `attempts >= max_attempts` to determine
    /// if the effect should be dead-lettered (no more retries).
    fn record_failure(
        &self,
        effect_id: Uuid,
        error: &str,
        backoff_duration: Duration,
    ) -> impl Future<Output = crate::Result<()>> + Send;

    /// Record a permanent failure, immediately dead-lettering the effect.
    ///
    /// This sets `attempts` to `max_attempts` to exclude the effect from retries.
    fn record_permanent_failure(
        &self,
        effect_id: Uuid,
        error: &str,
        max_attempts: u32,
    ) -> impl Future<Output = crate::Result<()>> + Send;

    /// Claim the next available timer effect for processing.
    ///
    /// Returns `None` if no timers are due. A timer is due when
    /// `due_at <= now()`. The effect is locked for `lock_duration`.
    ///
    /// Timer effects where `attempts >= max_attempts` are excluded (dead letters).
    ///
    /// Timer effects contain an embedded `input` field that should be
    /// routed directly to the workflow's decider.
    ///
    /// # Arguments
    ///
    /// * `worker_id` - Identifier for this worker (for debugging)
    /// * `lock_duration` - How long to hold the lock
    /// * `max_attempts` - Maximum attempts before a timer becomes dead-lettered
    fn claim_timer(
        &self,
        worker_id: &str,
        lock_duration: Duration,
        max_attempts: u32,
    ) -> impl Future<Output = crate::Result<Option<OutboxEffect>>> + Send;

    /// Fetch dead-lettered effects matching the query.
    ///
    /// Dead letters are effects where `attempts >= max_attempts` and
    /// `processed_at IS NULL`. Use [`DeadLetterQuery`] to filter results.
    ///
    /// # Arguments
    ///
    /// * `query` - Filter and pagination parameters
    /// * `max_attempts` - The configured maximum attempts threshold
    fn fetch_dead_letters(
        &self,
        query: &DeadLetterQuery,
        max_attempts: u32,
    ) -> impl Future<Output = crate::Result<Vec<DeadLetter>>> + Send;

    /// Retry a dead-lettered effect.
    ///
    /// Resets the effect's `attempts` to 0 and clears `locked_until`,
    /// making it available for processing again.
    ///
    /// Returns `Ok(true)` if the effect was found and reset,
    /// `Ok(false)` if the effect was not found or already processed.
    fn retry_dead_letter(
        &self,
        effect_id: Uuid,
    ) -> impl Future<Output = crate::Result<bool>> + Send;

    /// Count dead-lettered effects matching the query.
    ///
    /// Useful for monitoring and alerting on dead letter queue size.
    fn count_dead_letters(
        &self,
        query: &DeadLetterQuery,
        max_attempts: u32,
    ) -> impl Future<Output = crate::Result<u64>> + Send;

    /// Mark a timer as successfully processed.
    ///
    /// Sets `processed_at` to the current time, removing it from the
    /// pending queue in the timers table.
    fn mark_timer_processed(
        &self,
        timer_id: Uuid,
    ) -> impl Future<Output = crate::Result<()>> + Send;

    /// Record a timer execution failure and schedule retry with backoff.
    ///
    /// Increments `attempts`, records the error message, and sets
    /// `locked_until` to `now + backoff_duration` to delay retry.
    fn record_timer_failure(
        &self,
        timer_id: Uuid,
        error: &str,
        backoff_duration: Duration,
    ) -> impl Future<Output = crate::Result<()>> + Send;

    /// Fetch dead-lettered timers matching the query.
    ///
    /// Dead letters are timers where `attempts >= max_attempts` and
    /// `processed_at IS NULL`.
    fn fetch_timer_dead_letters(
        &self,
        query: &DeadLetterQuery,
        max_attempts: u32,
    ) -> impl Future<Output = crate::Result<Vec<DeadLetter>>> + Send;

    /// Retry a dead-lettered timer.
    ///
    /// Resets the timer's `attempts` to 0 and clears `locked_until`.
    fn retry_timer_dead_letter(
        &self,
        timer_id: Uuid,
    ) -> impl Future<Output = crate::Result<bool>> + Send;

    /// Count dead-lettered timers matching the query.
    fn count_timer_dead_letters(
        &self,
        query: &DeadLetterQuery,
        max_attempts: u32,
    ) -> impl Future<Output = crate::Result<u64>> + Send;
}
