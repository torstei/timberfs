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
use std::path::PathBuf;

use anyhow::{bail, Context};

use crate::format;
use crate::query::{fmt_ms, resolve_backing};
use crate::store::{self, Config, RotateStats, Store};

pub fn human_bytes(n: u64) -> String {
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

/// `timberfs trim`: enforce a store's declared retention once, now.
///
/// Load-bearing rather than convenient. Retention runs inside a live
/// WRITER — every appender, mount and intake enforces it on its own tick —
/// so a store whose producer went quiet keeps its data indefinitely, and
/// under `retain_unconsumed` that means keeping data already shipped off
/// the box. This is the cron-able answer to that, and deliberately NOT the
/// tempting shortcut of letting a follower collapse the head itself, which
/// would make a reader a writer and put two of them on one head.
///
/// A store somebody else is writing is left alone and said so: that
/// writer's own tick is already doing this, once a second, and taking its
/// lock away to repeat the work would be the one thing this must not do.
/// What one store's trim came to, for a caller deciding whether the store
/// is now worth keeping and for the summary a selection needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trimmed {
    /// Chunks left afterwards. `None` where nothing was ATTEMPTED — no
    /// declared retention, a live writer, a live mount — which is not the
    /// same as zero and must never be read as "empty".
    pub remaining: Option<usize>,
    /// Something has left this store over its life, so it once held data.
    ///
    /// ⚠ This is what separates a store retention EMPTIED from one that
    /// was pre-created and never written — and pre-creating is a
    /// documented workflow (`otlp-intake` without `--auto-create` refuses
    /// undeclared streams, so the operator creates each store ahead of
    /// its producer). Both have no chunks; only the first has dropped
    /// anything, and `info` says so in as many words: "emptied, not
    /// reset".
    pub ever_held: bool,
}

impl Trimmed {
    /// Nothing was attempted, so nothing is known about what remains.
    fn untouched() -> Trimmed {
        Trimmed {
            remaining: None,
            ever_held: false,
        }
    }

    /// Held data once and holds none now — the only state in which
    /// deleting the store is housekeeping rather than data loss.
    pub fn emptied(&self) -> bool {
        self.remaining == Some(0) && self.ever_held
    }
}

pub fn cmd_trim(store: &Path, dry_run: bool) -> anyhow::Result<Trimmed> {
    if crate::query::is_bundle(store) {
        bail!(
            "{} is a .timber transfer bundle — bundles are read-only, and a snapshot has no \
             retention to enforce",
            store.display()
        );
    }
    let (dir, name) = resolve_backing(store)?;
    let rings = format::rings_path(&dir, &name);
    if !rings.exists() {
        bail!("no timberfs log {name} in {}", dir.display());
    }

    // The policy first, so a store that declares nothing says so without
    // taking a lock — `trim` on an undeclared store is a no-op, not an
    // error, because that is what a cron entry over a whole forest needs.
    let policy = crate::bark::declared_retention(&dir, &name).with_context(|| {
        format!(
            "reading {}'s declared retention (fix it with `timberfs set`)",
            name
        )
    })?;
    if !policy.is_some() {
        crate::note!("timberfs: {name} declares no retention; nothing to enforce");
        return Ok(Trimmed::untouched());
    }

    let records = format::read_index(&rings)?;
    // From the rings HEADER, not from the last record: retention is allowed
    // to drop every chunk, and the numbering must not restart when it does
    // — so on an emptied store the records say 0 while the header says
    // where the next chunk actually continues from. The writer reads the
    // header, and a preview that disagreed with it would call a legitimate
    // position impossible.
    let next_seq = std::fs::File::open(&rings)
        .and_then(|f| format::read_header_next_seq(&f))
        .unwrap_or(0)
        .max(records.last().map(|c| c.seq + 1).unwrap_or(0));
    let fields = crate::follower::subject_of(&dir, &name);
    let held = crate::follower::TickInterest::default().floor(&policy, &fields, next_seq);
    if policy.unconsumed {
        match (&held.holder, held.floor) {
            (Some(h), Some(f)) => crate::note!(
                "timberfs: {name}: {} retaining follower(s); {h} is furthest behind, at chunk {f}",
                held.retaining
            ),
            (Some(h), None) => crate::note!(
                "timberfs: {name}: {h} retains everything (it has no usable position), so \
                 interest drops nothing"
            ),
            // ⚠ Blind is not the same answer, though it holds the same
            // amount back: the registry could not be read, so EVERY
            // store on this host is pinned on this axis until it can be.
            (None, _) if held.blind => crate::note!(
                "timberfs: {name}: the follower registry could not be read, so interest holds                  everything back on every store here — `timberfs follower list` names the one                  at fault"
            ),
            (None, _) => crate::note!(
                "timberfs: {name}: nothing retains this store, so interest drops nothing"
            ),
        }
    }

    if dry_run {
        // A preview, computed the same way the writer will compute it —
        // interest by the same partition over chunk numbers, age and size
        // left to the writer, which owns the hysteresis.
        let by_interest = held.droppable(&records);
        println!(
            "dry run: interest would drop {by_interest} of {} chunk(s); age and size are \
             decided by the writer at the moment it acts",
            records.len()
        );
        // A preview, so `remaining` is what WOULD be left.
        return Ok(Trimmed {
            remaining: Some(records.len() - by_interest),
            ever_held: ever_held(&dir, &name, &records),
        });
    }

    // Offline only, and by DESIGN. A live writer enforces this on its own
    // tick; taking its lock away to do the same work is the one thing that
    // could hurt it, and reporting "nothing to do" is the honest answer.
    let Some(_dir_guard) = store::lock_backing_shared(&dir)? else {
        let where_ = store::read_lock_mountpoint(&dir)
            .map(|mp| format!(" (mounted at {})", mp.display()))
            .unwrap_or_default();
        crate::note!(
            "timberfs: {name} is served by a live timberfs mount{where_}, which enforces \
             retention on its own tick; nothing to do here"
        );
        return Ok(Trimmed::untouched());
    };
    let Some(_file_lock) = store::lock_file_exclusive(&dir, &name)? else {
        let who = store::describe_file_writer(&dir, &name)
            .map(|w| format!(" ({w})"))
            .unwrap_or_default();
        crate::note!(
            "timberfs: {name} has a live writer{who}, which enforces retention on its own \
             tick; nothing to do here"
        );
        return Ok(Trimmed::untouched());
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
    st.create(&name)?;
    // Re-resolved under the lock: the preview above was lock-free, and a
    // floor is only a statement about the store as it is at the moment of
    // the drop.
    let next_seq = st.next_seq(&name).unwrap_or(next_seq);
    let held = crate::follower::TickInterest::default().floor(&policy, &fields, next_seq);
    let mut left = records.len();
    match st.enforce_retention(&name, policy.max_age_ms, policy.max_comp_bytes, held.floor)? {
        None => println!("nothing to trim: every chunk is within the declared policy"),
        Some(stats) => {
            println!(
                "trimmed {} chunk(s) ({} on disk), chunks {}..{}, written {} .. {}",
                stats.chunks_moved,
                human_bytes(stats.comp_bytes),
                stats.first_seq,
                stats.last_seq,
                fmt_ms(stats.first_write_ms),
                fmt_ms(stats.last_write_ms)
            );
            println!("  {} chunk(s) remain", stats.chunks_remaining);
            if let Some(record) = crate::follower::override_record(&name, &policy, &stats, &held) {
                eprintln!("{record}");
            }
            // A head-drop deletes the grain, exactly as it does for a
            // writer's tick; rebuild it if the store declares one.
            if crate::bark::index_declared(&dir, &name) {
                if let Err(e) = crate::grain::extend_grain(&dir, &name) {
                    eprintln!("timberfs: {name}: grain rebuild after trim failed: {e}");
                }
            }
            left = stats.chunks_remaining;
        }
    }
    Ok(Trimmed {
        remaining: Some(left),
        ever_held: ever_held(&dir, &name, &records),
    })
}

/// The single-store half of `--delete-empty`: same rules as the sweep,
/// so one store and a selection of one behave alike.
pub fn delete_if_emptied(store: &Path, outcome: &Trimmed, dry_run: bool) -> anyhow::Result<()> {
    if !outcome.emptied() {
        return Ok(());
    }
    let (dir, name) = resolve_backing(store)?;
    let fields = crate::follower::subject_of(&dir, &name);
    let reg = crate::follower::registry_dir();
    if let Some(who) = crate::follower::for_store(&reg, &fields)
        .into_iter()
        .find(|r| r.decl.retaining)
        .map(|r| r.name().to_string())
    {
        crate::note!(
            "timberfs: {name} is empty but {who} retains it — release it first \
             (`timberfs follower update {who} retaining=false`)"
        );
        std::process::exit(1);
    }
    if dry_run {
        println!("dry run: would delete {name} (empty, and it once held data)");
        return Ok(());
    }
    delete_emptied_store(&dir, &name)
}

/// `trim` over a SELECTION: the cron-able form, since a host where stores
/// come and go is exactly where a one-shot per store becomes a loop
/// somebody has to write.
///
/// `delete_empty` removes a store the trim left holding nothing that once
/// held something. Two stores are never deleted, and the difference
/// matters: one PRE-CREATED and never written (no chunks, nothing
/// dropped) is a placeholder waiting for its producer, and one a
/// RETAINING follower covers is refused rather than pulled out from under
/// it — the same two-step `follower delete` insists on.
pub fn cmd_trim_selection(
    expr: &str,
    dirs: &[PathBuf],
    dry_run: bool,
    delete_empty: bool,
) -> anyhow::Result<()> {
    let sel = crate::select::Selector::parse(expr)?;
    let matched = crate::select::resolve(dirs, &sel);
    if matched.is_empty() {
        crate::note!("timberfs: no store matches `{expr}`; nothing to enforce");
        return Ok(());
    }
    // The registry once for the whole sweep, not once per store: every
    // read of it places every follower's position.
    let registry = crate::follower::all(&crate::follower::registry_dir());
    let (mut trimmed, mut deleted, mut held_back) = (0usize, 0usize, 0usize);
    let (mut failed, mut busy) = (0usize, 0usize);
    for m in &matched {
        let store = m.dir.join(&m.name);
        println!("== {}", m.handle);
        let outcome = match cmd_trim(&store, dry_run) {
            Ok(o) => o,
            // One store's failure must not end a sweep: the rest of the
            // selection is still owed its retention.
            Err(e) => {
                eprintln!("timberfs: {}: {e:#}", m.handle);
                failed += 1;
                continue;
            }
        };
        match outcome.remaining {
            Some(_) => trimmed += 1,
            // Nothing attempted: a live writer, a live mount, or no
            // declared retention. Normal, and NOT a failure — the writer
            // enforces this on its own tick — but worth counting, because
            // on a busy host it is most of the selection and "trimmed 1
            // of 400" otherwise says nothing about the other 399.
            None => busy += 1,
        }
        if !delete_empty || !outcome.emptied() {
            continue;
        }
        // ⚠ A retaining follower's position is IN this store, so deleting
        // it is the silent release `follower delete` refuses. Said, not
        // done.
        let fields = crate::follower::subject_of(&m.dir, &m.name);
        let retainer = crate::follower::covering(&registry, &fields)
            .into_iter()
            .find(|r| r.decl.retaining)
            .map(|r| r.name().to_string());
        if let Some(who) = retainer {
            crate::note!(
                "timberfs: {} is empty but {who} retains it — release it first \
                 (`timberfs follower update {who} retaining=false`)",
                m.handle
            );
            held_back += 1;
            continue;
        }
        if dry_run {
            println!(
                "  dry run: would delete {} (empty, and it once held data)",
                m.handle
            );
        } else {
            delete_emptied_store(&m.dir, &m.name)?;
        }
        deleted += 1;
    }
    // ⚠ The preview computes the INTEREST axis only — age and size are
    // the writer's, hysteresis included — so it foresees a deletion for a
    // store that is ALREADY empty and cannot foresee one for a store this
    // run is about to empty by age or size. Under-reporting a deletion is
    // the direction that gets an operator's approval for a plan and then
    // does more, so it is said rather than left to be discovered.
    if dry_run && delete_empty {
        crate::note!(
            "timberfs: this preview covers interest; a store the age or size axis empties is \
             deleted by a real run without appearing above"
        );
    }
    let verb = if dry_run { "would trim" } else { "trimmed" };
    crate::note!(
        "timberfs: {verb} {trimmed} of {} matched store(s){}{}",
        matched.len(),
        if delete_empty {
            format!(
                "; {} {} empty",
                if dry_run { "would delete" } else { "deleted" },
                deleted
            )
        } else {
            String::new()
        },
        if held_back > 0 {
            format!("; {held_back} empty but retained")
        } else {
            String::new()
        }
    );
    if busy > 0 {
        crate::note!("timberfs: {busy} left to their own writer or declaring no retention");
    }
    // ⚠ Non-zero for what needs a HUMAN, and nothing else. A store left
    // to its live writer is the normal case on a busy host and must not
    // colour a cron entry red; a store that FAILED, or one that is empty
    // and retained, is a thing somebody has to look at — and a failure
    // reported only on stderr is a silent one in cron.
    if failed > 0 || held_back > 0 {
        crate::note!("timberfs: {} store(s) need attention", failed + held_back);
        std::process::exit(1);
    }
    Ok(())
}

/// Delete a store retention has emptied: every file it owns, and its
/// directory if that leaves it bare.
///
/// ⚠ Called under the store's own exclusive lock (`cmd_trim` holds it),
/// which is what makes removing `<name>.lock` safe here where store.rs
/// says lock files are never deleted. That rule protects a LIVE store's
/// mutual exclusion — unlink-and-recreate would let two writers hold
/// "the" lock on different inodes — and this store has no next writer of
/// that identity: its `.bark` id goes with it.
///
/// The RINGS go first, so an interrupted delete leaves something no
/// reader picks up (every scan tests for them) rather than a pair with no
/// index.
fn delete_emptied_store(dir: &Path, name: &str) -> anyhow::Result<()> {
    for p in format::every_path(dir, name) {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("removing {}", p.display()));
            }
        }
    }
    // The directory only if the store was the whole of it: two stores
    // sharing one directory is supported, and the forest's own lock file
    // does not count as an occupant.
    let leftovers: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .map(|e| e.file_name())
        .filter(|n| n != std::ffi::OsStr::new(store::LOCK_FILE_NAME))
        .collect();
    if leftovers.is_empty() {
        let _ = std::fs::remove_file(dir.join(store::LOCK_FILE_NAME));
        std::fs::remove_dir(dir)
            .with_context(|| format!("removing the now-empty {}", dir.display()))?;
        crate::note!(
            "timberfs: removed {name} and its directory {}",
            dir.display()
        );
    } else {
        crate::note!(
            "timberfs: removed {name}; {} keeps {} other file(s)",
            dir.display(),
            leftovers.len()
        );
    }
    Ok(())
}

/// Has anything ever left this store? The dropped counters are the whole
/// answer, and they are kept precisely so an emptied store cannot be
/// mistaken for a new one (see WHAT A STORE HAS DROPPED in timberfs(1)).
/// A store still holding chunks obviously held data, counters or not.
fn ever_held(dir: &Path, name: &str, records: &[format::ChunkRecord]) -> bool {
    if !records.is_empty() {
        return true;
    }
    std::fs::File::open(format::rings_path(dir, name))
        .and_then(|f| format::read_header_dropped(&f))
        .map(|d| d.comp_bytes > 0 || d.uncomp_bytes > 0)
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("timberfs-rot-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// ⚠ The whole safety of `--delete-empty` is one distinction: a store
    /// retention EMPTIED versus one PRE-CREATED and never written. Both
    /// hold no chunks. Only the first has dropped anything — and
    /// pre-creating is a documented workflow (`otlp-intake` without
    /// `--auto-create` refuses undeclared streams, so an operator creates
    /// each store ahead of its producer), so getting this backwards
    /// deletes the placeholders a host was waiting on.
    #[test]
    fn only_a_store_that_once_held_data_counts_as_emptied() {
        let emptied = Trimmed {
            remaining: Some(0),
            ever_held: true,
        };
        let never_written = Trimmed {
            remaining: Some(0),
            ever_held: false,
        };
        let holding = Trimmed {
            remaining: Some(3),
            ever_held: true,
        };
        assert!(emptied.emptied());
        assert!(
            !never_written.emptied(),
            "a placeholder is not an emptied store"
        );
        assert!(!holding.emptied());
        // ⚠ And "nothing was attempted" is not "empty": a live writer or
        // an undeclared policy leaves `remaining` unknown, and reading
        // that as zero would delete a store nobody looked at.
        assert!(!Trimmed::untouched().emptied());
        assert_eq!(Trimmed::untouched().remaining, None);
    }

    /// Deleting a store takes every file it owns and the directory only if
    /// that leaves it bare — two stores sharing one directory is
    /// supported, and the forest's own lock file is not an occupant.
    #[test]
    fn deleting_a_store_takes_its_whole_file_set_and_an_emptied_directory() {
        let root = scratch("del");
        let solo = root.join("solo");
        fs::create_dir_all(&solo).unwrap();
        for p in format::every_path(&solo, "solo.log") {
            fs::write(&p, b"x").unwrap();
        }
        fs::write(solo.join(store::LOCK_FILE_NAME), b"").unwrap();
        delete_emptied_store(&solo, "solo.log").unwrap();
        assert!(!solo.exists(), "the directory was the store's alone");

        // A shared directory keeps its other store, files and all.
        let shared = root.join("shared");
        fs::create_dir_all(&shared).unwrap();
        for n in ["a.log", "b.log"] {
            for p in format::every_path(&shared, n) {
                fs::write(&p, b"x").unwrap();
            }
        }
        delete_emptied_store(&shared, "a.log").unwrap();
        assert!(shared.is_dir(), "a shared directory must survive");
        assert!(
            format::rings_path(&shared, "b.log").exists(),
            "the neighbour is untouched"
        );
        for p in format::every_path(&shared, "a.log") {
            assert!(!p.exists(), "{} survived the delete", p.display());
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// ⚠ Every file a store owns, in one list, so a delete cannot leave
    /// one behind as the set grows — and the RINGS first, because a store
    /// without them is not a store to any reader, so an interrupted
    /// delete leaves something nothing picks up.
    #[test]
    fn the_file_set_leads_with_the_rings_and_covers_every_sidecar() {
        let paths = format::every_path(Path::new("/d"), "x.log");
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.first().unwrap(), "x.log.rings", "the rings go first");
        for ext in [
            format::TRUNK_EXT,
            format::RINGS_EXT,
            format::GRAIN_EXT,
            format::BARK_EXT,
            format::SEQ_EXT,
            format::TRIM_EXT,
            format::SAP_EXT,
            format::SAP_SEAL_EXT,
        ] {
            assert!(
                names.contains(&format!("x.log.{ext}")),
                "the set is missing .{ext}"
            );
        }
        // The per-file lock, last: it is held while the store is deleted.
        assert_eq!(names.last().unwrap(), "x.log.lock");
    }
}
