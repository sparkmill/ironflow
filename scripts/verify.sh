#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${ROOT_DIR}"

TOTAL_STEPS=10
STEP=1

step() {
    printf "\n==> [%d/%d] %s\n" "${STEP}" "${TOTAL_STEPS}" "$1"
    STEP=$((STEP + 1))
}

step "Checking SQL formatting"
"${SCRIPT_DIR}/format-sql.sh" --check

step "Checking TOML formatting"
"${SCRIPT_DIR}/format-toml.sh" --check

step "Checking Markdown formatting"
"${SCRIPT_DIR}/format-md.sh" --check

step "Preparing SQLx metadata"
cargo sqlx prepare --workspace --check

step "Running typecheck"
cargo check --workspace --all-targets --all-features

step "Running Rust formatter"
cargo fmt --all -- --check

step "Running clippy"
cargo clippy --all-targets --all-features -- -D warnings

step "Running security audit"
"${SCRIPT_DIR}/security-check.sh"

step "Running tests"
cargo test --workspace --all-features
