//! OTLP/HTTP for log records: rendering timberfs entries into an
//! `ExportLogsServiceRequest` and posting it.
//!
//! JSON encoding only (the protobuf JSON mapping: lowerCamelCase fields,
//! 64-bit integers as strings), and plaintext HTTP/1.1 only — the same
//! stance `forward-intake` takes, for the same reason: TLS belongs to a
//! collector or a proxy next to the shipper, not to a log tool. That
//! keeps this module dependency-free, `std::net` and `serde_json`.
//!
//! The mapping worth defending is time. OTLP separates the event's own
//! timestamp from when it was observed, which is exactly timberfs's two
//! axes: `timeUnixNano` gets the entry's parsed logline stamp (falling
//! back to arrival when a line carries none), `observedTimeUnixNano`
//! always gets the write time of the chunk it arrived in. A consumer can
//! therefore still see the divergence `--show-write-time` shows.
//!
//! Everything above `post` is pure, so the whole rendering path is
//! exercised by the tests below and by `--dry-run`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{bail, Context};
use regex::Regex;
use serde_json::{json, Map, Value};

/// One entry to ship, as the record stream describes it.
pub struct Entry<'a> {
    /// The entry's own logline timestamp, when it has one.
    pub ts_ms: Option<u64>,
    /// Write window of the chunk it arrived in.
    pub wf_ms: u64,
    /// Payload bytes, verbatim (trailing newline included).
    pub payload: &'a [u8],
}

// ---------------------------------------------------------------------
// Severity.
// ---------------------------------------------------------------------

/// Level names in log lines, mapped to OTLP severity numbers following
/// the spec's syslog mapping. Uppercase-only by default: a case-blind
/// match would read "connection error" as ERROR. Other conventions
/// (`level=info`, lowercase, bracketed) need `--severity-regex`.
const DEFAULT_LEVELS: &str =
    r"\b(TRACE|DEBUG|INFO|NOTICE|WARN|WARNING|ERROR|ERR|SEVERE|FATAL|PANIC|CRITICAL|CRIT)\b";

fn severity_number(text: &str) -> u32 {
    match text.to_ascii_uppercase().as_str() {
        "TRACE" => 1,
        "DEBUG" => 5,
        "INFO" => 9,
        "NOTICE" => 10,
        "WARN" | "WARNING" => 13,
        "ERROR" | "ERR" | "SEVERE" => 17,
        "FATAL" | "PANIC" => 21,
        "CRITICAL" | "CRIT" => 22,
        _ => 0, // UNSPECIFIED: keep the text, claim no level
    }
}

pub struct Severity {
    re: Regex,
}

impl Severity {
    /// A custom pattern reports its first capture group if it has one,
    /// else the whole match — so both `(?i)level=(\w+)` and a bare
    /// alternation work without a flag saying which.
    pub fn new(pattern: Option<&str>) -> anyhow::Result<Severity> {
        let src = pattern.unwrap_or(DEFAULT_LEVELS);
        let re = Regex::new(src).with_context(|| format!("bad severity regex {src:?}"))?;
        Ok(Severity { re })
    }

    /// Detected level of an entry: searched in its FIRST line only, where
    /// log formats put it — a stack trace below must not relabel it.
    pub fn of(&self, payload: &[u8]) -> Option<(String, u32)> {
        let first = payload.split(|&b| b == b'\n').next().unwrap_or_default();
        let head = String::from_utf8_lossy(&first[..first.len().min(512)]);
        let caps = self.re.captures(&head)?;
        let m = caps.get(1).or_else(|| caps.get(0))?;
        let text = m.as_str().to_string();
        let num = severity_number(&text);
        Some((text, num))
    }
}

// ---------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------

fn attributes(pairs: &[(String, String)]) -> Value {
    Value::Array(
        pairs
            .iter()
            .map(|(k, v)| json!({"key": k, "value": {"stringValue": v}}))
            .collect(),
    )
}

/// One `ExportLogsServiceRequest`. Every entry in a batch shares one
/// resource — a shipper is pointed at one store, and the store's `.bark`
/// is what the resource is derived from.
pub fn render(resource: &[(String, String)], scope_version: &str, entries: &[Entry]) -> Value {
    let sev = Severity::new(None).expect("the default severity pattern compiles");
    render_with(resource, scope_version, entries, &sev)
}

pub fn render_with(
    resource: &[(String, String)],
    scope_version: &str,
    entries: &[Entry],
    sev: &Severity,
) -> Value {
    let records: Vec<Value> = entries
        .iter()
        .map(|e| {
            let body = e.payload.strip_suffix(b"\n").unwrap_or(e.payload);
            let mut rec = Map::new();
            // Unix NANOS as a decimal string: the protobuf JSON mapping
            // requires it, and ms * 1e6 overflows nothing we can hold.
            rec.insert(
                "timeUnixNano".into(),
                Value::String((e.ts_ms.unwrap_or(e.wf_ms) as u128 * 1_000_000).to_string()),
            );
            rec.insert(
                "observedTimeUnixNano".into(),
                Value::String((e.wf_ms as u128 * 1_000_000).to_string()),
            );
            if let Some((text, num)) = sev.of(e.payload) {
                rec.insert("severityText".into(), Value::String(text));
                if num > 0 {
                    rec.insert("severityNumber".into(), Value::from(num));
                }
            }
            rec.insert(
                "body".into(),
                json!({"stringValue": String::from_utf8_lossy(body)}),
            );
            Value::Object(rec)
        })
        .collect();
    json!({
        "resourceLogs": [{
            "resource": {"attributes": attributes(resource)},
            "scopeLogs": [{
                "scope": {"name": "timberfs", "version": scope_version},
                "logRecords": records,
            }],
        }],
    })
}

// ---------------------------------------------------------------------
// Decoding: the same mapping read backwards, for the intake.
// ---------------------------------------------------------------------

/// One decoded LogRecord.
pub struct Incoming {
    /// The event's own time, when the sender set one.
    pub time_ms: Option<u64>,
    /// When the sender observed it — the fallback for `time_ms`.
    pub observed_ms: Option<u64>,
    pub severity: Option<String>,
    /// The body as text: a string body verbatim, anything structured as
    /// compact JSON (the same fallback `forward-intake` applies to a
    /// record whose payload key is missing or not a string).
    pub body: String,
    /// Record attributes, plus `trace_id`/`span_id` when the record
    /// carries them — the tokens a trace lookup greps for.
    pub attrs: Vec<(String, String)>,
}

/// One ResourceLogs: what the records are ABOUT, and the records.
pub struct IncomingBatch {
    pub resource: Vec<(String, String)>,
    pub records: Vec<Incoming>,
}

/// An OTLP `AnyValue` as text. A string is itself; everything else is
/// its compact JSON, so nothing is silently dropped and nothing pretends
/// to be a string it is not.
pub fn any_value_text(v: &Value) -> String {
    let Some(obj) = v.as_object() else {
        return v.to_string();
    };
    for key in [
        "stringValue",
        "intValue",
        "doubleValue",
        "boolValue",
        "bytesValue",
    ] {
        if let Some(x) = obj.get(key) {
            return match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
    }
    if let Some(arr) = obj.get("arrayValue").and_then(|a| a.get("values")) {
        return Value::Array(
            arr.as_array()
                .map(|vs| {
                    vs.iter()
                        .map(|v| Value::String(any_value_text(v)))
                        .collect()
                })
                .unwrap_or_default(),
        )
        .to_string();
    }
    if let Some(kvs) = obj.get("kvlistValue").and_then(|k| k.get("values")) {
        let mut m = Map::new();
        for kv in kvs.as_array().unwrap_or(&Vec::new()) {
            if let Some(k) = kv.get("key").and_then(Value::as_str) {
                let val = kv.get("value").map(any_value_text).unwrap_or_default();
                m.insert(k.to_string(), Value::String(val));
            }
        }
        return Value::Object(m).to_string();
    }
    v.to_string()
}

/// Unix nanos, as either the canonical JSON string or a bare number
/// (encoders emit both), to unix ms. Zero means unset.
fn nanos_to_ms(v: Option<&Value>) -> Option<u64> {
    let n: u128 = match v? {
        Value::String(s) => s.parse().ok()?,
        Value::Number(n) => n.as_u64()? as u128,
        _ => return None,
    };
    if n == 0 {
        return None;
    }
    Some((n / 1_000_000) as u64)
}

fn key_values(v: Option<&Value>) -> Vec<(String, String)> {
    v.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|kv| {
                    let k = kv.get("key").and_then(Value::as_str)?;
                    Some((
                        k.to_string(),
                        kv.get("value").map(any_value_text).unwrap_or_default(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Decode an `ExportLogsServiceRequest`. Absent optional fields are
/// absent, never invented: what the sender did not say, the store does
/// not claim. A body-less record is skipped rather than stored empty.
pub fn parse_export_request(v: &Value) -> anyhow::Result<Vec<IncomingBatch>> {
    let Some(resource_logs) = v.get("resourceLogs").and_then(Value::as_array) else {
        bail!("not an ExportLogsServiceRequest: no resourceLogs array");
    };
    let mut out = Vec::new();
    for rl in resource_logs {
        let resource = key_values(rl.get("resource").and_then(|r| r.get("attributes")));
        let mut records = Vec::new();
        for sl in rl
            .get("scopeLogs")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
        {
            for lr in sl
                .get("logRecords")
                .and_then(Value::as_array)
                .unwrap_or(&Vec::new())
            {
                let Some(body) = lr.get("body").map(any_value_text) else {
                    continue;
                };
                let mut attrs = key_values(lr.get("attributes"));
                for (field, key) in [("traceId", "trace_id"), ("spanId", "span_id")] {
                    if let Some(id) = lr.get(field).and_then(Value::as_str) {
                        if !id.is_empty() && id.chars().any(|c| c != '0') {
                            attrs.push((key.to_string(), id.to_string()));
                        }
                    }
                }
                records.push(Incoming {
                    time_ms: nanos_to_ms(lr.get("timeUnixNano")),
                    observed_ms: nanos_to_ms(lr.get("observedTimeUnixNano")),
                    severity: lr
                        .get("severityText")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    body,
                    attrs,
                });
            }
        }
        out.push(IncomingBatch { resource, records });
    }
    Ok(out)
}

impl Incoming {
    /// The event time to store this under: its own, else when the sender
    /// observed it, else now — arrival being the only honest answer left.
    pub fn event_ms(&self, now_ms: u64) -> u64 {
        self.time_ms.or(self.observed_ms).unwrap_or(now_ms)
    }

    /// The log line to append, with a trailing newline.
    ///
    /// Prefix only what the body LACKS: a body that already opens with a
    /// parseable timestamp is written verbatim, so a line that came from
    /// a timberfs store and went out over OTLP comes back byte for byte.
    /// An unstamped body gets `<RFC3339> [SEVERITY] ` so the store stays
    /// time-indexable — the timestamp has to be IN the line, that being
    /// where the read path looks for it. Attributes (and the trace ids)
    /// trail as `k=v`, where the token index can find them.
    pub fn to_line(&self, stamped: bool, now_ms: u64) -> Vec<u8> {
        let mut line = String::new();
        if !stamped {
            line.push_str(&crate::query::fmt_ms_rfc3339(self.event_ms(now_ms)));
            line.push(' ');
            if let Some(sev) = &self.severity {
                line.push_str(sev);
                line.push(' ');
            }
        }
        line.push_str(&self.body);
        for (k, v) in &self.attrs {
            line.push(' ');
            line.push_str(k);
            line.push('=');
            line.push_str(v);
        }
        // Internal newlines are kept: a stack trace is ONE entry, and its
        // continuation lines carry no stamp, so they attach on the way in.
        let mut bytes = line.into_bytes();
        if bytes.last() != Some(&b'\n') {
            bytes.push(b'\n');
        }
        bytes
    }
}

// ---------------------------------------------------------------------
// The endpoint.
// ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "http://{}:{}{}", self.host, self.port, self.path)
    }
}

/// `http://host[:port][/path]` -> an endpoint, appending the OTLP logs
/// path unless it is already there. Both spellings the ecosystem uses
/// therefore work: a base URL (`OTEL_EXPORTER_OTLP_ENDPOINT`) and a
/// signal URL (`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`).
pub fn parse_endpoint(url: &str) -> anyhow::Result<Endpoint> {
    let rest = match url.strip_prefix("http://") {
        Some(r) => r,
        None if url.starts_with("https://") => bail!(
            "{url}: https is not supported — terminate TLS in a collector or proxy \
             next to the shipper and point --endpoint at it over loopback"
        ),
        None => bail!("{url}: endpoint must be an http:// URL"),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        bail!("{url}: no host");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .with_context(|| format!("{url}: bad port {p:?}"))?,
        ),
        None => (authority.to_string(), 4318),
    };
    let trimmed = path.trim_end_matches('/');
    let path = if trimmed.ends_with("/v1/logs") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/logs")
    };
    Ok(Endpoint { host, port, path })
}

// ---------------------------------------------------------------------
// The client.
// ---------------------------------------------------------------------

/// What one export attempt means for the batch that produced it.
pub enum Outcome {
    /// Accepted. `rejected` counts records the receiver refused inside a
    /// 2xx (OTLP's partial success) — a permanent refusal, not a retry.
    Delivered {
        rejected: u64,
        message: Option<String>,
    },
    /// Try the same batch again, after `after` when the receiver said so.
    Retry {
        after: Option<Duration>,
        why: String,
    },
    /// Permanently refused (a 4xx that is not 429, per the OTLP spec).
    /// Retrying cannot help; the batch is dropped, loudly.
    Rejected(String),
}

pub struct Client {
    ep: Endpoint,
    headers: Vec<(String, String)>,
    timeout: Duration,
    conn: Option<BufReader<TcpStream>>,
}

impl Client {
    pub fn new(ep: Endpoint, headers: Vec<(String, String)>, timeout: Duration) -> Client {
        Client {
            ep,
            headers,
            timeout,
            conn: None,
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.ep
    }

    fn connect(&mut self) -> anyhow::Result<()> {
        if self.conn.is_some() {
            return Ok(());
        }
        let addr = (self.ep.host.as_str(), self.ep.port)
            .to_socket_addrs()
            .with_context(|| format!("resolving {}", self.ep.host))?
            .next()
            .with_context(|| format!("{} resolved to no address", self.ep.host))?;
        let s = TcpStream::connect_timeout(&addr, self.timeout)
            .with_context(|| format!("connecting to {addr}"))?;
        s.set_read_timeout(Some(self.timeout))?;
        s.set_write_timeout(Some(self.timeout))?;
        s.set_nodelay(true).ok();
        self.conn = Some(BufReader::new(s));
        Ok(())
    }

    /// POST one rendered request. Transport failures are retryable by
    /// definition; the connection is kept alive across batches and
    /// dropped on any error so the next attempt starts clean.
    pub fn post(&mut self, body: &str) -> Outcome {
        let reused = self.conn.is_some();
        match self.try_post(body) {
            Ok(o) => o,
            Err(e) => {
                self.conn = None;
                // A kept-alive connection the receiver closed while idle
                // fails on the next write, and that is ordinary HTTP, not
                // an outage: open a fresh one and try once before saying
                // anything. Only a failure on a NEW connection is news.
                if reused {
                    if let Ok(o) = self.try_post(body) {
                        return o;
                    }
                    self.conn = None;
                }
                Outcome::Retry {
                    after: None,
                    why: format!("{e:#}"),
                }
            }
        }
    }

    fn try_post(&mut self, body: &str) -> anyhow::Result<Outcome> {
        self.connect()?;
        let mut req = format!(
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: timber-otlp/{}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n",
            self.ep.path,
            self.ep.host,
            self.ep.port,
            env!("CARGO_PKG_VERSION"),
            body.len(),
        );
        for (k, v) in &self.headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        {
            let conn = self.conn.as_mut().expect("connected above");
            let s = conn.get_mut();
            s.write_all(req.as_bytes())?;
            s.write_all(body.as_bytes())?;
            s.flush()?;
        }
        let (status, headers) = self.read_head()?;
        let payload = self.read_body(&headers)?;
        if header(&headers, "connection").is_some_and(|v| v.eq_ignore_ascii_case("close")) {
            self.conn = None;
        }
        Ok(classify(status, &headers, &payload))
    }

    fn read_head(&mut self) -> anyhow::Result<(u16, Vec<(String, String)>)> {
        let conn = self.conn.as_mut().expect("connected");
        let mut line = String::new();
        if conn.read_line(&mut line)? == 0 {
            bail!("the receiver closed the connection without a response");
        }
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .with_context(|| format!("unparseable status line {:?}", line.trim_end()))?;
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            if conn.read_line(&mut h)? == 0 {
                bail!("the receiver closed the connection mid-headers");
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        Ok((status, headers))
    }

    /// Only a Content-Length body is read. A chunked or close-delimited
    /// response makes the connection unreusable and is not decoded: the
    /// status line already carries the verdict, and the body only ever
    /// adds OTLP's partial-success detail.
    fn read_body(&mut self, headers: &[(String, String)]) -> anyhow::Result<String> {
        let len = header(headers, "content-length").and_then(|v| v.parse::<usize>().ok());
        let conn = self.conn.as_mut().expect("connected");
        match len {
            Some(n) => {
                let mut buf = vec![0u8; n];
                conn.read_exact(&mut buf)?;
                Ok(String::from_utf8_lossy(&buf).into_owned())
            }
            None => {
                self.conn = None;
                Ok(String::new())
            }
        }
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// The OTLP/HTTP contract: 2xx is accepted (possibly partially), 429 and
/// 502/503/504 are retryable, every other 4xx/5xx is permanent. Honouring
/// exactly that list is what makes a shipper safe to leave running.
pub fn classify(status: u16, headers: &[(String, String)], body: &str) -> Outcome {
    let retry_after = header(headers, "retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    match status {
        200..=299 => {
            let (mut rejected, mut message) = (0u64, None);
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body) {
                if let Some(Value::Object(p)) = map.get("partialSuccess") {
                    // The count is a string per the JSON mapping, but
                    // receivers emit both spellings.
                    rejected = p
                        .get("rejectedLogRecords")
                        .and_then(|v| {
                            v.as_u64()
                                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        })
                        .unwrap_or(0);
                    message = p
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
            }
            Outcome::Delivered { rejected, message }
        }
        429 | 502 | 503 | 504 => Outcome::Retry {
            after: retry_after,
            why: format!("HTTP {status}"),
        },
        _ => Outcome::Rejected(format!(
            "HTTP {status}{}",
            if body.is_empty() {
                String::new()
            } else {
                format!(": {}", body.trim().chars().take(200).collect::<String>())
            }
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_take_both_spellings() {
        let base = parse_endpoint("http://localhost:4318").unwrap();
        assert_eq!(base.path, "/v1/logs");
        assert_eq!(base.port, 4318);
        let signal = parse_endpoint("http://collector/v1/logs").unwrap();
        assert_eq!(signal.path, "/v1/logs");
        assert_eq!(signal.port, 4318, "the OTLP/HTTP default port");
        let prefixed = parse_endpoint("http://gw:8080/otlp/").unwrap();
        assert_eq!(prefixed.path, "/otlp/v1/logs");
        assert_eq!(prefixed.port, 8080);
    }

    #[test]
    fn https_is_refused_with_a_way_out() {
        let e = parse_endpoint("https://collector:4318")
            .unwrap_err()
            .to_string();
        assert!(e.contains("proxy"), "{e}");
        assert!(parse_endpoint("collector:4318").is_err());
    }

    #[test]
    fn severity_reads_the_first_line_only() {
        let sev = Severity::new(None).unwrap();
        let (text, num) = sev.of(b"2026-08-15 ERROR boom\n\tat Foo.java:1\n").unwrap();
        assert_eq!((text.as_str(), num), ("ERROR", 17));
        // A level word in a stack trace below must not relabel the entry.
        assert!(sev
            .of(b"2026-08-15 plain line\n\tWARN inside a trace\n")
            .is_none());
        // Prose is not a level.
        assert!(sev.of(b"2026-08-15 connection error, retrying\n").is_none());
        assert_eq!(sev.of(b"WARNING x\n").unwrap().1, 13);
        assert_eq!(sev.of(b"CRIT x\n").unwrap().1, 22);
    }

    #[test]
    fn a_custom_pattern_reports_its_capture() {
        let sev = Severity::new(Some(r"level=(\w+)")).unwrap();
        let (text, num) = sev.of(b"ts=1 level=warn msg=hi\n").unwrap();
        assert_eq!((text.as_str(), num), ("warn", 13));
        // Unmapped text keeps the label and claims no number.
        let (text, num) = sev.of(b"ts=1 level=verbose msg=hi\n").unwrap();
        assert_eq!((text.as_str(), num), ("verbose", 0));
    }

    #[test]
    fn rendering_maps_both_time_axes() {
        let entries = vec![
            Entry {
                ts_ms: Some(1_700_000_000_123),
                wf_ms: 1_700_000_005_000,
                payload: b"2026 ERROR boom\n",
            },
            Entry {
                ts_ms: None,
                wf_ms: 1_700_000_005_000,
                payload: b"\tat Foo.java:1\n",
            },
        ];
        let res = vec![("service.name".to_string(), "app".to_string())];
        let v = render(&res, "0.0.0", &entries);
        let recs = &v["resourceLogs"][0]["scopeLogs"][0]["logRecords"];
        assert_eq!(recs[0]["timeUnixNano"], "1700000000123000000");
        assert_eq!(recs[0]["observedTimeUnixNano"], "1700000005000000000");
        assert_eq!(recs[0]["severityNumber"], 17);
        assert_eq!(recs[0]["body"]["stringValue"], "2026 ERROR boom");
        // No logline stamp: the event time falls back to arrival, and the
        // two fields being equal is what says "this is when we saw it".
        assert_eq!(recs[1]["timeUnixNano"], "1700000005000000000");
        assert_eq!(recs[1]["observedTimeUnixNano"], "1700000005000000000");
        assert!(recs[1].get("severityText").is_none());
        assert_eq!(
            v["resourceLogs"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "app"
        );
        assert_eq!(
            v["resourceLogs"][0]["scopeLogs"][0]["scope"]["name"],
            "timberfs"
        );
    }

    /// The property the two directions owe each other: entries shipped
    /// out over OTLP and received back arrive byte for byte, multiline
    /// bodies included. Each direction is the other's oracle, so neither
    /// can be quietly wrong about time, framing or body text.
    #[test]
    fn stamped_entries_roundtrip_byte_for_byte() {
        let payloads: Vec<&[u8]> = vec![
            b"2026-08-15T09:23:45.123+02:00 INFO starting up\n",
            b"2026-08-15T09:23:46.500+02:00 ERROR checkout failed for cart 9912\n\tat com.example.Cart.check(Cart.java:44)\n\tat com.example.Main.main(Main.java:9)\n",
            b"2026-08-15T09:23:47.000+02:00 WARN retrying\n",
        ];
        let entries: Vec<Entry> = payloads
            .iter()
            .enumerate()
            .map(|(i, p)| Entry {
                ts_ms: Some(1_786_778_625_123 + i as u64),
                wf_ms: 1_786_783_226_105,
                payload: p,
            })
            .collect();
        let resource = vec![("service.name".to_string(), "app".to_string())];

        let wire = render(&resource, "0.0.0", &entries);
        let batches = parse_export_request(&wire).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].resource, resource);
        assert_eq!(batches[0].records.len(), payloads.len());

        for (rec, original) in batches[0].records.iter().zip(&payloads) {
            // The intake sees a body that already opens with a stamp, so
            // it writes it verbatim rather than stamping it twice.
            let extractor = crate::import::Extractor::new(None, None, false).unwrap();
            let head = rec.body.lines().next().unwrap_or_default();
            let stamped = extractor.extract(head).is_some();
            assert!(stamped, "the shipper's body keeps the line's own stamp");
            assert_eq!(rec.to_line(stamped, 0), original.to_vec());
        }
        // And the event time survives as the entry's own timestamp.
        assert_eq!(batches[0].records[0].time_ms, Some(1_786_778_625_123));
        assert_eq!(batches[0].records[0].observed_ms, Some(1_786_783_226_105));
    }

    #[test]
    fn an_unstamped_body_is_stamped_on_the_way_in() {
        let rec = Incoming {
            time_ms: Some(1_786_778_625_123),
            observed_ms: None,
            severity: Some("ERROR".into()),
            body: "database is on fire".into(),
            attrs: vec![("trace_id".into(), "4bf92f3577b34da6a3ce929d0e0e4736".into())],
        };
        let line = String::from_utf8(rec.to_line(false, 0)).unwrap();
        assert!(
            line.ends_with(
                " ERROR database is on fire trace_id=4bf92f3577b34da6a3ce929d0e0e4736\n"
            ),
            "{line}"
        );
        // What was prefixed is what the read path parses back out.
        let extractor = crate::import::Extractor::new(None, None, false).unwrap();
        assert_eq!(extractor.extract(&line), Some(1_786_778_625_123));
    }

    #[test]
    fn structured_values_survive_as_text() {
        assert_eq!(any_value_text(&json!({"stringValue": "hi"})), "hi");
        assert_eq!(any_value_text(&json!({"intValue": "42"})), "42");
        assert_eq!(any_value_text(&json!({"boolValue": true})), "true");
        let kv = json!({"kvlistValue": {"values": [
            {"key": "a", "value": {"stringValue": "1"}},
        ]}});
        assert_eq!(any_value_text(&kv), r#"{"a":"1"}"#);
        // A structured body is JSON, not a lie about being a string.
        let req = json!({"resourceLogs": [{"scopeLogs": [{"logRecords": [
            {"body": {"kvlistValue": {"values": [{"key": "msg", "value": {"stringValue": "hi"}}]}}}
        ]}]}]});
        let b = parse_export_request(&req).unwrap();
        assert_eq!(b[0].records[0].body, r#"{"msg":"hi"}"#);
        // Nothing said about time: nothing claimed.
        assert_eq!(b[0].records[0].time_ms, None);
        assert_eq!(b[0].records[0].event_ms(7), 7);
    }

    #[test]
    fn an_all_zero_trace_id_is_not_a_trace_id() {
        let req = json!({"resourceLogs": [{"scopeLogs": [{"logRecords": [
            {"body": {"stringValue": "x"}, "traceId": "00000000000000000000000000000000",
             "spanId": "00f067aa0ba902b7"}
        ]}]}]});
        let b = parse_export_request(&req).unwrap();
        assert_eq!(
            b[0].records[0].attrs,
            vec![("span_id".to_string(), "00f067aa0ba902b7".to_string())]
        );
    }

    #[test]
    fn the_status_contract_is_the_spec_list() {
        let h = vec![];
        assert!(matches!(
            classify(200, &h, "{}"),
            Outcome::Delivered { rejected: 0, .. }
        ));
        for code in [429, 502, 503, 504] {
            assert!(
                matches!(classify(code, &h, ""), Outcome::Retry { .. }),
                "{code}"
            );
        }
        for code in [400, 401, 404, 500] {
            assert!(
                matches!(classify(code, &h, ""), Outcome::Rejected(_)),
                "{code}"
            );
        }
    }

    #[test]
    fn partial_success_and_retry_after_are_read() {
        let h = vec![("retry-after".to_string(), "7".to_string())];
        match classify(503, &h, "") {
            Outcome::Retry { after, .. } => assert_eq!(after, Some(Duration::from_secs(7))),
            _ => panic!("503 must retry"),
        }
        let body = r#"{"partialSuccess":{"rejectedLogRecords":"3","errorMessage":"too old"}}"#;
        match classify(200, &[], body) {
            Outcome::Delivered { rejected, message } => {
                assert_eq!(rejected, 3);
                assert_eq!(message.as_deref(), Some("too old"));
            }
            _ => panic!("200 is delivered"),
        }
    }
}
