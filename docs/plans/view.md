# view: reading a store as a tape, not as a result set

**Status: a first version SHIPPED.** The viewer is `tools/timberview.py`,
reached two ways — `view` inside timbersh, and `timberview(1)` beside it —
and [tools/README.md](../../tools/README.md) describes what it does. What
stays here is what is still open: the second front end, the resolver the
address is shaped for, and the questions the first version answered one
way and could answer another.

See [paging.md](paging.md) for walking a result set — this is the other
motion, and the difference is the point.

## What the first version settled

- **A chunk number is a place, not a resume position.** `from_chunk` with
  `max: {chunks: 1}` and `kind: "chunks"` is a seek, and a pager is
  nothing but seeks. That was the whole protocol change; everything else
  the loop needs was already true.
- **Four operations, and no fifth.** `stores`, `bounds`, `chunk`,
  `search`. Written against those, the viewer is tested against a fake
  rather than a terminal, and the fan-out stays the shell's.
- **A coordinate has a written form**, `timber://host/id#offset=N`:
  identity with the host as a hint, and a position that says which
  coordinate it is. Being pasteable into a ticket is the free
  consequence, not the reason.
- **Picking a token is a line-level pick with a vi-ish motion.** `Tab`
  moves between the searchable tokens of the line, which are exactly the
  runs `--has` matches — so the two candidate designs turned out to be
  one mechanism, and a 200-character log line does not need a character
  cursor to get across.
- **A fleet search returns to a list**, with `n`/`N` cycling from
  wherever you land. An identifier on six hosts is six answers, and
  jumping straight to one would pick for you.

## Not built: the records-stream front end

```sh
timberfs query --records --from 13:00 --has ERROR app.log | timberview
```

An entry-aware pager for a `records` stream — multi-line entries as
units, `ts`/`wf`/`offset` per entry — is useful to anyone piping timberfs
output, and it is a second implementation of the same four operations
over a buffered stream rather than a store. The seam is there; the
backend is not.

⚠ It is the one that makes the viewer a *tool* rather than a mode: the
store backend it ships with is the same one either front end uses, so
today's `timberview app.log` proves the seam holds but not that a second
kind of source fits behind it.

## Not built: aligning several hosts on one moment

Every position has a write window, so "show me http01 *here*" is the
chunk covering that instant:

```json
{"window": {"axis": "write", "from": WF, "to": WF},
 "max": {"chunks": 1}, "response_format": {"kind": "chunks"}}
```

The viewer can already open one store that way (`at '12:00'`, and
`#at=` in an address). What it cannot do is hold several open at once,
which is what an incident actually needs. That is a layout question
before it is a protocol one, and nothing in the wire has to change.

## /etc/hosts, and the DNS that would replace it

`TIMBERFS_CMD` plus `TIMBERFS_HOSTS` is **/etc/hosts for timberfs**: a
hand-maintained map from a name to how to reach it, which works, does not
scale, and has to be right before anything runs. Resolving a store today
means asking every configured host for its store list and matching the
id — a broadcast, which is exactly what /etc/hosts leaves you with.

Something more like **DNS** already exists in one place: a site-specific
wrapper around timbersh, in use on one fleet. It derives the queryable
set from service discovery — the ZooKeeper registrations, each probed for
whether the agent actually running there serves the query endpoint — and
execs timbersh with `TIMBERFS_HOSTS` already filled in. One command gets
a fleet-wide shell with no list to maintain. That is a resolver, and it
is derived rather than configured.

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
- Service discovery, as the wrapper above already does.
- Anything else: a registry, an inventory, a hosts file per environment.

⚠ And it is queried by more than one tool. The shell, the viewer, and
whatever comes next all ask the same question — "where is this store" —
and none of them should each grow their own answer. That is the same rule
the selector already has, one layer down.

⚠ What stays deferred is timberfs knowing any of this. Broadcast
resolution is fine at the measured fleet (8 queryable of 30) and needs no
hook at all, so nothing has to wait. What matters is only that the
address carries **identity**, which it now does: adding a resolver later
changes how a name is looked up and not what the name is.

## Open questions

- **Whether an address may carry a search.** `timber:?has=<token>` is the
  one query the viewer generates on its own, and pasting "this identifier,
  fleet-wide" is the thing an incident wants to share. It is also the
  first millimetre of a query language in an address, which is why the
  first version has no such form: an address is a PLACE, and the document
  is how a search is written down.
- **What an unresolvable address should do.** Today an id no host holds is
  refused with the count of stores that were asked. With a resolver it
  becomes a lookup failure, which is a different sentence and possibly a
  different exit code.
- **Whether `search` should be bounded by where you are looking.** It is
  deliberately not: the loop starts with an identifier and no idea which
  log holds it, so the predicate you opened with must not narrow it. But
  on a large fleet an unscoped `--has` for a common token is a lot of
  reading, and the viewer has no way to say "this store, this hour" yet.
- **Follow.** Not in scope: the live edge is not in a chunk, and `select
  … --follow` already tails it. The viewer only says it is not showing
  it — which is what the bottom marker does today, from the writer's
  presence rather than from a flush age the store object does not carry.
