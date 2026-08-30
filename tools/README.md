# tools

Programs that USE timberfs rather than being part of it. They live in this
repository on purpose: a client kept somewhere else stops being run, and
stops being edited in the same commit as the protocol change it needed.

## `timbersh` — an interactive shell for timberfs

⚠ **EXPERIMENTAL.** The statements it accepts are not promised and change
without notice. Do not build anything on the grammar.

Shipped as its own package, `timberfs-sh`, so that `timberfs` keeps a
dependency list of `fuse3, libc6` and installs on a bare host: a Python
script in the main package would put an interpreter there for one
optional tool. `apt install timberfs` pulls no Python.

⚠ `timberfs-sh` **recommends** timberfs rather than depending on it.
These are clients that speak `timberfs-query-document(5)` over a
transport, and against a fleet that transport is `ssh` or a site
wrapper — they need no timberfs on the machine they run on, which is
the same reason times are resolved here rather than by shelling out to
one. The default `TIMBERFS_CMD` is a local `timberfs`, so apt installs
it and the zero-config case works; `--no-install-recommends` is the
workstation that only ever reads other machines.

It releases on its own tag, and its version is `tools/VERSION`:

```sh
timbersh-v0.1.0     ->  builds timberfs-sh, and rebuilds the apt pool
v0.25.0             ->  builds timberfs, VM-tests it, publishes the crate
```

Because the CYCLES differ, not the source. A timberfs release is an
on-disk format re-tested in a VM and published to crates.io; this is a
script that changes while a session is open. Sharing a release event
would mean either the console ships stale or timberfs is released for
reasons that are not timberfs's.

⚠ Same repository, deliberately. The client and the protocol change in
one commit series — that is where this tool's value comes from, and every
protocol fix it has found was verified against both the old server and
the new in a single branch. Only the release is separate.

```sh
timbersh --help                   # options
timbersh                          # the local forest
timbersh --hosts web01,web02,db01 \
         --cmd "ssh _TIMBERHOST_ timberfs query --query -"
```

```sql
view   [id=79d7f23a];              -- a pager over the tape, at the last chunk
view   [] at '12:00';              -- or anchored at an instant
add host web03;                    -- ask it too, from now on
drop host web03;                   -- stop asking it
create logview [type=console] console;
select stores  from console;
select records from console where entry has 'ERROR' limit 100;
select records from [] where chunk may have 'req-8f3a';
declare errs cursor for select records from console where entry has 'ERROR';
fetch 100 from errs;
```

Every option has an environment variable beside it, so a flag overrides an
export for one session and nothing else:

| flag | variable | |
|---|---|---|
| `--resolver` | `TIMBERFS_RESOLVER` | a command that prints the fleet — see below |
| `--targets` | `TIMBERFS_TARGETS` | the same document, from a file |
| `--cmd` | `TIMBERFS_CMD` | one command reaching every host, with `_TIMBERHOST_` substituted |
| `--hosts` | `TIMBERFS_HOSTS` | the hosts that command reaches |
| `--rc` | `TIMBERSH_RC` | statements run at startup |
| `--histfile` | `TIMBERSH_HISTFILE` | line history, `~/.timbersh-history`, mode 0600 |
| `--histsize` | `TIMBERSH_HISTSIZE` | how many lines to keep (2000) |
| `--ttl` | `TIMBERSH_STORE_TTL` | expire the cached store list after N seconds; 0 (default) never expires it |

A target is only ever handed a document on stdin, so anything that reaches a
timberfs works — a wrapper, `ssh`, a container exec.

With several hosts the stores present as one set with a `HOST` column, and
each remembers where it lives. A host it cannot reach is named rather than
quietly missing.

A read goes to every host **at once** and is rendered **as it arrives**, each
line naming the host it came from. There is no point asking first which of
them hold a matching store: a read whose predicate matches nothing resolves to
nothing and returns immediately — the same 0.00 s as asking. The cost was
never the search, it was N round trips taken one after another. Measured on
seven hosts at a second of latency each: **7.0 s → 1.1 s**.

⚠ It used to collect every answer and then replay them host by host, so the
fastest host's first line waited for the slowest host's last one. Measured
against a server emitting an entry every 300 ms: **first line at 1.9 s, which
was also when the last one arrived**. Now 0.4 s on one host, and on a fleet
where one host is slow, 0.1 s for the fast one's first line.

No ordering is lost by interleaving. Within a host a bounded answer is
`order=sequential` — store after store, not time — and across hosts this shell
never claimed one; the grouping was a rendering. Ordering by the clock an entry
carries needs the whole answer in hand and is a separate thing
(`docs/plans/logline-order.md`). `limit N` is N in total, so which entries a
bounded fleet read returns is decided by arrival, and it says how many more had
already come.

⚠ A `limit` is sent to every host and enforced across the answers, so a host
whose output falls past the limit did work that is never shown — bounded by
the limit itself, and the price of not paying the latency serially. Hosts
with nothing are reported as a count rather than as a row of empty headers.

⚠ **Nothing streams.** One call is one process whose whole output is buffered,
times the hosts asked at once — measured at 3.4× the answer on the terminal
path and 2.5× held for the life of an answer screen. So an unbounded `records`
read is walked a page at a time (`--page`, default 10000 entries), handing the
`position` records back as the next page's `cursor`: same answer, 215 MB → 33 MB
on a 400k-entry read. A `limit` is a total enforced in the client, so a later
host's entries are read and dropped and a page built from its positions would
resume past exactly what nobody saw — a bounded read is therefore ONE call.
`loglines` and `chunks` carry no `position` record and cannot be continued, so
they are left unpaged rather than truncated.

`select ... into view` takes one page: an answer screen materialises every
entry it is handed, so paging a gigabyte into it would only move where the
memory goes. The bottom of the screen says the answer continues.

### What it is for

Dogfooding `timberfs-query-document(5)`. A protocol nobody writes a
client against is a protocol whose awkward parts stay theoretical. Two
found within an hour of it existing:

* **store selection had no substring operator** — typing part of a name
  is the commonest thing anyone does, and `name=~.*apache.*` reads the
  typed text as a pattern. Fixed in 0.24.0 by `=*`.
* **paging by timestamp does not work at all.** `fetch` fakes a cursor
  with `window.from = last_ts + 1`, and on a fleet where entries share a
  timestamp — a synchronised event, or just second-granularity logs —
  the second page silently drops everything that shared the last one.
  Fifteen of eighteen entries, in the first fleet it was pointed at.
  That is the argument for a position being an absolute byte offset
  rather than a clock: see [docs/plans/paging.md](../docs/plans/paging.md).

### What it does not implement, and what it must

**The selector** is never reimplemented — every `from` is a real
`kind: "stores"` query. A second copy would drift, and a drifted selector
answers a different question without saying so. That is the defect this
repo has spent several releases removing.

**Times are resolved here**, and that is not a compromise. A query
document is meant to be self-contained: a client that speaks it needs the
protocol and nothing else, so building one must not require a timberfs on
the client's machine to interpret a string. This used to shell out to
`--from X --dump-json` and read the milliseconds back, which failed
outright where no timberfs was installed — and could never have been
right for a remote store, since the probe ran locally.

`window.from` is milliseconds because a document is a **value**. `11:10`
means today, in the reader's timezone; a document carrying that text would
mean a different instant tomorrow, and another one parsed at the far end.
Resolution belongs at the edge where the person and their clock are.

The duplication that remains is bounded, unlike the selector's: a
disagreement about a date format yields a different *number*, never a
search the server reads as a different question.

### Design notes

* A **logview** is a NAMED PREDICATE, re-resolved every time it is used,
  so a store that appears afterwards is in it. The selection is a query,
  never a captured list.
* `[...]` is a predicate literal, so a view name and a label can never be
  mistaken for one another — and a completer always knows which of them
  it is completing.
* The **subject** of a `where` clause carries what the document requires:
  `entry has` versus `chunk may have` IS `match.granularity`, and
  `logline since` versus `written since` IS `window.axis`. Neither can be
  defaulted by accident, and asking for both granularities at once is
  ungrammatical rather than merely refused. (`may` because a Bloom filter
  cannot say more than that.)
* **`show hosts;`** is what you are talking to: the version each one
  reported, how many stores it has, and whether anything it said went
  wrong. A host answering without a version is a timberfs from before the
  field existed — a fact about that host, not a gap in the listing.
* Every call goes through one place that RECORDS what came back. timberfs
  writes its explanations to stderr and still exits 0 (`no store matches
  ...`, `retention overtook this follower`), and those are exactly the
  sentences that answer "why did I get nothing" — they used to be
  discarded on every successful call.
* The **store list is read once and kept** for the session, and shared by
  completion and `\d`. It is fetched in a background thread at startup, so
  the first TAB does not pay a round trip — against a remote forest that is
  seconds, and paying it there is what makes completion feel broken rather
  than slow. `refresh` re-reads it and says what changed, which beats a
  timer: it happens when you know something did.
* `~/.timbershrc` is RUN, not read: a script of statements, the way
  `.psqlrc` is. `save` therefore refuses to default to it — writing the
  logviews back would delete whatever else it does.

## Where the fleet is

A **target** is a name and the command that reaches it. The command belongs
to the target, not to the session — which is the whole point: an
`ssh mail01 timberfs …` and a site wrapper taking the host as an argument
are one fleet, and neither has to be expressible as the other with a
placeholder swapped into it. `TIMBERFS_CMD` + `_TIMBERHOST_` could not do
that, and it is still there as one *producer* of a target list rather than
as the only way to describe a fleet.

The document a resolver prints, and the file that holds the same thing:

```json
{"v": "1.0-EXPERIMENTAL",
 "targets": [
   {"name": "mail01", "cmd": ["ssh", "mail01", "timberfs", "query", "--query", "-"]},
   {"name": "web01",  "cmd": ["site-wrapper", "query", "web01"]},
   {"cmd": ["timberfs", "query", "--query", "-"]}
 ]}
```

`cmd` is a **list** because a command line written as one string has to be
split again at this end, under rules we would have had to invent and get
wrong — the same call the query document makes for `stores.select`. A
`name` is what the `HOST` column and a `timber://<host>/…` address say; it
is a **hint and not identity**, so renaming a target, or changing how it is
reached, leaves every written address valid.

**Where it comes from**, most explicit first — `show hosts;` says which won:

```
--resolver | --targets | --cmd/--hosts        one of them, not two
$TIMBERFS_RESOLVER
$TIMBERFS_TARGETS
$TIMBERFS_CMD / $TIMBERFS_HOSTS
~/.config/timberfs/targets.json, /etc/timberfs/targets.json
one local `timberfs query --query -`          found on PATH
```

A flag beats an export as everywhere else here. Two *flags* is a usage
error — they are three ways to answer one question, typed together, and
preferring one silently is the mistake this replaced. Two *exports* is not:
a stale one in a profile is ordinary, so the order decides and the
provenance is reported rather than assumed.

⚠ **A resolver that failed is fatal, and an empty fleet is refused.**
Falling back would answer a question about one fleet with a different one,
and the empty case is worse: a session that quietly asks the local machine
instead looks exactly like a fleet that held nothing.

⚠ **A TARGET that failed is not.** Something in the system being down must
not stop you reading the logs that are available, so a target that does not
answer is named and the rest are still asked — in a listing, in a search,
and in the viewer's store picker. The two are different failures: the
resolver is how you know what the fleet IS, and being wrong about that
makes every later answer describe the wrong thing; one machine refusing a
connection only means that machine's logs are missing from this answer,
which is worth saying and not worth stopping for.

The three ways that has to hold, because they read the same when it does
not: an unreachable fleet must say **"nothing was listed"** rather than
"no store" · a store that was not found must name **who was not asked**,
since it may be on exactly that host · and a chunk that could not be read
must say so **where the boundary marker would go**, or a scroll that
stopped is the same screen as the end of the log.

⚠ **A target this build cannot reach is NAMED, never dropped.** A future
`{"name": …, "url": …}` leaves that one target unreachable-with-a-reason
and the rest of the fleet working — refusing all of it over one would be
the wrong blast radius, and dropping it silently would make a host that was
never asked read as a host that had nothing.

The resolver is asked **one** question — what is the fleet — and gets no
arguments. "Who has this store" is a different question that has not been
thought through, and reserving an argument for it now would design it by
accident. `refresh` re-runs it, because a resolver derives its answer.

## An answer on the pager's screen

```sql
select records from [] where entry has 'ERROR' limit 50 into view;
```
```sh
timberfs query --records --from 13:00 --has ERROR app.log | timberview
```

`select` walks a result set and `view` walks the log — different motions,
and this puts the first on the second's screen. What makes it fit is that
an entry record carries `chunk` and `offset`, so every line in an answer
knows the PLACE it came from: the same coordinate the tape is addressed
by. `Tab` and the search work as they do anywhere — `Enter` searches the
picked term on an answer exactly as it does on the tape, because a key
that changes meaning with the screen is the surprise a mode is. `y`
gives you that entry's address and copies it, `c` copies the entry
itself — all forty lines of a stack trace, not the one the cursor is on —
and **`o` leaves the answer for the log around it** — the one motion an answer cannot make for itself, since
what you usually want next is what was happening either side of a
match.

- **A multi-line entry is one entry.** The lines of a stack trace belong
  to the entry that raised it; splitting them would be the same lie as
  splitting a line across a chunk boundary.
- **An answer is a closed set.** Both ends are ends, nothing extends, and
  the boundary says "end of the answer" rather than naming a chunk you
  are not in.
- **An entry still at the live edge carries no offset either** — it is in
  no chunk yet — so it opens by its write window like any other, landing
  at the end of the tape near where it is about to be. The line itself
  will not be found there, and the message says which coordinate was
  used. On a store being written this is ordinary: the newest matches
  are in the WAL, and a read delivers them before any chunk holds them.
- ⚠ **An entry with no place is still an entry**, and an old target in a
  fleet must not make a term unusable. `offset` on an entry record
  landed in **0.26.0**, so a target still on 0.25.0 answers with entries
  that carry none — but they are listed, read, and searched from like
  any other. Dropping them made a term that matched only there report
  "no hit", which is false, and took the terms in those entries with it.
- ⚠ **And they are opened anyway, by WHEN rather than where.** An entry
  record has carried `wf` — the write window it arrived in — since long
  before `offset`, and a write-axis window of one millisecond is a seek
  to the chunk covering it. Measured against a live store, `wf` alone
  lands on the entry's own chunk; the line is then found in what comes
  back. What is lost is exactness, not the ability to open: the hit list
  marks such a row `·`, and the message says it was opened by the window
  and which target could not give an offset.
- ⚠ Only `records` can go into a view. The other kinds carry no offset,
  so nothing in such an answer could say where it came from — refused as
  a statement, wherever it is run, rather than as a terminal problem.

Reading a piped answer needs a keyboard from somewhere other than stdin,
which is the answer: `timberview` reopens `/dev/tty` for keys, as every
pager does, and says so plainly where a session has no controlling
terminal to reopen.

## `timberview` — a pager over one store

⚠ **EXPERIMENTAL**, like everything else here.

`select` walks a result set; `view` walks the log. They share the absolute
tape offset and nothing else, which is why the handle is a chunk number
rather than a cursor: a cursor is a place in an answer, and there is no
answer to be in.

```sh
timberview app.log                 # the last chunk, and back from there
timberview --at '12:00' app.log
timberview 'timber://mail01/79d7f23a-…#offset=33724753900'
```

It parses nothing — no timestamps, no window verification, no `.grain`.
Chunks, decompressed, shown as lines. That is the feature rather than a
simplification: the two stores `select` serves worst, one whose lines
timberfs cannot parse and one with no index, are exactly the ones you most
want to look at.

**The loop it exists for**: see an identifier, search it across every host,
jump to the coordinate a hit comes back with. `Tab` moves between the
searchable **terms** on a line and `Enter` finds one everywhere.

⚠ A term is not an index token, and conflating them is what made a UUID
unselectable. The *index* holds alphanumeric runs of 3–64, so
`9da3dcf1-5a4b-4d36-b907-917daa60bd90` is five of them and none is the id.
A **`has` term** is wider: timberfs ANDs the runs inside it on the index
and then matches the whole thing word-anchored — so the UUID is ONE term,
and it is the one worth offering. Measured on a store where the whole id
matched one entry, its piece `5a4b` matched every entry in the chunk.

The hit list is searchable the same way: `Tab` walks the terms of the
highlighted hit and `*` searches that one, because following an
identifier is rarely one hop and the second one is usually sitting in the
answer to the first. `Enter` there still opens the hit.

So `Tab` offers the widest identifier at each position — separators and
all, joined on `-` `.` `_` `:` but never across `=` or `/`, which separate
fields rather than sit inside a name. A term the index cannot hold at all
(`26.1.18`, three runs of under three characters) is refused where you
point at it, with the reason, rather than discovered later as an empty
answer.

**Taking a selection away.** `m` sets the mark, the cursor is the other
end, and `c` copies the region; `x` swaps the ends, and `^Space`/`^G` are
the same keys for a hand that came from emacs. With **no mark** `c` copies
the whole ENTRY under the cursor, which is the case it exists for: a stack
trace is one entry and forty lines, so the line you are on is one frame of
it and what you want is the trace, in something that analyses one. On an
answer that framing is timberfs's; on the tape, which parses nothing,
every line is its own, exactly as the entry motion is a line there. `z`
first if you want entries as rows — a joined row copies as the log's own
lines rather than with the `↵` in, because that rendering is for reading.

⚠ **The route is said, because one of them cannot be confirmed.** A
clipboard helper (`wl-copy`, `xclip`, `xsel`, `pbcopy`) where there is a
display to use one on: it writes the clipboard of the machine the pager is
on, and its exit status says whether it did. **OSC 52** otherwise, which
is the one route that crosses an ssh or a multiplexer — and whose failure
is *silence*, since a terminal that does not implement it does nothing and
says nothing. So the status line names the route it used and hedges on
that one, `y` keeps showing the address as well as copying it, and a copy
neither route could make is **written to a file** whose path is said. A
selection is never lost quietly.

**One module, two front ends.** timbersh calls it in process, because a
separate program would have to leave the alt screen carrying "search this
token", let the shell print, and re-enter at a coordinate — a flicker per
hop. It reaches the log through four operations and nothing else:

```
stores()                        the list, to switch between
bounds(store)                   first_seq, last_seq, what was dropped
chunk(store, seq=N | at=MS)     lines + the ring around them
search(token)                   addresses
```

Written against those, it is tested against a fake rather than a terminal,
and the fan-out stays the shell's: `search` is handed a token and does not
know there are ten hosts.

**Latency is the cost, so it is spent once.** A pager over a fleet waits
on real round trips, and the three things that made that felt as
sluggishness are gone: every target is asked **at once** rather than in
turn (the same fix, and the same reason, as timbersh's `7.0 s → 1.1 s`);
opening waits for **one** chunk rather than three, because the neighbours
are read ahead while you are looking at the one you landed on; and a
chunk once fetched is **cached**, which is safe precisely because a
chunk's bytes never change after it is written. Measured against three
targets at 400 ms each:

| | before | after |
|---|---|---|
| the store list, at startup | 1.23 s | **0.41 s** |
| opening a store | 0.83 s | **0.42 s** |
| `G` — seek to the end | 0.83 s | **0.01 s** |
| `Enter` — search the fleet | 1.24 s | **0.42 s** |
| scrolling into the next chunk | 0.42 s | **0.00 s** |

**What is left, it says.** A screen that stops answering cannot be told
from one that has hung, so a read that takes longer than 0.3 s paints
what it is waiting for, on which host, and for how long — and `^C` gives
up on that answer rather than on the session. Nothing is drawn for a
fast one: a flash of "waiting" on every keystroke is its own kind of
noise. The FIRST read is inside that too, since it is the slowest and
the one with nothing on screen yet to explain it.

**The client decompresses.** One chunk is ten screenfuls of log for a few
KiB on the wire, so the run is held either side of where you are and
scrolling never falls out of a window; asking the far end to decompress
would cost 13× the bytes for nothing. `compression.zstd` is stdlib from
Python 3.14 and `zstd -dcq` is the fallback, which is why the package
depends on `zstd`.

**A coordinate has a written form** — `timber://host/store-id#offset=N` —
so a search returning one, opening one, and handing a place back to the
shell are the same operation. The store id is the name and the host is a
hint: bake the host in and a pasted link breaks the day a store moves.

## Tests

```sh
tests/timbersh/test-timbersh              # all
tests/timbersh/test-timbersh short_id     # one, by substring
tests/timberview/test-timberview          # the viewer's model
tests/timberfs-client/test-timberfs-client   # the fleet resolver
```

No VM, no timberfs, no network. `scripts/check.sh` runs all three, so they
gate a push like everything else.

For timbersh, `--cmd` points at a fake that answers from a script. For the
viewer, the fake is one level up: it implements the four operations, which
is a whole server as far as the viewer is concerned — so where you are,
what a screen shows and what the boundaries say are testable without a
terminal. The statements that DO open a screen (`view`) are driven through
a pty, waiting on what the screen printed rather than on a sleep.

The fake **records every document it is asked**, which is the useful half —
almost every bug here has been in what was *sent* rather than what was
printed, and a test that reads only the output cannot see a window on the
wrong axis or a cursor that lost a store's place.

⚠ Each case was checked by reintroducing its bug and watching it fail. The
paging test passed with the fix deleted, twice, for two different reasons
before it meant anything.

The fake answers `chunks` with a real compressed frame, so a viewer that
decodes what it sends is a viewer that decodes what timberfs sends.
