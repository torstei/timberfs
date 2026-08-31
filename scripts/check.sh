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
echo "==> cargo test"
# Doctests gate too: shell examples in doc comments are ```text-fenced
# (rustdoc skips them); any Rust example in docs must compile.
cargo test
echo "==> cargo build --release"
cargo build --release
# timbersh's own tests. Seconds, and they need no VM and no timberfs —
# `--cmd` points at a fake that answers from a script and records what it
# was asked, and the url transport's cases serve that same fake over a
# loopback port and a unix socket in the test process. Gated here rather
# than in the VM suite because nothing about them needs a package.
echo "==> tests/timberfs-client/test-timberfs-client"
tests/timberfs-client/test-timberfs-client
echo "==> tests/timbersh/test-timbersh"
tests/timbersh/test-timbersh
echo "==> tests/timberview/test-timberview"
tests/timberview/test-timberview
echo "==> all checks passed"
