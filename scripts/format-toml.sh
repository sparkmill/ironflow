#!/usr/bin/env bash
set -euo pipefail

if ! command -v taplo > /dev/null 2>&1; then
    echo "taplo not found. Install with: cargo install --locked taplo-cli"
    exit 1
fi

taplo fmt "$@"
