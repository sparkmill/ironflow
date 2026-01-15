# How-To Guides

Practical guidance for common workflow patterns.

---

## Accept and Reject Inputs

Principle:

- Use `Err` only for execution failures.
- Use explicit events for business-level rejections.

### Validate at the service boundary

```rust
if !input_is_well_formed(input) {
    return Err(ServiceError::InvalidInput);
}
```

### Handle business rules in decide()

```rust
match input {
    Input::Ship { .. } if !state.is_paid => {
        Decision::event(Event::InputIgnored { reason: "not paid" })
    }
    _ => { /* normal decision */ }
}
```

### User feedback

Business outcomes are encoded as events, not return values. To inform users:

- **Query state**: Check the workflow state after execution
- **Query events**: Look at the events emitted (e.g., `InputIgnored`, `CancelRejected`)
- **Use projections**: Build read models for common queries

---

## Naming Events

Events are the permanent audit trail. Name them as **facts that happened** so
they read clearly in history and are stable across API changes.

Guidelines:

- Use past tense (`Created`, `Cancelled`, `Rejected`)
- Prefer domain language over technical jargon
- Keep names stable; change payloads before renaming events

### Rejection Events

When you persist a rejected/ignored input, name the event as a **fact** in
past tense, not as a return status.

Recommended patterns:

- `InputRejected` / `CommandRejected` — explicit rejection
- `InputIgnored` — valid input, but not applicable in current state
- `TransitionDenied` / `InvalidTransition` — state machine violation
- `RequestDeclined` — business decision (policy, risk, etc.), matches `Declined` result

Guidelines:

- Use past tense (`Rejected`, `Ignored`, `Declined`)
- Include `reason` or `code` in the payload
- Be consistent across workflows

---

## Timer Keys and Rescheduling

Timers can carry a `key` to support replacement semantics. For a given
`(workflow_type, workflow_id, key)` there is at most one active timer.

### Reschedule a timer

Use the same key to replace the existing timer:

```rust
Decision::event(Event::PaymentPending)
    .with_timer(
        Timer::after(Duration::from_secs(1800), Input::PaymentTimeout)
            .with_key("payment-timeout")
    )
```

### Multiple concurrent timers

Use distinct keys when you want more than one active timer:

```rust
let key = format!("item-timeout:{}", item_id);
Decision::event(Event::ItemPending { item_id })
    .with_timer(Timer::after(Duration::from_secs(300), Input::ItemTimeout { item_id })
        .with_key(key))
```

### Auditability

The timer key is stored with the timer entry. If you want it in the event
history, include it in the event payload as well.

---

## Cancel Timers

If a pending timer is no longer relevant, cancel it by key instead of
waiting for it to fire:

```rust
Decision::event(Event::PaymentReceived)
    .cancel_timer("payment-timeout")
```

For auditability, emit a `TimerCancelled { key }` event when cancelling.

---

## Idempotent Effects

Effects are delivered **at least once**. Handlers may be re-invoked due to
retries or worker crashes, so side effects must be idempotent.

Recommended practices:

- Use `EffectContext::idempotency_key()` when calling external APIs that support it.
- Prefer natural idempotency (e.g., "create if missing" or "upsert" semantics).
- Store external request IDs so retries can detect duplicates.
- Separate **expected failures** from errors: return inputs for domain rejections
  instead of `Err` so they become events.

Example:

```rust
async fn handle(
    &self,
    effect: &PaymentEffect,
    ctx: &EffectContext,
) -> Result<Option<PaymentInput>, PaymentError> {
    let result = self.payment_client
        .charge(effect.amount, ctx.idempotency_key())
        .await?;

    Ok(Some(PaymentInput::ChargeResult {
        order_id: ctx.workflow.workflow_id().to_string(),
        success: result.success,
    }))
}
```

---

## Workflows Without Effects

If a workflow never emits effects, you can register it with a no-op handler:

```rust
use ironflow::{WorkflowRuntime, WorkflowServiceConfig};

let runtime = WorkflowRuntime::builder(store, WorkflowServiceConfig::default())
    .register_without_effects::<MyWorkflow>()
    .build_runtime()?;
```
