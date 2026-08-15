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
  trunk as its own timestamp index; this would only accelerate it.
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
- **OTLP intake gaps**: `otlp-intake` receives OTLP/JSON over HTTP; still
  open are protobuf request bodies (the Collector's default encoding, so
  `encoding: json` is required today), gzip request bodies (likewise
  `compression: none`), and gRPC on :4317 — which wants HTTP/2 and an async
  runtime, so the answer stays "put a Collector in front" until something
  forces it. Metrics and traces remain out of scope by design. Smaller: a
  pre-created store keeps the operator's `.bark` untouched, so its resource
  attributes are never seeded — right, but it means `timberfs create` +
  `--route` needs the operator to declare `service` themselves.
- **`timber-otlp` gaps**: protobuf request bodies (the Collector's default
  encoding — and the RESPONSE is free, since an empty
  `ExportLogsServiceResponse` is zero bytes); gzip request bodies;
  `trace_id`/`span_id` and structured attributes lifted into LogRecord
  fields instead of staying inside the body text (`--has <trace_id>` over
  the `.grain` index already finds a trace's lines with no trace backend);
  and a bounded `--max-retries`, since retrying forever is right for a
  daemon and wrong for a one-shot replay in a script.
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
  shipper would reuse it verbatim. Open: a `list`-style view of who is
  reading a store and how far behind they are (the state directory knows,
  nothing surfaces it), and whether a cursor should ever hold retention
  back so a disconnected consumer cannot be truncated out from under.
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
- **Real-time follow, phase 2 (on the `.sap` substrate)**: phase 1 —
  durability — shipped as `--wal`: a `<name>.sap` write-ahead sidecar holds
  every appended entry raw, fsynced once a second, so a crash loses at most
  that tick instead of up to `--flush-age`. `--follow` today still only sees
  entries once their chunk is flushed and compressed, lagging by the flush
  interval; phase 2 is a follower reading the sap's live edge directly
  (drain the compressed store, then tail the sap; a follower that falls
  behind drops back to the store and catches up) for sub-flush-age
  `--follow` latency.
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
