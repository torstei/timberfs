# A file intake: one declaration, a set of files, one process

**Status: built** (branch `feature/file-intake`). The ingest counterpart of
[follower-selection.md](follower-selection.md) and
[frames-selection.md](frames-selection.md), which made the same move on the
read and replication sides. It does NOT copy their mechanism, and the section
«Why there is no registry» is why.

Today one `timberfs import --follow` is one file, one unit and one process.
A system rarely has one log: Exim writes `mainlog`, `rejectlog` and
`paniclog`; Apache writes an access and an error log; a database writes a
query log beside an error log. So the shipped `timberfs-follow.conf.example`
demonstrates its own problem — three `follow-exim-*.conf` files, three
`systemctl enable`, and the retention policy for «exim» stated three times
and free to drift three ways.

## The subject of a file intake becomes a named SET

    A file intake is a NAME and a list of sources, and a store for each
    source in that list.

The name is the set's — `exim`, `apache`, `host` — and it names the unit
instance and the config file, not any store. Each source names its own store,
its own labels and its own retention, defaulting to the set's.

    /etc/timberfs/file.d/exim.conf

        STORE_DIR=/var/log/timberfs
        DECLARE=index=true retain=90d

        [exim-main]
        SOURCE=/var/log/exim4/mainlog

        [exim-reject]
        SOURCE=/var/log/exim4/rejectlog
        DECLARE=index=true retain=365d

        [exim-panic]
        SOURCE=/var/log/exim4/paniclog
        DECLARE=index=true retain=365d wal=true
        FLUSH_AGE=2s

    systemctl enable --now timberfs-file@exim

**The section name IS the store name**, which is also the handle it answers
to (`timberfs query exim-reject`), and the store lands at
`<STORE_DIR>/<section>/<section>.log` — the layout every timberfs intake
writes, where the path spells what the store IS and never how the data got
in. So a section is exactly what one `follow-<instance>.conf` was, and
migrating a host is mechanical: each per-file conf becomes a section, its
`SOURCE` unchanged, its instance name becoming the section header.

**Per-section beats set-wide, key by key** — the rule
`timberfs-text@.service` already has between `text.conf` and
`text-<instance>.conf`, one level further in. `DECLARE` in the preamble is
the set's policy; a section that states its own replaces it wholly, because
a partial merge of a space-separated string is a syntax nobody can predict.

⚠ **A source is an absolute path and nothing is anchored to a directory.**
A set is a named list of files that are usually — not necessarily — near
each other, so there is no `SOURCE_DIR` to make the common case shorter: a
set spanning `/var/log/exim4` and `/var/log/mail.log` must not be
second-class, and a second way to spell a path is a second thing that can be
wrong. The cost is retyping a prefix, which an operator writes once.

## Why there is no registry

The follower side keeps `/var/lib/timberfs/followers/<name>/` with a
`positions.json`, and it needs to: a follower's position is durable state
that **another subsystem reads** — `retain_unconsumed` takes the retention
floor from it. A file intake has no equivalent, by construction:

> a restart re-syncs against the store's own lines rather than a position
> file, and can neither lose nor duplicate

The tail's checkpoint is the store it writes. There is no position to keep,
nothing else to read one, and so nothing for a registry to hold. Adding one
would mean a second durable object, a second lock discipline, a second
«unreadable declaration fails closed» rule, and a `create`/`list`/`delete`
surface for state that does not exist. The declaration is a config file in
`/etc`, as `timberfs-follow@` and `timberfs-text@` already have.

## One process, because this path is stateless

Frames and followers earned their multiplexing on a shared destination —
*one destination means one queue, so a stalled endpoint stalls every store
in the selection, and that is the right coupling.* **That argument does not
apply here**: N files and N stores share no resource but the process. So the
case for one process is not correctness, and must not be dressed as it.

It is two smaller things, and the honest ones:

- **The management surface.** N units to enable, monitor and upgrade, and N
  places for one system's retention policy to drift.
- **The runtime.** Measured on a release build, idle: **~9.1 MB RSS per
  `import --follow`**, and one `file-intake` over Exim's three logs is
  **10.6 MB in 7 threads against 27.5 MB in three processes**. So a source
  costs ~0.5 MB inside a set and ~9.1 MB outside one. On a mail server the
  difference is noise; on a host tailing fifty logs it is ~450 MB against
  ~30 MB, which is not.

What makes one process *safe* is the same property that removes the
registry: a tail that dies re-syncs against its store's own lines. If the
process dies, every store in the set stops — and `Restart=always` brings it
back to a few seconds of lag in each, not a gap and not a duplicate. Say
this out loud, because it is the whole of the risk assessment and it is not
obvious.

⚠ **A thread that dies must be NOTICED.** One tail per thread and
`panic = unwind` means a panicking thread dies alone and silently: that
store stops while the unit stays `active (running)` and every other store
keeps filling. That is the failure this shape invites and nothing else in
the process would report it. So the set's runner joins its threads and
**exits non-zero naming the source that ended**, letting systemd restart the
whole set — a dead tail is not a thing to survive quietly. A set is a unit
of supervision as well as of configuration.

## Per system on the host, not per host

An «everything on this host» set is just a longer list and is supported by
saying nothing special. The shipped examples are per system anyway, for a
reason the current unit already records:

> `LogsDirectory=timberfs/%i` — per instance, not per intake kind, because a
> directory is what a writer needs permission on: each one can then be owned
> by its own `User=` drop-in without a directory every instance can write to.

Per system, that still holds: every Exim log is readable by one user and its
stores are written by one. Collapse the host into a single set and the intake
must run as whoever can read the union of every log on the box, which on a
Debian host is root. Blast radius says the same thing more cheaply — one
restart interrupting three related tails is a different event from one
interrupting forty unrelated ones.

## What this deliberately is not

**Not a glob.** `SOURCES=/var/log/exim4/*log` is the tempting spelling and it
matches `mainlog.1` the first time logrotate runs — re-importing a rotated
file as a store of its own, under a name derived from a number. Rotation is
already `--rotated`'s concept and the two would disagree about the same file.
An explicit list cannot do this.

**Not a replacement for `timberfs-follow@`.** That template stays, reads its
`follow-<instance>.conf` env files, and is what a genuinely single-file
subject should keep using. The new config lives in `/etc/timberfs/file.d/`
so a set file and an env file can never be mistaken for each other by either
parser.

**Not a new tailer.** Each section runs the `cmd_follow` that exists, with
the arguments it already takes. What is new is the declaration, the process
that holds several, and the supervision over them.

**Not an `EXTRA_OPTS` string.** The systemd units carry one because a unit
can only hand a CLI a string, so their knobs must be spelled as flags. A
file timberfs parses itself has no such excuse, and structure inside a
string is structure re-parsed under rules neither side owns. `POLL`,
`FLUSH_AGE` and `ROTATED` are keys: validated at startup with a line
number, and a misspelling refused rather than passed through to be ignored.

## What the build settled

`declare()` is `set` without the printing — the same validation and the
same atomic write, returning the manifest instead of putting it on stdout.
The set converges every store through it at startup, and one manifest per
store per restart in the journal is noise nobody reads.

`--check` declares and converges the whole set, prints what each source
resolved to, and follows nothing: the command to run after editing a set
and before restarting the unit that serves it.

## The name

`file-intake`, beside `forward-intake`, `otlp-intake`, `frames-intake` and
`incus-intake`: a verb that runs a listener or a tapper, named for what it
takes in. The unit is `timberfs-file@<set>.service` and the config
`/etc/timberfs/file.d/<set>.conf`.

⚠ Deliberately **not** `follow`. `timberfs-follow@` and `timberfs-follower@`
already differ by one letter in opposite directions, and both units carry a
warning saying so; a `timberfs follow` verb beside `timberfs follower` would
put that collision on the command line, where there is no header comment to
warn in. `file` also says the thing that distinguishes this intake from
`timberfs-text@`: the producer owns a real file and timberfs reads it,
rather than writing into a pipe this side must keep draining.
