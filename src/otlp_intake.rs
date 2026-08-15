//! `timberfs otlp-intake`: an OTLP/HTTP receiver for logs — the wire
//! protocol every OpenTelemetry SDK and the Collector already speak, so
//! one receiver makes all of them valid timberfs producers, and the
//! Collector then bridges syslog, journald, Kafka and Fluent Bit in for
//! free. The mirror of `timber-otlp`, which ships the same wire format
//! out; the mapping itself lives once, in `otlp.rs`.
//!
//! Like `forward-intake`, and for the same reason, this is a subcommand
//! rather than a filter: the HTTP response IS the acknowledgement, and
//! only the writer can promise durability. A 200 is returned once the
//! batch is fsynced into the store's `.sap` write-ahead sidecar (every
//! store this receiver touches declares `"wal": true`), so a sender's
//! retry logic and the store's chunk cadence stay independent.
//!
//! OTLP's structure lands on timberfs's: one ResourceLogs is one stream
//! (routed by `--route`, default `service.name`), its resource
//! attributes are what the store IS and are seeded into `.bark` at
//! creation, and each LogRecord is one entry. Stores are pre-created by
//! the operator by default; an undeclared stream gets 503 + `Retry-After`
//! so the sender buffers and provisioning converges with nothing lost —
//! the same contract as an unacked Forward chunk, spelled in HTTP.
//!
//! Deliberate limitations, each refused explicitly rather than fudged:
//!   - JSON bodies only (`application/json`) — a protobuf body is 415,
//!     naming the collector setting that fixes it;
//!   - no `Content-Encoding: gzip` — 415, likewise;
//!   - no TLS — loopback or a private network, like every other intake;
//!   - `/v1/logs` only: traces and metrics are not a log store's job;
//!   - no chunked request bodies — 411, since a receiver that must
//!     acknowledge durability needs to know what it is acknowledging.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Context;
use serde_json::{json, Value as Json};

use crate::intake::{self, Intake};
use crate::otlp::{parse_export_request, IncomingBatch};
use crate::store::{self, Config};

/// Per-store policy for the receiver, one field per CLI flag.
pub struct OtlpOpts {
    /// Resource attribute to route by; its value names the store.
    pub route: String,
    pub retain: Option<String>,
    pub retain_size: Option<String>,
    pub index: bool,
    /// Mint a store for a never-seen stream. OFF by default: creation is
    /// the operator's decision, not the network's.
    pub auto_create: bool,
    /// Largest request body accepted, in bytes.
    pub max_body: usize,
}

/// Resource attributes are per-stream constants, so they belong in the
/// manifest, not in every line. Bounded because they are network-supplied:
/// a sender cannot turn a `.bark` into a database.
const MAX_SEEDED_ATTRS: usize = 32;
const MAX_SEEDED_KEY: usize = 64;
const MAX_SEEDED_VALUE: usize = 256;

/// Seed a brand-new stream's `.bark`: identity + lineage, the resource
/// that describes it, the declared retention/index this receiver applies
/// — and `"wal"`, because the 200 means durable and the sap is what makes
/// that cheap. `service`/`host` are also written under the names the read
/// path and `timber-otlp` already look for, so a store received here
/// ships back out describing itself the same way.
fn seed_bark(
    dir: &Path,
    name: &str,
    resource: &[(String, String)],
    opts: &OtlpOpts,
) -> anyhow::Result<()> {
    let mut map = crate::bark::derived_map(None, "otlp-intake");
    for (k, v) in resource.iter().take(MAX_SEEDED_ATTRS) {
        if k.len() > MAX_SEEDED_KEY || v.len() > MAX_SEEDED_VALUE {
            continue;
        }
        map.insert(k.clone(), Json::String(v.clone()));
        match k.as_str() {
            "service.name" => {
                map.insert("service".to_string(), Json::String(v.clone()));
            }
            "host.name" => {
                map.insert("host".to_string(), Json::String(v.clone()));
            }
            _ => {}
        }
    }
    if let Some(r) = &opts.retain {
        map.insert("retain".to_string(), Json::String(r.clone()));
    }
    if let Some(r) = &opts.retain_size {
        map.insert("retain_size".to_string(), Json::String(r.clone()));
    }
    if opts.index {
        map.insert("index".to_string(), Json::Bool(true));
    }
    map.insert("wal".to_string(), Json::Bool(true));
    crate::bark::save(dir, name, &map)
}

// ---------------------------------------------------------------------
// The HTTP surface. Small on purpose: one method, one path, one encoding.
// ---------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// What to answer, and whether the connection survives it.
struct Reply {
    status: u16,
    reason: &'static str,
    body: String,
    /// Extra header lines, already formatted.
    extra: Vec<String>,
}

impl Reply {
    fn ok(body: String) -> Reply {
        Reply {
            status: 200,
            reason: "OK",
            body,
            extra: Vec::new(),
        }
    }

    /// An error the sender should read: OTLP has no error schema for
    /// HTTP, so the body is a plain message and the STATUS is the
    /// contract (retryable or not).
    fn err(status: u16, reason: &'static str, msg: impl Into<String>) -> Reply {
        Reply {
            status,
            reason,
            body: msg.into(),
            extra: Vec::new(),
        }
    }

    fn with(mut self, header_line: impl Into<String>) -> Reply {
        self.extra.push(header_line.into());
        self
    }

    fn content_type(&self) -> &'static str {
        if self.status < 300 {
            "application/json"
        } else {
            "text/plain; charset=utf-8"
        }
    }
}

fn write_reply(w: &mut TcpStream, reply: &Reply, keep_alive: bool) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
        reply.status,
        reply.reason,
        reply.content_type(),
        reply.body.len(),
        if keep_alive { "keep-alive" } else { "close" },
    );
    for line in &reply.extra {
        head.push_str(line);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(reply.body.as_bytes())?;
    w.flush()
}

/// Read one request. `Ok(None)` is a clean end of connection.
fn read_request(
    reader: &mut BufReader<TcpStream>,
    max_body: usize,
) -> anyhow::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    // Tolerate the blank line some clients send between requests.
    while line.trim().is_empty() {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        anyhow::bail!("malformed request line {:?}", line.trim_end());
    }
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            anyhow::bail!("connection closed mid-headers");
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = match header(&headers, "content-length").map(str::parse) {
        Some(Ok(n)) => n,
        _ => 0,
    };
    if len > max_body {
        // Drain nothing: the body is not read, so this connection cannot
        // continue — the caller answers 413 and closes.
        return Ok(Some(Request {
            method,
            path,
            headers,
            body: Vec::new(),
        }));
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader
            .read_exact(&mut body)
            .context("connection closed mid-body")?;
    }
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

/// Everything that can be answered without touching a store: the method,
/// the path, and the two encodings a default-configured collector would
/// send. Each refusal names the setting that fixes it.
fn precheck(req: &Request, max_body: usize) -> Option<Reply> {
    if req.method != "POST" {
        return Some(
            Reply::err(
                405,
                "Method Not Allowed",
                format!("{} is not allowed; OTLP/HTTP posts\n", req.method),
            )
            .with("Allow: POST"),
        );
    }
    let path = req
        .path
        .split('?')
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    if !path.ends_with("/v1/logs") {
        let hint = if path.ends_with("/v1/traces") || path.ends_with("/v1/metrics") {
            "this is a log store: only /v1/logs is served\n"
        } else {
            "not found; OTLP logs are posted to /v1/logs\n"
        };
        return Some(Reply::err(404, "Not Found", hint));
    }
    if let Some(enc) = header(&req.headers, "content-encoding") {
        if !enc.eq_ignore_ascii_case("identity") {
            return Some(Reply::err(
                415,
                "Unsupported Media Type",
                format!(
                    "Content-Encoding {enc} is not supported; send the body uncompressed \
                     (collector: compression: none)\n"
                ),
            ));
        }
    }
    let ctype = header(&req.headers, "content-type").unwrap_or("");
    if !ctype.to_ascii_lowercase().starts_with("application/json") {
        return Some(Reply::err(
            415,
            "Unsupported Media Type",
            format!(
                "Content-Type {ctype:?} is not supported; send OTLP/JSON \
                 (collector: encoding: json)\n"
            ),
        ));
    }
    if header(&req.headers, "transfer-encoding").is_some_and(|t| t.contains("chunked")) {
        return Some(Reply::err(
            411,
            "Length Required",
            "a chunked body cannot be acknowledged as durable; send Content-Length\n",
        ));
    }
    match header(&req.headers, "content-length").map(str::parse::<usize>) {
        Some(Ok(n)) if n > max_body => Some(Reply::err(
            413,
            "Payload Too Large",
            format!("body of {n} bytes exceeds --max-body\n"),
        )),
        Some(Ok(_)) => None,
        _ => Some(Reply::err(
            411,
            "Length Required",
            "a Content-Length is required\n",
        )),
    }
}

/// The route value for one ResourceLogs, or the OTel default when the
/// routing attribute is absent — never a guess at a better name.
fn route_of(batch: &IncomingBatch, route: &str) -> String {
    batch
        .resource
        .iter()
        .find(|(k, _)| k == route)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "unknown_service".to_string())
}

/// Store one request's batches, then make them durable, then answer.
///
/// Routing is resolved for EVERY batch before anything is appended: a
/// stream whose store cannot be opened makes the whole request retryable
/// (503), and a sender retrying a request we had half-written would
/// duplicate the half we kept.
fn handle_export(
    intake: &Mutex<Intake>,
    dir: &Path,
    cfg: &Config,
    opts: &OtlpOpts,
    extractor: &crate::import::Extractor,
    batches: &[IncomingBatch],
) -> Reply {
    let now = store::now_ms();
    let mut targets: Vec<String> = Vec::with_capacity(batches.len());
    {
        let mut g = intake.lock().unwrap();
        for batch in batches {
            let route = route_of(batch, &opts.route);
            let name = intake::store_name(&route);
            if let Err(e) = intake::ensure_store(
                &mut g,
                dir,
                &name,
                &format!("undeclared stream {route:?}"),
                opts.auto_create,
                |dir, name| seed_bark(dir, name, &batch.resource, opts),
            ) {
                if g.refused.insert(name.clone()) {
                    eprintln!("timberfs: otlp-intake: {name}: {e}");
                }
                return Reply::err(503, "Service Unavailable", format!("{e}\n"))
                    .with("Retry-After: 5");
            }
            targets.push(name);
        }
    }

    let mut stored = 0u64;
    let mut rejected = 0u64;
    {
        let mut g = intake.lock().unwrap();
        for (batch, name) in batches.iter().zip(&targets) {
            for rec in &batch.records {
                let head = rec.body.split('\n').next().unwrap_or_default();
                let line = rec.to_line(extractor.extract(head).is_some(), now);
                let t = rec.event_ms(now);
                match g.store.files.get_mut(name) {
                    Some(f) => match f.append_windowed(&line, t, t, cfg) {
                        Ok(()) => stored += 1,
                        Err(e) => {
                            rejected += 1;
                            eprintln!("timberfs: otlp-intake: {name}: append failed: {e}");
                        }
                    },
                    None => rejected += 1,
                }
            }
        }
    }

    // The acknowledgement: durable BEFORE the 200. With a live wal that is
    // one sap fsync per store — concurrent requests coalesce in the page
    // cache, and chunks keep their own size/age cadence regardless of how
    // often senders export.
    for name in &targets {
        let mut g = intake.lock().unwrap();
        let durable = match g.store.files.get_mut(name) {
            Some(f) if f.has_wal() => f.sap_sync().is_ok(),
            Some(f) => f.flush_chunk(cfg).and_then(|()| f.sync(cfg)).is_ok(),
            None => false,
        };
        if !durable {
            // Never answer 200 on hope: a retryable status leaves the
            // records with the sender, which is where they are still safe.
            return Reply::err(
                503,
                "Service Unavailable",
                format!("{name}: could not make the batch durable\n"),
            )
            .with("Retry-After: 5");
        }
    }

    if rejected > 0 {
        return Reply::ok(
            json!({"partialSuccess": {
                "rejectedLogRecords": rejected.to_string(),
                "errorMessage": format!("{rejected} record(s) could not be stored"),
            }})
            .to_string(),
        );
    }
    let _ = stored;
    Reply::ok("{}".to_string())
}

fn handle_connection(
    stream: TcpStream,
    intake: Arc<Mutex<Intake>>,
    dir: PathBuf,
    cfg: Config,
    opts: Arc<OtlpOpts>,
) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("timberfs: otlp-intake: {peer}: cannot clone connection: {e}");
            return;
        }
    };
    // One extractor per connection: deciding whether a body already opens
    // with a timestamp is the same question the read path asks, asked with
    // the same parser.
    let extractor = match crate::import::Extractor::new(None, None, false) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("timberfs: otlp-intake: {peer}: {e}");
            return;
        }
    };
    let mut reader = BufReader::new(stream);

    loop {
        let req = match read_request(&mut reader, opts.max_body) {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(e) => {
                eprintln!("timberfs: otlp-intake: {peer}: {e:#}");
                break;
            }
        };
        let mut keep_alive =
            !header(&req.headers, "connection").is_some_and(|c| c.eq_ignore_ascii_case("close"));
        let reply = match precheck(&req, opts.max_body) {
            // A refusal that left the body unread desynchronizes the
            // stream, so those close rather than pretend to continue.
            Some(r) => {
                if r.status == 413 {
                    keep_alive = false;
                }
                r
            }
            None => match serde_json::from_slice::<Json>(&req.body) {
                Err(e) => Reply::err(400, "Bad Request", format!("body is not JSON: {e}\n")),
                Ok(v) => match parse_export_request(&v) {
                    Err(e) => Reply::err(400, "Bad Request", format!("{e}\n")),
                    Ok(batches) => handle_export(&intake, &dir, &cfg, &opts, &extractor, &batches),
                },
            },
        };
        if write_reply(&mut writer, &reply, keep_alive).is_err() || !keep_alive {
            break;
        }
    }
}

/// `timberfs otlp-intake`: receive OTLP/HTTP logs and write each stream
/// into its own store under `into_dir`. See the module doc comment for
/// the supported subset and deliberate limitations.
pub fn cmd_otlp_intake(
    listen: &str,
    into_dir: &Path,
    opts: OtlpOpts,
    exit_on_upgrade: bool,
) -> anyhow::Result<()> {
    opts.retain
        .as_deref()
        .map(crate::append::parse_duration_ms)
        .transpose()?;
    opts.retain_size
        .as_deref()
        .map(crate::append::parse_size_bytes)
        .transpose()?;
    let _dir_lock = intake::open_backing_dir(into_dir)?;

    let cfg = Config {
        chunk_size: 256 * 1024,
        level: 3,
        flush_age_ms: 5000,
    };
    let intake = Arc::new(Mutex::new(Intake::new(into_dir, cfg, ())));
    let opts = Arc::new(opts);

    crate::append::install_signal_handlers();

    let stop = Arc::new(AtomicBool::new(false));
    // Nothing to complete after a tick: an OTLP acknowledgement is the
    // HTTP response, and the request thread has already waited for it.
    let maint = intake::spawn_maintenance(
        Arc::clone(&intake),
        into_dir.to_path_buf(),
        Arc::clone(&stop),
        exit_on_upgrade,
        |_, _| {},
    );

    let listener = match intake::socket_activated_listener() {
        Some(l) => l,
        None => TcpListener::bind(listen)
            .with_context(|| format!("binding otlp-intake listener on {listen}"))?,
    };
    eprintln!(
        "timberfs: otlp-intake listening on {} -> {} (POST /v1/logs, OTLP/JSON)",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| listen.to_string()),
        into_dir.display()
    );

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("timberfs: otlp-intake: accept failed: {e}");
                continue;
            }
        };
        let intake = Arc::clone(&intake);
        let dir = into_dir.to_path_buf();
        let opts = Arc::clone(&opts);
        thread::spawn(move || handle_connection(stream, intake, dir, cfg, opts));
    }

    stop.store(true, Ordering::Relaxed);
    let _ = maint.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::parse_export_request;

    fn req(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    const JSON: (&str, &str) = ("content-type", "application/json");
    const LEN: (&str, &str) = ("content-length", "2");

    #[test]
    fn a_well_formed_request_passes_the_precheck() {
        assert!(precheck(&req("POST", "/v1/logs", &[JSON, LEN]), 1 << 20).is_none());
        // A charset parameter and a trailing slash are still OTLP/JSON.
        assert!(precheck(
            &req(
                "POST",
                "/v1/logs/?x=1",
                &[("content-type", "application/json; charset=utf-8"), LEN]
            ),
            1 << 20
        )
        .is_none());
    }

    #[test]
    fn the_collector_defaults_are_refused_by_name() {
        // Both of these are what an out-of-the-box otlphttp exporter
        // sends, so both refusals must name the setting that fixes them.
        let proto = precheck(
            &req(
                "POST",
                "/v1/logs",
                &[("content-type", "application/x-protobuf"), LEN],
            ),
            1 << 20,
        )
        .unwrap();
        assert_eq!(proto.status, 415);
        assert!(proto.body.contains("encoding: json"), "{}", proto.body);

        let gzip = precheck(
            &req(
                "POST",
                "/v1/logs",
                &[JSON, LEN, ("content-encoding", "gzip")],
            ),
            1 << 20,
        )
        .unwrap();
        assert_eq!(gzip.status, 415);
        assert!(gzip.body.contains("compression: none"), "{}", gzip.body);
    }

    #[test]
    fn other_signals_and_methods_are_named_not_just_refused() {
        let traces = precheck(&req("POST", "/v1/traces", &[JSON, LEN]), 1 << 20).unwrap();
        assert_eq!(traces.status, 404);
        assert!(traces.body.contains("log store"), "{}", traces.body);
        let get = precheck(&req("GET", "/v1/logs", &[JSON, LEN]), 1 << 20).unwrap();
        assert_eq!(get.status, 405);
        assert!(get.extra.iter().any(|h| h == "Allow: POST"));
    }

    #[test]
    fn an_unmeasurable_or_oversized_body_is_refused() {
        let chunked = precheck(
            &req(
                "POST",
                "/v1/logs",
                &[JSON, LEN, ("transfer-encoding", "chunked")],
            ),
            1 << 20,
        )
        .unwrap();
        assert_eq!(chunked.status, 411);
        let no_len = precheck(&req("POST", "/v1/logs", &[JSON]), 1 << 20).unwrap();
        assert_eq!(no_len.status, 411);
        let big = precheck(
            &req("POST", "/v1/logs", &[JSON, ("content-length", "99")]),
            10,
        )
        .unwrap();
        assert_eq!(big.status, 413);
    }

    #[test]
    fn routing_falls_back_to_the_otel_default_name() {
        let request = json!({"resourceLogs": [{
            "resource": {"attributes": [{"key": "host.name", "value": {"stringValue": "h1"}}]},
            "scopeLogs": [{"logRecords": [{"body": {"stringValue": "x"}}]}],
        }]});
        let batches = parse_export_request(&request).unwrap();
        assert_eq!(route_of(&batches[0], "service.name"), "unknown_service");
        assert_eq!(route_of(&batches[0], "host.name"), "h1");
        // And the route is what names the store, sanitized.
        assert_eq!(intake::store_name("checkout/v2"), "checkout_v2.log");
    }
}
