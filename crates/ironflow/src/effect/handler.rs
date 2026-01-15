//! Effect handler trait for executing workflow side effects.

use async_trait::async_trait;

use super::context::EffectContext;
use crate::Workflow;
use tracing::warn;

/// No-op effect handler for workflows that never emit effects.
///
/// Useful when a workflow has no side effects but still needs a handler
/// to register with the runtime.
pub(crate) struct NoopHandler<W>(std::marker::PhantomData<W>);

impl<W> Default for NoopHandler<W> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

#[derive(Debug)]
pub struct NoopHandlerError;

impl std::fmt::Display for NoopHandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("noop handler does not fail")
    }
}

impl std::error::Error for NoopHandlerError {}

/// Handler for executing workflow effects.
///
/// Implement this trait for each workflow type that produces effects.
/// The handler receives effects and optionally returns inputs to route
/// back to the workflow.
///
/// # Results
///
/// | Result | Meaning |
/// |--------|---------|
/// | `Ok(Some(input))` | Effect succeeded, route input back to workflow |
/// | `Ok(None)` | Effect succeeded, no follow-up needed (fire-and-forget) |
/// | `Err(_)` | Failure, will retry with backoff until max attempts, then dead letter |
///
/// # Error Handling
///
/// All errors are treated as retryable. The worker will retry with exponential
/// backoff until `max_attempts` is reached, then move to the dead letter queue.
///
/// For **expected failures** (e.g., payment declined, user not found), model them
/// as workflow inputs rather than errors:
///
/// ```ignore
/// // Instead of returning an error for expected failures:
/// Err(CardDeclinedError)  // Don't do this
///
/// // Model it as an input so the workflow can handle it:
/// Ok(Some(PaymentInput::Declined { reason: "card declined" }))  // Do this
/// ```
///
/// This ensures expected failures are recorded in the event history and can be
/// handled by the workflow logic.
///
/// # Idempotency
///
/// Effects have **at-least-once** delivery semantics. Handlers may be called
/// multiple times for the same effect due to retries or worker failures.
/// Use [`EffectContext::idempotency_key()`] when calling external APIs that
/// support idempotency keys.
///
/// # Example
///
/// ```ignore
/// struct OrderEffectHandler {
///     payment_client: PaymentClient,
///     email_client: EmailClient,
/// }
///
/// impl EffectHandler for OrderEffectHandler {
///     type Workflow = OrderWorkflow;
///     type Error = anyhow::Error;
///
///     async fn handle(
///         &self,
///         effect: &OrderEffect,
///         ctx: &EffectContext,
///     ) -> Result<Option<OrderInput>, Self::Error> {
///         match effect {
///             OrderEffect::ProcessPayment { amount } => {
///                 // Use idempotency key for external API
///                 let result = self.payment_client
///                     .charge(*amount, ctx.idempotency_key())
///                     .await?;  // Natural ? usage
///
///                 // Route result back to workflow
///                 Ok(Some(OrderInput::PaymentResult {
///                     order_id: ctx.workflow.workflow_id().to_string(),
///                     success: result.success,
///                 }))
///             }
///
///             OrderEffect::SendConfirmation { email } => {
///                 self.email_client.send(email).await?;
///                 Ok(None) // Fire-and-forget
///             }
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait EffectHandler: Send + Sync + 'static {
    /// The workflow this handler is associated with.
    type Workflow: Workflow;

    /// The error type returned by this handler.
    ///
    /// Must implement `Display` for dead letter queue serialization.
    /// Common choices: `anyhow::Error`, `std::io::Error`, or custom error types.
    type Error: std::fmt::Display + Send + 'static;

    /// Execute an effect and optionally return an input to route back.
    ///
    /// # Arguments
    ///
    /// * `effect` - The effect to execute (from `Workflow::Effect`)
    /// * `ctx` - Execution context with correlation and idempotency info
    ///
    /// # Returns
    ///
    /// * `Ok(Some(input))` — Effect succeeded, route input back to workflow
    /// * `Ok(None)` — Effect succeeded, no follow-up needed
    /// * `Err(_)` — Failure, will retry with backoff
    async fn handle(
        &self,
        effect: &<Self::Workflow as Workflow>::Effect,
        ctx: &EffectContext,
    ) -> Result<Option<<Self::Workflow as Workflow>::Input>, Self::Error>;
}

#[async_trait]
impl<W> EffectHandler for NoopHandler<W>
where
    W: Workflow + Send + Sync + 'static,
    W::Input: Send,
{
    type Workflow = W;
    type Error = NoopHandlerError;

    async fn handle(
        &self,
        _effect: &<Self::Workflow as Workflow>::Effect,
        _ctx: &EffectContext,
    ) -> Result<Option<<Self::Workflow as Workflow>::Input>, Self::Error> {
        warn!(
            workflow_type = W::TYPE,
            "NoopHandler received an effect; workflow should not emit effects"
        );
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct BufferWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct NoopWorkflow;

    #[derive(Default)]
    struct NoopState;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct NoopInput {
        id: String,
    }

    #[derive(serde::Serialize, serde::Deserialize, Clone)]
    struct NoopEvent;

    #[derive(serde::Serialize)]
    struct NoopEffect;

    impl crate::HasWorkflowId for NoopInput {
        fn workflow_id(&self) -> crate::WorkflowId {
            crate::WorkflowId::new(&self.id)
        }
    }

    impl Workflow for NoopWorkflow {
        type State = NoopState;
        type Input = NoopInput;
        type Event = NoopEvent;
        type Effect = NoopEffect;

        const TYPE: &'static str = "noop";

        fn evolve(state: Self::State, _event: Self::Event) -> Self::State {
            state
        }

        fn decide(
            _now: time::OffsetDateTime,
            _state: &Self::State,
            _input: &Self::Input,
        ) -> crate::Decision<Self::Event, Self::Effect, Self::Input> {
            crate::Decision::event(NoopEvent)
        }
    }

    #[tokio::test]
    async fn noop_handler_logs_warning_on_handle() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer_buffer = Arc::clone(&buffer);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_buffer),
            })
            .with_ansi(false)
            .finish();

        let _guard = tracing::subscriber::set_default(subscriber);

        let handler = NoopHandler::<NoopWorkflow>::default();
        let ctx = EffectContext::new(
            uuid::Uuid::nil(),
            crate::WorkflowRef::new("noop", "noop-1"),
            1,
            time::OffsetDateTime::UNIX_EPOCH,
        );
        let effect = NoopEffect;

        let _ = handler.handle(&effect, &ctx).await;

        let locked = buffer.lock().unwrap();
        let output = String::from_utf8_lossy(&locked);
        assert!(output.contains("NoopHandler received an effect"));
    }
}
