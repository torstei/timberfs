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
| `--ttl` | `TSQL_STORE_TTL` | how long the store list is reused |

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

### Two things it deliberately does not implement

Because timberfs has them, and a second copy would drift — which is the
defect this repo has spent several releases removing:

* **the selector** — every `from` is a real `kind: "stores"` query
* **time parsing** — `--from X --dump-json` is asked what `X` means

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
* `~/.timberfsrc` is RUN, not read: a script of statements, the way
  `.psqlrc` is. `save` therefore refuses to default to it — writing the
  logviews back would delete whatever else it does.
