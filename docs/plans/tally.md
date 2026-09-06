# tally: metrics as a derived tape

**Status: the EXTRACTOR half is built.** The line format, the fold, the JSON
extractor document with its published schema
([docs/tally-extractor.schema.json](../tally-extractor.schema.json)),
`timberfs tally` reading a `timberfs-records(5)` stream, `--try` against a
plain file, `--fold`, and two shipped extractors tested by their own `--try`
output (`timberfs.1`, **tally**). ⚠ **The PROVISIONING half is not built** —
which stores get a tally store, named how, declaring what, with which
extractors applied. Its shape is settled below and its file is the whole
interface: the follower it registers is derived, so no command is typed. Until
it is built, a pipeline names its own source and the operator creates the tally
store. Not built either: the consumer/follower half
that fans out per store, the `samples` response kind, rollups. It rests on
the follower registry and its position per store
([follower-selection.md](follower-selection.md)), the consumer protocol
([consumer-protocol.md](consumer-protocol.md)), store selection (`select.rs`),
derived-store lineage (`.bark`), and head-drop retention.

The pipe is the whole of the first slice, deliberately — the store is written
by `append`, which already exists:

```sh
timberfs query --records app --from 13:00 \
  | timberfs tally --extractor /usr/lib/timberfs/tally.extractors.d \
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
2026-09-06T13:37:00.000Z 60s http_requests status=500 vhost=example.com sum=42 @1994848392+51221
2026-09-06T13:37:00.000Z 60s http_bytes vhost=example.com count=1204 sum=8419221 @1994848392+51221
2026-09-06T13:37:00.000Z 60s http_latency le=0.5 vhost=example.com count=1150 @1994848392+51221
2026-09-06T13:37:00.000Z 60s http_latency le=+Inf vhost=example.com count=1204 @1994848392+51221
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
stamp in a recent chunk). A bucket is therefore **sealed** once the watermark
has passed its end by `grace`, written, and **evicted**.

> **`grace` is the only mechanism for lateness. An entry arriving after its
> bucket sealed is counted in a `!late` marker and measured nowhere.**

Bounded loss, recorded exactly — the rule retention already follows — and the
marker is how an operator learns their `grace` is too small.

⚠ **A REVISE window was designed and dropped, and the reasoning is worth
keeping** because the shape recurs. It let a late entry re-open a sealed bucket
and emit a REVISION of it, newest-wins. That is a SECOND mechanism for the
problem `grace` already solves, and the second one costs everything expensive:

* A revision must restate the COMPLETE bucket. A process that advanced its
  position past a sealed bucket and then RESTARTED has forgotten that bucket's
  contents, so a late entry re-opens it from zero and emits a revision holding
  only the late entries. **Silently wrong numbers**, which is the worst failure
  this design can have.
* Avoiding that means the position may not advance until a bucket is EVICTED,
  so a follower lags `grace + revise` behind the live edge — an hour, at the
  defaults that were shipped. That is retention pressure on the SOURCE store,
  and `GAP`s wherever retention is tight.

A graph does not have to be exact. It does have to not lie about being exact,
and a `!late` counter is that, at none of the cost.

⚠ **Newest-wins survives, as a READ rule, for a different reason.** Re-running
an extractor over a window emits the same buckets again, and

> **the newest line for `(metric, labels, start, width)` wins**

is what makes a recompute idempotent rather than doubling. It is a property of
reading a tape, not of writing one: nothing emits a revision, but a reader must
still resolve a window before answering it. Bounded by series × buckets, not by
entries. A **`--follow` of a tally store delivers unresolved lines and must say
so**, exactly as a live-edge entry carries no chunk number.

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

A tally writer knows its own lag exactly (`grace`, plus the width of the bucket
being closed), so it should declare it on the store it creates.

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

**A tally store belongs to ONE source store** — many-to-one, so a site's own
consumer may write a second one beside the provisioned one — named by that
provisioning's `OUTPUT` template, `{name}-tally` by default, its `.bark`
carrying `class=tally`, `derived_from=<source id>`, `derived_op=tally`,
whatever the provisioning's `DECLARE` states, and the source's provenance
inherited as derived stores already inherit it. Per-source rather than one per provisioning because lineage
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

## The extractor and the provisioning: two objects, two formats

The first slice put four different things in one INI file: which STORES, which
LINES, how to READ one, and what to MEASURE. Only the first is deployment; the
other three are a definition, and fusing them makes the definition
**unshippable** — nobody can publish "how to measure an apache access log"
if the file also asserts which stores it applies to on somebody else's host.

⚠ **And the subject of a definition is a LINE SHAPE, not a store.** A store
carries many shapes at once — an app log has logfmt request lines and stack
traces and startup banners, a GC log has ~48 shapes inside one cycle, anything
syslog-ish aggregates many programs. Any design that says "this store is
logfmt" is wrong, which is why the shape is never declared on the store and
never in the `.bark` beside `timestamp_regex`, tempting as that looked.

| what | where it lives | why |
| --- | --- | --- |
| which **stores** | the provisioning | deployment; changes per host and per week |
| which **lines** | the extractor's `claim` predicate | part of the definition — a definition must claim its lines |
| how to **read** a claimed line | `decode` / `extract` | the shape |
| what to **measure** | `count`/`sum`/`histogram`… | the policy |

### The extractor: a named JSON document

```json
{
  "v": "1.0-EXPERIMENTAL",
  "name": "apache-access",
  "window": { "axis": "logline", "width_ms": 60000, "grace_ms": 120000 },
  "metrics": [
    { "name": "http_requests",
      "description": "requests by status and method",
      "claim":  { "all": [{ "regex": "\" \\d{3} " }] },
      "fields": { "decode": "apache-combined" },
      "labels": ["status", "method"],
      "measure": [{ "count": true }] },
    { "name": "http_latency",
      "claim":  { "all": [{ "regex": "\" \\d{3} " }] },
      "fields": { "extract": "\" (?P<status>\\d{3}) (?P<ms>\\d+) " },
      "labels": ["status"],
      "histogram": { "field": "ms", "unit": "ms",
                     "buckets": [10, 50, 100, 500, 2000] } }
  ]
}
```

**JSON and not INI, and the argument is the missing `SELECT`.** Once a file
says nothing about this host it stops being configuration and becomes an
ARTEFACT — shipped, shared, versioned, schema-checked — and this tree already
has a format for that: the query document, with its published schema,
`deny_unknown_fields`, and a `--dump-json` that renders flags into it so the
two cannot drift. `file.d` is INI because it genuinely is deployment; an
extractor is not.

Three things INI was actively costing, all the same cost:

* `LABELS=level tenant`, `BUCKETS=10 50 100`, `ANY=error FATAL` are LISTS
  smuggled through scalars and re-split at the far end — the thing that made a
  timbersh target's `cmd` a list rather than a command line.
* `EXTRACT=` holds a regex raw in a format with no quoting rule, and values are
  trimmed, so a pattern ending in `\s` is silently mangled.
* No nesting, so a histogram's `OBSERVE`+`BUCKETS` are two keys that must be
  kept in step, and the SESSION (`GROUP`/`CLOSE`/`TIMEOUT` plus its own
  measures) strains it further.

And a fourth that only structure can fix: **a unit has nowhere to live in INI**.
`sum=8419221` says nothing about whether it is bytes or milliseconds, and the
first INI draft of the shipped apache example carried second-scale buckets over
a millisecond field — a perfectly well-formed histogram that means nothing.

**`claim` is the query document's `Predicate`, verbatim** — same `$defs`, same
`has`/`substring`/`regex`/`caseless`. One vocabulary for matching an entry
across both documents, which is worth more than the format change on its own.

**The name is declared INSIDE**, never taken from the filename: renaming a file
must not change what a document IS, the same rule a metric name and a store
identity already follow. Two documents claiming one name is refused, naming
both files.

⚠ **What JSON costs, honestly**: regexes double-escape (`\\S`, `\\d{3}`),
there are no comments, and it is a second idiom in `/etc/timberfs`. The first
two have one answer, and it is the one this tree already committed to once —
a GENERATOR. `timberfs tally --dump-json` from flags, as `query --dump-json`
renders a search: you do not write the document, you generate one and edit it,
and it escapes correctly by construction. `description` per metric replaces
comments with something better, because a description can be SHOWN — in a
listing, in an error — where a comment can only be read in the file.

### The provisioning: which stores get one, and what it looks like

```ini
# /etc/timberfs/tally.d/apache.conf
STORE_DIR=/var/log/timberfs

[apache]
SELECT=[service=~apache-.*]
OUTPUT={name}-tally
APPLY=timberfs-apache-combined timberfs-volume
DECLARE=index=true retain=730d retain_size=5G
#WIDTH=30s          # overrides the extractors' own default
```

```sh
timberfs tally --check apache          # declare, converge, say what resolved
systemctl enable --now timberfs-tally@apache
```

INI here for the reason it is wrong for definitions: this file is ABOUT THIS
HOST, hand-edited, and it is `file.d`'s shape — a preamble, a section per
subject, `DECLARE` for the bark, a `--check` that declares and reports, a
templated unit named after the set.

It is a store-PROVISIONING rule, not merely a binding, and that is what it buys:
`forward-intake --auto-create` and `otlp-intake` mint source stores on first
sight, so without this every new service arrives with no tally store until
somebody runs `timberfs create` by hand. Same argument that killed
store-declared shapes.

**`{name}`** is the template spelling this tree already proposes for
attribution labels (`--label '{host}'`), so `{name}`, `{host}`, `{service}`
and `{id}` are the fields.

### The file is the interface; the FOLLOWER is state

⚠ **The operator types no command, and this is the point.** A follower's
`command` exists because "a destination is a program and timberfs does not need
to know which" — but here the destination is a TIMBERFS STORE, and timberfs
knows exactly how to write one. Feeding itself through a pipe protocol designed
for foreign programs is ceremony.

So `timberfs tally --check apache` REGISTERS the follower — named `tally-apache`,
its selection and its command both DERIVED from the file — and the operator
never writes either. The registration still exists, because that is where the
position and the retention floor live and a program that writes those can get
retention wrong silently (see [consumer-protocol.md](consumer-protocol.md)).
`follower status tally-apache` still shows a real command that would work if it
were typed, which keeps the registry honest.

That also answers an objection to having a file at all: two objects holding one
selection, kept in step by hand. They are not — one is derived from the other,
so drift is impossible by construction rather than merely detectable. The same
argument said the other way round is the one that decides it: **with the file,
the command is not needed; without the file, the command is all there is.**

Five rules fall out:

* ⚠ **A tally follower never reads a store whose `class` is `tally`, and that
  is IMPLICIT rather than something an operator remembers.** A tally store
  inherits its source's provenance, so `[service=~apache-.*]` matches
  `apache-access-tally`, whose every line `timberfs-volume` then claims,
  producing `apache-access-tally-tally`, and then that one. Not hypothetical:
  it follows from "inherit provenance" plus "selections re-resolve". Nothing
  legitimate is lost — measuring a tally store is a ROLLUP, which is tier 2 and
  a different verb.

* ⚠ **The collision check is on `OUTPUT`, not on `SELECT`.** Two provisionings
  may cover one source store as long as they produce DIFFERENT tally stores;
  two producing the same one is two writers and is refused. That is more
  permissive than "one tally follower per store" and exactly as safe — and it
  is checkable at load, where overlapping input selections are not decidable in
  general.
* ⚠ **`logline_lag` is DERIVED, not typed.** It is the `width + grace` of the
  applied extractors, and the provisioning knows both. Making an operator write
  it invites precisely the failure this note records twice. `DECLARE` may
  override it, which is what a backfill needs.
* **Provisioning CONVERGES and does not cascade.** A source store appearing
  gets its tally store on the next tick (`--check` to declare and say what
  resolved, as `file-intake` does). A source store being DELETED does not take
  its tally store — that is the retention asymmetry the whole thing exists for.
  A `DECLARE` that has drifted from what is on disk is reported, never silently
  rewritten over an operator's `timberfs set`.
* **`APPLY` composes**, so "applies to everything" stops being a predicate: the
  generic set (`timberfs-volume`) is just another named document listed beside
  the specific one, rather than an extractor carrying a `[]` selection that
  would collide with every other.

Extractors live in **`tally.extractors.d`** — `/usr/lib/timberfs/` for what a
package ships, `/etc/timberfs/` for the site, a same-named FILE in `/etc`
shadowing the packaged one wholly (shadowing by filename, refusal by declared
name: forking a shipped document means keeping its filename, while two
unrelated documents claiming one name is an ambiguity nobody should resolve by
readdir order).

⚠ **Every name this package ships begins with `timberfs-`, and nothing else
does.** Names live in one flat namespace and a collision is refused, so without
a reserved prefix a site writing its own `apache-combined` finds ours in the
way — and a name added in a later release could break a deployment whose own
document already used it. A one-sided promise, enforced by a test rather than
remembered.

Provisioning is site-only, in `/etc/timberfs/tally.d/`, which keeps the plain
`.d` name for the deployment file exactly as `file.d` has it. "Extractor" and not "rule" for the document,
because RULE already means one metric inside one, and one word meaning two
things is how a format becomes hard to talk about.

### What this leaves open

* **The window override is recorded only halfway.** A width is in every tally
  line, so a reader sees which applied; `grace` is not, and a bucket sealed
  under a different one is not distinguishable from a bucket that was not.
* **`timberfs tally --dump-json`** does not exist, and the JSON form is much
  worse without it than the INI form was.
* **`type=` vs `class=`.** Settled as `class=tally` — but the tree is already
  inconsistent, `concepts.md` documenting `--select 'type=console,host=web01'`
  where the README uses `[class=audit]` and `[class=container]`. Worth making
  one of them the example everywhere.

## How a metric reads a line: four levels

Four levels, and the first two cover most of what anyone writes. Level 4 is NOT
BUILT and is the one to build next; there is no level 5, and the section after
it says why.

1. **A claim and nothing else.** `{"count": true}` over the entries the
   `claim` predicate selected: entries logged, "how often does this exception
   appear". No parsing at all.

   ⚠ **Only a bare count is genuinely format-free**, which is why
   `timberfs-volume` contains exactly one metric. A generic *errors_logged*
   claiming `ERROR`/`FATAL` was shipped and withdrawn: whether a line is an
   error is SEMANTICS, and semantics come from the format. The token can sit in
   a URL, in a message about error handling, or in a stack trace's text, while a
   log writing `severity=3` or a syslog priority has real errors such a claim
   never sees. It belongs in an extractor that knows the shape — and for an
   access log it is not even a metric, since `http_requests` labels by status
   and 5xx is a read-time selection over the same series.

   If a format-free error metric is ever wanted, the honest basis is the
   severity heuristic timberfs ALREADY applies on the OTLP path
   (`otlp::Severity::of()`, which is also what Grafana's `detected_level`
   wanted), exposed as a derived pseudo-field — one answer to "what level is
   this line", not two.
2. **A named decoder.** `"fields": {"decode": "logfmt|json|apache-combined"}`
   turns a claimed line into fields the metric names. The list is CLOSED, and
   the criterion for being in it is narrow — see below.
3. **A regex with named captures.** `"fields": {"extract": "…(?P<ms>\\d+)…"}` —
   the universal escape for a format nobody standardised, which is most
   in-house logs. Site-specific extraction is almost always this one line.
4. **A session** — a window keyed by a FIELD rather than by time, for what a
   regex over one entry cannot express because the answer is spread across
   several: a ZGC cycle is ~48 separately-stamped lines, a request duration is
   a `BEGIN` and a `COMPLETE` sharing an id. Not built; see below.

Beyond that is not a level of this language at all — see **an extractor that
needs a program is a follower**, below.

### The claim is what makes a metric safe on a mixed store

⚠ **A store carries many line shapes, so a metric must say which are its own.**
That is what `claim` is for, and without it the machinery cannot tell two
different facts apart:

* the line is **not this metric's** — an ordinary event on a mixed store, and
  not a loss;
* the line **is** this metric's and could not be read — a real defect, and the
  number is now wrong rather than merely absent.

**Fixed with the redesign**: a shape mismatch — a decoder that cannot parse the
line, an `extract` that does not match — is a SKIP, and only *claimed, then
unreadable* is a drop. Before that, a logfmt metric over a store that is ten
percent stack traces emitted a drop counter proportional to the stack traces
and meaning nothing was wrong. `--try` reports both counts separately, and the
skipped one is the half worth reading on a mixed store.

A metric with a decoder and NO claim cannot make the distinction at all —
worth saying in the documentation rather than refusing, since a single-shape
store is common enough to be legitimate.

### What may be a decoder, and why the list is closed

The rule that keeps `decode` from becoming a taxonomy of everyone's log
formats — one variant per customer, which is the mistake the follower's `type`
field made before it became a command:

> **A decoder exists only where the KEY SET IS OPEN, and a regex therefore
> cannot express the shape.**

`logfmt` and `json` qualify: there is no capture group for a key you do not
know in advance. Everything POSITIONAL — a fixed grammar with fixed slots — is
what a regex does well and should be an `extract`.

⚠ Which makes **`apache-combined` the anomaly in its own list**, and its
justification is a different one: not that it cannot be regexed (it can), but
that it is a PUBLISHED grammar many sites share and one that is easy to get
subtly wrong by hand. The evidence is this tree's own: the first
the first hand-written apache regex shipped here matched nothing,
over a `$` that does not mean what it looks like.

**The list stops growing by PARAMETERISATION, not by plugins.** If a fourth
request arrives the answer is not a fourth variant, it is

```ini
DECODE=positional
FIELDS=host ident user time request status bytes referer agent
```

which subsumes `apache-combined`, covers CSV with quoted separators (which a
regex genuinely cannot do robustly), and turns the enum into SHAPES rather than
FORMATS. Worth building when something asks, and not before.

⚠ **The gap that is real today: `json` reads only the top level.** A nested
object is skipped outright, so `{"http":{"status":500}}` offers no `status` and
there is no path syntax — a hole in the one shape whose whole point is that the
key set is open.

### The session: the fold applied twice

**Not built. The next thing to build.**

The observation that makes this small: **a session is the bucket with a
different key and a different close condition.** A bucket is a window keyed by
`(metric, labels)` and closed by a time watermark; a session is a window keyed
by a field VALUE and closed by a pattern or a timeout. Same structure, same
combine rules, so `Roller` is most of it already. A session emits ONE
observation, which then goes into ordinary bucketing — two folds stacked, one
implementation.

```ini
[gc_pause]
EXTRACT=GC\((?P<cycle>\d+)\)|Pause \w+ \w+ (?P<pause>[0-9.]+)ms|->(?P<after>\d+)M\((?P<pct>\d+)%\) (?P<secs>[0-9.]+)s
GROUP=cycle              # the window's key, instead of the bucket's start
CLOSE=Major Collection   # the line that ends one
TIMEOUT=5m               # an unclosed one is dropped and counted
SUM=pause                # the measures apply WITHIN the session
LAST=secs
```

Three things make it fit the format rather than strain it:

- **No nesting.** One `EXTRACT` with ALTERNATION, applied per line, each line
  filling whichever named groups it carries and leaving the rest unmatched.
  That is how 48 lines of different shapes become one record without a
  sub-record syntax a `KEY=VALUE` file cannot express.
- **A duration needs one new measure, not a language.** `SPAN=field` is
  max−min within the session, and the entry's own stamp as a pseudo-field
  (`ts`) turns a `BEGIN`/`COMPLETE` pair keyed by request id into a duration.
  Nothing else is required for the correlation case.
- **The watermark problem disappears.** timberfs can see which entries are in
  an open session, so `safe_offset` is computed directly — none of the
  receipt-by-citation, `!at` marker, coprocess lifetime or failure policy
  below is needed. That is the bulk of level 5's machinery DELETED rather than
  moved, and it is the argument for building this first.

An incomplete session is **dropped and counted**, which is not a new rule:
`timberfs-records(5)` already says a bounded read drops the entry it was
assembling rather than flushing half a stack trace.

⚠ **Cardinality, one level down.** Open sessions are one per distinct key and a
request id is unbounded, so `MAX_SESSIONS` and `TIMEOUT` bound them with the
drops recorded — the rule `MAX_SERIES` already follows for labels.

⚠ **Backfill stops being edge-exact.** A recompute of 13:00–14:00 misses a
session that opened at 12:59, where buckets align and sessions do not. The safe
offset covers the live case; a windowed recompute is edge-lossy and has to say
so rather than look complete.

⚠ **A label's value inside a session needs a rule**, since only numbers have
combine rules. First non-empty wins is the obvious one — the key is shared by
construction — but it is a decision, not a default that falls out.

⚠ **One real extractor is thin evidence.** The ZGC cycle is the case this was
designed against and it is well understood, but a second SHAPE would test it
harder before it is built.

### What is left for a program, after the session

An external lookup; arithmetic beyond max−min (a ratio of two fields); a real
parser (a binary format, a deep JSON path); a sketch (distinct counting needs a
mergeable one — a plain distinct count is not additive and so is not storable);
and grouping by something that is not a captured field. Each of those is a
FOLLOWER of its own rather than a level of this rule language, which is what
the section after next is about. The session matters because it moves that
wall a long way out: writing a program should be where expressiveness ends,
not where cross-entry state begins.

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

### An extractor that needs a program is a follower

An `EXEC` would exist to let a foreign program take part in timberfs's tally
run. But **the tally run is itself just a consumer**, so the generality is
already there one level up: a program that needs state across entries registers
as its own follower, reads the same records stream every consumer gets, and
writes a tally store of its own.

Every axis favours that over a coprocess, and the comparison is the argument:

| | an `EXEC` coprocess | its own follower |
| --- | --- | --- |
| lifecycle | supervision hand-rolled inside tally | `timberfs-follower@name`, systemd's |
| watermark | an `!at` marker invented for it | the consumer protocol's `progress`, which already means exactly that |
| failure | takes the whole tally run with it | isolated to one follower |
| processes | (stores × EXEC rules) | one per destination — the follower model's point |
| visible in | a conf file | `follower list` |

The `!at` marker is the tell. It was being invented to say *"I have consumed
this far and made nothing of it"* — which is `progress`, in a second and worse
protocol, one level down.

⚠ **The objection that dissolves.** "One tally store per source store" would
make a site's own extractor and timberfs's rules two writers for one store. But
that rule's purpose is lineage, per-source retention, a citation needing no
per-line store id, and cardinality bounded by store count — and MANY-TO-ONE
keeps all four. Several tally stores deriving from one log is a fleet view,
which is a read-time merge by selection and already works. So the rule is: **a
tally store belongs to ONE source store; a source store may have several.**

What is genuinely lost is a SHARED READ of the tape: three site extractors as
three followers read the log three times. Real, and the accepted cost of the
follower model everywhere else in this tree.

**`timberfs tally --fold` is what makes this a real answer** rather than an
invitation to reimplement the hard parts. A program emits width-`0s`
observations and pipes them through the fold that ships, so sealing, revisions,
the citation span and the series cap are not its problem:

```sh
my-gc-extractor | timberfs tally --fold --width 60s | timberfs append --into …
```

⚠ A line the fold cannot read is FATAL, and so is one that is already a bucket.
A fold that quietly dropped a tenth of its input would report numbers that are
WRONG rather than missing, and nothing downstream could tell; accepting a
bucket line as an observation would double every count on a re-run.

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
* **Trace-shaped questions** (a request id correlating two entries into a
  duration) are what the session's `SPAN=ts` is meant to cover; whether that is
  enough is unanswered until somebody writes three.
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
* **The provisioning file** — `SELECT`, `OUTPUT`, `APPLY`, `DECLARE`, the
  `OUTPUT` collision check, the derived `logline_lag`, converge-not-cascade,
  the implicit `class!=tally`, and the derived follower registration. It needs
  the follower half to have anything to run it, so the two land together.
* **`revise_ms` must come OUT of the built code** — the document's `window`,
  `Roller`, `--fold --revise` and the man page all still carry it, and
  `!late` must replace it. Sealing then evicts, and a position lags by `grace`
  alone.
* **`timberfs tally --dump-json`** — a generator, which the JSON form wants
  much more than the INI form did: nobody should be escaping a regex by hand.
* **The session (level 4)** — the next thing to build: `GROUP`, `CLOSE`,
  `TIMEOUT`, `SPAN`, `MAX_SESSIONS`, and `Roller` keyed by a field value
  instead of a bucket start.
* **`EXEC` is gone**, and `--fold` is what replaced it: the boundary is a pipe
  and a text format rather than a coprocess protocol. The key stays reserved so
  that reaching for it gets an answer.
* **Re-bucketing an existing tape** is the same fold and needs its own flag —
  `--fold` deliberately refuses a line that is already a bucket rather than
  guessing which was meant.

## What must change elsewhere when this ships

Done with the first slice: `timberfs.1` gains **tally** and `logline_lag`, the
completions gain the verb, and `packaging/extractors/` holds the shipped
documents with their fixtures under `tests/extractors/`.

Done with the extractor half: `timberfs.1`'s **tally** section, the README, the
deployment guide, the published schema, and the shipped extractors with their
fixtures. Still owed: a `timberfs-tally-extractor(5)` for the document; `use-cases.md`'s "No aggregation, no
dashboards, no alerting" (two of three survive); `concepts.md` gains **tally**,
**observation**, **bucket**, **revision**, **logline lag**; `design.md` gains
the line format and the `logline_lag` manifest key; a `timberfs-tally(5)` and a
`tally.d` section in `deployment.md`; and `timberfs-query-document(5)` gains
`series`, `step` and the `samples` kind.
