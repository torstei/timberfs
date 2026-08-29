# view: reading a store as a tape, not as a result set

**Status: the timberfs side is DONE; the viewer is NOT BUILT.**
`window.from_chunk` seeks a bounded read, which was the whole protocol
change — what is left is the command in timbersh. Everything else it rests
on was already true: a chunk is addressable by number, carries its ring,
and ships compressed; an entry record carries the chunk AND the offset it
sits at; and a search already returns both.

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

A coordinate that gets passed between components wants a written form.
That is [the address](#the-address) below, and it is the same value
throughout: a search returns addresses, opening one is opening an
address, and handing a place back to the shell is handing it an address.
Being pasteable into a ticket is the free consequence, not the reason.

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

## Two front ends, one seam

The viewer is not a mode of the shell. It has a second use with no fleet
and no shell in it:

```sh
timberfs query --records --from 13:00 --has ERROR app.log | timberview
timberview /var/log/timberfs/app/app.log
```

An entry-aware pager for a `records` stream — multi-line entries as
units, `ts`/`wf`/`offset` per entry, indexable tokens highlighted — is
useful to anyone piping timberfs output. That, rather than "usable on the
box", is what makes it a tool. A shell on the host is the wrong thing to
reach for even where someone has one, and plenty of the people who should
be reading these logs do not — so the viewer runs on a workstation and
needs the shell's transport config either way.

So: a module with its own entry point, in `timberfs-sh`, and timbersh
calls it **in process**.

⚠ Not a separate process the shell execs. The loop is the whole point and
it would cross the boundary badly: the viewer would exit carrying "the
user asked to search F8854…", the shell would search, print, and
re-invoke at a coordinate — a leave-alt-screen, flicker, re-enter per
hop, which is the opposite of what a viewer is for.

⚠ And it does not get its own selector, document builder or records
parser. `tools/README.md` already says the selector is never
reimplemented; a second copy living in a second command is exactly the
drift this repo has spent several releases removing. Shared module, or it
is not worth doing.

The viewer needs four things from whoever hosts it, and nothing else:

```
chunk(store, seq)   -> lines + ring
bounds(store)       -> first_seq, last_seq, dropped bytes
search(token)       -> addresses
stores()            -> the list, for :n
```

Written against those from the first line, two things follow: it is
testable against a fake, the way `tests/timbersh/` already tests timbersh
against a scripted server; and the standalone entry point is a second
implementation of the same four — a local store, or a buffered `records`
stream on stdin. Cheap to decide now, expensive to retrofit.

⚠ The fleet search stays the shell's. Fan-out is not timberfs's job and
it is not a pager's: the viewer asks for `search(token)` and does not
know there are ten hosts.

## The address

A coordinate is passed from a search to a viewer, from one viewer to
another, and back to the shell. Give it a written form and all three are
the same operation:

```
timber://imap01/79d7f23a-b044-4a72-8be3-d26e0481d202#offset=33724753900
timber://imap01/79d7f23a-…#chunk=498248
timber:79d7f23a-…                       -- the store, no position, no host
```

Three rules decide the shape.

**Identity, not location.** The store id is the name; the host is a HINT
a resolver may confirm, override, or not need. Bake the host into the
address and every pasted link breaks when a store moves, and there is
nothing left for a resolver to resolve — which is the whole point of the
next section. Same rule the query document already follows: a document
names stores by identity, never by path.

**A position says which coordinate it is.** `#offset=` and `#chunk=` are
different numbers, and `#at=<time>` is a third thing again. A bare number
would be ambiguous, and an address that resolves to the wrong place
silently is worse than one that will not parse.

**The full id, in a written address.** timbersh expands a short id
git-style when you type one and refuses an ambiguous one — right for a
prompt, wrong for a link, where the ambiguity would be discovered by
whoever pasted it somewhere else.

An address that has rotted says which way it rotted, which is the reason
to prefer an offset over a timestamp: the offset is absolute on the tape,
so it stays valid until retention drops the data and then `first_seq` and
`dropped_bytes` say it was dropped rather than "not found".

⚠ An address is a PLACE, not a search. The document stays the way a
search is written down (`--dump-json` already emits one); growing query
syntax into the address would be a second query language, in the address
bar. Whether a narrow `timber:?has=<token>` — the one search the viewer
itself generates — is worth the exception is open.

## /etc/hosts, and the DNS that would replace it

`TIMBERFS_CMD` plus `TIMBERFS_HOSTS` is **/etc/hosts for timberfs**: a
hand-maintained map from a name to how to reach it, which works, does not
scale, and has to be right before anything runs. Resolving a store today
means asking every configured host for its store list and matching the
id — a broadcast, which is exactly what /etc/hosts leaves you with.

Something more like **DNS** already exists in one place. At Visena,
`visena-timberfs hosts` derives the queryable set from service discovery
— the ZooKeeper app registrations, each probed for whether the janitor2
actually running there serves the query endpoint — and `visena-timberfs
sh` execs timbersh with `TIMBERFS_HOSTS` already filled in. One command
gets a fleet-wide shell with no list to maintain. That is a resolver, and
it is derived rather than configured.

The generalisation is a hook, not a feature: a `TIMBERFS_RESOLVER`
command, the same shape as `TIMBERFS_CMD`, asked "who has this store" or
"what is the fleet" and answering with hosts and how to reach them. It
keeps discovery out of timberfs, where it does not belong — timberfs is a
single-node tool and the fan-out has always lived in the client.

The reason to define the hook early is that the cheap implementations are
useful the day it exists, and they are not the same programs:

- `ssh <host> timberfs list --json` over a list of hosts — a resolver in
  one line, and the honest floor.
- **A static directory**: a file of stores, the hosts holding them, and
  the command that reaches each. Which is /etc/hosts again, except as a
  FILE that can be generated, reviewed, checked in and shared, rather
  than an environment variable each person assembles. That alone covers
  most of what a small fleet needs.
- Service discovery, as `visena-timberfs hosts` already does.
- Anything else: a registry, an inventory, a hosts file per environment.

⚠ And it is queried by more than one tool. The shell, the viewer, and
whatever comes next all ask the same question — "where is this store" —
and none of them should each grow their own answer. That is the same rule
the selector already has, one layer down.

⚠ What stays deferred is timberfs knowing any of this. Broadcast
resolution is fine at the measured fleet (8 queryable of 30) and needs no
hook at all, so nothing has to wait. What matters today is only that the
address carries **identity**, so adding a resolver later changes how a
name is looked up and not what the name is.

## The protocol change: one refusal — DONE

`from_chunk` is a chunk number, exact where a timestamp is not. It used to
be refused on a bounded read:

> a chunk number is a resume position, and only a FOLLOWING read moves
> forward from one — a windowed query selects by the timestamps the lines
> carry

That was written when resuming a follow was the only reason to name a
chunk. Random access is a second reason: `from_chunk: N` with
`max: {chunks: 1}` and `kind: "chunks"` is a seek, and a pager is nothing
but seeks.

The refusal did not disappear, it got smaller and truer. A chunk number is
a PLACE, so it composes with everything that is not a place — the window's
far end, the predicates, either axis — and conflicts only with a second
START. `from`, a `tail` and a `cursor` each name one, and each was being
silently preferred over `from_chunk` where the two met; those three are now
refused instead of quietly resolved, which is the rule stated once rather
than three accidents.

Two things fell out of doing it. The seek lives in `select_chunks`, the one
function every bounded read reaches its chunks through, so no read path can
implement the member and no other path can forget it. And `stream-start`
echoes `from_chunk`, because an answer outlives the request that produced
it: one recording `from`/`to`/`has` while dropping the position it began at
describes a search nobody ran.

Nothing else was needed. A search hit already carries `chunk` and `offset`,
so seek-to-hit is a seek to a number the answer gave. `at '12:00'` is a
write-axis window with `max: {chunks: 1}`, which worked already.

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
- **Whether an address may carry a search.** `timber:?has=<token>` is the
  one query the viewer generates on its own, and pasting "this identifier,
  fleet-wide" is the thing an incident wants to share. It is also the
  first millimetre of a query language in an address.
- **Scheme name.** `timber:` beside `timber-filter`/`timber-otlp`, or
  `timberfs:` for the product. Cheap now, a compatibility problem later.
