mod append;
mod bark;
mod export;
mod format;
mod fs;
mod grain;
mod grep;
mod import;
mod note;
mod query;
mod rotate;
mod store;

use std::path::PathBuf;

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
    },
    /// Create an empty timberfs log with its properties declared up
    /// front in a .bark manifest — database-style: `create --index` is
    /// CREATE INDEX, and every later import maintains the .grain
    /// automatically
    Create {
        /// Backing file to create: logical name, .trunk or .rings path
        dest: PathBuf,
        /// Declare the token index for this log
        #[arg(long)]
        index: bool,
        /// Declare retention: continuously drop data older than this
        /// (e.g. 90d, 12h) — enforced by every writer
        #[arg(long)]
        retain: Option<String>,
        /// Declare a compressed-size budget (e.g. 50G, 512M); oldest
        /// data drops first — enforced by every writer
        #[arg(long)]
        retain_size: Option<String>,
        /// Set a manifest property (key=value, e.g. host=foo.bar.com);
        /// repeatable, free-form
        #[arg(long = "set", value_name = "KEY=VALUE")]
        sets: Vec<String>,
    },
    /// Declare or change a store's properties in its .bark manifest —
    /// validated and atomic, unlike hand-editing. Live writers re-read
    /// the manifest within a second, so changes need no restart:
    /// `timberfs set backing/app.log retain=30d`
    Set {
        /// Backing file: logical name, .trunk or .rings path
        store: PathBuf,
        /// KEY=VALUE to set: retain=90d, retain_size=50G,
        /// index=true|false, or any free-form provenance key
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
        /// Backing file: logical name, .trunk or .rings path
        file: PathBuf,
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
        /// Continuously drop data older than this (e.g. 30d, 12h, 90m)
        #[arg(long)]
        retain: Option<String>,
        /// Keep the on-disk (compressed) size at or under this budget
        /// (e.g. 200G, 512M); oldest data is dropped first
        #[arg(long)]
        retain_size: Option<String>,
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
        /// .timber bundles
        #[arg(required = true, num_args = 1..)]
        sources: Vec<PathBuf>,
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
    },
    /// Export a time window (or everything) from a timberfs log into a NEW
    /// timberfs log, chunks copied verbatim — no recompression. A DEST
    /// ending in .timber writes the single-file transfer bundle (plain
    /// tar: .rings first, .trunk second), which import accepts directly.
    Export {
        /// Source backing file: logical name, .trunk or .rings path
        source: PathBuf,
        /// Destination: new backing file, or a *.timber bundle
        dest: PathBuf,
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
        /// by chunk time-windows with grep-style "path:" line prefixes
        #[arg(required = true, num_args = 1..)]
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
        /// Never prefix output lines with the file name
        #[arg(long)]
        no_filename: bool,
    },
    /// Entry-aware grep: matches PATTERN against whole log entries (a
    /// timestamped line plus its continuation lines — stack traces stay
    /// attached to their entry). Reads raw log from stdin or a plain
    /// file, or a timberfs log/bundle where --from/--to/--has pre-select
    /// chunks first. Pipe several greps for entry-level AND.
    Grep {
        /// Text matched at token boundaries — the fast default, index-
        /// accelerated on grain-indexed logs (ERROR finds the word ERROR,
        /// not ERRORS). -F matches raw substrings, --regex full regexes;
        /// both read everything. May be left out when --has/--from/--to
        /// select instead: the first argument is then the file
        pattern: String,
        /// Raw log file(s), timberfs backing file(s), or .timber
        /// bundle(s), processed in order with "path:" prefixes when
        /// several (default: raw log on stdin)
        files: Vec<PathBuf>,
        /// Case-insensitive matching
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// Print entries that do NOT match
        #[arg(short = 'v', long)]
        invert: bool,
        /// PATTERN is a raw substring: matches inside tokens too
        /// (partial ids). Reads every chunk
        #[arg(short = 'F', long)]
        fixed: bool,
        /// Print only the number of matching entries
        #[arg(short = 'c', long)]
        count: bool,
        /// Start of the time window (timberfs sources only; same
        /// formats as query)
        #[arg(long, value_parser = query::parse_time)]
        from: Option<u64>,
        /// End of the time window (timberfs sources only)
        #[arg(long, value_parser = query::parse_time)]
        to: Option<u64>,
        /// .grain chunk pre-filter (timberfs sources only); repeatable
        #[arg(long)]
        has: Vec<String>,
        /// Never prefix output lines with the file name
        #[arg(long)]
        no_filename: bool,
        /// Custom entry-boundary timestamp: regex with one capture group
        #[arg(long, requires = "timestamp_format")]
        timestamp_regex: Option<String>,
        /// chrono format string for the captured timestamp
        #[arg(long, requires = "timestamp_regex")]
        timestamp_format: Option<String>,
        /// Write matching entries into a NEW timberfs log or .timber
        /// bundle instead of printing — the investigation as an artifact:
        /// its manifest records the command line, pattern, window and
        /// lineage. Takes exactly one timberfs source
        #[arg(long = "into", value_name = "DEST", conflicts_with_all = ["count", "no_filename"])]
        into: Option<PathBuf>,
        /// With --into: error instead of writing an empty artifact when
        /// nothing matches
        #[arg(long, requires = "into")]
        fail_on_empty: bool,
        /// Full scan: never pre-filter chunks via the .grain (word-mode
        /// patterns otherwise use it automatically when one exists)
        #[arg(long)]
        scan: bool,
        /// PATTERN is a regular expression. Reads every chunk — don't
        /// reach for this unless you need it; the word-mode default is
        /// index-accelerated
        #[arg(long, conflicts_with = "fixed")]
        regex: bool,
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
    /// Build or rebuild the .grain token index for a log: one Bloom filter
    /// per chunk over every token in it (~1% false positives), letting
    /// `query --has` skip chunks — e.g. find a request id with no known
    /// time range. Derived data: safe to delete, cheap to rebuild; rotation
    /// and retention drop it (rebuild afterwards).
    Reindex {
        /// Backing file: logical name, .trunk or .rings path
        file: PathBuf,
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
            fs::mount(s, &mountpoint, allow_other)?;
        }
        Command::Create {
            dest,
            index,
            retain,
            retain_size,
            sets,
        } => {
            bark::cmd_create(
                &dest,
                index,
                retain.as_deref(),
                retain_size.as_deref(),
                &sets,
            )?;
        }
        Command::Set {
            store,
            sets,
            unsets,
        } => {
            bark::cmd_set(&store, &sets, &unsets)?;
        }
        Command::Append {
            file,
            chunk_size,
            level,
            flush_age,
            retain,
            retain_size,
        } => {
            let cfg = store::Config {
                chunk_size: chunk_size.max(1),
                level,
                flush_age_ms: (flush_age * 1000.0).max(0.0) as u64,
            };
            append::cmd_append(&file, cfg, retain.as_deref(), retain_size.as_deref())?;
        }
        Command::Import {
            sources,
            dest,
            chunk_size,
            level,
            timestamp_regex,
            timestamp_format,
            utc,
            quick,
            index,
        } => {
            let cfg = store::Config {
                chunk_size: chunk_size.max(1),
                level,
                flush_age_ms: u64::MAX, // no age flushing during import
            };
            import::cmd_import(
                &sources,
                &dest,
                cfg,
                timestamp_regex.as_deref(),
                timestamp_format.as_deref(),
                utc,
                quick,
                index,
            )?;
        }
        Command::Export {
            source,
            dest,
            from,
            to,
            fail_on_empty,
        } => {
            export::cmd_export(&source, &dest, from, to, fail_on_empty)?;
        }
        Command::Query {
            files,
            from,
            to,
            has,
            no_filename,
        } => {
            query::cmd_query(&files, from, to, &has, no_filename)?;
        }
        Command::Grep {
            pattern,
            files,
            ignore_case,
            invert,
            fixed,
            count,
            from,
            to,
            has,
            no_filename,
            timestamp_regex,
            timestamp_format,
            into,
            fail_on_empty,
            scan,
            regex,
        } => {
            grep::cmd_grep(
                &pattern,
                &files,
                from,
                to,
                &has,
                ignore_case,
                invert,
                fixed,
                count,
                no_filename,
                timestamp_regex.as_deref(),
                timestamp_format.as_deref(),
                into.as_deref(),
                fail_on_empty,
                scan,
                regex,
            )?;
        }
        Command::Info { file, json } => {
            query::cmd_info(&file, json)?;
        }
        Command::Index { file } => {
            query::cmd_index(&file)?;
        }
        Command::Reindex { file } => {
            grain::cmd_reindex(&file)?;
        }
        Command::Rotate {
            source,
            dest,
            cutoff,
            delete,
            dry_run,
            fail_on_empty,
        } => {
            rotate::cmd_rotate(
                &source,
                dest.as_deref(),
                cutoff,
                delete,
                dry_run,
                fail_on_empty,
            )?;
        }
    }
    Ok(())
}
