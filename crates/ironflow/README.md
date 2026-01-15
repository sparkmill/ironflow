# Ironflow crate

`ironflow` is the runtime crate that powers the Ironflow workflow engine.
It exposes the core workflow state machine, Postgres-backed persistence, outbox processing,
and timers, and it integrates with the `ironflow-macros` helpers such as `HasWorkflowId`.

> NB: Ironflow is in active development and testing. Expect breaking changes, bugs, and potential concurrency or data issues in the current release and between releases; use in production only with caution.

## Highlights

- Event sourcing with SQLx-backed persistence for workflows, timers, outbox, and projections.
- Async effect support with idempotent retries and dead-letter tracking.
- Optional SQLx/Postgres feature set that pulls in `sqlx`, `tokio`, and `serde` to keep the runtime lean when the database integration is not needed.

## Quick start

1. Follow the workspace-level [`README.md`](../../README.md) to configure your environment, start Postgres, and run SQLx migrations.
2. Add the crate and the `postgres` feature to your workspace:

   ```toml
   [dependencies]
   ironflow = { version = "0.1", features = ["postgres"] }
   ```

3. Import the runtime pieces you need:

   ```rust
   use ironflow::{Decider, Workflow, WorkflowId};
   ```

## Migrations

The database schema lives under `migrations/20251230000001_create_ironflow_schema.sql`.
Downstream consumers should apply the migrations for their desired version before running the runtime
(for example, `cargo sqlx migrate run --source crates/ironflow/migrations` from the repository root).

## Testing

See `tests/postgres/README.md` for the integration suites that exercise the runtime against Postgres.
