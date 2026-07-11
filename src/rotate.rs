//! `timberfs rotate`: time-based rotation of a log's head.
//!
//! Everything written entirely before --cutoff moves out of the source into
//! the destination (or is dropped with --delete). Compressed frames are
//! relocated verbatim — no recompression — so rotating gigabytes of logs
//! costs I/O proportional to the *compressed* size.
//!
//! Works in two modes, auto-detected via the backing-dir flock:
//!   - offline: no daemon holds the lock; we take it and rewrite the
//!     backing files directly
//!   - mounted: a daemon holds the lock; we read its mountpoint from the
//!     lock file and send the request through the live mount as a
//!     setxattr control call, so the daemon rotates atomically under its
//!     own state lock

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{bail, Context};

use crate::format;
use crate::query::{fmt_ms, resolve_backing};
use crate::store::{self, Config, RotateStats, Store};

pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn setxattr(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let cname = CString::new(name).unwrap();
    let rc = unsafe {
        libc::setxattr(
            cpath.as_ptr(),
            cname.as_ptr(),
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Destination must be a plain name (or path) in the same backing dir;
/// .trunk/.rings suffixes are tolerated.
fn resolve_dest(dir: &Path, dest: &str, src_name: &str) -> anyhow::Result<String> {
    let p = Path::new(dest);
    let fname = p
        .file_name()
        .and_then(|s| s.to_str())
        .context("bad destination name")?;
    let base = fname
        .strip_suffix(&format!(".{}", format::TRUNK_EXT))
        .or_else(|| fname.strip_suffix(&format!(".{}", format::RINGS_EXT)))
        .unwrap_or(fname);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            let dir_c =
                fs::canonicalize(dir).with_context(|| format!("backing dir {}", dir.display()))?;
            let par_c = fs::canonicalize(parent)
                .with_context(|| format!("destination dir {}", parent.display()))?;
            if par_c != dir_c {
                bail!(
                    "destination must be in the same backing directory ({})",
                    dir.display()
                );
            }
        }
    }
    if base == src_name {
        bail!("destination equals source");
    }
    if base.is_empty() || base.starts_with('.') {
        bail!("bad destination name {base:?}");
    }
    Ok(base.to_string())
}

fn report(stats: &RotateStats, target: Option<&str>) {
    if stats.chunks_moved == 0 {
        println!("nothing to rotate: no chunks written entirely before the cutoff");
        return;
    }
    println!(
        "rotated {} chunk(s): {} uncompressed ({} on disk), written {} .. {}",
        stats.chunks_moved,
        human_bytes(stats.uncomp_bytes),
        human_bytes(stats.comp_bytes),
        fmt_ms(stats.first_write_ms),
        fmt_ms(stats.last_write_ms)
    );
    match target {
        Some(t) => println!("  moved into {t}"),
        None => println!("  deleted (--delete)"),
    }
    println!("  source keeps {} chunk(s)", stats.chunks_remaining);
}

pub fn cmd_rotate(
    source: &Path,
    dest: Option<&str>,
    cutoff_ms: u64,
    delete: bool,
    dry_run: bool,
    fail_on_empty: bool,
) -> anyhow::Result<()> {
    if crate::query::is_bundle(source) {
        bail!(
            "{} is a .timber transfer bundle — bundles are read-only \
             (query/index/export work directly on them); import it into a \
             log to write",
            source.display()
        );
    }
    let (dir, src_name) = resolve_backing(source)?;
    let rings = format::rings_path(&dir, &src_name);
    if !rings.exists() {
        bail!(
            "no index file {} (expected a timberfs backing file or its logical name)",
            rings.display()
        );
    }
    let target_name = match (dest, delete) {
        (Some(d), false) => Some(resolve_dest(&dir, d, &src_name)?),
        (None, true) => None,
        _ => bail!("give a destination file, or --delete to drop the rotated data"),
    };

    // Preview from the on-disk index (chunk-granular, like queries; a
    // prefix scan, not a binary search — imported files' windows are only
    // mostly sorted).
    let chunks = format::read_index(&rings)?;
    let k = chunks
        .iter()
        .take_while(|c| c.last_write_ms < cutoff_ms)
        .count();
    println!(
        "cutoff {}: {} of {} chunk(s) written entirely before it ({} uncompressed)",
        fmt_ms(cutoff_ms),
        k,
        chunks.len(),
        human_bytes(chunks[..k].iter().map(|c| c.uncomp_len).sum::<u64>())
    );
    if dry_run {
        println!(
            "dry run: nothing changed (a live mount may also flush buffered data at rotation time)"
        );
        return Ok(());
    }

    match store::lock_backing_shared(&dir)? {
        Some(_dir_guard) => {
            // No mount daemon. Take the per-file writer locks: the source
            // (and destination, which may belong to a live appender too).
            let _src_lock = store::lock_file_exclusive(&dir, &src_name)?.with_context(|| {
                format!("{src_name} has an active writer (appender?); stop it and retry")
            })?;
            let _dst_lock = match &target_name {
                Some(t) => Some(store::lock_file_exclusive(&dir, t)?.with_context(|| {
                    format!("{t} has an active writer (appender?); stop it and retry")
                })?),
                None => None,
            };
            let cfg = Config {
                chunk_size: 256 * 1024,
                level: 3,
                flush_age_ms: 5000,
            };
            let mut st = Store {
                dir: dir.clone(),
                cfg,
                files: BTreeMap::new(),
            };
            st.create(&src_name)?;
            let target_was_new = target_name
                .as_deref()
                .is_some_and(|t| !format::rings_path(&dir, t).exists());
            let stats = st.rotate_head(&src_name, target_name.as_deref(), cutoff_ms)?;
            if stats.chunks_moved == 0 && fail_on_empty {
                bail!("nothing to rotate: no chunks written entirely before the cutoff (--fail-on-empty)");
            }
            if let (Some(t), true) = (target_name.as_deref(), target_was_new) {
                if stats.chunks_moved == 0 {
                    // Rotating nothing into a new target still creates it:
                    // present-but-empty ("this window was rotated, nothing
                    // was there") and missing ("don't ingest past the gap")
                    // are opposite signals to whoever ships or ingests it.
                    st.create(t)?;
                }
                // A rotation-created segment is a derived store: rotate
                // holds the source's writer locks, so it may mint the
                // source's identity for a complete lineage chain.
                let src_bark = crate::bark::ensure_identified(&dir, &src_name)?;
                crate::bark::save(
                    &dir,
                    t,
                    &crate::bark::derived_map(Some(&src_bark), "rotate"),
                )?;
            }
            report(&stats, target_name.as_deref());
            if stats.chunks_moved == 0 && target_was_new {
                if let Some(t) = target_name.as_deref() {
                    println!("  created {t} empty — an attested empty result (--fail-on-empty to error instead)");
                }
            }
        }
        None => {
            let Some(mp) = store::read_lock_mountpoint(&dir) else {
                let holder = store::read_lock_raw(&dir)
                    .map(|s| s.split_whitespace().collect::<Vec<_>>().join(", "))
                    .unwrap_or_else(|| "unknown holder".to_string());
                bail!(
                    "backing directory is locked by another timberfs process ({holder}); \
                     stop it and retry"
                );
            };
            let mut value = match &target_name {
                Some(t) => format!("cutoff={cutoff_ms};target={t}"),
                None => format!("cutoff={cutoff_ms};delete"),
            };
            if fail_on_empty {
                value.push_str(";fail-on-empty");
            }
            setxattr(
                &mp.join(&src_name),
                "user.timberfs.rotate",
                value.as_bytes(),
            )
            .map_err(|e| {
                if e.raw_os_error() == Some(libc::ENODATA) {
                    anyhow::anyhow!(
                        "nothing to rotate: no chunks written entirely before the cutoff \
                         (--fail-on-empty)"
                    )
                } else {
                    anyhow::Error::new(e).context(format!(
                        "rotate request via live mount {} failed (see the timberfs daemon's \
                         stderr)",
                        mp.display()
                    ))
                }
            })?;
            println!("rotated through the live mount on {}", mp.display());
            let after = format::read_index(&rings)?;
            println!("  source keeps {} chunk(s)", after.len());
            if let Some(t) = &target_name {
                let ti = format::read_index(&format::rings_path(&dir, t))?;
                println!(
                    "  {} now has {} chunk(s), {} on disk",
                    t,
                    ti.len(),
                    human_bytes(ti.last().map(|c| c.comp_end()).unwrap_or(0))
                );
            }
        }
    }
    Ok(())
}
