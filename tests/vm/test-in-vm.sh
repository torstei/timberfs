#!/usr/bin/env bash
# Runs INSIDE the disposable test VM (delivered via cloud-init, executed as
# root by runcmd). Installs the timberfs .deb from /opt, exercises the
# package + systemd unit end to end, reports one "TEST PASS/FAIL: ..." line
# per case on the serial console, then powers the VM off. The host-side
# harness greps the serial log for the final ALL PASSED marker.
exec > /dev/ttyS0 2>&1
set -u

PASS=0
FAIL=0
DONE=0
CUT=""
TMPOUT=/tmp/test-output

# Power off no matter how we exit (a set -u abort included), so the host
# harness never has to wait for its timeout.
on_exit() {
    if [ "$DONE" != 1 ]; then
        echo "TIMBERFS-VM-TESTS: script aborted (PASS=$PASS FAIL=$FAIL so far)"
    fi
    sync
    sleep 2
    poweroff
}
trap on_exit EXIT

run_test() {
    local name=$1
    shift
    if "$@" >"$TMPOUT" 2>&1; then
        echo "TEST PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "TEST FAIL: $name"
        sed 's/^/    /' "$TMPOUT"
        FAIL=$((FAIL + 1))
    fi
}

BACKING=/var/log/timberfs-backing/test
MNT=/var/log/testlogs

install_package() {
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq /opt/timberfs.deb zstd
}

configure_instance() {
    mkdir -p /etc/timberfs
    cat > /etc/timberfs/test.conf << EOF
BACKING=$BACKING
MOUNTPOINT=$MNT
EXTRA_OPTS=--allow-other
EOF
}

start_unit() {
    systemctl enable --now timberfs@test
}

wait_mounted() {
    for _ in $(seq 1 20); do
        if mountpoint -q "$MNT"; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}

write_batches() {
    echo "batch-one line 1" >> "$MNT/app.log"
    echo "batch-one line 2" >> "$MNT/app.log"
    sleep 2
    CUT=$(date +%H:%M:%S)
    sleep 2
    echo "batch-two line 1" >> "$MNT/app.log"
    echo "batch-two line 2" >> "$MNT/app.log"
    grep -q "batch-one line 1" "$MNT/app.log" && grep -c "line" "$MNT/app.log" | grep -qx 4
}

query_after_cut() {
    timberfs query "$BACKING/app.log" --from "$CUT" | grep -q "batch-two" \
        && ! timberfs query "$BACKING/app.log" --from "$CUT" | grep -q "batch-one"
}

query_before_cut() {
    timberfs query "$BACKING/app.log" --to "$CUT" | grep -q "batch-one" \
        && ! timberfs query "$BACKING/app.log" --to "$CUT" | grep -q "batch-two"
}

online_rotate() {
    timberfs rotate "$BACKING/app.log" archive.log --cutoff "$CUT"
}

rotate_split_correct() {
    grep -q "batch-one" "$MNT/archive.log" \
        && ! grep -q "batch-two" "$MNT/archive.log" \
        && grep -q "batch-two" "$MNT/app.log" \
        && ! grep -q "batch-one" "$MNT/app.log"
}

retention_delete() {
    # unix-seconds cutoff in the future: drop everything in archive.log
    timberfs rotate "$BACKING/archive.log" --delete --cutoff "$(($(date +%s) + 3600))" \
        && [ "$(stat -c %s "$MNT/archive.log")" = 0 ]
}

big_file_integrity() {
    seq 1 100000 > "$MNT/big.log"
    seq 1 100000 | cmp - "$MNT/big.log" || return 1
    # backing must be recoverable with stock zstd, byte for byte
    seq 1 100000 | cmp - <(zstd -dc "$BACKING/big.log.trunk")
}

compression_on_disk() {
    local logical physical
    logical=$(stat -c %s "$MNT/big.log")
    physical=$(stat -c %s "$BACKING/big.log.trunk")
    [ "$physical" -lt $((logical / 5)) ]
}

stop_unit() {
    systemctl stop timberfs@test
}

stopped_cleanly() {
    ! mountpoint -q "$MNT" \
        && ! systemctl --quiet is-failed timberfs@test
}

offline_query_after_stop() {
    timberfs query "$BACKING/app.log" | grep -q "batch-two"
}

restart_persists() {
    systemctl start timberfs@test \
        && wait_mounted \
        && grep -q "batch-two" "$MNT/app.log" \
        && seq 1 100000 | cmp - "$MNT/big.log"
}

PIPE_BACKING=/var/log/timberfs-backing/pipe

appender_roundtrip() {
    seq 1 50000 | timberfs append "$PIPE_BACKING/piped.log" \
        && seq 1 50000 | cmp - <(timberfs query "$PIPE_BACKING/piped.log")
}

appender_lock_blocks_rotate() {
    mkfifo /tmp/live.fifo
    timberfs append "$PIPE_BACKING/live.log" --flush-age 60 < /tmp/live.fifo &
    LIVE_PID=$!
    exec 9>/tmp/live.fifo
    echo "live line" >&9
    sleep 1
    # rotation must be refused while the appender holds the dir lock
    if timberfs rotate "$PIPE_BACKING/live.log" dst.log --cutoff 23:59 2>/dev/null; then
        return 1
    fi
    return 0
}

appender_sigterm_flushes() {
    # data is 60s from an age flush, so only the SIGTERM path makes it durable
    kill -TERM "$LIVE_PID"
    wait "$LIVE_PID" || return 1
    exec 9>&-
    rm -f /tmp/live.fifo
    timberfs query "$PIPE_BACKING/live.log" | grep -q "live line"
}

appenders_share_directory() {
    mkfifo /tmp/sh1.fifo /tmp/sh2.fifo
    timberfs append "$PIPE_BACKING/share-one.log" < /tmp/sh1.fifo &
    SH1_PID=$!
    timberfs append "$PIPE_BACKING/share-two.log" < /tmp/sh2.fifo &
    SH2_PID=$!
    exec 7>/tmp/sh1.fifo 8>/tmp/sh2.fifo
    echo "one" >&7
    echo "two" >&8
    sleep 1
    kill -0 "$SH1_PID" && kill -0 "$SH2_PID" || return 1
    exec 7>&- 8>&-
    wait "$SH1_PID" && wait "$SH2_PID" || return 1
    rm -f /tmp/sh1.fifo /tmp/sh2.fifo
    timberfs query "$PIPE_BACKING/share-one.log" | grep -qx one \
        && timberfs query "$PIPE_BACKING/share-two.log" | grep -qx two
}

retain_size_budget() {
    seq 1 100000 | timberfs append "$PIPE_BACKING/cap.log" --chunk-size 8192 --retain-size 16K
    [ "$(stat -c %s "$PIPE_BACKING/cap.log.trunk")" -le 16384 ] \
        && timberfs query "$PIPE_BACKING/cap.log" | tail -1 | grep -qx 100000
}

import_historical_log() {
    python3 -c "
import datetime
d = datetime.datetime(2026, 6, 3, 14, 0, 0)
with open('/tmp/old.log', 'w') as f:
    for i in range(5000):
        ts = d + datetime.timedelta(seconds=i)
        f.write(f'{ts.isoformat()} INFO event number {i}\n')
"
    # small chunks so the 83-minute file spans many windows, not one
    timberfs import /tmp/old.log "$PIPE_BACKING/imported.log" --chunk-size 4096 \
        && zstd -dc "$PIPE_BACKING/imported.log.trunk" | cmp - /tmp/old.log \
        && timberfs query "$PIPE_BACKING/imported.log" \
               --from "2026-06-03 14:30:00" --to "2026-06-03 14:31:00" \
           | grep -q "event number 1800" \
        && ! timberfs query "$PIPE_BACKING/imported.log" \
               --from "2026-06-03 14:30:00" --to "2026-06-03 14:31:00" \
           | grep -q "event number 3000"
}

import_resume_grown() {
    # identical re-import: verified no-op
    timberfs import /tmp/old.log "$PIPE_BACKING/imported.log" 2>&1 \
        | grep -q "already up to date" || return 1
    # grown source: only the delta is appended, byte-exact
    echo "2026-06-03T15:30:00 INFO late event" >> /tmp/old.log
    timberfs import /tmp/old.log "$PIPE_BACKING/imported.log" 2>&1 \
        | grep -q "imported 1 lines" || return 1
    zstd -dc "$PIPE_BACKING/imported.log.trunk" | cmp - /tmp/old.log
}

purge_package() {
    systemctl disable --now timberfs@test
    apt-get purge -y -qq timberfs
}

purge_correct() {
    [ ! -e /usr/bin/timberfs ] \
        && [ ! -e /lib/systemd/system/timberfs@.service ] \
        && [ ! -e /etc/timberfs/README ] \
        && [ -f /etc/timberfs/test.conf ] \
        && [ -f "$BACKING/app.log.trunk" ]
}

echo "TIMBERFS-VM-TESTS: starting on $(uname -r), $(. /etc/os-release && echo "$PRETTY_NAME")"

man_page_installed() {
    zcat /usr/share/man/man1/timberfs.1.gz | grep -q "^.TH TIMBERFS 1"
}

run_test "install deb with dependencies" install_package
run_test "binary runs (--version)" timberfs --version
run_test "fuse3 dependency pulled in" command -v fusermount3
run_test "man page installed and gzipped" man_page_installed
run_test "package ships /etc/timberfs" test -f /etc/timberfs/README
configure_instance
run_test "systemctl enable --now timberfs@test" start_unit
run_test "mountpoint appears" wait_mounted
run_test "unit is active" systemctl --quiet is-active timberfs@test
run_test "append and read back through mount" write_batches
run_test "time query: --from cut finds only batch-two" query_after_cut
run_test "time query: --to cut finds only batch-one" query_before_cut
run_test "online rotation through live mount" online_rotate
run_test "rotation split is correct" rotate_split_correct
run_test "retention --delete empties file" retention_delete
run_test "100k-line integrity + stock-zstd recovery" big_file_integrity
run_test "compressed on disk (>5x)" compression_on_disk
run_test "systemctl stop timberfs@test" stop_unit
run_test "unmounted and not failed after stop" stopped_cleanly
run_test "offline query after stop" offline_query_after_stop
run_test "restart: data persisted" restart_persists
run_test "appender: pipe 50k lines, query round-trip" appender_roundtrip
run_test "appender: file lock blocks rotate while live" appender_lock_blocks_rotate
run_test "appender: SIGTERM flushes buffered data" appender_sigterm_flushes
run_test "appender: two files share one directory" appenders_share_directory
run_test "appender: --retain-size 16K budget enforced" retain_size_budget
import_segment_merge() {
    # ship a rotated segment into an archive: verbatim merge, idempotent
    timberfs rotate "$PIPE_BACKING/imported.log" seg-old.log \
        --cutoff "2026-06-03 14:40:00" > /dev/null
    timberfs import "$PIPE_BACKING/seg-old.log" "$PIPE_BACKING/archive.log" 2>&1 \
        | grep -q "merged verbatim" || return 1
    timberfs import "$PIPE_BACKING/seg-old.log" "$PIPE_BACKING/archive.log" 2>&1 \
        | grep -q "already up to date" || return 1
    timberfs query "$PIPE_BACKING/archive.log" --to "2026-06-03 14:10:00" \
        | grep -q "event number 100"
}

import_leading_backfill() {
    # a file starting mid-entry (rotation cut a stack trace): head lines
    # are backfilled with the first timestamp found
    printf '    at Frame.one\n    at Frame.two\n2026-06-02T08:00:00 INFO head test\n' \
        > /tmp/headless.log
    timberfs import /tmp/headless.log "$PIPE_BACKING/headless.log" 2>&1 \
        | grep -q "(1 stamped, 2 inherited)" \
        && timberfs query "$PIPE_BACKING/headless.log" --to "2026-06-02 08:00:00" \
           | grep -q "Frame.one"
}

run_test "import: historical log queryable by logged time" import_historical_log
run_test "import: mid-entry file head backfilled with first stamp" import_leading_backfill
run_test "import: idempotent re-import and grown-source resume" import_resume_grown
export_bundle_roundtrip() {
    # export a window as a .timber bundle, query it in place, import it
    # elsewhere, compare (archive.log holds the pre-14:40 chunks, which
    # the segment-merge test rotated out of imported.log)
    timberfs export "$PIPE_BACKING/archive.log" /tmp/win.timber \
        --from "2026-06-03 14:30:00" --to "2026-06-03 14:35:00" \
        && timberfs query /tmp/win.timber | grep -q "event number 1900" \
        && timberfs import /tmp/win.timber "$PIPE_BACKING/from-bundle.log" 2>&1 \
           | grep -q "merged verbatim" \
        && timberfs query "$PIPE_BACKING/from-bundle.log" | grep -q "event number 1900" \
        && tar tf /tmp/win.timber | head -1 | grep -q ".rings"
}

run_test "import: shipped segment merges verbatim, idempotently" import_segment_merge
grain_needle_search() {
    python3 -c "
import datetime
d = datetime.datetime(2026, 6, 4, 9, 0, 0)
with open('/tmp/haystack.log', 'w') as f:
    for i in range(20000):
        ts = (d + datetime.timedelta(seconds=i)).isoformat()
        if i == 15000:
            f.write(f'{ts} INFO request NEEDLE77AB31CD99 handled\n')
        else:
            f.write(f'{ts} INFO routine work {i}\n')
"
    timberfs import /tmp/haystack.log "$PIPE_BACKING/haystack.log" --chunk-size 4096 \
        && timberfs reindex "$PIPE_BACKING/haystack.log" \
        && [ -s "$PIPE_BACKING/haystack.log.grain" ] \
        && timberfs query "$PIPE_BACKING/haystack.log" --has NEEDLE77AB31CD99 2>/tmp/sel.txt \
           | grep -q "NEEDLE77AB31CD99" \
        && SEL=$(grep -oE '^timberfs: [0-9]+' /tmp/sel.txt | grep -oE '[0-9]+') \
        && [ "$SEL" -lt 20 ]
}

run_test "export: window to .timber bundle, import round trip" export_bundle_roundtrip
grep_entry_aware() {
    printf '2026-06-05T08:00:00 ERROR boom\n    at Frame.one\nCaused by: NEEDFRAME\n2026-06-05T08:00:01 INFO fine\n' > /tmp/entries.log
    # stdin: matching a continuation line prints the whole 3-line entry
    [ "$(timberfs grep NEEDFRAME < /tmp/entries.log | wc -l)" = 3 ] \
        && [ "$(timberfs grep -c -v ERROR < /tmp/entries.log)" = 1 ] \
        && timberfs grep NEEDLE77AB31CD99 "$PIPE_BACKING/haystack.log" \
               --has NEEDLE77AB31CD99 | grep -q "NEEDLE77AB31CD99"
}

run_test "grain: reindex + --has finds a needle, skipping chunks" grain_needle_search
multi_file_fleet_view() {
    printf '2026-06-06T10:00:00 INFO alpha one\n2026-06-06T10:00:02 INFO alpha two\n' > /tmp/hA.log
    printf '2026-06-06T10:00:01 INFO beta one\n2026-06-06T10:00:03 ERROR beta boom\n' > /tmp/hB.log
    timberfs import /tmp/hA.log "$PIPE_BACKING/hA.log" --chunk-size 1 2>/dev/null
    timberfs import /tmp/hB.log "$PIPE_BACKING/hB.log" --chunk-size 1 2>/dev/null
    # interleaved and attributed
    OUT=$(timberfs query "$PIPE_BACKING/hA.log" "$PIPE_BACKING/hB.log" 2>/dev/null)
    [ "$(echo "$OUT" | head -2 | grep -c 'one')" = 2 ] \
        && echo "$OUT" | head -1 | grep -q "hA.log:" \
        && echo "$OUT" | sed -n 2p | grep -q "hB.log:" \
        && timberfs grep -c ERROR "$PIPE_BACKING/hA.log" "$PIPE_BACKING/hB.log" \
           | grep -q "hB.log:1"
}

run_test "grep: entry-aware matching, stdin and grain-accelerated source" grep_entry_aware
run_test "multi-file: interleaved attributed query, per-file grep counts" multi_file_fleet_view
run_test "apt-get purge removes package" purge_package
run_test "purge keeps user conf and data, drops package files" purge_correct

echo "TIMBERFS-VM-TESTS: PASS=$PASS FAIL=$FAIL"
if [ "$FAIL" -eq 0 ]; then
    echo "TIMBERFS-VM-TESTS: ALL PASSED"
fi
DONE=1
