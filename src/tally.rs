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
pub(crate) fn render_width(ms: u64) -> String {
    format!("{}s", ms / 1000)
}

fn parse_width(t: &str) -> anyhow::Result<u64> {
    let ms = crate::append::parse_duration_ms(t)?;
    if ms % 1000 != 0 {
        bail!("a bucket width is whole seconds — got {t:?}");
    }
    Ok(ms)
}

pub(crate) fn parse_stamp(t: &str) -> anyhow::Result<u64> {
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
/// A bucket across widths: the only thing two lines have to agree on to
/// be the same bucket said twice, which is what `resolve` keys on.
type Revised = (u64, u64, String, Vec<(String, String)>);

#[derive(Default)]
struct Bucket {
    fields: BTreeMap<Field, (f64, u64)>,
    cite_lo: Option<u64>,
    cite_hi: Option<u64>,
    /// The oldest source byte folded in here — what a consumer's
    /// watermark may not pass while this bucket is still open.
    off_lo: Option<u64>,
    /// Changed since it was last written as a PROVISIONAL line, so a
    /// quiet store does not re-state the same numbers every tick.
    /// Meaningless for a sealed bucket, which is written exactly once.
    dirty: bool,
}

/// What a bucket LOST or was HANDED, per metric rather than per series:
/// one marker for the bucket, not one per label set.
#[derive(Default)]
struct Marks {
    capped: u64,
    late: u64,
    /// The largest `grace` that would have kept a displaced entry in its
    /// own bucket, which is exactly the number to act on.
    late_max: u64,
}

/// How much of a roller to write out.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Drain {
    /// Buckets the watermark has left behind: written once and evicted.
    /// Seal-once, which is the rule everything else here follows.
    Sealed,
    /// Those, plus every OPEN bucket that has changed — written as a
    /// REVISION and kept open.
    ///
    /// ⚠ The revision is the whole point. A bucket the producer has not
    /// finished filling has a number worth showing — a quiet store's
    /// newest minute, which nothing else will surface until data arrives
    /// in a later one — but showing it must not consume it. Written and
    /// EVICTED, each tick emitted only what had arrived since the last,
    /// and a reader resolving a bucket to its newest line (`resolve`,
    /// and `timbergraph.read`) then reported the last fragment as the
    /// whole: measured at 512 of 1536 entries. Kept open, the next line
    /// for that bucket carries the complete total and supersedes this
    /// one, which is what those readers already do with it.
    Provisional,
    /// Everything, evicted: the stream has ended and no bucket can
    /// receive another entry.
    Final,
}

/// Observations in, buckets out. Also the coarsener: feeding it buckets
/// of one width and asking for a larger one is the same fold.
///
/// Sealing runs off the WATERMARK — the greatest stamp seen — rather
/// than a wall clock, so a backfill of last month behaves exactly as the
/// live run did and a test is deterministic. A bucket is sealed once,
/// written, and evicted; nothing re-opens one.
pub struct Roller {
    width_ms: u64,
    grace_ms: u64,
    max_series: usize,
    buckets: BTreeMap<Key, Bucket>,
    marks: BTreeMap<(u64, String), Marks>,
    watermark: u64,
    /// The greatest stamp an actual ENTRY carried, which `watermark` is
    /// not once wall clock has stood in for it. `advance_to` clamps
    /// against this so it can never seal a bucket the data is still in.
    last_event: u64,
}

impl Roller {
    pub fn new(width_ms: u64, grace_ms: u64, max_series: usize) -> Roller {
        Roller {
            width_ms: width_ms.max(1),
            grace_ms,
            max_series,
            buckets: BTreeMap::new(),
            marks: BTreeMap::new(),
            watermark: 0,
            last_event: 0,
        }
    }

    /// A one-shot fold over a CLOSED set, for coarsening a tape.
    ///
    /// ⚠ A grace of `u64::MAX` rather than zero: nothing seals until the
    /// forced drain, so nothing is DISPLACED either. A batch fold is
    /// handed its input whole and must not reorder it into the current
    /// bucket the way a stream legitimately does.
    pub fn fold(width_ms: u64, samples: &[Sample]) -> Vec<Sample> {
        let mut r = Roller::new(width_ms, u64::MAX, usize::MAX);
        for s in samples {
            r.add(s);
        }
        r.drain(Drain::Final)
    }

    pub fn watermark(&self) -> u64 {
        self.watermark
    }

    /// Let WALL CLOCK stand in for event time, as far as it may.
    ///
    /// Event time advances when events arrive, which is what makes a
    /// backfill deterministic — and what leaves a quiet log's last
    /// buckets unsealed for ever, since nothing is coming to push them
    /// out. So a reader that has heard nothing for a while calls this.
    ///
    /// ⚠ Clamped to `last_event + grace`, and that clamp is the whole
    /// safety of it. It seals exactly the buckets in-order data has
    /// already passed — the ones whose grace has genuinely expired —
    /// and never the bucket the newest entry is in, which is the only
    /// one an in-order arrival can still land in. Unclamped, it ran the
    /// watermark ahead of the data instead: `advance(2000)` per tick
    /// during a stalled backfill injected 8 s of fabricated event time
    /// per 10 s against 2.56 s of real log time, after which every
    /// entry was late and every batch was displaced whole
    /// (`!late count=<batch>`, measured). Also never past `now`, so a
    /// clock that jumps back cannot rewind it.
    pub fn advance_to(&mut self, now_ms: u64) {
        let reach = self.last_event.saturating_add(self.grace_ms);
        self.watermark = self.watermark.max(now_ms.min(reach));
    }

    /// The oldest source byte any OPEN bucket still depends on. A
    /// consumer may not report past this: what is still open can still
    /// change, and re-reading those entries after a restart re-derives
    /// the identical lines.
    pub fn safe_offset(&self) -> Option<u64> {
        self.buckets.values().filter_map(|b| b.off_lo).min()
    }

    /// Has this bucket start already sealed under the current watermark?
    ///
    /// Decided by the same predicate `drain` seals on, so it does not
    /// depend on how often anyone drains.
    fn sealed(&self, start: u64) -> bool {
        self.watermark >= (start + self.width_ms).saturating_add(self.grace_ms)
    }

    /// Where a stamp belongs, and how late it was if that is not its own
    /// bucket.
    ///
    /// ⚠ A late entry is DISPLACED into the current bucket, never
    /// dropped and never re-opening a sealed one — so a count metric's
    /// sum over a window still equals the number of entries. What it
    /// costs is that the entry lands under a stamp that is not when the
    /// event happened, which is why the displacement is recorded.
    fn place(&self, ts: u64) -> (u64, Option<u64>) {
        let own = ts - ts % self.width_ms;
        if self.sealed(own) {
            let current = self.watermark - self.watermark % self.width_ms;
            // The grace that would have been needed to keep it.
            (current, Some(self.watermark - (own + self.width_ms)))
        } else {
            (own, None)
        }
    }

    pub fn add(&mut self, s: &Sample) {
        self.watermark = self.watermark.max(s.ts);
        self.last_event = self.last_event.max(s.ts);
        let (start, late) = self.place(s.ts);
        // A marker about a bucket is not itself displaceable data.
        if let Some(by) = late.filter(|_| !s.is_marker()) {
            let m = self.marks.entry((start, s.metric.clone())).or_default();
            m.late += 1;
            m.late_max = m.late_max.max(by);
        }
        let key = (start, s.metric.clone(), s.labels.clone());
        if !self.buckets.contains_key(&key) {
            let in_bucket = self
                .buckets
                .range((start, String::new(), Vec::new())..)
                .take_while(|((t, _, _), _)| *t == start)
                .count();
            if in_bucket >= self.max_series {
                // Bounded loss, recorded exactly — the rule retention
                // already follows.
                self.marks
                    .entry((start, s.metric.clone()))
                    .or_default()
                    .capped += 1;
                return;
            }
        }
        let b = self.buckets.entry(key).or_default();
        b.dirty = true;
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
    }

    /// The buckets `how` asks for, with whatever markers they owe.
    ///
    /// A sealed bucket is written once and evicted; an open one, under
    /// `Drain::Provisional`, is written as a revision and KEPT — see
    /// `Drain`.
    pub fn drain(&mut self, how: Drain) -> Vec<Sample> {
        let mut out = Vec::new();
        let mut evict: Vec<Key> = Vec::new();
        let mut clean: Vec<Key> = Vec::new();
        for (key, b) in self.buckets.iter() {
            let sealed = how == Drain::Final || self.sealed(key.0);
            if !sealed {
                // Unchanged since its last provisional line, so that
                // line still says what this one would.
                if how != Drain::Provisional || !b.dirty {
                    continue;
                }
                clean.push(key.clone());
            }
            let mut s = Sample::new(key.0, self.width_ms, &key.1);
            s.labels = key.2.clone();
            s.fields = b.fields.iter().map(|(f, (v, _))| (*f, *v)).collect();
            if let (Some(lo), Some(hi)) = (b.cite_lo, b.cite_hi) {
                s.cite = Some((lo, hi - lo));
            }
            out.push(s);
            if sealed {
                evict.push(key.clone());
            }
        }
        for k in evict {
            self.buckets.remove(&k);
        }
        for k in clean {
            if let Some(b) = self.buckets.get_mut(&k) {
                b.dirty = false;
            }
        }
        // ⚠ Markers are drained on SEALING only, provisional or not: a
        // `!cap` or `!late` count for an open bucket is still rising, and
        // a marker is not keyed by label set, so re-stating one would not
        // supersede its predecessor the way a bucket's own line does.
        let due: Vec<(u64, String)> = self
            .marks
            .keys()
            .filter(|(start, _)| how == Drain::Final || self.sealed(*start))
            .cloned()
            .collect();
        for k in due {
            let m = self.marks.remove(&k).expect("just listed");
            if m.capped > 0 {
                out.push(
                    Sample::new(k.0, self.width_ms, "!cap")
                        .label("metric", &k.1)
                        .field(Field::Count, m.capped as f64),
                );
            }
            if m.late > 0 {
                // A displacement is invisible in the line it lands in,
                // so it is stated beside it: how many, and the grace
                // that would have kept the worst of them.
                out.push(
                    Sample::new(k.0, self.width_ms, "!late")
                        .label("metric", &k.1)
                        .field(Field::Count, m.late as f64)
                        .field(Field::Max, m.late_max as f64),
                );
            }
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
    ///
    /// ⚠ The ONLY mechanism for lateness. An entry whose bucket has
    /// already sealed is displaced into the current one and counted in a
    /// `!late` marker; nothing re-opens a sealed bucket, so a position
    /// lags by this and no more.
    #[serde(default = "default_grace")]
    pub grace_ms: u64,
    /// Distinct series per bucket, after which a `!cap` marker counts
    /// what was lost. Cardinality is where every metrics system dies.
    #[serde(default = "default_max_series")]
    pub max_series: usize,
}

fn default_grace() -> u64 {
    120_000
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

impl Axis {
    /// The wire spelling. Reported rather than `{:?}`, which prints
    /// `Logline` — a Rust identifier nobody can type back into a
    /// document.
    pub fn as_str(self) -> &'static str {
        match self {
            Axis::Logline => "logline",
            Axis::Write => "write",
        }
    }
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
            roller: Roller::new(window.width_ms, window.grace_ms, window.max_series),
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
    /// Extractor documents, repeatable: a path, a directory of `*.json`,
    /// or a NAME resolved against the reading directories.
    pub extractors: Vec<PathBuf>,
    /// Where the site's extractors and provisionings live.
    pub etc: PathBuf,
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
    /// Write the numbers as columnar BLOCKS into this directory instead
    /// of as lines on stdout (docs/plans/tally-design.md).
    ///
    /// ⚠ One store in, one directory out, which is why it is on this
    /// path and not on `--run`: a provisioned run serves a SELECTION,
    /// one sink per source store, and where each one's blocks go is a
    /// provisioning question rather than a writer one.
    pub blocks: Option<PathBuf>,
    /// Samples buffered before a commit. The write-amplification
    /// control: a block is a day, so a commit rewrites up to a
    /// megabyte.
    pub block_flush: usize,
    pub block_buckets: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct FoldOpts {
    pub width_ms: u64,
    pub grace_ms: u64,
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
    let mut roller = Roller::new(o.width_ms, o.grace_ms, o.max_series);
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
        let _ = emit(out, None, roller.drain(Drain::Sealed))?;
    }
    let _ = emit(out, None, roller.drain(Drain::Final))?;
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

/// Where a READER looks for an extractor by name: the provisioning's
/// two, plus the user's own last, so a person can try a document without
/// root and shadow a shipped one while they do.
///
/// ⚠ Deliberately not the list a provisioning uses — see
/// `Provision::extractor_dirs`.
pub fn reading_dirs(etc: &Path) -> Vec<PathBuf> {
    let mut dirs = Provision::extractor_dirs(etc);
    if let Some(mine) = user_extractor_dir() {
        dirs.push(mine);
    }
    dirs
}

/// `$XDG_CONFIG_HOME/timberfs/tally.extractors.d`, else
/// `~/.config/timberfs/…` — where `targets.json` already lives.
fn user_extractor_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let dir = base.join("timberfs").join(EXTRACTOR_DIR);
    dir.is_dir().then_some(dir)
}

/// A path, or a NAME resolved against the reading directories.
///
/// An argument that exists as a file or a directory is taken as given;
/// anything else is a name — which is what makes a per-user directory
/// worth having rather than another place to type a long path from.
pub fn resolve_extractors(args: &[PathBuf], etc: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for arg in args {
        if arg.exists() {
            out.push(arg.clone());
            continue;
        }
        let dirs = reading_dirs(etc);
        let name = arg.to_string_lossy().to_string();
        // Reversed: the LAST directory shadows, which is the same rule a
        // same-named file already follows.
        match dirs
            .iter()
            .rev()
            .map(|d| d.join(format!("{name}.json")))
            .find(|p| p.is_file())
        {
            Some(p) => out.push(p),
            None => return Err(no_such_extractor(&name, &dirs, etc)),
        }
    }
    Ok(out)
}

/// Why a name resolved to nothing, in terms of where it was looked for.
///
/// Takes the directories rather than deriving them so it can be tested
/// against a list this host does not have: `PACKAGED_EXTRACTORS` exists
/// wherever the package is installed, which is most machines that run
/// the suite and none of the ones that used to.
fn no_such_extractor(name: &str, dirs: &[PathBuf], etc: &Path) -> anyhow::Error {
    // ⚠ A directory is listed only if it EXISTS, so where none does the
    // list is empty — and a resolution failure naming nowhere tells the
    // reader nothing about where to put the file.
    if dirs.is_empty() {
        return anyhow::anyhow!(
            "no extractor {name:?} — it is not a path that exists, and there is no \
             extractor directory to search: none of {PACKAGED_EXTRACTORS} (the timberfs \
             package), {} or ~/.config/timberfs/{EXTRACTOR_DIR} exists",
            etc.join(EXTRACTOR_DIR).display(),
        );
    }
    // ⚠ What is listed is the FILE STEM, because that is what this
    // lookup takes — a provisioning's APPLY names the DOCUMENT instead,
    // and the two can differ on a site's own file. Naming the document
    // here would print a word that does not resolve.
    let known = load_extractors(dirs)
        .map(|docs| {
            docs.iter()
                .map(|(p, d)| {
                    let stem = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if stem == d.name {
                        stem
                    } else {
                        format!("{stem} (the document {:?})", d.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    anyhow::anyhow!(
        "no extractor {name:?} — neither a path that exists nor a document in {}{}",
        dirs.iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        if known.is_empty() {
            ", which hold none".to_string()
        } else {
            format!(", which hold {known}")
        }
    )
}

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
        // ⚠ A metric is named once across the APPLIED set, not merely
        // within a document. Two documents may share a name — apache's
        // and nginx's `http_requests` are the same measurement — and
        // they are only wrong TOGETHER, where the two would fold into
        // one series and the numbers would be a sum of two different
        // things. Refused here, where the applied set is known, rather
        // than in the directory sweep, where holding both is right.
        let mut named: BTreeMap<&str, &Extractor> = BTreeMap::new();
        for (_, doc) in docs {
            for m in &doc.metrics {
                if let Some(other) = named.insert(&m.name, doc) {
                    if other.name != doc.name {
                        bail!(
                            "{} and {} both define the metric {:?} — applied together \
                             their samples fold into one series, and the numbers become \
                             a sum of two different measurements. Apply one",
                            other.name,
                            doc.name,
                            m.name
                        );
                    }
                }
            }
        }
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
        blocks: Option<&mut crate::tally_block::Writer>,
        observations: bool,
    ) -> anyhow::Result<Option<(u64, u64)>> {
        let mut batch: Vec<Sample> = Vec::new();
        let mut meta: Option<(u64, u64)> = None;
        let to_blocks = blocks.is_some();
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
                    // ⚠ Announced only on the LINE path. A block store
                    // carries the definitions, and a unit is a property
                    // of a definition — so writing it here would be the
                    // same fact in two places, one of them a marker the
                    // grid does not hold (docs/plans/tally-design.md).
                    if !l.announced && !to_blocks {
                        l.announced = true;
                        if let Some(u) = &l.unit {
                            meta = span(meta, Some((ts, ts)));
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
                batch.extend(l.roller.drain(Drain::Sealed));
            }
        }
        Ok(span(meta, emit(out, blocks, batch)?))
    }

    /// A quiet stream: let wall clock stand in for event time, write
    /// whatever that seals, and state the still-open buckets
    /// provisionally.
    ///
    /// ⚠ The provisional half is what surfaces a quiet store's NEWEST
    /// bucket, and nothing else can. Sealing it would need the watermark
    /// pushed past its end plus grace — i.e. past event time the data
    /// has not reached — and every entry arriving after that is then
    /// displaced out of its own bucket. So the newest bucket is shown
    /// rather than sealed, and superseded when it is complete.
    ///
    /// ⚠ Into BLOCKS this rests on `Block::merge` REPLACING a cell: a
    /// provisional bucket states its total so far, and the complete one
    /// later states the whole total again. When merge becomes additive
    /// for partials (docs/plans/tally-partials.md) this double-counts,
    /// and the third identity component that note asks for is what
    /// stops it.
    fn idle(
        &mut self,
        now_ms: u64,
        out: &mut impl Write,
        blocks: Option<&mut crate::tally_block::Writer>,
    ) -> anyhow::Result<Option<(u64, u64)>> {
        let mut batch: Vec<Sample> = Vec::new();
        for l in self.live.iter_mut() {
            if l.roller.watermark() == 0 {
                // Nothing has ever arrived, so there is no event time to
                // advance from and nothing could be sealed anyway.
                continue;
            }
            l.roller.advance_to(now_ms);
            batch.extend(l.roller.drain(Drain::Provisional));
        }
        emit(out, blocks, batch)
    }

    fn finish(
        &mut self,
        out: &mut impl Write,
        blocks: Option<&mut crate::tally_block::Writer>,
        observations: bool,
    ) -> anyhow::Result<Option<(u64, u64)>> {
        if observations {
            return Ok(None);
        }
        let mut batch: Vec<Sample> = Vec::new();
        for l in self.live.iter_mut() {
            batch.extend(l.roller.drain(Drain::Final));
        }
        emit(out, blocks, batch)
    }
}

pub fn cmd_tally(opts: &TallyOpts) -> anyhow::Result<()> {
    if let Some(fold) = &opts.fold {
        return cmd_fold(fold);
    }
    let docs = load_extractors(&resolve_extractors(&opts.extractors, &opts.etc)?)?;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    if opts.check {
        for (path, doc) in &docs {
            eprintln!(
                "{} — {} ({} metric(s), window {} {}, grace {})",
                doc.name,
                path.display(),
                doc.metrics.len(),
                render_width(doc.window.width_ms),
                doc.window.axis.as_str(),
                render_width(doc.window.grace_ms),
            );
            // Shown, because being SHOWN is the whole reason the format
            // has descriptions instead of comments.
            if let Some(d) = &doc.description {
                eprintln!("  {d}");
            }
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

    let mut blocks = match &opts.blocks {
        None => None,
        Some(dir) => {
            if opts.observations {
                bail!("--observations prints width-0s observations, which are not a grid");
            }
            Some(crate::tally_block::Writer::open(
                dir,
                width_of(&docs, opts)?,
                opts.block_buckets,
                opts.block_flush,
                3,
            )?)
        }
    };
    let stdin = std::io::stdin();
    let mut reader = crate::records::Reader::new(stdin.lock());
    let mut ended = false;
    while let Some(rec) = reader.next_rec()? {
        match rec {
            crate::records::Rec::Entry(e) => {
                run.feed(&e, axis, &mut out, blocks.as_mut(), opts.observations)?;
            }
            crate::records::Rec::End(_) => ended = true,
            _ => {}
        }
    }
    run.finish(&mut out, blocks.as_mut(), opts.observations)?;
    out.flush()?;
    if let Some(w) = &mut blocks {
        w.flush()?;
        crate::note!(
            "timberfs: {} sample(s) into {} block write(s){}",
            w.committed,
            w.blocks_written,
            if w.markers > 0 {
                format!("; {} marker(s) not stored", w.markers)
            } else {
                String::new()
            }
        );
    }
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
        run.feed(e, axis, &mut tally, None, opts.observations)?;
    }
    run.finish(&mut tally, None, opts.observations)?;
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
                "{} is on the {} axis and {} on the {} — one run reads one entry \
                 stream, so its extractors must agree",
                first_path.display(),
                first.window.axis.as_str(),
                path.display(),
                doc.window.axis.as_str()
            );
        }
    }
    Ok(first.window.axis)
}

/// Samples buffered before a commit, and buckets per block.
///
/// ⚠ The flush default is a WRITE-AMPLIFICATION choice, not a latency
/// one: a block is a day, so each commit rewrites up to a megabyte, and
/// a real day of a busy store is ~268,000 samples — so this is a
/// handful of rewrites a day rather than one per batch. A sample is not
/// in a block until it is flushed, which is the cost.
pub const DEFAULT_BLOCK_FLUSH: usize = 50_000;
/// A day at 60s, which two measurements settled
/// (docs/plans/tally-as-a-tally.md).
pub const DEFAULT_BLOCK_BUCKETS: usize = 1440;

/// The one bucket width a run makes, which a block store must hold.
///
/// ⚠ Refused rather than reconciled when the documents disagree: a
/// store holds one width, and combining two would be coarsening one of
/// them silently.
fn width_of(docs: &[(PathBuf, Extractor)], opts: &TallyOpts) -> anyhow::Result<u64> {
    if let Some(w) = opts.width_ms {
        return Ok(w);
    }
    let mut it = docs.iter();
    let (first_path, first) = it.next().expect("load_extractors refuses an empty set");
    for (path, doc) in it {
        if doc.window.width_ms != first.window.width_ms {
            bail!(
                "{} buckets at {} and {} at {} — one block store holds one width, \
                 so give --width to pick it",
                first_path.display(),
                render_width(first.window.width_ms),
                path.display(),
                render_width(doc.window.width_ms)
            );
        }
    }
    Ok(first.window.width_ms)
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
///
/// Reports the BUCKET WINDOW it wrote, which is what stamps the chunk:
/// a tally store's write axis is the minutes its lines are about.
fn emit(
    out: &mut impl Write,
    blocks: Option<&mut crate::tally_block::Writer>,
    mut batch: Vec<Sample>,
) -> anyhow::Result<Option<(u64, u64)>> {
    batch.sort_by(|a, b| (a.ts, &a.metric, &a.labels).cmp(&(b.ts, &b.metric, &b.labels)));
    let window = match (batch.first(), batch.last()) {
        (Some(f), Some(l)) => Some((f.ts, l.ts)),
        _ => None,
    };
    // ⚠ One destination or the other, never both: the position this
    // returns is what advances a consumer, and a sample counted twice
    // by a downstream that reads the lines AND the blocks would be
    // double-counted rather than merged.
    match blocks {
        Some(w) => w.take(batch)?,
        None => {
            for s in batch {
                writeln!(out, "{}", s.render())?;
            }
        }
    }
    Ok(window)
}

/// Two windows as one, either of which may be absent.
fn span(a: Option<(u64, u64)>, b: Option<(u64, u64)>) -> Option<(u64, u64)> {
    match (a, b) {
        (Some((af, al)), Some((bf, bl))) => Some((af.min(bf), al.max(bl))),
        (some, None) | (None, some) => some,
    }
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

// ------------------------------------------------------------ provisioning

/// Where a site declares which stores get a tally store, under
/// `/etc/timberfs`.
pub const PROVISION_DIR: &str = "tally.d";

/// Where extractor documents are looked up by NAME, in order — the
/// packaged set first, the site's second, so a same-named FILE in `/etc`
/// shadows the packaged one.
pub const EXTRACTOR_DIR: &str = "tally.extractors.d";
pub const PACKAGED_EXTRACTORS: &str = "/usr/lib/timberfs/tally.extractors.d";

const SELECT: &str = "SELECT";
const OUTPUT: &str = "OUTPUT";
const APPLY: &str = "APPLY";
const DECLARE: &str = "DECLARE";
const STORE_DIR: &str = "STORE_DIR";
const WIDTH: &str = "WIDTH";
const FROM: &str = "FOLLOW_FROM";

const PROVISION_KEYS: &[&str] = &[SELECT, OUTPUT, APPLY, DECLARE, STORE_DIR, WIDTH, FROM];

/// Just enough of a store to fill a template from a `source` record.
struct Named<'a> {
    handle: &'a str,
}

/// One provisioning: which stores get a tally store, named how, declaring
/// what, measured by which extractors.
///
/// ⚠ FLAT — one provisioning per file, where `file.d` has a section per
/// store. A section there names one store; here the SELECTION already
/// names a set, so a second section would only be a second selection,
/// which is a second file and a second unit. That also keeps the follower
/// simple: one selection, one process, one `timberfs-follower@tally-<set>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Provision {
    /// The file's name, which is the unit instance and the follower's.
    pub set: String,
    pub select: String,
    /// A template over the SOURCE store's facts: `{name}`, `{host}`,
    /// `{service}`, `{id}`.
    pub output: String,
    /// Extractor documents, by name.
    pub apply: Vec<String>,
    /// Declared on every tally store this provisioning creates, as
    /// KEY=VALUE pairs — the same string `file.d` takes.
    pub declare: Vec<String>,
    pub store_dir: PathBuf,
    pub width_ms: Option<u64>,
    /// Where a store this provisioning has never measured is picked up.
    ///
    /// ⚠ `begin` by default, where a follower's own default is
    /// `discovery`. A metric that can be computed over the log you
    /// ALREADY HAVE is the property this whole thing is for, and
    /// `discovery` would silently skip it for every store older than the
    /// provisioning — which is every store, the first time. The cost is
    /// one pass over what the source still holds; `FOLLOW_FROM=end` is
    /// for somebody who does not want it.
    pub follow_from: crate::ship::FollowFrom,
}

impl Provision {
    pub fn parse(set: &str, text: &str) -> anyhow::Result<Provision> {
        let mut select: Option<String> = None;
        let mut output = "{name}-tally".to_string();
        let mut apply: Vec<String> = Vec::new();
        let mut declare: Vec<String> = Vec::new();
        let mut store_dir = PathBuf::from("/var/log/timberfs");
        let mut width_ms: Option<u64> = None;
        let mut follow_from = crate::ship::FollowFrom::Begin;

        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            let t = raw.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if t.starts_with('[') {
                bail!(
                    "{set}.conf:{line}: a provisioning has no sections — one selection per                      file, so a second is a second file and a second unit"
                );
            }
            let Some((key, value)) = t.split_once('=') else {
                bail!("{set}.conf:{line}: expected KEY=VALUE — got {t:?}");
            };
            let (key, value) = (key.trim(), value.trim());
            if !PROVISION_KEYS.contains(&key) {
                bail!(
                    "{set}.conf:{line}: unknown key {key:?} — this build reads {}",
                    PROVISION_KEYS.join(", ")
                );
            }
            match key {
                SELECT => {
                    select = Some(
                        crate::select::canonical(value)
                            .with_context(|| format!("{set}.conf:{line}: {SELECT}"))?,
                    )
                }
                OUTPUT => output = value.to_string(),
                APPLY => apply = value.split_whitespace().map(str::to_string).collect(),
                DECLARE => declare = value.split_whitespace().map(str::to_string).collect(),
                STORE_DIR => store_dir = PathBuf::from(value),
                FROM => {
                    follow_from = match value {
                        "begin" => crate::ship::FollowFrom::Begin,
                        "end" => crate::ship::FollowFrom::End,
                        "discovery" => crate::ship::FollowFrom::Discovery,
                        other => {
                            bail!("{set}.conf:{line}: {FROM}={other:?} is begin, end or discovery")
                        }
                    }
                }
                _ => {
                    let ms = crate::append::parse_duration_ms(value)
                        .with_context(|| format!("{set}.conf:{line}: {WIDTH}"))?;
                    if ms == 0 || !ms.is_multiple_of(1000) {
                        bail!("{set}.conf:{line}: {WIDTH} is whole seconds and not zero");
                    }
                    width_ms = Some(ms)
                }
            }
        }

        let Some(select) = select else {
            bail!("{set}.conf: no {SELECT} — a provisioning that names no stores measures none");
        };
        if apply.is_empty() {
            bail!(
                "{set}.conf: no {APPLY} — name the extractor documents to measure with,                  e.g. `{APPLY}=timberfs-apache-combined timberfs-volume`"
            );
        }
        if !output.contains("{name}") && !output.contains("{id}") {
            // Every source store would otherwise map to ONE output, which
            // is several writers on one store — refused later, but the
            // template is where the mistake was made.
            bail!(
                "{set}.conf: {OUTPUT}={output:?} names the same store for every source \
                 — it must vary, so use {{name}} or {{id}}"
            );
        }
        for kv in &declare {
            if !kv.contains('=') {
                bail!("{set}.conf: {DECLARE} takes KEY=VALUE pairs — got {kv:?}");
            }
        }
        Ok(Provision {
            set: set.to_string(),
            select,
            output,
            apply,
            declare,
            store_dir,
            width_ms,
            follow_from,
        })
    }

    pub fn load(etc: &Path, set: &str) -> anyhow::Result<Provision> {
        let path = etc.join(PROVISION_DIR).join(format!("{set}.conf"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Provision::parse(set, &text)
    }

    /// The follower this provisioning registers. Derived, never typed:
    /// the file is the interface and the registration is state.
    pub fn follower_name(&self) -> String {
        format!("tally-{}", self.set)
    }

    /// ⚠ `class!=tally` is folded in here rather than left to an
    /// operator. A tally store inherits its source's provenance, so a
    /// selection like `[service=~apache-.*]` matches the tally store it
    /// just created, whose every line a volume metric then claims,
    /// producing another store one level deeper, forever. Measuring a
    /// tally store is a ROLLUP, which is a different verb.
    pub fn selector(&self) -> anyhow::Result<String> {
        let inner = self.select.trim_start_matches('[').trim_end_matches(']');
        Ok(if inner.trim().is_empty() {
            "[class=]".to_string()
        } else {
            format!("[{inner},class=]")
        })
    }

    /// Where the extractors named by `APPLY` are looked up.
    ///
    /// ⚠ NEVER a home directory. A provisioning runs as a service — it
    /// creates stores and registers followers — so resolving a name
    /// against `~` would make what a service does depend on whose home
    /// it happened to look in, and on a shared machine would let one
    /// user shadow a shipped extractor for a root-run provisioning.
    /// `reading_dirs` is the other list, for a person asking a question.
    pub fn extractor_dirs(etc: &Path) -> Vec<PathBuf> {
        vec![PathBuf::from(PACKAGED_EXTRACTORS), etc.join(EXTRACTOR_DIR)]
            .into_iter()
            .filter(|p| p.is_dir())
            .collect()
    }

    /// The extractors this provisioning applies, in the order named.
    pub fn extractors(&self, etc: &Path) -> anyhow::Result<Vec<Extractor>> {
        let dirs = Provision::extractor_dirs(etc);
        if dirs.is_empty() {
            bail!(
                "no extractor directory to search — expected {PACKAGED_EXTRACTORS} \
                 or {}",
                etc.join(EXTRACTOR_DIR).display()
            );
        }
        let available = load_extractors(&dirs)?;
        self.apply
            .iter()
            .map(|want| {
                available
                    .iter()
                    .find(|(_, d)| d.name == *want)
                    .map(|(_, d)| d.clone())
                    .with_context(|| {
                        format!(
                            "{}.conf: no extractor named {want:?} in {} — {APPLY} names                              DOCUMENTS, and this build found {}",
                            self.set,
                            dirs.iter()
                                .map(|d| d.display().to_string())
                                .collect::<Vec<_>>()
                                .join(" and "),
                            available
                                .iter()
                                .map(|(_, d)| d.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
            })
            .collect()
    }

    /// The name of the tally store for one source, from the template.
    pub fn output_for(&self, m: &crate::select::Match) -> anyhow::Result<String> {
        let labels = crate::select::selectable_of(&m.dir, &m.name);
        self.output_from(&m.handle, &labels, m.id.as_deref())
    }

    /// The same, from what a `source` record carries — a running consumer
    /// has labels and a path, never a `Match`.
    pub fn output_from(
        &self,
        handle: &str,
        labels: &Map<String, Value>,
        id: Option<&str>,
    ) -> anyhow::Result<String> {
        let m = Named { handle };
        // ⚠ An EMPTY substitution is refused rather than left as a hole.
        // `{host}` on a store that declares none produced
        // `.apache-access-tally` — a hidden directory — and a template
        // of nothing but absent fields would collapse every source onto
        // one store, which is several writers on one tally store.
        let field = |k: &str, from: &str| -> anyhow::Result<String> {
            let v = labels.get(k).and_then(|v| v.as_str()).unwrap_or_default();
            if v.is_empty() {
                bail!(
                    "{}.conf: {OUTPUT}={:?} wants {{{k}}}, and {} declares none",
                    self.set,
                    self.output,
                    from
                );
            }
            Ok(v.to_string())
        };
        let mut out = self.output.replace("{name}", m.handle);
        if out.contains("{host}") {
            out = out.replace("{host}", &field("host", handle)?);
        }
        if out.contains("{service}") {
            out = out.replace("{service}", &field("service", handle)?);
        }
        if out.contains("{id}") {
            let Some(id) = id.filter(|i| !i.is_empty()) else {
                bail!(
                    "{}.conf: {OUTPUT}={:?} wants {{id}}, and {handle} has no identity — \
                     `timberfs identity <store> --mint` gives it one",
                    self.set,
                    self.output
                );
            };
            out = out.replace("{id}", id);
        }
        if out.contains('{') || out.contains('}') {
            bail!(
                "{}.conf: {OUTPUT}={:?} leaves a placeholder unfilled for {} — the fields                  are {{name}}, {{host}}, {{service}} and {{id}}",
                self.set,
                self.output,
                m.handle
            );
        }
        // A handle names a directory and a file, so it takes the
        // character set a store name already has to survive being one.
        if out.is_empty()
            || !out
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            bail!(
                "{}.conf: {OUTPUT} produced {out:?} for {}, which is not a store name \
                 (letters, digits, _ - and .)",
                self.set,
                m.handle
            );
        }
        Ok(out)
    }
}

/// What a provisioning would do to one source store.
pub struct Planned {
    pub source: crate::select::Match,
    pub output: String,
    pub dest: PathBuf,
    /// The bark this tally store should declare.
    pub declare: Vec<String>,
    /// Absent when the store is already there, so a run says CREATE or
    /// EXISTS rather than doing both silently.
    pub exists: bool,
    /// Declared keys whose value on disk differs from what this
    /// provisioning says. Reported, never rewritten: somebody ran
    /// `timberfs set` and meant it.
    pub drift: Vec<(String, String, String)>,
}

/// Resolve a provisioning against the stores that are actually there.
///
/// ⚠ Every source is checked for an OUTPUT collision, not for a
/// SELECT overlap. Two provisionings may cover one store as long as they
/// produce different tally stores; two producing the same one is two
/// writers. Overlapping selections are not decidable in general, and two
/// names being equal is.
pub fn plan(p: &Provision, etc: &Path, dirs: &[PathBuf]) -> anyhow::Result<Vec<Planned>> {
    // Resolved for its refusals: a provisioning naming an extractor this
    // host does not have is a plan that cannot run.
    let _ = p.extractors(etc)?;
    let sel = crate::select::Selector::parse(&p.selector()?)?;
    let mut out: Vec<Planned> = Vec::new();
    let mut claimed: BTreeMap<String, String> = BTreeMap::new();
    for m in crate::select::resolve(dirs, &sel) {
        let name = p.output_for(&m)?;
        if let Some(other) = claimed.insert(name.clone(), m.handle.clone()) {
            bail!(
                "{}.conf: {} and {} both produce {name} — one tally store, two writers. \
                 {OUTPUT} must vary with the source",
                p.set,
                other,
                m.handle
            );
        }
        let dest = p.store_dir.join(&name).join(format!("{name}.log"));

        // Inherited provenance, then what this provisioning declares,
        // then the facts only the provisioning knows.
        let source_bark = crate::bark::load(&m.dir, &m.name).unwrap_or_default();
        let mut declare: Vec<String> = crate::bark::provenance(&source_bark)
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|v| format!("{k}={v}")))
            .collect();
        declare.push("class=tally".to_string());
        declare.push("derived_op=tally".to_string());
        if let Some(id) = &m.id {
            declare.push(format!("derived_from={id}"));
        }
        declare.push("wal=true".to_string());
        for kv in &p.declare {
            let key = kv.split_once('=').map(|(k, _)| k).unwrap_or(kv);
            declare.retain(|had| had.split_once('=').map(|(k, _)| k) != Some(key));
            declare.push(kv.clone());
        }
        declare.sort();

        let (dir, base) = (
            dest.parent().expect("a dest has a parent").to_path_buf(),
            format!("{name}.log"),
        );
        let exists = crate::format::rings_path(&dir, &base).exists();
        let mut drift = Vec::new();
        if exists {
            let have = crate::bark::load(&dir, &base).unwrap_or_default();
            for kv in &declare {
                let Some((k, want)) = kv.split_once('=') else {
                    continue;
                };
                let is = have.get(k).map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                });
                if is.as_deref() != Some(want) {
                    drift.push((
                        k.to_string(),
                        is.unwrap_or_else(|| "(absent)".to_string()),
                        want.to_string(),
                    ));
                }
            }
        }
        out.push(Planned {
            source: m,
            output: name,
            dest,
            declare,
            exists,
            drift,
        });
    }
    Ok(out)
}

pub struct ProvisionOpts {
    pub etc: PathBuf,
    pub dry_run: bool,
    /// Forests to resolve the selection against; empty means every
    /// configured one.
    pub forest: Vec<PathBuf>,
}

/// `timberfs tally --provision <set>`: declare, converge, and say what
/// resolved — the shape `file-intake --check` has.
///
/// ⚠ Converges and never cascades. A source store appearing gets its
/// tally store; a source store being DELETED does not take its tally
/// store, because outliving the log is the entire point.
pub fn cmd_provision(set: &str, opts: &ProvisionOpts) -> anyhow::Result<()> {
    let p = Provision::load(&opts.etc, set)?;
    let extractors = p.extractors(&opts.etc)?;
    let dirs = if opts.forest.is_empty() {
        crate::forest::forest_dirs()
    } else {
        opts.forest.clone()
    };
    let planned = plan(&p, &opts.etc, &dirs)?;

    println!("{set} — {} ({})", p.select, p.follower_name());
    println!(
        "  measure  {}",
        extractors
            .iter()
            .map(|e| format!(
                "{} ({})",
                e.name,
                e.metrics
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .collect::<Vec<_>>()
            .join("; ")
    );
    if planned.is_empty() {
        // "Matched nothing" and "nothing was searched" are different
        // answers, so the searched set is named.
        println!(
            "  stores   none — {} searched {} director{}",
            p.select,
            dirs.len(),
            if dirs.len() == 1 { "y" } else { "ies" }
        );
    }
    for pl in &planned {
        let what = if pl.exists { "exists" } else { "CREATE" };
        println!(
            "  {:<8} {} -> {}",
            what,
            pl.source.handle,
            pl.dest.display()
        );
        for (k, is, want) in &pl.drift {
            // Reported, never rewritten: somebody ran `timberfs set`.
            println!("           ⚠ {k} is {is}, this provisioning says {want}");
        }
        if !pl.exists && !opts.dry_run {
            crate::bark::cmd_create(&pl.dest, false, false, None, None, false, &pl.declare, true)?;
        }
    }

    if opts.dry_run {
        println!("  follower {} would be registered", p.follower_name());
        return Ok(());
    }
    register_follower(&p, &opts.etc)?;
    println!("  follower {} registered", p.follower_name());
    Ok(())
}

/// The follower, derived from the file and kept in step with it.
///
/// The operator writes neither its selection nor its command: the file is
/// the interface and the registration is STATE — which is also why there
/// is no drift to detect between them.
fn register_follower(p: &Provision, etc: &Path) -> anyhow::Result<()> {
    let name = p.follower_name();
    // ⚠ The RUNTIME, not this verb. `--provision` converges and exits;
    // what a follower execs must consume.
    let mut command = vec![
        "timberfs".to_string(),
        "tally".to_string(),
        "--run".to_string(),
        p.set.clone(),
    ];
    // ⚠ Carried when it is not the default, or a provisioning converged
    // elsewhere registers a follower that cannot find its own file — and
    // finds that out at the far end of a systemd unit.
    if etc != Path::new("/etc/timberfs") {
        command.push("--etc".to_string());
        command.push(etc.display().to_string());
    }
    let select = p.selector()?;
    let reg = crate::follower::registry_dir();
    match crate::follower::Declaration::load(&reg, &name) {
        Ok(mut have) => {
            if have.select == select && have.command == command {
                return Ok(());
            }
            have.select = select;
            have.command = command;
            have.save(&reg)
        }
        Err(_) => crate::follower::cmd_create(
            &name,
            crate::follower::CreateOpts {
                select: Some(select),
                store: None,
                retaining: false,
                follow_from: Some(p.follow_from),
                enable: false,
                start: false,
                dry_run: false,
                command,
            },
        ),
    }
}

// ---------------------------------------------------------------- the run

/// One source store being measured: its output store, held open with the
/// writer's lock, and the metrics folding into it.
struct Sink {
    name: String,
    /// Held for as long as this runs. One writer per store is the rule
    /// every writer in this tree follows.
    _lock: std::fs::File,
    store: crate::store::Store,
    run: Run,
    /// The end of the last entry delivered, which is where a position
    /// goes when nothing is open — and what is reported as `taken`
    /// meanwhile, so the follower keeps feeding while a bucket is held.
    delivered_to: Option<u64>,
    reported: Option<u64>,
    reported_taken: Option<u64>,
}

impl Sink {
    /// What this store's position may safely be moved to.
    ///
    /// ⚠ The oldest byte any OPEN bucket depends on, and only otherwise
    /// the last byte delivered. A bucket that has not sealed can still
    /// change, and a restart must be able to re-derive it — which it can,
    /// because re-reading those entries produces the identical lines.
    fn watermark(&self) -> Option<u64> {
        self.run
            .live
            .iter()
            .filter_map(|l| l.roller.safe_offset())
            .min()
            .or(self.delivered_to)
    }

    fn write(&mut self, lines: &[u8], window: Option<(u64, u64)>) -> anyhow::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let Some((first_ms, last_ms)) = window else {
            bail!("tally lines carrying no bucket window — every rendered line has a stamp");
        };
        let cfg = self.store.cfg;
        let f = self
            .store
            .files
            .get_mut(&self.name)
            .expect("the store was created with this name");
        // ⚠ Stamped with the BUCKETS, not the clock: a tally store's two
        // axes are the same minutes, so a logline-time query selects
        // chunks exactly. Stamping the moment of computation instead put
        // the axes GRACE apart and made every such query miss its own
        // chunks. Both receive intakes stamp the sender's event time for
        // the same reason.
        f.append_windowed(lines, first_ms, last_ms, &cfg)?;
        // ⚠ Flushed here rather than left to an age timer this loop does
        // not run: a tally batch is one bucket sealing, arriving a
        // minute apart, so nothing would become a chunk — and a minute's
        // numbers would sit unqueryable in a buffer until the next one.
        f.flush_chunk(&cfg)?;
        Ok(())
    }
}

impl TallyOpts {
    /// What a `--run` needs of them: no extractor paths (the provisioning
    /// names its documents), no metric narrowing, and the width it
    /// declares.
    fn for_run(width_ms: Option<u64>) -> TallyOpts {
        TallyOpts {
            extractors: Vec::new(),
            etc: PathBuf::from("/etc/timberfs"),
            try_it: false,
            check: false,
            observations: false,
            metrics: Vec::new(),
            width_ms,
            fold: None,
            // A provisioned run writes its sinks' tapes; where each
            // sink's blocks would go is the provisioning's question.
            blocks: None,
            block_flush: DEFAULT_BLOCK_FLUSH,
            block_buckets: DEFAULT_BLOCK_BUCKETS,
        }
    }
}

/// How long a run waits to hear anything before letting wall clock stand
/// in for event time. Shorter than any sensible `grace`, so it is the
/// clock that ticks rather than the thing that decides.
const IDLE_TICK: std::time::Duration = std::time::Duration::from_secs(2);

pub struct RunOpts {
    pub etc: PathBuf,
    /// Where a tally store is created if the provisioning has not run —
    /// which it may not have, for a store that appeared since.
    pub create: bool,
}

/// `timberfs tally --run <set>`: the CONSUMER a tally follower execs.
///
/// Reads `timberfs-records(5)` on stdin, writes tally lines into one
/// store per source, and reports a watermark per source on stdout. It
/// never writes tally lines to stdout: that channel belongs to the
/// consumer protocol.
pub fn cmd_run(set: &str, opts: &RunOpts) -> anyhow::Result<()> {
    let p = Provision::load(&opts.etc, set)?;
    let docs: Vec<(PathBuf, Extractor)> = p
        .extractors(&opts.etc)?
        .into_iter()
        .map(|d| (PathBuf::from(format!("{}.conf", p.set)), d))
        .collect();
    let axis = axis_of(&docs)?;

    let stdout = std::io::stdout();
    let mut reports = stdout.lock();
    // Every consumer declares itself before it is fed anything.
    write!(reports, "\x1ehello\x1fv=1\x1freads=records\0")?;
    reports.flush()?;

    // Read on a thread so the main loop can notice SILENCE, which is
    // what a quiet log looks like and what nothing else here can see.
    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<Option<crate::records::Rec>>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = crate::records::Reader::new(stdin.lock());
        loop {
            match reader.next_rec() {
                Ok(Some(r)) => {
                    if tx.send(Ok(Some(r))).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(Ok(None));
                    break;
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });

    let mut sinks: BTreeMap<String, Sink> = BTreeMap::new();
    let mut known: BTreeMap<String, (String, Map<String, Value>)> = BTreeMap::new();
    let mut ended = false;

    loop {
        let rec = match rx.recv_timeout(IDLE_TICK) {
            Ok(Ok(Some(r))) => r,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(e),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let now = crate::store::now_ms();
                for (id, sink) in sinks.iter_mut() {
                    let mut lines: Vec<u8> = Vec::new();
                    let window = sink.run.idle(now, &mut lines, None)?;
                    sink.write(&lines, window)?;
                    report(&mut reports, id, sink)?;
                }
                continue;
            }
        };
        match rec {
            crate::records::Rec::Source(fields) => {
                let get = |k: &str| fields.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
                let (Some(id), Some(path)) = (get("id"), get("path")) else {
                    continue;
                };
                let labels = get("labels")
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .and_then(|v| match v {
                        Value::Object(m) => Some(m),
                        _ => None,
                    })
                    .unwrap_or_default();
                let handle = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| crate::forest::handle_of_logical(n).to_string())
                    .unwrap_or(path);
                known.insert(id, (handle, labels));
            }
            crate::records::Rec::Entry(e) => {
                let Some(id) = e.id.clone() else {
                    bail!(
                        "an entry with no store identity — a tally run writes one store per \
                         SOURCE store, so it cannot place an entry it cannot attribute"
                    );
                };
                if !sinks.contains_key(&id) {
                    let (handle, labels) = known
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| (id.clone(), Map::new()));
                    let sink = open_sink(&p, &docs, &id, &handle, &labels, opts)?;
                    sinks.insert(id.clone(), sink);
                }
                let sink = sinks.get_mut(&id).expect("just inserted");
                let mut lines: Vec<u8> = Vec::new();
                let window = sink.run.feed(&e, axis, &mut lines, None, false)?;
                sink.write(&lines, window)?;
                if let Some(off) = e.offset {
                    sink.delivered_to = Some(off + e.payload.len() as u64);
                }
                report(&mut reports, &id, sink)?;
            }
            crate::records::Rec::End(_) => ended = true,
            _ => {}
        }
    }

    // End of the feed: seal everything held, so a stopped follower leaves
    // no half-counted minute behind.
    for (id, sink) in sinks.iter_mut() {
        let mut lines: Vec<u8> = Vec::new();
        let window = sink.run.finish(&mut lines, None, false)?;
        sink.write(&lines, window)?;
        let cfg = sink.store.cfg;
        if let Some(f) = sink.store.files.get_mut(&sink.name) {
            f.flush_chunk(&cfg)?;
        }
        sink.reported = None;
        report(&mut reports, id, sink)?;
    }
    reports.flush()?;
    if !ended {
        bail!("the record stream ended without stream-end — the answer is truncated");
    }
    Ok(())
}

/// A position moves only when it has somewhere new to go: a report per
/// entry would be one write per entry for a number that changes once a
/// bucket.
fn report(out: &mut impl Write, id: &str, sink: &mut Sink) -> anyhow::Result<()> {
    let Some(at) = sink.watermark() else {
        return Ok(());
    };
    // ⚠ Both numbers, and the second is what keeps this fed. `offset` is
    // where a restart may resume — behind every open bucket, because
    // those are re-derived from the source bytes rather than persisted —
    // and a follower reads a position as flow control too, so reporting
    // only that one parked the store on the oldest byte the oldest open
    // bucket depended on and then waited for the entries that would have
    // sealed it. `taken` says how far this has actually READ. See
    // consumer.rs and docs/plans/consumer-holding.md.
    let took = sink.delivered_to;
    if sink.reported == Some(at) && sink.reported_taken == took {
        return Ok(());
    }
    sink.reported = Some(at);
    sink.reported_taken = took;
    write!(out, "\x1eprogress\x1fid={id}\x1foffset={at}")?;
    if let Some(took) = took.filter(|t| *t > at) {
        write!(out, "\x1ftaken={took}")?;
    }
    out.write_all(b"\0")?;
    out.flush()?;
    Ok(())
}

/// Open — creating if the provisioning has not caught up — the tally
/// store one source writes into.
fn open_sink(
    p: &Provision,
    docs: &[(PathBuf, Extractor)],
    id: &str,
    handle: &str,
    labels: &Map<String, Value>,
    opts: &RunOpts,
) -> anyhow::Result<Sink> {
    let output = p.output_from(handle, labels, Some(id))?;
    let dest = p.store_dir.join(&output).join(format!("{output}.log"));
    let dir = dest.parent().expect("a dest has a parent").to_path_buf();
    let _ = &output;
    let name = format!("{output}.log");

    if !crate::format::rings_path(&dir, &name).exists() {
        if !opts.create {
            bail!(
                "no tally store {} for {handle} — run `timberfs tally --provision {}` \
                 first",
                dest.display(),
                p.set
            );
        }
        // A store that appeared since the provisioning last ran is the
        // case this exists for: an intake mints stores on first sight,
        // and a metric that waited for a human would miss the day.
        let declare = declared_for(p, id, labels);
        crate::bark::cmd_create(&dest, false, false, None, None, false, &declare, true)?;
    }

    let cfg = crate::store::Config {
        chunk_size: 256 * 1024,
        level: 3,
        // A tally line is small and rare; waiting five seconds for a
        // chunk would leave a minute's numbers unqueryable for no gain.
        flush_age_ms: 60_000,
    };
    let lock = crate::append::take_writer_lock(&dir, &name, 0.0)?
        .with_context(|| crate::append::writer_conflict(&dir, &name, 0.0))?;
    let mut store = crate::store::Store {
        dir: dir.clone(),
        cfg,
        files: BTreeMap::new(),
    };
    store.create(&name)?;

    Ok(Sink {
        name,
        _lock: lock,
        store,
        run: Run::new(docs, &TallyOpts::for_run(p.width_ms))?,
        delivered_to: None,
        reported: None,
        reported_taken: None,
    })
}

/// The bark a tally store declares: its source's provenance, the facts
/// only a provisioning knows, then whatever it declares itself.
fn declared_for(p: &Provision, id: &str, labels: &Map<String, Value>) -> Vec<String> {
    let mut declare: Vec<String> = crate::bark::provenance(labels)
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|v| format!("{k}={v}")))
        .collect();
    declare.push("class=tally".to_string());
    declare.push("derived_op=tally".to_string());
    declare.push(format!("derived_from={id}"));
    // ⚠ A tally store is worth a WAL where a log may not be: recomputing
    // a lost minute means re-reading the source from a reset position,
    // and losing one silently is worse than the second of fsync it costs
    // on a store that takes a handful of lines a minute.
    declare.push("wal=true".to_string());
    for kv in &p.declare {
        let key = kv.split_once('=').map(|(k, _)| k).unwrap_or(kv);
        declare.retain(|had| had.split_once('=').map(|(k, _)| k) != Some(key));
        declare.push(kv.clone());
    }
    declare.sort();
    declare
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(line: &str) -> Sample {
        Sample::parse(line).expect(line)
    }

    fn stamp(t: &str) -> u64 {
        parse_stamp(t).expect(t)
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
    fn a_late_entry_is_displaced_into_the_current_bucket() {
        // ⚠ Lateness is a DISPLACEMENT, never a loss: a count metric's
        // sum over a window still equals the number of entries. What it
        // costs is that the entry lands under a stamp that is not when
        // the event happened, which is why the marker states it.
        let mut r = Roller::new(60_000, 30_000, 1000);
        r.add(&s("2026-09-06T13:37:10.000Z 0s m count=1"));
        assert!(
            r.drain(Drain::Sealed).is_empty(),
            "not sealed while the grace stands"
        );

        r.add(&s("2026-09-06T13:39:00.000Z 0s m count=1"));
        let sealed = r.drain(Drain::Sealed);
        assert_eq!(
            sealed.iter().map(|s| s.render()).collect::<Vec<_>>(),
            vec!["2026-09-06T13:37:00.000Z 60s m count=1"]
        );

        // Its own bucket is gone, so it joins the one the watermark is
        // in — and nothing re-opens what was already written.
        r.add(&s("2026-09-06T13:37:30.000Z 0s m count=1"));
        let out = r.drain(Drain::Final);
        let rendered: Vec<String> = out.iter().map(|s| s.render()).collect();
        assert!(
            rendered.contains(&"2026-09-06T13:39:00.000Z 60s m count=2".to_string()),
            "{rendered:?}"
        );
        // 13:37:30's bucket ended at 13:38:00 and the watermark was
        // 13:39:00, so a grace of 60s would have kept it.
        assert!(
            rendered.contains(
                &"2026-09-06T13:39:00.000Z 60s !late metric=m count=1 max=60000".to_string()
            ),
            "{rendered:?}"
        );
        assert!(
            !rendered
                .iter()
                .any(|l| l.starts_with("2026-09-06T13:37:00")),
            "a sealed bucket is never written twice: {rendered:?}"
        );
    }

    #[test]
    fn a_batch_fold_never_displaces() {
        // Coarsening is handed a CLOSED set, so it must not reorder its
        // input into "the current bucket" the way a stream legitimately
        // does — a grace of u64::MAX is what keeps a batch a batch.
        let out = Roller::fold(
            60_000,
            &[
                s("2026-09-06T13:39:00.000Z 0s m count=1"),
                s("2026-09-06T13:37:00.000Z 0s m count=1"),
            ],
        );
        assert_eq!(out.len(), 2, "two buckets, neither displaced");
        assert!(out.iter().all(|s| !s.is_marker()), "{out:?}");
    }

    #[test]
    fn a_bucket_holds_the_oldest_byte_it_still_depends_on() {
        // What a consumer's watermark may not pass: a bucket that can
        // still be revised must be reconstructible after a restart.
        let mut r = Roller::new(60_000, u64::MAX, 1000);
        let mut a = s("2026-09-06T13:37:10.000Z 0s m count=1");
        a.cite = Some((4096, 100));
        let mut b = s("2026-09-06T13:37:20.000Z 0s m count=1");
        b.cite = Some((512, 100));
        r.add(&a);
        r.add(&b);
        assert_eq!(r.safe_offset(), Some(512));
        assert_eq!(
            r.drain(Drain::Final)[0].render(),
            "2026-09-06T13:37:00.000Z 60s m count=2 @512+3684"
        );
        assert_eq!(r.safe_offset(), None, "nothing held, nothing to hold back");
    }

    #[test]
    fn the_series_cap_is_recorded_rather_than_silent() {
        let mut r = Roller::new(60_000, u64::MAX, 2);
        for i in 0..5 {
            r.add(
                &Sample::new(1_788_700_000_000, 0, "m")
                    .label("id", &i.to_string())
                    .field(Field::Count, 1.0),
            );
        }
        let out = r.drain(Drain::Final);
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
        let mut r = Roller::new(60_000, u64::MAX, 1000);
        note_drop(&mut r, "m", "unreadable", Some(1_788_700_000_000));
        let out = r.drain(Drain::Final);
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
            // Nothing seals until end of input, so a test's ordering is
            // its own business rather than the roller's.
            grace_ms: u64::MAX,
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
    fn a_provisioning_never_resolves_a_name_against_a_home_directory() {
        // ⚠ A provisioning runs as a service — it creates stores and
        // registers followers — so what it does must not depend on
        // whose home it looked in, and on a shared machine one user
        // must not be able to shadow a shipped extractor for it.
        let home = tempdir();
        let mine = home.join("timberfs").join(EXTRACTOR_DIR);
        std::fs::create_dir_all(&mine).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &home);

        let etc = tempdir();
        let reading = reading_dirs(&etc);
        assert!(reading.contains(&mine), "a reader looks there: {reading:?}");
        assert!(
            !Provision::extractor_dirs(&etc).contains(&mine),
            "a provisioning does not"
        );

        std::env::remove_var("XDG_CONFIG_HOME");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&etc).ok();
    }

    #[test]
    fn the_shipped_provisioning_example_parses() {
        // An example nobody can copy is worse than none: it is read as
        // the syntax and then blamed on the parser.
        let text = include_str!("../packaging/timberfs-tally.conf.example");
        let p = Provision::parse("apache", text).unwrap();
        assert_eq!(
            p.select,
            crate::select::canonical("[service=~apache-.*]").unwrap()
        );
        assert_eq!(p.output, "{name}-tally");
        assert!(p.apply.contains(&"timberfs-volume".to_string()));
        // Every commented key is a key this build still reads.
        for line in text.lines() {
            let t = line.trim_start_matches('#').trim();
            let Some((key, _)) = t.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if key.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !key.is_empty() {
                assert!(
                    PROVISION_KEYS.contains(&key),
                    "the example writes {key:?}, which this build does not read"
                );
            }
        }
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

    #[test]
    fn a_name_that_resolves_nowhere_says_where_it_looked() {
        // A directory is listed only if it EXISTS, so where none does the
        // list is empty — and the failure then named nowhere at all, which
        // says nothing about where the file should go.
        //
        // ⚠ The directories are PASSED, not derived. Deriving them makes
        // the test depend on whether this host has the timberfs package
        // installed — it passed everywhere until 0.32.0 landed on the
        // machine it was written on, which is a test that has told you
        // nothing.
        let etc = PathBuf::from("/etc/timberfs");
        let err = no_such_extractor("nope", &[], &etc).to_string();
        assert!(err.contains(PACKAGED_EXTRACTORS), "{err}");
        assert!(err.contains("/etc/timberfs/tally.extractors.d"), "{err}");

        let site = tempdir();
        std::fs::write(
            site.join("x.json"),
            doc(r#"{"name":"m","measure":[{"count":true}]}"#),
        )
        .unwrap();
        let err = no_such_extractor("nope", std::slice::from_ref(&site), &etc).to_string();
        assert!(err.contains(&site.display().to_string()), "{err}");
        // The FILE STEM, because that is what this lookup takes. The
        // document is named `t`, and printing that alone would print a
        // word that does not resolve.
        assert!(err.contains("which hold x "), "{err}");
        assert!(err.contains(r#"the document "t""#), "{err}");

        // An empty directory says so rather than trailing off.
        let bare = tempdir();
        let err = no_such_extractor("nope", std::slice::from_ref(&bare), &etc).to_string();
        assert!(err.contains("which hold none"), "{err}");

        std::fs::remove_dir_all(&site).ok();
        std::fs::remove_dir_all(&bare).ok();
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
                etc: PathBuf::from("/etc/timberfs"),
                try_it: true,
                check: false,
                blocks: None,
                block_flush: DEFAULT_BLOCK_FLUSH,
                block_buckets: DEFAULT_BLOCK_BUCKETS,
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

    fn provision(text: &str) -> anyhow::Result<Provision> {
        Provision::parse("t", text)
    }

    #[test]
    fn a_provisioning_names_stores_and_extractors_or_it_measures_nothing() {
        assert!(provision("APPLY=timberfs-volume\n").is_err(), "no SELECT");
        assert!(provision("SELECT=[]\n").is_err(), "no APPLY");
        assert!(provision("SELECT=[]\nAPPLY=timberfs-volume\n").is_ok());

        // A section header is the shape `file.d` has and this does not:
        // one selection per file, so a second is a second file and a
        // second unit.
        let err = provision("[apache]\nSELECT=[]\nAPPLY=x\n").unwrap_err();
        assert!(format!("{err}").contains("no sections"), "{err}");

        let err = provision("SELECT=[]\nAPPLY=x\nWIDHT=60s\n").unwrap_err();
        assert!(format!("{err}").contains("unknown key"), "{err}");
    }

    #[test]
    fn an_output_template_that_does_not_vary_is_refused_at_the_template() {
        // Every source would map to one store, which is several writers
        // on one tally store. Refused where the mistake was MADE rather
        // than as a collision three stores later.
        let err = provision("SELECT=[]\nAPPLY=x\nOUTPUT=one-tally\n").unwrap_err();
        assert!(format!("{err}").contains("must vary"), "{err}");
        assert!(provision("SELECT=[]\nAPPLY=x\nOUTPUT={name}.tally\n").is_ok());
    }

    #[test]
    fn a_tally_selection_excludes_tally_stores_without_being_asked() {
        // ⚠ A tally store inherits its source's provenance, so
        // `[service=~apache-.*]` matches the tally store it just created,
        // whose every line a volume metric then claims — producing
        // another store one level deeper, forever. Implicit, because an
        // operator who forgot it would not find out for a week.
        let p = provision("SELECT=[service=~apache-.*]\nAPPLY=x\n").unwrap();
        let sel = p.selector().unwrap();
        assert!(sel.contains("class="), "{sel}");

        let apache = crate::select::Selector::parse(&sel).unwrap();
        let mut source = serde_json::Map::new();
        source.insert("service".into(), serde_json::json!("apache-access"));
        assert!(apache.matches(&source), "the source store is followed");

        let mut its_output = source.clone();
        its_output.insert("class".into(), serde_json::json!("tally"));
        assert!(
            !apache.matches(&its_output),
            "a tally store is never a source: that is a rollup, and a different verb"
        );

        // …and `[]` narrows the same way rather than becoming everything.
        let all = provision("SELECT=[]\nAPPLY=x\n").unwrap();
        let sel = crate::select::Selector::parse(&all.selector().unwrap()).unwrap();
        assert!(!sel.matches(&its_output), "{:?}", all.selector());
    }

    #[test]
    fn an_output_template_is_filled_from_what_a_source_record_carries() {
        let p = provision("SELECT=[]\nAPPLY=x\nOUTPUT={host}.{name}-tally\n").unwrap();
        let mut labels = Map::new();
        labels.insert("host".into(), serde_json::json!("web01"));
        assert_eq!(
            p.output_from("apache-access", &labels, Some("abc"))
                .unwrap(),
            "web01.apache-access-tally"
        );

        // ⚠ A field the source does not carry is REFUSED by name. It
        // produced `.apache-access-tally` — a hidden directory — and a
        // template of nothing but absent fields would collapse every
        // source onto one store.
        let err = p
            .output_from("apache-access", &Map::new(), None)
            .unwrap_err();
        assert!(format!("{err}").contains("declares none"), "{err}");
    }

    #[test]
    fn a_tally_store_declares_what_only_the_provisioning_knows() {
        let p = provision("SELECT=[]\nAPPLY=x\nDECLARE=retain=30d wal=false\n").unwrap();
        let mut labels = Map::new();
        labels.insert("service".into(), serde_json::json!("apache-access"));
        labels.insert("index".into(), serde_json::json!(true));
        let got = declared_for(&p, "src-id", &labels);

        assert!(got.contains(&"class=tally".to_string()));
        assert!(got.contains(&"derived_from=src-id".to_string()));
        assert!(
            got.contains(&"service=apache-access".to_string()),
            "{got:?}"
        );
        // A SETTING is not provenance and must not be inherited: the
        // source's index choice is not the tally store's.
        assert!(!got.iter().any(|k| k == "index=true"), "{got:?}");
        // …and DECLARE has the last word, even over a default this
        // thinks is a good idea.
        assert!(got.contains(&"retain=30d".to_string()));
        assert!(got.contains(&"wal=false".to_string()), "{got:?}");
        assert!(!got.iter().any(|k| k == "wal=true"), "{got:?}");
    }

    #[test]
    fn what_is_written_reports_the_minutes_it_is_about() {
        // The window this returns is what stamps the chunk, so a tally
        // store's write axis is the minutes its lines describe and a
        // logline-time query selects its chunks exactly. Stamping the
        // moment of computation instead put the two axes a bucket and a
        // grace apart, and a query for the minute the numbers describe
        // then read no chunk at all — which reads like a quiet minute.
        let docs = vec![(
            PathBuf::from("t"),
            parsed(r#"{"name":"m","measure":[{"count":true,"unit":"x"}]}"#).unwrap(),
        )];
        let opts = TallyOpts::for_run(None);
        let mut run = Run::new(&docs, &opts).unwrap();
        let mut out: Vec<u8> = Vec::new();

        // Nothing has sealed, but the `!meta` line went out and carries a
        // stamp — so the window must cover it or the chunk holding it is
        // stamped from somewhere else entirely.
        let first = entries_of_text(b"2026-09-06T13:37:10.000Z hello\n").unwrap();
        let meta = run
            .feed(&first[0], Axis::Logline, &mut out, None, false)
            .unwrap();
        assert_eq!(
            meta,
            Some((
                crate::query::parse_time("2026-09-06T13:37:10Z").unwrap(),
                crate::query::parse_time("2026-09-06T13:37:10Z").unwrap()
            ))
        );

        // Two buckets seal at the end: the window is the first and the
        // last, not the moment they were folded.
        let later = entries_of_text(b"2026-09-06T13:39:10.000Z hello\n").unwrap();
        run.feed(&later[0], Axis::Logline, &mut out, None, false)
            .unwrap();
        let sealed = run.finish(&mut out, None, false).unwrap();
        assert_eq!(
            sealed,
            Some((
                crate::query::parse_time("2026-09-06T13:37:00Z").unwrap(),
                crate::query::parse_time("2026-09-06T13:39:00Z").unwrap()
            ))
        );
    }

    /// Wall clock may seal the buckets in-order data has already left,
    /// and never the one it is still in — which is the only bucket an
    /// in-order arrival can land in. Advancing past it displaced every
    /// later entry out of its own bucket, marked `!late`.
    #[test]
    fn wall_clock_seals_what_event_time_has_left_and_no_more() {
        let mut r = Roller::new(60_000, 30_000, 1000);
        r.add(&s("2026-09-06T13:35:10.000Z 0s m count=1"));
        r.add(&s("2026-09-06T13:37:10.000Z 0s m count=1"));
        // Hours later by the clock, and it still may not seal 13:37.
        r.advance_to(stamp("2026-09-06T19:00:00.000Z"));
        assert_eq!(
            r.drain(Drain::Sealed)
                .iter()
                .map(|s| s.render())
                .collect::<Vec<_>>(),
            vec!["2026-09-06T13:35:00.000Z 60s m count=1"],
            "13:37 holds the newest entry, so nothing may seal it"
        );
        // And a later entry for it is therefore NOT late.
        r.add(&s("2026-09-06T13:37:50.000Z 0s m count=1"));
        let out = r.drain(Drain::Final);
        assert!(
            !out.iter().any(|s| s.metric == "!late"),
            "an in-order arrival was displaced: {:?}",
            out.iter().map(|s| s.render()).collect::<Vec<_>>()
        );
        assert!(out
            .iter()
            .any(|s| s.render() == "2026-09-06T13:37:00.000Z 60s m count=2"));
    }

    /// The defect that made a tally store under-report: a bucket shown
    /// before it was complete used to be EVICTED, so each showing
    /// carried only what had arrived since the last one — and a reader
    /// resolving a bucket to its newest line then read the last fragment
    /// as the whole. Measured at 512 of 1536 entries.
    #[test]
    fn a_provisional_bucket_is_superseded_by_its_complete_total() {
        let mut r = Roller::new(60_000, 120_000, 1000);
        let mut tape: Vec<Sample> = Vec::new();
        for i in 0..9 {
            r.add(&s(&format!(
                "2026-09-06T13:37:{:02}.000Z 0s m count=1",
                i * 5
            )));
            // A quiet tick between every entry, as a stalled feed
            // produced: each one shows the bucket without consuming it.
            tape.extend(r.drain(Drain::Provisional));
        }
        tape.extend(r.drain(Drain::Final));
        assert!(tape.len() > 1, "the bucket was never shown provisionally");
        let resolved = resolve(tape);
        assert_eq!(
            resolved.iter().map(|s| s.render()).collect::<Vec<_>>(),
            vec!["2026-09-06T13:37:00.000Z 60s m count=9"],
            "the newest line for a bucket must carry its whole total"
        );
    }

    /// A quiet store must not restate the same numbers every tick: the
    /// tape is what retention keeps, and a bucket nothing has added to
    /// says what its last line already said.
    #[test]
    fn an_unchanged_bucket_is_not_restated() {
        let mut r = Roller::new(60_000, 120_000, 1000);
        r.add(&s("2026-09-06T13:37:10.000Z 0s m count=1"));
        assert_eq!(r.drain(Drain::Provisional).len(), 1);
        assert!(
            r.drain(Drain::Provisional).is_empty(),
            "nothing arrived, so there is nothing to say again"
        );
        r.add(&s("2026-09-06T13:37:20.000Z 0s m count=1"));
        assert_eq!(r.drain(Drain::Provisional).len(), 1, "it changed again");
    }

    /// A provisional line must not move the position: the bucket is
    /// still open, so a restart has to re-read the entries behind it.
    #[test]
    fn showing_a_bucket_does_not_release_its_source_bytes() {
        let mut r = Roller::new(60_000, 120_000, 1000);
        let mut sample = s("2026-09-06T13:37:10.000Z 0s m count=1");
        sample.cite = Some((4096, 100));
        r.add(&sample);
        assert_eq!(r.safe_offset(), Some(4096));
        let _ = r.drain(Drain::Provisional);
        assert_eq!(
            r.safe_offset(),
            Some(4096),
            "the bucket is still open, so its oldest byte is still needed"
        );
    }

    #[test]
    fn the_follower_is_derived_from_the_file_and_never_typed() {
        let p = provision("SELECT=[class=audit]\nAPPLY=x\n").unwrap();
        assert_eq!(p.follower_name(), "tally-t");
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
