# How timberfs works

Under the "log filesystem" framing, timberfs is a small, general idea: a
**chunked store** with **per-chunk compression**, **per-chunk metadata**, and
**efficient deletion from the front**. Those four properties are the whole
thing; a log filesystem is just their flagship application.

- **Chunked** — the file is a sequence of independent chunks: the shared unit
  of compression, indexing, retention, and random-access read.
- **Per-chunk compression** — each chunk is one self-contained zstd frame, so a
  chunk decompresses alone and stock `zstd -dc` recovers the whole store.
- **Per-chunk metadata** — a fixed-stride record per chunk (`.rings`: its
  write-time window; `.grain`: a token Bloom filter) turns "scan everything"
  into "read only the chunks that can match."
- **Delete from the front** — drop the oldest chunks without rewriting the rest
  (`fallocate(COLLAPSE_RANGE)`) — the one primitive a log workload needs that
  POSIX lacks, and what makes this a *filesystem* for logs rather than a
  rotation scheme.

The rest of this document is how the store earns those properties. You don't
need any of it to *use* timberfs — the README covers that; this is for the
curious and contributors. For a term rather than a chapter,
[Concepts](concepts.md) indexes the vocabulary and points at wherever each one
is explained.

## Why FUSE (and not overlayfs / a kernel module)

- **overlayfs** layers *namespaces* (upper/lower directories, as used by
  container images). It has no hook for transforming *content*, so it can't
  compress on write or maintain an index. Wrong tool.
- **A native kernel filesystem in Rust** is where Rust-for-Linux is heading,
  but the filesystem bindings are still experimental. Not a good vehicle for
  iterating on a design.
- **FUSE** gives us the full VFS interface in userspace: loggers append
  through the mount unmodified, `tail -f`/`grep`/`less` all just work, and
  the implementation is ordinary safe Rust (`fuser` crate, no libfuse
  dependency — it only needs the `fusermount3` binary at runtime).

The design cleanly splits into a *store* (file format + chunking, no FUSE
types) and a thin FUSE layer, so the store could later be re-hosted in a
kernel module, a `LD_PRELOAD` shim, or a log-shipping daemon without change.

## On-disk format

Each logical file `<name>` is backed by two files in the backing directory:

```
<name>.trunk   concatenated zstd frames, one per chunk, no wrapper bytes
<name>.rings   64-byte header (magic "RING0002" | header_len |
              incompat_flags | next_seq | reserved), then 56-byte
              records (all u64 LE):
              uncomp_start | uncomp_len | comp_start | comp_len
              | first_write_ms | last_write_ms | seq
```

(The names take the timber metaphor seriously: the data is the trunk, and
the index is its growth rings — which really are a write-time index;
dendrochronology dates events by rings exactly the way `timberfs query`
dates bytes by chunks.)

Because the `.trunk` is a plain zstd frame concatenation, **stock tools can
always recover the data**: `zstd -dc app.log.trunk` prints the whole
uncompressed log, no timberfs required. The index is pure acceleration.

Records are appended in write order, so they are sorted both by uncompressed
offset **and** by wall-clock time — byte reads and time queries are each one
`partition_point` binary search.

Crash safety: chunks are written data-first, index-second; on open, index
records pointing past the end of the data are dropped and orphaned data
bytes are overwritten. `fsync()` through the mount flushes the buffer as a
chunk and syncs both backing files, so fsync = durable. Unsynced buffered
data is lost on a crash, bounded by `--flush-age`.

### The .sap write-ahead sidecar

Chunking has two masters that want opposite things: **compression** wants
chunks big and infrequent (fewer, larger zstd frames compress better and cost
less index/metadata overhead); **durability** wants every byte on disk the
instant it arrives. Coupling them — as the plain chunk-on-flush path above
does — forces a choice between the two: flush tiny chunks to bound data loss
(bloating the store, since even a 15-byte log line pays a full zstd frame plus
a 48-byte rings record), or accept losing up to `--flush-age` of buffered data
on a crash.

`"wal": true` in `.bark` (declared with `create --wal` / `append --wal`, or
`set wal=true` on an existing store) breaks the coupling with a third file,
`<name>.sap`: every appended entry is written there **raw**, alongside the
in-memory buffer, and `sap_sync()` — called every second by the same
maintenance tick that ages out chunks — fsyncs it. Chunking proceeds
completely unchanged, on its own size/age schedule; only the crash-loss
window shrinks, from `--flush-age` down to that tick. The cost is real and
explicit: wal-enabled writers write every byte twice (raw to the sap, then
again compressed into the chunk).

The invariant that makes this simple: **the sap and the in-memory buffer
hold the same bytes, by construction.** It is write-only in steady state and
is read exactly once, ever — by a writer's `FileStore::open`, after a crash.
Readers (`query`/`info`/`grep`) never touch it; the "unflushed tail not
included" note in `query`'s output stays true regardless of `--wal`.

On disk:

```
segment header (24 bytes): magic "SAP00001" (8) + u64 LE base + u64 LE uncomp_base
  base = the trunk's comp_size when this segment was created
  uncomp_base = the store's logical (uncompressed) position at the same
  moment — the segment's address in the uncompressed stream, readable
  without a rings lookup

record: u32 LE len | u64 LE wf | u64 LE wl | len payload bytes | u32 LE crc32
  crc32 (standard zlib/gzip polynomial 0xEDB88320) covers the 20-byte
  record header and the payload
```

Replay reads the **longest valid prefix** — stopping at EOF, a short read, or
the first CRC mismatch — and truncates the file to it before resuming
appends. A torn tail is expected crash debris, the same discipline as
`.rings`' trailing-partial-record handling above, never an error.

Flushing a chunk is a **seal-and-swap**: with a non-empty buffer, (1) fsync
the live sap (its content equals the buffer about to be flushed), (2) rename
it to `<name>.sap.seal`, (3) append the compressed frame, write the rings
record, and (for a wal store specifically) fsync both — a plain,
undeclared store still does none of that per-flush fsync, unchanged from
today, (4) create a fresh `<name>.sap` with its bases set to the new
`comp_size` and logical position, (5) unlink the seal. Because every flush rotates the segment, a
segment's lifetime spans exactly one chunk cycle, and its eventual flush
lands its frame at exactly its `base` — which is what makes recovery
decidable. An empty buffer never touches the sap at all.

Recovery at writer-open (after the existing `.trim` reconcile) reads:

| `.sap.seal` present, comparing its `base` to the CURRENT `comp_size` | meaning | action |
| --- | --- | --- |
| `base < comp_size` | the flush landed | discard the seal |
| `base == comp_size` | the flush never landed | replay the seal's entries and complete the flush now |
| `base > comp_size` | the trunk shrank underneath it (external damage) | warn, and still replay — preserving data wins over tidiness |

Then, independently: a plain `<name>.sap` (not a seal) has its valid prefix
replayed to **rebuild the in-memory buffer**, byte-identical to an uncrashed
run — same entries, same wf/wl, no forced flush (resuming the buffer keeps
chunk sizing stable across a crash). A sap present with `"wal"` no longer
declared (`set wal=false`) is still replayed — preserving data — then deleted
and not recreated.

Two operations move `comp_size` without ever touching the buffer or the
sap's entries: `append_frames` (verbatim chunk merges — rotation, `import`'s
timberfs-source path) and the retention/rotation head trims
(`remove_head`/`collapse_head`). Both refresh the live segment's base
headers in place immediately afterward, so a `base`-vs-`comp_size` comparison
on a later crash still tells the truth; and writer-open re-stamps a resumed
segment's bases from the store whenever they disagree (the header is a
witness, the store is the truth), closing the crash-inside-a-refresh window. Staged/atomic delivery (`import
--records`, and the staging machinery generally) bypasses the sap
completely — nothing is durable, or even visible, before commit, by design,
so there is nothing for a wal to add.

`remove`/`rename`/`reset` carry the sap alongside the rest of the sidecars
(deleted, renamed, or reset to an empty base-0 segment, respectively); a
`.timber` bundle never includes it — it is live writer state, not archive
content.

Four properties above are **load-bearing for the live-tail reader**
(live.rs) — `query --follow` tails the sap when a store declares one,
which is what makes a written line visible in a poll interval rather than
in a flush age. They must not be weakened by
refactors: (1) a segment's content is exactly the
next chunk's bytes, so the trunk and the sap are interchangeable sources
per segment and a reader keyed on logical position can never double-emit or
gap; (2) the swap is a rename — a reader's open fd never sees bytes mutate
or truncate in steady state, and the inode change plus the new header are
the generation marker; (3) the frame and rings are durable before a fresh
`.sap` exists, so any visible segment is always resolvable against the
index; (4) the header's `uncomp_base` locates a segment in the uncompressed
stream on its own, giving a reader an anchor that is valid even mid-rebase.

The reader (live.rs) turns (1) into its whole dedup rule: it counts the
payload bytes it has SERVED out of the live segment, and drops exactly
that many from the front of the chunk that segment becomes. The count is
relative to the segment, never an absolute position, because a retention
head trim rebases every logical offset in the store — which is also why
(2) matters twice over: the seal is a rename, so a NEW INODE says the
segment rolled, while the base rewritten in place by that trim does not.
A reader reconciles the segment BEFORE emitting newly flushed chunks, so
a flush landing between the ring snapshot and the write-out is charged to
the chunk that repeats it. Nothing on the read side calls `sap::replay`:
that path removes a file whose header it cannot parse, which is correct
for a writer opening its own store and wrong for a reader.

### The .bark manifest

An optional `<name>.bark` holds the log's *declared* facts as one flat JSON
object — the label on the timber. Plain enough to read by eye; changed with
`timberfs set`, which is validated and atomic where an editor is neither:

```json
{
  "id": "6f9c2a1e-…",             // identity: random UUID, minted on first write,
  "created": "2026-07-11T09:14:02Z", //   constant across renames, moves and hosts
  "host": "imap03.example.com",   // provenance: free-form, yours (--set k=v)
  "service": "checkout",          //   free-form, but timber-otlp reads it as the
                                  //   OTLP service.name (otlp-intake seeds it)
  "index": true,                  // settings: CREATE INDEX — imports maintain the grain
  "wal": true,                    //   write-ahead .sap — crash loss shrinks to ~1s
  "retain": "90d",                //   keep at least this long — enforced by EVERY writer
  "retain_size": "50G",           //   compressed-size budget, oldest dropped first
  "retain_unconsumed": true,      //   keep what retaining FOLLOWERS have not read;
                                  //   additive, and needs the budget above as its backstop
  "cursors": "/var/lib/timberfs", //   SUPERSEDED by the follower registry: a
                                  //   follower declares its store, so a store
                                  //   declares nothing. Honoured, reported so
  "timestamp_regex": "^(...)",    // content: exotic line-timestamp format, declared once
  "timestamp_format": "%m/%d/%Y %H:%M:%S", //   (import flags persist these; inherits)
  "timestamp_utc": true,          //   zoneless line stamps are UTC, not local time
  "derived_from": "41d0…",        // lineage: source store's id
  "derived_op": "export",         // …and how: export (copy) or rotate (move)
  "window_from": "2026-07-04T22:00:00.000Z", // the REQUESTED window (operation
  "window_to": "2026-07-05T22:00:00.000Z"    //   fact — what was asked)
}
```

Artifacts made by `export` and by rotation into a new segment are new
stores: fresh `id`, `derived_from`/`derived_op` lineage (chains compose
across re-carves and shipping), provenance inherited, settings and window
facts not. Content facts — actual spans, sizes — are never recorded (the
artifact's own rings state them authoritatively); the *requested* window
is recorded, because content can't state coverage: a file whose last line
is 17:00 doesn't say whether 17:00–24:00 was covered-but-silent or simply
not exported.

Which is why **an empty result is a result**: exporting or rotating a
window that contains nothing still produces the (empty) artifact.
Present-but-empty ("Saturday was covered, nothing was there — ingest
Sunday") and missing ("a day is missing — don't ingest past the gap") are
opposite signals to a consumer; `--fail-on-empty` turns a quiet day back
into an error for pipelines that want one. `import` skips empty sources
with a note, never an error. Unlike the derived `.grain`, bark survives
head-drops, travels on rename, and ships inside `.timber` bundles.

## Semantics

| Operation            | Behaviour                                                        |
| -------------------- | ---------------------------------------------------------------- |
| append (write @ EOF) | buffered, compressed into a chunk on size/age/close/fsync        |
| write elsewhere      | `EPERM` — the filesystem is append-only                          |
| read anywhere        | chunk located by binary search, decompressed, served             |
| truncate to 0        | allowed: starts the file over (copytruncate-style rotation)      |
| truncate elsewhere   | `EPERM`                                                          |
| rename / unlink      | supported (mv-based log rotation works)                          |
| `ls -l` size         | logical (uncompressed) size                                      |
| `du` blocks          | compressed size — `du -h` shows the real disk footprint          |
| subdirectories       | not yet — flat namespace in v0                                   |

**Chunk selection** is the write-time index's job and is deliberately
coarse: every chunk whose write-time window overlaps the requested range
(widened by about a minute to catch buffered stragglers) is read in full.
Chunk windows are bounded by `--flush-age` (default 5 s) for slow writers
and by `--chunk-size` (default 256 KiB) for fast ones, so that is the slop
the index alone would leave at the edges.

**Entry selection** then closes it: `query` verifies each entry against
`--from`/`--to` by the timestamp its own line carries, so the output
answers the question in the timestamps you can see — 13:37–13:38 never
shows a 13:42 line — while an entry whose timestamp can't be read is
always included rather than silently hidden. The coarse-then-exact split
is why the widening is safe: buffered loggers write lines slightly after
the timestamp they print, and over-reading chunks costs a little I/O where
under-reading would lose edge lines.

`--by-write-time` is the escape hatch to raw chunk output, selected by the
index alone with no parsing — the pre-0.7.4 behaviour, and the right tool
when the question really is "what arrived when". Downstream,
`timber-filter` (entry-aware) or ordinary `grep`/`awk` narrow by content
on the small extract.

## Custom indexes: the .grain token index

The write-time index generalizes: `.rings` is just a per-chunk summary
(byte ranges + a searchable time window), and queries never touch the
trunk except for the chunks the summary selects. The first content index
is implemented: **`.grain`**, one Bloom filter per chunk over every token
in it (~10 bits per distinct token, k=7, ~1% false positives — measured
0.86% on a 2.7 GB production log). Build it with `timberfs reindex`, use
it with `query --has`:

The index is a property of the LOG, declared once in its `.bark`
manifest — after that, every writer maintains the grain automatically:
`import`, the appender, the mount and both network intakes alike, each
extending it incrementally for new chunks and rebuilding it if
rotation/retention dropped it. There is no per-write flag to forget:

```sh
timberfs create --index --set host=foo.bar.com logs-backing/app.log
timberfs import day1/* --into logs-backing/app.log     # grain maintained
timberfs import day2/* --into logs-backing/app.log     # still maintained

timberfs import huge.log --into logs-backing/app.log --index  # or declare+build in one go
timberfs reindex logs-backing/app.log          # or later: 2.7 GB indexed in ~6 s
timberfs query logs-backing/app.log --has F454567068093ZHGZCL   # no time bound!
timberfs query logs-backing/app.log --from 13:00 --to 14:00 --has ERROR \
    | timber-filter --has 'tenantId=FOO'
```

Tokens are ASCII-alphanumeric runs of 3–64 characters, exact case,
config-free: rare tokens (request keys, message ids) skip nearly every
chunk, ubiquitous ones skip nothing and cost only the test. `--has` is a
**chunk-level pre-filter with whole-token matching** — an argument with
separators (`req-8f3a`) must match all its tokens in the same chunk, AND
across repeated `--has` flags is also chunk-level, and substrings of
tokens do not match; exact, entry-level filtering stays downstream in
`timber-filter`. A false positive costs one needless chunk
decompression. The design contract that made this a sidecar:

**Custom indexes are sidecars**: one file per index next to the `.trunk`/`.rings` pair (the
metaphor extends: `.rings` is time, content indexes are *grain*), with a
self-describing header (index type + extractor description) and one
append-only entry per chunk. Three rules:

1. **Derived and rebuildable.** A sidecar can always be regenerated by
   streaming the trunk (`timberfs reindex`), so indexes can be added to
   existing logs, reconfigured, or deleted at zero risk. The trunk and
   rings remain the only durable truth.
2. **Missing means scan.** A chunk without an index entry is "no
   information — scan it". Partial or lagging indexes degrade to
   conservative scans, never wrong answers; this is also the crash story.
3. **Rings rewrites never leave a sidecar as it was.** A rewrite renumbers
   chunks, and a positional sidecar left behind would then answer for the
   wrong ones — a false negative, the one error class this design refuses.
   A **head-drop** (retention, and rotation's source) removes exactly a
   chunk *prefix*, so the sidecar is prefix-trimmed in the same pass:
   `grain::rebase_head` drops its first `k` records with the same
   `COLLAPSE_RANGE` the trunk uses, and records where the survivors now
   start in the header. Nothing is decompressed and nothing is
   re-tokenized, which matters because retention fires repeatedly and a
   rebuild costs a read of the whole store. Any other rewrite deletes the
   sidecar, and `reindex` recreates it.

   The rebase happens inside the same seqlock window as the rings rebase,
   because the two must never be observable apart: a reader that paired
   filters from one generation with records from the other would skip
   chunks that do match. Readers sample that seqlock when they open a
   source and re-check it after loading the grain, dropping the grain (and
   scanning) rather than trusting a mismatched pair.

Consequences worth knowing: chunk size is an index-selectivity knob
(smaller chunks → sharper lookups, more overhead), the grain trails a live
writer by at most its once-a-second maintenance tick (lagging entries just
mean scanning those chunks), and `.timber` bundles carry no grain yet. A
chunk-sequence number was once rejected here, for sidecar survival, and
that reasoning still holds: rule 3's prefix trim needs no identity per
chunk, so the grain is rebased positionally rather than keyed by one. What
it did not weigh is a durable EXTERNAL reference — a consumer's cursor —
which does need identity and cannot get it from a timestamp, `now_ms()`
being the wall clock that an NTP step or a `date -s` moves backwards. So
`seq` exists as of `RING0002`, per RECORD and not in the header: a header
base plus the record's index is a positional key, and any future path
removing a record mid-file would silently re-point every cursor. The
header's `next_seq` is only a high-water mark, so numbering cannot restart
at 0 after retention drops every chunk. The header is 64 bytes because a
reader takes the record offset from the `header_len` the FILE declares
rather than from a compiled-in constant: without that, a later version's
longer header shifts every record underneath every deployed binary, and
reserved bytes nobody can grow into buy nothing. `incompat_flags` is the
other half — reserved space is safe for optional fields (0 reads as
absent), while a field that changes how records must be read sets a bit and
an older reader refuses instead of guessing. (Logged-timestamp
zone maps, the other planned index family, became largely moot: import
already writes logged time into the rings.)

## The chunk number, and what it is not

A chunk's number is a **position in one store**, not a fact about its
contents. Three rules follow, and each is the answer to a way of getting it
wrong.

**It is local, so it is ignored on ingest.** The number rides the records
stream, because a consumer's cursor has to be told which chunk an entry
came from — but `import` and `append --records` let the destination assign
its own. Honouring an incoming number would interleave two fan-in sources
into a sequence that is neither dense nor monotone. This is the one place
the stream's word is *not* law: `wf`/`wl` travel because they say when an
entry was written upstream, which stays true anywhere; a position does not.

**An entry from the live edge has no number**, its chunk not existing yet,
and the ABSENCE is the signal — a zero would be a lie, chunk 0 being a real
chunk. It does have an **offset**, and the two are written apart for that
reason: a chunk is a container, an offset is an address, and the live
segment is the tape's last stretch rather than a place off it. A segment's
bytes are exactly the next chunk's bytes, so the address a live entry
reports is the one that chunk will report for them — which is what lets a
consumer resume past an entry it was shown before any chunk held it.
Durability is the separate question: the sap is readable at `flush` and
durable at `sync`, so a live position is exact and survives as far as the
writer's last sync — the same bargain `tail -f` makes.

**A read that resumes serves the edge too**, not only `--follow`: a cursor
with no window is a consumer following the store, and making it wait out
the writer's flush age to be told about data already durable was the
latency nobody chose. Measured with a 20-second flush age, entries reached
a polling client in 0.03–0.64 s instead of not at all until the flush. The
segment is appended to the chunks only where it BEGINS where they end: a
flush landing between the ring snapshot and the sap read leaves the bytes
between them in a chunk that answer never saw, and delivering the segment
anyway would report a position past them — a gap nothing downstream could
detect. Being one poll late is the cheap failure.

**Both migrations are lazy, so no store needs an operator step.** A v1
index is read with its numbers synthesized — the oldest surviving record is
0, a definition rather than an attempt to recover how many were dropped
before anyone counted — and a v2 *writer* rewrites it on open, before the
rings are opened for writing (temp + rename, so a crash simply leaves the
v1 file and it runs again). A pre-numbering cursor resolves its write time
to a chunk the way `query --from` does, with `n` **reset**: `n` counted
entries within a window, so carrying it across axes could skip entries
nobody received, where resetting re-delivers at most one chunk. Wrong twice
is recoverable; wrong once and skipped is not. Resolution is a pure function
of `(wl, rings)`, which is what lets the converted cursor be persisted only
after a successful send, never ahead of a durability proof.
