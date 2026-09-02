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
# A VM test that is DEFINED and never registered runs never and says
# nothing, so it rots invisibly — nineteen registrations were once
# deleted with a block rewrite and the suite reported a clean 94 passes
# for a whole OTLP intake that had not been exercised. Static, because
# the VM suite costs ten minutes an iteration.
echo "==> tests/vm: every test function is registered"
defs=$(grep -oE "^[a-z_][a-z0-9_]*\(\)" tests/vm/test-in-vm.sh | tr -d "()" | LC_ALL=C sort -u)
regs=$(grep -oE 'run_test "[^"]*" [a-z_][a-z0-9_]*' tests/vm/test-in-vm.sh |
    awk "{print \$NF}" | LC_ALL=C sort -u)
orphans=""
for f in $(comm -23 <(echo "$defs") <(echo "$regs")); do
    # A helper is fine: what is not run must at least be CALLED.
    [ "$(grep -c "\b$f\b" tests/vm/test-in-vm.sh)" -le 1 ] && orphans="$orphans $f"
done
if [ -n "$orphans" ]; then
    echo "tests/vm/test-in-vm.sh defines these and neither runs nor calls them:$orphans" >&2
    echo "Add a run_test line, or delete them — an unregistered test is not a test." >&2
    exit 1
fi

echo "==> all checks passed"
