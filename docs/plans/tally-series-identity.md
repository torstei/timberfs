# A metric is a series, and combining it is the READER's decision

**Status: not built.** A defect in the shipped tally — the decision "are these
two things one series" is currently made by the STORAGE layer, at fold time,
irreversibly — and the remedy, which is small: the definitions travel with the
tally store, and their names are the namespace. Amends [tally.md](tally.md),
whose `Run::new` guard is right for a reason it does not give.

⚠ The ordinary change to a definition — a regex fixed, a metric added, a
metric removed — is an in-place UPDATE that does not break a series, and this
note is arranged so that case stays cheap. A changed MEANING is the exception;
everything about generations, supersession and closed state serves only that,
and can wait.

⚠ And one assumption underneath all of it is itself in question: that a tally
store is a tape like a log's. A log entry is a fact and a tally bucket is a
conclusion, and since 0.33.0 a tally tape already supersedes its own lines,
which no log tape does. See *A tally is not a log*.

## The defect, demonstrated

Two observations from two different definitions that happen to share a metric
name and a label set:

```
$ printf '2026-09-06T13:37:10.000Z 0s service_calls status=ok count=1
2026-09-06T13:37:20.000Z 0s service_calls status=ok count=1
' | timberfs tally --fold --width 60s --grace 0s
2026-09-06T13:37:00.000Z 60s service_calls status=ok count=2
```

**`count=2`.** Two unrelated measurements became one number, and no later
reader can separate them, because nothing was written down that would let it.
`Roller`'s bucket key is the whole of the reason:

```rust
type Key = (u64, String, Vec<(String, String)>);   // start, metric, labels
```

Two lines agreeing on those three ARE the same bucket — right for one series
said twice, wrong for two series that share a name.

**A metric is a series of measurements. Whether two series are drawn together,
summed or stacked is a property of the QUESTION**, and a name is a poor place
to keep an answer that depends on who is asking. That is the whole of the
argument; everything below is where the current design puts the answer instead.

## The guard is right; its reasoning is not

`Run::new` refuses two documents defining one metric name. The RULE is
correct — see the next section — but its comment and its error message assert
opposite things about the same two shipped documents:

> ⚠ Two documents may share a name — apache's and nginx's `http_requests`
> **are the same measurement** — and they are only wrong TOGETHER

> both define the metric `"http_requests"` — applied together their samples
> fold into one series, and the numbers become **a sum of two different
> measurements**

The fleet graph relies on the first; the guard enforces the second. And
because the guard is a patch over the fold rather than a stated property, it
leaks:

* **`tally --check` accepts what the run refuses** — it compiles each document
  alone and never sees the applied set, though it is the documented check.
* **`tally --provision` accepts it too**: converges, registers the follower,
  exit 0.
* **`Run::new` runs lazily, per store, on the FIRST ENTRY**, so a bad `APPLY`
  survives provisioning and kills the follower under systemd when data
  arrives. Measured.
* **The operator has no way out.** `OUTPUT` templates over the SOURCE store's
  facts (`{name}`, `{host}`, `{service}`, `{id}`) with no `{extractor}`, so
  one source store's tally cannot be fanned into a store per document — the
  natural resolution is inexpressible and renaming metrics is the only escape.

## A metric name is unique within a TALLY STORE

That is the scope, and it is already almost entirely enforced:

| scope | today |
|---|---|
| within one document | **enforced** — `metrics[1] repeats the name "dup"` |
| within an applied set, i.e. one tally store | **enforced** — the guard above |
| across tally stores | the reader's decision, which is where it belongs |

⚠ And **`:` is already legal in a metric name** — "letters, digits, `_` and
`:`" — so namespacing needs no format change. `apache:http_requests` parses
today; only the `-` in document names stops one being used verbatim.

## The definitions travel with the tally, and their names are the namespace

At creation a provisioning has resolved a list of definitions, each with a
declared name, under a resolution order that is already defined. **Copy them
into the tally store, keyed by that short name, and prefix each metric with
it.**

Then:

* **Uniqueness is mechanical.** Two documents cannot collide, because their
  metrics are `apache:http_requests` and `nginx:http_requests`. The guard
  keeps its rule and stops being the only thing standing between the operator
  and a silent sum.
* **The prefix is not a bare convention.** It is the key into a definition
  stored beside the numbers, so "which definition produced this" is answerable
  from the store rather than from whatever happens to be installed where the
  tape is being read. ⚠ That is the unsoundness in the current unit inference,
  which asks the LOCAL documents — the host that wrote a tape need not be the
  host reading it.
* **The tape is self-describing**, which is what makes a tally worth keeping
  for two years after its log is gone. A number whose definition is lost is a
  number nobody can act on — the same argument `!meta` was added for, one
  level up and without the per-run state.
* **Fleet comparison becomes a comparison of definitions**, not of names. Two
  tally stores whose `apache` differs are not combinable, and a reader can say
  so instead of adding them.
* **Drift detection already exists.** `plan()` compares every declared key
  against what the store holds and reports `⚠ {k} is {is}, this provisioning
  says {want}`. A tally store declares `class`, `derived_op`, `derived_from`,
  `wal` — and nothing about its definitions. Given them, a changed definition
  becomes visible drift through code that is already written.

## Editing a definition changes nothing. APPLYING one is an act

That is the property the stored copy buys, and it is worth stating plainly
because it is the opposite of how a config file usually behaves: a document
edited in `/etc/timberfs/tally.extractors.d` does **not** change any tally.
The store holds the definitions it was created with, and goes on producing
those numbers until somebody decides otherwise.

### Usually, applying just means UPDATING — and that is fine

⚠ **The common changes do not break a series, and the design must not make
them expensive.** What an operator actually does, most of the time:

* **fixes a regex** — a `claim` that was too narrow, or a producer whose
  format shifted. The instrument got better; the measurement is the same one.
* **adds a metric** — strictly additive.
* **removes a metric** — the series stops, exactly as a producer going quiet
  stops one.

For all three, replacing the store's recorded definitions in place is correct,
and the tape stays ONE series. "I am changing what this metric MEANS" is the
exception, not the rule, and a design that treated every edit as a
supersession would make the ordinary case ceremonial and get worked around.

So the recorded definitions are a **record, not a lock**: drift is REPORTED —
here is what changed — and the ordinary answer is "update them, I know what I
am doing". Only a changed meaning needs anything more.

⚠ **Record the change, do not make a reader reconcile it.** When the
definitions are updated, write down that they were, when, and at what offset.
That is the offset-scoped idea in its workable form: **provenance for a person
looking at a step in a graph, never a contract a reader has to honour.** The
version below that failed was the one that asked a reader to reconcile two
definitions inside one window; nobody has to reconcile a regex that got
better, they only have to be able to find out that it did.

⚠ **One subtlety the additive case raises**: [tally.md](tally.md)'s second
invariant is that **zero and unknown are different**. A metric added today did
not exist last week, so last week's buckets are UNKNOWN for it and not zero —
and today nothing says so. `!gap` is the marker for "a window the extractor
could not see" and is not written yet; a metric that did not exist is the same
shape of fact and wants the same treatment.

### The exception: a changed MEANING

Only here is a new tally warranted, and only here do the conventions below
matter. Two decisions, neither with a default right for everyone.

**What becomes of the old tally** — three answers, all legitimate:

| | expressible today |
|---|---|
| drop it | delete the store and its follower |
| keep it, stop writing | stop the follower; ⚠ nothing declares it closed |
| keep it, keep writing | leave its provisioning; add a second `.conf` with its own `OUTPUT` ✅ |

**Where the new tally starts** — three answers again:

| | expressible today |
|---|---|
| from now | `FOLLOW_FROM=end` ✅ |
| regenerate everything available | `FOLLOW_FROM=begin` ✅ |
| regenerate from a fixed time | ⚠ not expressible: `FollowFrom` has no clock |

⚠ On that last one, `FollowFrom::End` carries a warning worth reading before
adding a timestamp: "a POSITION rather than a clock, because a clock is what
broke a tail once already". **It does not settle this case, and the difference
matters.** A clock is treacherous as "start now" — it races the writer and the
host's own time. As "start at this point in a tape that already exists" it is
what `query --from` does routinely and safely. A backfill start and a tail
start are not the same question, and only the second is what that comment is
about.

⚠ **The SOURCE store's retention is the budget for changing your mind.**
`retain 30d` on the log means definitions are revisable over thirty days of
history and no further; throw the log away and the tally you have is the tally
you keep. A real operational consequence, documented nowhere.

The cheapest moment to adopt any of this is while stores are being dropped and
re-derived anyway — the state 0.33.0 left the production tally stores in.

### What the two tallies must say about each other

⚠ **Mostly needed only for the exception above** — but the supersedes
relation is also what "regenerate what the source still covers" rests on, so
it is not purely exceptional; see that section. Most of the mechanism already
exists; the conventions do not, and without them a fleet accumulates tally
stores nobody can order.

* **A supersedes relation, and an ORDER over the overlap.** `.bark` carries
  `derived_from` and `derived_op`, which point at the SOURCE. Nothing points
  from one tally to the tally it replaces, so a reader assembling a long
  window cannot know the two are the same measurement under two definitions,
  and a person cannot tell which is current. ⚠ Most needed where it is least
  visible: a definition that kept its name and changed its content leaves both
  stores carrying IDENTICAL metric names. ⚠ And newest-line-wins is defined
  WITHIN a tape — tape order is arrival order, and two tapes have none — so
  the relation must also say which store wins where they overlap. That is the
  one thing a reader cannot infer, and without it summing both double-counts
  precisely the period that was re-derived in order to be trusted.
* **A declared closed state.** "Nothing follows it" is visible in `list`'s
  `FOLLOWERS` column but means several things — never started, deliberately
  retired, or broken. A store that will not be written again should say so, so
  that a stalled follower and a finished one are not read alike.
* **A naming convention.** `OUTPUT` must vary per source store, so two tallies
  from one source need two templates and the operator invents the
  discriminator. A generation in the name (or a label carrying it) makes the
  ordering readable without parsing names.
* **Which properties are shared and which are not.** Both tallies describe the
  same source and want the same `retain`; only one is current. The split
  between "inherited from the provisioning" and "true of this generation only"
  has to be decided once rather than per site.

## Regenerating what the source still covers

The case worth having, and the one the retention budget above creates: the log
is kept 30 days, the tally two years, and today the regex got better. You want
the last 30 days re-derived with it and the previous 23 months left alone —
those numbers being the best that will ever exist for that period.

**Resetting the tally's TAIL and re-deriving it** is the direct way, and it
costs more than the wording it amends. "Chunks are immutable" is not a slogan;
three things rely on it, and none of them checks whether a store is derived:

* **The chunk number is an address that travels.** `docs/design.md` has it as
  "a position in one store, not a fact about its contents", and replication
  *preserves* numbering so that `dropped + uncomp_start` is "the same absolute
  number at both ends" — which is what lets a receiver state its coverage.
  Regenerating chunk 500 with different bytes makes two stores disagree about
  what 500 is, and `frames-send` sends what the receiver says it LACKS, so it
  would never notice.
* **Anything caching chunks by `(store id, number)`** serves the old bytes for
  ever. That cache exists.
* **Follower positions past the cut** point beyond the new end. A consumer
  shipping tally lines onward has a durable offset that no longer means
  anything.

⚠ A derived store *is* genuinely a different case — regenerable by
construction, which is what `derived_from` and `derived_op` say — so the
exception is not arbitrary. The trouble is that the machinery relying on
immutability does not ask.

**The same outcome is available without mutating anything**, and it is the
generation mechanism deferred above, earning its keep for the ordinary case
rather than only the exceptional one: keep the old tally and stop writing it,
and derive a NEW one from the source with `FOLLOW_FROM=begin` — which is
exactly "everything the source still holds", i.e. the 30 days. The seam then
sits at the source's retention horizon instead of inside a tape, and a long
window reads both stores.

Which is not a new capability. `graph … from [class=tally]` already spans many
stores; spanning two generations of one source's tally is that same operation.

⚠ **What it does need is an ORDER over the overlap.** The two stores both
cover the last 30 days, and newest-line-wins is defined WITHIN a tape — tape
order is arrival order, and two tapes have none. So the supersedes relation has
to say which store wins where they overlap, and that is the one piece a reader
cannot infer. Without it a reader summing both double-counts exactly the
period that was re-derived to be trusted.

### Keeping the old store whole is not the answer either

Two objections to "keep it and stop writing", and both are right:

* ⚠ **Retention is enforced by WRITERS.** `trim`'s own reason for existing:
  retention "runs inside a live writer … so a store whose producer went quiet
  keeps its data indefinitely". A retired generation is precisely that store,
  so the thing whose whole point is a two-year retention would sit there
  unbounded. `timberfs trim --select '[class=tally]'` is the cron-able answer
  and sweeps every generation at once, but somebody has to arrange it, and a
  design that needs a cron job to stop accumulating has got its defaults
  backwards.
* ⚠ **The proportions are usually wrong.** 1 GB of tally where the first
  100 MB is worth keeping and the trailing 900 MB is to be re-derived means
  holding a 1 GB store to serve a tenth of itself, plus the new one — 1.9 GB
  for 1 GB of numbers, with 90% of the old store dead weight that the overlap
  rule tells every reader to ignore.

**`export` already does the right thing**, and it is the piece that makes the
seam affordable: it copies a WINDOW into a NEW store, chunks verbatim and no
recompression, with "fresh identity, lineage to the source". So:

```sh
timberfs export tally --into tally-gen1 --to '<the source's horizon>'
# drop the old store; derive tally-gen2 from the source with the new definition
```

100 MB plus 900 MB, nothing duplicated, no tape mutated, and the immutability
the chunk number rests on is untouched. The price is copying the kept prefix
once rather than truncating in place — cheap, because it is verbatim frames
and it is the small end.

⚠ **So a tail truncation is the operation an operator WANTS, and `export` plus
a drop is how to give it to them without the three costs above.** What is
missing is not a primitive; it is that nothing puts those three steps behind
one verb, and doing them by hand in the wrong order loses the numbers that
cannot be re-derived. Retention on a retired generation is the one genuine
gap.

## A tally is not a log, and the tape model was inherited rather than chosen

Everything above works around one assumption: that a tally store is a tape
like any other. It is worth asking why a log tape is immutable, because the
answer does not transfer.

**A log entry is a historical FACT.** A producer wrote it; no later knowledge
revises it. That is why a sealed chunk can be immutable, and
[chunks-by-address.md](chunks-by-address.md) rests on exactly that — "a
head-drop changes a chunk's OFFSET inside the trunk, never its bytes … so a
cached copy cannot go stale". The property is inherited from the domain, not
imposed by the format.

**A tally bucket is a CONCLUSION.** It is derived, recomputable, and — since
0.33.0 — routinely revised: one bucket appears many times and the newest line
SUPERSEDES the earlier one. ⚠ **No log tape has any such rule.** So a tally
tape is already only BYTE-immutable and not semantically immutable: the
earlier line's bytes sit there while its meaning is annulled. The divergence
has already happened, in shipped code; what is left is a format insisting on a
discipline its content does not have.

### What is actually irreplaceable

Follows directly, and it is the useful part: a tally is regenerable wherever
its source still exists, so **the only irreplaceable stretch of a tally store
is the prefix older than the source's retention horizon.** Everything newer is
a cache — expensive to recompute, never impossible.

Which makes "export the prefix, drop the rest, re-derive" not a workaround but
the natural decomposition: it separates the part that is data from the part
that is a cache. And it says what a tally's retention is really for — the
`retain 730d` that outlives a 30-day log is protecting one stretch, and
re-deriving the other 30 days costs a read of a source that is still there.

### WAL timelines, and whether they are overkill

The postgres analogy is the right shape and it dissolves all three costs of
regenerating in place, which is worth recording even if nobody builds it.
Address a chunk by **(timeline, number)** and bump the timeline when history
diverges: a regenerated chunk 500 on timeline 2 does not collide with 500 on
timeline 1, so a cache keyed on the address cannot go stale, a replica sees a
new timeline rather than silently disagreeing about what 500 is, and a
follower's position is on a timeline it can be TOLD has ended instead of
pointing past a new end.

⚠ **Probably overkill, and the reason is surface rather than complexity.**
Every reader and consumer would have to learn about timelines — `query`,
`view`, the frames wire, follower positions, the chunk cache — for an
operation performed rarely, since the ordinary definition change is an
in-place update that regenerates nothing. `export` plus a drop buys the same
outcome today with no format change and no new concept, at the cost of
copying the kept prefix once.

So: recorded as the correct model for a store whose content is derived, and
the thing to reach for **if regeneration becomes routine** rather than
exceptional. The signal to watch for is operators doing the three-step
retirement often enough to want it behind one verb.

## Offset-scoped definitions: a dead end as SEMANTICS, kept as PROVENANCE

The most *correct* answer is that the tape knows which definition was in force
from which byte offset, superseded by the next — the general form
[tally.md](tally.md) already reaches for under "Declarations scoped to a range
of the tape". ⚠ The distinction that makes it usable is **who has to obey it**.
As a CONTRACT A READER HONOURS it does not work, and that is worth writing
down so it is not re-proposed; as a NOTE FOR A PERSON it is exactly right, and
is what the update path above records. The same fact, and only one of the two
uses is affordable:

if `some_metric` changes definition at offset `0x42424242`, a reader asking for
a window that spans it has no honest answer. Summing is wrong — they measure
different things. Showing both means the name meant two things in one answer.
Refusing the window makes the tape useless for the long cheap windows tallies
exist for. **More correct and less usable**, which is the signature of the
wrong granularity; and the store-scoped version above gets the same property —
the definition follows the tape — at a granularity somebody can act on.

⚠ **But nobody has to reconcile a regex that got better** — they only have to
be able to find out that it did. So the offset goes on the record as
provenance: a step in a graph becomes explicable instead of mysterious, and no
reader is obliged to do anything about it. That is the whole of the idea that
survives here, and it is cheap.

Worth keeping as an idea for elsewhere: a producer that changed its line format
mid-life has one `timestamp_regex` today, and that IS a range-scoped
declaration problem where regeneration is not an option.

## Settled: an assigned id, and the prefix is the display form

The id was open because a name is a handle rather than an identity. That
objection only bites if the id must CARRY identity — and it does not, once the
definition is stored beside the numbers: then the stored content is the
identity and the id is a pointer into it, unambiguous within one tally store
and nowhere else. So it is a small assigned number, and being short is free
rather than a trade.

* ⚠ **Assigned, never positional.** A monotone counter kept with the
  definitions, never reused. Removing a metric is an ordinary edit (above), and
  an index into a list renumbers everything after the hole while written blocks
  still hold the old numbers. `tally_block`'s `Entry` already carries this
  rule — "the address is `(t0, generation)`, never a position in a sequence".
* **It attaches to the METRIC, not the series.** Within a store a metric maps
  to exactly one definition, which is what makes uniqueness mechanical, so an
  id per series would repeat it once per series — 843 times for one metric on
  the measured day. A metric table in the block gives it one home, and stops
  the metric NAME being repeated across the dictionary too. Not a size win,
  zstd already squashing that repetition; one home rather than N is the reason.
* **The record is `id -> definition` plus an append-only log of replacements**
  — when, and at what offset. That is the provenance this note already
  requires, and it is why an id's definition may be replaced while the id
  never moves.
* **Ids never travel.** Fleet comparison is a comparison of definitions, so two
  stores' id 1 are unrelated, and nothing should try to make them global.

**And the prefix is the display form** — what a human types in a query and what
a rendered line shows. It is therefore NOT structural: inside a block,
`http_requests` under definition 1 and under definition 2 are already distinct
without it. Both forms are served, each by what suits it: the id inside the
block, the prefix in the text interchange, which has no metric table and needs
a name a reader can tell apart.

## Settled: one immutable file per applied set, named by its offset

`definitions/<zero-padded offset>.json` in the store's directory, written once
and never rewritten. **The filenames ARE the replacement log** the section
above requires — no separate log to keep consistent, nothing to rewrite, no
locking, and "which definitions were in force at X" is the newest file whose
offset is not greater than X.

⚠ **It fits the BLOCK store and not the tape, which is itself an argument for
where this belongs.** `format::every_path` is extension-keyed — `<name>.<ext>`
for a fixed list — so a tape sidecar must be exactly ONE file per store; a
file per applied set is not expressible there without inventing a directory
beside the pair. A block store is already a directory of many files.

⚠ **And the enumerator that has to learn about it is `Manifest::unreferenced`,
not `every_path`.** It treats every entry that is not `manifest.json`, `open`
or `*.tmp` as a block file, so a definitions file would be reported as debris
for the caller to unlink. This note's warning — "a part nothing picks up is
worse than a missing one" — was right and pointed one level away from where it
lands.

The rest follows from the name being a boundary rather than a label:

* **Zero-padded**, or `10` sorts before `9`. The block names are fixed-width
  for the same reason.
* **The offset is the tally consumer's position in its source store**, which
  is unambiguous because one tally has one source — the provisioning refuses a
  template naming one store for several sources, so a release that starts a
  new source store starts a new tally beside it and offsets never mix.
* ⚠ **The wall clock goes INSIDE, not in the name.** A reader asking "which
  definition produced this bucket" holds a bucket start, not an offset, and
  converting needs the citation — which is widened and switchable off. That is
  survivable only because this is provenance for a person looking at a step in
  a graph rather than a contract, and it is why the file carries `applied_at`
  in UTC for the human while the name stays exact and zone-free.
* **Content**: the assigned ids, the resolved documents, `applied_at`, the
  source store's id, and the id counter's high-water mark — the counter living
  with the definitions is what keeps it monotone across a removal.
* **Written only when the resolved content DIFFERS from the newest.** A
  provisioning converges on a timer and at every deploy, so an unconditional
  write would file one set per run and bury the changes that mattered.
* **Retention keeps the file in force for the OLDEST surviving block.**
  Definitions outlive the numbers they describe, or the numbers stop being
  readable — which is the whole reason for storing them.

**Two reads, in opposite directions, and neither is on a hot path.** The
writer reads its DOCUMENTS once at startup — `load_extractors` and `Run::new`
are each called once, and the record loop never re-reads — so a document
edited under a running writer takes effect at its next RESTART, which is
"applying is an act" in mechanical form. It reads the store's newest
definitions file at that same startup, and only to answer two questions: does
the record differ from what I am about to compute, and what is the id
high-water mark. The store is somewhere the writer files a record, never
somewhere it learns anything.

That also fixes the boundary: the offset in a filename is the follower's
resume position at that restart — a real event at a known position rather than
an arbitrary point.

⚠ **Which leaves drift with nobody to report it.** A document edited and never
applied keeps the store producing the old numbers, correctly, and silently: a
writer that read its documents once cannot notice a later edit even in
principle, and `--check` validates documents while reading no store. So the
"drift is REPORTED" promise above needs a comparison that does not exist —
recorded definitions against the documents on disk. Its home is the
provisioning's converge run, which already executes on a timer and at every
deploy and is where an operator already looks.

### Applying makes them current, in one act

⚠ **What this must never become is "apply, and then remember to restart
something".** A configuration write that needs a remembered second step is a
defect rather than a procedure: the interval between the two is a state where
the documents say one thing and the numbers are another, and nothing in it is
wrong enough to notice. So APPLY is one command with one outcome — the
definitions are current when it returns, or it failed and said so.

Whether that involves restarting a follower, stopping one, or only writing a
file is an implementation detail, and today it is a restart: the writer reads
its documents once — `load_extractors` and `Run::new` are each called once,
and the record loop never re-reads — so a document edited under a running
writer changes nothing until it starts again.

**That forces the order, and the obvious order is wrong.** Stop the follower
so its position becomes durable, write `definitions/<that position>.json` if
the content differs, then start it. Writing the file FIRST and restarting
after leaves the follower free to advance past the recorded offset before it
stops, so the file claims a set applied from a point where the previous one
was still producing numbers. Each step is idempotent, so a re-run converges;
and if the start fails the filed definitions are not merely harmless but
correct, the position not having moved.

⚠ **So there is no drift to report, and that is the point** — an unapplied
edit is a no-op by design, and the state where a store's record disagrees with
what is running is unreachable rather than monitored. What remains useful is
"what would applying change", which is `--dry-run` on the same command rather
than a mechanism of its own.

**Finding the ACTIVE set is a readdir and a sort, and that is deliberately not
optimised.** The cost is a rounding error against what the same directory
already holds — one block per day, so ~730 entries at a two-year retention,
against a definitions file per CHANGE, which is a handful a year — and the
lookup happens once when a reader opens the store, never per bucket or per
series.

⚠ **If it ever does need optimising, extend the MANIFEST; do not add a
`current` file.** A rewritten pointer is exactly the mutable, tearable,
lock-needing thing this scheme removes, and it would be the first thing anyone
reached for. The manifest is loaded anyway, is committed by temp-and-rename,
and is the store's commit point — so a pointer there costs no I/O and becomes
atomic with the blocks written under that set. The files stay the record;
the manifest would carry only the newest offset.

**The write order is `definitions -> blocks -> manifest`**, each step
referenced only by the next, so every crash window leaves unreferenced debris
rather than a dangling reference — a block must never cite an id nothing
defines. ⚠ A definitions file is found by readdir rather than named by the
manifest, so an orphan from a crash IS taken as the active set. Benign both
ways: the restarting writer either finds its definitions identical and files
nothing, or files a newer set and leaves the premature one standing as a
record of definitions nothing was produced under. Misleading provenance at
worst, never a wrong number.

**The write order is `definitions -> blocks -> manifest`**, each step
referenced only by the next, so every crash window leaves unreferenced debris
rather than a dangling reference — a block must never cite an id nothing
defines. ⚠ A definitions file is found by readdir rather than named by the
manifest, so an orphan from a crash IS taken as the active set. Benign,
because the position it names has not moved: the restarting writer either
finds its definitions identical and files nothing, or files a newer set and
leaves the premature one standing as a record of definitions nothing was
produced under.

⚠ **A lone block is deliberately not self-describing.** It names its
definition by id; the store holds the text. That is the split the assigned id
buys, and it answers the replica question below: a bundle carries the
directory, so a replicated block store is whole, where a tape's `.bark` never
travelled.

## What changes

A first cut, in order, and none of it needs the generation conventions:

1. **Copy the resolved definitions into the tally store at creation**, keyed by
   short name.
2. **Prefix metric names with that short name.** No format change; `:` is
   already legal.
3. **Report drift and let the operator apply it** — the ordinary path. Record
   that the definitions changed, when, and at what offset, as provenance.
4. **Fix the guard's message and its four leaks** — `--check`, `--provision`,
   the first-entry laziness, and `{extractor}` in `OUTPUT` so the refusal is
   actionable.
5. **Read the unit from the stored definitions**, so nothing is inferred from
   the local install.

Deferred, and only for a changed MEANING: the supersedes relation, the closed
state, the generation naming, and the shared-property split.

## Open

* ~~Where the definitions live.~~ **A file per applied set, named by the
  offset it takes effect at, in the block store's directory** — see above. The
  `.bark`-or-sidecar question was a tape question, and the tape is not the form
  this is being built for.
* ~~Whether a replica is self-describing.~~ **Yes, for a block store**: a
  bundle carries the directory, definitions included. A lone block names its
  definition without spelling it, which is intended.
* ~~Whether the prefix is mandatory or only on collision.~~ **Mandatory**,
  following from the prefix being the display form: a name that changes when a
  second document arrives is a worse display name than a uniform one, and a
  query typed against a name has to keep working. Uniqueness does not rest on
  it either way, the id being the discriminator where one is needed.
* **Whether the shipped `-apache-combined` and `-nginx-combined` should
  declare a shared measurement id**, so drawing them as one line is a stated
  fact rather than a name coincidence — the smallest test of whether the
  reader-side correspondence mechanism is any good.
* **Who trims a retired generation.** The provisioning knows its generations
  and could sweep them on its own tick; a cron'd `trim --select
  '[class=tally]'` needs no code and needs arranging. The first makes the
  common case correct by default, which is the argument for it.
* **Whether the three-step retirement should be ONE verb.** `export` the kept
  prefix, drop the old store, derive the new one — done by hand in the wrong
  order it loses exactly the numbers that cannot be re-derived, which is the
  case for not leaving it as three commands in a runbook.

## Separable, and worth fixing whatever is decided

* **`--check` and `--provision` must detect a colliding `APPLY`.** A follower
  dying on its first entry is the worst available place to learn of a config
  error.
* **`!meta` carries a unit without saying who asserted it.** Unsound
  regardless, for the reason above: the reader's local documents are not the
  writer's.
