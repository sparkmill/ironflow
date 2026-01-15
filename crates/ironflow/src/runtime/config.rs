//! Runtime configuration.

use std::time::Duration;

use crate::effect::RetryPolicy;

/// Configuration for the effect execution runtime.
///
/// Controls polling intervals, timeouts, retry behavior, and worker concurrency.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use ironflow::runtime::RuntimeConfig;
///
/// let config = RuntimeConfig {
///     effect_poll_interval: Duration::from_millis(100),
///     timer_poll_interval: Duration::from_secs(1),
///     timer_lock_duration: Duration::from_secs(300),
///     shutdown_timeout: Duration::from_secs(30),
///     effect_workers: 4,  // Process up to 4 effects in parallel
///     timer_workers: 2,   // Process up to 2 timers in parallel
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// How often to poll for immediate effects.
    ///
    /// Lower values reduce latency but increase store load.
    /// Default: 100ms.
    pub effect_poll_interval: Duration,

    /// How often to poll for due timers.
    ///
    /// Timer precision is limited by this interval.
    /// Default: 1 second.
    pub timer_poll_interval: Duration,

    /// How long to hold a lock on an effect while processing.
    ///
    /// Should be longer than the longest expected effect execution.
    /// If a worker crashes, the effect becomes available after this duration.
    /// Default: 5 minutes.
    pub effect_lock_duration: Duration,

    /// How long to hold a lock on a timer while processing.
    ///
    /// Should be longer than the longest expected workflow execution.
    /// Default: 5 minutes.
    pub timer_lock_duration: Duration,

    /// Maximum time to wait for in-flight effects during shutdown.
    ///
    /// After this timeout, the runtime will force stop.
    /// Default: 30 seconds.
    pub shutdown_timeout: Duration,

    /// Retry policy for transient effect failures.
    ///
    /// Controls exponential backoff and max attempts before dead letter.
    pub retry_policy: RetryPolicy,

    /// Worker identifier for distributed coordination.
    ///
    /// Used in `locked_by` field to identify which worker holds a lock.
    /// If `None`, a UUID is generated at runtime startup.
    pub worker_id: Option<String>,

    /// Number of effect workers to spawn.
    ///
    /// Each worker polls the outbox independently and processes effects
    /// in parallel. Workers coordinate via `FOR UPDATE SKIP LOCKED` to
    /// avoid processing the same effect twice.
    ///
    /// Increase this to improve throughput when effect handlers are slow
    /// (e.g., calling external APIs). Default: 1.
    pub effect_workers: usize,

    /// Number of timer workers to spawn.
    ///
    /// Each worker polls for due timers independently. Typically 1-2
    /// workers are sufficient since timers are less frequent than effects.
    /// Default: 1.
    pub timer_workers: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            effect_poll_interval: Duration::from_millis(100),
            timer_poll_interval: Duration::from_secs(1),
            effect_lock_duration: Duration::from_secs(300), // 5 minutes
            timer_lock_duration: Duration::from_secs(300),  // 5 minutes
            shutdown_timeout: Duration::from_secs(30),
            retry_policy: RetryPolicy::default(),
            worker_id: None,
            effect_workers: 1,
            timer_workers: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = RuntimeConfig::default();

        assert_eq!(config.effect_poll_interval, Duration::from_millis(100));
        assert_eq!(config.timer_poll_interval, Duration::from_secs(1));
        assert_eq!(config.effect_lock_duration, Duration::from_secs(300));
        assert_eq!(config.timer_lock_duration, Duration::from_secs(300));
        assert_eq!(config.shutdown_timeout, Duration::from_secs(30));
        assert!(config.worker_id.is_none());
        assert_eq!(config.effect_workers, 1);
        assert_eq!(config.timer_workers, 1);
    }
}
