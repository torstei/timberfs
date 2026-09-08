# Partial aggregates: never holding a bucket to completion

**Status: design, nothing built.** It removes a knob rather than adding one,
and its second half is a design for CRASH RECOVERY, which turns out to be the
same problem — see "Provenance is not coverage". It amends three notes: [tally.md](tally.md) (grace and displacement as the
lateness mechanism), [consumer-holding.md](consumer-holding.md) (the
provisional bucket and the revision rule it introduced), and
[tally-as-a-tally.md](tally-as-a-tally.md) (the block's cell merge, which is
replace and would have to add).

## The question that has no answer

`window.max_series` asks a document's author how many distinct series a bucket
will hold. Nobody can answer it. The author ships a package and does not watch
the host; the label sets grow at every deploy and every new customer; and the
traffic that decides it has not happened yet. It is a prediction, so moving it
to a site file changes who guesses, not whether it is a guess.

Three things make that concrete.

**The instinct reaches for the wrong quantity.** The number that is easy to
count is the day's distinct series; the cap sees the count *within one bucket*.
Measured over six contiguous real days of one site's performance log — 8,036
buckets, four metrics over one line shape:

| | max per bucket | mean per bucket | the day's union |
|---|---|---|---|
| widest label set (class + method) | **367** | 101 | 843 |
| tenant view | **98** | 41 | 119 |
| pool view | **6** | 3.9 | 7 |
| latency histogram | **1** | 1.0 | 23 |

Sizing on the union over-states by 2.3× on the widest view and by 23× on the
histogram. The document that produced this sets 4,000 — eleven times the real
worst bucket, which is evidence the field is decorative rather than tuned.

**The knob does not bound what it exists to bound.** It is enforced per
`(metric, bucket-start)` by `Roller::add`, and one `Roller` is built per metric
in `Metric::compile`. With a 60s width and 120s grace, `Roller::sealed` leaves
three bucket-starts open at once, so four metrics at 4,000 permit **~48,000
live series** — a number no file states. No unit in `packaging/` sets
`MemoryMax`, so this count is the only thing between a runaway label and the
host.

**Being wrong is silent and irreversible.** At the cap a *new* series is
dropped and `Marks::capped` counts the refused samples; series already in the
bucket stay correct. So the numbers written are understated, not wrong — but
which series survive is arrival order, re-rolled every bucket, so a capped
metric's series set flickers and its totals sag with nothing per-series saying
so. The `!cap` marker names the metric and a sample count, never which series
was lost, and it reaches the tape rather than the operator: nothing warns at
write time. ⚠ It is also slowest exactly when it is losing — a capped series is
never inserted, so every subsequent sample of it repeats `Roller::add`'s full
bucket scan and is then thrown away.

## Where the number comes from

Not from a hazard. From one decision: **a bucket is accumulated in memory until
it is complete, then written once.** Every ceiling is downstream of that.

A database would not have made it. A hash aggregate whose state outgrows its
budget spills to disk and merges the runs; PostgreSQL 13 added exactly that to
`HashAgg`, and what it removed was the need to *estimate cardinality in
advance* — being wrong about that estimate was the failure mode. `work_mem`
survives as a tuning knob whose violation costs I/O rather than correctness. An
LSM memtable that hit its size limit and started dropping keys instead of
flushing would be a broken database, and that is what `max_series` does.

## Why memory is not forced

Two properties already hold.

**The input is durable and replayable.** The fold reads a timberfs store with a
recorded position, so its accumulator was never precious: anything not yet
written can be re-derived by rewinding.

**The fold is already an associative reduce.** `Field::combine` is the merge
operator: `Count` and `Sum` add, `Min` and `Max` compare, and a histogram is
`Count` per `le`. So a partial aggregate is arithmetically legal — a bucket may
be written as several partials and combined later, and the result equals the
single-pass one.

⚠ **`Last` is the exception, twice over.** It is a gauge, so it combines by
carried timestamp rather than by value — `Field::combine` does this, holding
each field as `(value, ts)` and giving ties to the later arrival. But
`Sample::render` writes `last=<value>` and drops the timestamp, so **two
partials' `Last` cannot be merged from a tape line.** Either the wire gains
that timestamp or gauge series stay the one thing held to completion.

## What the design becomes

Spill when memory is tight — whatever tight happens to be that day — and merge
on read. Cardinality then costs **I/O and disk**, which is the resource
timberfs already governs by size through `retain_size` and head-drop: a runaway
label produces a large tally store that retention trims, instead of silent
understatement or an OOM. The threshold that remains is a spill point, and
being wrong about it costs write amplification, which is measured afterwards
rather than predicted beforehand.

What goes away with it:

- **`max_series` and `!cap`**, and the question of who sets them.
- **`grace_ms` as a correctness boundary.** It exists so a bucket gets exactly
  one line. As a latency and write-amplification choice it needs no correctness
  argument.
- **Displacement and `!late`.** An entry whose bucket has sealed is shoved into
  the current one because a sealed bucket cannot be reopened. A partial for an
  older bucket is an ordinary append.
- **Revisions and provisional buckets.** The newest-line-wins rule exists so an
  incomplete bucket can be restated. A bucket never held to completion is never
  restated — which is the elegant answer to "I had not completed that bucket
  after all", by dissolving the requirement rather than serving it.

One mechanism instead of five is the argument that the cap was a symptom.

## What it costs

Cost moves from write to read. A bucket may exist as several partials until
something merges them, so **compaction stops being optional** and the read path
merges forever. That is the bill to measure before committing.

⚠ **And additive partials are not idempotent.** This is the sharp edge: a
revision is a replace, so applying it twice is harmless, while applying an
additive partial twice double-counts (`Min`/`Max` survive it, `Count`/`Sum` do
not). So everything that can re-deliver one needs an identity for what it
accounts for — a re-fed range, a retried ship, a replayed replication stream.

⚠ **Crash recovery is that same problem, not a separate one.** Today it is free
*because* the tape replaces: a position is only advanced on a progress report,
so a restart re-reads bytes already folded, and restating those buckets is
harmless. Additive partials remove exactly that safety net. So a design for
partials is a design for recovery, and the note below is both.

The block manifest addresses a block by **`(t0, generation)`** — `Manifest::put`
supersedes on `had.t0 == e.t0` alone, and `Entry`'s own comment says "the
address is `(t0, generation)`, never a position in a sequence"; `n_buckets` is
coverage that `covering` filters on, not identity. That address is enough to
recognise a duplicated *generation*, and not enough for a partial, because two
partials of one bucket must coexist rather than supersede. **A tape line has no
identity at all**, which is the argument for the grid being where partials
live.

## Provenance is not coverage

Two byte ranges live in this design, and conflating them would be the third
silent defect in this format rather than the first.

| | what it is | exact? | optional? |
|---|---|---|---|
| a **cite**, `(offset, len)` per bucket | where to READ to see the lines behind a number | no — a widened union | yes |
| a **position**, `cursor::At::offset` | where to RESUME; everything before it is accounted | yes | no |

A cite is provenance, and `Sample::cite` says so: *"the span of source-store
tape this counted... **a range to READ, not the set of entries** — a rule
matching one line in a hundred cites the ninety-nine between them."* `Roller`
widens it to the min and max over the bucket's series, and a block widens it
again to one span per bucket. So it cannot serve as a resume point three times
over: the span contains bytes that fed *other* buckets, bytes outside it may
have been read and folded elsewhere, and a source offset is **not monotone in
event time** — which is the very reason `!late` exists — so no offset partitions
the buckets into done and not-done. ⚠ And it is switchable: `Metric::cite`
defaults to true and can be set false, so recovery resting on it would stop
working silently when someone turned provenance off.

**The exact quantity already exists**, and `cursor::At` already draws this
distinction in its own comment — "TWO positions, because they answer different
questions and neither can answer the other's: the offset is where to RESUME,
exact and valid inside the write-ahead segment; this is the RETENTION FLOOR,
chunks strictly below it being fully consumed". The `offset` is absolute on the
store's tape so retention cannot move it. **So a partial's identity is the
offset range it consumed**, and nothing new needs inventing — it is the number
the consumer protocol already persists.

⚠ **An interval is not enough; the ranges must TILE.** Dedup by overlap does
not survive the crash it exists for: a partial covering `[0,1000)` is applied,
the process dies before its position is durable, the fold resumes at 500 and
produces `[500,1500)` for the same bucket. It can neither be added (500–1000
counted twice) nor dropped (1000–1500 lost). The rule that works is that a
partial always begins where the previous one ended, so two partials of a bucket
are **either identical — drop it — or disjoint — add it**, and dedup is an
equality test rather than an interval calculus.

That in turn requires **the position to be committed with the partial, in one
write**, not beside it: if the two can diverge the tiling breaks, and a
diverged pair is exactly what a crash produces. The manifest's temp-and-rename
is already that commit point, which is the second argument for the grid.

**And `generation` stops doing two jobs.** A generation is a RE-DERIVATION — a
definition changed, a range recomputed — and it *replaces*, which is what
`Manifest::put` implements. A consumed range says which slice of the source a
partial accounts for, and it *adds*. Two axes, neither overloaded: a partial
cannot be expressed as a generation without making `put` mean both replace and
accumulate depending on which field moved.

## Three things that block it today

Facts about the code, found while building the prototype rather than while
implementing this:

1. **`Block::merge` is replace, not add.** It writes `mine[b] = *v` for every
   present cell, so it accepts a restatement and would silently take the last
   partial as the answer. Additive merge, per `Field`, is new code.
2. **`Sample::render` drops the timestamp on `last=`** (above), so `Last` is
   unmergeable as the tape stands.
3. **Add-versus-replace is not expressible.** `Block::pack` already carries
   `How::Merge` and `How::Regenerate` because inferring a regeneration from
   "I already hold this range" silently superseded three of six days of real
   data. The same distinction has to be explicit wherever a partial travels,
   at cell granularity, and never inferred.

## What it does to the other threads

**Re-generation is unchanged.** `How::Regenerate` stays a replace scoped to
`(t0, generation)` — which is now also what keeps re-derivation idempotent
while partials are not.

**Replication is unaffected in shape and gains one requirement.** It is still a
manifest diff; a partial is simply another block. But the receiver must dedup
rather than append what it is handed: by `(t0, generation)` for a generation,
and by the consumed offset range for a partial. A sender that retries is the
ordinary case, not the exceptional one, so this is a requirement on the
protocol and not a repair for it.

**The marker question gets easier.** `Block::pack` refuses markers today
because where they live is unsettled, and `!cap` was the marker that could not
be dropped — it declares the numbers understated. With no cap there is no
`!cap`, and `!late` becomes provenance rather than a correction, so what
remains to place cannot corrupt the grid by being lost.

## Open

- **The spill trigger.** Bytes held is the honest measure, and sizing a bucket
  map is approximate — `String` labels, a `BTreeMap` per cell, allocator slack.
  Approximation is affordable here precisely because being wrong costs write
  amplification rather than truth; it would not have been under a cap.
- **Where the ceiling comes from.** A process can read its own cgroup limit and
  target a fraction of it, which needs no new knob and tracks whatever the unit
  was given. Whether that is better than one number in `limits.conf` is
  unsettled; both are answerable, unlike a series count.
- **Compaction's schedule**, and whether a query merges partials or refuses to
  answer from an uncompacted range.
- **Where a partial's consumed range is written.** `Entry` would carry it, and
  it must be committed with the block rather than in the positions file, which
  is a separate write that can diverge. Whether `Positions` then becomes
  derived from the manifest — the manifest holding the truth and the positions
  file a cache of it — or the two stay independent with the manifest winning on
  disagreement, is unsettled.
- **How a partial's range is bounded.** Memory pressure does not respect chunk
  boundaries, so a partial may end mid-chunk; the position is exact there
  (`At::offset` is valid inside the write-ahead segment) but the retention
  floor is chunk-granular. Whether a spill may end anywhere or must reach a
  chunk boundary is a choice between write amplification and a simpler floor.
- **Gauges.** If `Last` keeps its timestamp on the wire, nothing is held to
  completion. If not, gauge series need their own bound, and the argument
  against a cap applies to them unchanged.
