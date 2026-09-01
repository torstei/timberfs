//! Offline access to the backing store: time-range extraction and index
//! inspection. These read the .trunk/.rings pair directly, so they work
//! whether or not the filesystem is mounted (concurrent use with a live
//! mount is safe: chunks are immutable once written and the index is
//! append-only).

use std::cell::Cell;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, Context};
use chrono::{DateTime, Local, LocalResult, NaiveDateTime, NaiveTime, SecondsFormat, TimeZone};

use crate::format::{self, ChunkRecord};

pub fn fmt_ms_rfc3339(ms: u64) -> String {
    match Local.timestamp_millis_opt(ms as i64) {
        LocalResult::Single(dt) => dt.to_rfc3339_opts(SecondsFormat::Millis, true),
        _ => format!("@{ms}ms"),
    }
}

pub fn fmt_ms(ms: u64) -> String {
    match Local.timestamp_millis_opt(ms as i64) {
        LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        _ => format!("@{ms}ms"),
    }
}

/// Accepts RFC3339, "YYYY-MM-DD HH:MM[:SS]" (dots as date separators
/// also work — paste straight from logback-style logs), a bare
/// "YYYY-MM-DD" (midnight, so --from 2026-07-10 --to 2026-07-11 selects
/// exactly that day), bare "HH:MM[:SS[.mmm]]" (today, local time), or
/// unix seconds/milliseconds. Zoneless forms are local time.
pub fn parse_time(s: &str) -> anyhow::Result<u64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis() as u64);
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
        "%Y.%m.%d %H:%M:%S",
        "%Y.%m.%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(dt) = Local.from_local_datetime(&naive).earliest() {
                return Ok(dt.timestamp_millis() as u64);
            }
        }
    }
    // A bare date is midnight local time.
    for fmt in ["%Y-%m-%d", "%Y.%m.%d"] {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, fmt) {
            if let Some(dt) = Local
                .from_local_datetime(&d.and_time(NaiveTime::MIN))
                .earliest()
            {
                return Ok(dt.timestamp_millis() as u64);
            }
        }
    }
    for fmt in ["%H:%M:%S%.3f", "%H:%M:%S", "%H:%M"] {
        if let Ok(t) = NaiveTime::parse_from_str(s, fmt) {
            let naive = Local::now().date_naive().and_time(t);
            if let Some(dt) = Local.from_local_datetime(&naive).earliest() {
                return Ok(dt.timestamp_millis() as u64);
            }
        }
    }
    if let Ok(n) = s.parse::<u64>() {
        // Heuristic: values this large are already milliseconds.
        return Ok(if n > 100_000_000_000 { n } else { n * 1000 });
    }
    bail!(
        "unrecognized time {s:?} (try RFC3339, 'YYYY-MM-DD [HH:MM[:SS]]', 'HH:MM[:SS]', \
         or unix seconds)"
    )
}

/// Resolve a user-supplied path (logical name, .trunk or .rings) to the
/// backing directory and logical file name.
pub fn resolve_backing(input: &Path) -> anyhow::Result<(PathBuf, String)> {
    let file_name = input
        .file_name()
        .and_then(|s| s.to_str())
        .with_context(|| format!("bad path {}", input.display()))?;
    let base = file_name
        .strip_suffix(&format!(".{}", format::TRUNK_EXT))
        .or_else(|| file_name.strip_suffix(&format!(".{}", format::RINGS_EXT)))
        .unwrap_or(file_name);
    let dir = match input.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok((dir, base.to_string()))
}

/// Write destinations are never read, so a destination that exists as a
/// plain file is always a mistake — most likely a forgotten destination
/// argument after a shell glob (`import /logs/*` makes the last match the
/// destination). A legitimate existing target is a pair, whose logical
/// name is not a file; its .trunk/.rings paths are allowed.
pub fn ensure_dest_is_not_plain_file(dest: &Path, verb: &str) -> anyhow::Result<()> {
    let artifact = matches!(
        dest.extension().and_then(|e| e.to_str()),
        Some(format::TRUNK_EXT) | Some(format::RINGS_EXT)
    );
    if dest.is_file() && !artifact {
        bail!(
            "destination {} is an existing file — did you forget the destination argument? \
             (a glob makes its last match the destination; {verb} writes <dest>.trunk/.rings \
             and never reads the destination itself)",
            dest.display()
        );
    }
    Ok(())
}

/// True when the path names a `.timber` transfer bundle.
pub fn is_bundle(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("timber")
}

/// A readable timberfs source: index records plus the file the compressed
/// frames live in, with comp offsets absolute in that file. Backing pairs
/// and `.timber` bundles look identical from here on — bundles are
/// first-class read-only logs (tar stores members contiguously and
/// uncompressed, so the trunk member is just a trunk at an offset).
pub struct SourceHandle {
    pub records: Vec<ChunkRecord>,
    pub file: File,
    pub bark: Option<serde_json::Map<String, serde_json::Value>>,
    /// The store's seqlock as it stood just BEFORE these records were
    /// read (`None` for a bundle, which nothing can collapse). The
    /// .grain is positional, so a retention head-drop renumbers the rings
    /// and the grain together; pairing records from one generation with
    /// filters from the other would skip chunks that do match. Sampling
    /// here — not at the grain load — is what makes the comparison in
    /// `select_chunks` cover the rings read too.
    pub seq_at_open: Option<u64>,
}

pub fn open_source(input: &Path) -> anyhow::Result<SourceHandle> {
    if is_bundle(input) {
        let file = File::open(input).with_context(|| format!("opening {}", input.display()))?;
        let mut archive = tar::Archive::new(&file);
        let mut rings_bytes: Option<Vec<u8>> = None;
        let mut trunk_pos: Option<(u64, u64)> = None;
        let mut bark: Option<serde_json::Map<String, serde_json::Value>> = None;
        for entry in archive.entries()? {
            let mut entry = entry?;
            let member = entry.path()?.to_string_lossy().to_string();
            if member.ends_with(&format!(".{}", format::RINGS_EXT)) {
                let mut v = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut v)?;
                rings_bytes = Some(v);
            } else if member.ends_with(&format!(".{}", format::TRUNK_EXT)) {
                trunk_pos = Some((entry.raw_file_position(), entry.header().entry_size()?));
            } else if member.ends_with(&format!(".{}", format::BARK_EXT)) {
                let mut v = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut v)?;
                if let Ok(serde_json::Value::Object(m)) = serde_json::from_slice(&v) {
                    bark = Some(m);
                }
            }
        }
        let rings_bytes = rings_bytes.with_context(|| {
            format!(
                "{} has no .rings member — not a timberfs bundle",
                input.display()
            )
        })?;
        let (trunk_base, trunk_size) = trunk_pos.with_context(|| {
            format!(
                "{} has no .trunk member — not a timberfs bundle",
                input.display()
            )
        })?;
        let mut records = format::parse_index_bytes(&rings_bytes)?;
        if records.last().is_some_and(|c| c.comp_end() > trunk_size) {
            bail!(
                "bundle {} is corrupt: the index points past the trunk member",
                input.display()
            );
        }
        for r in &mut records {
            r.comp_start += trunk_base;
        }
        return Ok(SourceHandle {
            records,
            file,
            bark,
            seq_at_open: None,
        });
    }
    let (dir, base) = resolve_backing(input)?;
    // Best-effort: a collapse that started but never finished (a writer
    // crash) leaves a `.trim` marker; reconcile it before reading so we
    // never see a half-landed cut. A read-only caller without write
    // access to the directory just leaves it for the next writer.
    let _ = crate::store::reconcile_trim(&dir, &base);
    let rings = format::rings_path(&dir, &base);
    if !rings.exists() {
        bail!(
            "no index file {} (expected a timberfs backing file, its logical name, \
             or a .timber bundle)",
            rings.display()
        );
    }
    let seq_at_open = Some(crate::store::read_seq(&dir, &base));
    let records =
        format::read_index(&rings).with_context(|| format!("reading index {}", rings.display()))?;
    let file = File::open(format::trunk_path(&dir, &base))
        .with_context(|| format!("opening {}", format::trunk_path(&dir, &base).display()))?;
    let bark = crate::bark::load(&dir, &base);
    Ok(SourceHandle {
        records,
        file,
        bark,
        seq_at_open,
    })
}

/// Identity for the collapse-head seqlock guard (store.rs): `None` for a
/// `.timber` bundle (its trunk member is written once and never mutated
/// again, so there's nothing a reader could race), `Some(dir, name)` for
/// a live backing pair, which a concurrent writer's retention can
/// collapse out from under a standalone reader in another process.
pub(crate) fn seq_guard(input: &Path) -> Option<(PathBuf, String)> {
    if is_bundle(input) {
        None
    } else {
        resolve_backing(input).ok()
    }
}

/// Read+decompress chunk `c`, safe against a concurrent `collapse_head`
/// (store.rs): a standalone reader's `.trunk` pread can land mid-collapse,
/// at an offset the kernel is actively shifting underneath it. Bracket
/// the read with the store's seqlock (odd = a collapse is in flight; a
/// value that changed since we sampled it means one just finished); on
/// either signal, re-open `input` fresh and re-locate the SAME chunk by
/// its write-time window and length (offsets shift under a collapse,
/// write times and lengths don't), then retry. A chunk the race retained
/// away comes back `None` — a legitimate outcome (the same as if the read
/// had started a moment later), never stale or garbage bytes.
fn read_chunk(
    input: &Path,
    guard: &Option<(PathBuf, String)>,
    handle: &mut SourceHandle,
    c: ChunkRecord,
) -> anyhow::Result<Option<Vec<u8>>> {
    match read_chunk_raw(input, guard, handle, c)? {
        Some(comp) => Ok(Some(zstd::stream::decode_all(&comp[..]).with_context(
            || "decompressing a stored chunk — the .trunk may be corrupt",
        )?)),
        None => Ok(None),
    }
}

/// The COMPRESSED frame of chunk `c`, under the same guard. Replication
/// moves frames verbatim, so it must not pay for a decompression it has no
/// use for — and the guard is far too subtle to have two copies of.
pub(crate) fn read_chunk_raw(
    input: &Path,
    guard: &Option<(PathBuf, String)>,
    handle: &mut SourceHandle,
    mut c: ChunkRecord,
) -> anyhow::Result<Option<Vec<u8>>> {
    // Bound the retries: a writer that died mid-collapse (before it could
    // bump the seqlock back to even) would otherwise leave the counter odd
    // forever and spin this loop — surface a clear error instead.
    let mut tries = 0usize;
    loop {
        let before = guard.as_ref().map(|(d, n)| crate::store::read_seq(d, n));
        let mut comp = vec![0u8; c.comp_len as usize];
        let read_res = handle.file.read_exact_at(&mut comp, c.comp_start);
        let raced = if let (Some(before), Some((d, n))) = (before, guard.as_ref()) {
            let after = crate::store::read_seq(d, n);
            before % 2 == 1 || after % 2 == 1 || before != after
        } else {
            false
        };
        if !raced {
            read_res.context("reading a stored chunk")?;
            return Ok(Some(comp));
        }
        tries += 1;
        if tries > 64 {
            anyhow::bail!(
                "{}: reads keep racing an in-flight collapse (or a writer died \
                 mid-collapse, leaving the store's seqlock unfinished) — rerun once \
                 a writer has reconciled it",
                input.display()
            );
        }
        *handle = open_source(input)?;
        // Re-locate the SAME chunk in the fresh index by its stable content
        // identity — the write-time window and BOTH lengths; offsets shift
        // under a collapse, these don't. A collapse only ever shifts
        // survivors DOWN, so on the vanishing chance two survivors share
        // that identity, take the one whose (shifted) comp_start is nearest
        // at or below the old one. No match => the race retained it away.
        let mut best: Option<ChunkRecord> = None;
        for r in &handle.records {
            let same = r.first_write_ms == c.first_write_ms
                && r.last_write_ms == c.last_write_ms
                && r.uncomp_len == c.uncomp_len
                && r.comp_len == c.comp_len
                && r.comp_start <= c.comp_start;
            if same && best.is_none_or(|b| r.comp_start > b.comp_start) {
                best = Some(*r);
            }
        }
        match best {
            Some(r) => c = r,
            None => return Ok(None),
        }
    }
}

/// Window + --has chunk selection, shared by query and grep: an
/// interval-overlap scan of the index, then the .grain Bloom pre-filter
/// (every token of every --has argument must probably be in the chunk).
/// Exact, entry-level filtering stays downstream.
///
/// `from_chunk` is where to START, by chunk number rather than by time.
/// It belongs here, beside the window, because every bounded read reaches
/// its chunks through this function — a seek implemented per read path is
/// a member one of them would accept and ignore.
#[allow(clippy::too_many_arguments)]
pub fn select_chunks(
    file: &Path,
    chunks: &[ChunkRecord],
    seq_at_open: Option<u64>,
    from_ms: u64,
    to_ms: u64,
    from_chunk: Option<u64>,
    has: &[String],
    any_of: &[String],
) -> anyhow::Result<(Vec<(usize, ChunkRecord)>, usize)> {
    let mut selected: Vec<(usize, ChunkRecord)> = chunks
        .iter()
        .enumerate()
        .filter(|(_, c)| c.last_write_ms >= from_ms && c.first_write_ms <= to_ms)
        // A seek below everything the store still holds lands on the oldest
        // survivor: the caller's position was dropped, and only the caller
        // can report that.
        .filter(|(_, c)| from_chunk.is_none_or(|n| c.seq >= n))
        .map(|(i, c)| (i, *c))
        .collect();
    let in_window = selected.len();

    if !has.is_empty() || !any_of.is_empty() {
        let mut tokens: Vec<Vec<u8>> = Vec::new();
        for h in has {
            let t = crate::grain::tokenize_query(h);
            if t.is_empty() {
                bail!(
                    "--has {h:?} contains no indexable tokens \
                     (runs of 3-64 alphanumeric characters)"
                );
            }
            tokens.extend(t);
        }
        // Repeated arguments/tokens are a set: checking a Bloom filter
        // for the same token twice buys nothing.
        tokens.sort();
        tokens.dedup();
        let grain = if is_bundle(file) {
            None
        } else {
            let (dir, base) = resolve_backing(file)?;
            let g = crate::grain::load(&crate::format::grain_path(&dir, &base)).ok();
            // Only trust it if the store has not been head-dropped since
            // the records were read: a collapse renumbers rings and grain
            // together, and either half alone answers for the wrong
            // chunks. Odd = one is in flight. Dropping the grain costs a
            // scan of the window; keeping a mismatched one would silently
            // skip matching chunks, so this errs to the slow answer.
            let after = crate::store::read_seq(&dir, &base);
            match seq_at_open {
                Some(before) if before == after && after.is_multiple_of(2) => g,
                None => g,
                _ => None,
            }
        };
        // OR-of-ANDs: a chunk survives when the AND tokens are all
        // present AND (no alternatives, or at least one alternative's
        // tokens are all present) — each branch exact, so the union is.
        let groups: Vec<Vec<Vec<u8>>> = any_of
            .iter()
            .map(|a| crate::grain::tokenize_query(a))
            .filter(|g| !g.is_empty())
            .collect();
        match grain {
            Some(g) => {
                selected.retain(|(i, _)| {
                    g.may_contain_all(*i, &tokens)
                        && (groups.is_empty()
                            || groups.iter().any(|grp| g.may_contain_all(*i, grp)))
                });
            }
            None => {
                eprintln!(
                    "timberfs: no .grain index — --has cannot skip anything here \
                     (run `timberfs reindex` on the log to build one); scanning the window"
                );
            }
        }
    }
    Ok((selected, in_window))
}

/// Print the bytes stamped inside [from, to]. Selection is at chunk
/// granularity: every chunk whose time window overlaps the requested range
/// is emitted in full, chosen by an interval-overlap scan of the index.
/// (A scan, not a binary search: imported files carry logged timestamps
/// whose windows are only mostly sorted. The index is 48 bytes per chunk,
/// so scanning it is negligible next to decompressing one chunk.)
/// How much the write-time selection is widened when the logline filter
/// can verify entries exactly: catches lines written slightly before or
/// after the stamps they carry (buffered producers), while the filter
/// keeps the OUTPUT exactly inside the asked window.
pub(crate) const WIDEN_MS: u64 = 60_000;

/// A whole search, as one value.
///
/// The point of grouping these rather than passing fifteen parameters is
/// that a search becomes a THING — one that a caller can build, hand
/// around, and (next) serialize. The CLI flags build one of these; a
/// `--query` document will deserialize into the same one, so the two
/// surfaces cannot drift into being two dialects of the same question.
/// The grouping deliberately mirrors the shape that document will have.
#[derive(Debug, Default, Clone)]
pub struct Query {
    /// The stores to read. Paths today; a label selection resolves to
    /// these.
    pub sources: Vec<std::path::PathBuf>,
    /// Where a previous read left each store, by identity. A store absent
    /// from this is read from the start of the window: for a TAIL that is
    /// right — it is new, and everything it holds is new — and a bounded
    /// walk that wants otherwise pins its store set instead.
    pub cursor: std::collections::BTreeMap<String, u64>,
    pub window: Window,
    pub matching: Match,
    pub limit: Limit,
    pub output: Output,
    pub follow: Follow,
}

/// Where to start and stop.
#[derive(Debug, Default, Clone)]
pub struct Window {
    /// Both are LOGLINE time in a windowed read and WRITE time under
    /// follow — the axis switches with the mode rather than being chosen,
    /// which is a known defect the document format is meant not to
    /// inherit. Grouped here so there is one place to put the axis when
    /// it becomes explicit.
    pub from: Option<u64>,
    pub to: Option<u64>,
    /// Where to START, by chunk NUMBER: a place on the tape rather than
    /// a time, and exact where a timestamp can match two chunks sharing a
    /// boundary millisecond. It resumes a FOLLOWING read from a consumer's
    /// cursor and SEEKS a bounded one — the same operation, which is why
    /// it is not confined to the first.
    pub from_chunk: Option<u64>,
}

/// Which entries. Only the index-riding predicates live here; the richer
/// vocabulary is `timber-filter`'s.
#[derive(Debug, Default, Clone)]
pub struct Match {
    /// Every predicate here must match. The CLI's `--has` lands here.
    pub all: Vec<crate::grep::Pred>,
    /// At least one of these must, when any are given. The CLI's `--any`.
    pub any: Vec<crate::grep::Pred>,
    /// None of these may match. No flag produces one — it is the query
    /// document's, because a chunk sweep cannot express it (see
    /// `Granularity`) and the flags are a chunk sweep.
    pub none: Vec<crate::grep::Pred>,
    /// What the predicate SELECTS. The token index can only ever skip
    /// whole chunks, so these are two different answers to the same
    /// terms and the difference is not small: on a 1.2 GiB store, a term
    /// in five entries selects 398 chunks and 325k lines.
    pub granularity: Granularity,
}

/// Whether a predicate names chunks or entries.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// Chunks that may contain the terms, emitted WHOLE. Cheap — the
    /// index alone answers it, nothing is decompressed to decide — and
    /// the answer is a superset. What `--has` has always meant.
    #[default]
    Chunks,
    /// The entries that actually contain them. The index still skips
    /// chunks it can prove are irrelevant; the survivors are then judged
    /// one entry at a time.
    Entries,
}

impl Match {
    pub fn is_empty(&self) -> bool {
        self.spec().is_empty()
    }

    pub fn spec(&self) -> crate::grep::PredSpec {
        crate::grep::PredSpec {
            all: self.all.clone(),
            any: self.any.clone(),
            none: self.none.clone(),
        }
    }

    /// What the token index can prove, as (`--has`, `--any`) terms. An
    /// execution detail: the predicate means what it says whatever this
    /// returns, and this only decides how much gets read.
    pub fn index_terms(&self) -> (Vec<String>, Vec<String>) {
        self.spec().pushdown()
    }

    /// The entry-level predicates, when the caller asked for entries.
    /// `None` leaves the read chunk-granular.
    pub fn entry_preds(&self) -> anyhow::Result<Option<crate::grep::Preds>> {
        if self.granularity != Granularity::Entries || self.is_empty() {
            return Ok(None);
        }
        Ok(Some(crate::grep::Preds::compile(self.spec())?))
    }
}

/// How much. `max` caps forward from the start and `tail` takes the last
/// N — different operations, not one with a sign, which is why they
/// conflict rather than compose.
#[derive(Debug, Default, Clone)]
pub struct Limit {
    pub max: Option<u64>,
    pub tail: Option<u64>,
    /// The same two bounds counted in CHUNKS. A separate field rather
    /// than a unit tag because the read paths differ: an entry bound
    /// needs decompression and framing to know where to stop, a chunk
    /// bound is answered from the index without reading anything.
    pub max_chunks: Option<u64>,
    pub tail_chunks: Option<u64>,
    /// How LONG the read may take, in milliseconds. The other bounds cap
    /// how much comes back; a fleet read is slow because it READS a lot,
    /// not because it matches a lot, so neither of them bounds the wait.
    /// Answered with whatever was gathered, which is what a client-side
    /// timeout cannot do — it drops the connection and everything on it.
    pub deadline_ms: Option<u64>,
    /// Which of these came from this machine's ceiling rather than from
    /// the request.
    pub imposed: Imposed,
}

/// This machine's ceilings as they bear on ONE read: what it declares,
/// and which of them it had to put on this request.
///
/// A bound the SERVICE put there is named apart when it stops a read,
/// because "you asked for this much" and "this is all one answer may
/// carry" are different facts and only the second says to page.
#[derive(Debug, Default, Clone, Copy)]
pub struct Imposed {
    /// Declared to the caller in `stream-start`, so a page can be sized
    /// before it is asked for rather than after an answer came back
    /// short. Empty on the flag path — the ceilings bound a request from
    /// elsewhere, and an answer the operator asked for by hand must not
    /// claim a bound it was not given.
    pub declared: crate::limits::Limits,
    pub max: bool,
    pub max_chunks: bool,
    pub deadline: bool,
}

impl Imposed {
    /// What `stream-end`'s `limit=` calls a bound.
    fn name(imposed: bool, member: &'static str) -> &'static str {
        if !imposed {
            return member;
        }
        match member {
            "max.entries" => "limits.max.entries",
            "max.chunks" => "limits.max.chunks",
            _ => "limits.deadline",
        }
    }
}

/// The read's remaining time, ASKED rather than computed at each site.
///
/// A deadline is the one bound whose effect depends on the clock, so
/// "it fired" can only be asserted against a real clock as a margin — a
/// bet on how fast the machine is, which the next slow CI runner
/// collects. Behind this seam the read paths ask a value instead, and a
/// test constructs one that is already out of time. What then gets
/// asserted is what an expired budget DOES — the answer names the
/// deadline, the staircase holds, no entry is invented — which is a
/// property the code guarantees rather than a race it usually wins.
pub(crate) enum Budget {
    /// No deadline was asked for.
    Unbounded,
    /// The real one, and the only kind `cmd_query` builds.
    Wall {
        start: std::time::Instant,
        limit: std::time::Duration,
    },
    /// Out of time from the `fire_on`-th ask onwards. Tests only, and it
    /// exists for the one property a budget that expires immediately
    /// cannot express: the deadline landing PART WAY through a fleet, so
    /// the stores before it are whole and the ones after it were never
    /// opened.
    #[cfg(test)]
    OnAsk {
        asks: std::cell::Cell<u64>,
        fire_on: u64,
    },
}

impl Budget {
    pub(crate) fn of(deadline_ms: Option<u64>) -> Self {
        match deadline_ms {
            None => Budget::Unbounded,
            Some(ms) => Budget::Wall {
                start: std::time::Instant::now(),
                limit: std::time::Duration::from_millis(ms),
            },
        }
    }

    /// Has the read run out of time? Every site that used to compare an
    /// elapsed duration asks this instead, so there is one answer and one
    /// place to make it deterministic.
    pub(crate) fn expired(&self) -> bool {
        match self {
            Budget::Unbounded => false,
            Budget::Wall { start, limit } => start.elapsed() >= *limit,
            #[cfg(test)]
            Budget::OnAsk { asks, fire_on } => {
                let n = asks.get() + 1;
                asks.set(n);
                n >= *fire_on
            }
        }
    }
}

/// What comes out. Every field here shapes the OUTPUT rather than
/// selecting anything, which is why they group together and why the
/// document's response format will be a kind plus options rather than a
/// bare name.
#[derive(Debug, Default, Clone)]
pub struct Output {
    pub no_filename: bool,
    pub show_write_time: bool,
    pub null_sep: bool,
    pub records: bool,
    /// The raw escape hatch: chunks selected by write time, no entry
    /// parsing and no logline filtering. Emits the chunks' CONTENT as
    /// text — `--by-write-time`, which is a pipe's input and a person's
    /// read.
    pub by_write_time: bool,
    /// The same selection, framed: each chunk as its ring record plus the
    /// COMPRESSED frame, verbatim.
    ///
    /// Separate from `by_write_time` because they are different answers to
    /// different readers. A text dump cannot say where one chunk ended,
    /// which number it was, or what window it covered — and it costs the
    /// decompression its reader did not ask for: 502,893 bytes shipped for
    /// 23,834 stored, measured on one store.
    pub chunk_records: bool,
}

/// Reading forward as data arrives.
#[derive(Debug, Default, Clone)]
pub struct Follow {
    pub follow: bool,
    pub poll: Option<f64>,
}

impl Query {
    /// Is this search coherent? The rules that decide it belong to the
    /// QUERY, not to the command that runs one — a caller building a
    /// query from a document needs to know it will work before handing
    /// it over, and a documented example that cannot run is worse than
    /// no example.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let (Some(f), Some(t)) = (self.window.from, self.window.to) {
            if f > t {
                bail!("the window starts after it ends");
            }
        }
        // Zero reads as "answer instantly" and means "read nothing" — a
        // generator's arithmetic escaping into the request rather than
        // anything anybody asks for. Which stores a search covers is a
        // response kind, not a deadline of nought.
        if self.limit.deadline_ms == Some(0) {
            bail!(
                "a deadline of zero would read nothing; ask for the stores a search \
                 selects if that is the question"
            );
        }
        // A deadline bounds a search. A follow does not end, and "stop
        // following after N seconds" is a different request that would
        // want its own member rather than this one reinterpreted.
        if self.follow.follow && self.limit.deadline_ms.is_some() {
            bail!(
                "a deadline bounds a search, and a following read does not end; to stop \
                 following after a while, bound the process rather than the query"
            );
        }
        let following = self.follow.follow || self.limit.tail.is_some();
        // A chunk number is a PLACE on the tape, not a time — which is why
        // it composes with a window's `to`, with predicates and with either
        // axis, and why it is not confined to a following read: `from_chunk`
        // with `max: {chunks: 1}` is a seek, and a pager is nothing but
        // seeks. What it cannot compose with is a second START. Each of
        // these names one, and there is no rule for which of two would win —
        // so the answer is the refusal rather than a position the caller did
        // not ask for.
        if self.window.from_chunk.is_some() {
            if self.window.from.is_some() {
                bail!(
                    "a chunk number and a `from` timestamp are both where to START, and \
                     a read has one start — name the place OR the time"
                );
            }
            if self.limit.tail.is_some() || self.limit.tail_chunks.is_some() {
                bail!(
                    "a chunk number says where to start and a tail says how far back \
                     from the END, and a read has one start — ask for one of them"
                );
            }
            if !self.cursor.is_empty() {
                bail!(
                    "a cursor is where a previous answer left each store, and a chunk \
                     number is where to start — hand back the cursor to go on, or name \
                     the chunk to go somewhere else"
                );
            }
        }
        // A CHUNK predicate is the token index, which has nothing to skip
        // on a live stream. An ENTRY predicate is just a filter, and
        // filtering a tail is exactly what a live search is — so the
        // refusal applies to the first and not the second.
        if following
            && !self.matching.is_empty()
            && self.matching.granularity == Granularity::Chunks
        {
            bail!(
                "a chunk predicate is the token index, which selects whole chunks offline \
                 and has nothing to skip on a following read (every new chunk must be \
                 read). Ask for entries instead, filter the live stream, or run a windowed \
                 query"
            );
        }
        Ok(())
    }
}

pub fn cmd_query(q: &Query) -> anyhow::Result<()> {
    q.validate()?;
    let files = &q.sources[..];
    let (from, to, from_chunk) = (q.window.from, q.window.to, q.window.from_chunk);
    // What the INDEX can prove, which is a subset of what was asked: a
    // caseless or regex predicate reads more rather than matching less.
    let (has_terms, any_terms) = q.matching.index_terms();
    let (has, any) = (&has_terms[..], &any_terms[..]);
    let cursor = &q.cursor;
    // The index selects chunks; these judge the entries inside them.
    // Absent, the read stays chunk-granular — every entry of every chunk
    // the index let through comes out.
    let entry_preds = q.matching.entry_preds()?;
    let (max, tail) = (q.limit.max, q.limit.tail);
    let (max_chunks, tail_chunks) = (q.limit.max_chunks, q.limit.tail_chunks);
    let budget = Budget::of(q.limit.deadline_ms);
    let (follow, poll) = (q.follow.follow, q.follow.poll);
    let Output {
        no_filename,
        show_write_time,
        null_sep,
        records,
        by_write_time,
        chunk_records,
    } = q.output;
    let from_ms = from.unwrap_or(0);
    let to_ms = to.unwrap_or(u64::MAX);
    // Follow / tail is its own read path: a poll loop over newly-committed
    // chunks rather than the one-shot windowed scan. --has/--any select whole
    // chunks via the offline .grain index, which neither composes with a live
    // stream (there is nothing to skip — every new chunk must be read) nor
    // filters at line granularity; filter a follow with a pipe instead.
    if follow || tail.is_some() || tail_chunks.is_some() {
        // The has/follow and from-chunk rules live on `Query::validate`,
        // which ran above: one implementation, so the flags and a
        // document cannot come to disagree about what is coherent.
        return query_follow(
            files,
            from,
            from_chunk,
            no_filename,
            show_write_time,
            null_sep,
            records,
            tail,
            tail_chunks,
            follow,
            max,
            poll,
            entry_preds,
            q.limit.imposed,
        );
    }
    let windowed = from.is_some() || to.is_some();
    // The entry pipeline engages when there is something to verify
    // (a window: the DEFAULT is that every printed entry's own timestamp
    // is inside it) or when the framing needs entries (-0, annotation), or
    // a --max cap (counting entries needs entry parsing).
    // --by-write-time is the raw escape hatch: chunk dump, no parsing.
    // An entry predicate needs entry parsing, exactly as --max does: the
    // raw path emits chunk bytes and has nothing to judge.
    if !by_write_time
        && (windowed
            || null_sep
            || show_write_time
            || records
            || max.is_some()
            || entry_preds.is_some())
    {
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        return query_entries(
            &mut out,
            files,
            from_ms,
            to_ms,
            from_chunk,
            windowed,
            has,
            any,
            entry_preds,
            max_chunks,
            cursor,
            no_filename,
            show_write_time,
            null_sep,
            records,
            max,
            q.limit.imposed,
            &budget,
        );
    }
    if chunk_records {
        return query_chunks_framed(
            files,
            from_ms,
            to_ms,
            from_chunk,
            has,
            any,
            max_chunks,
            q.limit.imposed,
            &budget,
        );
    }
    if files.len() == 1 {
        return query_single(
            &files[0], from_ms, to_ms, from_chunk, has, any, max_chunks, &budget,
        );
    }
    query_multi(
        files,
        from_ms,
        to_ms,
        from_chunk,
        has,
        any,
        no_filename,
        max_chunks,
        &budget,
    )
}

/// The default read path: select chunks by the write-time rings (widened
/// when the logline filter can verify), then emit whole ENTRIES whose own
/// timestamps fall inside the asked window. Unfilterable stores (no
/// parseable line timestamps) fall back to the unwidened raw window with
/// a note — never both looser AND unexplained.
/// Does this live segment begin exactly where the store's chunks end?
///
/// Only then can its entries be appended to what the chunks gave. A flush
/// landing between the ring snapshot and the sap read leaves the two
/// unrelated: the bytes in between are in a chunk this answer never saw,
/// and delivering the segment anyway would move the reported position
/// PAST them — a gap the consumer cannot know it has. Being one poll late
/// is the cheap failure; the chunk carries those bytes next time.
fn live_follows_the_chunks(at: u64, dropped: u64, chunks: &[ChunkRecord]) -> bool {
    at == dropped + chunks.last().map_or(0, |c| c.uncomp_end())
}

/// The live edge, after what the chunks gave: the newest entries, which
/// are durable and readable but in no chunk yet. For a read that is
/// RESUMING — a position and no window — because a consumer following the
/// store otherwise waits out the writer's flush age to see them.
///
/// Each entry is placed like a chunk would be: its own address, and the
/// same slicing where the position lands inside one.
fn emit_live_edge(
    live: &mut crate::live::LiveTail,
    dropped: u64,
    chunks: &[ChunkRecord],
    resume_at: Option<u64>,
    sink: &mut crate::entry::EntrySink,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let (mut at, entries) = live.poll()?;
    if entries.is_empty() || !live_follows_the_chunks(at, dropped, chunks) {
        return Ok(());
    }
    for e in entries {
        let end = at + e.payload.len() as u64;
        match resume_at {
            Some(r) if r >= end => {}
            Some(r) if r > at => {
                sink.push_chunk(&e.payload[(r - at) as usize..], None, (e.wf, e.wl), r, out)?
            }
            _ => sink.push_chunk(&e.payload, None, (e.wf, e.wl), at, out)?,
        }
        at = end;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn query_entries<W: Write>(
    mut out: &mut W,
    files: &[std::path::PathBuf],
    from_ms: u64,
    to_ms: u64,
    from_chunk: Option<u64>,
    windowed: bool,
    has: &[String],
    any: &[String],
    entry_preds: Option<crate::grep::Preds>,
    max_chunks: Option<u64>,
    cursor: &std::collections::BTreeMap<String, u64>,
    no_filename: bool,
    show_write_time: bool,
    null_sep: bool,
    records: bool,
    max: Option<u64>,
    imposed: Imposed,
    budget: &Budget,
) -> anyhow::Result<()> {
    struct Src {
        path: std::path::PathBuf,
        guard: Option<(PathBuf, String)>,
        handle: SourceHandle,
        chunks: Vec<(usize, ChunkRecord)>,
        total_chunks: usize,
        pos: usize,
        sink: crate::entry::EntrySink,
        /// Resume just past here, when the caller handed back a position.
        resume_at: Option<u64>,
        /// What has left this store over its life. Added to a chunk's own
        /// offset it gives a position on the tape that retention cannot
        /// move — `remove_head` rebases the one and grows the other by
        /// exactly as much.
        dropped_bytes: u64,
        /// Its sink was already closed, on reaching the end of its chunks.
        finished: bool,
        /// The store's live write-ahead segment, for a read that resumes.
        /// `none()` otherwise: a windowed read answers about the past, and
        /// the edge is not in it.
        live: crate::live::LiveTail,
    }
    let multi = files.len() > 1 && !no_filename;
    // A predicate the token index answers, rather than one judged on the
    // entry: the difference decides whether the live edge can be part of
    // the answer (see the `live` field below).
    let sweeping = (!has.is_empty() || !any.is_empty()) && entry_preds.is_none();
    // --max: a total entry cap shared by every source's sink.
    let limit = max.map(|m| (Rc::new(Cell::new(0u64)), m));
    // WHICH bound stopped the read. A consumer needs the name, not just
    // the fact: "your entry cap" and "your chunk cap" are different
    // things to raise, and the answer used to say `max.entries` whatever
    // had actually fired.
    let mut stopped_by: Option<&'static str> = None;
    let mut srcs: Vec<Src> = Vec::new();
    for f in files {
        if budget.expired() {
            stopped_by = Some("deadline");
            break;
        }
        let mut source = open_source(f)?;
        let guard = seq_guard(f);
        let guard_for_live = guard.clone();
        let tf = crate::bark::time_format(source.bark.as_ref());
        let extractor =
            crate::import::Extractor::new(tf.regex.as_deref(), tf.format.as_deref(), tf.utc)?;
        // Widened selection, then a probe: can this store's lines be
        // parsed at all? If not, no filter — and no widening either.
        let (selected, _) = select_chunks(
            f,
            &source.records,
            source.seq_at_open,
            from_ms.saturating_sub(WIDEN_MS),
            to_ms.saturating_add(WIDEN_MS),
            from_chunk,
            has,
            any,
        )?;
        let filterable = windowed
            && match selected.first() {
                Some((_, c)) => match read_chunk(f, &guard, &mut source, *c)? {
                    Some(data) => crate::entry::probe_stamps(&extractor, &data),
                    // Retained away by a race between selection and probe:
                    // default to unfilterable (never both looser and
                    // silent — the note below explains).
                    None => false,
                },
                None => false,
            };
        let window = if filterable {
            Some((from_ms, to_ms))
        } else {
            if windowed && !selected.is_empty() {
                crate::note!(
                    "timberfs: {}: no parseable line timestamps — showing the write-time \
                     window as-is (declare a format with `timberfs set` to filter exactly)",
                    f.display()
                );
            }
            None
        };
        // Unfilterable + windowed: fall back to the UNWIDENED selection —
        // never both looser and unexplained.
        let selected = if window.is_none() && windowed {
            select_chunks(
                f,
                &source.records,
                source.seq_at_open,
                from_ms,
                to_ms,
                from_chunk,
                has,
                any,
            )?
            .0
        } else {
            selected
        };
        // Where a previous answer left this store, if the caller handed
        // one back. Keyed by IDENTITY: a path would not survive a move,
        // and the answer that produced it named the store that way.
        let dropped = dropped_bytes_of(f);
        let resume_at = source
            .bark
            .as_ref()
            .and_then(|b| b.get("id"))
            .and_then(|v| v.as_str())
            .and_then(|id| cursor.get(id))
            .copied();
        let framing = crate::entry::Framing {
            null_sep,
            show_write: show_write_time,
            records,
            label: if multi {
                Some(f.display().to_string().into_bytes())
            } else {
                None
            },
            // Only where an entry is attributed at all, and only in a
            // record stream: the path is the human-readable half and this
            // is the durable one.
            store_id: (multi && records).then(|| store_id_of(&source)).flatten(),
        };
        // A position past a chunk's END makes it dead weight, and the
        // index is ordered by where a chunk sits — so the first one that
        // survives is found rather than walked to. A following read
        // carries no window, so its selection is the whole store and
        // this is the only thing narrowing it: on a store of 100k chunks
        // that is a search instead of 100k comparisons, every poll.
        let start = resume_at.map_or(0, |at| {
            selected.partition_point(|(_, c)| dropped + c.uncomp_end() <= at)
        });
        srcs.push(Src {
            path: f.clone(),
            guard,
            total_chunks: source.records.len(),
            handle: source,
            chunks: selected,
            pos: start,
            sink: crate::entry::EntrySink::new(
                extractor,
                window,
                framing,
                limit.clone(),
                &f.display().to_string(),
            )
            .with_preds(entry_preds.clone()),
            // Read once per source: what has left this store over its
            // life, which anchors every offset to the tape rather than to
            // the file as it stands.
            dropped_bytes: dropped,
            resume_at,
            finished: false,
            // A POSITION and no window is a consumer following this
            // store; anything else is a question about the past.
            //
            // Not under a CHUNK-granular predicate, though: that answer is
            // the chunks the token index says may contain the terms, and
            // the index cannot speak for a segment it has not covered.
            // Emitting it whole would answer a different question; one
            // flush later the chunk is indexed and it is answered.
            live: match (&guard_for_live, windowed, resume_at, sweeping) {
                (Some((dir, name)), false, Some(_), false) => {
                    crate::live::LiveTail::open(dir, name, false)
                }
                _ => crate::live::LiveTail::none(),
            },
        });
    }

    // --records brackets the stream with typed metadata: stream-start
    // carries the format version and an echo of the selection (canonical
    // ms values — downstream tools can record lineage), one source record
    // per input carries its selection stats, and stream-end (below)
    // carries totals — its PRESENCE is the completeness marker: a
    // consumer hitting EOF without it knows the stream was truncated.
    if records {
        write!(
            out,
            "\x1estream-start\x1fv=1\x1fserver_version={}{}",
            crate::querydoc::server_version(),
            imposed.declared.record_fields()
        )?;
        if from_ms > 0 {
            write!(out, "\x1ffrom={from_ms}")?;
        }
        if to_ms < u64::MAX {
            write!(out, "\x1fto={to_ms}")?;
        }
        // Where the read STARTED, when a place rather than a time said so.
        // The echo is the lineage: an answer outlives the request that
        // produced it, and one recording `from`/`to`/`has` while dropping
        // the position it began at describes a search nobody ran.
        if let Some(n) = from_chunk {
            write!(out, "\x1ffrom_chunk={n}")?;
        }
        for h in has {
            write!(out, "\x1fhas={h}")?;
        }
        for a in any {
            write!(out, "\x1fany={a}")?;
        }
        // WHAT those predicates selected. A consumer that cannot tell a
        // chunk sweep from an entry search cannot tell a superset from an
        // answer — which is the defect this field exists to close.
        if !has.is_empty() || !any.is_empty() {
            write!(
                out,
                "\x1fgranularity={}",
                if entry_preds.is_some() {
                    "entries"
                } else {
                    "chunks"
                }
            )?;
        }
        // Entries of one store come in that store's order; between stores
        // there is none. Said rather than implied: a consumer that reads a
        // multi-store answer as a timeline gets a wrong one in silence.
        write!(out, "\x1forder=sequential")?;
        write!(out, "\x1fsources={}", files.len())?;
        out.write_all(b"\0")?;
        for (f, s) in files.iter().zip(&srcs) {
            write!(out, "\x1esource\x1fpath={}", f.display())?;
            // The store's IDENTITY, which is what the request selected on
            // and what a position is recorded against. A path names a
            // store only within one response; everything durable — a
            // follower's declaration, a cursor, `list --json` — uses this.
            if let Some(id) = s
                .handle
                .bark
                .as_ref()
                .and_then(|b| b.get("id"))
                .and_then(|v| v.as_str())
            {
                write!(out, "\x1fid={id}")?;
            }
            write!(
                out,
                "\x1fkept={}\x1ftotal={}",
                s.chunks.len(),
                s.total_chunks
            )?;
            out.write_all(b"\0")?;
        }
    }
    // Sources in turn, each drained before the next. Order is claimed
    // WITHIN a store, never between: an interleave could only key on
    // arrival time, and that order does not survive the next page, which
    // resolves the store predicate again and may add a store carrying
    // older entries. `query_multi` interleaves for the human fleet view,
    // which has no next page.
    let mut chunks_out = 0u64;
    while let Some(i) = (0..srcs.len()).find(|&i| srcs[i].pos < srcs[i].chunks.len()) {
        if budget.expired() {
            stopped_by = Some(Imposed::name(imposed.deadline, "deadline"));
            break;
        }
        let s = &mut srcs[i];
        let c = s.chunks[s.pos].1;
        s.pos += 1;
        // Resume: a chunk wholly before the position holds nothing new,
        // and the one the position lands in is entered part-way. Byte
        // exact, where a timestamp cannot tell two entries of the same
        // second apart — which is why a position is an offset at all.
        //
        // The index carries the span, so a chunk the position is past is
        // skipped without decompressing it — what a poll resuming from a
        // cursor rests on.
        let chunk_base = s.dropped_bytes + c.uncomp_start;
        if s.resume_at
            .is_some_and(|at| at >= chunk_base + c.uncomp_len)
        {
            continue;
        }
        let Some(data) = read_chunk(&s.path, &s.guard, &mut s.handle, c)? else {
            continue; // retained away by a race between selection and read
        };
        let (data, base) = match s.resume_at {
            Some(at) if at >= chunk_base + data.len() as u64 => continue,
            Some(at) if at > chunk_base => (&data[(at - chunk_base) as usize..], at),
            _ => (&data[..], chunk_base),
        };
        s.sink.push_chunk(
            data,
            Some(c.seq),
            (c.first_write_ms, c.last_write_ms),
            // The chunk's place on the store's endless tape: what has
            // already left the store, plus where this chunk sits in what
            // remains. Retention rebases the second and the first
            // compensates it exactly, so the sum never moves.
            base,
            &mut out,
        )?;
        // --max reached: stop decompressing further chunks.
        if let Some((count, m)) = &limit {
            if count.get() >= *m {
                stopped_by = Some(Imposed::name(imposed.max, "max.entries"));
                break;
            }
        }
        // A chunk cap counts what was EMITTED, across sources, so a
        // fleet view of three stores capped at 5 chunks reads five, not
        // fifteen.
        chunks_out += 1;
        if max_chunks.is_some_and(|m| chunks_out >= m) {
            stopped_by = Some(Imposed::name(imposed.max_chunks, "max.chunks"));
            break;
        }
        // Read to its end: release the entry this store was holding open
        // NOW, before the next store's first. A sink keeps the last entry
        // pending until a following stamped line closes it, so leaving it
        // to the accounting pass puts every store's last entry after every
        // other store's body — the answer interleaved after all.
        let s = &mut srcs[i];
        if s.pos == s.chunks.len() {
            emit_live_edge(
                &mut s.live,
                s.dropped_bytes,
                &s.handle.records,
                s.resume_at,
                &mut s.sink,
                &mut out,
            )?;
            s.sink.finish(&mut out)?;
            s.finished = true;
        }
    }
    let (mut emitted, mut dropped) = (0u64, 0u64);
    let (mut read, mut total) = (0usize, 0usize);
    // Did a cap stop this short of the answer? Two exact signals, and
    // neither fires when the data merely happened to equal the cap:
    // an entry the sink DROPPED because the count was already there, and
    // chunks left unread because the loop broke on the cap rather than
    // running out. A count alone cannot tell "that was all" from "your
    // limit stopped me", and a consumer that cannot tell will present a
    // truncated answer as a complete one.
    let mut limited = stopped_by.is_some() || srcs.iter().any(|s| s.pos < s.chunks.len());
    for s in &mut srcs {
        // A source with chunks left had its open entry CUT OFF rather
        // than ended: the rest of it is in a chunk the bound stopped us
        // reading. Emitting it would invent an entry — a stack trace with
        // half its frames, presented as whole — so it is dropped, and a
        // caller resumes from its first byte.
        //
        // Only where chunks remain: a pending entry at genuine
        // end-of-data is merely unterminated, and must still be emitted.
        if !s.finished {
            if s.pos < s.chunks.len() {
                s.sink.discard_pending();
            } else {
                // The loop never ran for this one — a position at the
                // tape's end leaves no chunk to walk, which is the steady
                // state of a consumer that has caught up.
                emit_live_edge(
                    &mut s.live,
                    s.dropped_bytes,
                    &s.handle.records,
                    s.resume_at,
                    &mut s.sink,
                    &mut out,
                )?;
            }
            s.sink.finish(&mut out)?;
        }
        emitted += s.sink.emitted;
        dropped += s.sink.filtered_out;
        // An entry the sink DROPPED because the count was already there.
        // The loop stops feeding chunks on the cap, but a chunk already
        // in flight can still hold entries past it — so this fires where
        // the loop's own check does not.
        if s.sink.suppressed > 0 {
            limited = true;
            stopped_by = stopped_by.or(Some(Imposed::name(imposed.max, "max.entries")));
        }
        // What was actually READ, which is how far each source advanced —
        // not how many chunks were selected for it. They differ exactly
        // when a cap stopped the loop, which is when a consumer is most
        // likely to be counting.
        read += s.pos;
        total += s.total_chunks;
    }
    if records {
        // WHERE EACH STORE GOT TO, one record per source, before the
        // stream ends. An absolute offset on that store's tape: with its
        // id it addresses a byte of a log that has ever existed, and it
        // is the position a caller resumes from.
        //
        // Emitted for every store EXAMINED, including ones that matched
        // nothing — otherwise the next page re-scans them from the start
        // of the window, which on a fleet is most of the cost.
        for (f, s) in files.iter().zip(&srcs) {
            write!(out, "\x1eposition\x1fpath={}", f.display())?;
            if let Some(id) = s
                .handle
                .bark
                .as_ref()
                .and_then(|b| b.get("id"))
                .and_then(|v| v.as_str())
            {
                write!(out, "\x1fid={id}")?;
            }
            // Just past the last entry DELIVERED — or, where this store
            // delivered none, the position it was RESUMED from.
            //
            // The fallback is what makes the round trip lossless. A caller
            // hands the whole answer back as the next `cursor`, so a store
            // that went quiet on one page would otherwise come back with
            // no offset, and an offsetless entry means the start of the
            // window: every quiet store re-read from the beginning, every
            // entry in it delivered twice. Nothing moved, so the position
            // did not either.
            //
            // Absent only where there is no position at all — nothing
            // delivered and nothing resumed from — which is a store this
            // search has not consumed any of.
            if let Some(at) = s.sink.emitted_end.or(s.resume_at) {
                write!(out, "\x1foffset={at}")?;
            }
            write!(
                out,
                "\x1fchunks_read={}\x1fchunks_selected={}",
                s.pos,
                s.chunks.len()
            )?;
            out.write_all(b"\0")?;
        }
        // `status` and `limit` are new fields in v=1, which that format
        // permits: consumers ignore keys they do not know.
        let status = if limited { "limited" } else { "exhausted" };
        write!(
            out,
            "\x1estream-end\x1fentries={emitted}\x1fdropped={dropped}\x1fchunks_read={read}\x1fchunks_total={total}\x1fstatus={status}"
        )?;
        // Named, not assumed. `limited` can also be true with nothing to
        // name — chunks left unread for a reason the loop did not record
        // — and then saying nothing beats naming the wrong bound.
        if let Some(which) = stopped_by {
            write!(out, "\x1flimit={which}")?;
        }
        out.write_all(b"\0")?;
    }
    out.flush()?;
    // The same thing `status=limited` tells a program, told to a person:
    // a count alone reads as the whole answer, and this one is not.
    if limited {
        if let Some(m) = max {
            if imposed.max {
                crate::note!(
                    "timberfs: stopped at this machine's ceiling of {m} entries; ask again \
                     from the position records for the next page"
                );
            } else {
                crate::note!(
                    "timberfs: stopped at --max {m}; more entries matched than were shown"
                );
            }
        }
    }
    if windowed {
        crate::note!(
            "timberfs: {emitted} entr{} in the window; {read} of {total} chunk(s) read{}",
            if emitted == 1 { "y" } else { "ies" },
            if dropped > 0 {
                format!(
                    " ({dropped} nearby verified outside it — --show-write-time explains, \
                     --by-write-time shows raw chunks)"
                )
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

/// Count units in a chunk: entries (a stamped line starts one) normally, or
/// lines when the store has no parseable timestamps.
fn count_units(data: &[u8], extractor: &crate::import::Extractor, by_line: bool) -> u64 {
    let mut n = 0u64;
    for line in data.split_inclusive(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if by_line {
            n += 1;
        } else {
            let head = String::from_utf8_lossy(&line[..line.len().min(256)]);
            if extractor.extract(&head).is_some() {
                n += 1;
            }
        }
    }
    n
}

/// The first chunk index such that chunks[start..] hold at least `n` units,
/// walking back from the end. Chunk-granular: the start chunk is included
/// whole, so a few extra may precede the Nth-from-last. Exact-N would need a
/// per-entry offset/length index (a future ".grain"-like log-entry index);
/// until then the overshoot is bounded by one chunk (--flush-age or 256K).
fn tail_start(
    input: &Path,
    guard: &Option<(PathBuf, String)>,
    handle: &mut SourceHandle,
    chunks: &[ChunkRecord],
    extractor: &crate::import::Extractor,
    by_line: bool,
    n: u64,
) -> anyhow::Result<usize> {
    if chunks.is_empty() || n == 0 {
        return Ok(chunks.len());
    }
    let mut count = 0u64;
    let mut start = chunks.len();
    for i in (0..chunks.len()).rev() {
        if let Some(data) = read_chunk(input, guard, handle, chunks[i])? {
            count += count_units(&data, extractor, by_line);
        }
        start = i;
        if count >= n {
            break;
        }
    }
    Ok(start)
}

/// Where a follower left off: the LAST CHUNK IT HAS SEEN, identified
/// the way `read_chunk` re-locates one after a collapse — by its
/// write window and its lengths, which a head trim does not change
/// (offsets and logical positions, which it rebases, would). NOT a
/// timestamp: chunk windows come from the entries' own loglines on an
/// imported or followed store, so two chunks routinely share a
/// boundary millisecond, and "later than the last one" then skips
/// every second chunk for good.
#[derive(PartialEq, Eq, Clone, Copy)]
struct ChunkKey {
    uncomp_len: u64,
    comp_len: u64,
    first_write_ms: u64,
    last_write_ms: u64,
}

fn key(c: &ChunkRecord) -> ChunkKey {
    ChunkKey {
        uncomp_len: c.uncomp_len,
        comp_len: c.comp_len,
        first_write_ms: c.first_write_ms,
        last_write_ms: c.last_write_ms,
    }
}

/// Index of the first chunk this follower has NOT seen. The hint is
/// the position the anchor had last time — right whenever retention
/// did not trim in between, which is nearly always — and the scan
/// below it covers the case where it did. An anchor that is gone
/// entirely means retention overtook the follower.
fn resume_at(records: &[ChunkRecord], anchor: &Option<(ChunkKey, usize)>) -> Option<usize> {
    let Some((k, hint)) = anchor else {
        return Some(0); // nothing seen yet: everything is new
    };
    if records.get(*hint).map(key) == Some(*k) {
        return Some(hint + 1);
    }
    let from = (*hint).min(records.len().saturating_sub(1));
    (0..=from)
        .rev()
        .find(|&i| key(&records[i]) == *k)
        .map(|i| i + 1)
}

/// Follow / tail: emit (about) the last N units, then — with --follow — new
/// data as it arrives, until interrupted. Read-only and lock-free, so it runs
/// beside a live appender. Two sources feed it: flushed chunks from the ring,
/// and — when the store declares a wal — the live edge of the sap (live.rs),
/// which is what makes an entry visible before its chunk exists. A chunk
/// repeats what its segment already served, so each is emitted minus that
/// prefix.
///
/// Plain text follows RAW chunk bytes (line-oriented, no buffering — the
/// snappy tail -f). Only the framed modes (-0, --records, --show-write-time)
/// run the entry pipeline, where the last entry stays buffered until the next
/// one closes it (a multiline entry can't be known complete any sooner).
#[allow(clippy::too_many_arguments)]
fn query_follow(
    files: &[std::path::PathBuf],
    from: Option<u64>,
    from_chunk: Option<u64>,
    no_filename: bool,
    show_write_time: bool,
    null_sep: bool,
    records: bool,
    tail: Option<u64>,
    tail_chunks: Option<u64>,
    follow: bool,
    max: Option<u64>,
    poll: Option<f64>,
    entry_preds: Option<crate::grep::Preds>,
    imposed: Imposed,
) -> anyhow::Result<()> {
    let multi = files.len() > 1 && !no_filename;
    // Framing needs entries; plain text streams raw bytes (no one-entry lag).
    // --max caps entries; raw bytes have no entry count, so a cap forces the
    // entry pipeline (framed) just as it does in the one-shot path.
    let framed = records || null_sep || show_write_time || max.is_some();
    // --max: a total entry cap shared across sources; also a stop signal for
    // the follow loop (bounded follow).
    let limit = max.map(|m| (Rc::new(Cell::new(0u64)), m));
    let capped = |limit: &Option<crate::entry::EntryLimit>| {
        limit.as_ref().is_some_and(|(c, m)| c.get() >= *m)
    };

    // Raw emit: chunk bytes as-is, or a per-line "path:" prefix across files.
    fn emit_raw(out: &mut dyn Write, data: &[u8], label: Option<&[u8]>) -> io::Result<()> {
        match label {
            None => out.write_all(data),
            Some(lbl) => {
                for line in data.split_inclusive(|&b| b == b'\n') {
                    out.write_all(lbl)?;
                    out.write_all(b":")?;
                    out.write_all(line)?;
                }
                Ok(())
            }
        }
    }

    /// How long to wait before looking again. A store with a live
    /// segment is tailed several times a second: its new entries are
    /// already on disk, so this sleep is all that stands between a
    /// written line and a shown one. Without one, the writer's
    /// --flush-age decides that instead and a faster poll would only
    /// spend syscalls.
    fn poll_interval(srcs: &[FollowSrc], requested: Option<f64>) -> std::time::Duration {
        if let Some(secs) = requested {
            return std::time::Duration::from_millis((secs * 1000.0).max(10.0) as u64);
        }
        if srcs.iter().any(|s| s.live.live()) {
            std::time::Duration::from_millis(200)
        } else {
            std::time::Duration::from_millis(1000)
        }
    }

    /// Entries read from the live segment. Each carries its own write
    /// window, so the framed modes get finer stamps here than a chunk can
    /// give them.
    fn emit_live(
        out: &mut dyn Write,
        entries: &[crate::sap::SapEntry],
        sink: &mut Option<crate::entry::EntrySink>,
        label: Option<&[u8]>,
        base: u64,
    ) -> anyhow::Result<()> {
        let mut at = base;
        for e in entries {
            match sink {
                // In no chunk yet — but on the same tape, at `at`. A
                // segment's bytes are the next chunk's bytes, so this
                // address is the one that chunk will report for them.
                Some(s) => s.push_chunk(&e.payload, None, (e.wf, e.wl), at, out)?,
                None => emit_raw(out, &e.payload, label)?,
            }
            at += e.payload.len() as u64;
        }
        Ok(())
    }

    struct FollowSrc {
        path: std::path::PathBuf,
        label: Option<Vec<u8>>,
        sink: Option<crate::entry::EntrySink>,
        // The last chunk seen, and where it sat in the ring last time.
        anchor: Option<(ChunkKey, usize)>,
        // The store's live write-ahead segment, when it declares one:
        // entries become visible here as they are appended, instead of a
        // flushed chunk at a time.
        live: crate::live::LiveTail,
        // Announced once, not once a second.
        noted_overtaken: bool,
        // When this source last had something to show, and whether the
        // pending entry has already been closed for this quiet streak.
        last_data: std::time::Instant,
        flushed_idle: bool,
    }

    // An entry is only closed by the NEXT stamped line, so the newest
    // entry of a store that falls quiet would otherwise never be emitted
    // — exactly the entry an incident cares about. After this long
    // without new data the pending entry is closed. A wall-clock
    // duration, not a poll count: --poll (and the live tail's faster
    // default) must not change when a producer is judged to have stopped
    // mid-entry.
    const IDLE_FLUSH: std::time::Duration = std::time::Duration::from_secs(10);

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    if records {
        // A followed stream is unbounded: stream-start, then entries, and
        // deliberately no stream-end — its absence is the honest "still live
        // (or truncated)" marker. A bounded --tail (no --follow) does close.
        // WHAT produced this. An answer outlives the connection that
        // fetched it — relayed, piped, kept — and a consumer that finds
        // one behaving oddly should not have to infer the version from
        // the behaviour. Product and version, because the thing answering
        // need not be a timberfs.
        write!(
            out,
            "\x1estream-start\x1fv=1\x1fserver_version={}{}",
            crate::querydoc::server_version(),
            imposed.declared.record_fields()
        )?;
        if let Some(fr) = from {
            write!(out, "\x1ffrom={fr}")?;
        }
        if let Some(n) = tail {
            write!(out, "\x1ftail={n}")?;
        }
        write!(
            out,
            // A follow polls each source in turn and emits what it has, so
            // entries come in the order they reached the reader. A bounded
            // --tail has no such loop: each source's tail goes out whole,
            // before the next.
            "\x1ffollow={}\x1forder={}\x1fsources={}",
            u8::from(follow),
            if follow { "arrival" } else { "sequential" },
            files.len()
        )?;
        out.write_all(b"\0")?;
    }

    let mut srcs: Vec<FollowSrc> = Vec::new();
    for f in files {
        // Before the ring is read, so that a flush landing between the two
        // cannot leave a sealed segment this reader has already taken
        // entries from. From-now (no --from/--tail) passes over what the
        // segment already holds; --tail/--from want it — unflushed entries
        // are the newest ones there are.
        let backing = seq_guard(f);
        let mut live = match &backing {
            Some((dir, name)) => {
                crate::live::LiveTail::open(dir, name, from.is_none() && tail.is_none())
            }
            // A .timber bundle is a finished artifact: no writer, no tail.
            None => crate::live::LiveTail::none(),
        };
        if follow && !live.live() && backing.is_some() {
            // Not a warning — a store without one is a legitimate
            // configuration. But an operator waiting on a live log should
            // know which knob decides how long they wait, and that the
            // faster one costs no restart.
            crate::note!(
                "timberfs: {}: no live write-ahead sidecar, so new entries appear a \
                 flushed chunk at a time (the writer's --flush-age). `timberfs set {} \
                 wal=true` starts one within a second, without restarting the writer",
                f.display(),
                f.display()
            );
        }
        let mut source = open_source(f)?;
        let guard = backing;
        let tf = crate::bark::time_format(source.bark.as_ref());
        let extractor =
            crate::import::Extractor::new(tf.regex.as_deref(), tf.format.as_deref(), tf.utc)?;
        // Owned, not borrowed from `source`: read_chunk needs `&mut
        // source` on a race, which a live borrow of source.records
        // would forbid (chunk records are Copy, so cloning is cheap).
        let chunks = source.records.clone();
        // Where to begin: an entry-count tail, a write-time --from, or (the
        // default) the current end — following only genuinely new chunks.
        let start = if let Some(n) = tail_chunks {
            // A chunk tail costs nothing: the index knows how many there
            // are, so the last N is arithmetic and no chunk is read to
            // find the boundary. That cheapness IS the reason the unit
            // exists.
            chunks.len().saturating_sub(n as usize)
        } else if let Some(n) = tail {
            // --tail N counts log ENTRIES (a stamped line and its continuation
            // lines) the same way in text and framed output, falling back to
            // lines only when the store has no parseable timestamps. Probe the
            // first few chunks, not the last: a chunk can split mid-entry, so
            // the final one is often a bare continuation with no stamp.
            let mut parseable = false;
            for c in chunks.iter().take(4) {
                if let Some(data) = read_chunk(f, &guard, &mut source, *c)? {
                    if crate::entry::probe_stamps(&extractor, &data) {
                        parseable = true;
                        break;
                    }
                }
            }
            let by_line = !parseable;
            tail_start(f, &guard, &mut source, &chunks, &extractor, by_line, n)?
        } else if let Some(seq) = from_chunk {
            // Exact: a chunk number identifies one chunk, so there is no
            // window to widen and no ambiguity between two chunks that share
            // a boundary millisecond. A number below everything the store
            // still holds starts at the oldest survivor — the caller's
            // position was dropped, which only it can report.
            chunks
                .iter()
                .position(|c| c.seq >= seq)
                .unwrap_or(chunks.len())
        } else if let Some(fr) = from {
            chunks
                .iter()
                .position(|c| c.last_write_ms >= fr)
                .unwrap_or(chunks.len())
        } else {
            chunks.len()
        };
        let label = if multi {
            Some(f.display().to_string().into_bytes())
        } else {
            None
        };
        let mut sink = if framed {
            Some(
                crate::entry::EntrySink::new(
                    extractor,
                    None,
                    crate::entry::Framing {
                        null_sep,
                        show_write: show_write_time,
                        records,
                        label: label.clone(),
                        store_id: (label.is_some() && records)
                            .then(|| store_id_of(&source))
                            .flatten(),
                    },
                    limit.clone(),
                    &f.display().to_string(),
                )
                .with_preds(entry_preds.clone()),
            )
        } else {
            None
        };
        // ONE reading per pass, for the chunks and the live edge alike: a
        // head trim moves it and every offset under it in opposite
        // directions, so two readings can put the two halves of one
        // answer on different tapes.
        let dropped = dropped_bytes_of(f);
        for c in &chunks[start..] {
            if let Some(data) = read_chunk(f, &guard, &mut source, *c)? {
                match &mut sink {
                    Some(s) => s.push_chunk(
                        &data,
                        Some(c.seq),
                        (c.first_write_ms, c.last_write_ms),
                        dropped + c.uncomp_start,
                        &mut out,
                    )?,
                    None => emit_raw(&mut out, &data, label.as_deref())?,
                }
            }
            if capped(&limit) {
                break;
            }
        }
        // Then the live edge: entries the writer has appended but not yet
        // flushed into a chunk. Empty for a from-now follow (opening the
        // tail passed over them) — and this is the only place a BOUNDED
        // --tail can see them, since it never enters the poll loop.
        if !capped(&limit) {
            // Its address on the tape: the same `dropped` the chunks
            // above were addressed with, plus where the segment says its
            // own bytes sit.
            let (at, entries) = live.poll()?;
            let base = dropped + at;
            emit_live(&mut out, &entries, &mut sink, label.as_deref(), base)?;
        }
        // Anchor to the latest committed chunk even when nothing was emitted
        // (from-now), so only new chunks are followed.
        let anchor = chunks.last().map(|c| (key(c), chunks.len() - 1));
        // The live tail's first read happens BEFORE the entries above are
        // emitted (`live` was opened first), so a flush landing in between
        // seals a segment this reader has taken nothing from: its chunk is
        // emitted in ring order rather than after the entries that follow
        // it.
        srcs.push(FollowSrc {
            path: f.clone(),
            label,
            sink,
            anchor,
            live,
            noted_overtaken: false,
            last_data: std::time::Instant::now(),
            flushed_idle: false,
        });
    }
    out.flush()?;

    // Nothing to follow (a bounded --tail) or --max already reached during
    // backfill: skip straight to finalizing.
    let mut done = !follow || capped(&limit);

    // Poll for newly-committed chunks. Re-open each pass: the ring only grows
    // (the appender appends), and re-opening picks up a fresh trunk fd too.
    // Flush every pass so an interrupt never drops already-emitted output.
    while !done {
        std::thread::sleep(poll_interval(&srcs, poll));
        for s in &mut srcs {
            let mut source = match open_source(&s.path) {
                Ok(x) => x,
                Err(_) => continue, // transient (mid-rename by retention): retry
            };
            let guard = seq_guard(&s.path);
            // As in the backfill above: one reading, both halves.
            let dropped = dropped_bytes_of(&s.path);
            // BEFORE the chunks below are emitted: a flush that landed
            // since this ring snapshot was taken sealed a segment whose
            // entries may already have gone out live, and its chunk
            // repeats them.
            s.live.reconcile()?;
            // Owned: read_chunk needs `&mut source` on a race, which a
            // live borrow of source.records would forbid.
            let at = resume_at(&source.records, &s.anchor);
            if at.is_some() {
                // Following normally again: a later loss is a new
                // incident and gets its own line.
                s.noted_overtaken = false;
            } else if !s.noted_overtaken {
                // The chunk this follower was anchored to is no longer in
                // the store: retention dropped it, and with it whatever
                // came after. Say so and rejoin at the live end — a
                // follower that silently resumed somewhere else would be
                // reporting a hole as continuity.
                crate::note!(
                    "timberfs: {}: retention overtook this follower; rejoining at the \
                     current end",
                    s.path.display()
                );
                s.noted_overtaken = true;
            }
            let pending: Vec<ChunkRecord> = match at {
                Some(i) => source.records[i..].to_vec(),
                None => Vec::new(),
            };
            if let Some(last) = source.records.last() {
                s.anchor = Some((key(last), source.records.len() - 1));
            }
            let mut got = !pending.is_empty();
            for c in pending {
                match read_chunk(&s.path, &guard, &mut source, c)? {
                    Some(data) => {
                        // Whatever of this chunk already went out live is
                        // dropped from its front; the rest is new.
                        let skip = s.live.skip_for_chunk(data.len() as u64) as usize;
                        let fresh = &data[skip..];
                        if !fresh.is_empty() {
                            match &mut s.sink {
                                Some(sink) => sink.push_chunk(
                                    fresh,
                                    Some(c.seq),
                                    (c.first_write_ms, c.last_write_ms),
                                    // `skip` bytes of this chunk were
                                    // already delivered on an earlier poll.
                                    dropped + c.uncomp_start + skip as u64,
                                    &mut out,
                                )?,
                                None => emit_raw(&mut out, fresh, s.label.as_deref())?,
                            }
                        }
                    }
                    // Retained away between selection and read: the skip
                    // it carried has nothing left to apply to.
                    None => s.live.forget_skip(),
                }
                if capped(&limit) {
                    done = true;
                    break;
                }
            }
            // The live edge last: it is always newer than any chunk.
            if !done {
                let (at, entries) = s.live.poll()?;
                // On the same tape the chunks above sit on. `dropped` is
                // re-read per pass because a head trim moves it and the
                // segment's own base by the same amount in opposite
                // directions: the sum holds only while both are of one
                // generation.
                let base = dropped + at;
                got |= !entries.is_empty();
                emit_live(&mut out, &entries, &mut s.sink, s.label.as_deref(), base)?;
                done = capped(&limit);
            }
            // Fires once per idle streak, and only after new data has
            // reset it — a quiet store is not re-flushed every second.
            // Timed from the last data, not from the first idle poll, so
            // the wait is IDLE_FLUSH however long a poll takes.
            if got {
                s.last_data = std::time::Instant::now();
                s.flushed_idle = false;
            } else if !s.flushed_idle && s.last_data.elapsed() >= IDLE_FLUSH {
                if let Some(sink) = &mut s.sink {
                    sink.flush_pending(&mut out)?;
                }
                s.flushed_idle = true;
            }
            if done {
                break;
            }
        }
        out.flush()?;
    }

    // Flush any framed sink's last buffered entry and close a record stream —
    // for a bounded --tail or a --max-capped follow. An unbounded follow never
    // reaches here (it ends at interrupt), so a live stream has no stream-end,
    // which is the honest "still going" marker.
    for s in &mut srcs {
        if let Some(sink) = &mut s.sink {
            sink.finish(&mut out)?;
        }
    }
    if records {
        write!(out, "\x1estream-end")?;
        out.write_all(b"\0")?;
    }
    out.flush()?;
    Ok(())
}

/// Chunks as `timberfs-records(5)`: the ring, then the zstd frame.
///
/// The text path beside this decompresses and concatenates, which loses
/// every boundary a consumer needs — where one chunk ended, which number
/// it was, what window it covered — and pays for a decompression nobody
/// asked for. This ships what is stored and says what each piece is.
#[allow(clippy::too_many_arguments)]
fn query_chunks_framed(
    files: &[std::path::PathBuf],
    from_ms: u64,
    to_ms: u64,
    from_chunk: Option<u64>,
    has: &[String],
    any: &[String],
    max_chunks: Option<u64>,
    imposed: Imposed,
    budget: &Budget,
) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    write_chunks_framed(
        &mut out, files, from_ms, to_ms, from_chunk, has, any, max_chunks, imposed, budget,
    )?;
    out.flush()?;
    Ok(())
}

/// The stream itself, over any writer, so a test can read what a consumer
/// would receive rather than a paraphrase of it.
#[allow(clippy::too_many_arguments)]
fn write_chunks_framed<W: Write>(
    out: &mut W,
    files: &[std::path::PathBuf],
    from_ms: u64,
    to_ms: u64,
    from_chunk: Option<u64>,
    has: &[String],
    any: &[String],
    max_chunks: Option<u64>,
    imposed: Imposed,
    budget: &Budget,
) -> anyhow::Result<()> {
    write!(
        out,
        "\x1estream-start\x1fv=1\x1fserver_version={}{}\x1forder=sequential\x1fsources={}",
        crate::querydoc::server_version(),
        imposed.declared.record_fields(),
        files.len()
    )?;
    out.write_all(b"\0")?;

    let (mut sent, mut total_chunks) = (0u64, 0usize);
    let mut stopped: Option<&'static str> = None;
    for f in files {
        let mut source = open_source(f)?;
        let guard = seq_guard(f);
        let (selected, _) = select_chunks(
            f,
            &source.records,
            source.seq_at_open,
            from_ms,
            to_ms,
            from_chunk,
            has,
            any,
        )?;
        // The manifest's id as text; `store_id_of` hands back bytes for the
        // entry sink, which writes them straight out.
        let id: Option<String> = source
            .bark
            .as_ref()
            .and_then(|b| b.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        total_chunks += source.records.len();
        write!(out, "\x1esource\x1fpath={}", f.display())?;
        if let Some(id) = &id {
            write!(out, "\x1fid={id}")?;
        }
        write!(
            out,
            "\x1fkept={}\x1ftotal={}",
            selected.len(),
            source.records.len()
        )?;
        out.write_all(b"\0")?;

        for (_, c) in &selected {
            if max_chunks.is_some_and(|m| sent >= m) {
                stopped = Some(Imposed::name(imposed.max_chunks, "max.chunks"));
                break;
            }
            if budget.expired() {
                stopped = Some(Imposed::name(imposed.deadline, "deadline"));
                break;
            }
            // The COMPRESSED frame. `read_chunk` decompresses; this is the
            // whole reason the raw reader exists.
            let Some(frame) = read_chunk_raw(f, &guard, &mut source, *c)? else {
                continue; // retained away between selection and read
            };
            write!(out, "\x1echunk\x1flen={}", frame.len())?;
            if let Some(id) = &id {
                write!(out, "\x1fid={id}")?;
            }
            if files.len() > 1 {
                write!(out, "\x1fsrc={}", f.display())?;
            }
            write!(
                out,
                "\x1fchunk={}\x1funcomp_start={}\x1funcomp_len={}\x1fwf={}\x1fwl={}",
                c.seq, c.uncomp_start, c.uncomp_len, c.first_write_ms, c.last_write_ms
            )?;
            out.write_all(b"\0")?;
            out.write_all(&frame)?;
            out.write_all(b"\0")?;
            sent += 1;
        }
        if stopped.is_some() {
            break;
        }
    }

    write!(
        out,
        "\x1estream-end\x1fchunks={sent}\x1fchunks_total={total_chunks}\x1fstatus={}",
        if stopped.is_some() {
            "limited"
        } else {
            "exhausted"
        }
    )?;
    if let Some(b) = stopped {
        write!(out, "\x1flimit={b}")?;
    }
    out.write_all(b"\0")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn query_single(
    file: &Path,
    from_ms: u64,
    to_ms: u64,
    from_chunk: Option<u64>,
    has: &[String],
    any: &[String],
    max_chunks: Option<u64>,
    budget: &Budget,
) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    write_single(
        &mut out, file, from_ms, to_ms, from_chunk, has, any, max_chunks, budget,
    )
}

/// The dump itself, over any writer — the same split `write_chunks_framed`
/// has, and for the same reason: a test reads what a consumer would
/// receive rather than a paraphrase of it.
#[allow(clippy::too_many_arguments)]
fn write_single<W: Write>(
    out: &mut W,
    file: &Path,
    from_ms: u64,
    to_ms: u64,
    from_chunk: Option<u64>,
    has: &[String],
    any: &[String],
    max_chunks: Option<u64>,
    budget: &Budget,
) -> anyhow::Result<()> {
    let mut source = open_source(file)?;
    let (selected, in_window) = select_chunks(
        file,
        &source.records,
        source.seq_at_open,
        from_ms,
        to_ms,
        from_chunk,
        has,
        any,
    )?;
    let total_chunks = source.records.len();
    let guard = seq_guard(file);

    let mut uncomp_total = 0u64;
    // A chunk cap needs no parsing at all here: stop after N have gone
    // out. This is the path where the unit is genuinely free.
    let selected: Vec<_> = match max_chunks {
        Some(n) => selected.iter().take(n as usize).copied().collect(),
        None => selected.clone(),
    };
    for (_, c) in &selected {
        // No stream-end here to carry a status, so the note IS the marker:
        // a dump that stops early with nothing saying so reads as one that
        // ended.
        if budget.expired() {
            crate::note!("timberfs: the deadline stopped this read; the answer is partial");
            break;
        }
        if let Some(data) = read_chunk(file, &guard, &mut source, *c)? {
            out.write_all(&data)?;
            uncomp_total += c.uncomp_len;
        }
    }
    out.flush()?;
    eprintln!(
        "timberfs: {} of {} chunk(s){}, {} bytes (chunk granularity; unflushed tail not included)",
        selected.len(),
        total_chunks,
        if has.is_empty() {
            String::new()
        } else {
            format!(" ({in_window} in window before --has)")
        },
        uncomp_total
    );
    Ok(())
}

/// Multiple sources: per-file selection, then a k-way merge interleaving
/// chunks across files by their time windows (within-file order is
/// preserved — it is the content order). Output lines carry a grep-style
/// "path:" prefix unless suppressed, with partial lines at chunk
/// boundaries carried per file so every output line gets exactly one
/// prefix. Attribution lives in the filename — this is the fleet view
/// over per-stream logs.
#[allow(clippy::too_many_arguments)]
fn query_multi(
    files: &[std::path::PathBuf],
    from_ms: u64,
    to_ms: u64,
    from_chunk: Option<u64>,
    has: &[String],
    any: &[String],
    no_filename: bool,
    max_chunks: Option<u64>,
    budget: &Budget,
) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    write_multi(
        &mut out,
        files,
        from_ms,
        to_ms,
        from_chunk,
        has,
        any,
        no_filename,
        max_chunks,
        budget,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_multi<W: Write>(
    out: &mut W,
    files: &[std::path::PathBuf],
    from_ms: u64,
    to_ms: u64,
    from_chunk: Option<u64>,
    has: &[String],
    any: &[String],
    no_filename: bool,
    max_chunks: Option<u64>,
    budget: &Budget,
) -> anyhow::Result<()> {
    struct Src {
        path: PathBuf,
        guard: Option<(PathBuf, String)>,
        label: Vec<u8>,
        handle: SourceHandle,
        chunks: Vec<ChunkRecord>,
        pos: usize,
        carry: Vec<u8>,
    }
    let mut srcs: Vec<Src> = Vec::new();
    let mut total_chunks = 0usize;
    let mut total_selected = 0usize;
    for f in files {
        let handle = open_source(f)?;
        let (selected, _) = select_chunks(
            f,
            &handle.records,
            handle.seq_at_open,
            from_ms,
            to_ms,
            from_chunk,
            has,
            any,
        )?;
        eprintln!(
            "timberfs: {}: {} of {} chunk(s)",
            f.display(),
            selected.len(),
            handle.records.len()
        );
        total_chunks += handle.records.len();
        total_selected += selected.len();
        srcs.push(Src {
            path: f.clone(),
            guard: seq_guard(f),
            label: f.display().to_string().into_bytes(),
            handle,
            chunks: selected.into_iter().map(|(_, c)| c).collect(),
            pos: 0,
            carry: Vec::new(),
        });
    }

    // Counted across sources: a fleet view capped at 5 chunks reads five
    // in time order, not five per store.
    let mut chunks_out = 0u64;
    loop {
        if max_chunks.is_some_and(|m| chunks_out >= m) {
            break;
        }
        // A raw dump has no stream-end to carry a status, so the note IS
        // the marker: a log that stops early with nothing saying so reads
        // as a log that ended.
        if budget.expired() {
            crate::note!("timberfs: the deadline stopped this read; the answer is partial");
            break;
        }
        let next = srcs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.pos < s.chunks.len())
            .min_by_key(|(_, s)| s.chunks[s.pos].first_write_ms)
            .map(|(i, _)| i);
        let Some(i) = next else { break };
        let s = &mut srcs[i];
        let c = s.chunks[s.pos];
        s.pos += 1;
        let Some(data) = read_chunk(&s.path, &s.guard, &mut s.handle, c)? else {
            continue; // retained away by a race between selection and read
        };
        chunks_out += 1;
        if no_filename {
            out.write_all(&data)?;
        } else {
            s.carry.extend_from_slice(&data);
            let complete = s.carry.iter().rposition(|&b| b == b'\n').map(|p| p + 1);
            if let Some(end) = complete {
                for line in s.carry[..end].split_inclusive(|&b| b == b'\n') {
                    out.write_all(&s.label)?;
                    out.write_all(b":")?;
                    out.write_all(line)?;
                }
                s.carry.drain(..end);
            }
        }
    }
    for s in &srcs {
        if !s.carry.is_empty() {
            out.write_all(&s.label)?;
            out.write_all(b":")?;
            out.write_all(&s.carry)?;
        }
    }
    out.flush()?;
    eprintln!(
        "timberfs: total {} of {} chunk(s) across {} file(s)",
        total_selected,
        total_chunks,
        srcs.len()
    );
    Ok(())
}

/// The writer state of a backing pair, probed read-only (never acquired):
/// is it served by a live mount, does an appender/import/rotation hold the
/// file's own lock, or is nobody home? Shared by `info`'s prose and
/// `list`'s WRITER column, which only cares about the `Active` case (a
/// mount holds the directory lock, not the per-file one).
pub enum WriterState {
    /// The backing directory is held exclusively by a mount daemon.
    Mounted(Option<PathBuf>),
    /// The file's own writer lock is held: an appender, import or rotation.
    /// Carries the holder's own record of itself when it left one and the
    /// process is still there (see `store::describe_file_writer`).
    Active(Option<String>),
    /// A lock file exists but couldn't be opened (permissions) — unknown.
    Unreadable,
    /// Nobody holds anything.
    Idle,
}

impl WriterState {
    /// `list`'s WRITER column: is a writer live right now, per the
    /// per-file lock specifically (a mount holds the directory lock
    /// instead, so a mounted store reads `false` here).
    pub fn is_live(&self) -> bool {
        matches!(self, WriterState::Active(_))
    }
}

/// A store's vital signs, gathered once from its parsed rings index and
/// manifest — shared by `info`'s detailed print and `list`'s one-line row,
/// so the two commands report identical facts. `records`/`bark` are handed
/// in rather than re-read: `info` already has them from `open_source`, and
/// `list` reads them directly without opening the (unneeded) trunk file.
pub struct StoreSummary {
    pub chunks: usize,
    pub logical_bytes: u64,
    pub compressed_bytes: u64,
    pub first_write_ms: Option<u64>,
    pub last_write_ms: Option<u64>,
    pub rings_bytes: u64,
    /// The chunk NUMBERS held right now, first and last. `None` when the
    /// store holds no chunks.
    pub chunk_seq: Option<(u64, u64)>,
    /// The number the next chunk will get — the numbering high-water
    /// mark, read from the rings HEADER so it survives a head-drop that
    /// removed every record.
    ///
    /// `0` means the store has NEVER BEEN WRITTEN, which is a different
    /// state from "emptied by retention" and is not distinguishable by
    /// chunk count: numbering deliberately does not restart, because a
    /// store renumbering from 0 after being emptied would hand a fresh
    /// chunk a number some cursor counts as consumed.
    pub next_seq: u64,
    /// What has left this store over its life, from the rings header.
    /// All-zero on a store written before the counters existed — which the
    /// numbering tells apart from "nothing dropped".
    pub dropped: crate::format::Dropped,
    pub grain: Option<(u64, usize)>, // (bytes, chunks covered)
    pub index_declared: bool,
    pub wal_declared: bool,
    /// Bytes currently buffered in the `.sap` sidecar (header excluded),
    /// not yet folded into a chunk — a read-only stat, never replayed.
    pub sap_pending_bytes: Option<u64>,
    /// The store's durable identity, when it declares one. A store written
    /// by a plain `append` has no manifest and so no id — which is why a
    /// catalogue must be able to say "none" rather than assume.
    pub id: Option<String>,
    pub created: Option<String>,
    /// The id of the store these entries came FROM, when they arrived over
    /// the wire from another timberfs store. The cross-hop join key — and
    /// only truthful where routing gives one store per origin.
    pub origin_id: Option<String>,
    /// The manifest's provenance keys — what a fleet view selects on. See
    /// `bark::provenance`.
    /// The name the store DECLARES for itself. None where it declares
    /// none, and then the path is the only name it has — the caller
    /// supplies that fallback, because only it knows the path.
    pub declared_name: Option<String>,
    pub labels: serde_json::Map<String, serde_json::Value>,
    pub retain: Option<String>,
    pub retain_size: Option<String>,
    /// `retain_unconsumed`: the third axis is declared on this store, so
    /// its retaining followers' positions hold the head back.
    pub retain_unconsumed: bool,
    /// Lineage: what some other command derived this store FROM, and how.
    /// Read here rather than in one command's own JSON, so both surfaces
    /// report the same thing.
    pub derived_from: Option<String>,
    pub derived_op: Option<String>,
    pub window_from: Option<String>,
    pub window_to: Option<String>,
    pub command: Option<String>,
    pub pattern: Option<String>,
    /// The store's REGISTERED followers, furthest behind first. Empty is
    /// a real and complete answer here — the registry names every
    /// follower of every store, so "none" means none, where an absent
    /// `cursors` directory only ever meant "nowhere to look".
    pub followers: Vec<crate::follower::Registered>,
    /// Who is reading this store and how far behind, when it declares a
    /// `cursors` directory to look in. `None` means no declaration —
    /// which is not the same as nothing reading it, and a view must not
    /// render the two alike.
    ///
    /// ⚠ Superseded by `followers`: a follower declares its store, so a
    /// store declares nothing. Honoured for now (`cursors` shipped in a
    /// release), reported as superseded where it is found.
    pub consumers: Option<crate::cursor::Survey>,
    pub writer: WriterState,
}

impl StoreSummary {
    /// Everyone holding a position in this store, however they were
    /// found: registered followers plus any cursors in a declared
    /// (deprecated) `cursors` directory. The column's question is "is
    /// anyone reading this", which both sources answer.
    pub fn reader_count(&self) -> usize {
        self.followers.len() + self.consumers.as_ref().map_or(0, |sv| sv.consumers.len())
    }

    /// Whether anything at all has a claim to be rendered — so the
    /// column can appear for a store that declares `cursors` and has
    /// nothing in it (declared-and-empty is a state, and a dangerous
    /// one), not only for a store with readers.
    pub fn has_readers(&self) -> bool {
        !self.followers.is_empty() || self.consumers.is_some()
    }

    /// The furthest-behind reader's lag, from whichever source it came —
    /// the number an operator acts on, since a store is large because
    /// somebody is behind.
    ///
    /// Ranked as `follower::rank` does, and for the same reason: a
    /// retaining follower with no position holds the WHOLE store, so it
    /// outranks any measured backlog even though nothing has been
    /// measured for it. Legacy cursors can never be in that state (a
    /// cursor found in a directory declares no interest at all), so they
    /// compare on bytes alone.
    pub fn worst_lag(&self) -> Option<String> {
        let followers = self.followers.iter().map(|r| {
            (
                r.holds_everything(),
                r.standing.map_or(0, |s| s.behind_bytes),
                r.lag_text(),
            )
        });
        let legacy = self
            .consumers
            .iter()
            .flat_map(|sv| sv.consumers.iter())
            .map(|c| (false, c.standing.behind_bytes, c.standing.lag_text()));
        followers
            .chain(legacy)
            .max_by_key(|(everything, bytes, _)| (*everything, *bytes))
            .map(|(_, _, lag)| lag)
    }

    /// How many chunks this store has dropped over its life, exactly. The
    /// numbering knows: it is dense from 0 and only prefixes ever drop, so
    /// the oldest surviving number IS the total — and `next_seq` covers the
    /// store retention emptied, which has no surviving number.
    ///
    /// Nothing is recorded beside it, because nothing could improve on it:
    /// a counter only sees the drops it performed, so it is a subset by
    /// construction. The bytes in `dropped` are the part that HAS to be
    /// recorded, and they cover a suffix of this count — which is what
    /// makes them a floor. Shared by `info` and `list` so neither can
    /// describe the same store differently.
    pub fn dropped_chunks(&self) -> u64 {
        self.chunk_seq.map(|(f, _)| f).unwrap_or(self.next_seq)
    }

    /// `list`'s INDEX column: a `.grain` token index that is present, or
    /// declared (and due to be rebuilt on the next import if actually
    /// missing) — either way, `--has` queries are meant to work here.
    pub fn indexed(&self) -> bool {
        self.index_declared || self.grain.is_some()
    }
}

/// `followers` is passed IN rather than looked up here, so a command
/// summarising many stores reads the registry once (see
/// `follower::by_store`) instead of once per store.
pub fn summarize_store(
    dir: &Path,
    name: &str,
    records: &[ChunkRecord],
    bark: Option<&serde_json::Map<String, serde_json::Value>>,
    followers: Vec<crate::follower::Registered>,
) -> StoreSummary {
    let (chunks, logical_bytes, compressed_bytes) = match (records.first(), records.last()) {
        (Some(f), Some(l)) => (
            records.len(),
            l.uncomp_end() - f.uncomp_start,
            l.comp_end() - f.comp_start,
        ),
        _ => (0, 0, 0),
    };
    // Mostly-sorted windows: scan for the true extremes (48 B per chunk).
    let (first_write_ms, last_write_ms) = if records.is_empty() {
        (None, None)
    } else {
        let (mut min_ms, mut max_ms) = (u64::MAX, 0u64);
        for r in records {
            min_ms = min_ms.min(r.first_write_ms);
            max_ms = max_ms.max(r.last_write_ms);
        }
        (Some(min_ms), Some(max_ms))
    };
    let rings_path = format::rings_path(dir, name);
    let rings_bytes = std::fs::metadata(&rings_path).map(|m| m.len()).unwrap_or(0);
    let chunk_seq = match (records.first(), records.last()) {
        (Some(f), Some(l)) => Some((f.seq, l.seq)),
        _ => None,
    };
    // From the header, falling back to the records: a v1 rings file has no
    // high-water mark, and then the last record is all there is.
    let next_seq = std::fs::File::open(&rings_path)
        .and_then(|f| format::read_header_next_seq(&f))
        .unwrap_or(0)
        .max(records.last().map(|c| c.seq + 1).unwrap_or(0));
    let dropped = std::fs::File::open(&rings_path)
        .and_then(|f| format::read_header_dropped(&f))
        .unwrap_or_default();
    let gpath = format::grain_path(dir, name);
    let grain = std::fs::metadata(&gpath).ok().and_then(|m| {
        crate::grain::load(&gpath)
            .ok()
            .map(|g| (m.len(), g.chunk_count()))
    });
    let get = |k: &str| {
        bark.and_then(|b| b.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let index_declared = bark
        .and_then(|b| b.get("index"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let wal_declared = bark
        .and_then(|b| b.get("wal"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Read-only: a stat of the live sidecar's size, never opened for
    // replay (that only ever happens from a writer's FileStore::open).
    let sap_pending_bytes = std::fs::metadata(format::sap_path(dir, name))
        .ok()
        .map(|m| m.len().saturating_sub(crate::sap::HEADER_LEN));

    // Who is writing? flock presence is the truth (lock files persist and
    // their contents go stale). This is a READ-ONLY probe — an observation,
    // never an acquisition — so `info`/`list` work on a store they can only
    // read (e.g. a root-owned /var/log/timberfs).
    use crate::store::LockProbe;
    let writer = match crate::store::probe_backing_exclusive(dir) {
        LockProbe::Held => WriterState::Mounted(crate::store::read_lock_mountpoint(dir)),
        LockProbe::Unreadable => WriterState::Unreadable,
        LockProbe::Absent | LockProbe::Free => match crate::store::probe_file_writer(dir, name) {
            LockProbe::Held => WriterState::Active(crate::store::describe_file_writer(dir, name)),
            LockProbe::Unreadable => WriterState::Unreadable,
            LockProbe::Absent | LockProbe::Free => WriterState::Idle,
        },
    };

    StoreSummary {
        chunks,
        logical_bytes,
        compressed_bytes,
        first_write_ms,
        last_write_ms,
        rings_bytes,
        chunk_seq,
        next_seq,
        dropped,
        grain,
        index_declared,
        wal_declared,
        sap_pending_bytes,
        id: get("id"),
        created: get("created"),
        origin_id: bark.and_then(crate::bark::origin_id),
        declared_name: get("name"),
        labels: bark.map(crate::bark::provenance).unwrap_or_default(),
        retain: get("retain"),
        retain_size: get("retain_size"),
        retain_unconsumed: bark
            .and_then(|b| b.get("retain_unconsumed"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        derived_from: get("derived_from"),
        derived_op: get("derived_op"),
        window_from: get("window_from"),
        window_to: get("window_to"),
        command: get("command"),
        pattern: get("pattern"),
        followers,
        consumers: crate::cursor::survey(dir, name, bark, records),
        writer,
    }
}

/// `info`'s prose rendering of a writer state — also what its `--json`
/// mode prints under `"writer"`.
fn writer_text(w: &WriterState) -> String {
    match w {
        WriterState::Mounted(Some(mp)) => format!("mounted at {}", mp.display()),
        WriterState::Mounted(None) => "another timberfs process holds the directory".to_string(),
        WriterState::Active(Some(who)) => format!("active writer: {who}"),
        WriterState::Active(None) => "active writer (appender, import or rotation)".to_string(),
        WriterState::Unreadable => "unknown (lock file not readable)".to_string(),
        WriterState::Idle => "none".to_string(),
    }
}

/// Human-readable dump of the write-time index.
/// `timberfs info`: a store's vital signs on one screen — identity,
/// lineage, provenance, data/compression, time covered, index sizes and
/// coverage, writer state. The `\d+` of the database metaphor. Read-only;
/// works identically on backing pairs and .timber bundles.
pub fn cmd_info(input: &Path, json: bool) -> anyhow::Result<()> {
    let bundled = is_bundle(input);
    let handle = open_source(input)?;
    let records = &handle.records;

    let bark = handle.bark.clone().unwrap_or_default();
    let get = |k: &str| bark.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let id = get("id");
    let created = get("created");
    let derived_from = get("derived_from");
    let derived_op = get("derived_op");
    let window_from = get("window_from");
    let window_to = get("window_to");
    let index_declared = bark.get("index").and_then(|v| v.as_bool()).unwrap_or(false);
    let command = get("command");
    let pattern = get("pattern");
    let retain = get("retain");
    let retain_size = get("retain_size");
    let retain_unconsumed = bark
        .get("retain_unconsumed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // `bark` owns what its keys mean, so ask it rather than keeping a
    // second list here: a view that re-guessed the split would drift, and
    // this one had — leaking `wal`, the `timestamp_*` settings and the
    // `timberfs.store.*` lineage into what reads as a label set. `list`
    // has always used this; now the two agree.
    let provenance: Vec<(String, String)> = crate::bark::provenance(&bark)
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string()),
            )
        })
        .collect();

    // Pair-only facts: size/span, sidecar sizes, grain coverage, writer
    // state — computed by the same `summarize_store` that builds a `list`
    // row, so the two commands agree on what they report.
    // What the store is CALLED: the name it declares, else the one its
    // path gives it. `list` answers the same way, and `info <name>` that
    // replied with a uuid header would be answering a different question
    // than the one asked.
    let mut name = bark
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            input
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let mut location = String::new();
    let mut bundle_bytes: Option<u64> = None;
    // Pair-only facts, like the writer state: a bundle is a snapshot,
    // and nothing holds a position in a snapshot.
    let mut consumers: Option<crate::cursor::Survey> = None;
    let mut followers: Vec<crate::follower::Registered> = Vec::new();
    // Pair-only, and deliberately: `export` numbers a bundle's chunks from
    // 0 because it selects a window out of the MIDDLE, so a bundle's
    // numbering carries no history and reporting one would be a lie.
    let mut numbering: Option<Numbering> = None;
    // Kept whole, because `info --json` emits the SAME object `list --json`
    // does and that object is built from a summary. A bundle gets one too:
    // it is a store, it just has no writer, no sap and no cursors.
    // Both branches below set it; declared here so the human rendering
    // that follows can still take the summary apart.
    let store_json: serde_json::Value;
    let (
        chunks,
        logical,
        compressed,
        min_ms,
        max_ms,
        rings_bytes,
        grain,
        writer,
        wal_declared,
        sap_pending,
    ) = if bundled {
        bundle_bytes = std::fs::metadata(input).map(|m| m.len()).ok();
        let chunks = records.len();
        let (logical, compressed) = match (records.first(), records.last()) {
            (Some(f), Some(l)) => (l.uncomp_end() - f.uncomp_start, l.comp_end() - f.comp_start),
            _ => (0, 0),
        };
        // Mostly-sorted windows: scan for the true extremes (48 B per chunk).
        let (mut min_ms, mut max_ms) = (u64::MAX, 0u64);
        for r in records {
            min_ms = min_ms.min(r.first_write_ms);
            max_ms = max_ms.max(r.last_write_ms);
        }
        // A `.timber` bundle is a snapshot, not a live writer state:
        // it never carries a sap.
        let bark = handle.bark.clone().unwrap_or_default();
        let g = |k: &str| bark.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let synthesized = StoreSummary {
            chunks,
            logical_bytes: logical,
            compressed_bytes: compressed,
            first_write_ms: (chunks > 0).then_some(min_ms),
            last_write_ms: (chunks > 0).then_some(max_ms),
            rings_bytes: 0,
            // A bundle's chunks are renumbered from 0 by `export`, which
            // takes a window out of the middle — so its numbering carries
            // no history and reporting one would be a lie.
            chunk_seq: None,
            next_seq: 0,
            dropped: crate::format::Dropped::default(),
            grain: None,
            index_declared: bark.get("index").and_then(|v| v.as_bool()).unwrap_or(false),
            wal_declared: false,
            sap_pending_bytes: None,
            id: g("id"),
            created: g("created"),
            origin_id: crate::bark::origin_id(&bark),
            declared_name: g("name"),
            labels: crate::bark::provenance(&bark),
            retain: g("retain"),
            retain_size: g("retain_size"),
            retain_unconsumed: bark
                .get("retain_unconsumed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            derived_from: g("derived_from"),
            derived_op: g("derived_op"),
            window_from: g("window_from"),
            window_to: g("window_to"),
            command: g("command"),
            pattern: g("pattern"),
            followers: Vec::new(),
            consumers: None,
            writer: WriterState::Idle,
        };
        store_json = store_value(
            &synthesized,
            // The directory holding it, as for a pair — `path` is the
            // bundle itself.
            input.parent().unwrap_or(Path::new(".")),
            input,
            crate::forest::handle_of_logical(&name),
            crate::store_json::Kind::Bundle,
            std::fs::metadata(input).map(|m| m.len()).ok(),
        );
        (
            chunks, logical, compressed, min_ms, max_ms, None, None, None, false, None,
        )
    } else {
        let (dir, base) = resolve_backing(input)?;
        // Only where the store declares no name of its own: the path is
        // then the only name it has.
        if handle
            .bark
            .as_ref()
            .and_then(|b| b.get("name"))
            .and_then(|v| v.as_str())
            .is_none()
        {
            name = base.clone();
        }
        location = dir.display().to_string();
        let anchor = crate::cursor::store_anchor(&dir, &base, handle.bark.as_ref());
        let declared = crate::follower::for_store(&crate::follower::registry_dir(), &anchor);
        let s = summarize_store(&dir, &base, records, handle.bark.as_ref(), declared);
        // Classified before anything is moved out of the summary.
        numbering = Some(Numbering {
            held: s.chunk_seq,
            next_seq: s.next_seq,
            dropped: s.dropped,
            total_chunks: s.dropped_chunks(),
        });
        // The same handle `list` shows: a store cannot be called one thing
        // by one command and another by the next.
        store_json = store_value(
            &s,
            &dir,
            input,
            crate::forest::handle_of_logical(&base),
            crate::store_json::Kind::Pair,
            None,
        );
        consumers = s.consumers;
        followers = s.followers;
        (
            s.chunks,
            s.logical_bytes,
            s.compressed_bytes,
            s.first_write_ms.unwrap_or(u64::MAX),
            s.last_write_ms.unwrap_or(0),
            Some(s.rings_bytes),
            s.grain,
            Some(writer_text(&s.writer)),
            s.wal_declared,
            s.sap_pending_bytes,
        )
    };

    if json {
        // One shape for every surface that emits a store. This was a
        // hand-rolled map beside `list`'s own hand-rolled map: 39 keys
        // between them, 10 shared, the same data under different names —
        // and `name` holding the FILE's name here and the store's name
        // there, which no consumer could join across.
        println!("{}", serde_json::to_string_pretty(&store_json)?);
        return Ok(());
    }

    if bundled {
        println!(
            "{name} — .timber bundle ({}), read-only",
            crate::rotate::human_bytes(bundle_bytes.unwrap_or(0))
        );
    } else {
        println!("{name} — timberfs log in {location}/");
    }
    if let Some(id) = &id {
        println!(
            "  identity  {id}, created {}",
            created.as_deref().unwrap_or("?")
        );
    }
    if let Some(from) = &derived_from {
        let window = match (&window_from, &window_to) {
            (None, None) => String::new(),
            (f, t) => format!(
                ", window {} .. {}",
                f.as_deref().unwrap_or("start"),
                t.as_deref().unwrap_or("end")
            ),
        };
        println!(
            "  lineage   derived from {from} by {}{window}",
            derived_op.as_deref().unwrap_or("?")
        );
    }
    // The operation, as typed — an investigation artifact explains itself.
    if let Some(c) = &command {
        println!("  question  {c}");
    } else if let Some(pt) = &pattern {
        println!("  pattern   {pt}");
    }
    if !provenance.is_empty() || index_declared {
        let mut parts: Vec<String> = provenance.iter().map(|(k, v)| format!("{k}={v}")).collect();
        if index_declared {
            parts.push("index declared".to_string());
        }
        println!("  manifest  {}", parts.join(", "));
    }
    if chunks == 0 {
        println!("  data      empty (an attested empty result is still a result)");
    } else {
        println!(
            "  data      {} in {chunks} chunk(s) -> {} on disk ({:.1}x)",
            crate::rotate::human_bytes(logical),
            crate::rotate::human_bytes(compressed),
            logical as f64 / compressed.max(1) as f64
        );
        println!(
            "  covers    {} .. {}  ({})",
            fmt_ms(min_ms),
            fmt_ms(max_ms),
            human_duration(max_ms.saturating_sub(min_ms))
        );
    }
    if !bundled {
        let rings = crate::rotate::human_bytes(rings_bytes.unwrap_or(0));
        match grain {
            Some((b, n)) => println!(
                "  index     rings {rings}; grain {}, covers {n}/{chunks} chunk(s){}",
                crate::rotate::human_bytes(b),
                if n < chunks { " (rest is scanned)" } else { "" }
            ),
            None if index_declared => println!(
                "  index     rings {rings}; grain declared but MISSING — next import \
                 rebuilds it (or run reindex)"
            ),
            None => println!("  index     rings {rings}; no grain (reindex to build one)"),
        }
        if wal_declared {
            match sap_pending {
                Some(p) => println!(
                    "  wal       declared; {} buffered in .sap, not yet in a chunk",
                    crate::rotate::human_bytes(p)
                ),
                None => println!(
                    "  wal       declared but the .sap is MISSING — the next writer \
                     to open this log recreates it"
                ),
            }
        }
        if retain.is_some() || retain_size.is_some() || retain_unconsumed {
            let mut parts: Vec<String> = Vec::new();
            if let Some(r) = &retain {
                parts.push(format!("keep {r}"));
            }
            if let Some(r) = &retain_size {
                parts.push(format!("disk <= {r}"));
            }
            if retain_unconsumed {
                // Named as what it KEEPS, like the other two, and the
                // asymmetry stated: this axis only ever holds more, which
                // is why the budget beside it is required rather than
                // optional.
                parts.push("keep what retaining followers have not read".to_string());
            }
            // Retention only acts while a writer runs: an idle store with
            // a policy doesn't shrink — say so instead of surprising, and
            // point at the one-shot that does it without one.
            let over = retain_size
                .as_deref()
                .and_then(|r| crate::append::parse_size_bytes(r).ok())
                .is_some_and(|budget| compressed > budget)
                && writer.as_deref() == Some("none");
            println!(
                "  retention {} — enforced by writers{}",
                parts.join(", "),
                if over {
                    " (currently OVER budget, and none is running — `timberfs trim`)"
                } else {
                    ""
                }
            );
        }
        if !bundled {
            print_followers(&followers, compressed);
        }
        print_numbering(numbering);
        if let Some(sv) = &consumers {
            print_consumers(sv, compressed);
        }
        if let Some(w) = &writer {
            println!("  writer    {w}");
        }
    }
    Ok(())
}

/// `info`'s numbering line — shown only when it carries news, which is
/// when the store no longer holds its whole history.
///
/// The fact it exposes is not otherwise obtainable: numbering is dense,
/// starts at 0, and only ever loses a PREFIX (retention and rotation both
/// take from the head), so **the oldest surviving chunk number is exactly
/// how many chunks this store has dropped over its life**. A chunk count
/// cannot say that, and neither can a time span.
///
/// The count and the sizes are now RECORDED in the rings header rather than
/// derived, so neither rests on numbering starting at 0 — which is what a
/// window extract or a partial replica would break. The derivation survives
/// only as the fallback for a header that predates the counters, and
/// `chunks == 0` beside a non-zero oldest number is how that is detected:
/// a store whose oldest chunk is number 0 has genuinely dropped nothing.
///
/// Still pair-only: `export` numbers a bundle from 0, so a bundle has no
/// drop history to report.
///
/// It also separates two states a chunk count renders identically: a store
/// that was never written (`next_seq == 0`) and one that retention emptied
/// (`next_seq > 0`, no chunks held). Numbering deliberately does not
/// restart, so the second keeps its high-water mark — and only the first is
/// eligible to adopt an origin's numbering (see ROADMAP, "Globally
/// addressable chunks").
/// A store's numbering history: the chunk numbers it holds now, where the
/// numbering stands, and what has left over its life. Pair-only — see
/// `print_numbering`.
#[derive(Clone, Copy)]
struct Numbering {
    held: Option<(u64, u64)>,
    next_seq: u64,
    dropped: crate::format::Dropped,
    /// The TRUE lifetime drop count, from `StoreSummary::dropped_chunks`.
    /// The sizes in `dropped` cover a suffix of it, so they are a floor.
    total_chunks: u64,
}

fn print_numbering(numbering: Option<Numbering>) {
    let Some(Numbering {
        held: seq,
        next_seq: next,
        dropped,
        total_chunks,
    }) = numbering
    else {
        return;
    };
    // A count and a size. Zero measured bytes over dropped chunks is said
    // in words rather than printed as "0 B on disk", which on a store that
    // dropped gigabytes reads as a broken tool — and it is unambiguous: a
    // real chunk carries a frame header, so it can never compress to
    // nothing.
    let cost = match total_chunks {
        0 => String::new(),
        n if dropped.comp_bytes == 0 => format!("{n} dropped (size not measured yet)"),
        n => format!(
            "{n} dropped ({} on disk, {} uncompressed)",
            crate::rotate::human_bytes(dropped.comp_bytes),
            crate::rotate::human_bytes(dropped.uncomp_bytes)
        ),
    };
    match seq {
        // Holding its whole history: the chunk count on the `data` line
        // already says everything, so say nothing.
        Some((0, _)) if total_chunks == 0 => {}
        Some((first, last)) => println!("  numbering chunks {first}..{last} held; {cost}"),
        // Emptied, not reset — invisible from the chunk count alone.
        None if next > 0 => println!("  numbering no chunks held; {cost} — emptied, not reset"),
        None => {}
    }
}

/// `info`'s follower block: who is registered against this store, what
/// they hold, and where each stands. Silent when the registry names none
/// — an unused feature must not put a line on every `info`.
///
/// The retaining ones are what an operator is looking for, so the header
/// leads with them: they are the reason a store is large, and until
/// PR-next's `retain_unconsumed` lands they are also the reason it is
/// NOT, which is worth being honest about in one place.
fn print_followers(followers: &[crate::follower::Registered], compressed: u64) {
    if followers.is_empty() {
        return;
    }
    let retaining: Vec<&crate::follower::Registered> =
        followers.iter().filter(|r| r.decl.retaining).collect();
    // What no retention honouring these followers could drop: the
    // furthest-behind retaining one's backlog — or the whole store, when
    // one of them has never run, since it holds everything.
    let held = if retaining.iter().any(|r| r.holds_everything()) {
        compressed
    } else {
        retaining
            .iter()
            .map(|r| r.standing.map_or(0, |s| s.behind_bytes))
            .max()
            .unwrap_or(0)
    };
    if retaining.is_empty() {
        println!(
            "  followers {} registered, none retaining — nothing holds the head back",
            followers.len()
        );
    } else {
        println!(
            "  followers {} registered, {} retaining; {} of {} held",
            followers.len(),
            retaining.len(),
            crate::rotate::human_bytes(held),
            crate::rotate::human_bytes(compressed)
        );
    }
    let width = followers
        .iter()
        .map(|r| r.name().len())
        .max()
        .unwrap_or(0)
        .max(8);
    for r in followers {
        let mut detail = r.lag_text();
        if let Some(st) = &r.standing {
            if st.gap_chunks.is_none() && st.behind_chunks > 0 && !st.at_live_edge() {
                detail = format!(
                    "{detail}, {} unread in {} chunk(s)",
                    crate::rotate::human_bytes(st.behind_bytes),
                    st.behind_chunks
                );
            }
        }
        println!(
            "            {:<width$}  {}{}{}  [{}]",
            r.name(),
            if r.decl.retaining { "retaining, " } else { "" },
            detail,
            match &r.cursor {
                Some(c) if c.delivered > 0 => format!("; {} delivered", c.delivered),
                _ => String::new(),
            },
            r.live.word(),
            width = width
        );
    }
}

/// `info`'s consumer block: a header saying what the whole store's
/// backlog is, then one line per consumer, furthest-behind first. The
/// header leads with the held bytes because that is the number an
/// operator acts on — a store is large because someone is behind, and
/// this names who.
fn print_consumers(sv: &crate::cursor::Survey, compressed: u64) {
    let dir = sv.dir.display();
    // Reported as superseded wherever it is found. `cursors` shipped in
    // a release, so it is owed a deprecation rather than a silent
    // removal — but a store no longer declares its readers, a follower
    // declares its store, and only one of the two can be the truth.
    println!(
        "  cursors   {dir}/ — SUPERSEDED by the follower registry \
         (`timberfs follower create`); still honoured"
    );
    if sv.consumers.is_empty() {
        // Declared but empty is a real state, and a dangerous-looking
        // one: nothing holds a position, so nothing is protected.
        println!(
            "  consumers none in {dir}/ — no cursor here is a position in this store{}",
            if sv.unreadable > 0 {
                format!(" ({} unreadable file(s))", sv.unreadable)
            } else {
                String::new()
            }
        );
        return;
    }
    let held = sv.held_bytes();
    let worst = sv.worst().expect("non-empty");
    println!(
        "  consumers {} in {dir}/; {} of {} {} by {} ({})",
        sv.consumers.len(),
        crate::rotate::human_bytes(held),
        crate::rotate::human_bytes(compressed),
        // A gapped consumer has unread bytes but holds nothing back:
        // retention already went past it.
        if worst.standing.gap_chunks.is_some() {
            "unread"
        } else {
            "held"
        },
        worst.name,
        worst.standing.lag_text()
    );
    let width = sv
        .consumers
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0)
        .max(8);
    for c in &sv.consumers {
        let st = &c.standing;
        let detail = match st.gap_chunks {
            // The one case where the position itself is the news: what
            // it would resume at is gone, so the entries between are
            // unrecoverable and no retry re-delivers them. A count, not a
            // duration: the chunk numbers say exactly how many went.
            Some(n) => format!(
                "GAP — {n} chunk(s) were dropped before it read them; \
                 it resumes at the oldest one still here"
            ),
            // The open chunk a live-edge consumer sits in is not a
            // backlog, so it is not reported as one.
            None if st.caught_up() || st.at_live_edge() => st.lag_text(),
            None => format!(
                "{}, {} unread in {} chunk(s)",
                st.lag_text(),
                crate::rotate::human_bytes(st.behind_bytes),
                st.behind_chunks
            ),
        };
        println!(
            "            {:<width$}  {detail}; {} delivered",
            c.name,
            c.cursor.delivered,
            width = width
        );
    }
    if sv.unreadable > 0 {
        println!(
            "            plus {} file(s) in {dir}/ not readable as cursors",
            sv.unreadable
        );
    }
}

/// The per-consumer detail, shared by `info --json` and `list --json`
/// so a script sees the same fields whichever it asks.
pub(crate) fn consumers_json(sv: &crate::cursor::Survey) -> serde_json::Value {
    serde_json::Value::Array(
        sv.consumers
            .iter()
            .map(|c| {
                let st = &c.standing;
                let mut o = serde_json::Map::new();
                o.insert("consumer".to_string(), c.name.clone().into());
                o.insert("cursor".to_string(), c.path.display().to_string().into());
                o.insert(
                    "seq".to_string(),
                    c.cursor
                        .seq
                        .map(Into::into)
                        .unwrap_or(serde_json::Value::Null),
                );
                o.insert("wl".to_string(), c.cursor.wl.into());
                o.insert("n".to_string(), c.cursor.n.into());
                o.insert("delivered".to_string(), c.cursor.delivered.into());
                o.insert("consumed_chunks".to_string(), st.consumed_chunks.into());
                o.insert("behind_chunks".to_string(), st.behind_chunks.into());
                o.insert("behind_bytes".to_string(), st.behind_bytes.into());
                o.insert("behind_ms".to_string(), st.behind_ms.into());
                o.insert(
                    "gap_chunks".to_string(),
                    st.gap_chunks
                        .map(Into::into)
                        .unwrap_or(serde_json::Value::Null),
                );
                serde_json::Value::Object(o)
            })
            .collect(),
    )
}

pub fn human_duration(ms: u64) -> String {
    let s = ms / 1000;
    let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {}s", s % 60)
    } else {
        format!("{}.{:03}s", s, ms % 1000)
    }
}

pub fn cmd_index(file: &Path) -> anyhow::Result<()> {
    let chunks = open_source(file)?.records;
    println!(
        "{:>5}  {:>12}  {:>10}  {:>10}  {:>6}  {:<23}  {:<23}",
        "chunk", "uncomp@", "bytes", "comp", "ratio", "first write", "last write"
    );
    let mut total_uncomp = 0u64;
    let mut total_comp = 0u64;
    // The chunk's NUMBER, not its row: after a head-drop the two differ,
    // and the number is the one a cursor holds and a drop record names.
    for c in chunks.iter() {
        println!(
            "{:>5}  {:>12}  {:>10}  {:>10}  {:>5.1}x  {:<23}  {:<23}",
            c.seq,
            c.uncomp_start,
            c.uncomp_len,
            c.comp_len,
            c.uncomp_len as f64 / c.comp_len.max(1) as f64,
            fmt_ms(c.first_write_ms),
            fmt_ms(c.last_write_ms)
        );
        total_uncomp += c.uncomp_len;
        total_comp += c.comp_len;
    }
    println!(
        "total: {} chunk(s), {} bytes uncompressed, {} compressed ({:.1}x)",
        chunks.len(),
        total_uncomp,
        total_comp,
        total_uncomp as f64 / total_comp.max(1) as f64
    );
    Ok(())
}

#[cfg(test)]
mod numbering_tests {
    #[test]
    fn the_oldest_surviving_number_is_the_true_drop_count() {
        // Numbering is dense, starts at 0, and only ever loses a PREFIX
        // (retention and rotation both take from the head) — so the oldest
        // surviving number IS how many went, whatever the counters saw. No
        // chunk count and no time span can carry that.
        assert_eq!(
            line(Some((6, 16)), 17, 4096).unwrap(),
            "chunks 6..16 held; 6 dropped"
        );
        // Holding its whole history: nothing to say, since the chunk count
        // on the `data` line already says it.
        assert_eq!(line(Some((0, 16)), 17, 0), None);
        assert_eq!(line(Some((0, 0)), 1, 0), None);
    }

    #[test]
    fn an_unmeasured_size_is_said_in_words_not_printed_as_zero() {
        // A store that dropped under a binary older than the counters: the
        // count is exact (the numbering knows it) and the size is missing
        // entirely. "0 B on disk" beside 8 dropped chunks reads as a broken
        // tool, so the line says which of the two it is. Unambiguous
        // because a real chunk carries a frame header and can never
        // compress to nothing.
        assert_eq!(
            line(Some((8, 16)), 17, 0).unwrap(),
            "chunks 8..16 held; 8 dropped (size not measured yet)"
        );
        // Any measured size at all: reported as-is. It is a FLOOR — those
        // 8 chunks may include earlier ones nothing sized — and the count
        // is the exact number gone either way.
        assert_eq!(
            line(Some((8, 16)), 17, 1).unwrap(),
            "chunks 8..16 held; 8 dropped"
        );
        // Nothing dropped at all: nothing to say.
        assert_eq!(line(Some((0, 16)), 17, 0), None);
    }

    /// What `print_numbering` renders, with the byte text elided — the same
    /// count-and-size logic and the same outer match, so the tests exercise
    /// the shape rather than a paraphrase of it. `bytes` is the MEASURED
    /// compressed size, which is what decides the wording.
    fn line(seq: Option<(u64, u64)>, next: u64, bytes: u64) -> Option<String> {
        let total = seq.map(|(f, _)| f).unwrap_or(next);
        let cost = match total {
            0 => String::new(),
            n if bytes == 0 => format!("{n} dropped (size not measured yet)"),
            n => format!("{n} dropped"),
        };
        match seq {
            Some((0, _)) if total == 0 => None,
            Some((first, last)) => Some(format!("chunks {first}..{last} held; {cost}")),
            None if next > 0 => Some(format!("no chunks held; {cost} — emptied, not reset")),
            None => None,
        }
    }

    #[test]
    fn emptied_and_never_written_are_told_apart() {
        // A chunk count renders these identically, and they are opposite:
        // numbering does not restart, so only the never-written store is
        // eligible to adopt an origin's numbering (ROADMAP, "Globally
        // addressable chunks").
        let emptied = line(None, 12, 2048).unwrap();
        assert!(emptied.contains("12 dropped"), "{emptied}");
        assert!(emptied.contains("emptied, not reset"), "{emptied}");
        assert_eq!(
            line(None, 0, 0),
            None,
            "never written has no history to report"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_omitted_field_means_unbounded_not_empty() {
        // The semantics a `--query` document inherits: a member left out
        // widens the search rather than emptying it. `Default` is
        // therefore "every entry of every named store", which is what an
        // absent window, absent predicates and absent limits must mean —
        // get this backwards in the document format and an omitted field
        // silently returns nothing.
        let q = Query::default();
        assert!(q.window.from.is_none() && q.window.to.is_none());
        assert!(q.matching.is_empty(), "no predicate = match everything");
        assert!(q.limit.max.is_none() && q.limit.tail.is_none());
        assert!(!q.follow.follow);
        // ...and the output default is the plain log view, not a framing
        // that a consumer has to opt out of.
        assert!(!q.output.records && !q.output.null_sep && !q.output.by_write_time);
    }

    #[test]
    fn max_and_tail_are_different_operations() {
        // Not one bound with a sign: `max` caps forward from the start,
        // `tail` takes the last N. They conflict at the CLI for that
        // reason, and the document format has to keep them apart.
        let mut q = Query::default();
        q.limit.max = Some(10);
        q.limit.tail = Some(10);
        assert_ne!(
            (q.limit.max, None::<u64>),
            (None, q.limit.tail),
            "if these were interchangeable the format could merge them"
        );
    }

    /// The refusal `view` turns on. A chunk number was confined to a
    /// FOLLOWING read on the reasoning that it is a resume position; random
    /// access is the second reason to name one, and `from_chunk` with a
    /// one-chunk cap is a seek. What is refused now is a second START,
    /// which is a different sentence.
    #[test]
    fn a_chunk_number_seeks_a_bounded_read() {
        let mut seek = Query::default();
        seek.window.from_chunk = Some(412_000);
        seek.limit.max_chunks = Some(1);
        seek.output.chunk_records = true;
        assert!(
            seek.validate().is_ok(),
            "{:?}",
            seek.validate().unwrap_err()
        );

        // ...and it still resumes a follow, which is where it came from.
        let mut resume = Query::default();
        resume.window.from_chunk = Some(412_000);
        resume.follow.follow = true;
        assert!(resume.validate().is_ok());
    }

    /// Each of these names where the read begins, and so does a chunk
    /// number. Two starts have no rule for which wins, and the old code
    /// silently picked one — a caller asking for a place got the tail it
    /// also asked for, and never learnt that its position was dropped.
    #[test]
    fn a_read_has_one_start() {
        let seeking = || {
            let mut q = Query::default();
            q.window.from_chunk = Some(412_000);
            q
        };
        let refused = |name: &str, q: Query| {
            assert!(
                q.validate().is_err(),
                "{name} beside a chunk number is two starts, and was accepted"
            );
        };
        let mut q = seeking();
        q.window.from = Some(1_000);
        refused("a from timestamp", q);

        let mut q = seeking();
        q.limit.tail = Some(10);
        refused("a tail of entries", q);

        let mut q = seeking();
        q.limit.tail_chunks = Some(10);
        refused("a tail of chunks", q);

        let mut q = seeking();
        q.cursor.insert("79d7f23a".to_string(), 33_724_753_900);
        refused("a cursor", q);
    }

    /// A chunk number is a place, not a time: it selects beside the window
    /// rather than instead of it, so `to` still bounds the far end and a
    /// predicate still narrows what comes back.
    #[test]
    fn a_seek_starts_at_the_number_and_survives_retention() {
        let records = vec![
            chunk(0, 100, 1000, 2000),
            chunk(100, 100, 2000, 3000),
            chunk(200, 100, 3000, 4000),
        ];
        let seq = |n: Option<u64>| -> Vec<u64> {
            select_chunks(
                Path::new("/nonexistent"),
                &records,
                None,
                0,
                u64::MAX,
                n,
                &[],
                &[],
            )
            .unwrap()
            .0
            .iter()
            .map(|(_, c)| c.seq)
            .collect()
        };
        assert_eq!(seq(None), vec![0, 1, 2], "no seek reads the whole store");
        assert_eq!(seq(Some(1)), vec![1, 2], "forward from the number named");
        assert_eq!(
            seq(Some(9)),
            Vec::<u64>::new(),
            "past the head selects nothing, rather than wrapping to the start"
        );

        // A head-drop leaves the numbers alone (they are what a position
        // holds), so a seek below the floor lands on the oldest survivor
        // rather than on nothing: only the caller knows the place it named
        // was dropped, so only the caller can report it.
        let trimmed: Vec<ChunkRecord> = records
            .iter()
            .enumerate()
            .map(|(i, c)| ChunkRecord {
                seq: 5 + i as u64,
                ..*c
            })
            .collect();
        let survivors = select_chunks(
            Path::new("/nonexistent"),
            &trimmed,
            None,
            0,
            u64::MAX,
            Some(2),
            &[],
            &[],
        )
        .unwrap()
        .0;
        assert_eq!(
            survivors.iter().map(|(_, c)| c.seq).collect::<Vec<_>>(),
            vec![5, 6, 7]
        );
        // The window still bounds the far end.
        let bounded = select_chunks(
            Path::new("/nonexistent"),
            &records,
            None,
            0,
            2500,
            Some(1),
            &[],
            &[],
        )
        .unwrap()
        .0;
        assert_eq!(
            bounded.iter().map(|(_, c)| c.seq).collect::<Vec<_>>(),
            vec![1]
        );
    }

    fn chunk(uncomp_start: u64, len: u64, first: u64, last: u64) -> ChunkRecord {
        ChunkRecord {
            uncomp_start,
            uncomp_len: len,
            comp_start: 0,
            comp_len: len / 2,
            first_write_ms: first,
            last_write_ms: last,
            seq: uncomp_start / len.max(1),
        }
    }

    /// The regression that made this a key rather than a timestamp: chunk
    /// windows come from the entries' own loglines on an imported or
    /// followed store, so adjacent chunks routinely share a boundary
    /// millisecond (a log stamped to the second, or four flushes a second
    /// at a megabyte a second). "Later than the last one" silently drops
    /// every second chunk — measured 10 of 27 in one 45-second run.
    #[test]
    fn a_shared_boundary_millisecond_does_not_skip_a_chunk() {
        let records = vec![
            chunk(0, 100, 1000, 2000),
            chunk(100, 100, 2000, 3000),
            chunk(200, 100, 3000, 3000),
        ];
        let mut anchor = Some((key(&records[0]), 0));
        assert_eq!(resume_at(&records, &anchor), Some(1));
        anchor = Some((key(&records[1]), 1));
        assert_eq!(resume_at(&records, &anchor), Some(2));
        anchor = Some((key(&records[2]), 2));
        assert_eq!(
            resume_at(&records, &anchor),
            Some(3),
            "caught up, not behind"
        );
    }

    #[test]
    fn nothing_seen_yet_means_everything_is_new() {
        let records = vec![chunk(0, 100, 1000, 2000)];
        assert_eq!(resume_at(&records, &None), Some(0));
        assert_eq!(resume_at(&[], &None), Some(0));
    }

    /// A retention head trim renumbers every chunk (and rebases every
    /// logical offset), so the anchor's INDEX goes stale while its
    /// identity does not.
    #[test]
    fn a_head_trim_shifts_the_anchor_without_losing_it() {
        let before = [
            chunk(0, 100, 1000, 2000),
            chunk(100, 100, 2000, 3000),
            chunk(200, 100, 3000, 4000),
        ];
        let anchor = Some((key(&before[1]), 1));
        // The two oldest chunks are dropped and the rest rebased to 0.
        let after = vec![chunk(0, 100, 2000, 3000), chunk(100, 100, 3000, 4000)];
        assert_eq!(resume_at(&after, &anchor), Some(1), "the anchor moved to 0");
    }

    #[test]
    fn an_anchor_retention_dropped_is_reported_not_guessed() {
        let anchor = Some((key(&chunk(0, 100, 1000, 2000)), 0));
        let after = vec![chunk(0, 100, 5000, 6000)];
        assert_eq!(resume_at(&after, &anchor), None);
    }

    /// Identical adjacent chunks (a heartbeat log flushing the same bytes
    /// in the same millisecond) are told apart by position, not content.
    #[test]
    fn identical_neighbours_resume_at_the_right_one() {
        let records = vec![
            chunk(0, 100, 1000, 1000),
            chunk(100, 100, 1000, 1000),
            chunk(200, 100, 1000, 1000),
        ];
        assert_eq!(resume_at(&records, &Some((key(&records[0]), 0))), Some(1));
        assert_eq!(resume_at(&records, &Some((key(&records[1]), 1))), Some(2));
    }
}

/// A store as JSON, for `info --json`. The SAME object `list --json` emits
/// per row — see `store_json` for why there is no second shape. `forest` is
/// absent here because `info` did not reach the store through one.
fn store_value(
    s: &StoreSummary,
    dir: &Path,
    path: &Path,
    handle: &str,
    kind: crate::store_json::Kind,
    bundle_bytes: Option<u64>,
) -> serde_json::Value {
    let loc = crate::store_json::Location {
        forest: None,
        handle: handle.to_string(),
        dir: dir.display().to_string(),
        path: path.display().to_string(),
        kind,
        bundle_bytes,
    };
    serde_json::to_value(crate::store_json::Store::new(s, &loc)).unwrap_or(serde_json::Value::Null)
}

/// The store's declared id, as bytes for a record field. Absent for a
/// store with no manifest, which a plain `append` writes.
fn store_id_of(h: &SourceHandle) -> Option<Vec<u8>> {
    h.bark
        .as_ref()
        .and_then(|b| b.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.as_bytes().to_vec())
}

/// What has left this store over its life, uncompressed. Added to a
/// chunk's own offset it gives a position on the store's endless TAPE,
/// which retention cannot move: `remove_head` rebases the chunk offsets
/// down by exactly what it grows this by.
///
/// ⚠ A FLOOR on a store head-dropped before these counters existed, so
/// its offsets start from an origin that is not its first ever byte.
/// Harmless: the understatement is a constant, every later drop is
/// counted, and a position only ever has to be comparable with others
/// from the same store.
fn dropped_bytes_of(input: &Path) -> u64 {
    let Ok((dir, name)) = resolve_backing(input) else {
        return 0;
    };
    std::fs::File::open(format::rings_path(&dir, &name))
        .and_then(|f| format::read_header_dropped(&f))
        .map(|d| d.uncomp_bytes)
        .unwrap_or(0)
}

#[cfg(test)]
mod chunk_stream_tests {
    use super::*;
    use crate::store::{Config, Store};

    /// A store with several chunks, and the lines that went into it.
    pub(super) fn store_with_chunks(tag: &str, lines: usize) -> (std::path::PathBuf, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("timberfs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            chunk_size: 4096,
            level: 3,
            flush_age_ms: 60_000,
        };
        let mut st = Store::open(&dir, cfg).unwrap();
        st.create("app.log").unwrap();
        crate::bark::ensure_identified(&dir, "app.log").unwrap();
        let f = st.files.get_mut("app.log").unwrap();
        let mut content = Vec::new();
        for i in 0..lines {
            let line = format!("2026-08-28T10:00:00Z INFO worker {i} handled a request\n");
            f.append_stamped(line.as_bytes(), 1_000_000 + i as u64, &cfg)
                .unwrap();
            content.extend_from_slice(line.as_bytes());
        }
        f.flush_chunk(&cfg).unwrap();
        (dir, content)
    }

    /// Read the stream the way `timberfs-records(5)` says it must be read:
    /// header to the first NUL, then exactly `len` bytes. Splitting on the
    /// delimiters instead lands INSIDE a zstd frame, which contains 0x1e
    /// and 0x00 like any other bytes — the reason `len` is authoritative
    /// and not merely convenient.
    pub(super) fn records(
        buf: &[u8],
    ) -> Vec<(String, std::collections::HashMap<String, String>, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            assert_eq!(buf[i], 0x1e, "record does not start with RS at {i}");
            let end = i + buf[i..]
                .iter()
                .position(|&b| b == 0)
                .expect("unterminated header");
            let header = String::from_utf8(buf[i + 1..end].to_vec()).unwrap();
            let mut parts = header.split('\x1f');
            let kind = parts.next().unwrap().to_string();
            let fields: std::collections::HashMap<String, String> = parts
                .filter_map(|p| {
                    p.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect();
            i = end + 1;
            let payload = match fields.get("len") {
                Some(n) => {
                    let n: usize = n.parse().unwrap();
                    let p = buf[i..i + n].to_vec();
                    assert_eq!(buf[i + n], 0, "payload not closed by NUL");
                    i += n + 1;
                    p
                }
                None => Vec::new(),
            };
            out.push((kind, fields, payload));
        }
        out
    }

    #[test]
    fn chunks_are_shipped_as_stored_and_each_carries_its_ring() {
        // `kind: "chunks"` promised "compressed, verbatim — nothing
        // decompressed at either end" and shipped the DECOMPRESSED
        // contents, with no chunk boundaries: 502,893 bytes for 23,834
        // stored, and a consumer that could not tell where one chunk
        // ended, which number it was, or what window it covered.
        let (dir, content) = store_with_chunks("chunkstream", 3000);
        let mut buf = Vec::new();
        write_chunks_framed(
            &mut buf,
            &[dir.join("app.log")],
            0,
            u64::MAX,
            None, // from_chunk
            &[],
            &[],
            None,
            Default::default(),
            &Budget::Unbounded,
        )
        .unwrap();
        let recs = records(&buf);

        assert_eq!(recs.first().map(|r| r.0.as_str()), Some("stream-start"));
        assert_eq!(recs.last().map(|r| r.0.as_str()), Some("stream-end"));
        assert_eq!(recs.last().unwrap().1["status"], "exhausted");

        let chunks: Vec<_> = recs.iter().filter(|r| r.0 == "chunk").collect();
        assert!(
            chunks.len() > 1,
            "expected several chunks, got {}",
            chunks.len()
        );
        assert_eq!(
            recs.last().unwrap().1["chunks"],
            chunks.len().to_string(),
            "stream-end must count what it actually sent"
        );

        let mut shipped = 0usize;
        let mut rebuilt = Vec::new();
        let mut expect_at = 0u64;
        for (n, (_, f, payload)) in chunks.iter().enumerate() {
            // Compressed, and compressed as a STANDARD zstd frame: the
            // magic is what makes the answer decodable by a consumer that
            // does not link our zstd.
            assert_eq!(
                &payload[..4],
                &[0x28, 0xb5, 0x2f, 0xfd],
                "chunk {n} is not a zstd frame — it is being decompressed on the way out"
            );
            let plain = zstd::stream::decode_all(&payload[..]).unwrap();
            // The ring, which is what makes the bytes usable at all.
            assert_eq!(plain.len().to_string(), f["uncomp_len"]);
            assert_eq!(f["chunk"], n.to_string());
            assert_eq!(
                f["uncomp_start"],
                expect_at.to_string(),
                "offsets not dense"
            );
            assert!(
                f.contains_key("id") && f.contains_key("wf") && f.contains_key("wl"),
                "{f:?}"
            );
            assert!(f["wf"].parse::<u64>().unwrap() <= f["wl"].parse::<u64>().unwrap());
            expect_at += plain.len() as u64;
            shipped += payload.len();
            rebuilt.extend_from_slice(&plain);
        }
        // Reassembled by uncomp_start, the chunks ARE the log.
        assert_eq!(rebuilt, content);
        assert!(
            shipped * 4 < content.len(),
            "shipped {shipped} for {} of content — not the stored bytes",
            content.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole operation a pager performs: one chunk, named by number,
    /// compressed and framed. It is `view`'s entire timberfs side, and
    /// what it needed was a refusal relaxed rather than a read path
    /// written — the chunk carries its own number back, so a client that
    /// seeks somewhere can see where it landed.
    #[test]
    fn a_chunk_number_and_a_cap_of_one_is_a_seek() {
        let (dir, _) = store_with_chunks("chunkseek", 3000);
        let mut all = Vec::new();
        write_chunks_framed(
            &mut all,
            &[dir.join("app.log")],
            0,
            u64::MAX,
            None, // from_chunk
            &[],
            &[],
            None,
            Default::default(),
            &Budget::Unbounded,
        )
        .unwrap();
        let numbers: Vec<u64> = records(&all)
            .iter()
            .filter(|r| r.0 == "chunk")
            .map(|r| r.1["chunk"].parse().unwrap())
            .collect();
        assert!(numbers.len() > 2, "expected several chunks: {numbers:?}");

        let target = numbers[1];
        let mut one = Vec::new();
        write_chunks_framed(
            &mut one,
            &[dir.join("app.log")],
            0,
            u64::MAX,
            Some(target),
            &[],
            &[],
            Some(1),
            Default::default(),
            &Budget::Unbounded,
        )
        .unwrap();
        let recs = records(&one);
        let got: Vec<&(String, std::collections::HashMap<String, String>, Vec<u8>)> =
            recs.iter().filter(|r| r.0 == "chunk").collect();
        assert_eq!(got.len(), 1, "a cap of one chunk is one chunk");
        assert_eq!(got[0].1["chunk"], target.to_string());
        // The frame is the stored one, not a re-compression: byte-identical
        // to what the unseeked read shipped for the same number.
        let whole = records(&all);
        let same = whole
            .iter()
            .find(|r| r.0 == "chunk" && r.1["chunk"] == target.to_string())
            .unwrap();
        assert_eq!(got[0].2, same.2);
        // The source line still counts the store, so a seek that lands
        // nowhere is told apart from a store that holds nothing.
        let src = recs.iter().find(|r| r.0 == "source").unwrap();
        assert_eq!(
            src.1["kept"].parse::<usize>().unwrap(),
            numbers.len() - 1,
            "the seek selected the tail of the store, and the cap sent one of it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_capped_chunk_read_says_which_bound_stopped_it() {
        // A short answer that cannot be told from a complete one is the
        // defect this whole framing exists to remove.
        let (dir, _) = store_with_chunks("chunkcap", 3000);
        let mut buf = Vec::new();
        write_chunks_framed(
            &mut buf,
            &[dir.join("app.log")],
            0,
            u64::MAX,
            None, // from_chunk
            &[],
            &[],
            Some(2),
            Default::default(),
            &Budget::Unbounded,
        )
        .unwrap();
        let recs = records(&buf);
        let end = &recs.last().unwrap().1;
        assert_eq!(recs.iter().filter(|r| r.0 == "chunk").count(), 2);
        assert_eq!(end["status"], "limited");
        assert_eq!(end["limit"], "max.chunks");
        assert!(
            end["chunks_total"].parse::<usize>().unwrap() > 2,
            "the total must say how much was NOT sent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod paging_tests {
    use super::chunk_stream_tests::records;
    use super::*;
    use crate::store::{Config, Store};

    /// A store whose entries are the lines given, each stamped, and each
    /// sealed into its own chunk at the write time given. Two kinds of
    /// control, both needed here: the STAMP makes every line its own entry
    /// (an unstamped one is a continuation of the line before), and the
    /// per-chunk write time is what lets two stores' windows be
    /// interleaved on purpose.
    pub(super) fn store_of(tag: &str, lines: &[(&str, u64)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("timberfs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            chunk_size: 1 << 20,
            level: 3,
            flush_age_ms: 60_000,
        };
        let mut st = Store::open(&dir, cfg).unwrap();
        st.create("app.log").unwrap();
        crate::bark::ensure_identified(&dir, "app.log").unwrap();
        let f = st.files.get_mut("app.log").unwrap();
        for (i, (text, write_ms)) in lines.iter().enumerate() {
            let line = format!("2026-08-28T10:{:02}:{:02}Z {text}\n", i / 60, i % 60);
            f.append_stamped(line.as_bytes(), *write_ms, &cfg).unwrap();
            f.flush_chunk(&cfg).unwrap();
        }
        dir
    }

    fn read(
        files: &[std::path::PathBuf],
        cursor: &std::collections::BTreeMap<String, u64>,
        max: Option<u64>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        query_entries(
            &mut buf,
            files,
            0,
            u64::MAX,
            None,
            false,
            &[],
            &[],
            None,
            None,
            cursor,
            false,
            false,
            false,
            true, // records
            max,
            Default::default(),
            &Budget::Unbounded,
        )
        .unwrap();
        buf
    }

    fn bodies(buf: &[u8]) -> Vec<String> {
        records(buf)
            .into_iter()
            .filter(|r| r.0 == "entry")
            .map(|r| {
                let line = String::from_utf8_lossy(&r.2);
                line.trim()
                    .split_once(' ')
                    .map_or(String::new(), |(_, b)| b.to_string())
            })
            .collect()
    }

    /// The cursor a client hands back: VERBATIM, whatever the answer said.
    /// A position with no offset is dropped exactly as the format says —
    /// which is the behaviour that used to lose a quiet store.
    fn cursor_of(buf: &[u8]) -> std::collections::BTreeMap<String, u64> {
        records(buf)
            .into_iter()
            .filter(|r| r.0 == "position")
            .filter_map(|r| Some((r.1.get("id")?.clone(), r.1.get("offset")?.parse().ok()?)))
            .collect()
    }

    /// `--max` is an exact hard cap, not "about this many". It counts
    /// ENTRIES, so it has to survive a chunk boundary landing mid-cap —
    /// which is why the store here spreads its entries over several chunks
    /// rather than holding them in one.
    #[test]
    fn a_max_is_an_exact_cap_on_entries() {
        let lines: Vec<(String, u64)> = (1..=40).map(|i| (format!("line {i}"), i)).collect();
        let refs: Vec<(&str, u64)> = lines.iter().map(|(t, w)| (t.as_str(), *w)).collect();
        let d = store_of("maxcap", &refs);
        let files = [d.join("app.log")];
        assert_eq!(bodies(&read(&files, &Default::default(), None)).len(), 40);
        for cap in [1, 5, 39, 40, 41] {
            let got = bodies(&read(&files, &Default::default(), Some(cap)));
            assert_eq!(
                got.len(),
                (cap as usize).min(40),
                "a cap of {cap} was not exact"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The fleet view: many logs read as one, interleaved by time and
    /// attributed per line. Attribution lives in the filename, and every
    /// output line gets exactly one prefix even where a chunk boundary
    /// splits a line.
    #[test]
    fn the_text_fleet_view_interleaves_and_attributes_every_line() {
        let a = store_of("fleeta", &[("alpha one", 1_000), ("alpha two", 3_000)]);
        let b = store_of("fleetb", &[("beta one", 2_000), ("beta boom", 4_000)]);
        let files = [a.join("app.log"), b.join("app.log")];
        let mut out = Vec::new();
        write_multi(
            &mut out,
            &files,
            0,
            u64::MAX,
            None,
            &[],
            &[],
            false, // no_filename: attribute every line
            None,
            &Budget::Unbounded,
        )
        .unwrap();
        let text = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(
            lines.iter().all(|l| l.contains("app.log:")),
            "every line carries exactly one prefix: {text}"
        );
        // Interleaved by write time: the two stores alternate.
        let who: Vec<bool> = lines.iter().map(|l| l.contains("fleeta")).collect();
        assert_eq!(who, vec![true, false, true, false], "{text}");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// A framed answer claims order WITHIN a store and none between, so a
    /// store's entries come back contiguous. Built so the two rules
    /// disagree: the write windows alternate, so a read that interleaved
    /// them would emit a b a b.
    #[test]
    fn a_framed_answer_reads_stores_one_after_another() {
        let a = store_of("seqa", &[("seq a1", 1_000), ("seq a2", 3_000)]);
        let b = store_of("seqb", &[("seq b1", 2_000), ("seq b2", 4_000)]);
        let files = [a.join("app.log"), b.join("app.log")];
        let buf = read(&files, &Default::default(), None);
        let got = bodies(&buf);
        assert_eq!(got.len(), 4, "{got:?}");
        let runs: Vec<char> = got
            .iter()
            .map(|e| e.chars().nth(4).unwrap()) // "seq a1" -> 'a'
            .fold(Vec::new(), |mut acc, c| {
                if acc.last() != Some(&c) {
                    acc.push(c);
                }
                acc
            });
        assert_eq!(runs, vec!['a', 'b'], "stores interleaved: {got:?}");
        // ...and the answer SAYS so, rather than leaving it to be inferred.
        let start = &records(&buf)[0];
        assert_eq!(start.0, "stream-start");
        assert_eq!(start.1.get("order"), Some(&"sequential".to_string()));

        // The TEXT fleet view still interleaves: it makes many logs
        // readable as one, and has no next page to contradict.
        let mut text = Vec::new();
        write_multi(
            &mut text,
            &files,
            0,
            u64::MAX,
            None,
            &[],
            &[],
            true, // no_filename
            None,
            &Budget::Unbounded,
        )
        .unwrap();
        let order: String = String::from_utf8_lossy(&text)
            .lines()
            .filter_map(|l| l.split_whitespace().last().and_then(|w| w.chars().next()))
            .collect();
        assert_eq!(order, "abab", "the text view stopped interleaving");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// The defect: a store that delivered nothing on a page reported a
    /// `position` with NO offset, and an offsetless cursor entry IS the
    /// start of the window — so handing the answer back, exactly as the
    /// format says to, re-read every store that had gone quiet.
    ///
    /// Two stores of different lengths is what shows it. Stores are read
    /// one after another, so the short one is exhausted while the long one
    /// is still going, and the page after that used to re-deliver it.
    #[test]
    fn a_quiet_store_keeps_its_place_across_pages() {
        let a = store_of(
            "quieta",
            &[("quiet A entry 1", 1_000), ("quiet A entry 2", 1_100)],
        );
        let b = store_of(
            "quietb",
            &[
                ("quiet B entry 1", 1_000),
                ("quiet B entry 2", 1_100),
                ("quiet B entry 3", 1_200),
                ("quiet B entry 4", 1_300),
            ],
        );
        let files = [a.join("app.log"), b.join("app.log")];
        let mut seen: Vec<String> = Vec::new();
        let mut cursor = Default::default();
        for _ in 0..8 {
            let buf = read(&files, &cursor, Some(2));
            let got = bodies(&buf);
            let done = got.is_empty();
            seen.extend(got);
            cursor = cursor_of(&buf);
            if done {
                break;
            }
        }
        seen.sort();
        let mut once = seen.clone();
        once.dedup();
        assert_eq!(once.len(), 6, "delivered twice or lost: {seen:?}");
        assert_eq!(seen.len(), 6, "an entry came back on two pages: {seen:?}");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// Every entry once, in order, on a store whose entries ALL share a
    /// timestamp — the case that makes paging by clock lose everything
    /// sharing the last one. A position is an offset on the tape, so six
    /// entries of the same second are six distinct positions.
    #[test]
    fn paging_walks_a_result_set_a_page_at_a_time() {
        let lines: Vec<(String, u64)> = (1..=6)
            .map(|i| (format!("page entry {i}"), 1_000u64))
            .collect();
        let refs: Vec<(&str, u64)> = lines.iter().map(|(t, w)| (t.as_str(), *w)).collect();
        let d = store_of("pagesame", &refs);
        let files = [d.join("app.log")];
        let mut seen: Vec<String> = Vec::new();
        let mut cursor = Default::default();
        for _ in 0..8 {
            let buf = read(&files, &cursor, Some(2));
            let got = bodies(&buf);
            if got.is_empty() {
                break;
            }
            seen.extend(got);
            cursor = cursor_of(&buf);
        }
        let want: Vec<String> = (1..=6).map(|i| format!("page entry {i}")).collect();
        assert_eq!(seen, want, "every entry once, in order");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A position taken at the LIVE EDGE is honoured by the chunked read
    /// path once those bytes are in a chunk — the two paths address one
    /// tape. Without that a consumer could not resume past an entry it
    /// had been shown, which is what the burst of a tail is made of.
    #[test]
    fn a_position_taken_live_resumes_once_the_chunk_seals() {
        use crate::store::{Config, Store};
        let dir = std::env::temp_dir().join(format!("timberfs-liveseek-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            chunk_size: 1 << 20,
            level: 3,
            flush_age_ms: 60_000,
        };
        std::fs::create_dir_all(&dir).unwrap();
        // Declared BEFORE the writer opens: that is when the segment is
        // created, and the live edge is what this test is about.
        crate::bark::declare_wal(&dir, "app.log").unwrap();
        let mut st = Store::open(&dir, cfg).unwrap();
        st.create("app.log").unwrap();
        crate::bark::ensure_identified(&dir, "app.log").unwrap();
        let id = crate::bark::load(&dir, "app.log")
            .unwrap()
            .get("id")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let f = st.files.get_mut("app.log").unwrap();
        let line = |i: usize| format!("2026-08-28T10:00:0{i}Z line {i}\n");
        // Two entries, sealed into a chunk.
        for i in 0..2 {
            f.append_stamped(line(i).as_bytes(), 1_000 + i as u64, &cfg)
                .unwrap();
        }
        f.flush_chunk(&cfg).unwrap();
        // Two more, still only in the write-ahead segment: no chunk holds
        // them, and this is where a live reader would address them.
        for i in 2..4 {
            f.append_stamped(line(i).as_bytes(), 1_000 + i as u64, &cfg)
                .unwrap();
        }
        // What makes them readable at all: the segment is buffered until
        // a sync, which is the writer's once-a-second tick.
        f.sap_sync().unwrap();
        let files = [dir.join("app.log")];
        // What a live reader sees, and where it says those bytes sit —
        // the reader's own answer, not this test's arithmetic.
        let mut tail = crate::live::LiveTail::open(&dir, "app.log", false);
        let (live_at, live) = tail.poll().unwrap();
        assert_eq!(live.len(), 2, "the entries no chunk holds yet");
        assert_eq!(live_at, line(0).len() as u64 * 2, "the tape end");
        // Just past the FIRST of them: what a consumer shown that entry
        // would hand back.
        let at = live_at + live[0].payload.len() as u64;

        f.flush_chunk(&cfg).unwrap();
        drop(st);
        let cursor = std::collections::BTreeMap::from([(id, at)]);
        assert_eq!(
            bodies(&read(&files, &cursor, None)),
            vec!["line 3".to_string()],
            "the chunked path resumed exactly where the live edge left off"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store with a live segment, its `lines` in chunks and its
    /// `live` ones only in the sap. Returns the store id and the tape
    /// offset the chunks end at.
    fn store_with_a_live_edge(
        tag: &str,
        lines: &[&str],
        live: &[&str],
    ) -> (std::path::PathBuf, String, u64) {
        use crate::store::{Config, Store};
        let dir = std::env::temp_dir().join(format!("timberfs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            chunk_size: 1 << 20,
            level: 3,
            flush_age_ms: 60_000,
        };
        // Declared BEFORE the writer opens: that is when the segment is
        // created.
        crate::bark::declare_wal(&dir, "app.log").unwrap();
        let mut st = Store::open(&dir, cfg).unwrap();
        st.create("app.log").unwrap();
        crate::bark::ensure_identified(&dir, "app.log").unwrap();
        let id = crate::bark::load(&dir, "app.log")
            .unwrap()
            .get("id")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let f = st.files.get_mut("app.log").unwrap();
        let mut ms = 1_000u64;
        for text in lines {
            f.append_stamped(text.as_bytes(), ms, &cfg).unwrap();
            ms += 1;
        }
        f.flush_chunk(&cfg).unwrap();
        let sealed = lines.iter().map(|l| l.len() as u64).sum();
        for text in live {
            f.append_stamped(text.as_bytes(), ms, &cfg).unwrap();
            ms += 1;
        }
        // Buffered until a sync: that tick is what makes them readable.
        f.sap_sync().unwrap();
        drop(st);
        (dir, id, sealed)
    }

    fn stamped(n: usize) -> String {
        format!("2026-08-28T10:00:{n:02}Z line {n}\n")
    }

    /// A read that RESUMES also gives the live edge: those entries are
    /// durable and readable, and a consumer following the store would
    /// otherwise wait out the writer's flush age to be told about them.
    /// They carry an address and no chunk, and the position the answer
    /// reports is past them — so the next poll does not repeat them.
    #[test]
    fn a_resumed_read_gives_the_live_edge_too() {
        let all: Vec<String> = (0..4).map(stamped).collect();
        let text: Vec<&str> = all.iter().map(String::as_str).collect();
        let (d, id, end) = store_with_a_live_edge("liveresume", &text[..2], &text[2..]);
        let files = [d.join("app.log")];

        let at_the_end = std::collections::BTreeMap::from([(id.clone(), end)]);
        let buf = read(&files, &at_the_end, None);
        assert_eq!(
            bodies(&buf),
            vec!["line 2".to_string(), "line 3".to_string()],
            "the entries no chunk holds yet"
        );
        let recs = records(&buf);
        let live: Vec<_> = recs.iter().filter(|r| r.0 == "entry").collect();
        assert!(
            live.iter().all(|r| r.1.contains_key("offset")),
            "a live entry states where it sits"
        );
        assert!(
            live.iter().all(|r| !r.1.contains_key("chunk")),
            "...and names no chunk, there being none"
        );
        let after: std::collections::BTreeMap<String, u64> = cursor_of(&buf);
        assert_eq!(
            after.get(&id).copied(),
            Some(end + all[2].len() as u64 + all[3].len() as u64),
            "the position is past what was delivered"
        );
        // Handing that back delivers nothing twice.
        assert!(bodies(&read(&files, &after, None)).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Resuming INSIDE the live edge: the entry the position lands on and
    /// everything after it, once.
    #[test]
    fn a_position_inside_the_live_edge_resumes_there() {
        let all: Vec<String> = (0..4).map(stamped).collect();
        let text: Vec<&str> = all.iter().map(String::as_str).collect();
        let (d, id, end) = store_with_a_live_edge("livemid", &text[..2], &text[2..]);
        let files = [d.join("app.log")];
        let mid = std::collections::BTreeMap::from([(id, end + all[2].len() as u64)]);
        assert_eq!(
            bodies(&read(&files, &mid, None)),
            vec!["line 3".to_string()]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A WINDOWED read is a question about the past, and the live edge is
    /// not in it — unchanged, and deliberately: `--from`/`--until` select
    /// chunks by their write window, and the segment has none.
    #[test]
    fn a_windowed_read_still_stops_at_the_chunks() {
        let all: Vec<String> = (0..4).map(stamped).collect();
        let text: Vec<&str> = all.iter().map(String::as_str).collect();
        let (d, _id, _end) = store_with_a_live_edge("livewindow", &text[..2], &text[2..]);
        let files = [d.join("app.log")];
        let mut buf = Vec::new();
        query_entries(
            &mut buf,
            &files,
            0,
            u64::MAX,
            None,
            true, // windowed
            &[],
            &[],
            None,
            None,
            &Default::default(),
            true,
            false,
            false,
            true,
            None,
            Default::default(),
            &Budget::Unbounded,
        )
        .unwrap();
        assert_eq!(
            bodies(&buf),
            vec!["line 0".to_string(), "line 1".to_string()],
            "the chunks, and nothing the segment holds"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The segment is appended to the chunks ONLY where it begins where
    /// they end. A flush landing between the ring snapshot and the sap
    /// read leaves the bytes in between in a chunk this answer never saw,
    /// and delivering the segment anyway would report a position past
    /// them — a gap nothing downstream could detect.
    #[test]
    fn a_segment_that_does_not_follow_the_chunks_is_left_alone() {
        let chunk = |uncomp_start: u64, len: u64| ChunkRecord {
            uncomp_start,
            uncomp_len: len,
            comp_start: 0,
            comp_len: len / 2,
            first_write_ms: 1,
            last_write_ms: 2,
            seq: uncomp_start / len.max(1),
        };
        let held = [chunk(0, 100), chunk(100, 100)];
        assert!(live_follows_the_chunks(200, 0, &held));
        assert!(!live_follows_the_chunks(300, 0, &held), "a flush landed");
        assert!(!live_follows_the_chunks(150, 0, &held), "overlaps a chunk");
        // What left the store is part of the address, on both sides.
        assert!(live_follows_the_chunks(900, 700, &held));
        assert!(live_follows_the_chunks(0, 0, &[]), "nothing flushed yet");
    }

    /// A resumed read does not open what it has already delivered. A
    /// following read carries no window, so its selection is every chunk
    /// the store has and the POSITION is the only thing narrowing it —
    /// checked by SCRIBBLING over the compressed bytes below the cursor,
    /// which a read that decompressed them could not survive.
    #[test]
    fn a_cursor_skips_what_is_below_it_without_decompressing_it() {
        use std::io::Write;
        let lines: Vec<(String, u64)> = (1..=20).map(|i| (format!("line {i}"), i)).collect();
        let refs: Vec<(&str, u64)> = lines.iter().map(|(t, w)| (t.as_str(), *w)).collect();
        let d = store_of("cursorskip", &refs);
        let files = [d.join("app.log")];
        let cursor = cursor_of(&read(&files, &Default::default(), Some(10)));
        assert!(!cursor.is_empty(), "no position to resume from");

        let cut = open_source(&files[0]).unwrap().records[10].comp_start;
        assert!(cut > 0, "nothing below the cursor to scribble over");
        let mut trunk = std::fs::OpenOptions::new()
            .write(true)
            .open(crate::format::trunk_path(&d, "app.log"))
            .unwrap();
        trunk.write_all(&vec![0xff; cut as usize]).unwrap();
        trunk.sync_all().unwrap();

        let want: Vec<String> = (11..=20).map(|i| format!("line {i}")).collect();
        assert_eq!(bodies(&read(&files, &cursor, None)), want);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::chunk_stream_tests::{records, store_with_chunks};
    use super::*;

    /// Three stores, a records read, and whatever budget is handed in.
    fn read(files: &[std::path::PathBuf], budget: &Budget) -> Vec<u8> {
        let mut buf = Vec::new();
        query_entries(
            &mut buf,
            files,
            0,
            u64::MAX,
            None,  // from_chunk
            false, // windowed
            &[],
            &[],
            None,
            None,
            &Default::default(),
            false, // no_filename: several stores, so they are labelled
            false, // show_write_time
            false, // null_sep
            true,  // records
            None,
            Default::default(),
            budget,
        )
        .unwrap();
        buf
    }

    fn field<'a>(
        recs: &'a [(String, std::collections::HashMap<String, String>, Vec<u8>)],
        kind: &str,
        key: &str,
    ) -> Option<&'a String> {
        recs.iter().find(|r| r.0 == kind).and_then(|r| r.1.get(key))
    }

    /// A deadline that cannot fire must not, or every other assertion about
    /// one is passing for the wrong reason.
    #[test]
    fn a_budget_with_time_left_does_not_stop_the_read() {
        let (dir, _) = store_with_chunks("dlwhole", 400);
        let files = [dir.join("app.log")];
        let roomy = Budget::Wall {
            start: std::time::Instant::now(),
            limit: std::time::Duration::from_secs(600),
        };
        let recs = records(&read(&files, &roomy));
        assert_eq!(
            field(&recs, "stream-end", "status"),
            Some(&"exhausted".into())
        );
        assert_eq!(field(&recs, "stream-end", "entries"), Some(&"400".into()));
        assert_eq!(
            field(&recs, "stream-end", "limit"),
            None,
            "nothing stopped it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An expired budget NAMES itself. It used to borrow an entry cap's
    /// name, so "your limit stopped me" pointed at the wrong bound to
    /// raise. Asserted against a budget that is out of time by
    /// construction — the VM test bet a 1 ms deadline against ~300 chunks
    /// measured at 10–20 ms, which is a wager on the runner, not a
    /// property of the code.
    #[test]
    fn an_expired_budget_stops_the_read_and_says_so() {
        let (dir, _) = store_with_chunks("dlfires", 400);
        let files = [dir.join("app.log")];
        let spent = Budget::OnAsk {
            asks: std::cell::Cell::new(0),
            fire_on: 1,
        };
        let recs = records(&read(&files, &spent));
        assert_eq!(
            field(&recs, "stream-end", "status"),
            Some(&"limited".into())
        );
        assert_eq!(
            field(&recs, "stream-end", "limit"),
            Some(&"deadline".into())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The STAIRCASE: a bounded read takes stores one after another, so
    /// when the budget runs out the stores before it are whole, the one it
    /// landed in is partial, and the ones after it were never opened —
    /// `chunks_read=0` beside a non-zero `chunks_selected`. This is the
    /// shape a client needs to resume correctly, and the reason the budget
    /// has to be able to expire PART WAY: with one that fires immediately
    /// the property holds trivially at store one and proves nothing.
    #[test]
    fn the_stores_after_the_one_it_stopped_in_were_never_opened() {
        let dirs: Vec<_> = ["dlstair1", "dlstair2", "dlstair3"]
            .iter()
            .map(|t| store_with_chunks(t, 400).0)
            .collect();
        let files: Vec<_> = dirs.iter().map(|d| d.join("app.log")).collect();
        // The budget is asked once per store as the read opens it, then
        // once per chunk. Three stores of six chunks, so firing on the
        // 12th ask lands inside the SECOND store: 3 + 6 (store one, whole)
        // + 2, and the 9th chunk-ask stops it. The assertions below check
        // that placement rather than trusting it — a budget that fired too
        // early or too late fails them, loudly, instead of satisfying the
        // staircase trivially.
        let recs = records(&read(
            &files,
            &Budget::OnAsk {
                asks: std::cell::Cell::new(0),
                fire_on: 12,
            },
        ));
        let steps: Vec<(u64, u64)> = recs
            .iter()
            .filter(|r| r.0 == "position")
            .map(|r| {
                (
                    r.1["chunks_read"].parse().unwrap(),
                    r.1["chunks_selected"].parse().unwrap(),
                )
            })
            .collect();
        assert_eq!(steps.len(), 3, "a position per store EXAMINED: {steps:?}");
        let stopped_in = steps
            .iter()
            .position(|(read, selected)| *read > 0 && read < selected)
            .unwrap_or_else(|| panic!("the budget stopped inside no store: {steps:?}"));
        // Both ends must exist, or the staircase is not being tested: with
        // nothing before it there is no "whole" case, and with nothing
        // after it there is no "never opened" case.
        assert!(
            stopped_in > 0 && stopped_in < steps.len() - 1,
            "the budget fired at the edge of the fleet, so this proves nothing: {steps:?}"
        );
        for (read, selected) in &steps[..stopped_in] {
            assert_eq!(
                read, selected,
                "a store before the stop is whole: {steps:?}"
            );
        }
        for (read, selected) in &steps[stopped_in + 1..] {
            assert_eq!(*read, 0, "a store after the stop was opened: {steps:?}");
            assert!(*selected > 0, "...and it had been selected: {steps:?}");
        }
        assert_eq!(
            field(&recs, "stream-end", "limit"),
            Some(&"deadline".into())
        );
        for d in &dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Both refusals are the query's, not the command's, so they hold for
    /// a document exactly as for the flags.
    #[test]
    fn a_deadline_of_zero_and_a_deadline_on_a_follow_are_refused() {
        let mut zero = Query::default();
        zero.limit.deadline_ms = Some(0);
        let e = zero.validate().unwrap_err().to_string();
        assert!(e.contains("zero"), "{e}");

        let mut following = Query::default();
        following.limit.deadline_ms = Some(5_000);
        following.follow.follow = true;
        assert!(
            following.validate().is_err(),
            "a follow has no end to bound"
        );
    }
}

#[cfg(test)]
mod served_bytes_tests {
    use super::chunk_stream_tests::{records, store_with_chunks};
    use super::*;

    /// The same seek in the ENTRY pipeline, which reaches its chunks
    /// through the same selection — so what this checks is the half a
    /// chunk dump has no way to: that the answer SAYS where it began.
    /// `stream-start` echoes the selection so a stored answer describes
    /// the search that produced it, and a position left out of that echo
    /// makes a read from chunk 412,000 indistinguishable from one that
    /// started at the store's floor.
    #[test]
    fn a_records_answer_seeks_and_says_it_did() {
        let (dir, _) = store_with_chunks("seekrecords", 3000);
        let read = |from_chunk: Option<u64>| {
            let mut buf = Vec::new();
            query_entries(
                &mut buf,
                &[dir.join("app.log")],
                0,
                u64::MAX,
                from_chunk,
                false,
                &[],
                &[],
                None,
                None,
                &Default::default(),
                true,  // no_filename
                false, // show_write_time
                false, // null_sep
                true,  // records
                None,
                Default::default(),
                &Budget::Unbounded,
            )
            .unwrap();
            buf
        };
        let entry_chunks = |buf: &[u8]| -> Vec<u64> {
            records(buf)
                .iter()
                .filter(|r| r.0 == "entry")
                .filter_map(|r| r.1.get("chunk").map(|c| c.parse().unwrap()))
                .collect()
        };
        let whole = read(None);
        let all = entry_chunks(&whole);
        let last = *all.last().unwrap();
        assert!(last > 1, "expected several chunks, ended at {last}");

        let seeked = read(Some(last));
        assert_eq!(
            entry_chunks(&seeked)
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([last]),
            "a seek to the last chunk answers with that chunk and nothing before it"
        );
        let start = records(&seeked).into_iter().next().unwrap();
        assert_eq!(start.0, "stream-start");
        assert_eq!(start.1.get("from_chunk"), Some(&last.to_string()));
        assert_eq!(
            records(&whole)[0].1.get("from_chunk"),
            None,
            "a read that did not seek must not claim to have"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every record that serves bytes says where they sit: `offset` is
    /// where the run begins and `len` how long it is, so the record
    /// states BOTH ends of what it handed over. The runs then chain —
    /// the end of one entry is the start of the next, and the end of the
    /// last is the `position` the store reports — so a consumer can
    /// CHECK that what arrived is contiguous instead of trusting it.
    #[test]
    fn every_served_run_of_bytes_says_where_it_sits_and_they_chain() {
        let (dir, content) = store_with_chunks("servedbytes", 400);
        let mut buf = Vec::new();
        query_entries(
            &mut buf,
            &[dir.join("app.log")],
            0,
            u64::MAX,
            None, // from_chunk
            false,
            &[],
            &[],
            None,
            None,
            &Default::default(),
            true,  // no_filename
            false, // show_write_time
            false, // null_sep
            true,  // records
            None,
            Default::default(),
            &Budget::Unbounded,
        )
        .unwrap();

        let (mut at, mut entries, mut position, mut i) = (0u64, 0, None, 0);
        while i < buf.len() {
            assert_eq!(buf[i], 0x1e);
            let end = i + buf[i..].iter().position(|&b| b == 0).unwrap();
            let header = String::from_utf8(buf[i + 1..end].to_vec()).unwrap();
            let mut parts = header.split('\x1f');
            let kind = parts.next().unwrap();
            let f: std::collections::HashMap<&str, &str> =
                parts.filter_map(|p| p.split_once('=')).collect();
            i = end + 1;
            if kind == "position" {
                position = f.get("offset").map(|v| v.parse::<u64>().unwrap());
            }
            if let Some(n) = f.get("len") {
                let n: usize = n.parse().unwrap();
                if kind == "entry" {
                    let off: u64 = f["offset"].parse().unwrap();
                    assert_eq!(off, at, "entry {entries} does not follow the one before");
                    // What the record CLAIMS about its bytes, checked
                    // against the bytes it served.
                    assert_eq!(&buf[i..i + n], &content[off as usize..off as usize + n]);
                    at = off + n as u64;
                    entries += 1;
                }
                i += n + 1;
            }
        }
        assert_eq!(entries, 400);
        assert_eq!(at, content.len() as u64, "the runs must cover the store");
        assert_eq!(
            position,
            Some(at),
            "the position must be where the served bytes ended"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
