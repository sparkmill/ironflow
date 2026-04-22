# Repository Guidelines

For contributor guidelines (coding style, PR checklist, checks, testing) see
[CONTRIBUTING.md](CONTRIBUTING.md). For local setup and tooling see
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Project Structure

- `crates/ironflow/src/` — main runtime source code
- `crates/ironflow-macros/` — proc-macro crate
- `crates/ironflow/tests/postgres/` — Postgres integration tests
- `crates/ironflow/tests/postgres/support/` — shared test utilities and workflow fixtures
- `crates/ironflow/migrations/` — SQLx migrations
- `docs/` — architecture and workflow guides; start at `docs/README.md`
- `.env.example` — required database settings

## Quick Commands

```sh
cargo build --workspace
cargo test --workspace              # requires running Postgres
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
./scripts/verify.sh                 # runs all checks at once
```

## Configuration

- `.env` must exist with `DATABASE_URL` and `TEST_ADMIN_DATABASE_URL` (copy from `.env.example`).
- Postgres must be running for integration tests and compile-time SQLx checks.
- Set `SQLX_OFFLINE=true` to build without a database using cached `.sqlx/` metadata.
