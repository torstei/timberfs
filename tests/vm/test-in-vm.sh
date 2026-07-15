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

mounted_empty_rotation() {
    # rotating nothing into a NEW target through the live daemon still
    # creates it — present-but-empty and missing are different signals —
    # with lineage; and --fail-on-empty is relayed (refused with ENODATA)
    timberfs rotate "$BACKING/app.log" quiet.log --cutoff "2000-01-01 00:00" \
        && [ -e "$BACKING/quiet.log.rings" ] \
        && [ "$(stat -c %s "$MNT/quiet.log")" = 0 ] \
        && grep -q '"derived_op": "rotate"' "$BACKING/quiet.log.bark" \
        && ! timberfs rotate "$BACKING/app.log" quiet2.log \
             --cutoff "2000-01-01 00:00" --fail-on-empty 2>/dev/null \
        && [ ! -e "$BACKING/quiet2.log.rings" ]
}

mounted_retention() {
    # declared retention (bark) is enforced by the mount daemon, live: a
    # `timberfs set` while mounted takes effect on the next tick, and
    # O_APPEND writers survive the shrink (kernel attrs invalidated).
    # Own file: retention on a shared fixture breaks downstream tests.
    for i in $(seq 1 20); do seq 1 20000 >> "$MNT/ret.log"; done \
        && timberfs set "$BACKING/ret.log" retain_size=64K > /dev/null \
        && sleep 3 \
        && [ "$(stat -c %s "$BACKING/ret.log.trunk")" -le 262144 ] \
        && echo RETAINED-BUT-ALIVE >> "$MNT/ret.log" \
        && tail -1 "$MNT/ret.log" | grep -q RETAINED-BUT-ALIVE
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
    seq 1 50000 | timberfs append --into "$PIPE_BACKING/piped.log" \
        && seq 1 50000 | cmp - <(timberfs query "$PIPE_BACKING/piped.log")
}

appender_lock_blocks_rotate() {
    mkfifo /tmp/live.fifo
    timberfs append --into "$PIPE_BACKING/live.log" --flush-age 60 < /tmp/live.fifo &
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
    timberfs append --into "$PIPE_BACKING/share-one.log" < /tmp/sh1.fifo &
    SH1_PID=$!
    timberfs append --into "$PIPE_BACKING/share-two.log" < /tmp/sh2.fifo &
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
    seq 1 100000 | timberfs append --into "$PIPE_BACKING/cap.log" --chunk-size 8192 --retain-size 16K
    [ "$(stat -c %s "$PIPE_BACKING/cap.log.trunk")" -le 16384 ] \
        && timberfs query "$PIPE_BACKING/cap.log" | tail -1 | grep -qx 100000
}

info_readonly_nonroot() {
    # info and query are READ-ONLY: a non-root user must be able to
    # inspect a root-owned, world-readable store (the /var/log/timberfs
    # case) without any write access to the backing dir. Regression for
    # the writer-state probe, which used to open the lock O_RDWR|O_CREAT
    # and fail with EACCES.
    id timbertest >/dev/null 2>&1 || useradd -M -s /usr/sbin/nologin timbertest
    local d=/var/log/timberfs-rotest
    rm -rf "$d"
    mkdir -p "$d" # root-owned 0755
    printf '2026-06-08T10:00:00 INFO RONEEDLE hi\n' \
        | timberfs append --into "$d/app.log" --quiet
    # as a non-root user: info succeeds with no error, and query reads it
    runuser -u timbertest -- timberfs info "$d/app.log" > /tmp/ro.out 2>/tmp/ro.err
    local ex=$?
    local rows
    rows=$(runuser -u timbertest -- timberfs query "$d/app.log" 2>/dev/null | grep -c RONEEDLE)
    rm -rf "$d"
    [ "$ex" = 0 ] && [ ! -s /tmp/ro.err ] \
        && grep -q "writer" /tmp/ro.out \
        && [ "$rows" = 1 ]
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
    timberfs import /tmp/old.log --into "$PIPE_BACKING/imported.log" --chunk-size 4096 \
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
    timberfs import /tmp/old.log --into "$PIPE_BACKING/imported.log" 2>&1 \
        | grep -q "already up to date" || return 1
    # grown source: only the delta is appended, byte-exact
    echo "2026-06-03T15:30:00 INFO late event" >> /tmp/old.log
    timberfs import /tmp/old.log --into "$PIPE_BACKING/imported.log" 2>&1 \
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
run_test "mounted empty rotation attests; --fail-on-empty relays" mounted_empty_rotation
run_test "retention --delete empties file" retention_delete
run_test "100k-line integrity + stock-zstd recovery" big_file_integrity
run_test "mounted retention: declared in bark, enforced live" mounted_retention
run_test "compressed on disk (>5x)" compression_on_disk
run_test "systemctl stop timberfs@test" stop_unit
run_test "unmounted and not failed after stop" stopped_cleanly
run_test "offline query after stop" offline_query_after_stop
run_test "restart: data persisted" restart_persists
run_test "appender: pipe 50k lines, query round-trip" appender_roundtrip
records_sink_age_flush() {
    # A records producer trickling below the chunk threshold, with the
    # FIFO held open so `append --records` never sees EOF (the socket
    # intake case): the age timer must make it durable mid-stream, not
    # only when a chunk fills or at EOF.
    mkfifo /tmp/rec.fifo
    timberfs append --records --into "$PIPE_BACKING/rec.log" --flush-age 1 \
        < /tmp/rec.fifo &
    REC_PID=$!
    exec 6>/tmp/rec.fifo
    printf '2026-06-05T09:00:00 INFO trickle one\n' \
        | timber-filter --records --quiet >&6
    sleep 3
    # queryable while the sink is STILL running (before we close the FIFO)
    timberfs query "$PIPE_BACKING/rec.log" | grep -q "trickle one" || return 1
    exec 6>&-
    wait "$REC_PID" || return 1
    rm -f /tmp/rec.fifo
}

run_test "appender: file lock blocks rotate while live" appender_lock_blocks_rotate
run_test "appender: SIGTERM flushes buffered data" appender_sigterm_flushes
run_test "appender: two files share one directory" appenders_share_directory
run_test "appender: --retain-size 16K budget enforced" retain_size_budget
run_test "info/query: read-only, work for a non-root reader" info_readonly_nonroot
run_test "records sink flushes by age, before EOF" records_sink_age_flush

# The socket-activated log-intake units (timberfs-log@.socket/.service):
# exercise the real thing — socket activation, records intake, the
# drop-in override, and the robustness that is the whole point (a
# service restart is invisible to the producer because systemd holds
# the FIFO open O_RDWR).
LOGINST=vmtest
LOGSTORE=/var/log/timberfs/$LOGINST.log
LOGPIPE=/run/timberfs/$LOGINST.pipe

# Frame raw stamped lines as a timberfs-records(5) stream (what the
# --records service expects on the FIFO).
records() { timber-filter --records --quiet; }

# Poll the store (up to ~15s) for a line — the intake is async: socket
# activation starts the service, then the age timer flushes.
store_has() {
    local needle=$1 i=0
    while [ "$i" -lt 15 ]; do
        if timberfs query "$LOGSTORE" 2>/dev/null | grep -q "$needle"; then
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    return 1
}

socket_intake_setup() {
    # /run/timberfs comes from the shipped tmpfiles.d at boot; the package
    # was installed after boot here, so apply it now (the documented
    # before-a-reboot step).
    systemd-tmpfiles --create
    test -d /run/timberfs || return 1
    # An instance drop-in (also exercising the override path the docs lean
    # on): keep the --records default, just flush fast so the test is quick.
    mkdir -p "/etc/systemd/system/timberfs-log@$LOGINST.service.d"
    cat > "/etc/systemd/system/timberfs-log@$LOGINST.service.d/override.conf" << 'EOF'
[Service]
ExecStart=
ExecStart=/usr/bin/timberfs append --records --into /var/log/timberfs/%i.log --flush-age 1
EOF
    systemctl daemon-reload
    # Pre-create the store with a declared index (also makes /var/log/timberfs)
    # so the intake exercises grain maintenance on the live/socket path.
    timberfs create --index "$LOGSTORE" >/dev/null
    systemctl enable --now "timberfs-log@$LOGINST.socket"
    test -p "$LOGPIPE"
}

socket_intake_receives() {
    printf '2026-06-05T09:00:00 INFO socket alpha\n2026-06-05T09:00:01 INFO socket beta\n' \
        | records > "$LOGPIPE"
    # first write socket-activates the service; the age timer makes it durable
    store_has "socket alpha" || return 1
    timberfs query "$LOGSTORE" | grep -q "socket beta" || return 1
    systemctl --quiet is-active "timberfs-log@$LOGINST.service"
}

socket_intake_survives_restart() {
    # A long-lived producer holds the FIFO open; bounce the service under
    # it. systemd keeps the read end (O_RDWR), so the write straddling the
    # restart must NOT get EPIPE, and both entries must land.
    exec 8>"$LOGPIPE"
    if ! printf '2026-06-05T09:02:00 INFO before restart\n' | records >&8; then
        exec 8>&-
        return 1
    fi
    systemctl restart "timberfs-log@$LOGINST.service"
    if ! printf '2026-06-05T09:02:01 INFO after restart\n' | records >&8; then
        exec 8>&-
        return 1
    fi
    exec 8>&-
    store_has "after restart" || return 1
    timberfs query "$LOGSTORE" | grep -q "before restart"
}

socket_intake_index_maintained() {
    # The store was created --index; the streaming sink keeps the grain
    # current while live (not just declared-but-empty), so a --has query
    # is index-accelerated rather than a full scan.
    for _ in $(seq 1 10); do
        [ -f "$LOGSTORE.grain" ] && break
        sleep 1
    done
    [ -f "$LOGSTORE.grain" ] || return 1
    ! timber-filter --has restart "$LOGSTORE" -c 2>&1 >/dev/null | grep -q "no .grain" \
        && [ "$(timber-filter --has restart "$LOGSTORE" -c 2>/dev/null)" -ge 1 ]
}

socket_intake_stop_removes_fifo() {
    systemctl stop "timberfs-log@$LOGINST.socket"
    # RemoveOnStop=yes drops the FIFO node from /run
    test ! -e "$LOGPIPE"
}

run_test "socket intake: tmpfiles + drop-in, socket enabled, FIFO created" socket_intake_setup
run_test "socket intake: records stream lands in the store" socket_intake_receives
run_test "socket intake: producer survives a service restart" socket_intake_survives_restart
run_test "socket intake: declared index maintained while live" socket_intake_index_maintained
run_test "socket intake: stop removes the FIFO" socket_intake_stop_removes_fifo
import_segment_merge() {
    # ship a rotated segment into an archive: verbatim merge, idempotent
    timberfs rotate "$PIPE_BACKING/imported.log" seg-old.log \
        --cutoff "2026-06-03 14:40:00" > /dev/null
    timberfs import "$PIPE_BACKING/seg-old.log" --into "$PIPE_BACKING/archive.log" 2>&1 \
        | grep -q "merged verbatim" || return 1
    timberfs import "$PIPE_BACKING/seg-old.log" --into "$PIPE_BACKING/archive.log" 2>&1 \
        | grep -q "already up to date" || return 1
    timberfs query "$PIPE_BACKING/archive.log" --to "2026-06-03 14:10:00" \
        | grep -q "event number 100"
}

import_leading_backfill() {
    # a file starting mid-entry (rotation cut a stack trace): head lines
    # are backfilled with the first timestamp found
    printf '    at Frame.one\n    at Frame.two\n2026-06-02T08:00:00 INFO head test\n' \
        > /tmp/headless.log
    timberfs import /tmp/headless.log --into "$PIPE_BACKING/headless.log" 2>&1 \
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
    timberfs export "$PIPE_BACKING/archive.log" --into /tmp/win.timber \
        --from "2026-06-03 14:30:00" --to "2026-06-03 14:35:00" \
        && timberfs query /tmp/win.timber | grep -q "event number 1900" \
        && timberfs import /tmp/win.timber --into "$PIPE_BACKING/from-bundle.log" 2>&1 \
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
    timberfs import /tmp/haystack.log --into "$PIPE_BACKING/haystack.log" --chunk-size 4096 --index \
        && [ -s "$PIPE_BACKING/haystack.log.grain" ] \
        && timberfs reindex "$PIPE_BACKING/haystack.log" \
        && timberfs query "$PIPE_BACKING/haystack.log" --has NEEDLE77AB31CD99 2>/tmp/sel.txt \
           | grep -q "NEEDLE77AB31CD99" \
        && SEL=$(grep -oE '^timberfs: [0-9]+' /tmp/sel.txt | grep -oE '[0-9]+') \
        && [ "$SEL" -lt 20 ]
}

run_test "export: window to .timber bundle, import round trip" export_bundle_roundtrip
grep_entry_aware() {
    printf '2026-06-05T08:00:00 ERROR boom\n    at Frame.one\nCaused by: NEEDFRAME\n2026-06-05T08:00:01 INFO fine\n' > /tmp/entries.log
    # stdin: matching a continuation line prints the whole 3-line entry
    [ "$(timber-filter --has NEEDFRAME < /tmp/entries.log | wc -l)" = 3 ] \
        && [ "$(timber-filter -c --not-has ERROR < /tmp/entries.log)" = 1 ] \
        && timber-filter --has NEEDLE77AB31CD99 "$PIPE_BACKING/haystack.log" \
           | grep -q "NEEDLE77AB31CD99"
}

run_test "grain: reindex + --has finds a needle, skipping chunks" grain_needle_search
multi_file_fleet_view() {
    printf '2026-06-06T10:00:00 INFO alpha one\n2026-06-06T10:00:02 INFO alpha two\n' > /tmp/hA.log
    printf '2026-06-06T10:00:01 INFO beta one\n2026-06-06T10:00:03 ERROR beta boom\n' > /tmp/hB.log
    timberfs import /tmp/hA.log --into "$PIPE_BACKING/hA.log" --chunk-size 1 2>/dev/null
    timberfs import /tmp/hB.log --into "$PIPE_BACKING/hB.log" --chunk-size 1 2>/dev/null
    # interleaved and attributed
    OUT=$(timberfs query "$PIPE_BACKING/hA.log" "$PIPE_BACKING/hB.log" 2>/dev/null)
    [ "$(echo "$OUT" | head -2 | grep -c 'one')" = 2 ] \
        && echo "$OUT" | head -1 | grep -q "hA.log:" \
        && echo "$OUT" | sed -n 2p | grep -q "hB.log:" \
        && [ "$(timber-filter --has ERROR -c "$PIPE_BACKING/hA.log" "$PIPE_BACKING/hB.log" 2>/dev/null)" = 1 ]
}

run_test "timber-filter: entry-aware matching, stdin and grain-accelerated source" grep_entry_aware
forgotten_destination_refused() {
    # `import /logs/*` with no --into: a hard argument error, no matter
    # what the glob expanded to; and a plain-file --into is refused too
    printf '2026-06-07T08:00:00 a\n' > /tmp/fg1.log
    printf '2026-06-07T08:00:01 b\n' > /tmp/fg2.log
    if timberfs import /tmp/fg1.log /tmp/fg2.log 2>/tmp/fg.err; then
        return 1
    fi
    grep -q "\-\-into" /tmp/fg.err \
        && [ ! -e /tmp/fg2.log.trunk ] \
        && ! timberfs import /tmp/fg1.log --into /tmp/fg2.log 2>/dev/null \
        && ! echo x | timberfs append --into /tmp/fg1.log 2>/dev/null
}

run_test "multi-file: interleaved attributed query, per-file grep counts" multi_file_fleet_view
sticky_declared_index() {
    # create --index declares; imports maintain the grain with no flag
    printf '2026-06-08T09:00:00 INFO alpha STICKYNEEDLE42
' > /tmp/s1.log
    printf '2026-06-08T09:00:01 INFO beta
' > /tmp/s2.log
    timberfs create "$PIPE_BACKING/sticky.log" --index --set host=vm.test 2>/dev/null \
        && grep -q '"index": true' "$PIPE_BACKING/sticky.log.bark" \
        && grep -qE '"id": "[0-9a-f-]{36}"' "$PIPE_BACKING/sticky.log.bark" \
        && timberfs import /tmp/s1.log --into "$PIPE_BACKING/sticky.log" 2>/dev/null \
        && [ -s "$PIPE_BACKING/sticky.log.grain" ] \
        && timberfs query "$PIPE_BACKING/sticky.log" --has STICKYNEEDLE42 \
           | grep -q STICKYNEEDLE42
}

empty_results_are_results() {
    # a quiet day: the empty export still ships (bark records the asked
    # window), imports as a clean no-op, and --fail-on-empty restores
    # the error
    timberfs export "$PIPE_BACKING/sticky.log" --into /tmp/quietday.timber \
        --from "2031-01-01 00:00" --to "2031-01-02 00:00" 2>/dev/null \
        && tar xOf /tmp/quietday.timber quietday.bark | grep -q '"window_to"' \
        && timberfs import /tmp/quietday.timber --into "$PIPE_BACKING/sticky.log" 2>/tmp/qd.err \
        && grep -q "is empty" /tmp/qd.err \
        && ! timberfs export "$PIPE_BACKING/sticky.log" --into /tmp/nope.timber \
             --from "2031-01-01 00:00" --to "2031-01-02 00:00" --fail-on-empty 2>/dev/null \
        && [ ! -e /tmp/nope.timber ]
}

daily_bulk_load() {
    # day 2 into a non-empty store appends; an overlapping capture is
    # deduplicated line by line; a re-run is a no-op
    printf '2026-06-09T08:00:00 d1 a\n2026-06-09T08:00:01 d1 b\n' > /tmp/bl1.log
    printf '2026-06-09T08:00:01 d1 b\n2026-06-10T08:00:00 d2 c\n' > /tmp/bl2.log
    timberfs import /tmp/bl1.log --into /tmp/blstore/app.log 2>/dev/null \
        && timberfs import /tmp/bl2.log --into /tmp/blstore/app.log 2>/tmp/bl.err \
        && grep -q "1 duplicate line(s) skipped" /tmp/bl.err \
        && [ "$(timberfs query /tmp/blstore/app.log 2>/dev/null | wc -l)" = 3 ] \
        && timberfs import /tmp/bl2.log --into /tmp/blstore/app.log 2>/dev/null \
        && [ "$(timberfs query /tmp/blstore/app.log 2>/dev/null | wc -l)" = 3 ]
}

grep_into_artifact() {
    # the investigation as an artifact: filter | import --records builds
    # a store whose bark records the whole pipe; export bundles it
    printf '2026-06-11T10:00:00 ERROR tenant=FOO boom\n  at deep.Stack\n2026-06-11T10:00:01 INFO tenant=BAR fine\n' > /tmp/gi.log
    timberfs import /tmp/gi.log --into /tmp/gistore/app.log 2>/dev/null \
        && timber-filter --records --has 'tenant=FOO' /tmp/gistore/app.log --quiet \
           | timberfs import --records --into /tmp/gistore/case.log 2>/dev/null \
        && timberfs export /tmp/gistore/case.log --into /tmp/gicase.timber 2>/dev/null \
        && [ "$(timberfs query /tmp/gicase.timber 2>/dev/null | wc -l)" = 2 ] \
        && timberfs query /tmp/gicase.timber 2>/dev/null | grep -q "deep.Stack" \
        && grep -q '"command": "timberfs import --records' /tmp/gistore/case.log.bark \
        && grep -q '"stream_stages": "timber-filter .*tenant=FOO' /tmp/gistore/case.log.bark
}

run_test "write guards: forgotten destination after a glob is refused" forgotten_destination_refused
run_test "bark: create --index makes imports maintain the grain" sticky_declared_index
run_test "empty results are results: export ships, import no-ops" empty_results_are_results
run_test "daily bulk-load: day-2 appends, overlap dedups, re-run no-ops" daily_bulk_load
info_vital_signs() {
    # one screen of truth: data, coverage, index state, writer
    OUT=$(timberfs info "$PIPE_BACKING/sticky.log")         && echo "$OUT" | grep -qE 'identity  [0-9a-f-]{36}'         && echo "$OUT" | grep -qE 'data      .* chunk\(s\)'         && echo "$OUT" | grep -q 'covers    '         && echo "$OUT" | grep -q 'writer    none'         && timberfs info "$PIPE_BACKING/sticky.log" --json | grep -q '"kind": "pair"'
}

time_story() {
    # queries answer in the timestamps you can SEE: entries verified
    # against the window by their own stamps; -0 frames whole entries
    python3 -c "
import datetime
d = datetime.datetime(2026, 6, 12, 9, 0, 0)
with open('/tmp/ts.log', 'w') as f:
    for i in range(5000):
        ts = d + datetime.timedelta(seconds=i)
        if i == 2500:
            f.write(f'{ts.isoformat()} ERROR kaboom\n  at deep.Stack\n')
        else:
            f.write(f'{ts.isoformat()} INFO ok {i}\n')
"         && timberfs import /tmp/ts.log --into "$PIPE_BACKING/ts.log" --chunk-size 4096 --quiet         && [ "$(timberfs query "$PIPE_BACKING/ts.log" --from '2026-06-12 09:10:00' --to '2026-06-12 09:10:04' 2>/dev/null | wc -l)" = 5 ]         && [ "$(timberfs query "$PIPE_BACKING/ts.log" --from '2026-06-12 09:10:00' --to '2026-06-12 09:10:04' --by-write-time 2>/dev/null | wc -l)" -gt 5 ]         && timberfs query "$PIPE_BACKING/ts.log" --from '2026-06-12 09:41:40' --to '2026-06-12 09:41:40' -0 2>/dev/null            | head -zn1 | grep -q "deep.Stack"         && timberfs query "$PIPE_BACKING/ts.log" --from '2026-06-12 09:10:00' --to '2026-06-12 09:10:00' --show-write-time 2>/dev/null            | grep -q '^\[w 2026-06-12'
}

run_test "filter | import --records: the investigation as an artifact" grep_into_artifact
run_test "time story: exact windows, raw escape, -0 records, annotation" time_story
run_test "info: a store's vital signs on one screen" info_vital_signs
run_test "apt-get purge removes package" purge_package
run_test "purge keeps user conf and data, drops package files" purge_correct

echo "TIMBERFS-VM-TESTS: PASS=$PASS FAIL=$FAIL"
if [ "$FAIL" -eq 0 ]; then
    echo "TIMBERFS-VM-TESTS: ALL PASSED"
fi
DONE=1
