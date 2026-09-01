//! The shipping loop every follower type runs: read a SELECTION forward
//! from where each of its stores was left, hand the caller one batch, and
//! advance the positions once the destination has taken it.
//!
//! Direction matters. This reads OUT of stores towards somewhere else;
//! `follow.rs` reads a producer's file INTO one.
//!
//! **The positions are advanced by the destination, not by the read.** A
//! batch is durable when the receiver says so — an HTTP 200, an ack — and
//! a read proves nothing, so `accept` is a separate call the destination
//! makes. Delivery is therefore at-least-once, which is all OTLP and the
//! Forward protocol offer anyway.
//!
//! The selection is resolved per poll, so a store that appears is in the
//! next batch and one that stops matching stops appearing. Nothing
//! watches the forest: a poll is a readdir per forest plus one manifest
//! read per store, which is what makes re-resolving cheap enough to do
//! every time rather than cache and invalidate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::cursor::Positions;
use crate::records::{EntryRec, Reader, Rec};
use crate::select::Selector;

/// A store in a selection: its identity, where it is, and its labels.
type Store = (String, PathBuf, Map<String, Value>);

/// How many stores a batch may span, and how many entries it may hold, by
/// default. The entry cap is the destination's batch size; the store cap
/// bounds a poll's syscalls, since every store in the selection is opened
/// by the read whether or not it has anything.
pub const BATCH_ENTRIES: u64 = 512;

/// One store's share of a batch: what it is, what it said, and where it
/// now stands.
pub struct Slice {
    /// The store's identity. A store without one is never in a batch —
    /// see `Shipper::poll`.
    pub id: String,
    pub path: PathBuf,
    /// Its `.bark` provenance, for the labels a destination attaches.
    pub labels: Map<String, Value>,
    pub entries: Vec<EntryRec>,
    /// Where this store is left. `None` when the answer reported no
    /// position at all — nothing delivered and nothing resumed from — so
    /// there is nothing to save.
    pub offset: Option<u64>,
}

/// What one poll produced.
pub struct Batch {
    pub slices: Vec<Slice>,
    /// A bound stopped the read, so there is more waiting: poll again
    /// rather than sleep.
    pub more: bool,
    /// How many stores the selection matched, including those this batch
    /// carries nothing for. `0` is "the selector matched nothing", which
    /// is not the same as "nothing has arrived".
    pub matched: usize,
}

impl Batch {
    pub fn entries(&self) -> usize {
        self.slices.iter().map(|s| s.entries.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.slices.iter().all(|s| s.entries.is_empty())
    }
}

pub struct Shipper {
    selector: Selector,
    /// The forests to search; empty means every configured one.
    dirs: Vec<PathBuf>,
    positions: Positions,
    /// Where they are kept, or `None` for a run that keeps them only in
    /// memory — a preview, which must write nothing yet must still
    /// advance, or every poll would show the same batch again.
    positions_path: Option<PathBuf>,
    read_only: bool,
    batch_entries: u64,
    /// Stores excluded for declaring no identity, so each is said once.
    idless: Vec<String>,
    /// The store a bounded poll stopped in. The next poll starts AFTER
    /// it: the read drains its sources in order under one shared entry
    /// cap, so a store producing faster than the cap drains it would take
    /// the whole cap every poll and every store behind it would ship
    /// nothing, forever. Its own backlog waits a turn instead, which
    /// retention is already the budget for.
    stopped_in: Option<String>,
}

impl Shipper {
    pub fn open(
        consumer: &str,
        selector: Selector,
        dirs: Vec<PathBuf>,
        positions_path: Option<&Path>,
    ) -> anyhow::Result<Shipper> {
        let positions = positions_path
            .map(Positions::load)
            .transpose()?
            .flatten()
            .unwrap_or_else(|| Positions::new(consumer));
        Ok(Shipper {
            selector,
            dirs,
            positions,
            positions_path: positions_path.map(Path::to_path_buf),
            read_only: false,
            batch_entries: BATCH_ENTRIES,
            idless: Vec::new(),
            stopped_in: None,
        })
    }

    /// Advance in memory and persist nothing: a preview.
    pub fn read_only(mut self) -> Shipper {
        self.read_only = true;
        self
    }

    pub fn with_batch_entries(mut self, n: u64) -> Shipper {
        self.batch_entries = n.max(1);
        self
    }

    pub fn positions(&self) -> &Positions {
        &self.positions
    }

    /// Resolve the selection and read one batch. Reads nothing when the
    /// selector matches no store with an identity.
    pub fn poll(&mut self) -> anyhow::Result<Batch> {
        let matches = crate::select::resolve(&self.dirs, &self.selector);
        let matched = matches.len();
        let mut stores: Vec<Store> = Vec::new();
        for m in matches {
            let path = m.dir.join(&m.name);
            match m.id {
                Some(id) => stores.push((id, path, m.labels)),
                // A cursor is keyed by identity, so a store without one
                // gets no position back and would be re-read whole on
                // every poll, forever. Excluded, and said once: nothing
                // shipped beats shipping the same store endlessly, and a
                // reader has no business writing an identity into
                // someone else's manifest.
                None => {
                    let shown = path.display().to_string();
                    if !self.idless.contains(&shown) {
                        crate::note!(
                            "timberfs: {shown}: matched, but declares no identity, so a \
                             position in it cannot be recorded — not followed. \
                             `timberfs identity {shown} --mint` gives it one"
                        );
                        self.idless.push(shown);
                    }
                }
            }
        }
        rotate_past(&mut stores, self.stopped_in.as_deref());
        if stores.is_empty() {
            return Ok(Batch {
                slices: Vec::new(),
                more: false,
                matched,
            });
        }

        let files: Vec<PathBuf> = stores.iter().map(|(_, p, _)| p.clone()).collect();
        let mut buf = Vec::new();
        crate::query::read_forward(
            &mut buf,
            &files,
            &self.positions.cursor(),
            self.batch_entries,
        )?;
        self.take(&buf, stores, matched)
    }

    /// The destination has the batch: advance and persist. One
    /// tmp+rename for the whole batch, so every store in it moves or none
    /// does.
    pub fn accept(&mut self, batch: &Batch) -> anyhow::Result<()> {
        for s in &batch.slices {
            if let Some(offset) = s.offset {
                self.positions.advance(
                    &s.id,
                    &s.path.display().to_string(),
                    offset,
                    s.entries.iter().filter_map(|e| e.chunk).max(),
                    s.entries
                        .iter()
                        .map(|e| e.wl.unwrap_or(0))
                        .max()
                        .unwrap_or(0),
                    s.entries.len() as u64,
                );
            }
        }
        match (&self.positions_path, self.read_only) {
            (Some(path), false) => self.positions.save(path),
            _ => Ok(()),
        }
    }

    /// Parse one answer into slices, in the order the stores were read.
    fn take(&mut self, buf: &[u8], stores: Vec<Store>, matched: usize) -> anyhow::Result<Batch> {
        // With ONE store the answer does not repeat its id on every
        // entry — the source and position records name it — so the sole
        // store is the attribution.
        let sole = (stores.len() == 1).then(|| stores[0].0.clone());
        let mut slices: Vec<Slice> = stores
            .into_iter()
            .map(|(id, path, labels)| Slice {
                id,
                path,
                labels,
                entries: Vec::new(),
                offset: None,
            })
            .collect();
        let index: HashMap<String, usize> = slices
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.clone(), i))
            .collect();
        let mut more = false;
        let mut last_delivering: Option<String> = None;
        let mut reader = Reader::new(buf);
        while let Some(rec) = reader.next_rec()? {
            match rec {
                Rec::Entry(e) => {
                    let Some(id) = e.id.clone().or_else(|| sole.clone()) else {
                        continue;
                    };
                    if let Some(&i) = index.get(&id) {
                        last_delivering = Some(id);
                        slices[i].entries.push(e);
                    }
                }
                Rec::Position(p) => {
                    if let Some(&i) = p.id.as_deref().and_then(|id| index.get(id)) {
                        slices[i].offset = p.offset;
                    }
                }
                Rec::End(fields) => {
                    more = fields.iter().any(|(k, v)| k == "status" && v == "limited");
                }
                Rec::Start(_) | Rec::Source(_) => {}
            }
        }
        // Only when a bound cut the read short: otherwise every store was
        // read to its end and the order it happens next time does not
        // matter.
        self.stopped_in = more.then_some(last_delivering).flatten();
        Ok(Batch {
            slices,
            more,
            matched,
        })
    }
}

/// Start the order just AFTER the store `id`, keeping the rest in
/// sequence: a rotation, not a sort, so each poll gives the next store a
/// turn and every store is reached before this one comes round again. An
/// id no longer in the selection leaves the order alone.
fn rotate_past(stores: &mut [Store], id: Option<&str>) {
    let Some(id) = id else { return };
    if let Some(i) = stores.iter().position(|s| s.0 == id) {
        stores.rotate_left((i + 1) % stores.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Config, Store};

    /// A forest holding one store per (name, label) pair, each line its
    /// own entry sealed into its own chunk — so a read can be stopped
    /// between them and resumed.
    fn forest(tag: &str, stores: &[(&str, &str, usize)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("timberfs-ship-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cfg = Config {
            chunk_size: 1 << 20,
            level: 3,
            flush_age_ms: 60_000,
        };
        for (name, service, lines) in stores {
            let dir = root.join(name);
            let log = format!("{name}.log");
            let mut st = Store::open(&dir, cfg).unwrap();
            st.create(&log).unwrap();
            crate::bark::ensure_identified(&dir, &log).unwrap();
            let mut bark = crate::bark::load(&dir, &log).unwrap_or_default();
            bark.insert("service".into(), Value::String(service.to_string()));
            crate::bark::save(&dir, &log, &bark).unwrap();
            let f = st.files.get_mut(&log).unwrap();
            for i in 0..*lines {
                let line = format!("2026-09-01T10:00:{:02}Z INFO {name} line {i}\n", i % 60);
                f.append_stamped(line.as_bytes(), 1_000_000 + i as u64, &cfg)
                    .unwrap();
                f.flush_chunk(&cfg).unwrap();
            }
        }
        root
    }

    /// More entries in an existing store, as a live producer would add
    /// them: each its own entry, each sealed, so a read can stop between.
    fn append(root: &Path, name: &str, lines: usize) {
        let cfg = Config {
            chunk_size: 1 << 20,
            level: 3,
            flush_age_ms: 60_000,
        };
        let dir = root.join(name);
        let log = format!("{name}.log");
        // `open` loads every store already in the directory.
        let mut st = Store::open(&dir, cfg).unwrap();
        let f = st.files.get_mut(&log).unwrap();
        let base = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        for i in 0..lines {
            let line = format!(
                "2026-09-01T11:00:{:02}Z INFO {name} more {base}-{i}\n",
                i % 60
            );
            f.append_stamped(line.as_bytes(), base + i as u64, &cfg)
                .unwrap();
            f.flush_chunk(&cfg).unwrap();
        }
    }

    fn shipper(root: &Path, expr: &str, batch: u64) -> Shipper {
        Shipper::open(
            "test",
            Selector::parse(expr).unwrap(),
            vec![root.to_path_buf()],
            Some(&root.join("positions.json")),
        )
        .unwrap()
        .with_batch_entries(batch)
    }

    fn bodies(b: &Batch) -> Vec<String> {
        let mut out = Vec::new();
        for s in &b.slices {
            for e in &s.entries {
                out.push(String::from_utf8_lossy(&e.payload).trim().to_string());
            }
        }
        out
    }

    /// One declaration, several stores, one read — and every entry stays
    /// attributed to the store it came from, which is what a destination
    /// needs to label it.
    #[test]
    fn a_selection_ships_every_matching_store_attributed() {
        let root = forest("many", &[("web", "apache", 3), ("api", "apache", 2)]);
        let mut sh = shipper(&root, "service=apache", 100);
        let b = sh.poll().unwrap();
        assert_eq!(b.matched, 2);
        assert_eq!(b.entries(), 5);
        for s in &b.slices {
            let want = s.path.file_stem().unwrap().to_string_lossy().to_string();
            for e in &s.entries {
                let line = String::from_utf8_lossy(&e.payload);
                assert!(line.contains(&want), "{line} is not {want}'s");
            }
            assert!(s.offset.is_some(), "{want} reported no position");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The selection decides membership, so a store outside it is not
    /// read at all — the check that `--select` is a predicate and not a
    /// filter applied after everything was shipped.
    #[test]
    fn a_store_outside_the_selection_is_not_read() {
        let root = forest("some", &[("web", "apache", 2), ("db", "postgres", 2)]);
        let b = shipper(&root, "service=apache", 100).poll().unwrap();
        assert_eq!(b.matched, 1);
        assert_eq!(b.slices.len(), 1);
        assert!(bodies(&b).iter().all(|l| l.contains("web")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Nothing is delivered twice: the batch is only acknowledged by
    /// `accept`, and the poll after it resumes past what was taken.
    #[test]
    fn a_batch_is_delivered_once_and_only_after_accept() {
        let root = forest("once", &[("web", "apache", 4)]);
        let mut sh = shipper(&root, "service=apache", 100);
        let first = sh.poll().unwrap();
        assert_eq!(first.entries(), 4);
        // Not accepted: the same entries come back, which is what makes a
        // crash before the receiver answered re-deliver rather than skip.
        assert_eq!(bodies(&sh.poll().unwrap()), bodies(&first));
        sh.accept(&first).unwrap();
        assert!(sh.poll().unwrap().is_empty(), "already shipped");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A position survives the process: the whole point of holding it in
    /// a file rather than in the loop.
    #[test]
    fn a_restart_resumes_where_the_last_batch_ended() {
        let root = forest("restart", &[("web", "apache", 3)]);
        let mut sh = shipper(&root, "service=apache", 100);
        let b = sh.poll().unwrap();
        sh.accept(&b).unwrap();
        drop(sh);
        let mut again = shipper(&root, "service=apache", 100);
        assert!(again.poll().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A store producing faster than one poll's cap can drain it would
    /// take the whole cap every time, and every store behind it would
    /// ship NOTHING, forever — permanent starvation, not a delay. So a
    /// bounded poll starts the next one after the store it stopped in.
    #[test]
    fn a_store_behind_a_busy_one_is_not_starved() {
        let root = forest("fair", &[("aaa", "apache", 4), ("zzz", "apache", 4)]);
        let mut sh = shipper(&root, "service=apache", 2);
        let mut shipped: Vec<String> = Vec::new();
        for _ in 0..4 {
            // aaa gains more than the cap can take, every round: without
            // rotation it is always the front of the queue with a backlog.
            append(&root, "aaa", 5);
            let b = sh.poll().unwrap();
            assert!(b.more, "the cap stopped it, which is the case under test");
            shipped.extend(bodies(&b));
            sh.accept(&b).unwrap();
        }
        assert!(
            shipped.iter().any(|l| l.contains("zzz")),
            "the store behind the busy one shipped nothing: {shipped:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every entry of every store arrives, and none twice — the property
    /// the rotation must not break while it is being fair.
    #[test]
    fn a_bounded_poll_drains_every_store_exactly_once() {
        let root = forest("drain", &[("aaa", "apache", 6), ("zzz", "apache", 6)]);
        let mut sh = shipper(&root, "service=apache", 3);
        let mut shipped: Vec<String> = Vec::new();
        for _ in 0..20 {
            let b = sh.poll().unwrap();
            if b.is_empty() {
                break;
            }
            shipped.extend(bodies(&b));
            sh.accept(&b).unwrap();
        }
        assert_eq!(shipped.iter().filter(|l| l.contains("aaa")).count(), 6);
        assert_eq!(shipped.iter().filter(|l| l.contains("zzz")).count(), 6);
        let mut uniq = shipped.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), shipped.len(), "an entry was delivered twice");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A selector that matched nothing must be distinguishable from a
    /// selection where nothing has arrived.
    #[test]
    fn a_selector_matching_nothing_says_so() {
        let root = forest("none", &[("web", "apache", 1)]);
        let b = shipper(&root, "service=nowhere", 100).poll().unwrap();
        assert_eq!(b.matched, 0);
        assert!(b.slices.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A store with no identity cannot hold a position, so following it
    /// would re-ship it whole on every poll forever. It is excluded.
    #[test]
    fn a_store_with_no_identity_is_not_followed() {
        let root = forest("idless", &[("web", "apache", 2)]);
        let cfg = Config {
            chunk_size: 1 << 20,
            level: 3,
            flush_age_ms: 60_000,
        };
        let dir = root.join("plain");
        let mut st = Store::open(&dir, cfg).unwrap();
        st.create("plain.log").unwrap();
        let f = st.files.get_mut("plain.log").unwrap();
        f.append_stamped(b"2026-09-01T10:00:00Z INFO plain line\n", 1_000_000, &cfg)
            .unwrap();
        f.flush_chunk(&cfg).unwrap();
        let mut bark = crate::bark::load(&dir, "plain.log").unwrap_or_default();
        bark.remove("id");
        bark.insert("service".into(), Value::String("apache".into()));
        std::fs::write(
            crate::format::bark_path(&dir, "plain.log"),
            serde_json::to_string(&Value::Object(bark)).unwrap(),
        )
        .unwrap();

        let b = shipper(&root, "service=apache", 100).poll().unwrap();
        assert_eq!(b.matched, 2, "it matched");
        assert_eq!(b.slices.len(), 1, "and only one of them is followed");
        assert!(bodies(&b).iter().all(|l| l.contains("web")));
        let _ = std::fs::remove_dir_all(&root);
    }
}
