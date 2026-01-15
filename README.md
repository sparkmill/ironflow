# Ironflow

![crates.io](https://img.shields.io/crates/v/ironflow?label=ironflow&logo=rust) ![docs](https://img.shields.io/badge/docs-latest-brightgreen)

Rust workflow runtime that keeps inputs deterministic, timers reliable, and effects idempotent so you can orchestrate recurring jobs, polling, or automation with minimal overhead. Ironflow packages both the runtime crate (`ironflow`) and the helper macros (`ironflow-macros`), and exposes detailed architecture and usage guidance under [`crates/ironflow/docs/README.md`](crates/ironflow/docs/README.md).

> NB: Ironflow is in active development and testing. Expect breaking changes, bugs, and potential concurrency or data issues in the current release and between releases; use in production only with caution.

## What is Ironflow?

Ironflow implements an event-sourced workflow engine with an async effect outbox, timers, and input observation tracking. The `ironflow` crate provides the core runtime plus Postgres-backed persistence, while `ironflow-macros` ships derive helpers such as `HasWorkflowId` so downstream crates can keep input handling concise. Refer to `crates/ironflow/docs/README.md` for a guided tour of the architecture, core workflow model, and practical how-tos.

## Getting started

1. **Copy and adjust the configuration**
   The repository ships `.env.example` with:

   ```env
   DATABASE_URL="postgres://postgres:postgres@localhost:5432/ironflow"
   TEST_ADMIN_DATABASE_URL="postgres://postgres:postgres@localhost:5432/postgres"
   ```

   `DATABASE_URL` is used by SQLx for compile-time query checking and migration execution, so keep it pointed at the schema you want the runtime to use. `TEST_ADMIN_DATABASE_URL` is used by the integration tests to create and tear down per-test databases, so grant it permissions to create databases or adjust as needed for CI.

   ```sh
   cp .env.example .env
   # edit .env to match your setup (DATABASE_URL, other overlays, etc.)
   ```

2. **Run a local Postgres instance (for development)**

   ```sh
   ./scripts/spin_up_docker_services.sh
   ```

   The script brings up the Docker services and waits for Postgres to accept connections, using the same credentials as `.env.example`.

3. **Prepare and migrate the database**  
   Ironflow ships its schema inside `crates/ironflow/migrations/20251230000001_create_ironflow_schema.sql`. Run SQLx against the `DATABASE_URL` defined in your `.env` file to create the database (if needed) and apply the migration.

   ```sh
   ./scripts/sqlx_migrate.sh
   ```

   Downstream crates that depend on Ironflow should either run `cargo sqlx migrate run --source crates/ironflow/migrations` from this repository (if they vendor the `crates/ironflow/migrations/` directory) or manually apply the named SQL file for the release they depend on (`crates/ironflow/migrations/20251230000001_create_ironflow_schema.sql`). Keeping migrations additive ensures each version exposes a clear upgrade path for consumers.

## Usage & development

- Add Ironflow as a dependency:

  ```toml
  [dependencies]
  ironflow = { version = "0.1", features = ["postgres"] }
  ```

  Enable the `postgres` feature to pull in SQLx/Postgres support and pair it with `ironflow-macros` when you rely on derives such as `HasWorkflowId`.

- Reuse the fixtures under `crates/ironflow/tests/postgres/` for integration scenarios; the README in that directory explains each test group and supporting helper.

### Tooling

```sh
cargo install --locked bacon
cargo install --locked cargo-deny
# Unused dependency checker (requires nightly)
cargo install --locked cargo-udeps
# Install sqlx-cli with the repo-pinned version
./scripts/sqlx_install.sh
# Add, remove, and upgrade dependencies
cargo install --locked cargo-edit
# TOML formatting
cargo install --locked taplo-cli
```

```sh
# SQL formatter (pgFormatter):
npm install -g pgformatter
# Markdown formatter (Prettier):
npm install -g prettier@3.7.4
```

### Update/Upgrade/Prune Dependencies

```sh
# cargo upgrade is provided by `cargo-edit`
cargo upgrade --incompatible --dry-run
# Analyze above output and if happy:
cargo upgrade --incompatible
# Then update lockfile
cargo update
# Find unused deps (requires nightly)
cargo +nightly udeps --all-features
```

### Update Toolchain

```sh
# Update rustup and the default toolchain
rustup self update
rustup update

# If the repo pins a toolchain via rust-toolchain.toml, ensure it is installed
rustup show
```

### Optional Tooling

These can be useful but are not required for day-to-day work:

- `shfmt -d` if you want shell formatting enforcement

## Checks

Recommended one-shot check:

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
```

## Testing

```sh
cargo test --workspace
```

### Test Utilities

The Postgres integration suite includes shared test helpers under `crates/ironflow/tests/postgres/support/` for database isolation and environment variables.

#### Database Testing

Using the macro (recommended):

```rust
use crate::db_test;

db_test!(test_users, |pool| {
    sqlx::query!("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    Ok(())
});
```

Force preservation for any test:

```sh
TEST_KEEP_DB=1 cargo test test_users
```

#### Database Lifecycle

1. Create - Generates unique database: `test_{test_name}_{uuid_v7}`
2. Migrate - Applies Unity migrations automatically
3. Test - Runs your test code with connection pool
4. Cleanup - On success: drops database | On failure: preserves for debugging

## Documentation & contributions

Read the design notes, workflow core, and how-to guides in `crates/ironflow/docs/README.md`. When contributing, keep schema migrations and docs synchronized with code changes so downstream dependents know which migration to apply for each release.
