# A consumer that is HOLDING entries: `taken` beside the position

**Status: not built.** A measured defect in the shipped feed→tally pair, and
the protocol amendment that fixes it. What it rests on is built: the consumer
protocol's `progress` report, the per-store positions, and `Roller`'s sealing
rule. See [consumer-protocol.md](consumer-protocol.md) for the first and
[tally.md](tally.md) for the last — this note amends a claim in each.

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

⚠ **`!late count=512` on a tally store, with 512 the follower's batch size,
is this defect's signature.** It is the one thing an operator can grep for
before the fix lands.

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

With the pipeline flowing there is no stall, so the 10-second force-seal
never fires mid-stream — and both wrong answers go with it. One change, three
symptoms.

## Independently wrong, and worth fixing either way

* **`finish()` is not a mid-stream flush.** It force-drains regardless of
  sealing, and a bucket that later receives more entries simply re-opens at
  the same start and is emitted again — no displacement, no marker, because
  the watermark never crossed `start + width + grace`. That is the unmarked
  undercount above. `finish()` is sound only at true end of stream (stdin
  closed). The tail of a genuinely quiet log should come out via `advance()`,
  which seals through the watermark and therefore marks a later arrival
  `!late` honestly. The cost is that a quiet store's last bucket appears after
  `grace` of quiet — which is what `grace` means, and what a smaller `grace`
  is for.
* **`idle()`'s wall-clock advance must be gated on the live edge.** It is
  documented "For a LIVE reader only" (`Roller::advance`), but it fires on
  any 2-second gap in the record stream — including a backfill stalled by the
  park. That injects 8 s of fabricated event time per 10 s cycle against 2.56
  s of real log time per batch, the watermark outruns the data, and every
  entry after that is displaced. Fixing the park removes the stall that
  triggers it, but the gate is still missing and should be explicit rather
  than left to depend on the park being fast.
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

## The numbers already collected

Not salvageable, and not partially salvageable: an undercount of an unknown
factor in the early minutes and displaced buckets after that, with markers on
only the second. The source stores still hold the lines, so the remedy is to
drop the tally stores and re-derive from a reset position once the fix is in —
which is the backfill property tally was designed for and is the thing worth
checking the fix against.

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
