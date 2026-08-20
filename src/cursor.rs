//! A consumer's durable position in a store's entry stream.
//!
//! Kept OUTSIDE the store, at a path the operator names: a cursor is
//! CONSUMER state, not store state. Several consumers read one store at
//! different positions, a cursor must not travel inside a `.timber`
//! bundle as if it were provenance, and retention dropping chunks must
//! not drop it. `.bark` is what the store declares about itself; this is
//! what someone reading it remembers.
//!
//! The position is on the WRITE axis, the only monotonic one. Logline
//! timestamps go backwards — an entry written now can be stamped an hour
//! ago — so resuming from the highest logline stamp seen would SKIP such
//! an entry permanently, while resuming from a write time can only
//! re-deliver. Both are failure modes; only one is acceptable.
//!
//! A position is `(wf, wl, n)`: the write window of the chunk an entry
//! arrived in, plus how many entries carrying that exact window were
//! already delivered. `query --records --follow --from <wl>` positions at
//! the chunk the cursor sits in (chunk granularity is all the rings
//! offer) and `n` skips what was already delivered inside it. Two
//! distinct chunks sharing both ends of their window re-deliver rather
//! than skip.
//!
//! Delivery is therefore at-least-once — which is all OTLP and the
//! Forward protocol offer anyway: neither carries a deduplication key.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};

use crate::format::ChunkRecord;

/// One consumer's position, as persisted. The file is a flat JSON object
/// so an operator can rewind by editing `wl` (the supported edit) — but
/// it is machine-owned state: unknown keys are ignored, not preserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Who wrote it, for the operator staring at a state directory.
    pub consumer: String,
    /// What the store this is a position in is identified BY: its
    /// `.bark` id when it declares one — identity, not address, so a
    /// moved store still matches and a different store at the same path
    /// does not — else `path:<canonical>`, since a store written by a
    /// plain `append` has no manifest to ask.
    pub store: String,
    /// The store's path when last written — informational only.
    pub path: String,
    pub wf: u64,
    pub wl: u64,
    /// Entries already delivered from the window `(wf, wl)`.
    pub n: u64,
    /// Total entries delivered by this consumer, for observability.
    pub delivered: u64,
}

impl Cursor {
    pub fn new(consumer: &str, store: &str, path: &str) -> Cursor {
        Cursor {
            consumer: consumer.to_string(),
            store: store.to_string(),
            path: path.to_string(),
            wf: 0,
            wl: 0,
            n: 0,
            delivered: 0,
        }
    }

    /// Missing is `None` (a first run); present-but-unreadable is an
    /// error — never silently restart from the beginning, which would
    /// re-ship the whole store.
    pub fn load(path: &Path) -> anyhow::Result<Option<Cursor>> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
            bail!(
                "{} is not a JSON object (delete it to start over)",
                path.display()
            );
        };
        let str_at = |k: &str| -> String {
            map.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let u64_at = |k: &str| -> u64 { map.get(k).and_then(Value::as_u64).unwrap_or(0) };
        let store = str_at("store");
        if store.is_empty() {
            bail!(
                "{} has no \"store\" id — it is not a timberfs cursor",
                path.display()
            );
        }
        Ok(Some(Cursor {
            consumer: str_at("consumer"),
            store,
            path: str_at("path"),
            wf: u64_at("wf"),
            wl: u64_at("wl"),
            n: u64_at("n"),
            delivered: u64_at("delivered"),
        }))
    }

    /// A cursor is a position in ONE store: a mismatch means the path now
    /// holds a different store (recreated, or a stale state directory),
    /// where resuming by write time would deliver arbitrary data.
    pub fn check_store(&self, id: &str, path: &Path) -> anyhow::Result<()> {
        if self.store != id {
            bail!(
                "{} is a cursor for store {} but {} is store {} — delete the cursor \
                 to start over, or point --cursor elsewhere",
                path.display(),
                self.store,
                self.path,
                id
            );
        }
        Ok(())
    }

    /// Count one delivered entry. `n` counts within a write window, so it
    /// restarts whenever the window does.
    pub fn advance(&mut self, wf: u64, wl: u64) {
        if (self.wf, self.wl) != (wf, wl) {
            self.wf = wf;
            self.wl = wl;
            self.n = 0;
        }
        self.n += 1;
        self.delivered += 1;
    }

    /// What to hand `query --from`: the window END, since that is what
    /// chunk positioning compares against (`last_write_ms >= from`).
    pub fn from_ms(&self) -> u64 {
        self.wl
    }

    /// Atomic and durable: a torn or empty cursor after a crash would be
    /// unreadable, and an unreadable cursor stops the shipper. tmp +
    /// fsync + rename + fsync of the directory; the pre-rename content is
    /// always a valid older position, which re-delivers rather than skips.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut map = Map::new();
        map.insert("consumer".into(), Value::String(self.consumer.clone()));
        map.insert("store".into(), Value::String(self.store.clone()));
        map.insert("path".into(), Value::String(self.path.clone()));
        map.insert("wf".into(), Value::from(self.wf));
        map.insert("wl".into(), Value::from(self.wl));
        map.insert("n".into(), Value::from(self.n));
        map.insert("delivered".into(), Value::from(self.delivered));
        map.insert(
            "updated".into(),
            Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
        let text = serde_json::to_string_pretty(&Value::Object(map))? + "\n";

        let dir = path.parent().unwrap_or(Path::new("."));
        let tmp = path.with_extension("tmp");
        {
            let mut f =
                fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
        // Durable NAME, not just durable bytes: without this the rename
        // itself can be lost and the cursor reverts to its previous value.
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

/// Skips what a resumed stream re-delivers. `query --from` positions at
/// chunk granularity, so the first chunk of a resumed stream is re-read
/// from its start; this drops the entries inside it that the cursor says
/// were already delivered, and then gets out of the way for good.
pub struct Resume {
    at: Option<(u64, u64, u64)>,
    seen_in_window: u64,
    skipped: u64,
    done: bool,
}

impl Resume {
    pub fn new(cursor: Option<&Cursor>) -> Resume {
        Resume {
            at: cursor.map(|c| (c.wf, c.wl, c.n)),
            seen_in_window: 0,
            skipped: 0,
            done: false,
        }
    }

    /// Should this entry be delivered? Windows are ordered by `(wl, wf)`
    /// — the appender stamps `now()`, so a live store's chunk windows
    /// only move forward. A store written by an INTAKE is the exception:
    /// those stamp the sender's event time, so a sender replaying old
    /// events moves the windows backwards and a cursor over such a store
    /// can skip rather than re-deliver.
    pub fn deliver(&mut self, wf: u64, wl: u64) -> bool {
        if self.done {
            return true;
        }
        let Some((cwf, cwl, n)) = self.at else {
            self.done = true;
            return true;
        };
        if (wl, wf) < (cwl, cwf) {
            self.skipped += 1;
            return false;
        }
        if (wl, wf) == (cwl, cwf) && self.seen_in_window < n {
            self.seen_in_window += 1;
            self.skipped += 1;
            return false;
        }
        self.done = true;
        true
    }

    /// How many already-delivered entries this resume dropped — the cost
    /// of chunk-granular positioning, worth reporting once.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }
}

/// What a cursor for the store at `dir`/`name` anchors to: the `.bark`
/// id when it declares one — identity, so a moved store still matches
/// and a different store at the same path does not — else
/// `path:<canonical>`, since a store written by a plain `append` has no
/// manifest to ask. One rule, shared by whoever WRITES a cursor and
/// whoever goes looking for one, or a store would not recognise its own
/// consumers.
pub fn store_anchor(dir: &Path, name: &str, bark: Option<&Map<String, Value>>) -> String {
    bark.and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "path:{}",
                fs::canonicalize(dir)
                    .unwrap_or_else(|_| dir.to_path_buf())
                    .join(name)
                    .display()
            )
        })
}

/// The directory a store declares its consumers keep cursors in (the
/// `cursors` key in `.bark`). DECLARED, not discovered: a cursor is
/// consumer state at an operator-named path, so the store names the
/// place to look and nothing ever writes back into it. One directory can
/// serve a whole host — cursors are matched to stores by the identity
/// they carry, not by living in a per-store place.
pub fn declared_dir(bark: Option<&Map<String, Value>>) -> Option<PathBuf> {
    bark.and_then(|m| m.get("cursors"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// How many head chunks a cursor has FULLY consumed. This is the exact
/// complement of what `Resume` would deliver — anything it would hand
/// over must still exist — so the two orderings must stay identical:
/// windows compare as `(last, first)`, and EQUALITY HOLDS the chunk. The
/// cursor sits inside that one, `n` counts within it, and `query --from`
/// re-positions to its start; dropping it would make `n` skip live
/// entries instead of already-delivered ones.
///
/// A prefix scan, not a binary search, for the same reason
/// `rotation_split` is: an intake-written store stamps the sender's
/// event time, so its windows can move backwards, and a partition over
/// an unsorted array would return a count covering chunks the cursor has
/// never reached. A scan simply stops early — it holds too much, never
/// too little.
pub fn consumed_prefix(records: &[ChunkRecord], wf: u64, wl: u64) -> usize {
    records
        .iter()
        .take_while(|c| (c.last_write_ms, c.first_write_ms) < (wl, wf))
        .count()
}

/// Where one consumer stands in a store: what it has consumed, what it
/// is still holding, and whether retention has already outrun it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    /// Head chunks fully consumed — what interest-based retention could
    /// drop if this were the only consumer.
    pub consumed_chunks: usize,
    /// Chunks from the cursor's own chunk to the end of the store.
    pub behind_chunks: usize,
    /// Compressed bytes over that same range: what this consumer still
    /// has to read, and — for the furthest-behind one — what no
    /// retention policy honouring cursors could drop.
    pub behind_bytes: u64,
    /// Write-axis distance from the cursor to the store's newest write.
    pub behind_ms: u64,
    /// The store's oldest write, when it is NEWER than the cursor's
    /// position: what the cursor would resume at has been dropped, so
    /// everything between the two is gone. Retention acts on the head
    /// and nothing coordinates it with a consumer's progress, so this is
    /// reachable today — and silent, because resuming just reads from
    /// whatever is now oldest. Only a cursor that has delivered
    /// something can gap; a fresh one has no position to lose.
    pub gap_to_ms: Option<u64>,
}

impl Standing {
    pub fn caught_up(&self) -> bool {
        self.behind_chunks == 0
    }

    /// Nothing newer than this consumer's position exists in the store.
    /// Distinct from `caught_up`: a consumer keeping up with a LIVE store
    /// always sits inside the newest chunk, never past it, so measuring
    /// by chunks alone would report every healthy shipper as behind.
    pub fn at_live_edge(&self) -> bool {
        self.behind_ms == 0
    }

    /// How far behind, in one phrase — shared by `list`'s column and
    /// `info`'s per-consumer line so the two never characterise the same
    /// consumer differently. A gap outranks a distance: once the position
    /// is gone, how far behind it was is no longer the fact that matters.
    pub fn lag_text(&self) -> String {
        if self.gap_to_ms.is_some() {
            "GAP".to_string()
        } else if self.caught_up() {
            "caught up".to_string()
        } else if self.at_live_edge() {
            "at the live edge".to_string()
        } else {
            format!("{} behind", crate::query::human_duration(self.behind_ms))
        }
    }
}

/// Windows are mostly-sorted, so both extremes are found by scanning
/// (48 B per chunk) — the same choice `summarize_store` makes.
pub fn standing(c: &Cursor, records: &[ChunkRecord]) -> Standing {
    let consumed_chunks = consumed_prefix(records, c.wf, c.wl);
    let behind_bytes = match (records.get(consumed_chunks), records.last()) {
        (Some(next), Some(last)) => last.comp_end().saturating_sub(next.comp_start),
        _ => 0,
    };
    let newest = records.iter().map(|r| r.last_write_ms).max().unwrap_or(0);
    let oldest = records.iter().map(|r| r.first_write_ms).min();
    Standing {
        consumed_chunks,
        behind_chunks: records.len() - consumed_chunks,
        behind_bytes,
        behind_ms: newest.saturating_sub(c.wl),
        gap_to_ms: oldest.filter(|o| c.delivered > 0 && c.wl < *o),
    }
}

/// One consumer of a store, as a view renders it.
pub struct Consumer {
    /// Who: the cursor's own `consumer`, falling back to its file name
    /// for a hand-written cursor that names nobody.
    pub name: String,
    pub path: PathBuf,
    pub cursor: Cursor,
    pub standing: Standing,
}

/// Who is reading a store and how far behind they are. `None` from
/// `survey` means the store declares no `cursors` directory — not that
/// nothing reads it, which is the distinction a view must keep.
pub struct Survey {
    /// The declared directory these were found in.
    pub dir: PathBuf,
    /// Furthest-behind first.
    pub consumers: Vec<Consumer>,
    /// Files in the directory that could not be read as cursors —
    /// somebody else's state, or a torn write. Counted rather than
    /// reported one by one: the directory is shared by design, so a
    /// neighbour's file must not make a listing fail, and "nothing is
    /// reading this" must not look the same as "something in there could
    /// not be accounted for".
    pub unreadable: usize,
}

impl Survey {
    /// Compressed bytes no cursor-honouring retention could drop — the
    /// furthest-behind consumer's backlog.
    pub fn held_bytes(&self) -> u64 {
        self.consumers
            .iter()
            .map(|c| c.standing.behind_bytes)
            .max()
            .unwrap_or(0)
    }

    pub fn worst(&self) -> Option<&Consumer> {
        self.consumers.first()
    }

    pub fn gapped(&self) -> impl Iterator<Item = &Consumer> {
        self.consumers
            .iter()
            .filter(|c| c.standing.gap_to_ms.is_some())
    }
}

/// Every cursor in `dir` that is a position in the store `anchor`
/// identifies, placed against that store's chunks and ranked
/// furthest-behind first. Read-only and never fatal: a cursor for
/// another store is simply not ours, and the `.tmp` of an in-flight
/// `save` is skipped rather than counted as damage.
pub fn consumers_in(dir: &Path, anchor: &str, records: &[ChunkRecord]) -> std::io::Result<Survey> {
    let mut consumers = Vec::new();
    let mut unreadable = 0usize;
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| p.extension().is_none_or(|e| e != "tmp"))
        .collect();
    entries.sort();
    for path in entries {
        match Cursor::load(&path) {
            Ok(Some(c)) if c.store == anchor => {
                let name = if c.consumer.is_empty() {
                    path.file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    c.consumer.clone()
                };
                consumers.push(Consumer {
                    name,
                    path,
                    standing: standing(&c, records),
                    cursor: c,
                });
            }
            // A position in some other store: not ours, not a problem.
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(_) => unreadable += 1,
        }
    }
    // Furthest behind first: that consumer is the one deciding how much
    // of the store is unread, so it is the one an operator needs named.
    consumers.sort_by(|a, b| {
        b.standing
            .behind_bytes
            .cmp(&a.standing.behind_bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(Survey {
        dir: dir.to_path_buf(),
        consumers,
        unreadable,
    })
}

/// The whole view for one store: its declared cursor directory, every
/// consumer in it, and where each stands. `None` when the store declares
/// no directory — nothing to look at, as opposed to nothing found.
pub fn survey(
    dir: &Path,
    name: &str,
    bark: Option<&Map<String, Value>>,
    records: &[ChunkRecord],
) -> Option<Survey> {
    let cdir = declared_dir(bark)?;
    let anchor = store_anchor(dir, name, bark);
    Some(consumers_in(&cdir, &anchor, records).unwrap_or(
        // A declared directory that cannot be read is still a
        // declared directory: report it as empty, not as absent.
        Survey {
            dir: cdir,
            consumers: Vec::new(),
            unreadable: 0,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cur(wf: u64, wl: u64, n: u64) -> Cursor {
        Cursor {
            consumer: "test".into(),
            store: "id".into(),
            path: "p".into(),
            wf,
            wl,
            n,
            delivered: n,
        }
    }

    fn chunk(comp_start: u64, comp_len: u64, first: u64, last: u64) -> ChunkRecord {
        ChunkRecord {
            uncomp_start: 0,
            uncomp_len: 0,
            comp_start,
            comp_len,
            first_write_ms: first,
            last_write_ms: last,
        }
    }

    /// Three 10-byte chunks covering 100..400, contiguous in the trunk.
    fn three() -> Vec<ChunkRecord> {
        vec![
            chunk(0, 10, 100, 200),
            chunk(10, 10, 200, 300),
            chunk(20, 10, 300, 400),
        ]
    }

    #[test]
    fn consumed_prefix_holds_the_chunk_the_cursor_sits_in() {
        // At (200, 300) the cursor is inside the middle chunk: only the
        // first is fully consumed, because `n` counts inside the middle
        // one and `query --from` re-reads it from its start.
        assert_eq!(consumed_prefix(&three(), 200, 300), 1);
        assert_eq!(consumed_prefix(&three(), 0, 0), 0);
        // Past the end of the store: everything is consumed.
        assert_eq!(consumed_prefix(&three(), 400, 500), 3);
    }

    #[test]
    fn consumed_prefix_stops_at_a_backwards_window() {
        // An intake-written store can stamp a chunk with an older window
        // than its predecessor. A scan stops there; a binary search
        // would have counted the third chunk as consumed too.
        let records = vec![
            chunk(0, 10, 10, 20),
            chunk(10, 10, 500, 600),
            chunk(20, 10, 30, 40),
        ];
        assert_eq!(consumed_prefix(&records, 30, 45), 1);
    }

    #[test]
    fn resume_never_delivers_from_a_consumed_chunk() {
        // The invariant: the drop rule is the exact complement of the
        // resume rule. Every chunk `consumed_prefix` calls droppable must
        // be one a fresh `Resume` for that cursor would skip entirely.
        let records = three();
        for (wf, wl) in [(0, 0), (150, 200), (200, 300), (300, 400), (400, 500)] {
            let c = cur(wf, wl, 1);
            let k = consumed_prefix(&records, wf, wl);
            for r in &records[..k] {
                let mut resume = Resume::new(Some(&c));
                assert!(
                    !resume.deliver(r.first_write_ms, r.last_write_ms),
                    "cursor at ({wf}, {wl}) would still be delivered chunk {}..{}",
                    r.first_write_ms,
                    r.last_write_ms
                );
            }
            // And the first chunk NOT called droppable is one it would.
            if let Some(r) = records.get(k) {
                let mut resume = Resume::new(Some(&c));
                assert!(resume.deliver(r.first_write_ms, r.last_write_ms) || c.n > 0);
            }
        }
    }

    #[test]
    fn standing_reports_the_backlog_it_is_holding() {
        let records = three();
        let st = standing(&cur(200, 300, 2), &records);
        assert_eq!(st.consumed_chunks, 1);
        assert_eq!(st.behind_chunks, 2);
        assert_eq!(st.behind_bytes, 20); // from chunk 1's start to the end
        assert_eq!(st.behind_ms, 100); // newest write is 400
        assert_eq!(st.gap_to_ms, None);
        assert!(!st.caught_up());
        assert_eq!(st.lag_text(), "0.100s behind");

        let done = standing(&cur(400, 500, 1), &records);
        assert!(done.caught_up());
        assert_eq!(done.behind_bytes, 0);
        assert_eq!(done.lag_text(), "caught up");

        // Inside the newest chunk: nothing newer exists, so this is a
        // shipper keeping up, not one 0.000s behind.
        let edge = standing(&cur(300, 400, 2), &records);
        assert!(!edge.caught_up());
        assert!(edge.at_live_edge());
        assert_eq!(edge.behind_chunks, 1);
        assert_eq!(edge.lag_text(), "at the live edge");
    }

    #[test]
    fn a_dropped_position_is_a_gap_but_a_fresh_cursor_is_not() {
        let records = three();
        // Delivered something, and its position now predates the store:
        // retention outran it and the entries between are gone.
        let mut behind = cur(40, 50, 1);
        behind.delivered = 7;
        let st = standing(&behind, &records);
        assert_eq!(st.gap_to_ms, Some(100));
        assert_eq!(st.lag_text(), "GAP");

        // A cursor that has never delivered is at (0, 0) because it has
        // not started, not because it lost its place.
        let mut fresh = cur(0, 0, 0);
        fresh.delivered = 0;
        assert_eq!(standing(&fresh, &records).gap_to_ms, None);
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("timberfs-cursor-{tag}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn consumers_in_matches_by_store_identity() {
        let dir = scratch("survey");
        let mine = |name: &str, wl: u64| {
            let mut c = cur(wl, wl, 1);
            c.consumer = name.to_string();
            c.store = "id-a".into();
            c.delivered = 1;
            c.save(&dir.join(format!("{name}.cursor"))).unwrap();
        };
        mine("otlp", 400);
        mine("splitter", 200);
        // Another store's consumer, in the same shared directory.
        let mut other = cur(400, 400, 1);
        other.consumer = "elsewhere".into();
        other.store = "id-b".into();
        other.save(&dir.join("elsewhere.cursor")).unwrap();
        // An in-flight save, and something that is not a cursor at all.
        let mut pending = cur(400, 400, 1);
        pending.store = "id-a".into();
        pending.save(&dir.join("pending.tmp")).unwrap();
        fs::write(dir.join("notes.txt"), "hello").unwrap();

        // No chunks: this is about which cursors are OURS, so every
        // standing is empty and the ranking falls back to the name.
        let sv = consumers_in(&dir, "id-a", &[]).unwrap();
        let names: Vec<&str> = sv.consumers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["otlp", "splitter"]);
        assert_eq!(sv.unreadable, 1); // notes.txt; the .tmp is skipped
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn survey_needs_a_declaration_and_ranks_the_worst_first() {
        let dir = scratch("declared");
        let save = |name: &str, wl: u64| {
            let mut c = cur(wl, wl, 1);
            c.consumer = name.to_string();
            c.store = "id-a".into();
            c.delivered = 1;
            c.save(&dir.join(format!("{name}.cursor"))).unwrap();
        };
        save("ahead", 400);
        save("behind", 150);

        let records = three();
        let mut bark = Map::new();
        bark.insert("id".into(), Value::String("id-a".into()));
        // No `cursors` key: nothing to look at, which is not the same as
        // nothing found.
        assert!(survey(Path::new("/store"), "x.log", Some(&bark), &records).is_none());

        bark.insert("cursors".into(), Value::String(dir.display().to_string()));
        let sv = survey(Path::new("/store"), "x.log", Some(&bark), &records).unwrap();
        assert_eq!(sv.consumers.len(), 2);
        // Furthest behind first — that one decides what could be dropped.
        assert_eq!(sv.worst().unwrap().name, "behind");
        assert_eq!(sv.held_bytes(), 30);
        assert_eq!(sv.gapped().count(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_declared_directory_that_is_gone_reads_as_empty_not_absent() {
        let mut bark = Map::new();
        bark.insert(
            "cursors".into(),
            Value::String("/nonexistent/timberfs-cursors".into()),
        );
        let sv = survey(Path::new("/store"), "x.log", Some(&bark), &three()).unwrap();
        assert!(sv.consumers.is_empty());
    }

    #[test]
    fn store_anchor_prefers_declared_identity_over_the_path() {
        let mut bark = Map::new();
        bark.insert("id".into(), Value::String("abc".into()));
        assert_eq!(store_anchor(Path::new("/x"), "a.log", Some(&bark)), "abc");
        let anchor = store_anchor(Path::new("/x"), "a.log", None);
        assert!(anchor.starts_with("path:"), "{anchor}");
        assert!(anchor.ends_with("/x/a.log"), "{anchor}");
    }

    #[test]
    fn no_cursor_delivers_everything() {
        let mut r = Resume::new(None);
        assert!(r.deliver(1, 2));
        assert!(r.deliver(1, 2));
        assert_eq!(r.skipped(), 0);
    }

    #[test]
    fn skips_the_delivered_prefix_of_the_boundary_chunk() {
        let c = cur(100, 200, 2);
        let mut r = Resume::new(Some(&c));
        assert!(!r.deliver(100, 200));
        assert!(!r.deliver(100, 200));
        assert!(r.deliver(100, 200)); // the third is new
        assert!(r.deliver(100, 200));
        assert_eq!(r.skipped(), 2);
    }

    #[test]
    fn skips_whole_chunks_before_the_cursor() {
        let c = cur(100, 200, 1);
        let mut r = Resume::new(Some(&c));
        assert!(!r.deliver(1, 50));
        assert!(!r.deliver(60, 199));
        assert!(!r.deliver(100, 200)); // the one already delivered
        assert!(r.deliver(100, 200));
        assert_eq!(r.skipped(), 3);
    }

    #[test]
    fn a_later_chunk_ends_the_skipping_even_mid_window() {
        let c = cur(100, 200, 5);
        let mut r = Resume::new(Some(&c));
        assert!(!r.deliver(100, 200));
        assert!(r.deliver(201, 300));
        // Once past the cursor, nothing is ever dropped again — including
        // a repeated window, which would otherwise re-skip live entries.
        assert!(r.deliver(100, 200));
        assert_eq!(r.skipped(), 1);
    }

    #[test]
    fn advance_restarts_n_on_a_new_window() {
        let mut c = cur(0, 0, 0);
        c.delivered = 0;
        c.advance(10, 20);
        c.advance(10, 20);
        assert_eq!((c.wf, c.wl, c.n, c.delivered), (10, 20, 2, 2));
        c.advance(21, 30);
        assert_eq!((c.wf, c.wl, c.n, c.delivered), (21, 30, 1, 3));
    }

    #[test]
    fn roundtrip_through_a_file() {
        let dir = std::env::temp_dir().join(format!("timberfs-cursor-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.cursor");
        let c = cur(10, 20, 3);
        c.save(&path).unwrap();
        let back = Cursor::load(&path).unwrap().unwrap();
        assert_eq!(back, c);
        assert!(back.check_store("id", &path).is_ok());
        assert!(back.check_store("other", &path).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_cursor_is_none_but_garbage_is_an_error() {
        let dir = std::env::temp_dir().join(format!("timberfs-cursor-bad-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nope.cursor");
        assert!(Cursor::load(&missing).unwrap().is_none());
        let bad = dir.join("bad.cursor");
        fs::write(&bad, "not json").unwrap();
        assert!(Cursor::load(&bad).is_err());
        let no_store = dir.join("nostore.cursor");
        fs::write(&no_store, "{\"wl\": 5}").unwrap();
        assert!(Cursor::load(&no_store).is_err());
        fs::remove_dir_all(&dir).ok();
    }
}
