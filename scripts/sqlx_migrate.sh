#!/usr/bin/env bash
set -euo pipefail

echo "Migrating..."
cargo sqlx database create
cargo sqlx migrate run --source crates/ironflow/migrations
