#!/usr/bin/env bash
set -euo pipefail

CHECK_MODE=false
for arg in "$@"; do
  case "$arg" in
    --check)
      CHECK_MODE=true
      ;;
  esac
done

if ! command -v prettier >/dev/null 2>&1; then
  echo "prettier not found. Install with: npm install -g prettier"
  exit 1
fi

if [ "$CHECK_MODE" = true ]; then
  prettier --check "**/*.md"
else
  prettier --write "**/*.md"
fi
