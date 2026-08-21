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
  the existing predicates, grain-accelerated as locally. The protocol is
  mostly already written — the control plane is what `--json` already
  emits, and the data plane is a **timberfs-records(5) stream**, whose
  stream-end totals prove the response arrived complete, which a bare HTTP
  body cannot. HTTP, not gRPC, for the reason `otlp-intake` gives. The
  client then stays thin and composable: a remote selection pipes into
  `timber-filter` and `import --records` unchanged, so the
  investigation-as-artifact workflow works across the network with the
  tools that already exist. Two things to design in rather than bolt on:
  a **cost preflight** (chunk selection precedes decompression, so the
  server can state how many chunks and roughly how many bytes a query
  would read *before* reading them — most log servers cannot answer that
  without doing the work), and **fleet shape**: the client fans out to N
  hosts and merges, exactly as multi-file `query` interleaves today. No
  proxying server, no cluster, no leader. A Loki-compatible facade — the
  compatibility bet worth taking for Grafana and its client ecosystem —
  layers *over* this, never under it: LogQL's label model would flatten
  the entry and two-clock semantics if it were the core. Concurrency needs
  nothing new; a server is just another standalone reader, already covered
  by the collapse seqlock and the grain/rings generation check.
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
  store through OTLP — or through a records stream — throws away the
  compression already paid for at ingest: both decompress at the source,
  send plaintext, and make the receiver spend CPU compressing it back into
  something close to what was sent. Moving the `.trunk` frames verbatim
  reuses that work twice, for storage and for transport, turning
  replication into a byte copy at roughly a tenth of the bandwidth. Most
  of the format exists: a `.timber` bundle is already tar(`.rings`,
  `.trunk`, `.bark`) and `import` takes it directly, so this is the
  incremental, streaming form of the batch artifact sawmill already plans
  to accept over HTTP. Chunk boundaries survive the hop, which is also
  what would let a `.grain` travel — the index is chunk-positional, so it
  can only be shipped by a transport that preserves alignment (and is why
  bundles COULD carry one; they don't yet). Three constraints define it:
  (1) the resume key is a **write window plus lengths, never a byte
  offset** — a retention head-drop shifts every offset, and windows and
  lengths are exactly what `read_chunk` already re-locates a chunk by for
  that reason; (2) raw is **1:1 mirroring, not fan-in** — frames are
  opaque and the rings must stay sorted, so interleaving two sources into
  one store requires decoding, which is the records path's job. Raw
  mirrors, records merge and transform; the two transports are for
  different jobs, not competitors; (3) validating what arrives costs the
  decompression the design exists to avoid, so verification is a choice,
  not a default — trust the link as the intakes do, keep the cheap
  structural checks (ring records consistent with frame sizes), and leave
  a corrupt frame to fail at read, where `zstd -dc` is already the stated
  recovery path. This is the WRITE-direction complement to read-only
  serve; whether one endpoint family serves both representations — records
  for "I want to read this", frames for "I want a copy of this" — is worth
  deciding only once the read side exists.
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
  behind is now surfaced (a store declares a `cursors` directory; `list`
  gains a CONSUMERS column, `info` the per-consumer detail, and the
  shipper warns on a GAP), so what remains open is acting on it — see
  interest-based retention below.
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
- **Interest-based retention (a third axis)**: retention drops by age and
  by size. A frontend box wants a third rule — drop what is CONFIRMED
  DELIVERED — because two requirements hold at once there: keep as little
  log data on the box as possible (a breach then reaches less of it, and
  "shipped off the edge promptly" is a statement that can be made and
  shown rather than asserted), yet never erase what has not landed
  elsewhere, including across a network outage. No time window satisfies
  both at any setting: `retain` is a bet on how long the link stays down,
  and the safe bet is the month of hoarding the requirement exists to
  avoid. Only delivery can decide, which is what a cursor already knows.
  This is NATS's Interest retention, and it makes timberfs a log with
  interest-based truncation, not a work queue: still position-based and
  at-least-once, with no per-entry ack, redelivery or dead-letter.
  **The classic problem, minus the quantum.**
  Ship-then-delete is as old as log rotation, and its exposure quantum has
  always been the ROTATION INTERVAL — because rotation couples erasure
  granularity to the producer's file handle. Every shortening buys
  exposure with a coordination event: SIGHUP and reopen, or `copytruncate`
  and its race that loses whatever was written between the copy and the
  truncate. The prior art gets closer than it is given credit for.
  `rsync --remove-source-files` unlinks only after a confirmed transfer —
  delivery-gated deletion, decades old. A shipper's registry (filebeat,
  fluentd's `pos_file`) is a cursor by another name, computed locally from
  acks. Neither can delete the delivered PREFIX. rsync derives its hold by
  COMPARING AGAINST THE DESTINATION, so the edge needs read access to the
  archive — trust pointing the wrong way for this threat model, since a
  breached frontend then holds a key to wherever the data went — and it
  unlinks whole files, which drags rotation back in through the side door.
  A registry avoids both of those, but a plain file has no prefix-removal:
  `FALLOC_FL_COLLAPSE_RANGE` is available to anyone, and what a plain log
  lacks is a FORMAT THAT SURVIVES LOSING ITS HEAD, since every reader's
  byte offsets shift and nothing rebases them.
  So what is new here is neither the cursor nor the erasure but their
  composability on top of design.md's fourth property, "delete from the
  front": chunk framing plus the rings index IS that format, and this is
  the second classic file problem that primitive makes easy rather than
  hard. Two consequences worth stating. Delivery is entry-granular
  (sub-second with `wal=true`, via the `.sap` live edge) while erasure is
  chunk-granular, so what remains on the box after a successful ship is
  ONE CHUNK — tunable with `--chunk-size`/`--flush-age`, with the producer
  uninvolved — against one rotation interval. And the edge needs
  PUSH-ONLY credentials, which is exactly what rsync-with-hold cannot
  offer.
  **The decisions.**
  The mechanism is nearly free — `cursor::consumed_prefix` already
  computes the drop-eligible chunk prefix for a cursor (the exact
  complement of what `Resume` would deliver), `enforce_retention` already
  reduces every axis to such a prefix, and head-drop is already a prefix
  operation.
  (1) Interest is **additive**: `k = max(age, size, interest)`, never
  `min`. Letting interest CAP the drop would let one stalled consumer pin
  the store until the disk fills, which kills the PRODUCER — losing the
  newest data to protect the oldest, strictly the worse trade. So
  `retain_size` stays a hard budget and a store declaring interest must
  declare one too. ⚠ Which means the cap, not the consumption rule, is
  what decides an outage: it has to be sized as ingest-rate × the outage
  worth surviving. Interest retention does NOT remove that sizing — it
  removes the STEADY-STATE hoarding, i.e. the weeks of already-shipped
  bytes kept just in case, which is the actual win.
  (2) When the cap overrides consumption, the loss is **recorded exactly**,
  and this is a requirement rather than a nicety. With finite disk,
  bounded loss is a choice already made — the alternative is blocking the
  producer, which for telemetry is a worse outcome than losing an hour of
  access logs — so what is owed is precise accounting at the moment it
  happens, and the writer holds both halves of the comparison right
  there: `retain_size (50G) reached with consumer otlp 6d 2h behind —
  dropped 4831 chunks covering <from> .. <to> that it had not read`. The
  shipper's GAP warning is the same fact inferred later, from the other
  side, bounded only by timestamps; this one is exact.
  (3) **Fail closed** everywhere: no cursor found, an unreadable one, or
  one anchored to another store all drop nothing by interest — the minimum
  over an empty set is 0, not infinity.
  (4) Nothing about how the store was written, and nothing about arrival
  time: both were only ever proxies for a write axis that could move
  backwards, and cursors no longer ride it. The droppable prefix is
  `cursor::consumed_prefix` — a subtraction over chunk numbers — so the
  hazard a provenance test was meant to exclude does not exist to be
  excluded. This was the prerequisite, and it shipped.
  Two things the guarantee rests on, neither of them ours: the receiver's
  `200` must mean PERSISTED, not merely accepted — a Collector with an
  in-memory queue acks and then loses the batch on restart, which silently
  voids the whole chain since erasure follows the cursor and the cursor
  follows that ack; and the cursor directory must not be writable by
  anything but the shipper, or an attacker with that account (a read-only
  role today) can fast-forward every cursor and have the next tick erase
  the record of their own intrusion.
  Also wanted: a one-shot `timberfs trim`, load-bearing rather than
  convenient, since retention only runs inside a live writer and a store
  whose producer went quiet would otherwise keep delivered data
  indefinitely — and NOT the tempting shortcut of letting the shipper
  collapse the head itself, which would make a reader a writer and put two
  of them on one head. Dropped on purpose: a declared consumer roster
  (only earns its keep at two or more consumers, so it stays optional
  until there are), and a "keep 15m even when consumed" floor for local
  forensics (it competes for the same disk that buffers an outage, and
  delivery wins).
- **`head -f`: following the erasure edge (the drop journal)**: in every
  other filesystem a file's head is fixed by construction — POSIX has no
  prefix-delete, so `head` has no `-f` and never could. Here the head
  MOVES, and under interest-based retention it moves as a function of
  delivery, which makes following it a meaningful thing to do: it is
  watching what leaves the box. That is the streaming form of the exact
  loss record the entry above requires — one event per drop, carrying the
  window (`<from> .. <to>`), its size, and the REASON: `consumed`
  (delivered and then erased, the healthy case), `age`/`size` (the
  declared ceiling doing its job), or `cap override` naming the consumer
  that had not read it.
  ⚠ The obvious implementation is the wrong one. A reader tailing the head
  can only observe that data VANISHED — never why, never by whose
  authority — and it silently merges two drops that fall between polls, so
  it can be neither complete nor attributed. Only the WRITER holds both
  halves of the comparison at the moment it acts, so this record is
  EMITTED, not observed: the slogan names the property, the implementation
  lands on the other side of the file. Which also makes it cheap — writers
  already log their retention actions, so making those structured and
  complete is most of the value with no new API surface.
  Open: whether that is enough, or whether the journal deserves to be a
  store of its own (it is a log, and this project has opinions about where
  logs go). Either way it must go OFF-BOX — a record of loss that dies
  with the box it was written on proves nothing — which the systemd
  journal already gets for free wherever that journal is itself shipped.
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
  interest-based retention above would act on. Costs
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
