# Chunks by address: a manifest now, bytes on demand

**Status: design.** Two things it rests on are already true — a sealed chunk is
immutable, and every store is already a run of a longer tape, since retention
drops only prefixes. Sparse stores, the cache layout and discovery are not
built; the digest is deferred and optional (see below).

See also [native replication](native-replication.md) for the wire that carries
this, and the roadmap's "Globally addressable chunks" for the addressing rules
it depends on.

The model is a TAPE. A store is an endless tape with a beginning — chunk
#0, then an unbounded run of potential chunks (u64-bounded, which at 256
KB a chunk is not a practical ceiling) — and a node holds ZERO OR MORE
RUNS of some tape. Two concepts, and only the second is new: a CONTIGUOUS
piece of the tape, which is today's store, and an ordered list of
FRAGMENTS with holes between them. Nodes stay equivalent either way — how
much of a tape one holds is a deployment fact, not a kind of node.

## The property that makes fragments meaningful is that a chunk carries its meaning alone

It does, at the byte level: the zstd frame is independent, the portable rings
fields (`uncomp_len`, `comp_len`, `first_write_ms`, `last_write_ms`, `seq`)
are per-chunk, and a grain page is self-sizing. ⚠ It does NOT at the entry
level — an entry may straddle a boundary, which `timberfs-records(5)` states
("a line split across two chunks reports the second") and `EntrySink`
implements by carrying the trailing partial line across pushes. So reading
across a hole would splice chunk N's tail onto chunk M's head and produce a
line THAT NEVER EXISTED: not missing data but fabricated data, which is worse.
A fragmented reader therefore needs a "next chunk is not adjacent" signal, and
at a hole must terminate the partial line and report both ends as incomplete
rather than joining them. That is the one real cost of the model.

## The current format already expresses it

, which is the strongest form of not making today's files less useful: same
files, same format, one loosened invariant. `seq` is SEARCHED and never
computed from position (`position(|c| c.seq >= seq)`), `next_seq` comes from
the last record rather than a count, `read_chunk` addresses by offset, and
`.grain` is indexed by rings POSITION so gaps in seq do not disturb it. The
delta is the splice above plus `dropped_chunks()`, which reads the count
straight off `first_seq` and so conflates NEVER HAD IT with HAD IT AND DROPPED
IT — today those coincide because only prefixes go.

## Two concepts, two layouts

the layout follows the concept instead of one being bent to serve both. A
CONTIGUOUS piece of tape stays `.trunk` + `.rings`: one seek, a sequential
read, the existing write path and the existing `zstd -dc <name>.trunk`
promise. A FRAGMENT LIST wants a directory of frames, one file per chunk,
named by number (Maildir-style, and the namespace is obviously NOT flat — see
the open details below), with conversion between the two a defined operation:
assemble a complete fragment set into a contiguous store, explode a store into
fragments.

## The fragment layout's use case is the CACHE

a fetch-on-demand tier, holding whatever runs it has been asked for. That is
what makes its trade-offs the right ones: insert and evict dominate,
whole-store scans do not happen, and eviction is not loss because a peer still
has the chunk.

## And the caller never needs to know which it is

The read API spans both layouts, and collections of them, presenting one view
— which is what licenses having two layouts rather than forcing a single
format to be adequate at both jobs. The abstraction boundary is the API, not
the storage layer, and getting it there is a REQUIREMENT on the API rather
than a hope: a query that must ask "is this a store or a cache" has put the
boundary in the wrong place.

What the directory buys is not mainly trivial insert and delete, though
it has those (write tmp + rename is atomic, so no WAL; unlink returns
the space at once, so `PUNCH_HOLE`, `COLLAPSE_RANGE`, skippable-frame
stamping, offset rebasing, `.trim` and the seqlock ALL stop applying
here). It is that the on-disk fragment can BE the wire frame: receive is
"write these bytes to a file" and serve is `sendfile`, with no
re-derivation of offsets into someone else's coordinate space on the way
in and no reading them back out on the way out. Parallel fetches from
different peers also stop contending, writing separate files rather than
sharing one trunk's write lock.

Iteration is the cost, and the FILENAME is what makes it bearable — the
Maildir trick of putting in the name what you would otherwise open the
file to learn. With `seq` and the write window in the name, `readdir`
yields coverage, time spans and sizes with zero opens, so only the
chunks a query actually needs get read; a time-range query is a
contiguous run, and whole-store scans are the anti-pattern
(`SELECT * FROM huge_table`) rather than the case to optimise. Merging
adjacent fragments is then plain CONCATENATION, since zstd frames
concatenate — `cat a b > ab`, and the merged fragment names a RANGE,
which is the run concept again. That gives hot/cold tiering for free:
many small files where inserts land, merged runs for cold data, which
also retires the per-file slack (10.6% at the measured mean frame size
of 25,919 B, plus an inode each). Remaining costs to size rather than
discover: directory scaling (400k files for a 100 GB store is fine with
`dir_index`; 10M wants sharding on the high bits of `seq`), and a
recovery incantation that changes shape — `cat $(ls -v) | zstd -dc`
instead of `zstd -dc <name>.trunk`, still stock tools but dependent on
getting the sort right.

For a contiguous store the record shape already answers the one layout
question that is not a detail: `comp_start` and `uncomp_start` are
separate fields, so keep `comp_start` dense (the bytes actually held,
trunk stays compact) and let `uncomp_start` carry the ORIGIN's logical
position. The hole is then real and visible in the uncompressed
coordinate space, so a byte offset means the same thing on every node
holding that tape — no sparse-file tricks and no new field.

Note what a hole is NOT: rings referencing bytes that are absent. The
trunk holds exactly the frames the node has — the hole is in the
NUMBERING, not in the file — so `zstd -dc <name>.trunk` still prints
every byte present and the standing recovery promise is untouched.

## The address is what makes the runs fungible

With `(origin_id, seq)` global, rings + `.grain` are a MANIFEST and the trunk
is content to fetch when needed, from ANY holder, because the address says
what the bytes are and not where they live. Two properties make that sound and
both already hold: a sealed chunk is **immutable** — append-only, and a
head-drop changes a chunk's OFFSET inside the trunk, never its bytes, the same
fact behind "offsets never travel, lengths do" — so a cached copy cannot go
stale; and the address is **location-independent**, so one holder is as good
as another.

## The payoff is query planning with no data

`.grain` is per-chunk, so a `--has` predicate evaluated against a manifest
names the chunks worth fetching before a byte of trunk moves: measured
selectivity is 1 chunk of 136 for a rare identifier, against 136 of 136 for a
substring scan. Sizes set the deployment shape rather than leaving it to taste
— rings alone are ~0.2% of the trunk, rings + grain ~11% (264 KB against 2.33
MB) — so rings for everything and grain for hot stores is a real
configuration, not a hypothetical one.

## A corrupt chunk is not a hole

Three states, not two. NOT HELD — never received, or dropped — has nothing to
retrieve. HELD BUT UNVERIFIED is different: the bytes are there, they are
readable, and most of the content usually survives. Measured on a 1.5 MB chunk
with one byte flipped, streaming decompression recovers what precedes the
damage, which `zstd -dc` already writes to stdout before it errors:

    damage at 25% of the frame   1,441,792 of 1,540,000 bytes   18,724 lines
    damage at 50%                  393,216 bytes                 5,106 lines
    damage at 90%                1,179,648 bytes                15,320 lines

The recovered amounts are multiples of zstd's block size, so what survives is
whole BLOCKS decoded before the damage — the ratio depends on where in a block
the corruption landed, not how far through the frame. A chunk is internally
divisible, which is the fragment model one level down.

So the action on a failed check is to ROUTE AROUND, never to condemn: skip it
in normal reads, decline to serve it onward as authoritative, prefer a peer's
copy — while keeping the bytes and reaching them through an explicit path.
Deleting a chunk because it failed a checksum would destroy the only copy of
data that is 90% readable.

⚠ The salvaged prefix ends mid-line, which is the same hazard as reading
across a hole. One mechanism covers both: know where the trustworthy bytes
stop, and terminate the partial line rather than splicing.

⚠ And `coverage` as specified cannot SAY this — a run list of `(start, end)`
expresses held-or-not and has no third value. Suspect chunks are exceptional,
so this wants a run list plus a sparse exception list, which the framing
already permits additively (a sidecar kind on the coverage frame, or a new
frame kind). No codec change is needed for it; the shape is.

## A digest is computed on send, not stored

It was recorded here as a prerequisite with a sidecar file. It is neither. The
case against storing it:

- **Gross corruption is already loud.** One flipped bit breaks zstd's entropy
  decoding, so `zstd -dc` and `timberfs query` both error today. A digest is
  not what stands between an operator and silent corruption.
- **A stored mismatch is DISAGREEMENT, not proof.** A digest sidecar is
  derived and rebuildable, so it rots too, and nothing distinguishes a corrupt
  chunk from a corrupt digest without a peer to break the tie.
- **AEAD subsumes it for anyone who needs it properly.** Encrypting chunks
  brings a mandatory per-chunk authentication tag — a cryptographic digest
  decryption cannot skip — so the population most concerned about integrity
  gets something strictly better for free.
- **And the transfer case needs no storage at all.** The sender is already
  reading those bytes in order to send them, so it hashes in the same pass and
  puts the result in the frame's sidecar. No file, no rebase question, and no
  corrupt-sidecar ambiguity, because nothing is kept.

What that buys is not saved work — the receive path never decompresses, so
there is no decompression for a check to come "before". It is WHEN damage is
found: at ingest the sender still holds a good copy and a re-fetch is trivial,
while at first read, weeks later and possibly on a downstream tier, the source
may have dropped that chunk to retention and the loss is permanent. Retention
is what makes finding out late expensive, not CPU. It is also the textbook
end-to-end case: TCP's checksum is 16 bits, and corruption is as likely in the
sender's disk read, the receiver's write or a middlebox, which no hop but the
endpoints can see.

⚠ The limit of computing on send: it certifies the TRANSFER, not the original
write. A sender whose disk rotted hashes the corrupt bytes and the receiver
accepts them. A digest stored when the chunk was written would catch that —
but a pure forwarding tier never decompresses its own chunks, so it would not
notice its own rot in any case, and the receiver finds it at first read. The
same late-detection argument, and equally marginal.

**The frame layout already permits computing it while streaming**, by luck
rather than design: the sidecar TABLE (kind and length) sits in the prefix and
a digest is fixed-length, so its length is known before its value is, and
bodies follow the chunk bytes. A sender writes the header and table, streams
the compressed bytes while hashing, then writes the digest — no buffering, no
second pass.

(One exception argues for shipping grain pages rather than digests: a receiver
that declares `--index` and is NOT sent them must decompress every chunk to
tokenize it. The path is decompression-free only when the sidecars it needs
come along.)

## Scrubbing is the only customer for a stored digest

And it is a genuine cost argument, unlike the transfer case: hashing
compressed bytes rather than decompressing them measured ~10x faster and 17x
less memory traffic (238 MB plain / 14 MB compressed: 0.06 s to decompress,
under 0.01 s to CRC), which makes a periodic archive-wide verify feasible
where decompressing everything is not.

A scrub needs a baseline recorded EARLIER to compare against, which is the one
thing computing on send cannot provide. Since a retroactive digest certifies
the bytes only as of the scrub anyway, the first scrub can write that baseline
itself. So the file, if it ever exists, is a scrub artifact and not part of the
wire.

## Discovery: ask trusted peers, then fetch from the best answer

`WHOHAS (origin_id, seq)` to a known peer set, then a normal ranged fetch from
whoever answers best — which keeps the trust boundary where the intakes
already put it, and needs no DHT. The answer is a `coverage` response (a run
list), not an index dump. Ranking mostly falls out: the asker measures RTT
itself, so a responder only needs a cost hint for what RTT cannot see — cold
storage, spinning disk, a tier that would itself have to fetch onward. Two
things not to confuse with it. A UDP multicast stops at the first router, so
it serves one VLAN and is a possible later TRANSPORT for this question rather
than a design. And where the peer set is known and stable, tiers exchanging
coverage on a schedule reduces WHOHAS to a LOCAL lookup, with the live query
as the fallback for a cold or newly-joined peer. Gossip and DHTs stay out
until membership is genuinely unknown.

Telling NEVER HAD IT from HAD IT AND DROPPED IT is the same distinction
as "renumbering destroys the evidence of a gap" above, which is what
makes this a consistent extension rather than a new doctrine.

## Three mechanics a sparse store changes, and none of them is a blocker

*Retention still works, but stops being erasure.* Head-drop means "drop
the lowest-seq run", so `retain` and `retain_size` need nothing new.
What inverts is the consequence: on a contiguous origin store retention
is ERASURE — irreversible, which is why the follower machinery exists —
while on a fragment set whose peers hold the same chunks it is EVICTION,
undone by a fetch. `retain_unconsumed` generalises with it, from "a
follower has not read this" to "I MAY BE THE LAST HOLDER": the same
interest floor from a different source, and under the same settled rule
that interest is ADDITIVE, never a cap, or one stale catalogue entry
pins the disk until it fills. Evicting from the MIDDLE is a different
and easier primitive than head-drop — `PUNCH_HOLE` rather than
`COLLAPSE_RANGE`, so nothing rebases and no offset moves — and
`stamp_skippable_frame` is the precedent for keeping the trunk readable
across the gap: stamp the 8-byte header, punch the rest.

*Inserting a chunk in the middle* is easy for the frame (trunk tail, or
a punched hole that fits) and a choice for the RECORD. Rewriting the
rings in seq order is nothing at 90 chunks and O(N) per insert at ten
million — hopeless for a cache that fetches constantly. Appending the
record and SORTING AT OPEN keeps insertion O(1), and `read_index_file`
already reads the whole file into a `Vec` so the sort is nearly free;
the invariant spent is "records are sorted by seq on disk" becoming "the
reader sorts", whose knock-on is `.grain` — indexed by rings POSITION,
so pages land in insertion order, still consistent but harder to rebase
on a head-drop. Either way `comp_start` stops being monotone with `seq`,
which nothing requires (`read_chunk` addresses by `comp_start + len`)
but which fragments `export`/`rotate`'s verbatim runs into more and
smaller copies. Note the payoff of keeping the origin's `uncomp_start`:
an inserted chunk fills the uncompressed gap EXACTLY, so uncomp order
stays identical to seq order and offset-based addressing is untouched.

*Streaming out of a sparse store* needs no protocol change: `coverage`
advertises the runs, and the frames' own `seq` values are authoritative
with the range as a bound. Streaming INTO one is the insertion case
above. `--follow` on a sparse store means "tell me when fragments
arrive" rather than tailing a live edge — and sparse is not exclusive
with being an origin, since a node can lose middle chunks and keep
producing.

*Reading into a hole is an ERROR*, never a silent short read — and it is
the natural trigger for a fetch. Which is the same fact as the splice
hazard above seen from the read side: the reader must know where the
hole is either way.

## Deliberately open

, so none of the above is read as a spec: the fragment namespace (flat is
wrong; sharding, and on what — `seq` high bits, origin, time — is undecided),
what exactly a filename carries and what stays in a rebuildable index beside
it, eviction POLICY for a cache (head-drop is one; recency or a last-holder
check are others), when merging runs and who triggers it, whether the API
exposes coverage to callers or only uses it internally, and how a cache miss
surfaces — block and fetch, or answer with a declared gap. All of it
downstream of the read API existing.
