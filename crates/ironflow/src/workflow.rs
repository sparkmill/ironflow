//! Core workflow traits and types.

use std::time::Duration;

use nonempty::NonEmpty;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
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
///     type Rejection = OrderRejection;
///
///     const TYPE: &'static str = "order";
///
///     fn evolve(state: Self::State, event: Self::Event) -> Self::State { /* ... */ }
///
///     fn decide(_now, state, input)
///         -> Decision<Self::Event, Self::Effect, Self::Input, Self::Rejection>
///     {
///         match input {
///             OrderInput::Create { .. } => Decision::accept(OrderEvent::Created { /* ... */ })
///                 .with_effect(OrderEffect::SendConfirmation),
///             OrderInput::Cancel { .. } if state.is_shipped() => {
///                 Decision::reject(OrderRejection::AlreadyShipped)
///             }
///             OrderInput::Ship { tracking, .. } => {
///                 Decision::accept(OrderEvent::Shipped { tracking: tracking.clone() })
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

    /// Typed rejection payload returned when [`decide`](Self::decide) chooses
    /// [`Decision::Reject`].
    ///
    /// Common choices:
    /// - A domain enum for structured rejections: `OrderRejection::AlreadyPaid`.
    /// - `Cow<'static, str>` for simple string reasons.
    /// - [`Never`] if the workflow can never reject.
    type Rejection: Serialize + Send + std::fmt::Debug;

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
    /// Returns a [`Decision`]: [`Decision::Accept`] with events/effects/timers,
    /// or [`Decision::Reject`] with a typed rejection that causes the input to
    /// be discarded without mutating state.
    fn decide(
        now: OffsetDateTime,
        state: &Self::State,
        input: &Self::Input,
    ) -> Decision<Self::Event, Self::Effect, Self::Input, Self::Rejection>;

    /// Check if the state is terminal. Terminal instances are marked
    /// completed in the store and silently drop further inputs. Defaults
    /// to never terminal.
    ///
    /// ```ignore
    /// fn is_terminal(state: &Self::State) -> bool {
    ///     matches!(state.status, OrderStatus::Completed | OrderStatus::Cancelled)
    /// }
    /// ```
    fn is_terminal(_state: &Self::State) -> bool {
        false
    }

    /// Optional at-most-one-active-workflow key. While the first workflow
    /// with a given key is active, further starts with the same key fail
    /// with [`Error::UniqueKeyConflict`](crate::Error::UniqueKeyConflict).
    /// The key releases once [`is_terminal`](Self::is_terminal) returns
    /// `true`.
    ///
    /// **Pair with [`is_terminal`](Self::is_terminal).** Without it the
    /// key is held forever.
    ///
    /// ```ignore
    /// fn unique_key(input: &Self::Input) -> Option<String> {
    ///     match input {
    ///         PaymentInput::Create { order_id, .. } => Some(format!("order-{order_id}")),
    ///         _ => None,
    ///     }
    /// }
    /// ```
    fn unique_key(_input: &Self::Input) -> Option<String> {
        None
    }
}

/// Uninhabited [`Workflow::Rejection`] type for workflows that can never reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Never {}

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// What [`Workflow::decide`] returns for an input.
///
/// `Accept` advances state via events and may enqueue effects/timers;
/// `Reject` discards the input with a typed rejection payload.
///
/// Use the fluent constructors [`accept`](Self::accept) and
/// [`reject`](Self::reject) together with `with_*` / `cancel_*` builders:
///
/// ```ignore
/// match (state, input) {
///     (State::Draft, Input::Create { .. }) => Decision::accept(Event::Created)
///         .with_effect(Effect::SendConfirmation)
///         .with_timer_after(Duration::from_secs(3600), Input::PaymentTimeout),
///     (State::Paid { .. }, Input::Pay { .. }) => {
///         Decision::reject(OrderRejection::AlreadyPaid)
///     }
///     // ...
/// }
/// ```
///
/// Builder methods applied to a `Reject` are silent no-ops — this keeps
/// fluent chaining ergonomic and only matters if you accidentally chain
/// `.with_effect(..)` after `Decision::reject(..)`, which is typically a
/// typo the author would spot immediately.
#[derive(Debug, Clone)]
pub enum Decision<E, F, I, R> {
    /// Input accepted. Events are appended, effects enqueued, timers
    /// scheduled/cancelled, and the workflow's state advances via
    /// [`Workflow::evolve`]. At least one event is required (enforced by
    /// [`NonEmpty`]).
    Accept {
        /// Events to append to the store.
        events: NonEmpty<E>,
        /// Effects to enqueue to the outbox.
        effects: Vec<F>,
        /// Timers to schedule.
        timers: Vec<Timer<I>>,
        /// Pending timers (by key) to cancel.
        cancel_timers: Vec<String>,
    },
    /// Input rejected with a typed payload. No state change.
    Reject(R),
}

impl<E, F, I, R> Decision<E, F, I, R> {
    /// Start an `Accept` from a single event. Chain `with_*`/`cancel_*`
    /// to add effects, timers, or cancellations.
    pub fn accept(event: E) -> Self {
        Self::Accept {
            events: NonEmpty::new(event),
            effects: Vec::new(),
            timers: Vec::new(),
            cancel_timers: Vec::new(),
        }
    }

    /// Start an `Accept` from a non-empty collection of events.
    pub fn accept_events(events: NonEmpty<E>) -> Self {
        Self::Accept {
            events,
            effects: Vec::new(),
            timers: Vec::new(),
            cancel_timers: Vec::new(),
        }
    }

    /// Try to start an `Accept` from an iterator of events. Returns
    /// `None` if the iterator is empty.
    pub fn try_accept(events: impl IntoIterator<Item = E>) -> Option<Self> {
        NonEmpty::collect(events).map(Self::accept_events)
    }

    /// Reject the input with a typed payload.
    pub fn reject(reason: R) -> Self {
        Self::Reject(reason)
    }

    /// Add an effect to an `Accept`. No-op on `Reject`.
    pub fn with_effect(mut self, effect: F) -> Self {
        if let Self::Accept { effects, .. } = &mut self {
            effects.push(effect);
        }
        self
    }

    /// Add multiple effects to an `Accept`. No-op on `Reject`.
    pub fn with_effects(mut self, effects_in: impl IntoIterator<Item = F>) -> Self {
        if let Self::Accept { effects, .. } = &mut self {
            effects.extend(effects_in);
        }
        self
    }

    /// Add a timer to an `Accept`. No-op on `Reject`.
    pub fn with_timer(mut self, timer: Timer<I>) -> Self {
        if let Self::Accept { timers, .. } = &mut self {
            timers.push(timer);
        }
        self
    }

    /// Add a timer that fires after `delay` from persist time (DB-side).
    /// No-op on `Reject`.
    pub fn with_timer_after(self, delay: Duration, input: I) -> Self {
        self.with_timer(Timer::after(delay, input))
    }

    /// Add multiple timers to an `Accept`. No-op on `Reject`.
    pub fn with_timers(mut self, timers_in: impl IntoIterator<Item = Timer<I>>) -> Self {
        if let Self::Accept { timers, .. } = &mut self {
            timers.extend(timers_in);
        }
        self
    }

    /// Cancel a pending timer by key. No-op on `Reject`.
    pub fn cancel_timer(mut self, key: impl Into<String>) -> Self {
        if let Self::Accept { cancel_timers, .. } = &mut self {
            cancel_timers.push(key.into());
        }
        self
    }

    /// Cancel multiple pending timers by key. No-op on `Reject`.
    pub fn cancel_timers(mut self, keys: impl IntoIterator<Item = String>) -> Self {
        if let Self::Accept { cancel_timers, .. } = &mut self {
            cancel_timers.extend(keys);
        }
        self
    }

    /// Returns `true` if this is an `Accept`.
    pub fn is_accept(&self) -> bool {
        matches!(self, Self::Accept { .. })
    }

    /// Returns `true` if this is a `Reject`.
    pub fn is_reject(&self) -> bool {
        matches!(self, Self::Reject(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Decision tests
    // =========================================================================

    // Test type alias to keep matches short and unambiguous about R.
    type D<E, F, I> = Decision<E, F, I, &'static str>;

    fn unwrap_accept<E, F, I, R>(
        d: Decision<E, F, I, R>,
    ) -> (NonEmpty<E>, Vec<F>, Vec<Timer<I>>, Vec<String>) {
        match d {
            Decision::Accept {
                events,
                effects,
                timers,
                cancel_timers,
            } => (events, effects, timers, cancel_timers),
            Decision::Reject(_) => panic!("expected Accept, got Reject"),
        }
    }

    #[test]
    fn decision_single_event() {
        let decision = D::<&str, i32, ()>::accept("created")
            .with_effect(1)
            .with_effect(2);
        let (events, effects, timers, _) = unwrap_accept(decision);

        assert_eq!(events.len(), 1);
        assert_eq!(events.first(), &"created");
        assert_eq!(effects, vec![1, 2]);
        assert!(timers.is_empty());
    }

    #[test]
    fn decision_accept_events() {
        let events = NonEmpty::collect(["a", "b", "c"]).unwrap();
        let decision = D::<&str, (), ()>::accept_events(events);
        let (events, effects, timers, _) = unwrap_accept(decision);

        let collected: Vec<_> = events.iter().copied().collect();
        assert_eq!(collected, vec!["a", "b", "c"]);
        assert!(effects.is_empty());
        assert!(timers.is_empty());
    }

    #[test]
    fn decision_try_accept_some() {
        let decision: Option<D<&str, (), ()>> = Decision::try_accept(["a", "b"]);
        assert!(decision.is_some_and(|d| d.is_accept()));
    }

    #[test]
    fn decision_try_accept_none() {
        let decision: Option<D<&str, (), ()>> = Decision::try_accept(std::iter::empty());
        assert!(decision.is_none());
    }

    #[test]
    fn decision_with_effects_batch() {
        let decision = D::<&str, i32, ()>::accept("created").with_effects([1, 2, 3]);
        let (_, effects, _, _) = unwrap_accept(decision);
        assert_eq!(effects, vec![1, 2, 3]);
    }

    #[test]
    fn decision_with_timer() {
        let decision = D::<&str, (), &str>::accept("created")
            .with_timer_after(Duration::from_secs(60), "timeout");
        let (_, _, timers, _) = unwrap_accept(decision);

        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].input, "timeout");
    }

    #[test]
    fn decision_with_timer_after_carries_delay() {
        let delay = Duration::from_secs(3600);
        let decision = D::<&str, (), &str>::accept("created").with_timer_after(delay, "reminder");
        let (_, _, timers, _) = unwrap_accept(decision);

        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].input, "reminder");
        assert_eq!(timers[0].delay, delay);
    }

    #[test]
    fn decision_with_timer_direct() {
        let timer = Timer::after(Duration::from_secs(60), "timeout").with_key("my-timer");
        let decision = D::<&str, (), &str>::accept("created").with_timer(timer);
        let (_, _, timers, _) = unwrap_accept(decision);

        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].key.as_deref(), Some("my-timer"));
    }

    #[test]
    fn decision_with_timers_batch() {
        let timers_in = vec![
            Timer::after(Duration::from_secs(60), "t1"),
            Timer::after(Duration::from_secs(120), "t2"),
        ];
        let decision = D::<&str, (), &str>::accept("created").with_timers(timers_in);
        let (_, _, timers, _) = unwrap_accept(decision);

        assert_eq!(timers.len(), 2);
    }

    #[test]
    fn decision_with_multiple_timers() {
        let decision = D::<&str, (), &str>::accept("created")
            .with_timer_after(Duration::from_secs(60), "timeout1")
            .with_timer_after(Duration::from_secs(120), "timeout2");
        let (_, _, timers, _) = unwrap_accept(decision);

        assert_eq!(timers.len(), 2);
    }

    #[test]
    fn decision_cancel_timer() {
        let decision = D::<&str, (), ()>::accept("completed").cancel_timer("payment-timeout");
        let (_, _, _, cancel_timers) = unwrap_accept(decision);

        assert_eq!(cancel_timers, vec!["payment-timeout".to_string()]);
    }

    #[test]
    fn decision_cancel_timers_batch() {
        let decision = D::<&str, (), ()>::accept("completed")
            .cancel_timers(["timer-1".to_string(), "timer-2".to_string()]);
        let (_, _, _, cancel_timers) = unwrap_accept(decision);

        assert_eq!(cancel_timers.len(), 2);
    }

    #[test]
    fn decision_with_effect_and_timer() {
        let decision = D::<&str, &str, &str>::accept("created")
            .with_effect("send_email")
            .with_timer_after(Duration::from_secs(60), "timeout");
        let (_, effects, timers, _) = unwrap_accept(decision);

        assert_eq!(effects.len(), 1);
        assert_eq!(timers.len(), 1);
    }

    #[test]
    fn decision_reject_carries_payload() {
        let decision: Decision<&str, (), (), &'static str> = Decision::reject("nope");
        assert!(decision.is_reject());
        match decision {
            Decision::Reject(reason) => assert_eq!(reason, "nope"),
            Decision::Accept { .. } => panic!("expected Reject"),
        }
    }

    #[test]
    fn builder_methods_are_noop_on_reject() {
        // with_effect and friends on a Reject should silently no-op —
        // the Reject variant must stay unchanged.
        let decision: Decision<&str, i32, (), &'static str> = Decision::reject("nope")
            .with_effect(1)
            .with_timer(Timer::after(Duration::from_secs(1), ()))
            .cancel_timer("k");
        assert!(decision.is_reject());
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
