# Ironflow Architecture (Target Design)

This document is the source of truth for the target system design. It covers
system overview, workflow model, runtime, storage, and operational guidance.

---

## System Overview

```
Client Input
    |
    v
Workflow Service (single entrypoint)
    |  - execute<W>(input) [typed]
    |  - execute_dynamic(workflow_type, payload) [untyped]
    v
Workflow Service.execute (decider internal)
    | 1) lock workflow instance
    | 2) replay events -> state
    | 3) decide(state, input)
    | 4) append events
    | 5) enqueue effects
    | 6) schedule timers
    | 7) mark completed (optional)
    v
Commit

Effects flow:

Outbox (effects table) -> EffectWorker (claim/lock) -> EffectHandler
    -> Ok(None) -> mark processed
    -> Ok(Some(input)) -> Workflow Service.execute -> mark processed
    -> Err -> record_failure + backoff -> retry or dead-letter

Timers flow:

Timers table -> TimerWorker (claim/lock) -> Input -> Workflow Service.execute
    -> success -> mark processed
    -> failure -> record_failure + backoff -> retry or dead-letter
```

The Workflow Service is the single app-facing entrypoint. Decision execution
(lock, replay, decide, persist) is handled internally. All inputs flow through
the service, including HTTP, queues/webhooks, effect results, and timers.

Execution returns `Result<()>`:

- `Ok(())`: Input processed or silently skipped (workflow completed)
- `Err`: Infrastructure failure (storage, serialization, locking)

Business outcomes (acceptance, rejection) are encoded in the event stream,
not in the return value. This keeps the API simple and idempotent.

---

## Workflow Model

A workflow type defines:

- State: reconstructed from events
- Input: commands and signals
- Event: facts appended to the store
- Effect: side effects executed by handlers

Workflow instances are identified by `(workflow_type, workflow_id)`.

### Decision

`decide()` returns a Decision that always contains at least one event.
Effects and timers are optional.

```rust
Decision::event(OrderEvent::Created)
    .with_effect(OrderEffect::SendEmail { .. })
    .with_timer_after(Duration::from_secs(3600), OrderInput::Timeout { .. })
```

### Timers

Timers schedule future inputs and are part of the decision output. They are
routed back into the Workflow Service for execution.

Timers are not effects. They are state-aware inputs that fire later.

### Timer Cancellation (Design-Ready)

Provide a cancel-by-key capability so workflows can remove pending timers
without waiting for them to fire. This avoids no-op executions and clarifies
intent. For auditability, emit a `TimerCancelled { key }` event when cancelling.

Example API shape (design target):

```rust
Decision::event(OrderEvent::PaymentReceived)
    .cancel_timer("payment-timeout")
```

### Identity and Correlation

Inputs implement `HasWorkflowId` so the system can route them to the correct
workflow instance.

```
correlation_key = (Workflow::TYPE, input.workflow_id())
```

### Terminal State

Workflows may declare terminal state via `Workflow::is_terminal`. Completed
instances can be skipped for future inputs.

### Input Observation (Optional)

If enabled, the Workflow Service records input metadata alongside normal
event storage. This enables later introspection without changing the core
workflow model.

---

## Service and Runtime Architecture

This section defines the app-facing entrypoint, the worker runtime, and how
they share a single registry.

### Service + Runtime Split (Target)

The runtime depends on the service so that **all inputs** (including effect
results and timers) flow through a single chokepoint.

Recommended structure:

```rust
struct WorkflowService { /* registry + store */ }

impl WorkflowService {
    async fn execute<W: Workflow>(&self, input: &W::Input) -> Result<()>;
    async fn execute_dynamic(&self, workflow_type: &str, payload: &Value) -> Result<()>;
}

struct WorkflowRuntime { /* workers + config + service handle */ }

impl WorkflowRuntime {
    async fn run(&self, shutdown: impl Future<Output=()>);
}
```

Optionally expose a convenience `WorkflowEngine` that bundles both for
single-process apps.

### Builder + Registry (Target)

Workflow registration should be done once and shared by both the service and
runtime. A single builder can produce both artifacts:

```rust
let builder = WorkflowBuilder::new(store)
    .register(OrderHandler)
    .register(InventoryHandler);

let engine = builder.build_engine()?; // returns WorkflowEngine { service, runtime }
```

This ensures `execute_dynamic` and effect routing use the same registry.

### Client API

#### Target: Single Workflow Service

Expose a single app-facing service with two entrypoints:

```rust
service.execute::<OrderWorkflow>(&OrderInput::Create { .. }).await?;
service.execute_dynamic("order", payload).await?;
```

Both routes end up executing the correct workflow's decision logic internally.
This is the same service described in "Service + Runtime Split (Target)".

#### Execution Results

The service returns `Result<()>`:

- `Ok(())`: Input was processed, or silently skipped if workflow was completed
- `Err`: Infrastructure failure (storage, serialization, locking)

**Business outcomes are events, not return values.** If an input is rejected
for business reasons, the workflow should emit a rejection event (e.g.,
`CancelRejected`, `InputIgnored`). Callers who need to know the outcome should:

1. Check the workflow state after execution
2. Look at the events emitted
3. Use projections for read-heavy queries

This design keeps the API simple and idempotent. Completed workflows silently
succeed when receiving new inputs—there's no special "skipped" status to handle.

Any effects or timers triggered by the decision are **asynchronous** and may
succeed or fail later.

### Component Overview

The system is divided into **core components** (essential for workflow execution)
and **auxiliary components** (optional, for specific use cases).

**Core Components (Required):**

| Component            | Purpose                                     |
| -------------------- | ------------------------------------------- |
| **Workflow trait**   | Pure functional core (`evolve`, `decide`)   |
| **Decision**         | Output structure: events + effects + timers |
| **Store/UnitOfWork** | Event persistence, locking, transactions    |
| **WorkflowService**  | Single entrypoint for all inputs            |
| **Effect System**    | Side effect processing (handlers, workers)  |
| **Timer System**     | Scheduled input delivery                    |
| **WorkflowRuntime**  | Worker coordination, registry, shutdown     |

**Auxiliary Components (Optional):**

| Component              | Purpose                            | When to Use                         |
| ---------------------- | ---------------------------------- | ----------------------------------- |
| **Projections**        | Build read models from events      | List queries, dashboards, analytics |
| **StateMachineView**   | Visualization (Mermaid/DOT export) | Documentation, debugging UIs        |
| **Input Observations** | Audit trail of inputs received     | Debugging, compliance               |

The core loop operates without auxiliary components:

```
Input → WorkflowService.execute → Events persisted → Effects executed → (repeat)
                       ↓
              State reconstructed via replay
```

Projections are **event consumers**, not producers. They build optimized read
models but don't affect workflow execution. See [PROJECTIONS.md](./PROJECTIONS.md)
for details.

### Multi-Workflow Runtime

A single runtime hosts multiple workflow types:

- Each workflow type registers one EffectHandler
- The shared registry routes by workflow_type
- Workers process effects and timers across all types

This model supports:

- Many workflow types in one process
- Many instances per workflow type
- Per-instance serialization with parallel cross-instance processing

### Effect Worker

- Polls outbox
- Claims one effect at a time
- Executes handler
- Routes result input back through the Workflow Service
- Marks processed or records failure

### Timer Worker

- Polls timers for due entries
- Claims one timer at a time
- Routes embedded input through the Workflow Service
- Marks processed or records failure

### Concurrency Model

- Multiple workers run in parallel for effects and timers
- Different workflow types and instances are processed concurrently
- Per-instance serialization is enforced by the Store lock on
  `(workflow_type, workflow_id)`
- Concurrent inputs targeting the same instance are serialized by the lock
- Effect and timer workers coordinate with claim/lock semantics

---

## Storage Model (Postgres)

We define a generic `Store` trait to keep the core model backend-agnostic,
but the primary implementation target is SQLx/Postgres in the first phase.

### Tables

- workflow_instances
  - row-level lock per (workflow_type, workflow_id)
  - completed_at marker

- events
  - append-only event stream per instance
  - ordered by sequence
  - global ordering field for projections (e.g., global_sequence)

- outbox
  - immediate effects

- timers
  - scheduled inputs

- input_observations (optional)
  - input type + timestamp for introspection

### Why Separate Tables

- Effects and timers have different semantics and access patterns
- Separate tables keep queries and monitoring clearer

### Locking

- Execution acquires a row lock on workflow_instances
- This serializes processing per workflow instance
- Different instances can proceed concurrently

### Ordering

- Per-instance order is sequence-based
- Global order is not required for workflow correctness, but projections
  rely on it for replay ordering

---

## Effects and Timers

### Effects

Lifecycle:

```
Decision::effects -> outbox -> EffectWorker -> EffectHandler
  -> Ok(None) -> mark processed
  -> Ok(Some(input)) -> Workflow Service.execute -> mark processed
  -> Err(_) -> retry with backoff (until max_attempts, then dead letter)
```

Key properties:

- At-least-once execution
- Idempotency via EffectContext; handlers must tolerate retries
- Routing by workflow_type
- Ordering is not guaranteed per workflow instance; if ordering matters,
  model it as state transitions that gate the next effect.

### Timers

Lifecycle:

```
Decision::timers -> timers table -> TimerWorker -> Workflow Service.execute
  -> success -> mark processed
  -> failure -> retry with backoff (until max_attempts, then dead letter)
```

Key properties:

- Timer fires as an input, not an effect
- State-aware execution on delivery
- Optional key-based replacement per instance

## Runtime Guarantees

### Concurrency Requirements

- Inputs for the same workflow instance are serialized (no concurrent processing).
- Effects are not processed concurrently for the same outbox entry.
- Timers are not processed concurrently for the same timer entry.

### PostgreSQL Mechanisms

- Instance serialization uses row locks on `workflow_instances` (`SELECT ... FOR UPDATE`).
- Effect and timer workers claim work with `FOR UPDATE SKIP LOCKED`.

### Guarantees Provided

| Guarantee                              | Status         | Mechanism                                           |
| -------------------------------------- | -------------- | --------------------------------------------------- |
| **Same-instance serialization**        | ✅ Guaranteed  | `SELECT ... FOR UPDATE` on `workflow_instances` row |
| **Different-instance parallelism**     | ✅ Guaranteed  | Different rows, independent locks                   |
| **Effect at-least-once delivery**      | ✅ Guaranteed  | Outbox pattern + retry on failure                   |
| **Timer at-least-once delivery**       | ✅ Guaranteed  | Timer table + retry on failure                      |
| **No duplicate concurrent processing** | ✅ Guaranteed  | `FOR UPDATE SKIP LOCKED` coordination               |
| **Worker crash recovery**              | ✅ Guaranteed  | Lock expiry (`locked_until`) releases stale claims  |
| **Graceful shutdown**                  | ✅ Best-effort | Shutdown signal + timeout                           |

### Guarantees NOT Provided

| Non-Guarantee                   | Implication                                        | Mitigation                                                      |
| ------------------------------- | -------------------------------------------------- | --------------------------------------------------------------- |
| **Effect ordering**             | Effects for same instance may execute out of order | Design workflows to be order-independent, or sequence via state |
| **Exactly-once effects**        | Effects may run multiple times (at-least-once)     | Use `ctx.idempotency_key()` with external APIs                  |
| **Timer precision**             | Timers may fire up to `timer_poll_interval` late   | Reduce poll interval if needed                                  |
| **Cross-instance transactions** | No distributed transactions                        | Design workflows to be self-contained                           |

### Reliability and Scaling

- Multiple replicas can run concurrently (active-active) with safe coordination via row locks and `SKIP LOCKED`.
- Throughput scales by increasing worker counts or adding instances; per-instance processing is still serialized.
- The database is the shared bottleneck; plan capacity and indexes accordingly.
- Worker crashes are recovered by lock expiry; deliveries remain at-least-once.

## Deployment Guidelines

**Safe Operations:**

- Adding new workflow types
- Adding new event variants (with `#[serde(default)]`)
- Adding new effect variants
- Adding optional fields to existing events
- Increasing `max_attempts` or adjusting retry policy

**Unsafe Operations (require coordination):**

- Removing event variants (old events fail to deserialize)
- Changing event field types
- Removing workflow types with pending timers/effects
- Changing `Workflow::TYPE` constant

**Recommendation for Safe Deployments:**

1. Use additive-only event schema changes
2. Deploy new code before changing schemas
3. Drain pending timers/effects before removing workflow types
4. Consider a "maintenance mode" that pauses processing

---

## Introspection

### Manual StateMachineView (Design-Ready)

Implement `StateMachineView` to describe current status and transitions:

- StateInfo: status + fields + terminal marker
- TransitionInfo: input name + target status + effects/timers
- StateMachineDefinition: optional full graph

### Observation-Based Introspection (Design-Ready)

- Record (from_status, input_type, to_status)
- Aggregate observed transitions
- Provide per-instance snapshots with history and pending work

Observation reduces drift between intended and actual behavior, but only
shows paths that have been executed.

---

## Invariants

- Workflow logic is pure and deterministic
- Every input yields at least one event
- Effects and timers are at-least-once
- Event ordering is guaranteed per workflow instance
- Effect ordering is not guaranteed; enforce ordering through workflow state

---

## Future Improvements (Nice to Have)

### Workflow Runtime

- Clarify the actor-like execution model (per-workflow mailbox, serialized state transitions).
- Strengthen guidance around idempotent effect handling.

### Input Inbox

Optional input inbox to close the routing/mark_processed atomicity gap and allow
async routing. Currently, if an effect handler returns `Some(input)` and the
subsequent `execute()` fails, we have an atomicity gap. With inbox, all inputs
(HTTP, effect results, timer fires) flow through the same durable queue.

**Benefits:**

- Durability: Input survives process crash between API call and execution
- Back-pressure: Inbox queue depth is observable/alertable
- Unified path: All inputs flow through inbox → worker → decider

**Challenges:**

1. **Semantic change to `execute()`** - Currently synchronous (events persisted
   when call returns). With inbox, becomes async (input queued, processed later).
   Callers expecting immediate consistency would break:

   ```rust
   service.execute(&CreateOrder { id: "123" }).await?;
   // With inbox: events may not exist yet!
   let events = store.load_events("order", "123").await?;
   ```

2. **Input ordering with multiple workers** - Critical concern. With multiple
   inbox workers (or multiple nodes), inputs for the same workflow instance can
   be claimed by different workers simultaneously. The `workflow_instances` row
   lock serializes execution, but the order becomes non-deterministic:

   ```
   HTTP 1: execute(InputA) → INSERT inbox (seq=1)
   HTTP 2: execute(InputB) → INSERT inbox (seq=2)

   Worker 1: claim(InputB) → acquired
   Worker 2: claim(InputA) → acquired
   Worker 1: acquire workflow_instances lock → runs InputB FIRST
   Worker 2: blocked → runs InputA SECOND (wrong order!)
   ```

   **Solutions for ordering:**

   | Approach               | Trade-off                                                                                          |
   | ---------------------- | -------------------------------------------------------------------------------------------------- |
   | Advisory lock on claim | `pg_try_advisory_xact_lock(workflow_id)` prevents claiming 2 inputs for same instance concurrently |
   | Correlated subquery    | Only claim if no earlier pending inputs; expensive, head-of-line blocking                          |

   Advisory locks are likely the best approach:

   ```sql
   SELECT * FROM ironflow.inbox
   WHERE processed_at IS NULL
     AND pg_try_advisory_xact_lock('inbox', hashtext(workflow_type || ':' || workflow_id))
   ORDER BY created_at
   LIMIT 1
   FOR UPDATE SKIP LOCKED
   ```

3. **Test suite impact** - All integration tests call `execute()` and immediately
   assert on state/effects. With async inbox, tests would need `wait_until()`
   helpers or a synchronous flush mechanism for testing.

**Priority:** Low. The current synchronous model is correct and simpler. Consider
inbox only if durability or back-pressure becomes a concrete requirement.

### Input Idempotency

Built-in command/input ID tracking to make retries safe by default. Currently,
idempotency is the workflow author's responsibility, which is error-prone.

**Problem:** If a client retries an input (e.g., due to network timeout after
the server processed it), the input executes twice. Workflow authors must
manually track processed command IDs in state to prevent this.

**Potential solutions:**

1. **Optional `command_id` field** - Inputs can declare an idempotency key via
   attribute (e.g., `#[idempotent(key = "charge_id")]`). The framework tracks
   processed keys per workflow instance and returns early for duplicates.

2. **Automatic deduplication window** - Track last N input hashes per workflow
   with a TTL. Duplicates within the window are silently ignored.

3. **Document as user responsibility** - Keep the framework simple and provide
   clear guidance for implementing idempotency in `decide()`.

**Priority:** Medium. Many workflows need this, but it can be worked around
with careful `decide()` implementation. Consider adding after core stabilizes.

### Test Utilities

- Provide a lightweight in-memory `Store` implementation for unit tests.
- Add workflow test helpers to run decisions and assert stored events/effects/timers.

### Pluggable Serialization

Abstract serialization behind a `Codec` trait, defaulting to JSON but allowing
MessagePack (smaller payloads), Protobuf (schema evolution), or custom formats.

**Priority:** Low. JSON is debuggable, human-readable, and sufficient for most
workloads.

### Event Versioning / Upcasting

Formal versioning system with explicit version numbers and upcast functions to
migrate old events to current schema. Would enable non-additive schema changes.

**Priority:** Low. Use `#[serde(default)]` and additive-only changes for now.

---

## Source of Truth

This document is a target design. Implementation will be aligned to this spec
over time.
