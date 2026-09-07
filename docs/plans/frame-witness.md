# A frame nothing witnesses

**Status: NOT BUILT, and it names a defect that is real today.** The write
ordering and the wal's seal recovery described below are what ships; the stage
marker, the recovery decision per case, and the tests are the work. Reproduced
against 0.32.0.

A chunk reaches disk as two writes: the compressed frame into the `.trunk`,
then its 56-byte record into the `.rings`. Data first, always. A crash between
them leaves a frame no record describes — and today, on three of the four paths
that can produce one, nothing on disk says what that frame is.

Found by the VM fuzz `collapse-head retention survives repeated kill -9`, which
is what it is for. Reproduced outside a VM at 2 in 60 rounds with a release
build (a debug build never hit it in 25 — the window is one `write_all_at`
wide).

## What one looks like

```
rings file          358128  (6394 records)
last record     seq=123876  comp_start=450464  comp_len=70
comp_size           450534   <- end of the last INDEXED frame
trunk file          450604
ORPHAN                  70 bytes past the index
```

Those 70 bytes are a complete, valid zstd frame holding 1,284 lines. `query`
reads the index and returns a correct prefix; `zstd -dc` reads the file and
returns 1,284 lines more.

⚠ **A later append makes it worse, not better.** The next frame is written at
`comp_size`; if it is shorter than the orphan, the orphan's tail survives
spliced after it, and `zstd -dc` then fails on the whole trunk:

```
trunk: 81 bytes          (unchanged: nothing truncates)
query:      AAAA-line-one / C
zstd -dc:   AAAA-line-one / C
  zstd exit=1   zstd: s.log.trunk: unsupported format
```

Stock-zstd recovery is a promise with its own test (`collapse: stock zstd -dc
still recovers the whole survivor`). This silently revokes it while timberfs
itself reads the store correctly and reports nothing.

## The order of writes

`append_windowed`:

1. fold the window in memory (`buffer_first_ms` = min, `buffer_last_ms` = max);
2. extend the in-memory buffer;
3. if not staged and the wal is on, `wal.append(first_ms, last_ms, data)` — a
   sap record `len | wf | wl | payload | crc32`, into a `BufWriter`;
4. at `chunk_size`, `flush_chunk`.

`sap_flush()` (one `write(2)` per batch) makes those bytes VISIBLE to a live
tail; `sap_sync()` fsyncs and is the DURABILITY point. The intakes that ack a
sender call the second one; the ones tailing a file that still exists call only
the first.

`flush_chunk`:

1. compress — touches no on-disk state, deliberately first, so a failure here
   leaves the sap live and unchanged;
2. `sealing = !staged && wal.is_some()`;
3. if sealing: `wal.sync()`, then `rename(.sap -> .sap.seal)`;
4. `trunk.write_all_at(comp, comp_size)` — **the frame lands**;
5. build the record, taking the window from the in-memory buffer;
6. if not staged: `rings.write_all_at(rec, rec_off)` — **the record lands**;
7. advance `comp_size`/`buffer_start`, clear the buffer and the window;
8. if sealing: fsync both files, start a fresh `.sap` based at the new
   `comp_size`, and only then unlink the seal.

Steps 4 to 6 are the window. Each step is the durable witness for the next
one's absence — which is the whole design, and why the fix is not to reorder
anything.

## Where the write window lives

**Without a wal** it exists only in memory until step 6 writes it into the
record. An orphan frame's window is therefore unrecoverable: the only copy died
with the process.

**With a wal** every entry's `wf`/`wl` is on disk from step 3, CRC-covered,
before the frame is written. `apply_entries` folds them back on recovery with
the identical min/max, so a recovered chunk gets exactly the window it would
have had.

## The wal path is already correct, and shows the shape of the answer

The seal is the witness. When the frame is written, `.sap.seal` holds exactly
that frame's entries — which is why `sync_wal_declaration` refuses to turn the
wal on mid-buffer: a segment's content must be exactly the next chunk's bytes.
Its header records the `base` it started at. So on open:

* `seal.base < comp_size` — the record landed; the seal is stale debris, unlink.
* `seal.base >= comp_size` — the flush never landed; replay the sealed entries,
  compress, write the frame at `comp_size`, write the record, fsync both.

That rewrite is byte-identical to the orphan already there (same bytes, same
level), so the state converges exactly, window included.

## The four cells

|                   | killed flush                       | crashed stage        |
|-------------------|------------------------------------|----------------------|
| **wal store**     | recovered, window and all          | orphan, no witness   |
| **non-wal store** | orphan, window unrecoverable       | orphan, no witness   |

Three are broken, and **on disk they are indistinguishable** — which is the
actual defect. It is not that the index disagrees with the trunk; it is that
the stage path and the non-wal flush path write a frame with no durable witness
of what it is.

`stage()` records its baseline in memory only:

```rust
self.staged = Some(StageBaseline { chunks, comp_size, buffer_start });
```

So a crashed stage cannot be told from a killed flush, and the two want
opposite treatment: adopting a killed flush's frame keeps real data, while
adopting a crashed stage's frame silently commits a delivery that was
explicitly all-or-nothing (`sink.rs` is the caller).

## What to build

**1. An on-disk stage marker — the one required change.** `stage()` writes it,
`commit_stage` and `abort_stage` unlink it. It need only carry the baseline
`comp_size` (the `chunks` count and `buffer_start` are derivable from the rings
and the last record). Recovery then has no ambiguity:

* marker present — truncate the trunk to the marker's `comp_size`. This is
  `abort_stage` semantics applied after a crash, which is what the caller asked
  for by staging.
* marker absent, tail is a complete decodable frame — a killed flush.
* tail is not a complete frame — a torn write, and genuinely unrecoverable;
  truncate.

Same shape as the `.trim` marker `reconcile_trim` already uses for an
interrupted collapse, and for the same reason.

**2. A killed flush on a wal store: keep adopting.** Already the case, via the
seal. No change beyond letting the marker take precedence.

**3. A killed flush on a non-wal store: discard, and say so.** The frame is
real data and this is the uncomfortable half, so the reason has to be better
than tidiness:

* A chunk needs a write window. `comp_start`, `comp_len`, `uncomp_start`,
  `uncomp_len` and `seq` are all recoverable from the frame and its neighbour;
  `first_write_ms`/`last_write_ms` are not, and their only copy is gone.
* A FABRICATED window poisons the write-time index, which is what chunk
  selection, age retention and cursors all run on. An absent chunk is a bounded
  loss; a chunk with an invented date is a wrong answer to every later query
  that crosses it.
* The loss is inside the stated contract. The non-wal trade is documented in
  `sap.rs` as "flush tiny chunks for durability, or lose up to `flush_age` on a
  crash", and this frame is inside that window. A store that wants the promise
  declares `wal`, where the window is on disk before the frame is.

⚠ It must be ANNOUNCED, not silent — one line naming the byte count, as the
mirror case already prints `dropping index record for truncated chunk`. A
store that quietly shortens itself on open is the thing nobody can debug.

**4. `open()` gets the missing direction.** It reconciles the index claiming
MORE than the trunk holds and never the reverse; `trunk.set_len(comp_size)`
belongs beside that loop, gated on the decision above rather than
unconditional, which is what the first draft of this got wrong.

## Why the non-wal path stays

Not for compatibility. The wal writes every entry twice — raw into the sap,
then compressed into the trunk — so on a store compressing 7.0x (11.2 GiB
logical, 1.60 GiB compressed, measured on a real access log) it turns 1.60 GiB
of writes into 12.8 GiB. Eight times the bytes, plus one fsync a second and two
metadata operations per chunk.

And for the file-tailing intakes the store is not the only copy: the source
file is still there and logrotate still keeps it, so a lost buffer is recovered
by re-reading it — which is what the follower position exists for. A wal there
is a third copy of bytes already on disk twice.

⚠ For those intakes the wal is a VISIBILITY switch, not a durability one:
`query --follow` tails the sap, so declaring it is how a rare and urgent log
becomes queryable as it lands rather than at the next flush. Durability is the
source file's job. The shipped `file.d` example does exactly this on a panic
log (`FLUSH_AGE=2s`, `wal=true`).

## Tests

One deterministic test per cell of the table, since the fuzz proves a cell is
reachable and not which one it was:

* killed flush, wal — the seal replays and the trunk converges byte-identically.
* killed flush, non-wal — the orphan is discarded, announced, and `query` and
  `zstd -dc` agree afterwards.
* crashed stage — the marker is honoured and the staged frames go, whether or
  not a wal is declared.
* torn tail — an incomplete frame is truncated rather than adopted.

The `zstd -dc` failure above is its own test: an orphan followed by a shorter
append must not be able to break stock recovery.

Keep the VM fuzz. It found this, and a deterministic suite only covers the
cells somebody thought of.

## Adjacent, not in scope

`file-intake` hardcodes `chunk_size: 256 * 1024` and exposes only `FLUSH_AGE`,
where `append` takes `--chunk-size`. Deliberate or not, it is the other half of
what bounds a crash's loss there, and worth deciding on its own.
