# Ironflow PostgreSQL Integration Tests

This directory contains integration tests for the Ironflow workflow engine using PostgreSQL as the backing store.

## Structure

```
tests/postgres/
├── main.rs           # Module declarations
├── store.rs          # PgStore persistence layer tests
├── service.rs        # WorkflowService and builder tests
├── runtime.rs        # WorkflowRuntime effect processing tests
├── e2e.rs            # End-to-end integration scenarios
└── support/          # Test infrastructure (NOT tests)
    ├── db.rs         # Database query helpers
    ├── helpers.rs    # TestApp, wait_until, test configs
    └── workflows/    # Test workflow implementations
```

## Test Modules

### `store.rs` (34 tests)

Tests the `PgStore` persistence layer directly:

- Lock acquisition and advisory locking
- Event persistence and retrieval
- Sequence numbering (per-stream and global)
- Timer scheduling, replacement, cancellation, claiming, and processing
- Effect claiming, processing, failure handling, and dead letter queue
- Input observation recording
- Concurrent access patterns
- Terminal state handling
- Projection position storage

### `service.rs` (13 tests)

Tests `WorkflowService` and `WorkflowBuilder`:

- Builder configuration and workflow registration
- Duplicate registration rejection
- Typed and dynamic execution routing
- Event replay and state reconstruction
- Effect enqueueing
- Terminal state detection
- Input observation recording

### `runtime.rs` (7 tests)

Tests `WorkflowRuntime` effect processing:

- Full effect lifecycle with mock handlers
- Effect processing after previous failures
- Business error routing back to workflows
- Multiple workflow types in same runtime
- Parallel effect processing with multiple workers

### `e2e.rs` (5 tests)

End-to-end integration scenarios testing complete workflows:

- Payment order with timer cancellation on success
- Reservation with transient failure retry
- Dead letter queue after max retries
- Business outcome routing (missing user → rejection)
- Dynamic execution routing

## Support Infrastructure

The `support/` directory contains test infrastructure only (no tests):

- **`db.rs`** - Database query helpers (`fetch_events`, `count_events`, `seed_events`, etc.)
- **`helpers.rs`** - `TestApp` builder, `wait_until` polling helper, `test_runtime_config`
- **`workflows/`** - Test workflow implementations demonstrating various patterns:
  - `counter.rs` - Simple fire-and-forget effects (uses built-in `CounterWorkflow`)
  - `user_fetch.rs` - Effect lifecycle with result routing back to workflow
  - `reservation.rs` - Retry behavior testing via `TestMode` (transient/permanent failures)
  - `payment_order.rs` - Timer scheduling and cancellation
