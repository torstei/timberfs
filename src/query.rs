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

/// Accepts RFC3339, "YYYY-MM-DD HH:MM[:SS]", bare "HH:MM[:SS[.mmm]]"
/// (today, local time), or unix seconds/milliseconds.
pub fn parse_time(s: &str) -> anyhow::Result<u64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis() as u64);
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(dt) = Local.from_local_datetime(&naive).earliest() {
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
        "unrecognized time {s:?} (try RFC3339, 'YYYY-MM-DD HH:MM:SS', 'HH:MM[:SS]', \
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
) -> anyhow::Result<(Vec<(usize, ChunkRecord)>, usize)> {
    let mut selected: Vec<(usize, ChunkRecord)> = chunks
        .iter()
        .enumerate()
        .filter(|(_, c)| c.last_write_ms >= from_ms && c.first_write_ms <= to_ms)
        .map(|(i, c)| (i, *c))
        .collect();
    let in_window = selected.len();

    if !has.is_empty() {
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
        let grain = if is_bundle(file) {
            None
        } else {
            let (dir, base) = resolve_backing(file)?;
            crate::grain::load(&crate::format::grain_path(&dir, &base)).ok()
        };
        match grain {
            Some(g) => {
                selected.retain(|(i, _)| g.may_contain_all(*i, &tokens));
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
pub fn cmd_query(
    files: &[std::path::PathBuf],
    from: Option<&str>,
    to: Option<&str>,
    has: &[String],
    no_filename: bool,
) -> anyhow::Result<()> {
    let from_ms = from.map(parse_time).transpose()?.unwrap_or(0);
    let to_ms = to.map(parse_time).transpose()?.unwrap_or(u64::MAX);
    if from_ms > to_ms {
        bail!("--from is after --to");
    }
    if files.len() == 1 {
        return query_single(&files[0], from_ms, to_ms, has);
    }
    query_multi(files, from_ms, to_ms, has, no_filename)
}

fn query_single(file: &Path, from_ms: u64, to_ms: u64, has: &[String]) -> anyhow::Result<()> {
    let source = open_source(file)?;
    let chunks = source.records;
    let (selected, in_window) = select_chunks(file, &chunks, from_ms, to_ms, has)?;

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
        let (selected, _) = select_chunks(f, &source.records, from_ms, to_ms, has)?;
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
