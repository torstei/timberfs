# Native replication: frames on the wire, not entries

**Status: design, with the first pieces built.** The batch ancestor exists —
`timberfs export --into x.timber` writes a tar of `.rings`, `.trunk` and
`.bark`, `import` reads it, and identity now crosses that hop. The frame codec
is implemented in `src/frame.rs` (encode, decode, skip-what-you-do-not-know,
and the bounds checks a length off the network needs), the SERVE side in
`src/serve.rs` — a store read out as `coverage`, `index` or `frames`, reusing
`query`'s seqlock guard and shipping `.grain` pages as the first real sidecar
kind — and the RECEIVE side in `src/receive.rs`, which turns a stream back
into a store, byte-identically, either as a replica (origin and numbering
preserved together) or a copy (neither). The TRANSPORT is built too, in
`src/frames.rs`: `timberfs frames-intake` and `timberfs frames-send`, with the
registration handshake — a sender resumes from the receiver's position rather
than a cursor of its own, and a colliding origin is refused at setup naming
the holder. `follower --type frames` registers it as a
supervised shipper, so the whole loop closes: retention releases a prefix only
once the far end has acknowledged it. Multiplexing and the receiving end's
naming policy are settled in
[frames-selection.md](frames-selection.md), which supersedes two sentences
below — the id the destination mints, and the flow control the mux was waiting
on. The digest is deferred (see chunks-by-address.md).

See also [chunks by address](chunks-by-address.md), which this is the
transport
for, and [the receiving end](receiving-end.md) for identity and routing.

Shipping a store's `.trunk` frames verbatim instead of re-encoding its
entries. **The win is CPU, not bandwidth** — measured on 200k apache
entries (23.5 MB plain, 2.33 MB compressed at rest): a `.timber` bundle is
2,341,376 bytes and costs 0.00 s to write, 0.04 s to import; the same data
as gzipped OTLP protobuf is 2,579,528 bytes and costs 6.17 s of sender
CPU. Uncompressed OTLP is 28.4 MB and a records stream 38.7 MB (per-entry
metadata makes it larger than the plain log), so the "tenth of the
bandwidth" framing only holds against an uncompressed peer: gzip recovers
nearly all of it. What it cannot recover is the work — the node
decompresses zstd, encodes protobuf per entry and gzips, while the
receiver reverses all three and re-zstds, to move data that was zstd at
both ends. That CPU lands on the machine serving production traffic.
Shipping frames also carries what re-encoding destroys: chunk boundaries,
and therefore chunk numbers and a `.grain` (the index is chunk-positional,
so only an alignment-preserving transport can move it — 264 KB shipped
versus ~1 s of rebuild per 200k entries).

## The frame (built: `src/frame.rs`)

One hello per connection, then typed frames each carrying a stream id and
their own length — so any frame is skippable without understanding it, which
is what serves extensibility and multiplexing with one mechanism.

    connection hello (once)               magic "TIMBSTR1", version 1
       0..8   magic                            8 bytes
       8..12  version                          u32
      12..16  incompat_flags                   u32

    every frame thereafter
       0..4   stream id (0 = the only stream)  u32
       4..8   frame kind                       u32
       8..12  payload length                   u32

    stream-open payload                   kind 1
       0..16  origin_id (the travelling half)  uuid
      16..32  sender's own id -> derived_from  uuid
      32..40  first seq in this stream         u64
      40..48  last seq, or u64::MAX = open      u64
      48..52  mode: 1 coverage, 2 index, 3 frames  u32
      52..56  provenance length                u32
      56..60  sidecar count n                  u32
      60..60+12n  n x { kind: 8 bytes, len: u32 }
      then    provenance JSON, then each sidecar's bytes

    chunk payload                         kind 2
       0..8   seq                              u64
       8..16  uncomp_len                       u64
      16..24  comp_len (TRUE size, always)     u64
      24..32  first_write_ms                   u64
      32..40  last_write_ms                    u64
      40..44  sidecar count n                  u32
      44..44+12n  n x { kind: 8 bytes, len: u32 }
      then    comp_len bytes verbatim (absent in index mode),
              then each sidecar's bytes

    coverage payload                      kind 3
       0..4   run count n                      u32
       4..4+16n  n x { start: u64, end: u64 }  inclusive

    accepted payload                      kind 4
       0..16  registration id (server-assigned)  uuid
      16..    coverage: what the server holds

    conflict payload                      kind 5
       0..16  holder origin_id                 uuid
      16..    coverage of the holder, then a u32-prefixed reason

Two corrections the implementation forced, both cases of the written layout
being undecodable rather than merely awkward:

**`last_seq` uses `u64::MAX`, not 0, for open-ended.** Zero is a legitimate
last seq — a stream carrying only chunk 0 — so the sentinel had to move.

**A `provenance length` field was missing.** With variable-length sidecar
bodies *and* variable-length provenance, nothing said where the provenance
ended; the two were undecidable from each other. Every length now sits in the
fixed prefix, which is the property the whole design leans on.

There is no separate ack frame: an ack is a `coverage` frame, since a
contiguous receiver acking a store it holds to 424242 is sending one run —
the degenerate case of the same answer rather than a second kind.

Per-chunk cost is 12 bytes of header plus 44 of fixed fields, so 0.2% on the
measured mean frame size of 25,919 bytes, and 12 bytes per sidecar beyond
that. Asserted by a test, because a field added carelessly is invisible until
it is on every chunk.

## Offsets never travel

A rings record is 56 bytes but only 40 of them are portable: `uncomp_start`
and `comp_start` are local, and a head-drop rebases them. The receiver
accumulates its own. Same rule as the drop counters — lengths, never offsets.

## The chunk payload is optional, and that makes the wire a catalogue too

In `index` mode the frames carry their metadata and sidecars with no trunk
bytes — `comp_len` still reports the chunk's TRUE size, because half of what a
catalogue is for is how big the thing is. It is a declared mode rather than a
silently absent payload: a sender that simply stopped sending bytes is
indistinguishable from a broken replica. Rings alone are ~0.2% of the data
(4.7 KB against 2.34 MB on a 90-chunk store), which is what makes "what do you
hold" cheap enough to ask often — discovery, cross-tier query planning, and a
reconciliation richer than an ack (`have 4831` resumes a stream; exchanging
indexes finds HOLES). This is the control direction of a central server
talking to nodes, or to other tiers with different retention.

## Three granularities, and the coarsest is the one discovery needs

`coverage` answers with a RUN LIST — the `(start, end)` seq intervals this
node holds — `index` with one metadata frame per chunk, `frames` with the
bytes. The gap between the first two is what makes coverage its own mode
rather than holes implied by absent frames: ~16 bytes per run either way,
against 4.7 KB of index on a 90-chunk store and ~520 MB on a 10M-chunk
archive. A discovery ping is a run list; `index` is for per-chunk detail of a
range already known to be worth asking about.

## Trunk-only is deliberately NOT a mode

The write windows exist only in the rings — frame headers give sizes and
nothing else — so dropping them discards the time axis that makes this a store
rather than a pile of zstd, and saves 0.2%. What that direction actually wants
is the seq RANGE above: full frames, fewer of them, for backfilling a tier
that found a gap.

## Sidecars are a list, not a slot

`.grain` is one kind; the zone-map and record-length sidecars above are two
more, and a hardcoded `grain_len` field would need a format change for each.
Unknown kinds are **dropped**, which is safe by the sidecar contract itself —
derived, rebuildable, and a chunk with no index entry means "scan it" — so
this needs no negotiation, no handshake and no incompat bit. That is what
makes it cheap. The line: sidecars are droppable, the chunk is not, so
`incompat_flags` guards the chunk (a codec or framing change) while sidecars
ride an ignore-what-you-do-not-know list. Same split as `header_len` versus
`incompat_flags` in the rings header. Cost is 12 bytes per sidecar, 0.06% on a
25 KB chunk. Folding a sidecar's parameters into its kind tag also removes a
hazard rather than documenting one: `.grain`'s header records case-folding,
`MIN_TOKEN`, `MAX_TOKEN` and `K` in bytes 8..12 but `first_record_offset`
validates only the magic, so a page written under different constants would be
read under the reader's own — a silent FALSE NEGATIVE, the one direction a
Bloom filter must never fail in. Parameters in the tag make a mismatch an
unrecognised kind, hence a rebuild. (Latent today; shipping pages across a
fleet at mixed versions is what makes it reachable.)

## The resume key is a coverage answer, one number in the common case

For a CONTIGUOUS receiver the highest contiguous `seq` is the entire cursor:
it reports `have 4831`, the sender continues at 4832. A SPARSE receiver has no
such number — "highest contiguous" stalls at its first hole and re-requests
forever — so its position is a run list, and an ack is therefore a DEGENERATE
COVERAGE RESPONSE: the same information at two resolutions, an integer when
contiguous and a run list otherwise. This supersedes the earlier formulation
of the resume key as a write window plus lengths, which predates chunk
numbers; window-plus-lengths remains how `read_chunk` re-locates a chunk
internally, and is no longer what the wire needs. A registered `retaining`
follower plus `retain_unconsumed` is what guarantees 4832 still exists on
reconnect.

## 1:1 mirroring, not fan-in

, and now for two reasons rather than one. Frames are opaque and the rings
must stay sorted, so interleaving two sources needs decoding — the records
path's job. And shipping `seq` *is* a claim on the origin's numbering: the
invariant on `ChunkRecord::seq` is never claim an origin and renumber, and two
senders into one numbering-preserving store cannot both be honoured. So the
header's numbering-preserving flag is load-bearing: set, the destination
copies `origin_id` VERBATIM, preserves `seq`, and refuses a second sender;
clear, it makes no origin claim and renumbers, which is what `export` does
today. ⚠ **The destination no longer mints its own `id`** — see
[frames-selection.md](frames-selection.md). A replica keeps the sender's,
because a position, a chunk address and a tape offset are all keyed by identity
and a replica is the same tape in another place; the boundary is the numbering,
so the renumbering half of this paragraph leaves the frames path altogether and
`export`/`import` is where a new, independent tape is made. This is the transport for
"Globally addressable chunks".

## The header carries `provenance()`, not the whole `.bark`

labels for routing, while the receiver keeps its own retention and index
policy. Operational settings are the receiving tier's business.

## Transport: multiple streams per connection in the protocol, 1:1 on the wire
first

The stream identity lives in the frame, not the connection: a `stream-open`
frame binds a small stream id that every chunk and ack frame carries, so
one-connection-per-stream is simply a connection with one open stream, and
multiplexing later is a transport change with no format change, no version
bump and no incompat bit. Pipelining is **per stream from day one** — a sender
must not stall on each chunk's ack, so there is an in-flight window
regardless, and scoping it per stream rather than per connection is what
leaves the mux as bookkeeping instead of a redesign. ⚠ **The flow-control objection lapsed**, and
[frames-selection.md](frames-selection.md) says why: it assumed the streams
want to be independent, where a sender of a selection is one destination and so
one queue by design. N connections buy an independence the arrangement does not
have. Per-stream credit is still what the PULL direction will need, where a
receiver asks and cannot decline what it gets. Note that HTTP/2 would supply muxing, flow control and
TLS ready-made, at the cost of an async runtime and a TLS stack in a tree that
today has neither and serves with blocking `TcpListener` plus `thread::spawn`;
that is a dependency-posture decision, not a free win (see "OTLP gaps").

## The frames cursor is a cache, and is named like a position

Built, and worth revisiting. `frames-send --cursor` writes what the far end
has acknowledged, and the sender NEVER READS IT to decide where to start:
`first_seq` is always 0 and the resume point comes from the receiver's
`accepted` answer. So for the thing a cursor is normally for — resumption —
it is dead weight.

It does have two consumers, and both need state that is local, synchronous
and available offline, so something has to persist: the retention interest
floor (`min(seq)` over retaining followers, computed inside a live writer
on every flush, which cannot make a network call) and `follower list`'s
POSITION and LAG columns.

**But it is a CACHE, not a cursor, and that distinction is load-bearing.**
(It is a `positions.json` keyed by identity once a sender ships a selection —
same file, same rule.)
Lose an OTLP cursor and `--start` decides, so a store is re-shipped whole
or skipped to the end. Lose a frames cursor and nothing happens: the next
connect re-learns the position from the receiver, and the only cost is
slightly conservative retention until the first ack. It may be stale,
absent or hand-deleted with no consequence.

⚠ **The naming is the hazard.** Calling it a cursor, storing it in
`cursor.json` and typing it as `Cursor` all invite a future reader to
"simplify" by resuming from it — reintroducing exactly the re-send that
making the receiver authoritative was for. That regression would look like
a cleanup. It wants a name that cannot be mistaken for a position.

**It used to be written far too eagerly (fixed).** `write_cursor` read the
manifest, read the cursor, called `format::read_index` — parsing the WHOLE
rings file — and wrote, all to record one chunk's write time for a display
column. With acks arriving per chunk that was N full rings reads to ship N
chunks: 56 bytes a record, so 560 KB parsed per chunk shipped on a
10,000-chunk store, quadratic over a run. Two changes, and neither costs
accuracy: `serve` hands back the highest chunk it sent with its write
window, so the value comes from the frame instead of the rings; and the
loop DRAINS pending acks and writes once per pass rather than once per ack,
skipping the write entirely when it would say what the file already says.
The write time is supplied only when the acknowledgement has caught up with
what was sent — behind that, the previous value stands rather than being
overstated by a newer chunk's time.

## A WAL frame would lift the latency floor, at a price

Not needed yet, and worth writing down because the constraint is
non-obvious. Only sealed chunks ship, so a replica is always one chunk-seal
behind while the `.sap` serves a 0.2 s tail locally. A frame carrying WAL
bytes would move that capability across the wire: a live tail on the
archive, which is grepping the fleet's live edge from one box — the thing
this design otherwise gives up.

**The WAL is append-only, and that is what makes it simple.** Bytes are
written from beginning to end and never in the middle, so a frame is a pure
byte-range append: `uncomp_base` plus bytes, and the receiver takes only
what lies past what it holds. Nothing already sent can change, so there is
no invalidation and no reconciliation — one integer of state per stream, on
both sides, and a reconnect resumes from the receiver's offset. When the
chunk covering those bytes finally arrives, the receiver drops that many
bytes from the FRONT of its tail buffer: a truncate, because the chunk
always covers a prefix of an append-only log.

**It wants a separate file, not a new state for `.sap`.** A crashed store's
`.sap` is replayed into a chunk on the next open — `FileStore::open`
compresses the replayed entries and writes a `ChunkRecord` — and for a
replica that is fatal, since it would mint a chunk the origin never made,
with boundaries the origin does not have, so `(origin_id, seq)` would name
different bytes at each end. But that replay is a WRITER's path keyed on
the sap's filename, and the tail READER is already separate: `live.rs`
deliberately does not use `sap::replay` ("right for a writer opening its
own store, wrong for every reader") and reads the longest CRC-valid prefix
by path. So a differently-named received-tail file is never replayed by
construction, and the read path needs to learn one more filename rather
than the sap needing a new state. Discarding that file after a crash is
also safe: the sender still holds those bytes in its own unsealed sap and
resends from the offset the receiver reports.

**The cost is duplication, and it only applies while someone is watching.**
WAL bytes are uncompressed, so the same entries cross twice: raw now, and
again inside the chunk that eventually compresses them — at 10:1, 256 KB
plus 25 KB where the chunk alone was 25 KB, an 11x increase over the live
window. But the tail only has to flow when a reader wants it, which makes
this a RUNTIME STATE rather than a configuration decision: nobody watching
costs nothing, and no operator has to predict in advance where a live tail
will be wanted.

**Interest travels upstream, on the connection that already exists.** The
reverse direction carries acks today, so a tail-interest frame is one more
additive kind — and the first use of that direction for something other
than acknowledging. It composes over hops: a tier asks its own upstream
whenever it has an interested downstream, so interest propagates and lapses
along the chain without anything coordinating it.

How the archive notices a reader is open, and `flock` fits the question the
way it already does elsewhere: `LockProbe` exists precisely for read-only
"is anyone there", and a lock is released by the kernel when the holder
dies — which is what an ephemeral `query --follow` needs and what a
registry entry would get wrong. Some linger before dropping interest keeps
a reader that restarts from thrashing the upstream.

⚠ **Starting interest must send the CURRENT unsealed tail**, not only what
arrives next. The sender's WAL already holds bytes written before anyone
asked, and a reader that sees nothing until the next entry reads as broken
rather than as idle. So "start" means "your tail from its base, then keep
going".

⚠ **The obvious way to avoid that duplication is a trap.** Since the
receiver already holds the raw bytes, the sender could send chunk metadata
only and let the receiver compress its own — zstd is deterministic, so with
the same input and level the frame is byte-identical. It is not a guarantee
worth resting byte-identity on: output can change between zstd versions and
build settings, and a fleet is never on one version. Ship the chunk.

Smaller consequence: `follower list` reporting "at the live edge" would
become optimistic, since caught up on chunks is not caught up on entries.
The two would want telling apart.

## Latency puts a floor under it

Only sealed chunks exist to ship, so this wire is one chunk-seal behind the
live edge (256 KB or 5 s idle, whichever comes first). The `.sap` live tail is
0.2 s. That, not "transform versus not", is why the entry wire stays: frames
replicate, records merge, transform and tail, and the two are chosen by
latency and by whether the shape must change — not competitors.

## Identity across a hop (done)

A timberfs import seeds the destination's manifest from its source: the
destination mints its own `id`, records the immediate parent as
`derived_from`, and inherits the labels, while operational settings —
retention, the index, the wal — stay the destination's own. Lineage is claimed
only when exactly one source declares a manifest, since a stitched set of
segments has no single parent to name, and a re-import never rewrites a
manifest that already exists.

This was the prerequisite: `import` used to discard the source's manifest
entirely, so an imported store came out anonymous and nothing that cites an
origin could be built on it.

## Validation stays a choice, not a default

checking what arrives costs the decompression this design exists to avoid.
Keep the cheap structural checks (ring records consistent with frame sizes)
and leave a corrupt frame to fail at read, where `zstd -dc` is already the
stated recovery path.

This is the WRITE-direction complement to read-only serve; whether one
endpoint family serves both representations — records for "I want to
read this", frames for "I want a copy of this" — is worth deciding only
once the read side exists.
