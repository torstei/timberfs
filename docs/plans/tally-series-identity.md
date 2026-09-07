# A metric is a series, and combining it is the READER's decision

**Status: not built.** A defect in the shipped tally — the decision "are these
two things one series" is currently made by the STORAGE layer, at fold time,
irreversibly — and the remedy, which is small: the definitions travel with the
tally store, and their names are the namespace. Amends [tally.md](tally.md),
whose `Run::new` guard is right for a reason it does not give.

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

Applying new definitions is therefore a deliberate operation with two
decisions in it, neither of which has a default that is right for everyone.

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

Most of the mechanism above already exists; the conventions do not, and
without them a fleet accumulates tally stores nobody can order.

* **A supersedes relation.** `.bark` carries `derived_from` and `derived_op`,
  which point at the SOURCE. Nothing points from one tally to the tally it
  replaces, so a reader assembling a long window cannot know that the two are
  the same measurement under two definitions, and a person cannot tell which
  is current. ⚠ This is most needed in exactly the case where it is least
  visible: if the definition kept its name and only its content changed, the
  metric names in both stores are IDENTICAL and nothing distinguishes them.
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

## Recorded dead end: offset-scoped definitions

The most *correct* answer is that the tape knows which definition was in force
from which byte offset, superseded by the next — the general form
[tally.md](tally.md) already reaches for under "Declarations scoped to a range
of the tape". It is a good idea and it is **not a workable solution to this
problem**, which is worth writing down so it is not re-proposed:

if `some_metric` changes definition at offset `0x42424242`, a reader asking for
a window that spans it has no honest answer. Summing is wrong — they measure
different things. Showing both means the name meant two things in one answer.
Refusing the window makes the tape useless for the long cheap windows tallies
exist for. **More correct and less usable**, which is the signature of the
wrong granularity; and the store-scoped version above gets the same property —
the definition follows the tape — at a granularity somebody can act on.

Worth keeping as an idea for elsewhere: a producer that changed its line format
mid-life has one `timestamp_regex` today, and that IS a range-scoped
declaration problem where regeneration is not an option.

## What changes

1. **Copy the resolved definitions into the tally store at creation**, keyed by
   short name.
2. **Prefix metric names with that short name.** No format change; `:` is
   already legal.
3. **Fix the guard's message and its four leaks** — `--check`, `--provision`,
   the first-entry laziness, and `{extractor}` in `OUTPUT` so the refusal is
   actionable.
4. **Read the unit from the stored definitions**, so nothing is inferred from
   the local install.

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

## Separable, and worth fixing whatever is decided

* **`--check` and `--provision` must detect a colliding `APPLY`.** A follower
  dying on its first entry is the worst available place to learn of a config
  error.
* **`!meta` carries a unit without saying who asserted it.** Unsound
  regardless, for the reason above: the reader's local documents are not the
  writer's.
