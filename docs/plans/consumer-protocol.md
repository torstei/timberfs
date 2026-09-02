# The consumer protocol: timberfs holds the position, the consumer says how far

**Status: design.** Nothing here is built. What it rests on is: the records
stream with its per-entry `offset`/`len` and `id`, the per-store positions a
selection already keeps, and the poll loop that fills them. See
[follower-selection.md](follower-selection.md) for those, and
`timberfs-records(5)` for the stream this extends.

A registered **follower** is the object: a selection, a destination, a
position per store. Its **consumer** is the program it feeds. This note is
the contract between them.

⚠ Not "consumer": `consumer.rs` is already the engine that writes a records stream
INTO a store, so the word points the other way in this tree. `consumer` is
what `cursor.rs` has always called this party — `Cursor { consumer }`,
`struct Consumer`, `consumers_in` — and it is written into every cursor
file, so it is the vocabulary already in use rather than a new one.

## The position is timberfs's, and a consumer reports

Every position advances on a proof of delivery, and only the thing that
talks to the destination has one. The shipped answer to that was to let the
shipper own the whole path — read, position and destination — so `otlp` and
`frames` are binaries in this tree and a new destination means a new one.

The cost is not the binary. It is that the position file is where identity
keying, atomic batch advance, fail-closed reads and the RETENTION FLOOR
live, so a program that writes it can get retention wrong silently. A
program that only *reports* cannot.

So: timberfs owns the position and runs the loop; a consumer says how far to
move it. One contract for every consumer, including the ones we ship.

## The watermark means "do not send me these again"

Not "these are safe", and the difference is what removes the error
taxonomy.

A receiver refusing an entry for being outside its ingestion window is
refusing it PERMANENTLY. Under a positive-acknowledgement rule — advance
only on confirmed delivery — one such batch wedges the follower forever. So
the report cannot be "these succeeded". It is "I am done with everything up
to here", and what the consumer actually did with the data is the consumer's
business and its journal's:

  * the receiver is down → report nothing → the same entries come again,
    for as long as it takes; the store is the buffer, which is what
    retention is the budget for;
  * an entry is too old, or malformed → report PAST it → never resent, and
    the consumer's log says why;
  * something unexpected — disk full, a garbage response → report nothing.

⚠ This also disposes of OTLP's `partialSuccess`, which carries a COUNT of
rejected records and no identities. There is no subset to retry, so it is
an accounting line and not a position decision. A protocol able to say
"record 10 of store 4242 failed" would be more precise than any destination
we have can supply.

## Two messages, one direction

Consumer to timberfs, framed as `timberfs-records(5)` is — RS-marked,
US-separated, NUL-terminated — so there is one framing discipline and one
parser shape at both ends. Nothing travels the other way but the stream
itself.

    hello      v=1  reads=records          (once, before anything else)
               [ id=<store> offset=<n> ]*  what it ALREADY holds — a HINT
    progress   id=<store>  offset=<n>      (whenever it likes)
    note       [ id=<store> ] [ offset=<n> ]  text=<json string>

**No hello, no run.** timberfs refuses rather than silently falling back to
advancing on write-out, because "every consumer implements this" is what makes
silence unambiguous: a consumer that has not reported has not got there.

**The watermark is a number the consumer was handed.** An entry record states
`offset` and `len`, and the runs chain — the end of one entry is the start
of the next — so a consumer's watermark is the last accepted entry's
`offset + len`, per `id`. Same quantity the `position` record reports:
there is one kind of position in this protocol.

**And ONE unit, whatever the consumer reads: the absolute offset on the
store's tape** — what has ever left the store plus where the bytes sit in
what remains. Retention cannot move it, `remove_head` rebasing the chunk
offsets down by exactly what it grows `dropped` by, so the number is the
count of bytes ever written before that point. A CHUNK boundary is simply
an offset that happens to land on one; `view … at chunk N` and
`at offset N` have always been two views of the same axis.

⚠ It is therefore also a SHARED address across a replica: chunks move
verbatim so their uncompressed lengths match, and replica mode preserves
numbering, so `dropped + uncomp_start` is the same absolute number at both
ends. A receiver can state its coverage in it. ⚠ For a replica only — a
receiver that numbers its own chunks shares no address, which is what
`receiving-end.md` means by an ingesting writer numbering its own.

**`hello` may carry initial watermarks**, which is how a consumer whose
destination knows more than we do tells us where to begin. That is exactly
the frames handshake — the receiver answering with the coverage it holds —
expressed as the same message, so replication stops being an exception to
the model. ⚠ Not served in a first cut: see the deferred list.

⚠ **A claim is a HINT, not a proof, so it is honoured only where timberfs
has nothing recorded for that store.** A number cannot demonstrate that the
bytes are held; honouring one over a recorded position would skip what was
never delivered, silently. Where there IS no recorded position the claim
costs nothing we know about and saves everything: a rebuilt follower
pointed at a receiver already holding terabytes must not re-ship them. A
claim BEHIND our position is ignored too — a rewind is safe but pure waste
— and where a receiver really does hold what we then re-send, it
deduplicates on `(origin, seq)`, chunks being addressed. The cost is
bandwidth, never data.

So the claim joins a question this design already has rather than adding
one. Where a store with no position starts is decided in this order:

    a recorded position  >  the consumer's claim  >  --follow-from

The claim outranks the declared policy because it is specific knowledge
about THAT store, and yields to a recorded position because that is
knowledge we own.

**Batching is the consumer's own business.** It reports when it likes; timberfs
bounds its own reads for its own reasons. Backpressure is the pipe.

## `note`: because silence is now a legitimate state

Making the watermark mean "do not send me these again" made SILENCE
meaningful — a consumer that has not reported has not got there — so a stalled
follower is a state the design intends. `follower status` can see that it
is stalled and cannot see WHY, and only the consumer knows. `note` is that
half.

    note  id=499544f0-…  offset=33724753900
          text="400 from collector.internal: entry refused"

  * **Opaque to timberfs**, displayed verbatim, never parsed — the same
    reason the watermark carries no error codes: a taxonomy here would be a
    second thing to keep in step with what consumers actually say.
  * **Persisted, or it is pointless.** `follower status` is a DIFFERENT
    PROCESS, so an in-memory note is invisible to the thing that needs it.
    It belongs in `positions.json`, which is already atomic, already
    per-store, and already what `status`, `list` and the interest axis read.
  * ⚠ **Writable when nothing advanced**, which is a real change: positions
    are written when a batch is accepted, so a stalled follower would
    otherwise never write the note explaining why. A note alone justifies a
    write — deduped by text, so a consumer retrying once a second produces one
    note and one write, not sixty.
  * **Bounded by construction.** One per store, plus one follower-wide (an
    absent `id` being "about me, not a store" — an unreachable endpoint is
    not per store). Replaced, never accumulated; history is what the
    journal is for.
  * ⚠ **Kept in its OWN map, not beside the position.** A note and a
    position have different lifetimes, and the store a consumer most needs
    to explain is the one it has never got past — which has no position to
    hang a note on. Keeping them together lost the store's name from
    exactly the note that needed it most, turning it into the
    consumer-wide one.

⚠ **`offset` in a note is an ADDRESS.** `timberview` opens
`timber://host/<store-id>#offset=N`, so a note naming the entry a consumer
choked on gives `follower status` something an operator can open the log
AT — that exact entry, in the pager — rather than "go read the consumer's
journal and correlate". It is the same quantity as a watermark and a
position, which is the point of there being one kind of position here.

⚠ But `status` prints a STORE and an OFFSET, not the URL: the host in such
an address is whatever name the READER reaches this machine by, and this
machine does not know it. `gethostname()` is conventionally the short name,
which is why two hosts in different environments present the same one —
[receiving-end.md](receiving-end.md)'s door 4. Composing the URL is the
reader's job; ours is to say which store and where in it.

Named `note` and not `status` because `stream-end` already carries a
`status` FIELD, whose values are `exhausted`/`limited`. Two `status`es
meaning different things is a wart to not acquire. It also matches
`crate::note!`, already the vocabulary for a line addressed to an operator.

## What the stream carries

**Attribution is unconditional.** `timberfs-records(5)` today omits `src`
and `id` where "a read of one store attributes nothing, because there is
nothing to tell apart" — right for a person or a one-off pipe, wrong for a
protocol: a consumer written against a three-store selection would break the
day someone points it at one. A follower stream runs in the multi-store
shape whatever matched. A 36-byte id on every entry is the price; if it
ever bites, the fix is a short per-stream handle assigned in `source`, not
a conditional.

**`source` carries the store's labels**, as one compact-JSON field:

    source  path=/var/log/timberfs/web01/web01.log  id=499544f0-…
            labels={"host":"web01","service":"apache"}

One field and not flattened `label.*` keys, for a framing reason rather
than taste: labels are open-ended, so a value could hold `0x1f` or NUL and
break the framing, and a label key could collide with `path`/`id`/`kept`/
`total`.

⚠ Which generalises into the format's one encoding rule: **any field whose
value this protocol does not constrain is a JSON string.** `labels` and a
note's `text` are both such fields, and so is the next one somebody adds.
A JSON string cannot contain a raw control character, so content cannot
break the framing — by construction rather than by hoping nobody labels a
store oddly or puts a tab in an error message. The labels are `bark::provenance` — the one place that says what
counts as a label — so settings (`wal`, `retain`, `index`) are not in it:
they are operational and not a consumer's business.

⚠ **`source` is emitted three times over**, and a consumer needs all three:

  * at stream start, one per store the selection then matches — so a consumer's
    picture is complete from the first bytes and it reconstructs nothing;
  * when a store JOINS mid-run, the selection being re-resolved every poll;
  * when a store's LABELS CHANGE, because labels are mutable and a consumer
    that looked them up itself would attribute entries to a store's later
    identity.

⚠ **A `source` record is a FLUSH BOUNDARY for that store.** A batching consumer
can be mid-batch when labels change; adopting them at once would ship
entries that arrived under the old labels attributed to the new ones,
silently. Cheap to obey, impossible to guess, and the reason this is
written down rather than left to three consumers to each infer.

Nothing is emitted when a store LEAVES. The consumer stops seeing that id and
its map entry goes quiet; a routing consumer that must close a per-store output
wants a withdrawal record, which is a thing to add when one exists.

**The polls are spliced into ONE stream.** Each internal read is a complete
bounded answer, but a consumer must see what `query --records --follow` looks
like: `stream-start` once, `source` as stores join, entries indefinitely,
and NO `stream-end` — whose absence is already this format's honest "still
live" marker. So timberfs strips the per-poll brackets and does not forward
its own `position` records: the consumer's watermarks are the authority, and two
authorities for one number is a bug waiting to be written.

## The state, and where each part lives

| | lives in | survives a restart |
|---|---|---|
| positions (tape offset, and the chunk it lands in, per store) | `positions.json` | **yes** — or every store is re-shipped |
| the consumer's last `note`, per store and one for itself | `positions.json` | yes — `status` is another process |
| labels last ANNOUNCED, per store | the loop's memory | no — and must not |
| the consumer's own copy of them | the consumer's memory | no — rebuilt from stream start |

The announced-labels map is what detects a change: the poll resolves the
selection and already reads each matched store's manifest, so the fresh
labels are in hand at no extra I/O, and a comparison against the last
announced ones costs a small map compare. About 150 bytes a store, so under
a megabyte at five thousand.

⚠ **Not a revision counter in the manifest.** Remembering 8 bytes instead
of the labels sounds cheaper and can LIE: a hand-edited `.bark`, a restore,
or a writer that changes labels without bumping leaves the number unchanged
while the labels moved, and the consumer is never told. That is the silent
direction. Comparing the labels cannot be wrong about the labels — the same
reason the interest axis refuses to gate on an mtime.

⚠ And not a hash of them either: a collision is a missed announcement,
which is silent, and the saving is a fraction of a megabyte.

**It does not need persisting because the consumer's copy has the same
lifetime.** Both are born at stream start and die with the stream, so they
cannot get out of step across a restart. Which is an argument FOR the
lifecycle below rather than a consequence of it: were the consumer to outlive
the follower, the announced state would have to be persisted and reconciled
against a consumer that might have missed an update.

## Filtering goes on the far side of the reporter

A watermark means "do not send me these again", not "these were
delivered" — so a consumer may DROP whatever it likes, as long as it
reports past what it dropped. That is the same rule that makes a
permanently-refused entry work, and it means filtering is safe wherever
the party doing it is also the party reporting.

⚠ What is not safe is a filter UPSTREAM of the reporter. A
`timber-filter … | timber-otlp` between the feeder and the consumer drops
entries the consumer then never sees and never acknowledges, so the
position never advances past them and the same entries arrive for ever.
The filter belongs after — inside the consumer, or beyond the
destination — never between.

## One store in trouble must not cost the others

A consumer that will not take one store's entries — refused by its
destination, or a destination wedged for that stream alone — must leave every
other store shipping at full speed. Two things make that so, and the second
was a defect until it was measured.

**A store with anything UNACKNOWLEDGED is parked.** Its recorded position only
moves when the consumer acknowledges, so a read starting there hands back the
entries already in flight — duplicates, filling the shared entry cap, on
every poll, for as long as the trouble lasts. Measured: a consumer taking
entries and acknowledging none held the process at **99% of a core** without
the park and **0%** with it, and the test written for it does not merely fail
without the park — it never finishes.

**The loop waits for a REPORT, not for the clock.** With everything parked
there is nothing to send, and the thing worth waking for is an
acknowledgement; so a consumer that catches up is served at once rather than
at the next tick, and the poll interval never becomes the ceiling on one
store's throughput.

⚠ So the depth is one outstanding batch per store, and pipelining deeper is
not a matter of raising a number: it needs a SENT offset per store, kept
beside the acknowledged one and read from instead of it. Otherwise "further
ahead" means "the same entries again", which is what the park exists to stop.

What the stalled store then costs is retention, not throughput: its position
stops moving, so a `retaining` follower holds everything from there — which
is the promise, with `retain_size` as the backstop — and its data waits until
the trouble is fixed, at which point it resumes from exactly where it stopped.

## The command is recorded, not inspected

A follower declares a COMMAND, and `create` does not check it. Not as a
convenience — the check would be worse than its absence:

  * it cannot be enforced later. The binary can be deleted, replaced or
    have its exec bit removed after registration, so a create-time check
    covers one case and reassures about every other;
  * every way of getting it wrong already fails loudly at start. A missing
    binary is a spawn error, `/bin/false` is «the consumer exited exit
    status: 1», and a program that says no hello is refused by name;
  * it would make `create` a command with SIDE EFFECTS — it would run a
    program the operator named — and `--dry-run` would then have to
    either skip it, and be unfaithful, or perform it, and be a dry run
    that executes something;
  * and it buys nothing the documented incantation does not: `--enable
    --start` reveals a broken command at once.

⚠ Including the retention consequence, which is an ACCEPTABLE failure and
not an argument for checking: a `retaining` follower whose command is
broken never runs, and one that has never run holds everything. That is
the state the flag exists to express — it protects a follower deployed
before it first runs — and `list` reports it, `holds_everything` being a
first-class question for exactly this reason.

So the command is recorded verbatim, the same rule the registry already
applies to a shipper's flags and `visena-timberfs` to a query document:
what is not ours to interpret is passed on unread.

## Lifecycle

The consumer is a child of the follower. If the consumer dies the follower exits
non-zero and systemd restarts the unit — one lifecycle, one place, and a
fresh stream every time, rather than a re-spawn that has to decide what a
half-consumed consumer's state meant.

## A consumer may be remote, and the transport needs no design

The contract is two file descriptors, so a remote destination is
`-- ssh archive01 my-consumer`: the protocol rides its stdin and stdout
unchanged. The same arrangement timbersh's `cmd` targets use for the same
reason.

⚠ And `retaining` WORKS for a remote destination, which the earlier reading
of this got wrong. The loop and the positions are local; only the
destination is elsewhere, so the host-local interest axis has everything it
needs. What stays ephemeral is a remote READER — one pulling from another
host's stores — which is a different thing and not a registered follower.

## A followed stream's EOF is not truncation

`timberfs-records(5)` treats end-of-input without a `stream-end` as
truncation, and it is right to: for a bounded answer that absence means
the producer died. But a FOLLOWED stream carries no `stream-end` by
design — its absence is the format's own «still live» marker — so a
consumer reading one to the end would report the feeder's ordinary
shutdown as a broken stream.

`stream-start` says which kind of stream this is (`follow=1`), so the
distinction is read from the wire rather than assumed. A consumer that
gets this wrong fails at exit rather than at work, which is why it is
worth stating: the failure looks like a bug in the feeder.

## The trivial consumer we ship

A consumer that runs a command per record, with the exit code as the report:

    timberfs follower create alert --select '[service=~apache-.*]' \
        --type exec -- /usr/local/bin/page-someone

  * `0` — accepted; move past it.
  * `65` (`EX_DATAERR`) — will never work; move past it anyway, counted
    and logged.
  * anything else — not accepted; do not move; try again.

Three codes and not two, for the same reason a too-old rejection needed a
skip: with only accept and retry, one poisonous entry stalls the follower
forever. It is the record-granular form of what the watermark does for a
batch.

⚠ A fork per record is an honest property of THIS consumer, not of the
protocol. Reach for it for a watcher, a `logger`, an escalation script,
`/dev/null`; not at fifty thousand lines a second. A long-lived consumer
reporting watermarks needs no forks at all — which is why the exit-code
model is one consumer rather than the contract.

This is also what makes the **watchers** direction (ROADMAP) a registered,
resumable thing rather than a shell pipeline: its stated MVP is
`query --follow … | timber-filter … | your-action`, and what a built-in
form was said to add is configuration and durability. This is both.

## Deferred, and named rather than implied

  * **`chunks` granularity** — and it is the EASIER payload, not the
    harder one, which an earlier draft of this had backwards. Resuming by
    offset has no partial case for a chunk: it is wholly before the
    position or it is not, where an entry needed the mid-chunk skip. Three
    small things are missing, each mirroring the entries path: the TAPE
    offset on the `chunk` record (keeping `uncomp_start`, which a consumer
    reassembling frames needs); `write_chunks_framed` taking the cursor map
    it currently has no parameter for; and `position` records from the
    chunks path, which emits none — one per store EXAMINED, or the next
    page rescans the quiet ones. ⚠ Which is also why a chunks query can
    only be SEEKED (`from_chunk`) and not paged today, and why only a
    records answer carries a completeness marker.
  * **`frames` joining the protocol.** Not a unit problem — see the shared
    address above. What is left is that it ships sidecars and a manifest,
    and wants the receiver's own dedup.
  * **A withdrawal record** for a store leaving a selection.
  * **Per-destination watermarks from one consumer** — a routing consumer where one
    destination is down and the others are not. The message shape already
    allows a watermark per store; what is missing is a reason to.
  * **`text` granularity is deliberately NOT in the protocol.** A consumer must
    be able to report an address, and raw bytes carry none. Rendering
    entries as text for a command is the `exec` consumer's own business.

## What it costs

A fourth wire format beside `timberfs-records(5)`, the query document and
the frames wire — with a version, a man page and a compatibility story.
What it buys is the ROADMAP's own stated goal reached properly: "today
resuming means linking `cursor.rs`, i.e. writing our own shipper, and after
it anyone's script is one" — without letting third parties write the
position file.

And it reworks the shipped ownership. `timber-otlp` becomes a stdin consumer
that renders, posts and reports, losing its `--select`/`--positions`/
`--cursor` machinery; the loop moves into a `timberfs` subcommand that owns
the read, the selection, the fairness rotation and the positions.
`ship.rs`, `Positions`, `read_forward`, the resource grouping and the
round-robin all survive and are re-homed. Better done before `follower
create` is built on the current ownership than after.
