# Changelog

All notable changes to this project will be documented in this file.
This project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/sparkmill/ironflow/compare/v0.2.0...HEAD
