# Repository Guidelines

## Project Structure & Module Organization

- `crates/ironflow/` contains the main runtime crate and most source code in `crates/ironflow/src/`.
- `crates/ironflow-macros/` holds the proc-macro crate used by the runtime.
- `crates/ironflow/tests/postgres/support/` holds shared test utilities for the Postgres integration suite.
- `crates/ironflow/tests/postgres/` contains Postgres integration tests and fixtures.
- `crates/ironflow/migrations/` holds SQLx migrations (currently `crates/ironflow/migrations/20251230000001_create_ironflow_schema.sql`).
- `docs/` contains architecture and workflow guides; start at `docs/README.md`.

## Build, Test, and Development Commands

- `cargo build --workspace` builds all crates.
- `cargo test --workspace` runs unit + integration tests; the Postgres tests require a running DB.
- `cargo fmt --all` formats Rust code using rustfmt.
- `cargo clippy --all -- -D warnings` runs linting with warnings as errors.
- `cargo sqlx database create` and `cargo sqlx migrate run` set up the database defined in `.env`.
- Example Postgres dev container:
  ```sh
  docker run -d --name ironflow -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:18.1
  ```

## Coding Style & Naming Conventions

- Rust 2024 edition; follow rustfmt defaults.
- Use `snake_case` for modules/functions, `CamelCase` for types/traits, and `SCREAMING_SNAKE_CASE` for constants.
- Prefer explicit error types (`thiserror`) and `Result<T, Error>` patterns already used in the codebase.

## Testing Guidelines

- Integration tests target Postgres in `crates/ironflow/tests/postgres/` and rely on `.env` variables
  (`DATABASE_URL`, `TEST_ADMIN_DATABASE_URL`).
- Tests use SQLx with real migrations; keep migrations additive and deterministic.
- Name test helpers under `crates/ironflow/tests/postgres/support/` and keep workflow fixtures in
  `crates/ironflow/tests/postgres/support/workflows/`.

## Commit & Pull Request Guidelines

- Use conventional commits
- Prefer descriptive, imperative commit subjects (e.g., “Add timer retry policy”).
- PRs should include a summary, testing status (commands run), and migration notes if schema changes are involved.

## Configuration Notes

- `.env.example` documents required database settings; keep it in sync with SQLx usage.
- Migrations live under `migrations/` and must be applied before running the runtime or tests.
