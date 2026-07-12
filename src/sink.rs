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

    let mut st = Store {
        dir: dir.clone(),
        cfg,
        files: BTreeMap::new(),
    };
    st.create(&name)?;

    let reader: Box<dyn BufRead> = match source {
        Some(p) => Box::new(BufReader::new(
            fs::File::open(p).with_context(|| format!("opening {}", p.display()))?,
        )),
        None => Box::new(BufReader::new(io::stdin())),
    };
    let mut rec_reader = Reader::new(reader);

    let f = st.files.get_mut(&name).unwrap();
    if matches!(delivery, Delivery::Atomic) {
        f.stage();
    }

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
                f.append_windowed(&e.payload, wf, wl, &st.cfg)?;
                entries += 1;
            }
            Ok(Some(Rec::End(_))) => {}
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };

    match (&result, &delivery) {
        (Ok(()), Delivery::Atomic) => f.commit_stage(&st.cfg)?,
        (Ok(()), Delivery::Streaming) => {
            f.flush_chunk(&st.cfg)?;
            f.sync(&st.cfg)?;
        }
        (Err(_), Delivery::Atomic) => {
            f.abort_stage()?;
            return result.context("nothing imported — the store is unchanged (atomic sink)");
        }
        (Err(_), Delivery::Streaming) => {
            f.flush_chunk(&st.cfg)?;
            f.sync(&st.cfg)?;
            return result.context(format!(
                "{entries} entr{} received before the break are appended \
                 (streaming sink; re-running may duplicate)",
                if entries == 1 { "y" } else { "ies" }
            ));
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

    // Declared retention and declared index, like every writer.
    match crate::bark::declared_retention(&dir, &name) {
        Ok(policy) if policy.is_some() => {
            if let Some(stats) =
                st.enforce_retention(&name, policy.max_age_ms, policy.max_comp_bytes)?
            {
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

    let f = st.files.get(&name).unwrap();
    let span = match (f.first_write_ms(), f.last_write_ms()) {
        (Some(a), Some(b)) => format!(", spanning {} .. {}", fmt_ms(a), fmt_ms(b)),
        _ => String::new(),
    };
    crate::note!(
        "timberfs: {name}: {entries} entr{} from the record stream \
         ({carried} with their original write windows); now {} chunk(s){span}",
        if entries == 1 { "y" } else { "ies" },
        f.chunks.len()
    );
    Ok(())
}
