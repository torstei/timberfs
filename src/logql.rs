//! ⚠ **A SPIKE.** Enough of Loki's read API for Grafana to point at a
//! timberfs forest, built to find out what Grafana actually demands —
//! not a Loki implementation, and not finished. What it does not support
//! it REFUSES by name (HTTP 400) rather than answering something
//! plausible, because a query language that silently ignores half a
//! pipeline is worse than one that says no.
//!
//! Supported: the stream selector, the four line filters, the time
//! window, `limit` and `direction`. Absent: every parser stage
//! (`| json`, `| logfmt`, `| pattern`, `| regexp`), label filters, line
//! and label formatting, and all metric queries (`rate`,
//! `count_over_time`, `sum by`, …).
//!
//! **The store is the stream.** LogQL's `{app="nginx", env="prod"}` is a
//! label set naming one stream, and timberfs already stores one log per
//! stream with its labels in the `.bark` — so the selector is a scan over
//! manifests, and the time window is what the `.rings` index is for. The
//! two halves of a Loki query are the two things this filesystem is.
//!
//! **The record stream is the interface.** A query runs
//! `timber-filter --records` and reads its output with `records::Reader`,
//! rather than reaching into the read path — which is the composition
//! model this project already states, and means chunk selection, the
//! grain index and the two-clocks entry filtering all apply with no new
//! read code. The cost is a process per store per request; an in-process
//! call would replace it without the HTTP layer noticing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};
use serde_json::{json, Map, Value};

use crate::records::{Reader, Rec};

/// Manifest keys that are NOT labels: identity, lineage, operational
/// settings and content descriptions. What is left is provenance — which
/// is exactly what a label is.
const NOT_LABELS: &[&str] = &[
    "id",
    "created",
    "derived_from",
    "derived_op",
    "window_from",
    "window_to",
    "index",
    "wal",
    "retain",
    "retain_size",
    "retain_unconsumed",
    "cursors",
    "timestamp_regex",
    "timestamp_format",
    "timestamp_utc",
    "command",
    "pattern",
];

// ---------------------------------------------------------------------------
// LogQL, the subset
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOp {
    Eq,
    Ne,
    Re,
    NotRe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matcher {
    pub label: String,
    pub op: MatchOp,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineFilter {
    Contains(String),
    NotContains(String),
    Regex(String),
    NotRegex(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub selector: Vec<Matcher>,
    pub filters: Vec<LineFilter>,
}

/// Parse the supported subset, and refuse the rest BY NAME. A LogQL
/// surface that accepted `| json | status >= 400` and quietly returned
/// unfiltered lines would be worse than useless: the caller would trust
/// a narrowing that never happened.
pub fn parse(q: &str) -> anyhow::Result<Query> {
    let s = q.trim();
    if !s.starts_with('{') {
        // A metric query starts with an aggregation, so this is where
        // `rate(...)` and `sum by (...)` land — named, not "syntax error".
        bail!(
            "only log queries are supported, and they start with a stream selector — \
             `{{app=\"x\"}}`. Metric queries (rate, count_over_time, sum by, topk, …) are \
             not implemented"
        );
    }
    let close = s
        .find('}')
        .context("the stream selector is missing its closing `}`")?;
    let selector = parse_selector(&s[1..close])?;
    if selector.is_empty() {
        bail!("the stream selector matches every stream; name at least one label");
    }
    let filters = parse_filters(s[close + 1..].trim())?;
    Ok(Query { selector, filters })
}

fn parse_selector(inner: &str) -> anyhow::Result<Vec<Matcher>> {
    let mut out = Vec::new();
    for part in split_top(inner, ',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Longest operator first: `!=` and `=~` both contain a one-char op.
        let (label, op, rest) = if let Some((l, r)) = part.split_once("!~") {
            (l, MatchOp::NotRe, r)
        } else if let Some((l, r)) = part.split_once("=~") {
            (l, MatchOp::Re, r)
        } else if let Some((l, r)) = part.split_once("!=") {
            (l, MatchOp::Ne, r)
        } else if let Some((l, r)) = part.split_once('=') {
            (l, MatchOp::Eq, r)
        } else {
            bail!("`{part}` is not a label matcher (want `label=\"value\"`)");
        };
        out.push(Matcher {
            label: label.trim().to_string(),
            op,
            value: unquote(rest.trim())?,
        });
    }
    Ok(out)
}

fn parse_filters(mut rest: &str) -> anyhow::Result<Vec<LineFilter>> {
    let mut out = Vec::new();
    while !rest.is_empty() {
        let (op, tail) = if let Some(t) = rest.strip_prefix("|=") {
            (LineFilter::Contains as fn(String) -> LineFilter, t)
        } else if let Some(t) = rest.strip_prefix("!=") {
            (LineFilter::NotContains as fn(String) -> LineFilter, t)
        } else if let Some(t) = rest.strip_prefix("|~") {
            (LineFilter::Regex as fn(String) -> LineFilter, t)
        } else if let Some(t) = rest.strip_prefix("!~") {
            (LineFilter::NotRegex as fn(String) -> LineFilter, t)
        } else if let Some(t) = rest.strip_prefix('|') {
            // A pipeline stage. Name it, so the caller knows which part of
            // their query is not implemented rather than guessing.
            let stage = t.split_whitespace().next().unwrap_or("");
            bail!(
                "the pipeline stage `| {stage}` is not implemented — only the line filters \
                 `|=`, `!=`, `|~` and `!~` are. Parsers (json, logfmt, pattern, regexp), label \
                 filters and formatting are absent"
            );
        } else {
            bail!("unexpected `{}` after the stream selector", rest.trim());
        };
        let (lit, after) = take_string(tail.trim_start())?;
        out.push(op(lit));
        rest = after.trim_start();
    }
    Ok(out)
}

/// Split on `sep` at the top level, ignoring separators inside string
/// literals — a label value may legitimately contain a comma.
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '`' => {
                quote = Some(c);
                cur.push(c);
            }
            None if c == sep => out.push(std::mem::take(&mut cur)),
            None => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// A whole string literal, and whatever follows it.
fn take_string(s: &str) -> anyhow::Result<(String, &str)> {
    let mut it = s.char_indices();
    let (_, q) = it.next().context("expected a quoted string")?;
    if q != '"' && q != '`' {
        bail!("expected a quoted string, got `{}`", s.trim());
    }
    let mut lit = String::new();
    let mut escaped = false;
    for (i, c) in it {
        if escaped {
            lit.push(match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            escaped = false;
            continue;
        }
        if c == '\\' && q == '"' {
            escaped = true;
            continue;
        }
        if c == q {
            return Ok((lit, &s[i + c.len_utf8()..]));
        }
        lit.push(c);
    }
    bail!("unterminated string literal in `{}`", s.trim())
}

fn unquote(s: &str) -> anyhow::Result<String> {
    let (lit, rest) = take_string(s)?;
    if !rest.trim().is_empty() {
        bail!("trailing `{}` after a label value", rest.trim());
    }
    Ok(lit)
}

// ---------------------------------------------------------------------------
// stores as streams
// ---------------------------------------------------------------------------

/// One store, as a Loki stream.
pub struct Stream {
    /// The store's logical-name path, for `timber-filter`.
    pub path: PathBuf,
    pub labels: BTreeMap<String, String>,
}

/// A Loki label name is `[a-zA-Z_][a-zA-Z0-9_]*`, so a dotted OTLP
/// attribute (`service.name`) has to be flattened — the same
/// transformation Loki itself applies, and worth knowing about, since two
/// manifest keys can flatten onto one label.
fn sanitize(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for (i, c) in key.chars().enumerate() {
        let ok = c.is_ascii_alphanumeric() || c == '_';
        if !ok {
            out.push('_');
        } else if i == 0 && c.is_ascii_digit() {
            out.push('_');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        "_".to_string()
    } else {
        out
    }
}

/// Every store in the configured forests, with its label set. Two
/// synthetic labels always present: `store` (the handle `timberfs query`
/// resolves it by) and `forest`, so a selector can always name exactly
/// one store even on a host whose manifests declare nothing.
pub fn streams() -> Vec<Stream> {
    let mut out = Vec::new();
    for forest in crate::forest::forests_for_list(&[]) {
        for (handle, path) in crate::forest::scan_forest(&forest.dir) {
            let Ok((dir, name)) = crate::query::resolve_backing(&path) else {
                continue;
            };
            let mut labels = BTreeMap::new();
            labels.insert("store".to_string(), handle.clone());
            labels.insert("forest".to_string(), forest.name.clone());
            if let Some(bark) = crate::bark::load(&dir, &name) {
                for (k, v) in &bark {
                    if NOT_LABELS.contains(&k.as_str()) {
                        continue;
                    }
                    let Some(v) = v.as_str() else { continue };
                    // First writer wins on a flattening collision, in the
                    // manifest's own sorted order, so the answer is at
                    // least deterministic.
                    labels.entry(sanitize(k)).or_insert_with(|| v.to_string());
                }
            }
            out.push(Stream { path, labels });
        }
    }
    out
}

/// Does this stream's label set satisfy every matcher? A missing label is
/// the empty string, which is Prometheus's rule and therefore Loki's:
/// `{env!="prod"}` matches a stream with no `env` at all.
pub fn matches(labels: &BTreeMap<String, String>, selector: &[Matcher]) -> bool {
    selector.iter().all(|m| {
        let have = labels.get(&m.label).map(String::as_str).unwrap_or("");
        match m.op {
            MatchOp::Eq => have == m.value,
            MatchOp::Ne => have != m.value,
            // An unanchored-looking regex is fully anchored in LogQL.
            MatchOp::Re => anchored(&m.value).is_some_and(|re| re.is_match(have)),
            MatchOp::NotRe => anchored(&m.value).is_some_and(|re| !re.is_match(have)),
        }
    })
}

fn anchored(pat: &str) -> Option<regex::Regex> {
    regex::Regex::new(&format!("^(?:{pat})$")).ok()
}

// ---------------------------------------------------------------------------
// running one stream's query
// ---------------------------------------------------------------------------

/// The sibling binary, so a dev build drives its own `timber-filter`
/// rather than the installed one.
fn filter_binary() -> PathBuf {
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let sibling = dir.join("timber-filter");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("timber-filter")
}

/// One entry, as Loki wants it.
struct Line {
    ns: u128,
    text: String,
}

/// Run the query against one store and collect its entries.
fn read_stream(s: &Stream, q: &Query, from_ms: u64, to_ms: u64) -> anyhow::Result<Vec<Line>> {
    let mut cmd = Command::new(filter_binary());
    cmd.arg("--records").arg("--quiet");
    cmd.arg("--from").arg(from_ms.to_string());
    cmd.arg("--to").arg(to_ms.to_string());
    for f in &q.filters {
        match f {
            // LogQL's `|=` is a SUBSTRING match, so it maps to
            // --substring and not to --has: --has is word-anchored (and
            // is what rides the grain index), so routing `|=` there would
            // silently narrow the query. See the findings note.
            LineFilter::Contains(t) => {
                cmd.arg("--substring").arg(t);
            }
            LineFilter::NotContains(t) => {
                cmd.arg("--not-substring").arg(t);
            }
            // --regex takes an ATTACHED value: a bare --regex flags the
            // positional instead, which would eat the store path.
            LineFilter::Regex(t) => {
                cmd.arg(format!("--regex={t}"));
            }
            LineFilter::NotRegex(t) => {
                cmd.arg("--not-regex").arg(t);
            }
        }
    }
    cmd.arg(&s.path);
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", filter_binary().display()))?;
    let stdout = child.stdout.take().context("piped stdout")?;
    let mut reader = Reader::new(BufReader::new(stdout));

    let mut out = Vec::new();
    loop {
        match reader.next_rec() {
            Ok(Some(Rec::Entry(e))) => {
                // The entry's OWN stamp is what a reader sees, so it is
                // the Loki timestamp. Falling back to the chunk's write
                // time for an unparseable line, because Loki must have
                // one and dropping the entry would hide data.
                let ms = e.ts.or(e.wl).unwrap_or(0);
                let mut text = String::from_utf8_lossy(&e.payload).into_owned();
                while text.ends_with('\n') || text.ends_with('\r') {
                    text.pop();
                }
                out.push(Line {
                    ns: ms as u128 * 1_000_000,
                    text,
                });
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                // A truncated stream is a real failure, not a short
                // result — that is the whole point of the end marker.
                let _ = child.wait_with_output();
                return Err(e).with_context(|| format!("reading records for {}", s.path.display()));
            }
        }
    }
    let done = child.wait_with_output()?;
    if !done.status.success() {
        bail!(
            "{} failed for {}: {}",
            filter_binary().display(),
            s.path.display(),
            String::from_utf8_lossy(&done.stderr).trim()
        );
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// the HTTP surface
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

/// Percent-decoding, plus `+` for space in a form body. No url crate, and
/// this is the only decoding the API needs.
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every `k=v` in a query string or form body, decoded. Repeated keys are
/// kept in order, since `match[]` is repeatable.
fn params(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (urldecode(k), urldecode(v)),
            None => (urldecode(p), String::new()),
        })
        .collect()
}

fn first<'a>(ps: &'a [(String, String)], k: &str) -> Option<&'a str> {
    ps.iter()
        .find(|(pk, _)| pk == k)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// Grafana sends nanosecond epochs; `logcli` and hand-written curl send
/// RFC3339. Accept both, and say so when it is neither.
fn time_ms(v: &str, default_ms: u64) -> anyhow::Result<u64> {
    if v.is_empty() {
        return Ok(default_ms);
    }
    if let Ok(n) = v.parse::<u128>() {
        // Disambiguated by magnitude, the way every Loki client does it:
        // ns since 1970 is ~1e18 today, seconds ~1e9.
        return Ok(match v.len() {
            0..=11 => (n * 1000) as u64,
            12..=14 => n as u64,
            15..=17 => (n / 1_000) as u64,
            _ => (n / 1_000_000) as u64,
        });
    }
    crate::query::parse_time(v).with_context(|| format!("unparseable time {v:?}"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ok(data: Value) -> (u16, Value) {
    (200, json!({"status": "success", "data": data}))
}

/// Loki's error shape: a plain-text-ish body with the message, which is
/// what Grafana surfaces to the user. Being specific here is the whole
/// reason unsupported LogQL is refused rather than ignored.
fn err(status: u16, message: impl std::fmt::Display) -> (u16, Value) {
    (
        status,
        json!({"status": "error", "errorType": "bad_data", "error": message.to_string()}),
    )
}

/// An expression made only of `vector(N)` terms added together — which is
/// the entire PromQL surface Grafana's Loki health check uses. `None` for
/// anything else, so a real metric query still gets its "not implemented".
fn eval_vector_literal(q: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut any = false;
    for term in q.split('+') {
        let t = term.trim();
        let inner = t.strip_prefix("vector(")?.strip_suffix(')')?;
        total += inner.trim().parse::<f64>().ok()?;
        any = true;
    }
    any.then_some(total)
}

fn handle(req: &Request) -> (u16, Value) {
    let ps = if req.method == "POST" && !req.body.is_empty() {
        params(&String::from_utf8_lossy(&req.body))
    } else {
        params(&req.query)
    };
    let path = req.path.as_str();
    match path {
        "/ready" | "/loki/api/v1/status/buildinfo" => ok(json!({"version": "timberfs-spike"})),

        "/loki/api/v1/labels" => {
            let mut names: BTreeSet<String> = BTreeSet::new();
            for s in streams() {
                names.extend(s.labels.keys().cloned());
            }
            ok(Value::Array(names.into_iter().map(Value::String).collect()))
        }

        // The values a label takes across the forest.
        p if p.starts_with("/loki/api/v1/label/") && p.ends_with("/values") => {
            let name = &p["/loki/api/v1/label/".len()..p.len() - "/values".len()];
            let mut vals: BTreeSet<String> = BTreeSet::new();
            for s in streams() {
                if let Some(v) = s.labels.get(name) {
                    vals.insert(v.clone());
                }
            }
            ok(Value::Array(vals.into_iter().map(Value::String).collect()))
        }

        "/loki/api/v1/series" => {
            let selectors: Vec<&str> = ps
                .iter()
                .filter(|(k, _)| k == "match[]" || k == "match")
                .map(|(_, v)| v.as_str())
                .collect();
            let mut out = Vec::new();
            for s in streams() {
                let keep = selectors.is_empty()
                    || selectors.iter().any(|sel| match parse(sel) {
                        Ok(q) => matches(&s.labels, &q.selector),
                        Err(_) => false,
                    });
                if keep {
                    out.push(Value::Object(
                        s.labels
                            .into_iter()
                            .map(|(k, v)| (k, Value::String(v)))
                            .collect::<Map<String, Value>>(),
                    ));
                }
            }
            ok(Value::Array(out))
        }

        // Grafana asks for these to size a query before running it. Zeros
        // are honest here: there is no global index to consult, and a
        // fabricated estimate would be worse than none.
        "/loki/api/v1/index/stats" => ok(json!({
            "streams": 0, "chunks": 0, "entries": 0, "bytes": 0
        })),

        "/loki/api/v1/query_range" | "/loki/api/v1/query" => {
            let Some(qs) = first(&ps, "query") else {
                return err(400, "no `query` parameter");
            };
            // ⚠ Grafana's datasource HEALTH CHECK is a metric query:
            //   GET /query?query=vector(1)%2Bvector(1)&time=4000000000
            // A PromQL vector literal, at a year-2096 instant so it can
            // touch no data. So no amount of log-query support makes the
            // datasource go green — this one expression has to evaluate.
            // Answered as the special case it is, rather than pretending
            // to have an expression engine.
            if let Some(v) = eval_vector_literal(qs) {
                return ok(json!({
                    "resultType": "vector",
                    "result": [{
                        "metric": {},
                        "value": [time_ms(first(&ps, "time").unwrap_or(""), now_ms()).unwrap_or(0) / 1000,
                                  v.to_string()],
                    }],
                    "stats": json!({}),
                }));
            }
            let q = match parse(qs) {
                Ok(q) => q,
                Err(e) => return err(400, e),
            };
            let now = now_ms();
            // An instant query has no range; Grafana still sends one for
            // logs. An hour is the least surprising default.
            let to = match time_ms(first(&ps, "end").unwrap_or(""), now) {
                Ok(t) => t,
                Err(e) => return err(400, e),
            };
            let from = match time_ms(
                first(&ps, "start").unwrap_or(""),
                to.saturating_sub(3_600_000),
            ) {
                Ok(t) => t,
                Err(e) => return err(400, e),
            };
            let limit: usize = first(&ps, "limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(100);
            let backward = first(&ps, "direction").unwrap_or("backward") != "forward";

            let mut result = Vec::new();
            for s in streams() {
                if !matches(&s.labels, &q.selector) {
                    continue;
                }
                let mut lines = match read_stream(&s, &q, from, to) {
                    Ok(l) => l,
                    Err(e) => return err(500, e),
                };
                if lines.is_empty() {
                    continue;
                }
                // Sorted here rather than trusted from the store: logline
                // stamps are not monotonic (an entry written now can be
                // stamped an hour ago), and Loki requires order.
                lines.sort_by_key(|l| l.ns);
                if backward {
                    lines.reverse();
                }
                lines.truncate(limit);
                result.push(json!({
                    "stream": s.labels,
                    "values": lines.iter()
                        .map(|l| json!([l.ns.to_string(), l.text]))
                        .collect::<Vec<Value>>(),
                }));
            }
            ok(json!({
                "resultType": "streams",
                "result": result,
                "stats": json!({}),
            }))
        }

        other => err(404, format!("{other} is not implemented by this spike")),
    }
}

fn write_reply(w: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let text = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    write!(
        w,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        text.len()
    )?;
    w.write_all(&text)?;
    w.flush()
}

fn read_request(reader: &mut BufReader<&TcpStream>) -> anyhow::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || target.is_empty() {
        bail!("malformed request line {:?}", line.trim_end());
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            bail!("connection closed mid-headers");
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len.min(1 << 20)];
    if !body.is_empty() {
        std::io::Read::read_exact(reader, &mut body).context("closed mid-body")?;
    }
    Ok(Some(Request {
        method,
        path,
        query,
        body,
    }))
}

/// `timberfs logql-serve`: the spike's entry point.
pub fn cmd_logql_serve(listen: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(listen).with_context(|| format!("binding {listen}"))?;
    eprintln!(
        "timberfs: logql-serve (A SPIKE) on http://{listen} — point a Grafana Loki datasource here"
    );
    eprintln!(
        "timberfs: serving {} store(s); log queries only, no pipeline stages, no metrics",
        streams().len()
    );
    for conn in listener.incoming() {
        let mut conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("timberfs: accept failed: {e}");
                continue;
            }
        };
        let req = {
            let mut reader = BufReader::new(&conn);
            read_request(&mut reader)
        };
        match req {
            Ok(Some(req)) => {
                let started = std::time::Instant::now();
                let (status, body) = handle(&req);
                // The whole query, verbatim: a spike exists to find out
                // what a client actually sends, and a summarised log
                // would hide exactly that.
                let asked = if req.method == "POST" && !req.body.is_empty() {
                    String::from_utf8_lossy(&req.body).into_owned()
                } else {
                    req.query.clone()
                };
                crate::note!(
                    "timberfs: {} {} -> {status} in {:?}\n           {}",
                    req.method,
                    req.path,
                    started.elapsed(),
                    params(&asked)
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("  ")
                );
                if let Err(e) = write_reply(&mut conn, status, &body) {
                    eprintln!("timberfs: reply failed: {e}");
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("timberfs: bad request: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selector_with_line_filters_parses() {
        let q = parse(r#"{store="app", env=~"prod|stage"} |= "ERROR" != "healthz" |~ "id=[0-9]+""#)
            .unwrap();
        assert_eq!(q.selector.len(), 2);
        assert_eq!(q.selector[0].label, "store");
        assert_eq!(q.selector[0].op, MatchOp::Eq);
        assert_eq!(q.selector[1].op, MatchOp::Re);
        assert_eq!(
            q.filters,
            vec![
                LineFilter::Contains("ERROR".into()),
                LineFilter::NotContains("healthz".into()),
                LineFilter::Regex("id=[0-9]+".into()),
            ]
        );
    }

    #[test]
    fn what_is_not_implemented_is_named_not_ignored() {
        // The whole point: a query language that silently drops half a
        // pipeline hands back a narrowing that never happened.
        let e = parse(r#"{store="app"} | json | status >= 400"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("| json"), "{e}");
        assert!(e.contains("not implemented"), "{e}");

        let e = parse(r#"sum by (level) (count_over_time({store="app"}[5m]))"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Metric queries"), "{e}");

        // And a bare selector with no braces is not silently accepted.
        assert!(parse("app").is_err());
        assert!(parse(r#"{}"#).is_err());
    }

    #[test]
    fn label_values_may_contain_the_separator() {
        let q = parse(r#"{msg="a,b", other="c"}"#).unwrap();
        assert_eq!(q.selector.len(), 2);
        assert_eq!(q.selector[0].value, "a,b");
    }

    #[test]
    fn backticks_are_raw_and_quotes_escape() {
        let q = parse(r#"{store="app"} |~ `\d+` |= "a\"b""#).unwrap();
        assert_eq!(
            q.filters,
            vec![
                LineFilter::Regex(r"\d+".into()),
                LineFilter::Contains("a\"b".into()),
            ]
        );
    }

    #[test]
    fn a_missing_label_is_the_empty_string() {
        // Prometheus's rule, and therefore Loki's: `{env!="prod"}` matches
        // a stream that has no `env` at all.
        let labels: BTreeMap<String, String> = [("store".to_string(), "app".to_string())]
            .into_iter()
            .collect();
        assert!(matches(
            &labels,
            &parse(r#"{env!="prod"}"#).unwrap().selector
        ));
        assert!(!matches(
            &labels,
            &parse(r#"{env="prod"}"#).unwrap().selector
        ));
        assert!(matches(
            &labels,
            &parse(r#"{store=~"a.*"}"#).unwrap().selector
        ));
        // LogQL anchors a regex fully, so a partial match does not count.
        assert!(!matches(
            &labels,
            &parse(r#"{store=~"a"}"#).unwrap().selector
        ));
    }

    #[test]
    fn label_names_are_flattened_the_way_loki_flattens_them() {
        assert_eq!(sanitize("service.name"), "service_name");
        assert_eq!(sanitize("deployment.environment"), "deployment_environment");
        assert_eq!(sanitize("host"), "host");
        assert_eq!(sanitize("9lives"), "_9lives");
        assert_eq!(sanitize(""), "_");
    }

    #[test]
    fn times_are_accepted_in_every_spelling_a_client_sends() {
        // Grafana sends nanoseconds; logcli and curl send RFC3339.
        assert_eq!(time_ms("1787000000000000000", 0).unwrap(), 1787000000000);
        assert_eq!(time_ms("1787000000000", 0).unwrap(), 1787000000000);
        assert_eq!(time_ms("1787000000", 0).unwrap(), 1787000000000);
        assert_eq!(time_ms("", 42).unwrap(), 42);
        assert!(time_ms("2026-08-21T10:00:00Z", 0).unwrap() > 1_700_000_000_000);
        assert!(time_ms("not a time", 0).is_err());
    }

    #[test]
    fn query_strings_and_form_bodies_decode_the_same() {
        let ps = params("query=%7Bstore%3D%22app%22%7D+%7C%3D+%22x%22&limit=50");
        assert_eq!(first(&ps, "query"), Some(r#"{store="app"} |= "x""#));
        assert_eq!(first(&ps, "limit"), Some("50"));
        // Repeated keys survive, since `match[]` is repeatable.
        let ps = params("match%5B%5D=%7Ba%3D%221%22%7D&match%5B%5D=%7Bb%3D%222%22%7D");
        assert_eq!(ps.iter().filter(|(k, _)| k == "match[]").count(), 2);
    }
}
