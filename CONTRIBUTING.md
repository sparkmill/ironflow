# Contributing

## Project Structure

- `crates/ironflow/` — main runtime crate (`src/`, `migrations/`, `tests/`)
- `crates/ironflow-macros/` — proc-macro crate used by the runtime
- `docs/` — architecture and workflow guides; start at `docs/README.md`

## Development Setup

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for full local setup instructions
(Docker, database, tooling). The short version:

```sh
cp .env.example .env
./scripts/spin_up_docker_services.sh
./scripts/sqlx_migrate.sh
```

## Coding Style

- Rust 2024 edition; follow `rustfmt` defaults.
- `snake_case` for modules/functions, `CamelCase` for types/traits, `SCREAMING_SNAKE_CASE` for constants.
- Prefer explicit error types (`thiserror`) and `Result<T, Error>` patterns already used in the codebase.

## Before Submitting a PR

- [ ] Migrations are additive (new columns, new tables, new indexes — no drops or renames)
- [ ] Migration SQL is reflected in the README and release notes if user-facing
- [ ] `CHANGELOG.md` has an entry under `[Unreleased]` (prefix with **ironflow-macros** if the change is macros-only, e.g. `**ironflow-macros (0.2.0):** Added ...`)
- [ ] Crate version is bumped in `Cargo.toml` if this is a release PR
- [ ] SQLx offline metadata is refreshed (`./scripts/sqlx_prepare.sh`)
- [ ] Events, inputs, and effects use `#[serde(tag = "type")]` for stable serialization
- [ ] New event variants use `#[serde(default)]` on optional fields for backward compatibility
- [ ] Docs are updated if behavior or API changes (see `docs/README.md`)

## Commit & PR Guidelines

- Use conventional commits with descriptive, imperative subjects (e.g., "Add timer retry policy").
- PRs should include a summary, testing status, and migration notes if schema changes are involved.

## Running Checks

One-shot:

```sh
./scripts/verify.sh
```

Individual checks:

```sh
./scripts/format-sql.sh --check
./scripts/format-toml.sh --check
./scripts/format-md.sh --check
./scripts/security-check.sh
cargo check --workspace --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

## Testing

Integration tests live in `crates/ironflow/tests/postgres/` and require a running
Postgres instance (see [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for setup).
Tests use `.env` variables (`DATABASE_URL`, `TEST_ADMIN_DATABASE_URL`).

Shared helpers and workflow fixtures are under `crates/ironflow/tests/postgres/support/`.

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md#test-utilities) for the `db_test!`
macro, database lifecycle, and `TEST_KEEP_DB` preservation.

## Unsafe Schema Changes

These require coordination and should not be done in a normal PR:

- Removing or renaming event variants (old events fail to deserialize)
- Changing event field types
- Removing workflow types with pending timers/effects
- Changing `Workflow::TYPE` constants
- Non-additive migrations (column drops, type changes)

See [ARCHITECTURE.md](docs/ARCHITECTURE.md#deployment-guidelines) for details.
