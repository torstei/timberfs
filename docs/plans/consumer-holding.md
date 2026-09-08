# A consumer that is HOLDING entries: `taken` beside the position

**Status: BUILT.** `taken` on the progress report, the park and the read
gating on it, the provisional bucket, `advance_to`'s clamp, and `feed` saying
`stream-end`. Measured after: **51 → 51,949 entries/s** at the unchanged
default batch size, and the tape a batched pipeline writes is now
byte-identical to one in-memory pass over the same store. What is NOT built
is the hot-loop work in its own section below, and the open questions at the
end.

It amends a claim in [consumer-protocol.md](consumer-protocol.md) (a consumer
took it or dropped it) and one in [tally.md](tally.md) (`safe_offset` is
already the answer). ⚠ And it amends ITSELF: the fix this note first proposed
for the quiet tail was wrong, and the section below says how — that is the
one part worth reading if you read nothing else.

## The defect

A tally follower on a busy store never catches up, and the numbers it has
already written are wrong. Measured against a 500k-line apache store
(100k-line store for the first row; 200 requests/s of log time, the shipped
`timberfs-apache-combined` document, `feed -- timberfs tally --run`):

| | |
|---|---|
| what the extractor can do (`--try`, 2 metrics) | **310,000 entries/s** |
| what the pipeline does at the hard-coded `BATCH_ENTRIES` of 512 | **51 entries/s** |
| ratio | **0.016%** |

Throughput is `batch_size ÷ 10 s`, and nothing else — confirmed by sweeping
it: 512 → 51/s, 4096 → 341/s, 65536 → 5750/s. Above ~51 entries/s a store
falls permanently behind, and the gap widens for as long as it is enabled.

The numbers on the tape are wrong in two distinct ways, and neither is
visible as an error:

* **Undercount, unmarked.** One bucket is written repeatedly as fragments,
  and `timbergraph.read` keeps "the NEWEST line for a bucket winning" on the
  stated grounds that "nothing emits a revision". Measured: 1536 entries
  delivered and counted, the tape holds all 1536 across fragments, and the
  reader reports **512**. The undercount factor is
  entries-per-bucket ÷ batch size, so ~23× on a store at 200 requests/s. No
  `!late`, no `!cap`, no `!drop` — zero markers.
* **Displacement, marked.** Once the stall has run long enough, every
  subsequent batch lands in a bucket that is not when it happened. Measured
  with a 10 s/0 s window to bring the crossover forward: 5632 entries
  spanning 28 s of log time were placed across 50+ s of buckets, with ten
  `!late metric=http_requests count=512` markers — *the whole batch*, every
  batch. At the shipped 60 s/120 s window the crossover arrives after ~5.5
  minutes of backfill.

⚠ **The two phases leave different traces, and only one leaves a marker.**
`!late count=<batch size>` is the displacement phase's signature, and it
needs an arrival rate high enough (~64/s at the shipped window) for the
injected wall clock to outrun the data — so a store slower than that is
under-reporting with *nothing* to grep for. What finds the fragment phase is
duplicate bucket-and-series lines, which no correct tape has:

```sh
timberfs query <tally-store> | grep -v ' !' \
  | sed -E 's/ (count|sum|min|max|last)=[^ ]*//g; s/ @[0-9]+\+[0-9]+$//' \
  | sort | uniq -d
```

⚠ Since the fix a tape carries deliberate revisions, so that check no longer
means what it meant: it is for reading a tape written by an older build.

## Two rules, each right alone

**The park.** "A store with anything UNACKNOWLEDGED is parked"
(`feed.rs`, and [consumer-protocol.md](consumer-protocol.md)) — its
recorded position only moves when the consumer acknowledges, so a read
starting there would hand back the entries already in flight. Measured when
it was written: a consumer taking entries and acknowledging none held the
process at 99% of a core without the park and 0% with it.

**The watermark.** tally reports `Roller::safe_offset` — the oldest byte any
*open* bucket still depends on — so a restart re-derives identical lines.
[tally.md](tally.md) records this as settled: "`Roller::safe_offset` is
already the answer".

Composed, they deadlock. While a bucket is open, `safe_offset` is the offset
of its **first** entry, so `take` finds no pending entry with `end <= offset`
and acknowledges nothing; the store is parked; no more entries can be sent;
event time cannot advance; the bucket cannot seal. The consumer's own report
stream says it plainly:

```
progress offset=0        <- first entry opens a bucket; the watermark pins to byte 0
                            ...nothing for 10 s...
progress offset=93135    <- one 512-entry batch, acknowledged at once
progress offset=186350   <- 10 s later, the next
```

What breaks the standoff is `cmd_run`'s idle path: after `QUIET_TICKS ×
IDLE_TICK` = 10 seconds of silence it calls `run.finish()`, force-seals
everything, `safe_offset` goes `None`, the watermark falls back to
`delivered_to`, and one batch clears. That is the 10 s in `batch_size ÷ 10 s`.

Both wrong answers above follow from that force-seal, and the second from the
`idle()` wall-clock advance beside it — see the two sections below.

## The protocol has two categories and needs a third

[consumer-protocol.md](consumer-protocol.md) says a watermark means "do not
send me these again", not "these are safe", and draws the consequence that a
consumer "may DROP whatever it likes, as long as it reports past what it
dropped". So the vocabulary has **took it** and **dropped it**.

tally is neither. It is **holding** the entries: it has them, it does not want
them again *now*, and it will need them again if it restarts, because an open
bucket is re-derived from its source bytes rather than persisted. The protocol
gives it one number to say two things with, and it correctly chooses the
conservative one — at which point the follower, which reads that number as
flow control, stops feeding it.

⚠ So this is not a number to raise. The `depth is one outstanding batch`
warning in [consumer-protocol.md](consumer-protocol.md) already says
pipelining deeper "needs a SENT offset per store, kept beside the
acknowledged one and read from instead of it". That is the shape of the fix;
what this note adds is that the *consumer* has to be the one to say where
that offset is, because only it knows what it is holding.

## The fix: `taken` beside `offset`

One optional field on the report that already exists:

```
progress id=<id> offset=<safe> taken=<flow-control>
```

* **`offset`** keeps its exact meaning and its exact use: the durable
  position, persisted, the retention floor, the place a restart resumes from.
  For tally that stays `safe_offset`.
* **`taken`** is flow control only: "I have these, do not wait for me before
  sending more." In memory, never persisted, resets to `offset` on restart —
  where re-sending is correct, because a consumer that was holding entries
  lost them when it died.
* **Omitted means `taken == offset`**, which is today's behaviour exactly. No
  existing consumer changes, and a shell-script consumer is still three
  `printf`s.

The follower then parks on `taken` rather than on `offset`, and reads from
`taken`. The anti-duplicate property the park was written for is preserved
unchanged — a consumer that acknowledges nothing *and* takes nothing still
parks at one outstanding batch, which is the case that was measured at 99% of
a core.

**The in-flight bound becomes the consumer's**, which is the right place for
it: tally already bounds what it holds by `max_series` per bucket across the
open buckets, and that bound is `width + grace` of event time wide by
construction. A consumer with no such bound reports no `taken` and is paced
as it is today.

⚠ **A bound the operator has to compute is not acceptable here**, which is
why `taken` is the consumer's own number rather than a raised
`--batch-size`. A batch large enough to span `width + grace` of the *busiest*
store's traffic is data-dependent, silently wrong when the traffic changes,
and exactly the failure this defect already is: measured at 65536 the
undercount only shrinks to 5.5%, because the batch-edge buckets are still
fragments.

### What it also fixes

With the pipeline flowing there is no stall, so the force-seal never fires
mid-stream during a backfill — which removed the displacement outright.

## ⚠ Where this note was WRONG: the quiet tail

The first version of this note said `finish()` should be reserved for true
end of stream and that "the tail of a genuinely quiet log should come out via
`advance()`, which seals through the watermark". **That does not work, and
implementing it is how the hole showed up.**

`advance()` can never seal the bucket the newest entry is IN. Sealing it
needs the watermark past `start + width + grace`, i.e. past event time the
data has not reached — and every entry arriving afterwards is then late and
displaced out of its own bucket, which is the second wrong answer above,
caused deliberately this time. So under that proposal a store's newest bucket
never appears while it is live: a store getting one request a minute would
have shown nothing until the following minute, and the shipped 3-entry VM
test could never have produced a line at all.

**The real defect was narrower than the note claimed.** Force-draining a
bucket is not wrong; force-draining and then EVICTING it is. Each tick then
emitted only what had arrived since the last one, and a reader resolving a
bucket to its newest line took the last fragment for the whole. So:

* a quiet tick states every changed OPEN bucket and **keeps it open**
  (`Drain::Provisional`), so the next line for that bucket carries the
  complete total and supersedes the provisional one — which is what `resolve`
  and `timbergraph.read` already do with two lines for one bucket;
* `Drain::Final` — emit and evict — is for a stream that has genuinely ended;
* an unchanged bucket is not restated, so a quiet store does not write the
  same numbers every two seconds;
* and a provisional line does **not** move the position, because the bucket
  is still open. That falls out of keeping it: `safe_offset` still names its
  oldest byte.

⚠ **Amended by [tally-partials.md](tally-partials.md):** a bucket never held
to completion is never restated, so revisions and provisional buckets both
dissolve. Until that is built, the rule below holds and readers must apply it.

⚠ **A tally tape therefore carries revisions in normal operation**, where
before it only did in principle ("nothing emits a revision", `timbergraph`).
Reading one newest-line-wins per bucket is now load-bearing rather than a
guard against re-running an extractor. Summing every line double-counts.

⚠ **Known cost**: a follower restarted before a bucket seals re-reads it and
states it again, so the tape grows by one line per series per restart until
that bucket closes. Bounded by restarts rather than by time, and every such
line carries the same value, so the resolved answer does not move — verified
across three passes over a one-bucket store.

`advance` did still need fixing, and separately:

* **`advance_to` is clamped to `last_event + grace`.** It was
  `advance(by_ms)`, unclamped, so a stalled backfill's 2-second ticks
  injected 8 s of fabricated event time per 10 s against 2.56 s of real log
  time, the watermark outran the data, and every batch after that was
  displaced whole. The clamp makes it seal exactly the buckets in-order data
  has already left and never the one it is in — which is the same invariant
  the provisional bucket rests on, so there is one rule rather than two. It
  also tracks `last_event` apart from `watermark`, since the latter is no
  longer only event time.

## Found while building: `feed` never said `stream-end`

A one-shot `feed` closed the consumer's stdin and wrote no `stream-end`, and
a records stream without one is TRUNCATED by definition (`records.rs` treats
it as an error, never a short result). So `tally --run` bailed with "record
stream truncated — no stream-end (producer died or pipe broke)", abandoned
the buckets it was holding and exited 1 — **on every clean stop of a tally
follower, and at the end of every one-shot**, losing the tail each time.
Measured before the fix on a 500k-entry store: 468,000 delivered, exit 1.
`feed` now says `stream-end status=exhausted` before closing, but only where
its own loop succeeded: on our own failure the stream really is truncated,
and claiming otherwise would have the consumer seal partial buckets as whole
ones.
* **The `--max` note leaks the follower's internals.** `timberfs: stopped at
  --max 512; more entries matched than were shown` (`query.rs`, reached
  through `ship.rs`) is `query`'s message about a bound the operator never
  typed — in the ship path it is `BATCH_ENTRIES`. One line per batch, i.e. one every
  10 seconds per follower, for as long as it is behind. `feed` already reads
  `stream-end status=limited` programmatically and needs no prose. Suppress it
  on the ship path.
* **`batch_entries` has no knob.** `follower::cmd_run` hard-codes
  `ship::BATCH_ENTRIES`, so there is no way to mitigate any of this on a
  running host. Worth a per-follower setting on its
  own merits, but ⚠ **not as the answer to this defect** — see the bound
  above.

## The hot loop, after the ceiling is lifted

None of these are why tally is slow today; at 51 entries/s they are
invisible. They are what it hits next, and the first two are cliffs rather
than costs. Measured on 500k lines:

1. **`Roller::add`'s series-count scan is O(series-in-bucket), per new
   series.** At 12k series/bucket: 4.16 s capped at 1000,
   **61.7 s uncapped**. The cap hides the cliff and then becomes one of its
   own — a bucket at the cap pays a fresh 1000-step scan for *every*
   subsequent new label set, for ever. Keep a per-`start` series count instead
   of scanning.
2. **`drain(false)` scans every open bucket on every entry, for every
   metric** (`Roller::drain`, from `Run::feed`). Keys are ordered by
   `(start, …)` and `sealed(start)` is monotone in `start`, so the sealed
   buckets are a prefix: `break` instead of `continue` makes it O(1) when
   nothing is sealed. The `marks` loop beside it is the same shape.
3. **`Sink::watermark` repeats that scan per entry** via `safe_offset`. Wants
   a cached minimum, invalidated on add and on drain.
4. **The decode is duplicated across metrics.** Two metrics with the same
   `claim` and the same `decode` — which is what the shipped
   `timberfs-apache-combined` document is — run the claim regex twice and
   build a second `BTreeMap<String, String>` from the same line: 1.09 s for
   one such metric, 1.62 s for two, linear in the count. Memoize the decode
   per decoder per entry inside `Run::feed`.

For the record, since it is the question that prompted the search: **the
regexes are compiled once** and were never a suspect. `Metric::compile` builds
`Source::Extract` at startup, `apache_re` is behind a `OnceLock`, and claims
go through `compile_preds`. Nothing recompiles per entry.

## The test that would have caught it

`tally_provisioning_end_to_end` feeds **3 entries**
(`tests/vm/test-in-vm.sh`) — fewer than one batch. Neither the
throughput cliff nor either wrong answer can appear below 512 entries, so the
test cannot fail on any of this.

The assertion that catches all three at once: feed a store that crosses
several batch boundaries, resolve the resulting tape the way
`timbergraph.read` does, and require the total to equal the number of entries
fed. Per the test-placement rule this is an integration test of the feed→tally
pair rather than a VM test — nothing here needs a VM.

## The numbers collected before the fix

Not salvageable, and not partially salvageable: an undercount of an unknown
factor, and displaced buckets too on a store fast enough to reach that phase,
with markers on only the second. The source stores still hold the lines, so
the remedy is to drop the tally stores and re-derive from a reset position —
which is the backfill property tally was designed for, and is the check the
fix was verified against: the tape a batched pipeline writes over a
500k-entry store is now **byte-identical** to one in-memory `--try` pass over
the same lines, and its resolved `http_requests` total is exactly the 500,000
entries fed.

## Open

* **Where the bound lives if a consumer will not state one.** `taken` makes
  the consumer responsible for what it holds; a consumer that reports `taken`
  and then holds without limit is a memory leak the follower can no longer
  see. A ceiling on `taken - offset` in the follower, refused rather than
  silently paced, is probably right — but it wants a number, and the argument
  against operator-computed numbers above applies to this one too.
* **Whether `progress` should carry it, or a separate record should.** A field
  keeps one message and the omitted-means-equal compatibility; a `taken`
  record keeps `progress` meaning exactly one thing. The field looks right
  because the two numbers are always reported together, but the protocol has
  been kept small deliberately and this is the first field that is not the
  position.
* **Whether a consumer holding state should declare it at hello**, so the
  follower can refuse a `retaining` follower whose consumer re-derives from
  bytes retention may drop. Today the two are unrelated by construction,
  because `offset` is conservative; under `taken` they stop being.
