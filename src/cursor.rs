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
use std::path::Path;

use anyhow::{bail, Context};
use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};

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
