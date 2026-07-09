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

## Build

Needs the Rust toolchain and a C compiler (for the vendored zstd), plus
fuse3 at runtime:

```sh
sudo apt install rustup build-essential fuse3   # or rustup.rs installer
rustup default stable
cargo build --release                            # target/release/timberfs
```

## Ideas / future work

- **zstd seekable format / dictionaries**: adopt the official seekable-zstd
  framing for ecosystem compat; train a dictionary per file for much better
  small-chunk ratios; long-range mode for cold recompression.
- **Cold-chunk recompression**: rewrite old chunks at zstd -19 in the
  background; the index makes this a local, safe operation.
- **Scheduled rotation**: a `timberfs rotated`-style timer (or systemd timer
  recipe) driving `rotate --cutoff`/`--delete` policies per file.
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
