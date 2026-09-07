# A tally store, designed from the data

**Status: design, nothing built.** An earlier version of this note asked how a
tally could be made to fit a timberfs tape. That was the wrong question three
times over, so this one starts from the data instead. Follows
[tally-series-identity.md](tally-series-identity.md), which established that
the tape model was inherited rather than chosen.

## What a tally actually is, measured

A 42-minute tally of a 500k-entry apache store, 20 series, the shipped format:

| | bytes | share |
|---|---|---|
| series identity, each repeated 42× | 27,720 | 35% |
| bucket stamp + width, on every line | 24,360 | 31% |
| citations | 14,957 | 19% |
| **the numbers** | **11,140** | **14%** |

20 series × 42 buckets = 840 lines, exactly. **A tally is a dense, regular
GRID of series × buckets, and the shipped format writes the row key into every
cell.** 86% of a line is not the number.

⚠ **And compression does not recover it**, which is the measurement that
decides this:

| | bytes |
|---|---|
| text, raw | 78,177 |
| text, `zstd -19` | 8,375 |
| columnar, raw | 9,235 |
| columnar, `zstd -19` | **2,756** |

**3× better than the best-compressed text**, with fixed-width 8-byte integers
and no delta coding at all — so that is a floor rather than a ceiling. The
*uncompressed* columnar form is already about the size of the
*best-compressed* text. Structure beats an entropy coder here because the
redundancy is positional, not textual.

## Three things follow from the grid

### A series is an object, not a repeated string

    series_id → (metric, labels, unit, definition)

Assigned at first sight, a small integer, written **once**. Bounded by the
store's cardinality over its life rather than by time.

⚠ This is where [tally-series-identity.md](tally-series-identity.md) lands,
and it dissolves the question that note spent its length on: **a series
carries its definition, so no name prefix is needed** — the name was never the
identity, and once identity is an object there is nothing to qualify.

### A bucket start is a POSITION, not a value

A block covers `[t0, t0 + n·width)`. Sample *i* of a series is bucket
`t0 + i·width`. **Zero bytes of timestamp**, and no parsing to group by time.

⚠ Which requires a **presence bitmap**, and that is a feature rather than a
cost: "zero and unknown are different" is tally's second invariant and is
today a convention nothing can enforce. One bit per `(series, bucket)` makes
it structural — and answers the open question the identity note left, where a
metric added today leaves last week UNKNOWN for it rather than zero.

### Measures are columns, not fields on a row

`count`/`sum`/`min`/`max`/`last` coarsen differently and compress
differently. As columns they are runs of like numbers; as fields on a text row
they interleave with labels and stamps. And **coarsening becomes a column
operation** — 60s to 5m is combining five adjacent cells per column, under the
rule the field name already names, which is the invariant tally was built on
expressed directly instead of re-derived from text.

## The open edge stops being a special case

A block whose range has not closed is **open**: mutable, checkpointed,
rewritten as its cells fill. When the range passes `grace` it is **finalised**
and compressed.

⚠ **There is no revision concept, because there is nothing to revise.** A
provisional value is a cell in an open block; its final value is the same cell
later. The supersede rule shipped in 0.33.0 exists only because a tape cannot
express "this value is not final yet" — the tape's only verb is *append*, so
correcting a number meant writing another one. Given a mutable cell the whole
mechanism, and the newest-line-wins rule it forced on every reader,
disappears.

And a reader knows a value is provisional **structurally** — it came from an
open block — rather than by comparing it with a later line.

## Regeneration is replacing blocks

A block is addressed `(range, generation)`. Regenerating means writing
generation+1 for the ranges covered, swapping those manifest entries
atomically, and dropping the old blocks. Nothing else in the store moves, and
there is no numbering to collide because **a block was never a position in a
sequence** — which is the whole difficulty on a tape, where a chunk number is
"a position in one store" and travels to replicas as a shared address.

Head-drop is dropping leading blocks and raising the manifest's floor. No
`FALLOC_FL_COLLAPSE_RANGE`, no offset rebasing, no seqlock: a block is a file
and forgetting it is unlinking it.

## Replication is a manifest diff

The manifest IS the state: `(range, generation, digest)` per block.

* the receiver states what it holds;
* the sender ships blocks it lacks **or holds a stale generation of**.

Idempotent, diff-based, and needing no offsets. ⚠ Which is why this cannot be
the frames wire: frames ships what the receiver LACKS, keyed on chunk number,
on the assumption that a log only ever grows. A derived store needs to say
"replace that", and a diff over a manifest says it in one round trip.

## "Queryable through timberfs query" does not force a tape

⚠ **The text line format survives as the INTERCHANGE form rather than as the
storage.** `query` renders lines out of blocks; `timbergraph`, `--fold`,
`--try`, a pipe into `grep` and a human reading the terminal all keep working
unchanged.

That is the resolution of the requirement that looked like it forced a tape:
the line format was doing two jobs — storage and interchange — and only one of
them needed a tape. Keeping it as the rendering keeps every reader and every
test, and gives up nothing, because a projection of a grid into lines is
cheaper than parsing lines into a grid.

## What is genuinely given up

* **The `.grain` token index.** Meaningless here — you select a tally by label,
  not by substring, and the identity is in a dictionary rather than in the
  bytes.
* **Two clocks.** A tally has ONE, the bucket start. ⚠ A simplification rather
  than a loss: the 0.33.0 fix had to stamp chunks with bucket windows so that
  the write and logline axes would agree, and a store with one clock cannot
  have that bug.
* **`view` as a tape to scroll.** A grid is not a tape; the answer screen over
  a rendered query is the equivalent.
* **The head-drop machinery**, replaced by unlinking a file.

## How big is a block, and how many

Measured with delta+zigzag+varint columns then `zstd -19`, 50 series, two
measures each:

| block | B/cell | one day | files / 2y | rewrite while open |
|---|---|---|---|---|
| 15 min | 2.07 | 291 KB | 70,080 | 3 KB |
| 1 hour | 1.88 | 264 KB | 17,520 | 11 KB |
| 6 hours | 1.59 | 223 KB | 2,920 | 56 KB |
| **1 day** | **1.31** | **184 KB** | **730** | 184 KB |
| 7 days | 1.29 | 182 KB | 104 | 1,274 KB |

⚠ **Compression has a knee at about a day and then flattens** — 1.31 to 1.29
B/cell for seven times the range buys nothing, while 15-minute blocks cost 58%
more bytes for 96× the files. So a day is the block, and the file count is 730
for a two-year retention, which is nothing.

Whole-store sizes, at 60s buckets and two years:

| series | per day | two years |
|---|---|---|
| 20 | 74 KB | **54 MB** |
| 50 | 184 KB | 134 MB |
| 1000 (the `max_series` cap) | 3.7 MB | 2.7 GB |

The 20-series row is worth noting: [tally.md](tally.md) estimated "50 MB of
tally for two years" from first principles, and this lands on 54 MB. At the
cardinality cap it is 2.7 GB, which is the cap doing its job of making
cardinality an explicit decision rather than a surprise.

### ⚠ The last column is the catch, and it is the `.sap` tension again

A day's block is 184 KB at 50 series and 3.7 MB at 1000, and an open block
rewritten on every checkpoint means rewriting that much every couple of
seconds. Which is exactly the bind
[docs/design.md](../design.md) names for logs — "chunking has two masters that
want opposite things: compression wants chunks big and infrequent; durability
wants every byte on disk the instant it arrives" — arriving here as
*compression wants a day and checkpointing wants a handful of buckets*.

**The resolution is the same shape: decouple them.**

* the **open region** holds only the buckets that have not sealed — `width +
  grace`, so three of them at the defaults. 3 buckets × 1000 series × 2
  measures is ~12 KB, cheap to rewrite at any cardinality;
* a **sealed bucket is appended to the current day's block as a SEGMENT** — a
  short column region covering the buckets that sealed since the last append;
* when the day closes the segments are **compacted** into one column region,
  which is where the compression knee is actually collected. Compaction reads
  immutable input and writes a new generation of the same block, so it is the
  regeneration path doing double duty.

⚠ So a block has internal structure and a reader reads its segments. That is
an LSM in miniature, and it is what every time-series store ends up with for
this exact reason — worth saying plainly rather than arriving at it by
accident three revisions later.

## How the files are named

    web-access-tally/
      web-access-tally.bark          declared properties, as any store
      web-access-tally.tally        the MANIFEST — authoritative
      b/20260906T000000Z.g1         a sealed day, generation 1
      b/20260907T000000Z.g1
      b/20260908T000000Z.g2         this day was regenerated
      open                          the unsealed buckets, temp+rename

Four properties, each the reason for a part of it:

* **Lexicographically sortable**, so a retention sweep and a range scan are
  both directory order and neither needs the manifest to find candidates.
* **The generation is in the NAME**, so `ls` says what you have and a replaced
  generation is visibly a different file. ⚠ Deliberately not content-addressed:
  a digest as the filename makes replication idempotent and dedupes, but makes
  a directory an operator inspects completely opaque. The digest goes in the
  manifest, where the replication protocol wants it anyway.
* **Derivable from `(range, generation)`**, so a receiver can place a shipped
  block without asking anything.
* **A subdirectory**, so 730 block files do not sit beside the manifest and
  the `.bark`, and a forest scan — which looks for a store's marker files —
  does not see them at all.

### The manifest is the commit point

Which is what makes every crash window benign:

1. write the new block file and fsync it;
2. rewrite the manifest, temp-plus-rename — **this is the commit**;
3. unlink the superseded block.

A crash between 1 and 2 leaves a block the manifest does not reference; a
crash between 2 and 3 leaves a superseded one nothing reads. Both are
unreferenced files a sweep can collect, and neither is a store that lies. ⚠
The alternative — the directory as truth — makes step 2 unnecessary and every
partial write a corrupt store, which is the trade `.bark`'s temp-plus-rename
already makes in this tree.

## A block, concretely enough to argue with

    header    magic, version, width_ms, t0, n_buckets, generation
    series    [ series_id, metric, labels, unit, definition_id ]   (or a ref
              to a store-level dictionary — see Open)
    segments  one or more, appended as buckets seal; a compacted block has
              exactly one:
                bucket range covered
                presence  bitmap, n_series × n_buckets_in_segment bits
                columns   per (series, measure): the segment's values,
                          delta + zigzag + varint, then zstd per column region
    footer    digest, segment offsets

Everything a reader needs to answer "metric M matching P over this range" is
in the header and the series table; the columns are fetched only for the
series that matched, which is the query-planning property
[chunks-by-address.md](chunks-by-address.md) wants and gets here for free.

⚠ A compacted block has one segment and an open one has many, so **a reader
does not care which it is** — it reads the segments it needs. Compaction is
then a pure optimisation that can be skipped, deferred, or run by something
other than the writer, which is the property that makes it safe to leave out
of a first cut.

## Open

* **Segment length**, which is the one number this design actually turns on:
  how many sealed buckets accumulate before a segment is appended. Short
  segments cost compression (2.07 B/cell at 15 minutes against 1.31 at a day)
  and long ones cost a bigger open region to rewrite. The block range is
  settled at a day by the measurements; the segment inside it is not.
* **Whether compaction is the writer's job or a sweep's.** It reads immutable
  input and writes a new generation, so it need not be, and a `trim`-shaped
  cron-able verb would fit the tree — which also answers who trims a retired
  generation ([tally-series-identity.md](tally-series-identity.md)).
* **Where the dictionary lives.** Per block makes a block self-contained and
  therefore replicable and readable alone; per store is smaller and dedupes
  across the store's whole life. Self-contained probably wins, since it is
  what makes the manifest diff a complete protocol.
* **What `!cap`, `!late` and `!drop` become.** They are per-bucket statements
  about quality, so probably their own columns or bits beside the presence
  bitmap — which would make them selectable rather than markers a reader has
  to notice.
* **The open block's checkpoint interval**, and whether it is durable at all:
  the cells are re-derivable from the source, which is what
  `Roller::safe_offset` currently holds a consumer's position back for. ⚠ A
  durable open block would let that position advance freely, which is the
  same knot that produced the 51-entries/s deadlock
  ([consumer-holding.md](consumer-holding.md)) seen from the storage end.
* **Whether a tally store is still a timberfs "store"** for `list`, `info`,
  selection and the follower registry. It should be — those read `.bark`, and
  a manifest can sit beside one.
