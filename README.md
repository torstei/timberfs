# timberfs

Experiment: a purpose-built home for log files — stored compressed,
searchable in milliseconds.

`timberfs` keeps logs compressed (zstd) as they are written, and still
answers *"what happened between 13:42 and 13:43?"* or *"who logged req-8f3a?"*
in milliseconds, on files of any size, without decompressing them.

It can be this fast and this small because log files have a particular access
pattern that general-purpose storage doesn't exploit:

- **append-only writes** — nothing ever rewrites the middle of a log
- **highly compressible content** — typically 10–20x with zstd
- **time-correlated reads** — "what happened around 13:42?" is *the* question,
  but on a plain file it means scanning gigabytes
- **oldest-first deletion** — logs age out from the front, which a plain file
  can't do without a rewrite; timberfs drops old data cheaply

The one trade-off: data must arrive in log order (by timestamp). `import`
stitches historical files into order for you, and live ingestion is in order
by definition — so in practice it rarely bites.

## Getting started: you have a pile of logs

Install (see [Install](#install) for details and verification):

```sh
sudo apt install ./timberfs_amd64.deb   # from the latest GitHub release
cargo install timberfs                  # or, with a Rust toolchain
```

### 1. Create a store, import your logs

```sh
timberfs create --index --set host=$(hostname) backing/app.log
timberfs import /var/log/myapp/app.log* --into backing/app.log
```

### 2. Ask it things

```sh
timberfs info backing/app.log                  # vital signs: size, ratio, time covered
timberfs query backing/app.log --from "2026-07-10 13:40" --to "2026-07-10 14:10"
timber-filter --has ERROR backing/app.log --from 2026-07-10  # word-match, index-fast
timber-filter --has req-8f3a backing/app.log                  # request id, no time bound
timberfs query backing/app.log --from 13:40 --to 14:10 | grep -c 'tenantId=FOO'
```

`query` selects by time — verified against each line's own timestamp, so
13:37–13:38 never shows a 13:42 line — while `timber-filter` matches whole
entries (stack traces stay intact) by named predicates, exact word predicates
riding the token index automatically. `-f`/`--follow`, `--tail N` and `--max N`
stream or cap. Stores in a **forest** (`/var/log/timberfs`) take a bare handle
(`timberfs query nginx`), `timberfs list` shows what's there, and the package
ships shell completion for both tools. Full reference: `man timberfs`,
`man timber-filter`.

Ship an investigation — with its provenance — as one self-describing file:

```sh
timber-filter --records --has 'tenantId=FOO' backing/app.log --from 13:40 --to 14:10 \
  | timberfs import --records --into case/case.log
timberfs export case/case.log --into case.timber   # queryable in place; records where it
                                                    # came from and how (timberfs info case.timber)
```

### 3. Make it *the* logger (when you're ready)

Live ingestion also retires rotation's "make room" job: **retention** drops the
oldest data continuously (no rotate-and-delete, no seams), as a property of the
*log* — declared in the manifest, enforced by every writer: `create
--retain-size 50G` or `timberfs set backing/app.log retain=90d` (live, no
restart).

Three ways in, in increasing order of commitment:

**a) Keep importing on a timer** — zero changes to your logging. Re-import
verifies what's already stored and appends only the growth, so a cron or
logrotate hook is cheap even on huge files:

```sh
# cron, or logrotate postrotate:
timberfs import --quiet /var/log/myapp/app.log --into backing/app.log --quick
```

**b) Pipe it** — if the producer can write to a pipe, cut the plain file
out entirely (svlogd-style, retention built in):

```sh
timberfs set backing/app.log retain_size=50G     # once (or at create time)
myapp 2>&1 | timberfs append --into backing/app.log
# (flags work too and persist the declaration: --retain-size 50G)

# apache2: piped logs are a first-class Apache feature
CustomLog "|/usr/bin/timberfs append --quiet /var/log/apache2-backing/access.log" combined
ErrorLog  "|/usr/bin/timberfs append --quiet /var/log/apache2-backing/error.log"

# journald-only software:
journalctl -u myapp -f -o short-iso | timberfs append --into backing/myapp.log --retain 90d
```

One rule: **don't backfill history through the pipe** — `append` indexes
by write time, so old data lands under today's timestamps. Historical
files go through `import`, which parses their own timestamps (and is
resumable, deduplicating and idempotent).

**c) Mount it** — if the software insists on writing to a real file path,
give it one; compression, indexing and retention happen transparently
underneath:

```sh
timberfs mount /var/log/myapp-backing /var/log/myapp
# the app writes /var/log/myapp/app.log as always; tail/less/grep work
```

One nuance worth knowing: `import` stamps chunks with timestamps **parsed
from the log lines** (right for historical data), while `append`/`mount`
stamp with the **write-time wall clock** (right for live ingestion, where
they coincide). Either way, `query --from/--to` asks about the time the
log talks about.

## Beyond the getting-started path

The tour above is the whole core loop. The main thing it leaves out is the
**fleet view**: keep one log per host/app and merge them at *read* time, so a
single query spans the fleet — chunks interleave by timestamp across files, and
each line carries a `path:` prefix showing who logged it.

```sh
timberfs query --from 13:42 --to 13:43 collector/host*-app.log
timber-filter --has req-8f3a collector/*.log        # which hosts saw it?
```

The full command reference — every flag, `import`/`export`/`rotate`, retention,
forests, `.timber` bundles, and the records stream — is in the man pages:
`man timberfs`, `man timber-filter`, and `man timberfs-records`.

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

It's cheap because chunks are immutable zstd frames: rotation relocates
compressed bytes verbatim (no re/decompression) and rebases the index, so it
costs I/O proportional to the compressed size. It runs against a live mount
(auto-detected, routed through the daemon atomically) and is chunk-granular
like queries. Details: `man timberfs`.

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

## How it works

Two files per log: the data (`<name>.trunk`, concatenated zstd frames) and a
write-time index (`<name>.rings`). Everything else is derived, and stock tools
can always recover your data — `zstd -dc <name>.trunk` prints the whole log, no
timberfs required; the index is pure acceleration.

The full design — why FUSE, the on-disk format, the `.bark` manifest, the
semantics table, and the `.grain` token index — lives in
**[docs/design.md](docs/design.md)**. You don't need any of it to use timberfs;
the curious and the contributors start there.

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

The package installs `/usr/bin/timberfs` and two systemd template unit
families: `timberfs@<instance>` to mount a store at boot, and a
socket-activated `timberfs-log@<instance>` to stream into a store without a
mount. See **[Deploying timberfs](docs/deployment.md)** for the directory
layout, both unit families, the ownership/permission model, and
self-restart-on-upgrade.

## Roadmap

Ideas and future work live in [ROADMAP.md](ROADMAP.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
