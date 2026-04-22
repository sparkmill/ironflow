# Ironflow

![crates.io](https://img.shields.io/crates/v/ironflow?label=ironflow&logo=rust) ![docs](https://img.shields.io/badge/docs-latest-brightgreen)

Rust workflow runtime that keeps inputs deterministic, timers reliable and effects idempotent so you can orchestrate recurring jobs, polling, or automation with minimal overhead.

> NB: Ironflow is in active development and testing. Expect breaking changes, bugs and potential concurrency or data issues in the current release and between releases; use in production only with caution.

## What is Ironflow?

Ironflow is an event-sourced workflow engine with Postgres-backed persistence, an async effect outbox and scheduled timers. Workflow logic is pure and deterministic — the runtime handles locking, replay, at-least-once effect and timer delivery and idempotency keys.

Multiple instances can run active-active with safe coordination via Postgres row locks and `SKIP LOCKED`. See [`docs/README.md`](docs/README.md) for architecture, core model and how-to guides.

## Getting started

1. **Add Ironflow to your project**

   ```toml
   [dependencies]
   ironflow = { version = "0.3", features = ["postgres"] }
   ```

   Enable the `postgres` feature to pull in SQLx/Postgres support. Macros like `HasWorkflowId` are re-exported from the main crate.

2. **Apply the schema migration**

   Ironflow ships its schema in `crates/ironflow/migrations/`. Apply the SQL to your Postgres database however you manage migrations. Migrations are additive — each release exposes a clear upgrade path.

## Documentation & contributions

Read the design notes, workflow core and how-to guides in [`docs/README.md`](docs/README.md). See [CONTRIBUTING.md](CONTRIBUTING.md) for the PR checklist, coding style, checks and development setup.
