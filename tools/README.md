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
optional tool. `apt install timberfs` pulls no Python; `apt install
timberfs-sh` pulls both it and timberfs.

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
each remembers where it lives. Listings go out in parallel; reads go host by
host and claim no order between them, for the reason a bounded timberfs
answer is `order=sequential`. `limit N` is N in total, so a later host may go
unread — it says so when that happens, and a host it cannot reach is named
rather than quietly missing.

A read goes to every host **at once** and is rendered in the order the hosts
were given. There is no point asking first which of them hold a matching
store: a read whose predicate matches nothing resolves to nothing and returns
immediately — the same 0.00 s as asking. The cost was never the search, it
was N round trips taken one after another. Measured on seven hosts at a
second of latency each: **7.0 s → 1.1 s**.

⚠ A `limit` is sent to every host and enforced across the answers, so a host
whose output falls past the limit did work that is never shown — bounded by
the limit itself, and the price of not paying the latency serially. Hosts
with nothing are reported as a count rather than as a row of empty headers.

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
searchable tokens on a line — exactly the runs `--has` matches, so what can
be picked is what can be searched — and `Enter` finds one everywhere.
A word the index cannot hold (`26.1.18`) is refused where you point at it,
with the reason, rather than discovered later as an empty answer.

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
