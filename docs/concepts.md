# Concepts

The vocabulary, alphabetically: one line per term and where it is actually
explained. A lookup table, not a tutorial — nothing here is defined twice, so
every entry points at the document that owns it.

**Which door to use.** [README](../README.md) is the tour and the getting-started
path. [Use cases](use-cases.md) is the deployment *shapes* things compose into,
with their limits. [Deploying timberfs](deployment.md) is directory layout,
systemd units and permissions. [Design](design.md) is the internals — on-disk
format, why FUSE, the semantics table. The man pages are the reference:
`timberfs(1)`, `timber-filter(1)`, `timber-otlp(1)`, `timberfs-records(5)`, each
with the sections named below.

---

**`.bark`** — the manifest: one flat, human-editable JSON object holding a
store's *declared* facts (identity, settings, retention, provenance, lineage).
Travels with the store, survives head-drops, ships inside bundles.
→ [design](design.md#the-bark-manifest)

**backing pair** — `<name>.trunk` + `<name>.rings`, the two files that *are* the
store. Everything else beside them is a sidecar.
→ [design](design.md#on-disk-format)

**bundle** — see **`.timber`**.

**chunk** — the shared unit of compression, indexing, retention and
random-access read: one self-contained zstd frame plus one `.rings` record.
→ [design](design.md#on-disk-format)

**chunk number (`seq`)** — a position in one store, never a fact about its
contents: dense, only increasing, unchanged by a head-drop, and what a cursor
names. Assigned by the destination on ingest, so an incoming number is ignored.
→ [design](design.md#the-chunk-number-and-what-it-is-not)

**chunk selection vs entry selection** — `query` picks chunks by the write-time
index (coarse, widened by about a minute to catch buffered stragglers), then
verifies every entry against the timestamp its own line carries. Coarse then
exact, which is what makes the widening safe.
→ [design](design.md#semantics)

**cursor** — a consumer's durable position, on the write axis because it is the
only monotonic one. A follower's lives in the registry as `cursor.json`.
→ [README](../README.md#followers-who-is-reading-and-how-far-behind)

**derived store** — what `export` and `rotate` produce: a new store with a fresh
identity, its lineage recorded (`derived_from`, `derived_op`), provenance
inherited, settings and window facts not.
→ [design](design.md#the-bark-manifest), `timberfs(1)` **PROVENANCE**

**disconnection budget** — retention read from the shipper's side: `retain 30d`
means the receiver can be gone thirty days. What sizes it is `retain_size`.
→ [README](../README.md#shipping-onward-timber-otlp)

**dropped counters** — what has left a store over its whole life. The chunk
count is exact; the byte figures are a floor, because a head-drop leaves no
trace in the index and only a recording binary can have counted them.
→ `timberfs(1)` **WHAT A STORE HAS DROPPED**

**entry** — the unit everything downstream works in: one logical log record,
stack trace included, not one line.
→ `timber-filter(1)`, `timberfs-records(5)`

**fleet view** — one query across many stores, interleaved by timestamp at
*read* time, each line prefixed with the store it came from. A read-time merge
of files this machine can reach, deliberately not a cluster.
→ [README](../README.md#beyond-the-getting-started-path)

**`--flush-age`** — how long buffered data waits before it becomes a chunk
(default 5 s). Therefore also the crash-loss window without `--wal`, and the
slop the write-time index alone would leave at a window's edges.

**follower** — a registered reader of a store: a name, a type, a `retaining`
flag and a durable position. A *declared* object rather than a cursor found
lying in a directory, which is what makes the replication-slot analogy exact.
→ [README](../README.md#followers-who-is-reading-and-how-far-behind),
`timberfs(1)` **FOLLOWERS**

**follower registry** — `/var/lib/timberfs/followers/<name>/`, one directory per
follower, split by ownership: `follower.json` the operator writes, `cursor.json`
the follower writes, `follower.lock` held while it runs.
→ [README](../README.md#followers-who-is-reading-and-how-far-behind)

**forest** — a directory searched for stores by a short **handle**, so `timberfs
query nginx` needs no path. Configured by `/etc/timberfs/forests.d/*.conf`; a
bare token that names no store on disk is the only thing it applies to.
→ `timberfs(1)` **FORESTS**

**`forward-intake`** — the Fluentd Forward v1 receiver: what Docker's `fluentd`
log driver, Fluent Bit and the fluent-logger libraries already speak. Every tag
lands in its own store, and a chunk is acked only once durable in the `.sap`.
→ [README](../README.md#3-make-it-the-logger-when-youre-ready),
[deployment](deployment.md#systemd-units)

**frame** — two meanings, kept apart. A **zstd frame** is one chunk's compressed
bytes inside the `.trunk`. A **wire frame** is the native replication protocol's
unit. `frames-send` ships the first inside the second.
→ [design](design.md#on-disk-format), `timberfs(1)` **REPLICATION**

**`frames-send` / `frames-intake`** — native replication between timberfs hosts:
compressed chunks verbatim, nothing decompressed at either end, the receiver's
position authoritative so a sender keeps no cursor and cannot re-send.
→ [README](../README.md#replicating-to-another-timberfs-frames-send),
`timberfs(1)` **REPLICATION**

**`GAP` warning** — what a shipper reports on resume when the chunk its cursor
named has already been dropped: the size of the hole, inferred from the far
side. The **override record** is the same fact, exact, from the writer's side.
→ [README](../README.md#followers-who-is-reading-and-how-far-behind)

**`.grain`** — the token index: one Bloom filter per chunk over every token in
it (~1% false positives), letting `--has` skip chunks with no time bound at all.
Derived data — safe to delete, cheap to rebuild with `reindex`.
→ [design](design.md#custom-indexes-the-grain-token-index)

**handle** — a store's short name within a forest (`nginx`), as opposed to its
path. → `timberfs(1)` **FORESTS**

**head-drop** — dropping a chunk *prefix* in place with
`fallocate(COLLAPSE_RANGE)`, rebasing the index and the grain inside one seqlock
window. The one primitive a log workload needs that POSIX lacks, and what makes
this a filesystem for logs rather than a rotation scheme.
→ [design](design.md), [design](design.md#custom-indexes-the-grain-token-index)

**identity** — a store's `.bark` `id`: a UUID minted on first write, constant
across renames, moves and hosts. A follower records its store by identity, never
by path — a store can move, and a path can come to hold a different store.
→ [design](design.md#the-bark-manifest)

**intake** — a way in: plain text, the records stream, Fluentd Forward, OTLP, or
frames. A store's path says what it *is*, never which intake wrote it.
→ [deployment](deployment.md#one-layout-no-intake-in-the-path)

**interest axis** — the third retention axis, `retain_unconsumed`: keep what
this store's retaining followers have not read. Additive, never a cap — see
**retention**.

**live edge** — the newest data: buffered, not yet a chunk. `query --follow`
tails it through the `.sap`; a plain `query` never includes it; an entry from it
has no chunk number, and that absence is the signal (a zero would be a lie).
→ [README](../README.md#durability-and-the-live-edge---wal)

**lineage** — see **derived store**.

**mount** — the FUSE layer, for software that insists on writing to a real file
path. One of several ways in, not the substrate: the store itself contains no
FUSE types and could be re-hosted elsewhere unchanged.
→ [design](design.md#why-fuse-and-not-overlayfs--a-kernel-module)

**`otlp-intake`** — receives OTLP/HTTP logs from any OpenTelemetry SDK or
Collector. Each `ResourceLogs` stream lands in its own store routed by
`service.name`, its resource attributes seeded into the `.bark`, and the HTTP
200 is sent only once the batch is fsynced.
→ [README](../README.md#3-make-it-the-logger-when-youre-ready),
[deployment](deployment.md#systemd-units)

**override record** — what a writer prints when a size budget drops chunks a
retaining follower had not read: which follower, which chunks, at the moment it
happens. Owed rather than optional, because with finite disk bounded loss is a
choice already made.
→ [README](../README.md#followers-who-is-reading-and-how-far-behind),
`timberfs(1)` **RETENTION**

**predicate** — `timber-filter`'s matchers, applied per entry: `--has` (whole
token, rides the grain automatically), `--substring`, `--regex`, each with a
caseless and a `--not-` form, plus `--any` for OR.
→ `timber-filter(1)` **SELECT**

**provenance** — the free-form facts you declare about a store (`host`,
`service`, anything via `--set`), inherited by derived stores so an artifact
still says where it came from. → `timberfs(1)` **PROVENANCE**

**`query`** — time-window reads: chunks selected by the index, then every entry
verified against its own logline timestamp, so 13:37–13:38 never shows a 13:42
line. `--follow` tails the live edge. → [design](design.md#semantics)

**records stream** — timberfs's own line protocol carrying entries with their
timestamps and chunk numbers attached. What `--records` reads and writes, and
what makes `timber-filter … | timberfs import` lossless.
→ `timberfs-records(5)`

**`--replica`** — on `frames-intake`: keep the sender's chunk numbers and record
its origin, so a chunk answers to the same address at both ends. Numbering and
origin travel together or not at all.
→ [README](../README.md#replicating-to-another-timberfs-frames-send)

**retaining** — a follower's declaration that its position holds the store's
head back. One half of a pair — the store declares `retain_unconsumed` — and
additive rather than a cap, so `retain_size` still overrides it. A retaining
follower with **no position holds everything**, which is both the point and the
footgun. → [README](../README.md#followers-who-is-reading-and-how-far-behind),
`timberfs(1)` **FOLLOWERS**

**retention** — a property of the *log*, declared in its manifest on three axes
(`retain`, `retain_size`, `retain_unconsumed`) and enforced by every writer on
its own tick. They combine with `max`, never `min`: each names a head prefix it
would be happy to see gone and the largest wins, so no axis can hold data
another has released. → [README](../README.md#rotation--retention),
`timberfs(1)` **RETENTION**

**`.rings`** — the write-time index: one fixed-stride record per chunk (byte
ranges, write-time window, `seq`), appended in write order and therefore sorted
by both offset and time, so any lookup is one binary search.
→ [design](design.md#on-disk-format)

**rotate** — time-based rotation: everything written before a cutoff moves out
of the live log (or is dropped) while newer data stays put, relocating
compressed bytes verbatim. → [README](../README.md#rotation--retention)

**`.sap`** — the write-ahead sidecar, declared by `wal=true`: every entry raw as
it arrives, fsynced on the once-a-second maintenance tick, so the crash-loss
window shrinks from `--flush-age` to about a second while chunking keeps its own
schedule. Also what `query --follow` tails.
→ [design](design.md#the-sap-write-ahead-sidecar)

**seqlock** — the counter a reader samples to know the rings and the grain it
loaded are one generation. A pair straddling a head-drop would skip chunks that
do match, which is the one error class the sidecar design refuses.
→ [design](design.md#custom-indexes-the-grain-token-index)

**sidecar** — any file beside the backing pair, each with its own contract:
`.grain` derived and rebuildable, `.sap` live writer state read once after a
crash, `.bark` what you declared. Missing means scan, never a wrong answer.
→ [design](design.md#custom-indexes-the-grain-token-index)

**skippable frame** — the inert zstd frame a head-drop leaves behind where
filesystem-block alignment stopped the cut short. Stock `zstd -dc` ignores it.

**store** — the unit of everything: one log, its backing pair, its sidecars, its
declared facts and its identity. In OTLP terms a store *is* a service.

**`.timber` bundle** — a store packed into one self-describing file by `export`:
queryable in place, carrying its `.bark` and therefore its own provenance and
lineage. → [use cases](use-cases.md#hand-an-investigation-to-someone-else)

**`timber-otlp`** — the shipper: a store's entry stream posted to any OTLP/HTTP
receiver, one LogRecord per entry, resumable across restarts. It is a reader, so
an unreachable receiver stalls it and nothing else — the store is the send
buffer. → [README](../README.md#shipping-onward-timber-otlp)

**token** — an ASCII-alphanumeric run of 3–64 characters, exact case: what the
grain indexes and what `--has` matches whole.
→ [design](design.md#custom-indexes-the-grain-token-index)

**`trim`** — the cron-able one-shot retention run, for a store whose producer
went quiet: retention otherwise only fires inside a live writer. Leaves a store
somebody else is writing alone, because that writer's own tick is already doing
it. → [README](../README.md#rotation--retention)

**`.trunk`** — the data: concatenated zstd frames, one per chunk, no wrapper
bytes. `zstd -dc <name>.trunk` recovers the whole log with no timberfs
involved — the index is pure acceleration.
→ [design](design.md#on-disk-format)

**two clocks** — **logged time** is parsed from the line (what `import` and
followers stamp with); **write time** is the wall clock when the byte arrived
(what `append` and `mount` stamp with). `query` selects on the store's clock and
verifies on the line's, so where a producer's two clocks diverge the difference
shows at a window's edges.
→ [deployment](deployment.md#two-clocks-and-when-they-diverge), `timberfs(1)`
**THE TWO CLOCKS**

**`--wal`** — see **`.sap`**.

**write-time index** — see **`.rings`**.

## See also

- [README](../README.md) — the feature tour and getting started
- [Use cases](use-cases.md) — the deployment shapes, and what this is not
- [Deploying timberfs](deployment.md) — directory layout, systemd units,
  ownership and permissions
- [Design](design.md) — why FUSE, the on-disk format, the semantics table
- [Plans](plans/) — where a direction is still being designed rather than
  described
