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

## A block, concretely enough to argue with

    header    magic, version, width_ms, t0, n_buckets, generation
    series    [ series_id, metric, labels, unit, definition_id ]   (or a ref
              to a store-level dictionary — see Open)
    presence  bitmap, n_series × n_buckets bits
    columns   per (series, measure): n_buckets values
              delta + zigzag + varint, then zstd over the whole column region
    footer    digest, column offsets

Everything a reader needs to answer "metric M matching P over this range" is
in the header and the series table; the columns are fetched only for the
series that matched, which is the query-planning property
[chunks-by-address.md](chunks-by-address.md) wants and gets here for free.

## Open

* **Block range: fixed or adaptive?** A day at 60s is 1440 cells per column —
  a natural unit, and it makes head-drop and regeneration granular in a way an
  operator can reason about.
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
