# Publishing guidelines

This project publishes two crates to crates.io: `ironflow` (the runtime) and `ironflow-macros`
(procedural macros). Before every release:

1. **Switch to `main`.** Ensure all changes are merged and you are on the `main` branch.
2. **Update `CHANGELOG.md`.** Move entries from `[Unreleased]` into a new version heading
   (e.g. `## [0.3.0] - 2026-03-01`) and add a comparison link at the bottom of the file.
3. **Bump crate versions.** Update `crates/ironflow/Cargo.toml` and
   `crates/ironflow-macros/Cargo.toml` as needed (they may diverge).
4. **Verify metadata.** Ensure each manifest has accurate `description`, `readme`, `license`,
   `repository`, `documentation`, and `keywords`. Both crates should include their local `LICENSE`
   files and the crate README should include the "active development" notice.
5. **Check migrations.** Keep `crates/ironflow/migrations/` additive and confirm the latest SQL is
   reflected in the README and release notes.
6. **Install tooling (if needed).** Run `./scripts/sqlx_install.sh` for SQLx CLI and confirm
   `pg_format` is available for SQL checks.
7. **Prepare SQLx metadata.** Run `./scripts/sqlx_prepare.sh` so `crates/ironflow/.sqlx/` is refreshed
   and can be published for offline builds.
8. **Run the full verification suite.** `./scripts/verify.sh` runs formatters, typechecks,
   clippy, security checks, and tests in one shot.
9. **Check packaging (order matters).** Run `cargo package -p ironflow-macros` first. After the
   macros crate is published, run `cargo package -p ironflow`. If you need to inspect the ironflow
   tarball before publishing the macro, use `cargo package -p ironflow --no-verify`.

Publishing order matters because `ironflow` depends on `ironflow-macros`. Once the pre-flight
checks pass:

```sh
cargo login         # if not already logged in
cargo publish -p ironflow-macros
cargo publish -p ironflow
```

10. **Create a GitHub release.** Tag and publish a release so the version is visible on the
    repository's releases page. The version is extracted from `crates/ironflow/Cargo.toml` and
    the release notes are pulled from `CHANGELOG.md`:

```sh
VERSION=$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name=="ironflow") | .version')

NOTES=$(awk "/^## \[${VERSION}\]/{found=1} found && /^## \[/ && !/^## \[${VERSION}\]/{exit} found" CHANGELOG.md)

git tag -a "v${VERSION}" -m "v${VERSION}"
git push origin "v${VERSION}"
gh release create "v${VERSION}" --title "v${VERSION}" --notes "${NOTES}"
```

Alternatively, you can create the release from the GitHub web UI: go to **Releases** →
**Draft a new release**, type the tag name `v<VERSION>`, and fill in the release notes.
