# Deploying timberfs

The Debian package is opinionated about *where* things live and *how* they are
supervised, so a stock install works out of the box and multi-instance setups
stay tidy. This document describes that layout and the systemd units.

None of it is baked into the binary — `timberfs` reads and writes wherever you
point it, holds no global registry, and imposes no directory scheme. What
follows is the **packaged convention**; a bespoke deployment can ignore it and
pass its own paths.

## Directory layout

```
/usr/bin/timberfs                         the main binary (mount / query / append / …)
/usr/bin/timber-filter                    the entry-aware records filter

/etc/timberfs/<instance>.conf             config for a mount instance (see below)
/etc/timberfs/otlp-<instance>.conf        config for an OTLP shipper instance (see below)
/etc/timberfs/forests.d/*.conf            forests: directories searched by handle (see below)

/var/lib/timberfs/otlp-<instance>.cursor  the OTLP shipper's position in its store —
                                          CONSUMER state, deliberately not inside the
                                          store's backing directory (StateDirectory=)

/run/timberfs/<instance>.pipe             records intake FIFO, created by the .socket
                                          unit (the directory is created at boot by
                                          tmpfiles.d)
/run/timberfs/text/<instance>.pipe        plain-text intake FIFO, same but for the
                                          timberfs-text@ pair (Apache, nginx)

/var/log/timberfs/<instance>/             one directory per log-intake instance,
                                          owned by that instance's service user:
    <instance>.log.trunk                    the data — chunked zstd frames
    <instance>.log.rings                    the write-time index (per-chunk time bounds)
    <instance>.log.grain                    optional token index (present with --index)
    <instance>.log.bark                     JSON manifest: durable identity + retention
    <instance>.log.sap                      optional write-ahead sidecar (present with --wal)
    <instance>.log.lock                     the store's writer lock
  .timberfs.lock                            the directory lock (see Locking)

/var/log/timberfs/text/                   one directory shared by every plain-text
                                          intake instance (timberfs-text@), so one
                                          glob spans the fleet:
    <instance>.log.trunk / .rings / .grain / .bark / .lock   one store per instance
                                          (e.g. apache-access.log and
                                          apache-error.log for a whole web server)
  .timberfs.lock                            the directory lock (see Locking)

/var/log/timberfs/forward/                one directory shared by every tag the
                                          Fluentd Forward intake sees (it is a
                                          single TCP listener, not templated):
    <tag>.log.trunk / .rings / .grain / .bark / .sap / .lock   one store per tag
                                          (.sap always: an acking receiver
                                          declares "wal": true on every store)
  .timberfs.lock                            the directory lock (see Locking)

/var/log/timberfs/otlp/                   likewise for the OTLP intake, one
                                          store per stream (routed by
                                          service.name):
    <service>.log.trunk / .rings / .grain / .bark / .sap / .lock
  .timberfs.lock                            the directory lock (see Locking)
```

The store's **logical name** is `<instance>.log`, so you read it with the full
path:

```sh
timberfs query /var/log/timberfs/nginx/nginx.log --from 13:42 --to 13:43
timberfs info  /var/log/timberfs/nginx/nginx.log
```

Or by **handle**: the package ships `/etc/timberfs/forests.d/default.conf` with
`DIR=/var/log/timberfs`, and a bare token (no `/`) that names no store on disk
is looked up as a store under a configured forest — so `nginx` finds
`/var/log/timberfs/nginx/nginx.log`:

```sh
timberfs query nginx --from 13:42 --to 13:43
timberfs info  nginx
```

The handle is the `.rings` file name minus `.rings` and a single trailing
`.log`, so both a flat `nginx.rings` and a nested `nginx/nginx.log.rings`
resolve as `nginx`. Full paths always win and nothing existing changes; edit
`DIR`, drop in another `*.conf`, or delete the file to disable the lookup (it's
a conffile, so edits survive upgrades). See `man timberfs` (FORESTS).

### Why a directory per instance

Creating an index, or rotating — in fact **any writer operation** — needs write
permission on the *directory*, not just on the store files, because it creates
new files there. A directory per instance lets each one be owned and managed by
its own user without a directory that every instance can write to, and it keeps
per-store file ownership clean. The store is named after the instance (rather
than a fixed name) so its logical name stays unique and meaningful even across
hundreds of instances. An instance that needs more than one stream just sets a
custom `--into` in a drop-in.

## systemd units

Five independent families ship with the package: one mount, three intakes
(FIFO, Forward, OTLP) and one shipper.

### Mounting a store — `timberfs@.service`

Browse a store as a live, append-only filesystem. Configure the instance in
`/etc/timberfs/<instance>.conf`:

```ini
BACKING=/var/log/timberfs-backing/applogs
MOUNTPOINT=/var/log/apps
EXTRA_OPTS=--allow-other
```

```sh
systemctl enable --now timberfs@applogs
```

Stopping the unit unmounts first (`ExecStop`), so the daemon flushes everything
and exits cleanly.

### Streaming logs in — `timberfs-log@.socket` + `timberfs-log@.service`

Drain a producer's stream into a store over a FIFO, supervised on both sides —
the [svlogd](https://smarden.org/runit/svlogd.8.html)/s6-log pattern, but the
supervisor survives restarts of *either* end. It is **socket-activated**: enable
the `.socket`, not the `.service`.

- The `.socket` owns `/run/timberfs/<instance>.pipe` (`ListenFIFO`) and holds it
  open `O_RDWR`, so the producer never sees `EOF`/`EPIPE` across a service
  restart — writes buffer in the kernel pipe and drain when it returns.
- The `.service` drains it with `append --records` into
  `/var/log/timberfs/<instance>/<instance>.log`.

```sh
systemctl enable --now timberfs-log@applogs.socket
# then a producer writes a timberfs-records(5) stream to /run/timberfs/applogs.pipe
```

The producer must write a [timberfs-records(5)](../packaging/timberfs-records.5)
stream (that is what `--records` means) — the intended fit for a records-format
logging writer that frames its own events and timestamps. To archive a
plain-text source instead, drop `--records` from the `ExecStart` (see the
drop-in below).

### Logging from a producer that only writes paths — `timberfs-text@.socket` + `timberfs-text@.service`

The same FIFO pattern for a producer that has no pipe support and can only be
pointed at a *path*: Apache's `CustomLog`/`ErrorLog`, nginx's `access_log`,
HAProxy. Plain text rather than records, one store per instance, and
**socket-activated** like the pair above: enable the `.socket`.

Why route it this way rather than piping (`CustomLog "|timberfs append ..."`)
or writing into a mount:

- **The writer is not the producer's child.** Apache spawns a piped-log
  program itself, and on reload it spawns the replacement *before* the old one
  has drained its pipe and released the store's writer lock — one error per
  store per reload — while a piped writer left holding a lock takes that log
  down until someone intervenes. Here Apache only opens a path; the writer's
  lifecycle is systemd's.
- **A writer restart loses nothing.** The socket holds the FIFO open `O_RDWR`,
  so lines written while the service is restarting or being upgraded wait in
  the kernel pipe and land when it returns — the producer sees no `EOF`, no
  `EPIPE`, and needs no reload.
- **Failure is visible.** A wedged writer is a failed unit in
  `systemctl --failed`, not a silently dead logger.

#### One pair of stores for the whole server (start here)

Put the vhost in the log line and let every site share two stores — access and
error. A new vhost then needs **no logging configuration at all**: the
server-level directives are inherited.

```apache
# httpd, server level, once
LogFormat "%v %h %l %u %t \"%r\" %>s %O \"%{Referer}i\" \"%{User-Agent}i\"" vhost
CustomLog /run/timberfs/text/apache-access.pipe vhost
ErrorLogFormat "[%{u}t] [%-m:%l] [pid %P] [vhost %v] %M"
ErrorLog /run/timberfs/text/apache-error.pipe
```
```sh
systemctl enable --now timberfs-text@apache-access.socket \
                       timberfs-text@apache-error.socket
```
```ini
# /etc/timberfs/text-apache-access.conf
DECLARE=index=true retain=90d format=combined-vhost
```
```ini
# /etc/timberfs/text-apache-error.conf — errors are worth keeping longer
DECLARE=index=true retain=1y format=apache-error
```

Two stores rather than one because differentiated retention is the only thing a
single store really gives up: both of Apache's clocks are built-in extractors
(the access log's CLF `%t`, the error log's bracketed `ctime`), so a single
combined store would parse correctly too — it is the policy, not the parsing,
that argues for splitting. Keep `[%{u}t]` (or plain `%t`) at the FRONT of the
error format; that leading bracketed timestamp is what the extractor reads.

Consolidating also compresses better, because each chunk is filled by every
site at once instead of dribbling in per site: the same lines written in small
flushes cost about **half** as much on disk in one store as in three (ratio
11.9x vs 21.9x in a three-vhost measurement — the quieter the sites, the bigger
the gap).

One store per site is then a *filter*, not a file:

```sh
# everything one vhost did, both streams, in time order
timber-filter --has shop.example.com /var/log/timberfs/text/apache-'*'.log --from 13:40

# hand that site's window to someone as a store of its own, provenance recorded
timber-filter --records --has shop.example.com /var/log/timberfs/text/apache-access.log \
    | timberfs import --records --into /tmp/shop-case.log
```

Note what the token index can and cannot do here. A **rare** token — a request
id, a client IP, a failing URL — is narrowed to the few chunks that contain it,
and now that search covers every vhost in one pass. A **busy vhost's own name**
appears in every chunk, so filtering by it is a scan of the selected time
window rather than an index hit: still an order of magnitude less data than
plain files, but not free. Filter by time first.

#### One store per vhost (when a site needs its own policy)

Give a site its own instance when it needs its own retention, its own owner
(`User=` in a drop-in), or its own file for a tool that insists on one:

```sh
systemctl enable --now timberfs-text@www.example.com.socket \
                       timberfs-text@www.example.com.error.socket
```
```apache
# in that vhost only; the rest keep inheriting the server-level pair above
CustomLog /run/timberfs/text/www.example.com.pipe combined
ErrorLog  /run/timberfs/text/www.example.com.error.pipe
```

The two layouts mix freely — same units, different instance names — so
consolidate the small sites and split out the one that needs its own rules.
Per-vhost instances also contain a stall: a wedged writer holds up only its own
site, where the consolidated layout has one writer for everything. Their stores
share one directory, so a vhost's two streams still read as one interleaved,
attributed view:

```sh
timberfs query /var/log/timberfs/text/www.example.com'*'.log --from 13:40
```

#### Both layouts

Retention and the index are configuration, not a command someone has to
remember — `DECLARE` is applied to the store on every start, so changing a
default and restarting the instance is enough (the producer is not involved):

```ini
# /etc/timberfs/text.conf — defaults for every instance
STORE_DIR=/var/log/timberfs/text
DECLARE=index=true retain=90d
```

A `text-<instance>.conf` overrides it key by key. `DECLARE` takes any manifest
property — `retain`, `retain_size`, `index`, `wal`, or free-form provenance —
but not a value containing spaces: systemd splits variables at whitespace, so
set such a property once with `timberfs set` and the manifest keeps it.

The trade against piped logging is backpressure: if the writer stalls (as
opposed to dying, which systemd fixes), the pipe fills — 64 KiB by default —
and Apache then blocks on the log write rather than dropping the line. Raise
the buffer with `PipeSize=` in a socket drop-in to cover a slow restart.

**Never `stop` the socket while the producer runs.** Apache and nginx both
open their log files with `O_CREAT`: with the FIFO node gone they would create
a *regular file* at that path, log into it unnoticed, and keep the socket from
ever coming back. That is why this pair sets `RemoveOnStop=no`, unlike the
records pair — with the node in place, a stopped socket costs lines while it is
down and self-heals when it returns. Restarting the *service*, or upgrading the
package, is always safe.

### Speaking Fluentd Forward — `timberfs-forward.socket` + `timberfs-forward.service`

Receive the [Fluentd Forward protocol v1](https://github.com/fluent/fluentd/wiki/Forward-Protocol-Specification-v1)
over TCP — the wire protocol Docker's `fluentd` log driver, Fluent Bit,
Fluentd and the fluent-logger client libraries already speak — with no
producer-side changes needed. Unlike the FIFO pair above this is **one TCP
listener for every tag**, not a template: Forward multiplexes tags over a
single connection, and each tag lands in its own store under
`/var/log/timberfs/forward/<tag>.log`.

By default the store set is **operator-controlled**: pre-create each tag's
store (`timberfs create --wal /var/log/timberfs/forward/<tag>.log` — with
`--if-not-exists` where that provisioning re-runs on every boot), and an
unknown tag is refused — logged once, never acked, so an acking sender
buffers and retries until the store exists. On a Docker host, where tags
are container names that come and go, opt into per-tag store creation with
a drop-in:

```ini
# systemctl edit timberfs-forward.service — Docker hosts: mint stores per tag
[Service]
ExecStart=
ExecStart=/usr/bin/timberfs forward-intake --into-dir /var/log/timberfs/forward \
    --exit-on-upgrade --auto-create
```

```sh
systemctl enable --now timberfs-forward.socket
# then point a Forward-protocol producer at 127.0.0.1:24224
```

```sh
docker run --log-driver=fluentd --log-opt fluentd-address=127.0.0.1:24224 \
    --log-opt tag={{.Name}} --log-opt fluentd-async=true \
    --log-opt fluentd-request-ack=true --log-opt fluentd-sub-second-precision=true \
    myimage
```

`fluentd-async` keeps a receiver outage from blocking the container's stdout;
`fluentd-request-ack` makes Docker retry a batch until it sees `{"ack": id}`
back, which this receiver sends only once the batch is durable
(see **Reliability model** below). The default Docker tag is a 12-char
container id — a poor store name — hence `tag={{.Name}}`.

**Deliberate limitations**, all downstream of Forward v1 having no
authentication or negotiation phase and this receiver adding none of its own:

- **No TLS, no handshake** — bind it to loopback or a private network only
  (override the address with a drop-in, see below); anything that can reach
  the listening address can write to any store under `--into-dir`.
- **No `CompressedPackedForward` (gzip)** — refused; the connection is logged
  and closed rather than silently dropping data.
- **No UDP heartbeat listener.**

The verb name (`forward-intake`) is provisional. Details, the wire modes
supported, and the partial-message reassembly are in `man timberfs`.

### Speaking OTLP — `timberfs-otlp.socket` + `timberfs-otlp.service`

Receive [OTLP/HTTP](https://opentelemetry.io/docs/specs/otlp/) logs — the
OpenTelemetry protocol every SDK and the Collector speak — so an existing
OTel pipeline can write into timberfs, and the Collector bridges syslog,
journald, Kafka and Fluent Bit in behind it. Like the Forward intake this is
**one TCP listener for every stream**: OTLP multiplexes resources inside the
request body, and each `ResourceLogs` lands in its own store under
`/var/log/timberfs/otlp/<service.name>.log`.

```yaml
# an OpenTelemetry Collector exporting to it — nothing to configure beyond
# the endpoint: both OTLP/HTTP encodings are accepted, gzipped or not
exporters:
  otlphttp/timberfs:
    endpoint: http://127.0.0.1:4318
```

```sh
systemctl enable --now timberfs-otlp.socket
```

The store set is **operator-controlled** exactly as above: an undeclared
stream is answered `503` with `Retry-After`, so the sender buffers and
retries until `timberfs create --wal` has made the store — or run with
`--auto-create`. Route by a different resource attribute (`host.name`,
`k8s.namespace.name`) with `--route`.

What arrives is a normal timberfs log: a record body that already opens with
a timestamp is stored verbatim, an unstamped one is prefixed with
`<RFC3339> <SEVERITY> `, and record attributes plus any `trace_id`/`span_id`
trail as `k=v` — so `timberfs query --has <trace_id>` finds a trace's lines
through the token index, with no trace backend involved. The resource
attributes are seeded into the store's `.bark` when it is created, because
they describe the stream rather than any one line.

**Deliberate limitations**, each refused explicitly and by name rather than
silently: `POST /v1/logs` only (traces and metrics are 404 — not a log
store's job); no chunked bodies (411 — a receiver that must acknowledge
durability needs to know what it is acknowledging); no TLS; and no gRPC on
:4317, which wants HTTP/2 — put a Collector in front if a sender needs it.

The verb name (`otlp-intake`) is provisional. `timber-otlp` ships the same
wire format in the other direction; a store shipped out and received back
arrives byte for byte.

### Shipping a store out — `timberfs-otlp@.service`

The other direction: read one store's entry stream and post it to an OTLP/HTTP
receiver. **A template, one instance per store** — the cursor is a position in
*one* store, and a stalled receiver must not hold up an unrelated one.
Configure the instance in `/etc/timberfs/otlp-<instance>.conf`:

```ini
STORE=/var/log/timberfs-backing/applogs/app.log
ENDPOINT=http://127.0.0.1:4318
EXTRA_OPTS=--service checkout --resource deployment.environment=prod
```

```sh
systemctl enable --now timberfs-otlp@applogs
```

It runs `timber-otlp --follow --cursor /var/lib/timberfs/otlp-<instance>.cursor`.
Being a **reader**, it cannot hurt the store or the appender: an unreachable
receiver stalls the shipper and nothing else, and the store is the send buffer
— retention is the disconnection budget (`retain 30d` means the receiver can
be gone thirty days). The cursor is written only after the receiver accepts a
batch, so an interrupted send is re-delivered rather than skipped
(at-least-once); `Restart=always` is therefore safe. Details: `man timber-otlp`.

## Ownership and permissions

- **The store directory** is created by `LogsDirectory=timberfs/%i` (or plain
  `timberfs/text` for the plain-text intake, `timberfs/forward` for the Forward
  intake, `timberfs/otlp` for the OTLP intake), owned by the service's `User=`
  (root by default). Set `User=` in a drop-in to own the directory as a
  specific user.
- **The FIFO** is created `root:root 0660`. A non-root producer cannot write it
  until you set the socket's group to one that user belongs to (`SocketGroup=`);
  there is no sane default, because the producer's identity is site-specific.
  Apache and nginx are the case that needs nothing: their privileged parent
  opens every log file before dropping to the worker user.
  (The Forward intake has no FIFO — it is a TCP listener, gated by the address
  it binds, not by filesystem permissions.)
- **Readers vs. writers.** `query` and `info` are read-only — they need only
  read access to the store, not write access to its directory. `append`,
  `index`, `reindex` and `rotate` are writers and *do* need directory write
  (they create files). That asymmetry is the whole reason the per-instance
  directory matters: a store's owner can index and rotate it; anyone with read
  access can still query it.

Common drop-ins (`systemctl edit <unit>`):

```ini
# timberfs-log@applogs.service — own the store as a user and retain 30 days
[Service]
User=applog
ExecStart=
ExecStart=/usr/bin/timberfs append --records --exit-on-upgrade \
    --into /var/log/timberfs/%i/%i.log --retain 30d
```

```ini
# timberfs-log@applogs.socket — let the producer's group write the FIFO
[Socket]
SocketGroup=applog
```

```ini
# timberfs-log@applogs.service — opt into the write-ahead sidecar: a crash
# then loses at most ~1s of intake instead of up to --flush-age
[Service]
ExecStart=
ExecStart=/usr/bin/timberfs append --records --exit-on-upgrade --wal \
    --into /var/log/timberfs/%i/%i.log
```

```ini
# timberfs-text@www.example.com.socket — cover a slower writer restart with a
# bigger kernel buffer (the default 64 KiB is a second or two of a busy log)
[Socket]
PipeSize=1M
```

```ini
# timberfs-forward.socket — bind a private/internal interface instead of loopback
[Socket]
ListenStream=
ListenStream=10.0.0.5:24224
```

## Locking

Two levels, all `flock`-based, so locks die with their process — a crash never
leaves a stale lock behind.

- **The directory lock** `.timberfs.lock`: a mount daemon holds it **exclusive**
  (it owns in-memory state for every store in the directory); appenders and
  offline rotation hold it **shared**. So any number of appenders coexist in one
  directory, but a mount and appenders never share one.
- **The per-store lock** `<name>.lock`: the writer's **exclusive** lock. A second
  writer of the same store is cleanly refused ("already has a writer"), never
  raced.

## Restart and upgrade

The units pass `--exit-on-upgrade`. When a package upgrade replaces
`/usr/bin/timberfs` on disk, the daemon flushes everything durably and exits
with a dedicated code, `85`; `SuccessExitStatus=85` + `RestartForceExitStatus=85`
make systemd restart it onto the new binary regardless of `Restart=`.

- **FIFO intake** is seamless: the `.socket` holds the FIFO open across the
  swap, so the producer sees no gap.
- **Mount** is clean: `auto_unmount` tears the old FUSE session down, and systemd
  remounts on the new binary.
- **Forward and OTLP intakes** are *not* seamless: unlike the FIFO, a `.socket`
  does not hold TCP connections open across a restart, so a swap drops them.
  This is by design rather than a gap — both protocols' senders already retry
  what was never acknowledged (that is what the chunk/ack handshake and the
  HTTP response are for), so the cost is the same at-least-once duplication a
  network blip would cause anyway.
- **The OTLP shipper** is a reader, so a restart cannot hurt the store: it
  resumes from its cursor, which only advances after the receiver accepted a
  batch, so an interrupted send is re-delivered rather than skipped.

## Reliability model (both intakes)

One contract, spelled in each protocol's own vocabulary: Forward acks a
`chunk` id, OTLP answers `200`. Below, "acked" means both.

- **At-least-once, not exactly-once.** An acked chunk is durable; an unacked
  one may or may not have landed, so the sender retries
  it — a retry after a receiver restart or a lost ack can duplicate entries.
- **Durable = fsynced into the `.sap` write-ahead sidecar** (every store this
  receiver touches declares `"wal": true`, see [design.md](design.md)). An ack
  costs one raw append plus one fsync — chunk compression keeps its own
  size/age cadence, so per-message-ack senders don't shred the store into
  one chunk per line.
- **Ack timing.** Acks are sent synchronously, as soon as the batch is
  durable — a blocking sender's throughput is bounded by fsync rate, not by
  any receiver tick.
- **An undeclared stream is refused, not acked** (unless `--auto-create`), and
  the refusal takes each protocol's own form: Forward simply withholds the ack,
  OTLP answers `503` with `Retry-After`. Either way an acking sender buffers and
  retries until the operator creates the store, then converges with nothing
  lost. Non-acking Forward senders' events for unknown tags are dropped (logged
  once per tag).
- **A malformed request is refused, not fudged.** A desynced msgpack stream
  can't be resynchronized, so the Forward connection is dropped and logged and
  the sender reconnects. OTLP names the reason in the status instead and keeps
  the connection: `400` undecodable, `404` a signal other than `/v1/logs`,
  `405` a method other than POST, `411` a chunked or unmeasured body, `413`
  over `--max-body`, `415` a content type that is neither protobuf nor JSON.

## See also

`timberfs(1)`, `timber-filter(1)`, `timber-otlp(1)`, `timberfs-records(5)`, and
the example configs at `/usr/share/doc/timberfs/examples/`.
