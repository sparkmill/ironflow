//! Effect worker for processing immediate effects from the outbox.

use std::sync::Arc;

use tokio::sync::watch;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use super::RuntimeConfig;
use super::registry::WorkflowRuntime;
use crate::effect::EffectContext;
use crate::store::{OutboxEffect, OutboxStore, Store, WorkflowQueryStore};

/// Effect worker that polls the outbox for immediate effects.
///
/// The worker runs in a loop, claiming effects one at a time and
/// processing them sequentially. Results from handlers can route
/// back as new workflow inputs.
///
/// # Lifecycle
///
/// 1. Poll for available effect at `effect_poll_interval`
/// 2. Claim effect (atomic lock with timeout)
/// 3. Look up handler by `workflow_type`
/// 4. Call handler with effect payload
/// 5. If handler returns `Some(input)`, execute it as a new decision
/// 6. Mark processed or record failure with backoff
/// 7. Repeat until shutdown signal
pub(crate) struct EffectWorker<S, O>
where
    S: Store + WorkflowQueryStore,
    O: OutboxStore,
{
    runtime: Arc<WorkflowRuntime<S>>,
    outbox: O,
    config: RuntimeConfig,
    worker_id: String,
}

impl<S, O> EffectWorker<S, O>
where
    S: Store + WorkflowQueryStore,
    O: OutboxStore,
{
    /// Create a new effect worker.
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

    /// Run the effect worker until shutdown signal.
    ///
    /// The worker polls for effects at `effect_poll_interval` and processes
    /// them one at a time. When the shutdown receiver signals, the worker
    /// finishes processing the current effect (if any) and exits.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut poll_interval = interval(self.config.effect_poll_interval);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        info!(worker_id = %self.worker_id, "Effect worker started");

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    if let Err(e) = self.process_one().await {
                        error!(error = %e, "Error processing effect");
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(worker_id = %self.worker_id, "Effect worker shutting down");
                        break;
                    }
                }
            }
        }
    }

    /// Try to claim and process one effect.
    async fn process_one(&self) -> crate::Result<()> {
        let effect = self
            .outbox
            .claim_effect(
                &self.worker_id,
                self.config.effect_lock_duration,
                self.config.retry_policy.max_attempts,
            )
            .await?;

        let Some(effect) = effect else {
            return Ok(()); // No effects available
        };

        debug!(
            effect_id = %effect.id,
            workflow = %effect.workflow,
            attempt = effect.attempts + 1,
            "Processing effect"
        );

        // Look up the handler by workflow type
        let Some((_, entry)) = self
            .runtime
            .service()
            .get_entry(effect.workflow.workflow_type())
        else {
            // Unknown workflow type - this is a permanent error
            let error_msg = format!("Unknown workflow type: {}", effect.workflow.workflow_type());
            warn!(effect_id = %effect.id, error = %error_msg, "Dead letter: unknown workflow type");
            self.outbox
                .record_permanent_failure(
                    effect.id,
                    &error_msg,
                    self.config.retry_policy.max_attempts,
                )
                .await?;
            return Ok(());
        };

        // Build the effect context
        let ctx = EffectContext::new(
            effect.id,
            effect.workflow.clone(),
            effect.attempts + 1,
            effect.created_at,
        );

        // Execute the effect handler
        let result = entry.handle_effect(effect.payload.clone(), &ctx).await;

        match result {
            Ok(maybe_input) => {
                // Route result back as new input if present
                if let Some(input_json) = maybe_input {
                    debug!(
                        effect_id = %effect.id,
                        workflow = %effect.workflow,
                        "Routing effect result as new input"
                    );
                    // NOTE: This is not atomic with `mark_processed`; failures can lead to retries.
                    if let Err(e) = self
                        .runtime
                        .service()
                        .execute_dynamic(effect.workflow.workflow_type(), &input_json)
                        .await
                    {
                        // Routing failed - effect will be retried (handler must be idempotent)
                        let error_msg = format!("Failed to route result: {}", e);
                        warn!(effect_id = %effect.id, error = %error_msg, "Result routing failed");
                        self.record_failure_with_backoff(&effect, &error_msg)
                            .await?;
                        return Ok(());
                    }
                }

                // Mark as processed
                self.outbox.mark_processed(effect.id).await?;
                debug!(effect_id = %effect.id, "Effect processed successfully");
            }
            Err(effect_error) => {
                let error_msg = effect_error.to_string();
                let new_attempts = effect.attempts + 1;

                if new_attempts >= self.config.retry_policy.max_attempts {
                    warn!(
                        effect_id = %effect.id,
                        error = %error_msg,
                        attempts = new_attempts,
                        max_attempts = self.config.retry_policy.max_attempts,
                        "Effect exceeded max retries, moving to dead letter"
                    );
                    self.outbox
                        .record_permanent_failure(
                            effect.id,
                            &error_msg,
                            self.config.retry_policy.max_attempts,
                        )
                        .await?;
                } else {
                    debug!(
                        effect_id = %effect.id,
                        error = %error_msg,
                        attempts = new_attempts,
                        "Effect failed, will retry"
                    );
                    self.record_failure_with_backoff(&effect, &error_msg)
                        .await?;
                }
            }
        }

        Ok(())
    }

    /// Record a failure with exponential backoff delay.
    async fn record_failure_with_backoff(
        &self,
        effect: &OutboxEffect,
        error: &str,
    ) -> crate::Result<()> {
        let backoff = self
            .config
            .retry_policy
            .backoff_duration(effect.attempts + 1);
        self.outbox.record_failure(effect.id, error, backoff).await
    }
}

#[cfg(test)]
mod tests {
    // Tests would go here but require mock implementations
    // Will be added in integration tests
}
