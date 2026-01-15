//! Core workflow traits and types.

use std::time::Duration;

use nonempty::NonEmpty;
use serde::{Serialize, de::DeserializeOwned};
use time::OffsetDateTime;

use crate::Timer;

/// Pure workflow logic: state reconstruction via `evolve`, decisions via `decide`.
///
/// Both functions must be deterministic with no side effects.
/// Side effects are expressed as [`Self::Effect`] values executed by the runtime.
/// Scheduled inputs are expressed as [`Timer`] values in the decision.
///
/// # Correlation
///
/// Inputs are matched to workflow instances via a correlation key:
///
/// ```text
/// correlation_key = (Workflow::TYPE, input.workflow_id())
/// ```
///
/// # Effects vs Timers
///
/// - **Effects** are side effects executed immediately (send email, call API)
/// - **Timers** are scheduled inputs delivered at a future time
///
/// See the [`Timer`] documentation for more details on timer semantics.
///
/// # Example
///
/// ```ignore
/// impl Workflow for OrderWorkflow {
///     type State = OrderState;
///     type Input = OrderInput;
///     type Event = OrderEvent;
///     type Effect = OrderEffect;
///
///     const TYPE: &'static str = "order";
///
///     fn evolve(mut state: Self::State, event: Self::Event) -> Self::State {
///         match event {
///             OrderEvent::Created { items } => {
///                 state.items = items;
///                 state.status = OrderStatus::Created;
///             }
///             OrderEvent::Shipped { tracking } => {
///                 state.status = OrderStatus::Shipped { tracking };
///             }
///         }
///         state
///     }
///
///     fn decide(now: OffsetDateTime, state: &Self::State, input: &Self::Input)
///         -> Decision<Self::Event, Self::Effect, Self::Input>
///     {
///         match input {
///             OrderInput::Create { order_id, items, .. } => {
///                 Decision::event(OrderEvent::Created { items: items.clone() })
///                     .with_effect(OrderEffect::SendConfirmation)
///                     .with_timer_after(
///                         Duration::from_secs(3600),
///                         OrderInput::PaymentTimeout { order_id: order_id.clone() }
///                     )
///             }
///             OrderInput::Cancel { .. } if state.is_shipped() => {
///                 Decision::event(OrderEvent::CancelRejected { reason: "Already shipped" })
///                     .with_effect(OrderEffect::NotifySupport)
///             }
///             OrderInput::Ship { tracking, .. } => {
///                 Decision::event(OrderEvent::Shipped { tracking: tracking.clone() })
///             }
///         }
///     }
/// }
/// ```
pub trait Workflow {
    /// The workflow state, reconstructed by replaying events.
    type State: Default;

    /// Input commands/signals that trigger decisions.
    ///
    /// Must be serializable for timer storage.
    type Input: HasWorkflowId + Serialize + DeserializeOwned;

    /// Facts recorded to the event store. Must be serializable for persistence.
    type Event: Serialize + DeserializeOwned + Clone + Send;

    /// Side effects queued to the outbox for external processing.
    type Effect: Serialize + Send;

    /// Workflow type identifier. Combined with [`HasWorkflowId::workflow_id`]
    /// to form a [`WorkflowRef`] correlation key. Must be stable across deployments.
    const TYPE: &'static str;

    /// Reconstruct state from an event.
    ///
    /// Called during replay to rebuild the current state from historical events.
    /// Must be deterministic — same events must produce same state.
    fn evolve(state: Self::State, event: Self::Event) -> Self::State;

    /// Decide what actions to take given the current state and input.
    ///
    /// Must be deterministic and side-effect free. The `now` parameter provides
    /// the current time for decisions that depend on it (e.g., scheduling timers).
    ///
    /// Returns a [`Decision`] containing events to persist, effects to execute,
    /// and timers to schedule. Every input must produce at least one event.
    fn decide(
        now: OffsetDateTime,
        state: &Self::State,
        input: &Self::Input,
    ) -> Decision<Self::Event, Self::Effect, Self::Input>;

    /// Check if the state represents a terminal (completed) workflow.
    ///
    /// Terminal workflows are marked as completed in the store and can be
    /// skipped for cleanup, monitoring, or to reject further inputs.
    ///
    /// Default implementation returns `false` (workflow never terminates).
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn is_terminal(state: &Self::State) -> bool {
    ///     matches!(state.status, OrderStatus::Completed | OrderStatus::Cancelled)
    /// }
    /// ```
    fn is_terminal(_state: &Self::State) -> bool {
        false
    }
}

/// Extracts the workflow instance ID (business key) from an input.
///
/// Combined with [`Workflow::TYPE`] to form the correlation key.
///
/// # Example
///
/// ```ignore
/// impl HasWorkflowId for OrderInput {
///     fn workflow_id(&self) -> WorkflowId {
///         match self {
///             OrderInput::Create { order_id, .. } => WorkflowId::new(order_id),
///             OrderInput::Cancel { order_id, .. } => WorkflowId::new(order_id),
///         }
///     }
/// }
/// ```
pub trait HasWorkflowId {
    /// Returns the workflow instance ID for this input.
    ///
    /// Should return the same ID for all inputs targeting the same workflow instance.
    fn workflow_id(&self) -> WorkflowId;
}

/// A workflow instance identifier (business key).
///
/// Use natural business keys (order_id, listing_id) rather than synthetic UUIDs.
/// This makes correlation intuitive and idempotency natural.
///
/// # Example
///
/// ```
/// use ironflow::WorkflowId;
///
/// let id = WorkflowId::new("ord-123");
/// assert_eq!(id.as_str(), "ord-123");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkflowId(String);

impl WorkflowId {
    /// Create a new workflow ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Consume the wrapper and return the inner string.
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Borrow the ID as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkflowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl From<String> for WorkflowId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for WorkflowId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Reference to a specific workflow instance.
///
/// Combines workflow type and instance ID into a single correlation key.
/// Used throughout the system to identify workflow instances.
///
/// # Example
///
/// ```
/// use ironflow::{WorkflowRef, WorkflowId};
///
/// let workflow = WorkflowRef::new("order", "ord-123");
/// assert_eq!(workflow.workflow_type(), "order");
/// assert_eq!(workflow.workflow_id().as_str(), "ord-123");
/// assert_eq!(format!("{}", workflow), "order:ord-123");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkflowRef {
    workflow_type: String,
    workflow_id: WorkflowId,
}

impl WorkflowRef {
    /// Create a new workflow reference.
    pub fn new(workflow_type: impl Into<String>, workflow_id: impl Into<WorkflowId>) -> Self {
        Self {
            workflow_type: workflow_type.into(),
            workflow_id: workflow_id.into(),
        }
    }

    /// The workflow type (e.g., "order", "inventory").
    pub fn workflow_type(&self) -> &str {
        &self.workflow_type
    }

    /// The workflow instance ID (business key).
    pub fn workflow_id(&self) -> &WorkflowId {
        &self.workflow_id
    }

    /// Consume and return the inner workflow ID.
    pub fn into_workflow_id(self) -> WorkflowId {
        self.workflow_id
    }
}

impl std::fmt::Display for WorkflowRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.workflow_type, self.workflow_id)
    }
}

impl<S: Into<String>> From<(&'static str, S)> for WorkflowRef {
    fn from((workflow_type, workflow_id): (&'static str, S)) -> Self {
        Self::new(workflow_type, workflow_id.into())
    }
}

impl From<(String, WorkflowId)> for WorkflowRef {
    fn from((workflow_type, workflow_id): (String, WorkflowId)) -> Self {
        Self {
            workflow_type,
            workflow_id,
        }
    }
}

/// Actions to execute as a result of a workflow decision.
///
/// Every decision must produce at least one event (enforced by [`NonEmpty`]).
/// This ensures a complete audit trail — every input results in a recorded event,
/// even if it's a rejection event for invalid inputs.
///
/// # Structure
///
/// - **Events**: Facts about what happened (at least one required)
/// - **Effects**: Side effects to execute immediately (optional)
/// - **Timers**: Inputs to deliver at a future time (optional)
/// - **Timer cancellations**: Remove pending timers by key (optional)
///
/// # Example
///
/// ```ignore
/// use std::time::Duration;
/// use ironflow::Decision;
///
/// // Decision with event and effect
/// let decision = Decision::event(OrderEvent::Created)
///     .with_effect(OrderEffect::SendConfirmation);
///
/// // Rejection is just another event type
/// let decision = Decision::event(OrderEvent::CancelRejected { reason: "shipped" })
///     .with_effect(OrderEffect::NotifySupport);
/// ```
#[derive(Debug, Clone)]
pub struct Decision<E, F, I> {
    events: NonEmpty<E>,
    effects: Vec<F>,
    timers: Vec<Timer<I>>,
    cancel_timers: Vec<String>,
}

impl<E, F, I> Decision<E, F, I> {
    /// Create a decision with a single event.
    pub fn event(event: E) -> Self {
        Self {
            events: NonEmpty::new(event),
            effects: vec![],
            timers: vec![],
            cancel_timers: vec![],
        }
    }

    /// Create a decision from a non-empty collection of events.
    pub fn from_events(events: NonEmpty<E>) -> Self {
        Self {
            events,
            effects: vec![],
            timers: vec![],
            cancel_timers: vec![],
        }
    }

    /// Try to create a decision from an iterator of events.
    ///
    /// Returns `None` if the iterator is empty.
    pub fn try_from_iter(events: impl IntoIterator<Item = E>) -> Option<Self> {
        Some(Self {
            events: NonEmpty::collect(events)?,
            effects: vec![],
            timers: vec![],
            cancel_timers: vec![],
        })
    }

    /// Add an effect to this decision.
    ///
    /// Effects are side effects executed immediately by the effect worker
    /// (e.g., send email, call API, update external system).
    pub fn with_effect(mut self, effect: F) -> Self {
        self.effects.push(effect);
        self
    }

    /// Add multiple effects to this decision.
    pub fn with_effects(mut self, effects: impl IntoIterator<Item = F>) -> Self {
        self.effects.extend(effects);
        self
    }

    /// Add a timer to this decision.
    ///
    /// Timers schedule an input to be delivered at a future time.
    /// When the timer fires, the input is routed to the workflow's `decide` function.
    pub fn with_timer(mut self, timer: Timer<I>) -> Self {
        self.timers.push(timer);
        self
    }

    /// Add a timer that fires at a specific time.
    ///
    /// Convenience method for `with_timer(Timer::at(fire_at, input))`.
    pub fn with_timer_at(self, fire_at: OffsetDateTime, input: I) -> Self {
        self.with_timer(Timer::at(fire_at, input))
    }

    /// Add a timer that fires after a delay from now.
    ///
    /// Convenience method for `with_timer(Timer::after(delay, input))`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// Decision::event(OrderEvent::Created)
    ///     .with_timer_after(
    ///         Duration::from_secs(3600),
    ///         OrderInput::PaymentTimeout { order_id }
    ///     )
    /// ```
    pub fn with_timer_after(self, delay: Duration, input: I) -> Self {
        self.with_timer(Timer::after(delay, input))
    }

    /// Add multiple timers to this decision.
    pub fn with_timers(mut self, timers: impl IntoIterator<Item = Timer<I>>) -> Self {
        self.timers.extend(timers);
        self
    }

    /// Cancel a pending timer by key.
    pub fn cancel_timer(mut self, key: impl Into<String>) -> Self {
        self.cancel_timers.push(key.into());
        self
    }

    /// Cancel multiple pending timers by key.
    pub fn cancel_timers(mut self, keys: impl IntoIterator<Item = String>) -> Self {
        self.cancel_timers.extend(keys);
        self
    }

    /// Borrow the events produced by this decision.
    pub fn events(&self) -> &NonEmpty<E> {
        &self.events
    }

    /// Borrow the effects produced by this decision.
    pub fn effects(&self) -> &[F] {
        &self.effects
    }

    /// Borrow the timers produced by this decision.
    pub fn timers(&self) -> &[Timer<I>] {
        &self.timers
    }

    /// Borrow the timer cancellation keys.
    pub fn canceled_timers(&self) -> &[String] {
        &self.cancel_timers
    }

    /// Consume the decision into its parts.
    pub(crate) fn into_parts(self) -> (NonEmpty<E>, Vec<F>, Vec<Timer<I>>, Vec<String>) {
        (self.events, self.effects, self.timers, self.cancel_timers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Decision tests
    // =========================================================================

    #[test]
    fn decision_single_event() {
        let decision = Decision::<&str, i32, ()>::event("created")
            .with_effect(1)
            .with_effect(2);

        assert_eq!(decision.events().len(), 1);
        assert_eq!(decision.events().first(), &"created");
        assert_eq!(decision.effects(), &[1, 2]);
        assert!(decision.timers().is_empty());
    }

    #[test]
    fn decision_from_events() {
        let events = NonEmpty::collect(["a", "b", "c"]).unwrap();
        let decision = Decision::<&str, (), ()>::from_events(events);

        let collected: Vec<_> = decision.events().iter().copied().collect();
        assert_eq!(collected, vec!["a", "b", "c"]);
        assert!(decision.effects().is_empty());
        assert!(decision.timers().is_empty());
    }

    #[test]
    fn decision_try_from_iter_some() {
        let decision: Option<Decision<&str, (), ()>> = Decision::try_from_iter(["a", "b"]);
        assert!(decision.is_some());
    }

    #[test]
    fn decision_try_from_iter_none() {
        let decision: Option<Decision<&str, (), ()>> = Decision::try_from_iter(std::iter::empty());
        assert!(decision.is_none());
    }

    #[test]
    fn decision_with_effects_batch() {
        let decision = Decision::<&str, i32, ()>::event("created").with_effects([1, 2, 3]);

        assert_eq!(decision.effects(), &[1, 2, 3]);
    }

    #[test]
    fn decision_with_timer() {
        let decision = Decision::<&str, (), &str>::event("created")
            .with_timer_after(Duration::from_secs(60), "timeout");

        assert_eq!(decision.timers().len(), 1);
        assert_eq!(decision.timers()[0].input, "timeout");
    }

    #[test]
    fn decision_with_timer_at() {
        let fire_at = OffsetDateTime::now_utc() + Duration::from_secs(3600);
        let decision =
            Decision::<&str, (), &str>::event("created").with_timer_at(fire_at, "reminder");

        assert_eq!(decision.timers().len(), 1);
        assert_eq!(decision.timers()[0].input, "reminder");
        assert_eq!(decision.timers()[0].fire_at, fire_at);
    }

    #[test]
    fn decision_with_timer_direct() {
        let timer = Timer::after(Duration::from_secs(60), "timeout").with_key("my-timer");
        let decision = Decision::<&str, (), &str>::event("created").with_timer(timer);

        assert_eq!(decision.timers().len(), 1);
        assert_eq!(decision.timers()[0].key.as_deref(), Some("my-timer"));
    }

    #[test]
    fn decision_with_timers_batch() {
        let timers = vec![
            Timer::after(Duration::from_secs(60), "t1"),
            Timer::after(Duration::from_secs(120), "t2"),
        ];
        let decision = Decision::<&str, (), &str>::event("created").with_timers(timers);

        assert_eq!(decision.timers().len(), 2);
    }

    #[test]
    fn decision_with_multiple_timers() {
        let decision = Decision::<&str, (), &str>::event("created")
            .with_timer_after(Duration::from_secs(60), "timeout1")
            .with_timer_after(Duration::from_secs(120), "timeout2");

        assert_eq!(decision.timers().len(), 2);
    }

    #[test]
    fn decision_cancel_timer() {
        let decision = Decision::<&str, (), ()>::event("completed").cancel_timer("payment-timeout");

        assert_eq!(decision.canceled_timers(), &["payment-timeout"]);
    }

    #[test]
    fn decision_cancel_timers_batch() {
        let decision = Decision::<&str, (), ()>::event("completed")
            .cancel_timers(["timer-1".to_string(), "timer-2".to_string()]);

        assert_eq!(decision.canceled_timers().len(), 2);
    }

    #[test]
    fn decision_with_effect_and_timer() {
        let decision = Decision::<&str, &str, &str>::event("created")
            .with_effect("send_email")
            .with_timer_after(Duration::from_secs(60), "timeout");

        assert_eq!(decision.effects().len(), 1);
        assert_eq!(decision.timers().len(), 1);
    }

    #[test]
    fn decision_into_parts() {
        let decision = Decision::<&str, i32, ()>::event("created")
            .with_effect(42)
            .cancel_timer("old-timer");
        let (events, effects, timers, cancel_timers) = decision.into_parts();

        assert_eq!(events.first(), &"created");
        assert_eq!(effects, vec![42]);
        assert!(timers.is_empty());
        assert_eq!(cancel_timers, vec!["old-timer"]);
    }

    // =========================================================================
    // WorkflowId tests
    // =========================================================================

    #[test]
    fn workflow_id_new() {
        let id = WorkflowId::new("order-123");
        assert_eq!(id.as_str(), "order-123");
        assert_eq!(format!("{}", id), "order-123");
    }

    #[test]
    fn workflow_id_into_inner() {
        let id = WorkflowId::new("order-123");
        assert_eq!(id.into_inner(), "order-123");
    }

    #[test]
    fn workflow_id_from_string() {
        let id: WorkflowId = String::from("order-456").into();
        assert_eq!(id.as_str(), "order-456");
    }

    #[test]
    fn workflow_id_from_str() {
        let id: WorkflowId = "order-789".into();
        assert_eq!(id.as_str(), "order-789");
    }

    #[test]
    fn workflow_id_equality() {
        let id1 = WorkflowId::new("same");
        let id2 = WorkflowId::new("same");
        let id3 = WorkflowId::new("different");

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    // =========================================================================
    // WorkflowRef tests
    // =========================================================================

    #[test]
    fn workflow_ref_new() {
        let wf = WorkflowRef::new("order", "ord-123");
        assert_eq!(wf.workflow_type(), "order");
        assert_eq!(wf.workflow_id().as_str(), "ord-123");
    }

    #[test]
    fn workflow_ref_display() {
        let wf = WorkflowRef::new("order", "ord-123");
        assert_eq!(format!("{}", wf), "order:ord-123");
    }

    #[test]
    fn workflow_ref_into_workflow_id() {
        let wf = WorkflowRef::new("order", "ord-123");
        let id = wf.into_workflow_id();
        assert_eq!(id.as_str(), "ord-123");
    }

    #[test]
    fn workflow_ref_from_tuple_static_str() {
        let wf: WorkflowRef = ("order", "ord-123").into();
        assert_eq!(wf.workflow_type(), "order");
        assert_eq!(wf.workflow_id().as_str(), "ord-123");
    }

    #[test]
    fn workflow_ref_from_tuple_string_id() {
        let wf: WorkflowRef = (String::from("order"), WorkflowId::new("ord-123")).into();
        assert_eq!(wf.workflow_type(), "order");
        assert_eq!(wf.workflow_id().as_str(), "ord-123");
    }

    #[test]
    fn workflow_ref_equality() {
        let wf1 = WorkflowRef::new("order", "ord-123");
        let wf2 = WorkflowRef::new("order", "ord-123");
        let wf3 = WorkflowRef::new("order", "ord-456");
        let wf4 = WorkflowRef::new("inventory", "ord-123");

        assert_eq!(wf1, wf2);
        assert_ne!(wf1, wf3); // different id
        assert_ne!(wf1, wf4); // different type
    }
}
