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

/var/log/timberfs/<name>/                 one directory per STORE, named after the
                                          store, whatever intake writes it —
                                          owned by that store's writer:
    <name>.log.trunk                        the data — chunked zstd frames
    <name>.log.rings                        the write-time index (per-chunk time bounds)
    <name>.log.grain                        optional token index (present with --index)
    <name>.log.bark                         JSON manifest: durable identity + retention
    <name>.log.sap                          optional write-ahead sidecar (present with --wal;
                                            always, for the acking network intakes)
    <name>.log.lock                         the store's writer lock
  .timberfs.lock                            the directory lock (see Locking)

    <name> is the instance for the FIFO and follower templates, the tag for the
    Forward intake, and the route value (default service.name) for the OTLP one.
```

### One layout, no intake in the path

Every intake writes `/var/log/timberfs/<name>/<name>.log`. The path says what the
store **is**, never how the data reached it: this is the `apache-access` store,
not the *followed* or *piped* one, and it keeps that name — and that store — if
the same log is later fed by a different route. It is also the name you query it
by, since a store's handle is its file name minus `.log` (see below), so path,
handle and `timberfs list` all say the same word.

That holds while one thing owns a name. It stops holding as soon as two do —
the same service logging over two routes, or two hosts' stores landing in one
archive — because a name is a single slot and they compete for it. The name is
metadata, not identity: what tells such stores apart is their **labels**, and
`timberfs list --select 'type=console,host=web01'` is how you ask for them.
Reach for a label before you reach for a longer name; a name that has grown a
suffix to avoid a collision has just moved the collision.

A store may therefore **declare** its name (`name` in the manifest) instead of
taking one from its path, which is what lets a path be opaque — an intake that
mints its own stores names the directory after the store's id and puts the
readable name where it belongs. `timberfs list` shows that name, `--names`
offers it, and `timberfs info` answers to it; a store that declares none is
still called what its path calls it, so both kinds render in the same column.
Declared names are not unique, and looking one up that several stores answer to
reports them rather than picking.

The directory is per store rather than per intake because a directory is the unit
that matters: **any** writer operation (indexing, rotation) needs write
permission on it, it is what carries the mount exclusion, and it is what one
owner can be given. Grouping stores is still a `STORE_DIR` / `--forest` away —
by *subject*, the axis timberfs cannot know:

```ini
# /etc/timberfs/follow.conf — every mail log in one place, one owner
STORE_DIR=/var/log/timberfs/mail
```

What *does* stay intake-specific is intake **configuration** —
`/etc/timberfs/text-<instance>.conf`, `/run/timberfs/text/<instance>.pipe` —
because that belongs to the mechanism rather than to the log.

#### Upgrading from 0.15.0 or earlier

Four defaults moved to that layout, so an existing install must be told which
one it wants **before** the units restart. Otherwise the writer creates a store
at the new path and the old one simply stops growing — nothing is lost, but the
history is split across two paths.

| intake | store was | store is now |
|---|---|---|
| `timberfs-text@<i>` | `/var/log/timberfs/text/<i>.log` | `/var/log/timberfs/<i>/<i>.log` |
| `timberfs-follow@<i>` | `/var/log/timberfs/follow/<i>.log` | `/var/log/timberfs/<i>/<i>.log` |
| `forward-intake` | `/var/log/timberfs/forward/<tag>.log` | `/var/log/timberfs/<tag>/<tag>.log` |
| `otlp-intake` | `/var/log/timberfs/otlp/<name>.log` | `/var/log/timberfs/<name>/<name>.log` |

**Keep the old paths** — one line each, no data movement:

```ini
# /etc/timberfs/text.conf  (and follow.conf, with its own directory)
STORE_DIR=/var/log/timberfs/text
```

For the two network intakes, keep the old destination in the drop-in that
already overrides their `ExecStart`; a stock install has none, so add one
(`systemctl edit timberfs-forward.service`) or accept the move. Note that the
old layout put every one of an intake's stores in ONE directory, which the
receivers no longer do: pinning `--into-dir /var/log/timberfs/forward` now
yields `/var/log/timberfs/forward/<tag>/<tag>.log`, not the old flat file. For
those two, moving is usually easier than pinning.

**Or move a store** to the new path, writer stopped (`.log.*` is every artifact
— trunk, rings, grain, bark, sap, lock):

```sh
systemctl stop timberfs-text@apache-access.service
mkdir -p /var/log/timberfs/apache-access
mv /var/log/timberfs/text/apache-access.log.* /var/log/timberfs/apache-access/
systemctl start timberfs-text@apache-access.service
```

The handle is unchanged either way (`timberfs query apache-access`), because a
handle never contained the directory.

The store's **logical name** is `<name>.log`, so you read it with the full
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

Declare another forest — a second disk, an archive — with `timberfs forest
create`, which creates the directory and writes the `.conf`:

```sh
timberfs forest create /srv/archive              # named `archive`
timberfs forest list                             # what is declared, and usable
```

It is idempotent, so provisioning may run it every boot, and it refuses a
directory that overlaps an existing forest — a forest is scanned at its root
and one level deep, so a forest inside a forest would make the stores between
them members of both, and their handles unresolvable. `timberfs forest remove`
un-declares one and never touches data.

`timberfs forest list` is also the check worth running before a write path goes
live: a directory that is `MISSING` or `READONLY` otherwise shows up as "store
not found", later, in another process.

The handle is the `.rings` file name minus `.rings` and a single trailing
`.log`, so both a flat `nginx.rings` and a nested `nginx/nginx.log.rings`
resolve as `nginx`. Full paths always win and nothing existing changes; edit
`DIR`, drop in another `*.conf`, or delete the file to disable the lookup (it's
a conffile, so edits survive upgrades). See `man timberfs` (FORESTS).

A glob spans the fleet through the artifacts a shell can see — a logical name is
not a file, so `.../'*'.log` matches nothing and `*.trunk` is the form that
works:

```sh
timberfs query /var/log/timberfs/*/*.trunk --from 13:40
```

An instance that needs more than one stream in one directory just sets a custom
`--into` in a drop-in.

## systemd units

Seven independent families ship with the package: one mount, four intakes
(FIFO, Forward, OTLP and the native replication wire) and two shippers —
`timberfs-otlp@`, configured per instance in `/etc/timberfs`, and
`timberfs-follower@`, which takes its configuration from the follower
registry instead. Prefer the latter for anything long-lived.

The three protocol intakes take data from *other* systems and compress it
here; `timberfs-frames` takes it from another timberfs and copies the
compressed chunks verbatim. That is the axis to choose on — what is on the
other end, not how much data there is.

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
DECLARE=index=true retain=365d format=apache-error
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
timber-filter --has shop.example.com /var/log/timberfs/apache-*/*.trunk --from 13:40

# hand that site's window to someone as a store of its own, provenance recorded
timber-filter --records --has shop.example.com apache-access \
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
site, where the consolidated layout has one writer for everything. Reading a
vhost's two streams as one interleaved, attributed view is a query over both
stores — by handle, since each has its own directory:

```sh
timberfs query www.example.com www.example.com.error --from 13:40
```

#### Both layouts

Retention and the index are configuration, not a command someone has to
remember — `DECLARE` is applied to the store on every start, so changing a
default and restarting the instance is enough (the producer is not involved):

```ini
# /etc/timberfs/text.conf — defaults for every instance
STORE_DIR=/var/log/timberfs
DECLARE=index=true retain=90d
```

A `text-<instance>.conf` overrides it key by key. `DECLARE` takes any manifest
property — `retain`, `retain_size`, `index`, `wal`, or free-form
provenance —
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

#### Two clocks, and when they diverge

`append` — what this pair runs, and what a piped `CustomLog` runs too — stamps
a chunk with the wall clock as it reads the line, while the line's own
timestamp is whatever the producer wrote into it. `query --from/--to` selects
chunks by the first clock and then verifies every entry against the second, so
the two tracking each other is what keeps that selection honest.

Apache's `%t` is the time the request was **received**, and the line is written
when the request **completes**. A slow request therefore diverges by its own
duration, and the access log is out of order by up to the slowest one. Chunk
selection absorbs a minute of that (the requested window is widened at both
ends); past that, a logline-time query can miss the line altogether, because
the chunk holding it lies outside the widened window. The same five-minute
request, queried by the minute it started:

```
piped / FIFO store   chunk window 09:13:59 .. 09:13:59   ->  0 lines
followed store       chunk window 09:08:59 .. 09:08:59   ->  1 line
```

Two ways to close it, if your requests can outlive that margin:

- **Log the completion time**, keeping the CLF shape so the built-in extractor
  still reads it: `[%{end:%d/%b/%Y:%T %z}t]` in place of `%t`, plus `%D` for
  the duration you have just made explicit. The `begin:`/`end:` prefixes need
  httpd 2.4.13 or later.
- **Follow the file instead** (below): a follower stamps chunks from the
  loglines themselves, so there is only one clock. Slow requests then merely
  make chunk windows overlap, which readers handle by design.

Neither clock affects `--has` or `timber-filter`'s predicates: the token index
is content, not time.

#### Exim, and anything else that dies when logging fails

Exim can be pointed at a FIFO — it opens its logs
`O_WRONLY|O_APPEND|O_CREAT|O_NONBLOCK` and clears the non-blocking flag right
after, so it attaches to a live FIFO and then blocks on a full pipe like any
other producer — and its default timestamp (`2026-08-18 10:26:09 …`, with or
without `+millisec`) is a clock timberfs already extracts, so nothing needs
declaring. Three things differ from a web server, and the middle one decides
whether you want this route at all:

- **Logs are opened as the Exim user.** A root Exim forks a child, drops to
  `exim_uid`, creates the file and passes the descriptor back, so the FIFO must
  be writable by that user — set `SocketUser=` (or `SocketGroup=`) in a socket
  drop-in. Apache and nginx need nothing here, because their privileged parent
  opens logs before dropping to the worker user. Check who Exim will be with
  `exim4 -bP exim_user`.
- **A failed log write is fatal to the process.** Exim's write-failure path
  ends in `log_write_die()`: the SMTP session or delivery is closed down and
  the process exits. With the socket unit holding the FIFO open this never
  fires — restarting the *service*, or a package upgrade, is invisible to Exim
  exactly as it is to any other producer. But stopping the **socket** while
  Exim runs makes every process that logs die, and mail defers until it is
  back. Senders retry, so that is delay rather than loss, and it is loud
  rather than silent (a fresh process fails at open with `ENXIO`, thanks to
  that `O_NONBLOCK`; a long-lived one gets `EPIPE` on its held descriptor) —
  but for a mail server it turns "never stop the socket while the producer
  runs" from advice into a rule — and it is why the **follower** below is the
  better default for Exim: reading the file it already writes puts nothing of
  timberfs in the path of a delivery.
- **`log_file_path` takes one path plus optionally `syslog`.** A second path is
  refused; a path and `syslog` are written to *both*, so syslog can be a
  parallel copy — though it does not protect mail flow, since it is the file
  write failing that kills Exim. All three logs follow the single `%s`
  template:

```
log_file_path = /run/timberfs/text/exim-%s.pipe : syslog
```

That gives three instances — `exim-main`, `exim-reject`, `exim-panic` — with
their own retention each, which is the point of splitting them:

```ini
# /etc/timberfs/text-exim-main.conf
DECLARE=index=true retain=90d format=exim-main
# /etc/timberfs/text-exim-reject.conf — security-relevant, keep longer
DECLARE=index=true retain=365d format=exim-reject
```

Note that this takes the **panic log** with it, and the panic log is where
Exim reports being unable to write a log. It is not silent when that happens
(it falls back to syslog, then stderr), but a mail server is a fair place to
be conservative: keep `log_file_path` on files and import them, or accept that
the panic path shares a mechanism with the thing it reports on.

Two smaller notes. Exim stats the log path and compares the inode before
reusing its open descriptor, so a replaced FIFO node is noticed and reopened;
`RemoveOnStop=no` keeps the node in place, and because `/run/timberfs/text` is
root-owned, the `O_CREAT` that plants a stray regular file behind a missing
node fails for the Exim user instead of diverging silently. And enable
`log_timezone` only if the store is read in the same zone Exim logs in: the
`+0200` it then appends is separated from the time by a space, which the
built-in ISO clock does not accept, so the entry is read as local time.

### A log you cannot reconfigure — follow the file

Some producers cannot be pointed anywhere useful, or should not be: a vendor
appliance, a package's own log, an MTA where a logging failure defers mail —
and any producer where you would rather nothing of timberfs sat in the path of
its writes. Leave those writing their own file and read it:

```sh
timberfs import --follow /var/log/exim4/mainlog \
    --into /var/log/timberfs/exim-main/exim-main.log
```

**The coupling is the point.** With the FIFO pair the producer writes into a
pipe this side has to keep draining, so a wedged writer becomes the producer's
problem — a blocked Apache worker, or an ended Exim process. A follower is a
reader: if it stalls, dies or is upgraded badly, the worst it can do is fall
behind. The failure mode drops from "the web server stalls" to "the archive
lags", which is a change of kind rather than degree. What you accept in
exchange is that the plain file still exists and still needs rotating.

| | latency | coupling to the producer | on a writer restart |
|---|---|---|---|
| `timberfs-text@` (FIFO) | flush age | the producer's write blocks on it (Exim: the process ends) | nothing lost — the kernel pipe buffers |
| `timberfs-follow@` (follower) | poll + flush age | none — it is a reader | nothing lost — the store is the checkpoint |
| `import` on a timer | the tick | none | nothing lost — same checkpoint |

A third axis the table leaves out: a follower stamps chunks from the loglines,
the FIFO pair from the wall clock at the time it reads them. For a producer
whose line timestamp is not its write time, that decides whether a
logline-time query is exact or leans on the selection widening — see [Two
clocks, and when they diverge](#two-clocks-and-when-they-diverge).

Rotation of the plain file stays the producer's business, and its retention is
no longer your archive — only the follower's safety margin. That frees you to
rotate far more often than you would otherwise: **hourly rotation on a busy
site** is worth doing, because it bounds both the plain file's size and the
scan a follower does at startup to re-sync against the store.

```ini
# /etc/timberfs/follow-exim-main.conf   (SOURCE is required)
SOURCE=/var/log/exim4/mainlog
DECLARE=index=true retain=90d format=exim-main
```
```sh
systemctl enable --now timberfs-follow@exim-main
```

Three rules carry the whole thing, and each is stated where an operator will
look — in the journal, as it happens:

- **The store is the checkpoint.** A start re-syncs against the lines the store
  already holds over the window the source covers, so a restart can neither
  lose nor duplicate. There is no position file to go stale, be restored out of
  step, or disagree with the store.
- **A descriptor is never abandoned before EOF.** When the path stops being the
  file it was, the file already open is drained first — so the lines written
  between the last read and the rename cannot be stranded, which is exactly
  what `tail -F` drops. Rotation needs no pattern at all while the follower
  runs; `--rotated` exists only for data written while it was *not* running,
  and defaults to `<source>.1` and `<source>.0`.
- **Every position decision is announced.** `mainlog was replaced (rotation);
  drained its last 812 byte(s) and switched to the new file` — a follower that
  silently reads the wrong thing would be worse than one that stops.

A source that shrank is reported as a truncation (`copytruncate?`) and re-read
from the start, with the honest note that whatever was written between the copy
and the truncate is lost to every reader, not just this one. A source that does
not exist yet is waited for, so the unit may start before its producer.

**Chunk size, and why `--flush-age` defaults to a minute here.** A chunk closes
on whichever comes first — `--chunk-size` (256 KiB of uncompressed data) or
`--flush-age` — plus once at exit, so a busy log fills 256 KiB chunks and a
quiet one closes whatever arrived in the window. For the appender that window
bounds crash loss, because its input is a pipe and unflushed data exists only
in memory. A follower's input is a file that stays on disk, and the store is
its checkpoint: a partial chunk lost to a crash is simply re-read. So the age
here only decides how soon new lines become queryable, and a short one costs
compression for nothing:

| followed at | `--flush-age 5` | `--flush-age 60` |
|---|---|---|
| ~10 lines/s (25 KiB) | 5 chunks, 1.7 KiB on disk (14.3x) | 1 chunk, 1.2 KiB (20.4x) |
| ~1 line/s (1.7 KiB) | 4 chunks, 555 B (3.1x) | 1 chunk, 225 B (7.7x) |

The minute matches what a one-shot import of the same bytes achieves, and is
still far inside a "couple of minutes" freshness budget. Lower it when you want
new lines queryable sooner, not for safety.

#### Without a daemon: `import` on a trigger

The same job, one-shot, when you would rather not run a follower. A store is
its own checkpoint, so importing the same growing file repeatedly adds only
what is new, and rotation needs no bookkeeping:

```sh
timberfs import /var/log/exim4/mainlog \
    --into /var/log/timberfs/exim-main/exim-main.log
#  imported 2 lines
#  imported 1 lines after 110 bytes already imported      <- after it grew
#  exim-main.log is already up to date; nothing imported  <- nothing new
#  after logrotate: the rotated file adds nothing, the fresh one adds its lines
```

Because a redundant run is a no-op, any trigger is safe to fire more often
than necessary. Three of them, in order of how well they fit a busy log:

**A timer** — the default choice for a log that is written continuously. Import
the ROTATED file first and the live one second, on every tick:

```ini
# /etc/systemd/system/timberfs-import-exim.service
[Service]
Type=oneshot
ExecStartPre=/usr/bin/timberfs create --if-not-exists --index --retain 90d \
    /var/log/timberfs/exim-main/exim-main.log
# The leading "-" matters: before the first rotation there is no mainlog.1,
# and a failing ExecStart would abort the unit before the live import ran.
ExecStart=-/usr/bin/timberfs import --quick /var/log/exim4/mainlog.1 \
    --into /var/log/timberfs/exim-main/exim-main.log
ExecStart=/usr/bin/timberfs import --quick /var/log/exim4/mainlog \
    --into /var/log/timberfs/exim-main/exim-main.log
```
```ini
# /etc/systemd/system/timberfs-import-exim.timer
[Timer]
# Both lines matter: OnUnitInactiveSec= measures from the last time the
# service DEACTIVATED, so on its own it never schedules a first run (the
# timer sits with no NEXT). OnActiveSec= gives it that first elapse.
OnActiveSec=1min
OnUnitInactiveSec=2min
[Install]
WantedBy=timers.target
```

Two sources per tick is what closes the rotation gap. Lines written between the
last tick and the rotation exist only in the rotated file, and a tick that
imported just the live one would never see them; taking the rotated file first
picks them up on the next tick, and the overlap is deduplicated. It also has to
be that ORDER: importing the fresh file first and the rotated one afterwards is
refused, because the store then holds newer data than the file being offered —

```
Error: already-imported data differs from the source (bytes 147..196)
— rotated or rewritten file? import it to a new target instead
```

— and that stranded tail cannot be added to the store afterwards. One
requirement: the rotated file must still be readable as text at tick time, so
keep logrotate's `delaycompress` (Debian's exim4 config already does).
`import` reads plain logs and `.timber` bundles, not `.gz`.

`--quick` matters once the store is large: a full import verifies every
already-imported chunk against the source, which is proportional to the store
(a cold-cache read of the whole thing), while `--quick` checks the first,
middle and last chunks and is therefore constant. On a 22 MiB, 348-chunk store
a redundant run measured 0.05 s full versus 0.02 s quick — small either way,
but one of those grows with the archive and the other does not. What `--quick`
gives up is noticing a source that was rewritten in the middle, which an
append-only log does not do.

**A path unit (inotify)** — right for a log that is written *rarely*, wrong for
a firehose. `PathModified=` fires on every write; `PathChanged=` only when a
writer that had the file open closes it (for Exim, its short-lived per-message
processes do exactly that, so this still fires per message). The hazard is the
trigger limit: it defaults to 200 activations per 2 s, and **if the limit is
hit the path unit goes into a failure mode and stops watching until it is
restarted** — a busy log does not make the watcher noisy, it kills it. So use
it where events are rare and immediacy is worth something:

```ini
# /etc/systemd/system/timberfs-import-exim-panic.path — paniclog is normally
# empty, and a write to it is something to act on now
[Path]
PathModified=/var/log/exim4/paniclog
[Install]
WantedBy=paths.target
```

Watch the *file*, not the directory: `DirectoryNotEmpty=` on a log directory is
true forever, so it triggers once and never again.

**logrotate's `postrotate`** — an alternative to the two-source tick above, if
you would rather close the rotation gap at the moment it opens than one tick
later:

```
postrotate
    /usr/bin/timberfs import --quick /var/log/exim4/mainlog.1 \
        --into /var/log/timberfs/exim-main/exim-main.log
endscript
```

It carries an ordering race the two-source tick does not: the hook has to win
against the next timer tick, because once the tick has imported the fresh file
the rotated one is refused (the error above) and its tail is unrecoverable in
place. logrotate runs `postrotate` immediately after the rename, so it normally
wins — but a hook that fails or is skipped leaves a hole. Prefer the two
`ExecStart` lines unless you need the tighter timing.

An existence-style path trigger is a poor substitute for either, because a
rotated file persists and `PathExists=`/`PathExistsGlob=` do not re-fire while
the path is still there.

A timer and a path unit can point at the same service — inotify for immediacy,
the timer as a floor so a coalesced or missed event cannot leave data
unimported indefinitely.

**Why not `tail -F | timberfs append`?** It is the obvious shape, and
`--follow` above is what it should have been. `tail -F` does follow a rotation,
and the latency is immediate; what it has no answer for is its own
restart. `tail -F -n 0` resumes at the END of the
file, so everything written while the pipeline was down is missing from the
store — silently, with the file still on disk to prove it. Worse, that hole
cannot be filled afterwards: a tail-fed store's write axis is when the appender
RECEIVED each line, while an import's is the line's own clock, so importing the
same file into that store is refused —

```
Error: mainlog (starts 2026-08-18 13:56:48.000) predates everything in
store.log (starts 2026-08-18 13:56:48.939) — import in chronological order,
or to a new target
```

— by less than a second, because a line's own timestamp necessarily precedes
its arrival. Starting from the beginning instead (`-n +1`) duplicates the whole
file on every restart. All three supported routes have a definite answer where
`tail` has none: the FIFO's kernel buffer carries the gap across a writer
restart, and the follower and the one-shot import cannot lose or duplicate
anything because the store is their checkpoint — which is also why a follower
stamps entries from their own timestamps rather than from arrival.

One thing to be clear-eyed about all the same: triggering an import is polling
avoidance bolted onto a batch tool, and each run re-verifies before it appends.
If low latency is the actual goal, the FIFO route above is the design that
delivers it — the producer's line is pushed to a live writer, with nothing to
re-verify. Importing on a trigger is what you choose when the producer must not
be touched.

### Speaking Fluentd Forward — `timberfs-forward.socket` + `timberfs-forward.service`

Receive the [Fluentd Forward protocol v1](https://github.com/fluent/fluentd/wiki/Forward-Protocol-Specification-v1)
over TCP — the wire protocol Docker's `fluentd` log driver, Fluent Bit,
Fluentd and the fluent-logger client libraries already speak — with no
producer-side changes needed. Unlike the FIFO pair above this is **one TCP
listener for every tag**, not a template: Forward multiplexes tags over a
single connection, and each tag lands in its own store at
`/var/log/timberfs/<tag>/<tag>.log`.

By default the store set is **operator-controlled**: pre-create each tag's
store (`timberfs create --wal /var/log/timberfs/<tag>/<tag>.log` — with
`--if-not-exists` where that provisioning re-runs on every boot), and an
unknown tag is refused — logged once, never acked, so an acking sender
buffers and retries until the store exists. On a Docker host, where tags
are container names that come and go, opt into per-tag store creation with
a drop-in:

```ini
# systemctl edit timberfs-forward.service — Docker hosts: mint stores per tag
[Service]
ExecStart=
ExecStart=/usr/bin/timberfs forward-intake --forest default \
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
  the listening address can write to any store in the forest it writes to, and with
  `--auto-create` can create new ones there.
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
request body, and each `ResourceLogs` lands in its own store at
`/var/log/timberfs/<service.name>/<service.name>.log`.

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

### Replicating between timberfs hosts — `timberfs-frames.socket` + `timberfs-frames.service`

The two intakes above take log data from *other* systems and compress it here.
When the sender is also timberfs, the native wire skips that work entirely:
compressed chunks are copied **verbatim**, so nothing is decompressed at either
end and the store on this host is byte-identical to its source — `.grain`
included, so a `--has` lookup here skips chunks exactly as it does at the
origin.

```sh
# the archive: take senders from other hosts, so off loopback
systemctl edit timberfs-frames.socket
  [Socket]
  ListenStream=
  ListenStream=0.0.0.0:4319

# and its policy — which is ITS policy: settings never travel, only labels
systemctl edit timberfs-frames.service
  [Service]
  ExecStart=
  ExecStart=/usr/bin/timberfs frames-intake --forest default \
      --route service --replica --index --auto-create --exit-on-upgrade

systemctl enable --now timberfs-frames.socket
```

Like the other two this is **one listener for every stream**, and it routes by
a label rather than a tag or a resource attribute: `--route service` puts each
stream at `/var/log/timberfs/<service>/<service>.log`. `--auto-create` is
absent from the shipped unit on purpose — an undeclared stream is refused and
said so, as elsewhere.

`--replica` is the one flag worth understanding. With it, the destination keeps
the sender's chunk **numbering** and records its origin, so `(origin_id, seq)`
names the same bytes at both ends; without it the destination renumbers and
claims no origin, which is weaker but always possible. The two travel together
or not at all — recording an origin while renumbering would produce an address
that lies, so it is refused rather than configured. A replica is also refused
if the numbering would not continue exactly, which is what a stream beginning
mid-tape does.

**One destination store, one origin.** A second origin arriving at a store that
already holds one is refused, naming both — otherwise a host reinstall, or two
hosts sharing a short hostname, silently appends a second tape to the first
while the manifest describes only one of them.

On the sending host, `timberfs frames-send STORE --endpoint archive:4319` ships
once; as a registered follower it is `--type frames`, which is what a service
unit should run:

```sh
timberfs set apache-error retain_size=5G retain_unconsumed=true
timberfs follower create --store apache-error ship-apache-error \
    --type frames --endpoint archive:4319 --retaining --enable --start
```

A frames follower takes no `--start`: it resumes from the **receiver's**
position, which is authoritative, so re-running ships nothing rather than
re-sending, and there is no local decision to get wrong. With `--retaining`
plus `retain_unconsumed`, retention here releases a prefix only once the far
end has acknowledged it.

There is no TLS: a private network, or a tunnel. And only *sealed* chunks
ship, so a replica trails its source by one chunk flush — the live edge
belongs to `query --follow` on the source itself. The verb names
(`frames-intake`, `frames-send`) are provisional.

### Tapping incus consoles — `timberfs-incus.service`

The one intake that goes *out* rather than being connected to: it opens the
local incus unix socket, attaches to each instance's live console, and follows
incus's lifecycle events so an instance started later is tapped too.

```sh
systemctl enable --now timberfs-incus.service
timberfs list --select 'type=console'
```

The socket is `root:incus-admin 0660`, so the unit carries
`SupplementaryGroups=incus-admin`. That is the whole privilege it wants — it
reads instances and attaches to consoles, and never writes to incus.

Stores are found by their **labels**, and which labels is yours to choose:

```ini
# systemctl edit timberfs-incus.service
[Service]
ExecStart=
ExecStart=/usr/bin/timberfs incus-intake --exit-on-upgrade     --forest default --index --retain 7d     --key type,incus.project,incus.instance
```

`--key` defaults to one store per instance. Add `image` for one per image
version; give only `type` to put every console on the host in one store — and
then `--prefix '{time} {incus.instance} '`, so its lines say which instance
wrote them, since labels are per store and cannot. Whatever `--key` names is
written as a label, so a store is always findable by the key that made it.

Three things worth knowing before you enable it:

- **A console is exclusive.** While the tap holds one, `incus console <name>`
  is refused unless forced, and forcing it takes the console from the tap
  (which reconnects). `incus console --show-log` still works — it reads the
  ring, which is a different feed.
- **The ring is consumed, not preserved.** The tap reads it at attach to recover
  the backlog, and then keeps consuming it as it streams — because the two feeds
  are independent, so an attached tap would otherwise leave the ring filling with
  a copy of what it has already written, and the next attach would replay it
  (duplicating up to 128 KiB on every restart). So `incus console --show-log`
  has nothing to show for a tapped instance. That is the trade: timberfs holds
  that content instead, indexed, retained and queryable. `--keep-ring` opts out
  and accepts the duplication; `--drain-every` (default 30s) bounds how much an
  unclean kill can duplicate.
- **Containers only**, unless `--include-vms`. A VM's console is file-backed
  and carries the kernel's boot output rather than an application's stdout.

*This is not where an application's own logs belong.* Something that can emit
OTLP should, into `timberfs-otlp.socket` above. The console is for what arrives
when the application's logger is already dead.

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

### `timberfs-follower@.service` — a shipper declared once, run by name

Prefer this to `timberfs-otlp@` for anything long-lived. Same shipper, but the
store, the type and the endpoint live in a **declaration** rather than in a
per-instance `.conf`, so there is one place that answers "what follows this
store", validated when it is written instead of at the next restart:

```sh
timberfs follower create applogs \
    --store /var/log/timberfs-backing/applogs/app.log \
    --type otlp --endpoint http://127.0.0.1:4318 \
    --retaining --enable --start \
    -- --service checkout --resource deployment.environment=prod
```

`ExecStart` is `timberfs follower run %i`, which reads that declaration and
**execs** `timber-otlp` — a dispatcher, not a supervisor, so systemd keeps the
lifecycle, the restarts and the journal, and timberfs adds no daemon. The
`--follow`, `--cursor` and `--endpoint` flags come from the registry; `--start`
is derived from `retaining` (`begin`, so a retaining follower's first run does
not skip the backlog it exists to protect). Anything after `--` reaches the
shipper verbatim.

The unit's `StateDirectory=` is `/var/lib/timberfs/followers/%i`, holding the
declaration, the position and the lock. **Those permissions are load-bearing**:
a position in there decides what a store's retention may drop, so on a store
declaring `retain_unconsumed`, anything that can write one can destroy data.
0755 keeps it observable by anyone and writable only by the follower — so a
`User=` drop-in gives one follower its own unprivileged identity without every
follower being able to write every position.

Secrets do **not** go in the declaration. The registry is world-readable on
purpose, so that `timberfs follower list` and `timberfs info` work as read-only
observations without write access anywhere; a bearer token belongs in
`/etc/timberfs/follower-<instance>.conf`, mode 0600, which the unit reads as an
`EnvironmentFile` (`timber-otlp` picks up `$OTEL_EXPORTER_OTLP_HEADERS`, the
spelling every OTel SDK uses).

> Not to be confused with `timberfs-follow@` — one letter, opposite direction.
> That one is an *intake* (`timberfs import --follow`), reading a producer's file
> **into** a store. This one reads a store **out**.

### Watching the disconnection budget

`timberfs follower list` shows each follower's position, lag and whether it is
running; `timberfs list` carries a FOLLOWERS column (how many, and the worst)
and `timberfs info` names each one, how far behind it is, and how much of the
store it alone is holding.

Liveness is read from the follower's **lock**, not from systemd: a lock is
released by the kernel on process death so it cannot go stale, while a unit's
state answers about the unit. (The lock is taken by `run` and inherited across
the exec, which is why the shipper needs no lock code — and why liveness also
checks the recorded pid: the shipper spawns its own reader, that child inherits
the descriptor, and such a child can outlive its parent.)

The number to watch is the held bytes: **retention is the disconnection budget,
and nothing enforces that a follower stays inside it**. A shipper down longer
than `retain` comes back to a position pointing at a dropped chunk; it warns on
resume (`GAP — N chunk(s) were dropped before it read them`) and continues
from the oldest chunk, because the loss is already in the past and a shipper
that refuses to start ships nothing. Alert on that warning, and on a follower
whose lag approaches the retention window.

`retaining` is one half of a pair. The **store** declares the other half, and
that is what actually moves the head:

```sh
timberfs set /var/log/timberfs-backing/applogs/app.log \
    retain_size=50G retain_unconsumed=true
```

`retain_unconsumed` is refused without a `retain_size`, and that is the design
rather than a validation nicety: interest only ever holds **more**, so with no
budget beside it one stalled follower pins the store until the disk fills —
which kills the *producer*, losing the newest data to protect the oldest.

⚠ **So the cap, not the consumption rule, is what decides an outage.** Size
`retain_size` as ingest-rate × the outage worth surviving. Interest retention
does not remove that sizing; it removes the steady-state hoarding — the weeks of
already-shipped bytes kept just in case — which is the actual win.

When the budget does override a follower, the writer records the loss exactly,
at the moment it happens, and this is the line to alert on:

```
app.log: retain_size (50.0 GiB) reached with follower central at chunk 4200
         — dropped chunks 4200..4830 it had not read
```

Two dependencies the guarantee rests on, neither of them timberfs's:

- **The receiver's `200` must mean PERSISTED, not merely accepted.** A collector
  with an in-memory queue acks and then loses the batch on restart, which
  silently voids the whole chain — erasure follows the position, and the position
  follows that ack.
- **The registry directory must not be writable by anything but the followers.**
  An attacker with write access there can fast-forward a position and have the
  next tick erase the record of their own intrusion.

⚠ Retention only ever runs inside a live **writer**. A store whose producer went
quiet keeps its data indefinitely — including data already shipped off the box,
which is the opposite of what this axis is for. `timberfs trim` is the cron-able
one-shot; it leaves a store somebody else is writing alone, because that
writer's own tick is already doing the job, and a store that declares no
retention is a no-op rather than an error:

```ini
# /etc/systemd/system/timberfs-trim.service  (pair with a daily .timer)
[Service]
Type=oneshot
ExecStart=/bin/sh -c 'timberfs list --names | xargs -rn1 timberfs trim'
```

The older `cursors=<dir>` key still works and is reported as superseded wherever
it is found.

## What a store declares about itself

A manifest holds two kinds of key, and mixing them up is the thing that makes a
fleet view hard to build later.

**What the store *is*** — identity (`id`, `created`), lineage (`derived_from`,
`window_from`…), settings (`index`, `wal`, `retain`, `retain_size`,
`retain_unconsumed`, `cursors`) and content format (`timestamp_*`).

The `id` also lives in the `.rings` header, because the backing pair is the
store: lose the manifest and the data still says what it is. The manifest is the
source of truth and the header its mirror, so a store predating that field is
stamped on its next write, and `timberfs create --if-not-exists` completes a pair
that carries none — which the shipped `timberfs-follow@.service` already runs on
every start. Where the two disagree, every writer refuses, because no writer can
know which identity a follower's cursors mean. `timberfs identity <store>` reports
that state and exits non-zero on it; `--keep index` (the pair, and the usual
answer after a manifest was hand-edited or restored), `--keep manifest`, or
`--mint` for a pair with no identity at all, resolve it. Stop the store's writer
first — repair rewrites the same header a head-drop does.

**Where its entries *came from*** — everything else. Free-form, yours, and the
only part a fleet view should select on. `timberfs list --json` exposes it under
`labels`; selecting on a *setting* would mean querying by an operational choice.

**The minimum is `host`:** the machine that produced the entries, not the hop
that delivered them. Nothing recovers it by reading a line once the entries have
been shipped elsewhere, so whatever writes the store first is the only thing
that can say. Every templated intake now stamps it (`host=%H` in an
`ExecStartPre`), `otlp-intake` takes it from the wire, and `forward-intake`
honours a sender-declared `hostname`/`host`/`nodename`/`source_host` — the
Forward protocol carries none, so it also records `peer`, the connecting
address, which is a fact rather than a guess. A bare `timberfs append` declares
nothing, which is legitimate.

**The stream's own name is the store's handle**, so a fleet keys on `host` plus
the handle — `apache-error` on `apache01` and `apache02` is two stores differing
in one label. Add anything else you want to slice by:

```sh
timberfs set /var/log/timberfs/apache-error/apache-error.log \
    service=apache env=prod datacentre=osl1
```

⚠ **A hop can rename a store.** After shipping, the handle is the *destination's*
name — an OTLP receiver routes by `service.name`, so the origin's store name
arrives as a label rather than as a handle. A view spanning hops therefore keys
on provenance and never on the handle. `man timberfs`, section PROVENANCE, has
the full table.

## Handing timberfs a search as a document

`timberfs query --query FILE|-` reads a whole search as JSON — which stores, what
window, what to match, and in what form the answer comes. It is the same value the
flags build, so the two cannot drift into being two dialects of one question, and
`--dump-json` prints the document any set of flags describes:

```sh
timberfs query /var/log/timberfs/app/app.log     --from 2026-08-26 --has ERROR --max 10 --dump-json > search.json
timberfs query --query search.json
```

That round trip is the intended way to learn the format, and worked examples ship
alongside it at `/usr/share/doc/timberfs/query-examples/` — one per capability that
is easy to miss, with a README naming what each demonstrates. The document is meant
to be *generated* — by a tool, a client library, or eventually a query server — and
the flags are the human sugar over the common shapes.

⚠ Not everything the flags do can be written as a document, and not everything a
document does has a flag. `--follow` is **refused** by `--dump-json` rather than
dropped: a following read holds a stream open, where a document describes one
search. Going the other way, the document chooses whether its predicates select
entries or whole chunks, and carries `substring`, `regex`, caseless and `none`
predicates that `timberfs query` has no flags for — `timber-filter` is the command
line for those, and shares the implementation, so the two cannot disagree.

Two rules a generator has to know, both documented in `timberfs-query-document(5)`:
an **omitted member widens** the search rather than emptying it, and an **unknown
member is an error** rather than being ignored. The second is the opposite of the
rule for the records *response* format, deliberately: a consumer should tolerate a
producer that grew a field, but a request that tolerates a typo does something other
than what was asked.

It also means **forward compatibility is not possible**: a newer timberfs reads an
older document, but an older one refuses a document using a member added after it.
Generators must match or lag the timberfs they talk to, never lead it.

## Ownership and permissions

- **The store directory** is created by `LogsDirectory=timberfs/%i` in the
  templated intakes, owned by the service's `User=` (root by default). Set
  `User=` in a drop-in to own one store's directory as a specific user. The two
  network intakes are not templated, so systemd can only own their root
  (`LogsDirectory=timberfs`): with `--auto-create` such a receiver creates each
  store directory itself and therefore needs write permission on that root, so
  a confined instance is better given a **forest** of its own — a *sibling*,
  not a subdirectory, since forests may not nest (`timberfs forest create
  /srv/docker-logs`, then `--forest docker-logs`). Without `--auto-create` it
  creates nothing — the operator pre-creates each store, wherever its owner
  should be.
- **The FIFO** is created `root:root 0660`. A non-root producer cannot write it
  until you set the socket's group to one that user belongs to (`SocketGroup=`);
  there is no sane default, because the producer's identity is site-specific.
  Apache and nginx are the case that needs nothing: their privileged parent
  opens every log file before dropping to the worker user. The file follower has
  no FIFO at all — it needs READ access to the producer's log instead, so a
  `User=` drop-in there wants a group like `adm` on a Debian `/var/log`.
  (The Forward intake has no FIFO — it is a TCP listener, gated by the address
  it binds, not by filesystem permissions.)
- **Readers vs. writers.** `query` and `info` are read-only — they need only
  read access to the store, not write access to its directory. `append`,
  `index`, `reindex` and `rotate` are writers and *do* need directory write
  (they create files). That asymmetry is the whole reason the per-store
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
  batch, so an interrupted send is re-delivered rather than skipped. A restart
  long enough for retention to pass the cursor is the one lossy case, and it
  warns about it by name (GAP) instead of resuming silently.

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
[Concepts](concepts.md) indexes the vocabulary used here — store, forest,
follower, intake, the two clocks — and points at wherever each is explained.
