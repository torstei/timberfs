# timberfs

Experiment: a Linux filesystem purpose-built for log files.

Log files have a very particular access pattern that general-purpose
filesystems don't exploit:

- **append-only writes** — nothing ever rewrites the middle of a log
- **highly compressible content** — typically 10–20x with zstd
- **time-correlated reads** — "what happened between 13:42 and 13:43?" is
  *the* question, but answering it on a plain file means scanning gigabytes

`timberfs` is a FUSE filesystem that presents ordinary-looking log files while
storing them chunked + zstd-compressed, with a per-chunk **write-time index**
so a time-range query is a binary search + a few frame decompressions —
independent of file size.

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
lines), then `timberfs grep` (entry-aware) or ordinary `grep`/`awk` on the
small extract trims exactly using the timestamps the log lines carry
anyway. The slop is a feature there:
buffered loggers write lines slightly after the timestamp they print, so a
byte-exact write-time cut could miss edge lines that grep-on-content
catches.

## Usage

```sh
# mount: logical view on ./logs, compressed store in ./logs-backing
timberfs mount ./logs-backing ./logs &

# any process just appends normally
myapp >> logs/app.log
echo "hello" >> logs/app.log
tail -f logs/app.log
grep ERROR logs/app.log

# the killer feature: extract by wall-clock write time, O(log n)
timberfs query logs-backing/app.log --from 13:42 --to 13:43
timberfs query logs-backing/app.log --from "2026-07-09 13:42:00" --to "2026-07-09 13:43:00"

# entry-aware grep: matches whole log ENTRIES (a timestamped line plus
# its continuations — stack traces stay attached); pipe for AND
timberfs grep ERROR logs-backing/app.log --from 13:42 --to 13:43
cat any.log | timberfs grep 'tenantId=FOO' | timberfs grep -v DEBUG

# the fleet view: store one log per host/app, merge at READ time —
# chunks interleave by time across files, lines carry "path:" prefixes
timberfs query --from 13:42 --to 13:43 collector/host*-app.log
timberfs grep req-8f3a collector/*.log --has req-8f3a   # which hosts saw it?

# inspect the chunk index (offsets, compression ratio, time windows)
timberfs index logs-backing/app.log

# quick metadata via xattrs on the mounted file
getfattr -d -m 'user.timberfs.' logs/app.log

# escape hatch: recover everything with stock tools, no timberfs needed
zstd -dc logs-backing/app.log.trunk

# unmount
fusermount3 -u ./logs
```

### Piping without FUSE

The mount is optional: `timberfs append` writes the same store directly
from a pipe — the daemontools/runit/s6 log-processor pattern (`multilog`,
`svlogd`, `s6-log`), so it drops into supervision trees and containers
where FUSE is unwelcome (no `/dev/fuse`, no root, no mount):

```sh
myapp 2>&1 | timberfs append logs-backing/app.log
timberfs query logs-backing/app.log --from 13:42 --to 13:43
```

Each log has exactly one writer (a per-file lock), appenders for different
files share a directory freely, and a directory is either mounted *or*
appended to — never both (the mount daemon owns in-memory state for the
whole directory). End of input, `SIGTERM` or `SIGINT` flush and sync
everything before exit.

**Importing existing logs** works the same way, but with a twist that
matters: chunk time windows come from timestamps *parsed out of the log
lines* (auto-detected RFC3339/ISO, Apache/CLF, or leading epochs;
`--timestamp-regex`/`--timestamp-format` for anything else), because the
write time of historical data says nothing. Lines without a timestamp —
stack traces, continuations — inherit the previous line's, and mildly
out-of-order lines just widen chunk windows (queries select by interval
overlap, so nothing is lost):

```sh
timberfs import /var/log/old-app.log --into logs-backing/app.log
timberfs query logs-backing/app.log --from "2026-06-03 14:00" --to "2026-06-03 15:00"

# a whole rotated set, in any order — files are stitched chronologically
# by their own first timestamps (rotation numbering and glob order lie)
timberfs import /var/log/old-app.log.* /var/log/old-app.log --into logs-backing/app.log

# a timberfs source (say, a rotation segment shipped from another box)
# is detected automatically and merged VERBATIM — no decompression, no
# parsing, index included; re-shipping the same segment is a no-op
timberfs import /shipped/app-2026-07-09.log --into central-backing/hostA-app.log

# the daily bulk-load: each day's file just appends to the archive
timberfs import imap-2026-07-10.log --into backing/imap.log   # day 2
timberfs import imap-2026-07-11.log --into backing/imap.log   # day 3, and so on
```

Imports into a **non-empty store** are placed by each source's *first
timestamp*: after the store's end → append (the daily load above);
*inside* the store's window (day files cut with slack, a re-run) → the
overlap is deduplicated **line by line** — duplicates skip, genuinely
new lines in the covered window land with a warning, and re-importing an
already-covered file is a clean no-op; *before* everything in the store
→ refused. A source starting exactly where the store starts is the same
file regrown: its already-imported prefix is byte-verified and only the
growth is appended (truncated/rewritten files are refused before a byte
is written).

And `timberfs export` is the read-side twin: carve any time window (or a
whole log) out of an archive as a fresh timberfs log — or as a
single-file **`.timber` bundle** for shipping (a plain uncompressed tar,
`.rings` member first, so `tar xf` + `zstd -dc` always recovers it and a
hand-tarred pair is a valid bundle):

```sh
timberfs export backing/archive.log incident.timber --from 13:40 --to 14:10
timberfs query incident.timber --from 13:52 --to 13:53 | grep ERROR
timberfs import incident.timber --into elsewhere/incident.log
```

Note the middle line: bundles are first-class *read-only* logs — `query`,
`index` and `export` operate on a `.timber` file directly (tar keeps its
members contiguous and uncompressed, so the trunk member is just a trunk
at an offset). A directory of `.timber` case files is a queryable cold
archive; unpacking or importing is only ever needed to append.

Together these close the shipping loop: `rotate` cuts history out of live
logs, `export` carves arbitrary windows out of anything, `.timber` bundles
travel as single files, and `import` merges them anywhere, idempotently —
every step a verbatim copy of compressed chunks. The shipping format *is*
the storage format.

Re-importing is idempotent: the target is its own checkpoint. Already
imported bytes are verified against the source (all chunks, or
first/middle/last with `--quick`), then only the growth is appended —
an unchanged source is a no-op, a rotated/rewritten one is refused before
anything is written. So a periodic `timberfs import` of a growing file
is a safe, cheap catch-up (full verification of a multi-GB target runs
in well under a second).

The appender is also where **retention** lives, because it already owns
the file: `--retain 30d` continuously drops data older than 30 days, and
`--retain-size 200G` keeps the compressed on-disk size under a hard
budget, oldest first — combine them for "keep the last 30 days, but never
more than 200G":

```sh
myapp 2>&1 | timberfs append --retain 30d --retain-size 200G logs-backing/app.log
```

No dated-file rotation needed: the log is simply a single file that always
contains the recent past, and `timberfs query` finds things in it by time.
(Head-dropping currently compacts by rewriting the retained data, so
enforcement is batched — expired data goes once it's ~10% of the file, size
overruns trim to 95% of budget — and compaction briefly needs free space
proportional to what's kept. Hole-punching is the planned fix for very
large stores.)

## Rotation & retention

`timberfs rotate` does **time-based** rotation: everything written before the
cutoff moves out of the live log into another one (or is dropped), while
newer data stays put — a cut a normal filesystem can't do without rewriting
the whole file.

```sh
timberfs rotate backing/app.log app-2026-07-08.log --cutoff "2026-07-09T00:00"
timberfs rotate backing/app.log --delete --cutoff "2026-06-01T00:00"   # retention
timberfs rotate backing/app.log archive.log --cutoff 12:00 --dry-run   # preview
```

Why it's cheap: chunks are immutable zstd frames, so rotation relocates
**compressed bytes verbatim** — no decompression, no recompression — and
rebases the index records. Rotating gigabytes of logs costs I/O proportional
to the compressed size. The destination (same backing directory) is created
or appended to; appends are refused if they would break the index's time
ordering. Like queries, the cutoff is chunk-granular: a chunk straddling it
stays in the live file.

It works against a live mount: the daemon holds an `flock` on
`<backing>/.timberfs.lock` recording its mountpoint, and `timberfs rotate`
auto-detects it — offline it rewrites the backing files directly (holding
the same lock), mounted it routes the request through the daemon as a
`setxattr` control call (`user.timberfs.rotate`), which rotates atomically
under the daemon's state lock and then invalidates the kernel's attribute
cache so writers holding the file open with `O_APPEND` keep working across
the shrink (their next write re-bases to the new EOF).

`timberfs query`/`timberfs index` read the backing files directly and are safe to
run against a live mount (chunks are immutable, the index is append-only).
Note they only see flushed chunks — the still-buffered tail (≤ flush-age
old) is visible through the mount but not yet in the backing files.

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
    | timberfs grep 'tenantId=FOO'
```

Tokens are ASCII-alphanumeric runs of 3–64 characters, exact case,
config-free: rare tokens (request keys, message ids) skip nearly every
chunk, ubiquitous ones skip nothing and cost only the test. `--has` is a
**chunk-level pre-filter with whole-token matching** — an argument with
separators (`req-8f3a`) must match all its tokens in the same chunk, AND
across repeated `--has` flags is also chunk-level, and substrings of
tokens do not match; exact, entry-level filtering stays downstream in
`timberfs grep`. A false positive costs one needless chunk
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

## Install

Debian/Ubuntu, from the apt repository (rebuilt by CI from the GitHub
releases on every release, GPG-signed, `apt upgrade` works):

```sh
sudo curl -fsSL https://torstei.github.io/timberfs/key.gpg \
     -o /usr/share/keyrings/timberfs.gpg

sudo tee /etc/apt/sources.list.d/timberfs.sources >/dev/null <<'EOF'
Types: deb
URIs: https://torstei.github.io/timberfs
Suites: stable
Components: main
Signed-By: /usr/share/keyrings/timberfs.gpg
EOF

sudo apt update && sudo apt install timberfs
```

Or grab a single `.deb` from the latest GitHub release (built, VM-tested
and provenance-attested by CI — verify with
`gh attestation verify timberfs_amd64.deb --repo torstei/timberfs`):

```sh
curl -LO https://github.com/torstei/timberfs/releases/latest/download/timberfs_amd64.deb
sudo apt install ./timberfs_amd64.deb
```

Or from crates.io with a Rust toolchain: `cargo install timberfs`.

## Build

Needs the Rust toolchain and a C compiler (for the vendored zstd), plus
fuse3 at runtime:

```sh
sudo apt install rustup build-essential fuse3   # or rustup.rs installer
rustup default stable
cargo build --release                            # target/release/timberfs
```

### Debian package

```sh
cargo install cargo-deb
cargo deb                                        # target/debian/timberfs_*.deb
sudo dpkg -i target/debian/timberfs_*.deb
```

The package installs `/usr/bin/timberfs` plus a systemd template unit: drop
a config in `/etc/timberfs/<instance>.conf` (see
`/usr/share/doc/timberfs/examples/timberfs.conf.example`) and run
`systemctl enable --now timberfs@<instance>` to mount at boot. Stopping the
unit unmounts first, so the daemon flushes everything and exits cleanly.

## Ideas / future work

- **More `.bark`**: an `annotate` command for existing logs, attribution
  labels from manifest fields in multi-file output (`--label '{host}'`),
  auto-seeded provenance on import, bark-aware routing in the future
  sawmill server.
- **`grep --into`**: let `timberfs grep` emit a timberfs artifact instead
  of raw text — a filtered store with `derived_op: "grep"` and the pattern
  recorded as an operation fact, lineage intact. The empty-result rule
  already anticipates it: a pattern that matches nothing yields a valid
  empty artifact, same as an empty export window.
- **A timberfs server ("sawmill")**: bundles shipped in over HTTP (PUT +
  idempotent import = at-least-once ingest for free), routed to per-stream
  archives by their `.bark` manifests, queried over a thin REST layer
  wrapping query/grep. Tiering: keep rings+grain LOCAL, ship trunks to
  object storage — queries plan locally (time windows + blooms) and fetch
  candidate chunks as single S3 ranged GETs. Principle: the directory
  stays the database; the server owns no state that is not a plain
  timberfs file. Path: lib refactor → .bark → read-only serve → ingest →
  tiering.
- **zstd seekable format / dictionaries**: adopt the official seekable-zstd
  framing for ecosystem compat; train a dictionary per file for much better
  small-chunk ratios; long-range mode for cold recompression.
- **Cold-chunk recompression**: rewrite old chunks at zstd -19 in the
  background; the index makes this a local, safe operation.
- **Scheduled rotation**: a `timberfs rotated`-style timer (or systemd timer
  recipe) driving `rotate --cutoff`/`--delete` policies per file.
- **Appender growth toward s6-log**: `SIGHUP`-triggered and scheduled
  rotation into dated files (for shipping archives off-box), optional line
  timestamping, `--tee` passthrough, and a `--follow` reader.
- **Hole-punching retention**: drop the head via `FALLOC_FL_PUNCH_HOLE`
  instead of compact-rewrite, making `--retain-size` cheap on huge stores.
- **Expose the index in-band**: a virtual `.idx` twin file or ioctl so tools
  can query through the mount without knowing the backing dir.
- **tail(1) fast-path**: negative-offset "time seek" via `llseek` hooks.
- **Subdirectories, multi-writer O_APPEND atomicity, runtime rescan of the
  backing dir, real statfs passthrough.**
- **Kernel port**: the store layer is FUSE-free by design; revisit
  Rust-for-Linux filesystem bindings when they stabilize.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
