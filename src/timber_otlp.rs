//! timber-otlp — ship a timberfs entry stream to an OTLP/HTTP receiver
//! (an OpenTelemetry Collector, or any backend that speaks OTLP).
//!
//! A reader, not a writer: it consumes the record stream
//! (timberfs-records(5)) that `timberfs query --records` produces, so the
//! append path is untouched and an unreachable endpoint can only stall
//! the shipper. The store IS the send buffer — where a collector's queue
//! is sized by guessing, retention is the disconnection budget: `retain
//! 30d` means the receiver can be gone for thirty days.
//!
//! Two modes, because they need different time axes:
//!   - `--follow` (with `--cursor`): the durable shipper. Position is on
//!     the write axis and survives restarts (see cursor.rs).
//!   - a `--from`/`--to` window: replay. That axis is the LOGLINE one —
//!     "re-ship what happened during the incident" — so it takes no
//!     cursor: a replay is a deliberate act, not a resumable one.
//!
//! Rendering, the endpoint and the retry contract live in `otlp.rs`;
//! `--dry-run` prints exactly what would be posted.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::Parser;

use timberfs::append::parse_duration_ms;
use timberfs::cursor::{Cursor, Resume};
use timberfs::note;
use timberfs::otlp::{self, Client, Entry, Outcome, Severity};
use timberfs::records::{EntryRec, Reader, Rec};

const HEAD_DEST: &str = "Destination";
const HEAD_WHAT: &str = "What is sent";
const HEAD_POS: &str = "Position";

const CONSUMER: &str = "timber-otlp";
/// Backoff for a retryable endpoint, doubling to this ceiling. A shipper
/// never gives up: the store holds the backlog, and giving up would turn
/// a reachable-again endpoint into lost data.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Ship a timberfs store's entries to an OTLP/HTTP receiver, one
/// LogRecord per entry (a stamped line plus its continuation lines).
/// Reads a store through the query layer, or a record stream on stdin
#[derive(Parser)]
#[command(name = "timber-otlp", version)]
struct Cli {
    /// The store to ship; default: stdin, a record stream (produce it
    /// with `timberfs query --records` or `timber-filter --records`)
    store: Option<PathBuf>,

    /// OTLP/HTTP receiver: a base URL (/v1/logs is appended) or the
    /// signal URL itself. Default: $OTEL_EXPORTER_OTLP_LOGS_ENDPOINT,
    /// $OTEL_EXPORTER_OTLP_ENDPOINT, else http://localhost:4318
    #[arg(long, value_name = "URL", help_heading = HEAD_DEST)]
    endpoint: Option<String>,
    /// Extra request header, e.g. --header 'Authorization=Bearer x'
    /// (repeatable; $OTEL_EXPORTER_OTLP_HEADERS is also read)
    #[arg(long, value_name = "K=V", help_heading = HEAD_DEST)]
    header: Vec<String>,
    /// Connect/read/write timeout per request
    #[arg(long, value_name = "DUR", default_value = "10s", help_heading = HEAD_DEST)]
    timeout: String,

    /// Resource service.name; default: the store's .bark "service", else
    /// its name
    #[arg(long, value_name = "NAME", help_heading = HEAD_WHAT)]
    service: Option<String>,
    /// Extra resource attribute (repeatable); overrides derived ones
    #[arg(long, value_name = "K=V", help_heading = HEAD_WHAT)]
    resource: Vec<String>,
    /// Where an entry's level is, if not an uppercase level word: the
    /// first capture group (else the whole match) becomes severityText
    #[arg(long, value_name = "PATTERN", help_heading = HEAD_WHAT)]
    severity_regex: Option<String>,
    /// Maximum LogRecords per export request
    #[arg(long, value_name = "N", default_value = "512", help_heading = HEAD_WHAT)]
    batch_size: usize,
    /// Send a partial batch after this long with nothing new
    #[arg(long, value_name = "DUR", default_value = "1s", help_heading = HEAD_WHAT)]
    batch_timeout: String,
    /// Wire encoding: proto is what every OTLP sender defaults to and
    /// what a receiver is likeliest to accept; json is readable on the
    /// wire. --dry-run always prints the json spelling
    #[arg(long, value_name = "ENC", default_value = "proto", value_parser = ["proto", "json"], help_heading = HEAD_WHAT)]
    encoding: String,
    /// Compress request bodies with gzip (worth it over a network, noise
    /// over loopback)
    #[arg(long, value_name = "MODE", default_value = "none", value_parser = ["none", "gzip"], help_heading = HEAD_WHAT)]
    compress: String,
    /// Render and print the export requests instead of sending them; a
    /// cursor is read but never advanced
    #[arg(long, help_heading = HEAD_WHAT)]
    dry_run: bool,

    /// Keep shipping as entries are committed (like tail -f)
    #[arg(short = 'f', long, help_heading = HEAD_POS)]
    follow: bool,
    /// Persist the shipping position here, so a restart resumes rather
    /// than re-sends (requires --follow)
    #[arg(long, value_name = "PATH", help_heading = HEAD_POS)]
    cursor: Option<PathBuf>,
    /// Where to start when the cursor file does not exist yet: the end
    /// (only new entries) or the beginning (ship the whole store)
    #[arg(long, value_name = "WHERE", default_value = "end", value_parser = ["end", "begin"], help_heading = HEAD_POS)]
    start: String,
    /// Replay: start of the logline window (formats as timberfs query)
    #[arg(long, value_name = "TIME", conflicts_with = "cursor", help_heading = HEAD_POS)]
    from: Option<String>,
    /// Replay: end of the logline window
    #[arg(long, value_name = "TIME", conflicts_with_all = ["cursor", "follow"], help_heading = HEAD_POS)]
    to: Option<String>,

    /// Suppress progress notes on stderr; errors still print
    #[arg(long)]
    quiet: bool,
}

fn parse_kv(items: &[String], what: &str) -> anyhow::Result<Vec<(String, String)>> {
    items
        .iter()
        .map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .with_context(|| format!("{what} {s:?} is not K=V"))
        })
        .collect()
}

/// $OTEL_EXPORTER_OTLP_HEADERS is a comma-separated k=v list, the same
/// spelling every OTel SDK reads.
fn env_headers() -> Vec<(String, String)> {
    std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

fn endpoint_url(cli: &Cli) -> String {
    if let Some(e) = &cli.endpoint {
        return e.clone();
    }
    for var in [
        "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
    ] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return v;
            }
        }
    }
    "http://localhost:4318".to_string()
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// What the store says it is: the OTLP resource, plus what a cursor
/// anchors to. The anchor is the `.bark` id when the store declares one
/// — identity that survives a move — and its canonical path otherwise,
/// since a store written by a plain `append` has no manifest at all.
struct StoreIdentity {
    attrs: Vec<(String, String)>,
    anchor: Option<String>,
}

/// The resource is what the STORE is, so it comes from the store: its
/// `.bark` is already the manifest of declared properties and identity.
/// `--service`/`--resource` override, and a stdin stream (no store to
/// ask) falls back to OTel's own `unknown_service`.
fn resource_attrs(store: Option<&Path>, cli: &Cli) -> anyhow::Result<StoreIdentity> {
    let mut attrs: Vec<(String, String)> = Vec::new();
    let mut anchor = None;
    let mut service = cli.service.clone();
    if let Some(path) = store {
        let (dir, name) = timberfs::query::resolve_backing(path)?;
        let bark = timberfs::bark::load(&dir, &name);
        let get = |k: &str| -> Option<String> {
            bark.as_ref()
                .and_then(|m| m.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let store_id = get("id");
        anchor = Some(timberfs::cursor::store_anchor(&dir, &name, bark.as_ref()));
        service = service
            .or_else(|| get("service"))
            .or_else(|| Some(name.trim_end_matches(".log").to_string()));
        if let Some(h) = get("host") {
            attrs.push(("host.name".into(), h));
        }
        if let Some(id) = &store_id {
            attrs.push(("timberfs.store.id".into(), id.clone()));
        }
        attrs.push(("timberfs.store.path".into(), path.display().to_string()));
    }
    attrs.insert(
        0,
        (
            "service.name".into(),
            service.unwrap_or_else(|| "unknown_service".into()),
        ),
    );
    if !attrs.iter().any(|(k, _)| k == "host.name") {
        if let Some(h) = hostname() {
            attrs.push(("host.name".into(), h));
        }
    }
    for (k, v) in parse_kv(&cli.resource, "--resource")? {
        match attrs.iter_mut().find(|(ak, _)| *ak == k) {
            Some(slot) => slot.1 = v,
            None => attrs.push((k, v)),
        }
    }
    Ok(StoreIdentity { attrs, anchor })
}

enum Msg {
    Entry(EntryRec),
    End,
    Fail(String),
}

/// Read the record stream off-thread so the batch loop can also wake on
/// time: a partial batch must not wait for the next entry to exist.
fn spawn_reader<R: BufRead + Send + 'static>(r: R) -> mpsc::Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = Reader::new(r);
        loop {
            match reader.next_rec() {
                Ok(Some(Rec::Entry(e))) => {
                    if tx.send(Msg::Entry(e)).is_err() {
                        return;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    let _ = tx.send(Msg::End);
                    return;
                }
                Err(e) => {
                    let _ = tx.send(Msg::Fail(format!("{e:#}")));
                    return;
                }
            }
        }
    });
    rx
}

struct Shipper {
    client: Option<Client>,
    encoding: otlp::Encoding,
    resource: Vec<(String, String)>,
    severity: Severity,
    cursor: Option<(Cursor, PathBuf)>,
    dry_run: bool,
    shipped: u64,
    batches: u64,
    rejected: u64,
}

impl Shipper {
    /// One batch: render, deliver, then advance the cursor — in that
    /// order, so a crash re-delivers instead of skipping. A retryable
    /// endpoint is retried forever with capped backoff; a permanent
    /// refusal drops the batch loudly and moves on, because the
    /// alternative is a stream that never advances again.
    fn flush(&mut self, batch: &mut Vec<EntryRec>) -> anyhow::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let entries: Vec<Entry> = batch
            .iter()
            .map(|e| Entry {
                ts_ms: e.ts,
                wf_ms: e.wf.unwrap_or(0),
                payload: &e.payload,
            })
            .collect();
        if self.dry_run {
            // The json spelling whatever the wire encoding is: the two
            // carry the same request, and one of them is readable.
            let body = otlp::render_with(
                &self.resource,
                env!("CARGO_PKG_VERSION"),
                &entries,
                &self.severity,
            );
            println!("{}", serde_json::to_string_pretty(&body)?);
        } else {
            let text: Vec<u8> = match self.encoding {
                otlp::Encoding::Proto => otlp::render_proto(
                    &self.resource,
                    env!("CARGO_PKG_VERSION"),
                    &entries,
                    &self.severity,
                ),
                otlp::Encoding::Json => otlp::render_with(
                    &self.resource,
                    env!("CARGO_PKG_VERSION"),
                    &entries,
                    &self.severity,
                )
                .to_string()
                .into_bytes(),
            };
            let client = self.client.as_mut().expect("a client unless --dry-run");
            let mut backoff = Duration::from_secs(1);
            let mut attempt = 0u32;
            loop {
                match client.post(&text) {
                    Outcome::Delivered { rejected, message } => {
                        if rejected > 0 {
                            self.rejected += rejected;
                            eprintln!(
                                "timber-otlp: {} of {} record(s) refused by the receiver{}",
                                rejected,
                                entries.len(),
                                message.map(|m| format!(": {m}")).unwrap_or_default()
                            );
                        }
                        break;
                    }
                    Outcome::Rejected(why) => {
                        self.rejected += entries.len() as u64;
                        eprintln!(
                            "timber-otlp: {} record(s) permanently refused ({why}); \
                             dropping the batch and continuing",
                            entries.len()
                        );
                        break;
                    }
                    Outcome::Retry { after, why } => {
                        attempt += 1;
                        let wait = after.unwrap_or(backoff);
                        eprintln!(
                            "timber-otlp: {} unavailable ({why}), attempt {attempt}; \
                             retrying in {}s (the store keeps the backlog)",
                            client.endpoint(),
                            wait.as_secs().max(1)
                        );
                        thread::sleep(wait);
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                    }
                }
            }
        }
        self.shipped += batch.len() as u64;
        self.batches += 1;
        if let Some((cursor, path)) = &mut self.cursor {
            for e in batch.iter() {
                cursor.advance(e.wf.unwrap_or(0), e.wl.unwrap_or(0));
            }
            if !self.dry_run {
                cursor.save(path)?;
            }
        }
        batch.clear();
        Ok(())
    }
}

/// What a resume is worth saying out loud: where it starts, and how much
/// of the store is still ahead of it.
///
/// A GAP is the one case that is a warning rather than a note. Retention
/// acts on the head and nothing coordinates it with a consumer's
/// progress, so a shipper down longer than the store's `retain` window
/// comes back to find the chunk its cursor points at already dropped —
/// and `query --from` then simply starts at whatever is now oldest.
/// Without this check that loss is invisible: the shipper reports a
/// clean resume and the skipped entries are never mentioned again. It
/// stays a warning and not a refusal because the loss is already in the
/// past, and a shipper that will not start ships nothing.
fn report_resume(c: &Cursor, store: Option<&Path>) {
    let standing = store
        .and_then(|p| timberfs::query::resolve_backing(p).ok())
        .and_then(|(dir, name)| {
            timberfs::format::read_index(&timberfs::format::rings_path(&dir, &name)).ok()
        })
        .map(|records| timberfs::cursor::standing(c, &records));
    if let Some(oldest) = standing.and_then(|s| s.gap_to_ms) {
        eprintln!(
            "timber-otlp: warning: GAP — the cursor is at {} but the oldest data in the \
             store is {}; retention dropped {} of entries this consumer never read, and \
             they are not recoverable. Resuming at the oldest chunk. Either widen the \
             store's retention or find out why this shipper was behind.",
            timberfs::query::fmt_ms(c.wl),
            timberfs::query::fmt_ms(oldest),
            timberfs::query::human_duration(oldest.saturating_sub(c.wl))
        );
    }
    let backlog = match standing.filter(|s| s.gap_to_ms.is_none() && !s.caught_up()) {
        Some(s) => format!(
            ", {} unread in {} chunk(s)",
            timberfs::rotate::human_bytes(s.behind_bytes),
            s.behind_chunks
        ),
        None => String::new(),
    };
    note!(
        "timber-otlp: resuming at {} (+{} entries), {} delivered so far{backlog}",
        timberfs::query::fmt_ms(c.wl),
        c.n,
        c.delivered
    );
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    timberfs::note::set_quiet(cli.quiet);

    if cli.cursor.is_some() && !cli.follow {
        bail!(
            "--cursor needs --follow: a resumable position is on the write axis, and \
             only the follow path selects by it (a windowed query filters by the \
             timestamps the lines carry, which can move backwards)"
        );
    }
    if cli.batch_size == 0 {
        bail!("--batch-size must be at least 1");
    }
    let store = cli.store.clone();
    if store.is_none() && cli.cursor.is_some() {
        bail!("--cursor is a position in a store; name one instead of piping stdin");
    }
    let batch_timeout = Duration::from_millis(parse_duration_ms(&cli.batch_timeout)?.max(1));
    let timeout = Duration::from_millis(parse_duration_ms(&cli.timeout)?.max(1));
    let severity = Severity::new(cli.severity_regex.as_deref())?;
    let StoreIdentity {
        attrs: resource,
        anchor,
    } = resource_attrs(store.as_deref(), &cli)?;

    // The cursor is loaded before anything is read: its position decides
    // where the query starts, and a mismatched one must stop the run.
    let mut cursor = None;
    if let Some(path) = &cli.cursor {
        let id = anchor.clone().expect("a store was named");
        if id.starts_with("path:") {
            note!(
                "timber-otlp: {} declares no identity — anchoring the cursor to its path \
                 (declare one with `timberfs set` and it survives a move)",
                store.as_ref().expect("checked above").display()
            );
        }
        match Cursor::load(path)? {
            Some(c) => {
                c.check_store(&id, path)?;
                report_resume(&c, store.as_deref());
                cursor = Some(c);
            }
            None => {
                let path_str = store
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                note!(
                    "timber-otlp: no cursor yet — starting at the {} of the store",
                    cli.start
                );
                cursor = Some(Cursor::new(CONSUMER, &id, &path_str));
            }
        }
    }
    let mut resume = Resume::new(cursor.as_ref().filter(|c| c.delivered > 0));

    let encoding = if cli.encoding == "json" {
        otlp::Encoding::Json
    } else {
        otlp::Encoding::Proto
    };
    let client = if cli.dry_run {
        None
    } else {
        let url = endpoint_url(&cli);
        let ep = otlp::parse_endpoint(&url)?;
        let mut headers = env_headers();
        headers.extend(parse_kv(&cli.header, "--header")?);
        note!(
            "timber-otlp: shipping to {ep} ({}{})",
            if encoding == otlp::Encoding::Json {
                "json"
            } else {
                "protobuf"
            },
            if cli.compress == "gzip" {
                ", gzipped"
            } else {
                ""
            }
        );
        Some(Client::new(
            ep,
            headers,
            timeout,
            encoding,
            cli.compress == "gzip",
        ))
    };

    // Either spawn the selection layer over a store, or take a record
    // stream on stdin. Raw text is refused rather than guessed at: write
    // times and logline stamps are exactly what this needs, and only the
    // record stream carries them.
    let mut child: Option<Child> = None;
    let rx = match &store {
        Some(path) => {
            let mut cmd = Command::new(sibling_timberfs());
            cmd.args(["query", "--records"]);
            if cli.quiet {
                cmd.arg("--quiet");
            }
            if cli.follow {
                cmd.arg("--follow");
            }
            match (&cursor, &cli.from) {
                // A cursor that has delivered something positions the
                // read; a fresh one honours --start.
                (Some(c), _) if c.delivered > 0 => {
                    cmd.args(["--from", &timberfs::query::fmt_ms_rfc3339(c.from_ms())]);
                }
                (Some(_), _) if cli.start == "begin" => {
                    cmd.args(["--from", "0"]);
                }
                (_, Some(f)) => {
                    cmd.args(["--from", f]);
                }
                _ => {}
            }
            if let Some(t) = &cli.to {
                cmd.args(["--to", t]);
            }
            cmd.arg(path).stdout(Stdio::piped());
            let mut c = cmd
                .spawn()
                .context("spawning timberfs (is it installed next to timber-otlp?)")?;
            let out = BufReader::new(c.stdout.take().expect("piped stdout"));
            child = Some(c);
            spawn_reader(out)
        }
        None => {
            let mut reader = BufReader::new(io::stdin());
            let head = reader.fill_buf()?;
            if !head.starts_with(b"\x1estream-start") {
                bail!(
                    "stdin is not a record stream — produce one with \
                     `timberfs query --records` (write times and logline stamps come \
                     from its metadata, and raw text carries neither)"
                );
            }
            spawn_reader(reader)
        }
    };

    let mut shipper = Shipper {
        client,
        encoding,
        resource,
        severity,
        cursor: cursor.map(|c| (c, cli.cursor.clone().expect("cursor implies a path"))),
        dry_run: cli.dry_run,
        shipped: 0,
        batches: 0,
        rejected: 0,
    };
    let mut batch: Vec<EntryRec> = Vec::with_capacity(cli.batch_size);
    let mut failure = None;
    loop {
        match rx.recv_timeout(batch_timeout) {
            Ok(Msg::Entry(e)) => {
                if shipper.cursor.is_some() && (e.wf.is_none() || e.wl.is_none()) {
                    bail!(
                        "the record stream carries no write window; a cursor has nothing \
                         to be a position in"
                    );
                }
                if !resume.deliver(e.wf.unwrap_or(0), e.wl.unwrap_or(0)) {
                    continue;
                }
                batch.push(e);
                if batch.len() >= cli.batch_size {
                    shipper.flush(&mut batch)?;
                }
            }
            Ok(Msg::End) => {
                shipper.flush(&mut batch)?;
                break;
            }
            Ok(Msg::Fail(e)) => {
                shipper.flush(&mut batch)?;
                failure = Some(e);
                break;
            }
            Err(RecvTimeoutError::Timeout) => shipper.flush(&mut batch)?,
            Err(RecvTimeoutError::Disconnected) => {
                shipper.flush(&mut batch)?;
                break;
            }
        }
    }

    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
    note!(
        "timber-otlp: {} entr{} in {} request(s){}{}",
        shipper.shipped,
        if shipper.shipped == 1 { "y" } else { "ies" },
        shipper.batches,
        if resume.skipped() > 0 {
            format!(
                "; {} already-delivered entr{} skipped on resume",
                resume.skipped(),
                if resume.skipped() == 1 { "y" } else { "ies" }
            )
        } else {
            String::new()
        },
        if shipper.rejected > 0 {
            format!("; {} refused", shipper.rejected)
        } else {
            String::new()
        }
    );
    if let Some(e) = failure {
        bail!("{e}");
    }
    Ok(())
}

/// Prefer the timberfs next to this binary, so a build tree and an
/// installed package never mix versions.
fn sibling_timberfs() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("timberfs");
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from("timberfs")
}

fn main() {
    if let Err(e) = run() {
        eprintln!("timber-otlp: {e:#}");
        let _ = io::stderr().flush();
        std::process::exit(1);
    }
}
