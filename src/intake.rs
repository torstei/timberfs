//! The receiver core shared by every network intake: the stores it
//! writes, one writer lock each, and the maintenance tick that keeps them
//! flushed, retained and indexed.
//!
//! Each store gets its own directory, `<root>/<name>/<name>.log` — the
//! layout a systemd intake template writes too. A store's path spells the
//! store's name, which is also its query handle, and never the protocol
//! that delivered it. Per store rather than per protocol because the
//! directory is the boundary that matters: a writer needs permission on
//! it, it carries the mount exclusion, and it is what one owner can be
//! given.
//!
//! A protocol module supplies only what is protocol-specific — how bytes
//! become entries, how a store is named, what a manifest is seeded with,
//! and what an acknowledgement means on its wire. Everything below is the
//! same for the Fluentd Forward receiver and the OTLP one, and would be
//! the same for the next.
//!
//! `Intake<X>` carries a protocol's own state in `extra` deliberately,
//! rather than beside the struct: it sits under the SAME mutex as the
//! store, which is what lets a receiver hold the store lock across a sync
//! and then answer every acknowledgement that lock covered.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};

use crate::store::{self, Config, Store};

/// Every store one receiver writes, plus whatever that protocol needs to
/// keep under the same lock.
pub struct Intake<X = ()> {
    /// Where this receiver creates each store's own directory.
    pub root: PathBuf,
    pub cfg: Config,
    /// One store per logical name, alone in its own directory, keyed by
    /// that name — so the map, the lock maps and the wire all say the same
    /// word.
    pub stores: BTreeMap<String, Store>,
    /// The shared backing-directory lock of each store's directory (the
    /// mount exclusion) and its exclusive per-file writer lock. Kept alive
    /// for as long as this receiver writes that store; dropped (and so
    /// released) only on process exit.
    pub dir_locks: BTreeMap<String, File>,
    pub file_locks: BTreeMap<String, File>,
    /// Stores that could not be opened (typically: undeclared, with no
    /// --auto-create) — remembered so the refusal is logged once, not
    /// once per record of a retrying sender.
    pub refused: BTreeSet<String>,
    pub extra: X,
}

impl<X> Intake<X> {
    pub fn new(root: &Path, cfg: Config, extra: X) -> Intake<X> {
        Intake {
            root: root.to_path_buf(),
            cfg,
            stores: BTreeMap::new(),
            dir_locks: BTreeMap::new(),
            file_locks: BTreeMap::new(),
            refused: BTreeSet::new(),
            extra,
        }
    }

    /// The one file of the store called `name`, when this receiver has it
    /// open. Every protocol appends, syncs and acks through this.
    pub fn file(&mut self, name: &str) -> Option<&mut store::FileStore> {
        self.stores.get_mut(name)?.files.get_mut(name)
    }

    /// Whether this receiver has that store open — the question an ack
    /// asks: never acknowledge what has no store.
    pub fn holds(&self, name: &str) -> bool {
        self.stores.contains_key(name)
    }

    /// Every store open right now, for a tick that walks them.
    pub fn names(&self) -> Vec<String> {
        self.stores.keys().cloned().collect()
    }

    pub fn flush_all(&mut self) {
        for s in self.stores.values_mut() {
            s.flush_all();
        }
    }

    pub fn flush_aged(&mut self) {
        for s in self.stores.values_mut() {
            s.flush_aged();
        }
    }

    pub fn sap_sync_all(&mut self) {
        for s in self.stores.values_mut() {
            s.sap_sync_all();
        }
    }

    /// Apply `set wal=true|false` to every store this receiver writes —
    /// the same no-restart contract the appender has, and the one that
    /// counts here: a receiver is restarted only by dropping its
    /// senders' connections.
    pub fn sync_wal_declarations(&mut self) {
        for s in self.stores.values_mut() {
            s.sync_wal_declarations();
        }
    }

    pub fn enforce_retention(
        &mut self,
        name: &str,
        max_age_ms: Option<u64>,
        max_comp_bytes: Option<u64>,
        interest_floor: Option<u64>,
    ) -> std::io::Result<Option<store::RotateStats>> {
        match self.stores.get_mut(name) {
            Some(s) => s.enforce_retention(name, max_age_ms, max_comp_bytes, interest_floor),
            None => Ok(None),
        }
    }

    /// The next chunk number of one of the receiver's stores — what the
    /// interest axis needs to call a claimed position impossible.
    pub fn next_seq(&self, name: &str) -> Option<u64> {
        self.stores.get(name).and_then(|s| s.next_seq(name))
    }
}

/// A store's own directory under a receiver's root: the store's name
/// minus the trailing `.log` its logical name carries, which is exactly
/// the handle `timberfs query` resolves it by.
pub fn store_dir(root: &Path, name: &str) -> PathBuf {
    root.join(name.strip_suffix(".log").unwrap_or(name))
}

/// Take a backing directory a receiver writes in: create it, and refuse
/// one a mount already serves (two writers, one directory). Used twice —
/// once for the root at startup, so a misconfigured `--into-dir` fails
/// immediately, and once per store directory as it is created.
pub fn open_backing_dir(dir: &Path) -> anyhow::Result<File> {
    fs::create_dir_all(dir)
        .with_context(|| format!("creating backing directory {}", dir.display()))?;
    match store::lock_backing_shared(dir)? {
        Some(f) => Ok(f),
        None => {
            let mounted = store::read_lock_mountpoint(dir)
                .map(|m| format!(" (mounted on {})", m.display()))
                .unwrap_or_default();
            bail!(
                "backing directory {} is served by a timberfs mount{mounted}; \
                 write through the mount instead, or unmount first",
                dir.display()
            );
        }
    }
}

/// Lazily open a store on first use: take its exclusive per-file lock,
/// open it, and — only the very first time it is created on disk — seed
/// its manifest.
///
/// Creation is the operator's decision, not the network's: an undeclared
/// store is refused unless `auto_create`, so an acking sender buffers and
/// retries until it exists and provisioning converges with nothing lost.
/// `subject` names what the network asked for, in that refusal.
///
/// Every store a receiver writes declares `wal`: an acknowledgement means
/// durable, and the sap is what makes that cost one fsync instead of one
/// chunk per record.
pub fn ensure_store<X>(
    intake: &mut Intake<X>,
    name: &str,
    subject: &str,
    auto_create: bool,
    seed: impl FnOnce(&Path, &str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if intake.stores.contains_key(name) {
        return Ok(());
    }
    let dir = store_dir(&intake.root, name);
    // Refuse BEFORE anything is created: neither a lock file (they are
    // never unlinked) nor the store's directory, so a tag nobody declared
    // leaves no litter at all.
    let brand_new = !crate::format::rings_path(&dir, name).exists();
    if brand_new && !auto_create {
        let path = dir.join(name);
        bail!(
            "{subject}: no store {} — pre-create it \
             (timberfs create --wal {}) or run with --auto-create",
            path.display(),
            path.display()
        );
    }
    let dir_lock = open_backing_dir(&dir)?;
    let lock = store::lock_file_exclusive(&dir, name)?.ok_or_else(|| {
        anyhow::anyhow!("{name} already has a writer (another timberfs writer or a rotation)")
    })?;
    store::write_lock_info(&lock, &format!("intake pid={}\n", std::process::id()))?;
    // Declare before create: open() reads the manifest to decide whether
    // to mint the sap, and the ack contract needs it live from the start.
    if brand_new {
        seed(&dir, name)?;
    } else if !crate::bark::wal_declared(&dir, name) {
        crate::bark::declare_wal(&dir, name)?;
    }
    let mut opened = Store {
        dir,
        cfg: intake.cfg,
        files: BTreeMap::new(),
    };
    opened.create(name)?;
    intake.stores.insert(name.to_string(), opened);
    intake.dir_locks.insert(name.to_string(), dir_lock);
    intake.file_locks.insert(name.to_string(), lock);
    Ok(())
}

/// The once-a-second housekeeping every receiver needs: flush aged
/// chunks, sync the sap (un-acked traffic still gets the wal's ≤1s
/// power-loss window), enforce declared retention, extend the token index
/// for stores that declare one, and exit cleanly on SIGTERM or a binary
/// upgrade — flushing everything first.
///
/// `after_tick` is where a protocol completes whatever its own tick owes
/// (Forward retries acks a transient failure left pending); it runs with
/// the lock released, holding only the store names the tick saw.
pub fn spawn_maintenance<X, F>(
    intake: Arc<Mutex<Intake<X>>>,
    stop: Arc<AtomicBool>,
    exit_on_upgrade: bool,
    after_tick: F,
) -> thread::JoinHandle<()>
where
    X: Send + 'static,
    F: Fn(&Arc<Mutex<Intake<X>>>, &[String]) + Send + 'static,
{
    let watch = if exit_on_upgrade {
        store::BinaryWatch::current()
    } else {
        None
    };
    let root = intake.lock().unwrap().root.clone();
    let mut indexed_chunks: BTreeMap<String, usize> = BTreeMap::new();
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(1000));
            let graceful = if crate::append::stopping() {
                Some(0)
            } else if watch.as_ref().is_some_and(|w| w.changed()) {
                Some(store::EXIT_BINARY_UPGRADED)
            } else {
                None
            };
            if let Some(code) = graceful {
                let names: Vec<String> = {
                    let mut g = intake.lock().unwrap();
                    g.flush_all();
                    g.names()
                };
                for name in &names {
                    let dir = store_dir(&root, name);
                    if crate::bark::index_declared(&dir, name) {
                        let _ = crate::grain::extend_grain(&dir, name);
                    }
                }
                std::process::exit(code);
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let names: Vec<String> = {
                let mut g = intake.lock().unwrap();
                g.flush_aged();
                g.sap_sync_all();
                g.sync_wal_declarations();
                g.names()
            };
            // One per tick, however many tags this receiver has fanned
            // out to: the registry is read at most once, and only if some
            // store declares the axis.
            let mut interest = crate::follower::TickInterest::default();
            for name in &names {
                let dir = store_dir(&root, name);
                match crate::bark::declared_retention(&dir, name) {
                    Ok(policy) if policy.is_some() => {
                        let anchor = crate::follower::anchor_of(&dir, name);
                        let next_seq = intake.lock().unwrap().next_seq(name).unwrap_or(0);
                        let held = interest.floor(&policy, &anchor, next_seq);
                        let res = intake.lock().unwrap().enforce_retention(
                            name,
                            policy.max_age_ms,
                            policy.max_comp_bytes,
                            held.floor,
                        );
                        match res {
                            Err(e) => {
                                eprintln!("timberfs: {name}: background retention failed: {e}")
                            }
                            Ok(Some(stats)) => {
                                if let Some(record) =
                                    crate::follower::override_record(name, &policy, &stats, &held)
                                {
                                    eprintln!("{record}");
                                }
                            }
                            Ok(None) => {}
                        }
                    }
                    _ => {}
                }
                let cur = intake
                    .lock()
                    .unwrap()
                    .file(name)
                    .map(|f| f.chunks.len())
                    .unwrap_or(0);
                if cur != *indexed_chunks.get(name).unwrap_or(&0)
                    && crate::bark::index_declared(&dir, name)
                {
                    match crate::grain::extend_grain(&dir, name) {
                        Ok(()) => {
                            indexed_chunks.insert(name.clone(), cur);
                        }
                        Err(e) => {
                            eprintln!("timberfs: {name}: background grain extend failed: {e}")
                        }
                    }
                }
            }

            after_tick(&intake, &names);
        }
    })
}

/// A systemd-passed listening socket, when this process was socket-
/// activated: fd 3 is the first inherited descriptor by convention.
pub fn socket_activated_listener() -> Option<std::net::TcpListener> {
    use std::os::unix::io::FromRawFd;
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    if pid != std::process::id() {
        return None;
    }
    let fds: usize = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if fds < 1 {
        return None;
    }
    Some(unsafe { std::net::TcpListener::from_raw_fd(3) })
}

/// A network-supplied name mapped to a store filename: keep
/// `[A-Za-z0-9._-]`, replace everything else with `_`, strip leading
/// dots, empty -> `untagged`. This mapping must be structurally unable to
/// escape the backing directory: no `/` ever survives, and a stripped
/// leading dot rules out a bare `..` component.
pub fn sanitize_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.starts_with('.') {
        out.remove(0);
    }
    if out.is_empty() {
        out = "untagged".to_string();
    }
    out
}

/// The store's logical name for a network name: `<sanitized>.log`, so a
/// store a receiver creates reads exactly like every other timberfs log.
pub fn store_name(name: &str) -> String {
    format!("{}.log", sanitize_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_sanitization_keeps_safe_charset() {
        assert_eq!(sanitize_name("app.log"), "app.log");
        assert_eq!(sanitize_name("nginx-access_01"), "nginx-access_01");
        assert_eq!(sanitize_name("weird tag!"), "weird_tag_");
        // What an OTLP service.name looks like when it is a URL or a k8s
        // path: still one component, still no separator.
        assert_eq!(sanitize_name("checkout/v2"), "checkout_v2");
    }

    #[test]
    fn name_sanitization_blocks_traversal() {
        // '/' is replaced everywhere, so the result is always a single path
        // component relative to the backing directory — no traversal, no
        // matter what dots remain in the middle — and a leading run of dots
        // (a bare ".." included) is stripped outright.
        assert_eq!(sanitize_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_name(".."), "untagged");
        assert_eq!(sanitize_name("../app"), "_app");
        assert_eq!(sanitize_name(""), "untagged");
        assert!(!sanitize_name("../../etc/passwd").contains('/'));
    }

    #[test]
    fn a_store_lives_in_a_directory_named_after_it() {
        let root = Path::new("/var/log/timberfs");
        // The directory is the handle: path, handle and wire name agree,
        // and the protocol that delivered it appears nowhere.
        assert_eq!(
            store_dir(root, "nginx.log"),
            Path::new("/var/log/timberfs/nginx")
        );
        // Only a trailing `.log` is stripped, the same rule the forest's
        // handle uses, so a store keeps whatever else its name carries.
        assert_eq!(
            store_dir(root, "metrics.jsonl"),
            Path::new("/var/log/timberfs/metrics.jsonl")
        );
        // A name off the wire is sanitized into one component first, so
        // the directory cannot escape the root either.
        assert_eq!(
            store_dir(root, &store_name("../../etc/passwd")),
            Path::new("/var/log/timberfs/_.._etc_passwd")
        );
    }

    #[test]
    fn store_name_appends_dot_log() {
        assert_eq!(store_name("nginx"), "nginx.log");
        assert_eq!(store_name("app.log"), "app.log.log");
        assert_eq!(store_name("../../etc/passwd"), "_.._etc_passwd.log");
    }
}
