# Changelog

All notable changes to this project will be documented in this file.
This project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0] - 2026-04-23

### Breaking Changes

- `WorkflowService::fetch_latest_state` is now generic over `W: Workflow` and returns `Result<W::State>` instead of `Result<Value>`. The previous string-keyed JSON variant is still available as `fetch_latest_state_dynamic(workflow_type, workflow_id)`, mirroring the `execute<W>` / `execute_dynamic` pair.
- `Error::EventDeserialization::sequence` widened from `usize` to `i64` to match the DB sequence type. The constructor `Error::event_deserialization` takes `i64` accordingly.

### Added

- `Never` uninhabited type for `Workflow::Rejection`. Use as `type Rejection = Never;` when a workflow can never reject. Serializable alternative to `std::convert::Infallible`, which lacks serde impls and therefore doesn't satisfy the `Rejection` bound.

### Fixed

- `Workflow::Rejection` docstring no longer suggests `std::convert::Infallible`, which doesn't implement `Serialize`.
- Event-deserialization errors during replay now report the per-workflow sequence number from the event store (1-based), matching the `sequence` column. Previously reported the 0-based iteration index, which mismatched the DB value by 1 and made debugging harder.
- Crate-level rustdoc example in `lib.rs` no longer references the unexported `Decider` type or the renamed `Decision::event` / 3-param `Decision<E, F, I>`; now uses `WorkflowRuntime::builder(...).build_service()` with `type Rejection = Never;` and `Decision::accept(...)`.

### Migration

- Typed callers: `service.fetch_latest_state(W::TYPE, &id)` → `service.fetch_latest_state::<W>(&id)` (drops the JSON round-trip).
- Dynamic / HTTP callers who want `Value` back: rename to `fetch_latest_state_dynamic(W::TYPE, &id)`.

## [0.4.0] - 2026-04-22

### Breaking Changes

- `Workflow::decide` now returns a `Decision<E, F, I, R>` enum with `Accept { events, effects, timers, cancel_timers }` and `Reject(R)` variants.
- New associated type `Workflow::Rejection: Serialize + Send + Debug`. Use a domain enum, `Cow<'static, str>`, or `std::convert::Infallible`.
- Constructors renamed: `Decision::event` → `accept`, `from_events` → `accept_events`, `try_from_iter` → `try_accept`. Added `Decision::reject(payload)`.
- `WorkflowService::execute<W>` returns `Result<ExecuteOutcome<W::Rejection>>` with `Accepted { events_appended }`, `Rejected(R)`, and `AlreadyCompleted` variants. Inputs to completed workflows no longer silently return `Ok(())`. `execute_dynamic` returns `ExecuteOutcome<Value>`.
- `Timer::at` and `Decision::with_timer_at` removed. Use `Timer::after(delay, input)` / `with_timer_after(delay, input)`; fire time is computed DB-side. `Timer<I>`'s `fire_at: OffsetDateTime` field replaced with `delay: Duration`.
- `OutboxStore::mark_timer_processed` and `record_timer_failure` gained a `worker_id: &str` parameter. `OutboxStore::mark_processed`, `record_failure`, and `record_permanent_failure` gained the same parameter so the effect path can enforce the same stale-claim guard.

### Added

- `ExecuteOutcome<R>` with `map` / `try_map` helpers.
- TypeId-keyed typed dispatch registry that bypasses the serde round-trip `execute_dynamic` uses.
- `ObservationOutcome` enum; input observations now record whether each input was accepted, rejected (with payload), or dropped as already-completed.
- `Store::record_observation` for writing observations outside a unit-of-work transaction.
- Migration `20260422000001_add_observation_outcome.sql`: adds `outcome` and `rejection_payload` columns to `input_observations`.

### Changed

- Rejections are expressed via `Decision::Reject(R)` instead of synthetic "rejected" events; `evolve` no longer sees rejection events. The audit trail lives in `input_observations` when `record_input_observations` is enabled.
- Rejected bootstraps roll back the newly-inserted `workflow_instances` row so no ghost instances persist.
- Worker panics are logged at `tracing::error!` via a supervisor task and surface in real time; `run()` still returns `Ok(())` on panic — process-level supervision is expected.
- `record_input_observations = true` now covers accepted, rejected, and already-completed inputs uniformly.

### Fixed

- `schedule_timers` keyed upsert now resets `attempts`, `last_error`, `locked_until`, and `locked_by` on reschedule. Previously a rescheduled timer inherited the prior run's failure state.
- `mark_timer_processed` and `record_timer_failure` guard on `locked_by = $worker_id`, preventing self-rescheduling heartbeat timers from being clobbered and preventing stolen claims from being overwritten by the stale worker's late call.
- `mark_processed`, `record_failure`, and `record_permanent_failure` on the effect outbox now guard on `locked_by = $worker_id` (matching the timer path). A stale worker whose claim was taken over (lock expired, another worker re-claimed) no longer over-increments `attempts`, shortens the new claimant's `locked_until`, or clears `locked_by` while the new claimant is still processing.
- `WorkflowService::fetch_latest_state` now reports event-deserialization failures with workflow type, id, and sequence context (via `Error::EventDeserialization`), matching the decider's replay path instead of surfacing a context-free `serde_json` error.

### Migration

1. Add `type Rejection = ...;` to each `Workflow` impl.
2. Replace `Decision::event(...)` with `Decision::accept(...)`; replace synthesized rejection events with `Decision::reject(...)`.
3. Match on `ExecuteOutcome<W::Rejection>` at call sites of `service.execute::<W>(...)`.
4. Replace `Timer::at(fire_at, input)` with `Timer::after(delay, input)` (compute `delay = target - now_utc()` if absolute time is needed).
5. Custom `OutboxStore` impls: add `worker_id: &str` to `mark_timer_processed`, `record_timer_failure`, `mark_processed`, `record_failure`, and `record_permanent_failure`.
6. Run `cargo sqlx migrate run`.

## [0.3.0] - 2026-02-13

### Added

- Unique key constraint: opt-in at-most-one-active-workflow per business key via `Workflow::unique_key()`.
- New migration adding `unique_key` column and partial unique index on `workflow_instances`.
- `Error::UniqueKeyConflict` variant returned when a second workflow conflicts with an active one.

### Changed

- `Store::begin()` accepts an additional `unique_key: Option<&str>` parameter.

## [0.2.0] - 2026-02-10

### Changed

- Removed 38 unused workspace dependencies from root `Cargo.toml`.
- Dependencies updated.

## [0.2.0-alpha.2] - 2026-01-16

### Added

- WorkflowService query endpoints for listing workflows, fetching event history, and retrieving latest state as JSON.

### Changed

- WorkflowService is now generic over the store type (`WorkflowService<S>`).
- Workflow states must implement `serde::Serialize` to support latest state replay.

## [0.1.2] - 2026-01-15

### Added

- Foundation release of the Ironflow runtime crate and procedural macros.

[Unreleased]: https://github.com/sparkmill/ironflow/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/sparkmill/ironflow/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/sparkmill/ironflow/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/sparkmill/ironflow/compare/v0.2.0...v0.3.0
