# Projections

Building read models from event streams using Ironflow's projection infrastructure.

## Overview

Projections transform the append-only event log into queryable read models. They subscribe to all events via a global sequence number and maintain their own checkpoint for crash recovery.

### Why Projections?

In event-sourced systems, the event store is optimized for writes (append-only) but not for queries. Projections solve this by:

1. **Denormalizing data** — Pre-compute joins and aggregations
2. **Optimizing for reads** — Create indexes tailored to query patterns
3. **Separating concerns** — Read models can evolve independently from write models
4. **Enabling multiple views** — Same events can power different read models

### Architecture

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                              WRITE SIDE                                         │
│                                                                                 │
│   ┌─────────────┐     ┌─────────────┐     ┌─────────────────────────────────┐   │
│   │   Client    │────▶│  Workflow   │────▶│         Event Store             │   │
│   │   Input     │     │   Decider   │     │  (ironflow.events)              │   │
│   └─────────────┘     └─────────────┘     │                                 │   │
│                                           │  global_seq │ type  │ payload   │   │
│                                           │  ───────────┼───────┼────────── │   │
│                                           │  1          │ order │ Created   │   │
│                                           │  2          │ order │ Shipped   │   │
│                                           │  3          │ inv   │ Reserved  │   │
│                                           └─────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────────────┘
                                                          │
                                                          │ Global sequence
                                                          │ (total ordering)
                                                          ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                              READ SIDE                                          │
│                                                                                 │
│   ┌─────────────────────────────────────────────────────────────────────────┐   │
│   │                      Projection Workers                                 │   │
│   │                                                                         │   │
│   │  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐       │   │
│   │  │ OrderSummary     │  │ InventoryLevels  │  │ SalesAnalytics   │       │   │
│   │  │ Projection       │  │ Projection       │  │ Projection       │       │   │
│   │  │                  │  │                  │  │                  │       │   │
│   │  │ checkpoint: 2    │  │ checkpoint: 3    │  │ checkpoint: 1    │       │   │
│   │  └────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘       │   │
│   │           │                     │                     │                 │   │
│   └───────────┼─────────────────────┼─────────────────────┼─────────────────┘   │
│               ▼                     ▼                     ▼                     │
│   ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐              │
│   │ order_summaries  │  │ inventory_levels │  │ sales_by_day     │              │
│   │ (PostgreSQL)     │  │ (PostgreSQL)     │  │ (PostgreSQL)     │              │
│   └──────────────────┘  └──────────────────┘  └──────────────────┘              │
│                                                                                 │
│   ┌─────────────┐                                                               │
│   │   Client    │◀──── Query read models directly (fast, indexed)               │
│   │   Query     │                                                               │
│   └─────────────┘                                                               │
└─────────────────────────────────────────────────────────────────────────────────┘
```

### Key Concepts

| Concept             | Description                                                                |
| ------------------- | -------------------------------------------------------------------------- |
| **Global Sequence** | Monotonically increasing number across ALL events, enabling total ordering |
| **Checkpoint**      | Last processed `global_sequence`, persisted for crash recovery             |
| **Projection**      | Handler that transforms events into a read model                           |
| **Read Model**      | Queryable data structure (table, cache, search index)                      |

### Data Flow

```
1. Workflow commits events
       │
       ▼
2. Events written to ironflow.events with global_sequence
       │
       ▼
3. ProjectionWorker polls for events WHERE global_sequence > checkpoint
       │
       ▼
4. For each event:
   a. Deserialize and process (update read model)
   b. Store new checkpoint
       │
       ▼
5. Repeat on poll interval (default: 200ms)
```

## Core Components

### Projection Trait

```rust
use ironflow::{Projection, ProjectionEvent, Result};
use std::future::Future;
use std::pin::Pin;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Projection: Send + Sync + 'static {
    /// Unique identifier for checkpointing.
    fn name(&self) -> &'static str;

    /// Process a single event and update the read model.
    fn handle<'a>(&'a self, event: ProjectionEvent) -> BoxFuture<'a, Result<()>>;
}
```

### ProjectionEvent

Events delivered to projections include both global and per-workflow ordering:

```rust
pub struct ProjectionEvent {
    /// Global sequence across ALL workflows (total ordering)
    pub global_sequence: i64,

    /// Workflow type (e.g., "order", "inventory")
    pub workflow_type: String,

    /// Workflow instance ID (e.g., "ord-123")
    pub workflow_id: WorkflowId,

    /// Per-workflow sequence (1, 2, 3...)
    pub sequence: i64,

    /// Event payload as JSON
    pub payload: Value,

    /// When event was persisted
    pub created_at: OffsetDateTime,
}
```

### ProjectionConfig

```rust
pub struct ProjectionConfig {
    /// How often to poll for new events. Default: 200ms
    pub poll_interval: Duration,

    /// Maximum events per batch. Default: 100
    pub batch_size: u32,

    /// Base delay for retry backoff. Default: 200ms
    pub error_backoff_base: Duration,

    /// Maximum delay for retry backoff. Default: 5s
    pub error_backoff_max: Duration,
}
```

### ProjectionWorker

The worker polls for events and applies them to the projection:

```rust
use ironflow::{ProjectionWorker, ProjectionConfig, PgStore};
use std::sync::Arc;
use tokio::sync::watch;

let projection = Arc::new(MyProjection::new(db_pool));
let config = ProjectionConfig::default();
let (shutdown_tx, shutdown_rx) = watch::channel(false);

let worker = ProjectionWorker::new(
    store,
    projection,
    config,
    "projection-worker-1".to_string(),
);

// Run until shutdown signal
worker.run(shutdown_rx).await?;

// To shutdown gracefully:
shutdown_tx.send(true)?;
```

## Implementation Example

```rust
use ironflow::{Projection, ProjectionEvent, Result};
use serde::Deserialize;
use sqlx::PgPool;
use std::pin::Pin;
use std::future::Future;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct OrderSummaryProjection {
    db: PgPool,
}

impl OrderSummaryProjection {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

impl Projection for OrderSummaryProjection {
    fn name(&self) -> &'static str {
        "order_summary"
    }

    fn handle<'a>(&'a self, event: ProjectionEvent) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Filter to relevant workflow type
            if event.workflow_type != "order" {
                return Ok(());
            }

            // Deserialize the event payload
            #[derive(Deserialize)]
            #[serde(tag = "type")]
            enum OrderEvent {
                Created { order_id: String, total: f64 },
                Shipped { order_id: String, tracking: String },
                Completed { order_id: String },
                Cancelled { order_id: String, reason: String },
            }

            let order_event: OrderEvent = serde_json::from_value(event.payload)?;

            match order_event {
                OrderEvent::Created { order_id, total } => {
                    sqlx::query!(
                        r#"INSERT INTO order_summaries (order_id, total, status, created_at)
                           VALUES ($1, $2, 'created', $3)
                           ON CONFLICT (order_id) DO NOTHING"#,
                        order_id,
                        total,
                        event.created_at,
                    )
                    .execute(&self.db)
                    .await?;
                }

                OrderEvent::Shipped { order_id, tracking } => {
                    sqlx::query!(
                        r#"UPDATE order_summaries
                           SET status = 'shipped', tracking_number = $2, updated_at = now()
                           WHERE order_id = $1"#,
                        order_id,
                        tracking,
                    )
                    .execute(&self.db)
                    .await?;
                }

                OrderEvent::Completed { order_id } => {
                    sqlx::query!(
                        r#"UPDATE order_summaries
                           SET status = 'completed', updated_at = now()
                           WHERE order_id = $1"#,
                        order_id,
                    )
                    .execute(&self.db)
                    .await?;
                }

                OrderEvent::Cancelled { order_id, reason } => {
                    sqlx::query!(
                        r#"UPDATE order_summaries
                           SET status = 'cancelled', cancel_reason = $2, updated_at = now()
                           WHERE order_id = $1"#,
                        order_id,
                        reason,
                    )
                    .execute(&self.db)
                    .await?;
                }
            }

            Ok(())
        })
    }
}
```

## Required Store Traits

Projections require two store traits implemented by `PgStore`:

```rust
/// Fetch events by global sequence for projection replay.
pub trait EventStore: Send + Sync + Clone + 'static {
    async fn fetch_events_since(&self, after: i64, limit: u32) -> Result<Vec<StoredEvent>>;
}

/// Persist projection checkpoint positions.
pub trait ProjectionStore: Send + Sync + Clone + 'static {
    async fn load_projection_position(&self, projection_name: &str) -> Result<i64>;
    async fn store_projection_position(&self, projection_name: &str, global_sequence: i64) -> Result<()>;
}
```

## Database Schema

Projections use the `ironflow.projection_positions` table for checkpointing:

```sql
CREATE TABLE ironflow.projection_positions (
    projection_name TEXT PRIMARY KEY,
    last_sequence BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Events are read from `ironflow.events` using the `global_sequence` column:

```sql
SELECT global_sequence, workflow_type, workflow_id, sequence, payload, created_at
FROM ironflow.events
WHERE global_sequence > $1  -- After last checkpoint
ORDER BY global_sequence
LIMIT $2;  -- Batch size
```

## Guarantees

| Guarantee                  | Status | Mechanism                                     |
| -------------------------- | ------ | --------------------------------------------- |
| **At-least-once delivery** | Yes    | Checkpoint after each event; retry on failure |
| **Global ordering**        | Yes    | `global_sequence` column with `ORDER BY`      |
| **Crash recovery**         | Yes    | Checkpoint persisted to database              |
| **No data loss**           | Yes    | Events are immutable; projection catches up   |

### Not Guaranteed

| Non-Guarantee       | Implication                                   | Mitigation                                |
| ------------------- | --------------------------------------------- | ----------------------------------------- |
| **Exactly-once**    | Handler may run multiple times for same event | Make handler idempotent (upsert patterns) |
| **Real-time**       | Up to `poll_interval` latency                 | Reduce poll interval if needed            |
| **Event filtering** | Receives ALL events from ALL workflows        | Filter by `workflow_type` in handler      |

## Worker Behavior (Summary)

Projection workers poll at `poll_interval`, process events sequentially, and
apply exponential backoff on failure (capped by `error_backoff_max`). Shutdown
stops new polling; in-flight processing completes before exit.

## Best Practices

### 1. Make Handlers Idempotent

Use upsert patterns to handle duplicate deliveries:

```rust
// Good: Idempotent upsert
sqlx::query!(
    r#"INSERT INTO summaries (id, value) VALUES ($1, $2)
       ON CONFLICT (id) DO UPDATE SET value = $2"#,
    id, value
).execute(&db).await?;

// Bad: Non-idempotent insert
sqlx::query!(
    "INSERT INTO summaries (id, value) VALUES ($1, $2)",
    id, value
).execute(&db).await?;  // Fails on retry!
```

### 2. Filter Early

Skip irrelevant events at the start of your handler:

```rust
fn handle(&self, event: ProjectionEvent) -> BoxFuture<Result<()>> {
    Box::pin(async move {
        // Skip early if not relevant
        if event.workflow_type != "order" {
            return Ok(());
        }

        // Process relevant events...
    })
}
```

### 3. Use Transactions for Complex Updates

When updating multiple tables, use a transaction:

```rust
let mut tx = db.begin().await?;

sqlx::query!("UPDATE table1 ...").execute(&mut *tx).await?;
sqlx::query!("UPDATE table2 ...").execute(&mut *tx).await?;

tx.commit().await?;
```

### 4. Handle Unknown Event Types Gracefully

New event types may be added. If you use a tagged enum, include a catch-all
variant so deserialization doesn't fail:

```rust
#[derive(Deserialize)]
#[serde(tag = "type")]
enum OrderEvent {
    Created { order_id: String, total: f64 },
    Shipped { order_id: String, tracking: String },
    Completed { order_id: String },
    Cancelled { order_id: String, reason: String },
    #[serde(other)]
    Unknown,
}

match order_event {
    OrderEvent::Created { .. } => { /* handle */ }
    OrderEvent::Shipped { .. } => { /* handle */ }
    OrderEvent::Unknown => {
        tracing::debug!("Ignoring unknown event type");
    }
    _ => {}
}
```

### 5. Use Meaningful Projection Names

The projection name is used for checkpointing. Use descriptive, stable names:

```rust
fn name(&self) -> &'static str {
    "order_summary_v1"  // Include version if schema changes
}
```

## Rebuilding Projections

To rebuild a projection from scratch:

1. **Stop the projection worker**

2. **Reset the checkpoint**:

   ```sql
   UPDATE ironflow.projection_positions
   SET last_sequence = 0, updated_at = now()
   WHERE projection_name = 'order_summary';
   ```

3. **Clear the read model** (if needed):

   ```sql
   TRUNCATE order_summaries;
   ```

4. **Restart the projection worker**

The worker will replay all events from the beginning.

## Monitoring

### Key Metrics

| Metric            | Description                            | Alert Threshold    |
| ----------------- | -------------------------------------- | ------------------ |
| Projection lag    | `max(global_sequence) - last_sequence` | > 1000 events      |
| Events per second | Processing throughput                  | Baseline dependent |
| Error rate        | Failures per minute                    | > 0 sustained      |
| Checkpoint age    | Time since last update                 | > 5 minutes        |

### Checkpoint Query

```sql
SELECT
    p.projection_name,
    p.last_sequence,
    p.updated_at,
    (SELECT MAX(global_sequence) FROM ironflow.events) - p.last_sequence AS lag
FROM ironflow.projection_positions p;
```

## Limitations

| Limitation              | Description                  | Workaround                           |
| ----------------------- | ---------------------------- | ------------------------------------ |
| No event filtering      | Receives all events          | Filter in handler by `workflow_type` |
| Sequential processing   | One event at a time          | Run multiple projections in parallel |
| Per-event checkpointing | DB write per event           | Acceptable for most workloads        |
| No dead letter queue    | Blocks on permanent failures | Fix handler and restart              |
| No typed events         | Raw JSON payload             | Deserialize in handler               |

## Known Gaps

These are known limitations of the current projection subsystem:

- No event type metadata (handlers must deserialize to discover variant).
- No built-in event filtering (handlers must filter by workflow type).
- Per-event checkpointing (extra DB writes at high throughput).
- No dead-letter queue for projections (poison events can block progress).

### Correctness gaps

These are real correctness issues, not just missing features. Each has a
design sketch and a tracking section below under
[Planned work](#planned-work).

#### Bigserial snapshot gap (event loss)

**Symptom.** `fetch_events_since` can permanently skip events. `global_sequence`
is a `bigserial` assigned via `nextval()`, which is non-transactional. If tx-A
assigns sequence N (uncommitted) and tx-B assigns N+1 and commits first, a
projection worker polling between those commits sees only N+1, advances the
checkpoint past it, and will never revisit N when tx-A finally commits.

**Root cause.** `fetch_events_since` filters only by
`WHERE global_sequence > $last_position` — it has no notion of which rows are
committed under the reader's snapshot versus pending from still-in-flight
writers.

**Fix approaches.**

1. **Safe-horizon via `pg_snapshot_xmin` (preferred, PG 13+).** Filter to rows
   whose `xmin` is below the oldest still-running XID:

   ```sql
   SELECT ...
   FROM ironflow.events
   WHERE global_sequence > $1
     AND xmin::text::bigint < pg_snapshot_xmin(pg_current_snapshot())::text::bigint
   ORDER BY global_sequence
   LIMIT $2
   ```

   Events from in-flight transactions are invisible until those transactions
   commit, at which point they appear in the next poll. No events lost, no
   duplicates (checkpoint only advances over events we actually read). Cost
   is a slightly stale view — readers can be behind by the duration of the
   longest in-flight write transaction.

2. **Time-based gap buffer (simpler, looser).** `AND created_at < now() -
interval 'N seconds'`. Works if you can bound commit latency. Adds N
   seconds of projection latency unconditionally.

3. **Explicit commit ordering via an outbox-of-outboxes.** Write events
   through a staging table whose primary key is assigned inside the
   transaction and a publisher drains it in commit order. Larger change.

#### Non-monotonic checkpoint write

`store_projection_position` issues a plain `UPDATE … SET last_sequence = $2`
with no `AND last_sequence < $2` guard. A stale in-process retry or a
misconfigured second worker can rewind the checkpoint, causing handlers to
re-run against already-processed events. Not corrupting (handlers are
expected to be idempotent), just wasteful.

**Fix.** Add the monotonic guard:

```sql
UPDATE ironflow.projection_positions
SET last_sequence = $2, updated_at = now()
WHERE projection_name = $1
  AND last_sequence < $2
```

#### Missing UPSERT on checkpoint write

Same function: if an operator truncates `projection_positions` for a rebuild
between `load_projection_position` and `store_projection_position`, the
UPDATE matches zero rows and silently returns `Ok(())`. The position write is
lost. Rewriting the write as an UPSERT (or re-running the idempotent INSERT
first) closes this.

#### Redundant INSERT on every poll

`load_projection_position` issues an `INSERT … ON CONFLICT DO NOTHING` each
batch, even though it only matters on the very first call. Harmless but a
small per-poll DB write that could be hoisted to a one-time startup step.

## Planned work

### Typed projections with runtime integration

Targeted at a **0.7.0** release.

#### Problem

The projection subsystem today has two ergonomic gaps.

**1. Untyped event handling.** The `Projection` trait delivers events as raw
`serde_json::Value`:

```rust
impl Projection for MyProjection {
    fn handle<'a>(&'a self, event: ProjectionEvent) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if event.workflow_type != "order" { return Ok(()); }  // manual filter
            let order_event: OrderEvent = serde_json::from_value(event.payload)?;  // manual deserialize
            match order_event { /* ... */ }
        })
    }
}
```

- No compile-time safety. A typo in the workflow type string (`"ordr"`)
  silently skips all events. Forgetting to handle a workflow type produces
  no warning.
- Boilerplate on every projection (filter, deserialize, unknown-type branch).
- Fragile to refactoring: renaming a workflow's `TYPE` constant doesn't
  produce compile errors in projection handlers that reference the old
  string.

**2. No runtime integration.** `ProjectionWorker` exists but is disconnected
from `WorkflowBuilder` / `WorkflowRuntime`. Users manually create worker
instances, manage `watch::channel` for shutdown, spawn tokio tasks, and wire
shutdown into their lifecycle — ~35 lines of boilerplate per projection,
reimplementing what the runtime already does for effect and timer workers.

#### Goal

First-class projections with:

1. **Typed events** — handlers receive `W::Event` (the Rust enum), not `Value`.
2. **Static type checking** — compile error if a handler doesn't implement
   the trait for a registered workflow type.
3. **Multi-workflow support** — one projection can handle events from
   multiple workflow types, each with its own typed handler.
4. **Runtime integration** — register projections on the builder, auto-
   spawned by `run()`.
5. **SQL-level workflow-type filter** — events whose `workflow_type` is not
   registered with the projection must NOT be fetched from `ironflow.events`.
   At meaningful event volume, fetching every event just to drop it in
   `handle()` is unacceptable. The filter must reach the SQL: `EventStore`
   gains a fetch variant taking `workflow_types: &[&str]` and pushing it
   into the query as `workflow_type = ANY($workflow_types)`.
   `TypedProjection` exposes the registered types so `ProjectionWorker`
   passes them through. Acceptance: `EXPLAIN` on the per-projection event
   fetch shows an index scan filtered by `workflow_type`, not a seq scan
   over all events.
6. **Schema-drift tolerance** — when a payload fails to deserialize into the
   registered `W::Event` enum, the projection must skip-and-warn, not halt.
   Workflow event schemas evolve; a re-projection might encounter old
   shapes. Halting on the first old event blocks all future projection
   progress. The dispatcher logs at `warn` with `workflow_type`,
   `workflow_id`, `global_sequence`, and the deserialization error, then
   advances the checkpoint past the row.

#### Proposed API

**`HandleEvents<W>`** — per-workflow typed handler, analogous to
`EffectHandler`:

```rust
#[async_trait]
pub trait HandleEvents<W: Workflow>: Send + Sync + 'static {
    async fn handle(&self, event: W::Event, ctx: EventContext) -> Result<()>;
}
```

**`EventContext`** — metadata provided alongside the deserialized event:

```rust
#[derive(Debug, Clone)]
pub struct EventContext {
    pub global_sequence: i64,
    pub workflow_id: WorkflowId,
    pub sequence: i64,
    pub created_at: time::OffsetDateTime,
}
```

**`TypedProjectionBuilder`** — composes typed handlers into a `Projection`
implementation:

```rust
let projection = TypedProjectionBuilder::new("order_status", handler)
    .handles::<OrderWorkflow>()       // compile-time check: H: HandleEvents<OrderWorkflow>
    .handles::<InventoryWorkflow>()   // compile-time check
    .build();
```

Each `.handles::<W>()` call requires `H: HandleEvents<W>` — forgetting to
implement the trait is a compile error. The builder creates type-erased
dispatchers keyed by `W::TYPE`, so at runtime events are routed to the
correct typed handler by workflow type string. The set of registered
`W::TYPE` strings is also pushed into the SQL fetch (Goal #5), so
unregistered types never come back from the store; the dispatcher's "type
not found" branch is defense-in-depth only.

**Runtime integration** — register projections on the builder:

```rust
let engine = WorkflowRuntime::builder(store, config)
    .register(order_effect_handler)
    .projection(projection)                               // default config
    .projection_with_config(other_projection, custom_cfg) // custom config
    .build_engine()?;

// Projections auto-spawned alongside effect/timer workers
engine.runtime.run(shutdown).await?;
```

**Before/after comparison.**

Before (untyped):

```rust
impl Projection for OrderStatusProjection {
    fn handle<'a>(&'a self, event: ProjectionEvent) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if event.workflow_type != "order" { return Ok(()); }
            let event_type = event.payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match event_type {
                "OrderCreated" => { /* extract fields from JSON */ }
                "OrderCompleted" => { /* ... */ }
                _ => {}
            }
            Ok(())
        })
    }
}
// + ~35 lines of manual ProjectionWorker lifecycle management
```

After (typed):

```rust
struct OrderStatusHandler { pool: PgPool }

#[async_trait]
impl HandleEvents<OrderWorkflow> for OrderStatusHandler {
    async fn handle(&self, event: OrderEvent, ctx: EventContext) -> Result<()> {
        match event {
            OrderEvent::Created { order_id, total, .. } => {
                sqlx::query("INSERT INTO order_summaries ...")
                    .bind(order_id).bind(ctx.workflow_id.as_str())
                    .execute(&self.pool).await?;
            }
            OrderEvent::Completed { .. } => {
                sqlx::query("UPDATE order_summaries SET status = 'completed' ...")
                    .bind(ctx.workflow_id.as_str())
                    .execute(&self.pool).await?;
            }
            _ => {}
        }
        Ok(())
    }
}

let projection = TypedProjectionBuilder::new("order_status", handler)
    .handles::<OrderWorkflow>()
    .build();

// Registered on runtime, auto-spawned, auto-shutdown.
```

#### Non-goals

- **Synchronous (in-transaction) projections.** This covers async polling
  projections only. In-transaction hooks are a separate concern.
- **Event filtering within a workflow type.** Users match on enum variants
  in their handler — the infrastructure delivers all events for registered
  types.
- **Projection schema management.** Users own their read model tables and
  migrations.
- **Dead letter queue for projections.** The existing backoff behavior is
  preserved.

#### Breaking changes

- `WorkflowRuntime::run()` gains `EventStore + ProjectionStore` bounds.
  `PgStore` already implements both, so no real-world impact.
- `ProjectionWorker<S, P>` relaxes `P: Projection` to `P: Projection + ?Sized`.
  This is backward compatible (relaxes a constraint).

#### Implementation plan

All changes in `crates/ironflow/`.

**1. `src/projection.rs` — core types + type erasure.**

New public types:

- `EventContext` (above).
- `HandleEvents<W>` trait (above).
- `TypedProjectionBuilder<H>`:

  ```rust
  pub struct TypedProjectionBuilder<H> {
      name: &'static str,
      handler: Arc<H>,
      dispatchers: HashMap<&'static str, Box<dyn TypeErasedEventHandler>>,
  }

  impl<H: Send + Sync + 'static> TypedProjectionBuilder<H> {
      pub fn new(name: &'static str, handler: H) -> Self;

      /// Register a workflow type. Compile error if H doesn't impl HandleEvents<W>.
      pub fn handles<W: Workflow>(mut self) -> Self
      where
          W::Event: DeserializeOwned,
          H: HandleEvents<W>;

      pub fn build(self) -> TypedProjection;
  }
  ```

  `.handles::<W>()` creates a `TypedEventDispatcher<H, W>`, inserts it into
  `dispatchers` keyed by `W::TYPE`, and panics on duplicate registration.

- `TypedProjection` — implements `Projection` and exposes registered types
  for the SQL-level filter:

  ```rust
  pub struct TypedProjection {
      name: &'static str,
      dispatchers: HashMap<&'static str, Box<dyn TypeErasedEventHandler>>,
  }

  impl TypedProjection {
      /// Workflow types this projection handles. Passed to the
      /// EventStore fetch so unregistered types never come back from SQL.
      pub fn workflow_types(&self) -> Vec<&'static str> {
          self.dispatchers.keys().copied().collect()
      }
  }

  impl Projection for TypedProjection {
      fn name(&self) -> &'static str { self.name }

      fn handle<'a>(&'a self, event: ProjectionEvent) -> BoxFuture<'a, Result<()>> {
          // 1. Look up dispatcher by event.workflow_type
          // 2. If found: build EventContext, call dispatcher.handle_raw(payload, ctx)
          //    - On serde_json::from_value error: warn and return Ok(()) (schema drift)
          // 3. If not found: return Ok(()) — defense-in-depth; SQL filter
          //    should already prevent this
      }
  }
  ```

Internal (not public) type erasure:

```rust
trait TypeErasedEventHandler: Send + Sync {
    fn handle_raw<'a>(&'a self, payload: Value, ctx: EventContext) -> BoxFuture<'a, Result<()>>;
}

struct TypedEventDispatcher<H, W: Workflow> {
    handler: Arc<H>,
    _marker: PhantomData<W>,
}

// Deserializes Value -> W::Event, then calls HandleEvents<W>::handle()
impl<H, W> TypeErasedEventHandler for TypedEventDispatcher<H, W>
where
    H: HandleEvents<W>,
    W: Workflow,
    W::Event: DeserializeOwned,
{ ... }
```

Relax `ProjectionWorker` bounds to allow `Arc<dyn Projection>`:

```diff
-pub struct ProjectionWorker<S, P>
+pub struct ProjectionWorker<S, P: ?Sized>
```

**2. `src/store/mod.rs` + `src/store/postgres.rs` — SQL workflow-type filter.**

`EventStore` gains a typed-fetch variant:

```rust
pub trait EventStore: Send + Sync + Clone + 'static {
    // existing fetch_events_since stays for non-typed callers
    async fn fetch_events_since(&self, after: i64, limit: u32) -> Result<Vec<StoredEvent>>;

    /// Fetch events whose workflow_type is in the supplied set, ordered
    /// by global_sequence. Empty slice fetches nothing.
    async fn fetch_events_for_types_since(
        &self,
        workflow_types: &[&str],
        after: i64,
        limit: u32,
    ) -> Result<Vec<StoredEvent>>;
}
```

PgStore impl:

```sql
SELECT global_sequence, workflow_type, workflow_id, sequence, payload, created_at
FROM ironflow.events
WHERE global_sequence > $1
  AND workflow_type = ANY($2)
ORDER BY global_sequence
LIMIT $3
```

`ProjectionWorker` calls `fetch_events_for_types_since` with the slice
returned by `TypedProjection::workflow_types()`. For non-typed `Projection`
implementors, the worker falls back to `fetch_events_since` (existing
behavior).

**3. `src/runtime/registry.rs` — builder + runtime integration.**

New internal struct and builder methods:

```rust
pub(crate) struct RegisteredProjection {
    pub projection: Arc<dyn Projection>,
    pub config: ProjectionConfig,
}

impl<S> WorkflowBuilder<S> {
    pub fn projection(self, projection: impl Projection) -> Self {
        self.projection_with_config(projection, ProjectionConfig::default())
    }

    pub fn projection_with_config(
        mut self,
        projection: impl Projection,
        config: ProjectionConfig,
    ) -> Self {
        self.projections.push(RegisteredProjection {
            projection: Arc::new(projection),
            config,
        });
        self
    }
}
```

`WorkflowRuntime` gains a `projections: Vec<RegisteredProjection>` field,
populated in `build_parts()`. The `run()` impl block grows
`EventStore + ProjectionStore` bounds and spawns one supervisor per
projection after the timer workers (same supervisor pattern as effect /
timer workers).

**4. `src/lib.rs` — re-exports.**

```diff
-pub use projection::{Projection, ProjectionConfig, ProjectionEvent, ProjectionWorker};
+pub use projection::{
+    EventContext, HandleEvents, Projection, ProjectionConfig, ProjectionEvent,
+    ProjectionWorker, TypedProjection, TypedProjectionBuilder,
+};
```

**5. Tests.**

Unit tests in `src/projection.rs`:

- `typed_projection_routes_to_correct_handler` — two workflow types, send
  events for each, verify the correct handler was called with the correct
  typed event.
- `typed_projection_skips_unknown_workflow_types` — event with unregistered
  workflow type, verify `Ok(())` and no handler call.
- `typed_projection_warns_and_skips_on_schema_drift` — event whose payload
  doesn't match `W::Event`, verify `Ok(())`, no handler call, and a `warn`
  log carrying the workflow type / sequence / error. Replaces the prior
  "propagates deserialization errors" plan — schema drift must not halt
  progress.
- `typed_projection_workflow_types_returns_registered_set` — verifies the
  slice passed to the SQL fetch matches the `.handles::<W>()` calls.

Integration test in `tests/postgres/` (can be deferred): workflow emits
events → runtime spawns projection worker → handler receives typed events →
read model updated.

#### Type-erasure flow

```
compile time                               runtime
────────────                               ───────

builder.handles::<W>()                     ProjectionEvent arrives
  │                                          │
  ├─ H: HandleEvents<W> ← static check      ├─ lookup dispatchers[event.workflow_type]
  │                                          │
  └─ creates TypedEventDispatcher<H, W>      ├─ found? → handle_raw(payload, ctx)
       stores in HashMap<W::TYPE, Box<..>>   │    │
                                             │    ├─ serde_json::from_value::<W::Event>(payload)
                                             │    └─ H::handle(event, ctx)
                                             │
                                             └─ not found? → Ok(()) skip
```

#### Open questions

1. **`time` vs `chrono` on `EventContext::created_at`.** The current sketch
   uses `time::OffsetDateTime` (matching `ProjectionEvent`). Some downstream
   consumers standardize on `chrono::DateTime<Utc>` and would prefer that
   shape to avoid per-handler conversions. Worth deciding before the API
   stabilizes — switching after release is a breaking change.
2. **Position commit cadence.** The current `ProjectionWorker` commits per
   event. Some hand-rolled consumer projections commit per batch. Per-event
   is safer (smaller replay window on crash); per-batch is cheaper. Per-batch
   as a config knob is a candidate follow-up.

#### Relationship to the lease work

Typed projections describe **what the handler sees**; the lease describes
**which worker runs the handler**. The two are orthogonal and compose cleanly
— either can land first.

### Multi-worker HA via per-projection leases

Projections today are single-worker, user-driven, and uncoordinated:

- `ProjectionWorker::new(...).run(shutdown_rx).await` is entirely user
  wiring; the framework doesn't spawn projection workers.
- Running two workers against the same projection name causes every event
  to be processed by both. Handlers must be idempotent (contract above)
  but we still waste CPU and do duplicate read-model writes.
- No mechanism for "worker A dies, worker B takes over." HA requires
  manual coordination (systemd restart, Kubernetes leader election, etc.).

**Goal.** One active worker per projection at a time, automatic takeover
when the holder dies, works across process boundaries, uses only the
existing Postgres dependency.

#### Design sketch

**Schema.** New migration:

```sql
ALTER TABLE ironflow.projection_positions
    ADD COLUMN leased_by text,
    ADD COLUMN lease_expires_at timestamptz;

CREATE INDEX projection_positions_lease_idx
    ON ironflow.projection_positions (lease_expires_at)
    WHERE leased_by IS NOT NULL;
```

Existing rows get `NULL` lease fields — treated as "unheld" by the
acquisition query. Backward-compatible.

**Store API.**

```rust
pub trait ProjectionStore: Send + Sync + Clone + 'static {
    async fn load_projection_position(&self, projection_name: &str) -> Result<i64>;

    /// Write the checkpoint only if `worker_id` still holds a valid
    /// lease on the projection. Returns `false` if the lease has been
    /// lost (expired or taken over). The caller uses that signal to
    /// abort the current batch and return to the acquire loop.
    async fn store_projection_position(
        &self,
        projection_name: &str,
        worker_id: &str,
        global_sequence: i64,
    ) -> Result<bool>;

    /// Acquire or renew a lease. Returns `true` iff `worker_id` now
    /// holds it. Called once at startup and every `heartbeat_interval`
    /// thereafter.
    async fn try_acquire_projection_lease(
        &self,
        projection_name: &str,
        worker_id: &str,
        lease_duration: Duration,
    ) -> Result<bool>;

    /// Clear the lease on graceful shutdown. No-op if another worker
    /// already holds it.
    async fn release_projection_lease(
        &self,
        projection_name: &str,
        worker_id: &str,
    ) -> Result<()>;
}
```

**Acquisition SQL (single UPSERT).**

```sql
INSERT INTO ironflow.projection_positions
    (projection_name, leased_by, lease_expires_at)
VALUES ($1, $2, now() + ($3 * interval '1 second'))
ON CONFLICT (projection_name) DO UPDATE
SET leased_by = EXCLUDED.leased_by,
    lease_expires_at = EXCLUDED.lease_expires_at
WHERE projection_positions.leased_by IS NULL
   OR projection_positions.leased_by = EXCLUDED.leased_by   -- self-renewal
   OR projection_positions.lease_expires_at < now()          -- takeover after expiry
RETURNING leased_by = $2 AS owned
```

`lease_expires_at` is computed DB-side, sidestepping app/DB clock skew.

**Lease-gated checkpoint SQL.**

```sql
UPDATE ironflow.projection_positions
SET last_sequence = $2, updated_at = now()
WHERE projection_name = $1
  AND leased_by = $3
  AND lease_expires_at > now()
  AND last_sequence < $2
```

`rows_affected() > 0` means "we still own it, write succeeded."
`rows_affected() == 0` means "lease lost or stale sequence" — caller
aborts batch. This query also closes the [non-monotonic checkpoint
write](#non-monotonic-checkpoint-write) gap — the `last_sequence < $2`
guard is shared with the single-worker fix.

**Config additions.**

```rust
pub struct ProjectionConfig {
    // existing fields...

    /// How long the lease is valid before another worker can take over.
    /// Must be > heartbeat_interval. Default: 30s.
    pub lease_duration: Duration,

    /// How often to renew the lease. Should be 1/3 to 1/2 of
    /// lease_duration. Default: 10s.
    pub heartbeat_interval: Duration,

    /// How long to wait between acquisition attempts when the lease is
    /// held by someone else. Default: 5s.
    pub acquire_retry_interval: Duration,
}
```

Defaults give ~30s takeover on worker death, one DB UPSERT per 10s per
projection during steady state.

**Worker loop.**

```rust
'outer: loop {
    // Acquire loop
    while !self.store.try_acquire_projection_lease(name, worker_id, lease_duration).await? {
        tokio::select! {
            _ = sleep(acquire_retry_interval) => {},
            _ = shutdown.changed() => if *shutdown.borrow() { break 'outer; },
        }
    }

    // Process loop
    loop {
        if self.should_renew() {
            if !self.store.try_acquire_projection_lease(name, worker_id, lease_duration).await? {
                break; // Lost during renewal, back to acquire.
            }
        }

        let events = self.store.fetch_events_since(position, batch_size).await?;
        if events.is_empty() {
            // Idle; wait then retry. Renew during wait as needed.
            tokio::select! {
                _ = sleep(poll_interval) => {},
                _ = shutdown.changed() => if *shutdown.borrow() { break 'outer; },
            }
            continue;
        }

        for event in events {
            let next = event.global_sequence;
            self.projection.handle(event.into()).await?;

            let written = self.store
                .store_projection_position(name, worker_id, next)
                .await?;
            if !written {
                break; // Lost the lease mid-batch.
            }
            position = next;
        }
    }
}

self.store.release_projection_lease(name, worker_id).await?;
```

#### What this does NOT fix

**Handler side effects are not atomically gated on lease ownership.**
The handler's writes run in its own transaction, not ours. If the lease
expires between `handler.handle(event).await` returning and our
checkpoint write, the handler's side effects have already landed.

Mitigations already in place:

- Handler contract requires idempotence.
- Duplicate window is bounded by `heartbeat_interval` (we detect lease
  loss within one renewal cycle).
- Monotonic checkpoint guard means the final stored sequence is always
  the max any worker reached.

Could be improved by checking the lease _before_ each `handler.handle`
call (extra DB round-trip per event), but that only saves wasted
idempotent work — no correctness gain. Defer unless profiling shows it
matters.

#### Edge cases to verify in tests

- Acquire on a fresh projection (row doesn't exist yet): INSERT path.
- Re-acquire own lease (heartbeat): extends `lease_expires_at`.
- Another worker's acquire while held: returns `false`, row unchanged.
- Takeover after expiry: backdate `lease_expires_at`, second worker's
  acquire returns `true`.
- Release owned lease: clears fields.
- Release someone else's lease: no-op.
- Lease-gated write succeeds while owned.
- Lease-gated write returns `false` after takeover.
- End-to-end: two workers against same projection, one processes, other
  waits; kill the first, second takes over within
  `acquire_retry_interval + lease_duration`.

#### Backward compatibility

- Migration adds nullable columns with defaults — existing
  `projection_positions` rows stay valid.
- Old code without lease enforcement continues to work against the new
  schema (it just ignores the lease columns).
- Mixed rollout window: some workers enforce the lease, some don't.
  Guarantee during window degrades to current behavior (duplicate
  processing, no takeover) but never worse than today.

#### Scope

Roughly:

- Migration: 15 lines
- `ProjectionStore` methods (acquire, release, gated write): ~40 lines
- Worker loop changes: ~60 lines
- `ProjectionConfig` additions: ~15 lines
- Tests: ~70–100 lines

Maybe a half-day of work.

#### Open questions

1. **Fail hard or retry silently on acquisition failure?** Current
   proposal: sleep and retry (normal for rolling deploys). Alternative:
   return an error if startup acquisition fails, requiring explicit
   opt-in to the wait behavior.
2. **Should the monotonic `last_sequence < $2` guard stay?** With the
   lease-gated write, it's redundant for correctness (one worker at a
   time, positions always advance). But it's cheap and catches bugs
   where a stale in-process retry tries to write an old sequence.
   Probably keep.
3. **Are we confident the `RETURNING leased_by = $2 AS owned` idiom
   works in sqlx?** Should verify or fall back to a two-step (UPSERT +
   SELECT).
4. **Should `ProjectionWorker::new` take the lease config, or should it
   live on `ProjectionConfig`?** Currently proposing `ProjectionConfig`
   since that's where other timing knobs live.

#### Implementation checklist

1. Migration + schema.
2. Store trait changes + PgStore impl.
3. Worker loop refactor.
4. Tests for each lifecycle transition.
5. Update this document with the new semantics (move lease from
   "Planned work" to the main body).
