//! `timberfs append`: the FUSE-less write path — read stdin, write chunks
//! straight into the backing store (svlogd/s6-log style):
//!
//!     myapp 2>&1 | timberfs append logs-backing/app.log
//!
//! Locking: a SHARED lock on the backing directory (appenders coexist with
//! each other and with offline rotation of other files, but never with a
//! mount) plus an EXCLUSIVE per-file lock (one writer per log). Data is
//! flushed into chunks by the same size/age rules as the mount; EOF,
//! SIGTERM or SIGINT flush and sync everything before exit.
//!
//! Retention: --retain (max age) and --retain-size (compressed-size
//! budget) continuously drop the oldest chunks, checked every second, at
//! startup, and once more at shutdown. See Store::enforce_retention for
//! the hysteresis rules.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};

use crate::query::{fmt_ms, resolve_backing};
use crate::store::{self, Config, Store};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// SIGTERM/SIGINT set the stop flag; installed WITHOUT SA_RESTART so a
/// blocking stdin read returns EINTR and the main loop notices promptly.
fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

/// "30d", "12h", "45m", "90s", "2w", or bare seconds; fractions allowed.
pub fn parse_duration_ms(s: &str) -> anyhow::Result<u64> {
    let t = s.trim();
    let (num, mult_ms) = if let Some(r) = t.strip_suffix(['w', 'W']) {
        (r, 7.0 * 86_400_000.0)
    } else if let Some(r) = t.strip_suffix(['d', 'D']) {
        (r, 86_400_000.0)
    } else if let Some(r) = t.strip_suffix(['h', 'H']) {
        (r, 3_600_000.0)
    } else if let Some(r) = t.strip_suffix(['m', 'M']) {
        (r, 60_000.0)
    } else if let Some(r) = t.strip_suffix(['s', 'S']) {
        (r, 1_000.0)
    } else {
        (t, 1_000.0)
    };
    let v: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("unrecognized duration {s:?} (try 30d, 12h, 45m, 90s)"))?;
    if !v.is_finite() || v < 0.0 {
        bail!("duration {s:?} out of range");
    }
    Ok((v * mult_ms) as u64)
}

/// "200G", "512M", "1T", "64K" (powers of 1024; optional B/iB suffix), or
/// bare bytes; fractions allowed.
pub fn parse_size_bytes(s: &str) -> anyhow::Result<u64> {
    let mut t = s.trim().to_ascii_uppercase();
    if let Some(r) = t.strip_suffix('B') {
        t = r.to_string();
    }
    if let Some(r) = t.strip_suffix('I') {
        t = r.to_string();
    }
    let (num, mult) = if let Some(r) = t.strip_suffix('K') {
        (r.to_string(), 1u64 << 10)
    } else if let Some(r) = t.strip_suffix('M') {
        (r.to_string(), 1u64 << 20)
    } else if let Some(r) = t.strip_suffix('G') {
        (r.to_string(), 1u64 << 30)
    } else if let Some(r) = t.strip_suffix('T') {
        (r.to_string(), 1u64 << 40)
    } else {
        (t, 1)
    };
    let v: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("unrecognized size {s:?} (try 200G, 512M, 64K)"))?;
    if !v.is_finite() || v < 0.0 {
        bail!("size {s:?} out of range");
    }
    Ok((v * mult as f64) as u64)
}

fn run_retention(
    store: &Mutex<Store>,
    name: &str,
    max_age_ms: Option<u64>,
    max_comp_bytes: Option<u64>,
) {
    if max_age_ms.is_none() && max_comp_bytes.is_none() {
        return;
    }
    let result = store
        .lock()
        .unwrap()
        .enforce_retention(name, max_age_ms, max_comp_bytes);
    match result {
        Ok(Some(stats)) => {
            eprintln!(
                "timberfs: {name}: retention dropped {} chunk(s), {} compressed bytes, written {} .. {}",
                stats.chunks_moved,
                stats.comp_bytes,
                fmt_ms(stats.first_write_ms),
                fmt_ms(stats.last_write_ms)
            );
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("timberfs: {name}: retention failed: {e}");
        }
    }
}

pub fn cmd_append(
    target: &Path,
    cfg: Config,
    retain: Option<&str>,
    retain_size: Option<&str>,
) -> anyhow::Result<()> {
    let max_age_ms = retain.map(parse_duration_ms).transpose()?;
    let max_comp_bytes = retain_size.map(parse_size_bytes).transpose()?;
    if crate::query::is_bundle(target) {
        bail!(
            "{} is a .timber transfer bundle — bundles are read-only \
             (query/index/export work directly on them); import it into a \
             log to write",
            target.display()
        );
    }
    crate::query::ensure_dest_is_not_plain_file(target, "append")?;
    let (dir, name) = resolve_backing(target)?;
    fs::create_dir_all(&dir)?;

    let _dir_lock = match store::lock_backing_shared(&dir)? {
        Some(f) => f,
        None => {
            let mounted = store::read_lock_mountpoint(&dir)
                .map(|m| format!(" (mounted on {})", m.display()))
                .unwrap_or_default();
            bail!(
                "backing directory {} is served by a timberfs mount{mounted}; \
                 write through the mount instead, or unmount first",
                dir.display()
            );
        }
    };
    let file_lock = match store::lock_file_exclusive(&dir, &name)? {
        Some(f) => f,
        None => {
            bail!("{name} already has a writer (another timberfs appender or a rotation)");
        }
    };
    store::write_lock_info(
        &file_lock,
        &format!("appender pid={}\n", std::process::id()),
    )?;

    let mut st = Store {
        dir: dir.clone(),
        cfg,
        files: BTreeMap::new(),
    };
    st.create(&name)?;
    let store = Arc::new(Mutex::new(st));

    // Catch up on retention from before this run, then keep enforcing.
    run_retention(&store, &name, max_age_ms, max_comp_bytes);

    // Background thread: age-based chunk flushing (same as the mount) and
    // the once-a-second retention check.
    {
        let store = Arc::clone(&store);
        let name = name.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(1000));
            store.lock().unwrap().flush_aged();
            run_retention(&store, &name, max_age_ms, max_comp_bytes);
        });
    }

    install_signal_handlers();
    eprintln!(
        "timberfs: appending stdin to {}/{} (chunk {} B, zstd -{}, flush age {} ms)",
        dir.display(),
        name,
        cfg.chunk_size,
        cfg.level,
        cfg.flush_age_ms
    );

    let mut stdin = io::stdin().lock();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    let mut interrupted = false;
    loop {
        if STOP.load(Ordering::Relaxed) {
            interrupted = true;
            break;
        }
        match stdin.read(&mut buf) {
            Ok(0) => {
                break;
            }
            Ok(n) => {
                let mut s = store.lock().unwrap();
                let cfg = s.cfg;
                s.files.get_mut(&name).unwrap().append(&buf[..n], &cfg)?;
                total += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                continue;
            }
            Err(e) => {
                store.lock().unwrap().flush_all();
                return Err(e.into());
            }
        }
    }

    store.lock().unwrap().flush_all();
    run_retention(&store, &name, max_age_ms, max_comp_bytes);
    eprintln!(
        "timberfs: appended {} bytes to {} ({})",
        total,
        name,
        if interrupted {
            "stopped by signal, flushed"
        } else {
            "end of input"
        }
    );
    Ok(())
}
