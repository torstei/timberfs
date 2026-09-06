# tally: metrics as a derived tape

**Status: the first slice is BUILT** — the line format, the fold, the rule
file and `timberfs tally`, which reads a `timberfs-records(5)` stream and
writes tally lines (`timberfs.1`, **tally**). Not built: the consumer/follower
half that fans out per store, the `samples` response kind, rollups. It rests on
the follower registry and its position per store
([follower-selection.md](follower-selection.md)), the consumer protocol
([consumer-protocol.md](consumer-protocol.md)), store selection (`select.rs`),
derived-store lineage (`.bark`), and head-drop retention.

The pipe is the whole of the first slice, deliberately — the store is written
by `append`, which already exists:

```sh
timberfs query --records app --from 13:00 \
  | timberfs tally --rules /etc/timberfs/tally.d \
  | timberfs append --into backing/app-tally.log
```

A **tally** is a metric derived from a log: requests per minute, bytes
transferred, errors logged, GC pause time. Some are generic to any log
(entries, bytes); most are specific to one producer's format. This note says
where the numbers live, what a line is, how a site declares its own, and how
they are read back.

## Why this is in timberfs and not beside it

Three things a metrics system needs, all of which already exist here and none
of which a separate tool could borrow.

**A position on a tape, so a metric can be added retroactively.** An extractor
is a consumer: it has a durable place in each store and a selection saying
which. Reset the position and it recomputes — so a rule written today can be
run over thirty days of tape. Nothing that samples a running process can do
that, and it is the strongest argument for deriving metrics from the log
rather than emitting them beside it. It also settles extraction-at-ingest,
which is the obvious alternative: computing inside the writer costs no second
read and no lag, but a rule added then can never see yesterday.

**Head-drop, which is the reason to materialise at all.** The numbers are
already computable from the log; what makes them worth writing down is that
the log's head goes and they should not. 300 GB of access log for a week,
50 MB of tally for two years — the retention asymmetry IS the feature, and it
makes a tally store the long-term memory of a log whose body has been dropped.

**A sample can cite the log.** A bucket knows the tape offsets of the entries
it counted; `.bark`'s `derived_from` says which store; [view.md](view.md)
already defines the address (`timber://host/id#offset=N`). So "what were those
requests" is a read on the source store rather than a different system. When
the source head has been dropped the citation does not fail vaguely: the
store's dropped counters say those bytes left.

## The invariant

> **Every stored value must coarsen by addition, or by min/max/newest. A value
> that cannot is not storable.**

It decides most of the format, and everything below is a consequence:

* **Deltas, never cumulative counters.** A head-drop removes a cumulative
  series' base and turns counter-reset detection into a guess. A delta is
  additive and survives any prefix of the tape being dropped.
* **`sum` and `count`, never `avg`.** Averages do not average.
* **Histogram buckets, never quantiles.** A stored `p95` is not summable. A
  histogram is — and it needs no new syntax: it is *n* series told apart by an
  `le` label, which is Prometheus's own encoding, and a quantile becomes
  read-time interpolation over additive data.
* **The field name IS the coarsening rule.** Exactly five are reserved —
  `sum`, `count`, `min`, `max`, `last` — and each says how two of them
  combine. A reader needs no schema registry and the tape carries no type
  declaration, because the field already said.

Second invariant, the same one the rest of this tree keeps: **zero and unknown
are different**. A bucket with no matching entries is a zero; a window the
extractor never ran over is a hole, and must be written down as one.

## The line

```
2026-09-06T13:37:00.000Z 60s http_requests status=500 vhost=my.visena.com sum=42 @1994848392+51221
2026-09-06T13:37:00.000Z 60s http_bytes vhost=my.visena.com count=1204 sum=8419221 @1994848392+51221
2026-09-06T13:37:00.000Z 60s http_latency le=0.5 vhost=my.visena.com count=1150 @1994848392+51221
2026-09-06T13:37:00.000Z 60s http_latency le=+Inf vhost=my.visena.com count=1204 @1994848392+51221
2026-09-06T13:38:00.000Z 60s !gap reason=follower-gap chunks=4200..4830
```

```text
line   := ts SP width SP metric (SP label)* (SP field)+ (SP cite)?
ts     := RFC3339 with milliseconds — the BUCKET's start, on the declared axis
width  := the bucket's width in the time syntax `retain` takes; `0s` is an
          instant (see OBSERVATIONS)
metric := [A-Za-z_][A-Za-z0-9_]*, or a MARKER: a leading `!`, which no metric
          name may carry, so a marker can never collide with one and a reader
          dispatches on the first byte
label  := key=value, logfmt quoting; a key may not be a reserved field name
field  := sum= | count= | min= | max= | last= , a number
cite   := @<offset>+<len> on the SOURCE store's tape
```

The leading stamp is not decoration: it is what makes a tally store parse with
no `timestamp_regex`, so every existing time window, `--follow`, `import` and
fleet view works on it the day it is written. The width is in the LINE and not
in the configuration, because a step that changes must be visible where a
reader meets it rather than in a file they do not have.

**Rendering is canonical**: labels sorted by key, numbers in their shortest
round-tripping form, one space between fields. Two lines describing the same
series are then the same bytes — which zstd rewards, which makes a revision
comparable by memcmp, and which keeps `--has` predictable.

The metric name is POSITIONAL and not spelled `metric=`, which is worth being
explicit about because the read side calls it a key. `--has http_requests` is
the grep; `series.select` matching `{key: "metric"}` is a selection over a
PARSED line. That is the same relation store selection already has — `name`
and `id` are selectable keys that appear nowhere in a store's bytes — and it
keeps the line's three positional fields (stamp, width, metric) uniform rather
than spelling one of them as a label.

The grain indexes the rest for free: a metric name and a label value are both
runs of tokens, so finding a series in a year of tape skips the chunks that
cannot hold it, with no index of ours to build.

**Markers.** `!gap` (a window this extractor could not see, with the reason and
the chunk range), and `!rule` (the effective definition of a metric changed —
see RULE DRIFT). Both carry a bucket stamp and width so they sort into the tape
where they belong.

### On reusing an existing format

None survives the constraints intact. OpenMetrics and InfluxDB line protocol
both put the timestamp last, where a leading stamp is what buys the whole of
timberfs's time machinery; OpenMetrics is a *scrape* format — instantaneous
cumulative values, the opposite of a delta per closed bucket; Graphite has no
labels; OTLP metrics is protobuf, against a `.trunk` whose contract is that
`zstd -dc` recovers something a person can read. What is worth taking is the
**vocabulary** — `le`, the `_total`/`_bytes` naming habit, logfmt labels — so
nothing is novel for its own sake, and rendering a tally store AS OpenMetrics
stays a formatter over a query rather than a second storage decision.

## Observations: the same line at width `0s`

An **observation** is one measurement of one entry: the same grammar, width
`0s`. Bucketing is then literally the fold — group by `(metric, labels,
bucket)`, combine each field by its own rule — and coarsening a 60s tape to
300s is the SAME fold. One function, three call sites: extract, roll up, read.

That is also the whole protocol for a site's own extractor (see below): a
program emits width-`0s` lines and timberfs owns bucketing, sealing, revisions
and gap records — the four things every site would otherwise reimplement and
three of which they would get subtly wrong.

`timberfs tally --observations` prints them instead of writing buckets, which
is both the debugging path and the way to learn the format, the way
`--dump-json` is for a query document.

## Buckets, sealing and late data

A bucket is keyed by `(metric, labels, start, width)` on a **declared axis**.
`AXIS` is required with no default, for the reason `window.axis` is:
logline-time and write-time buckets differ, and a default is an assumption the
next reader makes wrongly.

Entries arrive in the store's order, which is arrival order — so on the
logline axis a stamp can arrive after its bucket has passed (Apache logs a
request's start and writes at completion; a 90-minute websocket puts an ancient
stamp in a recent chunk). A bucket is therefore **sealed** after `GRACE` of
arrival-time has passed its end, and written. Anything later:

> **A late entry emits a REVISION of the sealed bucket, and the newest line for
> a key wins.**

Which an append-only tape does natively, and which pays for itself three more
times:

* **Recompute is idempotent by the same mechanism.** Re-running a rule over a
  window rewrites its buckets; no dedupe, no delete, no transaction.
* **It composes with head-drop.** A revision is always nearer the tail than
  what it revises, so dropping the head can never leave a stale line winning
  over the fresh one it replaced.
* **A backfill and a live extractor produce the same tape.**

⚠ The cost lands on the READER: a window must be resolved before it is
answered. That is bounded by series × buckets, not by entries — small — but it
means a **`--follow` of a tally store delivers unresolved lines and must say
so**, exactly as a live-edge entry carries no chunk number. A windowed read is
closed and therefore resolved; a tail is not.

## A tally store has two clocks too, and they are FAR apart

The bucket stamp is on the declared axis; the store's own chunks are stamped
when the line was appended. So `axis: "write"` asks *when was this computed*
and `axis: "logline"` asks *which minute is this about* — and a revision
written an hour late is findable on the first.

⚠ **That gap breaks the read path, and building it is what showed how.** Chunk
selection runs on the write clock and is widened by a guess of a minute before
each entry is verified against its own stamp — which assumes the two clocks are
close. A tally store's are close only by accident: its lines are numbers about
a minute that closed `GRACE` ago, a revision's are older still, and a
backfill's are about last month. Measured on the first tally store built here:
a logline-time query for the minute the numbers describe read **0 of 1 chunks**
and answered nothing, which is indistinguishable from an empty minute.

The fix is a declaration rather than a bigger guess: **`logline_lag` in the
`.bark`** says how far a line's own stamp may sit from the moment it was
written, and chunk selection is widened by that instead of by `WIDEN_MS`.
Declaring `logline_lag=8h` on that same store made the same query read the
chunk and answer exactly. It is general, not tally's: the roadmap's
zone-map entry describes the same failure for an arrival-stamped Apache store
whose lines carry request-START times, and today that leans on the same guess.
Widening both ways only ever costs I/O — the per-entry verification keeps the
output exact — and the per-chunk logline range the zone-map sidecar would add
makes the declaration unnecessary rather than wrong.

A tally writer knows its own lag exactly (`GRACE` + `REVISE`), so it should
declare it on the store it creates, once that half exists.

⚠ **A BACKFILL's lag is unbounded** — its lines are about whenever the source
data is from — so a store that has been backfilled must declare a lag wide
enough to cover it, and a wide lag means chunk selection prunes nothing and
every logline-time query becomes a full scan of the tape. That is the honest
cost of a declaration standing in for an index, and it is the argument for the
zone-map sidecar: a per-chunk logline range answers the same question exactly,
and would make the declaration unnecessary rather than merely generous.
`info` reports the declared lag, because its failure mode is silent.

⚠ **`AXIS=write` buckets at CHUNK granularity**, because the only arrival stamp
an entry carries is its chunk's write window. On a busy log a chunk is a second
or two and this is fine; on a quiet one a chunk can span minutes and every
entry in it lands in one bucket. Measured on the store above: 300 entries
imported as one chunk became one bucket of 300. Tier 0 answers volume on that
axis exactly and for nothing, which is the better way to ask it.

## Citations

On by default. `@<offset>+<len>` is the span from the first to the last entry
the bucket counted, on the source store's tape.

⚠ It is a **range to read, not a set of entries**: a rule matching one line in
a hundred cites a span containing ninety-nine it did not count, and a reader
re-applies the rule's predicate. Cheap (about 40 bytes a sample, and highly
compressible), honest, and enough for the motion it exists for — open the log
around the spike. A revision extends the span.

## Where the numbers go

**One tally store per source store**, named `<source name>-tally` by default,
its `.bark` carrying `class=tally`, `derived_from=<source id>`,
`derived_op=tally`, and the source's provenance inherited as derived stores
already inherit it. Per-source rather than one per extractor because lineage
travels, retention is then per-source, a fleet view over tally stores works
exactly as over logs, the citation needs no per-line store id, and cardinality
is bounded by the store count rather than by a label space.

⚠ **A new store JOINS existing selections, and that is a real hazard.** A tally
store inheriting `service=apache` is matched by `[service=~apache-.*]` — so a
follower shipping apache logs to a collector would silently begin shipping
tally lines too. Two halves to the answer: `class!=tally` already excludes them
for every store on disk today (an absent key reads as the empty string), and
**creating a tally store reports which existing follower declarations' selections
it has just joined**, by name. The registry can answer that precisely; a
warning at the moment of creation is worth more than a sentence in a manual.

## Configuring it: `tally.d`

Two halves with two owners, so two files. **Which stores** is the follower's
`select` — the existing vocabulary, unchanged. **What to extract** is a rule
file, shaped like `file.d/<set>.conf`: a preamble of defaults and a section per
rule, where — as `[exim-main]` names a store there — **the section name is the
metric name**.

```ini
# /etc/timberfs/tally.d/apache.conf
SELECT=[service=~apache-.*]
AXIS=logline
WIDTH=60s
GRACE=2m
DECLARE=index=true retain=730d retain_size=5G

[http_requests]
DECODE=apache-combined
LABELS=vhost status
COUNT=

[http_bytes]
DECODE=apache-combined
LABELS=vhost
SUM=bytes
COUNT=

[http_latency]
DECODE=apache-combined
LABELS=vhost
OBSERVE=ms
BUCKETS=0.05 0.1 0.5 1 5 10 120
```

```ini
# /etc/timberfs/tally.d/generic.conf — anything, anywhere
SELECT=[]
AXIS=write
WIDTH=60s

[entries_logged]
COUNT=

[errors_logged]
ANY=ERROR FATAL SEVERE
COUNT=
```

Section keys: the store selection (`SELECT`), the entry predicate (`HAS`,
`ANY`, `SUBSTRING`, `REGEX` and their `NOT_` forms — `timber-filter`'s
vocabulary, so one language for matching an entry), the field source
(`DECODE`, `EXTRACT`, `EXEC`), what becomes a label (`LABELS`), the measures
(`COUNT`, `SUM`, `MIN`, `MAX`, `LAST`, or `OBSERVE` + `BUCKETS` for a
histogram), and the axis/width/grace/citation knobs, each inherited from the
preamble.

Drop-ins split the way systemd's do: `/usr/lib/timberfs/tally.d/` for what a
package ships, `/etc/timberfs/tally.d/` for the site, a same-named file in
`/etc` replacing the packaged one **wholly** — never merged, for the reason
`file.d`'s `DECLARE` is not merged: a partial-merge rule is one nobody can
predict from reading the file.

⚠ **Unlike `file.d`, the directory is read by ONE process.** A set there is a
unit of supervision; here it cannot be, because two rule files may match one
source store and there is one writer per store. So: one `timberfs tally`
follower per host, reading the whole directory, and the files split by topic
for editing rather than for `systemctl`. A second tally follower whose rules'
store selections intersect the first's is refused.

The follower's selection is the SUBJECT and a rule's `SELECT` narrows within
it — the same relation `timbersh`'s session window has with a statement's. A
rule reaching outside is reported at startup, naming the stores it wants and
will never be fed, rather than quietly measuring nothing.

## Site-specific extractors, and how they are addressed

Four levels, and the first two cover most of what anyone writes.

1. **A predicate and nothing else.** `COUNT=` over entries a predicate
   selected: entries logged, bytes logged, errors logged, "how often does this
   exception appear". No parsing at all, and generic across every log — which
   is why `generic.conf` above is pointed at `[]`.
2. **A named decoder.** `DECODE=apache-combined|logfmt|json` turns a line of
   known shape into fields a rule names. Few, and only for formats somebody
   else standardised — a decoder per customer's log would be a taxonomy that
   grows a binary per format, which is the mistake the follower's `type` field
   made before it became a command.
3. **A regex with named captures.** `EXTRACT=^\S+ (?P<status>\d{3}) (?P<ms>\d+)`
   — the universal escape for a format nobody standardised, which is most
   in-house logs. Site-specific extraction is almost always this line.
4. **A program.** `EXEC=/usr/local/lib/timberfs/tally/gc-cycles` for what a
   regex over ONE entry cannot express: state across entries (a ZGC cycle is
   ~48 lines and its fields must be paired), correlation by request id, a
   lookup. It is fed the same `timberfs-records(5)` stream every consumer gets
   and answers with **width-`0s` observation lines** — so it owns extraction
   and nothing else, and cannot get sealing or revisions wrong because it never
   sees them. Any language; `awk` is enough.

**Addressed by absolute path, with no search path** — the decision `file.d`
made for `SOURCE` and for the same reason: with a search path, which program
you get depends on install order. `/usr/local/lib/timberfs/tally/` is a
convention for where to put them, not a lookup.

### Where state lives, and why extraction has none

Two different things are being called stateless, and only one of them is.

**The fold has state and it is timberfs's**: open buckets, the watermark that
seals them, the re-opening a revision needs, the citation span, the safe
offset. That is precisely what is NOT delegated — it is the part every
extractor would otherwise reimplement, and the part where being subtly wrong is
invisible in the output.

**Field extraction has none**, at levels 1–3, and that is load-bearing rather
than a gap. It is what makes a backfill and a live run produce byte-identical
tape — no warm-up, no first-entry special case — and it keeps the fold
associative, so sharding a tape across processes later is a fold over partial
results rather than a redesign.

⚠ **The case that looks like it needs state and does not.** A log line carrying
a CUMULATIVE total (`total_requests: 12345`) where the wanted number is the
delta looks like "remember the previous value". It is not: `LAST=field` stores
the value per bucket and the READER differences adjacent buckets. Same
information, no extractor state, determinism intact. The cost is that a
head-drop losing bucket *n-1* loses one delta at the head — one, rather than
the base, which is the whole reason deltas beat cumulative counters.

That is also the argument against a `DELTA=` measure, which is the obvious
thing to add: it would buy this one case and spend the invariant that a
recompute equals a live run, since the first entry of any window has no
predecessor and would have to emit nothing.

What genuinely needs state is correlation (a duration from a request id in two
entries), cross-entry record assembly (a ZGC cycle is ~48 separately-stamped
lines — a Java stack trace is NOT this, an entry already carries its
continuations), and distinct counting. Those are programs, and the
non-determinism is then visibly the program's rather than the format's.

### How often an EXEC runs

**Once, for the life of the run** — a coprocess, not a callback. Per entry
would be a fork per line: 47 a second on one measured access log at its quiet
rate, and far more on a busy one.

- **One per (rule, store)**, which is the granularity a rule instance already
  has, since a tally store belongs to one source store.
- **Fed the stream AFTER the rule's predicate**, which is both the optimisation
  and what keeps the program simple: a GC reader narrows to the ~60% of lines
  it parses before a line reaches it. A rule whose program must COUNT what the
  predicate excludes states no predicate — it is declared per rule, so the
  author decides.
- **It answers with width-`0s` observation lines and nothing else.**

⚠ **The citation is the receipt.** Nothing can see inside the coprocess, so
nothing knows which entries it has accounted for — except that an observation
cites the tape span it came from. A program holding a half-assembled cycle
emits, when it finally does, a citation covering the whole span, and
`safe_offset` therefore holds the watermark at the start of the open cycle. A
restart re-reads exactly what the program had not finished with.

⚠ **Which needs a way to say "seen this far, nothing came of it"**, or a
program that consumes ten thousand entries and emits nothing pins the watermark
at the first of them. That is a marker in the grammar that already exists —
`!at offset=…` — and it is the consumer protocol's `progress` one level down.
An EXEC is a mini-consumer.

**Failure needs no policy of its own.** If it exits, the rule's numbers stop,
that is recorded, and the run fails; the follower restarts it from the safe
offset. A restart-in-place policy would risk a silent gap or a double count
where the position machinery already gets it right.

The open cost is process count — (stores matched) × (EXEC rules) coprocesses on
a large fan-out. Lazy spawn on the first matching entry and an idle exit are the
answers, and both can wait for a fleet that has one.

**Named by what they produce, never by where they came from.** The metric name
is the section name, declared; the rule file's name does not appear in the tape.
Deriving a metric name from a file would mean renaming a file rewrites the
meaning of history — the same rule as identity never being derived from a path.
Two rule files defining one metric name for one store is a **collision, refused
at startup naming both files**, rather than a merge or a last-one-wins.

### Rule drift

⚠ Changing a rule silently changes what a metric MEANS, and the tape then holds
two definitions under one name with a step in the graph and nothing to explain
it. So a rule carries a hash of its normalised definition, and when that
changes the extractor writes a `!rule` marker before the next bucket. A reader
sees a discontinuity where it happened. This is the same doctrine as the width
living in the line: what a reader must know to interpret a number belongs
beside the number.

## Reading it back

```json
{ "v": "1.0-EXPERIMENTAL",
  "stores": { "select": [{"key":"class","op":"=","value":"tally"}] },
  "window": { "axis": "logline", "from": 1788700000000, "to": 1788703600000 },
  "series": { "select": [{"key":"metric","op":"=","value":"http_requests"},
                         {"key":"status","op":"=*","value":"5"}] },
  "step": 300000,
  "response_format": { "kind": "samples" } }
```

`series` reuses `Term` unchanged, so there is **one selection vocabulary at
three levels** — store manifest, store labels, series labels — and a generator
that can write one can write all three.

`kind: "samples"` is a records-stream kind, which is how it inherits the things
that took work: `stream-end` and therefore truncation detection, `position`
records and therefore paging, and the store attribution every answer carries.

Until it exists, a tally store answers `query` like any other store, because it
is one: the lines come back as text and the caller parses them. That is not a
placeholder for the sake of one — it is the property being claimed, that
metrics need no second system — and it is what made the first slice
demonstrable with no read-side code at all.

The line this draws, and it is the one worth defending:

> **The extractor aggregates. The reader selects and coarsens, and does
> nothing else.**

`step` must be an integer multiple of the stored width or the request is
refused — never interpolated. A window whose tape holds mixed widths (a `!rule`
or a configuration change in the middle) says so per series rather than
blending them. There is no read-time `rate()`, no read-time percentile beyond
interpolating a stored histogram, and no alerting. `use-cases.md`'s "no query
language" survives this; "no aggregation" does not, and must be rewritten when
it ships.

In `timbersh` it lands in the shape that is already there:
`select samples from [class=tally] where metric = 'http_requests' step 5m`.

## Two tiers around it

**Tier 0 needs no extractor at all.** `.rings` already holds per-chunk byte
counts and write windows, so bytes-arrived-per-minute over the whole life of
any store is answerable today with one binary search and no decompression —
retroactive, unconfigured, exact. Entries-per-minute needs the record-length
index already on the roadmap for other reasons, which is a second argument for
it. Whatever tier 0 can answer, a rule should not be written for.

**Tier 2 is a rollup**, and it is `rotate`-shaped: a tally store coarsened into
another tally store, `derived_from` the first, its width larger, its retention
longer. The fold is the same one bucketing and reading use. Deferred until
there is a tape big enough to want it.

## Retention, gaps and cardinality

The extractor follower defaults to **`retaining=false`**: a tally that falls
behind must not pin the log it measures. The consequence is designed rather
than accepted — retention drops chunks it had not read, the registry reports
the GAP as it already does, and the extractor writes a `!gap` marker with the
chunk range. `retaining=true` is available to someone who would rather the log
grow than the numbers have holes, with the usual footgun attached.

The tally store's own retention is independent and much longer, which is the
point of the whole exercise.

**Cardinality** is where every metrics system dies, and an append-only text
tape dies more slowly than a TSDB index but still dies. So: **label keys are
declared per rule and never discovered from the data**, and the extractor caps
distinct series per bucket, writing a marker when it hits the cap. Bounded
loss, recorded exactly — the same rule retention already follows.

## Deferred, and open

* **Rollups** (tier 2) — the mechanism is the fold; only the verb is missing.
* **A cost preflight for a tally read**, as the roadmap wants for query: the
  chunk count is knowable before the read here too.
* **Exporters** — OpenMetrics and Influx renderings of a `samples` answer, and
  the Grafana datasource as one consumer of the API rather than the reason for
  its shape.
* **Whether `EXEC` may be fed CHUNKS** rather than entries, for an extractor
  cheap enough that framing dominates.
* **Trace-shaped questions** (a request id correlating two entries into a
  duration) are expressible at level 4 today and awkwardly; whether they
  deserve a level of their own is unanswered until somebody writes three.
* **What it costs**, barely measured: one 29 KB access log of 300 requests
  produced 1.9 KB of tally lines compressing to 346 B (5.5x, against the
  source's 9.1x — canonical lines repeat, but there are few of them). A real
  store and a real day are what would settle it, along with the price of the
  second read.
* **The follower half**: fanning out per source store, the watermark rule
  (`Roller::safe_offset` is already the answer — the oldest byte any held
  bucket still depends on, so a restart re-derives identical lines), creating
  the tally store with its labels, lineage and `logline_lag`, and writing the
  `!gap` marker from the registry's GAP.
* **`EXEC` is parsed and refused at run time, not yet spawned.** The
  observation format it answers in is settled and `--observations` prints it;
  what is missing is the coprocess plumbing, and the `!at` marker that lets one
  release the watermark over entries it consumed and made nothing of.

## What must change elsewhere when this ships

Done with the first slice: `timberfs.1` gains **tally** and `logline_lag`, the
completions gain the verb, and `packaging/tally.conf.example` is the rule file
to copy.

Still owed, when the rest lands: `use-cases.md`'s "No aggregation, no
dashboards, no alerting" (two of three survive); `concepts.md` gains **tally**,
**observation**, **bucket**, **revision**, **logline lag**; `design.md` gains
the line format and the `logline_lag` manifest key; a `timberfs-tally(5)` and a
`tally.d` section in `deployment.md`; and `timberfs-query-document(5)` gains
`series`, `step` and the `samples` kind.
