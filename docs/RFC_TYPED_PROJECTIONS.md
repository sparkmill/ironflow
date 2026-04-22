# RFC: Typed Projections with Runtime Integration

## Problem

Ironflow's projection subsystem has two gaps that create friction for users:

### 1. Untyped event handling

The `Projection` trait delivers events as raw `serde_json::Value`:

```rust
impl Projection for MyProjection {
    fn handle<'a>(&'a self, event: ProjectionEvent) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Manual workflow type filtering
            if event.workflow_type != "order" {
                return Ok(());
            }

            // Manual JSON deserialization
            let order_event: OrderEvent = serde_json::from_value(event.payload)?;

            match order_event { /* ... */ }
        })
    }
}
```

This has several downsides:

- **No compile-time safety.** A typo in the workflow type string (`"ordr"`) silently skips all events. Forgetting to handle a workflow type produces no warning.
- **Boilerplate.** Every projection manually filters by `workflow_type`, deserializes JSON, and handles unknown types.
- **Fragile to refactoring.** Renaming a workflow's `TYPE` constant won't produce compile errors in projection handlers that reference the old string.

In practice, projection handlers end up matching on event types via `payload.get("type").and_then(|v| v.as_str())` — pure string matching with no connection to the actual event enum defined in the workflow.

### 2. No runtime integration

The `ProjectionWorker` exists but is completely disconnected from `WorkflowBuilder` and `WorkflowRuntime`. Users must:

1. Manually create `ProjectionWorker` instances
2. Manually manage `watch::channel` for shutdown
3. Manually spawn tokio tasks
4. Manually wire shutdown to the application's lifecycle

This results in ~35 lines of boilerplate per projection that reimplements what the runtime already does for effect and timer workers.

## Goal

Make projections a first-class feature with:

1. **Typed events** — handlers receive `W::Event` (the Rust enum), not `Value`
2. **Static type checking** — compile error if a handler doesn't implement the trait for a registered workflow type
3. **Multi-workflow support** — one projection can handle events from multiple workflow types, each with its own typed handler
4. **Runtime integration** — register projections on the builder, auto-spawned by `run()`

## Proposed API

### HandleEvents trait

A per-workflow typed event handler, analogous to `EffectHandler`:

```rust
#[async_trait]
pub trait HandleEvents<W: Workflow>: Send + Sync + 'static {
    async fn handle(&self, event: W::Event, ctx: EventContext) -> Result<()>;
}
```

`EventContext` carries metadata (global_sequence, workflow_id, sequence, created_at) without the raw payload.

### TypedProjectionBuilder

Composes typed handlers into a `Projection` implementation:

```rust
let projection = TypedProjectionBuilder::new("order_status", handler)
    .handles::<OrderWorkflow>()       // compile-time check
    .handles::<InventoryWorkflow>()   // compile-time check
    .build();
```

Each `.handles::<W>()` call requires `H: HandleEvents<W>` — forgetting to implement the trait is a compile error. The builder creates type-erased dispatchers keyed by `W::TYPE`, so at runtime events are routed to the correct typed handler by workflow type string. Events from unregistered workflow types are silently skipped.

### Runtime integration

```rust
let engine = WorkflowRuntime::builder(store, config)
    .register(order_effect_handler)
    .projection(projection)                              // default config
    .projection_with_config(other_projection, custom_cfg) // custom config
    .build_engine()?;

// Projections auto-spawned alongside effect/timer workers
engine.runtime.run(shutdown).await?;
```

### Before/after comparison

**Before (without typed projections):**

```rust
// projection.rs - ~200 lines of manual plumbing
impl OrderStatusProjection {
    pub async fn process_batch(&self) -> Result<u64, sqlx::Error> {
        let last_position = self.get_last_position().await?;
        let rows = sqlx::query_as("SELECT ... FROM ironflow.events WHERE ...").fetch_all(&self.pool).await?;
        for row in rows {
            let event_type = row.payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match event_type {
                "OrderCreated" => { /* extract fields from JSON */ }
                "OrderCompleted" => { /* ... */ }
                _ => {}
            }
        }
        self.update_position(max_sequence).await?;
        Ok(count)
    }

    async fn get_last_position(&self) -> Result<i64, sqlx::Error> { /* manual SQL */ }
    async fn update_position(&self, position: i64) -> Result<(), sqlx::Error> { /* manual SQL */ }
}

// background_tasks.rs - 35 lines of boilerplate per projection
pub fn start_projection(&mut self, projection: OrderStatusProjection) {
    let shutdown = self.shutdown.clone();
    let handle = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = interval.tick() => { projection.process_batch().await; }
                _ = shutdown.notified() => { break; }
            }
        }
    });
    self.tasks.push(handle);
}
```

**After:**

```rust
// projection.rs - ~40 lines, typed, compile-checked
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

// Registration - 4 lines, no manual lifecycle management
let projection = TypedProjectionBuilder::new("order_status", handler)
    .handles::<OrderWorkflow>()
    .build();

// Registered on runtime, auto-spawned, auto-shutdown.
```

## Non-goals

- **Synchronous (in-transaction) projections.** This RFC covers async polling projections only. In-transaction hooks are a separate concern.
- **Event filtering within a workflow type.** Users match on enum variants in their handler — the infrastructure delivers all events for registered types.
- **Projection schema management.** Users own their read model tables and migrations.
- **Dead letter queue for projections.** The existing backoff behavior is preserved.

## Breaking changes

Targeted at a **0.5.0** release (0.4.0 is the Decision-enum / typed-rejection /
DB-time timers rework — see `CHANGELOG.md`):

- `WorkflowRuntime::run()` gains `EventStore + ProjectionStore` bounds. `PgStore` already implements both, so no real-world impact.
- `ProjectionWorker<S, P>` relaxes `P: Projection` to `P: Projection + ?Sized`. This is backward compatible (relaxes a constraint).

## Relationship to other projection work

This RFC addresses the **typing and ergonomics** of projections. A separate
draft at `docs/PROJECTION_LEASE.md` addresses **multi-instance HA** via a
DB-backed lease so only one worker processes a given projection at a time.
The two proposals are orthogonal and compose cleanly: typed projections
describe what the handler sees; the lease describes which worker runs the
handler. Either can land first.
