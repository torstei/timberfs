# A tally store, designed from the data

**Status: design, nothing built.** An earlier version of this note asked how a
tally could be made to fit a timberfs tape. That was the wrong question three
times over, so this one starts from the data instead. Follows
[tally-series-identity.md](tally-series-identity.md), which established that
the tape model was inherited rather than chosen.

## What a tally actually is, measured on a real one

One day of a real site's performance log — 2.7M lines, 780 MB — through that
site's own extractor: four metrics over one line shape, three label-set views
plus a latency histogram, `max_series` raised to 4000.

The tally it produces: **268,140 lines, 32.3 MB of text, 992 series over 1,260
buckets.** Where the bytes go:

| | share |
|---|---|
| series identity, repeated once per bucket | **44%** |
| bucket stamp + width, on every line | 24% |
| citations | 14% |
| **the numbers** | **17%** |

**83% of a tally line is not the number**, and on real data it is worse than
on synthetic, because real series identities are long — Java `class.method`
names, tenant names — where a synthetic `status=200` is short.

⚠ **And compression does not recover it.** Against what actually ships, at the
level a tally store actually writes (zstd 3, not 19):

| one real day | |
|---|---|
| tally as text | 32.3 MB |
| **the shipped store, on disk** | **5,040 KB** (6.3×) |
| **columnar** | **592 KB** — 542 columns + 29 bitmap + ~20 dictionary |
| **ratio** | **8.5×** |
| over a two-year retention | **3.77 GB against 0.44 GB** |

Structure beats an entropy coder here because the redundancy is *positional*,
not textual: zstd can shorten a repeated identity but cannot stop it being
there once per bucket.

### ⚠ And the grid is SPARSE, which corrects this note's first claim

An earlier version of this note called a tally "a dense, regular grid". Real
data says **21% density** — 268,136 present cells against 992 × 1,260
possible. One metric was 17%: most `class.method` pairs do not appear in most
minutes.

**The design does not depend on density, and it is worth being clear about
why the win survives.** It comes from writing each identity once instead of
once per bucket, which is 44% of the bytes and is *independent* of how full
the grid is. Density only decides how much the value columns cost. Sparsity
is then handled without a penalty:

* a column holds only the values that are PRESENT — a series appearing in a
  tenth of the buckets writes a tenth of the numbers;
* the bitmap locates them, and stays cheap when nearly empty: 992 × 1,260
  bits is 156 KB raw and **29 KB** compressed, the runs being exactly what an
  entropy coder is for.

So the honest framing is a **grid that is usually sparse**, whose row key the
shipped format writes into every occupied cell.

### Two things the same run measured, in passing

* **56,600 entries/s** for four metrics over 2.7M real entries (48 s), against
  310,000/s for the two-metric apache document on synthetic lines. Four
  `extract` regexes and a 20-rung histogram cost what one would expect, and
  ⚠ all four metrics share ONE `claim`, which is run four times — the
  duplicated-work finding in [consumer-holding.md](consumer-holding.md),
  visible on a real document.
* **The histogram emitted 54.5M observations from 2.7M entries** — a
  cumulative `le` ladder writes one sample per rung at or above the value, so
  ~20 per entry. Which is why `service_duration`'s 23 series are the densest
  thing in the store and the cheapest to encode as columns.

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

## A high-cardinality metric: bytes per vhost

⚠ Modelled rather than measured, unlike the real day above — the shapes are
synthetic. Kept because it varies cardinality deliberately, which one real
log cannot.

`http_bytes` by `vhost` on a shared-hosting box: a few busy vhosts all day, a
long tail with an hour here and there.

| vhosts | present cells/day | columns | bitmap | per day | two years |
|---|---|---|---|---|---|
| 50 | 19,380 | 76 KB | 4.9 KB | 81 KB | 0.06 GB |
| 500 | 190,504 | 757 KB | 45.5 KB | 802 KB | 0.60 GB |
| 1000 (the cap) | 374,931 | 1458 KB | 90.4 KB | 1.5 MB | 1.16 GB |

Against the shipped text format for the identical 500-vhost day: 183,664
lines, 17.9 MB raw, **1812 KB** compressed — so columnar is **2.26×** better
here rather than the 3× of the dense case, because a sparse series offers less
positional redundancy to exploit. Worth stating rather than quoting the
flattering number.

**Three things make sparsity work, and one of them settles an open question:**

* **A column holds only the values that are PRESENT**; the bitmap says which
  buckets they belong to. A vhost with one hour of traffic costs 60 values and
  not 1440, so an absent cell costs a bit rather than a number.
* **The bitmap is cheap even when it is mostly empty** — 1440 bits with 1% set
  is 180 bytes raw and **37 bytes** after zstd, because the runs are exactly
  what an entropy coder is for. Sparse series do not pay for the buckets they
  are missing.
* ⚠ **The dictionary must be PER BLOCK**, which the Open list below had left
  undecided. Vhosts churn: a store that has seen 50,000 of them over two years
  should have blocks listing only the ~500 active in each day. A store-level
  dictionary grows monotonically and every block ends up referencing ids from
  a table dominated by series that died a year ago. Per block makes
  **cardinality local in time**, and it is the same choice that makes a block
  self-contained enough to replicate on its own.

⚠ **What does NOT work is exceeding the cap**, and that is by design:
`max_series` is 1000 per bucket, so 500 vhosts fits and 5,000 does not — the
`!cap` marker fires and the excess is dropped, recorded. An operator wanting
the latter raises the cap or stops labelling by vhost, which is the decision
being forced into the open.

⚠ And this is the metric that meets the performance cliff already recorded in
[consumer-holding.md](consumer-holding.md): `Roller::add` scans the bucket's
existing series for every NEW one, measured at 4.16 s capped against **61.7 s
uncapped** over 500k entries. High cardinality is exactly where that bites, so
"bytes per vhost" is the metric that would find it. Fixing it is on that
note's list and is independent of any of this.

## Retention granularity is one day

Yes — head-drop unlinks leading blocks, so retention moves in whole days.
`retain 30d` keeps between 30 and 31 days.

That is coarser than a log's, where retention drops chunks of a few hundred
KB, but the absolute step is small because the whole store is: a day is 74 KB
at 20 series, 802 KB for 500 vhosts, 1.5 MB at the cap. Trading a day of
granularity for one unlink and no offset rebasing is a good trade at those
sizes; it would not be on a store measured in GB per day, which is why a log
keeps the finer mechanism.

⚠ **The consequence that matters is not the granularity, it is the POSITION.**
A consumer of a tally store — something shipping the numbers onward — holds a
byte offset today, because the store is a tape. With blocks there is no tape
to hold an offset on, so a position becomes a bucket time or a
`(range, generation)`. Which touches `retain_unconsumed` and `cursors`, both
of which a tally store can declare, and the consumer protocol's "ONE unit:
the absolute offset on the store's tape". A tally would need its own answer
there — the same "a tally is not a log" that this note turns on, arriving one
more time.

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

## Separable, found while measuring

* ⚠ **`tally --try` held 3 GB of RSS** for the 780 MB input, because
  `entries_of_text` collects every entry into a `Vec` before folding any of
  them. The follower path streams and does not do this, so it is `--try`'s own
  defect — and `--try` is exactly what an operator points at a day of log to
  develop a document, which is the case that makes it matter.

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
* ~~Where the dictionary lives.~~ **Settled by the vhost case above: per
  block**, so cardinality is local in time and a block is self-contained.
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
