#!/usr/bin/env bash
# Runs INSIDE the disposable test VM (delivered via cloud-init, executed as
# root by runcmd). Installs the timberfs .deb from /opt, exercises the
# package + systemd unit end to end, reports one "TEST PASS/FAIL: ..." line
# per case on a dedicated serial port (ttyS1), then powers the VM off. The
# host-side harness reads that port for the final ALL PASSED marker.
set -u

PASS=0
FAIL=0
DONE=0
CUT=""
TMPOUT=/tmp/test-output

# Power off no matter how we exit (a set -u abort or a failed redirect
# included), so the host harness never waits for its timeout. Installed
# BEFORE the redirect below, so poweroff still runs even if that fails.
on_exit() {
    if [ "$DONE" != 1 ]; then
        echo "TIMBERFS-VM-TESTS: script aborted (PASS=$PASS FAIL=$FAIL so far)"
    fi
    sync
    sleep 2
    poweroff
}
trap on_exit EXIT

# Results go to ttyS1, a port serial-getty never owns (ttyS0 is the console),
# so they can't be lost to a getty vhangup race.
exec > /dev/ttyS1 2>&1

run_test() {
    local name=$1
    shift
    local start=$SECONDS
    if "$@" >"$TMPOUT" 2>&1; then
        echo "TEST PASS: $name ($((SECONDS - start))s)"
        PASS=$((PASS + 1))
    else
        echo "TEST FAIL: $name ($((SECONDS - start))s)"
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

mounted_grain_maintained() {
    # The mount daemon maintains a declared index too — same contract as
    # the appender and the intakes. Own file: the index declaration must
    # not follow the shared fixture into downstream tests.
    for i in $(seq 1 200); do
        echo "2026-06-06T11:00:00 INFO mounted line $i" >> "$MNT/idx.log"
    done
    timberfs set "$BACKING/idx.log" index=true > /dev/null || return 1
    echo "2026-06-06T11:00:01 INFO MOUNTNEEDLE9A1F" >> "$MNT/idx.log"
    # the daemon's tick both flushes and indexes; give it room for both
    sleep 4
    [ -s "$BACKING/idx.log.grain" ] || return 1
    timberfs query "$BACKING/idx.log" --has MOUNTNEEDLE9A1F 2>/dev/null \
        | grep -q MOUNTNEEDLE9A1F
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

wal_kill9_durability() {
    # --wal's whole point: a kill -9 before any chunk flush must not lose
    # data that already made it through a sap-sync tick. Drive a real
    # appender over a FIFO (full control over exactly which lines land
    # before the kill), a --chunk-size/--flush-age big enough that nothing
    # would ever flush on its own within this test's window, so recovery
    # can ONLY come from the sap — and after a compression guard: the
    # recovered data must land in a couple of chunks, not one per line
    # (durability must not shred the very thing chunking exists for).
    local d=/var/log/timberfs-waltest
    rm -rf "$d"
    mkdir -p "$d"
    rm -f /tmp/wal.fifo
    mkfifo /tmp/wal.fifo
    timberfs append --into "$d/app.log" --wal --chunk-size 10485760 \
        --flush-age 3600 --quiet < /tmp/wal.fifo &
    local pid=$!
    exec 9>/tmp/wal.fifo
    local i
    for i in 1 2 3 4 5; do
        echo "wal-line-$i" >&9
    done
    # Let at least two 1-second maintenance ticks (which call sap_sync)
    # land before the kill.
    sleep 2.5
    kill -9 "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    exec 9>&-
    rm -f /tmp/wal.fifo

    # Confirm the crash landed with NOTHING chunked yet — a plain
    # (non-wal) store would lose all five lines from exactly this window.
    local chunks_before
    chunks_before=$(timberfs info "$d/app.log" --json 2>/dev/null | jq -r .chunks)
    if [ "$chunks_before" != "0" ]; then
        echo "expected 0 chunks before recovery (nothing should have flushed yet), got $chunks_before" >&2
        return 1
    fi

    # Restart the appender: FileStore::open replays the sap into its
    # buffer (no forced flush — chunk sizing survives the crash too), and
    # it keeps accepting writes exactly as before.
    rm -f /tmp/wal2.fifo
    mkfifo /tmp/wal2.fifo
    timberfs append --into "$d/app.log" --wal --chunk-size 10485760 \
        --flush-age 3600 --quiet < /tmp/wal2.fifo &
    local pid2=$!
    exec 9>/tmp/wal2.fifo
    for i in 6 7 8; do
        echo "wal-line-$i" >&9
    done
    sleep 1.5
    exec 9>&-
    rm -f /tmp/wal2.fifo
    # Graceful stop this time (SIGTERM): flushes and syncs everything.
    kill -TERM "$pid2"
    wait "$pid2" 2>/dev/null

    for i in 1 2 3 4 5 6 7 8; do
        if ! timberfs query "$d/app.log" 2>/dev/null | grep -qx "wal-line-$i"; then
            echo "missing wal-line-$i after recovery — the kill -9 lost data" >&2
            return 1
        fi
    done
    local chunks_after
    chunks_after=$(timberfs info "$d/app.log" --json 2>/dev/null | jq -r .chunks)
    if [ "$chunks_after" -gt 2 ]; then
        echo "durability shredded chunking: $chunks_after chunks for 8 short lines" >&2
        return 1
    fi
}

collapse_crash_kill_resilience() {
    # A `fallocate(COLLAPSE_RANGE)` retention cut interrupted mid-flight
    # must always be recoverable. Drive a real appender under a tight
    # --retain-size with a small --chunk-size (so collapses fire often),
    # kill -9 it repeatedly at randomized moments, and after every kill
    # require the store to reopen cleanly — exercising a real interrupted
    # collapse rather than a synthesized crash state.
    #
    # The feed is `yes`, which never reaches EOF on its own: a bounded
    # source (e.g. `seq`) risks the whole append finishing on its own
    # before the randomized sleep elapses (this VM's disk is fast enough
    # that it routinely did, in an earlier version of this test, making
    # every "kill" a no-op). An endless feed guarantees the appender is
    # still alive — reading, mid-flush, mid-retention-tick, or mid-collapse
    # — at the instant we send SIGKILL.
    local d=/var/log/timberfs-collapsetest
    rm -rf "$d"
    mkdir -p "$d"

    # ⚠ The appenders MUST NOT inherit this function's stderr. They are
    # backgrounded, so their output would land in the harness's single
    # $TMPOUT — which the next run_test truncates — and it drowned this
    # test's own failure messages: a CI failure here dumped three
    # "appending stdin" lines and NOTHING from any assertion, making the
    # one thing the test exists to report unreadable.
    local noise=/tmp/collapse-appender.log
    : > "$noise"

    # Everything an autopsy needs, written where a later failure can print
    # it: which iteration, what info said, and the store's own leftovers —
    # a `.trim` marker or a `.tmp` is the crash state itself.
    local state=/tmp/collapse-state.log
    dump_state() {
        echo "--- iteration ${1:-?} ---"
        echo "-- info stdout:"; sed 's/^/   /' /tmp/collapse.out 2>/dev/null
        echo "-- info stderr:"; sed 's/^/   /' /tmp/collapse.err 2>/dev/null
        echo "-- store directory (a .trim or .tmp IS the crash state):"
        ls -la "$d" 2>&1 | sed 's/^/   /'
        echo "-- rings header (64 bytes):"
        od -A d -t u8 -N 64 "$d/churn.log.rings" 2>&1 | sed 's/^/   /'
        echo "-- last appender output:"; tail -5 "$noise" 2>/dev/null | sed 's/^/   /'
    }

    local iter pid
    for iter in $(seq 1 8); do
        yes "COLLAPSE-CRASH-FILLER-LINE-0123456789-abcdefghij-$iter" \
            | timberfs append --into "$d/churn.log" --chunk-size 4096 \
                  --retain-size 16K --flush-age 1 --quiet >> "$noise" 2>&1 &
        pid=$!
        # Randomize the kill point (50-940ms) across the appender's
        # lifecycle: the startup retention catch-up, the read/flush loop,
        # and the once-a-second retention tick — sometimes landing right
        # in the middle of collapse_head.
        sleep "0.$(printf '%02d' $((RANDOM % 90 + 5)))"
        kill -9 "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null

        # ⚠ A kill can land before the appender has created the store at
        # all — the randomized point starts at 50ms, and a contended CI
        # runner with a cold cache is slower than any laptop. That is the
        # kill working as intended, not a store that failed to open, so it
        # is not a failure: there is simply nothing yet to reopen cleanly.
        if [ ! -e "$d/churn.log.rings" ]; then
            echo "iteration $iter: killed before the store existed; nothing to check" >&2
            continue
        fi
        if ! timberfs info "$d/churn.log" > /tmp/collapse.out 2>/tmp/collapse.err; then
            echo "iteration $iter: info FAILED after kill" >&2
            dump_state "$iter" >&2
            return 1
        fi
        if [ -s /tmp/collapse.err ]; then
            echo "iteration $iter: info wrote to stderr after kill" >&2
            dump_state "$iter" >&2
            return 1
        fi
    done

    # After all the kills the store must be intact: queryable, its trunk
    # still decodable with STOCK zstd (proving the skippable-frame stamp and
    # rebased index left valid frames behind), and byte-identical between the
    # two. The `yes` feed is ~1000x compressible, so the store decompresses to
    # gigabytes — compare STREAMED md5sums, never dumping it to disk (that
    # overflows a small VM's /tmp/RAM and is what a full-dump comparison hit).
    local qsum zsum empty
    empty=$(printf '' | md5sum)
    qsum=$(set -o pipefail; timberfs query "$d/churn.log" 2>/dev/null | md5sum) || {
        echo "final query failed" >&2
        dump_state final >&2
        return 1
    }
    zsum=$(set -o pipefail; zstd -dc "$d/churn.log.trunk" | md5sum) || {
        echo "stock zstd -dc failed on the post-kill trunk" >&2
        dump_state final >&2
        return 1
    }
    if [ "$qsum" = "$empty" ]; then
        echo "final query returned no data" >&2
        dump_state final >&2
        return 1
    fi
    if [ "$qsum" != "$zsum" ]; then
        echo "query output diverges from stock zstd -dc ($qsum vs $zsum)" >&2
        dump_state final >&2
        return 1
    fi

    # A fresh appender resumes and its marker is the last entry. Lead it with a
    # newline: a kill -9 can leave the committed trunk ending mid-line (chunks
    # are cut on a byte, not a line, boundary), and plain-text append is
    # byte-faithful — it will NOT insert a separator (that is exactly what keeps
    # `zstd -dc` an exact recovery). So a resuming logger opens a fresh line
    # itself, rather than fusing onto the partial line the crash left behind.
    if ! printf '\npost-kill-resume-marker\n' \
        | timberfs append --into "$d/churn.log" --quiet 2>/tmp/collapse.resume; then
        echo "the resuming append failed:" >&2
        cat /tmp/collapse.resume >&2
        timberfs info "$d/churn.log" 2>&1 | sed 's/^/  /' >&2
        return 1
    fi
    local last
    last=$(timberfs query "$d/churn.log" 2>/dev/null | tail -1)
    if [ "$last" != "post-kill-resume-marker" ]; then
        # The two steps above used to assert with no diagnostic at all, which
        # cost a CI round-trip to learn nothing from when this failed once on
        # focal (unreproducible in 20 local rounds on ext4, and green on a
        # re-run of the same artifact). Say what the store actually ends with.
        echo "the last entry is not the resume marker; it is:" >&2
        printf '  %s\n' "${last:0:120}" >&2
        echo "append stderr was:" >&2
        cat /tmp/collapse.resume >&2
        timberfs info "$d/churn.log" 2>&1 | sed 's/^/  /' >&2
        echo "last 3 entries:" >&2
        timberfs query "$d/churn.log" 2>/dev/null | tail -3 | cut -c1-120 | sed 's/^/  /' >&2
        return 1
    fi
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
    zcat /usr/share/man/man1/timberfs.1.gz | grep -q "^.TH TIMBERFS 1" \
        && zcat /usr/share/man/man1/timber-otlp.1.gz | grep -q "^.TH TIMBER..OTLP 1"
}

completion_scripts_installed() {
    test -f /usr/share/bash-completion/completions/timberfs \
        && test -f /usr/share/zsh/vendor-completions/_timberfs \
        && test -f /usr/share/bash-completion/completions/timber-filter \
        && test -f /usr/share/zsh/vendor-completions/_timber-filter \
        && test -f /usr/share/bash-completion/completions/timber-otlp \
        && test -f /usr/share/zsh/vendor-completions/_timber-otlp
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
run_test "mounted writes maintain a declared grain" mounted_grain_maintained
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

appender_maintains_grain() {
    # A declared index is maintained by EVERY streaming writer, not just
    # import: a plain-text appender must leave a grain covering what it
    # wrote, with no reindex — else `--has` scans everything forever.
    timberfs create --index "$PIPE_BACKING/idx.log" > /dev/null || return 1
    python3 -c "
import datetime
d = datetime.datetime(2026, 6, 7, 8, 0, 0)
for i in range(20000):
    ts = (d + datetime.timedelta(seconds=i)).isoformat()
    print(f'{ts} INFO work {i}' + (' APPENDNEEDLE5C2E' if i == 12345 else ''))
" | timberfs append --into "$PIPE_BACKING/idx.log" --chunk-size 4096 || return 1
    [ -s "$PIPE_BACKING/idx.log.grain" ] || return 1
    # covers every chunk, and actually skips: a needle in one chunk of many
    timberfs info --json "$PIPE_BACKING/idx.log" \
        | python3 -c "
import json,sys
i = json.load(sys.stdin)
sys.exit(0 if i['grain_chunks'] == i['chunks'] and i['chunks'] > 5 else 1)
" || return 1
    timberfs query "$PIPE_BACKING/idx.log" --has APPENDNEEDLE5C2E 2>/tmp/idxsel.txt \
        | grep -q APPENDNEEDLE5C2E || return 1
    SEL=$(grep -oE '^timberfs: [0-9]+' /tmp/idxsel.txt | grep -oE '[0-9]+')
    [ -n "$SEL" ] && [ "$SEL" -lt 20 ]
}

appender_grain_survives_retention() {
    # Retention rewrites the rings, which invalidates the positional grain:
    # the writer must rebuild it, and a surviving token must still be found
    # through the index (a stale grain would answer with the WRONG chunks).
    timberfs create --index "$PIPE_BACKING/idxret.log" > /dev/null || return 1
    python3 -c "
import datetime
d = datetime.datetime(2026, 6, 7, 9, 0, 0)
for i in range(40000):
    ts = (d + datetime.timedelta(seconds=i)).isoformat()
    print(f'{ts} INFO work {i} UNIQ{i:06d}')
" | timberfs append --into "$PIPE_BACKING/idxret.log" --chunk-size 8192 \
        --retain-size 40K || return 1
    # the head really was dropped
    [ "$(stat -c %s "$PIPE_BACKING/idxret.log.trunk")" -le 65536 ] || return 1
    # the last surviving line's token is findable via --has, and every
    # sampled survivor agrees with a full scan (no false negatives)
    LAST=$(timberfs query "$PIPE_BACKING/idxret.log" --no-filename 2>/dev/null \
        | tail -1 | awk '{print $NF}')
    timber-filter --has "$LAST" "$PIPE_BACKING/idxret.log" 2>/dev/null \
        | grep -q "$LAST" || return 1
    timberfs info --json "$PIPE_BACKING/idxret.log" \
        | python3 -c "
import json,sys
i = json.load(sys.stdin)
sys.exit(0 if i['grain_chunks'] == i['chunks'] else 1)
"
}

run_test "appender: file lock blocks rotate while live" appender_lock_blocks_rotate
run_test "appender: SIGTERM flushes buffered data" appender_sigterm_flushes
run_test "appender: two files share one directory" appenders_share_directory
run_test "appender: --retain-size 16K budget enforced" retain_size_budget
grain_rebase_survives_repeated_head_drops() {
    # The grain is POSITIONAL — record i is chunk i — so a retention
    # head-drop renumbers it. It is trimmed by the same prefix instead of
    # being rebuilt; this is the test that the trim lands on a record
    # boundary, on a real filesystem (tmpfs has no COLLAPSE_RANGE, so only
    # here does the collapse path run). A misalignment shows up as a FALSE
    # NEGATIVE, so the oracle is a full scan.
    #
    # Two phases, because "trimmed, not rebuilt" is only guaranteed when
    # the grain reaches at least as far as the drop: a writer that outruns
    # the once-a-second indexing tick can produce a drop bigger than the
    # grain's coverage, which is legitimately a delete-and-rebuild. Phase 1
    # writes with NO retention, so its shutdown leaves a grain covering
    # every chunk; phase 2 then declares the budget, and the catch-up drop
    # at its startup is necessarily within that coverage.
    timberfs create --index "$PIPE_BACKING/reb.log" > /dev/null || return 1
    python3 -c "
import datetime
d = datetime.datetime(2026, 6, 7, 10, 0, 0)
for i in range(300000):
    ts = (d + datetime.timedelta(seconds=i)).isoformat()
    print(f'{ts} INFO work {i} UNIQ{i:06d}')
" | timberfs append --into "$PIPE_BACKING/reb.log" --chunk-size 8192 2> /tmp/reb1.err \
        || { tail -5 /tmp/reb1.err; return 1; }
    timberfs info --json "$PIPE_BACKING/reb.log" | python3 -c "
import json,sys
i = json.load(sys.stdin)
if i['grain_chunks'] != i['chunks'] or i['chunks'] < 10:
    sys.exit(f\"phase 1 left {i['grain_chunks']}/{i['chunks']} chunks indexed\")
" || return 1

    # Phase 2: a budget the store is already far over, so retention cuts a
    # large prefix out from under a COMPLETE grain — the rebase path.
    timberfs set "$PIPE_BACKING/reb.log" retain_size=300K > /dev/null || return 1
    python3 -c "
import datetime
d = datetime.datetime(2026, 6, 8, 10, 0, 0)
for i in range(20000):
    ts = (d + datetime.timedelta(seconds=i)).isoformat()
    print(f'{ts} INFO later {i} LATER{i:06d}')
" | timberfs append --into "$PIPE_BACKING/reb.log" --chunk-size 8192 2> /tmp/reb2.err \
        || { tail -5 /tmp/reb2.err; return 1; }
    grep -q 'retention dropped' /tmp/reb2.err || {
        echo "no head-drop happened; phase 2 log:"; tail -5 /tmp/reb2.err; return 1
    }
    # Trimmed, not rebuilt: a full build announces itself as "indexed N
    # chunk(s)", and phase 2 must contain none.
    if grep -q 'timberfs: indexed .* chunk' /tmp/reb2.err; then
        echo "the grain was REBUILT after a head-drop, not trimmed:"
        grep 'timberfs: indexed .* chunk\|retention dropped' /tmp/reb2.err
        return 1
    fi

    # Whatever survived must still be findable THROUGH the index, and the
    # index must span the whole store.
    timberfs query "$PIPE_BACKING/reb.log" --no-filename 2>/dev/null \
        | awk '{print $NF}' | grep -E '^(UNIQ|LATER)' > /tmp/reb.all || return 1
    [ "$(wc -l < /tmp/reb.all)" -gt 100 ] || { echo "too few survivors to sample"; return 1; }
    for TOK in $(shuf -n 40 /tmp/reb.all); do
        timber-filter --has "$TOK" "$PIPE_BACKING/reb.log" 2>/dev/null \
            | grep -q "$TOK" || { echo "FALSE NEGATIVE for $TOK"; return 1; }
    done
    timberfs info --json "$PIPE_BACKING/reb.log" | python3 -c "
import json,sys
i = json.load(sys.stdin)
if i['grain_chunks'] != i['chunks']:
    sys.exit(f\"index covers {i['grain_chunks']} of {i['chunks']} chunks\")
"
}

run_test "appender: maintains a declared grain, no reindex" appender_maintains_grain
run_test "appender: grain stays correct across retention" appender_grain_survives_retention
run_test "grain: rebased, not rebuilt, across repeated head-drops" grain_rebase_survives_repeated_head_drops
run_test "wal: kill -9 after a sap-sync tick loses nothing, chunking intact" wal_kill9_durability
run_test "collapse-head retention survives repeated kill -9" collapse_crash_kill_resilience
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

query_follow_idle_flush() {
    # An entry is closed by the NEXT stamped line, so a store that falls
    # quiet would otherwise never emit its newest entry — the ERROR at
    # 03:00 with nothing after it, which is the one an incident is about.
    # After ten idle polls the follow loop closes it.
    rm -f "$PIPE_BACKING"/idle.log.*
    mkfifo /tmp/idle.fifo
    timberfs append --into "$PIPE_BACKING/idle.log" --flush-age 1 < /tmp/idle.fifo &
    local ap=$!
    exec 7>/tmp/idle.fifo
    # Seed first: a follower needs a store to attach to, so starting one on
    # a store the appender has not created yet is a race, not a test.
    local i
    printf '2026-06-08T08:59:00 INFO seed-line\n' >&7
    for i in $(seq 1 20); do
        [ -e "$PIPE_BACKING/idle.log.rings" ] && break
        sleep 0.5
    done
    sleep 2
    timberfs query "$PIPE_BACKING/idle.log" --follow > /tmp/idle.out 2>/dev/null &
    local fp=$!
    sleep 1
    printf '2026-06-08T09:00:00 ERROR last-and-only\n' >&7
    # Nothing follows it: only the idle flush can surface this entry.
    local got=""
    for i in $(seq 1 25); do
        sleep 1
        grep -q last-and-only /tmp/idle.out && { got=yes; break; }
    done
    kill "$fp" 2>/dev/null; wait "$fp" 2>/dev/null
    exec 7>&-; kill "$ap" 2>/dev/null; wait "$ap" 2>/dev/null
    rm -f /tmp/idle.fifo
    [ "$got" = yes ] || { echo "follower emitted:"; cat /tmp/idle.out; return 1; }
    # And it did not replay what was already there when it started.
    ! grep -q seed-line /tmp/idle.out
}

run_test "query --follow emits a quiet store's last entry (idle flush)" query_follow_idle_flush

query_follow_live_edge() {
    # The point of the live tail: an entry is visible BEFORE the chunk
    # holding it exists. The writer's flush age is a minute, so anything
    # the follower shows here came out of the .sap — and the trunk being
    # still empty when it does is what proves it.
    mkfifo /tmp/le.fifo
    timberfs append --into "$PIPE_BACKING/le.log" --wal --flush-age 60 < /tmp/le.fifo &
    local ap=$!
    exec 8>/tmp/le.fifo
    printf '2026-06-08T10:00:00 INFO seed-line\n' >&8
    local i
    for i in $(seq 1 20); do
        [ -e "$PIPE_BACKING/le.log.rings" ] && break
        sleep 0.5
    done
    sleep 1
    timberfs query "$PIPE_BACKING/le.log" --follow > /tmp/le.out 2>/dev/null &
    local fp=$!
    sleep 1
    printf '2026-06-08T10:00:01 ERROR unflushed-and-visible\n' >&8
    local got="" trunk=""
    for i in $(seq 1 10); do
        sleep 1
        if grep -q unflushed-and-visible /tmp/le.out; then
            got=yes
            trunk=$(stat -c%s "$PIPE_BACKING/le.log.trunk")
            break
        fi
    done
    kill "$fp" 2>/dev/null; wait "$fp" 2>/dev/null
    exec 8>&-; kill "$ap" 2>/dev/null; wait "$ap" 2>/dev/null
    rm -f /tmp/le.fifo
    [ "$got" = yes ] || { echo "the entry never surfaced; follower emitted:"; cat /tmp/le.out; return 1; }
    [ "$trunk" = 0 ] || { echo "trunk was $trunk bytes: a chunk was flushed, so this proves nothing"; return 1; }
    # …and from-now still means from now.
    ! grep -q seed-line /tmp/le.out
}

run_test "query --follow shows entries still unflushed (.sap live tail)" query_follow_live_edge

query_follow_no_gap_across_flushes() {
    # The handoff: every line the live tail served is repeated by the
    # chunk its segment becomes. Small chunks force many handoffs; the
    # follower must show each line exactly once — no gap, no double.
    mkfifo /tmp/hd.fifo
    timberfs append --into "$PIPE_BACKING/hd.log" --wal --chunk-size 4096 --flush-age 60 \
        < /tmp/hd.fifo &
    local ap=$!
    exec 9>/tmp/hd.fifo
    printf '2026-06-08T11:00:00 INFO seed-line\n' >&9
    local i
    for i in $(seq 1 20); do
        [ -e "$PIPE_BACKING/hd.log.rings" ] && break
        sleep 0.5
    done
    sleep 1
    timberfs query "$PIPE_BACKING/hd.log" --follow > /tmp/hd.out 2>/dev/null &
    local fp=$!
    sleep 1
    for i in $(seq 1 300); do
        printf '2026-06-08T11:00:01 INFO handoff id=%s padding-to-fill-a-chunk-quickly\n' "$i" >&9
        [ $((i % 50)) = 0 ] && sleep 1
    done
    # Wait for the tail to arrive rather than for a fixed time.
    for i in $(seq 1 20); do
        sleep 1
        grep -q "id=300 " /tmp/hd.out && break
    done
    kill "$fp" 2>/dev/null; wait "$fp" 2>/dev/null
    exec 9>&-; kill "$ap" 2>/dev/null; wait "$ap" 2>/dev/null
    rm -f /tmp/hd.fifo
    local chunks lines uniq
    chunks=$(timberfs index "$PIPE_BACKING/hd.log" | tail -1)
    lines=$(grep -c 'id=' /tmp/hd.out)
    uniq=$(grep -o 'id=[0-9]* ' /tmp/hd.out | sort -u | wc -l)
    echo "$chunks; follower emitted $lines line(s), $uniq distinct"
    [ "$lines" = 300 ] && [ "$uniq" = 300 ]
}

run_test "query --follow: no gap or duplicate across sap/chunk handoffs" query_follow_no_gap_across_flushes

query_follow_wal_enabled_under_a_live_writer() {
    # `set wal=true` is the mid-incident move: the producer cannot be
    # restarted, so the writer must pick the declaration up on its own —
    # and a follower already running must start seeing the live edge.
    mkfifo /tmp/sw.fifo
    timberfs append --into "$PIPE_BACKING/sw.log" --flush-age 60 < /tmp/sw.fifo &
    local ap=$!
    exec 3>/tmp/sw.fifo
    printf '2026-06-08T12:00:00 INFO seed-line\n' >&3
    local i
    for i in $(seq 1 20); do
        [ -e "$PIPE_BACKING/sw.log.rings" ] && break
        sleep 0.5
    done
    sleep 1
    timberfs query "$PIPE_BACKING/sw.log" --follow > /tmp/sw.out 2>/dev/null &
    local fp=$!
    sleep 1
    timberfs set "$PIPE_BACKING/sw.log" wal=true > /dev/null
    local live=""
    for i in $(seq 1 10); do
        sleep 1
        [ -e "$PIPE_BACKING/sw.log.sap" ] && { live=yes; break; }
    done
    printf '2026-06-08T12:00:01 ERROR live-after-set\n' >&3
    local got=""
    for i in $(seq 1 10); do
        sleep 1
        grep -q live-after-set /tmp/sw.out && { got=yes; break; }
    done
    kill "$fp" 2>/dev/null; wait "$fp" 2>/dev/null
    exec 3>&-; kill "$ap" 2>/dev/null; wait "$ap" 2>/dev/null
    rm -f /tmp/sw.fifo
    [ "$live" = yes ] || { echo "no .sap appeared: the running writer ignored the declaration"; return 1; }
    [ "$got" = yes ] || { echo "the follower never saw the live edge; it emitted:"; cat /tmp/sw.out; return 1; }
}

run_test "set wal=true starts the live edge under a running writer" query_follow_wal_enabled_under_a_live_writer

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
    # Pre-create the store with a declared index AND wal (also makes the
    # instance dir) so the intake exercises grain maintenance AND the
    # write-ahead sidecar together on the live/socket path.
    timberfs create --index --wal "$LOGSTORE" >/dev/null
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

socket_intake_wal_declared_and_working() {
    # The store was pre-created with --wal: the streaming sink (append
    # --records, same maintenance loop as a plain appender) must maintain
    # a live .sap and report it declared, while intake keeps working
    # normally — --wal is meant to be transparent to every other path.
    #
    # The sap exists for as long as the sink holds the store open (it is
    # created when a wal-declared store is opened), so wait for it rather
    # than sampling the instant after the restart the previous test did —
    # and if it never turns up, say enough to identify why.
    for _ in $(seq 1 10); do
        [ -f "$LOGSTORE.sap" ] && break
        sleep 1
    done
    [ -f "$LOGSTORE.sap" ] || {
        echo "no .sap sidecar for a wal-declared socket-intake store" >&2
        echo "  service: $(systemctl is-active "timberfs-log@$LOGINST.service" 2>&1)" >&2
        echo "  socket:  $(systemctl is-active "timberfs-log@$LOGINST.socket" 2>&1)" >&2
        echo "  store directory:" >&2
        ls -l "$(dirname "$LOGSTORE")" 2>&1 | sed 's/^/    /' >&2
        echo "  last journal lines for the service:" >&2
        journalctl -u "timberfs-log@$LOGINST.service" -n 15 --no-pager 2>&1 \
            | sed 's/^/    /' >&2
        return 1
    }
    timberfs info "$LOGSTORE" --json 2>/dev/null | jq -e '.wal_declared == true' >/dev/null \
        || return 1
    printf '2026-06-05T09:03:00 INFO socket wal check\n' | records > "$LOGPIPE"
    store_has "socket wal check"
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
run_test "socket intake: wal declared, .sap live, intake unaffected" socket_intake_wal_declared_and_working
run_test "socket intake: stop removes the FIFO" socket_intake_stop_removes_fifo

# The file follower (import --follow): the route for a producer that cannot
# be pointed anywhere — it keeps writing its own file and timberfs reads it.
# What is specific to it is position and rotation, so that is what these
# tests drive: a rotation UNDER the running follower, a restart across a
# rotation, copytruncate, and a restart with nothing new.
FOLDIR=/var/log/timberfs-follow
FOLLOG=$FOLDIR/app.log
FOLSTORE=$FOLDIR/store/app.log

fol_say() {
    printf '%s app[1]: %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$1" >> "$FOLLOG"
}
fol_start() {
    setsid timberfs import -F "$FOLLOG" --into "$FOLSTORE" --index \
        --poll 0.3 --flush-age 1 >> $FOLDIR/follower.log 2>&1 &
    FOLSESS=$(ps -o sess= -p $! | tr -d ' ')
    sleep 2
}
fol_stop() {
    pkill -TERM -s "$FOLSESS" -f 'import -F' 2>/dev/null
    sleep 2
}
fol_lines() { timberfs query "$FOLSTORE" 2>/dev/null | wc -l; }

follow_setup() {
    rm -rf $FOLDIR
    mkdir -p $FOLDIR/store
    : > "$FOLLOG"
    fol_say "before the follower started"
    fol_start
    fol_say "while following"
    for _ in $(seq 1 10); do
        [ "$(fol_lines)" = 2 ] && break
        sleep 1
    done
    [ "$(fol_lines)" = 2 ]
}

follow_survives_rotation_under_it() {
    # The line written just before the rename is the one a `tail -F`
    # pipeline drops: the follower must drain the descriptor it holds
    # before it looks at the new file.
    fol_say "written just before rotation"
    mv "$FOLLOG" "$FOLLOG.1"
    : > "$FOLLOG"
    sleep 1
    fol_say "after rotation"
    for _ in $(seq 1 10); do
        [ "$(fol_lines)" = 4 ] && break
        sleep 1
    done
    [ "$(fol_lines)" = 4 ] \
        && timberfs query "$FOLSTORE" | grep -q "just before rotation" \
        && grep -q "was replaced (rotation)" $FOLDIR/follower.log
}

follow_recovers_a_gap_across_a_restart() {
    # Down while lines are written AND while rotation moves them out of the
    # live path: the rotated candidate is where they have to be found.
    fol_stop
    fol_say "written while the follower was down"
    mv "$FOLLOG" "$FOLLOG.1"
    : > "$FOLLOG"
    fol_say "after the second rotation"
    fol_start
    for _ in $(seq 1 10); do
        [ "$(fol_lines)" = 6 ] && break
        sleep 1
    done
    [ "$(fol_lines)" = 6 ] \
        && timberfs query "$FOLSTORE" | grep -q "while the follower was down"
}

follow_handles_copytruncate() {
    fol_say "before the truncate"
    sleep 2
    cp "$FOLLOG" "$FOLLOG.copy"
    : > "$FOLLOG"
    sleep 1
    fol_say "after the truncate"
    for _ in $(seq 1 10); do
        [ "$(fol_lines)" = 8 ] && break
        sleep 1
    done
    [ "$(fol_lines)" = 8 ] && grep -q "copytruncate" $FOLDIR/follower.log
}

follow_restart_adds_nothing() {
    # The store is the checkpoint: a restart with nothing new must not
    # duplicate a single line, and the declared index must still be current.
    fol_stop
    fol_start
    sleep 3
    fol_stop
    [ "$(fol_lines)" = 8 ] \
        && [ "$(timberfs query "$FOLSTORE" 2>/dev/null | sort | uniq -d | wc -l)" = 0 ] \
        && timberfs info "$FOLSTORE" --json | jq -e '.grain.covers_all == true' >/dev/null 2>&1 \
        || { [ "$(fol_lines)" = 8 ] && [ -f "$FOLSTORE.grain" ]; }
}

follow_kill_loses_nothing() {
    # The default flush age is a minute, and that is only safe because the
    # SOURCE is the durable copy: a follower killed with a chunk unflushed
    # must re-read it rather than lose it. Runs with the shipped default
    # deliberately — the other follow tests use --flush-age 1 for speed.
    local kdir=$FOLDIR/kill klog kstore
    rm -rf "$kdir"
    mkdir -p "$kdir"
    klog=$kdir/app.log
    kstore=$kdir/store.log
    : > "$klog"
    setsid timberfs import -F "$klog" --into "$kstore" --poll 0.3 \
        >> "$kdir/follower.log" 2>&1 &
    sleep 2
    printf '%s app[1]: unflushed when killed\n' "$(date '+%Y-%m-%d %H:%M:%S')" >> "$klog"
    sleep 1
    kill -KILL "$(sed -n 's/.*pid=//p' "$kstore.lock")" 2>/dev/null
    sleep 1
    # Nothing should be in the store yet: the chunk never closed.
    [ "$(timberfs query "$kstore" 2>/dev/null | wc -l)" = 0 ] || {
        echo "expected an unflushed store after SIGKILL, found:" >&2
        timberfs query "$kstore" 2>&1 | sed 's/^/  /' >&2
        return 1
    }
    # A restart re-reads it from the file — the flock the killed process
    # held is gone, so the new one takes it.
    setsid timberfs import -F "$klog" --into "$kstore" --poll 0.3 \
        >> "$kdir/follower.log" 2>&1 &
    sleep 2
    kill -TERM "$(sed -n 's/.*pid=//p' "$kstore.lock")" 2>/dev/null
    sleep 2
    [ "$(timberfs query "$kstore" 2>/dev/null | wc -l)" = 1 ] \
        && timberfs query "$kstore" 2>/dev/null | grep -q "unflushed when killed"
}

run_test "follow: picks up an existing file and then tails it" follow_setup
run_test "follow: drains the held file when rotation replaces it" follow_survives_rotation_under_it
run_test "follow: recovers what was written while it was down" follow_recovers_a_gap_across_a_restart
run_test "follow: copytruncate is detected, not misread" follow_handles_copytruncate
run_test "follow: a restart with nothing new duplicates nothing" follow_restart_adds_nothing
run_test "follow: a kill with an unflushed chunk loses nothing" follow_kill_loses_nothing

# The plain-text FIFO pair (timberfs-text@.socket/.service): the route for a
# producer that can only log to a PATH — Apache's CustomLog/ErrorLog. Covers
# what is specific to it: the conf-file declaration (site-wide defaults plus a
# per-instance override, converged on every start), Apache's own two clocks
# with nothing declared, the store layout, and RemoveOnStop=no.
TEXTINST=vmtext
TEXTROOT=/var/log/timberfs
TEXTSTORE=$TEXTROOT/$TEXTINST/$TEXTINST.log
TEXTERRSTORE=$TEXTROOT/$TEXTINST.error/$TEXTINST.error.log
TEXTPIPE=/run/timberfs/text/$TEXTINST.pipe
TEXTERRPIPE=/run/timberfs/text/$TEXTINST.error.pipe

# Poll a store (up to ~15s) for a line: socket activation starts the service,
# then the age timer flushes.
text_has() {
    local store=$1 needle=$2 i=0
    while [ "$i" -lt 15 ]; do
        if timberfs query "$store" 2>/dev/null | grep -q "$needle"; then
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    return 1
}

text_intake_setup() {
    systemd-tmpfiles --create
    test -d /run/timberfs/text || return 1
    # Site-wide defaults and a per-instance override, exactly as documented:
    # the instance file must win on retain and add format=.
    printf 'DECLARE=index=true retain=90d\nEXTRA_OPTS=--flush-age 1\n' \
        > /etc/timberfs/text.conf
    printf 'DECLARE=index=true retain=45d format=combined\n' \
        > "/etc/timberfs/text-$TEXTINST.conf"
    printf 'DECLARE=index=true retain=365d format=apache-error\n' \
        > "/etc/timberfs/text-$TEXTINST.error.conf"
    systemctl enable --now "timberfs-text@$TEXTINST.socket" \
        "timberfs-text@$TEXTINST.error.socket"
    test -p "$TEXTPIPE" && test -p "$TEXTERRPIPE"
}

text_intake_apache_clocks() {
    # Apache's real output on both streams, LogFormat untouched: CLF %t on the
    # access log, bracketed ctime on the error log. Nothing is declared about
    # either, so this also asserts both are built-in clocks.
    ACC_T=$(date '+%d/%b/%Y:%H:%M:%S %z')
    ERR_T=$(date '+%a %b %e %H:%M:%S.123456 %Y')
    printf '10.0.0.7 - - [%s] "GET /vmtext HTTP/1.1" 200 42 "-" "curl/8.5.0"\n' \
        "$ACC_T" > "$TEXTPIPE"
    printf '[%s] [php:error] [pid 9] vmtext boom\n' "$ERR_T" > "$TEXTERRPIPE"
    # And the consolidated layout's claim: one store holding BOTH shapes
    # parses both, because each line is stamped by its own clock.
    printf '[%s] [php:error] [pid 9] vmtext mixed-store boom\n' "$ERR_T" > "$TEXTPIPE"
    text_has "$TEXTSTORE" "GET /vmtext" || return 1
    text_has "$TEXTSTORE" "mixed-store boom" || return 1
    text_has "$TEXTERRSTORE" "vmtext boom" || return 1
    # Entry-verified: a window ending before both lines must exclude them,
    # which only holds if each line's own clock was parsed (not write time).
    PAST=$(date -d '2 hours ago' '+%Y-%m-%d %H:%M:%S')
    [ "$(timberfs query "$TEXTSTORE" --to "$PAST" 2>/dev/null | wc -l)" = 0 ] \
        && [ "$(timberfs query "$TEXTERRSTORE" --to "$PAST" 2>/dev/null | wc -l)" = 0 ] \
        && [ "$(timberfs query "$TEXTSTORE" --from "$PAST" 2>/dev/null | wc -l)" = 2 ] \
        && [ "$(timberfs query "$TEXTERRSTORE" --from "$PAST" 2>/dev/null | wc -l)" = 1 ]
}

text_intake_store_path_is_the_store_name() {
    # The layout claim: <root>/<instance>/<instance>.log — one directory per
    # store, named after the store, with nothing about the intake in the
    # path (no /var/log/timberfs/text), and the handle is that same name.
    [ -f "$TEXTSTORE.rings" ] || return 1
    [ ! -e "$TEXTROOT/text" ] || return 1
    timberfs query "$TEXTINST" 2>/dev/null | grep -q "GET /vmtext"
}

text_intake_declares_from_conf() {
    # ExecStartPre declared the store: the per-instance DECLARE won over the
    # site-wide one, host= was stamped, and the declared index is maintained.
    # The grain lands on the writer's tick AFTER the chunk the entry made
    # visible, so wait for it rather than racing it.
    for _ in $(seq 1 10); do
        [ -f "$TEXTSTORE.grain" ] && break
        sleep 1
    done
    local bad=0
    want() { # FILE PATTERN WHAT
        grep -q "$2" "$1" || { echo "$3: $1 does not say $2" >&2; bad=1; }
    }
    want "$TEXTSTORE.bark" '"retain": "45d"' "per-instance DECLARE lost to the site-wide one"
    want "$TEXTSTORE.bark" '"format": "combined"' "free-form provenance not declared"
    want "$TEXTSTORE.bark" '"index": true' "index not declared"
    want "$TEXTSTORE.bark" '"host":' "host=%H not stamped"
    want "$TEXTERRSTORE.bark" '"retain": "365d"' "error instance's own DECLARE not applied"
    [ -f "$TEXTSTORE.grain" ] || { echo "no grain for an index-declared store" >&2; bad=1; }
    [ "$bad" = 0 ] || {
        for b in "$TEXTSTORE.bark" "$TEXTERRSTORE.bark"; do
            echo "--- $b" >&2
            cat "$b" >&2 2>/dev/null || echo "(missing)" >&2
        done
        return 1
    }
}

text_intake_declaration_converges() {
    # Change the site's retention and restart: the declaration is applied on
    # every start, so no hand-run command and no producer involvement.
    printf 'DECLARE=index=true retain=30d format=combined\n' \
        > "/etc/timberfs/text-$TEXTINST.conf"
    systemctl restart "timberfs-text@$TEXTINST.service" || return 1
    for _ in $(seq 1 10); do
        grep -q '"retain": "30d"' "$TEXTSTORE.bark" && break
        sleep 1
    done
    grep -q '"retain": "30d"' "$TEXTSTORE.bark" \
        && timberfs info "$TEXTSTORE" | grep -q 'keep 30d'
}

text_intake_survives_writer_restart() {
    # The property the whole route rests on: a producer holds its fd, the
    # writer is bounced under it, and the line written IN THE GAP still lands
    # (systemd keeps the read end, so the kernel pipe buffers it).
    exec 7>"$TEXTPIPE"
    printf '10.0.0.8 - - [%s] "GET /before-bounce HTTP/1.1" 200 1 "-" "-"\n' \
        "$(date '+%d/%b/%Y:%H:%M:%S %z')" >&7 || { exec 7>&-; return 1; }
    systemctl stop "timberfs-text@$TEXTINST.service"
    printf '10.0.0.9 - - [%s] "GET /in-the-gap HTTP/1.1" 200 1 "-" "-"\n' \
        "$(date '+%d/%b/%Y:%H:%M:%S %z')" >&7 || { exec 7>&-; return 1; }
    systemctl start "timberfs-text@$TEXTINST.service"
    exec 7>&-
    text_has "$TEXTSTORE" "in-the-gap" \
        && timberfs query "$TEXTSTORE" | grep -q "before-bounce"
}

text_intake_merged_vhost_view() {
    # A vhost's two streams read as one interleaved, attributed view — a
    # query over both stores, each in its own directory (the documented one
    # names them by handle; this one spells the paths to pin the layout).
    OUT=$(timberfs query "$TEXTSTORE" "$TEXTERRSTORE" 2>/dev/null)
    echo "$OUT" | grep -q "^$TEXTSTORE:.*GET /vmtext" \
        && echo "$OUT" | grep -q "^$TEXTERRSTORE:.*vmtext boom"
}

text_intake_stop_keeps_the_fifo() {
    # RemoveOnStop=no, deliberately unlike the records pair: Apache opens its
    # log with O_CREAT, so a missing node would become a regular file that
    # silently swallows the log and blocks the socket from returning.
    systemctl stop "timberfs-text@$TEXTINST.socket" "timberfs-text@$TEXTINST.error.socket"
    test -p "$TEXTPIPE" && test -p "$TEXTERRPIPE" || return 1
    systemctl disable "timberfs-text@$TEXTINST.socket" \
        "timberfs-text@$TEXTINST.error.socket" >/dev/null 2>&1
    rm -f /etc/timberfs/text.conf "/etc/timberfs/text-$TEXTINST.conf" \
        "/etc/timberfs/text-$TEXTINST.error.conf"
    true
}

run_test "text intake: conf files, two instances, FIFOs created" text_intake_setup
run_test "text intake: Apache's CLF and ctime clocks, nothing declared" text_intake_apache_clocks
run_test "text intake: store path is the store's name, not the intake's" text_intake_store_path_is_the_store_name
run_test "text intake: DECLARE applied, per-instance beats site-wide" text_intake_declares_from_conf
run_test "text intake: changed declaration converges on restart" text_intake_declaration_converges
run_test "text intake: line written during a writer bounce still lands" text_intake_survives_writer_restart
run_test "text intake: a vhost's two streams as one attributed view" text_intake_merged_vhost_view
run_test "text intake: stopping the socket keeps the FIFO node" text_intake_stop_keeps_the_fifo

# Fluentd Forward protocol intake (timberfs-forward.socket/.service): a hand-
# packed msgpack client (struct.pack, no msgpack library assumed) drives the
# real wire protocol — Message, Forward and PackedForward(+chunk ack), plus a
# split-line partial pair. A fixed event time (2026-06-20 09:00:00 UTC as a
# base) makes the write-time assertions below exact instead of racing the
# wall clock.
FWD_TAG=vmfwd
FWD_STORE=/var/log/timberfs/$FWD_TAG/$FWD_TAG.log
FWD_EPOCH=$(date -u -d "2026-06-20 09:00:00" +%s)

forward_intake_setup() {
    systemd-tmpfiles --create
    systemctl enable --now timberfs-forward.socket
}

# $1 = chunk id to request an ack for (also seeds a distinct partial_id, so
# two calls in the same test run never collide). Writes ACK_OK on success.
forward_intake_client() {
    # ARGS: CHUNK_ID [TAG] [ACK_TIMEOUT] — tag defaults to the suite's
    # store, the timeout to comfortably-long (a refusal test shortens it).
    command -v python3 >/dev/null 2>&1 || { echo "NO_PYTHON3"; return 1; }
    python3 - "$1" "${2:-$FWD_TAG}" "$FWD_EPOCH" "${3:-15}" << 'PYEOF'
import socket
import struct
import sys

chunk_id, tag, epoch = sys.argv[1], sys.argv[2], int(sys.argv[3])
ack_timeout = float(sys.argv[4])


def pack_uint(n):
    if n < 256:
        return b"\xcc" + bytes([n])
    if n < 65536:
        return b"\xcd" + struct.pack(">H", n)
    return b"\xce" + struct.pack(">I", n)


def pack_str(s):
    b = s.encode()
    n = len(b)
    if n <= 31:
        return bytes([0xA0 | n]) + b
    return b"\xd9" + bytes([n]) + b


def pack_bin(b):
    return b"\xc4" + bytes([len(b)]) + b


def pack_map(pairs):
    out = bytes([0x80 | len(pairs)])
    for k, v in pairs:
        out += pack_str(k) + pack_str(v)
    return out


def pack_arr(n):
    return bytes([0x90 | n])


s = socket.create_connection(("127.0.0.1", 24224), timeout=10)

# 1. Message mode: [tag, time, record]
s.sendall(
    pack_arr(3)
    + pack_str(tag)
    + pack_uint(epoch)
    + pack_map([("log", "message-mode-entry"), ("container_id", "a" * 64)])
)

# 2. Forward mode: [tag, [[time, record], [time, record]]]
entries = (
    pack_arr(2)
    + pack_arr(2) + pack_uint(epoch) + pack_map([("log", "forward-entry-one")])
    + pack_arr(2) + pack_uint(epoch + 1) + pack_map([("log", "forward-entry-two")])
)
s.sendall(pack_arr(2) + pack_str(tag) + entries)

# 3. A split-line partial pair that must reassemble into ONE entry
pid = chunk_id + "-p"
s.sendall(
    pack_arr(3)
    + pack_str(tag)
    + pack_uint(epoch + 2)
    + pack_map(
        [
            ("log", "partial-one "),
            ("partial_message", "true"),
            ("partial_id", pid),
            ("partial_ordinal", "1"),
            ("partial_last", "false"),
        ]
    )
)
s.sendall(
    pack_arr(3)
    + pack_str(tag)
    + pack_uint(epoch + 2)
    + pack_map(
        [
            ("log", "partial-two"),
            ("partial_message", "true"),
            ("partial_id", pid),
            ("partial_ordinal", "2"),
            ("partial_last", "true"),
        ]
    )
)

# 4. PackedForward (bin blob) with a chunk ack request
pair = pack_arr(2) + pack_uint(epoch + 3) + pack_map([("log", "packed-entry")])
packed = pack_arr(3) + pack_str(tag) + pack_bin(pair) + pack_map([("chunk", chunk_id)])
s.sendall(packed)

s.settimeout(ack_timeout)
try:
    data = s.recv(4096)
except socket.timeout:
    print("NO_ACK")
    sys.exit(1)

# Decode the ack reply by hand: fixmap(1) {"ack": "<chunk>"}
if len(data) < 2 or data[0] != 0x81:
    print("BAD_ACK_FRAME", data.hex())
    sys.exit(1)
pos = 1
klen = data[pos] & 0x1F
pos += 1 + klen
vlen = data[pos] & 0x1F
pos += 1
got = data[pos : pos + vlen].decode()
if got == chunk_id:
    print("ACK_OK")
    sys.exit(0)
print("ACK_MISMATCH:" + got)
sys.exit(1)
PYEOF
}

forward_intake_unknown_tag_refused_until_created() {
    # Conservative default (no --auto-create in the shipped unit): a
    # never-seen tag gets no store, no lock litter, and no ack. The name is
    # this intake's own: every intake writes into ONE store namespace now,
    # so sharing "vmrefused" with the OTLP test would have that test find a
    # store this one created and answer 200 instead of 503.
    forward_intake_client vmrefchunk vmfwdrefused 4 > /tmp/fwdref.out 2>&1
    grep -q NO_ACK /tmp/fwdref.out || { cat /tmp/fwdref.out; return 1; }
    [ ! -e /var/log/timberfs/vmfwdrefused/vmfwdrefused.log.rings ] || return 1
    [ ! -e /var/log/timberfs/vmfwdrefused/vmfwdrefused.log.lock ] || return 1
    # Not even the store's directory: a refused tag leaves nothing at all.
    [ ! -e /var/log/timberfs/vmfwdrefused ] || return 1
    # The operator provisions the store; the sender's retry then lands and
    # is acked — provisioning converges with nothing lost.
    timberfs create --wal /var/log/timberfs/vmfwdrefused/vmfwdrefused.log || return 1
    forward_intake_client vmrefchunk vmfwdrefused > /tmp/fwdref2.out 2>&1
    grep -q ACK_OK /tmp/fwdref2.out || { cat /tmp/fwdref2.out; return 1; }
}

forward_intake_enable_auto_create() {
    # The Docker-host mode for the rest of the flow: tags are container
    # names that come and go, so the receiver mints stores itself.
    mkdir -p /etc/systemd/system/timberfs-forward.service.d
    cat > /etc/systemd/system/timberfs-forward.service.d/auto-create.conf <<'EOF'
[Service]
ExecStart=
ExecStart=/usr/bin/timberfs forward-intake --forest default --exit-on-upgrade --auto-create
EOF
    systemctl daemon-reload
    systemctl restart timberfs-forward.service
    sleep 0.5
    systemctl --quiet is-active timberfs-forward.service
}

forward_intake_receives_and_acks() {
    forward_intake_client vmtestchunk1 > /tmp/fwd1.out 2>&1
    local rc=$?
    cat /tmp/fwd1.out
    if [ "$rc" != 0 ]; then
        journalctl -u timberfs-forward.service --no-pager -n 80 >&2
        return 1
    fi
    grep -q ACK_OK /tmp/fwd1.out || return 1
    # The ack means durable in the .sap write-ahead sidecar (the receiver
    # declares "wal" on every store it touches) — NOT flushed to a chunk.
    grep -q '"wal": true' "$FWD_STORE.bark" || return 1
    [ -s "$FWD_STORE.sap" ] || return 1
    # Visibility arrives with the normal chunk flush (flush-age): wait for
    # it, then everything is queryable — and in ONE chunk, not the
    # chunk-per-ack shredding the wal exists to prevent.
    local i
    for i in $(seq 1 20); do
        timberfs query "$FWD_STORE" 2>/dev/null | grep -q "packed-entry" && break
        sleep 0.5
    done
    timberfs query "$FWD_STORE" 2>/dev/null | grep -q "message-mode-entry" \
        && timberfs query "$FWD_STORE" 2>/dev/null | grep -q "forward-entry-one" \
        && timberfs query "$FWD_STORE" 2>/dev/null | grep -q "forward-entry-two" \
        && timberfs query "$FWD_STORE" 2>/dev/null | grep -q "packed-entry" \
        && timberfs info "$FWD_STORE" | grep -q "in 1 chunk(s)"
}

forward_intake_store_path_is_the_tag() {
    # One directory per tag, named after the tag: the receiver writes the
    # same layout as every other intake, so the store a Fluentd tag created
    # answers to that tag as a handle and nothing names the protocol.
    [ -f "$FWD_STORE.rings" ] || return 1
    [ ! -e /var/log/timberfs/forward ] || return 1
    timberfs query "$FWD_TAG" 2>/dev/null | grep -q "packed-entry"
}

forward_intake_partial_reassembles() {
    timberfs query "$FWD_STORE" 2>/dev/null | grep -qx "partial-one partial-two"
}

forward_intake_event_times_landed() {
    # The chunk's write window is the SENDER'S event times (epoch ..
    # epoch+3), not "now" — none of these payloads carry a parseable
    # timestamp of their own, so `timberfs info`'s chunk-granularity
    # "covers" span is the reliable way to check this (query's entry-aware
    # modes, e.g. --show-write-time, merge consecutive untimestamped lines
    # into one multi-line entry annotated only once — a query.rs behavior
    # this receiver doesn't change, so it isn't what this test is about).
    timberfs info "$FWD_STORE" > /tmp/fwd_info.out 2>&1
    if grep -q "covers    2026-06-20 09:00:00.000 .. 2026-06-20 09:00:03.000" /tmp/fwd_info.out; then
        return 0
    fi
    cat /tmp/fwd_info.out >&2
    return 1
}

forward_intake_container_id_seeded() {
    grep -qE '"container_id": "a{64}"' "$FWD_STORE.bark"
}

forward_intake_seeds_host_and_peer() {
    # `host` is the one label a fleet view cannot do without, and the
    # Forward protocol carries none — so the sender's own field is honoured
    # when there is one, and the connecting address is recorded either way.
    # A receiver that guessed a hostname (reverse DNS on the peer) would put
    # DNS in the write path and still only be guessing.
    #
    # The suite's own sender declares no hostname, so this store must have a
    # `peer` and no `host` at all.
    grep -q '"peer"' "$FWD_STORE.bark" || { cat "$FWD_STORE.bark"; return 1; }
    grep -q '"host"' "$FWD_STORE.bark" && { cat "$FWD_STORE.bark"; return 1; }

    # And a sender that DOES name itself is taken at its word.
    python3 - << 'PYEOF' || return 1
import socket, struct, time
def s8(v):
    b = v.encode()
    return bytes([0xa0 | len(b)]) + b if len(b) < 32 else bytes([0xd9, len(b)]) + b
def m(pairs):
    return bytes([0x80 | len(pairs)]) + b"".join(s8(k) + s8(v) for k, v in pairs)
def a(items):
    return bytes([0x90 | len(items)]) + b"".join(items)
ent = a([bytes([0xce]) + struct.pack(">I", int(time.time())),
         m([("log", "declared sender"), ("hostname", "vmsender07")])])
c = socket.create_connection(("127.0.0.1", 24224), timeout=10)
c.sendall(a([s8("vmfwdhost"), a([ent])]))
time.sleep(1)
c.close()
PYEOF
    local bark=/var/log/timberfs/vmfwdhost/vmfwdhost.log.bark
    local i
    for i in $(seq 1 20); do [ -f "$bark" ] && break; sleep 0.5; done
    [ -f "$bark" ] || { echo "no store for the declaring sender" >&2; return 1; }
    jq -e '.host == "vmsender07" and (.peer | startswith("127.0.0.1:"))' "$bark" >/dev/null \
        || { cat "$bark"; return 1; }
}

incus_intake_is_installed_and_validates_its_options() {
    # The VM has no incus, so what is testable here is the surface: the
    # unit ships, the options are checked BEFORE anything is opened, and
    # an absent daemon is said plainly rather than surfacing as a bare
    # errno. The tap itself is exercised against a real container, which
    # a VM without incus cannot provide.
    [ -f /lib/systemd/system/timberfs-incus.service ] || return 1
    grep -q 'SupplementaryGroups=incus-admin' /lib/systemd/system/timberfs-incus.service || return 1
    # It reaches OUT to a socket rather than being connected to, so it is
    # not socket-activated and must not have picked up a .socket by
    # accident.
    [ -f /lib/systemd/system/timberfs-incus.socket ] && return 1

    local d=/var/log/timberfs/vmincus
    rm -rf "$d"; mkdir -p "$d"
    # A key naming nothing selects everything, which is the one key that
    # is refused.
    timberfs incus-intake --into-dir "$d" --key '' > /tmp/vmincus.err 2>&1 && return 1
    grep -q 'at least one label' /tmp/vmincus.err || { cat /tmp/vmincus.err >&2; return 1; }
    # A prefix naming a fact we cannot supply would expand to nothing and
    # be noticed only at query time.
    timberfs incus-intake --into-dir "$d" --prefix '{nope} ' > /tmp/vmincus.err 2>&1 && return 1
    grep -q 'not a fact this intake has' /tmp/vmincus.err || { cat /tmp/vmincus.err >&2; return 1; }
    timberfs incus-intake --into-dir "$d" --prefix '{time' > /tmp/vmincus.err 2>&1 && return 1
    grep -q 'unclosed' /tmp/vmincus.err || { cat /tmp/vmincus.err >&2; return 1; }
    # No daemon: named as such, with the group that usually explains it.
    timberfs incus-intake --into-dir "$d" --socket /nonexistent.sock > /tmp/vmincus.err 2>&1 && return 1
    grep -q 'connecting to incus' /tmp/vmincus.err || { cat /tmp/vmincus.err >&2; return 1; }
    # Nothing was created before the options were checked.
    [ -z "$(ls -A "$d")" ] || { ls -A "$d" >&2; return 1; }
}

a_store_is_called_what_it_declares() {
    # Once a path is opaque, the only name a store has is the one it
    # declares. `list` shows that, `--names` offers it, and `info` answers
    # to it — while a store that declares none is still called what its
    # path calls it, so both worlds render in one column.
    local d=/var/log/timberfs/vmname
    rm -rf "$d"
    local u=0f0f0f0f-1111-4222-8333-444444444444
    mkdir -p "$d/$u"
    # Seed the manifest BEFORE the pair, with the id the path uses: the
    # store's two sides must agree from the first byte, and patching the
    # id afterwards is refused (as it should be).
    cat > "$d/$u/$u.log.bark" <<BARK
{"id": "$u", "name": "web01-console", "type": "console",
 "host": "vmhost", "incus.instance": "web01", "incus.project": "default"}
BARK
    printf 'console line\n' | timberfs append --into "$d/$u/$u.log" --quiet 2>/dev/null || return 1
    # ...and a store beside it that declares no name at all.
    printf 'legacy line\n' | timberfs append --into "$d/plainstore/plainstore.log" --quiet 2>/dev/null || return 1

    # NAME, not HANDLE: the opaque one shows what it declares, the other
    # what its path gives it.
    timberfs list "$d" > /tmp/vmname.tab 2>/dev/null || return 1
    head -1 /tmp/vmname.tab | grep -qE '^ID[[:space:]]+NAME' || { head -1 /tmp/vmname.tab >&2; return 1; }
    grep -q 'web01-console' /tmp/vmname.tab || { cat /tmp/vmname.tab >&2; return 1; }
    grep -q 'plainstore' /tmp/vmname.tab || { cat /tmp/vmname.tab >&2; return 1; }
    grep -q "$u" /tmp/vmname.tab && { echo "the uuid leaked into NAME" >&2; return 1; }

    # --names is what completion consumes, so it must offer the same.
    timberfs list "$d" --names 2>/dev/null | sort | tr '\n' ',' | grep -qx 'plainstore,web01-console,' \
        || { timberfs list "$d" --names >&2; return 1; }

    # `info` answers to the declared name, and leads with it rather than
    # with the uuid the path happens to use.
    timberfs info "$d/$u/$u.log" > /tmp/vmname.info 2>&1 || return 1
    head -1 /tmp/vmname.info | grep -q '^web01-console' || { head -1 /tmp/vmname.info >&2; return 1; }
    # The name is NOT among the labels: it has its own column and its own
    # line, and it is not where the entries came from.
    grep -E '^  manifest' /tmp/vmname.info | grep -q 'name=' && { grep manifest /tmp/vmname.info >&2; return 1; }
    timberfs list "$d" --json 2>/dev/null \
        | jq -e '.[] | select(.labels.type == "console") | .labels | has("name") == false' >/dev/null || return 1

    # Everything in the manifest is matchable — labels, the name, the id
    # and the settings alike. Nothing is withheld for being the wrong KIND
    # of fact.
    local got
    for q in 'type=console' 'name=web01-console' 'name=~.*-console' "id=$u" 'incus.instance=web01'; do
        got=$(timberfs list "$d" --names --select "$q" 2>/dev/null)
        [ "$got" = "web01-console" ] || { echo "select $q gave '$got'" >&2; return 1; }
    done
    # ...including a store whose name only its path supplies.
    got=$(timberfs list "$d" --names --select 'name=plainstore' 2>/dev/null)
    [ "$got" = "plainstore" ] || { echo "path-named select gave '$got'" >&2; return 1; }
}

selection_is_by_label_not_by_name() {
    # Two stores of one service, told apart only by a label — the case a
    # store NAME cannot express, and the reason selection is the primitive.
    local a=/var/log/timberfs/vmsel-a b=/var/log/timberfs/vmsel-b
    rm -rf "$a" "$b"
    timberfs create --set type=console --set service=vmsel --set host=vmhost \
        "$a/vmsel-a.log" >/dev/null 2>&1 || return 1
    timberfs create --set type=app --set service=vmsel --set host=vmhost \
        "$b/vmsel-b.log" >/dev/null 2>&1 || return 1
    printf 'console line\n' | timberfs append --into "$a/vmsel-a.log" --quiet 2>/dev/null || return 1
    printf 'app line\n' | timberfs append --into "$b/vmsel-b.log" --quiet 2>/dev/null || return 1

    local got
    # Terms are ANDed; the same service resolves to two stores, and the
    # label is what separates them.
    got=$(timberfs list --names --select 'service=vmsel' 2>/dev/null | sort | tr '\n' ',')
    [ "$got" = "vmsel-a,vmsel-b," ] || { echo "service=vmsel gave $got" >&2; return 1; }
    got=$(timberfs list --names --select 'service=vmsel,type=console' 2>/dev/null)
    [ "$got" = "vmsel-a" ] || { echo "type=console gave $got" >&2; return 1; }
    # Anchored regex, and a negated term.
    got=$(timberfs list --names --select 'service=~vms.*,type!=app' 2>/dev/null)
    [ "$got" = "vmsel-a" ] || { echo "regex/negation gave $got" >&2; return 1; }
    # Anchoring is not decoration: unanchored, `vms` would match here.
    got=$(timberfs list --names --select 'service=~vms' 2>/dev/null)
    [ -z "$got" ] || { echo "unanchored regex matched $got" >&2; return 1; }

    # `=*` is the substring the anchoring makes awkward: "the store with
    # `sel` in its name" is the commonest thing anyone asks, and saying it
    # as `=~.*sel.*` would read the typed text as a pattern.
    got=$(timberfs list --names --select 'name=*sel-' 2>/dev/null | sort | tr '\n' ',')
    [ "$got" = "vmsel-a,vmsel-b," ] || { echo "name=*sel- gave $got" >&2; return 1; }
    # Scoped by service, because the forest holds every other test's
    # stores too and `not containing -a` is true of most of them.
    got=$(timberfs list --names --select 'service=vmsel,name!*-a' 2>/dev/null)
    [ "$got" = "vmsel-b" ] || { echo "name!*-a gave $got" >&2; return 1; }
    # LITERAL: a dot is a dot. `vmsel.a` matches nothing, where the
    # equivalent regex would have matched `vmsel-a`.
    got=$(timberfs list --names --select 'name=*vmsel.a' 2>/dev/null)
    [ -z "$got" ] || { echo "a substring was read as a pattern: $got" >&2; return 1; }
    got=$(timberfs list --names --select 'name=~.*vmsel.a.*' 2>/dev/null)
    [ "$got" = "vmsel-a" ] || { echo "the regex form should match: $got" >&2; return 1; }
    # An empty substring is in every value, so it says nothing.
    timberfs list --select 'name=*' >/dev/null 2>&1 && return 1

    # Matched nothing is a successful answer WITH coverage, not an error
    # and not silence — "no results" must not read as "nothing searched".
    got=$(timberfs list --names --select 'service=vmsel,type=nope' 2>/tmp/vmsel.err) || return 1
    [ -z "$got" ] || return 1
    grep -q 'examined' /tmp/vmsel.err || { cat /tmp/vmsel.err >&2; return 1; }

    # A malformed predicate is refused before the walk, so it can never be
    # mistaken for an empty result.
    timberfs list --select 'service' >/dev/null 2>&1 && return 1

    # `info --json` and `list --json` emit the SAME OBJECT — not merely
    # agreeing values under different names, which is what this test used
    # to assert. `forest` is the one allowed difference: `info` did not
    # reach the store through one.
    timberfs info --json "$a/vmsel-a.log" > /tmp/vmsel.info 2>/dev/null || return 1
    timberfs list --json /var/log/timberfs > /tmp/vmsel.list 2>/dev/null || return 1
    jq -c '.[] | select(.handle == "vmsel-a")' /tmp/vmsel.list > /tmp/vmsel.row
    jq -e --slurpfile row /tmp/vmsel.row \
        '. as $i | ($row[0] | del(.forest)) == ($i | del(.forest))' /tmp/vmsel.info >/dev/null \
        || {
            echo "info and list disagree:" >&2
            diff <(jq -S 'del(.forest)' /tmp/vmsel.info) \
                 <(jq -S 'del(.forest)' /tmp/vmsel.row) >&2
            return 1
        }

    # Labels are what the manifest declares, and no more: `info` once kept
    # its own reserved-key list and had drifted, leaking `wal` and the
    # `timestamp_*` keys into what reads as provenance.
    jq -e '.labels | has("wal") == false and has("timestamp_utc") == false' \
        /tmp/vmsel.info >/dev/null || { jq -c .labels /tmp/vmsel.info; return 1; }

    # The names that used to differ between the two are gone for good.
    jq -e 'has("provenance") or has("size_bytes") or has("from_ms")
           or has("writer_live") or has("indexed") | not' \
        /tmp/vmsel.info >/dev/null || return 1
}

a_query_document_selects_stores_and_can_answer_with_them() {
    # The document is the same question the flags ask, so it selects
    # stores by LABEL — there is no path member to fall back to — and it
    # can stop at the stores rather than always reading entries.
    local a=/var/log/timberfs/vmqd-a b=/var/log/timberfs/vmqd-b
    rm -rf "$a" "$b"
    timberfs create --set type=console --set service=vmqd --set host=vmhost \
        "$a/vmqd-a.log" >/dev/null 2>&1 || return 1
    timberfs create --set type=app --set service=vmqd --set host=vmhost \
        "$b/vmqd-b.log" >/dev/null 2>&1 || return 1
    printf '2026-08-26T10:00:00Z ERROR console boom\n' \
        | timberfs append --into "$a/vmqd-a.log" --quiet 2>/dev/null || return 1
    printf '2026-08-26T10:00:00Z ERROR app boom\n' \
        | timberfs append --into "$b/vmqd-b.log" --quiet 2>/dev/null || return 1

    local got
    # A predicate over labels, read as entries.
    cat > /tmp/vmqd.json <<'JSON'
{ "v": "1.0-EXPERIMENTAL",
  "stores": { "select": [ {"key":"service","op":"=","value":"vmqd"},
                          {"key":"type","op":"=","value":"console"} ] },
  "window": { "axis": "logline" },
  "match": { "granularity": "entries", "all": [ {"has":"ERROR"} ] },
  "response_format": { "kind": "loglines", "options": { "no_filename": true } } }
JSON
    got=$(timberfs query --query /tmp/vmqd.json 2>/tmp/vmqd.err)
    [ "$got" = "2026-08-26T10:00:00Z ERROR console boom" ] || {
        echo "doc read gave '$got'" >&2
        cat /tmp/vmqd.err >&2
        return 1
    }

    # The same document from stdin, so a generator need not write a file.
    got=$(timberfs query --query - < /tmp/vmqd.json 2>/dev/null)
    [ "$got" = "2026-08-26T10:00:00Z ERROR console boom" ] || return 1

    # kind:stores answers with the stores themselves and reads no entries.
    cat > /tmp/vmqd-stores.json <<'JSON'
{ "v": "1.0-EXPERIMENTAL",
  "stores": { "select": [ {"key":"service","op":"=","value":"vmqd"} ] },
  "response_format": { "kind": "stores" } }
JSON
    timberfs query --query /tmp/vmqd-stores.json > /tmp/vmqd.stores 2>/dev/null || return 1
    got=$(jq -r '[.[] | select(.labels.service == "vmqd") | .name] | sort | join(",")' \
        /tmp/vmqd.stores)
    [ "$got" = "vmqd-a,vmqd-b" ] || { echo "kind:stores gave '$got'" >&2; return 1; }
    # It carries the identity a follower would key on, not just a name.
    jq -e '.[] | select(.name == "vmqd-a") | .id | test("^[0-9a-f-]{36}$")' \
        /tmp/vmqd.stores >/dev/null || { jq -c '.[0]' /tmp/vmqd.stores; return 1; }

    # Enumeration is that same request with no predicate, not a second verb.
    printf '{"v":"1.0-EXPERIMENTAL","stores":{},"response_format":{"kind":"stores"}}\n' \
        > /tmp/vmqd-all.json
    timberfs query --query /tmp/vmqd-all.json 2>/dev/null \
        | jq -e 'length >= 2' >/dev/null || return 1

    # A member the answer cannot honour is REFUSED, never ignored: a
    # document that parses and quietly does something else is the failure
    # this format's strictness exists to prevent.
    printf '{"v":"1.0-EXPERIMENTAL","stores":{},"match":{"granularity":"entries","all":[{"has":"x"}]},"response_format":{"kind":"stores"}}\n' \
        > /tmp/vmqd-bad.json
    timberfs query --query /tmp/vmqd-bad.json >/dev/null 2>&1 && return 1
    # ...as is an axis the kind cannot answer on.
    printf '{"v":"1.0-EXPERIMENTAL","stores":{},"window":{"axis":"write"},"response_format":{"kind":"loglines"}}\n' \
        > /tmp/vmqd-axis.json
    timberfs query --query /tmp/vmqd-axis.json >/dev/null 2>&1 && return 1
    # ...and a store named by path, which this format deliberately lacks.
    printf '{"v":"1.0-EXPERIMENTAL","stores":{"paths":[{"file_path":"%s/vmqd-a.log"}]}}\n' "$a" \
        > /tmp/vmqd-path.json
    timberfs query --query /tmp/vmqd-path.json >/dev/null 2>&1 && return 1

    # --dump-json is the bridge between the two surfaces: the flags name a
    # store by PATH, the document by IDENTITY, and the translation has to
    # find the same store again or the surfaces have drifted.
    timberfs query "$a/vmqd-a.log" --has ERROR --no-filename --dump-json \
        > /tmp/vmqd-rt.json 2>/dev/null || return 1
    jq -e --arg id "$(timberfs info --json "$a/vmqd-a.log" | jq -r .id)" \
        '.stores.select == [{"key":"id","op":"=","value":$id}]' \
        /tmp/vmqd-rt.json >/dev/null \
        || { jq -c .stores /tmp/vmqd-rt.json; return 1; }
    got=$(timberfs query --query /tmp/vmqd-rt.json 2>/dev/null)
    [ "$got" = "2026-08-26T10:00:00Z ERROR console boom" ] \
        || { echo "round-trip gave '$got'" >&2; cat /tmp/vmqd-rt.json >&2; return 1; }

    # Several stores become an alternation over their ids, because the
    # predicate is a conjunction — ugly to read, exact, and it round-trips.
    timberfs query "$a/vmqd-a.log" "$b/vmqd-b.log" --no-filename \
        --dump-json > /tmp/vmqd-rt2.json 2>/dev/null || return 1
    jq -e '.stores.select | length == 1 and .[0].op == "=~"' \
        /tmp/vmqd-rt2.json >/dev/null || { jq -c .stores /tmp/vmqd-rt2.json; return 1; }
    got=$(timberfs query --query /tmp/vmqd-rt2.json 2>/dev/null | sort | tr '\n' ',')
    [ "$got" = "2026-08-26T10:00:00Z ERROR app boom,2026-08-26T10:00:00Z ERROR console boom," ] \
        || { echo "two-store round-trip gave '$got'" >&2; return 1; }
}

a_forest_is_declared_by_a_command_not_by_hand_editing() {
    # A forest is the one thing a timberfs command names by path, and
    # until now the only way to declare one was to write the .conf. This
    # is that, against the real /etc — including the default.conf the
    # package ships, which every check here has to coexist with.
    local d=/srv/vmforest
    rm -rf "$d" /etc/timberfs/forests.d/vmforest.conf

    # The package's own forest is declared and usable.
    timberfs forest list --json > /tmp/vmf.json 2>/dev/null || return 1
    jq -e '.[] | select(.dir == "/var/log/timberfs")
                 | .exists == true and .writable == true' /tmp/vmf.json >/dev/null \
        || { cat /tmp/vmf.json; return 1; }

    # Declare a second one: the directory is created and the conf written.
    timberfs forest create "$d" --name vmforest >/dev/null 2>&1 || return 1
    [ -d "$d" ] || { echo "directory not created" >&2; return 1; }
    grep -q "^DIR=$d\$" /etc/timberfs/forests.d/vmforest.conf \
        || { cat /etc/timberfs/forests.d/vmforest.conf >&2; return 1; }
    # ...and what was written is what gets READ back.
    timberfs forest list --names 2>/dev/null | grep -qx vmforest || return 1

    # Idempotent, so provisioning may run it on every boot.
    timberfs forest create "$d" --name vmforest >/dev/null 2>&1 || return 1
    [ "$(ls /etc/timberfs/forests.d/*.conf | wc -l)" = 2 ] || return 1

    # A store in the new forest is reachable by its bare handle, which is
    # the whole point of declaring one.
    timberfs create --set type=app --set service=vmf "$d/vmf/vmf.log" >/dev/null 2>&1 \
        || return 1
    printf '2026-08-26T09:00:00Z vmf line\n' \
        | timberfs append --into "$d/vmf/vmf.log" --quiet 2>/dev/null || return 1
    local got
    got=$(timberfs query vmf --no-filename 2>/dev/null)
    [ "$got" = "2026-08-26T09:00:00Z vmf line" ] \
        || { echo "handle lookup gave '$got'" >&2; return 1; }
    timberfs forest list 2>/dev/null | grep -E "^vmforest +1 +ok" >/dev/null \
        || { timberfs forest list; return 1; }

    # Every overlap is refused, because a forest is scanned one level deep
    # and a store with two forests has an unresolvable handle.
    timberfs forest create "$d" --name second >/dev/null 2>&1 && return 1
    timberfs forest create "$d/inside" >/dev/null 2>&1 && return 1
    timberfs forest create /srv --name parent >/dev/null 2>&1 && return 1
    # Re-pointing a name would strand every store under it.
    timberfs forest create /srv/other --name vmforest >/dev/null 2>&1 && return 1
    # A refused create leaves nothing behind — not even the directory.
    [ ! -d "$d/inside" ] && [ ! -d /srv/other ] || return 1
    [ "$(ls /etc/timberfs/forests.d/*.conf | wc -l)" = 2 ] || return 1

    # MISSING is reported rather than the forest being silently skipped:
    # this is the check that catches an unwritable write path early.
    mv "$d" "$d.moved"
    timberfs forest list 2>/dev/null | grep -E "^vmforest +0 +MISSING" >/dev/null \
        || { timberfs forest list; mv "$d.moved" "$d"; return 1; }
    mv "$d.moved" "$d"

    # remove un-declares and keeps the data, so it can never be the
    # command that loses a store.
    timberfs forest remove vmforest >/dev/null 2>&1 || return 1
    [ ! -e /etc/timberfs/forests.d/vmforest.conf ] || return 1
    [ -f "$d/vmf/vmf.log.rings" ] || { echo "remove took the data" >&2; return 1; }
    timberfs forest remove vmforest >/dev/null 2>&1 && return 1

    rm -rf "$d"
}

an_intake_writes_into_a_forest_by_name() {
    # An intake creates stores as data arrives, so it has to be told
    # where. That is a forest NAME now; --into-dir still works and warns,
    # because a directory that is not a forest has no other way in.
    local d=/srv/vmintake
    rm -rf "$d" /etc/timberfs/forests.d/vmintake.conf
    timberfs forest create "$d" --name vmintake >/dev/null 2>&1 || return 1

    # An unknown name is an error that LISTS what is declared — the typo
    # answer that a bare "not found" cannot give.
    timberfs otlp-intake --forest nope >/tmp/vmi.out 2>/tmp/vmi.err && return 1
    grep -q "no forest named .nope" /tmp/vmi.err || { cat /tmp/vmi.err >&2; return 1; }
    grep -q "vmintake" /tmp/vmi.err || { cat /tmp/vmi.err >&2; return 1; }

    # Neither flag is a usage error naming both ways out.
    timberfs frames-intake >/dev/null 2>/tmp/vmi.err && return 1
    grep -q -- "--forest" /tmp/vmi.err && grep -q -- "--into-dir" /tmp/vmi.err \
        || { cat /tmp/vmi.err >&2; return 1; }

    # Both is refused rather than one being picked silently.
    timberfs otlp-intake --forest vmintake --into-dir /tmp >/dev/null 2>&1 && return 1

    # --into-dir still works, and says it is deprecated.
    timberfs forward-intake --into-dir /nonexistent-vmi >/dev/null 2>/tmp/vmi.err && return 1
    grep -qi "into-dir.*deprecated" /tmp/vmi.err || { cat /tmp/vmi.err >&2; return 1; }

    # `list --forest` is scoped to that one forest. The write path itself
    # is covered by the four intake suites, which now run through
    # `--forest default` via the units the package ships.
    timberfs create --set service=vmi "$d/vmi/vmi.log" >/dev/null 2>&1 || return 1
    timberfs list --forest vmintake --names 2>/dev/null | grep -qx vmi || return 1
    [ "$(timberfs list --forest vmintake --names 2>/dev/null | wc -l)" = 1 ] \
        || { timberfs list --forest vmintake; return 1; }

    timberfs forest remove vmintake >/dev/null 2>&1
    rm -rf "$d"
}

a_match_selects_what_it_says_it_selects() {
    # The defect 0.22.0 shipped: `match` called itself an entry predicate
    # and selected CHUNKS, so a term in one entry returned every entry of
    # every chunk that might hold it. Asserted on real behaviour, because
    # the round-trip tests that existed all passed while it was wrong.
    local d=/var/log/timberfs/vmgran
    rm -rf "$d"
    timberfs create --set index=true "$d/vmgran.log" >/dev/null 2>&1 || return 1
    # Four chunks; the needle is in the third and nowhere else.
    local c i
    for c in 1 2 3 4; do
        for i in $(seq 1 50); do
            printf '2026-08-26T1%d:%02d:00Z INFO chunk %d line %d\n' "$c" "$i" "$c" "$i"
        done > /tmp/vmgran.$c
        [ "$c" = 3 ] && printf '2026-08-26T13:51:00Z ERROR vmgranneedle here\n' >> /tmp/vmgran.$c
        timberfs append --into "$d/vmgran.log" --quiet < /tmp/vmgran.$c 2>/dev/null || return 1
    done
    [ "$(timberfs index "$d/vmgran.log" 2>/dev/null | grep -c '^ ')" = 4 ] || {
        timberfs index "$d/vmgran.log"; return 1; }

    doc() { cat > /tmp/vmgran.json <<JSON
{ "v":"1.0-EXPERIMENTAL", "stores":{"select":[{"key":"name","op":"=","value":"vmgran"}]},
  "window":{"axis":"$2"}, $1
  "response_format":{"kind":"$3","options":{"no_filename":true}} }
JSON
    }
    local n
    # entries: the one that matches, and nothing else.
    doc '"match":{"granularity":"entries","all":[{"has":"vmgranneedle"}]},' logline loglines
    n=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    [ "$n" = 1 ] || { echo "entries gave $n lines, want 1" >&2; return 1; }
    # chunks: the whole chunk it sits in — a superset, and that is the point.
    doc '"match":{"granularity":"chunks","all":[{"has":"vmgranneedle"}]},' logline loglines
    n=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    [ "$n" = 51 ] || { echo "chunks gave $n lines, want 51" >&2; return 1; }

    # Saying nothing is refused: the granularity has no default, because a
    # default is the assumption that shipped wrong.
    printf '{"v":"1.0-EXPERIMENTAL","stores":{},"match":{"all":[{"has":"x"}]}}\n' > /tmp/vmgran-bare.json
    timberfs query --query /tmp/vmgran-bare.json >/dev/null 2>&1 && return 1

    # Entries cannot be judged inside chunks nothing decompresses.
    doc '"match":{"granularity":"entries","all":[{"has":"vmgranneedle"}]},' write chunks
    timberfs query --query /tmp/vmgran.json >/dev/null 2>&1 && return 1

    # The response says which granularity it applied, so a consumer can
    # tell a superset from an answer.
    doc '"match":{"granularity":"chunks","all":[{"has":"vmgranneedle"}]},' logline records
    timberfs query --query /tmp/vmgran.json 2>/dev/null \
        | tr '\036\037\0' '\n|\n' | grep -q '^stream-start.*granularity=chunks' \
        || { echo "stream-start did not declare the granularity" >&2; return 1; }

    # A bound names its unit, and a chunk bound reads chunks.
    doc '"max":{"chunks":2},' logline loglines
    n=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    [ "$n" = 100 ] || { echo "max.chunks=2 gave $n lines, want 100" >&2; return 1; }
    # The LAST chunk, which is the 50-line one — the needle's chunk has 51.
    doc '"tail":{"chunks":1},' logline loglines
    n=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    [ "$n" = 50 ] || { echo "tail.chunks=1 gave $n lines, want 50" >&2; return 1; }
    doc '"max":{"entries":3},' logline loglines
    n=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    [ "$n" = 3 ] || { echo "max.entries=3 gave $n lines, want 3" >&2; return 1; }
    # Neither unit, or both, is refused rather than picked between.
    doc '"max":{},' logline loglines
    timberfs query --query /tmp/vmgran.json >/dev/null 2>&1 && return 1
    doc '"max":{"entries":3,"chunks":2},' logline loglines
    timberfs query --query /tmp/vmgran.json >/dev/null 2>&1 && return 1

    # The predicate set is what a CALLER wants to ask, not what the index
    # can do cheaply: substring, regex, caseless and exclusion all work at
    # entry granularity, and each is judged on the whole entry.
    doc '"match":{"granularity":"entries","none":[{"has":"vmgranneedle"}]},' logline loglines
    n=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    [ "$n" = 200 ] || { echo "none gave $n lines, want 200" >&2; return 1; }
    doc '"match":{"granularity":"entries","all":[{"substring":"granneedl"}]},' logline loglines
    n=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    [ "$n" = 1 ] || { echo "substring gave $n lines, want 1" >&2; return 1; }
    doc '"match":{"granularity":"entries","all":[{"regex":"vmgran[a-z]+ here$"}]},' logline loglines
    n=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    [ "$n" = 1 ] || { echo "regex gave $n lines, want 1" >&2; return 1; }
    doc '"match":{"granularity":"entries","all":[{"has":"VMGRANNEEDLE","caseless":true}]},' logline loglines
    n=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    [ "$n" = 1 ] || { echo "caseless gave $n lines, want 1" >&2; return 1; }

    # A chunk sweep narrows with the index and nothing else, so what it
    # cannot prove is REFUSED rather than accepted and ignored.
    for m in '"none":[{"has":"x"}]' '"all":[{"regex":"x"}]' \
             '"all":[{"has":"x","caseless":true}]'; do
        doc "\"match\":{\"granularity\":\"chunks\",$m}," logline loglines
        timberfs query --query /tmp/vmgran.json >/dev/null 2>&1 && {
            echo "chunk sweep accepted $m" >&2; return 1; }
    done
    # ...and a substring IS allowed there: it rides the index on its
    # interior whole words.
    doc '"match":{"granularity":"chunks","all":[{"substring":"a vmgranneedle b"}]},' logline loglines
    timberfs query --query /tmp/vmgran.json >/dev/null 2>&1 || return 1

    # Exactly one matcher per predicate.
    doc '"match":{"granularity":"entries","all":[{}]},' logline loglines
    timberfs query --query /tmp/vmgran.json >/dev/null 2>&1 && return 1
    doc '"match":{"granularity":"entries","all":[{"has":"a","regex":"b"}]},' logline loglines
    timberfs query --query /tmp/vmgran.json >/dev/null 2>&1 && return 1

    # timber-filter and the document are ONE implementation, so the same
    # predicate must give the same count through either surface.
    doc '"match":{"granularity":"entries","all":[{"substring":"granneedl"}],"none":[{"has":"INFO"}]},' logline loglines
    local viadoc viafilter
    viadoc=$(timberfs query --query /tmp/vmgran.json 2>/dev/null | wc -l)
    viafilter=$(timber-filter --substring granneedl --not-has INFO -c "$d/vmgran.log" 2>/dev/null)
    [ "$viadoc" = "$viafilter" ] \
        || { echo "document says $viadoc, timber-filter says $viafilter" >&2; return 1; }

    rm -rf "$d" /tmp/vmgran.*
}

a_bounded_read_says_what_stopped_it_and_invents_nothing() {
    # Three things a client cannot work without, and all three were wrong:
    # which bound fired, how much was actually read, and whether the last
    # entry is whole.
    local d=/var/log/timberfs/vmbound
    rm -rf "$d"
    timberfs create "$d/vmbound.log" >/dev/null 2>&1 || return 1
    # Each append seals a chunk at EOF, so splitting the writes puts the
    # 41-line entry across a chunk boundary DETERMINISTICALLY — no
    # dependence on where a size threshold happens to land.
    {
        for i in $(seq 1 20); do printf '2026-08-27T10:00:%02dZ INFO filler %d\n' "$i" "$i"; done
        printf '2026-08-27T10:00:30Z ERROR vmboundboom\n'
        for i in $(seq 1 20); do printf '\tat frame.number.%d(F.java:1)\n' "$i"; done
    } | timberfs append --into "$d/vmbound.log" --quiet 2>/dev/null || return 1
    {
        for i in $(seq 21 40); do printf '\tat frame.number.%d(F.java:1)\n' "$i"; done
        printf '2026-08-27T10:01:00Z INFO after\n'
    } | timberfs append --into "$d/vmbound.log" --quiet 2>/dev/null || return 1
    local chunks
    chunks=$(timberfs index "$d/vmbound.log" 2>/dev/null | grep -c '^ ')
    [ "$chunks" = 2 ] || { echo "want 2 chunks, got $chunks" >&2; timberfs index "$d/vmbound.log"; return 1; }
    # The entry is 41 lines when read whole.
    [ "$(timber-filter --has vmboundboom "$d/vmbound.log" 2>/dev/null | wc -l)" = 41 ] || return 1

    bdoc() { cat > /tmp/vmbound.json <<JSON
{ "v":"1.0-EXPERIMENTAL","stores":{"select":[{"key":"name","op":"=","value":"vmbound"}]},
  "window":{"axis":"logline"}, $1 "response_format":{"kind":"records"} }
JSON
    }
    local end frames
    # A chunk cap stops MID-ENTRY by construction. The half-read stack
    # trace must not be emitted: half a stack trace presented as whole is
    # not missing data, it is data that never existed.
    bdoc '"max":{"chunks":1},'
    frames=$(timberfs query --query /tmp/vmbound.json 2>/dev/null \
        | tr '\0' '\n' | grep -c 'at frame.number')
    [ "$frames" = 0 ] || { echo "emitted $frames frames of a cut-off entry" >&2; return 1; }

    # ...and it names the bound that fired, and counts what it READ.
    end=$(timberfs query --query /tmp/vmbound.json 2>/dev/null \
        | tr '\036\037\0' '\n|\n' | grep '^stream-end')
    echo "$end" | grep -q 'limit=max.chunks' || { echo "$end" >&2; return 1; }
    echo "$end" | grep -q 'chunks_read=1' || { echo "$end" >&2; return 1; }
    echo "$end" | grep -q 'status=limited' || { echo "$end" >&2; return 1; }

    # An entry cap names ITSELF, not the chunk cap.
    bdoc '"max":{"entries":5},'
    end=$(timberfs query --query /tmp/vmbound.json 2>/dev/null \
        | tr '\036\037\0' '\n|\n' | grep '^stream-end')
    echo "$end" | grep -q 'limit=max.entries' || { echo "$end" >&2; return 1; }

    # Unbounded: nothing was cut, so the entry comes back WHOLE — the
    # discard must not fire at genuine end-of-data, where a pending entry
    # is merely unterminated.
    bdoc ''
    end=$(timberfs query --query /tmp/vmbound.json 2>/dev/null \
        | tr '\036\037\0' '\n|\n' | grep '^stream-end')
    echo "$end" | grep -q 'status=exhausted' || { echo "$end" >&2; return 1; }
    echo "$end" | grep -q 'limit=' && { echo "named a bound with none set: $end" >&2; return 1; }
    frames=$(timberfs query --query /tmp/vmbound.json 2>/dev/null \
        | tr '\0' '\n' | grep -c 'at frame.number')
    [ "$frames" = 40 ] || { echo "unbounded gave $frames frames, want 40" >&2; return 1; }

    rm -rf "$d" /tmp/vmbound.json
}

a_framed_answer_reads_stores_one_after_another() {
    # A bounded framed answer claims order WITHIN a store and none between,
    # so each store's entries must come back contiguous. Built so the two
    # rules disagree: the appends alternate, so the stores' chunks alternate
    # in write time and a read that interleaved them would emit a b a b.
    local a=/var/log/timberfs/vmseq-a b=/var/log/timberfs/vmseq-b
    rm -rf "$a" "$b"
    timberfs create --set service=vmseq "$a/vmseq-a.log" >/dev/null 2>&1 || return 1
    timberfs create --set service=vmseq "$b/vmseq-b.log" >/dev/null 2>&1 || return 1
    # Each append seals a chunk at EOF, so alternating the appends puts the
    # two stores' write windows in a-b-a-b order deterministically.
    printf '2026-08-28T10:00:00Z vmseq a1\n' \
        | timberfs append --into "$a/vmseq-a.log" --quiet 2>/dev/null || return 1
    printf '2026-08-28T10:00:01Z vmseq b1\n' \
        | timberfs append --into "$b/vmseq-b.log" --quiet 2>/dev/null || return 1
    printf '2026-08-28T10:00:02Z vmseq a2\n' \
        | timberfs append --into "$a/vmseq-a.log" --quiet 2>/dev/null || return 1
    printf '2026-08-28T10:00:03Z vmseq b2\n' \
        | timberfs append --into "$b/vmseq-b.log" --quiet 2>/dev/null || return 1

    cat > /tmp/vmseq.json <<'JSON'
{ "v":"1.0-EXPERIMENTAL","stores":{"select":[{"key":"service","op":"=","value":"vmseq"}]},
  "window":{"axis":"logline"},"response_format":{"kind":"records"} }
JSON
    local out runs n
    out=$(timberfs query --query /tmp/vmseq.json 2>/dev/null | tr '\0' '\n')
    n=$(echo "$out" | grep -c 'vmseq [ab][12]')
    [ "$n" = 4 ] || { echo "want 4 entries, got $n" >&2; return 1; }
    # Collapsing adjacent duplicates leaves one run per store when the
    # stores are contiguous, and four when they are interleaved.
    runs=$(echo "$out" | grep -o 'vmseq [ab]' | awk '{print $2}' | uniq | tr -d '\n')
    [ "${#runs}" = 2 ] || { echo "stores interleaved: $runs" >&2; return 1; }
    # ...and the answer SAYS so, rather than leaving a consumer to infer it.
    timberfs query --query /tmp/vmseq.json 2>/dev/null \
        | tr '\036\037\0' '\n|\n' | grep '^stream-start' \
        | grep -q 'order=sequential' || { echo "stream-start omitted order" >&2; return 1; }

    # The TEXT fleet view still interleaves: it makes many logs readable as
    # one, and has no next page to contradict.
    runs=$(timberfs query "$a/vmseq-a.log" "$b/vmseq-b.log" 2>/dev/null \
        | grep -o 'vmseq [ab]' | awk '{print $2}' | uniq | tr -d '\n')
    [ "${#runs}" = 4 ] || { echo "text view stopped interleaving: $runs" >&2; return 1; }

    rm -rf "$a" "$b" /tmp/vmseq.json
}

a_deadline_bounds_the_wait_and_the_answer_says_where_it_stopped() {
    # A deadline is the one bound whose effect is not reproducible, so the
    # assertions are the INVARIANTS rather than a particular split.
    # A forest is scanned ONE level deep, so each store is its own
    # directory directly under it.
    local i n
    rm -rf /var/log/timberfs/vmdl-1 /var/log/timberfs/vmdl-2 /var/log/timberfs/vmdl-3
    for i in 1 2 3; do
        local st=/var/log/timberfs/vmdl-$i/vmdl-$i.log
        timberfs create --set svc=vmdl "$st" >/dev/null 2>&1 || return 1
        seq 1 8000 | awk -v n=$i \
            '{printf "2026-08-28T10:00:%02dZ INFO vmdl store %s line %d filler filler\n", $1%60, n, $1}' \
            | timberfs append --into "$st" --chunk-size 4096 --quiet 2>/dev/null \
            || return 1
    done

    ddoc() { cat > /tmp/vmdl.json <<JSON
{ "v":"1.0-EXPERIMENTAL","stores":{"select":[{"key":"svc","op":"=","value":"vmdl"}]},
  "window":{"axis":"logline"}, $1 "response_format":{"kind":"records"} }
JSON
    }
    local end
    # Generous: it must not fire on its own, or every other assertion here
    # would pass for the wrong reason.
    ddoc '"deadline":{"ms":600000},'
    end=$(timberfs query --query /tmp/vmdl.json 2>/dev/null \
        | tr '\036\037\0' '\n|\n' | grep '^stream-end')
    echo "$end" | grep -q 'status=exhausted' || { echo "600s deadline fired: $end" >&2; return 1; }
    n=$(echo "$end" | sed 's/.*|entries=\([0-9]*\)|.*/\1/')
    [ "$n" = 24000 ] || { echo "unbounded gave $n entries, want 24000" >&2; return 1; }

    # 1 ms against ~300 chunks: it fires, and NAMES itself rather than
    # borrowing an entry cap's name.
    ddoc '"deadline":{"ms":1},'
    end=$(timberfs query --query /tmp/vmdl.json 2>/dev/null \
        | tr '\036\037\0' '\n|\n' | grep '^stream-end')
    echo "$end" | grep -q 'status=limited' || { echo "$end" >&2; return 1; }
    echo "$end" | grep -q 'limit=deadline' || { echo "$end" >&2; return 1; }

    # The STAIRCASE, which holds wherever the deadline lands and is the
    # sequential read made visible: stores complete until the one it
    # stopped inside, and nothing after that was opened at all.
    timberfs query --query /tmp/vmdl.json 2>/dev/null | tr '\036\037\0' '\n|\n' \
        | grep '^position' | sed 's/.*|chunks_read=\([0-9]*\)|chunks_selected=\([0-9]*\).*/\1 \2/' \
        | awk 'BEGIN{done=0}
               {if (done && $1 != 0) {print "store read after the deadline stopped one"; exit 1}
                if ($1 < $2) done=1}
               END{exit 0}' || return 1

    # Zero is refused rather than obeyed, and a follow has no end to bound.
    ddoc '"deadline":{"ms":0},'
    timberfs query --query /tmp/vmdl.json >/dev/null 2>/tmp/vmdl.err && {
        echo "a zero deadline was accepted" >&2; return 1; }
    grep -q 'zero' /tmp/vmdl.err || { cat /tmp/vmdl.err >&2; return 1; }
    timberfs query --deadline 5 --follow /var/log/timberfs/vmdl-1/vmdl-1.log \
        >/dev/null 2>/tmp/vmdl.err && {
        echo "--deadline with --follow was accepted" >&2; return 1; }

    rm -rf /var/log/timberfs/vmdl-1 /var/log/timberfs/vmdl-2 /var/log/timberfs/vmdl-3 \
        /tmp/vmdl.json /tmp/vmdl.err
}

dump_json_needs_no_store_so_it_can_answer_what_a_time_means() {
    # `--dump-json` reads nothing, so requiring a store made the one job it
    # is uniquely good for impossible: telling a client what a typed time
    # means, without that client writing a second date parser.
    local out
    out=$(timberfs query --from '2026-08-28 11:10' --dump-json 2>&1) || {
        echo "$out" >&2; return 1; }
    echo "$out" | grep -q '"from": 17' || { echo "$out" >&2; return 1; }
    # No store named means EVERY store, which is what an empty predicate is.
    echo "$out" | grep -q '"stores": {}' || { echo "$out" >&2; return 1; }

    # A bad time still fails, and says so rather than reporting a missing
    # argument for one that was given.
    timberfs query --from 'not a time' --dump-json >/dev/null 2>/tmp/vmdj.err && {
        echo "a nonsense time was accepted" >&2; return 1; }
    grep -q 'unrecognized time' /tmp/vmdj.err || { cat /tmp/vmdj.err >&2; return 1; }

    # And a read that actually reads still requires one.
    timberfs query --from '11:00' >/dev/null 2>/tmp/vmdj.err && {
        echo "a real read ran with no store" >&2; return 1; }
    grep -q 'FILES' /tmp/vmdj.err || { cat /tmp/vmdj.err >&2; return 1; }

    rm -f /tmp/vmdj.err
}

an_unknown_selector_operator_is_refused_not_quietly_truncated() {
    # A near miss cannot be guessed at safely: the parser tries operators
    # longest-first, so an unknown one is not rejected there — a shorter
    # one is found inside it and the rest joins the VALUE. `=?` matched
    # nothing and `!=X` matched nearly everything, both answered as though
    # understood.
    local d=/var/log/timberfs/vmop
    rm -rf "$d"
    timberfs create --set svc=vmop "$d/vmop.log" >/dev/null 2>&1 || return 1
    printf '2026-08-28T10:00:00Z INFO vmop line\n' \
        | timberfs append --into "$d/vmop.log" --quiet 2>/dev/null || return 1

    opdoc() { printf '{"v":"1.0-EXPERIMENTAL","stores":{"select":[{"key":"svc","op":"%s","value":"vmop"}]},"response_format":{"kind":"stores"}}' "$1" > /tmp/vmop.json; }
    local op out
    for op in '=?' '!=Y' 'LIKE'; do
        opdoc "$op"
        if timberfs query --query /tmp/vmop.json >/dev/null 2>/tmp/vmop.err; then
            echo "operator $op was accepted" >&2; return 1
        fi
        # It names the operator it was given AND the ones that exist, so a
        # generator is told rather than left to diff two answers.
        grep -q -- "$op" /tmp/vmop.err || { cat /tmp/vmop.err >&2; return 1; }
        grep -q -- '=\*' /tmp/vmop.err || { cat /tmp/vmop.err >&2; return 1; }
    done

    # Every real operator still works, including the one whose absence
    # started this. `=~` is anchored, so the bare value is a whole match.
    for op in '=' '=~' '=*'; do
        opdoc "$op"
        out=$(timberfs query --query /tmp/vmop.json 2>/dev/null | grep -c '"svc"') || true
        [ "$out" -ge 1 ] || { echo "operator $op matched nothing" >&2; return 1; }
    done

    rm -rf "$d" /tmp/vmop.json /tmp/vmop.err
}

a_quiet_store_keeps_its_place_across_pages() {
    # The defect: a store that delivers nothing on a page reported a
    # `position` with NO offset, and an offsetless cursor entry IS the
    # start of the window — so handing the answer back, exactly as the
    # format says to, re-read every store that had gone quiet.
    #
    # Two stores of different lengths is what shows it. Stores are read one
    # after another, so the short one is exhausted while the long one is
    # still going, and the page after that used to re-deliver it.
    local a=/var/log/timberfs/vmquiet-a b=/var/log/timberfs/vmquiet-b i
    rm -rf "$a" "$b"
    timberfs create --set svc=vmquiet "$a/vmquiet-a.log" >/dev/null 2>&1 || return 1
    timberfs create --set svc=vmquiet "$b/vmquiet-b.log" >/dev/null 2>&1 || return 1
    for i in 1 2; do
        printf '2026-08-28T11:00:00Z vmquiet A entry %d\n' "$i" \
            | timberfs append --into "$a/vmquiet-a.log" --quiet 2>/dev/null || return 1
    done
    for i in 1 2 3 4; do
        printf '2026-08-28T11:00:00Z vmquiet B entry %d\n' "$i" \
            | timberfs append --into "$b/vmquiet-b.log" --quiet 2>/dev/null || return 1
    done

    # Walk two at a time, feeding each answer's positions straight back.
    python3 - <<'PY'
import json, subprocess, sys, collections

def page(cursor):
    doc = {"v": "1.0-EXPERIMENTAL",
           "stores": {"select": [{"key": "svc", "op": "=", "value": "vmquiet"}]},
           "window": {"axis": "logline"},
           "max": {"entries": 2},
           "response_format": {"kind": "records"}}
    if cursor:
        doc["cursor"] = cursor
    p = subprocess.run(["timberfs", "query", "--query", "-"],
                       input=json.dumps(doc), capture_output=True, text=True)
    assert p.returncode == 0, p.stderr
    entries, positions, limited = [], [], False
    for rec in p.stdout.split("\x1e"):
        if not rec:
            continue
        head, _, rest = rec.partition("\0")
        parts = head.split("\x1f")
        f = dict(x.split("=", 1) for x in parts[1:] if "=" in x)
        if parts[0] == "entry":
            entries.append(rest.split("\0", 1)[0].strip())
        elif parts[0] == "position" and f.get("id"):
            # VERBATIM: whatever the answer said, unedited. That is the
            # contract, and it is what used to lose a quiet store.
            q = {"id": f["id"]}
            if "offset" in f:
                q["offset"] = int(f["offset"])
            positions.append(q)
        elif parts[0] == "stream-end":
            limited = f.get("status") == "limited"
    return entries, positions, limited

seen, cursor = [], []
for _ in range(8):
    got, cursor, more = page(cursor)
    seen += got
    if not more:
        break

want = 6
counts = collections.Counter(seen)
dupes = {k: v for k, v in counts.items() if v > 1}
if len(seen) != want or dupes:
    print(f"walked {len(seen)} entries, {len(counts)} distinct (want {want})",
          file=sys.stderr)
    for k, v in sorted(dupes.items()):
        print(f"  DELIVERED {v}x: {k}", file=sys.stderr)
    sys.exit(1)
PY
    local rc=$?
    rm -rf "$a" "$b"
    return $rc
}

paging_walks_a_result_set_a_page_at_a_time() {
    # Every entry once, in order, on a store whose entries ALL share a
    # timestamp — the case that makes paging by clock lose everything
    # that shared the last one. A position is an offset on the store's
    # tape, so six entries of the same second are six positions.
    local d=/var/log/timberfs/vmpage
    rm -rf "$d"
    timberfs create --set service=vmpage "$d/vmpage.log" >/dev/null 2>&1 || return 1
    local i
    for i in 1 2 3 4 5 6; do
        printf '2026-08-27T10:00:00Z vmpage entry %d\n' "$i" \
            | timberfs append --into "$d/vmpage.log" --quiet 2>/dev/null || return 1
    done
    local id
    id=$(timberfs info --json "$d/vmpage.log" 2>/dev/null | jq -r .id)
    [ -n "$id" ] && [ "$id" != null ] || { echo "no store id" >&2; return 1; }

    local cur="" got="" page off all="" prev=""
    for page in 1 2 3 4; do
        cat > /tmp/vmpage.json <<JSON
{ "v":"1.0-EXPERIMENTAL",
  "stores":{"select":[{"key":"service","op":"=","value":"vmpage"}]},
  "window":{"axis":"logline"}, "max":{"entries":2}, $cur
  "response_format":{"kind":"records"} }
JSON
        got=$(timberfs query --query /tmp/vmpage.json 2>/dev/null \
              | tr '\0' '\n' | grep -o 'vmpage entry [0-9]' | tr '\n' ' ')
        off=$(timberfs query --query /tmp/vmpage.json 2>/dev/null \
              | tr '\036\037\0' '\n|\n' | grep '^position' \
              | grep -o 'offset=[0-9]*' | cut -d= -f2)
        all="$all$got"
        if [ "$page" = 4 ]; then
            # Walked out: nothing left, and the position UNCHANGED rather
            # than gone. Nothing moved, so it did not move either — and an
            # absent offset would mean the start of the window, so a client
            # handing this answer back would re-read the store whole.
            [ -z "$got" ] || { echo "page 4 gave '$got', want nothing" >&2; return 1; }
            [ "$off" = "$prev" ] \
                || { echo "exhausted page moved the position: $prev -> ${off:-none}" >&2
                     return 1; }
            break
        fi
        [ -n "$off" ] || { echo "page $page offered no position" >&2; return 1; }
        prev=$off
        cur="\"cursor\":[{\"id\":\"$id\",\"offset\":$off}],"
    done
    # Every entry, once, in order.
    [ "$all" = "vmpage entry 1 vmpage entry 2 vmpage entry 3 vmpage entry 4 vmpage entry 5 vmpage entry 6 " ] \
        || { echo "walked: '$all'" >&2; return 1; }

    # A store that matched NOTHING still reports where it got to, or the
    # next page rescans it from the start of the window.
    cat > /tmp/vmpage2.json <<'JSON'
{ "v":"1.0-EXPERIMENTAL",
  "stores":{"select":[{"key":"service","op":"=","value":"vmpage"}]},
  "window":{"axis":"logline"},
  "match":{"granularity":"entries","all":[{"has":"nosuchtermhere"}]},
  "response_format":{"kind":"records"} }
JSON
    timberfs query --query /tmp/vmpage2.json 2>/dev/null | tr '\036\037\0' '\n|\n' \
        | grep '^position' | grep -q "id=$id" \
        || { echo "a barren store reported no position" >&2; return 1; }

    rm -rf "$d" /tmp/vmpage.json /tmp/vmpage2.json
}

the_query_examples_ship_and_run() {
    # A format nobody can see worked examples of is one nobody uses. The
    # schema says what is legal; these say what is useful — so they are
    # installed, indexed, and every one of them actually runs.
    local dir=/usr/share/doc/timberfs/query-examples
    [ -f "$dir/README.md" ] || { echo "no README in $dir" >&2; ls -la "$dir" >&2; return 1; }
    local n
    n=$(ls "$dir"/*.json 2>/dev/null | wc -l)
    [ "$n" -ge 7 ] || { echo "only $n examples shipped" >&2; return 1; }

    # The README must name every one of them, or a new example is
    # invisible in exactly the place it was added to be visible.
    local f base
    for f in "$dir"/*.json; do
        base=$(basename "$f")
        grep -q "$base" "$dir/README.md" \
            || { echo "$base is not in the README" >&2; return 1; }
    done

    # Every example is a document this build accepts. They name stores
    # that need not exist here, so a "no store matched" answer is a pass;
    # a PARSE or coherence refusal is not.
    local d=/var/log/timberfs/vmex
    rm -rf "$d"
    timberfs create --set type=app --set host=web01 "$d/vmex.log" >/dev/null 2>&1 || return 1
    printf '2026-08-26T12:00:00Z ERROR vmex req-8f3a\n' \
        | timberfs append --into "$d/vmex.log" --quiet 2>/dev/null || return 1
    for f in "$dir"/*.json; do
        timberfs query --query "$f" >/dev/null 2>/tmp/vmex.err || {
            echo "$(basename "$f") does not run:" >&2; cat /tmp/vmex.err >&2; return 1; }
    done

    # Enumerating is the store predicate with nothing in it, and it must
    # find the store we just made — that is the request a client opens with.
    timberfs query --query "$dir/query-enumerate-stores.json" 2>/dev/null \
        | jq -e '[.[] | select(.name == "vmex")] | length == 1' >/dev/null \
        || { echo "enumerate did not find vmex" >&2; return 1; }

    # A response names its stores by IDENTITY, not only by path: that is
    # the join key between what a request asked for and what came back.
    cat > /tmp/vmex.json <<'JSON'
{ "v":"1.0-EXPERIMENTAL","stores":{"select":[{"key":"name","op":"=","value":"vmex"}]},
  "window":{"axis":"logline"},"match":{"granularity":"entries","all":[{"has":"vmex"}]},
  "response_format":{"kind":"records"} }
JSON
    local id
    id=$(timberfs info --json "$d/vmex.log" 2>/dev/null | jq -r .id)
    timberfs query --query /tmp/vmex.json 2>/dev/null | tr '\036\037\0' '\n|\n' \
        | grep '^source' | grep -q "id=$id" \
        || { timberfs query --query /tmp/vmex.json 2>/dev/null | tr '\036\037\0' '\n|\n' \
                | grep '^source' >&2; return 1; }

    # An ENTRY says where it came from durably too, not only by path: the
    # records stream is a pipeline format, so a stage that rewrites the
    # source records must not strand entries with an unstable key.
    local d2=/var/log/timberfs/vmex2
    rm -rf "$d2"
    timberfs create --set service=vmex2 "$d2/vmex2.log" >/dev/null 2>&1 || return 1
    printf '2026-08-26T12:00:00Z ERROR vmex req-8f3a\n' \
        | timberfs append --into "$d2/vmex2.log" --quiet 2>/dev/null || return 1
    local id2
    id2=$(timberfs info --json "$d2/vmex2.log" 2>/dev/null | jq -r .id)
    cat > /tmp/vmex2.json <<'JSON'
{ "v":"1.0-EXPERIMENTAL","stores":{"select":[{"key":"name","op":"=~","value":"vmex2?"}]},
  "window":{"axis":"logline"},"match":{"granularity":"entries","all":[{"has":"vmex"}]},
  "response_format":{"kind":"records"} }
JSON
    timberfs query --query /tmp/vmex2.json 2>/dev/null | tr '\036\037\0' '\n|\n' \
        | grep '^entry' | grep -q "id=$id2" || {
            echo "an attributed entry carries no identity:" >&2
            timberfs query --query /tmp/vmex2.json 2>/dev/null | tr '\036\037\0' '\n|\n' \
                | grep '^entry' >&2
            return 1; }
    # A single-store read attributes nothing — there is nothing to tell
    # apart, so neither src nor id is added.
    timberfs query --query /tmp/vmex.json 2>/dev/null | tr '\036\037\0' '\n|\n' \
        | grep '^entry' | grep -qE 'src=|id=' && {
            echo "single-store read attributed an entry it need not" >&2; return 1; }
    rm -rf "$d2" /tmp/vmex2.json

    # A following read is a process holding a stream open, not a search.
    # REFUSED rather than silently dropped, which is how a caller ends up
    # running a different query than the one it printed.
    timberfs query vmex --follow --dump-json >/dev/null 2>/tmp/vmex.err && return 1
    grep -q 'following read' /tmp/vmex.err || { cat /tmp/vmex.err >&2; return 1; }
    # ...while a non-following dump still round-trips.
    timberfs query vmex --has vmex --dump-json 2>/dev/null > /tmp/vmex-rt.json || return 1
    timberfs query --query /tmp/vmex-rt.json >/dev/null 2>&1 || return 1

    rm -rf "$d" /tmp/vmex.json /tmp/vmex-rt.json /tmp/vmex.err
}

identity_reports_and_repairs_the_three_broken_states() {
    # An id is a fact, not a setting, so `set` will not touch it. But it
    # can be broken in three ways, each with an obvious intended fix, and
    # an operator who knows which one applies needs a way to say so.
    local d=/var/log/timberfs/vmident
    rm -rf "$d"
    printf 'no manifest here\n' | timberfs append --into "$d/vmident.log" --quiet 2>/dev/null || return 1

    # 1. Nothing on either side: not a store. Reporting exits non-zero, so
    #    this doubles as the check a script runs.
    timberfs identity "$d/vmident.log" > /tmp/vmident.out 2>&1 && return 1
    grep -q 'verdict   NONE' /tmp/vmident.out || { cat /tmp/vmident.out >&2; return 1; }
    timberfs identity "$d/vmident.log" --mint >/dev/null 2>&1 || return 1
    timberfs identity "$d/vmident.log" > /tmp/vmident.out 2>&1 || return 1
    grep -q 'verdict   consistent' /tmp/vmident.out || { cat /tmp/vmident.out >&2; return 1; }
    local minted
    minted=$(jq -r .id "$d/vmident.log.bark")
    # Minting where one already exists is a --keep question, not a mint.
    timberfs identity "$d/vmident.log" --mint >/dev/null 2>&1 && return 1

    # 2. Two identities for one store: no writer may pick, and the report
    #    says which flag resolves it.
    python3 - "$d/vmident.log.bark" <<'PYBAD'
import json, sys
p = sys.argv[1]
m = json.load(open(p)); m["id"] = "ffffffff-0000-4000-8000-000000000000"
json.dump(m, open(p, "w"), indent=1)
PYBAD
    printf 'x\n' | timberfs append --into "$d/vmident.log" >/dev/null 2>&1 && return 1
    timberfs identity "$d/vmident.log" > /tmp/vmident.out 2>&1 && return 1
    grep -q 'verdict   DISAGREE' /tmp/vmident.out || { cat /tmp/vmident.out >&2; return 1; }

    # 3a. Keep the index: the pair IS the store, so this is the usual
    #     answer after a manifest was hand-edited or restored.
    timberfs identity "$d/vmident.log" --keep index >/dev/null 2>&1 || return 1
    [ "$(jq -r .id "$d/vmident.log.bark")" = "$minted" ] || return 1
    timberfs identity "$d/vmident.log" >/dev/null 2>&1 || return 1
    # A writer works again, and the data was never touched.
    printf 'second\n' | timberfs append --into "$d/vmident.log" --quiet 2>/dev/null || return 1
    timberfs query "$d/vmident.log" 2>/dev/null | grep -q 'no manifest here' || return 1

    # 3b. Keep the manifest: the other side of the same repair.
    python3 - "$d/vmident.log.bark" <<'PYBAD2'
import json, sys
p = sys.argv[1]
m = json.load(open(p)); m["id"] = "ffffffff-0000-4000-8000-000000000000"
json.dump(m, open(p, "w"), indent=1)
PYBAD2
    timberfs identity "$d/vmident.log" --keep manifest >/dev/null 2>&1 || return 1
    timberfs identity "$d/vmident.log" > /tmp/vmident.out 2>&1 || return 1
    grep -q 'ffffffff-0000-4000-8000-000000000000' /tmp/vmident.out || return 1
    grep -q 'verdict   consistent' /tmp/vmident.out || return 1

    # Repair rewrites the rings header, which a live writer also rewrites
    # on a head-drop. It refuses to race one.
    mkfifo /tmp/vmident.fifo
    timberfs append --into "$d/vmident.log" --flush-age 60 < /tmp/vmident.fifo >/dev/null 2>&1 &
    local wpid=$!
    exec 8>/tmp/vmident.fifo
    sleep 1
    timberfs identity "$d/vmident.log" --keep index > /tmp/vmident.err 2>&1
    local raced=$?
    exec 8>&-
    wait $wpid 2>/dev/null
    rm -f /tmp/vmident.fifo
    [ $raced -ne 0 ] || { echo "repair raced a live writer" >&2; return 1; }
    grep -q 'live writer' /tmp/vmident.err || { cat /tmp/vmident.err >&2; return 1; }
}

store_identity_lives_in_the_backing_pair() {
    # The `.bark` is a sidecar; the backing PAIR is the store. Identity
    # therefore lives in the .rings header too, so losing the manifest does
    # not lose what the store IS.
    local d=/var/log/timberfs/vmid
    rm -rf "$d"
    timberfs create --index --retain-size 16K --set host=vmhost "$d/vmid.log" >/dev/null 2>&1 || return 1
    local bark_id hdr_id
    bark_id=$(jq -r .id "$d/vmid.log.bark") || return 1
    hdr_id=$(python3 -c "
b = open('$d/vmid.log.rings','rb').read()[48:64]
h = b.hex()
print('NONE' if b == bytes(16) else '-'.join([h[0:8],h[8:12],h[12:16],h[16:20],h[20:32]]))")
    # Declared at create, so the pair carries it from the first byte.
    [ "$bark_id" = "$hdr_id" ] || { echo "bark=$bark_id header=$hdr_id" >&2; return 1; }

    # Retention rewrites the whole rings file. Identity must survive that,
    # or retention would quietly erase what the store is.
    python3 -c "
import base64, os, sys
for _ in range(3000):
    sys.stdout.write('2026-08-25T10:00:00Z ' + base64.b64encode(os.urandom(48)).decode() + '\n')" \
        | timberfs append --into "$d/vmid.log" --chunk-size 1024 --quiet 2>/dev/null || return 1
    local dropped after
    dropped=$(timberfs info --json "$d/vmid.log" | jq -r .dropped_chunks)
    [ "$dropped" -gt 0 ] || { echo "head-drop never ran (dropped=$dropped)" >&2; return 1; }
    after=$(python3 -c "
b = open('$d/vmid.log.rings','rb').read()[48:64]
h = b.hex()
print('NONE' if b == bytes(16) else '-'.join([h[0:8],h[8:12],h[12:16],h[16:20],h[20:32]]))")
    [ "$after" = "$bark_id" ] || { echo "retention lost the id: $after" >&2; return 1; }

    # A header written before this field existed is stamped from the
    # manifest on the next open: existing stores migrate themselves.
    python3 -c "
f = open('$d/vmid.log.rings','r+b'); f.seek(48); f.write(bytes(16)); f.close()"
    printf 'after the wipe\n' | timberfs append --into "$d/vmid.log" --quiet 2>/dev/null || return 1
    after=$(python3 -c "
b = open('$d/vmid.log.rings','rb').read()[48:64]
h = b.hex()
print('NONE' if b == bytes(16) else '-'.join([h[0:8],h[8:12],h[12:16],h[16:20],h[20:32]]))")
    [ "$after" = "$bark_id" ] || { echo "no self-migration: $after" >&2; return 1; }

    # Two identities for one store is refused, not resolved by picking.
    python3 -c "
import json
p = '$d/vmid.log.bark'
m = json.load(open(p)); m['id'] = 'ffffffff-0000-4000-8000-000000000000'
json.dump(m, open(p,'w'), indent=1)"
    printf 'x\n' | timberfs append --into "$d/vmid.log" > /tmp/vmid2.err 2>&1 && return 1
    grep -q 'two identities for one store' /tmp/vmid2.err || { cat /tmp/vmid2.err >&2; return 1; }
    python3 -c "
import json
p = '$d/vmid.log.bark'
m = json.load(open(p)); m['id'] = '$bark_id'
json.dump(m, open(p,'w'), indent=1)"

    # `create --if-not-exists` completes a pair that has no identity: it
    # has not been created yet in the only sense that matters, so reporting
    # "nothing created" at it would be reporting success at doing nothing.
    local bare=/var/log/timberfs/vmbareid
    rm -rf "$bare"
    printf 'no manifest here\n' | timberfs append --into "$bare/vmbareid.log" --quiet 2>/dev/null || return 1
    [ -e "$bare/vmbareid.log.bark" ] && { echo "bare append wrote a manifest?" >&2; return 1; }
    timberfs create --if-not-exists "$bare/vmbareid.log" > /tmp/vmine.out 2>&1 || return 1
    grep -q 'minted one' /tmp/vmine.out || { cat /tmp/vmine.out >&2; return 1; }
    local minted
    minted=$(jq -r .id "$bare/vmbareid.log.bark") || return 1
    # ...and the pair carries it, not just the manifest.
    python3 - "$bare/vmbareid.log.rings" "$minted" <<'PYMINT'
import sys
b = open(sys.argv[1], 'rb').read()[48:64]
h = b.hex()
got = '-'.join([h[0:8], h[8:12], h[12:16], h[16:20], h[20:32]])
sys.exit(0 if got == sys.argv[2] else 1)
PYMINT
    [ $? -eq 0 ] || { echo "header did not get the minted id" >&2; return 1; }
    # Idempotent: a second run has nothing to do.
    timberfs create --if-not-exists "$bare/vmbareid.log" 2>&1 | grep -q 'nothing created' || return 1
    # Manifest lost, pair intact: the identity is RECOVERED, not re-minted,
    # because the pair is the store.
    rm "$bare/vmbareid.log.bark"
    timberfs create --if-not-exists "$bare/vmbareid.log" > /tmp/vmine2.out 2>&1 || return 1
    grep -q 'recovered its identity from the index' /tmp/vmine2.out || { cat /tmp/vmine2.out >&2; return 1; }
    [ "$(jq -r .id "$bare/vmbareid.log.bark")" = "$minted" ] \
        || { echo "re-minted instead of recovering" >&2; return 1; }
    # And the data is still there.
    timberfs query "$bare/vmbareid.log" 2>/dev/null | grep -q 'no manifest here' || return 1

    # A derived artifact gets its OWN identity and records lineage —
    # carrying the source's would give two stores one id.
    timberfs export "$d/vmid.log" --into /tmp/vmid.timber >/dev/null 2>&1 || return 1
    local bid
    bid=$(tar -xOf /tmp/vmid.timber --wildcards '*.bark' | jq -r .id)
    [ "$bid" != "$bark_id" ] || { echo "bundle reused the source id" >&2; return 1; }
    tar -xOf /tmp/vmid.timber --wildcards '*.bark' | jq -e --arg s "$bark_id" '.derived_from == $s' >/dev/null
}

store_identity_is_printed_and_typeable() {
    # A listing that prints an id owes a way to type it back in, or it is
    # printing a token nothing accepts.
    local a=/var/log/timberfs/vmsel-a
    local id short
    id=$(timberfs info --json "$a/vmsel-a.log" 2>/dev/null | jq -r .id) || return 1
    [ "${#id}" = 36 ] || { echo "id was $id" >&2; return 1; }
    short=${id:0:8}

    # The table shows the leading 8 — a UUID's first group — and the
    # LABELS column beside it; --full-id spells the whole thing out.
    timberfs list /var/log/timberfs > /tmp/vmid.tab 2>/dev/null || return 1
    # Identity leads the table: it is what a store IS, not a contingent
    # fact like FOLLOWERS. A store that declares none reads as a dash.
    # NAME follows it — what the store is CALLED, which is a different
    # question and answered from the manifest when it declares one.
    head -1 /tmp/vmid.tab | grep -qE '^ID[[:space:]]+NAME[[:space:]]+FOREST' || { head -1 /tmp/vmid.tab >&2; return 1; }
    head -1 /tmp/vmid.tab | grep -q 'ID' || { head -1 /tmp/vmid.tab >&2; return 1; }
    head -1 /tmp/vmid.tab | grep -q 'LABELS' || { head -1 /tmp/vmid.tab >&2; return 1; }
    grep -E "^$short[[:space:]]+vmsel-a[[:space:]]" /tmp/vmid.tab >/dev/null || { grep vmsel-a /tmp/vmid.tab >&2; return 1; }
    timberfs list /var/log/timberfs --full-id 2>/dev/null | grep -qE "^$id[[:space:]]+vmsel-a[[:space:]]" || return 1

    # Typeable: whole id, and the printed prefix.
    timberfs info "$id" >/dev/null 2>&1 || return 1
    timberfs info "$short" >/dev/null 2>&1 || return 1
    # A handle is tried first, so a store can never be shadowed by another
    # store's id.
    timberfs info vmsel-a >/dev/null 2>&1 || return 1
    # Too short to be an id: reported as a missing store, not resolved
    # against every store that happens to start that way.
    timberfs info "${id:0:3}" >/dev/null 2>&1 && return 1

    # An id that prefixes several stores names none of them.
    local b=/var/log/timberfs/vmsel-b
    python3 - "$a/vmsel-a.log.bark" "$b/vmsel-b.log.bark" <<'PYID'
import json, sys
for i, p in enumerate(sys.argv[1:]):
    m = json.load(open(p))
    m["id"] = "deadbeef-aaaa-4bbb-8ccc-dddddddd%04d" % i
    json.dump(m, open(p, "w"), indent=1)
PYID
    timberfs info deadbeef > /tmp/vmid.err 2>&1 && return 1
    grep -q 'ambiguous' /tmp/vmid.err || { cat /tmp/vmid.err >&2; return 1; }
    timberfs info deadbeef-aaaa-4bbb-8ccc-dddddddd0000 >/dev/null 2>&1 || return 1
}

stream_end_says_whether_a_cap_stopped_it() {
    # `entries=2` is the same number whether that was everything or
    # whether --max stopped it, so a consumer reading the count alone
    # presents a truncated answer as a complete one. `status` closes
    # that, and has to be exact at the boundary or it is just noise.
    local d=/var/log/timberfs/vmcap
    rm -rf "$d"
    timberfs create --index "$d/vmcap.log" > /tmp/vmcap.err 2>&1 \
        || { echo "create: $(cat /tmp/vmcap.err)" >&2; return 1; }
    local n; n=$(date -u +%Y-%m-%dT%H:%M:%S)
    local i
    # A small chunk size so the cap can land on a chunk BOUNDARY as well
    # as mid-chunk — the boundary is the case that a "dropped an entry"
    # signal alone would miss, and it is covered by construction rather
    # than by asserting a chunk count this test does not otherwise need.
    for i in $(seq 1 20); do printf '%sZ INFO line %02d\n' "$n" "$i"; done \
        | timberfs append --into "$d/vmcap.log" --chunk-size 64 --quiet > /tmp/vmcap.err 2>&1 \
        || { echo "append: $(cat /tmp/vmcap.err)" >&2; return 1; }

    status_of() {
        timberfs query "$d/vmcap.log" --records "$@" 2>/dev/null \
            | tr '\0' '\n' | grep -a 'stream-end' \
            | tr '\037' '\n' | grep -a '^status=' | cut -d= -f2
    }
    local got
    # Short of the data, and one short of it: limited either way.
    for i in 3 19; do
        got=$(status_of --max $i)
        [ "$got" = limited ] || { echo "--max $i gave '$got', wanted limited" >&2; return 1; }
    done
    # EXACTLY the data, and more than it: exhausted. A naive
    # `emitted == max` check calls the first of these limited, which
    # would make the field noise.
    for i in 20 25; do
        got=$(status_of --max $i)
        [ "$got" = exhausted ] || { echo "--max $i gave '$got', wanted exhausted" >&2; return 1; }
    done
    got=$(status_of)
    [ "$got" = exhausted ] || { echo "no cap gave '$got'" >&2; return 1; }

    # ...and it names WHICH bound, so a client can decide whether to
    # widen or to paginate.
    timberfs query "$d/vmcap.log" --records --max 3 2>/dev/null | tr '\0' '\n' \
        | grep -aq 'limit=max.entries' || { echo "no limit= field" >&2; return 1; }

    # A person gets told too: a count alone reads as the whole answer.
    timberfs query "$d/vmcap.log" --max 3 2>&1 >/dev/null | grep -q 'more entries matched' \
        || { echo "no note when limited" >&2; return 1; }
    if timberfs query "$d/vmcap.log" --max 25 2>&1 >/dev/null | grep -q 'more entries matched'; then
        echo "noted a limit on a complete answer" >&2
        return 1
    fi
}

catalogue_fields_are_a_projection_of_list() {
    # What a query API's catalogue endpoint needs, and all of it from
    # `list --json`: identity to join on, provenance to select on, coverage
    # to route by, and what it has dropped.
    local d=/var/log/timberfs/vmcat
    rm -rf "$d"
    timberfs create --index --retain-size 5G --set host=apache01 --set service=apache \
        --set 'service.name=apache' "$d/vmcat.log" >/dev/null 2>&1 || return 1
    printf '2026-08-22T10:00:00Z INFO catalogued\n' \
        | timberfs append --into "$d/vmcat.log" --quiet 2>/dev/null || return 1

    timberfs list --json /var/log/timberfs > /tmp/cat.json 2>/dev/null || return 1
    jq -e '.[] | select(.handle == "vmcat")
           | (.id | type == "string" and length == 36)
           and .labels.host == "apache01"
           and .labels.service == "apache"
           and .labels["service.name"] == "apache"
           and .dropped_chunks == 0 and .dropped_bytes == 0
           and (.first_write_ms | type == "number")' /tmp/cat.json > /dev/null \
        || { jq -c '.[] | select(.handle == "vmcat")' /tmp/cat.json; return 1; }

    # Settings are NOT labels: selecting on `retain_size` or `index` would
    # be selecting on an operational choice.
    jq -e '.[] | select(.handle == "vmcat") | .labels
           | has("retain_size") == false and has("index") == false
             and has("id") == false and has("wal") == false' /tmp/cat.json > /dev/null \
        || { jq -c '.[] | select(.handle=="vmcat") | .labels' /tmp/cat.json; return 1; }

    # A store with no manifest declares nothing, and says so rather than
    # inventing an identity.
    rm -f /var/log/timberfs/vmbare/vmbare.log.*
    mkdir -p /var/log/timberfs/vmbare
    printf 'no manifest here\n' | timberfs append --into /var/log/timberfs/vmbare/vmbare.log --quiet 2>/dev/null
    timberfs list --json /var/log/timberfs \
        | jq -e '.[] | select(.handle == "vmbare") | .id == null and (.labels | length == 0)' \
            > /dev/null || return 1
    # ...and the human table still gives it an ID cell, as a dash: the
    # column is structural, so a store with no identity is visible as one
    # rather than absent from a column that quietly disappeared.
    timberfs list /var/log/timberfs | grep -qE '^-[[:space:]]+vmbare[[:space:]]'
}

forward_intake_restart_survives() {
    systemctl restart timberfs-forward.service
    sleep 1
    local before after
    before=$(timberfs query "$FWD_STORE" 2>/dev/null | wc -l)
    forward_intake_client vmtestchunk2 > /tmp/fwd2.out 2>&1
    grep -q ACK_OK /tmp/fwd2.out || return 1
    # Acked = durable in the sap; queryable follows at the next chunk
    # flush — poll for it.
    local i after
    for i in $(seq 1 20); do
        after=$(timberfs query "$FWD_STORE" 2>/dev/null | wc -l)
        [ "$after" -gt "$before" ] && break
        sleep 0.5
    done
    [ "$after" -gt "$before" ] \
        && systemctl --quiet is-active timberfs-forward.service
}

run_test "forward-intake: enable socket, unit activates" forward_intake_setup
run_test "forward-intake: unknown tag refused until operator creates it" forward_intake_unknown_tag_refused_until_created
run_test "forward-intake: --auto-create drop-in (Docker-host mode)" forward_intake_enable_auto_create
run_test "forward-intake: Message/Forward/PackedForward land, chunk acked" forward_intake_receives_and_acks
run_test "forward-intake: store path is the tag, resolvable as a handle" forward_intake_store_path_is_the_tag
run_test "forward-intake: split-line partial reassembles to one entry" forward_intake_partial_reassembles
run_test "forward-intake: entries carry the sender's own event time" forward_intake_event_times_landed
run_test "forward-intake: first record's container_id seeds the manifest" forward_intake_container_id_seeded
run_test "forward-intake: a declaring sender seeds host; peer is recorded either way" forward_intake_seeds_host_and_peer
run_test "forward-intake: service restart is a sender reconnect, no data lost" forward_intake_restart_survives
run_test "catalogue: list --json carries identity, provenance and coverage" catalogue_fields_are_a_projection_of_list
run_test "records: stream-end says whether a cap stopped it, exactly" stream_end_says_whether_a_cap_stopped_it
run_test "selection: list --select matches on labels, not on the name" selection_is_by_label_not_by_name
run_test "query document: selects by label, and can answer with stores" a_query_document_selects_stores_and_can_answer_with_them
run_test "query document: match and bounds name their granularity" a_match_selects_what_it_says_it_selects
run_test "bounded read: names the bound, counts what it read, invents no entry" a_bounded_read_says_what_stopped_it_and_invents_nothing
run_test "framed answers read stores one after another; text still interleaves" a_framed_answer_reads_stores_one_after_another
run_test "deadline: bounds the wait, names itself, and says where it stopped" a_deadline_bounds_the_wait_and_the_answer_says_where_it_stopped
run_test "dump-json: no store needed, so it can say what a time means" dump_json_needs_no_store_so_it_can_answer_what_a_time_means
run_test "selector: an unknown operator is refused, not truncated" an_unknown_selector_operator_is_refused_not_quietly_truncated
run_test "paging: a store that goes quiet keeps its place" a_quiet_store_keeps_its_place_across_pages
run_test "paging: a cursor walks every entry once, even at one timestamp" paging_walks_a_result_set_a_page_at_a_time
run_test "query examples: shipped, indexed, and every one of them runs" the_query_examples_ship_and_run
run_test "forest: declared by a command, refuses overlap, remove keeps data" a_forest_is_declared_by_a_command_not_by_hand_editing
run_test "forest: an intake writes into a forest by name; --into-dir warns" an_intake_writes_into_a_forest_by_name
run_test "naming: a store is called what it declares, and all of it is matchable" a_store_is_called_what_it_declares
run_test "incus-intake: unit installed; options checked before anything opens" incus_intake_is_installed_and_validates_its_options
run_test "selection: the id list prints is the id info accepts" store_identity_is_printed_and_typeable
run_test "identity: the backing pair carries the store id, and retention keeps it" store_identity_lives_in_the_backing_pair
run_test "identity: report exits non-zero when broken; --mint and --keep repair it" identity_reports_and_repairs_the_three_broken_states

# The OTLP/HTTP intake (timberfs-otlp.socket/.service) and its mirror,
# the timber-otlp shipper. A python3 http.client driver posts the real
# wire format — no requests/curl assumed — and the headline case runs BOTH
# directions: a store shipped out and received back must be byte for byte
# the same. A fixed event time (2026-06-20 09:00:00 UTC) keeps the
# write-window assertions exact instead of racing the wall clock.
# One store per stream, each in its own directory named after the route
# value: $OTLP_ROOT/<service>/<service>.log, the layout every intake writes.
OTLP_ROOT=/var/log/timberfs
otlp_store() { echo "$OTLP_ROOT/$1/$1.log"; }
OTLP_NANOS=$(( $(date -u -d "2026-06-20 09:00:00" +%s) * 1000000000 ))
OTLP_TRACE=4bf92f3577b34da6a3ce929d0e0e4736

# ARGS: METHOD PATH CONTENT_TYPE BODY [HEADER:VALUE]
# Prints "<status>|<Retry-After or ->" on line 1, then the response body.
otlp_request() {
    command -v python3 >/dev/null 2>&1 || { echo "NO_PYTHON3"; return 1; }
    python3 - "$1" "$2" "$3" "$4" "${5:-}" << 'PYEOF'
import sys, http.client
method, path, ctype, body, extra = sys.argv[1:6]
conn = http.client.HTTPConnection("127.0.0.1", 4318, timeout=15)
headers = {"Content-Type": ctype} if ctype else {}
if extra:
    k, _, v = extra.partition(":")
    headers[k.strip()] = v.strip()
conn.request(method, path, body.encode(), headers)
r = conn.getresponse()
print("%d|%s" % (r.status, r.getheader("Retry-After") or "-"))
print(r.read().decode(errors="replace"))
PYEOF
}

# ARGS: SERVICE — one OTel-native record: no timestamp in the body (an SDK
# puts it in the field, not the text), a severity, a trace id and an
# attribute. Exactly what the intake has to render into a log line itself.
otlp_body() {
    printf '%s' '{"resourceLogs":[{"resource":{"attributes":[' \
        '{"key":"service.name","value":{"stringValue":"'"$1"'"}},' \
        '{"key":"host.name","value":{"stringValue":"vmhost"}},' \
        '{"key":"deployment.environment","value":{"stringValue":"vmtest"}}' \
        ']},"scopeLogs":[{"scope":{"name":"vm"},"logRecords":[' \
        '{"timeUnixNano":"'"$OTLP_NANOS"'","severityText":"ERROR",' \
        '"body":{"stringValue":"otlp native record"},' \
        '"traceId":"'"$OTLP_TRACE"'",' \
        '"attributes":[{"key":"http.status_code","value":{"intValue":"500"}}]}' \
        ']}]}]}'
}

# As otlp_body, but with an explicit event time ($2, nanos) and body ($3).
otlp_body_at() {
    printf '%s' '{"resourceLogs":[{"resource":{"attributes":[' \
        '{"key":"service.name","value":{"stringValue":"'"$1"'"}}' \
        ']},"scopeLogs":[{"scope":{"name":"vm"},"logRecords":[' \
        '{"timeUnixNano":"'"$2"'","severityText":"INFO",' \
        '"body":{"stringValue":"'"$3"'"}}' \
        ']}]}]}'
}

# Poll until a store holds $2, or fail. Visibility follows the chunk flush.
otlp_wait_for() {
    local store=$1 needle=$2 i
    for i in $(seq 1 20); do
        timberfs query "$store" 2>/dev/null | grep -q "$needle" && return 0
        sleep 0.5
    done
    timberfs query "$store" 2>/dev/null >&2
    return 1
}

otlp_intake_setup() {
    systemd-tmpfiles --create
    systemctl enable --now timberfs-otlp.socket
}

otlp_intake_undeclared_refused_until_created() {
    # The shipped unit has no --auto-create: an undeclared stream is
    # answered 503 + Retry-After, leaves no store and no lock litter, and
    # the sender keeps the records (which is where they are still safe).
    otlp_request POST /v1/logs application/json "$(otlp_body vmotlprefused)" > /tmp/otlpref.out 2>&1
    head -1 /tmp/otlpref.out | grep -q '^503|5$' || { cat /tmp/otlpref.out; return 1; }
    [ ! -e "$(otlp_store vmotlprefused).rings" ] || return 1
    [ ! -e "$(otlp_store vmotlprefused).lock" ] || return 1
    # Not even the store's directory: a refused stream leaves nothing.
    [ ! -e "$OTLP_ROOT/vmotlprefused" ] || return 1
    # The operator provisions it; the sender's retry then lands with a 200.
    timberfs create --wal "$(otlp_store vmotlprefused)" || return 1
    otlp_request POST /v1/logs application/json "$(otlp_body vmotlprefused)" > /tmp/otlpref2.out 2>&1
    head -1 /tmp/otlpref2.out | grep -q '^200|' || { cat /tmp/otlpref2.out; return 1; }
    otlp_wait_for "$(otlp_store vmotlprefused)" "otlp native record"
}

otlp_intake_refuses_the_right_things() {
    # What is NOT an OTLP/HTTP encoding is named, not guessed at. (The two
    # a stock collector sends — protobuf and gzip — are accepted; the
    # cases below cover the rest.)
    otlp_request POST /v1/logs text/plain x > /tmp/o1.out 2>&1
    head -1 /tmp/o1.out | grep -q '^415|' || { cat /tmp/o1.out; return 1; }
    grep -q 'application/x-protobuf' /tmp/o1.out || { cat /tmp/o1.out; return 1; }

    otlp_request POST /v1/logs application/json '{}' 'Content-Encoding: br' > /tmp/o2.out 2>&1
    head -1 /tmp/o2.out | grep -q '^415|' || { cat /tmp/o2.out; return 1; }
    grep -q 'gzip or none' /tmp/o2.out || { cat /tmp/o2.out; return 1; }

    # Other signals and methods are named, not merely refused.
    otlp_request POST /v1/traces application/json '{}' > /tmp/o3.out 2>&1
    head -1 /tmp/o3.out | grep -q '^404|' || { cat /tmp/o3.out; return 1; }
    grep -q 'log store' /tmp/o3.out || { cat /tmp/o3.out; return 1; }

    otlp_request GET /v1/logs '' '' > /tmp/o4.out 2>&1
    head -1 /tmp/o4.out | grep -q '^405|' || { cat /tmp/o4.out; return 1; }

    otlp_request POST /v1/logs application/json 'not json' > /tmp/o5.out 2>&1
    head -1 /tmp/o5.out | grep -q '^400|' || { cat /tmp/o5.out; return 1; }
}

otlp_intake_enable_auto_create() {
    # The collector-host mode for the rest of the flow: streams appear as
    # services are deployed, and the declared index is maintained live.
    mkdir -p /etc/systemd/system/timberfs-otlp.service.d
    cat > /etc/systemd/system/timberfs-otlp.service.d/auto-create.conf <<'EOF'
[Service]
ExecStart=
ExecStart=/usr/bin/timberfs otlp-intake --into-dir /var/log/timberfs --exit-on-upgrade --auto-create --index
EOF
    systemctl daemon-reload
    systemctl restart timberfs-otlp.service
    sleep 0.5
    systemctl --quiet is-active timberfs-otlp.service
}

otlp_intake_renders_a_native_record() {
    otlp_request POST /v1/logs application/json "$(otlp_body vmotlp)" > /tmp/o6.out 2>&1
    head -1 /tmp/o6.out | grep -q '^200|' || { cat /tmp/o6.out; return 1; }
    otlp_wait_for "$(otlp_store vmotlp)" "otlp native record" || return 1
    # An unstamped body is prefixed with the record's own time and level,
    # so the store stays time-indexable; attributes and the trace id trail
    # as k=v where the token index can find them.
    timberfs query "$(otlp_store vmotlp)" > /tmp/o7.out 2>&1
    grep -q "ERROR otlp native record http.status_code=500 trace_id=$OTLP_TRACE" /tmp/o7.out \
        || { cat /tmp/o7.out; return 1; }
    # The prefix is a real timestamp: the entry lands in the SENDER'S
    # event time, not the wall clock the receiver happened to run at.
    # `info` renders in local time, so the expected string is derived the
    # same way rather than hardcoded to the VM's timezone.
    local covers
    covers=$(date -d "@$((OTLP_NANOS / 1000000000))" '+%Y-%m-%d %H:%M:%S.000')
    timberfs info "$(otlp_store vmotlp)" > /tmp/o8.out 2>&1
    grep -q "covers    $covers" /tmp/o8.out || { cat /tmp/o8.out; return 1; }
}

otlp_intake_flush_age_survives_a_senders_clock_skew() {
    # A sender whose clock runs ahead stamps records in the RECEIVER'S
    # future, and an intake stamps chunks with the sender's window. The
    # chunk flush age is LOCAL elapsed time, so such a record still becomes
    # a chunk -- and therefore visible to a windowed read, which is
    # chunk-granular.
    local ahead
    ahead=$(( ($(date +%s) + 86400) * 1000000000 ))
    otlp_request POST /v1/logs application/json \
        "$(otlp_body_at vmskew "$ahead" 'sender clock a day ahead')" > /tmp/oskew.out 2>&1
    head -1 /tmp/oskew.out | grep -q '^200|' || { cat /tmp/oskew.out; return 1; }
    otlp_wait_for "$(otlp_store vmskew)" "sender clock a day ahead" || return 1
    # And the chunk still carries the sender's window, not the local clock.
    timberfs info "$(otlp_store vmskew)" > /tmp/oskew2.out 2>&1
    grep -q "covers    $(date -d "@$((ahead / 1000000000))" '+%Y-%m-%d')" /tmp/oskew2.out \
        || { cat /tmp/oskew2.out; return 1; }
}

otlp_intake_store_path_is_the_route_value() {
    # One directory per stream, named after the route value (service.name):
    # the store answers to that name as a handle, and no directory names the
    # protocol that carried it.
    [ -f "$(otlp_store vmotlp).rings" ] || return 1
    [ ! -e "$OTLP_ROOT/otlp" ] || return 1
    timberfs query vmotlp 2>/dev/null | grep -q "otlp native record"
}

otlp_intake_trace_id_is_indexed() {
    # The declared .grain is maintained by the receiver's own tick, so a
    # trace id becomes a chunk-skipping lookup — a trace's log lines with
    # no trace backend involved.
    grep -q '"index": true' "$(otlp_store vmotlp).bark" || return 1
    local i
    for i in $(seq 1 20); do
        [ -s "$(otlp_store vmotlp).grain" ] && break
        sleep 0.5
    done
    [ -s "$(otlp_store vmotlp).grain" ] || return 1
    timberfs query --has "$OTLP_TRACE" "$(otlp_store vmotlp)" | grep -q "otlp native record" || return 1
    # A token that is in no chunk skips everything rather than scanning.
    [ -z "$(timberfs query --has deadbeefcafe0000deadbeefcafe0000 "$(otlp_store vmotlp)" 2>/dev/null)" ]
}

otlp_intake_seeds_the_resource() {
    # Resource attributes describe the stream, not any one line, so they
    # are seeded into the manifest — under their OTLP names and under the
    # names the read path and timber-otlp already look for.
    local bark="$(otlp_store vmotlp).bark"
    grep -q '"service.name": "vmotlp"' "$bark" || { cat "$bark"; return 1; }
    grep -q '"service": "vmotlp"' "$bark" || { cat "$bark"; return 1; }
    grep -q '"host": "vmhost"' "$bark" || { cat "$bark"; return 1; }
    grep -q '"deployment.environment": "vmtest"' "$bark" || { cat "$bark"; return 1; }
    # The 200 means durable, which is what the wal delivers.
    grep -q '"wal": true' "$bark" || { cat "$bark"; return 1; }
    [ -s "$(otlp_store vmotlp).sap" ]
}

otlp_intake_restart_survives() {
    systemctl restart timberfs-otlp.service
    sleep 1
    otlp_request POST /v1/logs application/json "$(otlp_body vmotlp)" > /tmp/o9.out 2>&1
    head -1 /tmp/o9.out | grep -q '^200|' || { cat /tmp/o9.out; return 1; }
    systemctl --quiet is-active timberfs-otlp.service
}

# The headline: ship a store OUT over OTLP and receive it back IN — over
# protobuf, the default encoding and the one every real sender uses. Each
# direction is the other's oracle, so a drift in time, framing or body
# text on either side shows up here as a diff.
otlp_roundtrip_is_byte_for_byte() {
    cat > /tmp/rt.src <<'EOF'
2026-06-20T09:00:01.500Z INFO roundtrip starting up
2026-06-20T09:00:02.250Z ERROR roundtrip checkout failed for cart 9912
	at com.example.Cart.check(Cart.java:44)
	at com.example.Main.main(Main.java:9)
2026-06-20T09:00:03.000Z WARN roundtrip retrying
EOF
    rm -f "$PIPE_BACKING"/rt.log.* "$OTLP_ROOT"/rt/rt.log.*
    timberfs import /tmp/rt.src --into "$PIPE_BACKING/rt.log" --quiet --utc || return 1
    timber-otlp --quiet --endpoint http://127.0.0.1:4318 "$PIPE_BACKING/rt.log" > /tmp/rt.ship 2>&1 \
        || { cat /tmp/rt.ship; return 1; }
    otlp_wait_for "$(otlp_store rt)" "roundtrip retrying" || return 1
    timberfs query "$PIPE_BACKING/rt.log" > /tmp/rt.a 2>/dev/null
    timberfs query "$(otlp_store rt)" > /tmp/rt.b 2>/dev/null
    # Byte for byte, multiline entry included — the stack trace must come
    # back as ONE entry, not three records.
    diff -u /tmp/rt.a /tmp/rt.b || return 1
    [ "$(timberfs query -0 "$(otlp_store rt)" | tr -cd '\0' | wc -c)" = 3 ]
}

timber_otlp_dry_run_shape() {
    # --dry-run prints exactly what would be posted, so the mapping is
    # inspectable without a receiver: one LogRecord per entry, both time
    # axes present, severity read from the line.
    timber-otlp --quiet --dry-run "$PIPE_BACKING/rt.log" > /tmp/rt.json 2>/dev/null || return 1
    local recs
    recs=$(jq '.resourceLogs[0].scopeLogs[0].logRecords | length' /tmp/rt.json)
    [ "$recs" = 3 ] || { cat /tmp/rt.json; return 1; }
    jq -e '.resourceLogs[0].scopeLogs[0].logRecords[1] | .severityText == "ERROR"
           and .severityNumber == 17
           and (.timeUnixNano | tonumber) > 0
           and (.observedTimeUnixNano | tonumber) > 0
           and (.body.stringValue | contains("Cart.java"))' /tmp/rt.json > /dev/null \
        || { cat /tmp/rt.json; return 1; }
    jq -e '.resourceLogs[0].resource.attributes[]
           | select(.key == "service.name") | .value.stringValue == "rt"' /tmp/rt.json > /dev/null
}

timber_otlp_cursor_resumes_without_duplicates() {
    # The durable shipper against the real receiver: kill it mid-stream,
    # append more, restart it. The cursor is on the write axis and is
    # written only after the receiver accepts, so a restart re-delivers at
    # worst — never skips, never re-sends what was already acknowledged.
    rm -f "$PIPE_BACKING"/cur.log.* "$OTLP_ROOT"/cur/cur.log.* /tmp/cur.cursor
    mkfifo /tmp/cur.fifo
    timberfs append --into "$PIPE_BACKING/cur.log" --flush-age 1 < /tmp/cur.fifo &
    local ap=$!
    exec 8>/tmp/cur.fifo
    printf '2026-06-20T09:10:01Z INFO cur-one\n2026-06-20T09:10:02Z INFO cur-two\n' >&8
    sleep 2
    timber-otlp --quiet --follow --cursor /tmp/cur.cursor --start begin \
        --endpoint http://127.0.0.1:4318 "$PIPE_BACKING/cur.log" > /tmp/cur.ship1 2>&1 &
    local sp=$!
    sleep 4
    printf '2026-06-20T09:10:03Z INFO cur-three\n2026-06-20T09:10:04Z INFO cur-four\n' >&8
    # Wait for the cursor to record them rather than sleeping a fixed time.
    # The last entry is closed by the follower's idle flush, ~10s after the
    # chunk holding it, so end to end this needs ~13s on an idle laptop —
    # a fixed wait is a race the VM loses whenever it is busy.
    local i
    for i in $(seq 1 40); do
        [ "$(jq -r .delivered /tmp/cur.cursor 2>/dev/null)" = 4 ] && break
        sleep 1
    done
    kill "$sp" 2>/dev/null; wait "$sp" 2>/dev/null
    local delivered
    delivered=$(jq -r .delivered /tmp/cur.cursor 2>/dev/null)
    [ "$delivered" = 4 ] || { cat /tmp/cur.cursor; return 1; }

    # More data while nothing is shipping, then a restart from the cursor.
    printf '2026-06-20T09:10:05Z INFO cur-five\n2026-06-20T09:10:06Z INFO cur-six\n' >&8
    sleep 2
    timber-otlp --quiet --follow --cursor /tmp/cur.cursor \
        --endpoint http://127.0.0.1:4318 "$PIPE_BACKING/cur.log" > /tmp/cur.ship2 2>&1 &
    sp=$!
    for i in $(seq 1 40); do
        [ "$(jq -r .delivered /tmp/cur.cursor 2>/dev/null)" = 6 ] && break
        sleep 1
    done
    kill "$sp" 2>/dev/null; wait "$sp" 2>/dev/null
    exec 8>&-; kill "$ap" 2>/dev/null; wait "$ap" 2>/dev/null
    rm -f /tmp/cur.fifo

    # The position is a chunk NUMBER, not a timestamp: the cursor names one
    # and `wl` is only there to orient a human.
    jq -e '.seq != null and (.seq | type) == "number"' /tmp/cur.cursor >/dev/null \
        || { cat /tmp/cur.cursor; return 1; }

    otlp_wait_for "$(otlp_store cur)" "cur-six" || return 1
    timberfs query "$(otlp_store cur)" > /tmp/cur.out 2>/dev/null
    # Every entry exactly once: six lines, none repeated.
    [ "$(wc -l < /tmp/cur.out)" = 6 ] || { cat /tmp/cur.out; return 1; }
    [ "$(sort /tmp/cur.out | uniq -d | wc -l)" = 0 ] || { cat /tmp/cur.out; return 1; }
    grep -q cur-one /tmp/cur.out && grep -q cur-six /tmp/cur.out || return 1
    [ "$(jq -r .delivered /tmp/cur.cursor)" = 6 ]
}

cursor_converts_from_a_write_time_position() {
    # A cursor written before chunk numbers existed holds a write time. It
    # is resolved to a chunk on first use, that chunk is re-read from its
    # start (bounded re-delivery, never a skip), and the converted file is
    # persisted only by the first save AFTER a successful send.
    local store cur
    store="$PIPE_BACKING/conv.log"
    cur=/tmp/conv.cursor
    rm -f "$store".* "$OTLP_ROOT"/conv/conv.log.* "$cur"
    local i
    for i in 1 2 3; do
        printf '2026-06-21T09:1%s:00Z conv line %s\n' "$i" "$i" \
            | timberfs append --into "$store" --quiet || return 1
        sleep 1.1
    done

    # Let the shipper mint a real cursor first — the store anchor is its
    # `.bark` id or a CANONICAL path, and hand-writing it would only test
    # whether this test can guess that. Then downgrade the file to the
    # pre-numbering shape: a write time, no `seq`.
    timeout 20 timber-otlp --follow --cursor "$cur" --start begin \
        --endpoint http://127.0.0.1:4318 "$store" >/dev/null 2>&1
    jq -e '.seq != null' "$cur" >/dev/null || { cat "$cur" 2>&1; return 1; }
    python3 - "$store" "$cur" << 'PYEOF' || return 1
import json, struct, sys
store, cur = sys.argv[1], sys.argv[2]
c = json.load(open(cur))
raw = open(store + ".rings", "rb").read()
# the middle chunk's window, i.e. a position inside chunk 1
rec = struct.unpack("<7Q", raw[64 + 56:64 + 112])
del c["seq"]
c["wf"], c["wl"], c["n"] = rec[4], rec[5], 2
json.dump(c, open(cur, "w"))
PYEOF

    timeout 20 timber-otlp --follow --cursor "$cur" \
        --endpoint http://127.0.0.1:4318 "$store" > /tmp/conv.ship 2>/tmp/conv.err
    grep -q 'converting a pre-numbering cursor' /tmp/conv.err \
        || { cat /tmp/conv.err >&2; return 1; }
    grep -q 'resolves to chunk 1' /tmp/conv.err || { cat /tmp/conv.err >&2; return 1; }
    # Converted in place. NOT pinned to the resolved chunk: continuing
    # past it is the whole point, and how far it gets inside the timeout is
    # a race — only that it became a number and never went backwards.
    jq -e '(.seq | type) == "number" and .seq >= 1' "$cur" >/dev/null \
        || { cat "$cur"; return 1; }
    # Nothing was skipped: the entry from the chunk it resolved into arrives.
    otlp_wait_for "$(otlp_store conv)" "conv line 2" || return 1
    rm -f "$cur" /tmp/conv.ship /tmp/conv.err
    return 0
}

records_carry_the_chunk_number() {
    # The records stream labels each entry with the chunk it came from, and
    # --from-chunk resumes at one exactly. The ABSENCE of the field is the
    # live-edge signal, so a consumer can tell "no resumable position yet"
    # from "chunk 0".
    local d=/var/log/timberfs/recchunk store
    d=/var/log/timberfs/recchunk
    store="$d/recchunk.log"
    rm -rf "$d"
    local i
    for i in 0 1 2; do
        printf '2026-08-21T10:0%s:00Z record chunk %s\n' "$i" "$i" \
            | timberfs append --into "$store" --quiet || return 1
        sleep 1.1
    done

    # One entry per chunk, each carrying its own number.
    timberfs query --records "$store" 2>/dev/null > /tmp/rc.bin || return 1
    python3 - /tmp/rc.bin << 'PYEOF' || return 1
import sys
raw = open(sys.argv[1], "rb").read()
got = []
for rec in raw.split(b"\x1e"):
    if not rec.startswith(b"entry"):
        continue
    head = rec.split(b"\x00", 1)[0].split(b"\x1f")
    kv = dict(f.split(b"=", 1) for f in head[1:] if b"=" in f)
    got.append(kv.get(b"chunk", b"-").decode())
assert got == ["0", "1", "2"], got
PYEOF

    # --from-chunk positions exactly. The framed follow path holds the last
    # entry open until a following stamped line closes it (a pre-existing
    # one-entry lag), so chunk 1 is what a run started at 1 emits here.
    timeout 4 timberfs query --records --follow --from-chunk 1 "$store" \
        > /tmp/rc1.bin 2>/dev/null
    python3 - /tmp/rc1.bin << 'PYEOF' || return 1
import sys
raw = open(sys.argv[1], "rb").read()
got = []
for rec in raw.split(b"\x1e"):
    if not rec.startswith(b"entry"):
        continue
    head = rec.split(b"\x00", 1)[0].split(b"\x1f")
    kv = dict(f.split(b"=", 1) for f in head[1:] if b"=" in f)
    got.append(kv.get(b"chunk", b"-").decode())
assert got and all(int(c) >= 1 for c in got), got
PYEOF

    # Requires --follow: a windowed read selects by the timestamps the lines
    # carry, which is a different axis.
    ! timberfs query --records --from-chunk 1 "$store" >/dev/null 2>&1 || return 1
    rm -rf "$d" /tmp/rc.bin /tmp/rc1.bin
    return 0
}

chunk_numbers_and_v1_migration() {
    # Chunk numbers: dense from 0, preserved verbatim by a head-drop (they
    # are what a cursor holds, so sliding them down would silently re-point
    # it), and synthesized when reading the pre-numbering layout, which a
    # writer migrates in place on open.
    local d=/var/log/timberfs/chunknum store nums cut
    d=/var/log/timberfs/chunknum
    store="$d/chunknum.log"
    rm -rf "$d"
    local i
    for i in 1 2 3 4; do
        printf 'chunknum line %s\n' "$i" | timberfs append --into "$store" --quiet || return 1
        sleep 1.1
    done
    nums=$(timberfs index "$store" | awk '$1 ~ /^[0-9]+$/ {printf "%s ", $1}')
    [ "$nums" = "0 1 2 3 " ] || { echo "dense-from-0 failed: $nums" >&2; return 1; }

    # Drop the two oldest chunks. The survivors keep 2 and 3.
    cut=$(timberfs index "$store" | awk 'NR==4 {print $6" "substr($7,1,8)}')
    timberfs rotate "$store" --cutoff "$cut" --delete --quiet >/dev/null 2>&1 || return 1
    nums=$(timberfs index "$store" | awk '$1 ~ /^[0-9]+$/ {printf "%s ", $1}')
    [ "$nums" = "2 3 " ] || { echo "numbers slid after a head-drop: $nums" >&2; return 1; }
    # A new chunk continues past the survivors rather than reusing a number.
    printf 'chunknum after drop\n' | timberfs append --into "$store" --quiet || return 1
    nums=$(timberfs index "$store" | awk '$1 ~ /^[0-9]+$/ {printf "%s ", $1}')
    [ "$nums" = "2 3 4 " ] || { echo "numbering did not continue: $nums" >&2; return 1; }

    # Downgrade the index to the pre-numbering layout (8-byte header,
    # 48-byte records) and confirm a reader copes and a writer migrates.
    python3 - "$store.rings" << 'PYEOF' || return 1
import sys
p = sys.argv[1]
raw = open(p, "rb").read()
assert raw[:8] == b"RING0002", raw[:8]
n = (len(raw) - 64) // 56
open(p, "wb").write(b"RING0001" + b"".join(raw[64 + i * 56:64 + i * 56 + 48] for i in range(n)))
PYEOF
    # A reader needs no migration: it numbers the oldest survivor 0.
    nums=$(timberfs index "$store" | awk '$1 ~ /^[0-9]+$/ {printf "%s ", $1}')
    [ "$nums" = "0 1 2 " ] || { echo "v1 read failed: $nums" >&2; return 1; }
    timberfs query "$store" 2>/dev/null | grep -q 'chunknum after drop' || return 1
    # A writer migrates in place, says so, and keeps appending.
    printf 'chunknum after migration\n' | timberfs append --into "$store" 2>/tmp/chunknum.err || return 1
    grep -q 'rings migrated to RING0002' /tmp/chunknum.err \
        || { cat /tmp/chunknum.err >&2; return 1; }
    head -c 8 "$store.rings" | grep -q 'RING0002' || return 1
    nums=$(timberfs index "$store" | awk '$1 ~ /^[0-9]+$/ {printf "%s ", $1}')
    [ "$nums" = "0 1 2 3 " ] || { echo "post-migration numbering: $nums" >&2; return 1; }
    rm -rf "$d" /tmp/chunknum.err
    return 0
}

consumer_view_and_gap() {
    # The consumer view, both halves. A store declares WHERE its consumers
    # keep their cursors and `list`/`info` then say who is reading it and
    # how far behind; a cursor whose position retention has already dropped
    # is a GAP, reported by the view AND by the shipper on resume. Nothing
    # writes into the cursor directory from the store's side, so this test
    # hand-writes the cursors exactly as a shipper would.
    local d=/var/log/timberfs/consumed cdir=/tmp/consumer-cursors
    local store="$d/consumed.log"
    rm -rf "$d" "$cdir"; mkdir -p "$cdir"
    # One append run per chunk, spaced so the write windows differ.
    local i
    for i in 1 2 3; do
        printf '2026-08-01T09:00:0%s INFO consumed line %s\n' "$i" "$i" \
            | timberfs append --into "$store" --quiet || return 1
        sleep 1.1
    done
    timberfs set "$store" "cursors=$cdir" >/dev/null || return 1

    # Two consumers of this store, plus a neighbour's cursor and a
    # non-cursor file in the same shared directory.
    python3 - "$store" "$cdir" << 'PYEOF' || return 1
import json, struct, sys
store, cdir = sys.argv[1], sys.argv[2]
sid = json.load(open(store + ".bark"))["id"]
raw = open(store + ".rings", "rb").read()
recs = [struct.unpack("<7Q", raw[64 + i * 56:120 + i * 56]) for i in range((len(raw) - 64) // 56)]
assert len(recs) >= 3, recs
def cursor(name, seq, wl, delivered):
    json.dump({"consumer": name, "store": sid, "path": store,
               "seq": seq, "n": 1, "wl": wl, "delivered": delivered},
              open("%s/%s.cursor" % (cdir, name), "w"))
# The position is a chunk NUMBER; wl is informational only.
cursor("edge", recs[-1][6], recs[-1][5], 100)   # in the newest chunk: keeping up
cursor("lagging", recs[0][6], recs[0][5], 10)   # still in the oldest: behind
json.dump({"consumer": "other", "store": "not-this-store", "seq": 1, "wl": 1},
          open(cdir + "/other.cursor", "w"))
open(cdir + "/README", "w").write("not a cursor\n")
PYEOF

    timberfs info "$store" > /tmp/consumers.info 2>&1 || return 1
    grep -q "consumers 2 in $cdir/" /tmp/consumers.info || { cat /tmp/consumers.info; return 1; }
    # A consumer keeping up with a live store sits INSIDE the newest chunk.
    grep -qE '^ +edge +at the live edge' /tmp/consumers.info || { cat /tmp/consumers.info; return 1; }
    grep -qE '^ +lagging +.*unread in 3 chunk\(s\)' /tmp/consumers.info \
        || { cat /tmp/consumers.info; return 1; }
    # The furthest behind leads, and is what the store is held by.
    grep -q "held by lagging" /tmp/consumers.info || { cat /tmp/consumers.info; return 1; }
    grep -q "not readable as cursors" /tmp/consumers.info || { cat /tmp/consumers.info; return 1; }
    # The key shipped in 0.18.0, so it is honoured -- and reported as
    # superseded wherever it is found, rather than silently removed.
    grep -q "SUPERSEDED by the follower registry" /tmp/consumers.info \
        || { cat /tmp/consumers.info; return 1; }
    timberfs info --json "$store" | jq -e '.cursors_superseded == true' >/dev/null || return 1

    timberfs info --json "$store" > /tmp/consumers.json || return 1
    jq -e '.consumers | length == 2' /tmp/consumers.json >/dev/null || return 1
    jq -e '.consumers[0].consumer == "lagging"' /tmp/consumers.json >/dev/null || return 1
    jq -e '.consumers[0].behind_bytes > .consumers[1].behind_bytes' /tmp/consumers.json >/dev/null \
        || return 1
    jq -e '.held_bytes == .consumers[0].behind_bytes' /tmp/consumers.json >/dev/null || return 1
    jq -e '.cursors_unreadable == 1' /tmp/consumers.json >/dev/null || return 1

    # `list` grows the column because a listed store declares a directory.
    timberfs list /var/log/timberfs > /tmp/consumers.list 2>/dev/null || return 1
    head -1 /tmp/consumers.list | grep -q 'FOLLOWERS' || { cat /tmp/consumers.list; return 1; }
    grep -E '[[:space:]]consumed[[:space:]]' /tmp/consumers.list | grep -q '2, ' \
        || { cat /tmp/consumers.list; return 1; }
    # A store that declares nothing keeps a dash in the shared column.
    timberfs list --json /var/log/timberfs > /tmp/consumers.ljson 2>/dev/null || return 1
    jq -e '[.[] | select(.handle != "consumed") | .consumers] | all(. == null)' \
        /tmp/consumers.ljson >/dev/null || return 1

    # A GAP now REQUIRES chunks to be gone, which is the point: the number
    # states it rather than a timestamp implying it. So drop the head, then
    # leave a cursor standing at a chunk that no longer exists.
    local cut
    cut=$(timberfs index "$store" | awk 'NR==3 {print $6" "substr($7,1,8)}')
    timberfs rotate "$store" --cutoff "$cut" --delete --quiet >/dev/null 2>&1 || return 1
    timberfs index "$store" | awk '$1 ~ /^[0-9]+$/ {print $1; exit}' | grep -qv '^0$' \
        || { echo "expected the surviving chunks to start above 0" >&2; return 1; }
    python3 - "$store" "$cdir" << 'PYEOF' || return 1
import json, struct, sys
store, cdir = sys.argv[1], sys.argv[2]
sid = json.load(open(store + ".bark"))["id"]
raw = open(store + ".rings", "rb").read()
oldest = struct.unpack("<7Q", raw[64:120])
json.dump({"consumer": "dropped", "store": sid, "path": store,
           "seq": 0, "n": 0, "wl": oldest[5], "delivered": 5000},
          open(cdir + "/dropped.cursor", "w"))
PYEOF
    timberfs info "$store" | grep -qE '^ +dropped +GAP' || { timberfs info "$store"; return 1; }
    timberfs list /var/log/timberfs | grep -E '[[:space:]]consumed[[:space:]]' | grep -q 'GAP' || return 1

    # And the shipper says so on resume rather than silently restarting
    # from whatever is now oldest.
    timeout 5 timber-otlp --follow --cursor "$cdir/dropped.cursor" --dry-run \
        "$store" > /dev/null 2> /tmp/consumers.gap
    grep -q 'GAP' /tmp/consumers.gap || { cat /tmp/consumers.gap; return 1; }
    # It keeps going: the loss is in the past, and a shipper that refuses
    # to start ships nothing.
    grep -q 'resuming at' /tmp/consumers.gap || { cat /tmp/consumers.gap; return 1; }

    rm -rf "$cdir" /tmp/consumers.info /tmp/consumers.json /tmp/consumers.list \
        /tmp/consumers.ljson /tmp/consumers.gap
    return 0
}

# ARGS: CONTENT_TYPE BODY_FILE [gzip] — post a body from a file, so binary
# survives the trip. Prints "<status>|<response body length>".
otlp_post_file() {
    command -v python3 >/dev/null 2>&1 || { echo "NO_PYTHON3"; return 1; }
    python3 - "$1" "$2" "${3:-}" << 'PYEOF'
import sys, gzip, http.client
ctype, path, comp = sys.argv[1], sys.argv[2], sys.argv[3]
body = open(path, "rb").read()
headers = {"Content-Type": ctype}
if comp == "gzip":
    body = gzip.compress(body)
    headers["Content-Encoding"] = "gzip"
conn = http.client.HTTPConnection("127.0.0.1", 4318, timeout=15)
conn.request("POST", "/v1/logs", body, headers)
r = conn.getresponse()
print("%d|%d" % (r.status, len(r.read())))
PYEOF
}

# ARGS: SERVICE OUTFILE — a binary ExportLogsServiceRequest packed here
# from varints and length-delimited fields, NOT by timberfs's own encoder:
# the intake has to decode what a foreign sender produces, and a shared
# bug between encoder and decoder would hide behind a self-roundtrip.
otlp_write_proto_body() {
    command -v python3 >/dev/null 2>&1 || { echo "NO_PYTHON3"; return 1; }
    python3 - "$1" "$2" "$OTLP_NANOS" "$OTLP_TRACE" << 'PYEOF'
import sys
service, out, nanos, trace = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]

def varint(n):
    b = b""
    while True:
        x = n & 0x7F
        n >>= 7
        if n:
            b += bytes([x | 0x80])
        else:
            return b + bytes([x])

def tag(f, w):
    return varint((f << 3) | w)

def ld(f, payload):
    return tag(f, 2) + varint(len(payload)) + payload

def st(f, text):
    return ld(f, text.encode())

def fixed64(f, v):
    return tag(f, 1) + v.to_bytes(8, "little")

def anyval(text):
    return st(1, text)

def keyvalue(k, v):
    return st(1, k) + ld(2, anyval(v))

record = (
    fixed64(1, nanos)
    + st(3, "ERROR")
    + ld(5, anyval("protobuf native record"))
    + ld(6, keyvalue("http.status_code", "500"))
    + ld(9, bytes.fromhex(trace))
    + varint((99 << 3) | 0) + varint(7)   # a field this decoder never heard of
)
resource = ld(1, keyvalue("service.name", service)) + ld(1, keyvalue("host.name", "vmhost"))
scope_logs = ld(1, st(1, "vm-sdk")) + ld(2, record)
resource_logs = ld(1, resource) + ld(2, scope_logs)
open(out, "wb").write(ld(1, resource_logs))
PYEOF
}

otlp_intake_accepts_a_foreign_protobuf_sender() {
    otlp_write_proto_body vmproto /tmp/vmproto.bin || return 1
    otlp_post_file application/x-protobuf /tmp/vmproto.bin > /tmp/op1.out 2>&1
    # Full success in protobuf is the EMPTY message: 200 with a zero-byte body.
    grep -q '^200|0$' /tmp/op1.out || { cat /tmp/op1.out; return 1; }
    otlp_wait_for "$(otlp_store vmproto)" "protobuf native record" || return 1
    timberfs query "$(otlp_store vmproto)" > /tmp/op2.out 2>&1
    grep -q "ERROR protobuf native record http.status_code=500 trace_id=$OTLP_TRACE" /tmp/op2.out \
        || { cat /tmp/op2.out; return 1; }
    grep -q '"service.name": "vmproto"' "$(otlp_store vmproto).bark" || return 1
    # The unknown field did not derail the decode, and the event time landed.
    local covers
    covers=$(date -d "@$((OTLP_NANOS / 1000000000))" '+%Y-%m-%d %H:%M:%S.000')
    timberfs info "$(otlp_store vmproto)" | grep -q "covers    $covers"
}

otlp_intake_inflates_gzip() {
    # What a stock collector sends: gzipped. Both encodings, since the
    # inflate happens before either decoder sees the body.
    otlp_body vmgzipjson > /tmp/vmgz.json
    otlp_post_file application/json /tmp/vmgz.json gzip > /tmp/og1.out 2>&1
    grep -q '^200|2$' /tmp/og1.out || { cat /tmp/og1.out; return 1; }
    otlp_wait_for "$(otlp_store vmgzipjson)" "otlp native record" || return 1

    otlp_write_proto_body vmgzipproto /tmp/vmgz.bin || return 1
    otlp_post_file application/x-protobuf /tmp/vmgz.bin gzip > /tmp/og2.out 2>&1
    grep -q '^200|0$' /tmp/og2.out || { cat /tmp/og2.out; return 1; }
    otlp_wait_for "$(otlp_store vmgzipproto)" "protobuf native record" || return 1

    # A body that claims gzip and is not gets a 400, not a panic.
    otlp_post_file application/json /tmp/vmgz.json > /dev/null 2>&1
    printf 'not gzipped at all' > /tmp/vmnotgz.bin
    python3 - << 'PYEOF' > /tmp/og3.out 2>&1
import http.client
conn = http.client.HTTPConnection("127.0.0.1", 4318, timeout=15)
conn.request("POST", "/v1/logs", b"not gzipped at all",
             {"Content-Type": "application/json", "Content-Encoding": "gzip"})
r = conn.getresponse()
print("%d" % r.status)
PYEOF
    grep -q '^400$' /tmp/og3.out || { cat /tmp/og3.out; return 1; }
}

otlp_roundtrip_over_json_and_gzip() {
    # The same store, shipped with the other encoding and compressed: a
    # transport choice must not change the data.
    rm -f "$OTLP_ROOT"/rtjson/rtjson.log.*
    timber-otlp --quiet --encoding json --compress gzip --service rtjson \
        --endpoint http://127.0.0.1:4318 "$PIPE_BACKING/rt.log" > /tmp/rtj.ship 2>&1 \
        || { cat /tmp/rtj.ship; return 1; }
    otlp_wait_for "$(otlp_store rtjson)" "roundtrip retrying" || return 1
    timberfs query "$PIPE_BACKING/rt.log" > /tmp/rtj.a 2>/dev/null
    timberfs query "$(otlp_store rtjson)" > /tmp/rtj.b 2>/dev/null
    diff -u /tmp/rtj.a /tmp/rtj.b
}

# P6: the follower registry. A follower is a REGISTERED reader of a store --
# a name, a type, a `retaining` flag and a durable position -- and
# timberfs-follower@.service runs it by name, `timberfs follower run %i`
# reading the declaration and EXEC'ing the shipper. These tests use the OTLP
# intake above as the receiver, so a real store really is shipped.
FOLLOWER_REG=/var/lib/timberfs/followers
FOLLOWER_SRC="$PIPE_BACKING/vmsrc.log"

follower_registry_declares_and_refuses() {
    rm -rf "$FOLLOWER_REG"/vmfollow "$FOLLOWER_SRC".* "$OTLP_ROOT"/vmfollowed
    timberfs create --wal "$FOLLOWER_SRC" >/dev/null 2>&1 || return 1
    printf '2026-08-01T10:00:01Z INFO followed one\n2026-08-01T10:00:02Z INFO followed two\n' \
        | timberfs append --into "$FOLLOWER_SRC" --quiet --flush-age 1 || return 1

    timberfs follower create vmfollow --store "$FOLLOWER_SRC" --type otlp \
        --endpoint http://127.0.0.1:4318 --retaining \
        -- --service vmfollowed > /tmp/f.create 2>&1 || { cat /tmp/f.create; return 1; }

    # The store is recorded by IDENTITY, minted by create when it had none --
    # a path would not do, a store being movable.
    local sid
    sid=$(jq -r .id "$FOLLOWER_SRC.bark")
    [ -n "$sid" ] && [ "$sid" != null ] || return 1
    jq -e --arg id "$sid" '.store == $id and .type == "otlp" and .retaining == true
                           and .args == ["--service", "vmfollowed"]' \
        "$FOLLOWER_REG/vmfollow/follower.json" >/dev/null \
        || { cat "$FOLLOWER_REG/vmfollow/follower.json"; return 1; }
    # The footgun is stated where it is created, not discovered later.
    grep -q 'holds the whole store until it first runs' /tmp/f.create \
        || { cat /tmp/f.create; return 1; }

    # A taken name is a REGISTRATION error -- never two processes overwriting
    # one position, which is the whole reason the registry exists.
    timberfs follower create vmfollow --store "$FOLLOWER_SRC" > /tmp/f.dup 2>&1 && return 1
    grep -q 'already exists' /tmp/f.dup || { cat /tmp/f.dup; return 1; }
    # A name that would need systemd-escape is refused, not escaped: the name
    # in `systemctl status` must be the name that was typed.
    timberfs follower create 'vm@follow' --store "$FOLLOWER_SRC" > /tmp/f.bad 2>&1 && return 1
    grep -q 'systemd-escape' /tmp/f.bad || { cat /tmp/f.bad; return 1; }
    # An unrunnable type fails at registration, not at the first start.
    timberfs follower create vmkafka --store "$FOLLOWER_SRC" --type kafka > /tmp/f.type 2>&1 && return 1
    grep -q 'unknown follower type' /tmp/f.type || { cat /tmp/f.type; return 1; }
    [ ! -d "$FOLLOWER_REG/vmkafka" ] || return 1

    # status says what retaining currently DOES, rather than letting a
    # declared interest read as an enforced one.
    timberfs follower status vmfollow > /tmp/f.status 2>&1 || return 1
    grep -q 'no writer honours it yet' /tmp/f.status || { cat /tmp/f.status; return 1; }
    grep -q 'never delivered' /tmp/f.status || { cat /tmp/f.status; return 1; }
    grep -q 'running   no' /tmp/f.status || { cat /tmp/f.status; return 1; }

    # And it refuses to be deleted while retaining, naming the two-step.
    timberfs follower delete vmfollow > /tmp/f.del 2>&1 && return 1
    grep -q 'retaining=false' /tmp/f.del || { cat /tmp/f.del; return 1; }
    [ -d "$FOLLOWER_REG/vmfollow" ]
}

follower_unit_execs_the_shipper() {
    # The headline: systemd runs `timberfs follower run vmfollow`, which
    # reads the declaration and EXECs timber-otlp with --follow, the
    # registry's own cursor, and --start begin derived from `retaining` --
    # so the backlog the follower was registered to protect is shipped
    # rather than skipped.
    systemctl enable --now timberfs-follower@vmfollow || return 1
    # Generous, and deliberately so: end to end this is the shipper's own
    # reader starting, a batch timeout, the POST, and the receiver's
    # flush-and-fsync before its ack -- ~13s on an idle laptop for the
    # cursor test next door, and more on a busy VM. A fixed short wait is
    # a race the VM loses whenever it is busy.
    local i
    for i in $(seq 1 40); do
        timberfs query "$(otlp_store vmfollowed)" 2>/dev/null | grep -q 'followed two' && break
        sleep 1
    done
    timberfs query "$(otlp_store vmfollowed)" 2>/dev/null | grep -q 'followed two' || {
        journalctl -u timberfs-follower@vmfollow --no-pager | tail -30
        cat "$FOLLOWER_REG/vmfollow/cursor.json" 2>&1
        return 1
    }
    # Both entries, from the beginning: --start end would have shipped none.
    timberfs query "$(otlp_store vmfollowed)" > /tmp/f.recv 2>/dev/null
    grep -q 'followed one' /tmp/f.recv || { cat /tmp/f.recv; return 1; }

    # The unit's main process IS the shipper: exec, not fork -- a
    # dispatcher, not a supervisor.
    local main
    main=$(systemctl show -p MainPID --value timberfs-follower@vmfollow)
    tr '\0' ' ' < "/proc/$main/cmdline" | grep -q 'timber-otlp' \
        || { tr '\0' ' ' < "/proc/$main/cmdline"; return 1; }
    tr '\0' ' ' < "/proc/$main/cmdline" | grep -q -- '--start begin' \
        || { tr '\0' ' ' < "/proc/$main/cmdline"; return 1; }

    # StateDirectory= made the registry directory, and the position landed
    # in it, anchored to the same store the declaration names.
    jq -e --arg id "$(jq -r .id "$FOLLOWER_SRC.bark")" \
        '.store == $id and .delivered >= 2' \
        "$FOLLOWER_REG/vmfollow/cursor.json" >/dev/null \
        || { cat "$FOLLOWER_REG/vmfollow/cursor.json"; return 1; }
}

follower_liveness_and_collision() {
    # Liveness comes from the lock, which `run` takes and the exec inherits
    # -- so the shipper needs no lock code and a second run of one follower
    # is refused rather than allowed to overwrite its position.
    timberfs follower list > /tmp/f.list 2>&1 || return 1
    head -1 /tmp/f.list | grep -q 'RUNNING' || { cat /tmp/f.list; return 1; }
    grep -E '^vmfollow[[:space:]]' /tmp/f.list | grep -q 'yes$' || { cat /tmp/f.list; return 1; }
    timberfs follower list --json | jq -e '.[0].running == true' >/dev/null || return 1

    timberfs follower run vmfollow > /tmp/f.race 2>&1 && return 1
    grep -q 'already running' /tmp/f.race || { cat /tmp/f.race; return 1; }
    # And it named the live holder, from the lock file's own record.
    grep -q 'timber-otlp' /tmp/f.race || { cat /tmp/f.race; return 1; }

    # A refusal must leave NOTHING behind, --stop/--disable included:
    # stopping the unit of a follower we then decline to delete would be
    # exactly the silent release the refusal exists to prevent.
    timberfs follower delete vmfollow --stop --disable > /tmp/f.delret 2>&1 && return 1
    grep -q 'is retaining' /tmp/f.delret || { cat /tmp/f.delret; return 1; }
    systemctl --quiet is-active timberfs-follower@vmfollow         || { echo "the refused delete stopped the unit anyway" >&2; return 1; }
    systemctl --quiet is-enabled timberfs-follower@vmfollow         || { echo "the refused delete disabled the unit anyway" >&2; return 1; }

    # And once released, a RUNNING follower still cannot be deleted out
    # from under itself: it would be left writing an unlinked position
    # file, doing nothing at all.
    timberfs follower update vmfollow retaining=false >/dev/null 2>&1 || return 1
    timberfs follower delete vmfollow > /tmp/f.delrun 2>&1 && return 1
    grep -q 'is running' /tmp/f.delrun || { cat /tmp/f.delrun; return 1; }
    systemctl --quiet is-active timberfs-follower@vmfollow || return 1
    timberfs follower update vmfollow retaining=true >/dev/null 2>&1
}

follower_store_side_view() {
    # From the store's side: `info` grows a followers block and `list` a
    # FOLLOWERS column, with the registered follower named.
    timberfs info "$FOLLOWER_SRC" > /tmp/f.info 2>&1 || return 1
    grep -q 'followers 1 registered, 1 retaining' /tmp/f.info || { cat /tmp/f.info; return 1; }
    grep -qE '^ +vmfollow +retaining,' /tmp/f.info || { cat /tmp/f.info; return 1; }
    # Always an array for a pair, never null: the registry knows every
    # follower of every store, so empty would mean empty.
    timberfs info --json "$FOLLOWER_SRC" \
        | jq -e '.followers | length == 1 and .[0].name == "vmfollow"' >/dev/null || return 1
    # A .timber bundle is a snapshot, so the question does not arise there.
    timberfs export "$FOLLOWER_SRC" --into /tmp/f.timber >/dev/null 2>&1 || return 1
    timberfs info --json /tmp/f.timber | jq -e 'has("followers") == false' >/dev/null || return 1

    timberfs list "$PIPE_BACKING" > /tmp/f.slist 2>/dev/null || return 1
    head -1 /tmp/f.slist | grep -q 'FOLLOWERS' || { cat /tmp/f.slist; return 1; }
    grep -E '[[:space:]]vmsrc[[:space:]]' /tmp/f.slist | grep -q '1, ' || { cat /tmp/f.slist; return 1; }
}

follower_retirement_is_two_commands() {
    # Retiring one is two commands because the destructive act deserves its
    # own. `update retaining=false` releases the head and says the part that
    # is easy to miss: the FLAG toggles, its EFFECT does not.
    timberfs follower update vmfollow retaining=false > /tmp/f.release 2>&1 || return 1
    grep -q 'releases the head' /tmp/f.release || { cat /tmp/f.release; return 1; }
    grep -q 'does not undo' /tmp/f.release || { cat /tmp/f.release; return 1; }
    # A running follower keeps the OLD declaration until it restarts, and is
    # told so rather than left to assume otherwise.
    grep -q 'running with the OLD declaration' /tmp/f.release \
        || { cat /tmp/f.release; return 1; }
    jq -e '.retaining == false' "$FOLLOWER_REG/vmfollow/follower.json" >/dev/null || return 1

    # An update that changes nothing writes nothing.
    timberfs follower update vmfollow retaining=false > /tmp/f.noop 2>&1 || return 1
    grep -q 'already declares that' /tmp/f.noop || { cat /tmp/f.noop; return 1; }

    # Then delete is bookkeeping -- and takes the unit down with it.
    timberfs follower delete vmfollow --stop --disable > /tmp/f.gone 2>&1 || {
        cat /tmp/f.gone; return 1; }
    [ ! -d "$FOLLOWER_REG/vmfollow" ] || return 1
    systemctl --quiet is-active timberfs-follower@vmfollow && return 1
    systemctl --quiet is-enabled timberfs-follower@vmfollow && return 1
    # An empty registry is a note, not an error, and --json stays an array.
    timberfs follower list --json | jq -e 'length == 0' >/dev/null || return 1
    timberfs follower list --names | grep -q . && return 1
    # A name nobody registered is an error that says how to look.
    timberfs follower status vmfollow > /tmp/f.absent 2>&1 && return 1
    grep -q 'follower list' /tmp/f.absent || { cat /tmp/f.absent; return 1; }
    return 0
}

follower_unit_installed() {
    test -f /lib/systemd/system/timberfs-follower@.service \
        && grep -q 'timberfs follower run %i' /lib/systemd/system/timberfs-follower@.service \
        && grep -q 'StateDirectory=timberfs/followers/%i' \
            /lib/systemd/system/timberfs-follower@.service \
        && test -f /usr/share/doc/timberfs/examples/timberfs-follower.conf.example \
        && zcat /usr/share/man/man1/timberfs.1.gz | grep -q 'follower create NAME'
}

run_test "otlp-intake: enable socket, unit activates" otlp_intake_setup
run_test "otlp-intake: undeclared stream 503s until the operator creates it" otlp_intake_undeclared_refused_until_created
run_test "otlp-intake: non-OTLP encodings, signals and methods refused by name" otlp_intake_refuses_the_right_things
run_test "otlp-intake: --auto-create --index drop-in" otlp_intake_enable_auto_create
run_test "otlp-intake: a native record becomes a stamped, greppable line" otlp_intake_renders_a_native_record
run_test "otlp-intake: a sender's clock skew does not stall the chunk flush" otlp_intake_flush_age_survives_a_senders_clock_skew
run_test "otlp-intake: store path is the route value, handle-resolvable" otlp_intake_store_path_is_the_route_value
run_test "otlp-intake: trace id rides the token index" otlp_intake_trace_id_is_indexed
run_test "otlp-intake: resource attributes seed the manifest, wal live" otlp_intake_seeds_the_resource
run_test "otlp-intake: service restart, sender retries, still lands" otlp_intake_restart_survives
run_test "otlp-intake: a foreign sender's protobuf decodes" otlp_intake_accepts_a_foreign_protobuf_sender
run_test "otlp-intake: gzipped bodies inflate, both encodings" otlp_intake_inflates_gzip
run_test "otlp: store shipped out and received back is byte for byte" otlp_roundtrip_is_byte_for_byte
run_test "otlp: the same roundtrip over json + gzip" otlp_roundtrip_over_json_and_gzip
run_test "timber-otlp: --dry-run renders one LogRecord per entry" timber_otlp_dry_run_shape
run_test "timber-otlp: cursor resumes after a kill, no duplicates" timber_otlp_cursor_resumes_without_duplicates
run_test "timber-otlp: a pre-numbering cursor converts to a chunk position" cursor_converts_from_a_write_time_position
run_test "chunk numbers: dense, survive a head-drop, v1 index migrates on open" chunk_numbers_and_v1_migration
run_test "records: entries carry chunk=, --from-chunk resumes at one" records_carry_the_chunk_number
run_test "consumers: list/info show lag and held bytes; a dropped position is a GAP" consumer_view_and_gap
# P7: retain_unconsumed -- the third retention axis. A retaining follower's
# position holds the store's head back, additively with age and size, and
# when the size budget overrides it the loss is recorded exactly.
RU_STORE="$PIPE_BACKING/vmunconsumed.log"

declared_booleans_are_booleans() {
    # `--set index=true` writing "index": "true" declares a key every
    # reader evaluates as FALSE -- silently declared, silently ignored. The
    # two spellings must agree, so both are checked here.
    rm -f "$PIPE_BACKING"/vmbool.log.*
    timberfs create --set index=true --set host=edge01 "$PIPE_BACKING/vmbool.log" >/dev/null 2>&1         || return 1
    jq -e '.index == true and .host == "edge01"' "$PIPE_BACKING/vmbool.log.bark" >/dev/null         || { cat "$PIPE_BACKING/vmbool.log.bark"; return 1; }
    timberfs set "$PIPE_BACKING/vmbool.log" wal=true >/dev/null 2>&1 || return 1
    jq -e '.wal == true' "$PIPE_BACKING/vmbool.log.bark" >/dev/null || return 1
    # And a value that is neither is refused rather than stored as truthy.
    timberfs create --set index=yes "$PIPE_BACKING/vmbool2.log" > /tmp/bool.err 2>&1 && return 1
    grep -q 'true or false' /tmp/bool.err || { cat /tmp/bool.err; return 1; }
}

retain_unconsumed_needs_its_backstop() {
    # Interest only ever holds MORE, so without a budget one stalled
    # follower fills the disk and kills the producer. Refused at the
    # keyboard, both ways round, on the whole resulting manifest.
    rm -f "$PIPE_BACKING"/vmbackstop.log.*
    timberfs create "$PIPE_BACKING/vmbackstop.log" >/dev/null 2>&1 || return 1
    timberfs set "$PIPE_BACKING/vmbackstop.log" retain_unconsumed=true > /tmp/ru.no 2>&1 && return 1
    grep -q 'retain_size' /tmp/ru.no || { cat /tmp/ru.no; return 1; }
    # An age window is not a backstop: it is a bet on how long the link
    # stays down, which is what this axis exists to stop anyone making.
    timberfs set "$PIPE_BACKING/vmbackstop.log" retain=90d retain_unconsumed=true \
        > /tmp/ru.age 2>&1 && return 1
    grep -q 'retain_size' /tmp/ru.age || { cat /tmp/ru.age; return 1; }
    # With a budget it takes.
    timberfs set "$PIPE_BACKING/vmbackstop.log" retain_size=50G retain_unconsumed=true \
        >/dev/null 2>&1 || return 1
    # And removing the budget out from under it is refused too, since the
    # check is on the whole manifest rather than on what this call set.
    timberfs set "$PIPE_BACKING/vmbackstop.log" --unset retain_size > /tmp/ru.unset 2>&1 && return 1
    grep -q 'retain_size' /tmp/ru.unset || { cat /tmp/ru.unset; return 1; }
    # create refuses the same combination before the store exists.
    rm -f "$PIPE_BACKING"/vmbackstop2.log.*
    timberfs create --retain-unconsumed "$PIPE_BACKING/vmbackstop2.log" > /tmp/ru.cr 2>&1 && return 1
    [ ! -e "$PIPE_BACKING/vmbackstop2.log.rings" ] || return 1
}

retain_unconsumed_holds_then_releases() {
    # The headline. A retaining follower with NO position holds
    # EVERYTHING -- which is the point, since that is a follower deployed
    # before it first runs -- and once it has a position the consumed
    # prefix goes, promptly, with no hysteresis.
    rm -rf "$FOLLOWER_REG"/vmru "$RU_STORE".*
    timberfs create --wal --retain-size 1G --retain-unconsumed "$RU_STORE" >/dev/null 2>&1 \
        || return 1
    local i
    for i in 1 2 3 4 5; do
        printf '2026-08-02T09:00:0%s INFO unconsumed line %s\n' "$i" "$i" \
            | timberfs append --into "$RU_STORE" --quiet 2>/dev/null || return 1
    done
    [ "$(timberfs index "$RU_STORE" | grep -cE '^ +[0-9]')" = 5 ] || return 1

    timberfs follower create vmru --store "$RU_STORE" --type otlp \
        --endpoint http://127.0.0.1:4318 --retaining >/dev/null 2>&1 || return 1
    # Registered, never run: the store must not shrink by a single chunk.
    printf '2026-08-02T09:00:06 INFO tick\n' | timberfs append --into "$RU_STORE" --quiet \
        > /tmp/ru.hold 2>&1
    grep -q 'retention dropped' /tmp/ru.hold && { cat /tmp/ru.hold; return 1; }
    [ "$(timberfs index "$RU_STORE" | grep -cE '^ +[0-9]')" = 6 ] || return 1
    # And info says who is holding it, and how much.
    timberfs info "$RU_STORE" | grep -q 'retaining, never run' || { timberfs info "$RU_STORE"; return 1; }

    # A position at chunk 3, exactly as the shipper writes one.
    local sid
    sid=$(jq -r .id "$RU_STORE.bark")
    python3 - "$sid" "$RU_STORE" "$FOLLOWER_REG/vmru/cursor.json" << 'PYEOF' || return 1
import json, sys
sid, store, out = sys.argv[1:4]
json.dump({"consumer": "timber-otlp", "store": sid, "path": store,
           "seq": 3, "n": 1, "wl": 1, "delivered": 4}, open(out, "w"))
PYEOF
    printf '2026-08-02T09:00:07 INFO tick\n' | timberfs append --into "$RU_STORE" --quiet \
        > /tmp/ru.drop 2>&1
    grep -q 'retention dropped 3 chunk(s)' /tmp/ru.drop || { cat /tmp/ru.drop; return 1; }
    # The follower's OWN chunk stays: `n` counts inside it, and a resume
    # re-reads it from the start.
    timberfs index "$RU_STORE" | awk '$1 ~ /^[0-9]+$/ {print $1; exit}' | grep -qx 3 \
        || { timberfs index "$RU_STORE"; return 1; }
    # No hysteresis: one more consumed chunk goes on the next tick, where
    # the age axis would wait for a tenth of the file.
    python3 - "$sid" "$RU_STORE" "$FOLLOWER_REG/vmru/cursor.json" << 'PYEOF' || return 1
import json, sys
sid, store, out = sys.argv[1:4]
json.dump({"consumer": "timber-otlp", "store": sid, "path": store,
           "seq": 4, "n": 1, "wl": 1, "delivered": 5}, open(out, "w"))
PYEOF
    printf '2026-08-02T09:00:08 INFO tick\n' | timberfs append --into "$RU_STORE" --quiet \
        > /tmp/ru.one 2>&1
    grep -q 'retention dropped 1 chunk(s)' /tmp/ru.one || { cat /tmp/ru.one; return 1; }
}

retain_unconsumed_fails_closed() {
    # Each of these is indistinguishable from "consumed" if read wrong, so
    # each must drop nothing by interest -- while age and size go on
    # working, since the axis is additive.
    local sid
    sid=$(jq -r .id "$RU_STORE.bark")
    local before
    before=$(timberfs index "$RU_STORE" | grep -cE '^ +[0-9]')

    # A position past everything the store has ever written: provably a
    # wrong anchor or a hand-edit, where a future TIMESTAMP could only
    # ever have been suspicious.
    python3 - "$sid" "$RU_STORE" "$FOLLOWER_REG/vmru/cursor.json" << 'PYEOF' || return 1
import json, sys
sid, store, out = sys.argv[1:4]
json.dump({"consumer": "x", "store": sid, "path": store,
           "seq": 99999, "n": 1, "wl": 1, "delivered": 9}, open(out, "w"))
PYEOF
    printf '2026-08-02T09:01:00 INFO tick\n' | timberfs append --into "$RU_STORE" --quiet \
        > /tmp/ru.fc1 2>&1
    grep -q 'retention dropped' /tmp/ru.fc1 && { cat /tmp/ru.fc1; return 1; }

    # A position that cannot be read is not a position.
    echo '{ not json' > "$FOLLOWER_REG/vmru/cursor.json"
    printf '2026-08-02T09:01:01 INFO tick\n' | timberfs append --into "$RU_STORE" --quiet \
        > /tmp/ru.fc2 2>&1
    grep -q 'retention dropped' /tmp/ru.fc2 && { cat /tmp/ru.fc2; return 1; }

    # An unreadable DECLARATION fails closed too, and is loud about it in
    # the one command that can see it.
    cp "$FOLLOWER_REG/vmru/follower.json" /tmp/ru.decl.bak
    echo 'not json' > "$FOLLOWER_REG/vmru/follower.json"
    printf '2026-08-02T09:01:02 INFO tick\n' | timberfs append --into "$RU_STORE" --quiet \
        > /tmp/ru.fc3 2>&1
    grep -q 'retention dropped' /tmp/ru.fc3 && { cat /tmp/ru.fc3; return 1; }
    timberfs follower list 2>&1 | grep -q 'not readable' || return 1
    cp /tmp/ru.decl.bak "$FOLLOWER_REG/vmru/follower.json"

    # Nothing was dropped through any of that.
    [ "$(timberfs index "$RU_STORE" | grep -cE '^ +[0-9]')" -ge "$before" ]
}

retain_unconsumed_cap_overrides_and_records_it() {
    # Interest is ADDITIVE, never a cap: letting it cap the drop would let
    # one stalled follower pin the store until the disk fills, which kills
    # the producer. So the budget wins -- and the loss is recorded exactly,
    # by the writer, at the moment it happens.
    local store="$PIPE_BACKING/vmoverride.log"
    rm -rf "$FOLLOWER_REG"/vmover "$store".*
    timberfs create --retain-size 1G --retain-unconsumed "$store" >/dev/null 2>&1 || return 1
    timberfs follower create vmover --store "$store" --type otlp \
        --endpoint http://127.0.0.1:4318 --retaining >/dev/null 2>&1 || return 1
    seq 1 4000 | sed 's/^/2026-08-02T10:00:00 INFO padding /' \
        | timberfs append --into "$store" --quiet --chunk-size 4096 2>/dev/null || return 1
    local sid
    sid=$(jq -r .id "$store.bark")
    # Stuck at the very first chunk, holding everything after it.
    python3 - "$sid" "$store" "$FOLLOWER_REG/vmover/cursor.json" << 'PYEOF' || return 1
import json, sys
sid, store, out = sys.argv[1:4]
json.dump({"consumer": "timber-otlp", "store": sid, "path": store,
           "seq": 0, "n": 1, "wl": 1, "delivered": 1}, open(out, "w"))
PYEOF
    # A budget below what is already on disk, so the cap has to bite.
    timberfs set "$store" retain_size=3K >/dev/null 2>&1 || return 1
    printf '2026-08-02T10:01:00 INFO tick\n' | timberfs append --into "$store" --quiet \
        > /tmp/ru.over 2>&1
    grep -q 'retention dropped' /tmp/ru.over || { cat /tmp/ru.over; return 1; }
    # The record: the budget, the follower, its position, and the exact
    # range of chunks it had not read. Not a count -- numbering survives a
    # head-drop, and a position is compared against numbers.
    grep -qE 'retain_size \(3\.0 KiB\) reached with follower vmover at chunk 0 — dropped chunks 0\.\.[0-9]+ it had not read' \
        /tmp/ru.over || { cat /tmp/ru.over; return 1; }
}

trim_is_the_one_shot_for_an_idle_store() {
    # Retention runs inside a live WRITER, so a store whose producer went
    # quiet keeps data already shipped off the box. `trim` is the answer,
    # and it must refuse to touch a store somebody else is writing.
    local store="$PIPE_BACKING/vmtrim.log"
    rm -rf "$FOLLOWER_REG"/vmtrim "$store".*
    timberfs create --retain-size 1G --retain-unconsumed "$store" >/dev/null 2>&1 || return 1
    local i
    for i in 1 2 3 4 5; do
        printf '2026-08-03T09:00:0%s INFO trim line %s\n' "$i" "$i" \
            | timberfs append --into "$store" --quiet 2>/dev/null || return 1
    done
    timberfs follower create vmtrim --store "$store" --type otlp \
        --endpoint http://127.0.0.1:4318 --retaining >/dev/null 2>&1 || return 1
    local sid
    sid=$(jq -r .id "$store.bark")
    python3 - "$sid" "$store" "$FOLLOWER_REG/vmtrim/cursor.json" << 'PYEOF' || return 1
import json, sys
sid, store, out = sys.argv[1:4]
json.dump({"consumer": "timber-otlp", "store": sid, "path": store,
           "seq": 3, "n": 1, "wl": 1, "delivered": 4}, open(out, "w"))
PYEOF
    # The preview changes nothing and names what interest would take.
    timberfs trim "$store" --dry-run > /tmp/tr.dry 2>&1 || return 1
    grep -q 'interest would drop 3 of 5' /tmp/tr.dry || { cat /tmp/tr.dry; return 1; }
    [ "$(timberfs index "$store" | grep -cE '^ +[0-9]')" = 5 ] || return 1

    # Then the real thing, reporting the chunk NUMBERS it took.
    timberfs trim "$store" > /tmp/tr.out 2>&1 || return 1
    grep -q 'trimmed 3 chunk(s)' /tmp/tr.out || { cat /tmp/tr.out; return 1; }
    grep -q 'chunks 0..2' /tmp/tr.out || { cat /tmp/tr.out; return 1; }
    [ "$(timberfs index "$store" | grep -cE '^ +[0-9]')" = 2 ] || return 1
    # Idempotent.
    timberfs trim "$store" 2>&1 | grep -q 'nothing to trim' || return 1

    # A store with a live writer is left ALONE and says so: that writer's
    # own tick is already enforcing this.
    mkfifo /tmp/trimlive.fifo
    timberfs append --into "$store" --flush-age 60 < /tmp/trimlive.fifo &
    local ap=$!
    exec 9>/tmp/trimlive.fifo
    sleep 1
    timberfs trim "$store" > /tmp/tr.live 2>&1
    local rc=$?
    exec 9>&-
    wait "$ap" 2>/dev/null
    rm -f /tmp/trimlive.fifo
    [ "$rc" = 0 ] || { cat /tmp/tr.live; return 1; }
    grep -q 'has a live writer' /tmp/tr.live || { cat /tmp/tr.live; return 1; }

    # And a store that declares no retention is a no-op, not an error --
    # what a cron entry over a whole forest needs.
    timberfs trim "$PIPE_BACKING/piped.log" > /tmp/tr.none 2>&1 || return 1
    grep -q 'declares no retention' /tmp/tr.none || { cat /tmp/tr.none; return 1; }

    # A MOUNTED store is the other hands-off case, and a different lock:
    # the mount daemon holds the whole directory, and its own tick is
    # already enforcing this once a second.
    timberfs set "$BACKING/app.log" retain_size=1G >/dev/null 2>&1 || return 1
    timberfs trim "$BACKING/app.log" > /tmp/tr.mnt 2>&1 || { cat /tmp/tr.mnt; return 1; }
    grep -q 'live timberfs mount' /tmp/tr.mnt || { cat /tmp/tr.mnt; return 1; }
    grep -q "$MNT" /tmp/tr.mnt || { cat /tmp/tr.mnt; return 1; }
    timberfs set "$BACKING/app.log" --unset retain_size >/dev/null 2>&1

    # A bundle has no retention to enforce, and says so rather than
    # pretending to try.
    timberfs trim /tmp/f.timber > /tmp/tr.bundle 2>&1 && return 1
    grep -q 'read-only' /tmp/tr.bundle || { cat /tmp/tr.bundle; return 1; }
    return 0
}

retain_unconsumed_views_agree() {
    # The declared axis shows up wherever retention does, so an operator
    # reading `list` or `info` sees the third axis rather than inferring it.
    timberfs info "$RU_STORE" > /tmp/ru.info 2>&1 || return 1
    grep -q 'keep what retaining followers have not read' /tmp/ru.info \
        || { cat /tmp/ru.info; return 1; }
    timberfs info --json "$RU_STORE" | jq -e '.retain_unconsumed == true' >/dev/null || return 1
    timberfs list "$PIPE_BACKING" > /tmp/ru.list 2>/dev/null || return 1
    grep -E '[[:space:]]vmunconsumed[[:space:]]' /tmp/ru.list | grep -q 'unconsumed' \
        || { cat /tmp/ru.list; return 1; }
    # And a rotated segment does NOT inherit it: an archive has no
    # followers, so inheriting would make it wait on a consumer that reads
    # the live store.
    timberfs rotate "$RU_STORE" vmarchive.log --cutoff 2027-01-01 >/dev/null 2>&1 || return 1
    [ -e "$PIPE_BACKING/vmarchive.log.bark" ] || return 1
    jq -e 'has("retain_unconsumed") == false and has("retain_size") == false' \
        "$PIPE_BACKING/vmarchive.log.bark" >/dev/null \
        || { cat "$PIPE_BACKING/vmarchive.log.bark"; return 1; }
}

run_test "follower: create records the store by identity, and refuses the rest" follower_registry_declares_and_refuses
run_test "follower: the unit execs the shipper and ships from the beginning" follower_unit_execs_the_shipper
run_test "follower: liveness from the inherited lock; a second run is refused" follower_liveness_and_collision
run_test "follower: info grows a followers block, list a FOLLOWERS column" follower_store_side_view
run_test "follower: retiring one is update-then-delete, and says what it frees" follower_retirement_is_two_commands
run_test "follower: unit, conf example and man page installed" follower_unit_installed
run_test "bark: create --set declares booleans as booleans, like set" declared_booleans_are_booleans
run_test "retain_unconsumed: refused without the retain_size backstop" retain_unconsumed_needs_its_backstop
run_test "retain_unconsumed: a never-run follower holds all; a position releases the prefix" retain_unconsumed_holds_then_releases
run_test "retain_unconsumed: every way of not knowing drops nothing" retain_unconsumed_fails_closed
run_test "retain_unconsumed: the size cap overrides, and records the loss exactly" retain_unconsumed_cap_overrides_and_records_it
run_test "trim: the one-shot for an idle store; a live writer is left alone" trim_is_the_one_shot_for_an_idle_store
run_test "retain_unconsumed: list/info show the axis; a rotated segment does not inherit it" retain_unconsumed_views_agree

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
    # default forest of state left by earlier tests (the socket, text,
    # Forward and OTLP intakes now each write one directory per store, plus
    # forest_handle_resolution's nginx store) so the counts below are exact,
    # then create two nested stores of our own. The two receivers are
    # stopped first: they are idle, but a maintenance tick that finds its
    # store directory gone logs noise into the journal.
    systemctl stop timberfs-forward.service timberfs-otlp.service >/dev/null 2>&1
    find /var/log/timberfs -mindepth 1 -maxdepth 1 -type d -exec rm -rf {} +
    printf '2026-07-08T09:00:00 INFO web one\n2026-07-08T09:00:01 INFO web two\n' \
        | timberfs append --into /var/log/timberfs/web/web.log --quiet || return 1
    printf '2026-07-08T09:05:00 INFO db one\n' \
        | timberfs append --into /var/log/timberfs/db/db.log --quiet || return 1

    local out names dir_names
    out=$(timberfs list) || return 1
    echo "$out" | head -1 | grep -qE '^ID[[:space:]]+NAME' || return 1
    # Both stores here were made by a bare `append`, which declares
    # nothing and so writes no manifest: the ID column is structural, so
    # they show a dash rather than the column disappearing.
    echo "$out" | grep -qE '^-[[:space:]]+web[[:space:]]' || return 1
    # a row for each, with a real (non-"empty") SPAN — both stores have data
    echo "$out" | grep -E '[[:space:]]web[[:space:]]+default[[:space:]]' | grep -q ' \.\. ' || return 1
    echo "$out" | grep -E '[[:space:]]db[[:space:]]+default[[:space:]]' | grep -q ' \.\. ' || return 1

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
        | jq -e '.[] | select(.handle=="live") | has("writer")' >/dev/null 2>&1 \
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

frames_follower_ships_and_releases_the_head() {
    # The whole point of a retaining follower on the native wire: retention
    # holds the head back until the far end has it, the ack advances the
    # cursor, and only then may the prefix go. A byte-window ack cadence
    # starved this loop -- a quiet store was never acked, so its cursor
    # never advanced and nothing was ever released.
    local d=/tmp/framesfol
    rm -rf $d; mkdir -p $d/node $d/archive $d/reg
    export TIMBERFS_FOLLOWERS=$d/reg
    timberfs create $d/node/src.log --set service=folwire >/dev/null 2>&1 || return 1
    timberfs set $d/node/src.log retain_size=1M retain_unconsumed=true >/dev/null || return 1
    local c i
    for c in 1 2 3 4; do
        for i in $(seq 1 200); do
            echo "2026-06-0${c}T10:00:00Z chunk $c line $i padding padding padding"
        done | timberfs append --into $d/node/src.log --quiet 2>/dev/null
    done

    # Nothing to release yet: with no position the follower's interest
    # drops nothing, and the budget is deliberately generous so THIS axis
    # is what the test exercises rather than the size backstop. (Interest
    # is additive -- it only ever drops the consumed prefix, and a small
    # budget would empty the store as it was written, before any of this.)
    timberfs trim $d/node/src.log 2>&1 | grep -q "nothing to trim" || {
        timberfs trim $d/node/src.log
        unset TIMBERFS_FOLLOWERS
        return 1
    }

    timberfs frames-intake --into-dir $d/archive --listen 127.0.0.1:4320 \
        --route service --auto-create --replica >$d/intake.log 2>&1 &
    local pid=$!
    sleep 1
    timberfs follower create --store $d/node/src.log ship \
        --type frames --endpoint 127.0.0.1:4320 --retaining >/dev/null 2>&1 || {
        kill $pid; unset TIMBERFS_FOLLOWERS; return 1
    }
    timeout 5 timberfs follower run ship >$d/run.log 2>&1
    kill $pid 2>/dev/null
    sleep 1

    # The cursor holds the FAR END's acknowledged position, and the lag
    # renders as caught up rather than decades behind (wl unset).
    jq -e '.consumer == "frames-send" and .seq == 3 and .n == 0 and .wl > 0' \
        $d/reg/ship/cursor.json > /dev/null || {
        cat $d/reg/ship/cursor.json
        unset TIMBERFS_FOLLOWERS
        return 1
    }
    timberfs follower list 2>&1 | grep -q "at the live edge" || {
        timberfs follower list
        unset TIMBERFS_FOLLOWERS
        return 1
    }

    # And now the shipped prefix may go: the budget is 1K, so the interest
    # floor is what decides, and it releases everything below chunk 3.
    timberfs trim $d/node/src.log 2>&1 | grep -q "chunks 0\.\.2" || {
        timberfs trim $d/node/src.log
        unset TIMBERFS_FOLLOWERS
        return 1
    }
    unset TIMBERFS_FOLLOWERS
    timberfs info $d/archive/folwire.log/folwire.log | grep -q "4 chunk(s)"
}

frames_fleet_two_nodes_one_archive() {
    # The COMPOSED story, which no per-verb test covers: two hosts, each
    # with two logs, replicating into one archive. Every verb below is
    # tested on its own elsewhere; what this asserts is that the setup an
    # operator actually performs produces the right four stores -- and
    # what happens when the routing is wrong, which is the mistake the
    # shape invites.
    local d=/tmp/framesfleet
    rm -rf $d; mkdir -p $d/apache01 $d/apache02 $d/archive-a $d/archive-b
    local h s i
    for h in apache01 apache02; do
        for s in apache-error apache-access; do
            timberfs create $d/$h/$s.log --index \
                --set host=$h --set service=$s --set stream=$h.$s >/dev/null 2>&1 || return 1
            for i in 1 2 3; do
                printf '2026-06-0%dT10:00:00Z %s %s entry %d tok%s%04d\n' \
                    "$i" "$h" "$s" "$i" "${h#apache}" "$i" \
                    | timberfs append --into $d/$h/$s.log --quiet 2>/dev/null
            done
        done
    done

    # ROUTING ON `service` IS THE MISTAKE: both hosts call their error log
    # apache-error, so both route to one store. The first lands; the second
    # is refused and NAMES the origin already there, which is the whole
    # point of one-store-one-origin.
    #
    # Without that check the failure is quiet in either of two ways, and
    # measured with the guard disabled it is the second: apache02 is told
    # the archive "already has everything" -- because its own chunks 0..2
    # match the coverage apache01 established -- so its logs silently ship
    # NOWHERE. Where the numbering does not line up they merge instead, and
    # the manifest then describes only one of the two hosts.
    timberfs frames-intake --into-dir $d/archive-a --listen 127.0.0.1:4330 \
        --route service --auto-create --replica >$d/a.log 2>&1 &
    local pid_a=$!
    sleep 1
    timberfs frames-send $d/apache01/apache-error.log --endpoint 127.0.0.1:4330 2>&1 \
        | grep -q "sent 3 chunk" || { kill $pid_a; cat $d/a.log; return 1; }
    sleep 1
    timberfs frames-send $d/apache02/apache-error.log --endpoint 127.0.0.1:4330 2>&1 \
        | grep -q "one store" || {
        timberfs frames-send $d/apache02/apache-error.log --endpoint 127.0.0.1:4330
        kill $pid_a
        return 1
    }
    kill $pid_a 2>/dev/null
    sleep 1
    # apache01's data only: the refusal wrote nothing.
    timberfs query $d/archive-a/apache-error.log/apache-error.log 2>/dev/null \
        | grep -q apache02 && { echo "apache02 data leaked in" >&2; return 1; }

    # THE WORKING SHAPE: route on a label whose value is unique per stream.
    timberfs frames-intake --into-dir $d/archive-b --listen 127.0.0.1:4331 \
        --route stream --auto-create --replica --index >$d/b.log 2>&1 &
    local pid_b=$!
    sleep 1
    for h in apache01 apache02; do
        for s in apache-error apache-access; do
            timberfs frames-send $d/$h/$s.log --endpoint 127.0.0.1:4331 2>&1 \
                | grep -q "sent 3 chunk" || { kill $pid_b; cat $d/b.log; return 1; }
            sleep 0.3
        done
    done
    kill $pid_b 2>/dev/null
    sleep 1

    # Four stores, each byte-identical, each still saying which host it is
    # -- the label travelled, the settings did not.
    local n=0
    for h in apache01 apache02; do
        for s in apache-error apache-access; do
            local dst=$d/archive-b/$h.$s.log/$h.$s.log
            cmp -s $d/$h/$s.log.trunk $dst.trunk || {
                echo "$h.$s trunk differs" >&2
                return 1
            }
            jq -e --arg h "$h" --arg s "$s" \
                '.host == $h and .service == $s and has("origin_id")' $dst.bark >/dev/null || {
                cat $dst.bark
                return 1
            }
            n=$((n + 1))
        done
    done
    [ "$n" = 4 ] || return 1
    # And the archive lists exactly those four.
    [ "$(timberfs list $d/archive-b --names 2>/dev/null | wc -l)" = 4 ] || {
        timberfs list $d/archive-b
        return 1
    }
    # The shipped index works on the far side, per host.
    timberfs query $d/archive-b/apache02.apache-error.log/apache02.apache-error.log \
        --has tok020002 2>&1 | grep -q "1 of 3 chunk" || {
        timberfs query $d/archive-b/apache02.apache-error.log/apache02.apache-error.log --has tok020002
        return 1
    }
    rm -rf $d
}

FRAMES_UNIT_SRC=/tmp/framesunit/src.log

frames_unit_installed() {
    # A wrong asset path in Cargo.toml only shows up in the built package,
    # which is what this VM installs. The man page is checked too, since a
    # unit nobody can find documented is half-shipped.
    test -f /lib/systemd/system/timberfs-frames.socket \
        && test -f /lib/systemd/system/timberfs-frames.service \
        && grep -q 'ListenStream=127.0.0.1:4319' \
            /lib/systemd/system/timberfs-frames.socket \
        && grep -q 'timberfs frames-intake' /lib/systemd/system/timberfs-frames.service \
        && grep -q 'RestartForceExitStatus=85' /lib/systemd/system/timberfs-frames.service \
        && zcat /usr/share/man/man1/timberfs.1.gz | tr -d ' \n' \
            | grep -q 'timberfs\\-frames.socket'
}

frames_unit_socket_activates() {
    # The SHIPPED unit, started by systemd rather than by hand: every other
    # unit family is exercised this way, and a unit file that has only been
    # parsed has never been proven to start.
    rm -rf /tmp/framesunit; mkdir -p /tmp/framesunit
    systemd-tmpfiles --create
    systemctl enable --now timberfs-frames.socket || return 1
    # Socket-activated, so the service is not running until something
    # connects -- that is the point of the pair.
    systemctl --quiet is-active timberfs-frames.socket || return 1
    ! systemctl --quiet is-active timberfs-frames.service || return 1

    timberfs create $FRAMES_UNIT_SRC --index --set service=unitwire >/dev/null 2>&1 || return 1
    local i
    for i in 1 2 3; do
        printf '2026-06-0%dT10:00:00Z unit line %d marker%04d padding\n' "$i" "$i" "$i" \
            | timberfs append --into $FRAMES_UNIT_SRC --quiet 2>/dev/null
    done

    # The shipped unit has no --auto-create, so an undeclared stream is
    # refused -- and the sender is TOLD, which is what the handshake buys.
    timberfs frames-send $FRAMES_UNIT_SRC --endpoint 127.0.0.1:4319 2>&1 \
        | grep -q "refused the stream" || {
        timberfs frames-send $FRAMES_UNIT_SRC --endpoint 127.0.0.1:4319
        return 1
    }
    # ...and the connection activated the service even so.
    systemctl --quiet is-active timberfs-frames.service
}

frames_unit_replicates_into_a_declared_store() {
    # The operator provisions the destination; the sender's retry lands.
    # The shipped unit passes --replica, so this is a replica: same bytes,
    # same numbering, origin recorded.
    timberfs create --wal /var/log/timberfs/unitwire.log/unitwire.log >/dev/null 2>&1 || return 1
    timberfs frames-send $FRAMES_UNIT_SRC --endpoint 127.0.0.1:4319 2>&1 \
        | grep -q "sent 3 chunk" || {
        timberfs frames-send $FRAMES_UNIT_SRC --endpoint 127.0.0.1:4319
        journalctl -u timberfs-frames.service -n 20 --no-pager
        return 1
    }
    sleep 1
    local dst=/var/log/timberfs/unitwire.log/unitwire.log
    cmp -s $FRAMES_UNIT_SRC.trunk $dst.trunk || {
        echo "trunk differs" >&2
        return 1
    }
    # --replica means the origin travelled and the numbering was preserved.
    jq -e '.origin_id == (input | .id)' $dst.bark $FRAMES_UNIT_SRC.bark >/dev/null || {
        cat $dst.bark
        return 1
    }
    # Re-running is a no-op: the receiver's position is authoritative.
    timberfs frames-send $FRAMES_UNIT_SRC --endpoint 127.0.0.1:4319 2>&1 \
        | grep -q "already has everything"
}

frames_unit_survives_a_restart() {
    # Socket activation means the address stays bound across a service
    # restart, so a sender that reconnects is not refused -- which is what
    # --exit-on-upgrade relies on when dpkg replaces the binary.
    systemctl restart timberfs-frames.service || return 1
    sleep 1
    printf '2026-06-09T10:00:00Z unit line after restart marker9999\n' \
        | timberfs append --into $FRAMES_UNIT_SRC --quiet 2>/dev/null
    timberfs frames-send $FRAMES_UNIT_SRC --endpoint 127.0.0.1:4319 2>&1 \
        | grep -q "sent 1 chunk" || {
        timberfs frames-send $FRAMES_UNIT_SRC --endpoint 127.0.0.1:4319
        journalctl -u timberfs-frames.service -n 20 --no-pager
        return 1
    }
    sleep 1
    cmp -s $FRAMES_UNIT_SRC.trunk /var/log/timberfs/unitwire.log/unitwire.log.trunk
}

frames_unit_teardown() {
    systemctl disable --now timberfs-frames.socket >/dev/null 2>&1
    systemctl stop timberfs-frames.service >/dev/null 2>&1
    rm -rf /tmp/framesunit /var/log/timberfs/unitwire.log
    true
}

frames_wire_replicates_a_store_byte_for_byte() {
    # The native wire end to end through the CLI: a store crosses a socket
    # as compressed frames, arrives byte-identical, and its shipped token
    # index still skips chunks on the far side. Re-running sends nothing,
    # because the receiver's position is what the sender resumes from.
    local d=/tmp/frames
    rm -rf $d; mkdir -p $d/node $d/archive
    timberfs create $d/node/src.log --index --set host=vmnode --set service=vmwire \
        >/dev/null 2>&1 || return 1
    local i
    for i in $(seq 1 6); do
        printf '2026-06-0%dT10:00:00Z wire line %d marker%04d padding padding\n' \
            "$i" "$i" "$i" | timberfs append --into $d/node/src.log --quiet 2>/dev/null
    done

    timberfs frames-intake --into-dir $d/archive --listen 127.0.0.1:4319 \
        --route service --auto-create --replica --index >$d/intake.log 2>&1 &
    local pid=$!
    sleep 1

    timberfs frames-send $d/node/src.log --endpoint 127.0.0.1:4319 2>&1 \
        | grep -q "sent 6 chunk" || { cat $d/intake.log; kill $pid; return 1; }
    sleep 1
    # Idempotent: the receiver already holds it all.
    timberfs frames-send $d/node/src.log --endpoint 127.0.0.1:4319 2>&1 \
        | grep -q "already has everything" || { kill $pid; return 1; }
    sleep 1
    kill $pid 2>/dev/null; sleep 1

    local dst=$d/archive/vmwire.log/vmwire.log
    # Byte-identical trunk AND grain -- nothing recompressed, nothing
    # re-tokenized.
    local ext
    for ext in trunk grain; do
        cmp -s "$d/node/src.log.$ext" "$dst.$ext" || {
            echo "$ext differs" >&2
            return 1
        }
    done
    # The origin travelled and the numbering was preserved together.
    jq -e '.origin_id == (input | .id) and .derived_op == "receive"' \
        "$dst.bark" "$d/node/src.log.bark" > /dev/null || {
        cat "$dst.bark"
        return 1
    }
    # And the shipped index is live on the replica. The token has to be a
    # run of 3+ alphanumerics to be indexed at all -- `unique-3` tokenizes
    # to [unique, 3] and the digit is below MIN_TOKEN, so it would scan
    # every chunk and prove nothing.
    timberfs query "$dst" --has marker0003 2>&1 | grep -q "1 of 6 chunk" || {
        timberfs query "$dst" --has marker0003
        return 1
    }
}

import_carries_identity_across_the_hop() {
    # A timberfs source describes itself, so an import must not land
    # anonymous: the destination mints its own id, records the IMMEDIATE
    # parent as derived_from, and inherits the labels. Operational settings
    # do NOT travel -- retention and the index are the destination's own
    # policy, so a store received here keeps what the operator declared.
    local d=/tmp/lineage
    rm -rf $d; mkdir -p $d
    timberfs create $d/src.log --index --set host=vmnode --set service=vmsvc         --set retain_size=5G >/dev/null 2>&1 || return 1
    printf '2026-06-02T08:00:00 INFO lineage one\n'         | timberfs append --into $d/src.log --quiet 2>/dev/null || return 1
    local src_id
    src_id=$(jq -r .id $d/src.log.bark)

    timberfs export $d/src.log --into $d/ship.timber --quiet 2>/dev/null || return 1
    timberfs import --into $d/dst.log $d/ship.timber --quiet 2>/dev/null || return 1

    # Labels and lineage arrived; identity is the destination's own; the
    # source's retention and index declarations did not follow.
    jq -e '.host == "vmnode" and .service == "vmsvc"
           and .derived_op == "import"
           and (.derived_from | type == "string")
           and (.id | type == "string")
           and has("retain_size") == false
           and has("index") == false' $d/dst.log.bark > /dev/null         || { cat $d/dst.log.bark; return 1; }
    [ "$(jq -r .id $d/dst.log.bark)" != "$src_id" ] || return 1

    # A pair source names itself as the parent directly.
    timberfs import --into $d/pair.log $d/src.log --quiet 2>/dev/null || return 1
    [ "$(jq -r .derived_from $d/pair.log.bark)" = "$src_id" ] || return 1

    # Two identified sources have no single parent, so no lineage is
    # claimed rather than one of them guessed at.
    timberfs create $d/src2.log --set host=vmnode2 >/dev/null 2>&1 || return 1
    printf '2026-06-02T09:00:00 INFO lineage two\n'         | timberfs append --into $d/src2.log --quiet 2>/dev/null || return 1
    timberfs import --into $d/multi.log $d/src.log $d/src2.log 2>&1         | grep -q "no single parent" || return 1
    [ ! -f $d/multi.log.bark ] || return 1

    # And a re-import leaves the manifest alone: it is the operator's now.
    local before
    before=$(jq -r .id $d/dst.log.bark)
    timberfs import --into $d/dst.log $d/ship.timber --quiet 2>/dev/null
    [ "$(jq -r .id $d/dst.log.bark)" = "$before" ]
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
run_test "import: identity and labels cross the hop, policy does not" import_carries_identity_across_the_hop
run_test "frames wire: a store replicates over a socket byte for byte" frames_wire_replicates_a_store_byte_for_byte
run_test "frames wire: a retaining follower ships, then the head releases" frames_follower_ships_and_releases_the_head
run_test "frames fleet: two nodes, one archive, and the routing mistake" frames_fleet_two_nodes_one_archive
run_test "frames unit: installed by the package, and documented" frames_unit_installed
run_test "frames unit: socket activates, undeclared stream refused" frames_unit_socket_activates
run_test "frames unit: replicates into a declared store" frames_unit_replicates_into_a_declared_store
run_test "frames unit: the socket outlives a service restart" frames_unit_survives_a_restart
run_test "frames unit: teardown" frames_unit_teardown
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

writer_handoff_waits() {
    # A supervisor's reload starts the replacement writer before the old
    # one has exited (Apache's piped logs): the new writer waits out the
    # handoff instead of failing, and a writer that never leaves is
    # reported by name.
    mkfifo /tmp/ho.pipe
    setsid timberfs append --into "$PIPE_BACKING/handoff.log" < /tmp/ho.pipe 2>/dev/null &
    sleep 1
    exec 3>/tmp/ho.pipe
    printf 'old writer line\n' >&3
    sleep 1
    OLD=$(sed -n 's/.*pid=//p' "$PIPE_BACKING/handoff.log.lock")
    # the old writer goes away a second into the new one's wait
    ( sleep 1; kill -TERM "$OLD" ) &
    printf 'new writer line\n' | timberfs append --into "$PIPE_BACKING/handoff.log" 2>/dev/null \
        && exec 3>&- \
        && [ "$(timberfs query "$PIPE_BACKING/handoff.log" 2>/dev/null | wc -l)" = 2 ] \
        || { exec 3>&-; return 1; }
    # a writer that stays put: named in the error, not just refused
    mkfifo /tmp/ho2.pipe
    setsid timberfs append --into "$PIPE_BACKING/squat.log" < /tmp/ho2.pipe 2>/dev/null &
    sleep 1
    exec 4>/tmp/ho2.pipe
    printf 'squatter\n' >&4
    sleep 1
    printf 'x\n' | timberfs append --into "$PIPE_BACKING/squat.log" --wait-for-writer 0.5 2>/tmp/squat.err
    RC=$?
    SQ=$(sed -n 's/.*pid=//p' "$PIPE_BACKING/squat.log.lock")
    kill -TERM "$SQ" 2>/dev/null
    exec 4>&-
    [ "$RC" != 0 ] \
        && grep -q "already has a writer: appender pid=$SQ" /tmp/squat.err \
        && grep -q "timberfs append --into" /tmp/squat.err \
        && grep -q "still held after waiting 0.5s" /tmp/squat.err
}

create_if_not_exists() {
    # provisioning that re-runs on every start: the second create is a
    # quiet success, a declaration the standing store disagrees with is
    # warned about and NOT applied, and without the flag an existing
    # store is still an error
    timberfs create --if-not-exists --index --retain 90d "$PIPE_BACKING/prov.log" 2>/dev/null \
        && timberfs create --if-not-exists --index --retain 90d "$PIPE_BACKING/prov.log" 2>/tmp/prov.err \
        && grep -q "nothing created" /tmp/prov.err \
        && ! grep -q "warning" /tmp/prov.err \
        && timberfs create --if-not-exists --retain 30d "$PIPE_BACKING/prov.log" 2>/tmp/prov2.err \
        && grep -q "retain is 90d, not 30d" /tmp/prov2.err \
        && grep -q '"retain": "90d"' "$PIPE_BACKING/prov.log.bark" \
        && ! timberfs create --index "$PIPE_BACKING/prov.log" 2>/dev/null
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
run_test "bark: create --if-not-exists is a quiet no-op on an existing store" create_if_not_exists
run_test "append: a reload's writer handoff waits, a squatter is named" writer_handoff_waits
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
    local lastline firstnum
    lastline=$(tail -1 "$COLLAPSE_SRC")
    timberfs query "$backing/app.log" | tail -1 | grep -qxF "$lastline" || return 1
    # Chunk numbers are identities, not positions: after a real
    # COLLAPSE_RANGE head-drop the oldest survivor keeps the number a cursor
    # already holds, so it must NOT have slid back to 0.
    firstnum=$(timberfs index "$backing/app.log" | awk '$1 ~ /^[0-9]+$/ {print $1; exit}')
    [ -n "$firstnum" ] && [ "$firstnum" -gt 0 ] || {
        echo "chunk numbering slid to $firstnum across the collapse" >&2
        timberfs index "$backing/app.log" | head -3 >&2
        return 1
    }
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

# Health checks run LAST: a test that leaks disk turns a clear failure into
# confusing cascades (a full /tmp made query fail silently, empty-stderr),
# so assert the filesystems aren't near-full and surface it as its own test.
health_filesystems_not_full() {
    local fs pct
    for fs in / /tmp /var; do
        pct=$(df --output=pcent "$fs" 2>/dev/null | tail -1 | tr -dc '0-9')
        [ -n "$pct" ] || continue
        if [ "$pct" -ge 90 ]; then
            df -h "$fs"
            echo "$fs is ${pct}% full — a test leaked disk"
            return 1
        fi
    done
}
run_test "health: filesystems not near-full" health_filesystems_not_full

echo "TIMBERFS-VM-TESTS: PASS=$PASS FAIL=$FAIL"
if [ "$FAIL" -eq 0 ]; then
    echo "TIMBERFS-VM-TESTS: ALL PASSED"
fi
DONE=1
