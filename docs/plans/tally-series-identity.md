# A metric is a series, and combining it is the READER's decision

**Status: not built, and it contradicts what is.** A design defect in the
shipped tally — the decision "are these two things one series" is currently
made by the STORAGE layer, at fold time, irreversibly — and the argument for
what replaces it. Amends [tally.md](tally.md), whose line format and
`Run::new` guard both rest on the thing this note says is wrong.

## The defect, demonstrated

Two observations, from two different definitions, that happen to share a
metric name and a label set:

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

Two lines agreeing on those three ARE the same bucket. Which is right for the
same series said twice, and wrong for two series that share a name.

## The guard is a symptom, and it contradicts itself

`Run::new` refuses two documents that define one metric name. Its comment and
its error message assert opposite things about the same two shipped documents:

> ⚠ A metric is named once across the APPLIED set […] Two documents may share
> a name — apache's and nginx's `http_requests` **are the same measurement** —
> and they are only wrong TOGETHER

> `timberfs-apache-combined` and `timberfs-nginx-combined` both define the
> metric `"http_requests"` — applied together their samples fold into one
> series, and the numbers become **a sum of two different measurements**

Both readings are in the tree at once: the fleet graph *relies* on the name
collision to draw one line for a fleet of apache and nginx hosts, while this
guard *forbids* the same collision one store down. It is not a rule with an
exception; it is two rules.

And because the guard is a patch over the fold rather than a property of the
data, it leaks:

* **`tally --check` accepts what the run refuses.** It compiles each document
  alone and never sees the applied set, though it is the documented check
  ("Compiling is the check").
* **`tally --provision` accepts it too** — converges the stores, registers the
  follower, exit 0.
* **`Run::new` runs lazily, per store, on the FIRST ENTRY.** So a bad `APPLY`
  survives provisioning and kills the follower later, under systemd, when data
  arrives. Measured: `--provision --dry-run` exit 0, then
  `Error: … Apply one` from the consumer on its first entry.
* **It forbids a legitimate case.** One store carrying both producers' lines —
  a combined access log — is exactly where both documents should be applied
  and folded, which is the comment's own reasoning.

## Where this came from

Prometheus's encoding, borrowed deliberately and correctly: a histogram as one
series per `le` label, cumulative buckets, `sum`+`count` over averages.
[tally.md](tally.md) says so.

⚠ **But the encoding and the NAMESPACE are two decisions, and only one of them
was made on purpose.** In Prometheus a metric name is global, and name identity
is what makes a fleet aggregate; that is the property this note disputes, and
it arrived as a side effect of adopting the encoding. It is also the part
people find hardest to reason about there: whether two series combine is
decided by a name coincidence, somewhere upstream of the query, rather than by
the question being asked.

**A metric is a series of measurements. Whether two series are drawn together,
summed, or stacked is a property of the QUESTION, not of the data** — and a
name is a poor place to keep an answer that depends on who is asking.

## Why a name cannot be the identity, on this tree's own terms

This is not an argument from another database's taste. timberfs already decided
it, for stores:

* a store's identity is a **minted id** in its `.bark`, not its name;
* a store is **found by what it declares** — `--select '[service=apache]'`;
* "**never by path** — a store can move, and a path can come to hold a
  different one".

The name is a handle. Identity is declared. Selection is by the declaration.

[tally.md](tally.md) then says a document's name

> must not change what a document IS, **the same rule a metric name and a store
> identity already follow**

which takes the *word* identity from the store model without the mechanism —
because a store identity is precisely the thing that is **not** a name. The
tally design named its join key "identity" and then made it a string that two
unrelated definitions can both write.

## What identity a definition needs

⚠ **Not its document name, though the local discipline is better than it
looks.** `load_extractors` refuses two documents declaring one name — "a name
is claimed once", because "which definition a number came from must not depend
on readdir order" — and shadowing is by FILENAME, a file in
`/etc/timberfs/tally.extractors.d` replacing the packaged one of the same
filename entirely. Both verified. So within one machine's installed set a
document name IS unambiguous, and this note does not dispute that.

**It is fleet-wide that it fails, and for exactly the reason a store has a
minted id.** Filename shadowing means host B's
`/etc/…/timberfs-apache-combined.json` can declare a document named
`timberfs-apache-combined` whose metrics differ from the shipped one host A is
running. Each host is internally consistent; the two tapes are not, and a line
carrying only the name cannot say so. "A name is claimed once" is a statement
about a directory, and a tape outlives directories and crosses machines —
which is the same argument that made a store's identity a minted id rather
than its name ("never by path — a store can move").

So a definition needs what a store has: **a minted id, declared in the
document**, stable across renaming, shadowing, replication and the fleet. Then
a tape line can say which definition produced it, and no reader has to infer it
from whichever documents happen to be installed where it is being read.

## What changes

1. **The line carries the definition**, so a series is identified by what it
   is rather than by what it is called.
2. **`Roller` keys on that**, so the fold *cannot* merge two definitions. The
   `Run::new` guard then has nothing to guard and goes away, along with its
   four leaks.
3. **Combining is the reader's**, stated where the question is asked — drawn
   together, summed, stacked — and reversible, because nothing was summed on
   the way to disk.
4. **Time-coarsening stays storage's business.** ⚠ The invariant "every stored
   value must coarsen by addition, or by min/max/newest" is about re-bucketing
   ONE series in time, and that is sound. Cross-definition combination is a
   different operation that currently rides on the same mechanism, which is why
   it is irreversible. Separating them is most of this change.

## What it costs

* **Every tape already written carries unqualified names.** A reader must go on
  reading them, so the definition is an optional part of a series' identity and
  its absence means "unknown", not "the same as". ⚠ The cheapest moment to
  change what a line means is while the stores are being dropped and
  re-derived anyway — which is exactly the state 0.33.0 left the production
  tally stores in.
* **The fleet graph stops merging by coincidence.** Drawing apache beside
  nginx as one line becomes something asked for. That is the point, and it is
  also a real loss of convenience for the case the shipped documents were
  written to serve — see the open question below.
* **Bytes on the line**, if it is a label. Canonical rendering plus zstd makes
  a repeated label nearly free, and `!meta` is the cheaper alternative at one
  line per definition per run rather than per bucket.

## Open

* **The spelling**: a label, a `!meta` field, or a fourth positional field.
  A label is selectable and groupable for free by machinery that already
  exists; `!meta` is far cheaper but is per-run state a reader must carry, and
  [tally.md](tally.md) already records that `!meta` "is in the wrong place, not
  merely on the wrong schedule".
* **How a reader states the correspondence.** "These two definitions measure
  the same thing, draw them as one" has to be sayable, and by somebody who did
  not write either document. A declared measurement id shared BY the
  definitions is one answer; an alias at plot time is another; they are not
  exclusive.
* **Whether the shipped `-apache-combined` and `-nginx-combined` should declare
  a shared measurement id**, which would make today's convenient behaviour a
  stated fact rather than a name coincidence — and is the smallest test of
  whether the correspondence mechanism is any good.
* **What `!gap` and the other markers key on**, since they name a metric
  today and would need the same qualification to be attributable.

## Separable, and worth fixing whatever a name comes to mean

* **`--check` and `--provision` must detect a colliding `APPLY`.** The follower
  dying on its first entry is the worst available place to learn of a config
  error. True under either reading, and small.
* **`!meta` carries the unit but not who asserted it**, so a reader infers the
  definition from the documents installed *locally* — unsound however the
  naming lands, because the host that wrote the tape need not be the host
  reading it.
