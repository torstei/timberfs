//! `timberfs import`: convert an existing plain log file into a timberfs
//! backing pair, stamping chunks with timestamps PARSED FROM THE LOG LINES
//! (the write time of historical data is meaningless).
//!
//!     timberfs import /var/log/old-app.log backing/app.log
//!
//! Timestamp extraction per line, first match wins. Auto-detected
//! timestamps must sit at the START of the line (which also makes
//! indented continuation lines inherit naturally):
//!   - RFC3339/ISO-8601 and friends: `2026-07-10T09:23:45.123+02:00`,
//!     space instead of T, `.` as the date separator, and `.`/`,`/`:`
//!     before the fraction (logback's `yyyy.MM.dd HH:mm:ss:SSS` included)
//!   - a leading epoch in seconds or milliseconds
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

use anyhow::{bail, Context};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use regex::Regex;

use crate::query::{fmt_ms, resolve_backing};
use crate::store::{self, Config, Store};

/// Give up if none of the first this-many lines have a timestamp.
const DETECT_WINDOW: usize = 1000;

pub struct Extractor {
    custom: Option<(Regex, String)>,
    iso: Regex,
    clf: Regex,
    epoch: Regex,
    utc: bool,
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
        Ok(Extractor {
            custom,
            iso: Regex::new(
                r"^(\d{4})[.-](\d{2})[.-](\d{2})[T ](\d{2}:\d{2}:\d{2})(?:[.,:](\d{1,9}))?(Z|[+-]\d{2}:?\d{2})?",
            )
            .unwrap(),
            clf: Regex::new(r"\[(\d{2}/[A-Z][a-z]{2}/\d{4}:\d{2}:\d{2}:\d{2} [+-]\d{4})\]")
                .unwrap(),
            epoch: Regex::new(r"^(\d{13}|\d{10})\b").unwrap(),
            utc,
        })
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

        if let Some(c) = self.iso.captures(head) {
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

        if let Some(c) = self.clf.captures(head) {
            let ms = DateTime::parse_from_str(c.get(1).unwrap().as_str(), "%d/%b/%Y:%H:%M:%S %z")
                .ok()?
                .timestamp_millis();
            return u64::try_from(ms).ok();
        }

        if let Some(c) = self.epoch.captures(head) {
            let digits = c.get(1).unwrap().as_str();
            let n: u64 = digits.parse().ok()?;
            return Some(if digits.len() == 13 { n } else { n * 1000 });
        }

        None
    }
}

#[allow(clippy::too_many_arguments)]
/// Re-import safety: the target is its own checkpoint. The source must be
/// a pure-growth descendant of what was imported, which we prove by
/// comparing already-imported chunks against the same source byte ranges
/// (all chunks, or first/middle/last with `quick`) BEFORE writing anything.
fn verify_prefix(
    chunks: &[crate::format::ChunkRecord],
    trunk_path: &Path,
    src: &File,
    quick: bool,
) -> anyhow::Result<()> {
    use std::os::unix::fs::FileExt;
    let trunk = File::open(trunk_path)?;
    let picks: Vec<usize> = if quick && chunks.len() > 3 {
        vec![0, chunks.len() / 2, chunks.len() - 1]
    } else {
        (0..chunks.len()).collect()
    };
    for i in picks {
        let c = chunks[i];
        let mut comp = vec![0u8; c.comp_len as usize];
        trunk.read_exact_at(&mut comp, c.comp_start)?;
        let imported = zstd::stream::decode_all(&comp[..])?;
        let mut current = vec![0u8; c.uncomp_len as usize];
        src.read_exact_at(&mut current, c.uncomp_start)
            .context("reading the source range matching already-imported data")?;
        if imported != current {
            bail!(
                "already-imported data differs from the source (bytes {}..{}) — \
                 rotated or rewritten file? import it to a new target instead",
                c.uncomp_start,
                c.uncomp_end()
            );
        }
    }
    Ok(())
}

/// FNV-1a 128 of a line (trailing newline stripped). Overlap dedup only
/// needs collisions to be unlikelier than hardware failure, not
/// cryptography: for 10^8 distinct lines the collision odds are ~10^-23.
fn line_hash(line: &[u8]) -> u128 {
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
fn overlap_line_counts(
    chunks: &[crate::format::ChunkRecord],
    trunk_path: &Path,
    t0: u64,
) -> anyhow::Result<HashMap<u128, u32>> {
    use std::os::unix::fs::FileExt;
    let trunk = File::open(trunk_path)?;
    let decomp = |c: &crate::format::ChunkRecord| -> anyhow::Result<Vec<u8>> {
        let mut comp = vec![0u8; c.comp_len as usize];
        trunk.read_exact_at(&mut comp, c.comp_start)?;
        Ok(zstd::stream::decode_all(&comp[..])?)
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
fn first_stamp(path: &Path, extractor: &Extractor) -> anyhow::Result<u64> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
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
        let records = crate::query::open_source(path)?.records;
        return Ok((
            records.first().map(|r| r.first_write_ms).unwrap_or(0),
            Source::Timber {
                trunk: path.to_path_buf(),
                records,
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
        },
    ))
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_import(
    sources_in: &[PathBuf],
    dest: &Path,
    cfg: Config,
    custom_regex: Option<&str>,
    custom_format: Option<&str>,
    utc: bool,
    quick: bool,
    index: bool,
) -> anyhow::Result<()> {
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
    fs::create_dir_all(&dir)?;
    // Flags override the store's DECLARED format (timestamp_regex /
    // timestamp_format / timestamp_utc in the manifest) — declare once,
    // and every later import of an exotic format is flag-free.
    let declared = crate::bark::time_format(crate::bark::load(&dir, &name).as_ref());
    let extractor = Extractor::new(
        custom_regex.or(declared.regex.as_deref()),
        custom_format.or(declared.format.as_deref()),
        utc || declared.utc,
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

    if sources.is_empty() {
        // Every source was empty. The import still succeeds — and still
        // materializes the destination: an empty store that EXISTS is the
        // attestation a shipping pipeline needs to keep ingesting.
        if index {
            crate::bark::declare_index(&dir, &name)?;
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

    // A non-empty target. A single plain source STARTING WHERE THE STORE
    // STARTS is the same file, regrown: verify the imported prefix and
    // resume (truncated/rewritten files are refused here). Every other
    // plain source is handled per-source below by its first timestamp —
    // after the store's end it simply appends; inside the store's window
    // it is deduplicated line-by-line against the overlap. Timberfs
    // sources append behind the ordering guard (segments already covered
    // are skipped below).
    let mut resume_from: u64 = 0;
    let mut last_ts: Option<u64> = None;
    {
        let f = st.files.get(&name).unwrap();
        if f.size() > 0 {
            let store_first = f.chunks.first().map(|c| c.first_write_ms);
            if let (false, (t0, Source::Plain(src_path))) = (multi, &sources[0]) {
                if Some(*t0) == store_first {
                    resume_from = f.size();
                    if total_bytes < resume_from {
                        bail!(
                            "source ({total_bytes} bytes) is smaller than the {resume_from} bytes \
                             already imported — rotated or truncated file? import it to a new target"
                        );
                    }
                    let src = File::open(src_path)?;
                    verify_prefix(
                        &f.chunks,
                        &crate::format::trunk_path(&dir, &name),
                        &src,
                        quick,
                    )?;
                    crate::note!(
                        "timberfs: {} of {} bytes already imported and verified{}; resuming",
                        resume_from,
                        total_bytes,
                        if quick { " (quick)" } else { "" }
                    );
                }
            }
            // Seed timestamp inheritance with the last imported stamp.
            last_ts = f.last_write_ms();
        }
    }

    let mut line: Vec<u8> = Vec::new();
    let mut leading: Vec<Vec<u8>> = Vec::new(); // lines before the first stamp
    let mut lines: u64 = 0;
    let mut stamped: u64 = 0;
    let mut inherited: u64 = 0;
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
            Source::Timber { trunk, records } => {
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
                    last_ts = Some(records.last().unwrap().last_write_ms);
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
                .is_some_and(|(_, until)| ts.or(last_ts).is_some_and(|e| e > *until))
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
                            last_ts = Some(t);
                        }
                        continue;
                    }
                }
                ov_new += 1;
            }

            match (ts, last_ts) {
                (Some(t), _) => {
                    if !leading.is_empty() {
                        // stamp the pre-first-timestamp lines with the first one
                        let f = st.files.get_mut(&name).unwrap();
                        for l in leading.drain(..) {
                            f.append_stamped(&l, t, &cfg)?;
                            inherited += 1;
                        }
                    }
                    st.files
                        .get_mut(&name)
                        .unwrap()
                        .append_stamped(&line, t, &cfg)?;
                    stamped += 1;
                    last_ts = Some(t);
                }
                (None, Some(t)) => {
                    st.files
                        .get_mut(&name)
                        .unwrap()
                        .append_stamped(&line, t, &cfg)?;
                    inherited += 1;
                }
                (None, None) => {
                    leading.push(line.clone());
                    if leading.len() > DETECT_WINDOW {
                        bail!(
                            "no timestamp found in the first {DETECT_WINDOW} lines; \
                         try --timestamp-regex/--timestamp-format"
                        );
                    }
                }
            }

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

    if !leading.is_empty() {
        bail!(
            "no timestamp found in any of the {} line(s); \
             try --timestamp-regex/--timestamp-format",
            leading.len()
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
            if let Some(stats) =
                st.enforce_retention(&name, policy.max_age_ms, policy.max_comp_bytes)?
            {
                crate::note!(
                    "timberfs: {name}: retention dropped {} chunk(s), {} compressed bytes",
                    stats.chunks_moved,
                    stats.comp_bytes
                );
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
    if custom_regex.is_some() || utc {
        // The flags persist, like --index: the format is a property of
        // the CONTENT, declared in the manifest, and all roads converge.
        let mut map = crate::bark::load(&dir, &name).unwrap_or_default();
        if let (Some(r), Some(f)) = (custom_regex, custom_format) {
            map.insert(
                "timestamp_regex".to_string(),
                serde_json::Value::String(r.to_string()),
            );
            map.insert(
                "timestamp_format".to_string(),
                serde_json::Value::String(f.to_string()),
            );
        }
        if utc {
            map.insert("timestamp_utc".to_string(), serde_json::Value::Bool(true));
        }
        crate::bark::save(&dir, &name, &map)?;
    }
    if index {
        crate::bark::declare_index(&dir, &name)?;
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
            "{lines} lines ({stamped} stamped, {inherited} inherited)"
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
