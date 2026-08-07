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
/etc/timberfs/forests.d/*.conf            forests: directories searched by handle (see below)

/run/timberfs/<instance>.pipe             intake FIFO, created by the .socket unit
                                          (the directory is created at boot by tmpfiles.d)

/var/log/timberfs/<instance>/             one directory per log-intake instance,
                                          owned by that instance's service user:
    <instance>.log.trunk                    the data — chunked zstd frames
    <instance>.log.rings                    the write-time index (per-chunk time bounds)
    <instance>.log.grain                    optional token index (present with --index)
    <instance>.log.bark                     JSON manifest: durable identity + retention
    <instance>.log.sap                      optional write-ahead sidecar (present with --wal)
    <instance>.log.lock                     the store's writer lock
  .timberfs.lock                            the directory lock (see Locking)

/var/log/timberfs/forward/                one directory shared by every tag the
                                          Fluentd Forward intake sees (it is a
                                          single TCP listener, not templated):
    <tag>.log.trunk / .rings / .grain / .bark / .lock     one store per tag
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

Two independent families ship with the package.

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

### Speaking Fluentd Forward — `timberfs-forward.socket` + `timberfs-forward.service`

Receive the [Fluentd Forward protocol v1](https://github.com/fluent/fluentd/wiki/Forward-Protocol-Specification-v1)
over TCP — the wire protocol Docker's `fluentd` log driver, Fluent Bit,
Fluentd and the fluent-logger client libraries already speak — with no
producer-side changes needed. Unlike the FIFO pair above this is **one TCP
listener for every tag**, not a template: Forward multiplexes tags over a
single connection, and each tag lands in its own store under
`/var/log/timberfs/forward/<tag>.log`.

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

## Ownership and permissions

- **The store directory** is created by `LogsDirectory=timberfs/%i` (or plain
  `timberfs/forward` for the Forward intake), owned by the service's `User=`
  (root by default). Set `User=` in a drop-in to own the directory as a
  specific user.
- **The FIFO** is created `root:root 0660`. A non-root producer cannot write it
  until you set the socket's group to one that user belongs to (`SocketGroup=`);
  there is no sane default, because the producer's identity is site-specific.
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
- **Forward intake** is *not* seamless: unlike the FIFO, the `.socket` does not
  hold TCP connections open across a restart, so a swap drops them. This is by
  design rather than a gap — Forward-protocol senders already reconnect and
  retry any unacked chunk (that is what the chunk/ack handshake is for), so the
  cost is the same at-least-once duplication a network blip would cause anyway.

## Reliability model (Forward intake)

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
- **A decode error closes the connection.** A desynced msgpack stream can't be
  resynchronized, so the connection is dropped and logged; the sender
  reconnects.

## See also

`timberfs(1)`, `timberfs-records(5)`, `timber-filter(1)`, and the example config
at `/usr/share/doc/timberfs/examples/timberfs.conf.example`.
