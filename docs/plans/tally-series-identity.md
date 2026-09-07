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

* **Where the definitions live.** A sidecar beside `.bark`, or inside it.
  ⚠ A sidecar must be added to `format::every_path`, which is what delete,
  rotation and retention enumerate — a part nothing picks up is worse than a
  missing one. `.bark` avoids that but is read constantly and would carry
  every applied document.
* **Whether a replica is self-describing.** `.timber` bundles carry `.rings`
  and `.trunk`; the `.bark` does not travel today. So "the definition follows
  the tape" across replication is unanswered, and it is the case that matters
  most for a tally kept longer than its log.
* **Whether the prefix is mandatory or only on collision.** Mandatory is
  uniform and makes every existing name change; on-collision keeps today's
  names and makes the prefix conditional, which is a rule with an exception.
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
