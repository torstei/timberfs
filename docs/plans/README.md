# Plans

Design notes for work that is not built. One file per thread, each opening
with a status line.

`ROADMAP.md` holds the backlog: a paragraph per direction, enough to know
whether an idea exists and roughly what it costs. A note lands here when the
thinking outgrows that — a wire format, an invariant with several
consequences, a decision whose reasoning would otherwise be re-litigated — and
the roadmap entry shrinks to a summary plus a pointer.

**Status lives in the document, not in the directory.** These files are mostly
not uniformly one thing: a note may rest on invariants that already hold,
describe a format that does not exist, and record defects that are real today.
Splitting "planned" from "implemented" by location would need a manual move to
stay true, and a directory that has to be maintained to avoid lying is worse
than one that never claimed anything — so each file says where it stands, and
sections say so again where they differ from the whole.

When something ships, its description belongs in
[docs/design.md](../design.md), which documents how timberfs actually works.
The plan file then keeps only what is still speculative, or goes away. What
stays here is never the record of *how* it was built — that is what commit
messages and pull requests are for.

## Notes

- [native-replication.md](native-replication.md) — shipping `.trunk` frames
  verbatim: the framed, sidecar-extensible, multiplexable wire.
- [chunks-by-address.md](chunks-by-address.md) — the tape model, manifests,
  and fetching bytes from whichever holder has them.
- [receiving-end.md](receiving-end.md) — identity, names and selection on an
  archive that many senders ship into.
- [frames-selection.md](frames-selection.md) — one frames connection for a SET
  of stores, and the identity that travels with them: the destination found by
  selection rather than named by a route.
- [follower-selection.md](follower-selection.md) — one follower declaration
  for a SET of stores: the selection as its subject, and the poll loop that
  serves the whole set from one process.
- [file-intake.md](file-intake.md) — the same move on the INGEST side: a named
  set of a system's log files tailed by one process, and why this one needs no
  registry.
- [consumer-protocol.md](consumer-protocol.md) — timberfs holds the
  position and a consumer says how far to move it: three messages, why the
  watermark means "do not send me these again", and what that makes possible
  (any language, and a destination on another machine).
- [consumer-holding.md](consumer-holding.md) — the third thing a consumer can
  be doing with an entry, beside taking it and dropping it: HOLDING it. The
  measured deadlock between the park and tally's watermark that capped a tally
  follower at 51 entries/s and silently corrupted its numbers, the `taken`
  report that separates flow control from the position, and the provisional
  bucket that replaced the force-seal — including the fix this note first
  proposed and got wrong.
- [paging.md](paging.md) — walking a bounded result set: a cursor beside the
  search rather than inside it, covering every store examined.
- [logline-order.md](logline-order.md) — ordering a multi-store answer by the
  clock an entry CARRIES: the frontier merge that makes it streamable, and the
  per-chunk logline range it needs.
- [frame-witness.md](frame-witness.md) — a `.trunk` frame whose `.rings`
  record never landed: which of the four crash paths can leave one, why the
  wal's seal already recovers its own, the on-disk stage marker that makes the
  rest decidable, and the source witness that would let `file-intake` adopt one
  (and serve a live edge) without writing every byte twice.
- [tally-as-a-tally.md](tally-as-a-tally.md) — a tally store designed from the
  data rather than from the tape it inherited. Measured on a real day: 83% of
  a tally line is not the number, the grid is 21% dense, and a columnar block
  is **8.5× smaller than the shipped store on disk** — 0.44 GB against
  3.77 GB over a two-year retention. A series becomes an object and a bucket
  start a position, which makes the presence bitmap enforce "zero and unknown
  are different", makes provisional values a mutable cell rather than a
  revision, and makes regeneration and replication a manifest diff. Sized
  against six contiguous real days: a day-sized block costs +4% over a
  six-day one, and the working set saturates rather than drifting.
- [tally-series-identity.md](tally-series-identity.md) — a metric is a series,
  and whether two series combine is the READER's decision: the shipped fold
  makes it at storage time and irreversibly. The remedy is that the definitions
  travel with the tally store and their names are the namespace, so a metric
  name is unique within one tally. The ordinary edit — a regex fixed, a metric
  added or removed — is an in-place update that breaks no series; a changed
  MEANING is the exception the generation machinery serves. Why an
  offset-scoped definition fails as a reader's contract and works as
  provenance; and why the tape model was inherited rather than chosen — a log
  entry is a fact, a tally bucket is a conclusion, and only the prefix older
  than the source's retention horizon is irreplaceable.
- [tally-partials.md](tally-partials.md) — a tally that never holds a bucket
  to completion, and therefore needs no cardinality cap. `window.max_series`
  asks its author to predict traffic that has not happened, and it does not
  bound the memory it exists to bound. It comes from one decision — a bucket
  is accumulated in memory until complete, then written once — and the input
  is replayable while `Field::combine` is already the associative merge, so
  spilling partials is legal. Four other mechanisms dissolve with the cap; the
  bill is mandatory compaction, and the sharp edge is that additive partials
  are not idempotent.
- [tally.md](tally.md) — metrics derived from the log as a tape of their own:
  the extractor as a consumer (and therefore backfillable), the one invariant
  that decides the line format, and how a site declares extractors of its own.
- [view.md](view.md) — reading a store as a tape rather than a result set.
  A first version has shipped, along with the fleet resolver the address was
  shaped for and the result-set screen an answer is read on, so what is left
  here is the "who has this store" half of resolution, the drill-down from a
  timestamp, and the questions those versions answered one way.
