#!/usr/bin/env bash
# Run the same gates as the CI `checks` job, locally — before pushing.
#
#   scripts/check.sh
#
# To run it automatically on every push, enable the tracked pre-push hook once
# per clone:
#
#   git config core.hooksPath .githooks
#
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt -- --check"
cargo fmt -- --check
echo "==> cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings
echo "==> cargo build --release"
cargo build --release
echo "==> all checks passed"
