# Roadmap / ideas

A backlog of directions for timberfs — not commitments. How the current design
works is in [docs/design.md](docs/design.md); entries too large to state in a
paragraph get a design note under [docs/plans/](docs/plans/) and a pointer from
here.

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
- **What the LogQL spike measured** (the branch is gone; this entry is the
  evidence): enough of Loki's read API for a real Grafana 11.3.0 to point
  at a forest. It is the reason the facade above is dropped, and the
  measurement is the part worth keeping — a negative result that stops the
  wrong build is worth more than an estimate, and it does not need the code
  that produced it. Rebuilding the spike from what is written here would
  cost an afternoon, which is the right price for evidence nobody has
  needed since.
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
  **The live edge has no chunk NUMBER**, `EntryRec.chunk` being `None`
  there until one exists — but it does have an address: the segment
  states where its bytes sit in the store's logical stream, and those
  bytes are the next chunk's, so the offset an entry reports there is the
  one that chunk will report for it. A `wal`-backed follower delivering
  sub-second can therefore be resumed past; what becomes available one
  chunk later is the citation `(origin, seq)`, not the position.
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
  Measured on 200k apache entries, the bytes are a wash against gzipped
  OTLP (2.34 MB vs 2.58 MB) while the CPU is not: 0.04 s against 6.17 s,
  and that work lands on the machine serving production traffic. Framed,
  sidecar-extensible, and multiplexable in the protocol before it is on
  the wire. Design note:
  [docs/plans/native-replication.md](docs/plans/native-replication.md).
- **Paging a bounded search** — SHIPPED in 0.24.0. A `position` record per
  examined store carries an absolute offset on that store's tape, and
  handing them back as the request's `cursor` resumes exactly there. A
  `deadline` bounds how LONG rather than how much, and is answered with the
  work done rather than abandoned. The **service-imposed limits** followed:
  ceilings that are ON by default (100k entries, 1k chunks, 30 s) and
  overridden in `/etc/timberfs/limits.conf`, bounding a DOCUMENT and not
  the flags beside it, announced in every answer that has somewhere to put
  them rather than discovered by having a request refused. Paging is what
  makes a default defensible: a `max` or `deadline` over a ceiling is
  lowered and the answer is then the first PAGE, with `stream-end` naming
  the ceiling apart from the request's own bound; a `tail` is refused
  instead, carrying no position to continue from. An unusable line is
  skipped and named, never fatal — `timberfs query` has no startup, so a
  refused policy file would answer every caller with an error they cannot
  fix; `timberfs limits` is that check, and the read-only serve below is
  where the strictness moves. Design note:
  [docs/plans/paging.md](docs/plans/paging.md).
- **`view`: reading a store as a tape** — SHIPPED, a first version. A pager
  over chunks that opens at the last one and scrolls back, with no
  predicate and no result set, for a loop a result set cannot close: point
  at an identifier, search it across every host, jump to the coordinate a
  hit comes back with. Lines rather than entries, so it needs no index and
  no parseable timestamps — the two cases where `select` helps least. It
  is one module reached two ways, `view` in timbersh and `timberview(1)`
  beside it, over four operations and nothing else, and a coordinate now
  has a written form (`timber://host/id#offset=N`) that names a store by
  IDENTITY. An ANSWER is read on the same screen — `select ... into view`
  in timbersh, or a piped `records` stream — because an entry record
  carries the offset it came from, so `Enter` leaves the answer for the
  log around a match. A SELECTION comes away as text: `m` sets the mark and
  `c` copies the region, or the whole entry under the cursor where no mark
  is set — a stack trace into a stack-trace analyser, which is one entry
  and forty lines. The route is a clipboard helper where there is a display
  for one and OSC 52 otherwise, since the case that matters is a
  workstation paging a fleet over ssh; OSC 52's failure is silence, so the
  status line says which route was taken and a copy neither route could
  make is written to a file rather than lost. What remains: "what happened
  in $FOO around here", the drill-down that starts from where you ARE and
  collides with the viewer's no-parsing rule; a selection as the DOCUMENT
  that reproduces it rather than as the bytes it produced; and an answer
  screen that EXTENDS — it is a closed set today, so it takes one page and
  says the answer continues, where the tape fetches the next chunk as you
  reach it.
  **An investigation window that outlives the statement** — DONE:
  `create session from X to Y` in timbersh bounds every statement (narrow
  within it, never out of it) and the pager with them, and `t` changes it
  from inside the viewer. The two clocks land differently on the two halves,
  so the tape's bound is the WRITE axis widened and its edge says so. What
  it opens up is the part still to build: a bounded window is a small enough
  set to hold whole, which is what makes sorting an answer by the clock its
  entries carry affordable — `logline-order.md`'s "sort in the consumer over
  a page it has whole", without the index field the streaming merge needs. Design note:
  [docs/plans/view.md](docs/plans/view.md).
- **A fleet is a list of TARGETS, and a resolver derives it** — SHIPPED.
  `TIMBERFS_CMD` with `_TIMBERHOST_` in it made the transport a property of
  the SESSION, so every host had to be reached the same way and an `ssh`
  could not sit beside a site wrapper taking the host as an argument. A
  target is now a name and the way to reach it; `$TIMBERFS_RESOLVER`
  is any command printing that list, `~/.config/timberfs/targets.json` holds
  the same document, and the old variables are one producer of it rather
  than the only way to describe a fleet. A resolver that failed is fatal and
  an empty fleet refused, because a session that quietly asks the local
  machine looks exactly like a fleet that held nothing; a target this build
  cannot reach is named rather than dropped.
  A target reaches its timberfs by `cmd` or by **`url`** — POSTed the
  document, streamed the answer, over TCP or a unix socket written into the
  url as `unix+http://[host]/path/to.sock//request/path`. `//` is the
  boundary because it is the one sequence that cannot MEAN anything inside a
  filesystem path (POSIX collapses repeated slashes), where `:` or `#` can
  legally be part of one and would split in the wrong place silently; and it
  is deliberately not `http+unix://`, which is requests-unixsocket's and
  spells the socket percent-encoded in the authority — a grammar that needs
  no boundary but has nowhere left to put a `Host:`. The gap it leaves is
  real and stated: a url target has no
  stderr, so timberfs's explanations — the sentences that say why an answer
  looks wrong — arrive only out of a failed response's body. Closing that
  means deciding what a timberfs server puts on the wire, which is the
  read-only-serve entry above and stays open. Nothing here serves such a
  URL; these are clients for endpoints that already exist.
  A RESOLVER is a command or a url too, told apart by the scheme, and asked
  with a GET since it takes no arguments — so a local agent that derives the
  fleet needs no command wrapped around it. Re-deriving is its own asker
  where it needs to be (`--refresh`, `$TIMBERFS_REFRESH`, the document's
  `refresh`, each a `cmd` or a `url`), since a full sweep to open a session
  with and a cheap re-ask are not the same call. `--targets` stays a file: a
  url derives its answer, a file is a thing you edit.
  What remains is the other half of DNS — "who has this store",
  deliberately not designed yet, since broadcast is fine at the measured
  fleet. Design note: [docs/plans/view.md](docs/plans/view.md).
- **Ordering by the clock an entry CARRIES**: a bounded answer reads stores
  one after another and claims no order between them, because a streaming
  merge can only key on arrival — the one key a store is already sorted by.
  Logline order is reachable with a frontier merge, where memory is
  proportional to the OVERLAP rather than to the data, but it rests on an
  index field that does not exist: a per-chunk logline range. That field
  pays for itself first, by replacing the 60-second `WIDEN_MS` guess in
  logline selection with the answer. Design note:
  [docs/plans/logline-order.md](docs/plans/logline-order.md).
- **Chunks by address (a manifest now, bytes on demand)**: a store is a
  TAPE of which a node holds zero or more runs — a contiguous piece
  (today's store) or an ordered list of fragments with holes. With the
  address global, rings plus `.grain` are a manifest and bytes can be
  fetched from any holder, which makes query planning possible with no
  data moved. Needs a digest, and a cache that is not a store with holes.
  Design note:
  [docs/plans/chunks-by-address.md](docs/plans/chunks-by-address.md).
- **The receiving end: identity, names, and selection**: an archive
  violates "one destination store, one origin" through four measured doors
  (route collision, sanitized-name collision, reinstall, and two hosts
  sharing a short hostname). The resolution separates the KEY (the origin
  store id) from LABELS (host, env, service — mutable and non-unique by
  design) from a NAME (a system-friendly string, in the manifest and never
  in a path), which makes the store path opaque; plus selection as the
  primitive for forwarders and the query API, a registration handshake,
  and adoption of a dead origin's numbering.
  Design note: [docs/plans/receiving-end.md](docs/plans/receiving-end.md).
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
- **A resumable position for the live edge** — SHIPPED, and by the axis
  rather than by the number. An entry from the `.sap` carries no chunk (one
  does not exist yet) and now states its `offset` anyway: the tape is a
  byte stream, the segment is its last stretch, and `chunk` is a fact about
  the container while `offset` is the address. So the two are written
  apart, and a consumer resumes past an entry it was shown before any chunk
  held it — measured against a store whose newest lines were in no chunk at
  all.
  The address comes from the segment's OWN header (`uncomp_base`, which
  `sap.rs` had already called "the planned live-tail reader's realignment
  anchor") plus what the reader has taken out of it, never from the ring
  index: a flush landing between a reader's rings read and its sap read
  creates a new segment further along, and a derived base would place those
  entries a whole chunk too low. That is the same unsoundness this entry
  used to describe for a derived chunk NUMBER — it applies to any derived
  coordinate, and the fix is to read the writer's own statement.
  No format bump was needed: the header already carried the field, and a
  head trim rewrites it in place (`Sap::refresh_base`), so the address
  follows a rebase by exactly what leaves the store and the TAPE offset
  does not move.
  ⚠ What stays with the chunk is durability, not position: the sap is
  readable at `flush` and durable at `sync`, so a live address is exact and
  survives as far as the writer's last sync — the bargain `tail -f` makes.
  The other half shipped with it: a read that RESUMES — a cursor and no
  window — is served the segment as well as the chunks, so a polling
  consumer no longer waits out the writer's flush age for data already
  durable (measured: 0.03-0.64 s against a 20-second flush age). A windowed
  read still stops at the chunks, and so does a chunk-granular predicate
  sweep: one selects by a write window the segment has not got, the other
  by an index that has not covered it.
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
- **A follower of a SET of stores**: forwarding N stores to one destination
  is N declarations, N units and N processes today, with the destination
  stated N times. So the subject of a follower becomes a SELECTION — the
  same `[]` predicate `list --select` and the query document already take —
  and a single-store follower is the one-term case `id=<the store's id>`.
  One process serves the whole set: a request carries a position per store
  and an answer returns one, so the reason the earlier design gave for one
  child shipper per store (each store's own chunk axis) no longer holds.
  What it costs: `positions.json` in place of `cursor.json`, the interest
  axis evaluating selectors against each store rather than matching an
  anchor, OTLP's `resourceLogs` carrying one group per store, and one
  frames connection per store from one process until the wire is
  multiplexed. Deliberately NOT a new object — no member of the set is ever
  named, enabled or locked — which leaves "follower group" free for the
  thing that has members: several processes sharing one selection, each
  taking a shard.
  Design note:
  [docs/plans/follower-selection.md](docs/plans/follower-selection.md).
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
