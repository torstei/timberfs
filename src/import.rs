//! `timberfs import`: convert an existing plain log file into a timberfs
//! backing pair, stamping chunks with timestamps PARSED FROM THE LOG LINES
//! (the write time of historical data is meaningless).
//!
//! ```text
//! timberfs import /var/log/old-app.log backing/app.log
//! ```
//!
//! Timestamp extraction per line, first match wins. Auto-detected
//! timestamps must sit at the START of the line (which also makes
//! indented continuation lines inherit naturally):
//!   - RFC3339/ISO-8601 and friends: `2026-07-10T09:23:45.123+02:00`,
//!     space instead of T, `.` as the date separator, and `.`/`,`/`:`
//!     before the fraction (logback's `yyyy.MM.dd HH:mm:ss:SSS` included)
//!   - a leading epoch in seconds or milliseconds
//!   - syslog's `Oct  1 00:00:02`, which carries no year: it is the latest
//!     one that does not put the stamp after the extractor's reference
//!     (see `Extractor::anchor`)
//!   - Apache/CLF `[10/Jul/2026:09:23:45 +0200]` — the one non-anchored
//!     exception, since CLF puts the bracketed timestamp mid-line
//!   - --timestamp-regex + --timestamp-format for everything else (the
//!     regex is searched, not anchored; use ^ to anchor)
//!
//! Naive timestamps (no zone) are taken as local time unless --utc. Lines
//! with no parseable timestamp (stack traces, continuations) inherit the
//! previous line's stamp, so multiline entries land in the right window.
//! Real logs are only mostly sorted; chunk windows are the min/max of the
//! stamps they contain, and queries select by interval overlap, so mild
//! disorder widens windows without losing data.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context};
use chrono::{DateTime, Datelike, Local, NaiveDateTime, TimeZone, Utc};
use regex::Regex;

use crate::query::{fmt_ms, resolve_backing};
use crate::store::{self, Config, Store};

/// How far past the reference a yearless stamp may sit and still be taken as
/// this year: clocks and zones disagree by hours, never by a year.
const YEARLESS_SLACK_MS: i64 = 24 * 3_600_000;

/// Years searched below the reference before a yearless stamp is refused.
const YEARLESS_REACH: i32 = 4;

/// Give up if none of the first this-many lines have a timestamp.
const DETECT_WINDOW: usize = 1000;

/// The patterns every extractor shares. Compiled ONCE for the process:
/// they are constants, and an extractor is built per store per read — a
/// fleet poll over 500 stores was compiling 2,500 regexes a second, which
/// was the whole cost of the read (measured: 1.01 s for a 500-store
/// `--records` answer against 0.01 s for the same stores as text).
struct Builtins {
    iso: Regex,
    clf: Regex,
    ctime: Regex,
    epoch: Regex,
    syslog: Regex,
}

fn builtins() -> &'static Builtins {
    static BUILTINS: std::sync::OnceLock<Builtins> = std::sync::OnceLock::new();
    BUILTINS.get_or_init(|| Builtins {
        iso: Regex::new(
            r"^(\d{4})[.-](\d{2})[.-](\d{2})[T ](\d{2}:\d{2}:\d{2})(?:[.,:](\d{1,9}))?(Z|[+-]\d{2}:?\d{2})?",
        )
        .unwrap(),
        clf: Regex::new(r"\[(\d{2}/[A-Z][a-z]{2}/\d{4}:\d{2}:\d{2}:\d{2} [+-]\d{4})\]").unwrap(),
        // Bracketed ctime, the shape Apache's error log uses (and its
        // 2.2-era form without the microseconds): the one common log
        // clock whose year comes last and whose zone is implied local.
        ctime: Regex::new(
            r"\[([A-Z][a-z]{2} [A-Z][a-z]{2} [ 0-9]\d \d{2}:\d{2}:\d{2}(?:\.\d+)? \d{4})\]",
        )
        .unwrap(),
        epoch: Regex::new(r"^(\d{13}|\d{10})\b").unwrap(),
        syslog: Regex::new(r"^([A-Z][a-z]{2}) ([ 0-9]\d \d{2}:\d{2}:\d{2})").unwrap(),
    })
}

pub struct Extractor {
    /// The only pattern that varies by store, and the only one this can
    /// therefore have to compile.
    custom: Option<(Regex, String)>,
    /// The declared format names no year, so one is put in front of it.
    custom_yearless: bool,
    utc: bool,
    /// Unix ms the yearless stamps are resolved against; 0 means now.
    reference_ms: AtomicU64,
}

fn names_no_year(format: &str) -> bool {
    use chrono::format::{Fixed, Item, Numeric};
    !chrono::format::StrftimeItems::new(format).any(|i| {
        matches!(
            i,
            Item::Numeric(
                Numeric::Year
                    | Numeric::YearDiv100
                    | Numeric::YearMod100
                    | Numeric::IsoYear
                    | Numeric::IsoYearDiv100
                    | Numeric::IsoYearMod100
                    | Numeric::Timestamp,
                _
            ) | Item::Fixed(Fixed::RFC2822 | Fixed::RFC3339)
        )
    })
}

impl Extractor {
    pub fn new(
        custom_regex: Option<&str>,
        custom_format: Option<&str>,
        utc: bool,
    ) -> anyhow::Result<Extractor> {
        let custom = match (custom_regex, custom_format) {
            (Some(re), Some(f)) => {
                let re = Regex::new(re).context("bad --timestamp-regex")?;
                if re.captures_len() < 2 {
                    bail!("--timestamp-regex needs one capture group around the timestamp");
                }
                Some((re, f.to_string()))
            }
            (None, None) => None,
            _ => bail!("--timestamp-regex and --timestamp-format go together"),
        };
        let custom_yearless = custom.as_ref().is_some_and(|(_, f)| names_no_year(f));
        Ok(Extractor {
            custom,
            custom_yearless,
            utc,
            reference_ms: AtomicU64::new(0),
        })
    }

    /// Resolve yearless stamps against `ms`: the end of what is being read,
    /// so a stamp is never taken to be later than the data that holds it.
    pub fn anchor(&self, ms: u64) {
        self.reference_ms.store(ms, Ordering::Relaxed);
    }

    /// `anchor` to a file's modification time.
    pub fn anchor_to(&self, meta: &fs::Metadata) {
        let ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        if let Some(ms) = ms {
            self.anchor(ms);
        }
    }

    /// The latest year for which `parse` yields a stamp not after the
    /// reference. Years that cannot hold the stamp (29 February) are skipped.
    fn in_latest_year(&self, parse: impl Fn(i32) -> Option<NaiveDateTime>) -> Option<u64> {
        let reference = match self.reference_ms.load(Ordering::Relaxed) {
            0 => Utc::now().timestamp_millis(),
            ms => ms as i64,
        };
        let year = Utc.timestamp_millis_opt(reference).single()?.year();
        (year - YEARLESS_REACH..=year + 1)
            .rev()
            .filter_map(|y| parse(y).and_then(|n| self.naive_to_ms(n)))
            .find(|&ms| ms <= reference + YEARLESS_SLACK_MS)
            .and_then(|ms| u64::try_from(ms).ok())
    }

    fn naive_to_ms(&self, naive: NaiveDateTime) -> Option<i64> {
        if self.utc {
            Some(Utc.from_utc_datetime(&naive).timestamp_millis())
        } else {
            Local
                .from_local_datetime(&naive)
                .earliest()
                .map(|dt| dt.timestamp_millis())
        }
    }

    /// Extract a unix-ms timestamp from the head of a log line, if there
    /// is one. The caller passes only the line's head — no slicing here.
    pub fn extract(&self, head: &str) -> Option<u64> {
        if let Some((re, fmt)) = &self.custom {
            let m = re.captures(head)?.get(1)?.as_str().to_string();
            if self.custom_yearless {
                let with_year = format!("%Y {fmt}");
                return self.in_latest_year(|y| {
                    NaiveDateTime::parse_from_str(&format!("{y} {m}"), &with_year).ok()
                });
            }
            let ms = DateTime::parse_from_str(&m, fmt)
                .map(|dt| dt.timestamp_millis())
                .ok()
                .or_else(|| {
                    NaiveDateTime::parse_from_str(&m, fmt)
                        .ok()
                        .and_then(|n| self.naive_to_ms(n))
                })?;
            return u64::try_from(ms).ok();
        }

        if let Some(c) = builtins().iso.captures(head) {
            // reassemble as strict RFC3339-ish regardless of which
            // separators the log used
            let normalized = format!(
                "{}-{}-{}T{}{}",
                c.get(1).unwrap().as_str(),
                c.get(2).unwrap().as_str(),
                c.get(3).unwrap().as_str(),
                c.get(4).unwrap().as_str(),
                c.get(5)
                    .map(|f| format!(".{}", f.as_str()))
                    .unwrap_or_default(),
            );
            let ms = match c.get(6) {
                Some(zone) => {
                    // normalize +0200 -> +02:00 for RFC3339 parsing
                    let z = zone.as_str();
                    let z = if z.len() == 5 && !z.contains(':') {
                        format!("{}:{}", &z[..3], &z[3..])
                    } else {
                        z.to_string()
                    };
                    DateTime::parse_from_rfc3339(&format!("{normalized}{z}"))
                        .ok()?
                        .timestamp_millis()
                }
                None => {
                    let naive =
                        NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
                    self.naive_to_ms(naive)?
                }
            };
            return u64::try_from(ms).ok();
        }

        if let Some(c) = builtins().clf.captures(head) {
            let ms = DateTime::parse_from_str(c.get(1).unwrap().as_str(), "%d/%b/%Y:%H:%M:%S %z")
                .ok()?
                .timestamp_millis();
            return u64::try_from(ms).ok();
        }

        if let Some(c) = builtins().ctime.captures(head) {
            let naive = NaiveDateTime::parse_from_str(
                c.get(1).unwrap().as_str(),
                "%a %b %e %H:%M:%S%.f %Y",
            )
            .ok()?;
            // ctime carries no zone; Apache writes it in local time.
            let ms = self.naive_to_ms(naive)?;
            return u64::try_from(ms).ok();
        }

        if let Some(c) = builtins().syslog.captures(head) {
            let (month, rest) = (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str());
            return self.in_latest_year(|y| {
                NaiveDateTime::parse_from_str(&format!("{y} {month} {rest}"), "%Y %b %e %H:%M:%S")
                    .ok()
            });
        }

        if let Some(c) = builtins().epoch.captures(head) {
            let digits = c.get(1).unwrap().as_str();
            let n: u64 = digits.parse().ok()?;
            return Some(if digits.len() == 13 { n } else { n * 1000 });
        }

        None
    }
}

#[allow(clippy::too_many_arguments)]
/// Lines into stamped entries, carrying the inheritance rule that makes a
/// stack trace one entry: an unstamped line belongs to the last stamped one,
/// and lines arriving before the FIRST stamp are held until it comes. That
/// state spans calls, which is why this is a struct — and why `import` and
/// `import --follow` share it instead of each keeping their own copy of a
/// rule the store's shape depends on.
pub struct Stamper {
    last_ts: Option<u64>,
    leading: Vec<Vec<u8>>,
    pub stamped: u64,
    pub inherited: u64,
}

impl Stamper {
    /// `last_ts` seeds inheritance from what the store already ends with,
    /// so an unstamped first line continues the last imported entry.
    pub fn resuming_from(last_ts: Option<u64>) -> Stamper {
        Stamper {
            last_ts,
            leading: Vec::new(),
            stamped: 0,
            inherited: 0,
        }
    }

    pub fn last_ts(&self) -> Option<u64> {
        self.last_ts
    }

    /// A stamp the caller saw on a line it is NOT appending (an overlap
    /// duplicate, or a merged segment's end) still seeds inheritance for
    /// the unstamped lines that follow it.
    pub fn observe(&mut self, ts: u64) {
        self.last_ts = Some(ts);
    }

    /// Lines held because no timestamp has been seen yet.
    pub fn unstamped_pending(&self) -> usize {
        self.leading.len()
    }

    /// Append one line (newline included) as its own entry, or as a
    /// continuation of the last stamped one.
    pub fn feed(
        &mut self,
        f: &mut crate::store::FileStore,
        line: &[u8],
        ts: Option<u64>,
        cfg: &Config,
    ) -> anyhow::Result<()> {
        match (ts, self.last_ts) {
            (Some(t), _) => {
                // The pre-first-timestamp lines belong to this one.
                for l in self.leading.drain(..) {
                    f.append_stamped(&l, t, cfg)?;
                    self.inherited += 1;
                }
                f.append_stamped(line, t, cfg)?;
                self.stamped += 1;
                self.last_ts = Some(t);
            }
            (None, Some(t)) => {
                f.append_stamped(line, t, cfg)?;
                self.inherited += 1;
            }
            (None, None) => {
                self.leading.push(line.to_vec());
                if self.leading.len() > DETECT_WINDOW {
                    bail!(
                        "no timestamp found in the first {DETECT_WINDOW} lines; \
                         try --timestamp-regex/--timestamp-format"
                    );
                }
            }
        }
        Ok(())
    }
}

/// How much of each end of the store is compared with the file.
const TAIL_CHECK: u64 = 16 * 1024;

enum Continuation {
    /// The file is the one the store was fed from, and this is where it ends.
    Yes(u64),
    /// The file is shorter than the store's data: truncated or rotated.
    TooSmall,
    /// The bytes at the join are not the store's.
    Differs,
}

/// Is `src` the file the store's data came from, grown since? The store's last
/// bytes must be the file's bytes just before the offset where the store ends,
/// and, while the store still has its head, its first bytes the file's first. A
/// few KiB at each end and not the store, which is what answering "same file?"
/// has to cost.
fn continuation(f: &mut crate::store::FileStore, src: &File) -> anyhow::Result<Continuation> {
    use std::os::unix::fs::FileExt;
    let size = f.size();
    let end = f.tape_end();
    if src.metadata()?.len() < end {
        return Ok(Continuation::TooSmall);
    }
    let n = size.min(TAIL_CHECK);
    let has_head = f.tape_start() == 0;
    let mut same = |store_at: u64, file_at: u64| -> anyhow::Result<bool> {
        let held = f.read(store_at, n as u32)?;
        let mut file = vec![0u8; n as usize];
        src.read_exact_at(&mut file, file_at)
            .context("reading the source where the store's data ends")?;
        Ok(held == file)
    };
    if !same(size - n, end - n)? {
        return Ok(Continuation::Differs);
    }
    if has_head && !same(0, 0)? {
        return Ok(Continuation::Differs);
    }
    Ok(Continuation::Yes(end))
}

/// FNV-1a 128 of a line (trailing newline stripped). Overlap dedup only
/// needs collisions to be unlikelier than hardware failure, not
/// cryptography: for 10^8 distinct lines the collision odds are ~10^-23.
pub(crate) fn line_hash(line: &[u8]) -> u128 {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let mut h: u128 = 0x6c62272e07bb014262b821756295c58d;
    for &b in line {
        h ^= b as u128;
        h = h.wrapping_mul(0x0000000001000000000000000000013b);
    }
    h
}

/// Multiset of the store's lines from the first chunk whose window
/// reaches `t0` to the end — what an overlapping source will be
/// deduplicated against. Chunks may start mid-line (FUSE writers flush at
/// byte thresholds), so the partial line spilling in from earlier chunks
/// is carried for exact reconstruction. Memory is one chunk plus ~50
/// bytes per distinct overlap line.
pub(crate) fn overlap_line_counts(
    chunks: &[crate::format::ChunkRecord],
    trunk_path: &Path,
    t0: u64,
) -> anyhow::Result<HashMap<u128, u32>> {
    let trunk =
        File::open(trunk_path).with_context(|| format!("opening {}", trunk_path.display()))?;
    let decomp = |c: &crate::format::ChunkRecord| -> anyhow::Result<Vec<u8>> {
        let comp = crate::format::read_frame(&trunk, c)?;
        Ok(crate::format::decode_frame(&comp, c.uncomp_len)?)
    };
    let k = chunks.iter().take_while(|c| c.last_write_ms < t0).count();
    // The tail of the line the overlap region starts inside, if any.
    let mut carry: Vec<u8> = Vec::new();
    let mut j = k;
    while j > 0 {
        let mut bytes = decomp(&chunks[j - 1])?;
        if let Some(p) = bytes.iter().rposition(|&b| b == b'\n') {
            bytes.drain(..=p);
            bytes.extend_from_slice(&carry);
            carry = bytes;
            break;
        }
        bytes.extend_from_slice(&carry);
        carry = bytes;
        j -= 1;
    }
    let mut counts: HashMap<u128, u32> = HashMap::new();
    for c in &chunks[k..] {
        let bytes = decomp(c)?;
        let mut start = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                let key = if carry.is_empty() {
                    line_hash(&bytes[start..i])
                } else {
                    carry.extend_from_slice(&bytes[start..i]);
                    let key = line_hash(&carry);
                    carry.clear();
                    key
                };
                *counts.entry(key).or_insert(0) += 1;
                start = i + 1;
            }
        }
        carry.extend_from_slice(&bytes[start..]);
    }
    if !carry.is_empty() {
        *counts.entry(line_hash(&carry)).or_insert(0) += 1;
    }
    Ok(counts)
}

/// First parsed timestamp in a file, scanning at most DETECT_WINDOW lines.
pub(crate) fn first_stamp(path: &Path, extractor: &Extractor) -> anyhow::Result<u64> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    extractor.anchor_to(&f.metadata()?);
    let mut reader = BufReader::new(f);
    let mut line: Vec<u8> = Vec::new();
    for _ in 0..DETECT_WINDOW {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if let Some(ts) = extractor.extract(&String::from_utf8_lossy(&line[..line.len().min(256)]))
        {
            return Ok(ts);
        }
    }
    bail!(
        "no timestamp found in the first {DETECT_WINDOW} lines of {}; \
         try --timestamp-regex/--timestamp-format",
        path.display()
    )
}

/// A source is either a plain log (lines get parsed and stamped) or an
/// existing timberfs log — a shipped rotation segment, say — whose chunks
/// merge verbatim, index included.
enum Source {
    Plain(PathBuf),
    Timber {
        trunk: PathBuf,
        records: Vec<crate::format::ChunkRecord>,
        /// The source's own manifest, so a timberfs-to-timberfs import can
        /// carry provenance and lineage across the hop. `None` for a
        /// source that declares nothing (a plain `append` writes no
        /// manifest at all).
        bark: Option<serde_json::Map<String, serde_json::Value>>,
    },
}

impl Source {
    fn display(&self) -> String {
        match self {
            Source::Plain(p) => p.display().to_string(),
            Source::Timber { trunk, .. } => format!("{} (timberfs)", trunk.display()),
        }
    }
}

/// Is this exact segment (as a consecutive run of records, compared by
/// lengths and time windows) already in the target's index? Candidates are
/// located by the first record, so this is O(target + segment).
fn segment_present(
    target: &[crate::format::ChunkRecord],
    seg: &[crate::format::ChunkRecord],
) -> bool {
    let same = |a: &crate::format::ChunkRecord, b: &crate::format::ChunkRecord| {
        a.uncomp_len == b.uncomp_len
            && a.comp_len == b.comp_len
            && a.first_write_ms == b.first_write_ms
            && a.last_write_ms == b.last_write_ms
    };
    if seg.len() > target.len() {
        return false;
    }
    for start in 0..=(target.len() - seg.len()) {
        if same(&target[start], &seg[0])
            && target[start..start + seg.len()]
                .iter()
                .zip(seg)
                .all(|(a, b)| same(a, b))
        {
            return true;
        }
    }
    false
}

/// A path names a timberfs source if it is a .trunk/.rings path, a
/// .timber bundle, or if no plain file exists at the exact path but the
/// backing pair does.
fn classify_source(path: &Path, extractor: &Extractor) -> anyhow::Result<(u64, Source)> {
    let ext = path.extension().and_then(|e| e.to_str());
    if crate::query::is_bundle(path) {
        // A bundle reads in place: open_source already shifted the record
        // offsets to the trunk member's position within the tar.
        let handle = crate::query::open_source(path)?;
        let (records, bark) = (handle.records, handle.bark);
        return Ok((
            records.first().map(|r| r.first_write_ms).unwrap_or(0),
            Source::Timber {
                trunk: path.to_path_buf(),
                records,
                bark,
            },
        ));
    }
    let pair_path = matches!(
        ext,
        Some(crate::format::TRUNK_EXT) | Some(crate::format::RINGS_EXT)
    );
    if !pair_path && path.is_file() {
        // A zero-byte plain file has no timestamp to sniff — it is an
        // empty source (a quiet day in a rotated set), not an error.
        if fs::metadata(path)?.len() == 0 {
            return Ok((0, Source::Plain(path.to_path_buf())));
        }
        return Ok((
            first_stamp(path, extractor)?,
            Source::Plain(path.to_path_buf()),
        ));
    }
    let (sdir, sname) = resolve_backing(path)?;
    let rings = crate::format::rings_path(&sdir, &sname);
    if !rings.exists() {
        bail!(
            "{} is neither a log file nor a timberfs log",
            path.display()
        );
    }
    let records = crate::format::read_index(&rings)?;
    Ok((
        records.first().map(|r| r.first_write_ms).unwrap_or(0),
        Source::Timber {
            trunk: crate::format::trunk_path(&sdir, &sname),
            records,
            bark: crate::bark::load(&sdir, &sname),
        },
    ))
}

/// Options for [`cmd_import`], one field per CLI flag.
pub struct ImportOpts {
    /// Caller's timestamp interpretation (--timestamp-regex /
    /// --timestamp-format / --utc); merged with the store's declared
    /// format before extraction.
    pub time: crate::bark::TimeFormat,
    /// Accepted and ignored: a re-import compares a few KiB at each end of what
    /// the store holds with the file, which is already the cheap check.
    pub quick: bool,
    /// Declare and maintain the .grain content index.
    pub index: bool,
    /// Declare the .sap write-ahead sidecar.
    pub wal: bool,
}

pub fn cmd_import(
    sources_in: &[PathBuf],
    dest: &Path,
    cfg: Config,
    opts: ImportOpts,
) -> anyhow::Result<()> {
    let ImportOpts {
        time,
        quick: _,
        index,
        wal,
    } = opts;
    if crate::query::is_bundle(dest) {
        bail!(
            "{} is a .timber transfer bundle — bundles are read-only \
             (query/index/export work directly on them); import it into a \
             log to write",
            dest.display()
        );
    }
    crate::query::ensure_dest_is_not_plain_file(dest, "import")?;
    let (dir, name) = resolve_backing(dest)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating backing directory {}", dir.display()))?;
    // Flags override the store's DECLARED format (timestamp_regex /
    // timestamp_format / timestamp_utc in the manifest) — declare once,
    // and every later import of an exotic format is flag-free.
    let declared = crate::bark::time_format(crate::bark::load(&dir, &name).as_ref());
    let extractor = Extractor::new(
        time.regex.as_deref().or(declared.regex.as_deref()),
        time.format.as_deref().or(declared.format.as_deref()),
        time.utc || declared.utc,
    )?;

    // Multiple sources are one logical stream: order them chronologically
    // by their own first timestamp (rotation numbering and glob order are
    // unreliable) and show the stitch plan. Empty sources carry no data
    // but are still valid ("covered, nothing there" — e.g. a quiet day's
    // shipped segment): they are skipped, never errors.
    let mut sources: Vec<(u64, Source)> = Vec::new();
    for p in sources_in {
        let (ts, s) = classify_source(p, &extractor)?;
        let empty = match &s {
            Source::Timber { records, .. } => records.is_empty(),
            Source::Plain(p) => fs::metadata(p).map(|m| m.len()).unwrap_or(1) == 0,
        };
        if empty {
            crate::note!(
                "timberfs: {} is empty — nothing to append from it",
                s.display()
            );
            continue;
        }
        sources.push((ts, s));
    }
    sources.sort_by_key(|(ts, _)| *ts);
    let multi = sources.len() > 1;
    if multi {
        for w in sources.windows(2) {
            if w[0].0 == w[1].0 {
                bail!(
                    "{} and {} start at the same timestamp ({}) — the same file twice?",
                    w[0].1.display(),
                    w[1].1.display(),
                    fmt_ms(w[0].0)
                );
            }
        }
        crate::note!(
            "timberfs: stitching {} files in timestamp order:",
            sources.len()
        );
        for (i, (ts, s)) in sources.iter().enumerate() {
            crate::note!(
                "timberfs:   {}. {}  (starts {})",
                i + 1,
                s.display(),
                fmt_ms(*ts)
            );
        }
    }
    let total_bytes: u64 = sources
        .iter()
        .map(|(_, s)| match s {
            Source::Plain(p) => fs::metadata(p).map(|m| m.len()).unwrap_or(0),
            Source::Timber { trunk, .. } => fs::metadata(trunk).map(|m| m.len()).unwrap_or(0),
        })
        .sum();

    // Same writer locks as the appender: shared on the directory,
    // exclusive on the file.
    let _dir_lock = match store::lock_backing_shared(&dir)? {
        Some(f) => f,
        None => bail!(
            "backing directory {} is served by a timberfs mount; unmount first",
            dir.display()
        ),
    };
    let _file_lock = match store::lock_file_exclusive(&dir, &name)? {
        Some(f) => f,
        None => bail!("{name} already has a writer (appender or rotation)"),
    };

    let mut st = Store {
        dir: dir.clone(),
        cfg,
        files: std::collections::BTreeMap::new(),
    };
    let dest_existed = crate::format::rings_path(&dir, &name).exists();
    st.create(&name)?;

    // Identity and provenance cross the hop. A timberfs source describes
    // itself, and without this the destination comes out anonymous — which
    // nothing that cites an origin can be built on. Only for a NEW
    // destination that declares nothing: an existing store's manifest is
    // the operator's, and re-importing must not rewrite it.
    //
    // Exactly ONE identified source, because lineage names a parent: a
    // stitched set of segments from several stores has no single one, and
    // claiming either would be a guess. `derived_map` inherits provenance
    // and drops identity, window and every operational setting, so the
    // destination keeps its own index/wal/retention policy and `save`
    // mints it a fresh id.
    if !dest_existed && crate::bark::load(&dir, &name).is_none() {
        let mut parents = sources.iter().filter_map(|(_, s)| match s {
            Source::Timber { bark: Some(b), .. } => Some(b),
            _ => None,
        });
        match (parents.next(), parents.next()) {
            (Some(parent), None) => {
                let map = crate::bark::derived_map(Some(parent), "import");
                crate::bark::save(&dir, &name, &map)?;
            }
            (Some(_), Some(_)) => crate::note!(
                "timberfs: {name}: several sources declare a manifest; \
                 importing without lineage (no single parent to name)"
            ),
            _ => {}
        }
    }

    if sources.is_empty() {
        // Every source was empty. The import still succeeds — and still
        // materializes the destination: an empty store that EXISTS is the
        // attestation a shipping pipeline needs to keep ingesting.
        if index {
            crate::bark::declare_index(&dir, &name)?;
        }
        if wal {
            crate::bark::declare_wal(&dir, &name)?;
        }
        if index || crate::bark::index_declared(&dir, &name) {
            crate::grain::extend_grain(&dir, &name)?;
        }
        crate::note!(
            "timberfs: all sources are empty; {name} {}",
            if dest_existed {
                "unchanged"
            } else {
                "created empty"
            }
        );
        return Ok(());
    }

    // A non-empty target. A single plain source that continues what the store
    // holds is the same file, grown: resume where the store's data ends in it.
    // That is decided by the bytes at the join, not by a timestamp, so it holds
    // for a store whose head retention has dropped. A file that begins where the
    // store begins says it is the same one, so for that file a mismatch is
    // refused. Every other plain source is handled per-source below by its first
    // timestamp: after the store's end it simply appends; inside the store's
    // window it is deduplicated line-by-line against the overlap. Timberfs
    // sources append behind the ordering guard (segments already covered are
    // skipped below).
    let mut resume_from: u64 = 0;
    let mut last_ts: Option<u64> = None;
    {
        let f = st.files.get_mut(&name).unwrap();
        if f.size() > 0 {
            let store_first = f.chunks.first().map(|c| c.first_write_ms);
            if let (false, (t0, Source::Plain(src_path))) = (multi, &sources[0]) {
                let src = File::open(src_path)
                    .with_context(|| format!("opening source {}", src_path.display()))?;
                let claims_same = Some(*t0) == store_first;
                match continuation(f, &src)? {
                    Continuation::Yes(end) => {
                        resume_from = end;
                        crate::note!(
                            "timberfs: {} of {} bytes already imported and verified; resuming",
                            resume_from,
                            total_bytes
                        );
                    }
                    Continuation::TooSmall if claims_same => bail!(
                        "source ({total_bytes} bytes) is smaller than the {} bytes \
                         already imported — rotated or truncated file? import it to a new target",
                        f.tape_end()
                    ),
                    Continuation::Differs if claims_same => bail!(
                        "already-imported data differs from the source — \
                         rotated or rewritten file? import it to a new target instead"
                    ),
                    _ => {}
                }
            }
            // Seed timestamp inheritance with the last imported stamp.
            last_ts = f.last_write_ms();
        }
    }

    let mut line: Vec<u8> = Vec::new();
    let mut stamper = Stamper::resuming_from(last_ts);
    let mut lines: u64 = 0;
    let mut merged_chunks: u64 = 0;
    let mut merged_segments: u64 = 0;
    let mut skipped_segments: u64 = 0;
    let mut bytes_done: u64 = resume_from;
    let mut next_progress = total_bytes / 10;
    while total_bytes >= 10 && next_progress <= bytes_done {
        next_progress += total_bytes / 10;
    }
    let cfg = st.cfg;
    let mut ov_skipped: u64 = 0; // duplicate lines dropped in overlaps
    let mut ov_new: u64 = 0; // lines imported INTO an already-covered window

    for (source_idx, (t0, source)) in sources.iter().enumerate() {
        let source_path = match source {
            Source::Timber { trunk, records, .. } => {
                // A shipped segment: merge the chunks verbatim — unless the
                // target's index already CONTAINS this exact segment (same
                // consecutive run of records), which makes re-running a
                // shipping script a no-op. An older-but-absent segment is
                // NOT skipped; it falls through to the ordering guard.
                let f = st.files.get_mut(&name).unwrap();
                if segment_present(&f.chunks, records) {
                    crate::note!(
                        "timberfs: {} skipped — the target already contains this segment \
                         (chunks through {})",
                        source.display(),
                        fmt_ms(records.last().unwrap().last_write_ms)
                    );
                    skipped_segments += 1;
                } else {
                    let trunk_file = File::open(trunk)
                        .with_context(|| format!("opening {}", trunk.display()))?;
                    f.append_frames(&trunk_file, records, &cfg)
                        .with_context(|| format!("merging {}", source.display()))?;
                    merged_chunks += records.len() as u64;
                    merged_segments += 1;
                    stamper.observe(records.last().unwrap().last_write_ms);
                }
                bytes_done += fs::metadata(trunk).map(|m| m.len()).unwrap_or(0);
                continue;
            }
            Source::Plain(p) => p,
        };
        // Where does this plain source land relative to the store? After
        // the store's end: plain append. Inside the store's window (day
        // files cut with slack, re-runs): deduplicate the overlap line by
        // line — duplicates are skipped, genuinely new lines are
        // imported. Before the store's window: refused. The resume path
        // above (same file, regrown) already seeks past its verified
        // prefix and needs none of this.
        let mut dedup: Option<(HashMap<u128, u32>, u64)> = None;
        if !(source_idx == 0 && resume_from > 0) {
            let f = st.files.get_mut(&name).unwrap();
            if f.last_write_ms().is_some_and(|last| *t0 <= last) {
                f.flush_chunk(&cfg)?; // make buffered lines comparable
                let store_last = f.last_write_ms().unwrap();
                let store_first = f
                    .chunks
                    .first()
                    .map(|c| c.first_write_ms)
                    .unwrap_or(u64::MAX);
                if *t0 < store_first {
                    bail!(
                        "{} (starts {}) predates everything in {} (starts {}) — \
                         import in chronological order, or to a new target",
                        source_path.display(),
                        fmt_ms(*t0),
                        name,
                        fmt_ms(store_first)
                    );
                }
                let counts =
                    overlap_line_counts(&f.chunks, &crate::format::trunk_path(&dir, &name), *t0)?;
                crate::note!(
                    "timberfs: {} starts at {}, inside already-imported data (through {}) — \
                     deduplicating the overlap",
                    source_path.display(),
                    fmt_ms(*t0),
                    fmt_ms(store_last)
                );
                dedup = Some((counts, store_last));
            }
        }

        let mut src = File::open(source_path)
            .with_context(|| format!("opening {}", source_path.display()))?;
        extractor.anchor_to(&src.metadata()?);
        if source_idx == 0 && resume_from > 0 {
            use std::io::Seek;
            src.seek(std::io::SeekFrom::Start(resume_from))?;
        }
        let mut reader = BufReader::with_capacity(1 << 20, src);

        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            lines += 1;
            bytes_done += line.len() as u64;

            let ts = extractor.extract(&String::from_utf8_lossy(&line[..line.len().min(256)]));

            // Past the overlap window: every further line is new, stop
            // consulting (and free) the multiset.
            if dedup
                .as_ref()
                .is_some_and(|(_, until)| ts.or(stamper.last_ts()).is_some_and(|e| e > *until))
            {
                dedup = None;
            }
            if let Some((counts, _)) = dedup.as_mut() {
                if let Some(c) = counts.get_mut(&line_hash(&line)) {
                    if *c > 0 {
                        *c -= 1;
                        ov_skipped += 1;
                        // The store already has this line; its stamp still
                        // seeds inheritance for following unstamped lines.
                        if let Some(t) = ts {
                            stamper.observe(t);
                        }
                        continue;
                    }
                }
                ov_new += 1;
            }

            stamper.feed(st.files.get_mut(&name).unwrap(), &line, ts, &cfg)?;

            if total_bytes > 0 && bytes_done >= next_progress && bytes_done < total_bytes {
                crate::note!(
                    "timberfs: import {}% ({} of {} bytes)",
                    bytes_done * 100 / total_bytes,
                    bytes_done,
                    total_bytes
                );
                next_progress += total_bytes / 10;
            }
        }
    }

    if stamper.unstamped_pending() > 0 {
        bail!(
            "no timestamp found in any of the {} line(s); \
             try --timestamp-regex/--timestamp-format",
            stamper.unstamped_pending()
        );
    }

    if ov_skipped > 0 || ov_new > 0 {
        crate::note!(
            "timberfs: overlap: {ov_skipped} duplicate line(s) skipped{}",
            if ov_new > 0 {
                format!(
                    "; {ov_new} line(s) imported into the already-covered window — \
                     the overlap held content the store lacked (double-check the source \
                     if that is unexpected)"
                )
            } else {
                String::new()
            }
        );
    }

    st.flush_all();
    // Declared retention is maintained by every writer (the manifest is
    // the truth). Trim BEFORE index maintenance: a head-drop deletes the
    // grain, and the declared-index pass right below rebuilds it.
    match crate::bark::declared_retention(&dir, &name) {
        Ok(policy) if policy.is_some() => {
            let fields = crate::follower::subject_of(&dir, &name);
            let next_seq = st.next_seq(&name).unwrap_or(0);
            let held = crate::follower::TickInterest::default().floor(&policy, &fields, next_seq);
            if let Some(stats) =
                st.enforce_retention(&name, policy.max_age_ms, policy.max_comp_bytes, held.floor)?
            {
                crate::note!(
                    "timberfs: {name}: retention dropped {} chunk(s), {} compressed bytes",
                    stats.chunks_moved,
                    stats.comp_bytes
                );
                if let Some(record) =
                    crate::follower::override_record(&name, &policy, &stats, &held)
                {
                    eprintln!("{record}");
                }
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("timberfs: {name}: manifest unreadable ({e}); retention not applied"),
    }
    // The index is a property of the LOG, declared in its .bark manifest
    // (like a database index): --index persists the declaration, and any
    // import into a declared log maintains the grain — extended
    // incrementally for new chunks, rebuilt if missing (e.g. after
    // rotation/retention dropped it). The writer locks are already held.
    if time.regex.is_some() || time.utc {
        // The flags persist, like --index: the format is a property of
        // the CONTENT, declared in the manifest, and all roads converge.
        let mut map = crate::bark::load(&dir, &name).unwrap_or_default();
        if let (Some(r), Some(f)) = (&time.regex, &time.format) {
            map.insert(
                "timestamp_regex".to_string(),
                serde_json::Value::String(r.to_string()),
            );
            map.insert(
                "timestamp_format".to_string(),
                serde_json::Value::String(f.to_string()),
            );
        }
        if time.utc {
            map.insert("timestamp_utc".to_string(), serde_json::Value::Bool(true));
        }
        crate::bark::save(&dir, &name, &map)?;
    }
    if index {
        crate::bark::declare_index(&dir, &name)?;
    }
    if wal {
        crate::bark::declare_wal(&dir, &name)?;
    }
    if index || crate::bark::index_declared(&dir, &name) {
        crate::grain::extend_grain(&dir, &name)?;
    }
    let f = st.files.get(&name).unwrap();
    let (first, last) = (f.first_write_ms(), f.last_write_ms());
    if lines == 0 && merged_segments == 0 && (resume_from > 0 || skipped_segments > 0) {
        crate::note!("timberfs: {name} is already up to date; nothing imported");
        return Ok(());
    }
    let mut parts: Vec<String> = Vec::new();
    if lines > 0 {
        parts.push(format!(
            "{lines} lines ({} stamped, {} inherited)",
            stamper.stamped, stamper.inherited
        ));
    }
    if merged_segments > 0 {
        parts.push(format!(
            "{merged_chunks} chunk(s) merged verbatim from {merged_segments} timberfs source(s)"
        ));
    }
    if skipped_segments > 0 {
        parts.push(format!("{skipped_segments} segment(s) already covered"));
    }
    crate::note!(
        "timberfs: imported {}{}; now {} chunk(s), {} bytes, {} compressed ({:.1}x), \
         spanning {} .. {}",
        parts.join(", "),
        if resume_from > 0 {
            format!(" after {resume_from} bytes already imported")
        } else {
            String::new()
        },
        f.chunks.len(),
        f.size(),
        f.comp_size,
        f.size() as f64 / f.comp_size.max(1) as f64,
        first.map(fmt_ms).unwrap_or_default(),
        last.map(fmt_ms).unwrap_or_default(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in clocks, checked against real producer output. Apache's
    /// error log is the ctime one: no zone, year last, and (2.2) no
    /// microseconds — so `query --from/--to` verifies error lines against
    /// their own clock with nothing declared, exactly as it does the access
    /// log's CLF.
    #[test]
    fn ctime_lines_are_stamped_like_every_other_builtin() {
        let e = Extractor::new(None, None, true).unwrap();
        let at = |s: &str| e.extract(s);

        // Apache 2.4 error log, and the same second in CLF and ISO.
        let ctime = at("[Sat Aug 15 10:26:09.123456 2026] [php:error] [pid 9] boom").unwrap();
        assert_eq!(
            ctime,
            at("[Sat Aug 15 10:26:09 2026] [core:warn] no microseconds").unwrap() + 123
        );
        // A space-padded single-digit day parses too (ctime pads with %2d).
        assert!(at("[Sat Aug  1 10:26:09.000000 2026] [php:error] one").is_some());

        // Not a ctime: no bracket, and a bracketed non-date.
        assert!(at("Sat Aug 15 10:26:09.123456 2026 unbracketed").is_none());
        assert!(at("[core:error] [pid 9] boom").is_none());

        // The other built-ins still win where they should: an ISO head is
        // read as ISO even when a ctime-shaped string trails it.
        assert_eq!(
            at("2026-08-15T10:26:09Z said [Sat Aug 15 11:00:00.0 2026]"),
            at("2026-08-15T10:26:09Z said nothing")
        );
    }

    /// A declared pattern REPLACES the built-ins rather than outranking
    /// them: a line it does not match carries no timestamp at all, even
    /// one holding an ISO stamp. That is what makes a multi-line entry
    /// work — a stack-trace line mentioning a date must not start a new
    /// entry — and it is the opposite of the natural assumption.
    ///
    /// The built-ins being shared for the process rather than owned per
    /// extractor must not change any of that: the store that declares a
    /// pattern and the store next to it that does not read their own
    /// clocks.
    #[test]
    fn a_declared_pattern_replaces_the_builtins() {
        let custom = Extractor::new(Some(r"at=(\d{10})"), Some("%s"), true).unwrap();
        let plain = Extractor::new(None, None, true).unwrap();
        // Both clocks are in the line; each extractor reads its own.
        let line = "2026-08-15T10:26:09Z ready at=1786778625";
        assert_eq!(custom.extract(line), Some(1_786_778_625_000));
        assert_eq!(plain.extract(line), Some(1_786_789_569_000));
        // The declared pattern absent, and a built-in one present.
        assert_eq!(custom.extract("2026-08-15T10:26:09Z no marker"), None);
        assert!(plain.extract("2026-08-15T10:26:09Z no marker").is_some());
    }

    /// Syslog's stamp has no year: it is the latest one that does not put
    /// the line after the data holding it, which also carries a file that
    /// spans New Year without any state between lines.
    #[test]
    fn a_syslog_stamp_takes_the_latest_year_not_after_the_anchor() {
        let e = Extractor::new(None, None, true).unwrap();
        let at = |s: &str| {
            e.extract(s).map(|ms| {
                Utc.timestamp_millis_opt(ms as i64)
                    .unwrap()
                    .format("%Y-%m-%d")
                    .to_string()
            })
        };
        let line = "Oct  1 00:00:02 services01 systemd[1]: logrotate.service: Succeeded.";

        e.anchor(
            Utc.with_ymd_and_hms(2025, 10, 1, 6, 0, 0)
                .unwrap()
                .timestamp_millis() as u64,
        );
        let same_day = at(line).unwrap();
        assert!(same_day.starts_with("2025-10-01"), "{same_day}");

        // The file ends in January: its December lines belong to the year before.
        e.anchor(
            Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0)
                .unwrap()
                .timestamp_millis() as u64,
        );
        assert!(at("Dec 31 23:59:59 h a").unwrap().starts_with("2025-12-31"));
        assert!(at("Jan  1 00:00:01 h a").unwrap().starts_with("2026-01-01"));

        // 29 February skips a year that has none.
        e.anchor(
            Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0)
                .unwrap()
                .timestamp_millis() as u64,
        );
        assert!(at("Feb 29 12:00:00 h a").unwrap().starts_with("2024-02-29"));

        // Not a syslog head.
        assert!(at("Foo  1 00:00:02 h a").is_none());
        assert!(at("  Oct  1 00:00:02 indented").is_none());
    }

    /// A yearless syslog line and an ISO one in the same set are both
    /// stamped with nothing declared, and a declared yearless format gets
    /// the same year rule.
    #[test]
    fn a_declared_format_without_a_year_is_resolved_like_the_builtin() {
        let anchor = Utc
            .with_ymd_and_hms(2025, 10, 1, 6, 0, 0)
            .unwrap()
            .timestamp_millis();
        let plain = Extractor::new(None, None, true).unwrap();
        plain.anchor(anchor as u64);
        assert_eq!(
            plain.extract("2025-10-01T00:00:02Z h a"),
            plain.extract("Oct  1 00:00:02 h a")
        );

        let custom = Extractor::new(
            Some(r"^(\w{3} [ \d]\d \d\d:\d\d:\d\d)"),
            Some("%b %e %H:%M:%S"),
            true,
        )
        .unwrap();
        custom.anchor(anchor as u64);
        assert_eq!(
            custom.extract("Oct  1 00:00:02 h a"),
            plain.extract("Oct  1 00:00:02 h a")
        );

        let dated = Extractor::new(
            Some(r"^(\d{4} \w{3} \d\d \d\d:\d\d:\d\d)"),
            Some("%Y %b %d %H:%M:%S"),
            true,
        )
        .unwrap();
        assert!(
            dated.extract("1999 Oct 01 00:00:00 x").unwrap()
                < anchor as u64 - 20 * 365 * 86_400_000 / 4
        );
    }

    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("timberfs-import-test-{}-{n}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn cfg() -> Config {
        Config {
            chunk_size: 1024,
            level: 1,
            flush_age_ms: u64::MAX,
        }
    }

    /// Stamped lines whose padding is pseudo-random, so a store of a few
    /// thousand of them is several filesystem blocks of compressed data (the
    /// size retention aims a couple of blocks below its budget).
    fn lines(from: usize, to: usize) -> String {
        let mix = |mut x: u64| {
            x = x.wrapping_add(0x9e3779b97f4a7c15);
            x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
            x ^ (x >> 31)
        };
        (from..to)
            .map(|i| {
                format!(
                    "2026-10-03T{:02}:{:02}:{:02}Z line {i} {:016x}{:016x}{:016x}\n",
                    10 + i / 3600,
                    i / 60 % 60,
                    i % 60,
                    mix(i as u64 * 3),
                    mix(i as u64 * 3 + 1),
                    mix(i as u64 * 3 + 2)
                )
            })
            .collect()
    }

    fn import_file(src: &Path, dest: &Path) -> anyhow::Result<()> {
        cmd_import(
            &[src.to_path_buf()],
            dest,
            cfg(),
            ImportOpts {
                time: crate::bark::TimeFormat {
                    regex: None,
                    format: None,
                    utc: true,
                },
                quick: false,
                index: false,
                wal: false,
            },
        )
    }

    /// What the store holds, and where its tape starts.
    fn stored(dest: &Path) -> (u64, Vec<u8>) {
        let (dir, name) = resolve_backing(dest).unwrap();
        let mut f = crate::store::FileStore::open(&dir, &name, &cfg()).unwrap();
        let start = f.tape_start();
        let size = f.size();
        (start, f.read(0, size as u32).unwrap())
    }

    #[test]
    fn a_grown_file_adds_only_the_growth() {
        let d = TempDir::new();
        let (src, dest) = (d.0.join("app.log"), d.0.join("store.log"));
        fs::write(&src, lines(0, 200)).unwrap();
        import_file(&src, &dest).unwrap();
        let before = stored(&dest).1;

        fs::write(&src, lines(0, 260)).unwrap();
        import_file(&src, &dest).unwrap();

        let (start, now) = stored(&dest);
        assert_eq!(start, 0);
        assert_eq!(now, fs::read(&src).unwrap(), "the store is the file");
        assert!(now.starts_with(&before));
    }

    #[test]
    fn a_store_whose_head_retention_dropped_still_resumes() {
        let d = TempDir::new();
        let (src, dest) = (d.0.join("app.log"), d.0.join("store.log"));
        fs::write(&src, lines(0, 3000)).unwrap();
        import_file(&src, &dest).unwrap();

        // What a writer's retention tick does: drop the oldest chunks.
        let (dir, name) = resolve_backing(&dest).unwrap();
        let mut st = crate::store::Store::open(&dir, cfg()).unwrap();
        let comp = st.files.get(&name).unwrap().comp_size;
        st.enforce_retention(&name, None, Some(comp / 2), None)
            .unwrap();
        drop(st);
        let (start, _) = stored(&dest);
        assert!(start > 0, "the head is gone");

        fs::write(&src, lines(0, 3200)).unwrap();
        import_file(&src, &dest).unwrap();

        let (start, now) = stored(&dest);
        let file = fs::read(&src).unwrap();
        assert_eq!(
            &now[..],
            &file[start as usize..],
            "what remains is the file's tail"
        );
        assert_eq!(start + now.len() as u64, file.len() as u64);
    }

    #[test]
    fn a_line_imported_in_part_is_completed_in_place() {
        let d = TempDir::new();
        let (src, dest) = (d.0.join("app.log"), d.0.join("store.log"));
        let mut text = lines(0, 50);
        text.push_str("2026-10-03T11:00:00Z a line still being writ");
        fs::write(&src, &text).unwrap();
        import_file(&src, &dest).unwrap();

        text.push_str("ten when the import ran\n");
        text.push_str(&lines(50, 80));
        fs::write(&src, &text).unwrap();
        import_file(&src, &dest).unwrap();

        assert_eq!(
            stored(&dest).1,
            text.as_bytes(),
            "byte for byte, no duplicated start"
        );
    }

    #[test]
    fn a_rewrite_that_begins_the_same_is_refused() {
        let d = TempDir::new();
        let (src, dest) = (d.0.join("app.log"), d.0.join("store.log"));
        fs::write(&src, lines(0, 1500)).unwrap();
        import_file(&src, &dest).unwrap();
        let held = stored(&dest).1;

        // Same start, same size, different bytes where the store ends (a store
        // several windows long, so the edit is outside the head's window).
        let mut text = lines(0, 1500).into_bytes();
        let at = text.len() - 100;
        text[at..].fill(b'X');
        fs::write(&src, &text).unwrap();
        let e = import_file(&src, &dest).unwrap_err();
        assert!(e.to_string().contains("differs from the source"), "{e}");
        assert_eq!(stored(&dest).1, held, "nothing was written");
    }

    #[test]
    fn a_truncated_file_that_begins_the_same_is_refused() {
        let d = TempDir::new();
        let (src, dest) = (d.0.join("app.log"), d.0.join("store.log"));
        fs::write(&src, lines(0, 200)).unwrap();
        import_file(&src, &dest).unwrap();

        fs::write(&src, lines(0, 100)).unwrap();
        let e = import_file(&src, &dest).unwrap_err();
        assert!(e.to_string().contains("smaller than"), "{e}");
    }

    #[test]
    fn a_different_file_after_the_store_is_appended_not_mistaken_for_a_regrowth() {
        let d = TempDir::new();
        let (a, b, dest) = (d.0.join("a.log"), d.0.join("b.log"), d.0.join("store.log"));
        fs::write(&a, lines(0, 100)).unwrap();
        import_file(&a, &dest).unwrap();

        let later: String = (0..150)
            .map(|i| {
                format!(
                    "2026-10-03T12:{:02}:{:02}Z other {i} padding padding\n",
                    i / 60,
                    i % 60
                )
            })
            .collect();
        fs::write(&b, &later).unwrap();
        import_file(&b, &dest).unwrap();

        let want = format!("{}{later}", lines(0, 100));
        assert_eq!(stored(&dest).1, want.as_bytes());
    }

    #[test]
    fn a_file_whose_start_changed_is_refused_even_when_its_end_matches() {
        let d = TempDir::new();
        let (src, dest) = (d.0.join("app.log"), d.0.join("store.log"));
        fs::write(&src, lines(0, 1500)).unwrap();
        import_file(&src, &dest).unwrap();
        let held = stored(&dest).1;

        // The join is untouched; the first line's text is not (a store several
        // windows long, so the edit is outside the tail's window).
        let mut text = lines(0, 1560).into_bytes();
        text[30..40].fill(b'Y');
        fs::write(&src, &text).unwrap();
        let e = import_file(&src, &dest).unwrap_err();
        assert!(e.to_string().contains("differs from the source"), "{e}");
        assert_eq!(stored(&dest).1, held, "nothing was written");
    }
}
