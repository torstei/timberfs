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

⚠ The collapse holds only because a store always HAS an id — see «the pair
is the store» below. The case that looked like it would break it, a store
anchored `path:<canonical>` and so not nameable by a predicate, is a pair
with no identity on either side, which `identity` calls «not a store» and
which `follower create` already mints one for before registering anything.
So there is no store `--store` can name that `--select` cannot.

    /var/lib/timberfs/followers/<name>/
        follower.json    select, type, endpoint, retaining, args
        positions.json   { "<store-id>": {offset, chunk, wl, delivered}, … }
                         (`select` is stored canonically: `[]`, `[id=…]`, `[k=v,…]`)
        follower.lock    held while it runs

`positions.json` is one file rather than a directory of cursors because a
batch spanning several stores is acknowledged once: one tmp+rename either
advances every store in that batch or none of them, where N files can tear.
There is exactly one writer, so nothing wants the finer granularity.

**Two positions per store, because they answer different questions.** The
`offset` is where to RESUME: what the answer's `position` record carries, so
no conversion can disagree with the wire, and valid inside the write-ahead
segment, which a chunk number is not — a chunk-granular position stands still
at the live edge, so a restart re-delivers everything written since the last
flush. The `chunk` is the RETENTION FLOOR: chunks strictly below it are fully
consumed. It is recorded, not derived, because the answer states it on every
entry that has one — deriving it would make the interest axis read a rings
file to convert an offset, which is exactly what it must not do. It stays
where it was while entries arrive only from the live edge, which is the
conservative direction.

A store that leaves the selection keeps its entry — bringing it back should
resume, not re-ship a history — and membership is decided by the selection,
never by what the file happens to hold, so a stale entry holds no retention.

⚠ **A pair with no identity cannot be followed**, and that is the whole of
the exclusion: a position is keyed by identity, so such a pair gets no
`position` record and no resume, and a poll loop would re-ship it in full
every poll, forever. It is excluded with a note naming
`timberfs identity <store> --mint`. Nothing shipped beats shipping the same
pair endlessly, and a reader has no business minting an identity into someone
else's manifest.

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
- **The wrapping `[…]`, and with it `[]`.** The brackets are a DELIMITER — a
  statement must know where the predicate ends, and on a command line the
  argument boundary already says so — so an expression may carry them or
  not, and what timbersh renders is what a declaration can hold.

`[]` is the one that earns its keep beyond paste-ability: **a predicate with
no terms is «every store», SAID**, where an empty string is nothing written
at all and is refused. A read may reasonably default to every store when
asked for no predicate (`list` with no `--select`); a DECLARATION may not,
because "follows everything on this host" has to be a sentence somebody
wrote. So a declaration stores the canonical bracketed form — `[]`,
`[id=<uuid>]`, `[service=~apache-.*]` — which reads like the shell, and a
missing `select` member stays an error rather than a default.

⚠ The cost, documented rather than discovered: `[` and `]` are no longer
literal at the EDGES of a whole expression, so a store named `[foo]` is
selected as `name=*[foo]`. Inside a value they are untouched.

An operator checks a selector against the fleet with the shell
(`select stores from [service=~apache-.*];`) and against one host with
`timberfs list --select`. `follower dry-run <name> --dump-json` emits the
poll document the follower will send, which is the same document either can
be handed — the follower states what it will ask rather than being described.

## The pair is the store, and identity is asked of the pair

Both sides hold a store's id: `bark::save` mints it into the manifest and the
next open mirrors it into the `.rings` header, which is why
`ensure_identified` RECOVERS an identity rather than minting a new one. So a
manifest that is lost, or a store restored without it, has not lost its
identity — and `identity` says so, exit 0, «index only».

Every reader that decides whether a store can be SELECTED, CURSORED or
FOLLOWED must therefore ask the pair, not the manifest. Reading one side
makes a store's name depend on which of its files survived: measured before
this was fixed, such a store showed `ID -` in `list`, emitted no `id` on its
`source` and `position` records — so no cursor could name it and a poll loop
would re-ship it whole, forever — and its anchor silently changed from its
uuid to `path:<canonical>`, orphaning every follower and cursor that referred
to it.

`bark::identity_of` is that accessor, and `select::resolve`,
`cursor::store_anchor`, `query::open_source` and `summarize_store` go through
it. ⚠ Still reading the manifest alone: the frames sender's origin id,
`timber-otlp`'s `timberfs.store.id` resource attribute on the single-store
path, and the lineage `export` and `incus` record. Each takes a manifest map
rather than a path, so each is its own small change.

**A pair carrying no identity on either side is not a store**, which is
`identity`'s own verdict and its exit 1 — it is what a plain `append` or a
bare `create` leaves behind. It is listed (a catalogue must be able to say
«none») and it is not followable, because a position keyed by identity has
nothing to be keyed by.

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

## The loop is local; a remote DESTINATION is not remote at all

⚠ An earlier reading of this said remote followers must be ephemeral,
because the interest axis is host-local so `retaining` could not mean
anything for one. That reasoning assumed the whole loop ran elsewhere. It
does not: under [sink-protocol.md](sink-protocol.md) the loop and the
positions are always local and only the destination is a pipe away — over
`ssh` if you like — so `retaining` works for a remote destination like any
other.

What stays ephemeral is a remote READER, one pulling from another host's
stores. That is an investigation or a temporary watch rather than a
registration, and it is why a selection may run without `--positions`, at
the live end, keeping its places in memory and forgetting them on exit.

## Where a store's first appearance starts is declared

`--start begin|end`, per follower, because it depends and the person setting
it up is the one who knows: a store created a moment ago and a thirty-day
store that has just been relabelled into the selection are the same
observation to the loop and opposite intentions to the operator. `begin`
falls out of the mechanism (no position means the start of the window);
`end` is the two-read join `tail` already performs — ask `kind: "stores"`
for where each tape ends and seed the position there.

The default: `end`, except that `retaining` implies `begin`, which is the
rule the single-store path already applies and for its reason — retaining
says the data is not lost until this follower has it, so skipping the backlog
on the first run contradicts the declaration.

## The consumer is a poll loop

⚠ **Who owns the position moved after this was written.** The loop below is
right; the process it runs in is not. `timber-otlp --select --positions`
holds the positions today, and under
[sink-protocol.md](sink-protocol.md) timberfs holds them and the sink
reports — because the position file is where the retention floor lives, and
a program that writes it can get retention wrong silently where one that
reports cannot. The loop, the fairness rotation and the resource grouping
survive; they are re-homed into a `timberfs` subcommand.

One process per destination, and its shape is the shell's `tail`:

1. `response_format: {kind: "stores"}` with the selection — which stores, and
   each one's labels, for the resource attributes a destination wants.
2. `{stores: {select}, cursor: {...}, max: {entries: N}}` — entries from each
   position forward, each tagged with the store it came from.
3. Ship the batch; on the destination's acknowledgement, save the positions.
4. `status=limited` means drain again now; `exhausted` means sleep.

## What a poll costs, and where a store index would go

Measured 2026-09-02 on synthetic forests of identically-shaped stores, warm
page cache, release build, one entry per store. A poll is process start plus
resolving the selection over the whole forest plus reading every matched
store.

    forest    matched    resolve    whole poll
     1,000        500      <0.01s        0.01s
    10,000      5,000       0.07s        0.15s

So ~6 µs per store SCANNED and ~16 µs per store READ, and at ten thousand
stores a one-second poll costs 15% of a core. Both halves are syscall-bound:
a readdir per store directory plus one manifest read, then one open of each
matched store's index.

⚠ Those numbers are what they are only because of a defect this work found
and fixed: `Extractor::new` compiled FIVE regexes and a records read built
one per store, so a fleet poll was compiling thousands of regexes a second.
Measured before and after — `query --records` over 5,000 stores **10.06 s →
0.11 s**, the 10,000-store poll above **9.55 s → 0.15 s**. It was never a
forwarding problem: every multi-store `--records` answer paid it, timbersh's
fleet reads included.

**A store index is therefore not the next thing.** Three cheaper levers come
first, in this order.

1. **Resolve on its own cadence.** The store set changes when a store is
   created; the data arrives continuously. Re-resolving every poll spends the
   scan at the read's rate for no reason — a new store starting to forward
   thirty seconds late costs nothing, its data being retained meanwhile. That
   turns 0.07 s per second into 0.07 s per thirty.
2. **Skip a store that cannot have anything new.** After (1) the read is the
   whole cost, and a store whose index and write-ahead segment have not grown
   since the last poll has nothing to give. A stat is cheaper than an open,
   but this one has to be got exactly right — the failure mode is silently
   skipping data — so it wants its own change with its own tests.
3. **Bound the selection.** A forwarder matching five thousand stores is a
   decision; `create --dry-run` reporting the count is what makes it one.

An index earns its place only when a NARROW selector must stop paying for the
whole forest — today `service=nope` over 10,000 stores costs 0.06 s and reads
nothing. ⚠ And the shape it must not take is a shared derived FILE somebody
maintains: its failure mode is a store silently invisible to forwarding,
which is the worst direction available. A process-local cache keyed on each
store directory's mtime has no such failure — a missed invalidation is a
stale label, and the tmp+rename `set` performs is INSIDE the store's
directory, so that mtime does move (unlike the forest's own, which is the
trap the follower registry's tick already refuses to fall into). What stays
true either way is receiving-end.md's sentence: an index is an implementation
detail behind "where is this store", not a change to the model.

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

## The interest axis is handed the store

The axis used to be given a store's ANCHOR and compare it to each retaining
follower's; it is now given the store's FIELDS — its labels, its name, its
identity — and matches each follower's selector against them. Everything else
about it is unchanged: it reads the registry and nothing else, an unreadable
declaration (now: or an unparsable selector) fails closed for every store,
and the floor is the minimum over the retaining followers that cover this
store, with «has never read it» holding all of it.

Handed IN rather than fetched, and the caller is already the right place: the
tick's `LivePolicy` stats the manifest once a second and re-reads only when
it changed, so the fields ride the reparse the policy already pays for. A
label change IS a manifest change, and so is the mint that gives a store an
identity, so the one stat catches every way the answer can move.

⚠ **No transactional reading is attempted, deliberately.** A selector is
matched against labels that were true at some moment, and a bounded staleness
is indistinguishable from the label having been changed a moment later. Nor
is there anything to guarantee on the other side: labels are mutable and
followers depend on them, so relabelling a store can always cost data. A
design that tried to prevent that would buy a limitation and no safety —
which is also what leaves the *implementation* of "get this store's labels"
free to become a cache or an index later, behind the same question.

Positions carry the floor, so nothing here converts an offset, and a
`cursor.json` still answers for the one store a legacy declaration named.

## Open

Nothing blocking. What is left is the operator surface: `follower create
--select`, `run` exec'ing the selection shipper with `--positions` and
`--start`, and `list`/`status` rendering a set rather than a store.
