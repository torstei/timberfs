//! tally: metrics derived from a log, on a tape of their own.
//!
//! A tally line is a bucket of one metric: a stamp, a width, a name,
//! labels, and one or more of five measures. The measures are the whole
//! type system — `count`, `sum`, `min`, `max`, `last`, each one being its
//! own coarsening rule — so the tape carries no schema and a reader needs
//! no registry to know that two buckets combine.
//!
//! An OBSERVATION is the same line at width `0s`: one measurement of one
//! entry. Bucketing is then the fold over observations, and coarsening a
//! tape is the same fold over buckets — one operation, three call sites.
//! It is also the whole protocol for a site's own extractor, which emits
//! width-`0s` lines and never has to get sealing or revisions right.
//!
//! See docs/plans/tally.md for what this is for and why the format is
//! shaped this way.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a site's rules live, under `/etc/timberfs`.
pub const CONF_DIR: &str = "tally.d";

/// A metric name may not start with this; a MARKER does, so a reader
/// tells them apart on the first byte and no rule can name one.
pub const MARKER: char = '!';

/// The five measures. Each one says how two of them combine, which is
/// why the tape needs no type declaration: `sum` and `count` add, `min`
/// and `max` take the extreme, `last` takes the newer. A measure that
/// could not be combined this way could not be re-bucketed, and would
/// therefore not be storable.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Field {
    Count,
    Sum,
    Min,
    Max,
    Last,
}

impl Field {
    pub const ALL: [Field; 5] = [
        Field::Count,
        Field::Sum,
        Field::Min,
        Field::Max,
        Field::Last,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Field::Count => "count",
            Field::Sum => "sum",
            Field::Min => "min",
            Field::Max => "max",
            Field::Last => "last",
        }
    }

    pub fn parse(s: &str) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.as_str() == s)
    }

    /// Fold `next` (stamped `next_ts`) into `have` (stamped `have_ts`).
    fn combine(self, have: (f64, u64), next: (f64, u64)) -> (f64, u64) {
        match self {
            Field::Count | Field::Sum => (have.0 + next.0, have.1.max(next.1)),
            Field::Min => {
                if next.0 < have.0 {
                    next
                } else {
                    have
                }
            }
            Field::Max => {
                if next.0 > have.0 {
                    next
                } else {
                    have
                }
            }
            // Ties go to the later arrival, which is the same rule a
            // revision is resolved by.
            Field::Last => {
                if next.1 >= have.1 {
                    next
                } else {
                    have
                }
            }
        }
    }
}

/// One line: a bucket, or an observation when `width_ms` is zero.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The bucket's START, on the rule's declared axis.
    pub ts: u64,
    /// Zero is an INSTANT — an observation, not a bucket of no width.
    pub width_ms: u64,
    pub metric: String,
    /// Sorted by key, so one series always renders as one run of bytes.
    pub labels: Vec<(String, String)>,
    /// In `Field` order, for the same reason.
    pub fields: Vec<(Field, f64)>,
    /// The span of source-store tape this counted: offset and length. A
    /// range to READ, not the set of entries — a rule matching one line
    /// in a hundred cites the ninety-nine between them, and a reader
    /// re-applies the predicate.
    pub cite: Option<(u64, u64)>,
}

impl Sample {
    pub fn new(ts: u64, width_ms: u64, metric: &str) -> Sample {
        Sample {
            ts,
            width_ms,
            metric: metric.to_string(),
            labels: Vec::new(),
            fields: Vec::new(),
            cite: None,
        }
    }

    pub fn label(mut self, k: &str, v: &str) -> Sample {
        self.labels.push((k.to_string(), v.to_string()));
        self.labels.sort();
        self
    }

    pub fn field(mut self, f: Field, v: f64) -> Sample {
        self.fields.retain(|(had, _)| *had != f);
        self.fields.push((f, v));
        self.fields.sort_by_key(|(f, _)| *f);
        self
    }

    pub fn is_marker(&self) -> bool {
        self.metric.starts_with(MARKER)
    }

    /// What identifies the SERIES: everything but the measures and the
    /// citation.
    pub fn series(&self) -> (&str, &[(String, String)]) {
        (&self.metric, &self.labels)
    }

    pub fn render(&self) -> String {
        let mut s = String::with_capacity(96);
        s.push_str(&crate::bark::ms_rfc3339(self.ts));
        s.push(' ');
        s.push_str(&render_width(self.width_ms));
        s.push(' ');
        s.push_str(&self.metric);
        for (k, v) in &self.labels {
            s.push(' ');
            s.push_str(k);
            s.push('=');
            s.push_str(&quote(v));
        }
        for (f, v) in &self.fields {
            s.push(' ');
            s.push_str(f.as_str());
            s.push('=');
            s.push_str(&number(*v));
        }
        if let Some((off, len)) = self.cite {
            s.push_str(&format!(" @{off}+{len}"));
        }
        s
    }

    pub fn parse(line: &str) -> anyhow::Result<Sample> {
        let toks = tokenize(line)?;
        if toks.len() < 3 {
            bail!("a tally line is `<ts> <width> <metric> …` — got {line:?}");
        }
        let ts = parse_stamp(&toks[0])?;
        let width_ms = parse_width(&toks[1])?;
        let metric = toks[2].clone();
        check_metric(&metric)?;
        let mut out = Sample::new(ts, width_ms, &metric);
        for tok in &toks[3..] {
            if let Some(cite) = tok.strip_prefix('@') {
                let (off, len) = cite
                    .split_once('+')
                    .with_context(|| format!("a citation is @<offset>+<len> — got {tok:?}"))?;
                out.cite = Some((off.parse()?, len.parse()?));
                continue;
            }
            let Some((k, v)) = tok.split_once('=') else {
                bail!("expected key=value, a citation, or nothing — got {tok:?}");
            };
            if let Some(f) = Field::parse(k) {
                if out.fields.iter().any(|(had, _)| *had == f) {
                    bail!("{} stated twice in {line:?}", f.as_str());
                }
                let v: f64 = v
                    .parse()
                    .with_context(|| format!("{k}={v:?} is not a number"))?;
                out.fields.push((f, v));
            } else {
                if out.labels.iter().any(|(had, _)| had == k) {
                    bail!("label {k:?} stated twice in {line:?}");
                }
                out.labels.push((k.to_string(), v.to_string()));
            }
        }
        if out.fields.is_empty() && !out.is_marker() {
            bail!("{line:?} carries no measure — a sample states at least one");
        }
        out.labels.sort();
        out.fields.sort_by_key(|(f, _)| *f);
        Ok(out)
    }
}

/// Whole seconds, always: a width is written in the syntax `retain`
/// takes and rendered back canonically, so `5m` in a rule and `300s` on
/// the tape are the same width said twice.
fn render_width(ms: u64) -> String {
    format!("{}s", ms / 1000)
}

fn parse_width(t: &str) -> anyhow::Result<u64> {
    let ms = crate::append::parse_duration_ms(t)?;
    if ms % 1000 != 0 {
        bail!("a bucket width is whole seconds — got {t:?}");
    }
    Ok(ms)
}

fn parse_stamp(t: &str) -> anyhow::Result<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(t)
        .with_context(|| format!("{t:?} is not an RFC3339 timestamp"))?;
    let ms = dt.timestamp_millis();
    if ms < 0 {
        bail!("{t:?} precedes the epoch");
    }
    Ok(ms as u64)
}

/// Shortest round-tripping form, and an integer where the value is one:
/// a count of 42 has no business rendering as 42.0.
fn number(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn quote(v: &str) -> String {
    let needs = v.is_empty()
        || v.chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\\' || c.is_control());
    if !needs {
        return v.to_string();
    }
    let mut s = String::with_capacity(v.len() + 2);
    s.push('"');
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\t' => s.push_str("\\t"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// Split on whitespace, except inside double quotes. The same splitter
/// reads a tally line and a logfmt log line, which is not a coincidence:
/// the label syntax IS logfmt.
pub fn tokenize(line: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            '\\' if in_quotes => match chars.next() {
                Some('n') => cur.push('\n'),
                Some('t') => cur.push('\t'),
                Some(c) => cur.push(c),
                None => bail!("a trailing backslash inside quotes in {line:?}"),
            },
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if in_quotes {
        bail!("unterminated quote in {line:?}");
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

fn check_metric(name: &str) -> anyhow::Result<()> {
    let body = name.strip_prefix(MARKER).unwrap_or(name);
    if body.is_empty() {
        bail!("a metric has a name");
    }
    let ok = body
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
    if !ok || body.starts_with(|c: char| c.is_ascii_digit()) {
        bail!("{name:?} is not a metric name (letters, digits, _ and :, not leading a digit)");
    }
    Ok(())
}

// ---------------------------------------------------------------- the fold

type Key = (u64, String, Vec<(String, String)>);
/// A bucket across widths: what a revision replaces, and the only thing
/// two lines have to agree on to be the same bucket said twice.
type Revised = (u64, u64, String, Vec<(String, String)>);

#[derive(Default)]
struct Bucket {
    fields: BTreeMap<Field, (f64, u64)>,
    cite_lo: Option<u64>,
    cite_hi: Option<u64>,
    /// The oldest source byte folded in here — what a consumer's
    /// watermark may not pass while this bucket can still change.
    off_lo: Option<u64>,
    emitted: bool,
    dirty: bool,
    capped: u64,
}

/// Observations in, buckets out. Also the coarsener: feeding it buckets
/// of one width and asking for a larger one is the same fold.
///
/// Sealing runs off the WATERMARK — the greatest stamp seen — rather
/// than a wall clock, so a backfill of last month behaves exactly as the
/// live run did and a test is deterministic.
pub struct Roller {
    width_ms: u64,
    grace_ms: u64,
    revise_ms: u64,
    max_series: usize,
    buckets: BTreeMap<Key, Bucket>,
    watermark: u64,
}

impl Roller {
    pub fn new(width_ms: u64, grace_ms: u64, revise_ms: u64, max_series: usize) -> Roller {
        Roller {
            width_ms: width_ms.max(1),
            grace_ms,
            revise_ms,
            max_series,
            buckets: BTreeMap::new(),
            watermark: 0,
        }
    }

    /// A one-shot fold with no sealing, for coarsening a closed set.
    pub fn fold(width_ms: u64, samples: &[Sample]) -> Vec<Sample> {
        let mut r = Roller::new(width_ms, 0, 0, usize::MAX);
        for s in samples {
            r.add(s);
        }
        r.drain(true)
    }

    pub fn watermark(&self) -> u64 {
        self.watermark
    }

    /// The oldest source byte any held bucket still depends on. A
    /// consumer may not report past this: a bucket that can still be
    /// revised must be reconstructible after a restart, and re-reading
    /// those entries re-derives the identical lines.
    pub fn safe_offset(&self) -> Option<u64> {
        self.buckets.values().filter_map(|b| b.off_lo).min()
    }

    pub fn add(&mut self, s: &Sample) {
        self.watermark = self.watermark.max(s.ts);
        let start = s.ts - s.ts % self.width_ms;
        let key = (start, s.metric.clone(), s.labels.clone());
        if !self.buckets.contains_key(&key) {
            let in_bucket = self
                .buckets
                .range((start, String::new(), Vec::new())..)
                .take_while(|((t, _, _), _)| *t == start)
                .count();
            if in_bucket >= self.max_series {
                // Bounded loss, recorded exactly — the rule retention
                // already follows. Charged to any bucket at this start,
                // so the count survives even when the cap is hit by a
                // series that never gets one of its own.
                if let Some((_, b)) = self
                    .buckets
                    .range_mut((start, String::new(), Vec::new())..)
                    .take_while(|((t, _, _), _)| *t == start)
                    .next()
                {
                    b.capped += 1;
                    b.dirty = true;
                }
                return;
            }
        }
        let b = self.buckets.entry(key).or_default();
        for (f, v) in &s.fields {
            let next = (*v, s.ts);
            let merged = match b.fields.get(f) {
                Some(have) => f.combine(*have, next),
                None => next,
            };
            b.fields.insert(*f, merged);
        }
        if let Some((off, len)) = s.cite {
            b.cite_lo = Some(b.cite_lo.map_or(off, |lo| lo.min(off)));
            b.cite_hi = Some(b.cite_hi.map_or(off + len, |hi| hi.max(off + len)));
            b.off_lo = Some(b.off_lo.map_or(off, |lo| lo.min(off)));
        }
        b.dirty = true;
    }

    /// Every bucket the watermark has left behind: first when it seals,
    /// again as a REVISION if a late observation changed it, and then
    /// evicted once nothing more can arrive for it.
    pub fn drain(&mut self, force: bool) -> Vec<Sample> {
        let mut out = Vec::new();
        let mut evict: Vec<Key> = Vec::new();
        for (key, b) in self.buckets.iter_mut() {
            let end = key.0 + self.width_ms;
            if !force && self.watermark < end.saturating_add(self.grace_ms) {
                continue;
            }
            if b.dirty {
                let mut s = Sample::new(key.0, self.width_ms, &key.1);
                s.labels = key.2.clone();
                s.fields = b.fields.iter().map(|(f, (v, _))| (*f, *v)).collect();
                if let (Some(lo), Some(hi)) = (b.cite_lo, b.cite_hi) {
                    s.cite = Some((lo, hi - lo));
                }
                out.push(s);
                if b.capped > 0 {
                    out.push(
                        Sample::new(key.0, self.width_ms, "!cap")
                            .label("metric", &key.1)
                            .field(Field::Count, b.capped as f64),
                    );
                }
                b.emitted = true;
                b.dirty = false;
            }
            if force || self.watermark >= end.saturating_add(self.grace_ms + self.revise_ms) {
                evict.push(key.clone());
            }
        }
        for k in evict {
            self.buckets.remove(&k);
        }
        out
    }
}

/// The newest line wins, per bucket — how a revision replaces what it
/// revises. Tape order is arrival order, so "newest" is "later in the
/// input", and no timestamp comparison is involved.
pub fn resolve(samples: Vec<Sample>) -> Vec<Sample> {
    let mut seen: BTreeMap<Revised, usize> = BTreeMap::new();
    let mut out: Vec<Option<Sample>> = Vec::with_capacity(samples.len());
    for s in samples {
        let key = (s.ts, s.width_ms, s.metric.clone(), s.labels.clone());
        if let Some(prev) = seen.insert(key, out.len()) {
            out[prev] = None;
        }
        out.push(Some(s));
    }
    out.into_iter().flatten().collect()
}

// ----------------------------------------------------------- the extractor

/// The extractor document's format version, matched exactly.
pub const EXTRACTOR_VERSION: &str = "1.0-EXPERIMENTAL";

/// A named set of metrics read off ONE SHAPE OF LINE.
///
/// It carries no store selection, and that absence is the design: a
/// document that says nothing about this host is an ARTEFACT — shippable,
/// shareable, versioned — rather than configuration. Which stores get
/// measured is the provisioning's business (docs/plans/tally.md).
///
/// ⚠ Its subject is a line SHAPE, never a store. A store carries many
/// shapes at once — logfmt request lines beside stack traces beside a
/// startup banner — so each metric CLAIMS its own lines rather than
/// assuming the store is uniform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Extractor {
    /// The format version, matched exactly against `EXTRACTOR_VERSION`.
    pub v: String,
    /// What this document IS, declared here and never taken from the
    /// filename: renaming a file must not change what a document is, the
    /// rule a metric name and a store identity already follow.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub window: Window,
    pub metrics: Vec<Metric>,
}

/// The bucket every metric in this document is folded into.
///
/// A DEFAULT, which a provisioning may override — the window is partly a
/// deployment choice (a busy host may want ten seconds where a quiet one
/// wants sixty) and partly incomplete without a value.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Window {
    /// Required, with no default, for the reason a query document's
    /// `window.axis` is: logline and write bucket differently, and a
    /// default is an assumption the next reader makes wrongly.
    pub axis: Axis,
    pub width_ms: u64,
    /// How long past a bucket's end before it is sealed and written.
    #[serde(default = "default_grace")]
    pub grace_ms: u64,
    /// How long after sealing a late entry may still restate it, as a
    /// revision.
    #[serde(default = "default_revise")]
    pub revise_ms: u64,
    /// Distinct series per bucket, after which a `!cap` marker counts
    /// what was lost. Cardinality is where every metrics system dies.
    #[serde(default = "default_max_series")]
    pub max_series: usize,
}

fn default_grace() -> u64 {
    120_000
}
fn default_revise() -> u64 {
    3_600_000
}
fn default_max_series() -> usize {
    1000
}

/// Which clock a bucket is on.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// The timestamps the lines themselves carry.
    Logline,
    /// When the data arrived. ⚠ An entry's arrival is its CHUNK's, so a
    /// bucket on this axis is only as fine as a chunk.
    Write,
}

/// One metric: which lines are mine, how to read one, and what to measure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub name: String,
    /// Shown where a comment could only be read in the file — in a
    /// listing, in an error. Which is why the format has no comments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// WHICH LINES ARE MINE. Absent means every entry, which is right for
    /// a volume metric and wrong for anything that parses: without a
    /// claim, "this line is not mine" and "this line is mine and broken"
    /// cannot be told apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
    /// Where fields come from. Absent means none are read, which only a
    /// bare `count` can do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<FieldSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub measure: Vec<MeasureSpec>,
    /// Cumulative buckets, emitted as one series per `le` — Prometheus's
    /// own encoding, so a quantile is interpolation over sums and needs
    /// no syntax of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram: Option<Histogram>,
    /// Record the source tape span each bucket counted. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cite: Option<bool>,
}

/// The lines a metric claims, in the query document's own vocabulary:
/// every `all` must match, at least one `any` must, and no `none` may.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Claim {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all: Vec<crate::querydoc::Predicate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any: Vec<crate::querydoc::Predicate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub none: Vec<crate::querydoc::Predicate>,
}

/// Where a claimed line's fields come from. Exactly one is set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FieldSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode: Option<Decoder>,
    /// A regex whose NAMED capture groups become the fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
}

/// A line of known shape turned into fields.
///
/// ⚠ The list is CLOSED, and narrowly: a decoder exists only where the
/// KEY SET IS OPEN and a regex therefore cannot express the shape.
/// `logfmt` and `json` qualify — there is no capture group for a key you
/// do not know in advance. Anything POSITIONAL is what a regex does well
/// and belongs in an `extract`; `apache-combined` is here because it is a
/// published grammar many sites share and easy to get subtly wrong by
/// hand, not because it could not be written as one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum Decoder {
    /// `key=value` pairs. ⚠ The names are the PRODUCER's — the keys its
    /// own lines carry — so nothing here can know them.
    Logfmt,
    /// A JSON object. ⚠ TOP LEVEL only: a nested object is skipped and
    /// there is no path syntax.
    Json,
    /// NCSA common and combined. The decoder names the fields, being
    /// positional: host, ident, user, time, request, status, bytes,
    /// referer, agent, plus method, path and protocol split out of the
    /// request line.
    ApacheCombined,
}

impl Decoder {
    /// The field names this decoder produces, where it decides them.
    /// `None` says the PRODUCER names them, so nothing can be checked
    /// against it at load and a misspelling is only visible at run time.
    pub fn fields(self) -> Option<&'static [&'static str]> {
        match self {
            Decoder::Logfmt | Decoder::Json => None,
            Decoder::ApacheCombined => Some(&[
                "host", "ident", "user", "time", "request", "status", "bytes", "referer", "agent",
                "method", "path", "protocol",
            ]),
        }
    }
}

/// One measure. Exactly one of the five is set — and each one IS its own
/// coarsening rule, which is what lets a tape be re-bucketed without a
/// schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MeasureSpec {
    /// One per claimed entry. Takes no field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub count: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// What the number IS — `ms`, `B`, `requests`. Free text, announced
    /// once per run in a `!meta` marker, because the tape outlives the
    /// document that described it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Histogram {
    pub field: String,
    /// Upper bounds, in the units of the field. ⚠ Buckets whose scale
    /// does not match the field's produce a well-formed histogram that
    /// means nothing, which is why `unit` is worth stating.
    pub buckets: Vec<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

// ------------------------------------------------------------- compiling

/// One metric, ready to run: predicates compiled, regex compiled, the
/// measures resolved into the fields they read.
pub struct Live {
    pub metric: String,
    pub unit: Option<String>,
    claim: Option<crate::grep::Preds>,
    fields: Option<Source>,
    labels: Vec<String>,
    measures: Vec<(Field, Option<String>)>,
    histogram: Option<(String, Vec<f64>)>,
    cite: bool,
    roller: Roller,
    announced: bool,
    /// What this run has seen, which is what `--try` reports.
    pub seen: Seen,
}

#[derive(Default, Clone, Copy)]
pub struct Seen {
    pub claimed: u64,
    pub skipped: u64,
    pub dropped: u64,
    pub observations: u64,
}

enum Source {
    Decode(Decoder),
    Extract(Box<regex::bytes::Regex>),
}

/// A document's name is not a metric's: it is an identifier an operator
/// types and a provisioning's `APPLY` lists, so it takes the character
/// set a follower name does — legal as a directory entry, needing no
/// escaping, and permitting the `-` that `apache-combined` wants.
fn check_doc_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("an extractor has a name");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        bail!("{name:?} is not an extractor name (letters, digits, _ - and .)");
    }
    Ok(())
}

impl Extractor {
    /// Read one from JSON, checking everything that can be checked
    /// without data: the version, the shape, the regexes, and every field
    /// name whose source declares what it can produce.
    pub fn parse(text: &str, from: &str) -> anyhow::Result<Extractor> {
        let doc: Extractor = serde_json::from_str(text)
            .with_context(|| format!("{from} is not a tally extractor document"))?;
        doc.validate(from)?;
        Ok(doc)
    }

    pub fn load(path: &Path) -> anyhow::Result<Extractor> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Extractor::parse(&text, &path.display().to_string())
    }

    fn validate(&self, from: &str) -> anyhow::Result<()> {
        if self.v != EXTRACTOR_VERSION {
            bail!(
                "{from}: this build reads extractor documents at v={EXTRACTOR_VERSION}, \
                 the document says v={}",
                self.v
            );
        }
        check_doc_name(&self.name).with_context(|| format!("{from}: name"))?;
        if self.metrics.is_empty() {
            bail!("{from}: an extractor with no metrics measures nothing");
        }
        if self.window.width_ms == 0 || !self.window.width_ms.is_multiple_of(1000) {
            bail!("{from}: window.width_ms is whole seconds and not zero");
        }
        let mut named: BTreeMap<&str, usize> = BTreeMap::new();
        for (i, m) in self.metrics.iter().enumerate() {
            check_metric(&m.name).with_context(|| format!("{from}: metrics[{i}]"))?;
            if m.name.starts_with(MARKER) {
                bail!("{from}: metrics[{i}]: {MARKER} starts a MARKER, which no metric may name");
            }
            if let Some(first) = named.insert(&m.name, i) {
                bail!(
                    "{from}: metrics[{i}] repeats the name {:?} from metrics[{first}] — \
                     a metric is named once",
                    m.name
                );
            }
            m.validate(from, i)?;
        }
        Ok(())
    }

    /// Compile every metric, in document order.
    pub fn compile(&self, window: Window) -> anyhow::Result<Vec<Live>> {
        self.metrics.iter().map(|m| m.compile(window)).collect()
    }
}

impl Metric {
    fn source(&self) -> anyhow::Result<Option<Source>> {
        let Some(f) = &self.fields else {
            return Ok(None);
        };
        match (f.decode, &f.extract) {
            (Some(d), None) => Ok(Some(Source::Decode(d))),
            (None, Some(re)) => {
                let re = regex::bytes::RegexBuilder::new(re)
                    // ⚠ multi_line, for the same reason `grep`'s
                    // predicates are: an entry may be several lines, and a
                    // `$` that meant end-of-ENTRY in one place and
                    // end-of-line in another is a trap nobody escapes twice.
                    .multi_line(true)
                    .build()
                    .with_context(|| format!("metric {:?}: fields.extract", self.name))?;
                if re.capture_names().flatten().count() == 0 {
                    bail!(
                        "metric {:?}: fields.extract has no NAMED captures — (?P<status>…) is \
                         what becomes a field",
                        self.name
                    );
                }
                Ok(Some(Source::Extract(Box::new(re))))
            }
            (None, None) => bail!(
                "metric {:?}: fields is present but empty — state `decode` or `extract`, \
                 or leave fields out",
                self.name
            ),
            (Some(_), Some(_)) => bail!(
                "metric {:?}: fields states both `decode` and `extract`, which is two \
                 ways of reading one line",
                self.name
            ),
        }
    }

    fn measures(&self) -> anyhow::Result<Vec<(Field, Option<String>)>> {
        let mut out = Vec::new();
        for (i, m) in self.measure.iter().enumerate() {
            let set: Vec<(Field, Option<String>)> = [
                m.count.then_some((Field::Count, None)),
                m.sum.clone().map(|f| (Field::Sum, Some(f))),
                m.min.clone().map(|f| (Field::Min, Some(f))),
                m.max.clone().map(|f| (Field::Max, Some(f))),
                m.last.clone().map(|f| (Field::Last, Some(f))),
            ]
            .into_iter()
            .flatten()
            .collect();
            match set.len() {
                1 => out.push(set.into_iter().next().expect("one")),
                0 => bail!(
                    "metric {:?}: measure[{i}] measures nothing — state `count`, or \
                     `sum`/`min`/`max`/`last` with a field",
                    self.name
                ),
                _ => bail!(
                    "metric {:?}: measure[{i}] states more than one measure — give each \
                     its own entry in the list",
                    self.name
                ),
            }
        }
        Ok(out)
    }

    fn validate(&self, from: &str, i: usize) -> anyhow::Result<()> {
        let ctx = |e: anyhow::Error| anyhow::anyhow!("{from}: metrics[{i}]: {e}");
        let source = self.source().map_err(ctx)?;
        let measures = self.measures().map_err(ctx)?;
        if measures.is_empty() && self.histogram.is_none() {
            bail!(
                "{from}: metrics[{i}]: {:?} measures nothing — state a `measure`, or a \
                 `histogram`",
                self.name
            );
        }
        if let Some(h) = &self.histogram {
            if h.buckets.is_empty() {
                bail!("{from}: metrics[{i}]: histogram.buckets is empty");
            }
            if h.buckets.iter().any(|b| !b.is_finite()) {
                bail!(
                    "{from}: metrics[{i}]: histogram.buckets must be finite — the +Inf \
                     bucket is always emitted and is not written down"
                );
            }
        }
        for l in &self.labels {
            if Field::parse(l).is_some() {
                bail!(
                    "{from}: metrics[{i}]: labels with {l:?}, which is a measure name — \
                     dispatch on a line is by key, so the two namespaces cannot overlap"
                );
            }
        }
        // Every field this metric refers to, against what its source can
        // produce — where the source declares that at all.
        let named: Vec<(&str, &str)> = measures
            .iter()
            .filter_map(|(_, f)| f.as_deref().map(|f| ("measures", f)))
            .chain(
                self.histogram
                    .iter()
                    .map(|h| ("observes", h.field.as_str())),
            )
            .chain(self.labels.iter().map(|l| ("labels with", l.as_str())))
            .collect();
        match &source {
            None => {
                if let Some((what, f)) = named.first() {
                    bail!(
                        "{from}: metrics[{i}]: {} {f:?} but states no `fields` to read it \
                         from",
                        what
                    );
                }
            }
            Some(src) => {
                let known: Option<Vec<String>> = match src {
                    Source::Extract(re) => {
                        Some(re.capture_names().flatten().map(str::to_string).collect())
                    }
                    Source::Decode(d) => d
                        .fields()
                        .map(|f| f.iter().map(|s| s.to_string()).collect()),
                };
                if let Some(known) = known {
                    for (what, f) in &named {
                        if !known.iter().any(|k| k == f) {
                            bail!(
                                "{from}: metrics[{i}]: {what} {f:?}, which this metric's \
                                 source cannot produce — it has {}",
                                known.join(", ")
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn compile(&self, window: Window) -> anyhow::Result<Live> {
        let claim = match &self.claim {
            None => None,
            Some(c) => {
                let spec = crate::grep::PredSpec {
                    all: compile_preds(&c.all)?,
                    any: compile_preds(&c.any)?,
                    none: compile_preds(&c.none)?,
                };
                if spec.is_empty() {
                    None
                } else {
                    Some(crate::grep::Preds::compile(spec)?)
                }
            }
        };
        Ok(Live {
            metric: self.name.clone(),
            unit: self
                .histogram
                .as_ref()
                .and_then(|h| h.unit.clone())
                .or_else(|| self.measure.iter().find_map(|m| m.unit.clone())),
            claim,
            fields: self.source()?,
            labels: self.labels.clone(),
            measures: self.measures()?,
            histogram: self
                .histogram
                .as_ref()
                .map(|h| (h.field.clone(), sorted(&h.buckets))),
            cite: self.cite.unwrap_or(true),
            roller: Roller::new(
                window.width_ms,
                window.grace_ms,
                window.revise_ms,
                window.max_series,
            ),
            announced: false,
            seen: Seen::default(),
        })
    }
}

fn sorted(b: &[f64]) -> Vec<f64> {
    let mut v = b.to_vec();
    v.sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));
    v
}

fn compile_preds(list: &[crate::querydoc::Predicate]) -> anyhow::Result<Vec<crate::grep::Pred>> {
    list.iter().map(|p| p.compile()).collect()
}

// ------------------------------------------------------------- extraction

/// One entry's fields, as whatever source the metric declared reads them.
///
/// `None` means the line is NOT OF THIS SHAPE — an ordinary event on a
/// store carrying many shapes, and not a loss. An empty map means the
/// shape fit and carried nothing.
fn decode(d: Decoder, entry: &[u8]) -> Option<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    match d {
        Decoder::Logfmt => {
            let text = String::from_utf8_lossy(entry);
            let line = text.lines().next().unwrap_or("");
            let toks = tokenize(line).ok()?;
            for t in toks {
                if let Some((k, v)) = t.split_once('=') {
                    out.insert(k.to_string(), v.to_string());
                }
            }
            // No pair at all is not a logfmt line.
            if out.is_empty() {
                return None;
            }
        }
        Decoder::Json => {
            let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(entry) else {
                return None;
            };
            for (k, v) in map {
                let s = match v {
                    Value::String(s) => s,
                    Value::Null | Value::Array(_) | Value::Object(_) => continue,
                    other => other.to_string(),
                };
                out.insert(k, s);
            }
        }
        Decoder::ApacheCombined => {
            let text = String::from_utf8_lossy(entry);
            let line = text.lines().next().unwrap_or("");
            let c = apache_re().captures(line)?;
            // ⚠ `-` means ABSENT for an identity and ZERO for a count:
            // CLF writes it for %b when no body was sent, so reading it
            // as absent turns every empty response into an unreadable
            // entry and a `!drop` that says the numbers are wrong.
            let mut put = |k: &str, i: usize| {
                if let Some(m) = c.get(i) {
                    match (m.as_str(), k) {
                        ("-", "bytes") => {
                            out.insert(k.to_string(), "0".to_string());
                        }
                        ("-", _) => {}
                        (v, _) => {
                            out.insert(k.to_string(), v.to_string());
                        }
                    }
                }
            };
            put("host", 1);
            put("ident", 2);
            put("user", 3);
            put("time", 4);
            put("request", 5);
            put("status", 6);
            put("bytes", 7);
            put("referer", 8);
            put("agent", 9);
            if let Some(req) = out.get("request").cloned() {
                let mut parts = req.split(' ');
                if let Some(m) = parts.next() {
                    out.insert("method".to_string(), m.to_string());
                }
                if let Some(p) = parts.next() {
                    out.insert("path".to_string(), p.to_string());
                }
                if let Some(p) = parts.next() {
                    out.insert("protocol".to_string(), p.to_string());
                }
            }
        }
    }
    Some(out)
}

/// NCSA common and combined, which differ only by the last two fields —
/// hence one pattern with them optional. Anything else a producer emits
/// is `extract`'s job, deliberately: Apache's `LogFormat` is
/// configurable, so "the Apache format" is not a thing a decoder can
/// claim to know.
fn apache_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"^(\S+) (\S+) (\S+) \[([^\]]*)\] "([^"]*)" (\S+) (\S+)(?: "([^"]*)" "([^"]*)")?"#,
        )
        .expect("the apache pattern compiles")
    })
}

pub enum Outcome {
    /// Not this metric's line: the claim did not select it, or it is not
    /// of the shape the source reads. ⚠ NOT a loss, and not counted as
    /// one — on a store carrying many shapes this is the ordinary case,
    /// and a counter that conflated it with a real failure would be noise
    /// exactly where signal is wanted.
    Skipped,
    Observed(Vec<Sample>),
    /// Claimed, of the right shape, and then unreadable. A real defect:
    /// the number is now wrong rather than merely absent.
    Dropped(&'static str),
}

impl Live {
    pub fn observe(&self, entry: &[u8], ts: u64, cite: Option<(u64, u64)>) -> Outcome {
        if let Some(preds) = &self.claim {
            if !preds.keep(entry) {
                return Outcome::Skipped;
            }
        }
        let fields = match &self.fields {
            None => BTreeMap::new(),
            Some(Source::Decode(d)) => match decode(*d, entry) {
                Some(f) => f,
                None => return Outcome::Skipped,
            },
            Some(Source::Extract(re)) => {
                let Some(c) = re.captures(entry) else {
                    return Outcome::Skipped;
                };
                let mut out = BTreeMap::new();
                for name in re.capture_names().flatten() {
                    if let Some(m) = c.name(name) {
                        out.insert(
                            name.to_string(),
                            String::from_utf8_lossy(m.as_bytes()).into(),
                        );
                    }
                }
                out
            }
        };

        // An absent label is OMITTED, which reads as the empty string in
        // every selector — the rule the whole tree already follows.
        let labels: Vec<(String, String)> = self
            .labels
            .iter()
            .filter_map(|k| fields.get(k).map(|v| (k.clone(), v.clone())))
            .collect();

        let num =
            |key: &str| -> Option<f64> { fields.get(key).and_then(|v| v.parse::<f64>().ok()) };
        let cite = if self.cite { cite } else { None };

        let mut out = Vec::new();
        if !self.measures.is_empty() {
            let mut s = Sample::new(ts, 0, &self.metric);
            s.labels = labels.clone();
            s.labels.sort();
            s.cite = cite;
            for (f, from) in &self.measures {
                let v = match from {
                    None => 1.0,
                    Some(key) => match num(key) {
                        Some(v) => v,
                        None => return Outcome::Dropped("unreadable"),
                    },
                };
                s = s.field(*f, v);
            }
            out.push(s);
        }

        if let Some((key, bounds)) = &self.histogram {
            let Some(v) = num(key) else {
                return Outcome::Dropped("unreadable");
            };
            // Cumulative, as Prometheus spells it: every bucket at or
            // above the value counts it, so coarsening stays addition and
            // a quantile is interpolation over sums.
            for b in bounds {
                if v <= *b {
                    let mut s = Sample::new(ts, 0, &self.metric);
                    s.labels = labels.clone();
                    s.labels.push(("le".to_string(), number(*b)));
                    s.labels.sort();
                    s.cite = cite;
                    out.push(s.field(Field::Count, 1.0));
                }
            }
            let mut inf = Sample::new(ts, 0, &self.metric);
            inf.labels = labels;
            inf.labels.push(("le".to_string(), "+Inf".to_string()));
            inf.labels.sort();
            inf.cite = cite;
            // The +Inf bucket carries the total, so an average needs no
            // second metric.
            out.push(inf.field(Field::Count, 1.0).field(Field::Sum, v));
        }

        Outcome::Observed(out)
    }
}

// -------------------------------------------------------------- the command

pub struct TallyOpts {
    /// Extractor documents, repeatable. A directory takes every `*.json`
    /// in it.
    pub extractors: Vec<PathBuf>,
    /// Validate and apply to PLAIN LOG LINES on stdin rather than a
    /// records stream, reporting per metric on stderr.
    ///
    /// The tally lines still go to stdout alone, which is what makes a
    /// `--try` run diffable against a golden file — and therefore what
    /// tests every extractor this repository ships.
    pub try_it: bool,
    /// Validate and report, reading nothing.
    pub check: bool,
    pub fold: Option<FoldOpts>,
    /// Print the width-`0s` observations instead of bucketing them.
    pub observations: bool,
    /// Only these metrics, for recomputing one over history.
    pub metrics: Vec<String>,
    /// Override the extractors' own window.
    pub width_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct FoldOpts {
    pub width_ms: u64,
    pub grace_ms: u64,
    pub revise_ms: u64,
    pub max_series: usize,
}

/// Bucket observation lines from stdin. The metric, labels and measures
/// are the input's; only the window is this side's.
pub fn cmd_fold(o: &FoldOpts) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    fold_stream(o, stdin.lock(), &mut out)?;
    out.flush()?;
    Ok(())
}

/// ⚠ A line that does not parse is FATAL, not skipped. A fold that
/// quietly dropped a tenth of its input would report numbers that are
/// wrong rather than missing, and nothing downstream could tell.
pub fn fold_stream(
    o: &FoldOpts,
    input: impl std::io::BufRead,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let mut roller = Roller::new(o.width_ms, o.grace_ms, o.revise_ms, o.max_series);
    for (n, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let s = Sample::parse(&line).with_context(|| format!("stdin line {}", n + 1))?;
        if s.width_ms != 0 {
            bail!(
                "stdin line {}: width {} — `--fold` takes OBSERVATIONS (width 0s). \
                 Re-bucketing an existing tape is the same operation and will be its \
                 own flag.",
                n + 1,
                render_width(s.width_ms)
            );
        }
        roller.add(&s);
        emit(out, roller.drain(false))?;
    }
    emit(out, roller.drain(true))?;
    Ok(())
}

/// The prefix reserved for what this package ships.
///
/// Names live in one flat namespace and a collision is refused, so
/// without a reserved prefix a site writing its own `apache-combined`
/// would find ours in the way — and a name we add in a later release
/// could break a deployment that was working. The promise is one-sided
/// and enforced by a test: everything shipped here carries the prefix,
/// and nothing else does.
pub const SHIPPED_PREFIX: &str = "timberfs-";

/// Every extractor named, with duplicate names refused across files.
///
/// ⚠ A directory given LATER SHADOWS an earlier one by FILE NAME, which
/// is what makes `--extractor /usr/lib/… --extractor /etc/…` mean "the
/// packaged set, with the site's edits winning". Shadowing is by
/// filename and refusal is by declared NAME, deliberately: forking a
/// shipped document means keeping its filename, while two unrelated
/// documents claiming one name is an ambiguity nobody should resolve by
/// readdir order.
pub fn load_extractors(paths: &[PathBuf]) -> anyhow::Result<Vec<(PathBuf, Extractor)>> {
    let mut by_file: BTreeMap<std::ffi::OsString, PathBuf> = BTreeMap::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(p)
                .with_context(|| format!("reading {}", p.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            found.sort();
            if found.is_empty() {
                bail!("no *.json under {}", p.display());
            }
            for f in found {
                if let Some(name) = f.file_name() {
                    by_file.insert(name.to_os_string(), f);
                }
            }
        } else {
            // Named outright rather than swept: taken as given, and never
            // shadowed by a directory.
            files.push(p.clone());
        }
    }
    files.extend(by_file.into_values());
    let mut out: Vec<(PathBuf, Extractor)> = Vec::new();
    for f in files {
        let doc = Extractor::load(&f)?;
        // Refused rather than merged or last-one-wins: which definition a
        // number came from must not depend on readdir order.
        if let Some((other, _)) = out.iter().find(|(_, d)| d.name == doc.name) {
            bail!(
                "{}: the extractor named {:?} is also defined by {} — a name is claimed \
                 once",
                f.display(),
                doc.name,
                other.display()
            );
        }
        out.push((f, doc));
    }
    Ok(out)
}

/// One run: every metric of every extractor, fed entries.
struct Run {
    live: Vec<Live>,
}

impl Run {
    fn new(docs: &[(PathBuf, Extractor)], opts: &TallyOpts) -> anyhow::Result<Run> {
        let mut live = Vec::new();
        for (path, doc) in docs {
            let mut window = doc.window;
            if let Some(w) = opts.width_ms {
                window.width_ms = w;
            }
            for m in doc
                .compile(window)
                .with_context(|| path.display().to_string())?
            {
                if opts.metrics.is_empty() || opts.metrics.contains(&m.metric) {
                    live.push(m);
                }
            }
        }
        for want in &opts.metrics {
            if !live.iter().any(|l| l.metric == *want) {
                bail!("no metric named {want:?} in the extractors given");
            }
        }
        if live.is_empty() {
            bail!("the extractors given define no metrics");
        }
        Ok(Run { live })
    }

    fn feed(
        &mut self,
        e: &crate::records::EntryRec,
        axis: Axis,
        out: &mut impl Write,
        observations: bool,
    ) -> anyhow::Result<()> {
        let mut batch: Vec<Sample> = Vec::new();
        for l in self.live.iter_mut() {
            let ts = match axis {
                Axis::Logline => e.ts,
                // The chunk's first write is the best arrival stamp an
                // entry carries, so a write-axis bucket is only as fine
                // as a chunk.
                Axis::Write => e.wf,
            };
            let cite = e.offset.map(|off| (off, e.payload.len() as u64));
            let Some(ts) = ts else {
                l.seen.dropped += 1;
                note_drop(&mut l.roller, &l.metric, "nostamp", None);
                continue;
            };
            match l.observe(&e.payload, ts, cite) {
                Outcome::Skipped => l.seen.skipped += 1,
                Outcome::Dropped(why) => {
                    l.seen.dropped += 1;
                    note_drop(&mut l.roller, &l.metric, why, Some(ts));
                }
                Outcome::Observed(obs) => {
                    l.seen.claimed += 1;
                    l.seen.observations += obs.len() as u64;
                    // The unit is announced ONCE per run rather than on
                    // every line: the tape outlives the document that
                    // described it, and a number whose unit is unknown is
                    // a number nobody can act on.
                    if !l.announced {
                        l.announced = true;
                        if let Some(u) = &l.unit {
                            writeln!(
                                out,
                                "{}",
                                Sample::new(ts, 0, "!meta")
                                    .label("metric", &l.metric)
                                    .label("unit", u)
                                    .render()
                            )?;
                        }
                    }
                    for o in &obs {
                        if observations {
                            writeln!(out, "{}", o.render())?;
                        } else {
                            l.roller.add(o);
                        }
                    }
                }
            }
            if !observations {
                batch.extend(l.roller.drain(false));
            }
        }
        emit(out, batch)
    }

    fn finish(&mut self, out: &mut impl Write, observations: bool) -> anyhow::Result<()> {
        if observations {
            return Ok(());
        }
        let mut batch: Vec<Sample> = Vec::new();
        for l in self.live.iter_mut() {
            batch.extend(l.roller.drain(true));
        }
        emit(out, batch)
    }
}

pub fn cmd_tally(opts: &TallyOpts) -> anyhow::Result<()> {
    if let Some(fold) = &opts.fold {
        return cmd_fold(fold);
    }
    let docs = load_extractors(&opts.extractors)?;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    if opts.check {
        for (path, doc) in &docs {
            eprintln!(
                "{} — {} ({} metric(s), window {} {:?}, grace {})",
                doc.name,
                path.display(),
                doc.metrics.len(),
                render_width(doc.window.width_ms),
                doc.window.axis,
                render_width(doc.window.grace_ms),
            );
            // Compiling is the check: predicates, regexes and every field
            // name whose source declares what it can produce.
            doc.compile(doc.window)?;
            for m in &doc.metrics {
                eprintln!(
                    "  {:<24} {}",
                    m.name,
                    m.description.as_deref().unwrap_or("")
                );
            }
        }
        return Ok(());
    }

    let axis = axis_of(&docs)?;
    let mut run = Run::new(&docs, opts)?;

    if opts.try_it {
        let mut text = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut text)?;
        let tried = try_text(&docs, opts, &text)?;
        eprintln!(
            "{} entr{} from {} byte(s) of plain text",
            tried.entries,
            if tried.entries == 1 { "y" } else { "ies" },
            text.len()
        );
        out.write_all(tried.tally.as_bytes())?;
        out.flush()?;
        for (metric, s) in &tried.seen {
            eprintln!(
                "  {metric:<24} claimed {}, skipped {}, dropped {} -> {} observation(s)",
                s.claimed, s.skipped, s.dropped, s.observations
            );
        }
        return Ok(());
    }

    let stdin = std::io::stdin();
    let mut reader = crate::records::Reader::new(stdin.lock());
    let mut ended = false;
    while let Some(rec) = reader.next_rec()? {
        match rec {
            crate::records::Rec::Entry(e) => run.feed(&e, axis, &mut out, opts.observations)?,
            crate::records::Rec::End(_) => ended = true,
            _ => {}
        }
    }
    run.finish(&mut out, opts.observations)?;
    out.flush()?;
    if !ended {
        bail!("the record stream ended without stream-end — the answer is truncated");
    }
    Ok(())
}

/// What a `--try` produced: the tally lines, and what each metric made
/// of the input.
pub struct Tried {
    pub entries: usize,
    pub tally: String,
    pub seen: Vec<(String, Seen)>,
}

/// Apply extractors to PLAIN LOG LINES, for trying one against a real
/// file — and for testing the extractors this repository ships, which is
/// the same operation.
pub fn try_text(
    docs: &[(PathBuf, Extractor)],
    opts: &TallyOpts,
    text: &[u8],
) -> anyhow::Result<Tried> {
    let axis = axis_of(docs)?;
    if axis == Axis::Write {
        bail!(
            "--try reads plain log lines, which carry no arrival time — an extractor \
             with window.axis=write cannot be tried against a file, only against a store"
        );
    }
    let mut run = Run::new(docs, opts)?;
    let entries = entries_of_text(text)?;
    let mut tally: Vec<u8> = Vec::new();
    for e in &entries {
        run.feed(e, axis, &mut tally, opts.observations)?;
    }
    run.finish(&mut tally, opts.observations)?;
    Ok(Tried {
        entries: entries.len(),
        tally: String::from_utf8_lossy(&tally).into_owned(),
        seen: run
            .live
            .iter()
            .map(|l| (l.metric.clone(), l.seen))
            .collect(),
    })
}

/// Every extractor in one run must agree on the axis: they share one
/// entry stream, and an entry cannot be on two clocks at once.
fn axis_of(docs: &[(PathBuf, Extractor)]) -> anyhow::Result<Axis> {
    let mut it = docs.iter();
    let (first_path, first) = it.next().expect("load_extractors refuses an empty set");
    for (path, doc) in it {
        if doc.window.axis != first.window.axis {
            bail!(
                "{} is on the {:?} axis and {} on the {:?} — one run reads one entry \
                 stream, so its extractors must agree",
                first_path.display(),
                first.window.axis,
                path.display(),
                doc.window.axis
            );
        }
    }
    Ok(first.window.axis)
}

/// Plain text into the SAME entries a store would yield: the real
/// assembly, so a `--try` run and a live run cannot disagree about where
/// one entry ends and the next begins.
fn entries_of_text(text: &[u8]) -> anyhow::Result<Vec<crate::records::EntryRec>> {
    let extractor = crate::import::Extractor::new(None, None, false)?;
    let mut sink = crate::entry::EntrySink::new(
        extractor,
        None,
        crate::entry::Framing {
            null_sep: false,
            records: true,
            show_write: false,
            label: None,
            store_id: None,
        },
        None,
        "-",
    );
    let mut framed: Vec<u8> = Vec::new();
    sink.push_chunk(text, None, (0, 0), 0, &mut framed)?;
    sink.finish(&mut framed)?;
    // The sink writes ENTRIES; a stream's brackets belong to whoever is
    // producing the answer, and here that is this function. Without the
    // marker the reader below correctly calls its own input truncated.
    framed.extend_from_slice(b"\x1estream-end\0");
    let mut reader = crate::records::Reader::new(std::io::Cursor::new(framed));
    let mut out = Vec::new();
    while let Some(rec) = reader.next_rec()? {
        if let crate::records::Rec::Entry(e) = rec {
            out.push(e);
        }
    }
    Ok(out)
}

/// One batch of sealed buckets, in time order.
fn emit(out: &mut impl Write, mut batch: Vec<Sample>) -> anyhow::Result<()> {
    batch.sort_by(|a, b| (a.ts, &a.metric, &a.labels).cmp(&(b.ts, &b.metric, &b.labels)));
    for s in batch {
        writeln!(out, "{}", s.render())?;
    }
    Ok(())
}

/// ⚠ Stamped with the ENTRY's own time where there is one. Charging it
/// to the watermark put every drop at the epoch until some other metric
/// had produced an observation — and a metric whose every entry drops has
/// no watermark of its own, which is exactly the case worth reading.
fn note_drop(roller: &mut Roller, metric: &str, why: &'static str, at: Option<u64>) {
    let ts = at.unwrap_or_else(|| roller.watermark());
    let s = Sample::new(ts, 0, "!drop")
        .label("metric", metric)
        .label("reason", why)
        .field(Field::Count, 1.0);
    roller.add(&s);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(line: &str) -> Sample {
        Sample::parse(line).expect(line)
    }

    #[test]
    fn a_line_round_trips_through_its_own_parser() {
        // The format is a wire format the moment somebody's own
        // extractor pipes one into `--fold`, so render and parse must be
        // one grammar rather than two.
        for line in [
            "2026-09-06T13:37:00.000Z 60s http_requests status=500 vhost=example.com sum=42 @1994848392+51221",
            "2026-09-06T13:37:00.000Z 0s heap_used count=1 last=8419221.5",
            "2026-09-06T13:38:00.000Z 60s !gap chunks=4200..4830 reason=follower-gap",
            "2026-09-06T13:38:00.000Z 60s http_latency agent=\"Mozilla 5.0\" le=+Inf count=3 sum=1.25",
        ] {
            assert_eq!(s(line).render(), line, "round trip of {line}");
        }
    }

    #[test]
    fn labels_and_fields_render_in_one_canonical_order() {
        // Two spellings of one series must be one run of bytes: it is
        // what zstd rewards and what makes a revision comparable without
        // parsing it.
        let a = s("2026-09-06T13:37:00.000Z 60s m b=2 a=1 sum=1 count=2");
        let b = s("2026-09-06T13:37:00.000Z 60s m a=1 b=2 count=2 sum=1");
        assert_eq!(a.render(), b.render());
        assert_eq!(
            a.render(),
            "2026-09-06T13:37:00.000Z 60s m a=1 b=2 count=2 sum=1"
        );
    }

    #[test]
    fn a_sample_states_a_measure_but_a_marker_need_not() {
        assert!(Sample::parse("2026-09-06T13:37:00.000Z 60s m a=1").is_err());
        assert!(Sample::parse("2026-09-06T13:37:00.000Z 60s !gap reason=x").is_ok());
    }

    #[test]
    fn a_metric_may_not_be_named_like_a_marker_or_a_measure() {
        assert!(check_metric("!gap").is_ok());
        assert!(check_metric("http_requests").is_ok());
        assert!(check_metric("9lives").is_err());
        assert!(check_metric("http-requests").is_err());
        // A label named after a measure would make the line ambiguous:
        // dispatch is on the key, so the two namespaces cannot overlap.
        let set = parsed(
            r#"{"name":"m","fields":{"decode":"logfmt"},
                "labels":["sum"],"measure":[{"count":true}]}"#,
        );
        assert!(set.is_err(), "a label may not be called sum");
    }

    #[test]
    fn observations_fold_into_buckets_by_their_own_field_rule() {
        let obs = [
            s("2026-09-06T13:37:10.000Z 0s m count=1 sum=5 min=5 max=5 last=5"),
            s("2026-09-06T13:37:20.000Z 0s m count=1 sum=3 min=3 max=3 last=3"),
            s("2026-09-06T13:37:50.000Z 0s m count=1 sum=9 min=9 max=9 last=9"),
        ];
        let out = Roller::fold(60_000, &obs);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].render(),
            "2026-09-06T13:37:00.000Z 60s m count=3 sum=17 min=3 max=9 last=9"
        );
    }

    #[test]
    fn coarsening_a_tape_is_the_same_fold_as_bucketing_it() {
        // The invariant the whole format rests on: a wider bucket is the
        // sum of the narrower ones, so a reader can re-bucket and an
        // extractor need not be asked twice.
        let obs: Vec<Sample> = (0..300)
            .map(|i| {
                let ts = 1_788_700_000_000u64 + i * 1000;
                Sample::new(ts, 0, "m")
                    .field(Field::Count, 1.0)
                    .field(Field::Sum, i as f64)
                    .field(Field::Max, i as f64)
            })
            .collect();
        let minutes = Roller::fold(60_000, &obs);
        let five_direct = Roller::fold(300_000, &obs);
        let five_from_minutes = Roller::fold(300_000, &minutes);
        assert_eq!(
            five_direct.iter().map(|s| s.render()).collect::<Vec<_>>(),
            five_from_minutes
                .iter()
                .map(|s| s.render())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_late_entry_revises_the_bucket_it_belongs_to() {
        // The reason the tape is append-only and the reader resolves:
        // a stamp can arrive after its bucket sealed (a request logged
        // at its START and written at completion), and the answer is a
        // new complete line, not a delta.
        let mut r = Roller::new(60_000, 30_000, 3_600_000, 1000);
        r.add(&s("2026-09-06T13:37:10.000Z 0s m count=1"));
        assert!(
            r.drain(false).is_empty(),
            "not sealed while the grace stands"
        );
        r.add(&s("2026-09-06T13:39:00.000Z 0s m count=1"));
        let sealed = r.drain(false);
        assert_eq!(sealed.len(), 1);
        assert_eq!(sealed[0].render(), "2026-09-06T13:37:00.000Z 60s m count=1");

        r.add(&s("2026-09-06T13:37:30.000Z 0s m count=1"));
        let revised = r.drain(false);
        assert_eq!(revised.len(), 1);
        assert_eq!(
            revised[0].render(),
            "2026-09-06T13:37:00.000Z 60s m count=2"
        );

        // …and the newest line wins, which is also what makes a
        // recompute idempotent rather than doubling.
        let resolved = resolve(vec![sealed[0].clone(), revised[0].clone()]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].render(), revised[0].render());
    }

    #[test]
    fn a_bucket_holds_the_oldest_byte_it_still_depends_on() {
        // What a consumer's watermark may not pass: a bucket that can
        // still be revised must be reconstructible after a restart.
        let mut r = Roller::new(60_000, 0, 0, 1000);
        let mut a = s("2026-09-06T13:37:10.000Z 0s m count=1");
        a.cite = Some((4096, 100));
        let mut b = s("2026-09-06T13:37:20.000Z 0s m count=1");
        b.cite = Some((512, 100));
        r.add(&a);
        r.add(&b);
        assert_eq!(r.safe_offset(), Some(512));
        assert_eq!(
            r.drain(true)[0].render(),
            "2026-09-06T13:37:00.000Z 60s m count=2 @512+3684"
        );
        assert_eq!(r.safe_offset(), None, "nothing held, nothing to hold back");
    }

    #[test]
    fn the_series_cap_is_recorded_rather_than_silent() {
        let mut r = Roller::new(60_000, 0, 0, 2);
        for i in 0..5 {
            r.add(
                &Sample::new(1_788_700_000_000, 0, "m")
                    .label("id", &i.to_string())
                    .field(Field::Count, 1.0),
            );
        }
        let out = r.drain(true);
        let cap: Vec<&Sample> = out.iter().filter(|s| s.metric == "!cap").collect();
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].fields, vec![(Field::Count, 3.0)]);
    }

    fn doc(metrics: &str) -> String {
        format!(
            r#"{{"v":"{EXTRACTOR_VERSION}","name":"t",
                "window":{{"axis":"logline","width_ms":60000}},
                "metrics":[{metrics}]}}"#
        )
    }

    fn parsed(metrics: &str) -> anyhow::Result<Extractor> {
        Extractor::parse(&doc(metrics), "t.json")
    }

    fn live(metrics: &str) -> Live {
        let d = parsed(metrics).expect("a valid document");
        d.compile(d.window).expect("compiles").pop().expect("one")
    }

    fn observed(l: &Live, line: &str, ts: u64) -> Vec<Sample> {
        match l.observe(line.as_bytes(), ts, None) {
            Outcome::Observed(v) => v,
            Outcome::Skipped => Vec::new(),
            Outcome::Dropped(why) => panic!("dropped: {why}"),
        }
    }

    #[test]
    fn a_document_states_its_version_and_measures_something() {
        assert!(parsed(r#"{"name":"m","measure":[{"count":true}]}"#).is_ok());

        // An unknown member is an error, not a shrug: a request that
        // tolerates a typo does something other than what was asked.
        let err = Extractor::parse(
            &doc(r#"{"name":"m","measure":[{"count":true}],"labls":["x"]}"#),
            "t.json",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("labls"), "{err:#}");

        let wrong = doc(r#"{"name":"m","measure":[{"count":true}]}"#).replace("1.0-", "9.9-");
        let err = Extractor::parse(&wrong, "t.json").unwrap_err();
        assert!(format!("{err}").contains("v="), "{err}");

        let err = parsed(r#"{"name":"m"}"#).unwrap_err();
        assert!(format!("{err}").contains("measures nothing"), "{err}");
    }

    #[test]
    fn a_metric_names_a_field_only_where_its_source_can_produce_one() {
        // No source at all, and a measure that names a field.
        let err = parsed(r#"{"name":"m","measure":[{"sum":"bytes"}]}"#).unwrap_err();
        assert!(format!("{err}").contains("no `fields`"), "{err}");

        // An extract whose captures do not include it.
        let err = parsed(
            r#"{"name":"m","fields":{"extract":"level=(?P<level>\\w+)"},
                "measure":[{"sum":"ms"}]}"#,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("cannot produce"), "{err}");

        // A positional decoder, whose field set is its own.
        let err = parsed(
            r#"{"name":"m","fields":{"decode":"apache-combined"},
                "labels":["vhost"],"measure":[{"count":true}]}"#,
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("vhost") && text.contains("referer"), "{text}");

        // …and logfmt takes its names from the PRODUCER, so there is
        // nothing to check against and anything is accepted.
        assert!(parsed(
            r#"{"name":"m","fields":{"decode":"logfmt"},
                "labels":["anything"],"measure":[{"sum":"whatever"}]}"#
        )
        .is_ok());
    }

    #[test]
    fn one_source_of_fields_and_one_measure_per_entry() {
        let err = parsed(
            r#"{"name":"m","fields":{"decode":"logfmt","extract":"(?P<a>x)"},
                "measure":[{"count":true}]}"#,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("two"), "{err}");

        let err = parsed(r#"{"name":"m","measure":[{"count":true,"sum":"x"}]}"#).unwrap_err();
        assert!(format!("{err}").contains("more than one"), "{err}");

        let err = parsed(r#"{"name":"m","measure":[{}]}"#).unwrap_err();
        assert!(format!("{err}").contains("measures nothing"), "{err}");
    }

    #[test]
    fn a_line_of_another_shape_is_skipped_and_never_counted_as_a_loss() {
        // The correction that matters on a real store: it carries many
        // line shapes at once, so "not mine" is the ORDINARY case. A
        // counter that conflated it with a real failure would be noise
        // exactly where signal is wanted.
        let l = live(
            r#"{"name":"m","fields":{"decode":"logfmt"},
                "labels":["level"],"measure":[{"count":true}]}"#,
        );
        assert!(matches!(
            l.observe(b"\tat com.example.Thing.method(Thing.java:42)", 1000, None),
            Outcome::Skipped
        ));
        assert!(matches!(
            l.observe(b"level=warn ms=3", 1000, None),
            Outcome::Observed(_)
        ));

        // Claimed, of the right shape, and then unreadable IS a loss.
        let l = live(r#"{"name":"m","fields":{"decode":"logfmt"},"measure":[{"sum":"ms"}]}"#);
        match l.observe(b"ms=notanumber", 1000, None) {
            Outcome::Dropped(why) => assert_eq!(why, "unreadable"),
            _ => panic!("a measure that cannot be read is a drop, not a zero"),
        }
    }

    #[test]
    fn a_claim_says_which_lines_are_mine() {
        let l =
            live(r#"{"name":"m","claim":{"all":[{"has":"ERROR"}]},"measure":[{"count":true}]}"#);
        assert!(matches!(
            l.observe(b"all is well", 1000, None),
            Outcome::Skipped
        ));
        assert!(matches!(
            l.observe(b"ERROR the thing", 1000, None),
            Outcome::Observed(_)
        ));
    }

    #[test]
    fn decoders_produce_the_fields_a_metric_names() {
        let l = live(
            r#"{"name":"m","fields":{"decode":"logfmt"},
                "labels":["level"],"measure":[{"sum":"ms"}]}"#,
        );
        assert_eq!(
            observed(&l, "level=warn ms=42 msg=\"a thing happened\"", 1000)[0].render(),
            "1970-01-01T00:00:01.000Z 0s m level=warn sum=42"
        );

        let l = live(
            r#"{"name":"m","fields":{"decode":"apache-combined"},
                "labels":["status","method"],"measure":[{"sum":"bytes"}]}"#,
        );
        let line =
            r#"10.0.0.1 - - [06/Sep/2026:13:37:00 +0200] "GET /x HTTP/1.1" 200 5120 "-" "curl/8""#;
        assert_eq!(
            observed(&l, line, 1000)[0].render(),
            "1970-01-01T00:00:01.000Z 0s m method=GET status=200 sum=5120"
        );
    }

    #[test]
    fn an_absent_label_is_omitted_and_reads_as_empty() {
        // The rule the selector already follows, one level down: an
        // absent key is the empty string, so a series with no `status`
        // is selectable by `status=`.
        let l = live(
            r#"{"name":"m","fields":{"decode":"logfmt"},
                "labels":["status"],"measure":[{"count":true}]}"#,
        );
        assert_eq!(
            observed(&l, "msg=hello", 1000)[0].render(),
            "1970-01-01T00:00:01.000Z 0s m count=1"
        );
    }

    #[test]
    fn a_histogram_is_cumulative_and_carries_its_total_on_inf() {
        let l = live(
            r#"{"name":"m","fields":{"decode":"logfmt"},
                "histogram":{"field":"ms","unit":"ms","buckets":[10,100]}}"#,
        );
        let out = observed(&l, "ms=40", 1000);
        assert_eq!(
            out.iter().map(|s| s.render()).collect::<Vec<_>>(),
            vec![
                "1970-01-01T00:00:01.000Z 0s m le=100 count=1",
                "1970-01-01T00:00:01.000Z 0s m le=+Inf count=1 sum=40",
            ]
        );
        // Summable, therefore re-bucketable: the +Inf count is the
        // number of observations and each `le` is a prefix of it.
        assert_eq!(Roller::fold(60_000, &out).len(), 2);
    }

    #[test]
    fn an_extractor_name_is_claimed_once_across_the_files_given() {
        let dir = tempdir();
        for f in ["a.json", "b.json"] {
            std::fs::write(
                dir.join(f),
                doc(r#"{"name":"m","measure":[{"count":true}]}"#),
            )
            .unwrap();
        }
        let err = load_extractors(std::slice::from_ref(&dir)).unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("a.json") && text.contains("b.json"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plain_text_yields_the_same_entries_a_store_would() {
        // What `--try` rests on: a stack trace is ONE entry, so a claim
        // written against a real store behaves the same against a file.
        let text = b"2026-09-06T10:00:00Z ERROR boom\n\tat a.b.C(C.java:1)\n\tat d.e.F(F.java:2)\n\
                     2026-09-06T10:00:01Z INFO fine\n";
        let entries = entries_of_text(text).unwrap();
        assert_eq!(entries.len(), 2, "a stack trace is not three entries");
        assert!(entries[0].payload.ends_with(b"F.java:2)\n"));
        assert!(entries[0].ts.is_some(), "stamped from its own line");
    }

    #[test]
    fn a_drop_is_charged_to_the_entry_that_caused_it() {
        // It was charged to the WATERMARK, which is zero until some
        // observation lands — so a metric whose every entry drops put all
        // of them at the epoch, which is exactly the one worth reading.
        let mut r = Roller::new(60_000, 0, 0, 1000);
        note_drop(&mut r, "m", "unreadable", Some(1_788_700_000_000));
        let out = r.drain(true);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].render().starts_with("2026-09-06T"),
            "{}",
            out[0].render()
        );
        assert_eq!(out[0].metric, "!drop");
    }

    fn folded(input: &str, width_ms: u64) -> anyhow::Result<String> {
        let o = FoldOpts {
            width_ms,
            grace_ms: 0,
            revise_ms: 0,
            max_series: 1000,
        };
        let mut out: Vec<u8> = Vec::new();
        fold_stream(&o, std::io::Cursor::new(input.as_bytes()), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn fold_buckets_observations_anybody_produced() {
        // The reason there is no external-extractor hook: a program emits
        // observations and the fold that ships does the rest, so nobody
        // reimplements sealing, revisions or the citation span.
        let out = folded(
            "2026-09-06T10:00:01.000Z 0s gc_pause count=1 sum=0.5\n\
             2026-09-06T10:00:44.000Z 0s gc_pause count=1 sum=1.5\n",
            60_000,
        )
        .unwrap();
        assert_eq!(out, "2026-09-06T10:00:00.000Z 60s gc_pause count=2 sum=2\n");
    }

    #[test]
    fn fold_refuses_a_line_it_cannot_read_and_one_already_bucketed() {
        // Fatal, not skipped: a fold that quietly dropped a tenth of its
        // input would report numbers that are WRONG rather than missing.
        let err = folded(
            "2026-09-06T10:00:01.000Z 0s m count=1\nnot a sample\n",
            60_000,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("line 2"), "{err}");

        let err = folded("2026-09-06T10:00:00.000Z 60s m count=3\n", 60_000).unwrap_err();
        assert!(format!("{err}").contains("OBSERVATIONS"), "{err}");
    }

    #[test]
    fn everything_shipped_carries_the_reserved_prefix() {
        // A one-sided promise, enforced rather than remembered: names
        // live in one flat namespace and a collision is refused, so a
        // name added in a later release could otherwise break a
        // deployment whose own document already used it.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for entry in std::fs::read_dir(root.join("packaging/extractors")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let doc = Extractor::load(&path).unwrap();
            assert!(
                doc.name.starts_with(SHIPPED_PREFIX),
                "{} is named {:?} — everything shipped from here carries {SHIPPED_PREFIX}",
                path.display(),
                doc.name
            );
            let stem = path.file_stem().unwrap().to_str().unwrap();
            assert_eq!(
                stem,
                doc.name,
                "{} declares a name its filename does not match — the name is what \
                 counts, but a reader looking for one should find it",
                path.display()
            );
        }
    }

    #[test]
    fn a_later_directory_shadows_an_earlier_one_by_filename() {
        // What makes `--extractor /usr/lib/… --extractor /etc/…` mean
        // "the packaged set, with the site's edits winning". Forking a
        // shipped document means keeping its filename.
        let packaged = tempdir();
        let site = tempdir();
        std::fs::write(
            packaged.join("x.json"),
            doc(r#"{"name":"m","measure":[{"count":true}]}"#),
        )
        .unwrap();
        std::fs::write(
            site.join("x.json"),
            doc(r#"{"name":"m","description":"the site's","measure":[{"count":true}]}"#),
        )
        .unwrap();
        let loaded = load_extractors(&[packaged.clone(), site.clone()]).unwrap();
        assert_eq!(loaded.len(), 1, "shadowed, not collided");
        assert_eq!(
            loaded[0].1.metrics[0].description.as_deref(),
            Some("the site's")
        );
        std::fs::remove_dir_all(&packaged).ok();
        std::fs::remove_dir_all(&site).ok();
    }

    /// Every extractor this repository SHIPS, run against a fixture and
    /// compared to a committed answer.
    ///
    /// It is the same operation an operator does with `--try`, which is
    /// the point: the tool for trying a definition against a real file
    /// and the harness that tests ours are one thing, so neither can rot
    /// while the other is exercised. The fixtures earn their keep — the
    /// apache one already caught `-` (CLF's zero-byte response) being
    /// read as unreadable, which turned every empty response into a
    /// `!drop` saying the numbers were wrong.
    #[test]
    fn the_shipped_extractors_produce_the_committed_answers() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut checked = 0;
        for entry in std::fs::read_dir(root.join("packaging/extractors")).unwrap() {
            let doc = entry.unwrap().path();
            if doc.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let name = doc.file_stem().unwrap().to_str().unwrap().to_string();
            let fixture = root.join(format!("tests/extractors/{name}.log"));
            let golden = root.join(format!("tests/extractors/{name}.tally"));
            assert!(
                fixture.exists(),
                "{name} ships with no fixture — an extractor nobody has run is one \
                 nobody has checked. Write tests/extractors/{name}.log"
            );
            let docs = load_extractors(std::slice::from_ref(&doc)).unwrap();
            let opts = TallyOpts {
                extractors: vec![doc],
                try_it: true,
                check: false,
                observations: false,
                metrics: Vec::new(),
                width_ms: None,
                fold: None,
            };
            let text = std::fs::read(&fixture).unwrap();
            let tried = try_text(&docs, &opts, &text).unwrap();
            let report: String = tried
                .seen
                .iter()
                .map(|(m, s)| {
                    format!(
                        "# {m}: claimed {} skipped {} dropped {}\n",
                        s.claimed, s.skipped, s.dropped
                    )
                })
                .collect();
            let produced = format!("{report}{}", tried.tally);
            if std::env::var("UPDATE_GOLDEN").is_ok() {
                std::fs::write(&golden, &produced).unwrap();
                checked += 1;
                continue;
            }
            let committed = std::fs::read_to_string(&golden).unwrap_or_default();
            assert_eq!(
                committed,
                produced,
                "{name} no longer produces its committed answer. Review the diff, then:\n  \
                 UPDATE_GOLDEN=1 cargo test --lib the_shipped_extractors_produce_the_committed_answers"
            );
            checked += 1;
        }
        assert!(checked > 0, "no shipped extractors were checked");
    }

    /// The contract is a FILE in the repository, not something a build
    /// produces: it has to show up in a diff so a change to it is
    /// reviewed rather than discovered by whoever ships an extractor.
    #[test]
    fn the_committed_extractor_schema_matches_the_types() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/tally-extractor.schema.json"
        );
        let generated =
            serde_json::to_string_pretty(&schemars::schema_for!(Extractor)).unwrap() + "\n";
        if std::env::var("UPDATE_SCHEMA").is_ok() {
            std::fs::write(path, &generated).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            committed, generated,
            "the tally extractor's contract has changed. Review the diff, then:\n  \
             UPDATE_SCHEMA=1 cargo test --lib the_committed_extractor_schema_matches_the_types"
        );
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("timberfs-tally-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
