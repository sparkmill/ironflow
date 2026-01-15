# Workflow Core

This document explains the core execution model for Ironflow workflows.
It is intentionally short and practical.

---

## Core Flow

All inputs go through a single entrypoint (`WorkflowService`). The service
serializes execution per workflow instance using the store lock.

```
Input
  -> WorkflowService.execute
  -> Store.begin (lock + replay)
  -> Workflow::decide (pure)
  -> append events
  -> enqueue effects
  -> schedule timers
  -> mark completed (optional)
  -> commit
```

Effects and timers are delivered asynchronously by workers, but both are
routed back into the same service entrypoint.

---

## Core Invariants

- `Workflow::evolve` and `Workflow::decide` are pure and deterministic.
- Every input produces at least one event.
- Inputs for the same workflow instance are serialized by a store lock.
- Effects and timers are at-least-once.
- Effects are unordered; if ordering matters, gate via state.

---

## Minimal Example

```rust
use ironflow::{Decision, Workflow};
use time::OffsetDateTime;

struct CounterWorkflow;

#[derive(Default)]
struct CounterState {
    value: i32,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
enum CounterInput {
    Increment { id: String },
    Stop { id: String },
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(tag = "type")]
enum CounterEvent {
    Incremented,
    Stopped,
}

#[derive(serde::Serialize)]
enum CounterEffect {
    Notify { message: String },
}

impl ironflow::HasWorkflowId for CounterInput {
    fn workflow_id(&self) -> ironflow::WorkflowId {
        match self {
            CounterInput::Increment { id } => id.as_str().into(),
            CounterInput::Stop { id } => id.as_str().into(),
        }
    }
}

impl Workflow for CounterWorkflow {
    type State = CounterState;
    type Input = CounterInput;
    type Event = CounterEvent;
    type Effect = CounterEffect;

    const TYPE: &'static str = "counter";

    fn evolve(mut state: Self::State, event: Self::Event) -> Self::State {
        match event {
            CounterEvent::Incremented => state.value += 1,
            CounterEvent::Stopped => {}
        }
        state
    }

    fn decide(
        _now: OffsetDateTime,
        state: &Self::State,
        input: &Self::Input,
    ) -> Decision<Self::Event, Self::Effect, Self::Input> {
        match input {
            CounterInput::Increment { .. } => Decision::event(CounterEvent::Incremented)
                .with_effect(CounterEffect::Notify {
                    message: format!("Counter is now {}", state.value + 1),
                }),
            CounterInput::Stop { .. } => Decision::event(CounterEvent::Stopped),
        }
    }

    fn is_terminal(state: &Self::State) -> bool {
        state.value >= 10
    }
}
```

---

## Common Pitfalls

- **Idempotency:** retries can re-run inputs and effects; guard in workflow state
  or use idempotency keys for external calls.
- **Terminal workflows:** completed instances silently skip new inputs.
- **Effect ordering:** effects are not ordered; sequence via state + events.
- **Serialization:** inputs/events/effects must be stable for replay.
