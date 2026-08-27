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
pub fn select_chunks(
    file: &Path,
    chunks: &[ChunkRecord],
    seq_at_open: Option<u64>,
    from_ms: u64,
    to_ms: u64,
    has: &[String],
    any_of: &[String],
) -> anyhow::Result<(Vec<(usize, ChunkRecord)>, usize)> {
    let mut selected: Vec<(usize, ChunkRecord)> = chunks
        .iter()
        .enumerate()
        .filter(|(_, c)| c.last_write_ms >= from_ms && c.first_write_ms <= to_ms)
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
    /// A resume position by chunk NUMBER: exact, where a timestamp can
    /// match two chunks sharing a boundary millisecond.
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
    /// parsing and no logline filtering.
    pub by_write_time: bool,
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
        let following = self.follow.follow || self.limit.tail.is_some();
        if self.window.from_chunk.is_some() && !following {
            bail!(
                "a chunk number is a resume position, and only a FOLLOWING read moves \
                 forward from one — a windowed query selects by the timestamps the \
                 lines carry"
            );
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
    // The index selects chunks; these judge the entries inside them.
    // Absent, the read stays chunk-granular — every entry of every chunk
    // the index let through comes out.
    let entry_preds = q.matching.entry_preds()?;
    let (max, tail) = (q.limit.max, q.limit.tail);
    let (max_chunks, tail_chunks) = (q.limit.max_chunks, q.limit.tail_chunks);
    let (follow, poll) = (q.follow.follow, q.follow.poll);
    let Output {
        no_filename,
        show_write_time,
        null_sep,
        records,
        by_write_time,
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
        return query_entries(
            files,
            from_ms,
            to_ms,
            windowed,
            has,
            any,
            entry_preds,
            max_chunks,
            no_filename,
            show_write_time,
            null_sep,
            records,
            max,
        );
    }
    if files.len() == 1 {
        return query_single(&files[0], from_ms, to_ms, has, any, max_chunks);
    }
    query_multi(files, from_ms, to_ms, has, any, no_filename, max_chunks)
}

/// The default read path: select chunks by the write-time rings (widened
/// when the logline filter can verify), then emit whole ENTRIES whose own
/// timestamps fall inside the asked window. Unfilterable stores (no
/// parseable line timestamps) fall back to the unwidened raw window with
/// a note — never both looser AND unexplained.
#[allow(clippy::too_many_arguments)]
fn query_entries(
    files: &[std::path::PathBuf],
    from_ms: u64,
    to_ms: u64,
    windowed: bool,
    has: &[String],
    any: &[String],
    entry_preds: Option<crate::grep::Preds>,
    max_chunks: Option<u64>,
    no_filename: bool,
    show_write_time: bool,
    null_sep: bool,
    records: bool,
    max: Option<u64>,
) -> anyhow::Result<()> {
    struct Src {
        path: std::path::PathBuf,
        guard: Option<(PathBuf, String)>,
        handle: SourceHandle,
        chunks: Vec<(usize, ChunkRecord)>,
        total_chunks: usize,
        pos: usize,
        sink: crate::entry::EntrySink,
    }
    let multi = files.len() > 1 && !no_filename;
    // --max: a total entry cap shared by every source's sink.
    let limit = max.map(|m| (Rc::new(Cell::new(0u64)), m));
    let mut srcs: Vec<Src> = Vec::new();
    for f in files {
        let mut source = open_source(f)?;
        let guard = seq_guard(f);
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
                has,
                any,
            )?
            .0
        } else {
            selected
        };
        let framing = crate::entry::Framing {
            null_sep,
            show_write: show_write_time,
            records,
            label: if multi {
                Some(f.display().to_string().into_bytes())
            } else {
                None
            },
        };
        srcs.push(Src {
            path: f.clone(),
            guard,
            total_chunks: source.records.len(),
            handle: source,
            chunks: selected,
            pos: 0,
            sink: crate::entry::EntrySink::new(
                extractor,
                window,
                framing,
                limit.clone(),
                &f.display().to_string(),
            )
            .with_preds(entry_preds.clone()),
        });
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    // --records brackets the stream with typed metadata: stream-start
    // carries the format version and an echo of the selection (canonical
    // ms values — downstream tools can record lineage), one source record
    // per input carries its selection stats, and stream-end (below)
    // carries totals — its PRESENCE is the completeness marker: a
    // consumer hitting EOF without it knows the stream was truncated.
    if records {
        write!(out, "\x1estream-start\x1fv=1")?;
        if from_ms > 0 {
            write!(out, "\x1ffrom={from_ms}")?;
        }
        if to_ms < u64::MAX {
            write!(out, "\x1fto={to_ms}")?;
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
    // K-way interleave by chunk write windows across files (within-file
    // order preserved), same as the raw fleet view.
    let mut chunks_out = 0u64;
    // WHICH bound stopped the read. A consumer needs the name, not just
    // the fact: "your entry cap" and "your chunk cap" are different
    // things to raise, and the answer used to say `max.entries` whatever
    // had actually fired.
    let mut stopped_by: Option<&'static str> = None;
    loop {
        let mut best: Option<usize> = None;
        for (i, s) in srcs.iter().enumerate() {
            if s.pos < s.chunks.len() {
                let key = s.chunks[s.pos].1.first_write_ms;
                if best.is_none_or(|b: usize| key < srcs[b].chunks[srcs[b].pos].1.first_write_ms) {
                    best = Some(i);
                }
            }
        }
        let Some(i) = best else { break };
        let s = &mut srcs[i];
        let c = s.chunks[s.pos].1;
        s.pos += 1;
        let Some(data) = read_chunk(&s.path, &s.guard, &mut s.handle, c)? else {
            continue; // retained away by a race between selection and read
        };
        s.sink.push_chunk(
            &data,
            Some(c.seq),
            (c.first_write_ms, c.last_write_ms),
            &mut out,
        )?;
        // --max reached: stop decompressing further chunks.
        if let Some((count, m)) = &limit {
            if count.get() >= *m {
                stopped_by = Some("max.entries");
                break;
            }
        }
        // A chunk cap counts what was EMITTED, across sources, so a
        // fleet view of three stores capped at 5 chunks reads five, not
        // fifteen.
        chunks_out += 1;
        if max_chunks.is_some_and(|m| chunks_out >= m) {
            stopped_by = Some("max.chunks");
            break;
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
        if s.pos < s.chunks.len() {
            s.sink.discard_pending();
        }
        s.sink.finish(&mut out)?;
        emitted += s.sink.emitted;
        dropped += s.sink.filtered_out;
        // An entry the sink DROPPED because the count was already there.
        // The loop stops feeding chunks on the cap, but a chunk already
        // in flight can still hold entries past it — so this fires where
        // the loop's own check does not.
        if s.sink.suppressed > 0 {
            limited = true;
            stopped_by = stopped_by.or(Some("max.entries"));
        }
        // What was actually READ, which is how far each source advanced —
        // not how many chunks were selected for it. They differ exactly
        // when a cap stopped the loop, which is when a consumer is most
        // likely to be counting.
        read += s.pos;
        total += s.total_chunks;
    }
    if records {
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
            crate::note!("timberfs: stopped at --max {m}; more entries matched than were shown");
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
    ) -> anyhow::Result<()> {
        for e in entries {
            match sink {
                Some(s) => s.push_chunk(&e.payload, None, (e.wf, e.wl), out)?,
                None => emit_raw(out, &e.payload, label)?,
            }
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
        write!(out, "\x1estream-start\x1fv=1")?;
        if let Some(fr) = from {
            write!(out, "\x1ffrom={fr}")?;
        }
        if let Some(n) = tail {
            write!(out, "\x1ftail={n}")?;
        }
        write!(
            out,
            "\x1ffollow={}\x1fsources={}",
            u8::from(follow),
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
                    },
                    limit.clone(),
                    &f.display().to_string(),
                )
                .with_preds(entry_preds.clone()),
            )
        } else {
            None
        };
        for c in &chunks[start..] {
            if let Some(data) = read_chunk(f, &guard, &mut source, *c)? {
                match &mut sink {
                    Some(s) => s.push_chunk(
                        &data,
                        Some(c.seq),
                        (c.first_write_ms, c.last_write_ms),
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
            emit_live(&mut out, &live.poll()?, &mut sink, label.as_deref())?;
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
                let entries = s.live.poll()?;
                got |= !entries.is_empty();
                emit_live(&mut out, &entries, &mut s.sink, s.label.as_deref())?;
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

fn query_single(
    file: &Path,
    from_ms: u64,
    to_ms: u64,
    has: &[String],
    any: &[String],
    max_chunks: Option<u64>,
) -> anyhow::Result<()> {
    let mut source = open_source(file)?;
    let (selected, in_window) = select_chunks(
        file,
        &source.records,
        source.seq_at_open,
        from_ms,
        to_ms,
        has,
        any,
    )?;
    let total_chunks = source.records.len();
    let guard = seq_guard(file);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut uncomp_total = 0u64;
    // A chunk cap needs no parsing at all here: stop after N have gone
    // out. This is the path where the unit is genuinely free.
    let selected: Vec<_> = match max_chunks {
        Some(n) => selected.iter().take(n as usize).copied().collect(),
        None => selected.clone(),
    };
    for (_, c) in &selected {
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
fn query_multi(
    files: &[std::path::PathBuf],
    from_ms: u64,
    to_ms: u64,
    has: &[String],
    any: &[String],
    no_filename: bool,
    max_chunks: Option<u64>,
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

    let stdout = io::stdout();
    let mut out = stdout.lock();
    // Counted across sources: a fleet view capped at 5 chunks reads five
    // in time order, not five per store.
    let mut chunks_out = 0u64;
    loop {
        if max_chunks.is_some_and(|m| chunks_out >= m) {
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
