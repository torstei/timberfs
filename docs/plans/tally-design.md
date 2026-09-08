# The tally design, as agreed

**Status: design. Nothing of it is built except the block prototype.** This is
the STATEMENT; the three notes beside it are the arguments that reached it and
are kept for their reasoning, not their conclusions —
[tally-as-a-tally.md](tally-as-a-tally.md) (the format, measured),
[tally-series-identity.md](tally-series-identity.md) (definitions and ids),
[tally-partials.md](tally-partials.md) (why the cap, the seal and the open
region are gone). Where any of them disagrees with this file, this file is
right and that one has an amendment marker.

⚠ **Read "It is not a timberfs store" before adding a mechanism here.** The
failure mode this document exists to prevent is reaching for a tape's answer
because it looks like the obvious way to do things — and the test for one is
in that section.

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

⚠ Of those, only `v`, `width_ms`, `block_buckets`, `floor` and the entries
exist in `Manifest` today. The identity, the source, the labels and the
retention are designed and unbuilt — do not read this section as a description
of the struct.

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

## Recovery is re-derivation

A follower's position is precious because it shipped bytes it cannot un-ship.
**A tally has no such problem**: its output is a function of source entries
still on disk, so it discards what may be incomplete and recomputes. A
position is an EFFICIENCY device — do not re-read 700 GB at every restart —
and not a correctness one.

⚠ Which is exact only because displacement is gone: which bucket was "current"
depended on the watermark, and so on where the read started. Purity is a
property this design GAINS.

## Copying is a file sync or a bundle

Blocks that are immutable once past the floor, under a manifest with a crc32
per entry, are copied correctly by a file sync — and `read_block` verifies that
crc, so a copied store self-validates on read. Measured: a day is 2.38 MB of
block against 780 MB of source, and re-deriving it costs ~9 s of CPU, so
neither cost decides and the failure modes do. Copying gives ONE answer where
re-deriving gives two, a receiver folding with a different document version
disagreeing silently.

A hand copy needs two rules: **blocks first, manifest last** (the reverse
leaves a manifest citing files that are not there), and a deleting sync must
delete after. ⚠ **A bundle beats a directory sync** — `export`'s `.timber`
shape — because a single file takes its atomicity from the container, so the
ordering rule stops being load-bearing.

## ⚠ It is not a timberfs store

That is the whole warning, and it is enough. A tally store is a directory of
files with a manifest; it is not a `.bark`/`.trunk` pair, and the mechanisms
that belong to one do not belong here.

**The property everything follows from: a tape gets ONE write per bucket.** It
is append-only text, so a bucket is stated once and cannot be revisited —
which is what sealing, `grace`, displacement and revisions all exist to serve.
A block is a file replaced whole by temp-and-rename, so that premise never
holds, and every one of those mechanisms is answering a question this design
does not have.

⚠ **So when a mechanism suggests itself, ask what it assumes.** If it assumes
one write per bucket, it is a tape's, and the properties above already cover
the case it was for: a cell is mutable and corrected in place, a sample goes
in its own bucket, a filling block is a block, the floor is what bounds a
rewrite, nothing refuses a series, and the grid holds numbers while statements
about a RUN are reports.

Its history is in [tally-partials.md](tally-partials.md), which is where the
mechanisms that were tried and removed are argued out one at a time. This file
does not list them, because a reader learning what a tally is should not have
to learn what it once was.

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
  the candidate, since one late sample otherwise rewrites a whole day block.

⚠ **A defect that exists today:** `commit` renames a block into place and THEN
saves the manifest, so a rewrite at the same `(t0, generation)` leaves a window
where the file is new and the manifest records the old crc32 — and
`read_block` fails on exactly that. Latent, nothing reading blocks yet, and
fixed by the third identity component above.
