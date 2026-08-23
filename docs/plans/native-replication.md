# Native replication: frames on the wire, not entries

**Status: design, with the first piece built.** The batch ancestor exists —
`timberfs export --into x.timber` writes a tar of `.rings`, `.trunk` and
`.bark`, `import` reads it, and identity now crosses that hop (see below). The
streaming wire, the sidecar list and the transport are not built.

See also [chunks by address](chunks-by-address.md), which this is the transport
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

## The frame

One hello per connection, then typed frames each carrying a stream id and
their own length — so any frame is skippable without understanding it, which
is what serves extensibility and multiplexing with one mechanism.

    connection hello (once)
       0..8   magic                            8 bytes
       8..12  version                          u32
      12..16  incompat_flags                   u32

    every frame thereafter
       0..4   stream id (0 = the only stream)  u32
       4..8   frame type                       u32
       8..12  payload length                   u32

    stream-open payload
       0..16  origin_id (the travelling half)  uuid
      16..32  sender's own id -> derived_from  uuid
      32..40  first seq in this stream         u64
      40..48  last seq, or 0 = open-ended      u64
      48..52  mode: coverage | index | frames  u32
      52..56  sidecar count n                  u32
      56..56+12n  n x { kind: 8 bytes, len: u32 }
      then    provenance JSON, then each sidecar's bytes

    chunk payload
       0..8   seq                              u64
       8..16  uncomp_len                       u64
      16..24  comp_len                         u64
      24..32  first_write_ms                   u64
      32..40  last_write_ms                    u64
      40..44  sidecar count n                  u32
      44..44+12n  n x { kind: 8 bytes, len: u32 }
      then    comp_len bytes verbatim, then each sidecar's bytes

    ack payload
       0..8   highest contiguous seq stored    u64

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
today. Either way the destination mints its own `id` — two stores sharing one
is treated as corruption and refused, per "Which id travels" above, which is
why the wire carries `origin_id` and the sender's `id` (the destination's
`derived_from`) as separate fields rather than one. This is the transport for
"Globally addressable chunks".

## The header carries `provenance()`, not the whole `.bark`

labels for routing, while the receiver keeps its own retention and index
policy. Operational settings are the receiving tier's business.

## Transport: multiple streams per connection in the protocol, 1:1 on the wire first

The stream identity lives in the frame, not the connection: a `stream-open`
frame binds a small stream id that every chunk and ack frame carries, so
one-connection-per-stream is simply a connection with one open stream, and
multiplexing later is a transport change with no format change, no version
bump and no incompat bit. Pipelining is **per stream from day one** — a sender
must not stall on each chunk's ack, so there is an in-flight window
regardless, and scoping it per stream rather than per connection is what
leaves the mux as bookkeeping instead of a redesign. Muxing waits because the
price of it is flow control: without per-stream credit one slow store (a full
disk, an fsync stall, an outsized chunk) head-of-line-blocks every other
stream on the connection, which is strictly worse than N connections. N
connections also give independent cursors, retry and backpressure for free,
and match the intakes' existing thread-per-connection model. The case that
will force muxing is a **dynamic store set** — 50 containers on one host is 50
connects, 50 receiver threads and constant churn — with a control/pull
direction (a central server asking a node what it holds) and per-stream TLS
handshakes behind it. Note that HTTP/2 would supply muxing, flow control and
TLS ready-made, at the cost of an async runtime and a TLS stack in a tree that
today has neither and serves with blocking `TcpListener` plus `thread::spawn`;
that is a dependency-posture decision, not a free win (see "OTLP gaps").

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
