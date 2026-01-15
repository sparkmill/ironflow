//! Effect execution context with correlation and idempotency metadata.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::WorkflowRef;

/// Context provided to effect handlers during execution.
///
/// Contains metadata for correlation (routing results back to the workflow)
/// and idempotency (safe retries with external services).
///
/// # Idempotency
///
/// Use [`idempotency_key()`](Self::idempotency_key) when calling external APIs
/// that support idempotency keys. The key is stable across retries but unique
/// per effect instance.
///
/// # Example
///
/// ```ignore
/// async fn handle(&self, effect: &MyEffect, ctx: &EffectContext) -> Result<Option<MyInput>, MyError> {
///     // Use idempotency key for external API calls
///     let result = self.payment_client
///         .charge(amount, ctx.idempotency_key())
///         .await?;
///
///     // Use workflow_id for routing result back
///     Ok(Some(MyInput::PaymentResult {
///         order_id: ctx.workflow.workflow_id().to_string(),
///         success: result.success,
///     }))
/// }
/// ```
#[derive(Debug, Clone)]
pub struct EffectContext {
    /// Unique identifier for this effect instance (UUID v7).
    ///
    /// Used as the correlation ID for tracking and routing.
    pub effect_id: Uuid,

    /// The workflow this effect belongs to.
    pub workflow: WorkflowRef,

    /// Current attempt number (1-based).
    ///
    /// First execution is attempt 1, first retry is attempt 2, etc.
    pub attempt: u32,

    /// When this effect was first created/enqueued.
    pub created_at: OffsetDateTime,
}

impl EffectContext {
    /// Create a new effect context.
    pub fn new(
        effect_id: Uuid,
        workflow: WorkflowRef,
        attempt: u32,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            effect_id,
            workflow,
            attempt,
            created_at,
        }
    }

    /// Get the idempotency key for external service calls.
    ///
    /// Format: `{workflow_type}:{workflow_id}:{effect_id}`
    ///
    /// This key is:
    /// - **Stable across retries** — same key for all attempts of the same effect
    /// - **Unique per effect** — different effects have different keys
    ///
    /// Use this when calling APIs that support idempotency keys (Stripe, etc.).
    pub fn idempotency_key(&self) -> String {
        format!("{}:{}", self.workflow, self.effect_id)
    }

    /// Returns `true` if this is a retry (attempt > 1).
    pub fn is_retry(&self) -> bool {
        self.attempt > 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> EffectContext {
        EffectContext::new(
            Uuid::nil(),
            WorkflowRef::new("order", "ord-123"),
            1,
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn idempotency_key_format() {
        let ctx = test_context();
        let key = ctx.idempotency_key();

        assert!(key.starts_with("order:ord-123:"));
        assert!(key.contains(&Uuid::nil().to_string()));
    }

    #[test]
    fn is_retry() {
        let mut ctx = test_context();

        assert!(!ctx.is_retry());

        ctx.attempt = 2;
        assert!(ctx.is_retry());
    }
}
