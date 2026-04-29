# RFC: Projections — Unified HA + Typed Delivery

**Status:** Implemented in a downstream fork; ready for upstream adoption.
**Supersedes:** [RFC_TYPED_PROJECTIONS.md](./RFC_TYPED_PROJECTIONS.md), [PROJECTION_LEASE.md](./PROJECTION_LEASE.md).

The two prior drafts split typed delivery and HA leasing into separate
work streams. This document proposes a single unified design that
addresses both concerns together, plus position-correctness guarantees
that neither prior draft fully specified. The design has been built and
shipped in a vendored copy of ironflow; this RFC describes it so it can
be ported back upstream.

## Summary

Replace the current `Projection::handle(ProjectionEvent)` interface with
three traits, a builder, and a tick-based worker that holds correctness
guarantees by construction:

1. **`Projection`** — identity (name).
2. **`WorkflowProjection<W>`** — typed handler that receives `W::Event`
   already deserialized. One impl per workflow type the projection
   consumes.
3. **`UntypedWorkflowProjection`** — escape hatch that receives raw
   `serde_json::Value` for projections that don't want a typed
   dependency on a workflow's event enum (audit logs, debug taps).

Single-writer-per-projection across replicas is enforced by a Postgres
**advisory lock** scoped to the per-tick transaction, plus a **CAS
upsert** on the position cursor as a defense-in-depth safety net.
Together they make position regression impossible by construction.

No schema changes required. The existing `ironflow.projection_positions`
table works as-is.

## Why combined

The two prior drafts each solve one half:

- The typed RFC removes JSON boilerplate from handlers but leaves
  position tracking single-writer-by-policy.
- The lease draft adds HA but leaves handlers untyped.

Combining them is cheap and the abstractions reinforce each other:

- The advisory lock keys naturally on the projection name (which the
  typed builder owns).
- The CAS upsert was needed regardless — `pg_try_advisory_xact_lock`
  alone doesn't protect against operator pokes or namespace collisions.
- Per-tick lifecycle is one transaction either way; combining the two
  concerns means *one* tx that does lock + read + apply + CAS, not two.

Splitting the work into two PRs would require throwaway intermediate
states. Doing it in one move is simpler.

## Problem recap

Current upstream `Projection`:

```rust
pub trait Projection: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn handle<'a>(&'a self, event: ProjectionEvent) -> BoxFuture<'a, Result<()>>;
}
```

Current upstream `ProjectionWorker::process_batch`:

```rust
let position = store.load_projection_position(name).await?;
let events = store.fetch_events_since(position, batch_size).await?;
for event in events {
    handle(event).await?;
    store.store_projection_position(name, next_position).await?;
}
```

Three problems:

1. **Untyped delivery.** Every handler manually does
   `serde_json::from_value::<MyEvent>(event.payload)`, dispatches by
   `event.workflow_type` string. No compile-time check that handlers
   stay aligned with `Workflow::Event`.
2. **Last-writer-wins position update.** Two workers running for the
   same projection name can each read position N, apply different
   events, and write back. The smaller value can land last, regressing
   the cursor — forcing re-application of N..M events on the next
   tick. Silent corruption of read-model state if handlers aren't
   strictly idempotent.
3. **No filtering at the SQL level.** A projection that cares about a
   subset of workflow types still fetches every event and discards
   non-matching ones in user code.

All three are addressable by the design below.

## Design

### Public types

```rust
/// Metadata accompanying every projection event delivery.
///
/// The event itself is delivered separately (typed `W::Event` for
/// `WorkflowProjection`, `serde_json::Value` for
/// `UntypedWorkflowProjection`).
#[derive(Debug, Clone)]
pub struct ProjectionEventMeta {
    pub workflow_type: &'static str,
    pub workflow_id: WorkflowId,
    pub global_sequence: i64,
    pub sequence: i64,
    pub created_at: OffsetDateTime,
}

/// Identity of a projection.
///
/// `name()` is used as the row key in `ironflow.projection_positions`
/// and as the seed for the per-projection advisory lock. Must be
/// stable across deploys.
pub trait Projection: Send + Sync + 'static {
    fn name(&self) -> &'static str;
}

/// Typed handler for events from a specific workflow type.
///
/// Implement once per workflow type the projection consumes. The
/// framework dispatches to the right impl based on the event's
/// workflow type. `event` arrives already deserialized as `W::Event`.
#[async_trait]
pub trait WorkflowProjection<W: Workflow>: Projection {
    type Error: Display + Send + Sync + 'static;

    async fn apply(
        &self,
        tx: &mut sqlx::PgConnection,
        event: W::Event,
        meta: ProjectionEventMeta,
    ) -> Result<(), Self::Error>;
}

/// Escape hatch — projections that don't want a typed dependency on a
/// workflow's event enum.
#[async_trait]
pub trait UntypedWorkflowProjection: Projection {
    type Error: Display + Send + Sync + 'static;

    async fn apply(
        &self,
        tx: &mut sqlx::PgConnection,
        payload: serde_json::Value,
        meta: ProjectionEventMeta,
    ) -> Result<(), Self::Error>;
}
```

A single projection can implement both traits — typed handlers for
some workflow types, untyped for others.

### Builder

```rust
let worker = ProjectionBuilder::new(MyProjection::new())
    .for_workflow::<OrderWorkflow>()        // typed
    .for_workflow::<PaymentWorkflow>()      // typed
    .for_workflow_types_raw(&["legacy_v1"]) // untyped
    .config(ProjectionConfig::default())
    .build(store)?;

tokio::spawn(worker.run(shutdown_rx));
```

Compile-time enforcement:

- `for_workflow::<W>()` requires `P: WorkflowProjection<W>`. Forgetting
  the impl is a compile error pointing at the missing impl.
- `for_workflow_types_raw(...)` requires `P: UntypedWorkflowProjection`.

Build-time enforcement:

- Same workflow type registered twice (typed × typed, raw × raw, or
  typed × raw) → `Error::DuplicateProjectionWorkflow`.
- No workflow types registered → `Error::EmptyProjection`. (Otherwise
  the SQL filter is empty and the worker loops forever doing nothing.)

### Type erasure

Mirrors the existing `EffectHandler` / `WorkflowEntry` pattern.
Per-workflow adapter:

```rust
struct WorkflowProjectionAdapter<P, W: Workflow> {
    projection: Arc<P>,
    name: &'static str,
    _marker: PhantomData<fn() -> W>,
}

#[async_trait]
impl<P, W> DynProjectionHandler for WorkflowProjectionAdapter<P, W>
where
    P: WorkflowProjection<W>,
    W: Workflow + Send + Sync + 'static,
    W::Event: DeserializeOwned + Send,
{
    async fn apply(&self, tx: &mut PgConnection, payload: Value, meta: ProjectionEventMeta) -> Result<()> {
        let event: W::Event = serde_json::from_value(payload)
            .map_err(|e| Error::event_deserialization(W::TYPE, meta.workflow_id.as_str(), meta.sequence, e))?;
        self.projection.apply(tx, event, meta).await
            .map_err(|e| Error::ProjectionHandler { projection: self.name.into(), error: e.to_string() })
    }
}
```

The builder stores `HashMap<&'static str, Arc<dyn DynProjectionHandler>>`
keyed by `W::TYPE`. The worker never touches `serde_json` itself —
each adapter knows its own `W::Event` and does the deserialize at the
trait-object boundary.

### Single-writer guarantee — two-mechanism design

#### 1. Postgres advisory lock (primary)

Each tick begins by trying:

```sql
SELECT pg_try_advisory_xact_lock(
    PROJECTION_LOCK_NAMESPACE::int,      -- fixed constant 0x6972_6F6E ("iron")
    hashtext(projection_name)::int
)
```

Properties:

- **Two-arg form.** `(int, int)` keys on `(namespace, id)`. The
  namespace constant isolates ironflow's locks from any other app
  using advisory locks on the same database.
- **Try, don't wait.** Replicas that lose the race no-op the tick (lock
  releases, no work done, no error logged) and try again on the next
  poll. No queueing.
- **Tx-scoped.** `_xact_` variant releases automatically on commit OR
  rollback OR connection drop — no "stuck lock after crash."
- **Per-projection, not per-workflow-type.** Two projections sharing
  `OrderWorkflow` don't contend with each other (different names →
  different lock keys).

#### 2. CAS upsert on position (defense in depth)

If somehow the lock was bypassed (operator poke, lock-namespace
collision, manual fixture data), the CAS catches it:

```sql
INSERT INTO ironflow.projection_positions (projection_name, last_sequence)
VALUES ($1, $2)
ON CONFLICT (projection_name)
DO UPDATE SET last_sequence = $2, updated_at = now()
WHERE ironflow.projection_positions.last_sequence = $3
RETURNING last_sequence
```

- `$1` — projection name
- `$2` — `max(global_sequence)` over events committed in this tick
- `$3` — position read at the *start* of this tick (the value we
  expect to still be in the DB)

The `WHERE last_sequence = $3` clause gates the UPDATE side. If the DB
holds something other than `$3`, the UPDATE doesn't fire and `RETURNING`
is empty. The framework treats empty `RETURNING` as a CAS conflict and
returns `Error::ProjectionPositionConflict`, which propagates out of the
tick — the entire transaction (applies + position) rolls back.

The INSERT side handles first-tick (no row exists yet); subsequent
ticks always go through the UPDATE side.

`GREATEST(...)` was considered as an alternative — it prevents
regression but silently masks unexpected drift. CAS is stricter: it
*detects* drift and bails loudly, so anything bypassing the lock
becomes immediately observable instead of silently absorbed.

### Per-tick lifecycle

Pseudocode for one `tick()`:

```rust
let mut tx = pool.begin().await?;

// 1. Lock. Try-only; lost races no-op the tick.
let acquired = sqlx::query_scalar!(
    "SELECT pg_try_advisory_xact_lock($1::int, hashtext($2)::int)",
    PROJECTION_LOCK_NAMESPACE, self.name,
).fetch_one(&mut *tx).await?.unwrap_or(false);
if !acquired { return Ok(0); }

// 2. Read position. SELECT-only — no auto-insert. First tick → 0.
let expected_position: i64 = sqlx::query_scalar!(
    "SELECT last_sequence FROM ironflow.projection_positions WHERE projection_name = $1",
    self.name,
).fetch_optional(&mut *tx).await?.unwrap_or(0);

// 3. Fetch events filtered by registered workflow types.
let rows = sqlx::query!(
    "SELECT global_sequence, workflow_type, workflow_id, sequence, payload, created_at
     FROM ironflow.events
     WHERE workflow_type = ANY($1) AND global_sequence > $2
     ORDER BY global_sequence
     LIMIT $3",
    &self.workflow_types as &[String],
    expected_position,
    i64::from(self.config.batch_size),
).fetch_all(&mut *tx).await?;

if rows.is_empty() { return Ok(0); }  // tx drops → rollback (no-op)

// 4. Apply. Stop on first error, commit progress so far.
let mut new_position = expected_position;
let mut applied = 0;
let mut stop_error = None;
for row in &rows {
    let handler = self.handlers.get(row.workflow_type.as_str()).expect(...);
    let meta = ProjectionEventMeta { /* ... */ };
    if let Err(e) = handler.apply(&mut tx, row.payload.clone(), meta).await {
        stop_error = Some(e);
        break;
    }
    new_position = row.global_sequence;
    applied += 1;
}

// 5. If first event failed, surface the error so backoff applies.
if applied == 0 {
    return match stop_error { Some(e) => Err(e), None => Ok(0) };
}

// 6. CAS upsert the position. RETURNING empty → CAS failed → rollback.
let confirmed: Option<i64> = sqlx::query_scalar!(
    "INSERT INTO ironflow.projection_positions (projection_name, last_sequence)
     VALUES ($1, $2)
     ON CONFLICT (projection_name)
     DO UPDATE SET last_sequence = $2, updated_at = now()
     WHERE ironflow.projection_positions.last_sequence = $3
     RETURNING last_sequence",
    self.name, new_position, expected_position,
).fetch_optional(&mut *tx).await?;

if confirmed.is_none() {
    return Err(Error::ProjectionPositionConflict { projection: self.name.into() });
}

// 7. Commit. Releases lock, persists applies + position atomically.
tx.commit().await?;
Ok(applied)
```

### Error policy

**Within a batch:** stop-on-error, commit progress so far. The first
event that errors blocks the cursor at the *previous* successful
event's `global_sequence`. The next tick retries the failed event.

This is the same shape as the existing upstream `process_batch` plus
the existing batch-level `error_backoff_base/max` retry behavior.

**Poison-pill caveat:** if an event errors permanently (handler bug,
schema drift), the projection stops advancing past it. This is *the*
shape of stop-on-error; the alternative ("skip and advance, log") trades
liveness for silent data inconsistency in the read model. Recommend
shipping stop-on-error and adding a per-projection skip policy later
only if real users hit it. Diagnosable via tracing logs from
`ProjectionHandler` errors.

**On CAS conflict:** `ProjectionPositionConflict` propagates as a
framework error. The existing exponential-backoff loop handles it.
This should be vanishingly rare — the lock prevents it under normal
operation. Logged at `error!` level so operators notice.

### Schema

Existing `ironflow.projection_positions` works unchanged:

```sql
CREATE TABLE ironflow.projection_positions(
    projection_name text PRIMARY KEY,
    last_sequence bigint NOT NULL DEFAULT 0,
    updated_at timestamptz NOT NULL DEFAULT now()
);
```

No new tables, columns, or indexes needed.

### Reserved namespace constant

```rust
const PROJECTION_LOCK_NAMESPACE: i32 = 0x6972_6F6E; // "iron"
```

Document this constant prominently so DB ops don't reuse it for other
advisory locks on the same instance. Suggested location: comment
adjacent to the constant + mention in `ARCHITECTURE.md`.

## API surface deltas

### Removed

- `trait Projection { fn handle(...) -> BoxFuture<...> }` — replaced
  by the three-trait split.
- `struct ProjectionEvent` — replaced by `ProjectionEventMeta` (no
  embedded payload, since payload is delivered separately as typed or
  raw event).
- `PgStore::fetch_events_since` — projection worker now runs its own
  filtered query inside its tick tx.
- `PgStore::load_projection_position` — same.
- `PgStore::store_projection_position` — same.
- `From<StoredEvent> for ProjectionEvent` impl.

### Added

- `trait WorkflowProjection<W: Workflow>`
- `trait UntypedWorkflowProjection`
- `struct ProjectionEventMeta`
- `struct ProjectionBuilder<P>`
- `Error::EmptyProjection { projection }`
- `Error::DuplicateProjectionWorkflow { projection, workflow_type }`
- `Error::ProjectionHandler { projection, error }`
- `Error::ProjectionPositionConflict { projection }`
- `pub(crate) fn PgStore::pool(&self) -> &PgPool` — used by the
  projection worker to manage its own transaction lifecycle.

### Kept

- `struct ProjectionConfig` (poll_interval, batch_size, error_backoff_*)
- `struct ProjectionWorker`
- `ProjectionWorker::run(shutdown)` — main loop.
- `ProjectionWorker::tick()` — single-step iteration, made `pub` (was
  effectively private). Useful for tests and manual orchestration.

## Testing strategy

The vendored implementation ships 15 integration tests. Recommended
parity for upstream. Categories:

| Category | Tests |
|---|---|
| Builder validation | empty registration → error; duplicate typed → error; duplicate typed × raw → error; disjoint typed + raw → ok |
| Typed dispatch | deserialization correctness; multi-workflow routing; SQL filtering of unregistered types |
| Untyped dispatch | raw `Value` delivery; `meta.workflow_type` correctness |
| Idle | empty event log → no position row; head-of-stream → no double apply |
| Errors | first-event failure → tx rolls back, no position write; mid-batch failure → progress committed, position = last successful |
| HA | two projections sharing one workflow advance independently |
| Position correctness | position persists across ticks; manual regress recovers cleanly (proves CAS doesn't false-positive on legit re-reads) |

The single test category *not* covered: a writer racing inside one
tick to surface a true CAS conflict. Doing so requires injecting a hook
between the SELECT and the UPSERT. Skipped in the initial implementation;
add later if the assurance is wanted.

A reusable `RecordingProjection` test fixture implementing all three
handler traits (with an optional fail-at-sequence hook for error tests)
is recommended — a few lines of harness eliminates duplication across
tests.

## Implementation notes

### `pub(crate) fn PgStore::pool()`

The projection worker owns its tx lifecycle (lock, fetch, apply,
upsert, commit must all share one tx). Easiest path: expose
`pool() -> &PgPool` as `pub(crate)` so the projection module starts
its own tx. Alternative would be adding a half-dozen
`fn x(tx: &mut PgConnection, ...)` methods on PgStore, which moves SQL
away from where the rest of the projection logic lives.

### sqlx text[] binding

The SQL filter `WHERE workflow_type = ANY($1)` expects `text[]`. sqlx's
`query!` macro accepts `&[String]`. Storing the workflow types as
`Vec<String>` on the worker (cloned once at build time from the
`HashMap<&'static str, ...>` keys) avoids per-tick allocation.

### Static-vs-dynamic workflow type lookup

DB rows return `workflow_type` as `String`. The handler map is keyed by
`&'static str`. `HashMap::get_key_value(row.workflow_type.as_str())`
returns `Option<(&&'static str, &V)>` and gives us back the static key,
so `ProjectionEventMeta::workflow_type` stays `&'static str`.

### `tick()` should be public

Was effectively private in the prior plan ("primarily an implementation
detail"). Making it public is genuinely useful for tests *and* for
users who want manual single-step control (ad-hoc backfills,
debugging, embedded orchestration). `run()` calls it internally.

## Things deliberately *not* included

- **Wildcard subscription (`for_all_workflow_types_raw()`).** YAGNI.
  Use cases for "subscribe to literally every type" are rare; an
  explicit array is fine and makes registrations greppable. Add a
  wildcard if a real consumer needs it.
- **Skip-and-advance error policy.** Stop-on-error with operator
  intervention is the documented contract. A skip policy hides bugs.
- **Connection-level statement timeouts.** A hung handler holds the
  lock forever; that's diagnosable via `pg_locks` and is the right
  signal. Adding a framework-level timeout would mask handlers that
  legitimately need long batches.
- **Lease renewal heartbeats.** Not needed because the advisory lock
  is tx-scoped — it lives only for the duration of one tick. No
  long-held lease, no heartbeat.
- **Snapshot/checkpoint integration.** Out of scope; events are
  consumed in global order and the position cursor is sufficient.
  Snapshots are a workflow-side concern, not a projection-side one.

## Open questions / future work

- **Generic concurrency test.** A deterministic test that surfaces a
  real CAS conflict would require a hook between SELECT and UPSERT.
  Worth doing if the lock + CAS interaction is ever in question.
- **`tracing` field shape.** The current implementation logs
  `projection = %name, global_sequence = ..., workflow_type = ...`.
  Worth aligning with whatever convention the rest of ironflow
  settles on for structured tracing fields.
- **Builder ergonomics for many workflow types.** A projection that
  consumes 10+ workflow types ends up with a long
  `.for_workflow::<...>().for_workflow::<...>()` chain. Could add a
  macro `register_workflows![A, B, C]` later if real consumers complain.

## Reference implementation

Available in a vendored fork of ironflow within the unybrands
white-rabbit monorepo. Contact the team (or pull the `red-pill` crate
in that repo) for code reference. The implementation is ~480 lines for
`projection.rs` plus error variants and a 600-line test suite.

## Migration steps for upstream

Approximate order. Each step compiles and tests independently.

1. Add the four new `Error` variants.
2. Add `pub(crate) fn PgStore::pool()`.
3. Rewrite `projection.rs`: new traits, adapters, builder, worker. Drop
   `From<StoredEvent> for ProjectionEvent`.
4. Update `lib.rs` re-exports: drop `Projection::handle` from public
   surface, add `ProjectionBuilder`, `ProjectionEventMeta`,
   `WorkflowProjection`, `UntypedWorkflowProjection`.
5. Drop `PgStore::fetch_events_since`, `load_projection_position`,
   `store_projection_position`. (Verify nothing else uses them — only
   the old projection worker should.)
6. Port the test suite from the vendored implementation.
7. Delete `RFC_TYPED_PROJECTIONS.md`, `PLAN_TYPED_PROJECTIONS.md`,
   `PROJECTION_LEASE.md` once this RFC is accepted; update
   `PROJECTIONS.md` to describe the new API.

No DB migration needed.
