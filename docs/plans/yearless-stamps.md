# A stamp with no year: syslog's `Oct  1 00:00:02`

**Status: built.** The sections below say where the build differs from the proposal.

Traditional syslog writes `Oct  1 00:00:02 services01 systemd[1]: …` — no year,
no zone. Newer rsyslog/journald configurations write ISO-8601 instead. One
file-intake set should serve both kinds of host without a per-host edit.

## Today

`Extractor::extract` (`src/import.rs`) is stateless and runs on both the write
path (`import`, `follow`) and the read path (`entry.rs`, `query.rs`, `grep.rs`).
A custom `timestamp_format` without `%Y` cannot parse, so every line of such a
file goes unstamped. And a file intake carries the pair through `DECLARE=`,
which cannot hold a regex containing spaces, so there is no way to say it
there at all.

## Decisions

**A built-in, not a declaration.** The syslog stamp is detected the way ISO,
CLF and ctime already are, anchored at the start of the line. The same set file
then works on every host: the ISO hosts match the ISO built-in, the old ones
match this one, and nothing is declared. A declared `timestamp_regex` still wins.

**The year comes from a reference instant.** The year is the latest one that
does not put the stamp after the reference (a day's slack for zones). That is
stateless, and it carries a file spanning New Year by itself: with the reference
in January, December's lines fall in the year before. No rollover state is
kept between lines, which the first draft proposed; it would only matter for a
source spanning more than a year.

- Write path: the reference is the source's mtime, re-read as a tail reads.
- Read path: the end of the chunk's write window. Imported chunks carry the
  stamps resolved at import, so the window is never earlier than a line in it.

**An explicit year declaration is not offered.** It is per-file configuration,
which is what sharing one set across hosts is meant to avoid. A copied or
touched file with a wrong mtime resolves to the wrong year; that is the known
limit, and the divergence report against the write window is where it shows.

**The zone is local unless `timestamp_utc`**, as for every other naive stamp.

A declared `timestamp_format` that names no year gets the same rule: the
year is put in front of it.
