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
    let records =
        format::read_index(&rings).with_context(|| format!("reading index {}", rings.display()))?;
    let file = File::open(format::trunk_path(&dir, &base))
        .with_context(|| format!("opening {}", format::trunk_path(&dir, &base).display()))?;
    let bark = crate::bark::load(&dir, &base);
    Ok(SourceHandle {
        records,
        file,
        bark,
    })
}

/// Identity for the collapse-head seqlock guard (store.rs): `None` for a
/// `.timber` bundle (its trunk member is written once and never mutated
/// again, so there's nothing a reader could race), `Some(dir, name)` for
/// a live backing pair, which a concurrent writer's retention can
/// collapse out from under a standalone reader in another process.
fn seq_guard(input: &Path) -> Option<(PathBuf, String)> {
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
    mut c: ChunkRecord,
) -> anyhow::Result<Option<Vec<u8>>> {
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
            return Ok(Some(zstd::stream::decode_all(&comp[..]).with_context(
                || "decompressing a stored chunk — the .trunk may be corrupt",
            )?));
        }
        *handle = open_source(input)?;
        match handle.records.iter().find(|r| {
            r.first_write_ms == c.first_write_ms
                && r.last_write_ms == c.last_write_ms
                && r.uncomp_len == c.uncomp_len
        }) {
            Some(r) => c = *r,
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
            crate::grain::load(&crate::format::grain_path(&dir, &base)).ok()
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

#[allow(clippy::too_many_arguments)]
pub fn cmd_query(
    files: &[std::path::PathBuf],
    from: Option<u64>,
    to: Option<u64>,
    has: &[String],
    any: &[String],
    no_filename: bool,
    show_write_time: bool,
    by_write_time: bool,
    null_sep: bool,
    records: bool,
    follow: bool,
    tail: Option<u64>,
    max: Option<u64>,
) -> anyhow::Result<()> {
    let from_ms = from.unwrap_or(0);
    let to_ms = to.unwrap_or(u64::MAX);
    if from_ms > to_ms {
        bail!("--from is after --to");
    }
    // Follow / tail is its own read path: a poll loop over newly-committed
    // chunks rather than the one-shot windowed scan. --has/--any select whole
    // chunks via the offline .grain index, which neither composes with a live
    // stream (there is nothing to skip — every new chunk must be read) nor
    // filters at line granularity; filter a follow with a pipe instead.
    if follow || tail.is_some() {
        if !has.is_empty() || !any.is_empty() {
            bail!(
                "--has/--any select whole chunks via the offline index and don't compose \
                 with --follow/--tail; filter the live stream with a pipe (e.g. | grep), \
                 or run a windowed query for offline chunk-skipping"
            );
        }
        return query_follow(
            files,
            from,
            no_filename,
            show_write_time,
            null_sep,
            records,
            tail,
            follow,
            max,
        );
    }
    let windowed = from.is_some() || to.is_some();
    // The entry pipeline engages when there is something to verify
    // (a window: the DEFAULT is that every printed entry's own timestamp
    // is inside it) or when the framing needs entries (-0, annotation), or
    // a --max cap (counting entries needs entry parsing).
    // --by-write-time is the raw escape hatch: chunk dump, no parsing.
    if !by_write_time && (windowed || null_sep || show_write_time || records || max.is_some()) {
        return query_entries(
            files,
            from_ms,
            to_ms,
            windowed,
            has,
            any,
            no_filename,
            show_write_time,
            null_sep,
            records,
            max,
        );
    }
    if files.len() == 1 {
        return query_single(&files[0], from_ms, to_ms, has, any);
    }
    query_multi(files, from_ms, to_ms, has, any, no_filename)
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
            select_chunks(f, &source.records, from_ms, to_ms, has, any)?.0
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
            ),
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
        write!(out, "\x1fsources={}", files.len())?;
        out.write_all(b"\0")?;
        for (f, s) in files.iter().zip(&srcs) {
            write!(
                out,
                "\x1esource\x1fpath={}\x1fkept={}\x1ftotal={}",
                f.display(),
                s.chunks.len(),
                s.total_chunks
            )?;
            out.write_all(b"\0")?;
        }
    }
    // K-way interleave by chunk write windows across files (within-file
    // order preserved), same as the raw fleet view.
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
        s.sink
            .push_chunk(&data, (c.first_write_ms, c.last_write_ms), &mut out)?;
        // --max reached: stop decompressing further chunks.
        if let Some((count, m)) = &limit {
            if count.get() >= *m {
                break;
            }
        }
    }
    let (mut emitted, mut dropped) = (0u64, 0u64);
    let (mut read, mut total) = (0usize, 0usize);
    for s in &mut srcs {
        s.sink.finish(&mut out)?;
        emitted += s.sink.emitted;
        dropped += s.sink.filtered_out;
        read += s.chunks.len();
        total += s.total_chunks;
    }
    if records {
        write!(
            out,
            "\x1estream-end\x1fentries={emitted}\x1fdropped={dropped}\x1fchunks_read={read}\x1fchunks_total={total}"
        )?;
        out.write_all(b"\0")?;
    }
    out.flush()?;
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

/// Follow / tail: emit (about) the last N units, then — with --follow — new
/// data as chunks are committed, until interrupted. Read-only and lock-free,
/// so it runs beside a live appender; a flushed chunk is the unit of
/// visibility, so latency tracks the writer's --flush-age.
///
/// Plain text follows RAW chunk bytes (line-oriented, no buffering — the
/// snappy tail -f). Only the framed modes (-0, --records, --show-write-time)
/// run the entry pipeline, where the last entry stays buffered until the next
/// one closes it (a multiline entry can't be known complete any sooner).
#[allow(clippy::too_many_arguments)]
fn query_follow(
    files: &[std::path::PathBuf],
    from: Option<u64>,
    no_filename: bool,
    show_write_time: bool,
    null_sep: bool,
    records: bool,
    tail: Option<u64>,
    follow: bool,
    max: Option<u64>,
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

    struct FollowSrc {
        path: std::path::PathBuf,
        label: Option<Vec<u8>>,
        sink: Option<crate::entry::EntrySink>,
        // Last emitted chunk's last_write_ms; new chunks arrive later (the
        // appender stamps now()), so this is a monotonic follow cursor.
        cursor_ms: u64,
    }

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
        let mut source = open_source(f)?;
        let guard = seq_guard(f);
        let tf = crate::bark::time_format(source.bark.as_ref());
        let extractor =
            crate::import::Extractor::new(tf.regex.as_deref(), tf.format.as_deref(), tf.utc)?;
        // Owned, not borrowed from `source`: read_chunk needs `&mut
        // source` on a race, which a live borrow of source.records
        // would forbid (chunk records are Copy, so cloning is cheap).
        let chunks = source.records.clone();
        // Where to begin: an entry-count tail, a write-time --from, or (the
        // default) the current end — following only genuinely new chunks.
        let start = if let Some(n) = tail {
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
            Some(crate::entry::EntrySink::new(
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
            ))
        } else {
            None
        };
        let mut cursor_ms = 0u64;
        for c in &chunks[start..] {
            if let Some(data) = read_chunk(f, &guard, &mut source, *c)? {
                match &mut sink {
                    Some(s) => {
                        s.push_chunk(&data, (c.first_write_ms, c.last_write_ms), &mut out)?
                    }
                    None => emit_raw(&mut out, &data, label.as_deref())?,
                }
            }
            cursor_ms = cursor_ms.max(c.last_write_ms);
            if capped(&limit) {
                break;
            }
        }
        // Anchor to the latest committed chunk even when nothing was emitted
        // (from-now), so only new chunks are followed.
        if let Some(last) = chunks.last() {
            cursor_ms = cursor_ms.max(last.last_write_ms);
        }
        srcs.push(FollowSrc {
            path: f.clone(),
            label,
            sink,
            cursor_ms,
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
        std::thread::sleep(std::time::Duration::from_millis(1000));
        for s in &mut srcs {
            let mut source = match open_source(&s.path) {
                Ok(x) => x,
                Err(_) => continue, // transient (mid-rename by retention): retry
            };
            let guard = seq_guard(&s.path);
            // Owned: read_chunk needs `&mut source` on a race, which a
            // live borrow of source.records would forbid.
            let pending: Vec<ChunkRecord> = source
                .records
                .iter()
                .filter(|c| c.first_write_ms > s.cursor_ms)
                .copied()
                .collect();
            for c in pending {
                if let Some(data) = read_chunk(&s.path, &guard, &mut source, c)? {
                    match &mut s.sink {
                        Some(sink) => {
                            sink.push_chunk(&data, (c.first_write_ms, c.last_write_ms), &mut out)?
                        }
                        None => emit_raw(&mut out, &data, s.label.as_deref())?,
                    }
                }
                s.cursor_ms = c.last_write_ms.max(s.cursor_ms);
                if capped(&limit) {
                    done = true;
                    break;
                }
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
) -> anyhow::Result<()> {
    let mut source = open_source(file)?;
    let (selected, in_window) = select_chunks(file, &source.records, from_ms, to_ms, has, any)?;
    let total_chunks = source.records.len();
    let guard = seq_guard(file);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut uncomp_total = 0u64;
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
        let (selected, _) = select_chunks(f, &handle.records, from_ms, to_ms, has, any)?;
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
    loop {
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
    Active,
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
        matches!(self, WriterState::Active)
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
    pub grain: Option<(u64, usize)>, // (bytes, chunks covered)
    pub index_declared: bool,
    pub retain: Option<String>,
    pub retain_size: Option<String>,
    pub writer: WriterState,
}

impl StoreSummary {
    /// `list`'s INDEX column: a `.grain` token index that is present, or
    /// declared (and due to be rebuilt on the next import if actually
    /// missing) — either way, `--has` queries are meant to work here.
    pub fn indexed(&self) -> bool {
        self.index_declared || self.grain.is_some()
    }
}

pub fn summarize_store(
    dir: &Path,
    name: &str,
    records: &[ChunkRecord],
    bark: Option<&serde_json::Map<String, serde_json::Value>>,
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
    let rings_bytes = std::fs::metadata(format::rings_path(dir, name))
        .map(|m| m.len())
        .unwrap_or(0);
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

    // Who is writing? flock presence is the truth (lock files persist and
    // their contents go stale). This is a READ-ONLY probe — an observation,
    // never an acquisition — so `info`/`list` work on a store they can only
    // read (e.g. a root-owned /var/log/timberfs).
    use crate::store::LockProbe;
    let writer = match crate::store::probe_backing_exclusive(dir) {
        LockProbe::Held => WriterState::Mounted(crate::store::read_lock_mountpoint(dir)),
        LockProbe::Unreadable => WriterState::Unreadable,
        LockProbe::Absent | LockProbe::Free => match crate::store::probe_file_writer(dir, name) {
            LockProbe::Held => WriterState::Active,
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
        grain,
        index_declared,
        retain: get("retain"),
        retain_size: get("retain_size"),
        writer,
    }
}

/// `info`'s prose rendering of a writer state — also what its `--json`
/// mode prints under `"writer"`.
fn writer_text(w: &WriterState) -> String {
    match w {
        WriterState::Mounted(Some(mp)) => format!("mounted at {}", mp.display()),
        WriterState::Mounted(None) => "another timberfs process holds the directory".to_string(),
        WriterState::Active => "active writer (appender, import or rotation)".to_string(),
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
    const RESERVED: &[&str] = &[
        "id",
        "created",
        "derived_from",
        "derived_op",
        "window_from",
        "window_to",
        "index",
        "command",
        "pattern",
        "retain",
        "retain_size",
    ];
    let provenance: Vec<(String, String)> = bark
        .iter()
        .filter(|(k, _)| !RESERVED.contains(&k.as_str()))
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string()),
            )
        })
        .collect();

    // Pair-only facts: size/span, sidecar sizes, grain coverage, writer
    // state — computed by the same `summarize_store` that builds a `list`
    // row, so the two commands agree on what they report.
    let mut name = input
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut location = String::new();
    let mut bundle_bytes: Option<u64> = None;
    let (chunks, logical, compressed, min_ms, max_ms, rings_bytes, grain, writer) = if bundled {
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
        (
            chunks, logical, compressed, min_ms, max_ms, None, None, None,
        )
    } else {
        let (dir, base) = resolve_backing(input)?;
        name = base.clone();
        location = dir.display().to_string();
        let s = summarize_store(&dir, &base, records, handle.bark.as_ref());
        (
            s.chunks,
            s.logical_bytes,
            s.compressed_bytes,
            s.first_write_ms.unwrap_or(u64::MAX),
            s.last_write_ms.unwrap_or(0),
            Some(s.rings_bytes),
            s.grain,
            Some(writer_text(&s.writer)),
        )
    };

    if json {
        let mut o = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            o.insert(k.to_string(), v);
        };
        put("name", name.clone().into());
        put("kind", if bundled { "bundle" } else { "pair" }.into());
        if let Some(id) = &id {
            put("id", id.clone().into());
        }
        if let Some(c) = &created {
            put("created", c.clone().into());
        }
        if let Some(d) = &derived_from {
            put("derived_from", d.clone().into());
        }
        if let Some(d) = &derived_op {
            put("derived_op", d.clone().into());
        }
        if let Some(w) = &window_from {
            put("window_from", w.clone().into());
        }
        if let Some(w) = &window_to {
            put("window_to", w.clone().into());
        }
        if let Some(c) = &command {
            put("command", c.clone().into());
        }
        if let Some(pt) = &pattern {
            put("pattern", pt.clone().into());
        }
        if let Some(r) = &retain {
            put("retain", r.clone().into());
        }
        if let Some(r) = &retain_size {
            put("retain_size", r.clone().into());
        }
        put("index_declared", index_declared.into());
        put(
            "provenance",
            serde_json::Value::Object(
                provenance
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect(),
            ),
        );
        put("chunks", chunks.into());
        put("logical_bytes", logical.into());
        put("compressed_bytes", compressed.into());
        if compressed > 0 {
            put(
                "ratio",
                ((logical as f64 / compressed as f64 * 10.0).round() / 10.0).into(),
            );
        }
        if chunks > 0 {
            put("first_write_ms", min_ms.into());
            put("last_write_ms", max_ms.into());
        }
        if let Some(b) = rings_bytes {
            put("rings_bytes", b.into());
        }
        if let Some((b, n)) = grain {
            put("grain_bytes", b.into());
            put("grain_chunks", n.into());
        }
        if let Some(b) = bundle_bytes {
            put("bundle_bytes", b.into());
        }
        if let Some(w) = &writer {
            put("writer", w.clone().into());
        }
        println!("{}", serde_json::to_string_pretty(&o)?);
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
        if retain.is_some() || retain_size.is_some() {
            let mut parts: Vec<String> = Vec::new();
            if let Some(r) = &retain {
                parts.push(format!("keep {r}"));
            }
            if let Some(r) = &retain_size {
                parts.push(format!("disk <= {r}"));
            }
            // Retention only acts while a writer runs: an idle store with
            // a policy doesn't shrink — say so instead of surprising.
            let over = retain_size
                .as_deref()
                .and_then(|r| crate::append::parse_size_bytes(r).ok())
                .is_some_and(|budget| compressed > budget)
                && writer.as_deref() == Some("none");
            println!(
                "  retention {} — enforced by writers{}",
                parts.join(", "),
                if over {
                    " (currently OVER budget, and none is running)"
                } else {
                    ""
                }
            );
        }
        if let Some(w) = &writer {
            println!("  writer    {w}");
        }
    }
    Ok(())
}

fn human_duration(ms: u64) -> String {
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
    for (i, c) in chunks.iter().enumerate() {
        println!(
            "{:>5}  {:>12}  {:>10}  {:>10}  {:>5.1}x  {:<23}  {:<23}",
            i,
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
