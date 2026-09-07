# Designing a tally as a tally

**Status: design, nothing built.** What a tally store must share with a log,
what it need not, and a shape for the parts that differ. Follows
[tally-series-identity.md](tally-series-identity.md), which established that
the tape model was inherited rather than chosen — a log entry is a fact and a
tally bucket is a conclusion — and left the format question open. This note is
that question.

## What is fixed, and why

Three properties are the reason a tally lives in timberfs at all, and none is
negotiable:

* **Compression.** 50 MB for two years of numbers against 300 GB of the log
  they came from; the canonical line rendering exists so that repeated series
  compress.
* **Head-drop.** The retention asymmetry IS the feature — the log's head goes
  and the numbers do not.
* **Queryable through timberfs's own query.** A tally store answers
  `--from`/`--to`, `--has`, `--follow` and the fleet resolver today because it
  IS a store, and that is most of what makes it usable.

Everything else is ours to choose.

## What a tally IS, structurally

**Not a stream of lines: a KEYED store.**
`(bucket start, width, metric, labels) → fields`, time-ordered, tiny, with
bounded cardinality per bucket (`max_series`, 1000 by default). Writes land in
a window that stays open for `width + grace` and then never change — unless
the whole range is regenerated.

Two regions follow, and they want opposite disciplines:

* a **SEALED BODY** — immutable once sealed, compressed, time-indexed. This is
  exactly what a tape is good at, and there is no argument for replacing it.
* an **OPEN EDGE** — a handful of buckets still accumulating, whose values
  change with every entry. A tape is the wrong thing for this, and every
  awkwardness in the shipped design is downstream of pretending otherwise.

## Requirement: "I had not finished that bucket after all"

Take this first, because it is the cheap one and it is independent of
everything else.

**Today** (0.33.0) an open bucket is emitted as a line and superseded by a
later line carrying the complete total. Pragmatic — it shipped, it is correct,
and it surfaced a quiet store's newest minute, which nothing else did. But it
puts revisions on a tape that every aggregating reader must now resolve, and
it made newest-line-wins load-bearing where it had been a guard against
re-running an extractor.

**The elegant answer is that the open edge is not on the tape at all.** Hold
the accumulating buckets in a readable sidecar, rewritten as they change, and
promote them into the sealed body when they seal. Then:

* no revisions on the tape, and newest-wins goes back to being a guard;
* a reader sees the current minute by reading the sidecar, and knows it is
  provisional from WHERE IT CAME FROM rather than from a marker;
* the sealed body is genuinely immutable, which is the property everything
  else wants.

⚠ **This is not a new decomposition — it is what a log already does.** `.sap`
is a readable live edge: `query` reads it when the store declares a wal, so
"the newest data is in a sidecar, not yet in a chunk" is shipped machinery.
The only difference is that a log's edge is APPENDED and a tally's is
REWRITTEN, because a bucket accumulates rather than arriving finished.

**And it is cheap, because the open edge is reconstructible.** The open
buckets are already re-derived from the source after a restart — that is what
`Roller::safe_offset` holds the consumer's position back for — so the sidecar
needs no write-ahead discipline of its own. Temp-plus-rename on each flush is
enough, which is what `.bark` already does. Its size is bounded by
construction: `max_series` per open bucket start, over a `width + grace` of
starts.

⚠ **One consequence worth seeing, even though it is not a reason.** If the
open edge were DURABLE rather than merely reconstructible, a tally's watermark
could be `delivered_to` and its position would not lag by `grace` at all —
which is the interaction that produced the 51-entries/s deadlock
([consumer-holding.md](consumer-holding.md)). The same knot, seen from the
storage end. Making it durable costs the double write `wal=true` costs; worth
knowing the option exists.

## Requirement: regeneration

**(a) Today, with no format change:** export the kept prefix into a new store,
drop the old one, re-derive the remainder from the source
([tally-series-identity.md](tally-series-identity.md) has the reasoning).
Correct, costs a copy of the small end and three operator steps in an order
that matters.

**(b) If the sealed body were designed for a tally:** RANGE-ADDRESSED blocks,
each carrying a **generation**. Replacing a range means writing new blocks and
switching atomically; the superseded ones are dropped. Head-drop stays
dropping leading blocks; a query still selects blocks by time.

⚠ The point of (b) is the ADDRESS. A block is `(range, generation)` rather
than a monotone number, so nothing that caches or replicates can confuse two
generations of the same stretch — which is precisely the failure that makes
in-place regeneration unsafe on a tape today, where the chunk number is "a
position in one store" and travels to replicas as a shared address. It is the
WAL-timeline idea localised to the one store class whose content is derived,
which is what makes it affordable here and overkill globally.

## Requirement: replication

Falls out of the two above rather than needing a protocol of its own:

* **sealed blocks** — immutable within a generation, shipped verbatim, the
  receiver keyed by `(range, generation)`;
* **the open edge** — a snapshot, idempotent overwrite, cheap because it is
  small and reconstructible;
* **a regenerated range** — a new generation the receiver REPLACES rather than
  appends.

⚠ **And that is a different contract from the frames wire**, which is the
answer to "might it be different?": frames ships what the receiver says it
LACKS, keyed on chunk number, because a log only ever grows. A tally would
ship what the receiver holds a STALE GENERATION of. The difference is not
incidental — it is the same "a log is append-only and a tally is not" that
this whole thread turns on.

## What going bespoke costs

Honest list, because it is the argument for doing as little as possible:
`query` with both time axes, `--has` and the `.grain`, `--follow`, retention
and head-drop, `frames-send`/`frames-intake`, `view`, `list`, `info`, the
fleet resolver and `timbergraph` — all of it works on a tally today for one
reason, which is that a tally store IS a store. Every part of the format that
stops being a tape is a part of that list which needs a reader written for it.

## Recommendation, in order

1. **The open-edge sidecar.** Independent of everything else, answers the
   bucket-completion problem properly, removes the revision rule from the
   tape, and reuses a decomposition that already ships. Do this first even if
   nothing else here is ever built.
2. **Keep the sealed body a tape** until regeneration is routine rather than
   exceptional. (a) covers it meanwhile.
3. **Range-addressed blocks with generations** when it is, and replication
   then follows from the addressing rather than needing its own design.

## Open

* **Whether the open edge should be durable.** Reconstructible is enough for
  correctness; durable would remove the position lag. It is the `wal=true`
  trade — a second write — on a store that takes a handful of lines a minute.
* **Whether a block is a chunk with a generation in `.bark`, or a new
  layout.** The first keeps every reader; the second is free to key on
  `(range, generation)` properly. Probably the first for as long as possible.
* **How `query` reads a tally that is no longer a tape**, if it comes to that.
  A reader per store class, or a view that presents one — and the answer
  decides how much of the list above survives.
* **What a coarsened read does across a generation boundary**, where two
  generations of one stretch may both be present mid-switch.
* **Whether the `!cap`, `!late` and `!drop` markers belong on the sealed body
  or beside the open edge.** They are statements about a bucket's quality and
  are currently drained with it.

## Separable, and found while writing this

* ⚠ **`docs/design.md` says of the `.sap` that "Readers (`query`/`info`/`grep`)
  never touch it".** That stopped being true when the live tail shipped —
  `query` reads the sap's live edge when the store declares a wal. It is the
  document that describes how timberfs actually works, so the sentence is
  actively misleading, and it is the sentence somebody would rely on when
  designing exactly what this note designs.
