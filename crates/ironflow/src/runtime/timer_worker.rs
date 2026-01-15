//! Timer worker for processing scheduled inputs from the timers table.

use std::sync::Arc;

use tokio::sync::watch;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use super::RuntimeConfig;
use super::registry::WorkflowRuntime;
use crate::store::{OutboxStore, Store, WorkflowQueryStore};

/// Timer worker that polls the timers table for due timers.
///
/// The worker runs in a loop, claiming timers one at a time and
/// processing them sequentially. Timer inputs are routed directly to the
/// workflow's decider.
///
/// # Timer Data Format
///
/// Timers are stored in a dedicated `ironflow.timers` table with:
/// - `fire_at`: When the timer should fire
/// - `input`: The workflow input to deliver (JSON)
/// - `key`: Optional deduplication key
///
/// # Lifecycle
///
/// 1. Poll for due timers at `timer_poll_interval`
/// 2. Claim timer (atomic lock with timeout)
/// 3. Look up entry by `workflow_type`
/// 4. Execute input as a new decision
/// 5. Mark processed or record failure
/// 6. Repeat until shutdown signal
pub(crate) struct TimerWorker<S, O>
where
    S: Store + WorkflowQueryStore,
    O: OutboxStore,
{
    runtime: Arc<WorkflowRuntime<S>>,
    outbox: O,
    config: RuntimeConfig,
    worker_id: String,
}

impl<S, O> TimerWorker<S, O>
where
    S: Store + WorkflowQueryStore,
    O: OutboxStore,
{
    /// Create a new timer worker.
    pub fn new(
        runtime: Arc<WorkflowRuntime<S>>,
        outbox: O,
        config: RuntimeConfig,
        worker_id: String,
    ) -> Self {
        Self {
            runtime,
            outbox,
            config,
            worker_id,
        }
    }

    /// Run the timer worker until shutdown signal.
    ///
    /// The worker polls for due timers at `timer_poll_interval` and processes
    /// them one at a time. When the shutdown receiver signals, the worker
    /// finishes processing the current timer (if any) and exits.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut poll_interval = interval(self.config.timer_poll_interval);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        info!(worker_id = %self.worker_id, "Timer worker started");

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    if let Err(e) = self.process_one().await {
                        error!(error = %e, "Error processing timer");
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(worker_id = %self.worker_id, "Timer worker shutting down");
                        break;
                    }
                }
            }
        }
    }

    /// Try to claim and process one timer.
    async fn process_one(&self) -> crate::Result<()> {
        let timer = self
            .outbox
            .claim_timer(
                &self.worker_id,
                self.config.timer_lock_duration,
                self.config.retry_policy.max_attempts,
            )
            .await?;

        let Some(timer) = timer else {
            return Ok(()); // No timers due
        };

        debug!(
            timer_id = %timer.id,
            workflow = %timer.workflow,
            attempt = timer.attempts + 1,
            "Processing timer"
        );

        // The input is stored directly in the payload (from the timers table)
        let timer_id = timer.id;
        let attempts = timer.attempts;
        let input = timer.payload;

        // Execute the input as a new decision
        match self
            .runtime
            .service()
            .execute_dynamic(timer.workflow.workflow_type(), &input)
            .await
        {
            Ok(_) => {
                // Timer processed (either executed or skipped if workflow completed)
                self.outbox.mark_timer_processed(timer_id).await?;
                debug!(timer_id = %timer_id, "Timer processed successfully");
            }
            Err(e) => {
                let error_msg = format!("Failed to execute timer input: {}", e);
                warn!(timer_id = %timer_id, error = %error_msg, "Timer execution failed");
                let backoff = self.config.retry_policy.backoff_duration(attempts + 1);
                self.outbox
                    .record_timer_failure(timer_id, &error_msg, backoff)
                    .await?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Tests would go here but require mock implementations
    // Will be added in integration tests
}
