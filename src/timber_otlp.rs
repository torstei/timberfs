//! timber-otlp — an OTLP/HTTP consumer.
//!
//! ONE MODE: a `timberfs-records(5)` stream arrives on stdin, one
//! `LogRecord` per entry goes to an OTLP/HTTP receiver, and the consumer
//! protocol goes back on stdout. Nothing else — it reads no store, holds
//! no position, and has no window.
//!
//! It is fed by `timberfs feed` or by a registered follower, and what it
//! reports is how far each store's position may move. The position
//! itself is timberfs's, because that file is where the retention floor
//! lives: a program able to write it could get retention wrong silently,
//! where one that reports cannot. See docs/plans/consumer-protocol.md.
//!
//! ⚠ A watermark means "do not send me these again", not "these were
//! delivered". So a receiver that is down gets the same entries again,
//! while an entry it refuses PERMANENTLY — too old for its ingestion
//! window, malformed — is reported past and never re-sent. Advancing
//! only on confirmed delivery would wedge a follower on one bad batch
//! for ever.
//!
//! ⚠ OTLP's own `partialSuccess` carries a COUNT of rejected records and
//! no identities, so there is no subset to retry: it is an accounting
//! line and a note, never a position decision.
//!
//! One request carries one `ResourceLogs` per store, built from the
//! labels the stream's `source` records declare — so a selection of
//! fifty stores keeps fifty identities rather than being flattened into
//! whichever one the shipper opened with.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::Parser;
use serde_json::{Map, Value};

use timberfs::append::parse_duration_ms;
use timberfs::consumer::{self, Diet};
use timberfs::note;
use timberfs::otlp::{self, Client, Entry, Group, Outcome, Severity};
use timberfs::records::{EntryRec, Reader, Rec};

const HEAD_DEST: &str = "Destination";
const HEAD_WHAT: &str = "What is sent";

/// Backoff for a retryable endpoint, doubling to this ceiling. A
/// consumer never gives up: the store holds the backlog, and giving up
/// would turn a reachable-again endpoint into lost data.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Ship a timberfs records stream to an OTLP/HTTP receiver, one
/// LogRecord per entry (a stamped line plus its continuation lines), and
/// report how far each store's position may move.
///
/// Reads the stream on stdin: it is a CONSUMER, fed by `timberfs feed`
/// or by a registered follower, and holds no position of its own
#[derive(Parser)]
#[command(name = "timber-otlp", version)]
struct Cli {
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

    /// Resource service.name; default: each store's own `service` label,
    /// else its name. Given here it overrides every store's
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
    /// Print the export requests instead of sending them, and report the
    /// entries as taken — which is honest: a watermark says "do not send
    /// me these again", and printing them twice is what it prevents. To
    /// preview without remembering anything, run the feeder without a
    /// positions file
    #[arg(long, help_heading = HEAD_WHAT)]
    dry_run: bool,

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

/// The resource of ONE store, from what the stream said it declares.
///
/// `labels` is what the `source` record carried — the store's `.bark`
/// provenance — so nothing here opens a store. A consumer needs no
/// filesystem access, which is what lets it run on another machine.
fn store_resource(
    name: &str,
    id: &str,
    path: &str,
    labels: &Map<String, Value>,
    cli: &Cli,
) -> anyhow::Result<Vec<(String, String)>> {
    let get = |k: &str| labels.get(k).and_then(Value::as_str).map(str::to_string);
    let mut attrs: Vec<(String, String)> = Vec::new();
    if let Some(h) = get("host") {
        attrs.push(("host.name".into(), h));
    }
    attrs.push(("timberfs.store.id".into(), id.to_string()));
    if !path.is_empty() {
        attrs.push(("timberfs.store.path".into(), path.to_string()));
    }
    let service = cli
        .service
        .clone()
        .or_else(|| get("service"))
        .or_else(|| Some(name.trim_end_matches(".log").to_string()));
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
    Ok(attrs)
}

/// What the stream has said about one store, and what of it is waiting to
/// go out.
struct Store {
    path: String,
    labels: Map<String, Value>,
    /// The resource, built once per label generation rather than per
    /// batch — `--resource` parsing and the hostname fallback do not
    /// change between batches.
    resource: Vec<(String, String)>,
    pending: Vec<EntryRec>,
    /// Just past the last pending entry: the watermark if the batch is
    /// accepted. A number the stream handed us, never one we compute.
    end: u64,
}

enum Msg {
    Rec(Rec),
    /// The stream ended cleanly — which for a FOLLOWED stream is EOF
    /// with no `stream-end`, that absence being the format's own "still
    /// live" marker rather than truncation.
    End,
    Fail(String),
}

/// Read the stream off-thread so a partial batch can also go out on
/// time: a batch must not wait for the next entry to exist.
fn spawn_reader<R: BufRead + Send + 'static>(r: R) -> mpsc::Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = Reader::new(r);
        // ⚠ A followed stream carries no `stream-end`, so its EOF is not
        // truncation — which is exactly what `Reader` reports it as.
        // `stream-start` says which kind of stream this is, so the
        // distinction is read from the wire rather than assumed.
        let mut following = false;
        loop {
            match reader.next_rec() {
                Ok(Some(rec)) => {
                    if let Rec::Start(fields) = &rec {
                        following = fields.iter().any(|(k, v)| k == "follow" && v == "1");
                    }
                    if tx.send(Msg::Rec(rec)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(Msg::End);
                    return;
                }
                Err(e) if following && e.to_string().contains("truncated") => {
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
    severity: Severity,
    dry_run: bool,
    shipped: u64,
    batches: u64,
    rejected: u64,
    out: io::Stdout,
}

impl Shipper {
    /// Report a store's watermark. The only thing that moves a position,
    /// and it moves it to a number the stream stated.
    fn progress(&mut self, id: &str, offset: u64) -> anyhow::Result<()> {
        self.out.write_all(&consumer::progress(id, offset))?;
        self.out.flush()?;
        Ok(())
    }

    /// Say why nothing is moving, for `follower status` to show. Opaque
    /// to timberfs and rendered verbatim; `id` absent means "about me,
    /// not a store".
    fn note(&mut self, id: Option<&str>, offset: Option<u64>, text: &str) -> anyhow::Result<()> {
        self.out.write_all(&consumer::note(id, offset, text))?;
        self.out.flush()?;
        Ok(())
    }

    /// One request for everything pending, then a watermark per store.
    ///
    /// In that order, and never the reverse: a watermark says "do not
    /// send me these again", so reporting before the receiver has them
    /// would lose a batch to a crash.
    fn flush(&mut self, stores: &mut BTreeMap<String, Store>) -> anyhow::Result<()> {
        let count: usize = stores.values().map(|s| s.pending.len()).sum();
        if count == 0 {
            return Ok(());
        }
        let ids: Vec<String> = stores
            .iter()
            .filter(|(_, s)| !s.pending.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        let entries: Vec<Vec<Entry>> = ids
            .iter()
            .map(|id| {
                stores[id]
                    .pending
                    .iter()
                    .map(|e| Entry {
                        ts_ms: e.ts,
                        wf_ms: e.wf.unwrap_or(0),
                        payload: &e.payload,
                    })
                    .collect()
            })
            .collect();
        let groups: Vec<Group> = ids
            .iter()
            .zip(&entries)
            .map(|(id, e)| Group {
                resource: &stores[id].resource,
                entries: e,
            })
            .collect();

        let outcome = self.deliver(&groups, count)?;
        drop(groups);
        drop(entries);

        for id in ids {
            let store = stores.get_mut(&id).expect("just seen");
            match &outcome {
                // Taken, or permanently refused: either way, do not send
                // them again. A permanent refusal that held the position
                // would wedge this follower on one bad batch for ever.
                Taken::Yes | Taken::Refused => {
                    let end = store.end;
                    store.pending.clear();
                    self.progress(&id, end)?;
                }
                // Kept: the same entries go out again next time.
                Taken::No => {}
            }
        }
        self.shipped += count as u64;
        self.batches += 1;
        Ok(())
    }

    /// Post, retrying a retryable endpoint for as long as it takes.
    fn deliver(&mut self, groups: &[Group], count: usize) -> anyhow::Result<Taken> {
        if self.dry_run {
            // The json spelling whatever the wire encoding is: the two
            // carry the same request, and one of them is readable.
            let body = otlp::render_groups(groups, env!("CARGO_PKG_VERSION"), &self.severity);
            println!("{}", serde_json::to_string_pretty(&body)?);
            return Ok(Taken::Yes);
        }
        let text: Vec<u8> = match self.encoding {
            otlp::Encoding::Proto => {
                otlp::render_groups_proto(groups, env!("CARGO_PKG_VERSION"), &self.severity)
            }
            otlp::Encoding::Json => {
                otlp::render_groups(groups, env!("CARGO_PKG_VERSION"), &self.severity)
                    .to_string()
                    .into_bytes()
            }
        };
        let mut backoff = Duration::from_secs(1);
        let mut attempt = 0u32;
        loop {
            let outcome = {
                let client = self.client.as_mut().expect("a client unless --dry-run");
                client.post(&text)
            };
            match outcome {
                Outcome::Delivered { rejected, message } => {
                    if rejected > 0 {
                        self.rejected += rejected;
                        // ⚠ A count and no identities, so there is no
                        // subset to retry: it is said and the batch
                        // moves on.
                        let why = format!(
                            "{rejected} of {count} record(s) refused by the receiver{}",
                            message.map(|m| format!(": {m}")).unwrap_or_default()
                        );
                        eprintln!("timber-otlp: {why}");
                        self.note(None, None, &why)?;
                    }
                    return Ok(Taken::Yes);
                }
                Outcome::Rejected(why) => {
                    self.rejected += count as u64;
                    let text = format!(
                        "{count} record(s) permanently refused ({why}); reported past them, \
                         because holding the position here would stop this follower for good"
                    );
                    eprintln!("timber-otlp: {text}");
                    self.note(None, None, &text)?;
                    return Ok(Taken::Refused);
                }
                Outcome::Retry { after, why } => {
                    attempt += 1;
                    let wait = after.unwrap_or(backoff);
                    let text = format!(
                        "{} unavailable ({why}), attempt {attempt}; retrying in {}s (the store \
                         keeps the backlog)",
                        self.client
                            .as_ref()
                            .map(|c| c.endpoint().to_string())
                            .unwrap_or_default(),
                        wait.as_secs().max(1)
                    );
                    eprintln!("timber-otlp: {text}");
                    // Said once per streak rather than per attempt: a
                    // note is deduped by text at the far end, so a
                    // repeated one costs one write.
                    self.note(None, None, &text)?;
                    thread::sleep(wait);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }
}

/// What became of a batch, as the position cares about it.
enum Taken {
    /// Delivered, or printed under --dry-run.
    Yes,
    /// Permanently refused: do not send them again either.
    Refused,
    /// Not taken; the same entries go out next time. (Reserved for a
    /// bounded-retry mode; the retry loop above does not return it.)
    #[allow(dead_code)]
    No,
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    timberfs::note::set_quiet(cli.quiet);
    if cli.batch_size == 0 {
        bail!("--batch-size must be at least 1");
    }
    let batch_timeout = Duration::from_millis(parse_duration_ms(&cli.batch_timeout)?.max(1));
    let timeout = Duration::from_millis(parse_duration_ms(&cli.timeout)?.max(1));
    let severity = Severity::new(cli.severity_regex.as_deref())?;
    let encoding = if cli.encoding == "json" {
        otlp::Encoding::Json
    } else {
        otlp::Encoding::Proto
    };
    let client = if cli.dry_run {
        None
    } else {
        let ep = otlp::parse_endpoint(&endpoint_url(&cli))?;
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

    let mut shipper = Shipper {
        client,
        encoding,
        severity,
        dry_run: cli.dry_run,
        shipped: 0,
        batches: 0,
        rejected: 0,
        out: io::stdout(),
    };
    // Before anything is read: the hello is what proves to the feeder
    // that this end implements the protocol, and it will send nothing
    // until it arrives. No `holds` — an OTLP receiver has no way to say
    // what it already has.
    shipper
        .out
        .write_all(&consumer::hello(Diet::Records, &[]))?;
    shipper.out.flush()?;

    let rx = spawn_reader(BufReader::new(io::stdin()));
    let mut stores: BTreeMap<String, Store> = BTreeMap::new();
    let mut sole: Option<String> = None;
    let mut pending = 0usize;
    let mut failure = None;
    loop {
        match rx.recv_timeout(batch_timeout) {
            Ok(Msg::Rec(Rec::Source(fields))) => {
                let get = |k: &str| fields.iter().find(|(f, _)| f == k).map(|(_, v)| v.clone());
                let Some(id) = get("id") else {
                    // A stream that did not come from a store: nothing to
                    // attribute or report, so nothing to remember.
                    continue;
                };
                let labels: Map<String, Value> = get("labels")
                    .and_then(|j| serde_json::from_str::<Value>(&j).ok())
                    .and_then(|v| match v {
                        Value::Object(m) => Some(m),
                        _ => None,
                    })
                    .unwrap_or_default();
                let path = get("path").unwrap_or_default();
                let name = path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&path)
                    .trim_end_matches(".log")
                    .to_string();
                let resource = store_resource(&name, &id, &path, &labels, &cli)?;
                match stores.get_mut(&id) {
                    // ⚠ A `source` record is a FLUSH BOUNDARY: entries
                    // that arrived under the old labels must go out
                    // attributed to them, or a label change silently
                    // relabels what it did not apply to.
                    Some(s) if s.labels != labels => {
                        if !s.pending.is_empty() {
                            shipper.flush(&mut stores)?;
                            pending = 0;
                        }
                        let s = stores.get_mut(&id).expect("just seen");
                        s.labels = labels;
                        s.resource = resource;
                        s.path = path;
                    }
                    Some(_) => {}
                    None => {
                        sole = match sole {
                            None if stores.is_empty() => Some(id.clone()),
                            _ => None,
                        };
                        stores.insert(
                            id,
                            Store {
                                path,
                                labels,
                                resource,
                                pending: Vec::new(),
                                end: 0,
                            },
                        );
                    }
                }
            }
            Ok(Msg::Rec(Rec::Entry(e))) => {
                // A followed stream attributes every entry. A hand-run
                // pipe from one store does not — the source record named
                // it once — so the sole source is the attribution there.
                let Some(id) = e.id.clone().or_else(|| sole.clone()) else {
                    bail!(
                        "an entry names no store and the stream named more than one source, so \
                         there is nothing to report a position against"
                    );
                };
                let Some(store) = stores.get_mut(&id) else {
                    bail!("an entry names store {id}, which no source record introduced");
                };
                // The watermark if this batch is taken: a number the
                // stream stated, since entry runs chain.
                if let (Some(off), len) = (e.offset, e.payload.len() as u64) {
                    store.end = off + len;
                }
                store.pending.push(e);
                pending += 1;
                if pending >= cli.batch_size {
                    shipper.flush(&mut stores)?;
                    pending = 0;
                }
            }
            Ok(Msg::Rec(_)) => {}
            Ok(Msg::End) => {
                shipper.flush(&mut stores)?;
                break;
            }
            Ok(Msg::Fail(e)) => {
                shipper.flush(&mut stores)?;
                failure = Some(e);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                shipper.flush(&mut stores)?;
                pending = 0;
            }
            Err(RecvTimeoutError::Disconnected) => {
                shipper.flush(&mut stores)?;
                break;
            }
        }
    }

    note!(
        "timber-otlp: {} entr{} in {} request(s){}",
        shipper.shipped,
        if shipper.shipped == 1 { "y" } else { "ies" },
        shipper.batches,
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

fn main() {
    if let Err(e) = run() {
        eprintln!("timber-otlp: {e:#}");
        let _ = io::stderr().flush();
        std::process::exit(1);
    }
}
