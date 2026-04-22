# Implementation Plan: Typed Projections

See [RFC_TYPED_PROJECTIONS.md](./RFC_TYPED_PROJECTIONS.md) for motivation and API design.

All changes in `crates/ironflow/`.

## 1. `src/projection.rs` — Core types + type erasure

### New public types

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

**`HandleEvents<W>` trait** — per-workflow typed handler:

```rust
#[async_trait]
pub trait HandleEvents<W: Workflow>: Send + Sync + 'static {
    async fn handle(&self, event: W::Event, ctx: EventContext) -> Result<()>;
}
```

Uses `#[async_trait]` matching the `EffectHandler` pattern.

**`TypedProjectionBuilder<H>`** — builds a `TypedProjection` from a handler:

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

The `handles::<W>()` method:

1. Creates a `TypedEventDispatcher<H, W>` that holds `Arc<H>`
2. Inserts it into `dispatchers` keyed by `W::TYPE`
3. Panics if the same `W::TYPE` is registered twice (programming error)

**`TypedProjection`** — implements `Projection`:

```rust
pub struct TypedProjection {
    name: &'static str,
    dispatchers: HashMap<&'static str, Box<dyn TypeErasedEventHandler>>,
}

impl Projection for TypedProjection {
    fn name(&self) -> &'static str { self.name }

    fn handle<'a>(&'a self, event: ProjectionEvent) -> BoxFuture<'a, Result<()>> {
        // 1. Lookup dispatcher by event.workflow_type
        // 2. If found: build EventContext, call dispatcher.handle_raw(payload, ctx)
        // 3. If not found: return Ok(()) — skip events from unregistered types
    }
}
```

### Internal (not public) type erasure

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

### Modification to `ProjectionWorker`

Relax `P: Projection` to `P: Projection + ?Sized` on the struct and impl block. This allows `Arc<dyn Projection>` to be used, which the runtime needs.

```diff
-pub struct ProjectionWorker<S, P>
+pub struct ProjectionWorker<S, P: ?Sized>
 where
     S: EventStore + ProjectionStore,
     P: Projection,

-impl<S, P> ProjectionWorker<S, P>
+impl<S, P: ?Sized> ProjectionWorker<S, P>
 where
     S: EventStore + ProjectionStore,
     P: Projection,
```

### New imports needed

```rust
use std::collections::HashMap;
use std::marker::PhantomData;

use async_trait::async_trait;
use serde::de::DeserializeOwned;

use crate::Workflow;
```

## 2. `src/runtime/registry.rs` — Builder + runtime integration

### New internal struct

```rust
pub(crate) struct RegisteredProjection {
    pub projection: Arc<dyn Projection>,
    pub config: ProjectionConfig,
}

impl Clone for RegisteredProjection {
    fn clone(&self) -> Self {
        Self {
            projection: Arc::clone(&self.projection),
            config: self.config.clone(),
        }
    }
}
```

### Modify `WorkflowBuilder`

Add field:

```rust
projections: Vec<RegisteredProjection>,
```

Initialize in `new()`:

```rust
projections: Vec::new(),
```

Add methods:

```rust
/// Register a projection with default config.
pub fn projection(self, projection: impl Projection) -> Self {
    self.projection_with_config(projection, ProjectionConfig::default())
}

/// Register a projection with custom config.
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
```

### Modify `WorkflowRuntime`

Add field:

```rust
projections: Vec<RegisteredProjection>,
```

Pass through in `build_parts()`.

### Modify `run()` impl block

Add store bounds:

```diff
 impl<S> WorkflowRuntime<S>
 where
-    S: Store + WorkflowQueryStore + OutboxStore,
+    S: Store + WorkflowQueryStore + OutboxStore + EventStore + ProjectionStore,
```

Spawn projection workers after timer workers:

```rust
for (i, registered) in runtime.projections.iter().enumerate() {
    let worker_id = format!("{}-projection-{}", runtime.worker_id, i);
    let worker = ProjectionWorker::new(
        runtime.store.clone(),
        Arc::clone(&registered.projection),
        registered.config.clone(),
        worker_id,
    );
    let rx = shutdown_rx.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = worker.run(rx).await {
            tracing::error!(error = %e, "Projection worker error");
        }
    });
    worker_handles.push(handle);
}
```

Add `projections` count to the startup log.

### New imports needed

```rust
use crate::projection::{Projection, ProjectionConfig, ProjectionWorker};
use crate::store::{EventStore, ProjectionStore};
```

## 3. `src/lib.rs` — Re-exports

```diff
-pub use projection::{Projection, ProjectionConfig, ProjectionEvent, ProjectionWorker};
+pub use projection::{
+    EventContext, HandleEvents, Projection, ProjectionConfig, ProjectionEvent,
+    ProjectionWorker, TypedProjection, TypedProjectionBuilder,
+};
```

## 4. Tests

### Unit tests in `src/projection.rs`

Add to the existing `#[cfg(test)] mod tests`:

1. **`typed_projection_routes_to_correct_handler`** — create a `TypedProjection` with two workflow types, send events for each, verify the correct handler was called with the correct typed event.

2. **`typed_projection_skips_unknown_workflow_types`** — send an event with an unregistered workflow type, verify `Ok(())` returned and no handler called.

3. **`typed_projection_propagates_deserialization_errors`** — send an event with a payload that doesn't match `W::Event`, verify `Err` returned.

These tests can use a simple inline test workflow:

```rust
struct TestWorkflow;
impl Workflow for TestWorkflow {
    type State = ();
    type Input = TestInput;
    type Event = TestEvent;
    type Effect = ();
    const TYPE: &'static str = "test";
    // ...
}
```

### Integration test (optional, can be deferred)

A test in `tests/postgres/` that exercises the full path: workflow emits events → runtime spawns projection worker → handler receives typed events → read model updated.

## 5. Type erasure flow diagram

```
compile time                              runtime
────────────                              ───────

builder.handles::<W>()                    ProjectionEvent arrives
  │                                         │
  ├─ H: HandleEvents<W> ← static check     ├─ lookup dispatchers[event.workflow_type]
  │                                         │
  └─ creates TypedEventDispatcher<H, W>     ├─ found? → handle_raw(payload, ctx)
       stores in HashMap<W::TYPE, Box<..>>  │    │
                                            │    ├─ serde_json::from_value::<W::Event>(payload)
                                            │    └─ H::handle(event, ctx)
                                            │
                                            └─ not found? → Ok(()) skip
```

## Verification

```bash
cargo check --workspace --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

## File summary

| File                      | Change                                                                                                                                                                                                         |
| ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src/projection.rs`       | Add `EventContext`, `HandleEvents`, type erasure internals, `TypedProjectionBuilder`, `TypedProjection`. Relax `ProjectionWorker` bounds to `P: ?Sized`. Add unit tests.                                       |
| `src/runtime/registry.rs` | Add `RegisteredProjection`. Add `.projection()` / `.projection_with_config()` to `WorkflowBuilder`. Store and spawn projection workers in `WorkflowRuntime::run()`. Add `EventStore + ProjectionStore` bounds. |
| `src/lib.rs`              | Re-export `EventContext`, `HandleEvents`, `TypedProjection`, `TypedProjectionBuilder`.                                                                                                                         |
