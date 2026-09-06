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
use serde_json::{Map, Value};

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

// --------------------------------------------------------------- the rules

/// Which clock a bucket is on. Required with no default, for the reason
/// a query document's `window.axis` is: the two answers differ and a
/// default is an assumption the next reader makes wrongly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    Logline,
    Write,
}

/// A line of known shape, turned into fields a rule can name. Few, and
/// only for formats somebody else standardised: a decoder per producer
/// would be a taxonomy growing a binary per format, which is what
/// `EXTRACT` and `EXEC` exist to avoid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoder {
    Logfmt,
    Json,
    ApacheCombined,
}

#[derive(Clone, Debug)]
pub enum Fields {
    /// No parsing at all: a predicate and a count. Most generic metrics.
    Entry,
    Decode(Decoder),
    Extract(Box<regex::bytes::Regex>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Measure {
    Count,
    Sum(String),
    Min(String),
    Max(String),
    Last(String),
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub metric: String,
    pub select: String,
    pub preds: crate::grep::PredSpec,
    pub fields: Fields,
    pub labels: Vec<String>,
    pub measures: Vec<Measure>,
    /// `OBSERVE` + `BUCKETS`: cumulative histogram buckets, emitted as
    /// one series per `le` — Prometheus's own encoding, so a quantile is
    /// read-time interpolation over additive data and needs no syntax of
    /// its own.
    pub histogram: Option<(String, Vec<f64>)>,
    pub axis: Axis,
    pub width_ms: u64,
    pub grace_ms: u64,
    pub revise_ms: u64,
    pub cite: bool,
    pub max_series: usize,
    /// Where it was declared, for the message a collision has to make.
    pub file: String,
    pub line: usize,
}

impl Rule {
    /// What a change to this rule changes about the numbers. Its own
    /// name is deliberately NOT in it: renaming a metric is a new
    /// series, not a drift of an old one.
    pub fn definition(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}",
            self.preds,
            match &self.fields {
                Fields::Entry => "entry".to_string(),
                Fields::Decode(d) => format!("{d:?}"),
                Fields::Extract(r) => r.as_str().to_string(),
            },
            self.labels,
            self.measures,
            self.histogram,
            self.axis,
            self.width_ms,
            self.select,
        )
    }
}

#[derive(Clone, Debug)]
pub struct Defaults {
    pub select: String,
    pub axis: Option<Axis>,
    pub width_ms: u64,
    pub grace_ms: u64,
    pub revise_ms: u64,
    pub cite: bool,
    pub max_series: usize,
    /// Declared on every tally store this set writes.
    pub declare: Vec<String>,
    pub store_dir: Option<PathBuf>,
    /// How a tally store is named after its source; `{name}` is the
    /// source's.
    pub name: String,
}

impl Default for Defaults {
    fn default() -> Defaults {
        Defaults {
            select: "[]".to_string(),
            axis: None,
            width_ms: 60_000,
            grace_ms: 120_000,
            revise_ms: 3_600_000,
            cite: true,
            max_series: 1000,
            declare: Vec::new(),
            store_dir: None,
            name: "{name}-tally".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
    pub defaults: Defaults,
}

const SELECT: &str = "SELECT";
const AXIS: &str = "AXIS";
const WIDTH: &str = "WIDTH";
const GRACE: &str = "GRACE";
const REVISE: &str = "REVISE";
const CITE: &str = "CITE";
const MAX_SERIES: &str = "MAX_SERIES";
const DECLARE: &str = "DECLARE";
const STORE_DIR: &str = "STORE_DIR";
const NAME: &str = "NAME";
const HAS: &str = "HAS";
const ANY: &str = "ANY";
const SUBSTRING: &str = "SUBSTRING";
const REGEX: &str = "REGEX";
const NOT_HAS: &str = "NOT_HAS";
const NOT_SUBSTRING: &str = "NOT_SUBSTRING";
const NOT_REGEX: &str = "NOT_REGEX";
const DECODE: &str = "DECODE";
const EXTRACT: &str = "EXTRACT";
const EXEC: &str = "EXEC";
const LABELS: &str = "LABELS";
const COUNT: &str = "COUNT";
const SUM: &str = "SUM";
const MIN: &str = "MIN";
const MAX: &str = "MAX";
const LAST: &str = "LAST";
const OBSERVE: &str = "OBSERVE";
const BUCKETS: &str = "BUCKETS";

const KEYS: &[&str] = &[
    SELECT,
    AXIS,
    WIDTH,
    GRACE,
    REVISE,
    CITE,
    MAX_SERIES,
    DECLARE,
    STORE_DIR,
    NAME,
    HAS,
    ANY,
    SUBSTRING,
    REGEX,
    NOT_HAS,
    NOT_SUBSTRING,
    NOT_REGEX,
    DECODE,
    EXTRACT,
    EXEC,
    LABELS,
    COUNT,
    SUM,
    MIN,
    MAX,
    LAST,
    OBSERVE,
    BUCKETS,
];

/// Keys that belong to the preamble alone.
const PREAMBLE_ONLY: &[&str] = &[DECLARE, STORE_DIR, NAME];

#[derive(Default)]
struct Open {
    metric: String,
    line: usize,
    select: Option<String>,
    axis: Option<Axis>,
    width_ms: Option<u64>,
    grace_ms: Option<u64>,
    revise_ms: Option<u64>,
    cite: Option<bool>,
    max_series: Option<usize>,
    preds: crate::grep::PredSpec,
    fields: Option<Fields>,
    fields_key: Option<&'static str>,
    labels: Vec<String>,
    measures: Vec<Measure>,
    observe: Option<String>,
    buckets: Option<Vec<f64>>,
}

/// Parse one rule file.
///
/// ⚠ A line this build cannot use is FATAL, as in `file.d` and for the
/// same reason: a rule that was skipped is a metric nobody is measuring,
/// and nothing later says so.
pub fn parse(file: &str, text: &str) -> anyhow::Result<RuleSet> {
    let mut defaults = Defaults::default();
    let mut open: Option<Open> = None;
    let mut rules: Vec<Rule> = Vec::new();
    let mut named: BTreeMap<String, usize> = BTreeMap::new();

    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }

        if let Some(head) = s.strip_prefix('[') {
            let Some(metric) = head.strip_suffix(']') else {
                bail!("{file}:{line}: a section header ends with ']' — got {s:?}");
            };
            let metric = metric.trim().to_string();
            check_metric(&metric).with_context(|| format!("{file}:{line}"))?;
            if metric.starts_with(MARKER) {
                bail!("{file}:{line}: {MARKER} starts a MARKER, which no rule may name");
            }
            if let Some(first) = named.insert(metric.clone(), line) {
                bail!(
                    "{file}:{line}: [{metric}] was already declared on line {first} — \
                     a metric is named once"
                );
            }
            if let Some(prev) = open.take() {
                rules.push(close(prev, &defaults, file)?);
            }
            open = Some(Open {
                metric,
                line,
                ..Open::default()
            });
            continue;
        }

        let Some((key, value)) = s.split_once('=') else {
            bail!("{file}:{line}: expected KEY=VALUE or [metric] — got {s:?}");
        };
        let key = key.trim();
        let value = value.trim();
        if !KEYS.contains(&key) {
            bail!(
                "{file}:{line}: unknown key {key:?} — this build reads {}",
                KEYS.join(", ")
            );
        }
        let ms = |k: &str| -> anyhow::Result<u64> {
            crate::append::parse_duration_ms(value).with_context(|| format!("{file}:{line}: {k}"))
        };
        let flag = || -> anyhow::Result<bool> {
            match value {
                "true" | "yes" | "1" | "" => Ok(true),
                "false" | "no" | "0" => Ok(false),
                other => bail!("{file}:{line}: {key}={other:?} is true or false"),
            }
        };
        let words = || -> Vec<String> { value.split_whitespace().map(str::to_string).collect() };

        let Some(cur) = open.as_mut() else {
            match key {
                SELECT => {
                    defaults.select = crate::select::canonical(value)
                        .with_context(|| format!("{file}:{line}: {SELECT}"))?
                }
                AXIS => defaults.axis = Some(parse_axis(value, file, line)?),
                WIDTH => defaults.width_ms = width(ms(WIDTH)?, file, line)?,
                GRACE => defaults.grace_ms = ms(GRACE)?,
                REVISE => defaults.revise_ms = ms(REVISE)?,
                CITE => defaults.cite = flag()?,
                MAX_SERIES => defaults.max_series = value.parse()?,
                DECLARE => defaults.declare = words(),
                STORE_DIR => defaults.store_dir = Some(PathBuf::from(value)),
                NAME => defaults.name = value.to_string(),
                other => {
                    bail!("{file}:{line}: {other} belongs to a section — it describes ONE metric")
                }
            };
            continue;
        };

        if PREAMBLE_ONLY.contains(&key) {
            bail!(
                "{file}:{line}: {key} belongs to the preamble — it is a property of \
                 the SET, not of [{}]",
                cur.metric
            );
        }
        let one_source = |cur: &mut Open, which: &'static str, f: Fields| -> anyhow::Result<()> {
            if let Some(had) = cur.fields_key {
                bail!(
                    "{file}:{line}: [{}] already states {had} — a rule has ONE source \
                     of fields",
                    cur.metric
                );
            }
            cur.fields_key = Some(which);
            cur.fields = Some(f);
            Ok(())
        };
        match key {
            SELECT => {
                cur.select = Some(
                    crate::select::canonical(value)
                        .with_context(|| format!("{file}:{line}: {SELECT}"))?,
                )
            }
            AXIS => cur.axis = Some(parse_axis(value, file, line)?),
            WIDTH => cur.width_ms = Some(width(ms(WIDTH)?, file, line)?),
            GRACE => cur.grace_ms = Some(ms(GRACE)?),
            REVISE => cur.revise_ms = Some(ms(REVISE)?),
            CITE => cur.cite = Some(flag()?),
            MAX_SERIES => cur.max_series = Some(value.parse()?),
            HAS => cur
                .preds
                .all
                .extend(words().iter().map(|w| pred(w, crate::grep::PredKind::Has))),
            ANY => cur
                .preds
                .any
                .extend(words().iter().map(|w| pred(w, crate::grep::PredKind::Has))),
            NOT_HAS => cur
                .preds
                .none
                .extend(words().iter().map(|w| pred(w, crate::grep::PredKind::Has))),
            SUBSTRING => cur
                .preds
                .all
                .push(pred(value, crate::grep::PredKind::Substring)),
            NOT_SUBSTRING => cur
                .preds
                .none
                .push(pred(value, crate::grep::PredKind::Substring)),
            REGEX => cur
                .preds
                .all
                .push(pred(value, crate::grep::PredKind::Regex)),
            NOT_REGEX => cur
                .preds
                .none
                .push(pred(value, crate::grep::PredKind::Regex)),
            DECODE => {
                let d = match value {
                    "logfmt" => Decoder::Logfmt,
                    "json" => Decoder::Json,
                    "apache-combined" => Decoder::ApacheCombined,
                    other => bail!(
                        "{file}:{line}: no decoder {other:?} — this build has logfmt, json, \
                         apache-combined; anything else is {EXTRACT} or {EXEC}"
                    ),
                };
                one_source(cur, DECODE, Fields::Decode(d))?
            }
            EXTRACT => {
                let re = regex::bytes::Regex::new(value)
                    .with_context(|| format!("{file}:{line}: {EXTRACT}"))?;
                if re.capture_names().flatten().count() == 0 {
                    bail!(
                        "{file}:{line}: {EXTRACT} needs NAMED captures — (?P<status>…) is \
                         what becomes a field"
                    );
                }
                one_source(cur, EXTRACT, Fields::Extract(Box::new(re)))?
            }
            // A RESERVED key rather than an unknown one: reaching for it
            // is a reasonable instinct, and the answer is a route that
            // already exists rather than a missing feature.
            EXEC => bail!(
                "{file}:{line}: [{}] states {EXEC} — there is no external extractor and \
                 there will not be one. A program that needs state across entries is a \
                 CONSUMER: register it as a follower, have it write a tally store of its \
                 own, and pipe its width-0s observations through `timberfs tally --fold` \
                 so it need not bucket them itself. Several tally stores may derive from \
                 one log; a reader selects across them. See docs/plans/tally.md",
                cur.metric
            ),
            LABELS => cur.labels = words(),
            COUNT => {
                if !flag()? {
                    bail!("{file}:{line}: {COUNT} is stated or absent, never false");
                }
                cur.measures.push(Measure::Count)
            }
            SUM => cur.measures.push(Measure::Sum(value.to_string())),
            MIN => cur.measures.push(Measure::Min(value.to_string())),
            MAX => cur.measures.push(Measure::Max(value.to_string())),
            LAST => cur.measures.push(Measure::Last(value.to_string())),
            OBSERVE => cur.observe = Some(value.to_string()),
            BUCKETS => {
                let mut b: Vec<f64> = Vec::new();
                for w in value.split_whitespace() {
                    b.push(
                        w.parse()
                            .with_context(|| format!("{file}:{line}: {BUCKETS} {w:?}"))?,
                    );
                }
                b.sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));
                cur.buckets = Some(b)
            }
            other => bail!("{file}:{line}: {other} is not a section key"),
        }
    }
    if let Some(prev) = open.take() {
        rules.push(close(prev, &defaults, file)?);
    }
    Ok(RuleSet { rules, defaults })
}

fn pred(text: &str, kind: crate::grep::PredKind) -> crate::grep::Pred {
    crate::grep::Pred {
        kind,
        text: text.to_string(),
        caseless: false,
    }
}

fn parse_axis(v: &str, file: &str, line: usize) -> anyhow::Result<Axis> {
    match v {
        "logline" => Ok(Axis::Logline),
        "write" => Ok(Axis::Write),
        other => bail!("{file}:{line}: {AXIS}={other:?} is logline or write"),
    }
}

fn width(ms: u64, file: &str, line: usize) -> anyhow::Result<u64> {
    if ms == 0 || !ms.is_multiple_of(1000) {
        bail!("{file}:{line}: {WIDTH} is whole seconds and not zero");
    }
    Ok(ms)
}

fn close(o: Open, d: &Defaults, file: &str) -> anyhow::Result<Rule> {
    let at = o.line;
    let histogram = match (o.observe, o.buckets) {
        (Some(f), Some(b)) => Some((f, b)),
        (Some(_), None) => bail!(
            "{file}:{at}: [{}] states {OBSERVE} without {BUCKETS}",
            o.metric
        ),
        (None, Some(_)) => bail!(
            "{file}:{at}: [{}] states {BUCKETS} without {OBSERVE}",
            o.metric
        ),
        (None, None) => None,
    };
    if o.measures.is_empty() && histogram.is_none() {
        bail!(
            "{file}:{at}: [{}] measures nothing — state {COUNT}, {SUM}=field, {MIN}/{MAX}/{LAST}, \
             or {OBSERVE}+{BUCKETS}",
            o.metric
        );
    }
    let fields = o.fields.unwrap_or(Fields::Entry);
    if matches!(fields, Fields::Entry) {
        let needs: Vec<&str> = o
            .measures
            .iter()
            .filter_map(|m| match m {
                Measure::Count => None,
                Measure::Sum(f) | Measure::Min(f) | Measure::Max(f) | Measure::Last(f) => {
                    Some(f.as_str())
                }
            })
            .chain(histogram.iter().map(|(f, _)| f.as_str()))
            .chain(o.labels.iter().map(|s| s.as_str()))
            .collect();
        if let Some(f) = needs.first() {
            bail!(
                "{file}:{at}: [{}] names the field {f:?} but states no {DECODE}, {EXTRACT} \
                 or {EXEC} to get it from",
                o.metric
            );
        }
    }
    for l in &o.labels {
        if Field::parse(l).is_some() {
            bail!(
                "{file}:{at}: [{}] labels with {l:?}, which is a measure name",
                o.metric
            );
        }
    }
    let axis = o.axis.or(d.axis).with_context(|| {
        format!(
            "{file}:{at}: [{}] has no {AXIS} and the preamble sets none — \
             logline and write bucket differently and there is no safe default",
            o.metric
        )
    })?;
    Ok(Rule {
        metric: o.metric,
        select: o.select.unwrap_or_else(|| d.select.clone()),
        preds: o.preds,
        fields,
        labels: o.labels,
        measures: o.measures,
        histogram,
        axis,
        width_ms: o.width_ms.unwrap_or(d.width_ms),
        grace_ms: o.grace_ms.unwrap_or(d.grace_ms),
        revise_ms: o.revise_ms.unwrap_or(d.revise_ms),
        cite: o.cite.unwrap_or(d.cite),
        max_series: o.max_series.unwrap_or(d.max_series),
        file: file.to_string(),
        line: at,
    })
}

/// Load a file, or every `*.conf` in a directory.
///
/// A directory is read WHOLE by one process: two rule files may match one
/// source store and there is one writer per store, so the file split is
/// for editing, not for supervision — unlike `file.d`, where a set is a
/// unit of both.
pub fn load(path: &Path) -> anyhow::Result<RuleSet> {
    let mut files: Vec<PathBuf> = Vec::new();
    if path.is_dir() {
        let mut names: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("reading {}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "conf"))
            .collect();
        names.sort();
        files.extend(names);
    } else {
        files.push(path.to_path_buf());
    }
    if files.is_empty() {
        bail!("no *.conf under {}", path.display());
    }
    let mut all: Vec<Rule> = Vec::new();
    let mut defaults = Defaults::default();
    let mut where_named: BTreeMap<String, (String, usize)> = BTreeMap::new();
    for f in &files {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        let name = f.display().to_string();
        let set = parse(&name, &text)?;
        for r in &set.rules {
            if let Some((other, line)) = where_named.get(&r.metric) {
                if *other != r.file {
                    // Refused rather than merged or last-one-wins: which
                    // definition a number came from must not depend on
                    // readdir order.
                    bail!(
                        "{}:{}: [{}] is also declared at {other}:{line} — a metric is \
                         defined once",
                        r.file,
                        r.line,
                        r.metric
                    );
                }
            }
            where_named.insert(r.metric.clone(), (r.file.clone(), r.line));
        }
        // The last file's preamble wins for the SET-wide facts; per-rule
        // ones were already resolved against their own file's preamble.
        defaults = set.defaults;
        all.extend(set.rules);
    }
    Ok(RuleSet {
        rules: all,
        defaults,
    })
}

impl RuleSet {
    /// The rules that apply to one store, by what it declares.
    pub fn for_store(&self, manifest: &Map<String, Value>) -> anyhow::Result<Vec<&Rule>> {
        let mut out = Vec::new();
        for r in &self.rules {
            let sel = crate::select::Selector::parse(&r.select)?;
            if sel.matches(manifest) {
                out.push(r);
            }
        }
        Ok(out)
    }
}

// ------------------------------------------------------------- extraction

/// One entry's fields, as whatever source the rule declared reads them.
fn decode(d: Decoder, entry: &[u8]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    match d {
        Decoder::Logfmt => {
            let text = String::from_utf8_lossy(entry);
            let line = text.lines().next().unwrap_or("");
            if let Ok(toks) = tokenize(line) {
                for t in toks {
                    if let Some((k, v)) = t.split_once('=') {
                        out.insert(k.to_string(), v.to_string());
                    }
                }
            }
        }
        Decoder::Json => {
            if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(entry) {
                for (k, v) in map {
                    let s = match v {
                        Value::String(s) => s,
                        Value::Null | Value::Array(_) | Value::Object(_) => continue,
                        other => other.to_string(),
                    };
                    out.insert(k, s);
                }
            }
        }
        Decoder::ApacheCombined => {
            let text = String::from_utf8_lossy(entry);
            let line = text.lines().next().unwrap_or("");
            if let Some(c) = apache_re().captures(line) {
                let mut put = |k: &str, i: usize| {
                    if let Some(m) = c.get(i) {
                        if m.as_str() != "-" {
                            out.insert(k.to_string(), m.as_str().to_string());
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
    }
    out
}

/// NCSA common and combined, which differ only by the last two fields —
/// hence one pattern with them optional. Anything else a producer emits
/// is `EXTRACT`'s job, deliberately: Apache's `LogFormat` is
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
    /// The predicate did not select this entry: not a loss, and not
    /// counted as one.
    Skipped,
    Observed(Vec<Sample>),
    /// Selected, and then not measurable. Counted and reported — never
    /// silently dropped.
    Dropped(&'static str),
}

pub fn observe(
    rule: &Rule,
    preds: &crate::grep::Preds,
    entry: &[u8],
    ts: u64,
    cite: Option<(u64, u64)>,
) -> Outcome {
    if !preds.is_empty() && !preds.keep(entry) {
        return Outcome::Skipped;
    }
    let fields = match &rule.fields {
        Fields::Entry => BTreeMap::new(),
        Fields::Decode(d) => decode(*d, entry),
        Fields::Extract(re) => {
            let Some(c) = re.captures(entry) else {
                return Outcome::Dropped("nomatch");
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
    let labels: Vec<(String, String)> = rule
        .labels
        .iter()
        .filter_map(|k| fields.get(k).map(|v| (k.clone(), v.clone())))
        .collect();

    let num = |key: &str| -> Option<f64> { fields.get(key).and_then(|v| v.parse::<f64>().ok()) };

    let mut out = Vec::new();
    if !rule.measures.is_empty() {
        let mut s = Sample::new(ts, 0, &rule.metric);
        s.labels = labels.clone();
        s.labels.sort();
        s.cite = if rule.cite { cite } else { None };
        for m in &rule.measures {
            let (f, v) = match m {
                Measure::Count => (Field::Count, 1.0),
                Measure::Sum(k) => match num(k) {
                    Some(v) => (Field::Sum, v),
                    None => return Outcome::Dropped("unparsed"),
                },
                Measure::Min(k) => match num(k) {
                    Some(v) => (Field::Min, v),
                    None => return Outcome::Dropped("unparsed"),
                },
                Measure::Max(k) => match num(k) {
                    Some(v) => (Field::Max, v),
                    None => return Outcome::Dropped("unparsed"),
                },
                Measure::Last(k) => match num(k) {
                    Some(v) => (Field::Last, v),
                    None => return Outcome::Dropped("unparsed"),
                },
            };
            s = s.field(f, v);
        }
        out.push(s);
    }

    if let Some((key, bounds)) = &rule.histogram {
        let Some(v) = num(key) else {
            return Outcome::Dropped("unparsed");
        };
        // Cumulative, as Prometheus spells it: every bucket at or above
        // the value counts it, so coarsening stays addition and a
        // quantile is interpolation over sums.
        for b in bounds {
            if v <= *b {
                let mut s = Sample::new(ts, 0, &rule.metric);
                s.labels = labels.clone();
                s.labels.push(("le".to_string(), number(*b)));
                s.labels.sort();
                s.cite = if rule.cite { cite } else { None };
                out.push(s.field(Field::Count, 1.0));
            }
        }
        let mut inf = Sample::new(ts, 0, &rule.metric);
        inf.labels = labels;
        inf.labels.push(("le".to_string(), "+Inf".to_string()));
        inf.labels.sort();
        inf.cite = if rule.cite { cite } else { None };
        // The +Inf bucket carries the total, so an average needs no
        // second metric.
        out.push(inf.field(Field::Count, 1.0).field(Field::Sum, v));
    }

    Outcome::Observed(out)
}

// -------------------------------------------------------------- the command

pub struct TallyOpts {
    /// Absent under `--fold`, which needs no rules: it buckets lines
    /// somebody else produced.
    pub rules: Option<PathBuf>,
    /// Read width-`0s` OBSERVATION lines on stdin and write buckets,
    /// instead of reading a records stream.
    ///
    /// This is what makes "write your own extractor" a real answer
    /// rather than an invitation to reimplement sealing, revisions and
    /// the citation span: a program emits observations, pipes them
    /// through here, and the fold is the one that ships.
    pub fold: Option<FoldOpts>,
    /// Print the width-`0s` observations instead of bucketing them —
    /// the debugging path, and the way to learn what an `EXEC`
    /// extractor is expected to emit.
    pub observations: bool,
    /// Only these metrics, for recomputing one over history.
    pub metrics: Vec<String>,
    /// The store the stream came from, when it cannot say — a
    /// `query --records` answer names a path but carries no labels.
    pub store: Option<PathBuf>,
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
///
/// ⚠ A line that does not parse is FATAL, not skipped. A fold that
/// quietly dropped a tenth of its input would report numbers that are
/// wrong rather than missing, and nothing downstream could tell.
pub fn cmd_fold(o: &FoldOpts) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    fold_stream(o, stdin.lock(), &mut out)?;
    out.flush()?;
    Ok(())
}

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
            // Re-bucketing a tape is legitimate and is the same fold, but
            // it must be asked for knowingly: silently accepting a
            // 60s line into a 60s fold would double every count on a
            // re-run.
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

struct Live<'a> {
    rule: &'a Rule,
    preds: crate::grep::Preds,
    roller: Roller,
}

pub fn cmd_tally(opts: &TallyOpts) -> anyhow::Result<()> {
    if let Some(fold) = &opts.fold {
        return cmd_fold(fold);
    }
    let Some(rules) = &opts.rules else {
        bail!("--rules names what to measure; --fold buckets observations somebody else made");
    };
    let set = load(rules)?;
    let stdin = std::io::stdin();
    let mut reader = crate::records::Reader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    let mut manifest: Option<Map<String, Value>> = opts.store.as_ref().and_then(|p| {
        Some(crate::select::selectable_of(
            p.parent()?,
            p.file_name()?.to_str()?,
        ))
    });
    let mut live: Option<Vec<Live>> = None;
    let mut seen_store: Option<String> = None;
    let mut ended = false;

    while let Some(rec) = reader.next_rec()? {
        match rec {
            crate::records::Rec::Source(fields) => {
                let get = |k: &str| fields.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
                if let Some(id) = get("id").or_else(|| get("path")) {
                    one_store(&mut seen_store, &id)?;
                }
                if let Some(text) = get("labels") {
                    if let Ok(Value::Object(m)) = serde_json::from_str(&text) {
                        manifest = Some(m);
                        live = None;
                    }
                } else if manifest.is_none() {
                    if let Some(p) = get("path") {
                        let p = PathBuf::from(p);
                        if let (Some(dir), Some(name)) = (p.parent(), p.file_name()) {
                            if let Some(name) = name.to_str() {
                                manifest = Some(crate::select::selectable_of(dir, name));
                            }
                        }
                    }
                }
            }
            crate::records::Rec::Entry(e) => {
                if let Some(id) = e.id.clone().or_else(|| e.src.clone()) {
                    one_store(&mut seen_store, &id)?;
                }
                let rules = match live.as_mut() {
                    Some(l) => l,
                    None => {
                        live = Some(resolve_rules(&set, manifest.as_ref(), &opts.metrics)?);
                        live.as_mut().expect("just set")
                    }
                };
                let mut batch: Vec<Sample> = Vec::new();
                for l in rules.iter_mut() {
                    let ts = match l.rule.axis {
                        Axis::Logline => e.ts,
                        // The chunk's first write is the best arrival
                        // stamp an entry carries — so a write-axis
                        // bucket is only as fine as a chunk, and on a
                        // quiet log that is coarse. `.rings` answers
                        // volume on this axis exactly and for nothing;
                        // prefer it where it can.
                        Axis::Write => e.wf,
                    };
                    let cite = e.offset.map(|off| (off, e.payload.len() as u64));
                    let Some(ts) = ts else {
                        note_drop(&mut l.roller, &l.rule.metric, "nostamp");
                        continue;
                    };
                    match observe(l.rule, &l.preds, &e.payload, ts, cite) {
                        Outcome::Skipped => {}
                        Outcome::Dropped(why) => note_drop(&mut l.roller, &l.rule.metric, why),
                        Outcome::Observed(obs) => {
                            for o in &obs {
                                if opts.observations {
                                    writeln!(out, "{}", o.render())?;
                                } else {
                                    l.roller.add(o);
                                }
                            }
                        }
                    }
                    if !opts.observations {
                        batch.extend(l.roller.drain(false));
                    }
                }
                emit(&mut out, batch)?;
            }
            crate::records::Rec::End(_) => ended = true,
            crate::records::Rec::Start(_) | crate::records::Rec::Position(_) => {}
        }
    }
    if !opts.observations {
        if let Some(rules) = live.as_mut() {
            let mut batch: Vec<Sample> = Vec::new();
            for l in rules.iter_mut() {
                batch.extend(l.roller.drain(true));
            }
            emit(&mut out, batch)?;
        }
    }
    out.flush()?;
    if !ended {
        // A stream that died and one that finished are the same
        // observation without this record, and a tally of a truncated
        // read is a graph that is wrong rather than short.
        bail!("the record stream ended without stream-end — the answer is truncated");
    }
    Ok(())
}

/// One batch of sealed buckets, in time order. Every rule seals off the
/// same entry stream, so ordering the batch is what keeps a tape written
/// by several rules readable — the store's own clock is arrival, and a
/// reader's is the stamp on the line, so neither depends on this; it is
/// for the person who runs `zstd -dc`.
fn emit(out: &mut impl Write, mut batch: Vec<Sample>) -> anyhow::Result<()> {
    batch.sort_by(|a, b| (a.ts, &a.metric, &a.labels).cmp(&(b.ts, &b.metric, &b.labels)));
    for s in batch {
        writeln!(out, "{}", s.render())?;
    }
    Ok(())
}

/// One tally store per source store, so one source per run. The fan-out
/// belongs to the follower that runs one of these per store, which is
/// not built yet — refusing here beats writing two stores' numbers into
/// one tape under one set of labels.
fn one_store(seen: &mut Option<String>, id: &str) -> anyhow::Result<()> {
    match seen {
        Some(had) if had == id => Ok(()),
        Some(had) => bail!(
            "this stream carries more than one store ({had} and {id}) — a tally store \
             belongs to ONE source store, so read them one at a time"
        ),
        None => {
            *seen = Some(id.to_string());
            Ok(())
        }
    }
}

fn note_drop(roller: &mut Roller, metric: &str, why: &'static str) {
    let ts = roller.watermark();
    let s = Sample::new(ts, 0, "!drop")
        .label("metric", metric)
        .label("reason", why)
        .field(Field::Count, 1.0);
    roller.add(&s);
}

fn resolve_rules<'a>(
    set: &'a RuleSet,
    manifest: Option<&Map<String, Value>>,
    only: &[String],
) -> anyhow::Result<Vec<Live<'a>>> {
    let empty = Map::new();
    let m = manifest.unwrap_or(&empty);
    let mut chosen = set.for_store(m)?;
    if !only.is_empty() {
        chosen.retain(|r| only.contains(&r.metric));
        for want in only {
            if !chosen.iter().any(|r| r.metric == *want) {
                bail!("no rule named {want:?} applies to this store");
            }
        }
    }
    if chosen.is_empty() {
        eprintln!(
            "tally: no rule matches this store — {} rule(s) loaded, none selecting it",
            set.rules.len()
        );
    }
    chosen
        .into_iter()
        .map(|rule| {
            Ok(Live {
                preds: crate::grep::Preds::compile(rule.preds.clone())?,
                roller: Roller::new(
                    rule.width_ms,
                    rule.grace_ms,
                    rule.revise_ms,
                    rule.max_series,
                ),
                rule,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(line: &str) -> Sample {
        Sample::parse(line).expect(line)
    }

    #[test]
    fn a_line_round_trips_through_its_own_parser() {
        // The format is a wire format the moment an EXEC extractor emits
        // one, so render and parse must be one grammar rather than two.
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
        let set = parse(
            "t.conf",
            "AXIS=logline\n[m]\nDECODE=logfmt\nLABELS=sum\nCOUNT=\n",
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

    #[test]
    fn a_rule_needs_an_axis_and_something_to_measure() {
        assert!(
            parse("t.conf", "[m]\nCOUNT=\n").is_err(),
            "no axis anywhere"
        );
        assert!(
            parse("t.conf", "AXIS=logline\n[m]\n").is_err(),
            "measures nothing"
        );
        assert!(parse("t.conf", "AXIS=logline\n[m]\nCOUNT=\n").is_ok());
    }

    #[test]
    fn a_field_named_with_no_source_to_read_it_from_is_refused() {
        // The failure this catches is a rule that parses, runs, and
        // measures nothing forever: SUM=bytes over an undecoded entry
        // has no `bytes` and would drop every line.
        let err = parse("t.conf", "AXIS=logline\n[m]\nSUM=bytes\n").unwrap_err();
        assert!(format!("{err}").contains("bytes"), "{err}");
    }

    #[test]
    fn a_metric_is_defined_once_across_the_directory() {
        let dir = tempdir();
        std::fs::write(dir.join("a.conf"), "AXIS=logline\n[m]\nCOUNT=\n").unwrap();
        std::fs::write(dir.join("b.conf"), "AXIS=logline\n[m]\nCOUNT=\n").unwrap();
        let err = load(&dir).unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("a.conf") && text.contains("b.conf"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_source_of_fields_per_rule() {
        let err = parse(
            "t.conf",
            "AXIS=logline\n[m]\nDECODE=logfmt\nEXTRACT=(?P<a>x)\nCOUNT=\n",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("ONE source"), "{err}");
    }

    #[test]
    fn exec_is_reserved_and_points_at_the_route_that_exists() {
        // There is no external extractor and there will not be one: a
        // program that needs state across entries is a CONSUMER, which
        // already has a lifecycle, a watermark rule and a registry. The
        // key stays reserved because reaching for it is a reasonable
        // instinct that deserves an answer rather than "unknown key".
        let err = parse(
            "t.conf",
            "AXIS=logline\n[m]\nEXEC=/usr/local/lib/timberfs/tally/x\nCOUNT=\n",
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(
            text.contains("follower") && text.contains("--fold"),
            "{text}"
        );
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
        // The whole reason there is no EXEC: a program emits
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
        // input would report numbers that are WRONG rather than missing,
        // and nothing downstream could tell.
        let err = folded(
            "2026-09-06T10:00:01.000Z 0s m count=1\nnot a sample\n",
            60_000,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("line 2"), "{err}");

        // And a bucket line is not an observation: accepting one would
        // double every count on a re-run.
        let err = folded("2026-09-06T10:00:00.000Z 60s m count=3\n", 60_000).unwrap_err();
        assert!(format!("{err}").contains("OBSERVATIONS"), "{err}");
    }

    #[test]
    fn a_decoder_this_build_lacks_names_the_escapes() {
        let err = parse("t.conf", "AXIS=logline\n[m]\nDECODE=gc\nCOUNT=\n").unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("EXTRACT") && text.contains("EXEC"), "{text}");
    }

    fn rule(text: &str) -> Rule {
        parse("t.conf", text)
            .expect(text)
            .rules
            .pop()
            .expect("a rule")
    }

    fn observed(r: &Rule, line: &str, ts: u64) -> Vec<Sample> {
        let preds = crate::grep::Preds::compile(r.preds.clone()).unwrap();
        match observe(r, &preds, line.as_bytes(), ts, None) {
            Outcome::Observed(v) => v,
            Outcome::Skipped => Vec::new(),
            Outcome::Dropped(why) => panic!("dropped: {why}"),
        }
    }

    #[test]
    fn logfmt_and_apache_decode_into_the_fields_a_rule_names() {
        let r = rule("AXIS=logline\n[m]\nDECODE=logfmt\nLABELS=level\nSUM=ms\n");
        let out = observed(&r, "level=warn ms=42 msg=\"a thing happened\"", 1000);
        assert_eq!(
            out[0].render(),
            "1970-01-01T00:00:01.000Z 0s m level=warn sum=42"
        );

        let r =
            rule("AXIS=logline\n[m]\nDECODE=apache-combined\nLABELS=status method\nSUM=bytes\n");
        let line =
            r#"10.0.0.1 - - [06/Sep/2026:13:37:00 +0200] "GET /x HTTP/1.1" 200 5120 "-" "curl/8""#;
        let out = observed(&r, line, 1000);
        assert_eq!(
            out[0].render(),
            "1970-01-01T00:00:01.000Z 0s m method=GET status=200 sum=5120"
        );
    }

    #[test]
    fn an_absent_label_is_omitted_and_reads_as_empty() {
        // The rule the selector already follows, one level down: an
        // absent key is the empty string, so a series with no `status`
        // is selectable by `status=`.
        let r = rule("AXIS=logline\n[m]\nDECODE=logfmt\nLABELS=status\nCOUNT=\n");
        let out = observed(&r, "msg=hello", 1000);
        assert_eq!(out[0].render(), "1970-01-01T00:00:01.000Z 0s m count=1");
    }

    #[test]
    fn a_histogram_is_cumulative_and_carries_its_total_on_inf() {
        let r = rule("AXIS=logline\n[m]\nDECODE=logfmt\nOBSERVE=ms\nBUCKETS=10 100\n");
        let out = observed(&r, "ms=40", 1000);
        let rendered: Vec<String> = out.iter().map(|s| s.render()).collect();
        assert_eq!(
            rendered,
            vec![
                "1970-01-01T00:00:01.000Z 0s m le=100 count=1",
                "1970-01-01T00:00:01.000Z 0s m le=+Inf count=1 sum=40",
            ]
        );
        // Summable, therefore re-bucketable: the +Inf count is the
        // number of observations and each `le` is a prefix of it.
        let folded = Roller::fold(60_000, &out);
        assert_eq!(folded.len(), 2);
    }

    #[test]
    fn an_entry_that_matched_but_would_not_parse_is_dropped_by_name() {
        let r = rule("AXIS=logline\n[m]\nDECODE=logfmt\nSUM=ms\n");
        let preds = crate::grep::Preds::compile(r.preds.clone()).unwrap();
        match observe(&r, &preds, b"ms=notanumber", 1000, None) {
            Outcome::Dropped(why) => assert_eq!(why, "unparsed"),
            _ => panic!("a measure that cannot be read is a drop, not a zero"),
        }
    }

    #[test]
    fn a_predicate_that_does_not_select_is_not_a_drop() {
        // "This line is not mine" and "this line is mine and broken" are
        // different facts; only the second is a loss worth counting.
        let r = rule("AXIS=logline\n[m]\nHAS=ERROR\nCOUNT=\n");
        let preds = crate::grep::Preds::compile(r.preds.clone()).unwrap();
        assert!(matches!(
            observe(&r, &preds, b"all is well", 1000, None),
            Outcome::Skipped
        ));
        assert!(matches!(
            observe(&r, &preds, b"ERROR the thing", 1000, None),
            Outcome::Observed(_)
        ));
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("timberfs-tally-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
