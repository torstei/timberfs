//! Offline access to the backing store: time-range extraction and index
//! inspection. These read the .trunk/.rings pair directly, so they work
//! whether or not the filesystem is mounted (concurrent use with a live
//! mount is safe: chunks are immutable once written and the index is
//! append-only).

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

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
    let file = File::open(format::trunk_path(&dir, &base))?;
    let bark = crate::bark::load(&dir, &base);
    Ok(SourceHandle {
        records,
        file,
        bark,
    })
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
    no_filename: bool,
    show_write_time: bool,
    by_write_time: bool,
    null_sep: bool,
    records: bool,
) -> anyhow::Result<()> {
    let from_ms = from.unwrap_or(0);
    let to_ms = to.unwrap_or(u64::MAX);
    if from_ms > to_ms {
        bail!("--from is after --to");
    }
    let windowed = from.is_some() || to.is_some();
    // The entry pipeline engages when there is something to verify
    // (a window: the DEFAULT is that every printed entry's own timestamp
    // is inside it) or when the framing needs entries (-0, annotation).
    // --by-write-time is the raw escape hatch: chunk dump, no parsing.
    if !by_write_time && (windowed || null_sep || show_write_time || records) {
        return query_entries(
            files,
            from_ms,
            to_ms,
            windowed,
            has,
            no_filename,
            show_write_time,
            null_sep,
            records,
        );
    }
    if files.len() == 1 {
        return query_single(&files[0], from_ms, to_ms, has);
    }
    query_multi(files, from_ms, to_ms, has, no_filename)
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
    no_filename: bool,
    show_write_time: bool,
    null_sep: bool,
    records: bool,
) -> anyhow::Result<()> {
    struct Src {
        file: File,
        chunks: Vec<(usize, ChunkRecord)>,
        total_chunks: usize,
        pos: usize,
        sink: crate::entry::EntrySink,
    }
    let decomp = |file: &File, c: &ChunkRecord| -> anyhow::Result<Vec<u8>> {
        let mut comp = vec![0u8; c.comp_len as usize];
        file.read_exact_at(&mut comp, c.comp_start)?;
        Ok(zstd::stream::decode_all(&comp[..])?)
    };
    let multi = files.len() > 1 && !no_filename;
    let mut srcs: Vec<Src> = Vec::new();
    for f in files {
        let source = open_source(f)?;
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
            &[],
        )?;
        let filterable = windowed
            && match selected.first() {
                Some((_, c)) => crate::entry::probe_stamps(&extractor, &decomp(&source.file, c)?),
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
            select_chunks(f, &source.records, from_ms, to_ms, has, &[])?.0
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
            file: source.file,
            total_chunks: source.records.len(),
            chunks: selected,
            pos: 0,
            sink: crate::entry::EntrySink::new(
                extractor,
                window,
                framing,
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
        let data = decomp(&s.file, &c)?;
        s.sink
            .push_chunk(&data, (c.first_write_ms, c.last_write_ms), &mut out)?;
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

fn query_single(file: &Path, from_ms: u64, to_ms: u64, has: &[String]) -> anyhow::Result<()> {
    let source = open_source(file)?;
    let chunks = source.records;
    let (selected, in_window) = select_chunks(file, &chunks, from_ms, to_ms, has, &[])?;

    let trunk = source.file;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut uncomp_total = 0u64;
    for (_, c) in &selected {
        let mut comp = vec![0u8; c.comp_len as usize];
        trunk.read_exact_at(&mut comp, c.comp_start)?;
        let data = zstd::stream::decode_all(&comp[..])?;
        out.write_all(&data)?;
        uncomp_total += c.uncomp_len;
    }
    out.flush()?;
    eprintln!(
        "timberfs: {} of {} chunk(s){}, {} bytes (chunk granularity; unflushed tail not included)",
        selected.len(),
        chunks.len(),
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
    no_filename: bool,
) -> anyhow::Result<()> {
    struct Src {
        label: Vec<u8>,
        file: File,
        chunks: Vec<ChunkRecord>,
        pos: usize,
        carry: Vec<u8>,
    }
    let mut srcs: Vec<Src> = Vec::new();
    let mut total_chunks = 0usize;
    let mut total_selected = 0usize;
    for f in files {
        let source = open_source(f)?;
        let (selected, _) = select_chunks(f, &source.records, from_ms, to_ms, has, &[])?;
        eprintln!(
            "timberfs: {}: {} of {} chunk(s)",
            f.display(),
            selected.len(),
            source.records.len()
        );
        total_chunks += source.records.len();
        total_selected += selected.len();
        srcs.push(Src {
            label: f.display().to_string().into_bytes(),
            file: source.file,
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
        let mut comp = vec![0u8; c.comp_len as usize];
        s.file.read_exact_at(&mut comp, c.comp_start)?;
        let data = zstd::stream::decode_all(&comp[..])?;
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

/// Human-readable dump of the write-time index.
/// `timberfs info`: a store's vital signs on one screen — identity,
/// lineage, provenance, data/compression, time covered, index sizes and
/// coverage, writer state. The `\d+` of the database metaphor. Read-only;
/// works identically on backing pairs and .timber bundles.
pub fn cmd_info(input: &Path, json: bool) -> anyhow::Result<()> {
    let bundled = is_bundle(input);
    let handle = open_source(input)?;
    let records = &handle.records;

    let chunks = records.len();
    let logical = match (records.first(), records.last()) {
        (Some(f), Some(l)) => l.uncomp_end() - f.uncomp_start,
        _ => 0,
    };
    let compressed = match (records.first(), records.last()) {
        (Some(f), Some(l)) => l.comp_end() - f.comp_start,
        _ => 0,
    };
    // Mostly-sorted windows: scan for the true extremes (48 B per chunk).
    let (mut min_ms, mut max_ms) = (u64::MAX, 0u64);
    for r in records {
        min_ms = min_ms.min(r.first_write_ms);
        max_ms = max_ms.max(r.last_write_ms);
    }

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

    // Pair-only facts: sidecar sizes, grain coverage, writer state.
    let mut name = input
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut location = String::new();
    let mut rings_bytes: Option<u64> = None;
    let mut grain: Option<(u64, usize)> = None; // (bytes, chunks covered)
    let mut writer: Option<String> = None;
    let mut bundle_bytes: Option<u64> = None;
    if bundled {
        bundle_bytes = std::fs::metadata(input).map(|m| m.len()).ok();
    } else {
        let (dir, base) = resolve_backing(input)?;
        name = base.clone();
        location = dir.display().to_string();
        rings_bytes = std::fs::metadata(format::rings_path(&dir, &base))
            .map(|m| m.len())
            .ok();
        let gpath = format::grain_path(&dir, &base);
        if let Ok(m) = std::fs::metadata(&gpath) {
            if let Ok(g) = crate::grain::load(&gpath) {
                grain = Some((m.len(), g.chunk_count()));
            }
        }
        // Who is writing? flock presence is the truth (lock files persist
        // and their contents go stale); probe without blocking anyone.
        writer = Some(match crate::store::lock_backing_shared(&dir)? {
            None => match crate::store::read_lock_mountpoint(&dir) {
                Some(mp) => format!("mounted at {}", mp.display()),
                None => "another timberfs process holds the directory".to_string(),
            },
            Some(_dir_guard) => match crate::store::lock_file_exclusive(&dir, &base)? {
                None => "active writer (appender, import or rotation)".to_string(),
                Some(_lock) => "none".to_string(),
            },
        });
    }

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
