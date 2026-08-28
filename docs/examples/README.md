# Query document examples

Worked examples of `timberfs-query-document(5)` — the whole search as JSON,
which `timberfs query --query FILE` reads and `--dump-json` writes.

Run any of them:

```sh
timberfs query --query query-windowed-error.json
```

Two schemas ship beside these: `query-document.schema.json` is what a
*request* may say, and `query-answer.schema.json` is what a JSON *answer*
carries — the `server_version` envelope and the store objects inside it.
(The records stream is a byte format, so its contract is
`timberfs-records(5)` rather than a schema.)

The request schema says what is *legal*. These say
what is *useful*, and each one exists to show a capability that is easy to
miss.

⚠ The format's version string carries `EXPERIMENTAL` and means it: expect
it to be broken in place while it settles, and upgrade a generator together
with the timberfs it talks to. The **store objects** a search answers with
are not covered by that — those are the same objects `info --json` and
`list --json` emit.

## Finding stores

| | |
|---|---|
| [query-enumerate-stores.json](query-enumerate-stores.json) | **Every store there is.** Enumerating is not a separate verb — it is the store predicate with nothing in it. A good first request: it tells a client what it can search. |
| [query-stores.json](query-stores.json) | The stores matching a label, and nothing read. What a fleet view needs before it knows which store to read. |

## Searching entries

| | |
|---|---|
| [query-windowed-error.json](query-windowed-error.json) | A time window, a term, a cap. The ordinary case. |
| [query-fleet-by-label.json](query-fleet-by-label.json) | **One request across a fleet**: every console store on a host matching a regex, searched for one request id. The store predicate is what makes this one query instead of N. |
| [query-any-of.json](query-any-of.json) | `any` — at least one of several terms. |
| [query-exclude-noise.json](query-exclude-noise.json) | **`none`** — entries that do NOT contain something, with `substring` and `regex` alongside `has`. The predicate set says what you want to ask; whether the token index can help is timberfs's problem. |
| [query-everything.json](query-everything.json) | The smallest legal document. An omitted member WIDENS the search rather than emptying it, so this is "every entry of that store". |
| [query-deadline.json](query-deadline.json) | **`deadline`** — bound how LONG rather than how much. A fleet read is slow because it READS a lot, not because it matches a lot, so no count bounds the wait. Unlike a timeout in the caller it is *answered*: the `position` records say which stores finished, which one it stopped inside, and which were never opened. |

## Reading cheaply

| | |
|---|---|
| [query-chunk-sweep.json](query-chunk-sweep.json) | **`granularity: "chunks"`** — the chunks that MAY contain a term, emitted whole. A superset, and the cheapest thing a store can answer: the index alone decides it and nothing is decompressed. Ask for it when the next stage does its own matching. |
| [query-raw-chunks.json](query-raw-chunks.json) | `kind: "chunks"` — compressed chunks verbatim, nothing decompressed at either end. |

## Two things worth knowing before you generate one

**`match.granularity` is required, and the two answers differ by orders of
magnitude.** `entries` gives the entries that actually match. `chunks` gives
the chunks that might contain them, whole — on a 1.2 GiB store, a term in
five entries selects 398 chunks and 325,767 lines. Neither is wrong; they
answer different questions, and the format will not guess which you meant.

**There is no way to name a store by path.** A path is neither unique nor
stable and says nothing about what the store holds, so `stores.select` is a
predicate over what a store *declares* — its labels, its name, its id.
`--dump-json` translates a path on the command line into the store's
identity, which is what round-trips.

## Not expressible here

**A live tail.** `timberfs query --follow` holds a stream open, where a
document describes one search — and one that never ends is a subscription,
which belongs to whatever protocol serves the document. `--dump-json`
refuses `--follow` rather than quietly dropping it. Use `--follow` on the
machine that has the store; a document's equivalent is to ask again from
where the last answer stopped.
