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
    apt-get install -y -qq /opt/timberfs.deb zstd jq
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

completion_scripts_installed() {
    test -f /usr/share/bash-completion/completions/timberfs \
        && test -f /usr/share/zsh/vendor-completions/_timberfs \
        && test -f /usr/share/bash-completion/completions/timber-filter \
        && test -f /usr/share/zsh/vendor-completions/_timber-filter
}

run_test "install deb with dependencies" install_package
run_test "binary runs (--version)" timberfs --version
run_test "fuse3 dependency pulled in" command -v fusermount3
run_test "man page installed and gzipped" man_page_installed
run_test "package ships /etc/timberfs" test -f /etc/timberfs/README
run_test "shell completion scripts installed to vendor paths" completion_scripts_installed
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

query_max_and_tail() {
    # --max is an exact hard cap; --tail is chunk-granular last-N entries.
    # A small chunk size spreads 40 lines over several chunks so --tail
    # selects a proper suffix, not the whole store.
    seq 1 40 | sed 's/^/2026-06-08T08:00:00 INFO line /' > /tmp/hl.src
    timberfs import /tmp/hl.src --into "$PIPE_BACKING/hl.log" --chunk-size 512 --quiet
    [ "$(timberfs query "$PIPE_BACKING/hl.log" | wc -l)" = 40 ] || return 1
    # exact cap
    [ "$(timberfs query "$PIPE_BACKING/hl.log" --max 5 | wc -l)" = 5 ] || return 1
    [ "$(timber-filter "$PIPE_BACKING/hl.log" --max 7 | wc -l)" = 7 ] || return 1
    # --tail: at least N, fewer than all (multi-chunk), includes the last entry
    local n
    n=$(timberfs query "$PIPE_BACKING/hl.log" --tail 3 | wc -l)
    [ "$n" -ge 3 ] && [ "$n" -lt 40 ] || return 1
    timberfs query "$PIPE_BACKING/hl.log" --tail 3 | tail -1 | grep -q "line 40"
}

query_follow_live() {
    # A live appender (FIFO held open, fast flush). --follow must pick up
    # entries written AFTER it starts, and not replay ones from before.
    mkfifo /tmp/fl.fifo
    timberfs append --into "$PIPE_BACKING/fl.log" --flush-age 1 < /tmp/fl.fifo &
    local ap=$!
    exec 6>/tmp/fl.fifo
    printf '2026-06-08T08:00:00 INFO seed-line\n' >&6
    sleep 2
    timberfs query "$PIPE_BACKING/fl.log" --follow > /tmp/fl.out 2>/dev/null &
    local fp=$!
    sleep 1
    printf '2026-06-08T08:00:01 INFO live-a\n2026-06-08T08:00:02 INFO live-b\n' >&6
    local got=""
    for _ in $(seq 1 12); do
        sleep 1
        grep -q live-b /tmp/fl.out && { got=yes; break; }
    done
    kill "$fp" 2>/dev/null; wait "$fp" 2>/dev/null
    exec 6>&-; kill "$ap" 2>/dev/null; wait "$ap" 2>/dev/null
    rm -f /tmp/fl.fifo
    [ "$got" = yes ] && grep -q live-a /tmp/fl.out && ! grep -q seed-line /tmp/fl.out
}

run_test "query --max caps exactly; --tail is entry-granular" query_max_and_tail
run_test "query --follow streams new entries live" query_follow_live

# The socket-activated log-intake units (timberfs-log@.socket/.service):
# exercise the real thing — socket activation, records intake, the
# drop-in override, and the robustness that is the whole point (a
# service restart is invisible to the producer because systemd holds
# the FIFO open O_RDWR).
LOGINST=vmtest
LOGSTORE=/var/log/timberfs/$LOGINST/$LOGINST.log
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
ExecStart=/usr/bin/timberfs append --records --into /var/log/timberfs/%i/%i.log --flush-age 1
EOF
    systemctl daemon-reload
    # Pre-create the store with a declared index (also makes the instance dir)
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

forest_handle_resolution() {
    # The package ships /etc/timberfs/forests.d/default.conf with
    # DIR=/var/log/timberfs, so a bare handle names a store under that tree
    # without spelling out the path. Create one (append makes the nested
    # dir), then check handle lookup for query and info, that a full path
    # still works unchanged, and that an unknown handle fails loudly.
    grep -q '^DIR=/var/log/timberfs$' /etc/timberfs/forests.d/default.conf || return 1
    local store=/var/log/timberfs/nginx/nginx.log
    printf '2026-07-07T08:00:00 INFO forest hello\n' \
        | timberfs append --into "$store" >/dev/null 2>&1 || return 1
    # bare handle "nginx" resolves to the nested store nginx/nginx.log
    timberfs query nginx 2>/dev/null | grep -q "forest hello" || return 1
    # info takes the same handle
    timberfs info nginx 2>/dev/null | grep -q "nginx.log" || return 1
    # a full path behaves exactly as before
    timberfs query "$store" 2>/dev/null | grep -q "forest hello" || return 1
    # an unknown handle is an error, not a silent miss
    ! timberfs query no-such-handle-here >/dev/null 2>&1
}

run_test "forest: bare handle resolves query/info; full path unchanged; unknown errors" forest_handle_resolution

forest_list_command() {
    # `timberfs list`: the directory-level complement to `info`. Clear the
    # default forest of state left by earlier tests (the socket-intake
    # instance and forest_handle_resolution's nginx store) so the counts
    # below are exact, then create two nested stores of our own.
    rm -rf /var/log/timberfs/vmtest /var/log/timberfs/nginx
    printf '2026-07-08T09:00:00 INFO web one\n2026-07-08T09:00:01 INFO web two\n' \
        | timberfs append --into /var/log/timberfs/web/web.log --quiet || return 1
    printf '2026-07-08T09:05:00 INFO db one\n' \
        | timberfs append --into /var/log/timberfs/db/db.log --quiet || return 1

    local out names dir_names
    out=$(timberfs list) || return 1
    echo "$out" | head -1 | grep -q '^HANDLE' || return 1
    # a row for each, with a real (non-"empty") SPAN — both stores have data
    echo "$out" | grep -E '^web[[:space:]]+default[[:space:]]' | grep -q ' \.\. ' || return 1
    echo "$out" | grep -E '^db[[:space:]]+default[[:space:]]' | grep -q ' \.\. ' || return 1

    names=$(timberfs list --names | sort | tr '\n' ',')
    [ "$names" = "db,web," ] || return 1

    timberfs list --json > /tmp/list.json || return 1
    jq -e 'length == 2' /tmp/list.json >/dev/null || return 1
    jq -e '([.[].handle] | sort) == ["db","web"]' /tmp/list.json >/dev/null || return 1
    rm -f /tmp/list.json

    # an explicit dir (not necessarily a configured forest) surfaces the
    # same STORES — its FOREST column is the directory itself, not the
    # configured forest's name, so compare handles rather than raw text
    dir_names=$(timberfs list /var/log/timberfs --names | sort | tr '\n' ',')
    [ "$dir_names" = "$names" ] || return 1

    # nice-to-have: a live appender shows WRITER=live; best-effort, never
    # fails the test (the lock-holding window is inherently a race)
    mkfifo /tmp/list-live.fifo
    timberfs append --into /var/log/timberfs/live/live.log --flush-age 60 < /tmp/list-live.fifo &
    local live_pid=$!
    exec 9>/tmp/list-live.fifo
    sleep 0.5
    timberfs list --json 2>/dev/null \
        | jq -e '.[] | select(.handle=="live") | .writer_live == true' >/dev/null 2>&1 \
        || echo "note: live-writer race missed for WRITER=live (non-fatal)"
    exec 9>&-
    wait "$live_pid" 2>/dev/null
    rm -f /tmp/list-live.fifo
    return 0
}

run_test "list: table/--names/--json/explicit-dir agree; WRITER=live best-effort" forest_list_command

# P3: shell completion. forest_list_command already left `web` and `db`
# stores under /var/log/timberfs; touch them again (append is safe to
# repeat) so this section doesn't depend on run order or prior tests.
completion_setup() {
    printf '2026-07-16T09:00:00 INFO web completion fixture\n' \
        | timberfs append --into /var/log/timberfs/web/web.log --quiet \
        && printf '2026-07-16T09:00:00 INFO db completion fixture\n' \
        | timberfs append --into /var/log/timberfs/db/db.log --quiet
}

bash_completion_lists_subcommands() {
    source /usr/share/bash-completion/completions/timberfs
    COMP_WORDS=(timberfs "")
    COMP_CWORD=1
    _timberfs
    printf '%s\n' "${COMPREPLY[@]}" | grep -qx query \
        && printf '%s\n' "${COMPREPLY[@]}" | grep -qx list \
        && printf '%s\n' "${COMPREPLY[@]}" | grep -qx rotate
}

bash_completion_offers_live_handles() {
    source /usr/share/bash-completion/completions/timberfs
    COMP_WORDS=(timberfs query "")
    COMP_CWORD=2
    _timberfs
    printf '%s\n' "${COMPREPLY[@]}" | grep -qx web \
        && printf '%s\n' "${COMPREPLY[@]}" | grep -qx db
}

bash_completion_falls_back_with_no_forests() {
    # empty TIMBERFS_FORESTS: `list --names` prints nothing (still exit 0),
    # so completion must not error and must still offer file completion
    # instead of handles.
    source /usr/share/bash-completion/completions/timberfs
    (
        TIMBERFS_FORESTS=
        export TIMBERFS_FORESTS
        cd /tmp || exit 1
        touch fallback-marker.log
        COMP_WORDS=(timberfs query "fallback-mark")
        COMP_CWORD=2
        _timberfs
        printf '%s\n' "${COMPREPLY[@]}" | grep -q "^fallback-marker.log$"
    )
}

zsh_completion_parses_cleanly() {
    command -v zsh >/dev/null 2>&1 || apt-get install -y -qq zsh || return 1
    zsh -n /usr/share/zsh/vendor-completions/_timberfs
}

run_test "completion setup: touch web/db stores" completion_setup
run_test "bash completion: timberfs <TAB> lists subcommands" bash_completion_lists_subcommands
run_test "bash completion: query <TAB> offers live store handles" bash_completion_offers_live_handles
run_test "bash completion: no forests falls back to file paths, no error" bash_completion_falls_back_with_no_forests
run_test "zsh completion: _timberfs compdef parses without error" zsh_completion_parses_cleanly

# P4: timber-filter handle resolution + its own completion. Reuses the
# `web` store completion_setup left under /var/log/timberfs.
timber_filter_handle_resolution() {
    # a bare handle resolves to the store exactly like `timberfs query web`
    timber-filter web --has "completion fixture" 2>/dev/null \
        | grep -q "web completion fixture" || return 1
    # a full store path still works unchanged
    timber-filter /var/log/timberfs/web/web.log --has "completion fixture" 2>/dev/null \
        | grep -q "web completion fixture" || return 1
    # a raw text file (not a store) still filters as plain text, unaffected
    # — timestamped lines so each is its own entry and --has narrows to one
    printf '2026-07-16T10:00:00 INFO plain line one\n2026-07-16T10:00:01 INFO plain line two\n' \
        > /tmp/tf-raw.log
    [ "$(timber-filter --has "line one" /tmp/tf-raw.log 2>/dev/null)" \
        = "2026-07-16T10:00:00 INFO plain line one" ] || return 1
    # an unknown bare token now fails as "no store", not "no such file"
    timber-filter no-such-handle-here 2>&1 >/dev/null | grep -q "no store" || return 1
    return 0
}

timber_filter_bash_completion_offers_handles() {
    source /usr/share/bash-completion/completions/timber-filter
    COMP_WORDS=(timber-filter "")
    COMP_CWORD=1
    _timber_filter
    printf '%s\n' "${COMPREPLY[@]}" | grep -qx web \
        && printf '%s\n' "${COMPREPLY[@]}" | grep -qx db
}

timber_filter_bash_completion_offers_flags() {
    source /usr/share/bash-completion/completions/timber-filter
    COMP_WORDS=(timber-filter "-")
    COMP_CWORD=1
    _timber_filter
    printf '%s\n' "${COMPREPLY[@]}" | grep -qx -- --has \
        && printf '%s\n' "${COMPREPLY[@]}" | grep -qx -- --records
}

timber_filter_zsh_completion_parses_cleanly() {
    command -v zsh >/dev/null 2>&1 || apt-get install -y -qq zsh || return 1
    zsh -n /usr/share/zsh/vendor-completions/_timber-filter
}

run_test "timber-filter: bare handle resolves; path/raw-file unaffected; unknown errors" timber_filter_handle_resolution
run_test "timber-filter bash completion: <TAB> offers store handles" timber_filter_bash_completion_offers_handles
run_test "timber-filter bash completion: -<TAB> offers flags" timber_filter_bash_completion_offers_flags
run_test "timber-filter zsh completion: _timber-filter compdef parses without error" timber_filter_zsh_completion_parses_cleanly

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
# P5: collapse-range retention. FALLOC_FL_COLLAPSE_RANGE drops the head of
# a store's .trunk in place (peak disk ~1x) instead of remove_head's
# rewrite (peak ~2x) -- proven on a filesystem sized BETWEEN 1x and 2x the
# retain-size cap, where the old rewrite would ENOSPC.
COLLAPSE_IMG=/root/collapse.img
COLLAPSE_MNT=/mnt/collapse-test
COLLAPSE_SRC=/tmp/collapse-src.txt

collapse_space_win_setup() {
    dd if=/dev/zero of="$COLLAPSE_IMG" bs=1M count=60 status=none \
        && mkfs.ext4 -q "$COLLAPSE_IMG" \
        && mkdir -p "$COLLAPSE_MNT" \
        && mount -o loop "$COLLAPSE_IMG" "$COLLAPSE_MNT"
}

collapse_space_win() {
    # High-entropy content barely compresses, so the compressed size tracks
    # the raw size closely -- comfortably past the 40M cap on this 60M
    # filesystem: a size the old rewrite (needing ~2x the cap, ~80M) could
    # never fit, but collapse (~1x, ~40M) does.
    #
    # Retention is only checked once a second, so the feed is throttled
    # (through a FIFO, in small pieces) rather than piped in one instant
    # burst -- a real producer trickles; an instantaneous firehose would
    # overshoot the loopback fs before the first check ever ran, which
    # would ENOSPC regardless of collapse vs. the old rewrite and prove
    # nothing about either.
    local backing="$COLLAPSE_MNT/backing"
    mkdir -p "$backing"
    head -c $((60 * 1024 * 1024)) /dev/urandom | base64 -w200 > "$COLLAPSE_SRC"
    mkfifo /tmp/collapse.fifo
    timberfs append --into "$backing/app.log" --chunk-size 65536 \
        --retain-size 40M --flush-age 1 < /tmp/collapse.fifo &
    local pid=$!
    exec 9>/tmp/collapse.fifo
    split -b 512k "$COLLAPSE_SRC" /tmp/collapse-part-
    for part in /tmp/collapse-part-*; do
        cat "$part" >&9 || break
        sleep 0.15
    done
    exec 9>&-
    wait "$pid" || return 1
    rm -f /tmp/collapse.fifo /tmp/collapse-part-*
    local size
    size=$(stat -c %s "$backing/app.log.trunk")
    [ "$size" -le $((40 * 1024 * 1024)) ] || return 1
    # the tail (most recent data) is intact across the collapse(s)
    local lastline
    lastline=$(tail -1 "$COLLAPSE_SRC")
    timberfs query "$backing/app.log" | tail -1 | grep -qxF "$lastline"
}

collapse_recovery_survives() {
    # Stock zstd still recovers the WHOLE surviving trunk after a collapse
    # (the skippable-frame stamp over the leftover sliver is transparent to
    # it), and timberfs's own index agrees with it exactly.
    local backing="$COLLAPSE_MNT/backing"
    local zcount qcount lastline
    zcount=$(zstd -dc "$backing/app.log.trunk" | wc -l) || return 1
    qcount=$(timberfs query "$backing/app.log" 2>/dev/null | wc -l) || return 1
    lastline=$(tail -1 "$COLLAPSE_SRC")
    [ "$zcount" -gt 0 ] && [ "$zcount" = "$qcount" ] \
        && zstd -dc "$backing/app.log.trunk" | tail -1 | grep -qxF "$lastline"
}

collapse_space_win_teardown() {
    umount "$COLLAPSE_MNT" 2>/dev/null
    rm -f "$COLLAPSE_IMG" "$COLLAPSE_SRC"
    return 0
}

CONC_BACKING=/var/log/timberfs-backing/conc

concurrent_reader_race() {
    # A live appender streaming under a tight cap, retaining (and so
    # collapsing) frequently, while `timberfs query` runs repeatedly in a
    # SEPARATE process: it must only ever print whole, well-formed entries
    # -- never garbage, never a non-zero exit or panic -- across however
    # many collapses land during the run.
    mkfifo /tmp/conc.fifo
    timberfs append --into "$CONC_BACKING/live.log" --chunk-size 4096 \
        --retain-size 32K --flush-age 1 < /tmp/conc.fifo &
    local conc_pid=$!
    exec 9>/tmp/conc.fifo

    # Give the appender a moment to create the backing pair before the
    # query loop starts: a store not existing YET is our own test startup
    # race, not the collapse race this test targets.
    for _ in $(seq 1 20); do
        [ -f "$CONC_BACKING/live.log.rings" ] && break
        sleep 0.1
    done

    (
        n=0
        while [ "$n" -lt 4000 ]; do
            n=$((n + 1))
            printf '2026-07-17T09:00:00 INFO conc line %d %d%d\n' "$n" "$RANDOM" "$RANDOM"
            # Spread the feed over several seconds (many flush/retention
            # ticks, so many collapses) instead of finishing instantly.
            [ $((n % 50)) -eq 0 ] && sleep 0.1
        done >&9
    ) &
    local feeder=$!

    local bad=0 iters=0
    while kill -0 "$feeder" 2>/dev/null; do
        iters=$((iters + 1))
        if ! timberfs query "$CONC_BACKING/live.log" >/tmp/conc.out 2>/tmp/conc.err; then
            echo "query exited non-zero"
            cat /tmp/conc.err
            bad=1
        fi
        if [ -s /tmp/conc.out ] \
            && grep -qvE '^2026-07-17T09:00:00 INFO conc line [0-9]+ [0-9]+$' /tmp/conc.out; then
            echo "malformed/garbage output:"
            grep -vE '^2026-07-17T09:00:00 INFO conc line [0-9]+ [0-9]+$' /tmp/conc.out | head -5
            bad=1
        fi
        [ "$bad" = 1 ] && break
    done
    wait "$feeder" 2>/dev/null
    exec 9>&-
    kill "$conc_pid" 2>/dev/null
    wait "$conc_pid" 2>/dev/null
    rm -f /tmp/conc.out /tmp/conc.err /tmp/conc.fifo
    [ "$bad" = 0 ] && [ "$iters" -gt 0 ]
}

TMPFS_COLLAPSE=/mnt/tmpfs-collapse

tmpfs_fallback_setup() {
    mkdir -p "$TMPFS_COLLAPSE" && mount -t tmpfs -o size=200M tmpfs "$TMPFS_COLLAPSE"
}

tmpfs_fallback_retention() {
    # tmpfs has no FALLOC_FL_COLLAPSE_RANGE (EOPNOTSUPP): retention must
    # fall back to remove_head's rewrite and still succeed, given enough
    # space (200M tmpfs, 256K cap -- ample headroom for the ~2x peak).
    seq 1 200000 | timberfs append --into "$TMPFS_COLLAPSE/app.log" \
        --chunk-size 8192 --retain-size 256K || return 1
    local size
    size=$(stat -c %s "$TMPFS_COLLAPSE/app.log.trunk")
    [ "$size" -le $((256 * 1024)) ] || return 1
    timberfs query "$TMPFS_COLLAPSE/app.log" | tail -1 | grep -qx 200000
}

tmpfs_fallback_teardown() {
    umount "$TMPFS_COLLAPSE" 2>/dev/null
    return 0
}

run_test "collapse: loopback fs setup (60M ext4, between 1x/2x the cap)" collapse_space_win_setup
run_test "collapse: retention succeeds where the old rewrite would ENOSPC" collapse_space_win
run_test "collapse: stock zstd -dc still recovers the whole survivor" collapse_recovery_survives
run_test "collapse: loopback teardown" collapse_space_win_teardown
run_test "collapse: concurrent standalone reader never sees garbage" concurrent_reader_race
run_test "collapse: tmpfs setup (no COLLAPSE_RANGE)" tmpfs_fallback_setup
run_test "collapse: retention falls back to remove_head on tmpfs" tmpfs_fallback_retention
run_test "collapse: tmpfs teardown" tmpfs_fallback_teardown

binary_upgrade_restarts_appender() {
    # Simulate a package upgrade under a live socket intake: replace the
    # binary with a new inode (dpkg's atomic rename, same filesystem). The
    # --exit-on-upgrade daemon must notice, exit 85, and systemd must
    # restart it on the new binary — seamlessly, since the socket holds the
    # FIFO across the swap.
    systemd-tmpfiles --create
    local inst=upgtest
    systemctl enable --now "timberfs-log@$inst.socket" >/dev/null 2>&1
    printf '2026-06-09T10:00:00 INFO before-upgrade\n' \
        | timber-filter --records --quiet > "/run/timberfs/$inst.pipe"
    for _ in $(seq 1 10); do
        systemctl --quiet is-active "timberfs-log@$inst.service" && break
        sleep 1
    done
    local pid1
    pid1=$(systemctl show -p MainPID --value "timberfs-log@$inst.service")
    [ -n "$pid1" ] && [ "$pid1" != 0 ] || { systemctl stop "timberfs-log@$inst.socket" 2>/dev/null; return 1; }
    # systemd-executor forks and reports the service active *before* it execs
    # our binary (LogsDirectory= setup widens that window). Only once MainPID
    # is genuinely running /usr/bin/timberfs does swapping the file replace a
    # *running* daemon's binary — swap any earlier and it just starts fresh on
    # the new inode, with nothing to self-exit for. Capture the inode it runs.
    local oldino=""
    for _ in $(seq 1 40); do
        if [ "$(readlink "/proc/$pid1/exe" 2>/dev/null)" = /usr/bin/timberfs ]; then
            oldino=$(stat -Lc %i "/proc/$pid1/exe" 2>/dev/null)
            break
        fi
        sleep 0.25
    done
    [ -n "$oldino" ] || { systemctl stop "timberfs-log@$inst.socket" 2>/dev/null; return 1; }
    cp /usr/bin/timberfs /usr/bin/timberfs.upg && mv /usr/bin/timberfs.upg /usr/bin/timberfs
    local newino
    newino=$(stat -c %i /usr/bin/timberfs)
    # Give the running daemon a moment to notice, exit 85, and be restarted;
    # then re-drive the intake (also re-activates it if systemd returned the
    # socket to listening). The post-upgrade write must land, and the service
    # must be back running the NEW inode — checked by running inode, not PID,
    # so PID reuse can't fool us.
    sleep 3
    printf '2026-06-09T10:00:05 INFO after-upgrade\n' \
        | timber-filter --records --quiet > "/run/timberfs/$inst.pipe"
    local onnew="" landed=""
    for _ in $(seq 1 15); do
        sleep 1
        local mp
        mp=$(systemctl show -p MainPID --value "timberfs-log@$inst.service")
        [ -n "$mp" ] && [ "$mp" != 0 ] \
            && [ "$(stat -Lc %i "/proc/$mp/exe" 2>/dev/null)" = "$newino" ] && onnew=yes
        timberfs query "/var/log/timberfs/$inst/$inst.log" 2>/dev/null | grep -q after-upgrade && landed=yes
        [ "$onnew" = yes ] && [ "$landed" = yes ] && break
    done
    local before=no
    timberfs query "/var/log/timberfs/$inst/$inst.log" 2>/dev/null | grep -q before-upgrade && before=yes
    local rc=1
    [ "$oldino" != "$newino" ] && [ "$onnew" = yes ] && [ "$landed" = yes ] && [ "$before" = yes ] \
        && ! systemctl --quiet is-failed "timberfs-log@$inst.service" && rc=0
    systemctl stop "timberfs-log@$inst.socket" 2>/dev/null
    return $rc
}

binary_upgrade_restarts_mount() {
    # Same, for a mount: on the binary swap the daemon exits 85,
    # auto_unmount tears the FUSE mount down, and systemd remounts on the
    # new binary (RestartForceExitStatus, despite Restart=on-failure).
    mkdir -p /etc/timberfs
    printf 'BACKING=/var/log/timberfs-backing/upmnt\nMOUNTPOINT=/var/log/upmnt\n' \
        > /etc/timberfs/upmnt.conf
    systemctl start timberfs@upmnt >/dev/null 2>&1
    for _ in $(seq 1 20); do mountpoint -q /var/log/upmnt && break; sleep 0.5; done
    mountpoint -q /var/log/upmnt || { systemctl stop timberfs@upmnt 2>/dev/null; return 1; }
    echo "upmnt data" > /var/log/upmnt/x.log
    local pid1
    pid1=$(systemctl show -p MainPID --value timberfs@upmnt)
    [ -n "$pid1" ] && [ "$pid1" != 0 ] || { systemctl stop timberfs@upmnt 2>/dev/null; return 1; }
    # The mount being up already proves the daemon is fully running on the old
    # binary (no systemd-executor pre-exec window to race), so we can capture
    # the inode it runs and swap straight away.
    local oldino
    oldino=$(stat -Lc %i "/proc/$pid1/exe" 2>/dev/null)
    cp /usr/bin/timberfs /usr/bin/timberfs.upg2 && mv /usr/bin/timberfs.upg2 /usr/bin/timberfs
    local newino
    newino=$(stat -c %i /usr/bin/timberfs)
    # Comes back running the NEW inode and remounted — verified by running
    # inode, not PID, so PID reuse can't fool us.
    local onnew=""
    for _ in $(seq 1 15); do
        sleep 1
        local mp
        mp=$(systemctl show -p MainPID --value timberfs@upmnt)
        [ -n "$mp" ] && [ "$mp" != 0 ] \
            && [ "$(stat -Lc %i "/proc/$mp/exe" 2>/dev/null)" = "$newino" ] \
            && mountpoint -q /var/log/upmnt && { onnew=yes; break; }
    done
    local rc=1
    [ "$oldino" != "$newino" ] && [ "$onnew" = yes ] \
        && mountpoint -q /var/log/upmnt \
        && ! systemctl --quiet is-failed timberfs@upmnt \
        && grep -q "upmnt data" /var/log/upmnt/x.log && rc=0
    systemctl stop timberfs@upmnt 2>/dev/null
    return $rc
}

run_test "upgrade: appender self-exits, systemd restarts it on the new binary" binary_upgrade_restarts_appender
run_test "upgrade: mount self-exits, remounts on the new binary" binary_upgrade_restarts_mount
run_test "apt-get purge removes package" purge_package
run_test "purge keeps user conf and data, drops package files" purge_correct

echo "TIMBERFS-VM-TESTS: PASS=$PASS FAIL=$FAIL"
if [ "$FAIL" -eq 0 ]; then
    echo "TIMBERFS-VM-TESTS: ALL PASSED"
fi
DONE=1
