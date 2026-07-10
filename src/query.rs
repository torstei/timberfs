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
}

pub fn open_source(input: &Path) -> anyhow::Result<SourceHandle> {
    if is_bundle(input) {
        let file = File::open(input).with_context(|| format!("opening {}", input.display()))?;
        let mut archive = tar::Archive::new(&file);
        let mut rings_bytes: Option<Vec<u8>> = None;
        let mut trunk_pos: Option<(u64, u64)> = None;
        for entry in archive.entries()? {
            let mut entry = entry?;
            let member = entry.path()?.to_string_lossy().to_string();
            if member.ends_with(&format!(".{}", format::RINGS_EXT)) {
                let mut v = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut v)?;
                rings_bytes = Some(v);
            } else if member.ends_with(&format!(".{}", format::TRUNK_EXT)) {
                trunk_pos = Some((entry.raw_file_position(), entry.header().entry_size()?));
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
        return Ok(SourceHandle { records, file });
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
    Ok(SourceHandle { records, file })
}

/// Print the bytes stamped inside [from, to]. Selection is at chunk
/// granularity: every chunk whose time window overlaps the requested range
/// is emitted in full, chosen by an interval-overlap scan of the index.
/// (A scan, not a binary search: imported files carry logged timestamps
/// whose windows are only mostly sorted. The index is 48 bytes per chunk,
/// so scanning it is negligible next to decompressing one chunk.)
pub fn cmd_query(file: &Path, from: Option<&str>, to: Option<&str>) -> anyhow::Result<()> {
    let source = open_source(file)?;
    let chunks = source.records;
    let from_ms = from.map(parse_time).transpose()?.unwrap_or(0);
    let to_ms = to.map(parse_time).transpose()?.unwrap_or(u64::MAX);
    if from_ms > to_ms {
        bail!("--from is after --to");
    }

    let selected: Vec<ChunkRecord> = chunks
        .iter()
        .filter(|c| c.last_write_ms >= from_ms && c.first_write_ms <= to_ms)
        .copied()
        .collect();

    let trunk = source.file;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut uncomp_total = 0u64;
    for c in &selected {
        let mut comp = vec![0u8; c.comp_len as usize];
        trunk.read_exact_at(&mut comp, c.comp_start)?;
        let data = zstd::stream::decode_all(&comp[..])?;
        out.write_all(&data)?;
        uncomp_total += c.uncomp_len;
    }
    out.flush()?;
    eprintln!(
        "timberfs: {} of {} chunk(s), {} bytes (chunk granularity; unflushed tail not included)",
        selected.len(),
        chunks.len(),
        uncomp_total
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
