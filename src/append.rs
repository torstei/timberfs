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

/// True once SIGTERM/SIGINT has been received — a graceful-stop request.
/// The records sink (a long-lived streaming appender) polls this from its
/// maintenance thread to flush before the process goes.
pub fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// SIGTERM/SIGINT set the stop flag; installed WITHOUT SA_RESTART so a
/// blocking stdin read returns EINTR and the main loop notices promptly.
pub fn install_signal_handlers() {
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

/// The store's retention policy, re-read from the manifest on every
/// tick: `timberfs set retain=30d` on a LIVE log takes effect within a
/// second, no restart (restarting the writer means restarting whatever
/// pipes into it — usually user-visible). A manifest that stops parsing
/// mid-flight keeps the LAST GOOD policy with one warning: never
/// silently unbounded, never a dead producer.
struct LivePolicy {
    dir: std::path::PathBuf,
    name: String,
    last: crate::bark::Retention,
    warned: bool,
    /// (mtime, len) of the manifest at the last parse: the file almost
    /// never changes, so the once-a-second re-read is a stat until it does.
    stamp: Option<(Option<std::time::SystemTime>, u64)>,
}

impl LivePolicy {
    fn refresh(&mut self) -> crate::bark::Retention {
        let cur = std::fs::metadata(crate::format::bark_path(&self.dir, &self.name))
            .ok()
            .map(|m| (m.modified().ok(), m.len()));
        if cur == self.stamp {
            return self.last;
        }
        self.stamp = cur;
        match crate::bark::declared_retention(&self.dir, &self.name) {
            Ok(p) => {
                self.warned = false;
                self.last = p;
                p
            }
            Err(e) => {
                if !self.warned {
                    eprintln!(
                        "timberfs: {}: manifest unreadable ({e}); keeping the previous \
                         retention policy (fix it with `timberfs set`)",
                        self.name
                    );
                    self.warned = true;
                }
                self.last
            }
        }
    }
}

fn run_retention(store: &Mutex<Store>, name: &str, policy: crate::bark::Retention) {
    if !policy.is_some() {
        return;
    }
    let result =
        store
            .lock()
            .unwrap()
            .enforce_retention(name, policy.max_age_ms, policy.max_comp_bytes);
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
    // Validate the flags up front; they are persisted below.
    retain.map(parse_duration_ms).transpose()?;
    retain_size.map(parse_size_bytes).transpose()?;
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

    // --retain/--retain-size DECLARE (like import --index): the policy is
    // written into the manifest, and this run — like every writer — reads
    // it from there. Retention is a property of the log, not of whoever
    // happens to be writing it.
    if retain.is_some() || retain_size.is_some() {
        let mut map = crate::bark::load(&dir, &name).unwrap_or_default();
        if let Some(r) = retain {
            map.insert(
                "retain".to_string(),
                serde_json::Value::String(r.to_string()),
            );
        }
        if let Some(r) = retain_size {
            map.insert(
                "retain_size".to_string(),
                serde_json::Value::String(r.to_string()),
            );
        }
        crate::bark::save(&dir, &name, &map)?;
    }
    let policy = Arc::new(Mutex::new(LivePolicy {
        dir: dir.clone(),
        name: name.clone(),
        last: crate::bark::Retention::default(),
        warned: false,
        stamp: None,
    }));

    // Catch up on retention from before this run, then keep enforcing.
    run_retention(&store, &name, policy.lock().unwrap().refresh());

    // Background thread: age-based chunk flushing (same as the mount) and
    // the once-a-second retention check, policy re-read each time.
    {
        let store = Arc::clone(&store);
        let name = name.clone();
        let policy = Arc::clone(&policy);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(1000));
            store.lock().unwrap().flush_aged();
            let p = policy.lock().unwrap().refresh();
            run_retention(&store, &name, p);
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
    let p = policy.lock().unwrap().refresh();
    run_retention(&store, &name, p);
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
