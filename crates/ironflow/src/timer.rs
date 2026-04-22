//! Timer types for scheduling future workflow inputs.
//!
//! Timers allow workflows to schedule inputs to be delivered at a future time.
//! Unlike effects (which execute side effects immediately), timers simply
//! deliver an input to the workflow's `decide` function when they fire.
//!
//! # Timers vs Effects
//!
//! | Aspect | Effect | Timer |
//! |--------|--------|-------|
//! | When | Execute now | Execute at scheduled time |
//! | What | Side effect (API call, email, etc.) | Deliver input to workflow |
//! | Handler | `EffectHandler` processes it | `TimerWorker` routes to `Decider` |
//! | Result | May return input to route back | Always delivers its embedded input |
//! | Failure | Transient (retry) or Permanent (dead letter) | Input execution may fail |
//!
//! # Ordering Guarantees
//!
//! When a timer fires, its input is routed through `Decider::execute()`, which
//! acquires an advisory lock on the workflow instance. This means:
//!
//! - Multiple timers firing for the same workflow are serialized
//! - Timer inputs are processed in the same way as external inputs
//! - The workflow sees consistent state when handling timer inputs
//!
//! # Example
//!
//! ```ignore
//! fn decide(now: OffsetDateTime, state: &OrderState, input: &OrderInput)
//!     -> Decision<OrderEvent, OrderEffect, OrderInput>
//! {
//!     match input {
//!         OrderInput::Create { order_id, .. } => {
//!             Decision::event(OrderEvent::Created { .. })
//!                 // Schedule a timeout 1 hour from now
//!                 .with_timer_after(
//!                     Duration::from_secs(3600),
//!                     OrderInput::PaymentTimeout { order_id: order_id.clone() }
//!                 )
//!         }
//!
//!         OrderInput::PaymentTimeout { order_id } => {
//!             if state.is_paid {
//!                 // Timer fired but order was already paid
//!                 Decision::event(OrderEvent::TimeoutIgnored)
//!             } else {
//!                 // Timer fired and order still unpaid - cancel it
//!                 Decision::event(OrderEvent::Cancelled { reason: "Payment timeout" })
//!             }
//!         }
//!         // ...
//!     }
//! }
//! ```

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A scheduled timer that will deliver an input to the workflow after a
/// delay. The actual fire time is computed DB-side as `now() + delay`
/// when the timer is written, so this type doesn't need to read the
/// wall clock and decide stays deterministic.
///
/// # Timer keys
///
/// Timers can optionally have a `key` for deduplication. Scheduling a
/// timer with the same key for the same workflow instance replaces the
/// existing one — useful for "reschedule" semantics like extending a
/// timeout.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use ironflow::Timer;
///
/// Timer::after(Duration::from_secs(3600), "PaymentTimeout")
///     .with_key("payment-timeout");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timer<I> {
    /// How long after persist time the timer should fire.
    pub delay: Duration,

    /// The input to deliver when the timer fires.
    pub input: I,

    /// Optional key for deduplication/replacement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl<I> Timer<I> {
    /// Create a timer that fires `delay` after it is persisted to the store.
    pub fn after(delay: Duration, input: I) -> Self {
        Self {
            delay,
            input,
            key: None,
        }
    }

    /// Set a key for deduplication/replacement.
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Returns the timer key, if set.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_after_carries_delay_and_does_not_read_clock() {
        // `after` must not read the wall clock — fire_at is computed
        // DB-side as now() + delay at persist time. The constructor is
        // a pure data carrier.
        let delay = Duration::from_secs(3600);
        let timer = Timer::after(delay, "TestInput");

        assert_eq!(timer.delay, delay);
        assert_eq!(timer.input, "TestInput");
        assert!(timer.key.is_none());
    }

    #[test]
    fn timer_with_key() {
        let timer = Timer::after(Duration::from_secs(60), "TestInput").with_key("my-timer-key");

        assert_eq!(timer.key(), Some("my-timer-key"));
    }

    #[test]
    fn timer_serialization() {
        let timer = Timer::after(Duration::from_secs(60), "TestInput").with_key("my-key");

        let json = serde_json::to_value(&timer).unwrap();
        assert!(json["delay"].is_object(), "serialized JSON: {json}");
        assert_eq!(json["input"], "TestInput");
        assert_eq!(json["key"], "my-key");
    }
}
