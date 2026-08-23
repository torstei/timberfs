# Roadmap / ideas

A backlog of directions for timberfs — not commitments. How the current design
works is in [docs/design.md](docs/design.md).

- **More `.bark`**: an `annotate` command for existing logs, attribution
  labels from manifest fields in multi-file output (`--label '{host}'`),
  auto-seeded provenance on import, bark-aware routing in the future
  sawmill server.
- **Docs: "why is my grep slow?"**: a short troubleshooting section
  walking the modes table — word mode + grain = fast; --regex/--substring/-i/-v =
  full scan and why; how to build the index (`create --index`/`reindex`)
  and read the full-scan notes.
- **Zone-map sidecar (`--written-from/--written-to`)**: per-chunk logline
  windows as a derived sidecar, making BOTH time axes queryable (arrival
  for "what came in during the incident", logline for history) and
  giving the sawmill lag observability. The read path already treats the
  trunk as its own timestamp index; this would only accelerate it. The
  concrete case asking for it: an arrival-stamped store (`append`, so the
  FIFO pair and piped logs) fed by a producer whose line stamps are not
  its write times — Apache logs a request's start and writes at completion
  — where a logline-time query today leans on the one-minute selection
  widening and misses a request slower than that. The sidecar would answer
  such a query exactly, without asking the producer to change its format.
- **Record-length index**: entry boundaries within a chunk are known when
  records are appended and currently discarded. An optional per-chunk sidecar —
  independent zstd frames plus a fixed-stride directory, the same shape as
  `.trunk`/`.rings` one level down — could persist them, giving cheap per-chunk
  entry counts and within-chunk record seeking without decompressing. Same
  sidecar discipline as `.grain`: rebuildable, deletable, no core-format change.
  Whether it earns its keep depends on real query patterns; `--has` to find an
  identifier, then a time-range extract around it, may already be enough.
- **`timberfs merge`**: entry-aware N-way merge — split sources (raw logs
  or timberfs) into log entries, merge-sort them by timestamp, emit one
  timberfs store or raw stream. Subsumes "grep a fleet into one artifact"
  (merge, then `grep --into`) and gives shipped per-host segments a
  single-timeline view at write time rather than only at read time.
- **A timberfs server ("sawmill")**: bundles shipped in over HTTP (PUT +
  idempotent import = at-least-once ingest for free), routed to per-stream
  archives by their `.bark` manifests, queried over a thin REST layer
  wrapping query/grep. Tiering: keep rings+grain LOCAL, ship trunks to
  object storage — queries plan locally (time windows + blooms) and fetch
  candidate chunks as single S3 ranged GETs. Principle: the directory
  stays the database; the server owns no state that is not a plain
  timberfs file. Path: lib refactor → .bark → read-only serve → ingest →
  tiering.
- **Read-only serve**: the next step on that path — the lib boundary and
  `.bark` are done. Make a forest readable over the network so an operator
  can search it without shell access on the log host: enumerate stores
  (`list`), report one (`info`), and select entries by time window and by
  the existing predicates, grain-accelerated as locally.
  **The API is timberfs's own, not a compatibility layer.** A
  Loki-compatible facade was the earlier bet and is dropped: LogQL cannot
  express what this store is good at. It has no syntax for a
  word-anchored token search, so the `.grain` index — 20x on a measured
  34 MB store, one chunk decompressed against 136 — is simply unreachable
  through it, and its single timestamp would flatten the two-clock
  semantics. An API shaped by a foreign query language would have made
  every distinctive property of the store either invisible or a lie.
  A **Grafana datasource plugin then becomes one CONSUMER** of that API
  rather than the reason for its shape (Grafana's plugin model asks for no
  query language at all — a query is a JSON object the plugin defines),
  and the CLI is another. Which is the ordering that keeps the API honest:
  it answers to the store, and clients adapt.
  Much is still open and should stay that way until there is code: the
  wire format (the control plane looks like what `--json` already emits;
  the data plane could be a **timberfs-records(5) stream**, whose
  stream-end totals prove a response arrived complete where a bare HTTP
  body cannot, but that is an argument for a property rather than a
  settled choice), whether anything proxies or fans out, and how a client
  addresses a fleet. HTTP, not gRPC, for the reason `otlp-intake` gives.
  One thing worth designing in rather than bolting on: a **cost
  preflight** — chunk selection precedes decompression, so the server can
  state how many chunks and roughly how many bytes a query would read
  *before* reading them, which most log servers cannot answer without
  doing the work. That one has a working prototype already, arrived at
  sideways: Grafana demands the same number, and the spike below computes
  it from `.rings` alone.
  Concurrency needs nothing new; a server is just another standalone
  reader, already covered by the collapse seqlock and the grain/rings
  generation check.
- **What the LogQL spike measured** (branch `spike/logql-serve`, not for
  merge — kept as evidence): enough of Loki's read API for a real Grafana
  11.3.0 to point at a forest. It is the reason the facade above is
  dropped, and it is worth keeping because a negative result that stops
  the wrong build is worth more than an estimate.
  Grafana asks for a CORNER of LogQL, not the language: four routes
  (`/query` for the health check, `/labels` for autocomplete,
  `/index/stats` for query sizing, `/query_range` for both panels) and
  exactly ONE metric shape. Four things no amount of reading the docs
  would have given: the datasource **health check is itself a metric
  query** (`vector(1)+vector(1)` at a year-2096 instant), so log-query
  support alone can never make a datasource go green; `/index/stats` is
  the one route that is **not enveloped** and wrapping it renders "will
  process approximately NaNundefined" in the UI; the volume query carries
  `detected_level` (a per-entry derived label — `otlp::Severity::of()`
  already computes exactly that) and a `| drop` stage that a metric
  subset would have to ACCEPT as a no-op; and Grafana passes a 400's text
  to the user verbatim, which is what makes refusing unsupported LogQL by
  name the right call rather than pedantry.
  Also settled, and load-bearing for why the facade is dead: **reusing
  Loki's engine is blocked by a licence, not an architecture.** Loki is
  AGPLv3 (verified) against timberfs's MIT/Apache, so linking it in makes
  an unshippable binary; and inverting it — teach Loki to read timberfs —
  dissolves that (an AGPL fork is permitted) only to hit the fact that
  Loki has no storage plugin seam at all, every backend being Go compiled
  in. Its actually-pluggable layer is an object store, where timberfs
  would hold Loki's own opaque chunks and none of its own indexes would
  apply.
- **Globally addressable chunks (an ingest choice, not a law)**: a chunk's
  number is local today — shipping renumbers, so the same data has two
  addresses and neither knows about the other. `(bark id, seq)` is already
  unique, the id being a UUID that survives renames, moves and hosts; the
  defect is not the scheme but that a hop discards half of it. Worth
  making a **declared choice at ingest** rather than a fixed rule, because
  in a fleet the addressability is the point: "chunk 42424242 of
  `8f14e45f-…`" is a citation that survives the network.
  **What it buys.** Three things, and the third is the one that argues
  hardest. Federated queries can **dedupe by ADDRESS** — the same chunk
  seen from an edge and from an archive is one `(origin, seq)` — where the
  only primitive today is `import`'s line-hash comparison. Cursor
  positions become **comparable across tiers**, so a position means the
  same thing wherever the data now lives. And **renumbering destroys the
  evidence of a gap**: an edge that drops chunk 102 to retention before
  shipping it hands the centre 100, 101, 103, which dense renumbering
  turns into 0, 1, 2 — no hole, no record, the loss visible only in a
  shipper warning on a box that may be thrown away. Preserved numbering
  puts the hole in the data permanently, which is the same doctrine as the
  exact loss record extended to survive a hop.
  **The invariant, and it is the load-bearing part.** The address has two
  halves and they travel together or not at all:
  > **Never claim an origin and renumber.** Recording an origin without
  > preserving `seq` produces an address that LIES, and that combination
  > must be refused rather than configured. Preserving `seq` without
  > recording an origin is legal but weaker — gap evidence survives,
  > addressing does not. Both, or neither, or numbers-only.
  Four quadrants, one of which must be impossible: origin+numbering is a
  true replica; origin+renumbered is broken; fresh+numbering keeps gap
  evidence only; fresh+renumbered is today's behaviour and is therefore
  CONSISTENT — nothing to fix, only a capability to add.
  **Which id travels.** Not `id` itself. Two stores sharing an `id` is
  currently treated as CORRUPTION and says so — `follower.rs` refuses with
  "a copied .bark gives two stores one identity", and `cursor::check_store`
  leans on the same assumption that an id names one store's bytes. So the
  travelling half is a separate lineage key (`origin_id`, copied VERBATIM
  and never rewritten), which leaves `id` unique per store and every
  existing check working. It also has to be distinct from `derived_from`,
  which is the IMMEDIATE parent: a chain of hops cannot be walked, since it
  crosses hosts, so only a verbatim-copied origin is stable across N hops.
  **The trade to decide, not to discover.** A number is only an ADDRESS if
  the chunk BOUNDARIES held. Re-chunk on the way in and the destination's
  chunk 42 holds a different set of entries — the number survives, the
  address lies. So "globally addressable" and "tune each tier's chunk size
  independently" are alternatives, per store: an archive wanting big chunks
  for compression cannot also inherit an edge's small ones for latency.
  Expect addressing to survive exactly ONE hop in a three-tier fleet, and
  say so rather than let it be found out.
  **The live edge has no address**, by construction: `EntryRec.chunk` is
  `None` there because the chunk does not exist yet, which is also why a
  cursor does not advance on those entries. So a `wal`-backed follower
  delivering sub-second carries no address for the newest data, and an
  address becomes available one chunk AFTER the entry does. Consistent
  with the asymmetry retention already lives with (delivery is
  entry-granular, erasure chunk-granular) rather than a new surprise.
  **The line is whole-chunk versus entry-level selection**, not window
  versus whole. A chunk copied INTACT keeps its address however few of its
  siblings came along: `export`'s window and `rotate`'s prefix both take
  whole frames verbatim, so `(origin, seq)` still names the same bytes, and
  a bundle carrying source numbers is a per-chunk CITATION plus a visible
  hole where something is absent. What destroys an address is selecting
  ENTRIES: a filtered ship (`timber-filter | import --records`) delivers
  chunk 42 with fewer entries than the origin's, so the number survives
  and the address lies. That case must refuse rather than be configured,
  and it is detectable — the records stream's stream-start carries an echo
  of the selection. Fan-in from two sources must refuse for the separate
  reason of monotonicity.
  **Also open**: how a store records that the boundary condition actually
  held, since a later re-chunk would silently invalidate every address
  without touching the manifest — a plain boolean is too easy to leave
  true by accident.
  **The precondition is a CHECK, not a hope**, and it is checkable at the
  one moment it has to be decided:
  > Numbering is preserved iff the destination has **never been written**
  > (`next_seq == 0`) and the ingest **binds** it to that origin. Once
  > bound, a stream from another origin — or any ordinary append — is
  > refused.
  ⚠ "Never written" is `next_seq == 0`, NOT `chunks.is_empty()`. Retention
  is allowed to drop every chunk and the numbering deliberately does not
  restart; `numbering_does_not_restart_when_retention_empties_the_store`
  exists because a store renumbering from 0 after being emptied "would
  hand a fresh chunk a number some cursor counts as consumed — which is
  silent data loss". So an emptied store is NOT eligible, and the two
  states are indistinguishable by chunk count alone.
  Empty is necessary and not sufficient — on its own it only covers the
  FIRST source, and source B arriving later would interleave. Which is
  what the binding is for: `origin_id` is not only the address's other
  half, it is the **exclusivity claim**, so "one source for life" is
  mechanically enforced rather than configured. It follows that a store is
  writable only by its origin's stream **while bound** — an ordinary
  `append`, or an `import` of anything else, would assign local numbers and
  silently break every address.
  **Unbinding is available and deliberate**, because "I need to add records
  and I accept that this store stops being streamable-from" is a legitimate
  thing to want. The two-step pattern the follower registry already
  settled applies unchanged: an explicit release that states what it costs,
  and then ordinary writes work — no `--force` on the write path, and
  nothing silently degrading. `append` and `import` are the same case here;
  both assign local numbers, and only `import --records` could ever have
  preserved.
  ⚠ Like `retaining=false`, unbinding is one-way in EFFECT and not just in
  flag: once a local chunk lands, origin chunks and local ones are
  indistinguishable, so the addresses the store used to serve stop being
  verifiable and re-binding would not restore them. Recording the boundary
  instead of a whole-store flag would fix that — the same per-range
  provenance shape the low-water mark below keeps asking for, and a reason
  to suspect the eventual answer is a range rather than a boolean. Monotonicity is what `partition_point`, `rotation_split` and the
  whole cursor axis rest on; density need not survive, since every
  comparison is `<` and none assumes `+1`.
  **What the views owe.** `info` should report the numbering a store
  holds and, when it is not the whole history, how much went — which is
  the single-store form of the gap-evidence argument above, and is exactly
  the fact a chunk count cannot carry. `list` gets nothing until there is
  a binding to show, at which point origin-versus-replica is very much a
  fleet question.
  **Five touchpoints**, all of which already state the local-only rule and
  its reason, so the choice is a relaxation of a written precondition
  rather than a surprise: `format.rs`'s `seq` doc; `store.rs`'s
  `append_frames` (rotate's move path, which renumbers and says why — and
  which deliberately owns its own chunking, so it is both the first place
  anyone would relax this and the place the cost is starkest);
  `sink.rs`'s deliberate discard of `e.chunk` (the number is already ON
  THE WIRE, so this is the cheapest seam and the one that matters for a
  fleet, since shipping is what crosses hosts); `export.rs`, whose stated
  reason for numbering a bundle from 0 — "neither dense nor meaningful" —
  is the WEAKEST of the three, since nothing requires density and the
  source's numbers are meaningful precisely as its identity, so a bundle
  is a candidate for preserving rather than an argument against it; and
  `bark.rs` for the new lineage key.
  **The low-water mark turned out not to be owed after all**, because the
  drop counters are now RECORDED rather than derived. The rings header had
  32 reserved bytes and a written growth contract ("reserved space is only
  safe for OPTIONAL fields, 0 reads as absent"), so `chunks`,
  `uncomp_bytes` and `comp_bytes` went in at 32..56 with no version bump,
  maintained by the same two head-drop paths that already keep `next_seq`
  current. Since the count no longer rests on numbering starting at 0, a
  window extract or partial replica that kept source numbers needs no
  correction — which removes the reason a low-water mark was wanted.
  Two things that had to be got right. The totals are sums of LENGTHS, not
  of offsets: `collapse_head` rebases survivors by the block-ALIGNED cut and
  leaves the sliver in `comp_start`, so summing offsets would count it again
  on the next drop; lengths are immune, agree across both drop paths, and
  mean "what left the store" rather than "what the filesystem reclaimed" —
  which genuinely differ by that sliver. And zero-reads-as-absent collides
  with "nothing dropped" for a byte count, resolved by the numbering
  itself: a store whose oldest chunk is number 0 has dropped nothing, so
  `chunks == 0` beside a non-zero oldest number means the header predates
  the counters, and `info` says "size not recorded" rather than a
  confident zero.
  The bytes are not otherwise obtainable at all — a head-drop rebases the
  survivors' offsets, so what went leaves no trace in the index.
- **Scoped, audited read access**: what a serve API makes possible and
  shell access cannot — a grant of *subject × store or forest × data
  window × grant lifetime*, the last two being different clocks ("this
  hour of yesterday, readable until next week"). It enforces where
  selection already happens, before decompression, so bytes outside the
  grant are never read rather than read-and-filtered. ⚠ The disposition
  must invert: `query` deliberately fails OPEN — an entry whose own
  timestamp cannot be parsed is always included, and the chunk window is
  widened — which is right for search and a leak for authorization, so a
  grant has to fail closed. Content predicates are NOT exactly enforceable
  (`--has` is chunk-level and Bloom filters carry ~1% false positives), so
  they may narrow a grant but must never be its boundary. A live grant
  probably implies a **retention hold** on the chunks it covers, else the
  window ages out mid-investigation — the same open question as a cursor
  holding retention back. The static alternative already exists and is
  airtight in a way a server cannot be: `export --from/--to` into a
  `.timber` bundle makes the capability the data itself — un-widenable,
  carrying its own recorded window and lineage — at the cost of copying it
  and of being unrevocable. The two are complementary, and a grant could
  be implemented as an ephemeral virtual bundle to keep one mental model.
  Audit records the **selection**, not merely the access (the records
  stream-start already carries it) plus the preflight's volume, so bulk
  collection reads differently from investigation; it ships off-box,
  because a trail on the host it audits is not evidence against local
  root; and its store stays out of the served forests, or a broad grant
  reads the record of its own reads.
- **Native replication (frames on the wire, not entries)**: shipping a
  store's `.trunk` frames verbatim instead of re-encoding its entries.
  **The win is CPU, not bandwidth** — measured on 200k apache entries
  (23.5 MB plain, 2.33 MB compressed at rest): a `.timber` bundle is
  2,341,376 bytes and costs 0.00 s to write, 0.04 s to import; the same
  data as gzipped OTLP protobuf is 2,579,528 bytes and costs 6.17 s of
  sender CPU. Uncompressed OTLP is 28.4 MB and a records stream 38.7 MB
  (per-entry metadata makes it larger than the plain log), so the "tenth
  of the bandwidth" framing only holds against an uncompressed peer: gzip
  recovers nearly all of it. What it cannot recover is the work — the node
  decompresses zstd, encodes protobuf per entry and gzips, while the
  receiver reverses all three and re-zstds, to move data that was zstd at
  both ends. That CPU lands on the machine serving production traffic.
  Shipping frames also carries what re-encoding destroys: chunk
  boundaries, and therefore chunk numbers and a `.grain` (the index is
  chunk-positional, so only an alignment-preserving transport can move
  it — 264 KB shipped versus ~1 s of rebuild per 200k entries).

  **The frame.** One hello per connection, then typed frames each carrying
  a stream id and their own length — so any frame is skippable without
  understanding it, which is what serves extensibility and multiplexing
  with one mechanism.

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

  **Offsets never travel.** A rings record is 56 bytes but only 40 of them
  are portable: `uncomp_start` and `comp_start` are local, and a head-drop
  rebases them. The receiver accumulates its own. Same rule as the drop
  counters — lengths, never offsets.

  **The chunk payload is optional, and that makes the wire a catalogue
  too.** In `index` mode the frames carry their metadata and sidecars with
  no trunk bytes — `comp_len` still reports the chunk's TRUE size, because
  half of what a catalogue is for is how big the thing is. It is a
  declared mode rather than a silently absent payload: a sender that
  simply stopped sending bytes is indistinguishable from a broken replica.
  Rings alone are ~0.2% of the data (4.7 KB against 2.34 MB on a 90-chunk
  store), which is what makes "what do you hold" cheap enough to ask
  often — discovery, cross-tier query planning, and a reconciliation
  richer than an ack (`have 4831` resumes a stream; exchanging indexes
  finds HOLES). This is the control direction of a central server talking
  to nodes, or to other tiers with different retention.

  **Three granularities, and the coarsest is the one discovery needs.**
  `coverage` answers with a RUN LIST — the `(start, end)` seq intervals
  this node holds — `index` with one metadata frame per chunk, `frames`
  with the bytes. The gap between the first two is what makes coverage its
  own mode rather than holes implied by absent frames: ~16 bytes per run
  either way, against 4.7 KB of index on a 90-chunk store and ~520 MB on a
  10M-chunk archive. A discovery ping is a run list; `index` is for
  per-chunk detail of a range already known to be worth asking about.

  **Trunk-only is deliberately NOT a mode.** The write windows exist only
  in the rings — frame headers give sizes and nothing else — so dropping
  them discards the time axis that makes this a store rather than a pile
  of zstd, and saves 0.2%. What that direction actually wants is the seq
  RANGE above: full frames, fewer of them, for backfilling a tier that
  found a gap.

  **Sidecars are a list, not a slot.** `.grain` is one kind; the zone-map
  and record-length sidecars above are two more, and a hardcoded
  `grain_len` field would need a format change for each. Unknown kinds are
  **dropped**, which is safe by the sidecar contract itself — derived,
  rebuildable, and a chunk with no index entry means "scan it" — so this
  needs no negotiation, no handshake and no incompat bit. That is what
  makes it cheap. The line: sidecars are droppable, the chunk is not, so
  `incompat_flags` guards the chunk (a codec or framing change) while
  sidecars ride an ignore-what-you-do-not-know list. Same split as
  `header_len` versus `incompat_flags` in the rings header. Cost is 12
  bytes per sidecar, 0.06% on a 25 KB chunk. Folding a sidecar's
  parameters into its kind tag also removes a hazard rather than
  documenting one: `.grain`'s header records case-folding, `MIN_TOKEN`,
  `MAX_TOKEN` and `K` in bytes 8..12 but `first_record_offset` validates
  only the magic, so a page written under different constants would be
  read under the reader's own — a silent FALSE NEGATIVE, the one direction
  a Bloom filter must never fail in. Parameters in the tag make a mismatch
  an unrecognised kind, hence a rebuild. (Latent today; shipping pages
  across a fleet at mixed versions is what makes it reachable.)

  **The resume key is a coverage answer, one number in the common case.**
  For a CONTIGUOUS receiver the highest contiguous `seq` is the entire
  cursor: it reports `have 4831`, the sender continues at 4832. A SPARSE
  receiver has no such number — "highest contiguous" stalls at its first
  hole and re-requests forever — so its position is a run list, and an ack
  is therefore a DEGENERATE COVERAGE RESPONSE: the same information at two
  resolutions, an integer when contiguous and a run list otherwise. This supersedes the earlier
  formulation of the resume key as a write window plus lengths, which
  predates chunk numbers; window-plus-lengths remains how `read_chunk`
  re-locates a chunk internally, and is no longer what the wire needs. A
  registered `retaining` follower plus `retain_unconsumed` is what
  guarantees 4832 still exists on reconnect.

  **1:1 mirroring, not fan-in**, and now for two reasons rather than one.
  Frames are opaque and the rings must stay sorted, so interleaving two
  sources needs decoding — the records path's job. And shipping `seq`
  *is* a claim on the origin's numbering: the invariant on
  `ChunkRecord::seq` is never claim an origin and renumber, and two
  senders into one numbering-preserving store cannot both be honoured. So
  the header's numbering-preserving flag is load-bearing: set, the
  destination copies `origin_id` VERBATIM, preserves `seq`, and refuses a
  second sender; clear, it makes no origin claim and renumbers, which is
  what `export` does today. Either way the destination mints its own `id`
  — two stores sharing one is treated as corruption and refused, per
  "Which id travels" above, which is why the wire carries `origin_id` and
  the sender's `id` (the destination's `derived_from`) as separate fields
  rather than one. This is the transport for "Globally addressable chunks".

  **The header carries `provenance()`, not the whole `.bark`** — labels
  for routing, while the receiver keeps its own retention and index
  policy. Operational settings are the receiving tier's business.

  **Transport: multiple streams per connection in the protocol, 1:1 on
  the wire first.** The stream identity lives in the frame, not the
  connection: a `stream-open` frame binds a small stream id that every
  chunk and ack frame carries, so one-connection-per-stream is simply a
  connection with one open stream, and multiplexing later is a transport
  change with no format change, no version bump and no incompat bit.
  Pipelining is **per stream from day one** — a sender must not stall on
  each chunk's ack, so there is an in-flight window regardless, and
  scoping it per stream rather than per connection is what leaves the mux
  as bookkeeping instead of a redesign. Muxing waits because the price of
  it is flow control: without per-stream credit one slow store (a full
  disk, an fsync stall, an outsized chunk) head-of-line-blocks every other
  stream on the connection, which is strictly worse than N connections.
  N connections also give independent cursors, retry and backpressure for
  free, and match the intakes' existing thread-per-connection model.
  The case that will force muxing is a **dynamic store set** — 50
  containers on one host is 50 connects, 50 receiver threads and constant
  churn — with a control/pull direction (a central server asking a node
  what it holds) and per-stream TLS handshakes behind it. Note that
  HTTP/2 would supply muxing, flow control and TLS ready-made, at the cost
  of an async runtime and a TLS stack in a tree that today has neither and
  serves with blocking `TcpListener` plus `thread::spawn`; that is a
  dependency-posture decision, not a free win (see "OTLP gaps").

  **Latency puts a floor under it.** Only sealed chunks exist to ship, so
  this wire is one chunk-seal behind the live edge (256 KB or 5 s idle,
  whichever comes first). The `.sap` live tail is 0.2 s. That, not
  "transform versus not", is why the entry wire stays: frames replicate,
  records merge, transform and tail, and the two are chosen by latency and
  by whether the shape must change — not competitors.

  **Prerequisite (a bug today):** `import` discards a bundle's `.bark`
  entirely. A bundle carries `id`, `derived_from`, `derived_op` and the
  labels; the imported store comes out with no manifest at all, so
  identity and provenance do not survive the hop. Nothing that claims an
  origin can be built on that.

  **Validation stays a choice, not a default** — checking what arrives
  costs the decompression this design exists to avoid. Keep the cheap
  structural checks (ring records consistent with frame sizes) and leave a
  corrupt frame to fail at read, where `zstd -dc` is already the stated
  recovery path.

  This is the WRITE-direction complement to read-only serve; whether one
  endpoint family serves both representations — records for "I want to
  read this", frames for "I want a copy of this" — is worth deciding only
  once the read side exists.
- **Chunks fetched by address (manifest now, bytes on demand)**: the model
  is a TAPE. A store is an endless tape with a beginning — chunk #0, then
  an unbounded run of potential chunks (u64-bounded, which at 256 KB a
  chunk is not a practical ceiling) — and a node holds ZERO OR MORE RUNS
  of some tape. Two concepts, and only the second is new: a CONTIGUOUS
  piece of the tape, which is today's store, and an ordered list of
  FRAGMENTS with holes between them. Nodes stay equivalent either way —
  how much of a tape one holds is a deployment fact, not a kind of node.

  **The property that makes fragments meaningful is that a chunk carries
  its meaning alone.** It does, at the byte level: the zstd frame is
  independent, the portable rings fields (`uncomp_len`, `comp_len`,
  `first_write_ms`, `last_write_ms`, `seq`) are per-chunk, and a grain
  page is self-sizing. ⚠ It does NOT at the entry level — an entry may
  straddle a boundary, which `timberfs-records(5)` states ("a line split
  across two chunks reports the second") and `EntrySink` implements by
  carrying the trailing partial line across pushes. So reading across a
  hole would splice chunk N's tail onto chunk M's head and produce a line
  THAT NEVER EXISTED: not missing data but fabricated data, which is
  worse. A fragmented reader therefore needs a "next chunk is not
  adjacent" signal, and at a hole must terminate the partial line and
  report both ends as incomplete rather than joining them. That is the
  one real cost of the model.

  **The current format already expresses it**, which is the strongest form
  of not making today's files less useful: same files, same format, one
  loosened invariant. `seq` is SEARCHED and never computed from position
  (`position(|c| c.seq >= seq)`), `next_seq` comes from the last record
  rather than a count, `read_chunk` addresses by offset, and `.grain` is
  indexed by rings POSITION so gaps in seq do not disturb it. The delta is
  the splice above plus `dropped_chunks()`, which reads the count straight
  off `first_seq` and so conflates NEVER HAD IT with HAD IT AND DROPPED
  IT — today those coincide because only prefixes go.

  **Two concepts, two layouts** — the layout follows the concept instead
  of one being bent to serve both. A CONTIGUOUS piece of tape stays
  `.trunk` + `.rings`: one seek, a sequential read, the existing write
  path and the existing `zstd -dc <name>.trunk` promise. A FRAGMENT LIST
  wants a directory of frames, one file per chunk, named by number
  (Maildir-style, and the namespace is obviously NOT flat — see the open
  details below), with conversion between the two a defined operation:
  assemble a complete fragment set into a contiguous store, explode a
  store into fragments.

  **The fragment layout's use case is the CACHE** — a fetch-on-demand
  tier, holding whatever runs it has been asked for. That is what makes
  its trade-offs the right ones: insert and evict dominate, whole-store
  scans do not happen, and eviction is not loss because a peer still has
  the chunk.

  **And the caller never needs to know which it is.** The read API spans
  both layouts, and collections of them, presenting one view — which is
  what licenses having two layouts rather than forcing a single format to
  be adequate at both jobs. The abstraction boundary is the API, not the
  storage layer, and getting it there is a REQUIREMENT on the API rather
  than a hope: a query that must ask "is this a store or a cache" has put
  the boundary in the wrong place.

  What the directory buys is not mainly trivial insert and delete, though
  it has those (write tmp + rename is atomic, so no WAL; unlink returns
  the space at once, so `PUNCH_HOLE`, `COLLAPSE_RANGE`, skippable-frame
  stamping, offset rebasing, `.trim` and the seqlock ALL stop applying
  here). It is that the on-disk fragment can BE the wire frame: receive is
  "write these bytes to a file" and serve is `sendfile`, with no
  re-derivation of offsets into someone else's coordinate space on the way
  in and no reading them back out on the way out. Parallel fetches from
  different peers also stop contending, writing separate files rather than
  sharing one trunk's write lock.

  Iteration is the cost, and the FILENAME is what makes it bearable — the
  Maildir trick of putting in the name what you would otherwise open the
  file to learn. With `seq` and the write window in the name, `readdir`
  yields coverage, time spans and sizes with zero opens, so only the
  chunks a query actually needs get read; a time-range query is a
  contiguous run, and whole-store scans are the anti-pattern
  (`SELECT * FROM huge_table`) rather than the case to optimise. Merging
  adjacent fragments is then plain CONCATENATION, since zstd frames
  concatenate — `cat a b > ab`, and the merged fragment names a RANGE,
  which is the run concept again. That gives hot/cold tiering for free:
  many small files where inserts land, merged runs for cold data, which
  also retires the per-file slack (10.6% at the measured mean frame size
  of 25,919 B, plus an inode each). Remaining costs to size rather than
  discover: directory scaling (400k files for a 100 GB store is fine with
  `dir_index`; 10M wants sharding on the high bits of `seq`), and a
  recovery incantation that changes shape — `cat $(ls -v) | zstd -dc`
  instead of `zstd -dc <name>.trunk`, still stock tools but dependent on
  getting the sort right.

  For a contiguous store the record shape already answers the one layout
  question that is not a detail: `comp_start` and `uncomp_start` are
  separate fields, so keep `comp_start` dense (the bytes actually held,
  trunk stays compact) and let `uncomp_start` carry the ORIGIN's logical
  position. The hole is then real and visible in the uncompressed
  coordinate space, so a byte offset means the same thing on every node
  holding that tape — no sparse-file tricks and no new field.

  Note what a hole is NOT: rings referencing bytes that are absent. The
  trunk holds exactly the frames the node has — the hole is in the
  NUMBERING, not in the file — so `zstd -dc <name>.trunk` still prints
  every byte present and the standing recovery promise is untouched.

  **The address is what makes the runs fungible.** With `(origin_id, seq)`
  global, rings + `.grain` are a MANIFEST and the trunk is content to
  fetch when needed, from ANY holder, because the address says what the
  bytes are and not where they live. Two properties make that sound and
  both already hold: a sealed chunk is **immutable** — append-only, and a
  head-drop changes a chunk's OFFSET inside the trunk, never its bytes,
  the same fact behind "offsets never travel, lengths do" — so a cached
  copy cannot go stale; and the address is **location-independent**, so
  one holder is as good as another.

  **The payoff is query planning with no data.** `.grain` is per-chunk, so
  a `--has` predicate evaluated against a manifest names the chunks worth
  fetching before a byte of trunk moves: measured selectivity is 1 chunk
  of 136 for a rare identifier, against 136 of 136 for a substring scan.
  Sizes set the deployment shape rather than leaving it to taste — rings
  alone are ~0.2% of the trunk, rings + grain ~11% (264 KB against
  2.33 MB) — so rings for everything and grain for hot stores is a real
  configuration, not a hypothetical one.

  **The prerequisite is a digest, and it is cheap.** Nothing checks a
  chunk today: there is no per-chunk checksum, and the encoder
  (`zstd::stream::encode_all`) leaves zstd's own frame checksum off, so a
  wrong or truncated frame is undetectable. `(origin_id, seq)` is a NAME
  rather than a self-verifying hash, so the bytes need their own witness.
  Between TRUSTED peers that witness is about INTEGRITY — bit rot, a
  truncated transfer, a buggy tier — and not resistance to a lying holder,
  so it wants xxhash or crc32c and no crypto. Over the COMPRESSED frame it
  costs no decompression, so it does not contradict "validation is a
  choice" above: that caveat is about decoding CONTENT, not hashing bytes.
  Natural home is a sidecar kind, riding the manifest where old readers
  ignore it. Worth having whatever the fetch story becomes.

  **Discovery: ask trusted peers, then fetch from the best answer.**
  `WHOHAS (origin_id, seq)` to a known peer set, then a normal ranged
  fetch from whoever answers best — which keeps the trust boundary where
  the intakes already put it, and needs no DHT. The answer is a `coverage`
  response (a run list), not an index dump. Ranking mostly falls out: the
  asker measures RTT itself, so a responder only needs a cost hint for
  what RTT cannot see — cold storage, spinning disk, a tier that would
  itself have to fetch onward. Two things not to confuse with it. A UDP
  multicast stops at the first router, so it serves one VLAN and is a
  possible later TRANSPORT for this question rather than a design. And
  where the peer set is known and stable, tiers exchanging coverage on a
  schedule reduces WHOHAS to a LOCAL lookup, with the live query as the
  fallback for a cold or newly-joined peer. Gossip and DHTs stay out until
  membership is genuinely unknown.

  Telling NEVER HAD IT from HAD IT AND DROPPED IT is the same distinction
  as "renumbering destroys the evidence of a gap" above, which is what
  makes this a consistent extension rather than a new doctrine.

  **Three mechanics a sparse store changes, and none of them is a
  blocker.**

  *Retention still works, but stops being erasure.* Head-drop means "drop
  the lowest-seq run", so `retain` and `retain_size` need nothing new.
  What inverts is the consequence: on a contiguous origin store retention
  is ERASURE — irreversible, which is why the follower machinery exists —
  while on a fragment set whose peers hold the same chunks it is EVICTION,
  undone by a fetch. `retain_unconsumed` generalises with it, from "a
  follower has not read this" to "I MAY BE THE LAST HOLDER": the same
  interest floor from a different source, and under the same settled rule
  that interest is ADDITIVE, never a cap, or one stale catalogue entry
  pins the disk until it fills. Evicting from the MIDDLE is a different
  and easier primitive than head-drop — `PUNCH_HOLE` rather than
  `COLLAPSE_RANGE`, so nothing rebases and no offset moves — and
  `stamp_skippable_frame` is the precedent for keeping the trunk readable
  across the gap: stamp the 8-byte header, punch the rest.

  *Inserting a chunk in the middle* is easy for the frame (trunk tail, or
  a punched hole that fits) and a choice for the RECORD. Rewriting the
  rings in seq order is nothing at 90 chunks and O(N) per insert at ten
  million — hopeless for a cache that fetches constantly. Appending the
  record and SORTING AT OPEN keeps insertion O(1), and `read_index_file`
  already reads the whole file into a `Vec` so the sort is nearly free;
  the invariant spent is "records are sorted by seq on disk" becoming "the
  reader sorts", whose knock-on is `.grain` — indexed by rings POSITION,
  so pages land in insertion order, still consistent but harder to rebase
  on a head-drop. Either way `comp_start` stops being monotone with `seq`,
  which nothing requires (`read_chunk` addresses by `comp_start + len`)
  but which fragments `export`/`rotate`'s verbatim runs into more and
  smaller copies. Note the payoff of keeping the origin's `uncomp_start`:
  an inserted chunk fills the uncompressed gap EXACTLY, so uncomp order
  stays identical to seq order and offset-based addressing is untouched.

  *Streaming out of a sparse store* needs no protocol change: `coverage`
  advertises the runs, and the frames' own `seq` values are authoritative
  with the range as a bound. Streaming INTO one is the insertion case
  above. `--follow` on a sparse store means "tell me when fragments
  arrive" rather than tailing a live edge — and sparse is not exclusive
  with being an origin, since a node can lose middle chunks and keep
  producing.

  *Reading into a hole is an ERROR*, never a silent short read — and it is
  the natural trigger for a fetch. Which is the same fact as the splice
  hazard above seen from the read side: the reader must know where the
  hole is either way.

  **Deliberately open**, so none of the above is read as a spec: the
  fragment namespace (flat is wrong; sharding, and on what — `seq` high
  bits, origin, time — is undecided), what exactly a filename carries and
  what stays in a rebuildable index beside it, eviction POLICY for a cache
  (head-drop is one; recency or a last-holder check are others), when
  merging runs and who triggers it, whether the API exposes coverage to
  callers or only uses it internally, and how a cache miss surfaces —
  block and fetch, or answer with a declared gap. All of it downstream of
  the read API existing.

- **The receiving end: identity, names, and what selects a store**: one
  invariant governs a receiver — **one destination store, one origin** —
  and today it is violated silently through four separate doors, all
  measured against a live intake. (1) The default `--route service.name`
  merges two hosts' identically-named stores into one, whose `.bark` then
  labels it with whichever sender created it. (2) `sanitize_name` maps `/`
  to `_`, so `checkout/v2` and `checkout_v2` both answer 200 and land in
  one store labelled `checkout/v2` — the collision is in the LOOKUP KEY,
  not the layout, so no directory naming fixes it; injective encoding
  (percent-escape rather than replace) or comparing the batch's route
  value against the one the `.bark` already records does. (3) A REINSTALL:
  a fresh store on apache01 has a new id and numbering back at chunk 0,
  routes to the same value, and is appended to the existing store —
  measured, with `timberfs.store.id` still naming install #1, so the
  provenance lies about half the data. (4) `apache01.prod.foo.com` and
  `apache01.dev.foo.com`: systemd's `%H` and `timber-otlp`'s fallback are
  both `gethostname()`, conventionally the SHORT name, so both hosts
  present `host.name=apache01` and merge. ⚠ A routing template
  (`--route '{host.name}.{service.name}'`) fixes host-versus-service
  composition and does NOT fix this one — the value is identical on both
  boxes. Door 4 is what settles the design, because unlike the others it
  involves no misconfiguration and no reading in which the merge is
  wanted.

  **Key, labels, name — three things, currently conflated into a path.**
  The KEY is the origin store `id`: the only value both stable and unique,
  minted per store, and already what `follower create` and `cursor.rs`
  record ("by IDENTITY … not by path — a store can move"). LABELS are
  `host`, `host.fqdn`, `env`, `service`: mutable and non-unique BY DESIGN,
  which is exactly why a hostname cannot be a key — hosts get rebuilt,
  renamed, reused, and duplicated across environments — and equally why
  the fully-qualified name is no rescue: it is more unique and LESS
  stable, tracking DNS and search-domain config and routinely wrong in
  containers. A NAME is a system-friendly string for a store or a
  forwarder; it belongs in the manifest, never in a path.
  ⛔ **The path is therefore opaque**: a store lives at
  `/var/log/timberfs/<something unique>` and nothing should need to know
  which store that is. **timberfs is the tool that answers where a store
  is** — `list`, or reading the `.bark` files. Discovery is a readdir plus
  a manifest read per store, which is comfortable at the store counts in
  play; if it ever is not, an index maintained on add and remove is an
  implementation detail behind the same question, not a change to this
  model.

  **Selection, not naming, is the primitive** — and it is what removes the
  last operational reason to encode anything in a path. A NODE's store set
  is static; an ARCHIVE's is not, so with `--auto-create` a new sender's
  data arrives and forwards NOWHERE until somebody registers a follower
  for it. So a follower wants a predicate rather than a store:
  `follower create --select 'service=~apache-.*' loki-apache --type … `,
  or `--select '*'` for the whole forest. That expands to one child
  shipper per matching store, re-evaluated as stores appear — one child
  rather than one merged stream, because each store has its own chunk axis
  and so its own resumable position. Three consequences. Cursors become
  per (follower, store) and must be keyed by store ID
  (`followers/<name>/cursors/<store-id>.json`); keyed by name, a rename or
  a reinstall silently rebinds or loses a position. `retain_unconsumed`
  interest is then computed per store FROM a predicate, which makes the
  existing "an unreadable declaration fails closed globally" rule more
  load-bearing, since one bad predicate spans every matching store. And
  labels do double duty: the same `host`/`service`/`env` are the timberfs
  selector AND the downstream stream labels, since `timber-otlp` already
  sends them as OTLP resource attributes and Loki maps resource attributes
  to labels — one vocabulary end to end. The QUERY API takes the same
  primitive: its unit of work is a selection, so a response owes a
  COVERAGE statement (which stores it read, and what span each
  contributed), or "no results" cannot be told from "that selector matched
  nothing". ⚠ Two things to decide rather than discover: `--select '*'`
  with `--auto-create` lets SENDERS determine what reaches the downstream
  (a mistyped route value on one node creates a store that forwards
  without anyone choosing it), which wants a `--dry-run` showing current
  matches and argues for `--auto-create` being a deliberate archive-side
  choice; and whether the store id travels as a forwarded label at all —
  useful for "which store was this" and useless to query on.

  **A registration handshake, which the frames wire can have and OTLP
  cannot.** An OTLP sender POSTs and gets 200 or 503; there is no channel
  for "I already have that, sitting at 424242". On the native wire the
  exchange already exists — it is `stream-open` plus a coverage answer —
  so `follower create` performs one, prints the result and registers,
  turning every door above into a sentence on a terminal at setup time
  instead of a mislabelled store found weeks later:

      client -> stream-open  origin_id, my store id, labels{...}, mode,
                             my coverage 0..N
      server -> coverage     accepted, registration id <assigned>,
                             I hold 0..424242
             or conflict     that name/labels are held by origin <other>
                             at 0..424242

  The conflict taxonomy is small and each case has an answer: origin ids
  MATCH, so resume at the server's position (authoritative, so the client
  need not guess); origin ids DIFFER while labels collide, so this is a
  new tape and the operator chooses between replace, distinguish and
  mistake; the client is at 0 while the server holds 424242, which the
  origin comparison has already classified as reinstall or rewind; the
  client's oldest is newer than the server's newest, so there is a gap and
  the server can size it. On ids: the ORIGIN id must never be assigned by
  the server (minted at the origin, copied verbatim, or the address lies),
  while a server-assigned REGISTRATION id is a good idea — the receiver
  then names its own stores from something it controls rather than
  deriving a path from client-supplied strings, which is the same
  conclusion as namespace policy belonging to the receiver. Lookup stays
  by origin id, so a reconnect gets the same registration back.
  ⚠ Two things this must get right: the handshake happens on EVERY
  connect, not only at create — create-time is the operator-facing check,
  connect-time is the enforcement, and a store can be deleted or a name
  claimed in between — and it needs an offline escape (`--no-verify`) so
  provisioning a node while the archive is down is possible, with the
  conflict surfacing at first connect instead.

  **Adoption: re-id a store to continue a dead origin's numbering.** The
  reinstall case has a correct answer that looks like a violation and is
  not. The old install minted 0..424242 under origin O; the disk is dead
  so it can never mint again; the new install adopts O and starts at
  424243. No two byte sets ever share an address, so `(origin_id, seq)`
  holds. What makes it safe is knowledge the system CANNOT derive — that
  the previous minter is permanently gone — which makes this a FENCING
  decision and gives it exactly one failure mode: **split-brain.** If the
  old node was partitioned rather than dead, or its disk is later
  resurrected, two minters share one origin and both produce chunk 424243
  with different bytes, and the address lies permanently and
  undetectably. So adoption is an explicit operator act that states its
  assumption, and it must resist being baked into configuration
  management: a template that always passes `--adopt` is right on every
  rebuild and catastrophic on the one partition, which is the worst
  possible shape for a flag.
  ⚠ **Start above the FLEET, not above the server you asked.** The dead
  disk may have minted 424243..424250 and died before shipping them; if
  another tier received 424250, adopting at 424243 collides with bytes
  that DO survive. The safe floor is the highest seq any holder has, which
  is a coverage query across peers — the discovery mechanism above, doing
  write-path work rather than only serving reads. Whatever the dead disk
  minted and never shipped is lost, and leaving those numbers unused
  records that truthfully: a third state beside NEVER HAD IT and HAD IT
  AND DROPPED IT, namely MAY NEVER HAVE EXISTED. Reusing the numbers would
  be the lie; the hole is the accurate account.
  **This is the first real customer for the numbering BASE.** A store
  whose oldest chunk is 424243 has `dropped_chunks()` return `first_seq`,
  i.e. 424,243 chunks dropped when nothing was dropped — so adoption
  requires the base that "Globally addressable chunks" above argues the
  reserved header space wants: `first_seq - base`, with `base = 424243`
  giving zero.

- **OTLP gaps**: gRPC on :4317 wants HTTP/2 and an async runtime, so the
  answer stays "put a Collector in front" until something forces it.
  Metrics and traces remain out of scope by design. Smaller: a pre-created
  store keeps the operator's `.bark` untouched, so its resource attributes
  are never seeded — right, but it means the operator declares `service`
  themselves; `trace_id`/`span_id` land in the line as `k=v` rather than in
  dedicated fields on the way back out; and `timber-otlp` retries a
  retryable endpoint forever, which is right for a daemon and wrong for a
  one-shot replay in a script (a bounded `--max-retries`).
- **A write-time window for one-shot reads (`--written-from`)**: `query
  --from` means LOGLINE time in a windowed read but WRITE time in
  `--follow` — the axis switches with the mode instead of being chosen.
  Naming it would also give `timber-otlp` a durable one-shot drain
  (`--cursor` without `--follow`, for cron-style shipping), which it
  refuses today because only the follow path selects on the axis a cursor
  can safely resume from. The cheap first half of the zone-map entry above.
- **Arrival time on a received store**: both intakes stamp chunks with the
  sender's EVENT time, so a store written by a receiver has no arrival axis
  of its own — a sender replaying old events moves its chunk windows
  backwards, and a `timber-otlp --cursor` shipping that store onward would
  then skip rather than re-deliver. Storing arrival alongside (the zone-map
  sidecar, or a second ring) is what closes it.
- **Cursors beyond one consumer**: `cursor.rs` is a general "consumer's
  position in a store's entry stream", not an OTLP thing — a Kafka or Loki
  shipper would reuse it verbatim. Who is reading a store and how far
  behind is now surfaced — the FOLLOWER REGISTRY (below) shipped, so `list`
  carries a FOLLOWERS column, `info` the per-follower detail, and the
  shipper warns on a GAP; the deprecated `cursors` key is still honoured
  and reported as superseded. What remains open is ACTING on it —
  `retain_unconsumed`, in the same entry.
  The larger step is putting a cursor on the RECORDS stream
  (`query --follow --records --cursor`, a flag rather than a new binary —
  the tool boundary is destination-shaped, not format-shaped), because it
  turns durable consumption from a Rust API into a pipe contract: today
  resuming means linking `cursor.rs`, i.e. writing our own shipper, and
  after it anyone's script is one. What makes that non-trivial is that a
  pipe has no acknowledgement — `timber-otlp` advances only once the
  receiver accepts a batch, and the HTTP 200 IS the durability proof,
  where a write to stdout proves nothing. So the rule is **the cursor
  belongs to whoever can prove durability**, and it wants two primitives,
  not one: `--cursor` on the producer (advance on write-out — correct
  whenever the consumer is idempotent, which `import --records` is, so
  store-to-store replication is safe under it), and a consumer-driven
  position so one that fsyncs can own it and drive the producer, with no
  protocol at all. Half of that landed: `query --follow --from-chunk N`
  resumes at a chunk number, which is a resume key a script can hold rather
  than a window pair it has to compare. What is missing is a consumer that
  can pass it without linking `cursor.rs` — i.e. the pipe contract above.
  It is also exactly the resume primitive a remote live tail needs once
  read-only serve exists.
  Note the records path is the cursor-FRIENDLY one: `append --records`
  keeps the source's `wf`/`wl`, so the write axis survives the hop and
  stays meaningful across chained replication, where the OTLP path
  restamps (see "Arrival time on a received store"). The axis rule kept its
  shape but not its axis: a cursor is sound on the chunk-number axis, so it
  still requires `--follow` — only that path streams continuously — and
  still refuses a stdin stream, which has no chunks to number.
- **Retiring rings v1**: chunk numbers shipped in `RING0002`, and v1 is
  still read so that no store needs an operator step (a v2 writer migrates
  one when it opens it; a reader synthesizes the same numbers and migrates
  nothing). That support is not meant to be permanent: keep it for a stated
  grace period, then move the v1 reader into a standalone converter and
  drop it from mainline. Two details decide whether that lands well. After
  removal a v1 index must fail with a message NAMING the converter, not
  "bad magic". And the long tail is not live stores — those migrate on
  first write — but `.timber` BUNDLES and archives nothing has written to
  since: a bundle is read-only by design and cannot self-migrate, so the
  converter has to handle bundles, while `import` of an old bundle is free
  because it is writing a new store anyway. ⚠ Migration is one-way: once a
  store is v2 an older timberfs cannot read it, so a package rollback needs
  either the newer binary kept around or a v2→v1 downgrade in that same
  converter. Nothing is at risk but the choice of binary.
- **A resumable position for the live edge**: an entry read from the `.sap`
  carries no chunk number, its chunk not existing yet, so it is delivered
  and counted but moves no position — a restart re-reads from the last
  chunk boundary. That is bounded and permitted by at-least-once, and it is
  the reason nothing more was built. If the re-delivery ever costs enough
  to notice, the exact fix is for the `.sap` header to declare the number of
  the chunk its entries will become: the writer knows it, and a reader
  deriving it instead (`max(header_hwm, last.seq + 1)`) is unsound — a flush
  landing between the reader's rings read and its sap read labels those
  entries one chunk too low, and `n` then skips real entries on resume.
  Cheap when wanted: the sap is recreated on every seal-and-swap, so a
  format bump there needs no converter and no grace period, unlike the
  rings.
- **Retaining what a follower has not consumed** (SHIPPED: the registry,
  `retain_unconsumed` and `trim`): retention drops
  by age and by size. A frontend box wants a third rule — drop what is
  CONFIRMED DELIVERED — because two requirements hold at once there: keep
  as little log data on the box as possible (a breach reaches less of it,
  and "shipped off the edge promptly" becomes a statement that can be shown
  rather than asserted), yet never erase what has not landed elsewhere,
  including across a network outage. No time window satisfies both at any
  setting: `retain` is a bet on how long the link stays down, and the safe
  bet is the month of hoarding the requirement exists to avoid. Only
  delivery can decide, which is what a cursor already knows.
  **A follower is a replication slot.** Postgres arrived at the same shape:
  a slot holds WAL until its consumer confirms it, a slot name is an
  operator-chosen string unique per CLUSTER while the slot itself records
  which database it belongs to, and an unused slot pins WAL forever —
  whose fix, `max_slot_wal_keep_size`, is precisely the backstop below.
  Independent arrival at the same design is the strongest argument for it.
  So timberfs gets registered followers rather than cursors found by
  convention, and it remains a log with interest-based truncation, not a
  work queue: still position-based and at-least-once, no per-entry ack, no
  redelivery, no dead-letter.
  **The registry** (built): one directory per follower, named by the
  follower:
  ```
  /var/lib/timberfs/followers/<name>/
      follower.json    store, type, retaining, config   (operator writes)
      cursor.json      seq, n, delivered                (follower writes)
      follower.lock    held while it runs               (`run` acquires)
  ```
  The declaration and the position have different OWNERS and that is why
  they are two: the declaration is the operator's, the position is the
  follower's, and a cursor save is a whole-file tmp+rename that
  deliberately drops keys it does not own. One file would make every
  position write preserve operator fields, and would race `update`.
  The LOCK is a third file for the reason the store's writer lock is never
  its `.rings`: a cursor save replaces the inode by rename, and a lock on
  a renamed-over inode silently stops excluding anyone.
  Two things building it settled that this note did not have. `retaining`
  IMPLIES `--start begin`, because the shipper defaults to `end` and a
  retaining follower's first run would otherwise skip exactly the backlog
  it was registered to protect, which retention would then drop — derived,
  so an explicit `--start` still wins. And liveness cannot come from the
  lock alone: `run` clears FD_CLOEXEC so the lock survives the exec, but
  the shipper spawns its own reader, that child inherits the descriptor,
  and it can outlive its parent — so the lock says somebody holds it and
  the recorded pid says whether that somebody is the follower (exec
  preserves the pid, which makes it a proof rather than a pid file).
  `<name>` is host-unique and constrained to `[A-Za-z0-9_.-]`, so it needs
  no `systemd-escape` and is a legal directory name as-is — a UUID was the
  first instinct and is unusable in `systemctl status
  timberfs-follower@…`, which is where these names are actually typed.
  Validated and refused-if-taken at `create`, so a collision is caught at
  registration rather than by two processes overwriting one position — the
  failure that would let follower A advance past data follower B never
  sent, and retention then drop it.
  **The follower declares its store, by identity.** Flat names mean the
  relation is recorded once, by the party that knows it — like a slot
  recording its database — so the store keeps no follower list and there is
  no reverse index to fall out of sync. A store must therefore have a
  declared `.bark` id before its retention can depend on external state;
  `create` mints one, and a path would not do, a store being movable. The
  cost is that a writer's retention tick scans `followers/*/follower.json`
  and filters by store id rather than reading one directory.
  ⚠ **The mtime gate this entry proposed for that scan is WRONG, and wrong
  in the unsafe direction.** A directory's mtime moves when an entry is
  created, removed or renamed IN it — and both a position save and an
  `update` are a tmp+rename inside `followers/<name>/`, which leaves
  `followers/`'s own mtime untouched (measured). So the gate would miss
  every position advance, freezing the floor and making the axis silently
  do nothing; and it would miss an `update retaining=true`, leaving the
  store dropping data a newly-retaining follower should hold — which is
  the EARLY direction the "dropping late is harmless" licence does not
  cover. Built without it: one `read_dir` plus two small reads per
  follower, page-cached, once per tick FOR ALL STORES AT ONCE (which is
  cheaper than the gated per-store version it replaced), and not read at
  all unless some store declares the axis.
  **The rule.** `retain_unconsumed=true` on the store, and the name's
  polarity is deliberate: every `retain_*` key names what is KEPT
  (`retain=90d` keeps 90 days), so `retain_consumed` would have read as
  exactly the opposite of the behaviour.
  ```
  floor = min position over registered followers with retaining=true
  ki    = chunks.partition_point(|c| c.seq < floor)
  k     = max(age_k, size_k, ki)
  ```
  (1) Interest is **additive**, never a cap. Letting it CAP the drop would
  let one stalled follower pin the store until the disk fills, which kills
  the PRODUCER — losing the newest data to protect the oldest, strictly the
  worse trade. So `retain_size` is REQUIRED alongside, playing
  `max_slot_wal_keep_size`. ⚠ Which means the cap, not the consumption
  rule, is what decides an outage: it has to be sized as ingest-rate × the
  outage worth surviving. This does NOT remove that sizing — it removes the
  steady-state hoarding, the weeks of already-shipped bytes kept just in
  case, which is the actual win.
  (2) A registered follower with `retaining=true` and NO position yet holds
  everything. That is the point — it is what protects a follower deployed
  before it first runs, which the earlier find-cursors-by-convention design
  could not express, "nobody has ever read this" and "the file was deleted"
  being the same observation there. It is also the footgun, and the same
  one Postgres has: `create --retaining` without starting it pins the head.
  So `create` says so in one line, and takes `--enable`/`--start` (systemd's
  two verbs kept distinct) to make the safe path the easy path.
  (3) **Fail closed**, since each of these is indistinguishable from
  "consumed" if read wrong: no registry, an unreadable one, no follower for
  this store, a follower with no position, or one claiming `seq >=
  next_seq` — all drop nothing by interest. The last is newly PROVABLE
  rather than merely suspicious: a chunk number beyond what the store has
  ever written is a wrong anchor or a hand-edit, where a future timestamp
  was indistinguishable from clock skew. Built with one addition the note
  did not anticipate: an unreadable DECLARATION fails closed for EVERY
  store, not just its own, because it might have been a retaining follower
  of any of them and there is no way to know which. Harsh, and bounded by
  the additivity — age and size keep working, so the cost is dropping
  late — and loud, since `follower list` reports the same declaration.
  Also settled while building: the interest axis takes NO hysteresis where
  age and size have it. Promptness is the whole point ("what remains on the
  box after a successful ship is one chunk"), and the in-place collapse
  makes a per-chunk cut cheap.
  (4) When the cap overrides consumption the loss is **recorded exactly**,
  and this is a requirement rather than a nicety. Bounded loss is a choice
  already made — the alternative is blocking the producer, which for
  telemetry is worse than losing an hour of access logs — so what is owed
  is precise accounting at the moment it happens, and the writer holds both
  halves of the comparison: `retain_size (50G) reached with follower
  central at chunk 4200 — dropped chunks 4200..4830 it had not read`. The
  shipper's GAP warning is the same fact inferred later, from the other
  side; this one is exact.
  **systemd runs them; timberfs only dispatches.** A template unit per
  follower, `StateDirectory=timberfs/followers/%i` creating and owning the
  directory (its permissions matter: a file there can pin a store and,
  once erasure follows it, destroy one), and
  `ExecStart=timberfs follower run %i`, which reads the declaration and
  EXECs the right binary for its `type` — replacing its own process, so
  systemd keeps the lifecycle, the restarts and the journal. A dispatcher,
  not a supervisor; no daemon of our own. This is also what lets the
  registry hold configuration at all: the objection to storing a type and
  an endpoint was that something must then run them, and `exec` is that
  something at zero supervisory cost.
  **Lifecycle.** `create` (with `--retaining`, `--enable`, `--start`),
  `list`, `status`, `update`, `delete`, `run`. Retiring a follower is two
  commands on purpose, because the destructive act deserves its own:
  ```
  timberfs follower update central retaining=false   # releases, and says what
  timberfs follower delete central --stop --disable  # bookkeeping
  ```
  `update retaining=false` quantifies what it frees (`releases chunks
  4200..4830, 1.2 GiB, that it alone was holding`) and says the part that
  is easy to miss — the FLAG toggles but its EFFECT does not: setting it
  back to true will not bring the data back, and the follower resumes at a
  position that may now be gapped. `--dry-run` fits here, as on `rotate`.
  `delete` refuses while `retaining=true` (set it false first) and while
  the follower is RUNNING, which the held `cursor.json` reveals — deleting
  under a live process would leave it writing an unlinked file, silently
  doing nothing. Both refusals are about deliberateness rather than
  prevention (`update && delete` is still one line), so no `--force`: the
  two-step IS the force. One escape: a follower whose store no longer
  exists deletes freely, there being nothing to release.
  ⚠ **The lock never gates interest.** A follower that is temporarily down
  holds no lock and must still pin the head — that is the entire purpose of
  the spool. The lock detects collisions and reports liveness; it decides
  nothing about retention. Inverting that would turn "the shipper is down"
  into "drop everything it had not read".
  Two dependencies the guarantee rests on, neither of them ours: the
  receiver's `200` must mean PERSISTED, not merely accepted — a Collector
  with an in-memory queue acks and then loses the batch on restart, which
  silently voids the chain, since erasure follows the position and the
  position follows that ack; and the registry directory must not be
  writable by anything but the followers.
  `timberfs trim` shipped with it, load-bearing rather than convenient,
  since retention only runs inside a live writer and a store whose producer
  went quiet would otherwise keep delivered data indefinitely — and NOT the
  tempting shortcut of letting a follower collapse the head itself, which
  would make a reader a writer and put two of them on one head. It leaves a
  store somebody else is writing alone and says so: that writer's own tick
  is already doing this, and taking its lock away to repeat the work is the
  one thing it must not do.
  ⚠ **This superseded something already released**, and did so without
  removing it. 0.18.0 shipped the read-only half against a `cursors=<dir>`
  key on the store; the registry replaces it, since a follower declares its
  store and so the store declares nothing. `list`'s column became
  FOLLOWERS and `info` renders each source in its own block — declared and
  found-lying-in-a-directory are not the same claim. The key is honoured
  and reported as superseded wherever it is found, which is what a
  documented release is owed; it stays the way to register a NON-retaining
  tap for as long as that is worth having.
  Deliberately absent: a staleness rule expiring a ghost follower's
  interest (the registry makes a ghost discoverable by `list` and removable
  by `delete`, which beats a heuristic that cannot tell a dead follower
  from an idle one), and any priority or weighting among followers
  (`retaining` is the only tier, and it is a declared property rather than
  a consequence of where a file happens to sit).
- **Splitting downstream of a spool (fan-out by cursor)**: one store as the
  intake spool — everything a web server writes, the vhost in the line — plus a
  cursor consumer that routes entries into per-stream stores, instead of routing
  at the intake. The motive is not tidiness: routing AT a FIFO cannot be made
  safe, because only writes up to `PIPE_BUF` (4096 B) are atomic, so a torn long
  line lands half in another destination's store — a misroute the router cannot
  detect, i.e. exactly the answer an index must never give. Read a store
  instead and entry boundaries are exact (the appender framed them, `--records`
  states them), which turns the same routing mechanical. The write axis
  survives too: `append --records` keeps the source's `wf`/`wl`, so a split
  store's entries carry the spool's write window rather than the splitter's run
  time, and the pipeline lands in the derived store's bark — a per-vhost
  artifact then differs from a directly-written one only in lineage, which is
  the point of recording lineage. Needs the cursor tap above plus a routing
  sink (`append --into-dir --route ...`; the intake core already keeps N stores
  flushed, retained and indexed, so the sink is mostly wiring). What the shape
  buys beyond per-stream files: a routing rule can be FIXED AND REPLAYED from
  the spool by rewinding one cursor, where a router at the intake has already
  written its mistakes irrecoverably; several consumers can tap one spool
  independently (split, ship onward, derive an ERROR-only store), each with its
  own position; and the tiering thread gets its natural mechanism — a cold
  consumer that re-carves. It also makes the spool's retention the consumer's
  downtime budget, exactly as it already is for `timber-otlp`, with the same
  hazard the cursor entry raises from the other side: retention that outruns an
  unconsumed cursor drops data, which the consumer view now makes visible and
  a retaining follower (above) would act on. Costs
  are honest ones: every entry is written twice for the spool's retention
  window, there is one more thing to supervise, and per-stream stores compress
  WORSE than the spool they came from (~1.8x on a three-vhost sample, because
  each chunk is then filled by one site instead of all of them) — so a split
  earns its keep when a per-stream artifact is the goal, not as a default
  layout. Open: whether the router is a flag on `append` or its own verb;
  whether a routing key is a field index, a regex capture, or a records-metadata
  field once senders can set one; and what an unroutable entry deserves — a
  fallback store, refuse-and-stall, or drop-with-count (the intakes' "refused,
  logged once, never acked" precedent argues for the first, since a spool makes
  the data recoverable either way).
- **Watchers (reactive rules)**: evaluate a predicate continuously over the
  append stream and fire a configurable action on a match — a single entry (an
  `OutOfMemoryError` is logged), a windowed count (more than N errors in M
  minutes), or a sequence with a deadline (an error with no matching recovery
  within ten minutes → escalate). The MVP is already a pipe
  (`query --follow … | timber-filter … | your-action`); a built-in form would
  add configuration, durability, and an event-time engine — which, being
  source-agnostic, could replay against stored logs to test a new rule against a
  past incident before trusting it.
- **zstd seekable format / dictionaries**: adopt the official seekable-zstd
  framing for ecosystem compat; train a dictionary per file for much better
  small-chunk ratios; long-range mode for cold recompression.
- **Cold-chunk recompression**: rewrite old chunks at zstd -19 in the
  background; the index makes this a local, safe operation.
- **Scheduled rotation**: a `timberfs rotated`-style timer (or systemd timer
  recipe) driving `rotate --cutoff`/`--delete` policies per file.
- **Appender growth toward s6-log**: `SIGHUP`-triggered and scheduled
  rotation into dated files (for shipping archives off-box), optional line
  timestamping, and a `--tee` passthrough.
- **Live follow for the other readers**: `query --follow` now tails the
  `.sap`, so a wal-declared store is followed at poll latency instead of
  at `--flush-age`. What has not caught up: `timber-filter` has no
  `--follow` of its own (a live `query --follow --records | timber-filter`
  pipeline works, but its record stream ends without a stream-end, which
  the reader is right to call truncation — a live stream needs a way to
  say "still going" that a torn one cannot); a mount still shows the
  buffered tail only through its own daemon; and a future network `serve`
  has to decide whether a live edge travels as records or as sap frames.
- **Read-only mount of a live store**: a mount takes the backing directory's
  lock exclusively, so a store with a live appender cannot currently be mounted.
  A read-only mount takes no writer lock and only reads the append-only
  trunk/rings — the same lock-free read `query`/`info` already do — so it could
  coexist with a running appender, exposing a being-written store as an ordinary
  filesystem path (`tail`/`less`/`grep` on `/mnt/app/app.log` as it fills). Pairs
  naturally with the live sidecar above; retention's tail-rewrite is the
  coherency case to handle.
- **Expose the index in-band**: a virtual `.idx` twin file or ioctl so tools
  can query through the mount without knowing the backing dir.
- **tail(1) fast-path**: negative-offset "time seek" via `llseek` hooks.
- **Subdirectories, multi-writer O_APPEND atomicity, runtime rescan of the
  backing dir, real statfs passthrough.**
- **Kernel port**: the store layer is FUSE-free by design; revisit
  Rust-for-Linux filesystem bindings when they stabilize.
