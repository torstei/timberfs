//! `timberfs export`: carve a time window (or everything) out of a
//! timberfs log into a NEW timberfs log — the read-side twin of rotation.
//! Selected chunks are copied verbatim (no decompression, no
//! recompression) and their records rebased into a fresh offset space, so
//! the cost is proportional to the compressed size of the window.
//!
//!     timberfs export backing/archive.log incident.log --from 13:40 --to 14:10
//!     timberfs export backing/archive.log incident.timber --from 13:40 --to 14:10
//!
//! A destination ending in `.timber` writes the single-file transfer
//! bundle: a plain uncompressed tar (the payload is already zstd) with the
//! tiny `.rings` member first and the `.trunk` member second, so readers
//! see the index before the data. Anyone without timberfs can always
//! recover: `tar xf x.timber && zstd -dc x.trunk`. `timberfs import`
//! accepts bundles directly.
//!
//! The source needs no lock (chunks are immutable, the index append-only —
//! the same guarantee query relies on). Export always creates; merging
//! into existing logs is import's job.
//!
//! An empty window still exports (an empty artifact whose bark records
//! the requested window as window_from/window_to — evidence of absence,
//! as opposed to the absence of evidence a MISSING file signals); pass
//! --fail-on-empty to get an error instead.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{bail, Context};

use crate::format::{self, ChunkRecord};
use crate::query::{fmt_ms, is_bundle, open_source, resolve_backing};
use crate::store;

/// Streams the selected chunks' compressed frames in order, so a bundle's
/// trunk member is written without materializing it (the selection may be
/// non-contiguous in the source for mostly-sorted imported files).
struct ChunksReader<'a> {
    trunk: &'a File,
    chunks: &'a [ChunkRecord],
    idx: usize,
    off: u64,
}

impl Read for ChunksReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.idx < self.chunks.len() {
            let c = &self.chunks[self.idx];
            if self.off == c.comp_len {
                self.idx += 1;
                self.off = 0;
                continue;
            }
            let want = ((c.comp_len - self.off) as usize).min(buf.len());
            let n = self
                .trunk
                .read_at(&mut buf[..want], c.comp_start + self.off)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "trunk ended inside a chunk",
                ));
            }
            self.off += n as u64;
            return Ok(n);
        }
        Ok(0)
    }
}

fn write_pair(
    dest: &Path,
    rings_bytes: &[u8],
    src_trunk: &File,
    selected: &[ChunkRecord],
) -> anyhow::Result<()> {
    let (ddir, dname) = resolve_backing(dest)?;
    fs::create_dir_all(&ddir)?;
    let _dir_lock = store::lock_backing_shared(&ddir)?.with_context(|| {
        format!(
            "destination directory {} is served by a timberfs mount",
            ddir.display()
        )
    })?;
    let _file_lock = store::lock_file_exclusive(&ddir, &dname)?
        .with_context(|| format!("{dname} already has a writer"))?;
    let trunk_p = format::trunk_path(&ddir, &dname);
    let rings_p = format::rings_path(&ddir, &dname);
    if trunk_p.exists() || rings_p.exists() {
        bail!(
            "{dname} already exists in {} — export always creates; merge with import",
            ddir.display()
        );
    }
    // Data first, index second — the same crash discipline as everywhere.
    let trunk = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&trunk_p)?;
    let mut reader = ChunksReader {
        trunk: src_trunk,
        chunks: selected,
        idx: 0,
        off: 0,
    };
    let mut off = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        trunk.write_all_at(&buf[..n], off)?;
        off += n as u64;
    }
    trunk.sync_all()?;
    let rings = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&rings_p)?;
    rings.write_all_at(rings_bytes, 0)?;
    rings.sync_all()?;
    Ok(())
}

fn write_bundle(
    dest: &Path,
    rings_bytes: &[u8],
    bark_bytes: Option<Vec<u8>>,
    src_trunk: &File,
    selected: &[ChunkRecord],
    total_comp: u64,
    mtime_secs: u64,
) -> anyhow::Result<()> {
    let stem = dest
        .file_stem()
        .and_then(|s| s.to_str())
        .context("bad bundle name")?;
    let out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut builder = tar::Builder::new(out);

    let mut header = tar::Header::new_ustar();
    header.set_mode(0o644);
    header.set_mtime(mtime_secs);
    header.set_size(rings_bytes.len() as u64);
    builder.append_data(&mut header, format!("{stem}.rings"), rings_bytes)?;

    if let Some(bark) = bark_bytes {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_mtime(mtime_secs);
        header.set_size(bark.len() as u64);
        builder.append_data(&mut header, format!("{stem}.bark"), &bark[..])?;
    }

    let mut header = tar::Header::new_ustar();
    header.set_mode(0o644);
    header.set_mtime(mtime_secs);
    header.set_size(total_comp);
    let reader = ChunksReader {
        trunk: src_trunk,
        chunks: selected,
        idx: 0,
        off: 0,
    };
    builder.append_data(&mut header, format!("{stem}.trunk"), reader)?;

    let out = builder.into_inner()?;
    out.sync_all()?;
    Ok(())
}

pub fn cmd_export(
    source: &Path,
    dest: &Path,
    from: Option<u64>,
    to: Option<u64>,
    fail_on_empty: bool,
) -> anyhow::Result<()> {
    let handle = open_source(source)?;
    let chunks = handle.records;
    let src_trunk = handle.file;
    let from_ms = from.unwrap_or(0);
    let to_ms = to.unwrap_or(u64::MAX);
    if from_ms > to_ms {
        bail!("--from is after --to");
    }
    let selected: Vec<ChunkRecord> = chunks
        .iter()
        .filter(|c| c.last_write_ms >= from_ms && c.first_write_ms <= to_ms)
        .copied()
        .collect();
    // An empty selection still yields an artifact: present-but-empty
    // ("the window was covered, nothing was there") and missing ("wait,
    // don't ingest past this gap") are opposite signals to a consumer.
    if selected.is_empty() && fail_on_empty {
        bail!("no chunks overlap the requested window (--fail-on-empty)");
    }
    // The derived artifact is a NEW store: fresh identity, lineage to the
    // source when it is identified, inherited data provenance, settings
    // dropped. Export never writes the source (read-only), so an
    // unidentified source simply yields no derived_from. The requested
    // window is an operation fact (what was asked, which content can
    // never state) and goes in the bark when given.
    let mut derived = crate::bark::derived_map(handle.bark.as_ref(), "export");
    if from.is_some() {
        derived.insert(
            "window_from".to_string(),
            serde_json::Value::String(crate::bark::ms_rfc3339(from_ms)),
        );
    }
    if to.is_some() {
        derived.insert(
            "window_to".to_string(),
            serde_json::Value::String(crate::bark::ms_rfc3339(to_ms)),
        );
    }
    let derived_bark = crate::bark::with_identity(derived)?;

    // Rebase into a fresh offset space (chunk-by-chunk: the selection may
    // be non-contiguous in the source).
    let mut out_records = Vec::with_capacity(selected.len());
    let mut uncomp_off = 0u64;
    let mut comp_off = 0u64;
    for c in &selected {
        out_records.push(ChunkRecord {
            uncomp_start: uncomp_off,
            comp_start: comp_off,
            ..*c
        });
        uncomp_off += c.uncomp_len;
        comp_off += c.comp_len;
    }
    let mut rings_bytes = Vec::with_capacity(8 + out_records.len() * 48);
    rings_bytes.extend_from_slice(format::RINGS_MAGIC);
    for r in &out_records {
        rings_bytes.extend_from_slice(&r.to_bytes());
    }

    let bundled = is_bundle(dest);
    if !bundled {
        crate::query::ensure_dest_is_not_plain_file(dest, "export")?;
    }
    let bark_text = serde_json::to_string_pretty(&serde_json::Value::Object(derived_bark))? + "\n";
    if bundled {
        let mtime_secs = selected
            .last()
            .map(|c| c.last_write_ms / 1000)
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            });
        write_bundle(
            dest,
            &rings_bytes,
            Some(bark_text.into_bytes()),
            &src_trunk,
            &selected,
            comp_off,
            mtime_secs,
        )?;
    } else {
        write_pair(dest, &rings_bytes, &src_trunk, &selected)?;
        let (ddir, dname) = resolve_backing(dest)?;
        fs::write(format::bark_path(&ddir, &dname), bark_text)?;
    }
    match (selected.first(), selected.last()) {
        (Some(first), Some(last)) => eprintln!(
            "timberfs: exported {} of {} chunk(s), {} bytes ({} compressed), spanning {} .. {} -> {}{}",
            selected.len(),
            chunks.len(),
            uncomp_off,
            comp_off,
            fmt_ms(first.first_write_ms),
            fmt_ms(last.last_write_ms),
            dest.display(),
            if bundled { " (bundle)" } else { "" }
        ),
        _ => eprintln!(
            "timberfs: exported 0 of {} chunk(s) — empty window; the artifact attests it \
             (--fail-on-empty to error instead) -> {}{}",
            chunks.len(),
            dest.display(),
            if bundled { " (bundle)" } else { "" }
        ),
    }
    Ok(())
}
