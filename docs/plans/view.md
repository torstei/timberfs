# view: reading a store as a tape, not as a result set

**Status: NOT BUILT.** One timberfs change (a refusal to relax), and a
command in timbersh. Everything it rests on is already true: a chunk is
addressable by number, carries its ring, and ships compressed; an entry
record carries the chunk AND the offset it sits at; and a search already
returns both.

See [paging.md](paging.md) for walking a result set — this is the other
motion, and the difference is the point.

## What it is

`view` opens a store the way a pager opens a file: the last chunk by
default, scroll back through it, no predicate and no result set.

```sql
view [id=79d7f23a…];          -- the last chunk, and back from there
view [] at '12:00';           -- anchored at a time
view [] at chunk 412000;      -- or at a position
```

`select` walks a **result set**; `view` walks the **log**. They share the
coordinate system — the absolute tape offset — and nothing else. That is
why the handle here is a chunk number rather than a cursor: a cursor is a
place in an answer, and there is no answer to be in.

## Lines, not entries

`view` does no entry parsing at all: no timestamp extraction, no window
verification, no `.grain`. It is chunks, decompressed, shown as lines.

That is not a simplification, it is the feature. The two cases where
`select` is least useful — a store whose lines timberfs cannot parse, and
one with no index — are exactly the cases where you most want to just
look at the log. `view` works on both, because it asks nothing of the
content.

## The loop this exists for

```
see F885495664326FQYNTW in imap01
  → search it across every host
  → hits carry host, store, chunk, offset
  → open that store, at that offset
```

The last step is the one that was impossible until recently. A hit could
name the store that contained an identifier, and locate it no better than
"somewhere in chunk N" — 77 KiB of somewhere. An `entry` record now
carries `offset` beside `chunk`, so a hit is a **coordinate**, and
jumping to it is an ordinary seek.

This is why `view` should not be `less`. Handing a temp file to a pager
gets scrolling and search for free, and gives up every part of the loop:
there is nowhere to put a fleet search, no way to show a position that
means anything (a pager's `%` is a percentage of the temp file), and no
way to come back with a coordinate and act on it.

### The selectable tokens are the searchable ones

`--has` matches whole ASCII-alphanumeric runs of 3–64 characters, exact
case. So the viewer highlights **exactly those** and tabs between them:
what can be selected is what can be searched, by construction rather than
by a UI guess. It also means a token the index cannot hold — `26.1.18` —
is refused where you point at it, with the reason, instead of being
discovered later as an empty answer.

### Cross-store by time

Every position has a write window, so "show me http01 *here*" is the
chunk covering that instant:

```json
{"window": {"axis": "write", "from": WF, "to": WF},
 "max": {"chunks": 1}, "response_format": {"kind": "chunks"}}
```

Aligning several hosts on one moment is what an incident actually needs,
and it falls out of the viewer knowing where it is.

## The protocol change: one refusal

`from_chunk` is a chunk number, exact where a timestamp is not. It is
refused on a bounded read:

> a chunk number is a resume position, and only a FOLLOWING read moves
> forward from one — a windowed query selects by the timestamps the lines
> carry

That was written when resuming a follow was the only reason to name a
chunk. Random access is a second reason: `from_chunk: N` with
`max: {chunks: 1}` and `kind: "chunks"` is a seek, and a pager is nothing
but seeks. Relaxing the refusal is the entire timberfs side.

Nothing else is needed. A search hit already carries `chunk` and
`offset`, so seek-to-hit is a seek to a number the answer gave. `at
'12:00'` is a write-axis window with `max: {chunks: 1}`, which works
today.

## Why the client fetches chunks

Measured on `visena-imap-email-server` at imap01 — 436,939 chunks,
33.7 GB logical, 2.6 GB compressed:

```
1 chunk  ≈  77 KiB of log  ≈  600–800 lines  ≈  10 screenfuls
                            for ~6 KiB on the wire
```

One round trip buys ten screenfuls, and paging inside it is local and
instant. Fetching `N±1` at the edges makes scrolling unbounded — there is
no window to fall out of, which is what lets "the last chunk by default"
be the whole rule rather than the start of one.

The client therefore decompresses. `compression.zstd` is stdlib from
Python 3.14; `zstd -dc` accepts concatenated frames, so one subprocess
decodes a whole run and the fallback is a few lines. Asking the server to
decompress instead costs 13× the bytes for nothing.

## Long lines are a preference, not a policy

Sometimes you want a log line wrapped and sometimes you want the columns
to line up, and it changes with what you are doing rather than with the
store. So: **both, toggled at runtime**, and in non-wrapping mode the
view scrolls sideways. Neither is a default that can be argued into being
correct — a viewer that only wraps makes a wide structured line
unreadable, and one that only truncates hides the end of every stack
frame.

## Boundaries are stated, never discovered

Retention moves the floor while you read, and the newest lines are not in
any chunk yet. Both are facts the viewer holds — `first_seq`,
`dropped_chunks`, `dropped_uncompressed_bytes` from the store object, and
the writer's flush age — so both are said:

```
── visena-imap-email-server @ imap01 · chunk 498248 · offset 33 724 753 900 · 99%
── 436,888 chunks older; head is 61310 (2.9 GB dropped)
── end of chunk 498248 · newer lines may be unflushed (flush-age 5s)
```

Scrolling to the top must distinguish *the top of the log* from *the top
of what is loaded*, and the bottom must say that the live edge is not
shown. An empty screen that could mean either is the same defect this
project keeps removing from its answers.

## Open questions

- **Picking a token.** A vi-ish cursor with `w`/`b` and `*` on the word
  under it, or a line-level pick (highlight the line's tokens, choose one)?
  The first is what a vim user reaches for; the second is faster on a
  200-character line, which log lines are.
- **What a fleet search returns to.** A result list to choose from, or
  jump straight to the first hit and cycle with `n`? A list is honest
  when an identifier appears on six hosts; jumping is faster when it does
  not.
- **Control bytes.** Log lines contain ANSI escapes and stray `\r`.
  Sanitising on display is required for a raw-mode screen, not polish.
- **Follow.** Not in scope: the live edge is not in a chunk, and `select
  … --follow` already tails it. The viewer only has to *say* it is not
  showing it.
