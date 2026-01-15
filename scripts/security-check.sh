#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo-deny >/dev/null 2>&1; then
  echo "cargo-deny not found. Install with: cargo install --locked cargo-deny"
  exit 1
fi

cargo deny --log-level error check
