# A stamp with no year: syslog's `Oct  1 00:00:02`

**Status: proposed.** Nothing here is built.

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

**The year comes from a reference instant plus rollover.** The year is the
latest one that does not put the stamp after the reference. Within one source
the `Stamper` carries the previous stamp, and a stamp whose month falls
*behind* the previous one's by more than a few days is the next year, so a file
spanning New Year resolves correctly.

- Write path: the reference is the source's mtime (`now` for a live tail).
- Read path: there is no file. The reference is the end of the chunk's write
  window, the same window the divergence report already uses.

**An explicit year declaration is not offered.** It is per-file configuration,
which is what sharing one set across hosts is meant to avoid. A copied or
touched file with a wrong mtime resolves to the wrong year; that is the known
limit, and the divergence report against the write window is where it shows.

**The zone is local unless `timestamp_utc`**, as for every other naive stamp.

## Open

- Whether the write path resolves once and the read path re-parses only for
  entry filtering must be confirmed against `entry.rs` before the reference
  is threaded through `extract`.
- The rollover threshold: a backwards month step is certain; a backwards step
  inside one month is disorder, not a new year.
