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
pub type Store = (String, PathBuf, Map<String, Value>);

/// Where a store this follower has never read is picked up.
///
/// It decides one thing and only for a store with no recorded position:
/// once there is one, that is where reading resumes and this has no say.
/// How far apart two `created` stamps may be and still be one moment.
/// A manifest and a declaration are both written to whole seconds, so
/// that is the resolution of the question, not a tuning knob.
const SLACK: chrono::TimeDelta = chrono::TimeDelta::seconds(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FollowFrom {
    /// The oldest byte the store still holds: everything it has.
    Begin,
    /// The next byte written. What an operator means by "start now" —
    /// and a POSITION rather than a clock, because a clock is what
    /// broke a tail once already.
    End,
    /// `Begin` for a store younger than this follower, `End` for one
    /// older.
    ///
    /// The default, and the reason is the case the other two each get
    /// wrong. `End` silently skips the seconds between a store's
    /// creation and the poll that first sees it — which on a host with
    /// an auto-creating intake can be a short-lived container's ENTIRE
    /// log. `Begin` floods when an old store is relabelled into the
    /// selection, dumping a month at the destination. This ships a store
    /// born under this follower's watch whole, and one that predates it
    /// from now on.
    ///
    /// ⚠ A heuristic, and it says so: a store's `created` is when its
    /// manifest was first written, so one whose manifest was lost and
    /// recovered reads as young. The failure is shipping more than
    /// needed, which is the safe direction.
    #[default]
    Discovery,
}

impl FollowFrom {
    pub fn as_str(&self) -> &'static str {
        match self {
            FollowFrom::Begin => "begin",
            FollowFrom::End => "end",
            FollowFrom::Discovery => "discovery",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<FollowFrom> {
        match s {
            "begin" => Ok(FollowFrom::Begin),
            "end" => Ok(FollowFrom::End),
            "discovery" => Ok(FollowFrom::Discovery),
            other => anyhow::bail!("--follow-from {other:?} is not one of begin, end, discovery"),
        }
    }
}

/// An RFC 3339 stamp as an instant, for comparing two of them.
fn instant(s: Option<&str>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s?.trim()).ok()
}

/// The store's tape END: what has ever left it, plus everything it still
/// holds in chunks.
///
/// ⚠ Not counting the write-ahead segment, so `End` ships up to one
/// flush of what was already there — «from about now» rather than from
/// an exact instant. Erring towards sending more is the safe direction,
/// and an exact answer here would have to read the segment under the
/// writer's own lock.
fn tape_end(dir: &Path, name: &str) -> u64 {
    let dropped = crate::query::dropped_bytes_of(&dir.join(name));
    let chunks =
        crate::format::read_index(&crate::format::rings_path(dir, name)).unwrap_or_default();
    dropped + chunks.last().map(|c| c.uncomp_end()).unwrap_or(0)
}

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
    /// Where a store with no recorded position is picked up.
    follow_from: FollowFrom,
    /// See `since_declared`: `None` means the positions file's own
    /// `created` is the reference.
    interest_since: Option<String>,
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
            follow_from: FollowFrom::default(),
            interest_since: None,
            stopped_in: None,
        })
    }

    /// When this consumer's interest BEGAN, which is what `discovery`
    /// compares a store's birth against.
    ///
    /// A follower has a truer answer than its positions file does: the
    /// file is written at its FIRST RUN, so one declared on Monday and
    /// started on Friday would count every store born in between as
    /// older than itself and skip it — the opposite of what `discovery`
    /// is for. `feed` has no declaration and its file is the only
    /// answer there is.
    pub fn since_declared(mut self, when: &str) -> Shipper {
        if !when.trim().is_empty() {
            self.interest_since = Some(when.to_string());
        }
        self
    }

    fn interest_since(&self) -> Option<&str> {
        Some(
            self.interest_since
                .as_deref()
                .unwrap_or(&self.positions.created),
        )
    }

    /// Advance in memory and persist nothing: a preview.
    pub fn read_only(mut self) -> Shipper {
        self.read_only = true;
        self
    }

    pub fn with_follow_from(mut self, from: FollowFrom) -> Shipper {
        self.follow_from = from;
        self
    }

    pub fn with_batch_entries(mut self, n: u64) -> Shipper {
        self.batch_entries = n.max(1);
        self
    }

    pub fn positions(&self) -> &Positions {
        &self.positions
    }

    /// Resolve the selection and read one batch, parsed into slices —
    /// for a caller that consumes entries in this process.
    pub fn poll(&mut self) -> anyhow::Result<Batch> {
        let (buf, stores, matched) = self.poll_raw()?;
        if stores.is_empty() {
            return Ok(Batch {
                slices: Vec::new(),
                more: false,
                matched,
            });
        }
        self.take(&buf, stores, matched)
    }

    /// The same read, UNPARSED: the answer's own bytes, the stores it
    /// covered, and how many the selector matched.
    ///
    /// For a caller that FORWARDS records to a consumer rather than
    /// consuming them here — what it passes on is the producer's bytes,
    /// so nothing drifts between what timberfs wrote and what the
    /// consumer reads.
    pub fn poll_raw(&mut self) -> anyhow::Result<(Vec<u8>, Vec<Store>, usize)> {
        self.poll_excluding(&|_| false)
    }

    /// The same read, skipping the stores `parked` names.
    ///
    /// PER-STORE FLOW CONTROL, which the caller owns because only it
    /// knows what its consumer has taken. Without it one store that
    /// never advances is re-read from the same place on every poll, its
    /// entries fill the shared cap, and the loop never rests — the
    /// head-of-line block the frames wire reserved a window for.
    pub fn poll_excluding(
        &mut self,
        parked: &dyn Fn(&str) -> bool,
    ) -> anyhow::Result<(Vec<u8>, Vec<Store>, usize)> {
        let matches = crate::select::resolve(&self.dirs, &self.selector);
        let matched = matches.len();
        let mut stores: Vec<Store> = Vec::new();
        for m in matches {
            let path = m.dir.join(&m.name);
            match m.id.clone() {
                Some(id) => {
                    self.pick_up(&id, &m, &path);
                    stores.push((id, path, m.labels))
                }
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
        // After the rotation, so parking a store does not change whose
        // turn it is.
        stores.retain(|(id, _, _)| !parked(id));
        if stores.is_empty() {
            return Ok((Vec::new(), stores, matched));
        }
        let files: Vec<PathBuf> = stores.iter().map(|(_, p, _)| p.clone()).collect();
        let mut buf = Vec::new();
        crate::query::read_forward(
            &mut buf,
            &files,
            &self.positions.cursor(),
            self.batch_entries,
        )?;
        Ok((buf, stores, matched))
    }

    /// Seed a store this follower has never read, per `--follow-from`.
    ///
    /// Only where there is NO recorded position: once there is one, that
    /// is where reading resumes and no policy has a say. `Begin` needs
    /// no seed at all — the absence of a cursor entry already means the
    /// start of the window — so it is the one that writes nothing.
    fn pick_up(&mut self, id: &str, m: &crate::select::Match, path: &Path) {
        if self.recorded(id).is_some() {
            return;
        }
        let from = match self.follow_from {
            FollowFrom::Begin => return,
            FollowFrom::End => FollowFrom::End,
            FollowFrom::Discovery => {
                // ⚠ PARSED, never compared as strings. Both sides write
                // RFC 3339 in UTC and neither writes the same precision:
                // a manifest's `created` is to the second, a positions
                // file's to the millisecond. Character-wise `Z` > `.`,
                // so `…:00Z` sorts AFTER `…:00.123Z` and a store born in
                // the same second as the follower reads as younger than
                // it. The tests passed on exactly that accident.
                //
                // A store whose date is missing or unparsable is treated
                // as OLDER: it predates the field, or nothing can be
                // said, and not shipping a history nobody asked for is
                // the conservative answer.
                //
                // ⚠ Within SLACK the store counts as younger, because
                // neither side's resolution can settle it: a manifest
                // and a declaration both write whole seconds, so a
                // follower declared and started in one breath shares a
                // stamp with every store created in that second — and
                // `>` alone then skipped each of them silently. Erring
                // the other way costs nothing: a store born that close
                // to this follower has a history a second long.
                match (
                    instant(m.created.as_deref()),
                    instant(self.interest_since()),
                ) {
                    (Some(born), Some(mine)) if born + SLACK >= mine => FollowFrom::Begin,
                    _ => FollowFrom::End,
                }
            }
        };
        if from == FollowFrom::Begin {
            return;
        }
        let end = tape_end(&m.dir, &m.name);
        self.positions
            .advance(id, &path.display().to_string(), end, None, 0, 0);
        crate::note!(
            "timberfs: {}: picked up at its end (offset {end}) — {}",
            m.handle,
            match self.follow_from {
                FollowFrom::End => "--follow-from end",
                _ => "it predates this follower, so its history is not shipped",
            }
        );
    }

    /// Where a bounded read stopped, so the next one starts after it.
    /// Set by whoever interpreted the answer — `take` for a parsed
    /// batch, a forwarder for a spliced one — because only they know
    /// which store the cap cut off.
    pub fn stopped_in(&mut self, id: Option<String>) {
        self.stopped_in = id;
    }

    /// A store's recorded position, or `None` where there is none — what
    /// decides whether a consumer's claimed watermark is honoured.
    pub fn recorded(&self, id: &str) -> Option<u64> {
        self.positions.at.get(id).map(|a| a.offset)
    }

    /// Seed a store this consumer's destination already holds. Refused
    /// where a position is recorded: a claim is a hint, and knowledge we
    /// own outranks it (see consumer.rs).
    pub fn seed(&mut self, id: &str, path: &str, offset: u64) -> bool {
        if self.recorded(id).is_some() {
            return false;
        }
        self.positions.advance(id, path, offset, None, 0, 0);
        true
    }

    /// Move one store's position, as a consumer's watermark says to.
    #[allow(clippy::too_many_arguments)]
    pub fn acknowledge(
        &mut self,
        id: &str,
        path: &str,
        offset: u64,
        chunk: Option<u64>,
        wl: u64,
        delivered: u64,
    ) {
        self.positions
            .advance(id, path, offset, chunk, wl, delivered);
    }

    /// Record a consumer's note, and say whether anything changed — so a
    /// caller knows whether the file is worth rewriting.
    pub fn take_note(&mut self, id: Option<&str>, offset: Option<u64>, text: &str) -> bool {
        self.positions.take_note(id, offset, text)
    }

    /// Persist. Called when a watermark moved something, and also when a
    /// note arrived and nothing moved — a stalled follower must be able
    /// to record WHY it is stalled, which is the whole point of the note.
    pub fn persist(&mut self) -> anyhow::Result<()> {
        match (&self.positions_path, self.read_only) {
            (Some(path), false) => self.positions.save(path),
            _ => Ok(()),
        }
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
            // The index mirrors the manifest's identity at OPEN, so a
            // store identified after it was opened carries it on one side
            // only until the next writer arrives. Reopen, as any restart
            // does, so these stores are shaped like deployed ones.
            drop(st);
            Store::open(&dir, cfg).unwrap();
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
        // These tests are about the loop; `begin` is what makes a store
        // built a moment ago readable regardless of the clock.
        .with_follow_from(FollowFrom::Begin)
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

    /// `--follow-from` decides only where a store with NO position is
    /// picked up, and `discovery` is the one that needs a comparison:
    /// a store born since this follower ships whole, one that predates
    /// it ships nothing of its history.
    #[test]
    fn follow_from_decides_where_an_unread_store_is_picked_up() {
        let root = forest("pickup", &[("old", "apache", 4)]);

        // `end` offers none of what is already there.
        let mut at_end = shipper(&root, "service=apache", 100).with_follow_from(FollowFrom::End);
        let b = at_end.poll().unwrap();
        assert_eq!(b.matched, 1);
        assert!(b.is_empty(), "end offered a history: {:?}", bodies(&b));

        // `begin` offers all of it.
        let mut at_begin =
            shipper(&root, "service=apache", 100).with_follow_from(FollowFrom::Begin);
        assert_eq!(at_begin.poll().unwrap().entries(), 4);

        // `discovery` on a store that predates this follower behaves as
        // `end`. ⚠ The gap has to be a real one: a store created in the
        // same second as the follower counts as younger (see SLACK), and
        // this store's manifest was written a moment ago.
        let mut found = shipper(&root, "service=apache", 100)
            .with_follow_from(FollowFrom::Discovery)
            .since_declared("2038-01-01T00:00:00Z");
        assert!(
            found.poll().unwrap().is_empty(),
            "a store older than the follower shipped its history"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⚠ `discovery` compares against when the INTEREST began, which
    /// for a follower is its declaration and not its positions file. The
    /// file is written at the first run, so a follower declared on Monday
    /// and started on Friday would count every store born in between as
    /// older than itself and ship none of it — measured against a live
    /// follower before `since_declared` existed.
    #[test]
    fn discovery_dates_the_interest_from_the_declaration_not_the_file() {
        let root = forest("since", &[("born", "apache", 3)]);

        // A positions file written after the store: by its own date the
        // store is history and `discovery` skips it.
        let mut later = shipper(&root, "service=apache", 100);
        later.positions.created = "2030-01-01T00:00:00.000Z".into();
        let mut later = later.with_follow_from(FollowFrom::Discovery);
        assert!(later.poll().unwrap().is_empty());

        // The same file, told when the follower was DECLARED: the store
        // was born since, so it ships whole.
        let mut declared = shipper(&root, "service=apache", 100);
        declared.positions.created = "2030-01-01T00:00:00.000Z".into();
        let mut declared = declared
            .with_follow_from(FollowFrom::Discovery)
            .since_declared("2020-01-01T00:00:00.000Z");
        assert_eq!(declared.poll().unwrap().entries(), 3);

        // An empty declaration date is not an instant, so it changes
        // nothing rather than reading as the epoch and shipping every
        // history on the host.
        let mut blank = shipper(&root, "service=apache", 100);
        blank.positions.created = "2030-01-01T00:00:00.000Z".into();
        let mut blank = blank
            .with_follow_from(FollowFrom::Discovery)
            .since_declared("");
        assert!(blank.poll().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⚠ The comparison must be on INSTANTS, not on the strings. A
    /// manifest writes `created` to the second and a positions file to
    /// the millisecond, and character-wise `Z` > `.` — so `…:00Z` sorts
    /// after `…:00.123Z`, and a store born in the same second as the
    /// follower read as younger than it. Every test here passed on that
    /// accident before it was fixed.
    #[test]
    fn a_store_born_in_the_same_second_is_not_younger_than_the_follower() {
        let same = "2026-09-02T13:00:00";
        let store = format!("{same}Z"); // as a manifest writes it
        let follower = format!("{same}.123Z"); // as a positions file does
        assert!(
            store.as_str() > follower.as_str(),
            "the string trap this exists to catch has gone away; the test has not"
        );
        let (born, mine) = (instant(Some(&store)), instant(Some(&follower)));
        assert!(born.is_some() && mine.is_some());
        assert!(born < mine, "parsed, the store is the older of the two");
        // ⚠ And yet it is picked up from its BEGINNING, because a second
        // is the resolution of the question: both stamps say the same
        // second, so which came first is not knowable, and a silent skip
        // is the wrong way to be wrong. A store born this close to the
        // follower has a second of history at most.
        assert!(born.unwrap() + SLACK >= mine.unwrap());
    }

    /// The slack is ONE second, not an era: a store from yesterday still
    /// ships no history under `discovery`, which is the whole point of
    /// the default.
    #[test]
    fn the_slack_does_not_swallow_a_real_age_difference() {
        let root = forest("slack", &[("old", "apache", 3)]);
        let mut s = shipper(&root, "service=apache", 100)
            .with_follow_from(FollowFrom::Discovery)
            .since_declared("2030-01-01T00:00:00Z");
        // The store's manifest was written now, which is decades before
        // that: no history.
        assert!(s.poll().unwrap().is_empty());
        // And two seconds is already outside it.
        let born = instant(Some("2026-09-02T15:13:24Z")).unwrap();
        let mine = instant(Some("2026-09-02T15:13:26Z")).unwrap();
        assert!(born + SLACK < mine);
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

    /// The PAIR is the store, so a lost manifest does not lose the
    /// identity — the `.rings` header mirrors it. Such a store stays
    /// selectable and followable, and keeps the SAME id it had: reading
    /// the manifest alone would make a selection, a position and a
    /// listing depend on which of a store's files happened to survive.
    #[test]
    fn a_store_whose_manifest_is_lost_keeps_its_identity() {
        let root = forest("lostbark", &[("web", "apache", 3)]);
        let before = shipper(&root, "service=apache", 100).poll().unwrap();
        let id = before.slices[0].id.clone();
        assert!(!id.is_empty());

        // The labels go with the manifest, so select on the name — which
        // is what is left of a store whose bark is gone.
        std::fs::remove_file(crate::format::bark_path(&root.join("web"), "web.log")).unwrap();
        let after = shipper(&root, "web", 100).poll().unwrap();
        assert_eq!(after.slices.len(), 1, "still one store");
        assert_eq!(after.slices[0].id, id, "and still the same store");
        assert_eq!(after.entries(), 3, "and readable, and cursorable");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A store with no identity on EITHER side cannot hold a position, so
    /// following it would re-ship it whole on every poll forever. It is
    /// excluded.
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
