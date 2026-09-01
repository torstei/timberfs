# Follower selection: one declaration, many stores

**Status: design.** Nothing here is built. The reader half it rests on is:
a selection resolved per read, a position per store keyed by store id, the
`position` records that carry them back, and a resumed read that includes the
live edge. See [paging.md](paging.md) for that half and
[receiving-end.md](receiving-end.md), which argued for selection as the
primitive and proposed a different mechanism for it.

Today one follower is one store, so forwarding N stores to one place is N
declarations, N units and N processes. The destination is stated N times and
drifts N ways.

## The subject of a follower becomes a selection

    A follower is a destination and a selection, and a position in each
    store that selection matches — by identity, one per store.

which replaces "a follower is a position in ONE store". A single-store
follower is the one-term case: `--store applogs` is sugar for
`--select 'id=<the store's id>'`, resolved when the declaration is written.
One shape rather than two, so `run`, `list`, `status` and the interest axis
each keep one code path.

    /var/lib/timberfs/followers/<name>/
        follower.json    select, type, endpoint, retaining, args
        positions.json   { "<store-id>": {offset, wl, delivered}, ... }
        follower.lock    held while it runs

`positions.json` is one file rather than a directory of cursors because a
batch spanning several stores is acknowledged once: one tmp+rename either
advances every store in that batch or none of them, where N files can tear.
There is exactly one writer, so nothing wants the finer granularity.

The position is an **offset** on the store's tape, not the `(seq, n)` a
`Cursor` holds. It is what the answer's `position` record carries, so no
conversion can disagree with the wire — and it addresses an entry in the
write-ahead segment, which a chunk number cannot: a chunk-granular position
stands still at the live edge, so a restart re-delivers everything written
since the last flush. The interest axis wants a chunk number and derives one,
the store's own rings being what says which chunk an offset sits in.

A store that leaves the selection keeps its entry — bringing it back should
resume, not re-ship a history — and membership is decided by the selection,
never by what the file happens to hold, so a stale entry holds no retention.

⚠ **A store with no identity cannot be followed.** The cursor is keyed by the
`.bark` id, so a store that declares none gets no `position` record and no
resume: a poll loop would re-ship it in full on every poll, forever. Such a
store is therefore excluded from the selection with a note naming
`timberfs identity <store> --mint`, which is the remedy. Nothing shipped
beats shipping the same store endlessly, and a reader has no business
minting an identity into someone else's manifest.

## Why there is no follower group

Every consumer of a per-store position needs the position and not a name for
it: the interest axis takes a floor, `list`'s FOLLOWERS column answers with
the declaration's name, `status` wants a per-store table, the loss record
names the declaration plus the store. No member is enabled, deleted, locked
or addressed. A member that is never addressed is not an object, and a set of
non-objects is bookkeeping.

Two costs settle it. A second registry would double the surface of the rule
everything else rests on — an unreadable declaration fails closed for every
store — and any name derived from `follower` lands next to the
`timberfs-follow@` / `timberfs-follower@` pair the units already warn about.

**"Follower group" is reserved** for several processes sharing one selection,
each taking a shard of it: the answer to head-of-line blocking and throughput
at high store counts, which needs a coordinator and has members that are
addressed. Spending the word on an object with one member means not having it
then.

## `[]` is the selector, and there is one of them

`select.rs` is the only matcher. The query document's structured terms are
rendered to the string grammar and parsed by it, with an unknown operator
refused rather than formatted (a shorter operator hides inside a longer one,
so `=?` reads as `=` against `?value`). `matches` compares against the whole
manifest, so `name`, `id` and settings are selectable beside labels.

So `--select 'service=~apache-.*'` is already the grammar
`[service=~apache-.*]` is. Two conveniences live only in timbersh and belong
here, and they are one change:

- **A bare word is the name, matched anywhere in it.** `Op::Contains`
  directly; timbersh spells it as an escaped anchored regex only to survive a
  build without `=*`.
- **The typo guard that makes it safe.** Once a bare word is legal,
  `service~api` becomes a name search rather than an error. A term holding
  any of `=~!*` with no operator this build knows is refused.

An operator checks a selector against the fleet with the shell
(`select stores from [service=~apache-.*];`) and against one host with
`timberfs list --select`. `follower dry-run <name> --dump-json` emits the
poll document the follower will send, which is the same document either can
be handed — the follower states what it will ask rather than being described.

## Short ids are a CLI convenience, never persisted

A general rule, not a follower one. The eight characters a listing prints are
input: resolved against the store list, refused when ambiguous, and expanded
to the whole id before anything is stored or sent. A prefix that is
unambiguous today is ambiguous the day a store is created, so a persisted
prefix silently changes what it selects — and for a follower that means
forwarding a store nobody chose, or refusing to run.

timbersh already observes it: one expansion point, through which the terms a
`create logview` stores also pass, so a saved view is exact. A declaration
does the same at `create`.

## The consumer is a poll loop

One process per destination, and its shape is the shell's `tail`:

1. `response_format: {kind: "stores"}` with the selection — which stores, and
   each one's labels, for the resource attributes a destination wants.
2. `{stores: {select}, cursor: {...}, max: {entries: N}}` — entries from each
   position forward, each tagged with the store it came from.
3. Ship the batch; on the destination's acknowledgement, save the positions.
4. `status=limited` means drain again now; `exhausted` means sleep.

⚠ **A shared cap needs a round-robin.** The read drains its sources in order
under one entry cap, so a store producing faster than one poll can drain it
takes the whole cap every time and every store behind it ships NOTHING —
permanent starvation, not a delay. So a bounded poll starts the next one
just AFTER the store it stopped in, and that store's remaining backlog waits
a turn, which retention is already the budget for.

The selection is resolved by the read, so a store that appears between polls
is in the next answer with no cursor entry, which reads it from the start —
and one that stops matching simply stops appearing. Nothing watches the
forest. A store's first appearance therefore ships its whole backlog; where
that is wrong, seeding its position from the `stores` read of step 1 is the
same two-read join step 1 already performs, and which of the two is the
default belongs in the declaration.

Not `query --records --follow`: it takes no cursor map, and `--from-chunk` is
one number for every source, which across stores means nothing. A resumed
bounded read reads the live edge, so the poll loop is not the slower option.
`--follow` stays the tool for a person and a pipe.

**A gap is the consumer's to report.** Entry runs chain — the end of one is
the start of the next, and the end of the last is that store's `position` —
so a resume that retention has overtaken shows up as a first `offset` past
the cursor handed in. Nothing else can see it.

## OTLP merges; frames does not

`resourceLogs` is a list, and our own intake already decodes it as one, so a
merged batch is one request carrying one group per store — the resource
attributes stay per store, which is the whole point of forwarding a set.
`otlp::render` emits a single-element list today.

The frames wire is one stream per connection; the stream id is in every frame
but multiplexing waits on per-stream flow control, since one stalled store
must not block the rest. So a frames follower holds one connection per store
from one process — the unit count is what collapses, not the socket count.
It needs no positions at all: a frames sender resumes from the receiver's
coverage.

One destination means one queue, so a stalled endpoint stalls every store in
the selection. That is the right coupling — they share the destination — and
retention is the budget for it, exactly as it is for one store.

## Retention: the unbounded case is allowed

`--retaining --select '*'` holds back every chunk no follower has read, on
every store on the host. It is a real configuration and it is not gated.

What makes it legible rather than surprising: interest is **additive**, so
age and size still apply. `retain_unconsumed` with a `retain_size` is
bounded — the budget wins, and the drop is recorded exactly, naming the
follower, its position and the chunks it never read. Without one it is
unbounded, and with `--select '*'` that is the whole host.

So the answer is visibility, not refusal. `create --dry-run` and `dry-run`
print the match count and, for a retaining declaration, the bytes it would
hold today; `status` reports held bytes per store and in total; `list` shows
the follower on every store it matches.

## Open

- Where a store's first appearance starts, as a declared default: its
  beginning (what the cursor's absence already means) or its end (the
  two-read join). A selection widened by a label edit ships an old store's
  whole history under the first.
- Whether the interest axis evaluates selectors against every store per tick
  (M selectors × N stores of cheap matching, replacing a hashmap lookup on
  the anchor) or caches a resolved set and re-derives it on a manifest
  change. The tick already refuses to gate on mtime, for reasons that apply
  here too.
- Whether a follower may select stores it cannot retain — a selection read
  over the wire, from a host that is not the one holding them. The interest
  axis is host-local, so a remote follower can ship but cannot hold.
