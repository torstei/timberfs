# The sink protocol: timberfs holds the position, the sink says how far

**Status: design.** Nothing here is built. What it rests on is: the records
stream with its per-entry `offset`/`len` and `id`, the per-store positions a
selection already keeps, and the poll loop that fills them. See
[follower-selection.md](follower-selection.md) for those, and
`timberfs-records(5)` for the stream this extends.

A registered **follower** is the object: a selection, a destination, a
position per store. Its **sink** is the program it feeds. This note is the
contract between them.

## The position is timberfs's, and a sink reports

Every position advances on a proof of delivery, and only the thing that
talks to the destination has one. The shipped answer to that was to let the
shipper own the whole path — read, position and destination — so `otlp` and
`frames` are binaries in this tree and a new destination means a new one.

The cost is not the binary. It is that the position file is where identity
keying, atomic batch advance, fail-closed reads and the RETENTION FLOOR
live, so a program that writes it can get retention wrong silently. A
program that only *reports* cannot.

So: timberfs owns the position and runs the loop; a sink says how far to
move it. One contract for every sink, including the ones we ship.

## The watermark means "do not send me these again"

Not "these are safe", and the difference is what removes the error
taxonomy.

A receiver refusing an entry for being outside its ingestion window is
refusing it PERMANENTLY. Under a positive-acknowledgement rule — advance
only on confirmed delivery — one such batch wedges the follower forever. So
the report cannot be "these succeeded". It is "I am done with everything up
to here", and what the sink actually did with the data is the sink's
business and its journal's:

  * the receiver is down → report nothing → the same entries come again,
    for as long as it takes; the store is the buffer, which is what
    retention is the budget for;
  * an entry is too old, or malformed → report PAST it → never resent, and
    the sink's log says why;
  * something unexpected — disk full, a garbage response → report nothing.

⚠ This also disposes of OTLP's `partialSuccess`, which carries a COUNT of
rejected records and no identities. There is no subset to retry, so it is
an accounting line and not a position decision. A protocol able to say
"record 10 of store 4242 failed" would be more precise than any destination
we have can supply.

## Two messages, one direction

Sink to timberfs, framed as `timberfs-records(5)` is — RS-marked,
US-separated, NUL-terminated — so there is one framing discipline and one
parser shape at both ends. Nothing travels the other way but the stream
itself.

    hello      v=1  reads=records          (once, before anything else)
               [ id=<store> offset=<n> ]*  what it ALREADY holds — a HINT
    progress   id=<store>  offset=<n>      (whenever it likes)
    note       [ id=<store> ] [ offset=<n> ]  text=<json string>

**No hello, no run.** timberfs refuses rather than silently falling back to
advancing on write-out, because "every sink implements this" is what makes
silence unambiguous: a sink that has not reported has not got there.

**The watermark is a number the sink was handed.** An entry record states
`offset` and `len`, and the runs chain — the end of one entry is the start
of the next — so a sink's watermark is the last accepted entry's
`offset + len`, per `id`. Same quantity the `position` record reports:
there is one kind of position in this protocol.

**`hello` may carry initial watermarks**, which is how a sink whose
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

    a recorded position  >  the sink's claim  >  --follow-from

The claim outranks the declared policy because it is specific knowledge
about THAT store, and yields to a recorded position because that is
knowledge we own.

**Batching is the sink's own business.** It reports when it likes; timberfs
bounds its own reads for its own reasons. Backpressure is the pipe.

## `note`: because silence is now a legitimate state

Making the watermark mean "do not send me these again" made SILENCE
meaningful — a sink that has not reported has not got there — so a stalled
follower is a state the design intends. `follower status` can see that it
is stalled and cannot see WHY, and only the sink knows. `note` is that
half.

    note  id=499544f0-…  offset=33724753900
          text="400 from collector.internal: entry refused"

  * **Opaque to timberfs**, displayed verbatim, never parsed — the same
    reason the watermark carries no error codes: a taxonomy here would be a
    second thing to keep in step with what sinks actually say.
  * **Persisted, or it is pointless.** `follower status` is a DIFFERENT
    PROCESS, so an in-memory note is invisible to the thing that needs it.
    It belongs in `positions.json`, which is already atomic, already
    per-store, and already what `status`, `list` and the interest axis read.
  * ⚠ **Writable when nothing advanced**, which is a real change: positions
    are written when a batch is accepted, so a stalled follower would
    otherwise never write the note explaining why. A note alone justifies a
    write — deduped by text, so a sink retrying once a second produces one
    note and one write, not sixty.
  * **Bounded by construction.** One per store, plus one follower-wide (an
    absent `id` being "about me, not a store" — an unreachable endpoint is
    not per store). Replaced, never accumulated; history is what the
    journal is for.

⚠ **`offset` in a note is an ADDRESS.** `timberview` opens
`timber://host/<store-id>#offset=N`, so a note naming the entry a sink
choked on gives `follower status` something an operator can open the log
AT — that exact entry, in the pager — rather than "go read the sink's
journal and correlate". It is the same quantity as a watermark and a
position, which is the point of there being one kind of position here.

Named `note` and not `status` because `stream-end` already carries a
`status` FIELD, whose values are `exhausted`/`limited`. Two `status`es
meaning different things is a wart to not acquire. It also matches
`crate::note!`, already the vocabulary for a line addressed to an operator.

## What the stream carries

**Attribution is unconditional.** `timberfs-records(5)` today omits `src`
and `id` where "a read of one store attributes nothing, because there is
nothing to tell apart" — right for a person or a one-off pipe, wrong for a
protocol: a sink written against a three-store selection would break the
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
they are operational and not a sink's business.

⚠ **`source` is emitted three times over**, and a sink needs all three:

  * at stream start, one per store the selection then matches — so a sink's
    picture is complete from the first bytes and it reconstructs nothing;
  * when a store JOINS mid-run, the selection being re-resolved every poll;
  * when a store's LABELS CHANGE, because labels are mutable and a sink
    that looked them up itself would attribute entries to a store's later
    identity.

⚠ **A `source` record is a FLUSH BOUNDARY for that store.** A batching sink
can be mid-batch when labels change; adopting them at once would ship
entries that arrived under the old labels attributed to the new ones,
silently. Cheap to obey, impossible to guess, and the reason this is
written down rather than left to three sinks to each infer.

Nothing is emitted when a store LEAVES. The sink stops seeing that id and
its map entry goes quiet; a routing sink that must close a per-store output
wants a withdrawal record, which is a thing to add when one exists.

**The polls are spliced into ONE stream.** Each internal read is a complete
bounded answer, but a sink must see what `query --records --follow` looks
like: `stream-start` once, `source` as stores join, entries indefinitely,
and NO `stream-end` — whose absence is already this format's honest "still
live" marker. So timberfs strips the per-poll brackets and does not forward
its own `position` records: the sink's watermarks are the authority, and two
authorities for one number is a bug waiting to be written.

## The state, and where each part lives

| | lives in | survives a restart |
|---|---|---|
| positions (offset, chunk, per store) | `positions.json` | **yes** — or every store is re-shipped |
| the sink's last `note`, per store and one for itself | `positions.json` | yes — `status` is another process |
| labels last ANNOUNCED, per store | the loop's memory | no — and must not |
| the sink's own copy of them | the sink's memory | no — rebuilt from stream start |

The announced-labels map is what detects a change: the poll resolves the
selection and already reads each matched store's manifest, so the fresh
labels are in hand at no extra I/O, and a comparison against the last
announced ones costs a small map compare. About 150 bytes a store, so under
a megabyte at five thousand.

⚠ **Not a revision counter in the manifest.** Remembering 8 bytes instead
of the labels sounds cheaper and can LIE: a hand-edited `.bark`, a restore,
or a writer that changes labels without bumping leaves the number unchanged
while the labels moved, and the sink is never told. That is the silent
direction. Comparing the labels cannot be wrong about the labels — the same
reason the interest axis refuses to gate on an mtime.

⚠ And not a hash of them either: a collision is a missed announcement,
which is silent, and the saving is a fraction of a megabyte.

**It does not need persisting because the sink's copy has the same
lifetime.** Both are born at stream start and die with the stream, so they
cannot get out of step across a restart. Which is an argument FOR the
lifecycle below rather than a consequence of it: were the sink to outlive
the follower, the announced state would have to be persisted and reconciled
against a consumer that might have missed an update.

## Lifecycle

The sink is a child of the follower. If the sink dies the follower exits
non-zero and systemd restarts the unit — one lifecycle, one place, and a
fresh stream every time, rather than a re-spawn that has to decide what a
half-consumed sink's state meant.

## A sink may be remote, and the transport needs no design

The contract is two file descriptors, so a remote destination is
`-- ssh archive01 my-sink`: the protocol rides its stdin and stdout
unchanged. The same arrangement timbersh's `cmd` targets use for the same
reason.

⚠ And `retaining` WORKS for a remote destination, which the earlier reading
of this got wrong. The loop and the positions are local; only the
destination is elsewhere, so the host-local interest axis has everything it
needs. What stays ephemeral is a remote READER — one pulling from another
host's stores — which is a different thing and not a registered follower.

## The trivial sink we ship

A sink that runs a command per record, with the exit code as the report:

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

⚠ A fork per record is an honest property of THIS sink, not of the
protocol. Reach for it for a watcher, a `logger`, an escalation script,
`/dev/null`; not at fifty thousand lines a second. A long-lived sink
reporting watermarks needs no forks at all — which is why the exit-code
model is one sink rather than the contract.

This is also what makes the **watchers** direction (ROADMAP) a registered,
resumable thing rather than a shell pipeline: its stated MVP is
`query --follow … | timber-filter … | your-action`, and what a built-in
form was said to add is configuration and durability. This is both.

## Deferred, and named rather than implied

  * **`chunks` granularity.** A chunk record carries `uncomp_start` —
    logical bytes, not the tape — so a chunks sink's watermark is a chunk
    NUMBER. `Positions.At` already holds both an offset and a chunk, so the
    file is ready; resuming a SELECTION by chunk is not, the document's
    `cursor` being offsets and `from_chunk` one number for every source at
    once. Accepted in the `hello` grammar and refused with "not served
    yet", rather than pretended.
  * **`frames` joining the protocol**, which follows the above: its
    initial-watermark hello is expressible, its granularity is not yet.
  * **A withdrawal record** for a store leaving a selection.
  * **Per-destination watermarks from one sink** — a routing sink where one
    destination is down and the others are not. The message shape already
    allows a watermark per store; what is missing is a reason to.
  * **`text` granularity is deliberately NOT in the protocol.** A sink must
    be able to report an address, and raw bytes carry none. Rendering
    entries as text for a command is the `exec` sink's own business.

## What it costs

A fourth wire format beside `timberfs-records(5)`, the query document and
the frames wire — with a version, a man page and a compatibility story.
What it buys is the ROADMAP's own stated goal reached properly: "today
resuming means linking `cursor.rs`, i.e. writing our own shipper, and after
it anyone's script is one" — without letting third parties write the
position file.

And it reworks the shipped ownership. `timber-otlp` becomes a stdin sink
that renders, posts and reports, losing its `--select`/`--positions`/
`--cursor` machinery; the loop moves into a `timberfs` subcommand that owns
the read, the selection, the fairness rotation and the positions.
`ship.rs`, `Positions`, `read_forward`, the resource grouping and the
round-robin all survive and are re-homed. Better done before `follower
create` is built on the current ownership than after.
