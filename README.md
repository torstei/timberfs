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
ships shell completion for all three tools. Full reference: `man timberfs`,
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

Five ways in, in increasing order of commitment:

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

The token index needs no attention either way: once `index` is declared,
every writer maintains it — a streaming one on its once-a-second tick, so
the grain trails the newest chunk by at most that tick, and an uncovered
chunk is scanned rather than missed. `timberfs info` shows the coverage.

**c) Mount it** — if the software insists on writing to a real file path,
give it one; compression, indexing and retention happen transparently
underneath:

```sh
timberfs mount /var/log/myapp-backing /var/log/myapp
# the app writes /var/log/myapp/app.log as always; tail/less/grep work
```

**d) Let it speak Fluentd Forward** — `timberfs forward-intake` is a TCP
receiver for the Fluentd Forward protocol v1, the wire protocol Docker's
`fluentd` log driver, Fluent Bit, Fluentd and the fluent-logger client
libraries already speak — no plain-file or FIFO producer needed. Every tag
lands in its own store — pre-created by the operator, or minted on
first sight with `--auto-create` (the Docker-host mode); a `chunk` id is
acked only once durable in the
`.sap` write-ahead sidecar (acks at fsync rate, chunks stay full-size)
(at-least-once, like the socket intake above):

```sh
timberfs forward-intake --into-dir /var/log/timberfs &

docker run --log-driver=fluentd --log-opt fluentd-address=127.0.0.1:24224 \
    --log-opt tag={{.Name}} --log-opt fluentd-async=true \
    --log-opt fluentd-request-ack=true --log-opt fluentd-sub-second-precision=true \
    myimage
```

`fluentd-async` keeps a down receiver from blocking the container's stdout;
`fluentd-request-ack` and `fluentd-sub-second-precision` are opt-in on
Docker's side. The default tag is a 12-char container id (a bad store
name), hence `tag={{.Name}}`. Deliberate limitations — no TLS/handshake
(loopback or a private network only), no gzip-compressed mode, no UDP
heartbeat — are in `man timberfs` and [docs/deployment.md](docs/deployment.md).
The verb name is provisional.

**e) Let it speak OTLP** — `timberfs otlp-intake` receives OTLP/HTTP logs,
the OpenTelemetry protocol every SDK and the Collector speak, so an OTel
pipeline can write straight into timberfs — and the Collector bridges syslog,
journald, Kafka and Fluent Bit in behind it. Each `ResourceLogs` stream lands
in its own store (routed by `service.name`), its resource attributes seeded
into the store's `.bark`, and each `LogRecord` becomes one entry; the HTTP 200
is sent only once the batch is fsynced:

```sh
timberfs otlp-intake --into-dir /var/log/timberfs --auto-create &
```

```yaml
# an OpenTelemetry Collector pointed at it — no settings to change
exporters:
  otlphttp/timberfs:
    endpoint: http://127.0.0.1:4318
```

Both OTLP/HTTP encodings work (binary protobuf, which every sender defaults
to, and JSON), gzipped or not. An undeclared stream gets 503 + `Retry-After`
so the sender buffers until you create the store — or `--auto-create` mints
them. `POST /v1/logs` only, no TLS (loopback or a private network), and no
gRPC on :4317 — put a Collector in front if a sender needs it.

It pairs with `timber-otlp` below: a store shipped out over OTLP and received
back arrives byte for byte, which is the property their tests hold each other
to.

One nuance worth knowing: `import` (`--follow` included) stamps chunks with
timestamps **parsed from the log lines**, while `append`/`mount` stamp with
the **write-time wall clock**. Either way, `query --from/--to` asks about the
time the log talks about: chunks are selected on the store's clock, then every
entry is verified against its own logline stamp. Where a producer's two clocks
diverge — Apache logs a request's start time and writes the line when the
request completes — that selection leans on a one-minute widening, and past
that a follower is the better route, its chunks carrying the logline clock.
See [Two clocks](docs/deployment.md#two-clocks-and-when-they-diverge).

## Beyond the getting-started path

The tour above is the whole core loop. The main thing it leaves out is the
**fleet view**: keep one log per host/app and merge them at *read* time, so a
single query spans the fleet — chunks interleave by timestamp across files, and
each line carries a `path:` prefix showing who logged it.

```sh
timberfs query --from 13:42 --to 13:43 collector/host*-app.log
timber-filter --has req-8f3a collector/*.log        # which hosts saw it?
```

The deployment *shapes* all of this composes into — giving an application OTLP
without touching it, a full-fidelity tier under an expensive backend, container
logs, replaying an incident window into a backend — are in
**[Use cases](docs/use-cases.md)**, with the limits that come with them.

The full command reference — every flag, `import`/`export`/`rotate`, retention,
forests, `.timber` bundles, and the records stream — is in the man pages:
`man timberfs`, `man timber-filter`, `man timber-otlp`, and `man timberfs-records`.

## Shipping onward (`timber-otlp`)

A store is not a dead end. `timber-otlp` reads a store's entry stream and posts
it to any OTLP/HTTP receiver — an OpenTelemetry Collector, or a backend that
speaks OTLP directly — one LogRecord per **entry**, so a stack trace arrives as
one record rather than forty:

```sh
# ship as it arrives, resumably across restarts
timber-otlp --follow --cursor /var/lib/timberfs/app.otlp \
    --endpoint http://collector:4318 backing/app.log

# replay an incident window into a fresh backend
timber-otlp --from '2026-08-11 14:00' --to '2026-08-11 15:00' \
    --endpoint http://new-backend:4318 backing/app.log
```

It is a reader, so an unreachable receiver can stall the shipper and nothing
else — the appender never notices. **The store is the send buffer**: where a
collector's queue is sized by guessing, retention is the disconnection budget
(`retain 30d` means the receiver can be gone for thirty days), and any window
can be re-shipped afterwards, which a collector cannot do because it retains
nothing.

The two OTLP time fields land on timberfs's two axes: `timeUnixNano` gets the
entry's own logline stamp, `observedTimeUnixNano` the write time it arrived at.
The position is persisted in a cursor file on the write axis (the only
monotonic one), so a restart resumes instead of re-sending; delivery is
at-least-once, as OTLP itself is. `--dry-run` prints exactly what would be
posted. Protobuf by default (`--encoding json` for a readable wire,
`--compress gzip` over a network); plaintext HTTP only — terminate TLS in a
collector beside it. Details: `man timber-otlp`.

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

## Durability and the live edge (`--wal`)

By default, a crash (SIGKILL, power loss) can lose up to `--flush-age`
(5s) of buffered-but-unflushed data, and a follower cannot see that data
either — chunking wants big, infrequent frames, which is at odds with
both. `--wal` decouples them: `create --wal` / `append --wal` (or
`timberfs set store wal=true` on an existing one) declares a write-ahead
sidecar, `<name>.sap`, holding every entry raw as it arrives. Every
streaming writer fsyncs it once a second — shrinking the crash window to
that tick, independent of `--flush-age` and the chunk-size schedule —
and `query --follow` tails its live edge, so entries reach an operator as
they are appended instead of a flushed chunk at a time (measured p50 0.5s
against 36s on the same one-line-a-second store, with the chunking and
its 8.7x compression unchanged). It's a property of the *store* (like
`--index`), declared once in the manifest: any later writer honors it
with no flag — including one already running, so a stream can be given a
live edge mid-incident without restarting whatever produces it.

```sh
timberfs create --wal --retain 90d backing/app.log
myapp 2>&1 | timberfs append --into backing/app.log
```

The cost is explicit: a wal-enabled writer writes every appended byte
twice — once raw to the sap, once compressed into its eventual chunk — so
turn it on for streams where a few seconds of loss, or a minute of
waiting, actually matters, not by default. The alternative — a short
`--flush-age` — buys the same visibility by making chunks small, which
costs compression on a quiet stream (1.9x against 8.7x at one line a
second) and multiplies the `.rings`/`.grain` index over it. `timberfs info` shows whether it's declared and how many
bytes are currently sitting in the sap, unflushed. Design and the crash
matrix: [docs/design.md](docs/design.md#the-sap-write-ahead-sidecar).

## Install

Debian/Ubuntu, from the apt repository (rebuilt by CI from the GitHub
releases on every release, GPG-signed, `apt upgrade` works). One `amd64`
package is built against an old glibc, so it installs on every current
release — Ubuntu 20.04+ and Debian 11+:

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

Two files carry the log: the data (`<name>.trunk`, concatenated zstd frames)
and a write-time index (`<name>.rings`). Stock tools can always recover your
data — `zstd -dc <name>.trunk` prints the whole log, no timberfs required; the
index is pure acceleration. Three optional sidecars sit beside them, each with
a different contract: `.grain` (token index) is derived — safe to delete, cheap
to rebuild; `.sap` (write-ahead) is live writer state, read exactly once, after
a crash; `.bark` holds what you *declared* — identity, retention, provenance —
which is why it travels with the store.

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

The package installs `/usr/bin/timberfs`, `timber-filter`, `timber-otlp` and
seven systemd unit families: `timberfs@<instance>` (a template) to mount a
store at boot, a socket-activated `timberfs-log@<instance>` (also a template)
to stream a records producer into a store without a mount, its plain-text
sibling `timberfs-text@<instance>` for a producer that can only log to a path
(Apache's `CustomLog`/`ErrorLog`, nginx's `access_log`), `timberfs-follow@<instance>`
to read a file a producer keeps writing (no coupling to that producer at all),
socket-activated
`timberfs-forward` and `timberfs-otlp` (not templated — both multiplex every
stream over one listener) for the two network intakes above, and
`timberfs-otlp@<instance>` (a template, one per store) to ship a store onward.
See **[Deploying timberfs](docs/deployment.md)** for the directory layout, all
seven unit families, the ownership/permission model, and
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
