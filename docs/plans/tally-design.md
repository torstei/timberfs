# The tally design

**Status: design. Nothing of it is built except the block prototype.**

This file is the design; **it is authoritative where the notes beside it
differ.** Those hold the reasoning behind parts of it —
[tally-as-a-tally.md](tally-as-a-tally.md) the format and its measurements,
[tally-series-identity.md](tally-series-identity.md) definitions and ids,
[tally-partials.md](tally-partials.md) the cardinality and durability
arguments.

⚠ Whether a mechanism belongs here is decided by **"It is not a timberfs
store"** below.

## What a tally is

A **grid**: series down one axis, bucket starts along the other, one number
per cell per measure, stored columnar in files in a directory.

A tally is a set of CONCLUSIONS about a log, and a conclusion can be improved
when more of its input arrives — so **a cell is mutable and a value is
corrected in place**, and a file is replaced whole rather than appended to.

**The grid holds numbers.** Anything that states something about a RUN rather
than about a bucket — how many lines a definition claimed and could not
measure, that a sample arrived for a range the store no longer answers for —
is a report, and reports go where an operator reads them.

## On disk

    <store>/manifest.json               identity, retention, the floor, the blocks
    <store>/20260907T000000.g0          one block: a range of buckets, columnar
    <store>/definitions/<offset>.json   the documents in force from <offset>

A **block** is one time range — settled at a day, 1440 buckets of 60s — holding
every series present in it. Header outside the zstd frame so a reader knows
what it covers without decompressing; then a metric table, a series table, a
presence bitmap and delta-coded columns per measure, and one citation span per
bucket. Addressed `(t0, generation)`, which is its filename.

The **manifest** is the commit point and the only thing a reader consults to
find blocks. It holds:

* `v`, `width_ms`, `block_buckets`;
* **its own id** — a block store is a store in its own right, copied and
  referred to;
* **the source store's id** — load-bearing, not provenance: a citation is an
  offset into the source's tape and means nothing without it;
* **the source's labels, copied at creation** — the source may be deleted long
  before a two-year tally, leaving this the only witness of what was measured.
  A record of what the source said then, never a live view;
* **`retain`** in whole blocks, which at the settled range is days, and
  **`retain_size`**;
* **`floor`** — the oldest bucket this store still answers for, which
  separates DROPPED from NEVER WRITTEN;
* one **entry per block**: `t0`, `n_buckets`, `generation`, `bytes`, `crc32`.

⚠ `Manifest` holds `v`, `width_ms`, `block_buckets`, `floor` and the entries;
the identity, the source, the labels and the retention are designed and
unbuilt.

## Writing

The extractor folds entries into buckets and writes them into blocks. A bucket
still filling is in a block like any other; the block is rewritten as more of
its range arrives.

**The write order is `definitions -> blocks -> manifest`**, each step
referenced only by the next, so every crash window leaves unreferenced debris
rather than a dangling reference. A block must never cite a definition id
nothing defines, and the manifest must never name a block that is not there.

A late sample — one whose bucket start is older than the newest — is an
ordinary write to its own bucket. There is no displacement.

## What bounds it, and there are three

| bound | what it stops | where |
|---|---|---|
| **`floor`** | how far back a write may reach | the manifest |
| **`retain` / `retain_size`** | disk | the manifest |
| **memory** | how long a fold holds before spilling | still open |

⚠ **`retain_size` is the backstop that a cardinality cap was standing in
for.** Nothing refuses a series; an unbounded label makes a bigger store, and
the store's size bound drops from the oldest end. A resource bound where
timberfs already puts resource bounds.

## Reading

Two prunings, neither reading a number it does not need: the **manifest** says
which blocks hold a bucket in the window, so the rest are never opened; the
**series table** at the front of each opened block says which series it holds,
so the columns of the rest are stepped over. Measured on a real day: 843 of
992 series matched, 149 stepped over, one block opened.

The **text line format is the INTERCHANGE form and not the storage** —
`--unpack` renders a block back to lines, and every existing reader
(`timbergraph`, `--fold`) keeps working. Values round-trip exactly; a citation
comes back widened, containing what it went in as.

## Definitions

The resolved extractor documents are **copied into the store**, and their ids
are what a block's metric table points at — so "which definition produced this
number" is answerable from the store rather than from whatever is installed on
the host reading it.

* one immutable file per applied set, `definitions/<zero-padded offset>.json`,
  where the offset is the consumer position the set takes effect at. **The
  filenames are the replacement log**;
* ids are **assigned, never positional** — a counter kept with the
  definitions, never reused, because removing a metric is an ordinary edit and
  an index into a list renumbers around the hole;
* the id is on the **metric**, not the series: a metric maps to one
  definition, so a field per series would be many homes for one fact. Absence
  is not definition 0;
* the prefix (`apache:http_requests`) is the **display form** — what a human
  types in a query and what a rendered line shows. Uniqueness rests on the id;
* ⚠ **applying is ONE act.** Stop the follower so its position is durable,
  write the definitions file at that position, start it. Never "apply, and then
  remember to restart something": the interval between two steps is a state
  where the documents say one thing and the numbers are another, and nothing in
  it is wrong enough to notice.

## It is fed sequentially, by a follower

**Settled, with numbers.** The fold reads its source from beginning to end,
once, in one process — the follower's `--run` consumer. The follower is not
merely the delivery mechanism: it holds the position durably, holds the
source's retention back while the tally is behind it, supplies the registry
and the systemd lifecycle, and restarts into exactly the re-fold `safe_offset`
makes correct.

⚠ **Parallel workers were considered and are not the first thing to reach
for.** Two facts:

* **It needs additive partials**, which is their THIRD use after spilling and
  repair — two workers can both contribute to one bucket, and with a merge
  that replaces a cell the second silently erases the first. And it must
  partition by SOURCE POSITION rather than by day: a day's entries are not
  contiguous in the source (late arrivals are why a citation exists at all),
  and there is no per-chunk logline range to find them by
  ([logline-order.md](logline-order.md)). Partitioning by chunk range falls
  out for free, a worker's consumed range being the partial identity already
  wanted.
* **The gain is small.** Measured over 300,000 real lines: reading records
  0.52 s, the fold 4.53 s, writing blocks 0.05 s — so blocks are 1% and the
  fold is 90%. A day is ~47 s single-threaded, and the largest backfill that
  can ever be asked for is the SOURCE's retention (weeks, not years, since
  nothing can tally what was dropped), so a 30-day rebuild is ~24 minutes
  once.

⚠ **And the obvious fold optimisation is not there either**, which is worth
recording so it is not re-proposed: the four metrics of the measured document
each run their own extract regex over every line, but cost is spread evenly
and the metric with the SMALLEST regex (17 characters) is joint-most expensive
at 1.31 s, because it is a histogram of 22 buckets and turns one line into 22
samples. The cost is per sample produced and per metric evaluated, not per
regex byte, so sharing the extraction wins much less than it looks.

## Recovery is re-derivation

A follower's position is precious because it shipped bytes it cannot un-ship.
**A tally has no such problem**: its output is a function of source entries
still on disk, so it discards what may be incomplete and recomputes. A
position is an EFFICIENCY device — do not re-read 700 GB at every restart —
and not a correctness one.

⚠ It is exact because a sample's bucket is decided by its own stamp and
nothing else. Nothing consults a watermark, so a re-derivation does not depend
on where the read started.

⚠ **And a record stream is forward-only, which is fine for a CRASH and not
for a repair.** The tally is fed a stream and cannot ask for bytes again, so
the two cases separate:

* **A crash needs no seek**, because of `Roller::safe_offset` — "the oldest
  source byte any OPEN bucket still depends on. A consumer may not report past
  this". The position is therefore always behind every unfinished bucket, so a
  restart re-sends from before that bucket's FIRST entry, re-folds it whole,
  and the complete total replaces the partial one. ⚠ **This is the invariant
  the block writer's `How::Merge` rests on**, and it is invisible from the
  writer: tighten `safe_offset` to advance further — and it sat in the middle
  of the 51-entries/s deadlock, so it has been optimisation-bait once already
  — and blocks are corrupted by code that never mentions them. ⚠⚠ **And that
  conservatism is itself a tape-shaped leftover, not a law**: it is a
  consequence of merge REPLACING a cell. Additive partials
  ([tally-partials.md](tally-partials.md)) let the position advance freely,
  which is the same knot as the 51-entries/s deadlock seen from the storage
  end. So it is load-bearing today and should not be written into the design
  as permanent.
* **A repair does need to go back, and a REWIND is the wrong way to do it.**
  A tally is not only a consumer: `query --records --from X --to Y | tally`
  is a bounded DIRECT read of the source, and it already exists. So "the regex
  was wrong, recompute last week" is a one-shot pass over that window while
  the follower keeps going forward — no position is moved, and the live path
  is never interrupted. ⚠ What it needs is `How::Regenerate` rather than
  `Merge`, which the writer does not expose: merging a recomputation into
  what is there would replace cell by cell and leave any series the new
  definition no longer produces standing.

## Copying is a file sync or a bundle

Blocks that are immutable once past the floor, under a manifest with a crc32
per entry, are copied correctly by a file sync — and `read_block` verifies that
crc, so a copied store self-validates on read.

**Copy rather than re-derive**, though both are cheap (a day is 2.38 MB of
block against 780 MB of source, and re-deriving it costs ~9 s of CPU): a copy
gives ONE answer, where a receiver folding with a different version of a
document disagrees with the sender silently.

A hand copy needs two rules: **blocks first, manifest last** (the reverse
leaves a manifest citing files that are not there), and a deleting sync must
delete after. ⚠ **A bundle beats a directory sync** — `export`'s `.timber`
shape — because a single file takes its atomicity from the container, so the
ordering rule stops being load-bearing.

## ⚠ It is not a timberfs store

A tally store is a directory of files with a manifest, not a `.bark`/`.trunk`
pair, and a tape's mechanisms do not belong in it.

**The property that decides it: a tape gets ONE write per bucket.** It is
append-only text, so a bucket is stated once and cannot be revisited — which
is what sealing, `grace`, displacement and revisions serve. A block is a file
replaced whole by temp-and-rename, so that premise never holds here and those
mechanisms answer a question this design does not have.

⚠ **So ask of a mechanism what it assumes.** If it assumes one write per
bucket it is a tape's, and the properties above cover the case: a cell is
mutable and corrected in place, a sample goes in its own bucket, a filling
block is a block, the floor bounds a rewrite, nothing refuses a series, and
the grid holds numbers while statements about a RUN are reports.

## What is measured

One real day of a real site's performance log, four metrics over one line
shape: 32,259,142 bytes of tally text, a `.trunk` of 5,154,486, and a block of
939,559 plus a 228-byte manifest — **5.5×**, and 0.69 GB against 3.76 GB over
a two-year retention. Citations cost 7,542 bytes of that block, **0.8%**. Six
contiguous days show a day-sized block costing +4% against a six-day one, and
a working set that saturates rather than drifting.

## What is not settled

* **the spill trigger and the memory ceiling** — bytes held is the honest
  measure, and a cgroup limit needs no new knob;
* **a third identity component**, so several writes for one range can coexist
  rather than rewriting a name the manifest already points at. `Manifest::put`
  retains one block per `t0` today, which is right for a generation and wrong
  for a partial — and it is also the atomicity defect below;
* **compaction's schedule**, and whether a query merges or refuses;
* **the write-batching mechanism** — a WAL for samples in the `.sap` shape is
  the candidate, since one late sample otherwise rewrites a whole day block;
* **a repair pass** — a bounded read with `How::Regenerate`, which is what
  makes "fix the definition and recompute" an operation rather than a plan.
  ⚠ Not a rewind of the follower: the direct read already exists, and moving
  a live position to recompute history would stop the live path to fix the
  past.

⚠ **A defect that exists today:** `commit` renames a block into place and THEN
saves the manifest, so a rewrite at the same `(t0, generation)` leaves a window
where the file is new and the manifest records the old crc32 — and
`read_block` fails on exactly that. Latent, nothing reading blocks yet, and
fixed by the third identity component above.
