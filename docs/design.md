# How timberfs works

Under the "log filesystem" framing, timberfs is a small, general idea: a
**chunked store** with **per-chunk compression**, **per-chunk metadata**, and
**efficient deletion from the front**. Those four properties are the whole
thing; a log filesystem is just their flagship application.

- **Chunked** — the file is a sequence of independent chunks: the shared unit
  of compression, indexing, retention, and random-access read.
- **Per-chunk compression** — each chunk is one self-contained zstd frame, so a
  chunk decompresses alone and stock `zstd -dc` recovers the whole store.
- **Per-chunk metadata** — a fixed-stride record per chunk (`.rings`: its
  write-time window; `.grain`: a token Bloom filter) turns "scan everything"
  into "read only the chunks that can match."
- **Delete from the front** — drop the oldest chunks without rewriting the rest
  (`fallocate(COLLAPSE_RANGE)`) — the one primitive a log workload needs that
  POSIX lacks, and what makes this a *filesystem* for logs rather than a
  rotation scheme.

The rest of this document is how the store earns those properties. You don't
need any of it to *use* timberfs — the README covers that; this is for the
curious and contributors.

## Why FUSE (and not overlayfs / a kernel module)

- **overlayfs** layers *namespaces* (upper/lower directories, as used by
  container images). It has no hook for transforming *content*, so it can't
  compress on write or maintain an index. Wrong tool.
- **A native kernel filesystem in Rust** is where Rust-for-Linux is heading,
  but the filesystem bindings are still experimental. Not a good vehicle for
  iterating on a design.
- **FUSE** gives us the full VFS interface in userspace: loggers append
  through the mount unmodified, `tail -f`/`grep`/`less` all just work, and
  the implementation is ordinary safe Rust (`fuser` crate, no libfuse
  dependency — it only needs the `fusermount3` binary at runtime).

The design cleanly splits into a *store* (file format + chunking, no FUSE
types) and a thin FUSE layer, so the store could later be re-hosted in a
kernel module, a `LD_PRELOAD` shim, or a log-shipping daemon without change.

## On-disk format

Each logical file `<name>` is backed by two files in the backing directory:

```
<name>.trunk   concatenated zstd frames, one per chunk, no wrapper bytes
<name>.rings   8-byte magic "RING0001", then 48-byte records (all u64 LE):
              uncomp_start | uncomp_len | comp_start | comp_len
              | first_write_ms | last_write_ms
```

(The names take the timber metaphor seriously: the data is the trunk, and
the index is its growth rings — which really are a write-time index;
dendrochronology dates events by rings exactly the way `timberfs query`
dates bytes by chunks.)

Because the `.trunk` is a plain zstd frame concatenation, **stock tools can
always recover the data**: `zstd -dc app.log.trunk` prints the whole
uncompressed log, no timberfs required. The index is pure acceleration.

Records are appended in write order, so they are sorted both by uncompressed
offset **and** by wall-clock time — byte reads and time queries are each one
`partition_point` binary search.

Crash safety: chunks are written data-first, index-second; on open, index
records pointing past the end of the data are dropped and orphaned data
bytes are overwritten. `fsync()` through the mount flushes the buffer as a
chunk and syncs both backing files, so fsync = durable. Unsynced buffered
data is lost on a crash, bounded by `--flush-age`.

### The .bark manifest

An optional `<name>.bark` holds the log's *declared* facts as one flat,
human-editable JSON object — the label on the timber:

```json
{
  "id": "6f9c2a1e-…",             // identity: random UUID, minted on first write,
  "created": "2026-07-11T09:14:02Z", //   constant across renames, moves and hosts
  "host": "imap03.example.com",   // provenance: free-form, yours (--set k=v)
  "index": true,                  // settings: CREATE INDEX — imports maintain the grain
  "retain": "90d",                //   keep at least this long — enforced by EVERY writer
  "retain_size": "50G",           //   compressed-size budget, oldest dropped first
  "timestamp_regex": "^(...)",    // content: exotic line-timestamp format, declared once
  "timestamp_format": "%m/%d/%Y %H:%M:%S", //   (import flags persist these; inherits)
  "derived_from": "41d0…",        // lineage: source store's id
  "derived_op": "export",         // …and how: export (copy) or rotate (move)
  "window_from": "2026-07-04T22:00:00.000Z", // the REQUESTED window (operation
  "window_to": "2026-07-05T22:00:00.000Z"    //   fact — what was asked)
}
```

Artifacts made by `export` and by rotation into a new segment are new
stores: fresh `id`, `derived_from`/`derived_op` lineage (chains compose
across re-carves and shipping), provenance inherited, settings and window
facts not. Content facts — actual spans, sizes — are never recorded (the
artifact's own rings state them authoritatively); the *requested* window
is recorded, because content can't state coverage: a file whose last line
is 17:00 doesn't say whether 17:00–24:00 was covered-but-silent or simply
not exported.

Which is why **an empty result is a result**: exporting or rotating a
window that contains nothing still produces the (empty) artifact.
Present-but-empty ("Saturday was covered, nothing was there — ingest
Sunday") and missing ("a day is missing — don't ingest past the gap") are
opposite signals to a consumer; `--fail-on-empty` turns a quiet day back
into an error for pipelines that want one. `import` skips empty sources
with a note, never an error. Unlike the derived `.grain`, bark survives
head-drops, travels on rename, and ships inside `.timber` bundles.

## Semantics

| Operation            | Behaviour                                                        |
| -------------------- | ---------------------------------------------------------------- |
| append (write @ EOF) | buffered, compressed into a chunk on size/age/close/fsync        |
| write elsewhere      | `EPERM` — the filesystem is append-only                          |
| read anywhere        | chunk located by binary search, decompressed, served             |
| truncate to 0        | allowed: starts the file over (copytruncate-style rotation)      |
| truncate elsewhere   | `EPERM`                                                          |
| rename / unlink      | supported (mv-based log rotation works)                          |
| `ls -l` size         | logical (uncompressed) size                                      |
| `du` blocks          | compressed size — `du -h` shows the real disk footprint          |
| subdirectories       | not yet — flat namespace in v0                                   |

Time-range queries are **chunk-granular** — by design, not as a
placeholder: every chunk whose write-time window overlaps the requested
range is returned in full. Chunk windows are bounded by `--flush-age`
(default 5 s) for slow writers and by `--chunk-size` (default 256 KiB) for
fast ones, so that's the worst-case slop at the edges of the window.

The intended workflow is: `timberfs query` does the coarse seek into a huge
file (cheap, no parsing, immune to multiline entries and timestamp-less
lines), then `timber-filter` (entry-aware) or ordinary `grep`/`awk` on the
small extract trims exactly using the timestamps the log lines carry
anyway. The slop is a feature there:
buffered loggers write lines slightly after the timestamp they print, so a
byte-exact write-time cut could miss edge lines that grep-on-content
catches.

## Custom indexes: the .grain token index

The write-time index generalizes: `.rings` is just a per-chunk summary
(byte ranges + a searchable time window), and queries never touch the
trunk except for the chunks the summary selects. The first content index
is implemented: **`.grain`**, one Bloom filter per chunk over every token
in it (~10 bits per distinct token, k=7, ~1% false positives — measured
0.86% on a 2.7 GB production log). Build it with `timberfs reindex`, use
it with `query --has`:

The index is a property of the LOG, declared once in its `.bark`
manifest — after that, every import maintains the grain automatically
(extended incrementally for new chunks, rebuilt if rotation/retention
dropped it). There is no per-import flag to forget:

```sh
timberfs create --index --set host=foo.bar.com logs-backing/app.log
timberfs import day1/* --into logs-backing/app.log     # grain maintained
timberfs import day2/* --into logs-backing/app.log     # still maintained

timberfs import huge.log --into logs-backing/app.log --index  # or declare+build in one go
timberfs reindex logs-backing/app.log          # or later: 2.7 GB indexed in ~6 s
timberfs query logs-backing/app.log --has F454567068093ZHGZCL   # no time bound!
timberfs query logs-backing/app.log --from 13:00 --to 14:00 --has ERROR \
    | timber-filter --has 'tenantId=FOO'
```

Tokens are ASCII-alphanumeric runs of 3–64 characters, exact case,
config-free: rare tokens (request keys, message ids) skip nearly every
chunk, ubiquitous ones skip nothing and cost only the test. `--has` is a
**chunk-level pre-filter with whole-token matching** — an argument with
separators (`req-8f3a`) must match all its tokens in the same chunk, AND
across repeated `--has` flags is also chunk-level, and substrings of
tokens do not match; exact, entry-level filtering stays downstream in
`timber-filter`. A false positive costs one needless chunk
decompression. The design contract that made this a sidecar:

**Custom indexes are sidecars**: one file per index next to the `.trunk`/`.rings` pair (the
metaphor extends: `.rings` is time, content indexes are *grain*), with a
self-describing header (index type + extractor description) and one
append-only entry per chunk. Three rules:

1. **Derived and rebuildable.** A sidecar can always be regenerated by
   streaming the trunk (`timberfs reindex`), so indexes can be added to
   existing logs, reconfigured, or deleted at zero risk. The trunk and
   rings remain the only durable truth.
2. **Missing means scan.** A chunk without an index entry is "no
   information — scan it". Partial or lagging indexes degrade to
   conservative scans, never wrong answers; this is also the crash story.
3. **Rings rewrites delete sidecars.** Any operation that rewrites the
   `.rings` (rotation, retention head-drop) deletes the file's custom
   indexes; `reindex` recreates them. No coordination logic, no corruption
   class. (Prefix-trimming sidecars in the same pass is a later
   optimization, since head-drops remove exactly a chunk prefix.)

Consequences worth knowing: chunk size is an index-selectivity knob
(smaller chunks → sharper lookups, more overhead), the grain lags a live
appender until the next `reindex` (lagging entries just mean scanning
those chunks), and `.timber` bundles carry no grain yet. A
chunk-sequence-number field in the rings header was considered to let
sidecars survive head-drops without deletion, and rejected: rule 3 makes
it unnecessary, and the on-disk format stays `RING0001`. (Logged-timestamp
zone maps, the other planned index family, became largely moot: import
already writes logged time into the rings.)
