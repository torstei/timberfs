use timberfs::{
    append, bark, export, feed, follow, follower, forest, forward, fs, grain, import, incus,
    incus_intake, list, note, otlp_intake, query, querydoc, rotate, select, ship, sink, store,
};

use std::path::PathBuf;

use anyhow::Context;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "timberfs",
    version,
    about = "Append-only, transparently compressed, write-time-indexed filesystem for log files"
)]
struct Cli {
    /// Suppress informational notes on stderr (scan reports, progress,
    /// summaries); errors and warnings still print
    #[arg(long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a timberfs: files under MOUNTPOINT are stored compressed and
    /// time-indexed in BACKING. Runs in the foreground; unmount with
    /// fusermount3 -u MOUNTPOINT (or Ctrl-C if auto_unmount is active).
    Mount {
        /// Backing directory holding the .trunk/.rings pairs
        backing: PathBuf,
        /// Directory to mount the logical view on
        mountpoint: PathBuf,
        /// Uncompressed chunk size threshold in bytes
        #[arg(long, default_value_t = 256 * 1024)]
        chunk_size: usize,
        /// zstd compression level
        #[arg(long, default_value_t = 3)]
        level: i32,
        /// Max seconds appended data may sit unflushed; bounds the
        /// write-time granularity of the index and crash data loss
        #[arg(long, default_value_t = 5.0)]
        flush_age: f64,
        /// Let other users access the mount (needs user_allow_other in
        /// /etc/fuse.conf)
        #[arg(long)]
        allow_other: bool,
        /// Exit for a clean re-exec when this binary is upgraded on disk
        /// (dpkg replaces it). Only for supervised runs that will be
        /// restarted — the systemd units set it and pair it with
        /// RestartForceExitStatus; leave it off for an interactive mount.
        #[arg(long)]
        exit_on_upgrade: bool,
    },
    /// Create an empty timberfs log with its properties declared up
    /// front in a .bark manifest — database-style: `create --index` is
    /// CREATE INDEX, and every later writer maintains the .grain
    /// automatically
    Create {
        /// Backing file to create: logical name, .trunk or .rings path
        dest: PathBuf,
        /// Declare the token index for this log
        #[arg(long)]
        index: bool,
        /// Declare the write-ahead sidecar (.sap): every streaming writer
        /// fsyncs it once a second, so an appender's crash window shrinks
        /// from --flush-age down to that tick, and query --follow tails
        /// it, so entries reach a follower as they are appended rather
        /// than a flushed chunk at a time. Costs writing raw bytes twice
        /// (once to the sap, once compressed into the chunk)
        #[arg(long)]
        wal: bool,
        /// Declare retention: continuously drop data older than this
        /// (e.g. 90d, 12h) — enforced by every writer
        #[arg(long)]
        retain: Option<String>,
        /// Declare a compressed-size budget (e.g. 50G, 512M); oldest
        /// data drops first — enforced by every writer
        #[arg(long)]
        retain_size: Option<String>,
        /// Declare interest-based retention: keep what this store's
        /// RETAINING followers have not read, on top of the age and size
        /// axes — never instead of them. Requires --retain-size as the
        /// backstop, since interest only ever holds MORE and one stalled
        /// follower would otherwise fill the disk
        #[arg(long, requires = "retain_size")]
        retain_unconsumed: bool,
        /// Set a manifest property (key=value, e.g. host=foo.bar.com);
        /// repeatable, free-form
        #[arg(long = "set", value_name = "KEY=VALUE")]
        sets: Vec<String>,
        /// Succeed quietly when the store is already there, instead of
        /// failing — CREATE IF NOT EXISTS, for provisioning that runs on
        /// every start. The existing store is left exactly as it is; a
        /// declaration it disagrees with is warned about, not applied
        #[arg(long)]
        if_not_exists: bool,
    },
    /// Declare or change a store's properties in its .bark manifest —
    /// validated and atomic, unlike hand-editing. Live writers re-read
    /// the manifest within a second, so changes need no restart:
    /// `timberfs set backing/app.log retain=30d`
    Set {
        /// Backing file: logical name, .trunk or .rings path
        store: PathBuf,
        /// KEY=VALUE to set: retain=90d, retain_size=50G,
        /// index=true|false, wal=true|false, or any free-form
        /// provenance key
        #[arg(value_name = "KEY=VALUE")]
        sets: Vec<String>,
        /// Remove a key (repeatable): --unset retain
        #[arg(long = "unset", value_name = "KEY")]
        unsets: Vec<String>,
    },
    /// Append stdin to a log in a backing directory, without FUSE
    /// (svlogd-style): `myapp 2>&1 | timberfs append backing/app.log`.
    /// One writer per file; appenders for different files share a
    /// directory. EOF, SIGTERM or SIGINT flush and sync before exit.
    Append {
        /// Destination backing file: logical name, .trunk or .rings
        /// path (destinations are always named --into; positionals are
        /// sources)
        #[arg(long = "into", value_name = "DEST")]
        into: Option<PathBuf>,
        /// stdin is a timberfs-records(5) stream, not raw text: entries
        /// arrive pre-framed, and ones carrying their original write
        /// window (wf/wl) keep it — write history survives the pipe.
        /// Without wf/wl, append stamps now, as always. Streaming
        /// delivery: data lands as it arrives; a truncated stream keeps
        /// what arrived and fails the exit code
        #[arg(long)]
        records: bool,
        #[arg(hide = true)]
        legacy: Vec<String>,
        /// Uncompressed chunk size threshold in bytes
        #[arg(long, default_value_t = 256 * 1024)]
        chunk_size: usize,
        /// zstd compression level
        #[arg(long, default_value_t = 3)]
        level: i32,
        /// Max seconds appended data may sit unflushed; bounds the
        /// write-time granularity of the index and crash data loss
        #[arg(long, default_value_t = 5.0)]
        flush_age: f64,
        /// Declare the write-ahead sidecar (.sap): entries are fsynced
        /// once a second (independent of --flush-age), so a crash loses
        /// at most that tick instead of up to a full unflushed chunk —
        /// and query --follow tails it, so they are visible as they are
        /// appended. A property of the store (like --index) — once
        /// declared, every later writer honors it with no flag, running
        /// ones included
        #[arg(long)]
        wal: bool,
        /// Continuously drop data older than this (e.g. 30d, 12h, 90m)
        #[arg(long)]
        retain: Option<String>,
        /// Keep the on-disk (compressed) size at or under this budget
        /// (e.g. 200G, 512M); oldest data is dropped first
        #[arg(long)]
        retain_size: Option<String>,
        /// Exit for a clean re-exec when this binary is upgraded on disk.
        /// Only for supervised runs (the log-intake unit sets it); leave
        /// it off for an interactive `producer | timberfs append`, which
        /// must not vanish on an unrelated upgrade.
        #[arg(long)]
        exit_on_upgrade: bool,
        /// Seconds to wait for a departing writer to release the log
        /// before failing. A supervisor starts the replacement before the
        /// old writer has exited (Apache spawns the new piped-log program
        /// on reload), so the handoff is normal, not a conflict; 0 fails
        /// on the first attempt
        #[arg(long, default_value_t = 5.0, value_name = "SECS")]
        wait_for_writer: f64,
    },
    /// Import existing plain log files into a timberfs log, stamping
    /// chunks with timestamps parsed from the log lines (auto-detects
    /// RFC3339/ISO, Apache/CLF and leading epochs; lines without a
    /// timestamp inherit the previous line's). Several source files (a
    /// rotated set, in any order) are stitched chronologically by their
    /// first timestamps. Re-importing a grown single source appends only
    /// the growth, after verifying the already-imported data.
    Import {
        /// Source log file(s): plain logs (stitched chronologically by
        /// their first timestamps when several), timberfs logs, or
        /// .timber bundles; with --records, one records file or stdin
        #[arg(num_args = 0..)]
        sources: Vec<PathBuf>,
        /// The source is a timberfs-records(5) stream (a file, or stdin
        /// when no source is given): entries arrive pre-framed, and
        /// ones carrying their original write window (wf/wl) keep it.
        /// Without wf/wl, import derives write time from the entry's
        /// own timestamp, as always. Atomic delivery: nothing is
        /// visible until stream-end; a truncated stream leaves the
        /// store unchanged
        #[arg(long)]
        records: bool,
        /// Destination backing file: logical name, .trunk or .rings path
        /// (a named flag on purpose — a glob can never eat it)
        #[arg(long = "into", value_name = "DEST")]
        dest: PathBuf,
        /// Uncompressed chunk size threshold in bytes
        #[arg(long, default_value_t = 256 * 1024)]
        chunk_size: usize,
        /// zstd compression level
        #[arg(long, default_value_t = 3)]
        level: i32,
        /// Custom timestamp extraction: regex with one capture group
        #[arg(long, requires = "timestamp_format")]
        timestamp_regex: Option<String>,
        /// chrono format string for the captured timestamp (e.g.
        /// '%Y-%m-%d %H:%M:%S%.f' or with %z for zoned)
        #[arg(long, requires = "timestamp_regex")]
        timestamp_format: Option<String>,
        /// Treat zoneless timestamps as UTC instead of local time
        #[arg(long)]
        utc: bool,
        /// On re-import, verify only the first/middle/last already-imported
        /// chunks against the source instead of all of them
        #[arg(long)]
        quick: bool,
        /// Declare and build the .grain token index for this log
        /// (persisted in the .bark manifest — needed once; every later
        /// import maintains the index automatically)
        #[arg(long)]
        index: bool,
        /// Declare the write-ahead sidecar (.sap): with --follow, what
        /// makes each line visible to query --follow as it is read
        /// instead of when its chunk is flushed (a batch import has no
        /// live tail to serve or protect, and only declares it for later
        /// writers); a property of the store, like --index
        #[arg(long)]
        wal: bool,
        /// Keep following the source instead of exiting at its end: the
        /// tailer. Reads new lines as they are written, drains the file it
        /// holds before switching when rotation replaces it, and resumes
        /// after a restart against the store's own lines — so it can
        /// neither lose nor duplicate, with no position file to go stale.
        /// One source, and the destination gets a live writer (retention,
        /// index and .sap are maintained as for any other)
        #[arg(short = 'F', long, conflicts_with_all = ["records", "quick"])]
        follow: bool,
        /// Where rotation moves the source, for the one case the live path
        /// cannot answer: data written while this follower was not running
        /// (repeatable). Default: <source>.1 and <source>.0 when they
        /// exist. While running, rotation needs no pattern — the
        /// descriptor already held is the file that moved
        #[arg(long, value_name = "PATH", requires = "follow")]
        rotated: Vec<PathBuf>,
        /// Seconds between looks for new data and for a replaced file
        /// (--follow only). With a wal declared this also bounds how soon
        /// a line reaches a live reader; without one, --flush-age does,
        /// and nothing observable improves below it
        #[arg(long, default_value_t = 1.0, value_name = "SECS", requires = "follow")]
        poll: f64,
        /// Max seconds followed data may sit unflushed (--follow only) —
        /// a VISIBILITY knob, not a durability one: the source file is the
        /// durable copy and the store is the checkpoint, so a follower that
        /// dies with a partial chunk re-reads it on the next start. Hence
        /// the minute rather than the appender's 5s, whose input is a pipe
        /// with nowhere else to hold the data: a short age on a quiet log
        /// closes chunks too small to compress (measured 3.1x against 7.7x
        /// at one line a second). To make new lines visible sooner,
        /// declare --wal instead: query --follow tails the sap, so
        /// visibility stops depending on this at all
        #[arg(long, default_value_t = 60.0, value_name = "SECS", requires = "follow")]
        flush_age: f64,
        /// Continuously drop data older than this (e.g. 90d) — declared in
        /// the manifest and enforced by every writer (--follow only)
        #[arg(long, requires = "follow")]
        retain: Option<String>,
        /// Declared compressed-size budget, oldest data first (--follow only)
        #[arg(long, requires = "follow")]
        retain_size: Option<String>,
        /// Exit for a clean re-exec when this binary is upgraded on disk
        /// (--follow only; for supervised runs, as for the appender)
        #[arg(long, requires = "follow")]
        exit_on_upgrade: bool,
        /// Seconds to wait for a departing writer to release the log
        /// before failing (--follow only; see append)
        #[arg(long, default_value_t = 5.0, value_name = "SECS", requires = "follow")]
        wait_for_writer: f64,
    },
    /// Export a time window (or everything) from a timberfs log into a NEW
    /// timberfs log, chunks copied verbatim — no recompression. A DEST
    /// ending in .timber writes the single-file transfer bundle (plain
    /// tar: .rings first, .trunk second), which import accepts directly.
    Export {
        /// Source backing file: logical name, .trunk or .rings path
        source: PathBuf,
        /// Destination: new backing file, or a *.timber bundle
        /// (destinations are always named --into)
        #[arg(long = "into", value_name = "DEST")]
        dest: Option<PathBuf>,
        #[arg(hide = true)]
        legacy: Vec<String>,
        /// Start of the window (same formats as query); default: beginning
        #[arg(long, value_parser = query::parse_time)]
        from: Option<u64>,
        /// End of the window; default: end
        #[arg(long, value_parser = query::parse_time)]
        to: Option<u64>,
        /// Error instead of writing an empty artifact when nothing matches
        /// (default: an empty result is a result — present-but-empty tells
        /// a consumer "covered, nothing there", unlike a missing file)
        #[arg(long)]
        fail_on_empty: bool,
    },
    /// Print the bytes written between --from and --to, reading the backing
    /// files directly (works with or without an active mount)
    Query {
        /// Backing file(s) or .timber bundle(s); several are interleaved
        /// by chunk time-windows with grep-style "path:" line prefixes.
        /// A `--query` document names its own stores instead, and
        /// `--dump-json` reads no store at all — omitted there, the
        /// document selects every one
        #[arg(required_unless_present_any = ["query", "dump_json"], num_args = 0..)]
        files: Vec<PathBuf>,
        /// Start of the time window (RFC3339, 'YYYY-MM-DD [HH:MM[:SS]]'
        /// — a bare date is midnight, dotted dates work too,
        /// 'HH:MM[:SS]' = today, or unix seconds); default: beginning
        #[arg(long, value_parser = query::parse_time)]
        from: Option<u64>,
        /// End of the time window (same formats); default: end
        #[arg(long, value_parser = query::parse_time)]
        to: Option<u64>,
        /// Only chunks that (probably) contain this token, via the .grain
        /// Bloom index (build with `timberfs reindex`); repeatable = AND;
        /// an argument with separators must match all its tokens
        #[arg(long)]
        has: Vec<String>,
        /// Chunks where at least ONE of these matches (repeat = OR; the
        /// union of exact branches, still exact); composes with --has
        #[arg(long, value_name = "TEXT")]
        any: Vec<String>,
        /// Never prefix output lines with the file name
        #[arg(long)]
        no_filename: bool,
        /// Annotate each entry with the write time it arrived at (and the
        /// offset to its own timestamp) — the invisible second field,
        /// made visible
        #[arg(long, conflicts_with = "by_write_time")]
        show_write_time: bool,
        /// Raw chunk output selected by write-time windows only: no entry
        /// parsing, no logline filtering (yesterday's exact behavior)
        #[arg(long)]
        by_write_time: bool,
        /// NUL-terminated entry records (multiline entries stay one
        /// record — pipe to xargs -0, sort -z, uniq -z ...)
        #[arg(short = '0', long = "null", conflicts_with = "by_write_time")]
        null_sep: bool,
        /// Typed record stream for timber-aware tools: NUL-terminated
        /// records where metadata records (stream-start with the format
        /// version and selection echo, one per source with its stats, a
        /// row header with len/ts/write-window before every entry, and
        /// stream-end with totals — its absence means truncation) are
        /// marked by a leading RS byte. Entry payloads are verbatim
        /// bytes. See timberfs-records(5)
        #[arg(
            long,
            conflicts_with_all = ["null_sep", "show_write_time", "by_write_time", "no_filename"]
        )]
        records: bool,
        /// Follow the store: after the selected output, keep emitting
        /// entries as they arrive, until interrupted (like tail -f). A
        /// store with a wal declared is tailed at its live edge, so
        /// entries surface as the writer appends them; without one, a
        /// flushed chunk is the unit of visibility and they surface
        /// within the writer's --flush-age.
        #[arg(short = 'f', long, conflicts_with_all = ["by_write_time", "to"])]
        follow: bool,
        /// Start from (about) the last N entries: with --follow, show them
        /// then follow; without, show them and exit (like tail -n). Rounded
        /// out to a chunk boundary, so a few extra may show.
        #[arg(long, value_name = "N", conflicts_with_all = ["by_write_time", "from"])]
        tail: Option<u64>,
        /// Stop after at most N log entries (a hard cap, like head -n).
        /// Composes with --follow to bound it; conflicts with --tail (last-N)
        /// and --by-write-time (raw chunks have no entry count).
        #[arg(long, value_name = "N", conflicts_with_all = ["tail", "by_write_time"])]
        max: Option<u64>,
        /// Read the whole search from a JSON document instead of from
        /// flags (`-` for stdin). The same value the flags build, so the
        /// two are one question asked two ways — and a member this
        /// timberfs does not know is an error, never a silently wider
        /// search
        #[arg(long, value_name = "FILE", conflicts_with_all = ["from", "to", "has", "any", "follow", "tail", "max", "from_chunk", "deadline"])]
        query: Option<String>,
        /// Print the search these flags describe as a JSON document, and
        /// exit. What `--query` reads back. Refused with `--follow`: a
        /// following read holds a stream open, where a document describes
        /// one search
        #[arg(long)]
        dump_json: bool,
        /// Seconds between looks for new data (--follow only). Default:
        /// 0.2 when the store has a live write-ahead sidecar to tail
        /// (`wal=true`), where the poll IS the latency, and 1 otherwise,
        /// where the writer's --flush-age decides instead
        #[arg(long, value_name = "SECS", requires = "follow")]
        poll: Option<f64>,
        /// Start at this chunk NUMBER (see `timberfs index`) — a place on
        /// the tape rather than a time. Exact, unlike --from: a number names
        /// one chunk, where a timestamp can match two that share a boundary
        /// millisecond — and chunk numbers only move forward, where the
        /// write axis is a wall clock that an NTP step can push backwards.
        /// Resumes a --follow from a consumer's cursor, and starts a bounded
        /// read there; a chunk cap beside it (the query document's
        /// `max: {chunks: 1}`) makes that a seek to exactly one. A number
        /// older than anything the store still holds starts at the oldest
        /// chunk it has
        #[arg(long, value_name = "N", conflicts_with_all = ["from", "tail"])]
        from_chunk: Option<u64>,
        /// Give up after this many seconds and answer with what was read.
        /// A bound on how LONG rather than how much: a fleet read is slow
        /// because it READS a lot, not because it matches a lot, so --max
        /// does not bound the wait. Unlike a timeout in the caller, the
        /// answer survives — stores read whole are whole, the one it
        /// stopped in carries a position to resume from, and the ones it
        /// never reached say so. Refused with --follow, which does not end
        #[arg(long, value_name = "SECS", conflicts_with = "follow")]
        deadline: Option<f64>,
    },
    /// Manage the followers of a store: registered readers, each with a
    /// name, a type, a `retaining` flag and a durable position. A
    /// follower is a declared object rather than a cursor found lying in
    /// a directory, so its intent is recorded, a collision is caught at
    /// registration, and a follower deployed before it first runs can
    /// still be known about
    Follower {
        #[command(subcommand)]
        command: FollowerCommand,
    },
    /// Feed a CONSUMER: read every store a predicate matches, hand the
    /// records to a program, and move each store's position as far as
    /// that program says it got. timberfs owns the position — the
    /// consumer reports, and a report is the only thing that advances
    /// one. Any program that speaks the consumer protocol will do,
    /// including a shell script; see the consumer protocol
    Feed {
        /// Which stores: the predicate `list --select` takes. `[]` is
        /// every store, which is a thing to have written rather than a
        /// flag to leave out
        #[arg(long, value_name = "EXPR")]
        select: String,
        /// ONE FILE holding every matched store's place, keyed by store
        /// identity, so a restart resumes rather than re-sends. One file
        /// and not one per store because an atomic save costs two
        /// fsyncs: 500 stores measured at 3.9ms against 542ms. Omitted,
        /// the places live only as long as this process — a temporary
        /// watch
        #[arg(long, value_name = "FILE")]
        positions: Option<PathBuf>,
        /// Keep going as entries arrive. Without it the selection is
        /// drained once and this exits — a durable one-shot, durable
        /// because the positions are
        #[arg(short = 'f', long)]
        follow: bool,
        /// Maximum entries handed over before the consumer's reports are
        /// collected
        #[arg(long, value_name = "N", default_value = "512")]
        batch_size: u64,
        /// How long to wait before asking again, once an answer was
        /// exhausted
        #[arg(long, value_name = "DUR", default_value = "1s")]
        poll: String,
        /// Where a store this has never read is picked up: `discovery`
        /// ships one born since this began from its beginning and one
        /// that predates it from its next byte; `begin` ships everything
        /// either has; `end` ships neither's history
        #[arg(long, value_name = "WHERE", default_value = "discovery",
              value_parser = ["begin", "end", "discovery"])]
        follow_from: String,
        /// Forests to search; default: every configured one
        #[arg(long, value_name = "DIR")]
        forest: Vec<PathBuf>,
        /// The consumer and its arguments, after `--`. A list, so
        /// nothing makes a quoting round trip: `-- ssh archive01 my-sink`
        #[arg(last = true, value_name = "CONSUMER")]
        consumer: Vec<String>,
    },
    /// Show a store's vital signs on one screen: identity, lineage,
    /// data and compression, time covered, index sizes and coverage,
    /// writer state. Works on backing pairs and .timber bundles alike
    Info {
        /// Backing file (logical name, .trunk/.rings path) or bundle
        file: PathBuf,
        /// Machine-readable JSON instead of the human summary
        #[arg(long)]
        json: bool,
    },
    /// Show the write-time chunk index of a backing file
    Index {
        /// Backing file: logical name, .trunk or .rings path
        file: PathBuf,
    },
    /// List every store across the configured forests (or the given
    /// directories): handle, forest, size, time span, writer state, index
    /// presence and declared retention — the directory-level complement to
    /// `info`. Read-only and lock-free, like `info`
    List {
        /// Directories to list stores in (ad-hoc; need not be configured
        /// forests). Default: every configured forest
        #[arg(num_args = 0..)]
        dirs: Vec<PathBuf>,
        /// Only this forest, by the name `timberfs forest list` gives it
        /// (repeatable). Without it, every configured forest is listed
        #[arg(long = "forest", value_name = "NAME", conflicts_with = "dirs")]
        forests: Vec<String>,
        /// Bare handles only, one per line, no header or columns (what
        /// shell completion consumes)
        #[arg(long, conflicts_with = "json")]
        names: bool,
        /// A JSON array of objects instead of the human table
        #[arg(long)]
        json: bool,
        /// Select stores by their manifest: `--select
        /// '[type=console,host=web01]'`. Comma-separated terms are ANDed;
        /// `key=value`, `key!=value`, `key=~regex`, `key!~regex`,
        /// `key=*text` (a literal anywhere in the value) and `key!*text`
        /// (regexes anchored at both ends); a BARE WORD is the name,
        /// matched anywhere in it. An absent label reads as the empty
        /// string, so `key!=` selects the stores that declare it. The
        /// wrapping brackets are optional here and are how a predicate
        /// is written in timbersh, so one pastes into the other; `[]` is
        /// the predicate with no terms, which is every store, as an
        /// omitted --select is. Quote a value that must contain a comma
        #[arg(long, value_name = "EXPR")]
        select: Option<String>,
        /// Print each store's whole id instead of the leading 8
        /// characters. The short form is a prefix `info` accepts, so this
        /// is for copying an id somewhere that wants all of it
        #[arg(long)]
        full_id: bool,
    },
    /// What this machine will let ONE REQUEST ask for, and where those
    /// ceilings came from. `timberfs query` has no startup to validate
    /// /etc/timberfs/limits.conf at, so this is that check: it exits
    /// non-zero when a line could not be used, which is what config
    /// management gates a rollout on
    Limits {
        /// The ceilings as JSON, exactly as an answer declares them
        #[arg(long)]
        json: bool,
    },
    /// Declare where stores live. A forest is a directory timberfs
    /// searches, and it is the ONE thing a timberfs command names by
    /// path — every other argument names a store, and a store is found
    /// by what it declares. Declaring one here means the config file in
    /// /etc/timberfs/forests.d stops being the interface
    Forest {
        #[command(subcommand)]
        command: ForestCommand,
    },
    /// Report or repair a store's identity — the id that makes a backing
    /// pair a store. Without a flag it only reports, and exits non-zero
    /// when the two sides do not agree or there is no identity at all, so
    /// it is also the check a script runs. An id is a fact rather than a
    /// setting, so `set` will not touch it; these are the three ways it
    /// can be broken and the fix each one wants.
    Identity {
        /// Backing file: logical name, .trunk or .rings path
        file: PathBuf,
        /// Make an identity for a pair that carries none on either side.
        /// Refused where one already exists — that is a --keep question
        #[arg(long)]
        mint: bool,
        /// Resolve a disagreement by keeping one side's identity and
        /// writing it to the other: `index` (the backing pair, which IS
        /// the store — the usual answer after a manifest was hand-edited
        /// or restored) or `manifest`
        #[arg(long, value_name = "SIDE")]
        keep: Option<IdentitySideArg>,
    },
    /// Build or rebuild the .grain token index for a log: one Bloom filter
    /// per chunk over every token in it (~1% false positives), letting
    /// `query --has` skip chunks — e.g. find a request id with no known
    /// time range. Derived data: safe to delete, cheap to rebuild; rotation
    /// and retention drop it (rebuild afterwards).
    Reindex {
        /// Backing file: logical name, .trunk or .rings path
        file: PathBuf,
    },
    /// Enforce a store's declared retention once, now — the cron-able
    /// complement to the continuous enforcement every writer already
    /// does. Load-bearing rather than convenient: retention runs inside a
    /// live WRITER, so a store whose producer went quiet keeps its data
    /// indefinitely, and under retain_unconsumed that means keeping data
    /// already shipped off the box. A store somebody else is writing is
    /// left alone and said so: that writer's own tick is already doing it
    Trim {
        /// Backing file: logical name, .trunk or .rings path
        store: PathBuf,
        /// Report what interest would drop without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Time-based rotation: move every chunk written before --cutoff into
    /// DEST (or drop it with --delete), relocating compressed frames
    /// verbatim — no recompression. Auto-detects a live mount and routes
    /// the request through the daemon when one is running.
    Rotate {
        /// Source backing file: logical name, .trunk or .rings path
        source: PathBuf,
        /// Destination log (same backing directory; appended to if it
        /// exists); omit when using --delete
        dest: Option<String>,
        /// Rotate data written before this time (RFC3339,
        /// 'YYYY-MM-DD [HH:MM[:SS]]' — a bare date is midnight,
        /// 'HH:MM[:SS]' = today, unix seconds)
        #[arg(long, value_parser = query::parse_time)]
        cutoff: u64,
        /// Drop the rotated chunks instead of moving them (retention)
        #[arg(long, conflicts_with = "dest")]
        delete: bool,
        /// Preview what would move without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Error when nothing rotates (default: rotating nothing into a
        /// new DEST still creates it empty — an attested empty result)
        #[arg(long)]
        fail_on_empty: bool,
    },
    /// Receive the Fluentd Forward protocol v1 over TCP — the wire protocol
    /// of Docker's fluentd log driver, Fluent Bit, Fluentd, and the
    /// fluent-logger client libraries — writing each tag into its own store
    /// under --into-dir. Acks a chunk only after it is flushed and fsynced.
    /// Deliberate limitations: no TLS/handshake (loopback or a private
    /// network only), no gzip-compressed PackedForward, no UDP heartbeat.
    /// The verb name is provisional
    #[command(name = "forward-intake")]
    ForwardIntake {
        /// Address to listen on (systemd socket activation on fd 3 is used
        /// instead when LISTEN_PID/LISTEN_FDS name this process)
        #[arg(long, default_value = "127.0.0.1:24224")]
        listen: String,
        /// The forest to write into, as `timberfs forest list` names it:
        /// one store per Forward tag, named <tag>.log
        #[arg(long, value_name = "NAME")]
        forest: Option<String>,
        /// Backing directory. DEPRECATED in favour of --forest; still
        /// the way to write into a directory that is NOT a forest
        #[arg(long = "into-dir", value_name = "DIR", conflicts_with = "forest")]
        into_dir: Option<PathBuf>,
        /// Record field holding the log line; falls back to the whole
        /// record as compact JSON when missing or not a string
        #[arg(long, default_value = "log")]
        payload_key: String,
        /// Continuously drop data older than this (e.g. 30d, 12h, 90m) in
        /// every store this receiver creates
        #[arg(long)]
        retain: Option<String>,
        /// Keep the on-disk (compressed) size of every store this receiver
        /// creates at or under this budget (e.g. 200G, 512M)
        #[arg(long)]
        retain_size: Option<String>,
        /// Declare and maintain the .grain token index on every store this
        /// receiver creates
        #[arg(long)]
        index: bool,
        /// Create a store for a never-seen tag automatically (the Docker-
        /// host mode: tags are container names that come and go). Default:
        /// refuse unknown tags — pre-create stores with `timberfs create
        /// --wal`; an acking sender retries until the store exists
        #[arg(long)]
        auto_create: bool,
        /// Exit for a clean re-exec when this binary is upgraded on disk
        /// (dpkg replaces it). Only for supervised runs (the systemd unit
        /// sets it and pairs it with RestartForceExitStatus)
        #[arg(long)]
        exit_on_upgrade: bool,
    },
    /// Receive the native replication wire: compressed chunks move
    /// verbatim, so nothing is decompressed at either end and the
    /// destination is byte-identical to its source. Answers a handshake
    /// first — what it already holds, so a sender resumes from the
    /// receiver's position rather than guessing, or why it is refused. The
    /// verb name is provisional.
    FramesIntake {
        /// Address to listen on (systemd socket activation on fd 3 is used
        /// instead when LISTEN_PID/LISTEN_FDS name this process)
        #[arg(long, default_value = "127.0.0.1:4319")]
        listen: String,
        /// The forest to write into, as `timberfs forest list` names it:
        /// one store per stream
        #[arg(long, value_name = "NAME")]
        forest: Option<String>,
        /// Backing directory. DEPRECATED in favour of --forest; still
        /// the way to write into a directory that is NOT a forest
        #[arg(long = "into-dir", value_name = "DIR", conflicts_with = "forest")]
        into_dir: Option<PathBuf>,
        /// The label whose value names the store
        #[arg(long, default_value = "service", value_name = "LABEL")]
        route: String,
        /// Create a store for a never-seen stream. Default: refuse it and
        /// say so, as the other intakes do
        #[arg(long)]
        auto_create: bool,
        /// Keep the sender's chunk numbering and record its origin, making
        /// this a replica whose `(origin, seq)` addresses match the
        /// source's. Refused when the numbering would not continue
        /// exactly; without it the destination renumbers and claims no
        /// origin, which is weaker but always possible
        #[arg(long)]
        replica: bool,
        /// Declare and maintain the token index on stores this receiver
        /// creates. Its own policy: settings never travel, only labels
        #[arg(long)]
        index: bool,
        /// Declare the write-ahead sidecar on stores this receiver creates
        #[arg(long)]
        wal: bool,
        /// Exit for a clean re-exec when this binary is upgraded on disk
        /// (dpkg replaces it). Only for supervised runs: the unit sets it
        /// and pairs it with RestartForceExitStatus
        #[arg(long)]
        exit_on_upgrade: bool,
    },
    /// Ship a store over the native replication wire. Sends what the
    /// receiver says it lacks, so re-running is a no-op rather than a
    /// re-send. The verb name is provisional.
    FramesSend {
        /// The store to ship
        store: PathBuf,
        /// host:port of a `frames-intake`
        #[arg(long, value_name = "ADDR")]
        endpoint: String,
        /// Keep shipping as chunks seal, on the same connection — what a
        /// service unit for this runs
        #[arg(long, short = 'f')]
        follow: bool,
        /// Record the far end's acknowledged position here, so a store
        /// declaring `cursors=<dir>` can REPORT what has left this box
        /// (`info`, `list`). Not a resume point — the receiver's own
        /// coverage is what a resume reads — and ⚠ not a retention hold
        /// either: `retain_unconsumed` reads the follower registry
        /// alone, and frames cannot be a follower until the `chunks`
        /// diet lands
        #[arg(long, value_name = "PATH")]
        cursor: Option<PathBuf>,
        /// Ship no sidecars, so the receiver rebuilds its own index
        #[arg(long)]
        no_sidecars: bool,
        /// How long to wait between polls of the store with --follow
        #[arg(long, default_value = "1s", value_name = "DUR")]
        poll: String,
        /// Socket read/write timeout
        #[arg(long, default_value = "30s", value_name = "DUR")]
        timeout: String,
    },
    /// Tap the consoles of incus containers into timberfs, over the local
    /// incus unix socket. The console is the "everything else" channel —
    /// boot output, a crashing JVM's fatal log, whatever a process writes
    /// on its way down — and incus keeps only the last 128 KiB of it, in a
    /// ring that wraps in silence. An app that ships its own logs is
    /// better served by `otlp-intake`; this is for the output that arrives
    /// when the app's own logging is already dead.
    #[command(name = "incus-intake")]
    IncusIntake {
        /// The forest to write into, as `timberfs forest list` names it:
        /// each store lives in its own directory named after its id, with the
        /// readable name in its manifest
        #[arg(long, value_name = "NAME")]
        forest: Option<String>,
        /// Backing directory. DEPRECATED in favour of --forest; still
        /// the way to write into a directory that is NOT a forest
        #[arg(long = "into-dir", value_name = "DIR", conflicts_with = "forest")]
        into_dir: Option<PathBuf>,
        /// The incus unix socket
        #[arg(long, default_value = incus::DEFAULT_SOCKET)]
        socket: String,
        /// The incus project whose instances to tap
        #[arg(long, default_value = "default")]
        project: String,
        /// Which facts identify a store, comma-separated. The default is
        /// one store per instance; `type,incus.project,incus.instance,image`
        /// is one per image version, and `type` puts every console on the
        /// host in one store. Whatever is named here is written as a label,
        /// so a store can always be found again by the key that made it
        #[arg(long, default_value = incus_intake::DEFAULT_KEY, value_name = "LABELS")]
        key: String,
        /// What to put in front of a line that STARTS an entry, as a
        /// template over the same facts plus `{time}`: `--prefix '{time}
        /// {incus.instance} '` attributes a store several instances share.
        /// A `{time}` that does not come first is fine — the store is
        /// taught how to find it
        #[arg(long, default_value = "{time} ", value_name = "TEMPLATE")]
        prefix: String,
        /// Tap only these instances (repeatable). Default: every one the
        /// project has
        #[arg(long, value_name = "NAME")]
        only: Vec<String>,
        /// Also tap virtual machines. Their console is file-backed and
        /// carries the kernel's boot output rather than an application's
        /// stdout, so it is a different thing to collect
        #[arg(long)]
        include_vms: bool,
        /// Continuously drop data older than this (e.g. 7d, 12h) in every
        /// store this intake creates
        #[arg(long)]
        retain: Option<String>,
        /// Keep the on-disk size of every store this intake creates at or
        /// under this budget (e.g. 2G, 512M)
        #[arg(long)]
        retain_size: Option<String>,
        /// Declare and maintain the .grain token index on every store this
        /// intake creates
        #[arg(long)]
        index: bool,
        /// How long an entry may go quiet before the next line starts a
        /// new one instead of continuing it. This is what keeps a stack
        /// trace whole while not swallowing the next message into it
        #[arg(long, default_value_t = 100, value_name = "MS")]
        idle: u64,
        /// Keep the console ring buffer instead of consuming it, so
        /// `incus console --show-log` still has something to show for a
        /// tapped container. The cost is duplication: the ring
        /// accumulates its own copy of what this intake has already
        /// written, and the next attach replays it
        #[arg(long)]
        keep_ring: bool,
        /// At most how often to consume the ring while attached. Also the
        /// bound on how much an unclean kill can duplicate
        #[arg(long, default_value = "30s", value_name = "DUR")]
        drain_every: String,
        /// Do not write a marker line at each attach. The marker carries
        /// the image, entrypoint and instance uuid — facts that change
        /// when a container is rebuilt, so they belong in the timeline
        /// rather than in labels that would claim today's values for all
        /// of history
        #[arg(long)]
        no_marker: bool,
        /// Exit for a clean re-exec when this binary is upgraded on disk
        #[arg(long)]
        exit_on_upgrade: bool,
    },
    /// Receive OTLP/HTTP logs — the OpenTelemetry wire protocol every SDK
    /// and the Collector speak — writing each stream into its own store
    /// under --into-dir. Answers 200 only after the batch is fsynced, and
    /// 503 + Retry-After for an undeclared stream, so a sender buffers and
    /// converges. Both OTLP/HTTP encodings are accepted (binary protobuf,
    /// what every sender defaults to, and JSON), gzipped or not, so a stock
    /// Collector needs no configuration. Deliberate limitations: POST
    /// /v1/logs only (no traces or metrics), no chunked request bodies, no
    /// TLS (loopback or a private network only), no gRPC — put a Collector
    /// in front for :4317. The verb name is provisional
    #[command(name = "otlp-intake")]
    OtlpIntake {
        /// Address to listen on (systemd socket activation on fd 3 is used
        /// instead when LISTEN_PID/LISTEN_FDS name this process)
        #[arg(long, default_value = "127.0.0.1:4318")]
        listen: String,
        /// The forest to write into, as `timberfs forest list` names it:
        /// one store per stream, named <route value>.log
        #[arg(long, value_name = "NAME")]
        forest: Option<String>,
        /// Backing directory. DEPRECATED in favour of --forest; still
        /// the way to write into a directory that is NOT a forest
        #[arg(long = "into-dir", value_name = "DIR", conflicts_with = "forest")]
        into_dir: Option<PathBuf>,
        /// Resource attribute whose value names the store; absent on a
        /// batch, OTel's own unknown_service is used
        #[arg(long, default_value = "service.name", value_name = "ATTR")]
        route: String,
        /// Continuously drop data older than this (e.g. 30d, 12h, 90m) in
        /// every store this receiver creates
        #[arg(long)]
        retain: Option<String>,
        /// Keep the on-disk (compressed) size of every store this receiver
        /// creates at or under this budget (e.g. 200G, 512M)
        #[arg(long)]
        retain_size: Option<String>,
        /// Declare and maintain the .grain token index on every store this
        /// receiver creates
        #[arg(long)]
        index: bool,
        /// Create a store for a never-seen stream automatically. Default:
        /// refuse undeclared streams with 503 — pre-create stores with
        /// `timberfs create --wal`; the sender retries until they exist
        #[arg(long)]
        auto_create: bool,
        /// Largest request body accepted (e.g. 16M, 64M)
        #[arg(long, default_value = "16M", value_name = "SIZE")]
        max_body: String,
        /// Exit for a clean re-exec when this binary is upgraded on disk
        /// (dpkg replaces it). Only for supervised runs (the systemd unit
        /// sets it and pairs it with RestartForceExitStatus)
        #[arg(long)]
        exit_on_upgrade: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum IdentitySideArg {
    Index,
    Manifest,
}

#[derive(Subcommand)]
enum ForestCommand {
    /// Declare a directory as a forest: create it if needed and write
    /// its config. Idempotent, so provisioning may run it on every boot.
    /// Refuses a directory that is already a forest, or that nests with
    /// one — a forest is scanned one level deep, so overlapping forests
    /// would make the stores between them members of both
    Create {
        /// The directory stores under this forest live in. Made
        /// absolute against the current directory if it is not already,
        /// because daemons read this config from a different one
        dir: PathBuf,
        /// The forest's name: [A-Za-z0-9_.-], defaults to the
        /// directory's last component. It names the config file and
        /// appears in every ambiguity message
        #[arg(long)]
        name: Option<String>,
        /// Print what would be declared and written, and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// What is declared, and whether it is usable: store count, and
    /// whether the directory is missing or unwritable — the failure that
    /// otherwise surfaces as "store not found" somewhere far away
    List {
        /// One forest name per line, for scripts
        #[arg(long)]
        names: bool,
        #[arg(long)]
        json: bool,
    },
    /// Un-declare a forest. The directory and every store in it are left
    /// exactly as they are — this removes the declaration, never data
    Remove {
        /// The forest's name, as `timberfs forest list` prints it
        name: String,
        /// Print what would be removed, and change nothing
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum FollowerCommand {
    /// Register a follower: a selection, a consumer to feed it to, and
    /// whether its positions hold retention back. The name is
    /// host-unique and is also the systemd instance
    /// (`timberfs-follower@<name>`), so it is what gets typed into
    /// `systemctl status` — refused if taken, so a collision is a
    /// registration error rather than two processes overwriting one
    /// position
    Create {
        /// The follower's name: [A-Za-z0-9_.-], host-unique
        name: String,
        /// WHICH stores to follow: the predicate `list --select` takes.
        /// `[]` is every store, which is a thing to have written rather
        /// than a flag to leave out. Re-resolved on every poll, so a
        /// store that appears later is picked up with no change here
        #[arg(long, value_name = "EXPR", conflicts_with = "store")]
        select: Option<String>,
        /// One store instead of a predicate: a path or a forest handle,
        /// recorded by IDENTITY (its .bark id, minted here if it has
        /// none) as the one-term selection `[id=...]` — a store can move
        #[arg(long, value_name = "STORE")]
        store: Option<PathBuf>,
        /// Where a store this follower has never read is picked up:
        /// `discovery` (the default unless --retaining) ships one born
        /// since the follower was declared from its beginning and one
        /// that predates it from its next byte; `begin` (the default
        /// WITH --retaining, which promises the data is not lost until
        /// this follower has it) ships everything; `end` ships no
        /// history at all
        #[arg(long, value_name = "WHERE", value_parser = ["begin", "end", "discovery"])]
        follow_from: Option<String>,
        /// Declare that this follower's position holds the store's head
        /// back: retention keeps what it has not read ON TOP OF what age
        /// and size keep, never as a cap on them — `retain_size` still
        /// overrides it, and the writer records what was dropped unread.
        /// Takes effect only where the store declares
        /// `retain_unconsumed`. Note that one with no position yet holds
        /// EVERYTHING — which is the point (it protects a follower
        /// deployed before it first runs) and also the footgun: start it
        #[arg(long)]
        retaining: bool,
        /// systemctl enable the unit
        #[arg(long)]
        enable: bool,
        /// systemctl start the unit
        #[arg(long)]
        start: bool,
        /// Print the declaration and the command line it would run,
        /// registering nothing
        #[arg(long)]
        dry_run: bool,
        /// The CONSUMER and its arguments, after `--`: the program fed
        /// the records, which reports how far to move the positions.
        /// `-- timber-otlp --endpoint http://collector:4318`, or
        /// `-- ssh archive01 my-consumer` for a destination on another
        /// machine. A list, so nothing makes a quoting round trip; and
        /// recorded verbatim, since what is not ours to interpret is
        /// passed on unread
        #[arg(last = true, value_name = "CONSUMER")]
        command: Vec<String>,
    },
    /// Every registered follower: name, store, type, whether it retains,
    /// its position, how far behind it is, and whether it is running
    /// (from its lock, so a stale unit state cannot lie about it)
    List {
        /// Only followers of this store (a path, or a forest handle)
        #[arg(long, value_name = "STORE")]
        store: Option<PathBuf>,
        /// Bare names only, one per line, no header or columns (what
        /// shell completion consumes)
        #[arg(long, conflicts_with = "json")]
        names: bool,
        /// A JSON array of objects instead of the human table
        #[arg(long)]
        json: bool,
    },
    /// One follower on one screen: what it declares, where it stands in
    /// its store, whether anything is honouring its `retaining` flag,
    /// and whether it is running
    Status {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Change a follower's declaration. `update <name> retaining=false`
    /// is the first half of retiring one, and it says what that frees —
    /// separate from `delete` because the destructive act deserves its
    /// own command
    Update {
        name: String,
        /// KEY=VALUE: retaining=true|false, select=EXPR, store=PATH,
        /// follow_from=begin|end|discovery.
        /// The consumer is replaced wholesale with what follows `--`
        #[arg(value_name = "KEY=VALUE")]
        sets: Vec<String>,
        /// Remove a key (repeatable): --unset endpoint
        #[arg(long = "unset", value_name = "KEY")]
        unsets: Vec<String>,
        /// Preview the new declaration without writing it
        #[arg(long)]
        dry_run: bool,
        /// Replace the CONSUMER wholesale with what follows `--`
        #[arg(last = true, value_name = "CONSUMER")]
        args: Vec<String>,
    },
    /// Unregister a follower. Refused while it is `retaining` (release
    /// the head deliberately first, with `update retaining=false`) and
    /// while it is running. Both refusals are about deliberateness, not
    /// prevention, so there is no --force: the two-step IS the force
    Delete {
        name: String,
        /// systemctl stop the unit first
        #[arg(long)]
        stop: bool,
        /// systemctl disable the unit too
        #[arg(long)]
        disable: bool,
    },
    /// Read a follower's declaration and RUN it — what the systemd
    /// template runs: the selection is read, its consumer is fed, and
    /// each store's position moves as far as that consumer says it got.
    /// The consumer is a child, so systemd keeps the lifecycle, the
    /// restarts and the journal
    Run { name: String },
}

fn main() -> anyhow::Result<()> {
    // Die quietly when a pipe closes (query | head), like any Unix tool,
    // instead of Rust's default panic-on-EPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    note::set_quiet(cli.quiet);
    match cli.command {
        Command::Mount {
            backing,
            mountpoint,
            chunk_size,
            level,
            flush_age,
            allow_other,
            exit_on_upgrade,
        } => {
            let cfg = store::Config {
                chunk_size: chunk_size.max(1),
                level,
                flush_age_ms: (flush_age * 1000.0).max(0.0) as u64,
            };
            let s = store::Store::open(&backing, cfg)?;
            eprintln!(
                "timberfs: serving {} on {} ({} existing file(s), chunk {} B, zstd -{}, flush age {}s)",
                backing.display(),
                mountpoint.display(),
                s.files.len(),
                cfg.chunk_size,
                cfg.level,
                flush_age
            );
            fs::mount(s, &mountpoint, allow_other, exit_on_upgrade)?;
        }
        Command::Create {
            dest,
            index,
            wal,
            retain,
            retain_size,
            retain_unconsumed,
            sets,
            if_not_exists,
        } => {
            bark::cmd_create(
                &dest,
                index,
                wal,
                retain.as_deref(),
                retain_size.as_deref(),
                retain_unconsumed,
                &sets,
                if_not_exists,
            )?;
        }
        Command::Set {
            store,
            sets,
            unsets,
        } => {
            let store = forest::resolve_source(&store)?;
            bark::cmd_set(&store, &sets, &unsets)?;
        }
        Command::Append {
            into,
            records,
            legacy,
            chunk_size,
            level,
            flush_age,
            wal,
            retain,
            retain_size,
            exit_on_upgrade,
            wait_for_writer,
        } => {
            let Some(into) = into else {
                if let Some(l) = legacy.first() {
                    anyhow::bail!(
                        "append writes --into DEST (destinations are always named; \
                         positionals are sources): timberfs append --into {l}"
                    );
                }
                anyhow::bail!("append needs a destination: --into DEST");
            };
            if let Some(l) = legacy.first() {
                anyhow::bail!(
                    "unexpected positional {l:?} (append reads stdin and writes \
                     --into DEST; positionals are sources, and append has none)"
                );
            }
            let cfg = store::Config {
                chunk_size: chunk_size.max(1),
                level,
                flush_age_ms: (flush_age * 1000.0).max(0.0) as u64,
            };
            if records {
                sink::cmd_records_sink(
                    None,
                    &into,
                    cfg,
                    sink::Delivery::Streaming,
                    sink::Clock::Now,
                    wal,
                    retain.as_deref(),
                    retain_size.as_deref(),
                    "append",
                    exit_on_upgrade,
                    wait_for_writer,
                )?;
            } else {
                append::cmd_append(
                    &into,
                    cfg,
                    wal,
                    retain.as_deref(),
                    retain_size.as_deref(),
                    exit_on_upgrade,
                    wait_for_writer,
                )?;
            }
        }
        Command::Import {
            sources,
            dest,
            records,
            chunk_size,
            level,
            timestamp_regex,
            timestamp_format,
            utc,
            quick,
            index,
            wal,
            follow,
            rotated,
            poll,
            flush_age,
            retain,
            retain_size,
            exit_on_upgrade,
            wait_for_writer,
        } => {
            let cfg = store::Config {
                chunk_size: chunk_size.max(1),
                level,
                flush_age_ms: if follow {
                    (flush_age * 1000.0).max(0.0) as u64
                } else {
                    u64::MAX // no age flushing during a one-shot import
                },
            };
            if follow {
                let [source] = &sources[..] else {
                    anyhow::bail!(
                        "--follow takes exactly ONE source: it is a live position in one \
                         file, and a store has one writer (run a follower per store)"
                    );
                };
                follow::cmd_follow(
                    source,
                    &dest,
                    cfg,
                    import::ImportOpts {
                        time: bark::TimeFormat {
                            regex: timestamp_regex,
                            format: timestamp_format,
                            utc,
                        },
                        quick: false,
                        index,
                        wal,
                    },
                    retain.as_deref(),
                    retain_size.as_deref(),
                    follow::FollowOpts {
                        poll_ms: (poll * 1000.0).max(50.0) as u64,
                        rotated,
                        exit_on_upgrade,
                        wait_for_writer,
                    },
                )?;
            } else if records {
                if sources.len() > 1 {
                    anyhow::bail!(
                        "--records takes ONE stream (a records file, or stdin \
                         when no source is given) — merge upstream, or import \
                         streams one at a time"
                    );
                }
                if index {
                    let (d, n) = query::resolve_backing(&dest)?;
                    std::fs::create_dir_all(&d)
                        .with_context(|| format!("creating backing directory {}", d.display()))?;
                    bark::declare_index(&d, &n)?;
                }
                sink::cmd_records_sink(
                    sources.first().map(|p| p.as_path()),
                    &dest,
                    cfg,
                    sink::Delivery::Atomic,
                    sink::Clock::FromStamps,
                    wal,
                    None,
                    None,
                    "import",
                    false,
                    // A one-shot command: no supervisor is handing a
                    // writer over, so a held lock is the answer.
                    0.0,
                )?;
            } else {
                if sources.is_empty() {
                    anyhow::bail!(
                        "at least one source log is required (or --records for a stream)"
                    );
                }
                import::cmd_import(
                    &sources,
                    &dest,
                    cfg,
                    import::ImportOpts {
                        time: bark::TimeFormat {
                            regex: timestamp_regex,
                            format: timestamp_format,
                            utc,
                        },
                        quick,
                        index,
                        wal,
                    },
                )?;
            }
        }
        Command::Export {
            source,
            dest,
            legacy,
            from,
            to,
            fail_on_empty,
        } => {
            let Some(dest) = dest else {
                if let Some(l) = legacy.first() {
                    anyhow::bail!(
                        "export writes --into DEST (destinations are always named; \
                         positionals are sources): timberfs export {} --into {l}",
                        source.display()
                    );
                }
                anyhow::bail!("export needs a destination: --into DEST");
            };
            if let Some(l) = legacy.first() {
                anyhow::bail!(
                    "unexpected positional {l:?} (export reads SOURCE and writes \
                     --into DEST)"
                );
            }
            let source = forest::resolve_source(&source)?;
            export::cmd_export(&source, &dest, from, to, fail_on_empty)?;
        }
        Command::Query {
            files,
            from,
            to,
            has,
            any,
            no_filename,
            show_write_time,
            by_write_time,
            null_sep,
            records,
            follow,
            tail,
            max,
            poll,
            from_chunk,
            deadline,
            query: query_doc,
            dump_json,
        } => {
            let files = files
                .iter()
                .map(|f| forest::resolve_source(f))
                .collect::<anyhow::Result<Vec<_>>>()?;
            // A document, or the flags that build the same value: one
            // question asked two ways, never two dialects of it.
            if let Some(doc) = query_doc {
                let doc = querydoc::read(&doc)?;
                // Listing stores and reading entries are the same
                // document asking for different answers, so the kind
                // decides which one runs.
                if doc.lists_stores() {
                    doc.to_query()?; // its refusals still apply
                    let stores = list::stores_json(&[], Some(&doc.store_selector()))?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&querydoc::Answer::with_stores(stores)?)?
                    );
                    return Ok(());
                }
                query::cmd_query(&doc.to_query()?)?;
                return Ok(());
            }
            let q = query::Query {
                sources: files,
                // No flag carries a position: resuming is the document's,
                // because a cursor is what a PREVIOUS ANSWER said, and the
                // flags describe a search rather than a place in one.
                cursor: Default::default(),
                window: query::Window {
                    from,
                    to,
                    from_chunk,
                },
                // The flags are the chunk sweep, and have always been:
                // `--has` selects chunks via the token index. The richer
                // predicates (substring, regex, caseless, negated) are the
                // document's, and `timber-filter` is their command line.
                matching: query::Match {
                    all: has.iter().map(|t| timberfs::grep::Pred::has(t)).collect(),
                    any: any.iter().map(|t| timberfs::grep::Pred::has(t)).collect(),
                    none: Vec::new(),
                    granularity: query::Granularity::Chunks,
                },
                // --max/--tail count entries; the flags have no chunk
                // bound, which is the document's.
                limit: query::Limit {
                    max,
                    tail,
                    max_chunks: None,
                    tail_chunks: None,
                    deadline_ms: deadline.map(|s| (s * 1000.0).max(0.0) as u64),
                    // The flags are the operator at a shell, and this
                    // machine's ceilings bound a request from elsewhere.
                    imposed: Default::default(),
                },
                output: query::Output {
                    no_filename,
                    show_write_time,
                    null_sep,
                    records,
                    by_write_time,
                    // `--by-write-time` is the text dump it has always
                    // been; framed chunks are the document's answer.
                    chunk_records: false,
                },
                follow: query::Follow { follow, poll },
            };
            if dump_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&querydoc::Document::of(&q)?)?
                );
                return Ok(());
            }
            query::cmd_query(&q)?;
        }
        Command::Limits { json } => {
            let (limits, problems, from) = timberfs::limits::describe()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&limits)?);
            } else {
                println!("ceilings on one request, from {from}:");
                let say = |what: &str, v: Option<u64>, unit: &str| {
                    println!(
                        "  {what:12} {}",
                        v.map_or("none".to_string(), |n| format!("{n} {unit}"))
                    );
                };
                say("MAX_ENTRIES", limits.max_entries, "entries");
                say("MAX_CHUNKS", limits.max_chunks, "chunks");
                say("DEADLINE_MS", limits.deadline_ms, "ms");
            }
            // A line that set no ceiling is the whole reason this command
            // exists, so it is the exit code and not a footnote.
            for why in &problems {
                eprintln!("timberfs: {why}");
            }
            if !problems.is_empty() {
                std::process::exit(1);
            }
        }
        Command::Forest { command } => match command {
            ForestCommand::Create { dir, name, dry_run } => {
                forest::cmd_create(&dir, name.as_deref(), dry_run)?
            }
            ForestCommand::List { names, json } => forest::cmd_list(json, names)?,
            ForestCommand::Remove { name, dry_run } => forest::cmd_remove(&name, dry_run)?,
        },
        Command::Feed {
            select,
            positions,
            follow,
            batch_size,
            poll,
            follow_from,
            forest,
            consumer,
        } => {
            if batch_size == 0 {
                anyhow::bail!("--batch-size must be at least 1");
            }
            feed::run(feed::Opts {
                selector: select::Selector::parse(&select)?,
                dirs: forest,
                positions,
                batch_entries: batch_size,
                poll: std::time::Duration::from_millis(append::parse_duration_ms(&poll)?.max(1)),
                follow,
                argv: consumer,
                hello_wait: feed::HELLO_WAIT,
                follow_from: ship::FollowFrom::parse(&follow_from)?,
                // No declaration, so the positions file is the only
                // record of when this interest began.
                since: None,
            })?;
        }
        Command::Follower { command } => match command {
            FollowerCommand::Create {
                name,
                select,
                store,
                follow_from,
                retaining,
                enable,
                start,
                dry_run,
                command,
            } => follower::cmd_create(
                &name,
                follower::CreateOpts {
                    select,
                    store,
                    follow_from: follow_from
                        .as_deref()
                        .map(ship::FollowFrom::parse)
                        .transpose()?,
                    retaining,
                    enable,
                    start,
                    dry_run,
                    command,
                },
            )?,
            FollowerCommand::List { store, names, json } => {
                follower::cmd_list(store.as_deref(), names, json)?
            }
            FollowerCommand::Status { name, json } => follower::cmd_status(&name, json)?,
            FollowerCommand::Update {
                name,
                sets,
                unsets,
                dry_run,
                args,
            } => {
                // An empty `--` tail and no `--` at all are the same
                // argv, so "replace the arguments with nothing" has to be
                // spelled `--unset args` rather than inferred from
                // silence — clearing them by accident would drop an
                // endpoint's headers.
                let args = if args.is_empty() { None } else { Some(args) };
                follower::cmd_update(&name, &sets, &unsets, args, dry_run)?
            }
            FollowerCommand::Delete {
                name,
                stop,
                disable,
            } => follower::cmd_delete(&name, stop, disable)?,
            FollowerCommand::Run { name } => follower::cmd_run(&name)?,
        },
        Command::Info { file, json } => {
            let file = forest::resolve_source(&file)?;
            query::cmd_info(&file, json)?;
        }
        Command::IncusIntake {
            forest,
            into_dir,
            socket,
            project,
            key,
            prefix,
            only,
            include_vms,
            retain,
            retain_size,
            index,
            idle,
            keep_ring,
            drain_every,
            no_marker,
            exit_on_upgrade,
        } => {
            let into_dir = forest::into_dir(forest.as_deref(), into_dir)?;
            let known = incus_intake::known_facts();
            incus_intake::run(incus_intake::IncusOpts {
                socket,
                project,
                into_dir,
                key: incus_intake::parse_key(&key)?,
                prefix: incus_intake::parse_prefix(&prefix, &known)?,
                include_vms,
                only,
                retain,
                retain_size,
                index,
                idle_ms: idle,
                keep_ring,
                drain_every_ms: append::parse_duration_ms(&drain_every)?,
                mark_episodes: !no_marker,
                exit_on_upgrade,
            })?;
        }
        Command::Identity { file, mint, keep } => {
            let file = forest::resolve_source(&file)?;
            bark::cmd_identity(
                &file,
                mint,
                keep.map(|k| match k {
                    IdentitySideArg::Index => bark::IdentitySide::Index,
                    IdentitySideArg::Manifest => bark::IdentitySide::Manifest,
                }),
            )?;
        }
        Command::Index { file } => {
            let file = forest::resolve_source(&file)?;
            query::cmd_index(&file)?;
        }
        Command::List {
            dirs,
            forests,
            names,
            json,
            select,
            full_id,
        } => {
            // A forest NAME resolves to its directory, so `list` keeps
            // taking one list of directories and gains no second path.
            let dirs = if forests.is_empty() {
                dirs
            } else {
                forests
                    .iter()
                    .map(|n| forest::dir_of(n))
                    .collect::<anyhow::Result<Vec<_>>>()?
            };
            list::cmd_list(&dirs, names, json, select.as_deref(), full_id)?;
        }
        Command::Reindex { file } => {
            let file = forest::resolve_source(&file)?;
            grain::cmd_reindex(&file)?;
        }
        Command::Trim { store, dry_run } => {
            let store = forest::resolve_source(&store)?;
            rotate::cmd_trim(&store, dry_run)?;
        }
        Command::Rotate {
            source,
            dest,
            cutoff,
            delete,
            dry_run,
            fail_on_empty,
        } => {
            let source = forest::resolve_source(&source)?;
            rotate::cmd_rotate(
                &source,
                dest.as_deref(),
                cutoff,
                delete,
                dry_run,
                fail_on_empty,
            )?;
        }
        Command::ForwardIntake {
            listen,
            forest,
            into_dir,
            payload_key,
            retain,
            retain_size,
            index,
            auto_create,
            exit_on_upgrade,
        } => {
            let into_dir = forest::into_dir(forest.as_deref(), into_dir)?;
            forward::cmd_forward_intake(
                &listen,
                &into_dir,
                forward::ForwardOpts {
                    payload_key,
                    retain,
                    retain_size,
                    index,
                    auto_create,
                },
                exit_on_upgrade,
            )?;
        }
        Command::FramesIntake {
            listen,
            forest,
            into_dir,
            route,
            auto_create,
            replica,
            index,
            wal,
            exit_on_upgrade,
        } => timberfs::frames::cmd_intake(&timberfs::frames::IntakeOpts {
            listen,
            into_dir: forest::into_dir(forest.as_deref(), into_dir)?,
            route,
            auto_create,
            replica,
            index,
            wal,
            exit_on_upgrade,
        })?,
        Command::FramesSend {
            store,
            endpoint,
            follow,
            cursor,
            no_sidecars,
            poll,
            timeout,
        } => {
            let ms = |s: &str| -> anyhow::Result<std::time::Duration> {
                Ok(std::time::Duration::from_millis(
                    timberfs::append::parse_duration_ms(s)?,
                ))
            };
            let sent = timberfs::frames::cmd_send(
                &store,
                &timberfs::frames::SendOpts {
                    endpoint: endpoint.clone(),
                    first_seq: 0,
                    sidecars: !no_sidecars,
                    timeout: ms(&timeout)?,
                    follow,
                    poll: ms(&poll)?,
                    cursor,
                },
            )?;
            if sent.chunks == 0 {
                timberfs::note!("timberfs: {endpoint} already has everything; nothing sent");
            } else {
                timberfs::note!(
                    "timberfs: sent {} chunk(s), {} to {endpoint}{}",
                    sent.chunks,
                    timberfs::rotate::human_bytes(sent.comp_bytes),
                    if sent.skipped_already_held > 0 {
                        format!(
                            " (resumed at {}, which it already held)",
                            sent.skipped_already_held
                        )
                    } else {
                        String::new()
                    }
                );
            }
        }
        Command::OtlpIntake {
            listen,
            forest,
            into_dir,
            route,
            retain,
            retain_size,
            index,
            auto_create,
            max_body,
            exit_on_upgrade,
        } => {
            let into_dir = forest::into_dir(forest.as_deref(), into_dir)?;
            let max_body = timberfs::append::parse_size_bytes(&max_body)? as usize;
            otlp_intake::cmd_otlp_intake(
                &listen,
                &into_dir,
                otlp_intake::OtlpOpts {
                    route,
                    retain,
                    retain_size,
                    index,
                    auto_create,
                    max_body,
                },
                exit_on_upgrade,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::CommandFactory;

    /// Clap validates the command tree only when one is built, so a
    /// duplicated name or a doc comment attached to the wrong variant
    /// panics at STARTUP and no amount of library testing sees it. This
    /// test is that check, and it costs microseconds.
    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// `--dump-json` reads nothing, so requiring a store made the one job
    /// it is uniquely good for impossible: telling a client what a typed
    /// time means, without that client writing a second date parser. The
    /// rule lives in one clap attribute, so it is checked here rather than
    /// by installing a package and running the binary.
    #[test]
    fn only_a_read_that_reads_needs_a_store() {
        assert!(
            Cli::try_parse_from(["timberfs", "query", "--from", "11:00", "--dump-json"]).is_ok(),
            "--dump-json reads nothing, so it needs no store"
        );
        assert!(
            Cli::try_parse_from(["timberfs", "query", "--query", "q.json"]).is_ok(),
            "a document names its own stores"
        );
        let e = match Cli::try_parse_from(["timberfs", "query", "--from", "11:00"]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a real read ran with no store"),
        };
        assert!(e.contains("FILES"), "a real read must still name one: {e}");
    }

    /// `import /logs/*` with the `--into` forgotten: the shell expands the
    /// glob, the last path becomes the destination, and a real log is
    /// overwritten by a store. It is an argument error whatever the glob
    /// expanded to — including a single file, where the mistake looks
    /// exactly like a correct one-file import.
    #[test]
    fn a_forgotten_destination_is_an_argument_error() {
        for args in [
            vec!["timberfs", "import", "a.log", "b.log"],
            vec!["timberfs", "import", "a.log"],
        ] {
            let e = match Cli::try_parse_from(&args) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("{args:?} was accepted with no destination"),
            };
            assert!(e.contains("--into"), "{args:?}: {e}");
        }
        assert!(Cli::try_parse_from(["timberfs", "import", "a.log", "--into", "s/s.log"]).is_ok());
    }

    /// Every subcommand must appear in the man page. Documentation drift
    /// is invisible to every other check: `forward-intake` and
    /// `otlp-intake` were each shipped before this existed, and only an
    /// audit found whether they were covered.
    #[test]
    fn every_subcommand_is_in_the_man_page() {
        let man = include_str!("../packaging/timberfs.1");
        let heads: Vec<String> = man
            .lines()
            .filter_map(|l| l.strip_prefix(".SS "))
            .map(|h| h.replace("\\-", "-").trim().to_string())
            .collect();
        for sub in Cli::command().get_subcommands() {
            let name = sub.get_name();
            if name == "help" {
                continue;
            }
            assert!(
                heads.iter().any(|h| h == name),
                "`{name}` has no `.SS {name}` in packaging/timberfs.1"
            );
        }
    }

    /// Every subcommand FLAG must appear in the man page too. The
    /// subcommand check above passes as soon as `.SS query` exists, so a
    /// flag added to a documented subcommand reaches nothing: `--query`
    /// and `--dump-json` shipped that way, and so did `list --select`,
    /// `frames-send --timeout` and `incus-intake --no-marker`. Presence
    /// anywhere in the page is enough — this catches ABSENCE, which is
    /// the failure nothing else sees.
    ///
    /// NESTED verbs are walked too: `follower create` is where the
    /// followers surface lives, and a check that stopped at the top level
    /// saw none of it.
    ///
    /// Omissions are allowed but must be stated here, so leaving one out
    /// is a decision somebody made rather than something nobody noticed.
    #[test]
    fn every_flag_is_in_the_man_page() {
        // (subcommand, flag, why it is deliberately absent)
        let allowed: &[(&str, &str, &str)] = &[];
        let man = include_str!("../packaging/timberfs.1");
        // roff escapes a leading hyphen as `\-`, so `--select` is written
        // `\-\-select`. Compare against the page with those undone.
        let plain = man.replace("\\-", "-");
        let mut missing = Vec::new();
        let cli = Cli::command();
        let mut queue: Vec<(String, &clap::Command)> = cli
            .get_subcommands()
            .map(|s| (s.get_name().to_string(), s))
            .collect();
        while let Some((name, sub)) = queue.pop() {
            if name.ends_with("help") {
                continue;
            }
            for nested in sub.get_subcommands() {
                queue.push((format!("{name} {}", nested.get_name()), nested));
            }
            for arg in sub.get_arguments() {
                let Some(long) = arg.get_long() else { continue };
                if long == "help" || long == "version" {
                    continue;
                }
                if allowed
                    .iter()
                    .any(|(s, f, _)| *s == name.as_str() && *f == long)
                {
                    continue;
                }
                if !plain.contains(&format!("--{long}")) {
                    missing.push(format!("{name} --{long}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these flags appear nowhere in packaging/timberfs.1: {missing:?}\n\
             Document them, or add them to this test's allowlist with a reason."
        );
    }

    /// Every long flag that takes a VALUE must be known to the bash
    /// completion, so the word after it gets file completion rather than
    /// the store handles a positional would get. This list had fallen
    /// SIXTEEN flags behind before the test existed — `--query <TAB>`
    /// offered store names where a file was wanted.
    #[test]
    fn every_value_flag_is_known_to_the_completion() {
        let comp = include_str!("../packaging/timberfs-completion.bash");
        // The block listing them, so a mention anywhere else in the
        // script (a dedicated handler, a comment) does not count as
        // covered by accident.
        let start = comp
            .find("Every long flag that takes a VALUE")
            .expect("the value-flag block moved or was renamed");
        let block = &comp[start..start + comp[start..].find("esac").unwrap()];
        // A flag with its own handler is covered by that instead.
        let handled_elsewhere = ["--forest"];
        let mut missing = Vec::new();
        for sub in Cli::command().get_subcommands() {
            for arg in sub.get_arguments() {
                let Some(long) = arg.get_long() else { continue };
                // Takes a value = not a bare switch.
                if matches!(arg.get_action(), clap::ArgAction::SetTrue)
                    || long == "help"
                    || long == "version"
                    || handled_elsewhere.contains(&format!("--{long}").as_str())
                {
                    continue;
                }
                let flag = format!("--{long}");
                if !block.contains(&flag) {
                    missing.push(flag);
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "the bash completion does not know these value-taking flags, so the word after \
             each gets store-handle completion instead of a file: {missing:?}"
        );
    }

    /// Every subcommand must be reachable from the prose docs too, not
    /// only the man page. The failures this catches are ABSENCES: a
    /// capability that landed and never reached a surface, which is how
    /// `frames-send` came to be missing from the deployment guide and the
    /// use cases while working fine. A drift check over commands that ARE
    /// documented would not have seen any of it.
    ///
    /// Omissions are allowed but must be stated here, so leaving one out
    /// is a decision somebody made rather than something nobody noticed.
    #[test]
    fn every_subcommand_reaches_the_prose_docs() {
        // (surface, subcommands deliberately absent, and why)
        let surfaces: &[(&str, &[(&str, &str)])] = &[
            (
                include_str!("../README.md"),
                &[(
                    "reindex",
                    "maintenance verb; the README sends readers to man timberfs for those",
                )],
            ),
            (include_str!("../docs/deployment.md"), &[]),
            (
                include_str!("../docs/use-cases.md"),
                &[
                    (
                        "reindex",
                        "a use case is a why, not a verb list; rebuilding an index is neither",
                    ),
                    (
                        "identity",
                        "a repair verb for a store that is already broken, not a reason to \
                         reach for timberfs; the man page and the deployment guide carry it",
                    ),
                ],
            ),
        ];
        for (text, allowed) in surfaces {
            for sub in Cli::command().get_subcommands() {
                let name = sub.get_name();
                if name == "help" || allowed.iter().any(|(a, _)| *a == name) {
                    continue;
                }
                assert!(
                    text.contains(name),
                    "`{name}` appears in no command block or prose of one of the \
                     documentation surfaces. Document it, or add it to that \
                     surface's allowlist with a reason."
                );
            }
        }
    }

    /// Every subcommand needs a description. A doc comment that ends up
    /// attached to the wrong variant leaves its own blank, which `--help`
    /// shows and nothing else does — `info` shipped that way once.
    #[test]
    fn every_subcommand_describes_itself() {
        for sub in Cli::command().get_subcommands() {
            let name = sub.get_name();
            if name == "help" {
                continue;
            }
            assert!(
                sub.get_about().is_some_and(|a| !a.to_string().is_empty()),
                "`{name}` has no description"
            );
        }
    }
}
