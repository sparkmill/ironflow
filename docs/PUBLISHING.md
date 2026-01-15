# Publishing guidelines

This project publishes two crates to crates.io: `ironflow` (the runtime) and `ironflow-macros`
(procedural macros). Before every release:

1. **Bump crate versions.** Update `crates/ironflow/Cargo.toml` and
   `crates/ironflow-macros/Cargo.toml` as needed (they may diverge).
2. **Verify metadata.** Ensure each manifest has accurate `description`, `readme`, `license`,
   `repository`, `documentation`, and `keywords`. Both crates should include their local `LICENSE`
   files and the crate README should include the “active development” notice.
3. **Check migrations.** Keep `crates/ironflow/migrations/` additive and confirm the latest SQL is
   reflected in the README and release notes.
4. **Install tooling (if needed).** Run `./scripts/sqlx_install.sh` for SQLx CLI and confirm
   `pg_format` is available for SQL checks.
5. **Prepare SQLx metadata.** Run `cargo sqlx prepare --workspace --all-features` so `.sqlx/`
   metadata is refreshed and can be published for offline builds.
6. **Run the full verification suite.** `./scripts/verify.sh` runs formatters, typechecks,
   clippy, security checks, and tests in one shot.
7. **Check packaging (order matters).** Run `cargo package -p ironflow-macros` first. After the
   macros crate is published, run `cargo package -p ironflow`. If you need to inspect the ironflow
   tarball before publishing the macro, use `cargo package -p ironflow --no-verify`.

Publishing order matters because `ironflow` depends on `ironflow-macros`. Once the pre-flight
checks pass:

```sh
cargo login         # if not already logged in
cargo publish -p ironflow-macros
cargo publish -p ironflow
```

## Release procedure (step-by-step)

1. Create a release branch (or use your release flow) and update versions.
2. Run the pre-flight checklist above and ensure the database migrations are in place.
3. Verify packaging output (order matters):
   ```sh
   cargo package -p ironflow-macros
   cargo package -p ironflow
   ```
4. Publish in order and monitor crates.io:
   ```sh
   cargo publish -p ironflow-macros
   cargo publish -p ironflow
   ```
5. Tag the release (e.g., `v0.1.0`) and update release notes with migration guidance.
6. Announce any breaking changes and reference the required migration file(s).

If the workspace grows additional publishable crates, include them in the checklist and update
the release notes. After publishing, update the docs/state in `docs/` (architecture notes,
migrations, etc.) so downstream dependents know which migration/feature set ships in a release.
