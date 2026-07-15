//! The records sink: ONE engine writing a timberfs-records(5) stream
//! into a store, parameterized by the only two things the verbs differ
//! on — delivery and the fallback clock. `import --records` is the
//! atomic bundle (nothing visible until stream-end; a truncated stream
//! leaves the store byte-for-byte unchanged); `append --records` is
//! the streaming bundle (data lands as it arrives; the exit code
//! carries incompleteness — at-least-once, the caller owns retries).
//!
//! The sink is FAITHFUL: an entry carrying its original write window
//! (wf/wl) keeps it — write history survives replication. Silence
//! falls back to the command's own clock: append stamps now (the live
//! doctrine), import derives from the entry's own timestamp (the
//! backfill doctrine). Explicit metadata always wins, so a pipeline
//! stage that sets wf/wl steers both verbs identically.
//!
//! Lineage: the stream-start selection echo and its stage= list are
//! recorded in the destination's .bark — an artifact remembers every
//! stage of the pipe that filled it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;

use crate::query::{ensure_dest_is_not_plain_file, fmt_ms, is_bundle, resolve_backing};
use crate::records::{Reader, Rec};
use crate::store::{self, now_ms, Config, Store};

pub enum Delivery {
    /// Commit only at stream-end; abort leaves the store unchanged.
    Atomic,
    /// Flush as received; truncation keeps the data and fails the exit.
    Streaming,
}

pub enum Clock {
    /// wf/wl absent: stamp now (append — the live doctrine).
    Now,
    /// wf/wl absent: derive from the entry's own timestamp, inheriting
    /// the last seen one for unstamped entries (import — backfill).
    FromStamps,
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_records_sink(
    source: Option<&Path>,
    dest: &Path,
    cfg: Config,
    delivery: Delivery,
    clock: Clock,
    retain: Option<&str>,
    retain_size: Option<&str>,
    op: &str,
) -> anyhow::Result<()> {
    retain.map(crate::append::parse_duration_ms).transpose()?;
    retain_size
        .map(crate::append::parse_size_bytes)
        .transpose()?;
    if is_bundle(dest) {
        bail!(
            "{} is a .timber transfer bundle — bundles are read-only; \
             sink the stream into a log instead",
            dest.display()
        );
    }
    ensure_dest_is_not_plain_file(dest, op)?;
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
    let file_lock = match store::lock_file_exclusive(&dir, &name)? {
        Some(f) => f,
        None => {
            bail!("{name} already has a writer (another timberfs appender or a rotation)");
        }
    };
    store::write_lock_info(
        &file_lock,
        &format!("records sink pid={}\n", std::process::id()),
    )?;

    // Declared retention, like every writer: the manifest is the truth.
    if retain.is_some() || retain_size.is_some() {
        let mut map = crate::bark::load(&dir, &name).unwrap_or_default();
        if let Some(r) = retain {
            map.insert("retain".to_string(), Value::String(r.to_string()));
        }
        if let Some(r) = retain_size {
            map.insert("retain_size".to_string(), Value::String(r.to_string()));
        }
        crate::bark::save(&dir, &name, &map)?;
    }

    let st = Arc::new(Mutex::new(Store {
        dir: dir.clone(),
        cfg,
        files: BTreeMap::new(),
    }));
    st.lock().unwrap().create(&name)?;

    let reader: Box<dyn BufRead> = match source {
        Some(p) => Box::new(BufReader::new(
            fs::File::open(p).with_context(|| format!("opening {}", p.display()))?,
        )),
        None => Box::new(BufReader::new(io::stdin())),
    };
    let mut rec_reader = Reader::new(reader);

    let streaming = matches!(delivery, Delivery::Streaming);
    if !streaming {
        // Atomic (import): stage — nothing is visible until the commit.
        st.lock().unwrap().files.get_mut(&name).unwrap().stage();
    }

    // Streaming maintenance, exactly as `timberfs append`: once a second,
    // flush any chunk older than flush_age_ms and enforce declared
    // retention. A FIFO-fed streaming sink may never see EOF, so
    // end-of-stream maintenance would otherwise never run — a quiet
    // producer's data would sit unflushed (and undurable) until a chunk
    // filled. NOT for atomic: staged data must not surface before commit,
    // and an import is a bounded batch that maintains once at the end.
    // SIGTERM/SIGINT (systemctl stop/restart, upgrades, Ctrl-C) must flush
    // the buffer before the process dies — otherwise every restart of a
    // socket-fed streaming sink loses whatever was buffered since the last
    // age flush. The maintenance thread below performs that final flush and
    // exits. Handlers are installed only for streaming: a killed atomic
    // import must leave its staged data uncommitted (store unchanged).
    if streaming {
        crate::append::install_signal_handlers();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let maint = if streaming {
        let st = Arc::clone(&st);
        let stop = Arc::clone(&stop);
        let dir = dir.clone();
        let name = name.clone();
        let op = op.to_string();
        // Chunks already folded into the declared grain. Extending re-reads
        // the whole grain file, so we only do it when the flushed-chunk set
        // actually changed — not every idle second.
        let mut indexed_chunks: usize = 0;
        Some(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(1000));
                // A graceful-stop signal: flush + sync everything, bring the
                // declared index current, make sure the store carries its
                // identity, and exit durably. The main thread is abandoned
                // mid-read (the process is going away).
                if crate::append::stopping() {
                    {
                        let mut s = st.lock().unwrap();
                        let cfg = s.cfg;
                        let f = s.files.get_mut(&name).unwrap();
                        let _ = f.flush_chunk(&cfg);
                        let _ = f.sync(&cfg);
                    }
                    // Grain after releasing the lock: extend_grain only reads
                    // committed chunks and owns the grain, so it needn't (and
                    // shouldn't) hold the lock — a full rebuild here would
                    // otherwise delay shutdown past systemd's stop timeout.
                    if crate::bark::index_declared(&dir, &name) {
                        let _ = crate::grain::extend_grain(&dir, &name);
                    }
                    if crate::bark::load(&dir, &name).is_none() {
                        if let Ok(m) =
                            crate::bark::with_identity(crate::bark::derived_map(None, &op))
                        {
                            let _ = crate::bark::save(&dir, &name, &m);
                        }
                    }
                    std::process::exit(0);
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                st.lock().unwrap().flush_aged();
                match crate::bark::declared_retention(&dir, &name) {
                    Ok(policy) if policy.is_some() => {
                        if let Err(e) = st.lock().unwrap().enforce_retention(
                            &name,
                            policy.max_age_ms,
                            policy.max_comp_bytes,
                        ) {
                            eprintln!("timberfs: {name}: background retention failed: {e}");
                        }
                    }
                    _ => {}
                }
                // Keep the declared index current while streaming: extend the
                // grain whenever the flushed-chunk set changed (a flush added
                // chunks, or retention dropped some and deleted the grain).
                // Deliberately NOT holding the store lock: extend_grain reads
                // only committed, immutable chunks and is the sole writer of
                // the grain, so it can't race the append thread (which only
                // appends new chunks and never touches the grain). This
                // matters — a missing grain triggers a full rebuild, and
                // holding the lock through that would stall ingestion for as
                // long as the rebuild takes.
                let cur = st
                    .lock()
                    .unwrap()
                    .files
                    .get(&name)
                    .map(|f| f.chunks.len())
                    .unwrap_or(0);
                if cur != indexed_chunks && crate::bark::index_declared(&dir, &name) {
                    match crate::grain::extend_grain(&dir, &name) {
                        Ok(()) => indexed_chunks = cur,
                        Err(e) => {
                            eprintln!("timberfs: {name}: background grain extend failed: {e}")
                        }
                    }
                }
            }
        }))
    } else {
        None
    };

    let mut entries: u64 = 0;
    let mut carried: u64 = 0;
    let mut last_ts: Option<u64> = None;
    let mut start_fields: Vec<(String, String)> = Vec::new();
    let result = loop {
        match rec_reader.next_rec() {
            Ok(Some(Rec::Start(fields))) => start_fields = fields,
            Ok(Some(Rec::Source(_))) => {}
            Ok(Some(Rec::Entry(e))) => {
                if let Some(t) = e.ts {
                    last_ts = Some(t);
                }
                let (wf, wl) = match (e.wf, e.wl) {
                    // The stream's word is law.
                    (Some(a), Some(b)) => {
                        carried += 1;
                        (a, b)
                    }
                    _ => match clock {
                        Clock::Now => {
                            let n = now_ms();
                            (n, n)
                        }
                        Clock::FromStamps => {
                            let t = e.ts.or(last_ts).unwrap_or_else(now_ms);
                            (t, t)
                        }
                    },
                };
                // Locked per record so the maintenance thread can flush
                // while this thread blocks reading the next one.
                let mut s = st.lock().unwrap();
                let cfg = s.cfg;
                if let Err(e) = s
                    .files
                    .get_mut(&name)
                    .unwrap()
                    .append_windowed(&e.payload, wf, wl, &cfg)
                {
                    break Err(anyhow::Error::from(e));
                }
                entries += 1;
            }
            Ok(Some(Rec::End(_))) => {}
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };

    // Stop and join maintenance before the final flush/commit so it can't
    // race the closing write.
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = maint {
        let _ = h.join();
    }

    {
        let mut s = st.lock().unwrap();
        let cfg = s.cfg;
        let f = s.files.get_mut(&name).unwrap();
        match (&result, &delivery) {
            (Ok(()), Delivery::Atomic) => f.commit_stage(&cfg)?,
            (Ok(()), Delivery::Streaming) => {
                f.flush_chunk(&cfg)?;
                f.sync(&cfg)?;
            }
            (Err(_), Delivery::Atomic) => {
                f.abort_stage()?;
                drop(s);
                return result.context("nothing imported — the store is unchanged (atomic sink)");
            }
            (Err(_), Delivery::Streaming) => {
                f.flush_chunk(&cfg)?;
                f.sync(&cfg)?;
                // Index what we flushed before returning — this arm is the
                // main thread's shutdown when its blocked read is interrupted
                // (a genuine stream error, or the SIGTERM path if it wins the
                // race with the maintenance thread).
                if crate::bark::index_declared(&dir, &name) {
                    let _ = crate::grain::extend_grain(&dir, &name);
                }
                drop(s);
                return result.context(format!(
                    "{entries} entr{} received before the break are appended \
                     (streaming sink; re-running may duplicate)",
                    if entries == 1 { "y" } else { "ies" }
                ));
            }
        }
    }

    // Lineage into the .bark: the pipe that filled this store, fully
    // told — the upstream selection echo and every stage= it passed.
    let mut map = match crate::bark::load(&dir, &name) {
        Some(m) => m,
        None => crate::bark::derived_map(None, op),
    };
    map.insert(
        "command".to_string(),
        Value::String(crate::grep::command_line()),
    );
    let selection: Vec<String> = start_fields
        .iter()
        .filter(|(k, _)| k == "from" || k == "to" || k == "has" || k == "any")
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if !selection.is_empty() {
        map.insert(
            "stream_selection".to_string(),
            Value::String(selection.join(" ")),
        );
    }
    let stages: Vec<String> = start_fields
        .iter()
        .filter(|(k, _)| k == "stage")
        .map(|(_, v)| v.clone())
        .collect();
    if !stages.is_empty() {
        map.insert(
            "stream_stages".to_string(),
            Value::String(stages.join(" | ")),
        );
    }
    let map = crate::bark::with_identity(map)?;
    crate::bark::save(&dir, &name, &map)?;

    // Declared retention and declared index, like every writer. (For a
    // streaming sink the maintenance thread already enforced retention
    // as it ran; this is the final pass, and the only one for an import.)
    match crate::bark::declared_retention(&dir, &name) {
        Ok(policy) if policy.is_some() => {
            if let Some(stats) = st.lock().unwrap().enforce_retention(
                &name,
                policy.max_age_ms,
                policy.max_comp_bytes,
            )? {
                crate::note!(
                    "timberfs: {name}: retention dropped {} chunk(s), {} compressed bytes",
                    stats.chunks_moved,
                    stats.comp_bytes
                );
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("timberfs: {name}: manifest unreadable ({e}); retention not applied"),
    }
    if crate::bark::index_declared(&dir, &name) {
        crate::grain::extend_grain(&dir, &name)?;
    }

    let (chunk_count, first, last) = {
        let s = st.lock().unwrap();
        let f = s.files.get(&name).unwrap();
        (f.chunks.len(), f.first_write_ms(), f.last_write_ms())
    };
    let span = match (first, last) {
        (Some(a), Some(b)) => format!(", spanning {} .. {}", fmt_ms(a), fmt_ms(b)),
        _ => String::new(),
    };
    crate::note!(
        "timberfs: {name}: {entries} entr{} from the record stream \
         ({carried} with their original write windows); now {chunk_count} chunk(s){span}",
        if entries == 1 { "y" } else { "ies" },
    );
    Ok(())
}
