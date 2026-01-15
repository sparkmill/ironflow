# Projections

Building read models from event streams using Ironflow's projection infrastructure.

---

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

---

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

---

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

---

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

---

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

---

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

---

## Worker Behavior (Summary)

Projection workers poll at `poll_interval`, process events sequentially, and
apply exponential backoff on failure (capped by `error_backoff_max`). Shutdown
stops new polling; in-flight processing completes before exit.

---

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

---

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

---

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

---

## Limitations

| Limitation              | Description                  | Workaround                           |
| ----------------------- | ---------------------------- | ------------------------------------ |
| No event filtering      | Receives all events          | Filter in handler by `workflow_type` |
| Sequential processing   | One event at a time          | Run multiple projections in parallel |
| Per-event checkpointing | DB write per event           | Acceptable for most workloads        |
| No dead letter queue    | Blocks on permanent failures | Fix handler and restart              |
| No typed events         | Raw JSON payload             | Deserialize in handler               |

---

## Known Gaps

These are known limitations of the current projection subsystem:

- No event type metadata (handlers must deserialize to discover variant).
- No built-in event filtering (handlers must filter by workflow type).
- Per-event checkpointing (extra DB writes at high throughput).
- No dead-letter queue for projections (poison events can block progress).
