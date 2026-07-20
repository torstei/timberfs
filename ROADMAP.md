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
- **Real-time follow (a `.live` write-ahead sidecar)**: `--follow` today only
  sees entries once their chunk is flushed and compressed, so it lags by the
  flush interval — the unflushed tail lives only in the appender's memory. A
  small write-ahead sidecar holding the current chunk's raw bytes would let a
  follower read the live edge immediately (drain the compressed store, then tail
  the sidecar; a follower that falls behind drops back to the store and catches
  up). It doubles as crash durability for the not-yet-flushed buffer, which an
  appender crash currently loses.
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
