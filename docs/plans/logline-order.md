# Ordering by the clock an entry carries

**Status: NOT BUILT, and it rests on an index field that does not exist.**
A bounded read drains each store before the next and claims no order
between stores (`order=sequential`, `timberfs-records(5)`). This note
records what it would take to answer a multi-store search in **logline**
order, why the obvious version cannot work, and the one addition that
makes it fall out.

## Why there is no cross-store timeline today

Timberfs has two clocks. An entry has a WRITE time — when it reached the
store — and a LOGLINE time, the stamp it carries. They diverge by however
long the producer buffered, and an import puts an entry on disk years
after the moment it describes.

A streaming merge can only order on a key its inputs are ALREADY sorted
by: to emit the next-smallest without holding everything, each source
must hand them over in that order. A store is appended to, so write time
is such a key. The logline is not, not even within one store.

That is the whole constraint, and it is the same one a query planner
meets: a partitioned table streams in order through `MergeAppend` when
every partition is sorted on the key, and otherwise needs a `Sort`, which
cannot emit its first row until it has consumed its last. Sorting a
stream by a key it is not sorted on means holding all of it.

So the two honest positions are: order by arrival (free, and not the
timeline it resembles), or sort in the consumer over a page it has whole.

## The third position

Sorting is only blocking when the disorder is UNBOUNDED. Logline disorder
is not — a producer buffers for seconds, not for days — so a merge with a
frontier works:

1. resolve the store predicate; for each store, find the first and last
   chunk in the window. The working set is those cut pieces.
2. collect `(store, chunk, first, last)` for every chunk in it and sort by
   `first`.
3. walk that list. A chunk whose range overlaps no other chunk can be
   written straight through — nothing can interleave into it. Only
   overlapping ranges are buffered and sorted.

Everything below `min(first)` over the chunks not yet opened is safe to
emit. Memory is proportional to the OVERLAP, not to the data: typically
about one chunk per store.

## What it needs, and why that is worth doing anyway

**A per-chunk logline range in the index** — `first_logline_ms` /
`last_logline_ms` beside the write times in `ChunkRecord`.

Step 3 cannot use the write window. An entry in a chunk written 10:00 to
10:05 may carry a stamp of 08:00, so "this chunk overlaps nothing" says
nothing about the loglines inside it, and the merge would silently emit
out of order.

The field pays for itself before any merge exists. Selecting by a logline
window is APPROXIMATE today: the write-time selection is widened by
`WIDEN_MS` (60 s) because the write window is a proxy for the logline
window and nobody knows the real bound. A recorded range replaces the
guess with the answer — an exact selection, and one that cannot be too
narrow.

Costs to weigh when it is built: it is written on flush, so it must come
from entries the writer has already parsed; a store whose lines carry no
parseable stamp has no range and must say so rather than report zero; and
the range must survive `remove_head` and verbatim frame appends, where a
destination renumbers chunks but must not restamp them.

## Isolation, not correctness

This mode PINS its working set: the answer covers exactly the chunks
selected at step 1, so a store that appears while it runs is not in it.

That is the same bargain SQL paging makes without a snapshot, and worth
naming as an isolation level rather than treated as a flaw:

| | |
|---|---|
| bounded paged read | no snapshot. A store appearing between pages is read from the start of the window, and its old entries arrive after newer ones. Accepted. |
| this mode | a snapshot of the working set. Complete and ordered for exactly what it covers. |
| `--follow` | the opposite requirement — a store appearing IS the point, and must join the stream. |

Our positions are keyset pagination, which is stable under appends and
not under inserts BEFORE the cursor; a new store's old entries are such
an insert. No cursor scheme fixes that, in SQL or here. The choice is
which level to offer, and to say which one an answer was.

## What it does not change

`--follow` cannot use any of this: sequential needs a first stream that
ends, and a frontier needs a last chunk. It keeps `order=arrival`, and
gets away with it — everything is arriving now, so the spread across
stores is one poll interval.

Nor does it replace the sequential read. It stalls on a full index sweep
before the first byte, which the sparse case does not notice (the sweep
IS the query) and a wide one does.
