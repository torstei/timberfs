//! `timberfs append`: the FUSE-less write path — read stdin, write chunks
//! straight into the backing store (svlogd/s6-log style):
//!
//! ```text
//! myapp 2>&1 | timberfs append logs-backing/app.log
//! ```
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
use std::path::{Path, PathBuf};
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
pub struct LivePolicy {
    pub dir: std::path::PathBuf,
    pub name: String,
    pub last: crate::bark::Retention,
    pub warned: bool,
    /// (mtime, len) of the manifest at the last parse: the file almost
    /// never changes, so the once-a-second re-read is a stat until it does.
    pub stamp: Option<(Option<std::time::SystemTime>, u64)>,
}

impl LivePolicy {
    pub fn refresh(&mut self) -> crate::bark::Retention {
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

/// Flushed chunks of one store; 0 if it is gone (it never is here — the
/// appender owns it for the whole run).
fn chunk_count(store: &Mutex<Store>, name: &str) -> usize {
    store
        .lock()
        .unwrap()
        .files
        .get(name)
        .map(|f| f.chunks.len())
        .unwrap_or(0)
}

/// Extend the token index if this store declares one. Best-effort: the
/// grain is derived, a chunk it doesn't cover is simply scanned, so a
/// failure here must never fail the append.
fn extend_declared_grain(dir: &Path, name: &str) {
    if crate::bark::index_declared(dir, name) {
        if let Err(e) = crate::grain::extend_grain(dir, name) {
            eprintln!("timberfs: {name}: grain extend failed: {e}");
        }
    }
}

pub fn run_retention(store: &Mutex<Store>, name: &str, policy: crate::bark::Retention) {
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

/// The once-a-second housekeeping a long-lived single-store writer owes:
/// age-based chunk flushing (same as the mount), the retention check with
/// the policy re-read each time, keeping a declared grain current, and a
/// clean exit when this binary is replaced on disk. Shared by the appender
/// and by `import --follow`, which differ only in where their bytes come
/// from.
pub fn spawn_maintenance(
    store: Arc<Mutex<Store>>,
    dir: PathBuf,
    name: String,
    policy: Arc<Mutex<LivePolicy>>,
    exit_on_upgrade: bool,
) {
    let watch = if exit_on_upgrade {
        crate::store::BinaryWatch::current()
    } else {
        None
    };
    let mut indexed_chunks = chunk_count(&store, &name);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(1000));
        // Binary upgraded on disk: flush and exit for a clean re-exec
        // (the supervisor restarts us on the new one).
        if watch.as_ref().is_some_and(|w| w.changed()) {
            store.lock().unwrap().flush_all();
            // Index what we flushed before the re-exec, off the lock:
            // a full rebuild here would otherwise delay shutdown past
            // systemd's stop timeout.
            extend_declared_grain(&dir, &name);
            std::process::exit(crate::store::EXIT_BINARY_UPGRADED);
        }
        store.lock().unwrap().flush_aged();
        store.lock().unwrap().sap_sync_all();
        let p = policy.lock().unwrap().refresh();
        run_retention(&store, &name, p);
        // Keep the declared index current while streaming, exactly as
        // the records sink and the network intakes do: extend whenever
        // the flushed-chunk set changed (a flush added chunks, or
        // retention dropped some and deleted the grain). Deliberately
        // NOT holding the store lock — extend_grain reads only
        // committed, immutable chunks and is the sole writer of the
        // grain, so it cannot race the append thread, which only
        // appends new chunks and never touches the grain.
        let cur = chunk_count(&store, &name);
        if cur != indexed_chunks && crate::bark::index_declared(&dir, &name) {
            match crate::grain::extend_grain(&dir, &name) {
                Ok(()) => indexed_chunks = cur,
                Err(e) => eprintln!("timberfs: {name}: background grain extend failed: {e}"),
            }
        }
    });
}

/// Take a log's writer lock, waiting out a departing writer for up to
/// `wait_secs` (see `store::lock_file_exclusive_waiting`).
pub fn take_writer_lock(
    dir: &Path,
    name: &str,
    wait_secs: f64,
) -> anyhow::Result<Option<std::fs::File>> {
    let wait = Duration::from_millis((wait_secs.max(0.0) * 1000.0) as u64);
    if wait.is_zero() {
        Ok(store::lock_file_exclusive(dir, name)?)
    } else {
        Ok(store::lock_file_exclusive_waiting(dir, name, wait)?)
    }
}

/// The message for a lock we could not get: name the holder if it
/// recorded itself, and say that we waited, so a reload handoff that
/// really is stuck reads differently from two writers started by mistake.
pub fn writer_conflict(dir: &Path, name: &str, wait_secs: f64) -> String {
    let waited = if wait_secs > 0.0 {
        format!(" — still held after waiting {}s", fmt_secs(wait_secs))
    } else {
        String::new()
    };
    match store::describe_file_writer(dir, name) {
        Some(who) => format!(
            "{name} already has a writer: {who}{waited}; one writer per log, \
             so that one must exit before this one can take over"
        ),
        None => format!(
            "{name} already has a writer (another timberfs writer or a \
             rotation){waited}"
        ),
    }
}

/// Seconds without a trailing ".0" on the whole ones.
fn fmt_secs(s: f64) -> String {
    if s.fract() == 0.0 {
        format!("{}", s as u64)
    } else {
        format!("{s}")
    }
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_append(
    target: &Path,
    cfg: Config,
    wal: bool,
    retain: Option<&str>,
    retain_size: Option<&str>,
    exit_on_upgrade: bool,
    wait_for_writer: f64,
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
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating backing directory {}", dir.display()))?;

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
    let file_lock = match take_writer_lock(&dir, &name, wait_for_writer)? {
        Some(f) => f,
        None => bail!(writer_conflict(&dir, &name, wait_for_writer)),
    };
    store::write_lock_info(
        &file_lock,
        &format!("appender pid={}\n", std::process::id()),
    )?;

    // --wal DECLARES, like --index: written into the manifest before this
    // run's own FileStore::open so it takes effect immediately, not just
    // for the next writer.
    if wal {
        crate::bark::declare_wal(&dir, &name)?;
    }
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

    spawn_maintenance(
        Arc::clone(&store),
        dir.clone(),
        name.clone(),
        Arc::clone(&policy),
        exit_on_upgrade,
    );

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
                extend_declared_grain(&dir, &name);
                return Err(e.into());
            }
        }
    }

    store.lock().unwrap().flush_all();
    let p = policy.lock().unwrap().refresh();
    run_retention(&store, &name, p);
    // Last: index everything this run flushed, retention included, so a
    // short-lived appender still leaves a complete index behind.
    extend_declared_grain(&dir, &name);
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
