# Frames of a selection: many stores on one connection, and an identity that travels

**Status: design.** The frames counterpart of
[follower-selection.md](follower-selection.md), and it settles what
[receiving-end.md](receiving-end.md) left as design: the receiver's naming
policy. The wire it rests on is [native-replication.md](native-replication.md),
whose "multiplexing waits on per-stream flow control" and "the destination
mints its own id" this note supersedes, each for a reason that arrived after
those sentences were written.

Today one `frames-send` is one store, one connection and one stream. Fifty
containers on a host is fifty of each, and the destination decides where each
one lands by a LABEL — which is the mechanism receiving-end.md measured four
separate collisions in.

## The subject of a send becomes a selection

    A frames sender is a destination and a selection, and a stream in the
    connection for each store that selection matches.

which is the same sentence follower-selection.md wrote for followers, and the
same `select.rs` predicate: `--select '[service=~apache-.*]'`, `[]` for every
store, and the one-store case is the positional `STORE` argument it has always
taken. The selection is resolved per pass, so a store that appears joins the
connection with its own `stream-open` and one that stops matching stops
producing frames — nothing watches the forest, exactly as `ship.rs` does not.

## Multiplexing, and the objection that lapsed

The stream id has been in every frame since the codec was written; what kept
one connection to one store was flow control. Without per-stream credit a
stalled store head-of-line-blocks the others, "which is strictly worse than N
connections" — N connections buying independent cursors, retry and
backpressure.

That argument assumed the streams WANT to be independent. A sender of a
selection is one destination, and follower-selection.md already settled what
that means: «One destination means one queue, so a stalled endpoint stalls
every store in the selection. That is the right coupling — they share the
destination — and retention is the budget for it.» The stores are coupled by
the destination whether or not the sockets are, so N connections buy
independence that the arrangement does not have.

So the mux is bookkeeping, as the codec's author intended, and there is no
credit protocol here: a pass walks the selection, gives each store a bounded
turn, reads the acks that turn produced, and lets TCP's own backpressure do
the rest.

⚠ **The bound is required for correctness, not for pacing.** The receiver acks
every chunk, so a sender that writes a whole backlog without reading fills the
receiver's write buffer; the receiver then blocks writing and stops reading,
and both ends wait on each other. (Latent before this too — a one-shot send of
a large enough store could reach it — and universal once several stores ship
before one drain.) It also bounds the memory a turn holds, `serve` rendering
its whole range into one buffer. A store with more waiting simply takes another
turn, so **no store is starved**: the cap is per store, where the round-robin
follower-selection.md needs exists because its entry cap is shared.

⚠ **A COUNT, not a seq range.** The two are the same only while the numbering
is dense, and a store whose oldest chunk sits above the range would answer with
nothing — leaving a sender that resumes from what it examined stuck at a
position it can never move past.

Per-stream credit stays deferred, for the case that will actually force it: the
pull direction, where a receiver asks and cannot decline what it gets.

⚠ **The receiver is still one thread per connection**, so a slow store's
`fsync` does hold up the frames behind it. That is the same coupling as above
seen from the other end, not a separate thing to fix.

## A stream needs an end: `stream-close`

One additive frame kind (6, empty payload). Without it a session lives until
the connection drops, and a receiver holds the writer locks and the open
`Store` of every store the selection has EVER matched — which on a host of
short-lived containers is unbounded. The sender sends it when a store leaves
the selection or is deleted; the receiver finishes that session, answers a
final `coverage`, and forgets it.

Additive by construction: the payload length is what lets an older receiver
skip a kind it does not know, so one that predates this simply keeps the
session open until the connection ends — which is what it does today.

## Identity travels, and the numbering travels with it

**The reversal.** `format.rs` says of the id in the rings header: «It must
never be copied from a sender or a source: replication and `export` mint a
fresh identity at the destination.» That sentence predates three decisions
that now govern:

  * a position — a cursor, a follower's place — is keyed by store IDENTITY;
  * selection is the primitive, so **the path is opaque** and a store is found
    by what it declares;
  * `(origin_id, seq)` addresses chunks across the fleet, and the tape offset
    `dropped + uncomp_start` is the same absolute number at both ends of a
    replica (consumer-protocol.md).

Under all three, a replica is not a derivative of the store — it IS the store,
in another place. Minting a second identity for it makes one tape answer to
two names, so nothing can merge a fleet answer, dedup a chunk, or carry a
position from one tier to another. So:

    A replica keeps the sender's id. `derived_from` is not written, because a
    replica derives from nothing.

`origin_id` and the id then hold one value for every store frames has ever
touched, which is the point: the id IS the origin, and the wire's two fields
carry it in the two places the layout already has for them.

**And the boundary is the numbering.** The rule this replaces was really "never
claim an origin and renumber", and keeping the id is the same claim one field
wider — this is the same tape. A destination that renumbers is a different
tape and must not wear the id.

So **copy mode goes**. `--replica` stops being a flag and becomes what frames
IS; `Numbering::Renumber` leaves this path. What copy mode was for — ingest
someone else's chunks as a new, independent store — is `export`/`import`, which
does it with a manifest, a lineage pointer and no wire.

That is also the honest fix for a defect copy mode had all along: **a
renumbering receiver's coverage is stated in ITS numbering, and the sender
resumes from it as though it were the sender's.** For a fresh destination
receiving from chunk 0 the two coincide, which is why this has never been
seen; for a store whose head has been dropped they do not, and the sender
re-ships from its oldest chunk on every pass, forever. Replication and
resumption were never separable: only a receiver that preserves the numbering
can answer where the sender got to.

## The receiving end is a lookup, not a route

`--route <label>` picks a label whose value names the destination store, and
receiving-end.md measured what that costs: two hosts with one service name
merge; `checkout/v2` and `checkout_v2` sanitize into one store; a reinstall
appends a second tape to the first; and two hosts that share a short hostname
merge with no misconfiguration at all. Every one of those is a collision in
the LOOKUP KEY, so no directory layout fixes them — a key that is stable and
unique does.

    The destination is found by SELECTION, on the store id the stream carries.

Three ordered steps, and the same `select.rs` the sender uses:

1. `[origin_id=<the stream's id>]` — **the destination declares which store it
   holds.** A store this receiver has already received writes it; a store an
   operator pre-created declares it by hand (`create --set origin_id=…`), which
   is how a destination's `retain`/`index` policy is settled before any data
   arrives; and a store received by an older build wrote it too, so an upgraded
   archive keeps receiving instead of starting a second copy of everything it
   holds. That legacy store keeps its own minted `id` — only new destinations
   inherit — and `origin_id` is what says the two are one store.
2. `[id=<the stream's id>]` — a store that already wears the identity and
   declares no origin.
3. Neither: `--auto-create` mints the destination, and without it the stream
   is refused and says so — the same posture as every other intake.

⚠ `origin_id` and not `derived_from`, though both were recorded: lineage says
where a DERIVATIVE came from, and only the origin says which store this is. It
is also the one an operator can write, `derived_from` being protected against
`set` exactly because it is lineage.

⚠ A legacy archive that received WITHOUT `--replica` cannot be continued: it
recorded no origin, and its chunks were renumbered, so it is not that store's
replica and never was. It keeps working as the store it is; a replica of the
source starts beside it.

`--route` is then a flag with no job. Accepted for one release with a warning
that it no longer routes, then removed; the shipped unit drops it.

**A new destination is `<forest>/<id>/<id>.log`** — the id-named directory
`incus-intake` already writes, for the reason receiving-end.md gives: a store
lives at «something unique» and timberfs is the tool that answers where. It
also removes the last thing `sanitize_name` was needed for on this path: a
uuid needs no escaping, so no two route values can round to one directory.

## What travels: the store, not the pair

Identity and labels are what the store IS, and they travel. Everything else
describes THIS pair, here, and stays local:

| travels | stays local |
| --- | --- |
| `id` | `created` — when this pair was established on this host |
| `name` | `index`, `wal`, `retain`, `retain_size` — the receiving tier's policy |
| labels (`provenance()`) | the path |

**`name` has to travel now**, and did not before: the destination's name came
from `--route`, so it was RECONSTRUCTED at the receiver from a label. With the
directory an id, there is nothing left to reconstruct it from, and a replica
with no name reads as its own uuid in `list`. It rides the `stream-open`'s
JSON blob beside the labels — an added key, so an older receiver adopts it
into the manifest and is simply right about the name.

⚠ **`created` deliberately does not travel**, though it is minted with the id.
It answers «since when has this host held this store», which is what
`--follow-from discovery` compares a follower's declaration against; inheriting
the origin's would make a store that has just arrived read as three years old,
and a follower on the archive would then ship nothing of it.

## The sender's positions

One `--cursor` file cannot hold N stores, so the sender writes the shape the
follower registry already uses: `positions.json`, one entry per store keyed by
identity, `chunk` the acked seq and `offset` that chunk's tape offset
(`dropped + uncomp_start`) — the same expression the follower's own
`cursor.json` migration computes, so the two agree.

⚠ It is still what native-replication.md calls it: **a cache, not a cursor.**
The sender never reads it to decide where to start — the receiver's coverage
is authoritative — and losing it costs nothing but slightly conservative
retention until the first ack. Its two consumers are the retention interest
floor and the reporting columns, both of which need a local, synchronous,
offline answer.

Both fields advance together and only when the acknowledgement has caught up
with what was sent; behind that the previous entry stands rather than being
overstated. A chunk boundary IS an offset, so for a frames sender the resume
point and the retention floor are one fact, and recording them from two
different chunks would be the only way to make them disagree.

`cursor::consumers_in` learns to read a positions file beside a cursor, or a
store declaring `cursors=<dir>` reports the frames sender as one unreadable
file instead of a consumer.

## What this is not

**Not frames joining the consumer protocol.** A follower feeds its consumer
`timberfs-records(5)` entries and takes a watermark back; a frames sender reads
stores itself and takes an ack from the far end. Registering one as a follower
— and so putting a frames destination on the retention interest axis — needs
the `chunks` diet (consumer-protocol.md's deferred list). Until then **an edge
store replicating by frames is bounded by `retain`/`retain_size` alone**, and
the positions file above is reporting, not a hold.

**Not per-stream flow control**, and not the pull direction that will need it.

**Not the WAL frame.** Only sealed chunks ship, so a replica still trails its
source by one chunk flush, however many stores share the connection.
