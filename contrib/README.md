# contrib

Things that use timberfs rather than being part of it. Not installed by
the package, not covered by the VM suite, and not held to the same
compatibility promises — they are here because they exercise the
interfaces, and a client that lives outside the repo stops being run.

## `tsql.py` — a SQL-ish console

**A prototype.** The grammar is still moving; do not build anything on it.

```sh
./contrib/tsql.py --help          # options
./contrib/tsql.py                 # the local forest
./contrib/tsql.py --hosts web01,web02,db01 \
                  --cmd "ssh _TIMBERHOST_ timberfs query --query -"
```

```sql
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
| `--cmd` | `TIMBERFS_CMD` | how to reach a timberfs; it gets the document on stdin |
| `--hosts` | `TIMBERFS_HOSTS` | fan out, substituting each host for `_TIMBERHOST_` in `--cmd` |
| `--rc` | `TIMBERFS_RC` | statements run at startup |
| `--ttl` | `TSQL_STORE_TTL` | expire the cached store list after N seconds; 0 (default) never expires it |

`--cmd` is only ever handed a document on stdin, so anything that reaches a
timberfs works — a wrapper, `ssh`, a container exec.

With several hosts the stores present as one set with a `HOST` column, and
each remembers where it lives. Listings go out in parallel; reads go host by
host and claim no order between them, for the reason a bounded timberfs
answer is `order=sequential`. `limit N` is N in total, so a later host may go
unread — it says so when that happens, and a host it cannot reach is named
rather than quietly missing.

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
* `~/.timberfsrc` is RUN, not read: a script of statements, the way
  `.psqlrc` is. `save` therefore refuses to default to it — writing the
  logviews back would delete whatever else it does.
