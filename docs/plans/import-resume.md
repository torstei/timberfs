# Resuming a source: where the store's data ends in the file

**Status: partly built.** Step 0, the aligned candidate with its byte
comparison, is in `import` and in the live importer. Step 2 is being built in its
simplest form, a scan of the file for the store's tail. The hint file and the
bisection are not built. It replaces how `import` and `import --follow` decide
where in a plain-text source to carry on.

A source file grows, is read, and is read again later. The question each time:
**which byte of the file is the first one the store does not have?**

## Today

Two paths answer it, differently.

- **`import`** resumes by offset when the file's first timestamp equals the
  store's first chunk's: it seeks to `size()` and `verify_prefix` decompresses
  every imported chunk and compares it with the same range of the file
  (`--quick`: first, middle and last chunk). O(store) per resume.
- **`import --follow` and file intake** (`catch_up`) resume by content: the
  store's lines from the file's first stamp to the end of the store go into a
  hash multiset, and the whole file is read with the lines it holds dropped.
  O(file) read, O(overlap) decompress, memory O(overlap lines).

What is wrong with both:

1. **The head is the anchor, and retention removes it.** `first stamp ==
   store's first` stops holding once the head is dropped, and `size()` is
   relative to the current head (the dropped bytes are counted separately, in
   the rings header), so the offset it seeks to is only right before the first
   drop.
2. **A regrown file costs the store, or the overlap, every time.**
3. **The first stamp is weak evidence of identity.** A fixed header line, a
   boot time, or a coarse stamp makes two different files agree.
4. **Both run under `Restart=always`, `RestartSec=2`.** Any exit, clean or not,
   is retried every two seconds, and each retry would repeat the resume.

## Principles

- **Content decides.** Never an offset on its own, never a stamp on its own.
- **The store's tail is the anchor.** It always exists; the head does not.
- **No line is skipped because of its timestamp.** The store holds the file's
  lines in the file's order, so "already imported" is monotone in *file
  position* even where stamps are not monotone in time. A stamp may locate a
  probe, and say a line is older than anything the store still retains; it
  never decides that a line is old.
- **A resume always ends in progress** (see Failure policy).
- **A hint is a guess that is verified.** Losing one costs time, not data.

## The resume

`B` is the store's last `N` bytes (`N` about 16 KiB, or what the store holds),
read from its last chunk or two. The answer is the offset in the file where `B`
ends.

**Step 0, the aligned candidate.** A store fed only by this file, from byte 0,
holds the file's bytes verbatim, so the file offset of its end is the tape
position `dropped + size()`. Compare `B` with the file just before that offset.
Equal: resume there. One chunk and `N` bytes read.

That alignment holds only until the first rotation. After it the same log is a
different file, whose byte 0 sits at whatever the tape end was when it was
opened, so the common case (one log, one store) stops being aligned after one
rotation.

**Step 1, the hinted candidate.** The hint file records, per source file, the
tape position of its byte 0 (`tape_start`), and where the last run ended (a file
offset and the tape position it matched). The candidate is the tape end minus
`tape_start`; compare `B` just before it. This covers rotation, and anything else
that left the store and the file misaligned. It is only written by a run that
verified its own position, so it never turns a guess into a belief.

**Step 2, the search.** For everything else: a store with no hint yet (the first
run after upgrading), a hint that no longer matches, a file regenerated in place,
a store that lost lines.

The first version is a scan: read the file in blocks and look for `B`; the end of
the match is the cutoff. It is bounded memory and one pass over the file, which
is what the fallback costs today, and it needs no timestamp and no copy of the
store. It covers a store with a gap (the tail is contiguous in the file wherever
the gap is), a rotated log (the tail is in the new file), and a store that lost
its unflushed tail. It stops at the first match, so a repeated block errs towards
a duplicate and not a loss. The needle is the store's last 16 KiB; a gap closer
to the end than that makes it straddle the gap and it is in no file, so the last
KiB is tried next. A store holding less than a KiB has no tail worth searching
for. The bisection below only makes the scan cheaper on a very large
file.

- *Head check, when the store still has its head* (`dropped == 0`): compare the
  first couple of KiB of the file with the store's. One chunk. It says "this
  is not the file" or "this is it" without depending on one record; a store
  whose head was retained away skips it and relies on the tail.
- *Narrow by bisection.* Probe at a file offset: seek, skip to the next record
  start (a line that parses as a stamp), classify it:
  - older than the store's first retained stamp: before the cutoff;
  - found in the store: before the cutoff;
  - anything else: after the cutoff.

  Narrow until the region is small (about 1 MiB). A file of a few chunks skips
  this and is scanned whole.
- *Byte-match inside the region.* Search it for `B`; the end of the match is
  the cutoff. Verify all `N` bytes.

A record is looked up in the store by its stamp (the rings select the chunks
whose write window can hold it, widened by the clock skew), then the `.grain`
narrows them if the store has one, then an exact comparison. About
`log2(file / 1 MiB)` probes, each a chunk or a few.

The last line the store holds may be partial (a live file read to EOF), so the
final match is on bytes, not records: the resume then continues in the middle
of a line.

## Failure policy

Nobody is watching, and the units restart on any exit. An exit is therefore not
a report, it is a loop that repeats the expensive part. So:

**Ambiguity never exits.** `B` not found in the file means a new generation of
it (copytruncate and regrow, a rewrite, a different file), and each case has an
action that makes progress:

| Situation | Action |
|---|---|
| `B` found | resume at the cutoff |
| `B` not found, the file's first stamp is after the store's last | append the whole file: there is no overlap to dedup |
| `B` not found, the file overlaps what the store retains | read it from the start; a line older than the store's first stamp is not imported (retention dropped that history on purpose), a line newer than its last is, and a line between is dropped only if the store's last `W` already holds it (a bounded set), because a duplicate is the cheaper mistake |

The old startup got the overlap case wrong: it looked at the file's first stamp,
found it older than the store's head, and skipped the whole file, then followed
from its end. Only the lines older than the head are retention's to drop.

One line to stderr (the journal) and a note in the hint file say which. The
hint is then written, so the next start takes Step 1: the fallback runs once
per change of generation, not once per restart.

**What can change while the process runs is waited for, not exited on.** A
followed file that does not exist yet already is: the tail waits for it. A file
with no timestamp *yet* is the same kind of thing, and today it is not: the
first stamp is looked for in the first 1000 lines and its absence is an error,
which exits, which the unit restarts every two seconds, each time re-scanning
the same lines until a stamp turns up. It recovers by accident and meanwhile
loops. Stopping the unit instead would be worse: a file that gains a stamp
later would never be looked at again.

So for a followed source, no stamp yet is a state, kept in the hint file and
not in memory:

- The follower reads on looking for the first stamp and keeps only a position:
  *awaiting a stamp, scanned through offset X*. The lines before the first stamp
  are not held. They are a byte range of the file, and the file already has them.
- What stays in memory is constant, whatever the file holds: the descriptor, the
  read buffer, X, the first 256 bytes of the line being read (all a stamp is
  looked for in; once they hold none, the rest of that line is not kept, only X
  moves on), and the hint. Today's `Stamper.leading` holds up to 1000 whole
  lines, bounded by their number and not by their size.
- The hint records it (identity, size, X, and a hash of the file's first KiB),
  refreshed as the scan advances, so a restart continues from X and does not
  read the same hundred megabytes again.
- When a stamp turns up at offset S, the range `[0, S)` is streamed from the file
  into the store with that stamp's time, which is what holding the lines used to
  do, and the follower carries on from S. A garbage or half-written first line
  is simply part of that range.
- If the file was replaced or truncated in the meantime (identity, size or head
  hash no longer match) the hint is dropped and the scan starts over.
- The wait is **visible**. Nothing is imported while it lasts, and a file that
  never carries a stamp looks exactly like a quiet one. So: a note when the scan
  passes 100 MiB and at each doubling, the state in the hint file, and in `info`.
- A live source that legitimately has no stamps declares arrival stamping and is
  imported as it is read. That is a declaration and not a fallback: while a
  file can still gain a stamp, the default is to wait, for ever if need be,
  rather than guess a time. **Decided.**
- The hint also records which timestamp declaration it scanned with. A changed
  regex or format makes it stale, so fixing a declaration takes effect on a file
  that is still there, without anyone clearing state.

**A file that is closed without ever carrying a stamp is different, because
waiting stops being free.** While running, the tail holds the file's descriptor,
so a rotation leaves it reading the rotated file to its end before it reopens
the new one. That moment is the last time anything reads that file: logrotate
deletes it in time, and at startup only `.1` and `.0` are looked for at all. A
wait that cannot end is a loss that has not happened yet.

So a file closed while awaiting a stamp is imported, stamped with its
modification time: the end of its content's life, and the same reference the
extractor already uses for a stamp with no year. The time is approximate (all
of it at that instant) and the content is kept. One note says so: which file,
how many bytes, and the time they were given. The hint records it, so a restart
does not do it twice. **Decided.**

A one-shot `import` of a file with no stamp has no live edge to stamp by
arrival, so there it is a usage error: declare the format, or ask for arrival
stamping.

**Only what a restart cannot change stops the unit.** A timestamp regex that
does not compile, an unknown format: retrying cannot help. These exit with a
distinct status (78, `EX_CONFIG`), and the units gain
`RestartPreventExitStatus=78`. Transient faults (a writer holds the lock, the
disk is full) exit non-zero as now, because a restart is the right answer to
them.

## Hints

`<name>.resume`, a sidecar beside the store, listed in `format::every_path`.
Derived data under the sidecar contract: deleting it costs a search.

Per source: its identity (dev, ino, path), and either *resumed* (the file offset
and tape position the last run ended at) or *awaiting a stamp* (the offset
scanned through, the size, a hash of the first KiB); when; and the note of the
last fallback taken. Written
atomically, when a run ends and (for `--follow`) when the tape end moves at a
flush tick.

Not the `.bark`: that holds what was *declared* and travels with the store; a
hint is a volatile guess about one file on one host.

## What this replaces

- `verify_prefix`, and `--quick` with it (still accepted, ignored).
- The `first stamp == store's first` gate.
- `overlap_line_counts` for a file regrown in place. It stays for two
  genuinely different sources that overlap, bounded by `W` under the same
  policy.
- `catch_up`'s read-everything-and-dedup for the same case: both importers
  share one resume.

## Open questions

- `N`, the region size, and `W`.
- A source with no parseable stamps: no probe can be classified, and the rings
  cannot select chunks for it, so only Steps 0 and 1 apply, then the whole file
  is scanned for `B`.
- A store fed by a *set* of files (`import app.log*`, day files) is an initial or
  bulk import: sources ordered by first stamp, appended after the store's end, or
  deduplicated where they overlap. That handling stays as it is, and re-running
  it rescans every overlap; a per-source "unchanged since last run" check in the
  hint would turn that into a stat, later. Several *live* files feeding one store
  is not a use: the writer lock allows one writer per store, and the resume
  assumes a single live source, with the files its rotations leave behind.
- Whether the note belongs in the hint file or also in `info`.
