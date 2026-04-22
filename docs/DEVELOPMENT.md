# Development Guide

Local setup, tooling, dependency management, and test utilities for contributors
working on Ironflow itself. For the PR checklist, coding style, and contribution
guidelines see [CONTRIBUTING.md](../CONTRIBUTING.md).

## Local Development Setup

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

2. **Run a local Postgres instance**

   ```sh
   ./scripts/spin_up_docker_services.sh
   ```

   The script brings up the Docker services and waits for Postgres to accept connections, using the same credentials as `.env.example`.

3. **Prepare and migrate the database**

   ```sh
   ./scripts/sqlx_migrate.sh
   ```

## Tooling

```sh
cargo install --locked bacon
cargo install --locked cargo-deny
# Unused dependency checker (requires nightly)
cargo install --locked cargo-udeps
# Install sqlx-cli with the repo-pinned version
./scripts/sqlx_install.sh
# Add, remove and upgrade dependencies
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

### Optional Tooling

These can be useful but are not required for day-to-day work:

- `shfmt -d` if you want shell formatting enforcement

## Dependency Management

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

## Toolchain Updates

```sh
# Update rustup and the default toolchain
rustup self update
rustup update

# If the repo pins a toolchain via rust-toolchain.toml, ensure it is installed
rustup show
```

## Test Utilities

The Postgres integration suite includes shared test helpers under `crates/ironflow/tests/postgres/support/` for database isolation and environment variables.

### Database Testing

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

### Database Lifecycle

1. Create - Generates unique database: `test_{test_name}_{uuid_v7}`
2. Migrate - Applies Ironflow migrations automatically
3. Test - Runs your test code with connection pool
4. Cleanup - On success: drops database | On failure: preserves for debugging
