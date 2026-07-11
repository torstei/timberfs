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

CACHE=${TIMBERFS_VM_CACHE:-$HOME/.cache/timberfs-vm-tests}
IMG_URL=https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2
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
  # agetty vhangup()s ttyS0 when it starts, killing the test script's
  # already-open fd on the serial console — keep the port to ourselves
  - [systemctl, mask, --now, serial-getty@ttyS0.service]
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
# shellcheck disable=SC2086
timeout "$TIMEOUT" qemu-system-x86_64 $KVM_ARGS \
    -m 1024 -smp 2 \
    -display none \
    -serial file:"$WORK/serial.log" \
    -drive file="$WORK/disk.qcow2",if=virtio,format=qcow2 \
    -drive file="$WORK/seed.iso",if=virtio,format=raw,read-only=on \
    -nic user,model=virtio-net-pci \
    -no-reboot
QEMU_RC=$?
set -e

# Materialize the CR-stripped log ONCE: piping it into an early-exiting
# consumer (grep -q) under pipefail flips the verdict to FAILED when tr
# catches EPIPE — a pipe-buffer race that only shows up once the log
# grows big enough. No pipes in the verdict path.
CLEAN="$WORK/serial.clean"
tr -d '\r' < "$WORK/serial.log" > "$CLEAN"

if [ "$QEMU_RC" = 124 ]; then
    echo "=== VM timed out after ${TIMEOUT}s; last serial output: ===" >&2
    tail -30 "$CLEAN" >&2
    exit 1
fi

echo "=== test results ==="
grep -E "^(TEST (PASS|FAIL)|TIMBERFS-VM-TESTS)" "$CLEAN" || {
    echo "no test output found on serial console; last serial output:" >&2
    tail -30 "$CLEAN" >&2
    exit 1
}

if grep -q "^TIMBERFS-VM-TESTS: ALL PASSED" "$CLEAN"; then
    echo "OK (full log: $WORK/serial.log)"
    exit 0
else
    echo "FAILED — failing test output above; full log: $WORK/serial.log" >&2
    grep -A5 "^TEST FAIL" "$CLEAN" >&2 || true
    exit 1
fi
