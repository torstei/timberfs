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
- [paging.md](paging.md) — walking a bounded result set: a cursor beside the
  search rather than inside it, covering every store examined.
- [logline-order.md](logline-order.md) — ordering a multi-store answer by the
  clock an entry CARRIES: the frontier merge that makes it streamable, and the
  per-chunk logline range it needs.
- [view.md](view.md) — reading a store as a tape rather than a result set: a
  pager over chunks, the identifier-to-coordinate loop it exists for, the
  address that coordinate is written as, and the resolver that address
  eventually wants.
