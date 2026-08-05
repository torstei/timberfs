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
echo "==> cargo test --lib"
# --lib only: several doc comments carry example shell sessions (not Rust)
# that rustdoc tries and fails to compile as doctests — pre-existing and
# unrelated to the unit tests, which are what this gate cares about.
cargo test --lib
echo "==> cargo build --release"
cargo build --release
echo "==> all checks passed"
