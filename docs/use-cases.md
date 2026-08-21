# What timberfs is for

A store is not a destination but an **interchange**: several ways in, several
ways out, durable in the middle. Most of what follows is a composition of those
two lists rather than a feature of its own.

```
in                              store                       out
──                              ─────                       ───
import      historical files ┐                    ┌─ query / timber-filter   the pipe
append      a pipe           │                    │
mount       a real path      ├─>   app.log      ──┼─ timber-otlp             OTLP/HTTP
forward-intake  Docker, …    │   compressed       │
otlp-intake     OTel, …      ┘   indexed          └─ export                  .timber bundle
                                 retained
```

The README is the feature tour; this is the deployment shapes. Each section is
a situation, the composition that answers it, and why the usual tool doesn't.

## Keep months of logs on the disk you already have

The baseline. zstd at 10–20x, and **retention as a property of the log** rather
than a cron job: the oldest data is dropped continuously from the front, with
no rotate-and-delete seam and no rewrite.

```sh
timberfs create --index --set host=$(hostname) backing/app.log
timberfs set backing/app.log retain_size=50G      # or retain=90d; live, no restart
myapp 2>&1 | timberfs append --into backing/app.log
```

*Why not logrotate + gzip:* a rotated archive is unsearchable without
decompressing it, and rotation buys room by making yesterday expensive to read.
Here the compressed form is the queryable form, and the retention number is
declared once in the manifest instead of enforced by a schedule.

## Answer "what happened at 13:42" across a fleet

Keep one store per host/app and merge them at **read** time. There is no
cluster and no ingest tier to run: chunks interleave by timestamp across files,
and each line carries a `path:` prefix naming who logged it.

```sh
timberfs query --from 13:42 --to 13:43 collector/host*-app.log
timber-filter --has req-8f3a collector/*.log       # which hosts saw it?
```

A **forest** (`/var/log/timberfs` by default) lets a bare handle stand in for a
path, so `timberfs query nginx` works from anywhere and `timberfs list` shows
what is there.

*Why not a log cluster:* this answers the two questions that dominate an
incident — a time window, and where an id appears — against files you already
have, with nothing to keep running between incidents.

## Give an application OTLP without touching the application

An app that cannot be instrumented can still be a first-class OTLP log
producer, because the store sits between "writes lines" and "speaks OTLP":

```sh
# 1. get its output into a store — a pipe if it writes stdout,
myapp 2>&1 | timberfs append --into backing/app.log
#    a mount if it insists on a real path
timberfs mount /var/log/myapp-backing /var/log/myapp

# 2. ship the store, resumably across restarts
timber-otlp --follow --cursor /var/lib/timberfs/app.otlp \
    --endpoint http://collector:4318 backing/app.log
```

The shipper reads the standard `OTEL_EXPORTER_OTLP_ENDPOINT` /
`OTEL_EXPORTER_OTLP_HEADERS` environment, takes `--service` and `--resource
k=v` for the resource attributes (defaulting to the store's `.bark`), and
`packaging/timberfs-otlp@.service` makes it one systemd instance per store.
`--dry-run` prints exactly what would be posted.

*Why not the Collector's `filelog` receiver:* three things the store already
did and a tail cannot.

- **Entry boundaries.** A stack trace is one entry, therefore one LogRecord —
  no per-application multiline regex, the most reliably broken part of a
  filelog pipeline.
- **Both timestamps.** `timeUnixNano` gets the line's own stamp,
  `observedTimeUnixNano` the write time it arrived at. A tail has only the
  second until you configure a parser per format.
- **A buffer with a number on it** — the next section.

What it does *not* do: this is OTLP **logs**. No traces, no metrics, no context
propagation, and the body is the line — see [What this is not](#what-this-is-not).

## Survive a backend outage, and replay the window afterwards

**The store is the send buffer.** A collector's queue is sized by guessing, and
`filelog`'s durability is a cursor into a file whose lifetime belongs to
logrotate; if the backend is gone for an hour and rotation fires, the data is
gone with no way to know how much. Here retention *is* the disconnection
budget: `retain 30d` means the receiver can be gone for thirty days, and the
shipper backs off forever rather than giving up.

Because the data is still there afterwards, any window can be re-sent:

```sh
# the backend ate 14:00–15:00; send it again
timber-otlp --from '2026-08-11 14:00' --to '2026-08-11 15:00' \
    --endpoint http://collector:4318 backing/app.log

# evaluate or migrate to a candidate backend on real traffic
timber-otlp --from 2026-08-01 --endpoint http://candidate:4318 backing/app.log
```

Replay takes no cursor deliberately: it is on the logline axis, and a deliberate
act rather than a resumable one. Delivery is at-least-once, as OTLP itself is.

*Why not a collector's persistent queue:* it retains nothing once delivered, so
none of the three moves above exist there — an outage past the queue's size is
loss, and "send it again" has no source to send from.

## Put a full-fidelity tier under an expensive backend

The mirror of the adapter case, for applications that already speak OTLP. Point
them at a collector, fan out: everything to timberfs, the warm subset onward.

```sh
timberfs otlp-intake --into-dir /var/log/timberfs --auto-create &
```

```yaml
exporters:
  otlphttp/timberfs: { endpoint: http://127.0.0.1:4318 }
  otlphttp/vendor:   { endpoint: https://ingest.example.com }
service:
  pipelines:
    logs:
      exporters: [otlphttp/timberfs, otlphttp/vendor]
```

Each `ResourceLogs` lands in its own store (routed by `service.name`, or
`--route` something else), resource attributes seeded into its `.bark`, and the
HTTP 200 is sent only once the batch is fsynced into the `.sap` sidecar — so a
sender's retry logic and the store's chunk cadence stay independent.

Two things this inverts:

- **Cost.** Cheap retention normally means *not having* the data. Here the full
  stream stays local and greppable while the per-GB backend sees only what you
  chose to send it.
- **Trace correlation without a trace backend.** Record attributes and any
  `trace_id`/`span_id` trail into the stored line as `k=v`, so
  `timberfs query --has <trace_id>` pulls a trace's log lines out of the token
  index with nothing else running.

To send a *subset* onward from timberfs rather than fanning out at the
collector, filter into the shipper:

```sh
timberfs query --follow --records backing/app.log \
  | timber-filter --records --has ERROR \
  | timber-otlp --endpoint http://collector:4318
```

One caveat, and it is the reason to prefer the collector fan-out for a
permanent feed: a stdin stream is not a store, so it takes no `--cursor` and a
restart does not resume where it stopped. Filtered shipping is at its best for
a window you send on purpose.

## Take container logs without running a logging stack

`timberfs forward-intake` speaks Fluentd Forward v1 — the protocol Docker's
`fluentd` log driver, Fluent Bit, Fluentd and the fluent-logger clients already
use. One store per tag, minted on sight in the Docker-host mode, and a `chunk`
id is acked only once durable:

```sh
timberfs forward-intake --into-dir /var/log/timberfs --auto-create &

docker run --log-driver=fluentd --log-opt fluentd-address=127.0.0.1:24224 \
    --log-opt tag={{.Name}} --log-opt fluentd-async=true \
    --log-opt fluentd-request-ack=true --log-opt fluentd-sub-second-precision=true \
    myimage
```

*Why not the `json-file` driver:* it is uncompressed, unindexed, and its
rotation is a size cap that silently discards. This gives the same containers
compression, a time index and a declared retention, and — composed with the
section above — an OTLP feed they never had to know about.

## Move logs between hosts over a protocol you already trust

`timber-otlp` out and `timberfs otlp-intake` in are the same mapping in both
directions: a store shipped out and received back arrives byte for byte. That
makes an edge store and a central store a replication link over an ordinary
standardised protocol — and anything that speaks OTLP (a Collector, a vendor
tier, a queue behind one) can sit in the middle or replace either end.

```sh
# on the central host
timberfs otlp-intake --into-dir /var/log/timberfs

# on each edge host
timber-otlp --follow --cursor /var/lib/timberfs/edge.cursor \
    --endpoint http://central:4318 --compress gzip backing/app.log
```

Plaintext HTTP only: loopback or a private network, or terminate TLS in a
collector beside it.

Register the shipper as a **retaining follower** and the edge store stops being
a hoard. It gets run by name, with no flags of its own, and its position holds
the head back:

```sh
timberfs follower create central --store backing/app.log \
    --endpoint http://central:4318 --retaining --enable --start -- --compress gzip
timberfs set backing/app.log retain_size=20G retain_unconsumed=true
```

Now the two requirements that hold at once on an edge box are both satisfied.
Keep as little log data there as possible — a breach reaches less of it, and
"shipped off the edge promptly" becomes something you can show rather than
assert — yet never erase what has not landed elsewhere, including across a
network outage. No `retain` window satisfies both at any setting: it is a bet on
how long the link stays down, and the safe bet is the month of hoarding the
requirement exists to avoid. Only delivery can decide, and that is what a
position knows.

What remains on the box after a successful ship is **one chunk**, tunable with
`--chunk-size`/`--flush-age` and with the producer uninvolved — against one
rotation interval for the ship-then-delete pattern this replaces. And the edge
needs **push-only** credentials, which is what `rsync`-with-a-hold cannot offer:
deriving the hold by comparing against the destination means the edge needs read
access to the archive, i.e. a breached frontend holding a key to wherever the
data went.

`retain_size` is still required, and is now the disconnection budget outright:
size it as ingest-rate × the outage worth surviving. When it overrides the
follower, the writer says exactly what that cost:

```
app.log: retain_size (20.0 GiB) reached with follower central at chunk 4200
         — dropped chunks 4200..4830 it had not read
```

A shipper that fell outside the window also says so on resume (`GAP — N
chunk(s) were dropped before it read them`) rather than restarting silently from
whatever is now oldest — the same fact from the other side, inferred rather than
exact.

## Hand an investigation to someone else

A filtered slice, with its provenance, as one self-describing file — queryable
in place, no timberfs deployment on the other end beyond the binary:

```sh
timber-filter --records --has 'tenantId=FOO' backing/app.log --from 13:40 --to 14:10 \
  | timberfs import --records --into case/case.log
timberfs export case/case.log --into case.timber
timberfs info case.timber      # where it came from, and how it was selected
```

*Why not a tarball of greps:* the bundle records the selection that produced
it, so the receiver can tell what was excluded — and it stays a queryable store
rather than becoming a text file.

## What this is not

Named limits, so the compositions above are not read as more than they are.

- **Logs, not telemetry.** No traces and no metrics: `otlp-intake` answers
  `/v1/logs` and 404s the rest, and the shipper emits log records only. An
  application that emits no trace ids still has none.
- **The body is the line.** There is no per-record attribute extraction on the
  way out — no JSON or logfmt field mapping. Resource attributes are per store
  (a store *is* a service), and severity comes from a level word or
  `--severity-regex`. If you need fields parsed into attributes, that is what a
  collector's operators are for; put one downstream.
- **No query language.** Time windows and named predicates, composed with Unix
  pipes. No aggregation, no dashboards, no alerting.
- **One writer per store, and data arrives in log order.** `import` stitches
  historical files into order; live ingestion is in order by definition.
- **No TLS on any network path**, and no gRPC on :4317. Loopback or a private
  network; a collector terminates TLS if a sender needs it.
- **No cluster.** A fleet view is a read-time merge of files this machine can
  reach, which is the point — but it is not a distributed system, and it will
  not do what one does.

## See also

- [README](../README.md) — the feature tour and getting started
- [Deploying timberfs](deployment.md) — directory layout, systemd units,
  ownership and permissions
- [Design](design.md) — why FUSE, the on-disk format, the semantics table
