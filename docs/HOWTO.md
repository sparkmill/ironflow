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
        Decision::reject(Rejection::NotPaid)
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
Decision::accept(Event::PaymentPending)
    .with_timer(
        Timer::after(Duration::from_secs(1800), Input::PaymentTimeout)
            .with_key("payment-timeout")
    )
```

### Multiple concurrent timers

Use distinct keys when you want more than one active timer:

```rust
let key = format!("item-timeout:{}", item_id);
Decision::accept(Event::ItemPending { item_id })
    .with_timer(Timer::after(Duration::from_secs(300), Input::ItemTimeout { item_id })
        .with_key(key))
```

### Heartbeat / recurring timers

Re-arm a timer from the input it fires to implement heartbeats,
polling, or any periodic work. The framework detects the self-reschedule
and keeps the new timer fresh (resets attempts, clears the stale claim
from the firing worker):

```rust
fn decide(_now, _state, input)
    -> Decision<Event, Effect, Input, Rejection>
{
    match input {
        Input::Tick => {
            // Do the periodic work here (typically via an effect)...
            // Then re-arm the next tick using the SAME key.
            Decision::accept(Event::Ticked)
                .with_effect(Effect::CheckUpstream)
                .with_timer(
                    Timer::after(Duration::from_secs(60), Input::Tick)
                        .with_key("heartbeat"),
                )
        }
        Input::Stop => {
            // Cancel the heartbeat when you're done — otherwise it
            // keeps firing until the workflow reaches a terminal state.
            Decision::accept(Event::Stopped).cancel_timer("heartbeat")
        }
        // ... other variants
    }
}
```

Two things to remember:

- **Use the same key every tick.** The keyed upsert guarantees one
  active heartbeat per workflow. Without the key, every tick schedules
  an additional timer and you'll eventually drown in fires.
- **Cancel the heartbeat on terminal inputs** (`Stop`, `Complete`,
  `Cancel`, etc.) — or mark the workflow terminal via `is_terminal`.
  A heartbeat that keeps firing for a terminated workflow is
  gracefully dropped by the runtime, but you'll see `already_completed`
  log spam proportional to your tick rate.

### Auditability

The timer key is stored with the timer entry. If you want it in the event
history, include it in the event payload as well.

---

## Cancel Timers

If a pending timer is no longer relevant, cancel it by key instead of
waiting for it to fire:

```rust
Decision::accept(Event::PaymentReceived)
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

## Unique Key Constraint (At-Most-One Active Workflow)

Use `Workflow::unique_key()` to enforce that only one active workflow of a
given type can exist per business key. This is useful when callers generate
fresh UUIDs for each workflow but you need exclusivity on an external key
(e.g., one active price change per listing).

### Implement `unique_key()` and `is_terminal()`

```rust
impl Workflow for PaymentWorkflow {
    // ...

    fn unique_key(input: &PaymentInput) -> Option<String> {
        match input {
            PaymentInput::Create { order_id, .. } => Some(format!("order-{order_id}")),
            _ => None, // subsequent inputs don't need the constraint
        }
    }

    fn is_terminal(state: &PaymentState) -> bool {
        matches!(state.status, PaymentStatus::Settled | PaymentStatus::Failed)
    }
}
```

### Handle the conflict

When a second workflow tries to start with the same key while the first is
still active, the service returns `Error::UniqueKeyConflict`:

```rust
match service.execute::<PaymentWorkflow>(&input).await {
    Ok(()) => { /* processed */ }
    Err(ironflow::Error::UniqueKeyConflict { .. }) => {
        // another active payment already exists for this order
    }
    Err(e) => return Err(e.into()),
}
```

### Key points

- The constraint is enforced by a PostgreSQL partial unique index — no race window.
- Only applies while the workflow is active (`completed_at IS NULL`).
- Once the workflow completes, the key is released and a new workflow can start.
- `is_terminal()` **must** be implemented; otherwise the key is held forever.

---

## Workflows Without Effects

If a workflow never emits effects, you can register it with a no-op handler:

```rust
use ironflow::{WorkflowRuntime, WorkflowServiceConfig};

let runtime = WorkflowRuntime::builder(store, WorkflowServiceConfig::default())
    .register_without_effects::<MyWorkflow>()
    .build_runtime()?;
```
