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
use time::OffsetDateTime;

/// A scheduled timer that will deliver an input to the workflow at a future time.
///
/// Timers are created in `Workflow::decide()` and stored in a dedicated table.
/// When the scheduled time arrives, the `TimerWorker` extracts the input and
/// routes it to the workflow via `Decider::execute()`.
///
/// # Timer Keys
///
/// Timers can optionally have a `key` for deduplication. If you schedule a timer
/// with the same key for the same workflow instance, it replaces the existing timer.
/// This is useful for "reschedule" semantics (e.g., extending a timeout).
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use ironflow::Timer;
///
/// // Timer that fires in 1 hour
/// let timer = Timer::after(Duration::from_secs(3600), "PaymentTimeout");
///
/// // Timer with a key for deduplication
/// let timer = Timer::after(Duration::from_secs(3600), "PaymentTimeout")
///     .with_key("payment-timeout");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timer<I> {
    /// When the timer should fire (UTC).
    pub fire_at: OffsetDateTime,

    /// The input to deliver when the timer fires.
    pub input: I,

    /// Optional key for deduplication/replacement.
    ///
    /// If set, scheduling a timer with the same key for the same workflow
    /// instance will replace any existing timer with that key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl<I> Timer<I> {
    /// Create a timer that fires at a specific time.
    ///
    /// # Arguments
    ///
    /// * `fire_at` - When the timer should fire (UTC)
    /// * `input` - The input to deliver when the timer fires
    pub fn at(fire_at: OffsetDateTime, input: I) -> Self {
        Self {
            fire_at,
            input,
            key: None,
        }
    }

    /// Create a timer that fires after a delay from now.
    ///
    /// # Arguments
    ///
    /// * `delay` - How long to wait before firing
    /// * `input` - The input to deliver when the timer fires
    pub fn after(delay: Duration, input: I) -> Self {
        let delay_time = time::Duration::new(delay.as_secs() as i64, delay.subsec_nanos() as i32);
        Self::at(OffsetDateTime::now_utc() + delay_time, input)
    }

    /// Set a key for deduplication/replacement.
    ///
    /// If a timer with this key already exists for the same workflow instance,
    /// scheduling this timer will replace the existing one.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use ironflow::Timer;
    ///
    /// // If user extends their session, reschedule the timeout
    /// let timer = Timer::after(Duration::from_secs(1800), "SessionTimeout")
    ///     .with_key("session-timeout");
    /// ```
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
    fn timer_at() {
        let fire_at = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let timer = Timer::at(fire_at, "TestInput");

        assert_eq!(timer.fire_at, fire_at);
        assert_eq!(timer.input, "TestInput");
        assert!(timer.key.is_none());
    }

    #[test]
    fn timer_after() {
        let before = OffsetDateTime::now_utc();
        let timer = Timer::after(Duration::from_secs(3600), "TestInput");
        let after = OffsetDateTime::now_utc();

        // fire_at should be approximately 1 hour from now
        assert!(timer.fire_at >= before + time::Duration::hours(1));
        assert!(timer.fire_at <= after + time::Duration::hours(1));
    }

    #[test]
    fn timer_with_key() {
        let timer = Timer::after(Duration::from_secs(60), "TestInput").with_key("my-timer-key");

        assert_eq!(timer.key(), Some("my-timer-key"));
    }

    #[test]
    fn timer_serialization() {
        let timer = Timer::at(
            OffsetDateTime::from_unix_timestamp(1704067200).unwrap(), // 2024-01-01 00:00:00 UTC
            "TestInput",
        );

        let json = serde_json::to_value(&timer).unwrap();
        assert!(json.get("fire_at").is_some());
        assert_eq!(json["input"], "TestInput");
        assert!(json.get("key").is_none()); // Skipped when None
    }

    #[test]
    fn timer_serialization_with_key() {
        let timer = Timer::after(Duration::from_secs(60), "TestInput").with_key("my-key");

        let json = serde_json::to_value(&timer).unwrap();
        assert_eq!(json["key"], "my-key");
    }
}
