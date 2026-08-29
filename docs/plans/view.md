# view: reading a store as a tape, not as a result set

**Status: a first version SHIPPED, and the fleet resolver with it.** The
viewer is `tools/timberview.py`, reached two ways — `view` inside timbersh,
and `timberview(1)` beside it — and a fleet is a list of targets from
`tools/timberfs_client.py`. [tools/README.md](../../tools/README.md)
describes both. What stays here is what is still open: the second front
end, the "who has this store" half of resolution, and the questions those
versions answered one way and could answer another.

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
- **An answer fits on the log's screen, and that is the second
  backend.** `select ... into view` and a piped `records` stream are the
  same source read two ways. It works because an entry record carries
  `chunk` and `offset`: every line in an answer knows the place it came
  from, which is the same coordinate the tape is addressed by. `Enter`
  there leaves the answer for the log AROUND that entry — the one motion
  an answer cannot make for itself. A multi-line entry is one entry; an
  answer is a closed set; an entry at the live edge is shown and cannot
  be opened.
- **A fleet search returns to a list**, with `n`/`N` cycling from
  wherever you land. An identifier on six hosts is six answers, and
  jumping straight to one would pick for you.
- **How a target is reached belongs to the TARGET.** One command with
  `_TIMBERHOST_` in it made it a property of the session, so a fleet had
  to be uniform. A target is a name and an argv, and the argv is a list
  because a command line written as one string has to be split again at
  the far end under rules we would have had to invent.

## Not built: "what happened in $FOO around here"

The motion a reader actually makes: drilling down, finding something,
and wanting the same moment in a different store. It starts from **where
you are** rather than from a typed time, which is what makes it an
interaction rather than a flag.

The far end is ready — a write-axis window whose ends are the same
millisecond, with `max: {chunks: 1}`, is the chunk covering an instant,
and `at '12:00'` already does it. What is missing is where the instant
comes from, and there it meets the viewer's own rule: **it parses
nothing.** Three ways out, and they are not equally good:

- **The chunk's write window.** Free, already on screen, and coarse: a
  chunk can span minutes, so "here" becomes "somewhere in here".
- **The timestamp as a TERM.** `Tab` now selects `2026-08-29T12:02:30.100`
  whole, and `when()` already resolves that string. Nothing is parsed by
  the viewer — the READER pointed at it — so the rule survives intact,
  and it costs no round trip. It only works where the reader can see a
  timestamp, which is the case that motivates the feature anyway.
- **Ask timberfs.** A `records` read at that offset answers with the
  entry's own `ts`, parsed by the thing whose job that is. Exact, works
  on a line whose timestamp is not selectable, and costs a round trip.

The second is the cheapest thing that is not a lie, and the third is the
one that always works; they are not exclusive.

⚠ The UI half is the unsolved part, and is why this waits. Does the
other store REPLACE the view or sit beside it? Two panes is a different
program from a pager. And which store — a picker, or "the same labels on
another host", which is what an incident usually means? Better answered
after living with the single-store version for a while.

## Not built: taking a selection away

Mark a run of lines and get it out — to a file, or to the clipboard.

- **The unit is lines**, not entries: lines are what the viewer shows,
  and entries would mean parsing.
- **A selection is a RANGE**, so it is two coordinates where an address
  is one. `#offset=A..B` is still a PLACE, so it may belong in the
  address grammar rather than beside it — unlike a search, which does
  not.
- ⚠ **What is exported matters more than where it goes.** The bytes are
  the obvious answer and the weaker one: this project's habit is to hand
  back something that can be RE-RUN. A selection is a start, a bound and
  a store — which is to say a query document, and `--dump-json` already
  renders one. Offering both is cheap; offering only the bytes throws
  away the half that survives being pasted into a ticket.

## Not built: getting the address OUT

`y` shows the address of the line you are on and `q` prints it on the
way out, so it can be read and selected with a mouse. What is missing is
the clipboard — and the case that matters is a workstation viewing a
fleet, which means it has to work over ssh and through a multiplexer.

⚠ OSC 52 does, and its failure mode is **silence**: a terminal that does
not support it does nothing and says nothing, which is the failure this
project keeps removing from its answers. So a copy must either be
confirmable or must keep showing the address, so that the fallback —
selecting it by hand — is always there.

## /etc/hosts, and the DNS that would replace it

**The hosts file half is BUILT.** `TIMBERFS_CMD` plus `TIMBERFS_HOSTS` was
/etc/hosts for timberfs — a hand-maintained map from a name to how to reach
it, which works, does not scale, and has to be right before anything runs.
Worse, it made the transport a property of the SESSION: one command with a
placeholder meant every host had to be reached the same way, so an `ssh`
and a site wrapper taking the host as an argument could not be one fleet.

A **target** is now a name and the argv that reaches it, and a **resolver**
is any command that prints the list. `tools/README.md` has the document and
the order the sources are tried in. `TIMBERFS_CMD`/`TIMBERFS_HOSTS` survive
as one producer of a target list rather than as the only way to describe a
fleet.

The generalisation is a hook rather than a feature, so discovery stays out
of timberfs where it does not belong — timberfs is a single-node tool and
the fan-out has always lived in the client. The cheap implementations were
the reason to define it early, and they are not the same programs:

- `ssh <host> timberfs list --json` over a list of hosts — the honest floor.
- **A static directory**: the same document as a FILE that can be generated,
  reviewed, checked in and shared, rather than an environment variable each
  person assembles. That is what `~/.config/timberfs/targets.json` is, and
  it covers most of what a small fleet needs.
- **Service discovery**, which one site-specific wrapper already does: it
  derives the queryable set from the service registry, each candidate probed
  for whether the agent actually running there serves the query endpoint.
  That wrapper becomes a resolver by printing what it already computes.

⚠ It is queried by more than one tool. The shell, the viewer, and whatever
comes next all ask "where is this store", and none of them should grow its
own answer — the same rule the selector already has, one layer down.

⚠ **Failing to resolve and failing to reach are different failures.** The
resolver is how you know what the fleet IS, so being wrong about that makes
every later answer describe the wrong thing: it is fatal, and an empty
answer from it is refused rather than replaced by a local default. One
target refusing a connection only means that machine's logs are missing
from this answer — it is named, the rest are still asked, and nothing
stops. The three places that distinction is invisible unless stated are an
unreachable fleet ("nothing was listed", not "no store"), a store that was
not found (name who was not asked — it may be there), and a chunk that
could not be read (say so where the boundary marker goes, or a stopped
scroll is the end of the log).

### What is still DNS, and is not built

The resolver is asked **one** question: what is the fleet. Broadcast
resolution — ask everyone, match the id — is what happens next, and it is
fine at the measured fleet (8 queryable of 30).

**"Who has this store" has deliberately not been designed.** Not even an
argument is reserved for it, because reserving one would design it by
accident. The questions it opens and does not answer:

- What is a negative answer worth? A resolver that says "nobody has it" is
  either authoritative or merely uninformed, and a client cannot tell —
  which is the difference between "that store is gone" and "ask the others".
- Caching, and therefore staleness. A lookup that is worth doing is worth
  not repeating, and then a store that moved is at an address that resolves
  to the wrong host until something expires.
- Whether it composes with the fleet question at all, or is a second
  program. A resolver that must answer both is a bigger contract than most
  sites want to implement.

What matters today is only that the address carries **identity**, which it
does: adding a lookup later changes how a name is resolved and not what the
name is.

## Open questions

- **Whether an address may carry a search.** `timber:?has=<token>` is the
  one query the viewer generates on its own, and pasting "this identifier,
  fleet-wide" is the thing an incident wants to share. It is also the
  first millimetre of a query language in an address, which is why the
  first version has no such form: an address is a PLACE, and the document
  is how a search is written down.
- **What an unresolvable address should do.** Today an id no target holds
  is refused with the count of stores that were asked, which is honest for
  a broadcast. A lookup would make it a resolution failure — a different
  sentence, and possibly a different exit code.
- **Whether `search` should be bounded by where you are looking.** It is
  deliberately not: the loop starts with an identifier and no idea which
  log holds it, so the predicate you opened with must not narrow it. But
  on a large fleet an unscoped `--has` for a common token is a lot of
  reading, and the viewer has no way to say "this store, this hour" yet.
- **Follow.** Not in scope: the live edge is not in a chunk, and `select
  … --follow` already tails it. The viewer only says it is not showing
  it — which is what the bottom marker does today, from the writer's
  presence rather than from a flush age the store object does not carry.
