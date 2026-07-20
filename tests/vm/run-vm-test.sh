#!/usr/bin/env bash
# Boot a disposable Debian VM under QEMU and run the timberfs package test
# suite (tests/vm/test-in-vm.sh) inside it: install the .deb, exercise the
# systemd unit, filesystem, queries, and rotation, verify clean shutdown.
#
#   cargo deb && tests/vm/run-vm-test.sh [path/to/timberfs.deb]
#
# The Debian cloud base image (~400 MB) is downloaded once into
# ~/.cache/timberfs-vm-tests/. Each run boots a fresh qcow2 overlay — the
# base image is never modified and no state survives between runs. Exits 0
# iff every test inside the VM passed. Needs qemu-system-x86_64, qemu-img
# and genisoimage; uses KVM when /dev/kvm is writable, else falls back to
# slow emulation.
set -euo pipefail

cd "$(dirname "$0")/../.."

DEB=${1:-}
if [ -z "$DEB" ]; then
    DEB=$(ls -t target/debian/timberfs_*_amd64.deb 2>/dev/null | head -1 || true)
fi
if [ -z "$DEB" ] || [ ! -f "$DEB" ]; then
    echo "no .deb found — run 'cargo deb' first (or pass the path)" >&2
    exit 2
fi

# When we auto-picked the newest .deb, guard against silently testing a stale
# one: if any packaged source is newer than it, someone forgot to re-run
# 'cargo deb' and the VM would exercise the wrong binary. Warn loudly rather
# than quietly mislead. (An explicit path is the caller's own choice.)
if [ -z "${1:-}" ]; then
    STALE=$(find src packaging Cargo.toml Cargo.lock -type f -newer "$DEB" -print -quit 2>/dev/null || true)
    if [ -n "$STALE" ]; then
        echo "WARNING: $DEB is older than tracked sources (e.g. $STALE)." >&2
        echo "         It may not reflect your changes — run 'cargo deb' to rebuild." >&2
    fi
fi

CACHE=${TIMBERFS_VM_CACHE:-$HOME/.cache/timberfs-vm-tests}
# Base image: Debian trixie by default. TIMBERFS_VM_IMAGE selects a named
# preset; TIMBERFS_VM_IMG_URL overrides the URL outright. Focal (Ubuntu 20.04:
# systemd 245, glibc 2.31) is the oldest release the compat .deb targets — run
# it to prove the package actually WORKS there (units start, mounts hold), not
# just that dpkg lets it install.
case "${TIMBERFS_VM_IMAGE:-trixie}" in
    trixie) IMG_URL=https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2 ;;
    focal)  IMG_URL=https://cloud-images.ubuntu.com/focal/current/focal-server-cloudimg-amd64.img ;;
    jammy)  IMG_URL=https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.img ;;
    noble)  IMG_URL=https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img ;;
    *) echo "unknown TIMBERFS_VM_IMAGE '${TIMBERFS_VM_IMAGE}' (trixie|focal|jammy|noble)" >&2; exit 2 ;;
esac
IMG_URL=${TIMBERFS_VM_IMG_URL:-$IMG_URL}
BASE=$CACHE/$(basename "$IMG_URL")
WORK=target/vm-test
TIMEOUT=${TIMBERFS_VM_TIMEOUT:-1200}

mkdir -p "$CACHE"
rm -rf "$WORK"
mkdir -p "$WORK"

if [ ! -f "$BASE" ]; then
    echo "downloading base image (one-time): $IMG_URL"
    curl -fL --progress-bar -o "$BASE.tmp" "$IMG_URL"
    mv "$BASE.tmp" "$BASE"
fi

qemu-img create -q -f qcow2 -b "$BASE" -F qcow2 "$WORK/disk.qcow2" 8G

cat > "$WORK/meta-data" << EOF
instance-id: timberfs-test-$(date +%s)
local-hostname: timberfs-test
EOF

cat > "$WORK/user-data" << EOF
#cloud-config
write_files:
  - path: /opt/timberfs.deb
    encoding: b64
    content: $(base64 -w0 "$DEB")
    permissions: '0644'
  - path: /opt/test-in-vm.sh
    encoding: b64
    content: $(base64 -w0 tests/vm/test-in-vm.sh)
    permissions: '0755'
runcmd:
  - [bash, /opt/test-in-vm.sh]
EOF

genisoimage -quiet -output "$WORK/seed.iso" -volid cidata -joliet -rock \
    "$WORK/user-data" "$WORK/meta-data"

KVM_ARGS=""
if [ -w /dev/kvm ]; then
    KVM_ARGS="-enable-kvm -cpu host"
else
    echo "warning: /dev/kvm not writable — using TCG emulation, this will be slow" >&2
fi

echo "booting test VM ($(basename "$DEB"))..."
set +e
# Two serial ports: ttyS0 (serial.log) is the kernel console + serial-getty;
# ttyS1 (test.log) is a dedicated channel for the test script's output, which
# serial-getty never owns — so results can't be lost to a getty vhangup race.
# shellcheck disable=SC2086
timeout "$TIMEOUT" qemu-system-x86_64 $KVM_ARGS \
    -m 1024 -smp 2 \
    -display none \
    -serial file:"$WORK/serial.log" \
    -serial file:"$WORK/test.log" \
    -drive file="$WORK/disk.qcow2",if=virtio,format=qcow2 \
    -drive file="$WORK/seed.iso",if=virtio,format=raw,read-only=on \
    -nic user,model=virtio-net-pci \
    -no-reboot
QEMU_RC=$?
set -e

# Read the verdict from the results port (ttyS1 -> test.log), not the console.
# Materialize the CR-stripped file ONCE and grep the file: no pipes in the
# verdict path, since piping into grep -q under pipefail can flip the verdict
# when tr catches EPIPE on grep's early exit.
CLEAN="$WORK/test.clean"
tr -d '\r' < "$WORK/test.log" > "$CLEAN"

if [ "$QEMU_RC" = 124 ]; then
    echo "=== VM timed out after ${TIMEOUT}s ===" >&2
    echo "--- results (ttyS1) ---" >&2
    tail -30 "$CLEAN" >&2
    echo "--- boot log (ttyS0) ---" >&2
    tr -d '\r' < "$WORK/serial.log" | tail -30 >&2
    exit 1
fi

echo "=== test results ==="
grep -E "^(TEST (PASS|FAIL)|TIMBERFS-VM-TESTS)" "$CLEAN" || {
    echo "no test output on the results port (ttyS1); boot log tail:" >&2
    tr -d '\r' < "$WORK/serial.log" | tail -30 >&2
    exit 1
}

if grep -q "^TIMBERFS-VM-TESTS: ALL PASSED" "$CLEAN"; then
    echo "OK (logs: $WORK/test.log, $WORK/serial.log)"
    exit 0
else
    echo "FAILED — failing test output above; logs: $WORK/test.log, $WORK/serial.log" >&2
    grep -A5 "^TEST FAIL" "$CLEAN" >&2 || true
    exit 1
fi
