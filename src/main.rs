mod append;
mod format;
mod fs;
mod import;
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
        /// Source log file(s), then the destination backing file LAST
        #[arg(required = true, num_args = 2..)]
        files: Vec<PathBuf>,
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
    },
    /// Print the bytes written between --from and --to, reading the backing
    /// files directly (works with or without an active mount)
    Query {
        /// Backing file: logical name, .trunk or .rings path
        file: PathBuf,
        /// Start of the time window (RFC3339, 'YYYY-MM-DD HH:MM:SS',
        /// 'HH:MM[:SS]' = today, or unix seconds); default: beginning
        #[arg(long)]
        from: Option<String>,
        /// End of the time window (same formats); default: end
        #[arg(long)]
        to: Option<String>,
    },
    /// Show the write-time chunk index of a backing file
    Index {
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
        /// 'YYYY-MM-DD HH:MM[:SS]', 'HH:MM[:SS]' = today, unix seconds)
        #[arg(long)]
        cutoff: String,
        /// Drop the rotated chunks instead of moving them (retention)
        #[arg(long, conflicts_with = "dest")]
        delete: bool,
        /// Preview what would move without changing anything
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
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
            files,
            chunk_size,
            level,
            timestamp_regex,
            timestamp_format,
            utc,
            quick,
        } => {
            let cfg = store::Config {
                chunk_size: chunk_size.max(1),
                level,
                flush_age_ms: u64::MAX, // no age flushing during import
            };
            let (dest, sources) = files.split_last().expect("clap enforces >= 2 args");
            import::cmd_import(
                sources,
                dest,
                cfg,
                timestamp_regex.as_deref(),
                timestamp_format.as_deref(),
                utc,
                quick,
            )?;
        }
        Command::Query { file, from, to } => {
            query::cmd_query(&file, from.as_deref(), to.as_deref())?;
        }
        Command::Index { file } => {
            query::cmd_index(&file)?;
        }
        Command::Rotate {
            source,
            dest,
            cutoff,
            delete,
            dry_run,
        } => {
            rotate::cmd_rotate(&source, dest.as_deref(), &cutoff, delete, dry_run)?;
        }
    }
    Ok(())
}
