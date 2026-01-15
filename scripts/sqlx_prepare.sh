#!/usr/bin/env bash

set -euo pipefail

CHECK_MODE=false
if [ "${1:-}" = "--check" ]; then
    CHECK_MODE=true
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
CRATE_DIR="${ROOT_DIR}/crates/ironflow"

cd "${CRATE_DIR}"

if [ "$CHECK_MODE" = true ]; then
    echo "Checking SQLx metadata for crates/ironflow..."
    cargo sqlx prepare --check
else
    echo "Preparing SQLx metadata for crates/ironflow..."
    cargo sqlx prepare
fi
