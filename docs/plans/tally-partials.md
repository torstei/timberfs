# Partial aggregates: never holding a bucket to completion

**Status: design, nothing built.** It removes a knob rather than adding one.
Its second half is about recovery, and the load-bearing claim there is that a
tally can always go back to the source while the chunks are there — so a
position is efficiency and a re-derivation is the answer to a crash. It amends
three notes: [tally.md](tally.md) (grace and displacement as the
lateness mechanism), [consumer-holding.md](consumer-holding.md) (the
provisional bucket and the revision rule it introduced), and
[tally-as-a-tally.md](tally-as-a-tally.md) (the block's cell merge, which is
replace and would have to add).

⚠ **The design is STATED in [tally-design.md](tally-design.md); this note is
one of the arguments that reached it.** Where the two disagree, that one is
right. Read its "What a tally is NOT" before reintroducing anything here.

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
- **Displacement.** An entry whose bucket has sealed is shoved into the
  current one because a sealed bucket cannot be reopened. A write for an older
  bucket is ordinary, and `!late` goes with it — see below.
- **Revisions and provisional buckets.** The newest-line-wins rule exists so an
  incomplete bucket can be restated. A bucket never held to completion is never
  restated — which is the elegant answer to "I had not completed that bucket
  after all", by dissolving the requirement rather than serving it.
- **The SEAL, and with it the open region.** See its own section below: it is
  the tape's answer to "you get one shot at this bucket", and a block has no
  such constraint.

One mechanism instead of six is the argument that the cap was a symptom.

## What it costs

Cost moves from write to read. A bucket may exist as several partials until
something merges them, so **compaction stops being optional** and the read path
merges forever. That is the bill to measure before committing.

⚠ **And additive partials are not idempotent.** This is the sharp edge: a
revision is a replace, so applying it twice is harmless, while applying an
additive partial twice double-counts (`Min`/`Max` survive it, `Count`/`Sum` do
not). So everything that can re-deliver one needs an identity for what it
accounts for — a re-fed range, a retried ship, a replayed replication stream.

⚠ **Crash recovery looks like that same problem and is not**, which took two
passes to see. Today it is free *because* the tape replaces: a position is only
advanced on a progress report, so a restart re-reads bytes already folded, and
restating those buckets is harmless. Additive partials remove that safety net —
but a tally can re-derive what it is unsure of, so recovery is answered by
recomputation rather than by reconciling partials. What genuinely needs solving
is narrower: the representation (a set of blocks, never a running total) and
identity for replication. The two sections below.

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
store's tape so retention cannot move it. **So what a partial accounts for is
the offset range it consumed**, and the quantity needs no inventing — it is the
number the consumer protocol already persists. What that range is FOR is the
subject of the two sections below, and it is not what I first took it for.

## Recovery is re-derivation, not reconciliation

A generic follower must never re-read: it shipped bytes somewhere and cannot
un-ship them, so its position is precious. **A tally is not that**, and this is
where being special rather than being a follower pays: its output is a function
of source entries that are still on disk, so it can always go back. The
position is an EFFICIENCY device — do not re-read 700 GB at every restart — and
not a correctness one.

So the crash case needs no dedup. Discard the buckets that may be incomplete,
re-derive them from the source, and replace: `How::Regenerate` is already that
operation and is already idempotent, because a replace applied twice is a
replace. Reconciling what was half-applied is work that never has to be done.

**And this is the job the cite is for.** As a rewind point it is exactly right
where it was useless as a dedup key: "a range to READ" is literally what a
rewind point is, its widening is conservative in the safe direction — rewinding
too far costs work, never correctness — and being switchable off costs a
coarser rewind (the retention floor, say) rather than a wrong answer. Both
properties that disqualified it above qualify it here.

⚠ **Re-derivation is exact only if the fold is a pure function of the source,
and TODAY IT IS NOT.** Displacement puts a late entry in the *current* bucket,
and which bucket is current depends on the watermark, which depends on where
the read started; sealing depends on it too. So the shipped fold is
reproducible only from the same start offset — which is why the 0.33.0
verification compared a batched pipeline against one in-memory pass over the
same store, and not against a re-derivation from elsewhere. Removing
displacement, which partials do, is what makes re-derivation exact. **Purity is
a property this design gains, not one it assumes**, and it is a further
argument for it rather than a precondition.

## There is no seal, and the bound is the floor

⚠ **This section corrects something an earlier draft of this note said**:
that `grace` stops being a correctness boundary full stop. It stops being a
write-once boundary; what replaces it as the bound on rewriting is below, and
it is not `grace`.

**What sealing was for.** A tape is append-only text, so a bucket gets ONE
line and you cannot go back — you must know it is complete before writing it.
That is what `grace` buys. A block has no such constraint: it is a file,
replaced by temp-and-rename, addressed so a range can be re-stated. The
premise sealing answers does not exist here.

**And the seal manufactured the open region.** A block was assumed immutable,
so the still-filling edge needed somewhere else to live — a separate file, a
separate read path, and the only file in the store whose bytes nothing
vouches for. Drop the seal and a filling block is just a block that gets
rewritten. `checkpoint`, `read_open` and `Roller::open_buckets` go with it.

**So what stops a block changing?** ⚠ Not the source's retention, which was
this note's first answer and is wrong in the direction that matters: retention
drops the OLDEST source bytes, while a late entry comes from the newest end —
an entry the tally has not read yet, carrying an old stamp. Source retention
bounds RE-DERIVATION (what
[tally-series-identity.md](tally-series-identity.md) means by irreplaceable)
and bounds lateness not at all.

**The bound is the tally's own `Manifest::floor`** — already there, already
"a claim about what this store no longer answers for". A late entry has two
fates and no third: its block still exists, so the block is rewritten and the
numbers get BETTER; or it is below the floor, so there is nothing to write
into and it is a bounded loss.

⚠ **And that loss is REPORTED, not stored.** A marker is keyed by bucket, so
recording one below the floor would mean storing a fact about a bucket the
store has forgotten, in a store organised entirely by range. So there is no
`!late` in this design: a sample whose block still exists needs no annotation
(the number is simply right), and one below the floor has nothing to annotate.
What the arrival says is operational — the producer is later than this store's
retention — and it belongs in a run-level count or a consumer `note`, where a
fact about a run belongs.

**Which leaves NO markers at all**, and that is the answer to "where do
markers live in a block": they do not. `!meta` is subsumed by the definitions
travelling with the store; `!cap` goes with the cap; `!late` goes with
displacement.

⚠ **`!drop` goes too**, which took three passes to see. It counts lines a
metric CLAIMED and then could not read — not lines that failed to match, which
are skipped and counted nowhere. So it exists only where a definition claims
more than it can measure: a capture loose enough to match a non-number, an
optional group that did not participate, or a decode source where a claimed
line does not carry the measured key. A self-inflicted category, and a number
that appears only when the definition is wrong does not belong in the storage
format.

The distinction between "not mine" and "mine but unusable" is a DIAGNOSTIC,
and it already has a home: `--try` reports claimed/skipped/dropped/observations
per metric against a sample, which is where a loose capture is found and fixed.
⚠ And the general alarm beats the specific one — a producer whose format
changes makes the metric FLATLINE, which is the signal either way, and if the
claim stops matching too there are no drops at all. So `Outcome::Dropped` and
the `Seen` counters stay as accounting; what goes is `note_drop` writing a
sample into the tally.

⚠ This is a decision about the BLOCK design. The tape writes `!drop` today and
that shipped in 0.33.0; whether to stop it there is a separate call, and while
the tape is being superseded the answer is probably no.

⚠ **And no imposed lateness limit should be added**, tempting as it is for
making part of the store stable. The trade would be a number that is stable
but knowingly incomplete against one that improves when late data arrives, and
for a tally the second is plainly right — correcting a number in place is the
whole reason a mutable cell was the point. Stability is not a property anything
here needs; a flag claiming a range is "done" would be a claim about the
future.

**What survives of `grace` is write batching**: how long to hold before
writing, which decides how often a block is rewritten. Performance, not
correctness. ⚠ And the cost is real — a single late entry forces
read-merge-rewrite of a whole day block, ~917 KB measured. The answer to that
is to BATCH late arrivals, which is what a WAL for tally samples would be for
(the `.sap` shape, one level up): append cheaply, fold into blocks in batches.
Never to bound them, which would pay in truth.

⚠ **A defect this makes normal rather than exceptional.** `commit` renames the
new block into place, THEN saves the manifest, so between the two the file is
the new bytes and the manifest records the old crc32 — and `read_block` fails
with "the block changed under the manifest, which a sealed block may not do".
A new block and a regeneration are both safe, their names being unreferenced
until the manifest names them; the window belongs to a rewrite at the same
`(t0, generation)`, which was the exception and is now the common case. The fix
is the third identity component this note already asks for, so that a write
never lands on a name the manifest already points at.

## What that leaves for the consumed range

⚠ **A partial must never be folded on receipt.** Keep partials as blocks in a
set and merge at read or compaction time: re-applying one is then re-inserting
the same member of a set, which is idempotent, and the arithmetic
non-idempotence above only ever bites a receiver that accumulates. That leaves
replication needing IDENTITY and no arithmetic guard — and a byte-identical
retry is already recognisable from `(t0, generation)` with `bytes` and `crc32`.

So the consumed range's real job is not dedup. It is **block identity**, so
that several partials of one range can coexist at all: `Manifest::put` retains
only `had.t0 != e.t0`, one block per `t0`, which is right for a generation and
makes a second partial of a range impossible. Identity needs a third component,
and "which slice of the source this accounts for" is the honest one.

**Which also frees `generation` of a second job.** A generation is a
RE-DERIVATION — a definition changed, a range recomputed — and it *replaces*. A
consumed range says which slice of the source a partial accounts for, and it
*accumulates*. Two axes, neither overloaded: a partial expressed as a
generation would make `put` mean replace or accumulate depending on which field
moved.

## Four things that block it today

Facts about the code, found while building the prototype rather than while
implementing this:

1. **`Manifest::put` allows one block per `t0`.** It retains only
   `had.t0 != e.t0`, so putting a second partial of a range evicts the first
   and returns it as superseded — correct for a generation, and fatal for a
   partial. The narrowest and most concrete of the four.
2. **`Block::merge` is replace, not add.** It writes `mine[b] = *v` for every
   present cell, so it accepts a restatement and would silently take the last
   partial as the answer. Additive merge, per `Field`, is new code.
3. **`Sample::render` drops the timestamp on `last=`** (above), so `Last` is
   unmergeable as the tape stands.
4. **Add-versus-replace is not expressible.** `Block::pack` already carries
   `How::Merge` and `How::Regenerate` because inferring a regeneration from
   "I already hold this range" silently superseded three of six days of real
   data. The same distinction has to be explicit wherever a partial travels,
   at cell granularity, and never inferred.

## What it does to the other threads

**Re-generation is unchanged.** `How::Regenerate` stays a replace scoped to
`(t0, generation)` — which is now also what keeps re-derivation idempotent
while partials are not.

**Replication is not being built** — see the section below, which is the
decision rather than a deferral.

**The marker question gets easier.** `Block::pack` refuses markers today
because where they live is unsettled, and `!cap` was the marker that could not
be dropped — it declares the numbers understated. With no cap there is no
`!cap`, no `!late` and no `!drop` either — so there is nothing left to place,
and `Block::pack`'s refusal of markers is right by design rather than a
placeholder. A marker states something about a RUN; the grid holds numbers.

## Why there is no replication

Decided rather than deferred, on the numbers. Measured on one real day: the
source log is 780 MB over 2.7M lines, the tally it produces is 30.8 MB of text
and **2.38 MB as a block** — 0.3% of its source — and re-deriving it costs
about 9 s of CPU at the extractor's measured 310,000 entries/s. Both shipping
and recomputing are so cheap that cost cannot decide, so the failure modes do.

**Two use cases, and neither needs a protocol.**

*Somewhere else with a longer retention.* Either re-derive there from the
source that was shipped anyway, or copy the files. Copying is the safer of the
two because it gives **one answer** rather than two: a receiver folding with a
different version of a document produces numbers that silently disagree with
the sender's, which is the hazard
[tally-series-identity.md](tally-series-identity.md) exists about. And copying
needs nothing built — the format is immutable blocks named `(t0, generation)`
under a manifest with a crc32 per entry, which `read_block` already verifies
and refuses on mismatch, so **a copied store self-validates on read**.

*Reading the tally somewhere local, for graphs.* That is a query, and it
already works.

⚠ **This is a property the block format bought, and it cost something to buy.**
A tally store TODAY is an ordinary timberfs store, so the existing frames
machinery already ships it; the block format takes that away. What it gives
back is a directory a plain file copy handles correctly, which an append-only
tape cannot be — the live edge is why frames exists at all.

**The one case where the bytes are irreplaceable** is worth naming, because it
is not either of the above: a tally kept for two years against a source kept
for weeks cannot be re-derived once the source ages out, anywhere. That argues
for BACKUP, and a backup is a file copy too.

### What the invariant rests on

Regenerating centrally is sound because the references run one way — tally to
source, never back. Verified at the four places it could fail: the tally store
inherits its source's provenance and cites source offsets; a store's manifest
names no consumer; a follower's `positions.json` lives in the follower's own
directory rather than the store's; and `retain_unconsumed`, the only place a
store depends on consumers at all, is resolved by `TickInterest::floor` reading
the follower REGISTRY, so followers find the store by selection and the store
holds no reference to any of them.

⚠ But `retain_unconsumed` travels in a shipped manifest. It is inert where no
follower is registered, and pins retention where one is registered and stalls —
bounded only by the `retain_size` that bark requires alongside it for this
reason.

### Until then, the copy is documented rather than provided

A `timberfs tally push`/`pull`/`sync` is where the ordering and any locking
belong when this is wanted. Doing it by hand needs two rules:

- **Blocks first, manifest last.** The manifest is the commit point, so the
  reverse order leaves a receiver holding a manifest that cites files it does
  not have. It is the same order the local commit already follows — superseded
  blocks are unlinked only AFTER the manifest is saved — and a deleting sync
  must delete after the transfer for the same reason.
- **Do not copy `open`.** It is rewritten in place, so a copy can be torn.
  Sealed history copies; the live edge is queried.

⚠ **Better still, do not sync a directory at all — send a bundle.** `export`
already writes one for a store: "a plain uncompressed tar (the payload is
already zstd)", and the same shape fits a block set. It is not merely tidier.
A single file takes its **atomicity from the container**, so a torn
intermediate state is unobservable and the ordering rule above stops being
load-bearing — the bundle's own members are ordered to suit the READER
(`export` puts the tiny `.rings` before the `.trunk` "so readers see the index
before the data"), which a directory sync cannot afford to do. So the rule
above is what a hand copy needs, and the bundle is what the command should
provide: a hazard made structurally impossible beats a hazard documented.

## Open

- **The spill trigger.** Bytes held is the honest measure, and sizing a bucket
  map is approximate — `String` labels, a `BTreeMap` per cell, allocator slack.
  Approximation is affordable here precisely because being wrong costs write
  amplification rather than truth; it would not have been under a cap.
- **Where the MEMORY ceiling comes from.** A process can read its own cgroup
  limit and target a fraction of it, which needs no new knob and tracks
  whatever the unit was given. Whether that is better than one number in
  `limits.conf` is unsettled; both are answerable, unlike a series count.
  ⚠ Two different ceilings, and only this one is open: how much MEMORY before
  spilling is a performance knob, while what stops an unbounded label filling
  the DISK is `retain_size` on the store, settled in
  [tally-as-a-tally.md](tally-as-a-tally.md) — dropped from the oldest end,
  where timberfs already puts resource bounds. An earlier argument in this
  note for "bound the resource, not the count" conflated the two.
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
- **What "may be incomplete" means on restart**, since that is the set a
  recovery re-derives. The conservative answer is every bucket the newest
  partial touches; a cheaper one needs a durable statement that a bucket is
  closed, which is a claim about the future and therefore a `grace` in
  disguise.
- **Gauges.** If `Last` keeps its timestamp on the wire, nothing is held to
  completion. If not, gauge series need their own bound, and the argument
  against a cap applies to them unchanged.
