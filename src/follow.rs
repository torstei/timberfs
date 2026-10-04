//! `import --follow`: consume a log file that someone else writes.
//!
//! ⚠ Direction. This is an INTAKE: it reads a producer's file INTO a store.
//! A FOLLOWER (follower.rs) is the opposite — a registered reader of a
//! store, shipping it OUT. Both run `--follow`, so the verb is honestly
//! shared, but `timberfs-follow@` and `timberfs-follower@` are one letter
//! apart and point opposite ways. The prose below therefore calls this
//! side a TAIL, and reserves "follower" for the registered object.
//!
//! The reason this belongs in timberfs rather than in a `tail -F |` pipeline
//! is that position and rotation ARE the problem, and only the side that owns
//! the store can solve them. Three invariants say how:
//!
//!   * **The store is the checkpoint.** A start finds where the store's data
//!     ends in the file by comparing bytes, and where it cannot tell it
//!     re-syncs against what the store already holds, line by line over the
//!     overlapping window, so a restart can neither lose nor duplicate — and
//!     there is no position file to go stale, be restored out of step, or
//!     disagree with the store.
//!   * **A descriptor is never abandoned before EOF.** When the path is
//!     replaced, the file we still hold is drained first, so rotation cannot
//!     strand the lines written between the last read and the rename.
//!   * **Every position decision is announced.** Which file, how much, and
//!     why — a tail that silently reads the wrong thing is worse than one
//!     that stops.
//!
//! Entries are stamped from their own timestamps, exactly as `import` does.
//! That is what keeps a followed store indistinguishable from an imported one
//! (and re-importable into), where `tail -F | timberfs append` stamps arrival
//! instead and produces a store the two can never reconcile.
//!
//! Wakeups are a `stat` loop, not inotify, deliberately: one stat per second
//! costs nothing, while inotify brings watch limits, no NFS, and event storms
//! to coalesce. `--poll` is what bounds how soon a line reaches the store —
//! and, with a `wal` declared, how soon a reader sees it, since the sap is
//! flushed once per batch and `query --follow` tails it.
//!
//! `--flush-age` means something different here than it does for the appender,
//! which is why the default is a minute rather than five seconds. The
//! appender's input is a pipe, so unflushed data exists only in memory and the
//! flush age bounds what a crash loses. A tail's input is a file that
//! stays on disk, and the store is the checkpoint — a partial chunk lost to a
//! crash is simply re-read — so the age only decides how soon new lines become
//! queryable. Setting it low costs compression instead: a chunk holds whatever
//! arrived within it, and on a quiet log that is too little for zstd to work
//! with (3.1x at five seconds against 7.7x at sixty, one line a second).
//!
//! Which is why `--wal` is the better answer to "I need to see it now": a
//! tail with one appends every line to the sap as it reads it, and
//! `query --follow` tails that, so the store keeps its long flush age and its
//! compression while a reader sees lines within a poll.

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

/// How much of the store's end a file that overlaps it is compared against:
/// uncompressed bytes. It bounds the memory of that comparison.
const DEDUP_WINDOW: u64 = 64 << 20;

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
    /// A fragment of the line now being written has already been committed
    /// (it ran past `LINE_CAP`), so the rest continues it and is not stamped.
    mid_line: bool,
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
            mid_line: false,
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
        if let Ok(m) = open.file.metadata() {
            extractor.anchor_to(&m);
        }
        open.pending.extend_from_slice(&buf[..n]);
        // Commit up to the last newline; keep the rest. What was pending
        // had none, so only a read that brought one needs the search.
        let last = buf[..n]
            .contains(&b'\n')
            .then(|| open.pending.iter().rposition(|b| *b == b'\n'))
            .flatten();
        if let Some(last) = last {
            let complete: Vec<u8> = open.pending.drain(..=last).collect();
            let mut s = store.lock().unwrap();
            let f = s.files.get_mut(name).expect("the store this tail opened");
            for (i, line) in complete.split_inclusive(|b| *b == b'\n').enumerate() {
                let ts = if i == 0 && open.mid_line {
                    None
                } else {
                    extractor.extract(&String::from_utf8_lossy(&line[..line.len().min(256)]))
                };
                stamper.feed(f, line, ts, cfg)?;
            }
            open.mid_line = false;
            // Once per batch, not per line: put what we just read in front
            // of anyone tailing the sap now, rather than at the next
            // maintenance tick. A failure here is reported by that tick's
            // sap_sync, which hits the same file.
            let _ = f.sap_flush();
        }
        // A producer that never writes a newline (a progress bar redrawing
        // with carriage returns) must not grow this without end: what has
        // accumulated is committed as a fragment of the line it belongs to.
        if open.pending.len() >= crate::entry::LINE_CAP {
            let fragment = std::mem::take(&mut open.pending);
            let ts = if open.mid_line {
                None
            } else {
                extractor.extract(&String::from_utf8_lossy(&fragment[..256]))
            };
            let mut s = store.lock().unwrap();
            let f = s.files.get_mut(name).expect("the store this tail opened");
            stamper.feed(f, &fragment, ts, cfg)?;
            open.mid_line = true;
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
    let ts = if std::mem::take(&mut open.mid_line) {
        None
    } else {
        extractor.extract(&String::from_utf8_lossy(&line[..line.len().min(256)]))
    };
    let mut s = store.lock().unwrap();
    let f = s.files.get_mut(name).expect("the store this tail opened");
    stamper.feed(f, &line, ts, cfg)
}

/// Where in `path` the store's data ends, if the store was fed by that file:
/// the store's last bytes are the file's bytes just before that offset, or, when
/// the file's offsets and the store's no longer line up, found by searching the
/// file for them (the flag says which). It
/// decides by bytes and not by timestamps, so it holds for a store whose head
/// retention has dropped; and it never errors on an ambiguity, which would
/// restart a service, only says it cannot tell.
fn resume_offset(
    store: &Mutex<Store>,
    name: &str,
    path: &Path,
) -> anyhow::Result<Option<(u64, bool)>> {
    let src = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut s = store.lock().unwrap();
    let f = s.files.get_mut(name).expect("the store this tail opened");
    if f.size() == 0 {
        return Ok(None);
    }
    if let Some(end) = crate::import::resume_point(f, &src, false)? {
        return Ok(Some((end, false)));
    }
    // Not where the file's offset says: a rotation, a store that lost lines.
    // The store's last bytes are still in the file, so look for them.
    Ok(crate::import::search_point(f, &src)?.map(|end| (end, true)))
}

/// Bring the store up to date with one file, dropping what it already holds.
///
/// This is the resume path, and it is deliberately content-based rather than
/// offset-based. When the store ends where it ends in this file, the bytes say
/// so and it carries on from there. Otherwise the store's own lines over the
/// window this file covers are the checkpoint, so re-reading a file the store
/// already has costs a scan and produces nothing. Returns the offset the file
/// was read to.
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
    if let Some((end, searched)) = resume_offset(store, name, path)? {
        crate::note!(
            "timberfs: {}: {name} ends at byte {end} of it ({size} bytes){}; resuming there",
            path.display(),
            if searched {
                ", found by searching for its last bytes"
            } else {
                ""
            }
        );
        if live {
            return Ok(end);
        }
        let mut open = Open::at(path, end)?;
        let tail = drain(&mut open, store, name, extractor, stamper, cfg)?;
        commit_fragment(&mut open, store, name, extractor, stamper, cfg)?;
        if tail > 0 {
            crate::note!(
                "timberfs: {}: {tail} byte(s) it gained since {name} last read it",
                path.display()
            );
        }
        return Ok(open.offset);
    }
    let t0 = first_stamp(path, extractor)?;
    // What does the store already cover?
    let (store_first, store_last) = {
        let mut s = store.lock().unwrap();
        let f = s.files.get_mut(name).expect("the store this tail opened");
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
    // Lines older than this are what retention dropped from the store's head.
    let mut head = None;
    if let (Some(first), Some(last)) = (store_first, store_last) {
        if t0 < first {
            // Older than anything the store holds: that history is retention's
            // to drop, and appending it now would write backwards along the
            // write axis, which the index is ordered by. Only those lines are
            // left out; what the file has since is still read.
            crate::note!(
                "timberfs: {} starts {} , before the oldest data in {name} ({}); the lines \
                 older than that are what retention dropped and are not imported, the rest are",
                path.display(),
                crate::query::fmt_ms(t0),
                crate::query::fmt_ms(first)
            );
            head = Some(first);
        }
        if t0 <= last {
            let trunk = crate::format::trunk_path(&store.lock().unwrap().dir.clone(), name);
            // Only the store's last stretch is compared against, so this stays
            // bounded however large the store is; a line older than that
            // stretch is imported again, which is the cheaper mistake.
            let (chunks, cut) = {
                let s = store.lock().unwrap();
                let all = &s.files.get(name).unwrap().chunks;
                let (mut start, mut bytes) = (all.len(), 0u64);
                while start > 0 && bytes + all[start - 1].uncomp_len <= DEDUP_WINDOW {
                    start -= 1;
                    bytes += all[start].uncomp_len;
                }
                (all[start..].to_vec(), start > 0)
            };
            if cut {
                crate::note!(
                    "timberfs: {name}: the overlap with {} is larger than {} MiB; lines older than \
                     {} are compared with only the last {} MiB and may be repeated",
                    path.display(),
                    DEDUP_WINDOW >> 20,
                    crate::query::fmt_ms(chunks.first().map(|c| c.first_write_ms).unwrap_or(first)),
                    DEDUP_WINDOW >> 20
                );
            }
            let counts = overlap_line_counts(&chunks, &trunk, t0.max(first))?;
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
    extractor.anchor_to(&open.file.metadata()?);
    let mut skipped = 0u64;
    let mut retained_away = 0u64;
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
        let f = s.files.get_mut(name).expect("the store this tail opened");
        for line in complete.split_inclusive(|b| *b == b'\n') {
            let ts = extractor.extract(&String::from_utf8_lossy(&line[..line.len().min(256)]));
            if head.is_some_and(|h| ts.or(stamper.last_ts()).is_some_and(|e| e < h)) {
                retained_away += 1;
                if let Some(t) = ts {
                    stamper.observe(t);
                }
                continue;
            }
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
    if skipped > 0 || added > 0 || retained_away > 0 {
        crate::note!(
            "timberfs: {}: {added} line(s) new, {skipped} already in {name}{}",
            path.display(),
            if retained_away > 0 {
                format!(", {retained_away} older than {name}'s head (dropped by retention)")
            } else {
                String::new()
            }
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
    store::write_lock_info(&file_lock, &format!("tail pid={}\n", std::process::id()))?;

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

    // Wait for the file rather than failing: a supervised tail may well
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
        fields: Default::default(),
        warned: false,
        stamp: None,
        reparsed: false,
    }));
    {
        let (p, fields) = {
            let mut pol = policy.lock().unwrap();
            (pol.refresh(), pol.fields.clone())
        };
        append::run_retention(
            &store,
            &name,
            p,
            &fields,
            &mut crate::follower::TickInterest::default(),
        );
    }
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
                    open.mid_line = false;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::LINE_CAP;
    use crate::import::Stamper;
    use std::io::Write;

    #[test]
    fn a_producer_that_never_writes_a_newline_is_committed_in_fragments() {
        let dir = std::env::temp_dir().join(format!("timberfs-follow-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("store")).unwrap();
        let path = dir.join("in.log");

        let mut text = b"2026-10-03T10:00:00Z ".to_vec();
        text.resize(2 * LINE_CAP + 777, b'x');
        fs::write(&path, &text).unwrap();

        let cfg = Config {
            chunk_size: 1 << 20,
            level: 1,
            flush_age_ms: u64::MAX,
        };
        let mut st = Store {
            dir: dir.join("store"),
            cfg,
            files: std::collections::BTreeMap::new(),
        };
        st.create("f.log").unwrap();
        let store = Mutex::new(st);
        let extractor = Extractor::new(None, None, false).unwrap();
        let mut stamper = Stamper::resuming_from(None);
        let mut open = Open::at(&path, 0).unwrap();

        drain(&mut open, &store, "f.log", &extractor, &mut stamper, &cfg).unwrap();
        assert!(open.pending.len() < LINE_CAP, "held {}", open.pending.len());
        assert!(open.mid_line, "a fragment of the line is already committed");

        // The producer finishes the line, and writes another.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"2026-10-03T10:00:09Z the end of the long line\n")
            .unwrap();
        f.write_all(b"2026-10-03T10:00:10Z a second line\n")
            .unwrap();
        drop(f);
        drain(&mut open, &store, "f.log", &extractor, &mut stamper, &cfg).unwrap();

        assert!(!open.mid_line && open.pending.is_empty());
        assert_eq!(
            stamper.stamped, 2,
            "the long line, and the next; the tail that merely looks stamped is not"
        );
        let mut s = store.lock().unwrap();
        let f = s.files.get_mut("f.log").unwrap();
        f.flush_chunk(&cfg).unwrap();
        let held: u64 = f.chunks.iter().map(|c| c.uncomp_len).sum();
        assert_eq!(held, fs::metadata(&path).unwrap().len(), "every byte kept");
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_end_of_a_cut_line_is_not_stamped_again() {
        let dir =
            std::env::temp_dir().join(format!("timberfs-follow-test-b-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("store")).unwrap();
        let path = dir.join("in.log");

        // One stamped line that fills the cap exactly, whose end then reads
        // as a timestamp, and then a genuinely new line.
        let mut text = b"2026-10-03T10:00:00Z ".to_vec();
        text.resize(LINE_CAP, b'x');
        text.extend_from_slice(b"2026-10-03T10:00:09Z looks stamped, but ends the long line\n");
        text.extend_from_slice(b"2026-10-03T10:00:10Z a second line\n");
        fs::write(&path, &text).unwrap();

        let cfg = Config {
            chunk_size: 1 << 20,
            level: 1,
            flush_age_ms: u64::MAX,
        };
        let mut st = Store {
            dir: dir.join("store"),
            cfg,
            files: std::collections::BTreeMap::new(),
        };
        st.create("f.log").unwrap();
        let store = Mutex::new(st);
        let extractor = Extractor::new(None, None, false).unwrap();
        let mut stamper = Stamper::resuming_from(None);
        let mut open = Open::at(&path, 0).unwrap();

        drain(&mut open, &store, "f.log", &extractor, &mut stamper, &cfg).unwrap();

        assert_eq!(stamper.stamped, 2, "two lines, not three");
        assert_eq!(stamper.inherited, 1, "the end of the long line inherits");
        assert!(!open.mid_line && open.pending.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stamped lines whose padding is pseudo-random, so a few thousand of them
    /// are several filesystem blocks of compressed data (size retention aims a
    /// couple of blocks below its budget).
    fn block(hour: usize, from: usize, to: usize) -> String {
        let mix = |mut x: u64| {
            x = x.wrapping_add(0x9e3779b97f4a7c15);
            x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
            x ^ (x >> 31)
        };
        (from..to)
            .map(|i| {
                format!(
                    "2026-10-03T{:02}:{:02}:{:02}Z line {i} {:016x}{:016x}{:016x}\n",
                    hour + i / 3600,
                    i / 60 % 60,
                    i % 60,
                    mix(i as u64 * 3),
                    mix(i as u64 * 3 + 1),
                    mix(i as u64 * 3 + 2)
                )
            })
            .collect()
    }

    fn lines(from: usize, to: usize) -> String {
        block(10, from, to)
    }

    fn test_cfg() -> Config {
        Config {
            chunk_size: 4096,
            level: 1,
            flush_age_ms: u64::MAX,
        }
    }

    /// What a process does when it starts: open the store, bring it level with
    /// the rotated files and then the live one, and read on to the end. Returns
    /// the store as a clean stop leaves it.
    fn start(dir: &Path, source: &Path, rotated: &[PathBuf]) -> Mutex<Store> {
        let cfg = test_cfg();
        let mut st = Store {
            dir: dir.join("store"),
            cfg,
            files: std::collections::BTreeMap::new(),
        };
        st.create("f.log").unwrap();
        let last_ts = st.files.get("f.log").and_then(|f| f.last_write_ms());
        let store = Mutex::new(st);
        let extractor = Extractor::new(None, None, false).unwrap();
        let mut stamper = Stamper::resuming_from(last_ts);
        for c in rotated {
            catch_up(&store, "f.log", c, &extractor, &mut stamper, &cfg, false).unwrap();
        }
        let from = catch_up(
            &store,
            "f.log",
            source,
            &extractor,
            &mut stamper,
            &cfg,
            true,
        )
        .unwrap();
        let mut open = Open::at(source, from).unwrap();
        drain(&mut open, &store, "f.log", &extractor, &mut stamper, &cfg).unwrap();
        store.lock().unwrap().flush_all();
        store
    }

    /// Where the store's tape starts, and what it holds.
    fn stored(store: &Mutex<Store>) -> (u64, Vec<u8>) {
        let mut s = store.lock().unwrap();
        let f = s.files.get_mut("f.log").unwrap();
        let size = f.size();
        (f.tape_start(), f.read(0, size as u32).unwrap())
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("timberfs-follow-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("store")).unwrap();
        dir
    }

    fn append_to(path: &Path, text: &str) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    /// What a writer's retention tick does: drop the oldest chunks.
    fn drop_the_head(store: &Mutex<Store>) {
        let mut s = store.lock().unwrap();
        let comp = s.files.get("f.log").unwrap().comp_size;
        s.enforce_retention("f.log", None, Some(comp / 2), None)
            .unwrap();
    }

    #[test]
    fn a_restart_after_retention_dropped_the_head_picks_up_what_was_written_while_down() {
        let dir = scratch("retention");
        let src = dir.join("app.log");
        fs::write(&src, lines(0, 3000)).unwrap();
        let store = start(&dir, &src, &[]);
        drop_the_head(&store);
        assert!(stored(&store).0 > 0, "the head is gone");
        drop(store);

        append_to(&src, &lines(3000, 3100));
        let store = start(&dir, &src, &[]);

        let (start_at, held) = stored(&store);
        let file = fs::read(&src).unwrap();
        assert_eq!(
            &held[..],
            &file[start_at as usize..],
            "the new lines arrived"
        );
        assert_eq!(start_at + held.len() as u64, file.len() as u64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_with_nothing_new_changes_nothing() {
        let dir = scratch("quiet");
        let src = dir.join("app.log");
        fs::write(&src, lines(0, 400)).unwrap();
        let first = stored(&start(&dir, &src, &[]));
        let second = stored(&start(&dir, &src, &[]));
        assert_eq!(first, second);
        assert_eq!(second.1, fs::read(&src).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_still_being_written_when_it_stopped_arrives_whole() {
        let dir = scratch("partial");
        let src = dir.join("app.log");
        let mut text = lines(0, 400);
        text.push_str("2026-10-03T11:00:00Z a line still being writ");
        fs::write(&src, &text).unwrap();
        start(&dir, &src, &[]);

        append_to(&src, "ten when it stopped\n");
        append_to(&src, &lines(400, 460));
        let store = start(&dir, &src, &[]);
        assert_eq!(stored(&store).1, fs::read(&src).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_rotated_while_down_is_finished_and_then_the_new_one_is_read() {
        let dir = scratch("rotated");
        let src = dir.join("app.log");
        fs::write(&src, lines(0, 3000)).unwrap();
        let store = start(&dir, &src, &[]);
        drop_the_head(&store);
        drop(store);

        // While it was down: the file gained lines, was rotated, and a new
        // one began.
        append_to(&src, &lines(3000, 3100));
        let rotated = dir.join("app.log.1");
        fs::rename(&src, &rotated).unwrap();
        fs::write(&src, block(20, 0, 200)).unwrap();

        let store = start(&dir, &src, std::slice::from_ref(&rotated));
        let (start_at, held) = stored(&store);
        let want = format!(
            "{}{}",
            fs::read_to_string(&rotated).unwrap(),
            fs::read_to_string(&src).unwrap()
        );
        assert_eq!(&held[..], &want.as_bytes()[start_at as usize..]);
        assert_eq!(start_at + held.len() as u64, want.len() as u64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_does_not_continue_the_store_is_resynced_line_by_line_as_before() {
        let dir = scratch("overlap");
        let (a, b) = (dir.join("a.log"), dir.join("b.log"));
        fs::write(&a, lines(0, 1500)).unwrap();
        start(&dir, &a, &[]);

        // Another file that begins inside what the store holds.
        fs::write(&b, lines(1490, 2000)).unwrap();
        let store = start(&dir, &b, &[]);
        let want = format!("{}{}", lines(0, 1500), lines(1500, 2000));
        assert_eq!(stored(&store).1, want.as_bytes());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_that_lost_lines_resumes_after_its_tail_and_loses_no_more() {
        let dir = scratch("gap");
        // What an earlier restart left behind: the file, minus 100 lines in the middle.
        let part = dir.join("part.log");
        fs::write(&part, format!("{}{}", lines(0, 3000), lines(3100, 3200))).unwrap();
        let store = start(&dir, &part, &[]);
        drop_the_head(&store);
        assert!(stored(&store).0 > 0, "the head is gone");
        let held_before = stored(&store).1;
        drop(store);

        // The real file has the gap's lines too, and 100 more written while down.
        let src = dir.join("app.log");
        fs::write(&src, lines(0, 3300)).unwrap();
        let store = start(&dir, &src, &[]);

        let (_, held) = stored(&store);
        assert_eq!(
            held,
            [held_before, lines(3200, 3300).into_bytes()].concat(),
            "the new lines arrived, and nothing else did"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn after_a_rotation_the_new_file_is_found_by_the_stores_tail() {
        let dir = scratch("rotation-search");
        let src = dir.join("app.log");
        fs::write(&src, lines(0, 1500)).unwrap();
        start(&dir, &src, &[]);
        let rotated = dir.join("app.log.1");
        fs::rename(&src, &rotated).unwrap();
        fs::write(&src, block(20, 0, 300)).unwrap();
        let store = start(&dir, &src, std::slice::from_ref(&rotated));
        let imported = block(20, 0, 300).len() as u64;

        // The store now ends in the middle of the new file, and holds the old
        // one before it, so the file's offsets and the store's do not line up.
        append_to(&src, &block(20, 300, 400));
        assert_eq!(
            resume_offset(&store, "f.log", &src).unwrap(),
            Some((imported, true)),
            "found by searching, not by the offset"
        );

        let store = start(&dir, &src, std::slice::from_ref(&rotated));
        let want = format!("{}{}", lines(0, 1500), block(20, 0, 400));
        assert_eq!(stored(&store).1, want.as_bytes());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_without_the_tail_drops_only_what_retention_dropped() {
        let dir = scratch("no-tail");
        let src = dir.join("app.log");
        fs::write(&src, lines(0, 3000)).unwrap();
        let store = start(&dir, &src, &[]);
        drop_the_head(&store);
        let retained = String::from_utf8(stored(&store).1).unwrap();
        assert!(stored(&store).0 > 0, "the head is gone");
        drop(store);

        // The file was rewritten: its last line differs, so the store's tail is
        // nowhere in it. It also still begins before the store does.
        let mut text = lines(0, 3100).into_bytes();
        let end_of_2999 = lines(0, 3000).len() - 2;
        text[end_of_2999] = b'Z';
        fs::write(&src, &text).unwrap();
        let store = start(&dir, &src, &[]);

        let changed =
            String::from_utf8(text[lines(0, 2999).len()..lines(0, 3000).len()].to_vec()).unwrap();
        let want = format!("{retained}{changed}{}", lines(3000, 3100));
        assert_eq!(
            String::from_utf8(stored(&store).1).unwrap(),
            want,
            "the history retention dropped is not brought back, and the new lines are read"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
