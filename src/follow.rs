//! `import --follow`: consume a log file that someone else writes.
//!
//! The reason this belongs in timberfs rather than in a `tail -F |` pipeline
//! is that position and rotation ARE the problem, and only the side that owns
//! the store can solve them. Three invariants say how:
//!
//!   * **The store is the checkpoint.** A start re-syncs against what the
//!     store already holds, line by line over the overlapping window, so a
//!     restart can neither lose nor duplicate — and there is no position file
//!     to go stale, be restored out of step, or disagree with the store.
//!   * **A descriptor is never abandoned before EOF.** When the path is
//!     replaced, the file we still hold is drained first, so rotation cannot
//!     strand the lines written between the last read and the rename.
//!   * **Every position decision is announced.** Which file, how much, and
//!     why — a follower that silently reads the wrong thing is worse than one
//!     that stops.
//!
//! Entries are stamped from their own timestamps, exactly as `import` does.
//! That is what keeps a followed store indistinguishable from an imported one
//! (and re-importable into), where `tail -F | timberfs append` stamps arrival
//! instead and produces a store the two can never reconcile.
//!
//! Wakeups are a `stat` loop, not inotify, deliberately: a flushed chunk is
//! the unit of visibility, so nothing observable improves below `--flush-age`,
//! and one stat per second costs nothing while inotify brings watch limits,
//! no NFS, and event storms to coalesce.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};

use crate::append::{self, LivePolicy};
use crate::import::{first_stamp, line_hash, overlap_line_counts, Extractor, ImportOpts, Stamper};
use crate::query::{ensure_dest_is_not_plain_file, resolve_backing};
use crate::store::{self, Config, Store};

pub struct FollowOpts {
    /// How often to look for new data (and for a replaced file).
    pub poll_ms: u64,
    /// Where rotation moves the file, for the ONE case the live path cannot
    /// answer: data written while this process was not running. Empty means
    /// the derived defaults.
    pub rotated: Vec<PathBuf>,
    pub exit_on_upgrade: bool,
    pub wait_for_writer: f64,
}

/// Where a rotation is likely to have put the previous file. Only used at
/// startup, and only to find data the live path can no longer reach: while
/// running, rotation needs no pattern at all, because the descriptor we hold
/// still points at the file that moved.
fn rotated_candidates(source: &Path, given: &[PathBuf]) -> Vec<PathBuf> {
    if !given.is_empty() {
        return given.to_vec();
    }
    let name = source.file_name().map(|n| n.to_owned()).unwrap_or_default();
    let dir = source.parent().unwrap_or(Path::new("."));
    // logrotate's default numbering, oldest of the two first: a start that
    // missed two rotations still stitches them in order.
    [".1", ".0"]
        .iter()
        .map(|suffix| {
            let mut n = name.clone();
            n.push(suffix);
            dir.join(n)
        })
        .filter(|p| p.exists())
        .rev()
        .collect()
}

/// A file we are reading, with the identity that tells us when it has been
/// replaced under us and the partial trailing line we must not commit yet.
struct Open {
    file: File,
    dev: u64,
    ino: u64,
    /// Bytes consumed from THIS file, so a shrinking size means truncation.
    offset: u64,
    /// A tail without its newline: the producer is mid-write.
    pending: Vec<u8>,
}

impl Open {
    fn at(path: &Path, from: u64) -> anyhow::Result<Open> {
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let m = file.metadata()?;
        file.seek(SeekFrom::Start(from))?;
        Ok(Open {
            file,
            dev: m.dev(),
            ino: m.ino(),
            offset: from,
            pending: Vec::new(),
        })
    }
}

/// Read what is there and commit every COMPLETE line; a trailing fragment is
/// held for the next read. Returns bytes consumed.
fn drain(
    open: &mut Open,
    store: &Mutex<Store>,
    name: &str,
    extractor: &Extractor,
    stamper: &mut Stamper,
    cfg: &Config,
) -> anyhow::Result<u64> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut consumed = 0u64;
    loop {
        let n = open.file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        consumed += n as u64;
        open.offset += n as u64;
        open.pending.extend_from_slice(&buf[..n]);
        // Commit up to the last newline; keep the rest.
        if let Some(last) = open.pending.iter().rposition(|b| *b == b'\n') {
            let complete: Vec<u8> = open.pending.drain(..=last).collect();
            let mut s = store.lock().unwrap();
            let f = s
                .files
                .get_mut(name)
                .expect("the store this follower opened");
            for line in complete.split_inclusive(|b| *b == b'\n') {
                let ts = extractor.extract(&String::from_utf8_lossy(&line[..line.len().min(256)]));
                stamper.feed(f, line, ts, cfg)?;
            }
        }
    }
    Ok(consumed)
}

/// The last line of a file that will never grow again (it has been rotated
/// away) is complete whether or not it ends in a newline.
fn commit_fragment(
    open: &mut Open,
    store: &Mutex<Store>,
    name: &str,
    extractor: &Extractor,
    stamper: &mut Stamper,
    cfg: &Config,
) -> anyhow::Result<()> {
    if open.pending.is_empty() {
        return Ok(());
    }
    let line = std::mem::take(&mut open.pending);
    crate::note!(
        "timberfs: committing a final {} byte(s) that never got their newline \
         (the file was replaced mid-line)",
        line.len()
    );
    let ts = extractor.extract(&String::from_utf8_lossy(&line[..line.len().min(256)]));
    let mut s = store.lock().unwrap();
    let f = s
        .files
        .get_mut(name)
        .expect("the store this follower opened");
    stamper.feed(f, &line, ts, cfg)
}

/// Bring the store up to date with one file, dropping what it already holds.
///
/// This is the resume path, and it is deliberately content-based rather than
/// offset-based: the store's own lines over the window this file covers are
/// the checkpoint, so re-reading a file the store already has costs a scan
/// and produces nothing. Returns the offset the file was read to.
fn catch_up(
    store: &Mutex<Store>,
    name: &str,
    path: &Path,
    extractor: &Extractor,
    stamper: &mut Stamper,
    cfg: &Config,
    live: bool,
) -> anyhow::Result<u64> {
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Ok(0);
    }
    let t0 = first_stamp(path, extractor)?;
    // What does the store already cover?
    let (store_first, store_last) = {
        let mut s = store.lock().unwrap();
        let f = s
            .files
            .get_mut(name)
            .expect("the store this follower opened");
        if f.size() > 0 {
            // Buffered lines must be on disk to be comparable.
            f.flush_chunk(cfg)?;
        }
        (
            f.chunks.first().map(|c| c.first_write_ms),
            f.last_write_ms(),
        )
    };

    let mut dedup = None;
    if let (Some(first), Some(last)) = (store_first, store_last) {
        if t0 < first {
            // Older than anything the store holds. Either retention dropped
            // that history or it was never imported; either way appending it
            // now would write backwards along the write axis, which the index
            // is ordered by. Say so and skip rather than wedge a service.
            crate::note!(
                "timberfs: skipping {} — it starts {} , before the oldest data in {name} ({}); \
                 a store's write axis only moves forward, so import that history \
                 into a store of its own if you need it",
                path.display(),
                crate::query::fmt_ms(t0),
                crate::query::fmt_ms(first)
            );
            return Ok(size);
        }
        if t0 <= last {
            let trunk = crate::format::trunk_path(&store.lock().unwrap().dir.clone(), name);
            let chunks = {
                let s = store.lock().unwrap();
                s.files.get(name).unwrap().chunks.clone()
            };
            let counts = overlap_line_counts(&chunks, &trunk, t0)?;
            crate::note!(
                "timberfs: {} overlaps what {name} already holds (through {}) — \
                 re-syncing against the store, line by line",
                path.display(),
                crate::query::fmt_ms(last)
            );
            dedup = Some((counts, last));
        }
    }

    // Stream the file, dropping lines the store already has.
    let mut open = Open::at(path, 0)?;
    let mut skipped = 0u64;
    let mut added = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = open.file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        open.offset += n as u64;
        open.pending.extend_from_slice(&buf[..n]);
        let Some(last_nl) = open.pending.iter().rposition(|b| *b == b'\n') else {
            continue;
        };
        let complete: Vec<u8> = open.pending.drain(..=last_nl).collect();
        let mut s = store.lock().unwrap();
        let f = s
            .files
            .get_mut(name)
            .expect("the store this follower opened");
        for line in complete.split_inclusive(|b| *b == b'\n') {
            let ts = extractor.extract(&String::from_utf8_lossy(&line[..line.len().min(256)]));
            if let Some((counts, until)) = dedup.as_mut() {
                if ts.or(stamper.last_ts()).is_some_and(|e| e > *until) {
                    dedup = None; // past the overlap: everything else is new
                } else if let Some(c) = counts.get_mut(&line_hash(line)) {
                    if *c > 0 {
                        *c -= 1;
                        skipped += 1;
                        if let Some(t) = ts {
                            stamper.observe(t);
                        }
                        continue;
                    }
                }
            }
            stamper.feed(f, line, ts, cfg)?;
            added += 1;
        }
    }
    // A fragment at the end of the LIVE file is a line still being written;
    // leave it for the next read (or the next run — the store's own lines are
    // what we resume against, so nothing is lost by not committing it).
    if !live {
        commit_fragment(&mut open, store, name, extractor, stamper, cfg)?;
    }
    if skipped > 0 || added > 0 {
        crate::note!(
            "timberfs: {}: {added} line(s) new, {skipped} already in {name}",
            path.display()
        );
    }
    Ok(open.offset - open.pending.len() as u64)
}

/// `timberfs import --follow`: keep one store level with a file another
/// program writes, across that file's rotations and this process's restarts.
#[allow(clippy::too_many_arguments)]
pub fn cmd_follow(
    source: &Path,
    dest: &Path,
    cfg: Config,
    iopts: ImportOpts,
    retain: Option<&str>,
    retain_size: Option<&str>,
    fopts: FollowOpts,
) -> anyhow::Result<()> {
    retain.map(append::parse_duration_ms).transpose()?;
    retain_size.map(append::parse_size_bytes).transpose()?;
    if crate::query::is_bundle(dest) {
        bail!(
            "{} is a .timber transfer bundle — bundles are read-only; \
             follow into a log instead",
            dest.display()
        );
    }
    ensure_dest_is_not_plain_file(dest, "follow")?;
    let (dir, name) = resolve_backing(dest)?;
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
    let file_lock = match append::take_writer_lock(&dir, &name, fopts.wait_for_writer)? {
        Some(f) => f,
        None => bail!(append::writer_conflict(&dir, &name, fopts.wait_for_writer)),
    };
    store::write_lock_info(
        &file_lock,
        &format!("follower pid={}\n", std::process::id()),
    )?;

    // Declared, exactly as import and the appender declare them: the manifest
    // is what every later writer reads.
    if iopts.index {
        crate::bark::declare_index(&dir, &name)?;
    }
    if iopts.wal {
        crate::bark::declare_wal(&dir, &name)?;
    }
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

    let declared = crate::bark::time_format(crate::bark::load(&dir, &name).as_ref());
    let extractor = Extractor::new(
        iopts.time.regex.as_deref().or(declared.regex.as_deref()),
        iopts.time.format.as_deref().or(declared.format.as_deref()),
        iopts.time.utc || declared.utc,
    )?;

    let mut st = Store {
        dir: dir.clone(),
        cfg,
        files: BTreeMap::new(),
    };
    st.create(&name)?;
    let last_ts = st.files.get(&name).and_then(|f| f.last_write_ms());
    let store = Arc::new(Mutex::new(st));
    let mut stamper = Stamper::resuming_from(last_ts);

    append::install_signal_handlers();
    crate::note!(
        "timberfs: following {} into {}/{} (poll {} ms, chunk {} B, zstd -{}, flush age {} ms)",
        source.display(),
        dir.display(),
        name,
        fopts.poll_ms,
        cfg.chunk_size,
        cfg.level,
        cfg.flush_age_ms
    );

    // Wait for the file rather than failing: a supervised follower may well
    // start before the producer that writes its log.
    let mut waited = false;
    while !source.exists() {
        if append::stopping() {
            return Ok(());
        }
        if !waited {
            crate::note!(
                "timberfs: {} does not exist yet; waiting for it",
                source.display()
            );
            waited = true;
        }
        std::thread::sleep(Duration::from_millis(fopts.poll_ms.max(100)));
    }

    // Catch up on data this process was not running for: what rotation moved
    // out of the way first (oldest first), then the live file.
    for c in rotated_candidates(source, &fopts.rotated) {
        catch_up(&store, &name, &c, &extractor, &mut stamper, &cfg, false)?;
    }
    let from = catch_up(&store, &name, source, &extractor, &mut stamper, &cfg, true)?;
    let mut open = Open::at(source, from)?;

    let policy = Arc::new(Mutex::new(LivePolicy {
        dir: dir.clone(),
        name: name.clone(),
        last: crate::bark::Retention::default(),
        warned: false,
        stamp: None,
    }));
    append::run_retention(&store, &name, policy.lock().unwrap().refresh());
    append::spawn_maintenance(
        Arc::clone(&store),
        dir.clone(),
        name.clone(),
        Arc::clone(&policy),
        fopts.exit_on_upgrade,
    );

    let mut total = 0u64;
    let mut missing_noted = false;
    while !append::stopping() {
        // Has the path stopped being the file we hold?
        match fs::metadata(source) {
            Ok(m) => {
                missing_noted = false;
                if (m.dev(), m.ino()) != (open.dev, open.ino) {
                    // Rotation. Finish the file we hold BEFORE looking at the
                    // new one: its tail is the data a `tail -F` loses.
                    let tail = drain(&mut open, &store, &name, &extractor, &mut stamper, &cfg)?;
                    total += tail;
                    commit_fragment(&mut open, &store, &name, &extractor, &mut stamper, &cfg)?;
                    crate::note!(
                        "timberfs: {} was replaced (rotation); drained its last {tail} byte(s) \
                         and switched to the new file",
                        source.display()
                    );
                    open = Open::at(source, 0)?;
                } else if m.len() < open.offset {
                    crate::note!(
                        "timberfs: {} shrank from {} to {} bytes (copytruncate?); re-reading it \
                         from the start — whatever was written between the copy and the truncate \
                         is lost to every reader, not just this one",
                        source.display(),
                        open.offset,
                        m.len()
                    );
                    open.file.seek(SeekFrom::Start(0))?;
                    open.offset = 0;
                    open.pending.clear();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Rotated away and not yet recreated: the descriptor we hold
                // is still the right place to read from.
                if !missing_noted {
                    crate::note!(
                        "timberfs: {} is gone for now; still reading the file it named",
                        source.display()
                    );
                    missing_noted = true;
                }
            }
            Err(e) => return Err(e).context(format!("stat {}", source.display())),
        }

        let n = drain(&mut open, &store, &name, &extractor, &mut stamper, &cfg)?;
        total += n;
        if n == 0 {
            std::thread::sleep(Duration::from_millis(fopts.poll_ms));
        }
    }

    // Stopped: commit what is committable and make it durable. A trailing
    // fragment is deliberately left — the next run resumes against the
    // store's own lines, so it arrives complete rather than split in two.
    store.lock().unwrap().flush_all();
    if crate::bark::index_declared(&dir, &name) {
        let _ = crate::grain::extend_grain(&dir, &name);
    }
    crate::note!(
        "timberfs: stopped following {}; {total} byte(s) this run, {} entries stamped, \
         {} inherited{}",
        source.display(),
        stamper.stamped,
        stamper.inherited,
        if open.pending.is_empty() {
            String::new()
        } else {
            format!(
                " ({} byte(s) of an unfinished line left for the next run)",
                open.pending.len()
            )
        }
    );
    Ok(())
}
