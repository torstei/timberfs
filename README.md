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
lines), then ordinary `grep`/`awk` on the small extract trims exactly using
the timestamps the log lines carry anyway. The slop is a feature there:
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

## Custom indexes (design contract — not yet implemented)

The write-time index generalizes: `.rings` is just a per-chunk summary
(byte ranges + a searchable time window), and queries never touch the
trunk except for the chunks the summary selects. Any index over log
*content* — the logged timestamp, request IDs, arbitrary identifiers — has
the same shape, and the design is fixed here so implementations don't
drift into format changes.

Two index families cover the useful cases (both are standard practice in
column stores — ClickHouse skip indexes, Parquet statistics and bloom
filters, Loki's label index):

- **Zone maps** for ordered-ish values: per chunk, store `(min, max)` of
  the extracted value; a range query selects overlapping chunks. The
  *logged* timestamp is the flagship — and zone maps stay correct under
  out-of-order logging (threads, imports, replays); mostly-increasing data
  just makes them sharper. This is what makes logs *imported* through
  `append` time-searchable, where write time says nothing.
- **Bloom filters** for identifiers: per chunk, a filter over extracted
  (or simply all) tokens; a lookup decompresses only chunks whose filter
  matches. ~1–2 KB per 256 KiB chunk covers thousands of distinct tokens
  at ~1% false positives. Sharp for rare identifiers (the "find this
  request across 30 days" case); honest about ubiquitous ones. A
  config-free tokenize-everything default gives an indexed `grep`;
  regex/JSON field extraction is an advanced layer that only changes what
  goes into the filter, never the file structure.

The contract that keeps the core format frozen — **custom indexes are
sidecars**: one file per index next to the `.trunk`/`.rings` pair (the
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

Consequences worth knowing: chunk size becomes an index-selectivity knob
(smaller chunks → sharper lookups, more overhead), and extraction can run
inline at flush time or lazily over cold chunks — both fit, per file.
A chunk-sequence-number field in the rings header was considered to let
sidecars survive head-drops without deletion, and rejected: rule 3 makes
it unnecessary, and the on-disk format stays `RING0001`.

Build order when this happens: logged-timestamp zone map first, token
blooms second.

## Install

Debian/Ubuntu, from the latest GitHub release (the `.deb` is built, tested
in a VM, and provenance-attested by CI):

```sh
curl -LO https://github.com/torstei/timberfs/releases/latest/download/timberfs_amd64.deb
sudo apt install ./timberfs_amd64.deb
```

Optionally verify the artifact really came from this repo's CI:

```sh
gh attestation verify timberfs_amd64.deb --repo torstei/timberfs
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

- **Custom indexes**: logged-timestamp zone maps, then token blooms — the
  design contract is fixed above; only implementation remains.
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
