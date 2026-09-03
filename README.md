# timberfs

*The only filesystem where `head -f` would make sense.*

A purpose-built home for log files — stored compressed, searchable in
milliseconds.

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

Storage is the middle of a log pipeline, and timberfs speaks both ends of it
too. **In**: OTLP/HTTP from any OpenTelemetry SDK or Collector, Fluentd Forward
from Docker's log driver and Fluent Bit, a pipe, a file it tails, or a FUSE
mount for software that insists on a real path. **Out**: [OTLP to any
backend](#shipping-onward-timber-otlp) — one record per entry, resumable across
restarts, with the store itself as the send buffer, so retention *is* the
disconnection budget and any window can be re-shipped afterwards; or [native
replication](#replicating-to-another-timberfs-frames-send) to another timberfs,
which moves the compressed chunks verbatim — token index included, nothing
decompressed at either end.

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
timberfs identity backing/app.log              # is the store's id sound? (repairs with --mint/--keep)
timberfs query backing/app.log --from 2026-08-26 --has ERROR --dump-json   # the search, as a document
timberfs query --query search.json             # ...and run one back
timberfs incus-intake --forest default   # tap incus container consoles
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
(`timberfs query nginx`), `timberfs list` shows what's there, `timberfs forest
create` declares another one, and the package ships shell completion for all
three tools. Full reference: `man timberfs`,
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
timberfs umount /var/log/myapp            # finds fusermount3 or fusermount
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
timberfs forward-intake --forest default &

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
timberfs otlp-intake --forest default --auto-create &
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
single query spans the fleet — chunks interleave by the window they were
WRITTEN in, and each line carries a `path:` prefix showing who logged it. (The
framed answer reads the stores one after another instead and claims no order
between them; see `timberfs-records(5)`, ORDERING.)

```sh
timberfs query --from 13:42 --to 13:43 collector/host*-app.log
timber-filter --has req-8f3a collector/*.log        # which hosts saw it?
```

For a fleet that is not one machine's directory, there is a console:
`timbersh`, in the separate `timberfs-sh` package. It reads many hosts as
one set of stores, selecting them by what they DECLARE rather than by
where they sit:

```sh
timbersh --hosts web01,web02,db01 --cmd "ssh _TIMBERHOST_ timberfs query --query -"
```

How a fleet is reached is a property of each **target**, not of the session,
so a `ssh mail01 timberfs …` and a site wrapper taking the host as an
argument can be in one set. A resolver — any command that prints the list —
derives it, and `~/.config/timberfs/targets.json` holds the same document
where there is nothing to derive it from:

```json
{"v": "1.0-EXPERIMENTAL",
 "targets": [{"name": "mail01", "cmd": ["ssh", "mail01", "timberfs", "query", "--query", "-"]},
             {"name": "web01",  "cmd": ["site-wrapper", "query", "web01"]}]}
```
```sql
select loglines from [type=apache] where logline since '00:00' and entry has 'error' limit 20;
```

Beside it, `timberview` reads ONE store the way a pager reads a file — the
last chunk first, back from there, no predicate and no result set. It
parses nothing, so it works where a query helps least: a store whose lines
timberfs cannot read, and one with no index. `Tab` moves between the
tokens the index actually holds, `Enter` finds one on every host, and a
hit is a coordinate you can open, cycle and paste. `m` sets the mark and
`c` copies the region — or, with no mark, the whole ENTRY under the
cursor, which is how a stack trace gets out of a log and into something
that analyses one:

```sh
timberview app.log
timberview 'timber://mail01/79d7f23a-b044-4a72-8be3-d26e0481d202#offset=33724753900'
```

⚠ EXPERIMENTAL, both of them: timbersh's statements are not promised. They
exist to be used against a real fleet, because a protocol nobody writes a
client against is one whose awkward parts stay theoretical — see
[tools/](tools/README.md).

The deployment *shapes* all of this composes into — giving an application OTLP
without touching it, a full-fidelity tier under an expensive backend, container
logs, replaying an incident window into a backend — are in
**[Use cases](docs/use-cases.md)**, with the limits that come with them.

The full command reference — every flag, `import`/`export`/`rotate`, retention,
forests, `.timber` bundles, and the records stream — is in the man pages:
`man timberfs`, `man timber-filter`, `man timber-otlp`, and `man timberfs-records`.
A search can also be handed over as a JSON document rather than assembled from
flags — what a tool, a client library or a query server hands to timberfs.
`timberfs query --query FILE` runs one and `--dump-json` prints the document any
set of flags describes. It says what to search (stores by *label*, never by
path), over what window, for what — `has`, `substring`, `regex`, caseless, and
`none` for what must NOT appear — and in what form the answer comes, down to
"just tell me which stores match". Worked examples ship at
`/usr/share/doc/timberfs/query-examples/`, with a README naming what each one
demonstrates; the contract is `man timberfs-query-document` plus a JSON Schema
at `/usr/share/doc/timberfs/query-document.schema.json`.

Because a document comes from somewhere else, the machine answering it has
ceilings on what one may ask for — on by default, overridden in
`/etc/timberfs/limits.conf`, and shown by `timberfs limits`. They bound a
document rather than the flags an operator types, and a bounded answer is a
page: it names which ceiling stopped it and carries the positions that resume
it, so an unbounded document is answered with a first page rather than with
everything.

When a term wants a definition rather than a tour — chunk, entry, follower,
forest, the two clocks — **[Concepts](docs/concepts.md)** indexes the
vocabulary, one line each, with a pointer to wherever it is explained.

## Shipping onward (`timber-otlp`)

A store is not a dead end. Declare a **follower** — which stores, and what
consumes them — and its records go to any OTLP/HTTP receiver, one LogRecord per
**entry**, so a stack trace arrives as one record rather than forty:

```sh
timberfs list --select '[service=~apache-.*]'     # what it will ship
timberfs follower create collector \
    --select '[service=~apache-.*]' --enable --start \
    -- timber-otlp --endpoint http://collector:4318
```

One process and one unit per **destination**, whatever the store count — the
predicate is re-resolved on every poll, so a container that starts tomorrow is
picked up with no new unit and no edit. `timberfs feed` is the same loop with
the declaration on the command line, for trying a consumer out:

```sh
timberfs feed --follow --select '[service=~apache-.*]' \
    -- timber-otlp --endpoint http://collector:4318
```

Every consumer is a **child process**, so an unreachable receiver stalls its own
follower and nothing else — the appender never notices. **The store is the send
buffer**: where a collector's queue is sized by guessing, retention is the
disconnection budget (`retain 30d` means the receiver can be gone for thirty
days), and any window can be re-shipped afterwards, which a collector cannot do
because it retains nothing. A replay is a bounded query piped in, being a
deliberate act rather than a mode:

```sh
timberfs query backing/app.log --records \
    --from '2026-08-11 14:00' --to '2026-08-11 15:00' \
  | timber-otlp --endpoint http://new-backend:4318
```

The two OTLP time fields land on timberfs's two axes: `timeUnixNano` gets the
entry's own logline stamp, `observedTimeUnixNano` the write time it arrived at.
One request carries a `ResourceLogs` group **per store**, each with that store's
own `service.name`, so a selection of four hundred stores is never flattened
into whichever one the stream opened with. Delivery is at-least-once, as OTLP
itself is. `--dry-run` prints exactly what would be posted. Protobuf by default
(`--encoding json` for a readable wire, `--compress gzip` over a network);
plaintext HTTP only — terminate TLS in a collector beside it. Details: `man
timber-otlp`.

### Followers: who is reading, and how far behind

Retention acts on the head of a store and nothing coordinates it with a
consumer's progress, so a follower down longer than the retention window comes
back to find the chunk its position points at already dropped. That is reported
rather than absorbed — and reported from the store's side, *before* it becomes
loss, for every **registered follower**. A follower is a declared object: a
name, a selection of stores, a consumer to feed them to, a `retaining` flag and
a durable position **per store**.

```
$ timberfs follower list
NAME       SELECT                  STORES  RETAINING  WORST LAG      RUNNING
collector  [service=~apache-.*]    12      yes        6d 2h behind   yes
audit      [class=audit]           3       yes        never read     no

$ timberfs follower status collector
collector  12 store(s)  retaining  running (pid 4711)
  select   [service=~apache-.*]
  consumer timber-otlp --endpoint http://collector:4318
  apache-web01   chunk 4831   6d 2h behind   1.2 GiB unread   41.2k delivered
  apache-web02   chunk 9902   at the live edge                88.1k delivered
  apache-web03   —            never read
                 "418 I am a teapot" (2026-09-01 14:02)
```

The held figure is the number to act on: a store is large because somebody is
behind, and this names which. `audit` leads that in `list` because a retaining
follower with **no position holds everything** — which is the point (it is what
protects a follower deployed before it first runs) and equally the footgun, the
same one an unused Postgres replication slot has.

That parallel is not decoration. A follower *is* a replication slot: an
operator-chosen name unique per host, the registration recording what it reads,
and an unused one pinning data forever with a size budget as the backstop.
timberfs stays a log with interest-based truncation, not a work queue —
position-based and at-least-once, no per-entry ack, no redelivery, no
dead-letter.

**Which stores, not which store.** That is the whole reason a follower has a
selection: one forwarder per destination, rather than one systemd unit and one
process per store. There is deliberately no *add a store to this follower*
verb — the answer is to label the store, or to widen the predicate — and no
follower *group*, because a group over a selection is a second way to say the
same thing. A store joining the selection is picked up per `--follow-from`:
`discovery` (the default) reads one born since the follower was declared from
its beginning and one that predates it from its next byte, `begin` reads
everything, `end` reads no history at all. `--retaining` implies `begin`, having
promised the data is not lost until this follower has it.

Each store is remembered by identity (its `.bark` id, minted by `--store` when
it has none) — a store can move, and a path can come to hold a different store.
A store with no identity is not followed, and `follower status` says so rather
than silently reading fewer stores than it matched.

The registry is one directory per follower, and the file split follows
ownership:

```
/var/lib/timberfs/followers/collector/
    follower.json    select, retaining, command    (the operator writes)
    positions.json   a place per store it has read  (the follower writes)
    follower.lock    held while it runs             (`run` acquires)
```

"The operator writes" means *through the verbs* — all three are readable and
none is edited by hand. `follower.json` is configuration and `follower update`
changes it; `positions.json` is **state**, and its `offset` (resume point) and
`chunk` (retention floor) mean different things and move together, so editing
one is a lie about the other. To make a follower read a store again: stop it,
remove that store's entry, start it — absence moves both fields at once, where
an edit moves one. `man timberfs`, **THE FILES ARE NOT THE INTERFACE**.

systemd runs them: `timberfs-follower@collector`'s `ExecStart` is `timberfs
follower run collector`, which resolves the selection, spawns the consumer and
feeds it. No per-instance `.conf` holding a store and an endpoint — that is what
the registry is for. Retiring one is deliberately two commands, because the
destructive act deserves its own:

```sh
timberfs follower update collector retaining=false   # releases the heads, and says what
timberfs follower delete collector --stop --disable  # bookkeeping
```

`update retaining=false` quantifies what it frees and says the part that is easy
to miss — the *flag* toggles but its *effect* does not: setting it back will not
bring dropped data back. `delete` refuses while a follower is retaining or
running; both refusals are about deliberateness rather than prevention, so there
is no `--force` — the two-step *is* the force.

`retaining` is one half of a pair — the **store** declares the other half, and
that is where retention actually changes:

```sh
timberfs set app retain_size=50G retain_unconsumed=true
```

Now the head follows delivery. `retain_unconsumed` is refused without a
`retain_size`, and that is the design rather than a validation nicety: interest
only ever holds **more**, so with no budget beside it one stalled follower pins
the store until the disk fills — which kills the *producer*, losing the newest
data to protect the oldest. Which means the cap, not the consumption rule, is
what decides an outage: size it as ingest-rate × the outage worth surviving.
Interest retention doesn't remove that sizing — it removes the *steady-state
hoarding*, the weeks of already-shipped bytes kept just in case, which is the
actual win.

The three axes combine with `max`, never `min`: each names a head prefix it
would be happy to see gone, and the largest wins, so no axis can hold data
another has released. And when the budget does override a follower, the writer
records the loss exactly, at the moment it happens:

```
app.log: retain_size (50.0 GiB) reached with follower central at chunk 4200
         — dropped chunks 4200..4830 it had not read
```

That's owed, not optional. With finite disk, bounded loss is a choice already
made — the alternative is blocking the producer — so what's owed is precise
accounting, and the writer holds both halves of the comparison right there.
`follower status`'s `GAP` is the same fact read back afterwards, from the
position's side; this one is written as it happens.

Retention only ever runs inside a live **writer**, so a store whose producer went
quiet keeps its data — including data already shipped off the box. `timberfs
trim` is the cron-able one-shot for that, and it leaves a store somebody else is
writing alone, because that writer's own tick is already doing the job:

```sh
timberfs trim app --dry-run   # how many chunks interest would drop
timberfs trim app

# the cron-able form: every store a predicate matches, and drop one the
# trim leaves empty — see `man timberfs` before pointing this at `[]`
timberfs trim --select '[class=container]' --delete-empty
```

`--delete-empty` removes a store left holding nothing **that once held
something**: one pre-created and never written has no chunks either, and it is a
placeholder waiting for its producer, so what tells them apart is that only the
first has *dropped* anything. A store with a live writer is never touched, and
one a retaining follower covers is refused rather than pulled out from under it.
⚠ Deleting a store takes its `.bark` — identity, labels and retention policy —
so scope it with the predicate rather than `[]`: an intermittent producer is
empty most of the time.

> The older `cursors=<dir>` key still works and is reported as superseded.

## Feeding any consumer (`timberfs feed`)

`timber-otlp` is one consumer. Any program that reads the record stream and
reports back is another, and `timberfs feed` is the loop that runs one without
a declaration:

```sh
# every apache store on the host, to a program that speaks the protocol
timberfs feed --follow --select '[service=~apache-.*]' \
    --positions /var/lib/timberfs/collector.positions.json \
    -- my-consumer

# a destination on another machine: the contract is two file descriptors
timberfs feed --follow --select '[]' \
    --positions /var/lib/timberfs/all.positions.json \
    -- ssh archive01 my-consumer
```

One process whatever the store count, and a store created tomorrow is picked up
without touching the command. `[]` is the predicate with no terms — every store
— which is a thing to have written rather than a flag to leave out. Without
`--positions` the places live only as long as the process, which is a temporary
watch; a follower always has one, in its registry directory.

**timberfs owns the position; the consumer says how far to move it.** A consumer
says hello, is fed `timberfs-records(5)`, and answers with a watermark per store
meaning *do not send me these again* — so a receiver that is down gets the same
entries again, while an entry refused for being too old is reported past and
never re-sent. A `note` says why nothing is moving, and is kept where `follower
status` can show it. No hello, no run.

It is small enough to implement in a shell script, which is the point of it
being a protocol rather than a trait:

```sh
printf '\036hello\037v=1\037reads=records\000'
while IFS= read -r -d '' rec; do
    # ... an entry record states id, offset and len ...
    printf '\036progress\037id=%s\037offset=%s\000' "$id" "$((off + len))"
done
printf '\036note\037text="%s"\000' 'the receiver said 418'
```

A `note`'s text — and a hello's `holds` — is JSON, because a record value may
contain neither NUL nor US and free text can contain both. `man
timberfs-records` is the reference; the store's own labels arrive with the
entries as a `source` record, so a consumer needs no access to the store it is
being fed.

## Replicating to another timberfs (`frames-send`)

OTLP above ships **entries** to anything that speaks the protocol. When the far
end is also timberfs, the native wire ships **frames** instead — the compressed
chunks, verbatim:

```sh
# on the archive
timberfs frames-intake --forest default --listen 0.0.0.0:4319 \
    --route service --auto-create --replica --index

# on the node
timberfs frames-send /var/log/timberfs/apache-error/apache-error.log \
    --endpoint archive:4319
```

Nothing is decompressed at either end, so the destination's `.trunk` is
byte-identical to the source's — `.grain` included, which means a `--has` lookup
on the replica skips chunks exactly as it does at home. Re-running sends
nothing: the receiver's position is authoritative, so a sender keeps no cursor
of its own and cannot re-send.

With `--replica` the destination also keeps the sender's chunk numbers and
records its origin, so a chunk answers to the same address at both ends; without
it, the destination renumbers and claims no origin. The two travel together or
not at all. Declared as a follower — `-- timberfs frames-send --endpoint …` —
retention releases a prefix only once the far end has acknowledged it.

Frames replicate, records merge: interleaving two sources into one store needs
decoding, which is the entries path's job. See **REPLICATION** in
`man timberfs`.

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

Continuous retention is declared on the store, on three axes, and enforced by
every writer on its own tick:

```sh
timberfs set app retain=90d retain_size=50G retain_unconsumed=true
```

Keep at least 90 days, stay under 50 GiB compressed, and keep whatever this
store's [retaining followers](#followers-who-is-reading-and-how-far-behind)
have not read. They combine with `max`, never `min` — each names a head prefix
it would be happy to see gone, and the largest wins, so no axis can hold data
another has released.

Retention runs *inside a writer*, so an idle store keeps its data; `timberfs
trim` is the cron-able one-shot, and it leaves a store somebody else is writing
alone because that writer's tick is already doing the job.

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
releases on every release, GPG-signed, `apt upgrade` works). The `amd64`
package is built against an old glibc, so it installs on every current
release — Ubuntu 20.04+ and Debian 11+ (18.04 has its own package, below):

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

# The console, separately — it needs python3, which the filesystem does not.
sudo apt install timberfs-sh
```

Or grab a single `.deb` from the latest GitHub release (built, VM-tested
and provenance-attested by CI — verify with
`gh attestation verify timberfs_amd64.deb --repo torstei/timberfs`):

```sh
curl -LO https://github.com/torstei/timberfs/releases/latest/download/timberfs_amd64.deb
sudo apt install ./timberfs_amd64.deb
```

Or from crates.io with a Rust toolchain: `cargo install timberfs`.

### Ubuntu 18.04

One release below that floor gets its own package, `timberfs-bionic`, from
the same repository — built against glibc 2.27 and depending on `fuse3 |
fuse`, since bionic has no fuse3. It `Provides: timberfs` and conflicts with
it, so the two are alternatives rather than an upgrade path:

```sh
sudo apt install timberfs-bionic
```

Only `libc6` is a hard dependency, so the `.deb` from the release also
installs on a host whose apt sources no longer resolve:

```sh
curl -LO https://github.com/torstei/timberfs/releases/latest/download/timberfs-bionic_amd64.deb
sudo dpkg -i ./timberfs-bionic_amd64.deb
```

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
the curious and the contributors start there. Where a direction is still being
designed rather than described, the note is under
**[docs/plans/](docs/plans/)** and the roadmap points at it.

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
socket-activated `timberfs-forward` and `timberfs-otlp` (not templated — both
multiplex every stream over one listener) for the two network intakes above,
and — in the other direction — `timberfs-follower@<instance>`, which runs a
*registered* follower: which stores and what consumes them come from the
declaration rather than from a per-instance `.conf`, so it is one unit per
destination and not one per store.

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
