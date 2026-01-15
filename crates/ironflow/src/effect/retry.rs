//! Retry policy configuration for effect execution.

use std::time::Duration;

use time::OffsetDateTime;

/// Configuration for effect retry behavior with exponential backoff.
///
/// When an effect handler returns [`EffectError::Transient`](super::EffectError::Transient),
/// the effect is retried according to this policy. After `max_attempts` failures,
/// the effect is moved to the dead letter queue.
///
/// # Backoff Calculation
///
/// The delay before retry N is: `min(base_delay * 2^(N-1), max_delay)`
///
/// With defaults (base=1s, max=300s):
/// - Attempt 2: 1s delay
/// - Attempt 3: 2s delay
/// - Attempt 4: 4s delay
/// - Attempt 5: 8s delay (then dead letter)
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use ironflow::effect::RetryPolicy;
///
/// let policy = RetryPolicy::default();
/// assert_eq!(policy.max_attempts, 5);
///
/// // Custom policy for high-value operations
/// let strict = RetryPolicy {
///     max_attempts: 10,
///     base_delay: Duration::from_millis(500),
///     max_delay: Duration::from_secs(60),
/// };
/// ```
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of attempts before moving to dead letter queue.
    ///
    /// Includes the initial attempt. Default: 5 (1 initial + 4 retries).
    pub max_attempts: u32,

    /// Base delay for exponential backoff.
    ///
    /// The delay doubles with each retry. Default: 1 second.
    pub base_delay: Duration,

    /// Maximum delay between retries.
    ///
    /// Caps the exponential growth. Default: 5 minutes (300 seconds).
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(300),
        }
    }
}

impl RetryPolicy {
    /// Calculate when the next attempt should occur.
    ///
    /// Returns the timestamp for `locked_until` based on exponential backoff.
    ///
    /// # Arguments
    ///
    /// * `attempt` - The attempt number that just failed (1-based)
    pub fn next_attempt_at(&self, attempt: u32) -> OffsetDateTime {
        let delay = self.delay_for_attempt(attempt);
        OffsetDateTime::now_utc() + delay
    }

    /// Calculate the delay duration for a given attempt.
    ///
    /// # Arguments
    ///
    /// * `attempt` - The attempt number that just failed (1-based)
    pub fn delay_for_attempt(&self, attempt: u32) -> time::Duration {
        // Exponential backoff: base * 2^(attempt-1), capped at max
        let multiplier = 2u32.saturating_pow(attempt.saturating_sub(1));
        let delay = self.base_delay.saturating_mul(multiplier);
        let capped = delay.min(self.max_delay);

        // Convert std::time::Duration to time::Duration
        time::Duration::new(capped.as_secs() as i64, capped.subsec_nanos() as i32)
    }

    /// Returns `true` if another retry should be attempted.
    ///
    /// # Arguments
    ///
    /// * `attempt` - The attempt number that just failed (1-based)
    pub fn should_retry(&self, attempt: u32) -> bool {
        attempt < self.max_attempts
    }

    /// Calculate the backoff duration for a given attempt.
    ///
    /// Returns `std::time::Duration` for use with async runtimes and stores.
    ///
    /// # Arguments
    ///
    /// * `attempt` - The attempt number that just failed (1-based)
    pub fn backoff_duration(&self, attempt: u32) -> Duration {
        // Exponential backoff: base * 2^(attempt-1), capped at max
        let multiplier = 2u32.saturating_pow(attempt.saturating_sub(1));
        let delay = self.base_delay.saturating_mul(multiplier);
        delay.min(self.max_delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy() {
        let policy = RetryPolicy::default();

        assert_eq!(policy.max_attempts, 5);
        assert_eq!(policy.base_delay, Duration::from_secs(1));
        assert_eq!(policy.max_delay, Duration::from_secs(300));
    }

    #[test]
    fn exponential_backoff() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(300),
        };

        // Attempt 1 fails -> delay before attempt 2
        assert_eq!(
            policy.delay_for_attempt(1),
            time::Duration::seconds(1) // 1 * 2^0 = 1
        );

        // Attempt 2 fails -> delay before attempt 3
        assert_eq!(
            policy.delay_for_attempt(2),
            time::Duration::seconds(2) // 1 * 2^1 = 2
        );

        // Attempt 3 fails -> delay before attempt 4
        assert_eq!(
            policy.delay_for_attempt(3),
            time::Duration::seconds(4) // 1 * 2^2 = 4
        );

        // Attempt 4 fails -> delay before attempt 5
        assert_eq!(
            policy.delay_for_attempt(4),
            time::Duration::seconds(8) // 1 * 2^3 = 8
        );
    }

    #[test]
    fn backoff_capped_at_max() {
        let policy = RetryPolicy {
            max_attempts: 20,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        };

        // 1 * 2^9 = 512, but capped at 60
        assert_eq!(policy.delay_for_attempt(10), time::Duration::seconds(60));
    }

    #[test]
    fn should_retry() {
        let policy = RetryPolicy {
            max_attempts: 3,
            ..Default::default()
        };

        assert!(policy.should_retry(1)); // Attempt 1 failed, retry
        assert!(policy.should_retry(2)); // Attempt 2 failed, retry
        assert!(!policy.should_retry(3)); // Attempt 3 failed, dead letter
    }
}
