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
