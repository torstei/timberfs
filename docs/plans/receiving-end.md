# The receiving end: identity, names, and selection

**Status: design, with measured defects.** The four collisions below were each
reproduced against a live intake and are real today. The resolution — keying on
identity, selection as the primitive, the registration handshake and adoption —
is not built.

See also [native replication](native-replication.md), whose wire carries the
handshake described here.

One invariant governs a receiver — **one destination store, one origin** —
and today it is violated silently through four separate doors, all
measured against a live intake. (1) The default `--route service.name`
merges two hosts' identically-named stores into one, whose `.bark` then
labels it with whichever sender created it. (2) `sanitize_name` maps `/`
to `_`, so `checkout/v2` and `checkout_v2` both answer 200 and land in one
store labelled `checkout/v2` — the collision is in the LOOKUP KEY, not the
layout, so no directory naming fixes it; injective encoding
(percent-escape rather than replace) or comparing the batch's route value
against the one the `.bark` already records does. (3) A REINSTALL: a fresh
store on apache01 has a new id and numbering back at chunk 0, routes to
the same value, and is appended to the existing store — measured, with
`timberfs.store.id` still naming install #1, so the provenance lies about
half the data. (4) `apache01.prod.foo.com` and `apache01.dev.foo.com`:
systemd's `%H` and `timber-otlp`'s fallback are both `gethostname()`,
conventionally the SHORT name, so both hosts present `host.name=apache01`
and merge. ⚠ A routing template (`--route '{host.name}.{service.name}'`)
fixes host-versus-service composition and does NOT fix this one — the
value is identical on both boxes. Door 4 is what settles the design,
because unlike the others it involves no misconfiguration and no reading
in which the merge is wanted.

## Key, labels, name — three things, currently conflated into a path

The KEY is the origin store `id`: the only value both stable and unique,
minted per store, and already what `follower create` and `cursor.rs` record
("by IDENTITY … not by path — a store can move"). LABELS are `host`,
`host.fqdn`, `env`, `service`: mutable and non-unique BY DESIGN, which is
exactly why a hostname cannot be a key — hosts get rebuilt, renamed, reused,
and duplicated across environments — and equally why the fully-qualified name
is no rescue: it is more unique and LESS stable, tracking DNS and
search-domain config and routinely wrong in containers. A NAME is a
system-friendly string for a store or a forwarder; it belongs in the manifest,
never in a path. ⛔ **The path is therefore opaque**: a store lives at
`/var/log/timberfs/<something unique>` and nothing should need to know which
store that is. **timberfs is the tool that answers where a store is** —
`list`, or reading the `.bark` files. Discovery is a readdir plus a manifest
read per store, which is comfortable at the store counts in play; if it ever
is not, an index maintained on add and remove is an implementation detail
behind the same question, not a change to this model.

## Selection, not naming, is the primitive

and it is what removes the last operational reason to encode anything in a
path. A NODE's store set is static; an ARCHIVE's is not, so with
`--auto-create` a new sender's data arrives and forwards NOWHERE until
somebody registers a follower for it. So a follower wants a predicate rather
than a store: `follower create --select 'service=~apache-.*' loki-apache
--type … `, or `--select '*'` for the whole forest. ⚠ This note argued that
such a declaration expands to one CHILD SHIPPER per matching store, because
each store has its own chunk axis and so its own resumable position. That
reason no longer holds: a request carries a position per store and an answer
returns one, so one process serves the whole selection — see
[follower-selection.md](follower-selection.md), which supersedes the
mechanism here and keeps the primitive. Two consequences stand either way.
`retain_unconsumed` interest is computed per store FROM a predicate, which
makes the existing "an unreadable declaration fails closed globally" rule
more load-bearing, since one bad predicate spans every matching store. And
labels do double duty: the same `host`/`service`/`env` are the timberfs
selector AND the downstream stream labels, since `timber-otlp` already sends
them as OTLP resource attributes and Loki maps resource attributes to labels
— one vocabulary end to end. The QUERY API takes the same primitive: its unit of work is a selection,
so a response owes a COVERAGE statement (which stores it read, and what span
each contributed), or "no results" cannot be told from "that selector matched
nothing". ⚠ Two things to decide rather than discover: `--select '*'` with
`--auto-create` lets SENDERS determine what reaches the downstream (a mistyped
route value on one node creates a store that forwards without anyone choosing
it), which wants a `--dry-run` showing current matches and argues for
`--auto-create` being a deliberate archive-side choice; and whether the store
id travels as a forwarded label at all — useful for "which store was this" and
useless to query on.

## A registration handshake, which the frames wire can have and OTLP cannot

An OTLP sender POSTs and gets 200 or 503; there is no channel for "I already
have that, sitting at 424242". On the native wire the exchange already exists
— it is `stream-open` plus a coverage answer — so `follower create` performs
one, prints the result and registers, turning every door above into a sentence
on a terminal at setup time instead of a mislabelled store found weeks later:

    client -> stream-open  origin_id, my store id, labels{...}, mode,
                           my coverage 0..N
    server -> coverage     accepted, registration id <assigned>,
                           I hold 0..424242
           or conflict     that name/labels are held by origin <other>
                           at 0..424242

The conflict taxonomy is small and each case has an answer: origin ids
MATCH, so resume at the server's position (authoritative, so the client
need not guess); origin ids DIFFER while labels collide, so this is a
new tape and the operator chooses between replace, distinguish and
mistake; the client is at 0 while the server holds 424242, which the
origin comparison has already classified as reinstall or rewind; the
client's oldest is newer than the server's newest, so there is a gap and
the server can size it. On ids: the ORIGIN id must never be assigned by
the server (minted at the origin, copied verbatim, or the address lies),
while a server-assigned REGISTRATION id is a good idea — the receiver
then names its own stores from something it controls rather than
deriving a path from client-supplied strings, which is the same
conclusion as namespace policy belonging to the receiver. Lookup stays
by origin id, so a reconnect gets the same registration back.
⚠ Two things this must get right: the handshake happens on EVERY
connect, not only at create — create-time is the operator-facing check,
connect-time is the enforcement, and a store can be deleted or a name
claimed in between — and it needs an offline escape (`--no-verify`) so
provisioning a node while the archive is down is possible, with the
conflict surfacing at first connect instead.

## Adoption: re-id a store to continue a dead origin's numbering

The reinstall case has a correct answer that looks like a violation and is
not. The old install minted 0..424242 under origin O; the disk is dead so it
can never mint again; the new install adopts O and starts at 424243. No two
byte sets ever share an address, so `(origin_id, seq)` holds. What makes it
safe is knowledge the system CANNOT derive — that the previous minter is
permanently gone — which makes this a FENCING decision and gives it exactly
one failure mode: **split-brain.** If the old node was partitioned rather than
dead, or its disk is later resurrected, two minters share one origin and both
produce chunk 424243 with different bytes, and the address lies permanently
and undetectably. So adoption is an explicit operator act that states its
assumption, and it must resist being baked into configuration management: a
template that always passes `--adopt` is right on every rebuild and
catastrophic on the one partition, which is the worst possible shape for a
flag. ⚠ **Start above the FLEET, not above the server you asked.** The dead
disk may have minted 424243..424250 and died before shipping them; if another
tier received 424250, adopting at 424243 collides with bytes that DO survive.
The safe floor is the highest seq any holder has, which is a coverage query
across peers — the discovery mechanism above, doing write-path work rather
than only serving reads. Whatever the dead disk minted and never shipped is
lost, and leaving those numbers unused records that truthfully: a third state
beside NEVER HAD IT and HAD IT AND DROPPED IT, namely MAY NEVER HAVE EXISTED.
Reusing the numbers would be the lie; the hole is the accurate account. **This
is the first real customer for the numbering BASE.** A store whose oldest
chunk is 424243 has `dropped_chunks()` return `first_seq`, i.e. 424,243 chunks
dropped when nothing was dropped — so adoption requires the base that
"Globally addressable chunks" above argues the reserved header space wants:
`first_seq - base`, with `base = 424243` giving zero.
