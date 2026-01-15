#!/usr/bin/env bash
set -euo pipefail

# Check if cargo-sqlx is already installed and cached
if ! cargo sqlx --version &> /dev/null; then
    echo "Installing sqlx-cli..."
    # NB! Keep in sync with the one in `../Cargo.toml`
    cargo install --version="~0.8" sqlx-cli --no-default-features --features rustls,postgres
else
    echo "sqlx-cli is already installed"
fi
