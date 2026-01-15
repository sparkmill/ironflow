# Publishing guidelines

This project publishes two crates to crates.io: `ironflow` (the runtime) and `ironflow-macros`
(procedural macros). Before every release:

1. **Bump crate versions.** Update `crates/ironflow/Cargo.toml` and
   `crates/ironflow-macros/Cargo.toml` and keep the workspace consistent.
2. **Verify metadata.** Ensure each manifest has accurate `description`, `readme`, `license`,
   `repository`, `documentation`, `keywords`, and `categories`. Both crates should point at the
   shared `README`/docs, and the `LICENSE` file must be published alongside them.
3. **Run formatting & linting.** `cargo fmt --all` and `cargo clippy --all -- -D warnings`.
4. **Run the suite.** `cargo test --workspace` (including the Postgres integration tests).
5. **Check packaging.** Use `cargo package -p ironflow-macros` and `cargo package -p ironflow` to
   confirm the files and metadata that crates.io will receive.

Publishing order matters because `ironflow` depends on `ironflow-macros`. Once the pre-flight
checks pass:

```sh
cargo login         # if not already logged in
cargo publish -p ironflow-macros
cargo publish -p ironflow
```

If the workspace grows additional publishable crates, include them in the checklist and update
the release notes. After publishing, update the docs/state in `docs/` (architecture notes,
migrations, etc.) so downstream dependents know which migration/feature set ships in a release.
