# Paging: a cursor beside the search, not inside it

**Status: design, nothing built.** The pieces it rests on are real: a store
carries an id, `Cursor { seq, n }` already means "chunk, and entries delivered
from it" and already resolves against retention, the records stream already
emits one `source` record per examined store, and `stream-end` already says
whether a cap stopped the read. What is missing is a position in the response
and a way to hand one back.

See [the query document](../../packaging/timberfs-query-document.5) for the
request this extends, and `timberfs-records(5)` for the response.

## What a client can do today

Nothing. `max` truncates, `stream-end` says `status=limited`, and there the
answer stops. None of what is there is a resume position:

- `window.from_chunk` is for a FOLLOWING read and is refused on a windowed one
- a timestamp separates neither two entries inside a chunk nor two chunks that
  share a boundary millisecond
- an entry's `chunk=` names the chunk it sits in, not where in it, so resuming
  there re-delivers everything before it

So a bound is "show me some", never "walk this result set". That is stated in
the man page so nobody builds paging on `from_chunk`.

## The cursor is a separate object

The search is a value: a predicate, a window, a format. WHERE YOU ARE in it is
not part of it — the same search paged twice differs only in position. So the
cursor is its own member, opaque to the caller, produced by one response and
handed to the next request:

```json
{ "v": "...", "stores": {...}, "window": {...}, "match": {...},
  "max": { "entries": 100 },
  "cursor": [ { "id": "S1", "chunk": 10, "n": 20 },
              { "id": "S2", "chunk": 20, "n": 40 },
              { "id": "S3", "done": true } ] }
```

Keeping it out of the search is what makes the search reusable, and it is also
what makes a live multi-store search fall out: a cursor plus `follow` is
"resume here and keep going", which is `tail -f | grep` over a whole forest as
ONE request rather than N pipelines. The follower registry already does this
for one store with a durable position; this is the same thing addressed by a
predicate.

## The cursor covers every store EXAMINED, not every store with hits

The requirement that is easy to miss. A search over S1..S5 with hits only in S1
and S2: a cursor holding just those two leaves S3, S4 and S5 with no position,
so the next page re-scans all three from the start of the window, correctly
finds nothing again, and pays the whole cost to do it. On a fleet where most
stores never contain the term, that is nearly the entire query, repeated per
page.

So the cursor is HOW FAR THE SEARCH GOT, per store — not where the last hit
was. Those two differ exactly for the stores that produced nothing, which is
usually most of them.

The response already reports a store it examined and found nothing in:

```
source|path=/var/log/timberfs/m/m.log|kept=0|total=4
```

`kept=0` means the token index proved no chunk could match, so nothing was
decompressed. That store is finished for this window at zero cost, and saying
so in the cursor saves the whole scan next time.

Consequence to decide deliberately: **the cursor is O(stores examined)**. Five
is nothing; five thousand is five thousand entries on every page. Worth
exploring: the merge is time-ordered, so when it stops, every store's next
chunk begins at or after one frontier time — which suggests a watermark plus
explicit positions only for the stores straddling it. Not safe alone (chunk
boundaries sharing a millisecond is the hazard `from_chunk` exists to avoid),
but as watermark-with-exceptions it could collapse the common case to nearly
constant size.

## Two gaps in the response

**`source` names the store by `path=`.** A cursor is keyed by id, and must be:
a path is neither stable nor unique, which is why `stores.select` has no path
member at all. `id=` on `source` is worth adding on its own — it is the join
key between a request and its answer.

**`source` is emitted before the read**, carrying selection stats, so the final
position is not known yet. The position therefore wants a second per-store
record at the end rather than a field on this one. That keeps the early "here
is what I am about to read" signal and is additive, since `timberfs-records(5)`
requires consumers to ignore unknown record kinds as well as unknown keys.

## `n` counts entries, positionally

Not bytes: a chunk decompresses as a unit either way, so a byte offset saves
only the framing pass. An entry count is what `Cursor { seq, n }` already
means and what `resolve`/`advance` already implement against retention.

Not hits: a hit count is only valid for the predicate that produced it, so
changing the predicate between pages would resume somewhere meaningless.
Positional `n` plus re-applying the predicate is self-correcting.

## Three decisions still open

### 1. Is the bound per store or total?

The schema reserved `scope` for this and said it would wait for a second thing
to choose between. Paging is that second thing.

**Per store composes.** Each store's cursor is independent and self-contained.
A store that starts matching on page 3 gets its own budget and its own cursor;
a store retained away drops its cursor. Nothing is coupled.

**Total makes the cursor a joint object**, and then the store set becomes part
of the query's identity. `stores.select` is a predicate, re-evaluated per
request, so page 2 can match a different set. A newly-matching store has no
cursor, starts at its beginning, and its early entries interleave BEFORE
things already seen — so pages stop concatenating to the result set, which is
the one property paging exists to provide.

Fixable by pinning the store set by id in the cursor, making it a snapshot.
Coherent, but it means a long walk deliberately ignores a host that started
logging, and a store retained away mid-walk becomes an error rather than a
fact.

⚠ Underneath both: the k-way merge advances WHOLE CHUNKS, picking the source
whose next chunk starts earliest. "The first 10 entries" is already
first-10-in-merge-order, not the 10 earliest by logline time. Deterministic, so
paging works over it — but a total-scoped page boundary lands mid-chunk in
exactly one store and at chunk boundaries in the others.

**Recommendation: ship `scope: "per_store"` only, with `"total"` ABSENT** rather
than present and subtly wrong. Total-scope paging is then its own decision,
where the snapshot semantics get designed on purpose instead of falling out.
Per store is also what a fleet search usually wants — "the next 100 from each
host" — and it is the version that survives the store set changing underneath
it.

### 2. What does an absent store in the cursor mean?

"New, start at the beginning" or "not part of this snapshot"? The first keeps a
walk current and lets results appear out of order; the second keeps the walk
stable and goes stale. It follows decision 1: per-store scope makes "start at
the beginning" harmless, because a new store's budget is its own and its
entries are not claimed to be ordered against another store's.

### 3. Does "done" survive a late entry?

Not necessarily. A store scanned to the end of a 12:00–15:00 window is finished
as it was, but timberfs's two clocks mean an entry stamped 14:00 can arrive in
a chunk written at 15:30. So `done: true` on the logline axis is a statement
about the store at the time it was read.

Either a paging session pins what it saw and is honestly a snapshot, or it
re-examines and stops being a stable walk. On the WRITE axis the question does
not arise: write time only moves forward, so "past the end of the window" is
permanent. That asymmetry is worth stating in whatever ships, because a client
cannot infer it.

## What to build first

1. `id=` on the `source` record — useful alone, and the join key everything
   else needs.
2. A closing per-store record carrying id, position and whether that store is
   finished.
3. `cursor` in the request, with `scope: "per_store"` required on the bound.

Each is additive within `v=1`. None of it is urgent: `status=limited` already
keeps a client from mistaking a truncated answer for a complete one, which is
the failure that actually costs someone something.
