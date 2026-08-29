# Paging: a cursor beside the search, not inside it

**Status: the position, the cursor and the deadline are BUILT; the
service-imposed limits are not.** A `position` record per examined
store says where each got to, and handing those back as the request's
`cursor` resumes exactly there. What follows is the reasoning, and what
remains. The pieces it rested on were already there: a store carries an id,
`Cursor { seq, n }` means "chunk, and entries delivered from it" and resolves
against retention, the records stream emits one `source` record per examined
store, and `stream-end` says whether a bound stopped the read.

See [the query document](../../packaging/timberfs-query-document.5) for the
request this extends, and `timberfs-records(5)` for the response.

## What a client can do today

Page. A `position` record per examined store carries an absolute offset on
that store's tape; `cursor` in the request hands them back. Six entries
sharing one timestamp are six distinct positions, which is the case that
made the timestamp approach useless.

Bound the wait, with `deadline: {"ms": 5000}`. A fleet read is slow because
it READS a lot, not because it matches a lot, so no count bounds it; and
unlike a timeout in the caller — which drops the connection and everything
already on it — a deadline is answered. Stores read to the end are complete,
the one it stopped inside carries a position, a store selected but never
opened has `chunks_read=0`, and one it never reached at all has no `source`
record while `stream-start` still counts it.

What is still missing are the **service-imposed limits** a query server
will want to declare — its own ceilings on what a request may ask for,
announced rather than discovered by having a request refused.

## What a client could do before that (kept, because it is the argument)

Nothing. `max` truncates, `stream-end` says `status=limited`, and there the
answer stops. None of what is there is a resume position:

- `window.from_chunk` was for a FOLLOWING read and refused on a windowed one
- a timestamp separates neither two entries inside a chunk nor two chunks that
  share a boundary millisecond
- an entry's `chunk=` names the chunk it sits in, not where in it, so resuming
  there re-delivers everything before it

So a bound is "show me some", never "walk this result set".

`from_chunk` has since been relaxed onto a bounded read — see
[view.md](view.md) — and none of that makes it a cursor. It is chunk-granular
where a position is byte-exact, so resuming there still re-delivers the whole
chunk you were inside; and it is a place in the STORE where a cursor is a place
in an ANSWER, which is the distinction the two handles exist to keep. A read has
one start, so naming both is refused rather than resolved. Page with the cursor;
seek with the chunk number.

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
              { "id": "S3", "chunk": 3,  "n": 50 } ] }
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
decompressed. The position is still the end of what existed — the search
COVERED those chunks, it just did not have to read them. Recording that costs
nothing and saves the whole scan next time.

Consequence to decide deliberately: **the cursor is O(stores examined)**. Five
is nothing; five thousand is five thousand entries on every page. A watermark
was sketched here on the grounds that the merge was time-ordered, so a stop
left every store's next chunk at or after one frontier time. A bounded read
no longer interleaves — it drains each store before the next — so there is no
such frontier and the sketch does not apply. What sequential gives instead is
that the positions COMPRESS by shape: every store before the one it stopped in
is at its end, every store after it is untouched, and only one is part-way.
Whether that is worth encoding is open; see
[logline-order.md](logline-order.md) for the mode that would restore a real
frontier, and what it needs first.

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

## Every entry carries a position; there is no `done`

S3 produced no hits, so it is tempting to record it as finished. That is wrong,
and not only in the subtle two-clocks way: **something can append to S3 between
the two pages.** A store with nothing to say at 12:05 may have plenty by 12:06,
and a consumer that trusts `done` skips it for the rest of the walk.

So a cursor entry is uniformly `{ id, chunk, n }`. A store scanned to the end
records the position it reached — the last chunk that existed, fully consumed —
and the next page resumes at the chunk after it, which is either new data or
nothing. `Cursor` already behaves this way: `n` is the delivered prefix of the
chunk it stands in, `advance` restarts `n` on a new chunk, and `resolve` clamps
to the ends of the store.

That leaves no separate "finished" state to represent, to go stale, or to be
believed. Exhaustion is not a fact about a store; it is a fact about a store AT
A MOMENT, and a position is how you say that.

WRITE-axis exhaustion really is permanent — write time only moves forward, so a
chunk written after `to` can never enter the window. But the position already
expresses it, and chunk selection already skips chunks whose write window falls
outside the range, so a flag would save nothing and could still be believed on
the wrong axis.

## The position is an absolute byte offset on the tape

Torstein's, and better than the two coordinates that preceded it here.
`chunks-by-address.md` already models a store as an endless TAPE with a
beginning; a position is then simply a point on it:

```
absolute_offset = dropped.uncomp_bytes + chunk.uncomp_start + bytes into the chunk
```

**One integer per store**, not a pair — comparable, orderable, and
subtractable, so progress falls out of the position itself. With the store
id it is globally unique: `(store_id, offset)` addresses every byte the
store has ever held. A timberfs URL is a short step from there, and it
would be a permanent citation for a log line — stable across moves,
retention and replication, since a replica keeps the bytes AND the
numbering (a COPY receiver renumbers, but it declares a new identity, so
the address stays sound).

Resolution is a binary search: subtract `dropped.uncomp_bytes` for the
file-relative offset, then search `uncomp_start`, which is monotonic.

### Why retention does not threaten it

`dropped.uncomp_bytes` is a FLOOR — it covers only the drops a
byte-recording binary performed — so on a store head-dropped before those
counters existed, absolute offsets are understated. **By a constant,
forever**, which is harmless:

```
legacy: an old binary drops chunks 0-2 (3000 B, unrecorded)
        dropped=0, chunk5.uncomp_start=2000  ->  absolute = 2000
a recording binary then drops 3-4 (2000 B)
        dropped=2000, chunk5.uncomp_start=0  ->  absolute = 2000   (unchanged)
```

Every later drop is recorded and compensates exactly. Positions stay
unique, monotonic and stable; all that is lost is the claim that offset 0
is that store's first ever byte, which nothing needs.

⚠ The offset must be in the STORE's space, not the file's.
`remove_head` rebases `uncomp_start` (`c.uncomp_start -= uncomp_cut`), so
a file-relative offset moves under retention. Adding `dropped` is what
makes it absolute.

### It dissolves the straddling entry

An entry beginning in chunk 10 and ending in chunk 11 has ONE address —
the byte its first line starts at. Resuming there reads its head from 10
and completes it in 11, with no special case and no entry counting. Which
is also why a bounded read must DISCARD a cut-off entry rather than emit
it: the position names the entry's start, so the next page delivers it
whole, and emitting a fragment now would deliver it twice, once wrong.

⚠ `Cursor { seq, n }` — every follower's position — is entries-into-chunk,
a different representation of the same idea. Whether followers move to
this address is deliberately left open: there may be a use for more than
one kind of cursor, and paging does not have to settle it.

## What `chunk` and `entries` are still for

Progress, not position. "Chunk 400 of 9000" reads to a human where a byte
count does not, and both numbers come from the index without decompressing
anything — which is what lets a search that has found NOTHING still report
that it is advancing.

So the two are nested apart, and a client cannot resume from the wrong
one by mistake:

```json
{ "id": "S1",
  "position": { "offset": 40810234 },
  "progress": { "chunks_read": 400, "chunks_total": 9000,
                "bytes_read": 41943040, "bytes_total": 912345678 } }
```

⚠ `progress.bytes_total` is bytes SELECTED for this search, not the
store's size — the fraction is against the work this query will do. It
therefore changes if the same cursor is replayed with a different
predicate, which is fine, because progress decides nothing.

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

**Both, and the caller's shape decides which.** A selection is a PREDICATE, not
a resolved list — which is what lets a store that appears mid-walk be found at
all. So:

- **A tail** re-resolves the predicate each poll, and an absent store is NEW:
  start from the beginning of what it holds. For a genuinely new store that IS
  "from now", since everything it holds arrived after the tail began. This is
  the case that matters operationally — containers come and go, and a tail that
  cannot pick up one started five minutes ago is a tail you have to keep
  restarting.
- **A bounded walk** wants the store set pinned, because pages that stop
  concatenating have stopped being a result set.

The difference is not a per-request flag to invent: it is `follow` versus a
bound, which the request already distinguishes.

### 3. Do later pages stay in logline order?

No — and this is a property to state rather than a decision to make. It is what
carrying positions instead of `done` buys.

Resuming S3 after its recorded position reads whatever arrived since. On the
logline axis those chunks can hold an entry stamped 14:00 that arrived in a
chunk written at 15:30, so a match inside the original window can surface on
page 3 that logically belongs beside page 1. Declaring S3 finished would not
have found it at all, so this is the better failure.

What stays open is only whether a walk is a snapshot: pinning the store set
(decision 1) also pins how much of this a caller sees. A client needing logline
order across pages has to sort the union itself, which it can — every entry
carries its own timestamp.

## What to build first

1. `id=` on the `source` record — useful alone, and the join key everything
   else needs.
2. A closing per-store record carrying id and the position reached — for every
   store examined, including the ones that matched nothing.
3. `cursor` in the request, with `scope: "per_store"` required on the bound.

Each is additive within `v=1`. None of it is urgent: `status=limited` already
keeps a client from mistaking a truncated answer for a complete one, which is
the failure that actually costs someone something.
