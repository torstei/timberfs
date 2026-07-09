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
use crate::query::{fmt_ms, parse_time, resolve_backing};
use crate::store::{self, Config, RotateStats, Store};

fn human_bytes(n: u64) -> String {
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
            let dir_c = fs::canonicalize(dir)
                .with_context(|| format!("backing dir {}", dir.display()))?;
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
    cutoff: &str,
    delete: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let cutoff_ms = parse_time(cutoff)?;
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

    // Preview from the on-disk index (chunk-granular, like queries).
    let chunks = format::read_index(&rings)?;
    let k = chunks.partition_point(|c| c.last_write_ms < cutoff_ms);
    println!(
        "cutoff {}: {} of {} chunk(s) written entirely before it ({} uncompressed)",
        fmt_ms(cutoff_ms),
        k,
        chunks.len(),
        human_bytes(chunks[..k].iter().map(|c| c.uncomp_len).sum::<u64>())
    );
    if dry_run {
        println!("dry run: nothing changed (a live mount may also flush buffered data at rotation time)");
        return Ok(());
    }

    match store::try_lock_backing(&dir)? {
        Some(_guard) => {
            // Offline: we hold the lock, so no daemon can start mid-rewrite.
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
            let stats = st.rotate_head(&src_name, target_name.as_deref(), cutoff_ms)?;
            report(&stats, target_name.as_deref());
        }
        None => {
            let mp = store::read_lock_mountpoint(&dir).context(
                "backing dir is locked by a daemon but the lock file names no mountpoint",
            )?;
            let value = match &target_name {
                Some(t) => format!("cutoff={cutoff_ms};target={t}"),
                None => format!("cutoff={cutoff_ms};delete"),
            };
            setxattr(&mp.join(&src_name), "user.timberfs.rotate", value.as_bytes())
                .with_context(|| {
                    format!(
                        "rotate request via live mount {} failed (see the timberfs daemon's stderr)",
                        mp.display()
                    )
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
