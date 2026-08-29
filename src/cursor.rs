//! A consumer's durable position in a store's entry stream.
//!
//! Kept OUTSIDE the store, at a path the operator names: a cursor is
//! CONSUMER state, not store state. Several consumers read one store at
//! different positions, a cursor must not travel inside a `.timber`
//! bundle as if it were provenance, and retention dropping chunks must
//! not drop it. `.bark` is what the store declares about itself; this is
//! what someone reading it remembers.
//!
//! The position is a CHUNK NUMBER, the only axis that cannot move
//! backwards. Logline timestamps go backwards by nature — an entry
//! written now can be stamped an hour ago — and the write axis does too,
//! `now_ms()` being a wall clock that an NTP step or a `date -s` can push
//! back, and an intake stamping the sender's event time on purpose. A
//! range or a prefix survives that; a single point does not, which is
//! what a cursor is.
//!
//! A position is `(seq, n)`: the number of the chunk an entry arrived in,
//! plus how many entries of that chunk were already delivered.
//! `query --records --follow --from-chunk <seq>` seeks to exactly that
//! chunk and `n` skips what was already delivered inside it.
//!
//! An entry from the LIVE EDGE carries no chunk number, because its chunk
//! does not exist yet. It is delivered and counted, but the position does
//! not move: there is nowhere inside a chunk that has not been written to
//! resume from. The cost is re-reading from the last chunk boundary after
//! a restart, which chunk-granular resume already does.
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
/// so an operator can rewind by editing `seq` (the supported edit) — but
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
    /// The chunk this consumer stands in. `None` on a cursor written
    /// before chunk numbers existed, which `resolve` converts on the next
    /// save, and on one that has never delivered anything.
    pub seq: Option<u64>,
    /// Entries already delivered from chunk `seq`.
    pub n: u64,
    /// The write time of the newest entry delivered — INFORMATIONAL, so a
    /// human reading the file can see roughly where the position is. It
    /// decides nothing; `seq` is the position. On a pre-numbering cursor
    /// it is the only position there is, and `resolve` reads it once.
    pub wl: u64,
    /// Total entries delivered by this consumer, for observability.
    pub delivered: u64,
}

impl Cursor {
    pub fn new(consumer: &str, store: &str, path: &str) -> Cursor {
        Cursor {
            consumer: consumer.to_string(),
            store: store.to_string(),
            path: path.to_string(),
            seq: None,
            n: 0,
            wl: 0,
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
            seq: map.get("seq").and_then(Value::as_u64),
            n: u64_at("n"),
            wl: u64_at("wl"),
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

    /// Count one delivered entry. `chunk` is `None` for an entry read from
    /// the live edge: it is counted, and `wl` follows it so the file still
    /// shows where the consumer is in time, but the POSITION does not move
    /// — a chunk that does not exist yet has no inside to resume from. The
    /// next run therefore re-reads from the last chunk boundary, which
    /// chunk-granular resume already does anyway.
    pub fn advance(&mut self, chunk: Option<u64>, wl: u64) {
        self.delivered += 1;
        self.wl = wl;
        let Some(seq) = chunk else {
            return;
        };
        if self.seq != Some(seq) {
            self.seq = Some(seq);
            self.n = 0;
        }
        self.n += 1;
    }

    /// Convert a pre-numbering cursor: resolve its write-time position to a
    /// chunk the way `query --from` does — the first chunk whose window
    /// reaches it — and start at that chunk's BEGINNING.
    ///
    /// `n` is deliberately RESET. It counted entries within a write
    /// WINDOW, so carrying it over would skip entries nobody received
    /// whenever the resolved chunk is not the one the old cursor sat in
    /// (its chunk dropped, or two chunks sharing a window). Resetting costs
    /// at most one re-delivered chunk, which at-least-once already permits:
    /// wrong twice is recoverable, wrong once and skipped is not.
    ///
    /// A pure function of `(wl, records)`, so re-running it is harmless —
    /// which is what lets the result be persisted only on the first save
    /// AFTER a delivery, keeping the rule that a cursor is never written
    /// ahead of a durability proof.
    pub fn resolve(&mut self, records: &[ChunkRecord]) -> Option<u64> {
        if self.seq.is_some() {
            return self.seq;
        }
        let seq = match records.iter().find(|c| c.last_write_ms >= self.wl) {
            Some(c) => c.seq,
            // Past everything the store holds: start after the newest, so
            // nothing already delivered is re-sent.
            None => records.last()?.seq + 1,
        };
        self.seq = Some(seq);
        self.n = 0;
        Some(seq)
    }

    /// Should this cursor be converted at all, and to what?
    ///
    /// `Some(Ok(seq))` converted, `Some(Err(()))` needed converting but had
    /// no chunks to resolve against yet, `None` nothing to do. The GUARD is
    /// the part worth having in one place: a cursor with no `seq` and
    /// nothing delivered is not pre-numbering, it is NEW — its `wl` is a
    /// default rather than a position, and resolving against it would
    /// silently override `--start`. Only a cursor that has delivered
    /// something has a write time that means anything.
    pub fn resolve_if_pre_numbering(&mut self, records: &[ChunkRecord]) -> Option<Result<u64, ()>> {
        if self.seq.is_some() || self.delivered == 0 {
            return None;
        }
        Some(self.resolve(records).ok_or(()))
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
        match self.seq {
            Some(seq) => map.insert("seq".into(), Value::from(seq)),
            None => map.insert("seq".into(), Value::Null),
        };
        map.insert("n".into(), Value::from(self.n));
        map.insert("wl".into(), Value::from(self.wl));
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

/// Skips what a resumed stream re-delivers. `--from-chunk` seeks to a whole
/// chunk, so the first chunk of a resumed stream is re-read from its start;
/// this drops the entries inside it the cursor says were already delivered,
/// and then gets out of the way for good.
pub struct Resume {
    at: Option<(u64, u64)>,
    seen_in_chunk: u64,
    skipped: u64,
    done: bool,
}

impl Resume {
    pub fn new(cursor: Option<&Cursor>) -> Resume {
        Resume {
            at: cursor.and_then(|c| c.seq.map(|seq| (seq, c.n))),
            seen_in_chunk: 0,
            skipped: 0,
            done: false,
        }
    }

    /// Should this entry be delivered? Chunk numbers only move forward, so
    /// this is an ordinary comparison — no window ordering, and none of the
    /// intake/clock-step caveats the write axis carried, where a position
    /// could move backwards and a resume could SKIP.
    ///
    /// `None` is the live edge, which is by construction newer than every
    /// chunk: always delivered.
    pub fn deliver(&mut self, chunk: Option<u64>) -> bool {
        if self.done {
            return true;
        }
        let Some((cseq, n)) = self.at else {
            self.done = true;
            return true;
        };
        let Some(seq) = chunk else {
            self.done = true;
            return true;
        };
        if seq < cseq {
            self.skipped += 1;
            return false;
        }
        if seq == cseq && self.seen_in_chunk < n {
            self.seen_in_chunk += 1;
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

/// How many head chunks a cursor has FULLY consumed: the exact complement
/// of what `Resume` would deliver, since anything it would hand over must
/// still exist. The cursor's OWN chunk is not counted — `n` counts inside
/// it and a resume re-reads it from the start, so dropping it would make
/// `n` skip live entries instead of already-delivered ones.
///
/// A binary search, which the write axis could not support: chunk numbers
/// are dense and only increase, so a partition is exact where a partition
/// over write windows would have counted chunks the cursor never reached.
/// A cursor with no position yet has consumed nothing.
pub fn consumed_prefix(records: &[ChunkRecord], seq: Option<u64>) -> usize {
    let Some(seq) = seq else {
        return 0;
    };
    records.partition_point(|c| c.seq < seq)
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
    /// How many chunks were dropped between this cursor's position and the
    /// oldest the store still holds — an EXACT count, where comparing
    /// timestamps could only infer a duration. Retention acts on the head
    /// and nothing coordinates it with a consumer's progress, so this is
    /// reachable today, and silent unless reported: a resume just starts
    /// from whatever is now oldest. Only a cursor with a position can gap;
    /// one that has never delivered has nothing to lose.
    pub gap_chunks: Option<u64>,
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
        if self.gap_chunks.is_some() {
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
    let consumed_chunks = consumed_prefix(records, c.seq);
    let behind_bytes = match (records.get(consumed_chunks), records.last()) {
        (Some(next), Some(last)) => last.comp_end().saturating_sub(next.comp_start),
        _ => 0,
    };
    // Windows are only mostly sorted, so both extremes are scanned for
    // (48 B per chunk) — the same choice `summarize_store` makes.
    let newest = records.iter().map(|r| r.last_write_ms).max().unwrap_or(0);
    Standing {
        consumed_chunks,
        behind_chunks: records.len() - consumed_chunks,
        behind_bytes,
        behind_ms: newest.saturating_sub(c.wl),
        gap_chunks: match (c.seq, records.first()) {
            (Some(seq), Some(first)) if seq < first.seq => Some(first.seq - seq),
            _ => None,
        },
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
            .filter(|c| c.standing.gap_chunks.is_some())
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

    /// A cursor standing in chunk `seq`, `n` entries into it.
    fn cur(seq: u64, n: u64) -> Cursor {
        Cursor {
            consumer: "test".into(),
            store: "id".into(),
            path: "p".into(),
            seq: Some(seq),
            n,
            wl: 100 + seq,
            delivered: n.max(1),
        }
    }

    fn chunk(seq: u64, comp_start: u64, comp_len: u64, first: u64, last: u64) -> ChunkRecord {
        ChunkRecord {
            uncomp_start: 0,
            uncomp_len: 0,
            comp_start,
            comp_len,
            first_write_ms: first,
            last_write_ms: last,
            seq,
        }
    }

    /// Three 10-byte chunks numbered 0..2, covering 100..400.
    fn three() -> Vec<ChunkRecord> {
        vec![
            chunk(0, 0, 10, 100, 200),
            chunk(1, 10, 10, 200, 300),
            chunk(2, 20, 10, 300, 400),
        ]
    }

    #[test]
    fn consumed_prefix_holds_the_chunk_the_cursor_sits_in() {
        // `n` counts inside the cursor's own chunk and a resume re-reads it
        // from the start, so that chunk is not consumed.
        assert_eq!(consumed_prefix(&three(), Some(1)), 1);
        assert_eq!(consumed_prefix(&three(), Some(0)), 0);
        assert_eq!(consumed_prefix(&three(), Some(3)), 3);
        // A cursor with no position has consumed nothing.
        assert_eq!(consumed_prefix(&three(), None), 0);
    }

    #[test]
    fn consumed_prefix_counts_numbers_not_positions() {
        // After a head-drop the survivors keep their numbers, so the record
        // at index 0 is chunk 2. A cursor at 3 has consumed exactly one of
        // what is left — not three, which is what counting positions would
        // have said, and which would have dropped live data under stage 3.
        let dropped = vec![
            chunk(2, 0, 10, 300, 400),
            chunk(3, 10, 10, 400, 500),
            chunk(4, 20, 10, 500, 600),
        ];
        assert_eq!(consumed_prefix(&dropped, Some(3)), 1);
        assert_eq!(consumed_prefix(&dropped, Some(2)), 0);
        // A position older than anything left consumes nothing of it.
        assert_eq!(consumed_prefix(&dropped, Some(1)), 0);
    }

    #[test]
    fn resume_never_delivers_from_a_consumed_chunk() {
        // The invariant: the drop rule is the exact complement of the
        // resume rule. Every chunk `consumed_prefix` calls droppable must
        // be one a fresh `Resume` for that cursor would skip entirely.
        let records = three();
        for seq in 0..=3 {
            let c = cur(seq, 1);
            let k = consumed_prefix(&records, Some(seq));
            for r in &records[..k] {
                let mut resume = Resume::new(Some(&c));
                assert!(
                    !resume.deliver(Some(r.seq)),
                    "cursor at {seq} would still be delivered chunk {}",
                    r.seq
                );
            }
        }
    }

    #[test]
    fn a_live_edge_entry_is_delivered_but_moves_nothing() {
        let mut c = cur(2, 3);
        let before = (c.seq, c.n);
        c.advance(None, 999);
        assert_eq!((c.seq, c.n), before, "no chunk to stand in, so no move");
        assert_eq!(c.delivered, 4, "still counted as delivered");
        assert_eq!(c.wl, 999, "and the informational time follows it");
        // A resumed stream always delivers the live edge: it is newer than
        // every chunk by construction.
        let mut resume = Resume::new(Some(&c));
        assert!(resume.deliver(None));
    }

    #[test]
    fn standing_reports_the_backlog_it_is_holding() {
        let records = three();
        let st = standing(&cur(1, 2), &records);
        assert_eq!(st.consumed_chunks, 1);
        assert_eq!(st.behind_chunks, 2);
        assert_eq!(st.behind_bytes, 20); // from chunk 1's start to the end
        assert_eq!(st.gap_chunks, None);
        assert!(!st.caught_up());

        let done = standing(&cur(3, 1), &records);
        assert!(done.caught_up());
        assert_eq!(done.behind_bytes, 0);
        assert_eq!(done.lag_text(), "caught up");
    }

    #[test]
    fn a_gap_is_counted_in_chunks_not_inferred_from_time() {
        // Retention dropped chunks 0 and 1; a cursor still at 0 has lost
        // exactly two, which the numbers state rather than imply.
        let records = vec![chunk(2, 0, 10, 300, 400), chunk(3, 10, 10, 400, 500)];
        let st = standing(&cur(0, 1), &records);
        assert_eq!(st.gap_chunks, Some(2));
        assert_eq!(st.lag_text(), "GAP");
        // A cursor that has never delivered has no position to lose.
        let fresh = Cursor::new("c", "id", "p");
        assert_eq!(standing(&fresh, &records).gap_chunks, None);
    }

    #[test]
    fn resolving_an_old_cursor_lands_on_a_chunk_and_resets_n() {
        // Pre-numbering cursors hold a write time. Resolution is the same
        // rule `query --from` uses: the first chunk whose window reaches it.
        let records = three();
        let mut c = Cursor::new("c", "id", "p");
        c.wl = 250;
        c.n = 7;
        c.delivered = 7;
        assert_eq!(c.resolve(&records), Some(1));
        assert_eq!(c.seq, Some(1));
        // `n` counted entries in a WINDOW, so keeping it could skip entries
        // nobody received; at most one chunk is re-delivered instead.
        assert_eq!(c.n, 0);
        // Idempotent, which is what lets it be persisted only after a
        // delivery rather than at startup.
        assert_eq!(c.resolve(&records), Some(1));
    }

    /// The guard, which decides whether to convert at all. A cursor with
    /// no `seq` and nothing delivered is NOT pre-numbering — it is new, and
    /// its `wl` is a default rather than a position. Resolving against that
    /// would override `--start` with a chunk nobody chose, which is a
    /// silent skip of everything before it.
    #[test]
    fn only_a_cursor_that_delivered_something_is_pre_numbering() {
        let records = three();
        let mut fresh = cur(0, 0);
        fresh.seq = None;
        fresh.delivered = 0;
        assert_eq!(
            fresh.resolve_if_pre_numbering(&records),
            None,
            "a new cursor has no position to convert"
        );
        assert_eq!(fresh.seq, None, "...and must be left alone for --start");

        let mut old = cur(0, 0);
        old.seq = None;
        old.wl = 250;
        old.delivered = 9;
        assert_eq!(old.resolve_if_pre_numbering(&records), Some(Ok(1)));

        // Already numbered: nothing to do, and the position stands.
        let mut numbered = cur(2, 5);
        assert_eq!(numbered.resolve_if_pre_numbering(&records), None);
        assert_eq!((numbered.seq, numbered.n), (Some(2), 5));

        // A store with no chunks yet is told apart from "nothing to do":
        // the shipper honours --start until there are some, and says so.
        let mut nothing_to_resolve = cur(0, 0);
        nothing_to_resolve.seq = None;
        nothing_to_resolve.delivered = 9;
        assert_eq!(
            nothing_to_resolve.resolve_if_pre_numbering(&[]),
            Some(Err(()))
        );
    }

    #[test]
    fn resolving_clamps_to_the_ends_of_the_store() {
        let records = three();
        // Older than anything held: start at the oldest survivor, the same
        // place a timestamp resume would have landed.
        let mut old = Cursor::new("c", "id", "p");
        old.wl = 1;
        old.delivered = 5;
        assert_eq!(old.resolve(&records), Some(0));
        // Past the newest: start after it, so nothing already sent is
        // re-sent.
        let mut ahead = Cursor::new("c", "id", "p");
        ahead.wl = 9_999;
        ahead.delivered = 5;
        assert_eq!(ahead.resolve(&records), Some(3));
        // Nothing to resolve against yet: leave it unresolved rather than
        // inventing a position.
        let mut empty = Cursor::new("c", "id", "p");
        empty.wl = 500;
        assert_eq!(empty.resolve(&[]), None);
        assert_eq!(empty.seq, None);
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
        let mine = |name: &str, seq: u64| {
            let mut c = cur(seq, 1);
            c.consumer = name.to_string();
            c.store = "id-a".into();
            c.save(&dir.join(format!("{name}.cursor"))).unwrap();
        };
        mine("otlp", 2);
        mine("splitter", 1);
        // Another store's consumer, in the same shared directory.
        let mut other = cur(2, 1);
        other.consumer = "elsewhere".into();
        other.store = "id-b".into();
        other.save(&dir.join("elsewhere.cursor")).unwrap();
        // An in-flight save, and something that is not a cursor at all.
        let mut pending = cur(2, 1);
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
        let save = |name: &str, seq: u64| {
            let mut c = cur(seq, 1);
            c.consumer = name.to_string();
            c.store = "id-a".into();
            c.save(&dir.join(format!("{name}.cursor"))).unwrap();
        };
        save("ahead", 3);
        save("behind", 0);

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
        assert!(r.deliver(Some(0)));
        assert!(r.deliver(Some(0)));
        assert_eq!(r.skipped(), 0);
        // And a cursor with no position yet behaves the same.
        let fresh = Cursor::new("c", "id", "p");
        let mut r = Resume::new(Some(&fresh));
        assert!(r.deliver(Some(0)));
    }

    #[test]
    fn skips_the_delivered_prefix_of_the_boundary_chunk() {
        let c = cur(2, 2);
        let mut r = Resume::new(Some(&c));
        assert!(!r.deliver(Some(2)));
        assert!(!r.deliver(Some(2)));
        assert!(r.deliver(Some(2))); // the third is new
        assert!(r.deliver(Some(2)));
        assert_eq!(r.skipped(), 2);
    }

    #[test]
    fn skips_whole_chunks_before_the_cursor() {
        let c = cur(2, 1);
        let mut r = Resume::new(Some(&c));
        assert!(!r.deliver(Some(0)));
        assert!(!r.deliver(Some(1)));
        assert!(!r.deliver(Some(2))); // the one already delivered
        assert!(r.deliver(Some(2)));
        assert_eq!(r.skipped(), 3);
    }

    #[test]
    fn a_later_chunk_ends_the_skipping_for_good() {
        let c = cur(2, 5);
        let mut r = Resume::new(Some(&c));
        assert!(!r.deliver(Some(2)));
        assert!(r.deliver(Some(3)));
        // Once past the cursor, nothing is ever dropped again — including a
        // repeated number, which would otherwise re-skip live entries.
        assert!(r.deliver(Some(2)));
        assert_eq!(r.skipped(), 1);
    }

    #[test]
    fn advance_restarts_n_on_a_new_chunk() {
        let mut c = Cursor::new("c", "id", "p");
        c.advance(Some(10), 500);
        c.advance(Some(10), 501);
        assert_eq!((c.seq, c.n, c.delivered), (Some(10), 2, 2));
        c.advance(Some(11), 502);
        assert_eq!((c.seq, c.n, c.delivered), (Some(11), 1, 3));
    }

    #[test]
    fn roundtrip_through_a_file() {
        let dir = scratch("roundtrip");
        let path = dir.join("app.cursor");
        let c = cur(10, 3);
        c.save(&path).unwrap();
        let back = Cursor::load(&path).unwrap().unwrap();
        assert_eq!(back, c);
        assert!(back.check_store("id", &path).is_ok());
        assert!(back.check_store("other", &path).is_err());
        // A pre-numbering file has no `seq`, which is how `resolve` knows
        // to convert it.
        let old = dir.join("old.cursor");
        fs::write(
            &old,
            "{\"consumer\":\"c\",\"store\":\"id\",\"wf\":1,\"wl\":250,\"n\":4,\"delivered\":9}",
        )
        .unwrap();
        let back = Cursor::load(&old).unwrap().unwrap();
        assert_eq!(
            (back.seq, back.wl, back.n, back.delivered),
            (None, 250, 4, 9)
        );
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
