//! Reading the write-ahead sidecar while it is being written: the live
//! edge of a store, for `query --follow`.
//!
//! A flushed chunk is the unit of visibility in the trunk, so a follower
//! reading only chunks lags by the writer's `--flush-age` — a minute on a
//! followed store, which is a long time to wait during an incident. The
//! bytes are not missing meanwhile: a wal-declared store already mirrors
//! every entry into `<name>.sap` (sap.rs) as it arrives, for durability.
//! This is the read side of that same file, and it costs the writer
//! nothing new.
//!
//! What makes it safe are the properties docs/design.md calls
//! load-bearing:
//!
//!   * A segment's content is exactly the next chunk's bytes. So a reader
//!     that served N bytes live drops N bytes from the front of the chunk
//!     that segment becomes (`skip_for_chunk`) — no double-emit and no
//!     gap. It also means a live entry HAS an address: the segment starts
//!     where the store's chunks end, so `served` counts into the same
//!     tape a chunk's bytes sit on (`LiveTail::served`). Logical offsets
//!     are the ones a head trim rebases; the tape is what it holds
//!     still, because what leaves the store is added back to every
//!     address.
//!   * The swap is a rename, so a reader's own descriptor never sees
//!     bytes mutate or truncate, and a NEW INODE is the generation
//!     marker. The header's bases are rewritten in place by a head trim
//!     (`Sap::refresh_base`), so the inode — never the base — is what
//!     says a segment rolled.
//!   * Only the longest valid (CRC-checked) prefix is read. A torn tail
//!     is a writer mid-append, not damage: it is simply re-read.
//!
//! Nothing here writes, and it deliberately does not reuse
//! `sap::replay`: that is the recovery path, and it REMOVES a file whose
//! header it cannot parse — right for a writer opening its own store,
//! wrong for every reader.

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::sap::{self, SapEntry, HEADER_LEN, SAP_MAGIC};

/// The segment currently being tailed, identified by the inode the reader
/// is actually holding (fstat, not a path lookup).
struct Segment {
    dev: u64,
    ino: u64,
    /// File offset of the first byte not yet parsed.
    off: u64,
    /// Payload bytes of this segment already served — or deliberately
    /// passed over, when the follow started mid-segment: both mean "do
    /// not emit these again from the chunk".
    served: u64,
    /// Where this segment starts in the store's logical stream, as its
    /// header states it. Re-read on every poll: a head trim rewrites it
    /// in place, and reading it here — from the same descriptor the
    /// records come off — is what keeps the address and the bytes from
    /// coming from two different generations of the store.
    base: u64,
}

/// A reader's view of a store's live write-ahead segment.
pub struct LiveTail {
    path: PathBuf,
    seg: Option<Segment>,
    /// Payload bytes served out of segments that have since been sealed,
    /// waiting for the chunks those segments became.
    skip: u64,
}

/// The segment's `uncomp_base`: where its bytes sit in the store's
/// logical stream. `None` for a file too short to hold a header — a
/// writer between creating it and writing one.
fn read_base(f: &File) -> Option<u64> {
    let mut b = [0u8; 8];
    f.read_exact_at(&mut b, 16).ok()?;
    Some(u64::from_le_bytes(b))
}

impl LiveTail {
    /// Attach to `<dir>/<name>.sap` if it exists. `from_now` skips over
    /// whatever the segment already holds — the "follow only new data"
    /// default; `--tail`/`--from` pass false, because unflushed entries
    /// are the newest ones and exactly what those ask for.
    pub fn open(dir: &Path, name: &str, from_now: bool) -> LiveTail {
        let mut t = LiveTail {
            path: crate::format::sap_path(dir, name),
            seg: None,
            skip: 0,
        };
        if from_now {
            // Attach and pass over what the segment already holds: not
            // new, but still part of the chunk it will become — which is
            // what `poll` counts into `served`.
            let _ = t.poll();
        } else {
            let _ = t.reconcile();
        }
        t
    }

    /// A tail bound to nothing: a `.timber` bundle is a finished
    /// artifact, with no writer to follow.
    pub fn none() -> LiveTail {
        LiveTail {
            path: PathBuf::new(),
            seg: None,
            skip: 0,
        }
    }

    /// Is a segment being tailed right now? False for a store with no
    /// wal declared — the caller then behaves exactly as before.
    pub fn live(&self) -> bool {
        self.seg.is_some()
    }

    /// Notice a sealed or replaced segment. Call this BEFORE emitting
    /// newly flushed chunks: a flush that landed since the ring snapshot
    /// was taken has to be accounted for before its chunk is written out,
    /// or the bytes already served live go out twice.
    pub fn reconcile(&mut self) -> io::Result<()> {
        match File::open(&self.path) {
            Ok(f) => {
                self.attach(&f)?;
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Either mid-flush (sealed, the fresh segment not created
                // yet) or no wal at all. Both mean: whatever we served
                // out of the old segment now belongs to a chunk.
                self.roll();
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// How much of a chunk about to be emitted was already served live.
    /// Chunks arrive in order, so a skip larger than one chunk simply
    /// carries into the next.
    pub fn skip_for_chunk(&mut self, chunk_len: u64) -> u64 {
        let s = self.skip.min(chunk_len);
        self.skip -= s;
        s
    }

    /// The chunk a pending skip was meant for is gone — retention
    /// dropped it between selection and read. Forget the skip: it
    /// described bytes that no longer exist anywhere, and holding it
    /// would cut the front off an unrelated later chunk. Erring toward a
    /// possible repeat rather than a possible gap is the same choice the
    /// consumer cursor makes.
    pub fn forget_skip(&mut self) {
        self.skip = 0;
    }

    /// The entries appended since the last call, in order, with WHERE
    /// the first of them sits in the store's logical stream — the
    /// segment's own base plus what this reader has already taken out of
    /// it. Add what has left the store and the result is an address on
    /// the tape, the same one the chunk this segment becomes will report.
    ///
    /// ⚠ The base is read here rather than derived from the ring index.
    /// A flush landing between a caller's ring snapshot and this call
    /// creates a new segment further along the stream, and a derived base
    /// would place these entries a whole chunk too low.
    ///
    /// An empty result is the normal quiet case; the address still says
    /// where the next entry will sit.
    pub fn poll(&mut self) -> io::Result<(u64, Vec<SapEntry>)> {
        let f = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.roll();
                return Ok((0, Vec::new()));
            }
            Err(e) => return Err(e),
        };
        self.attach(&f)?;
        let Some(seg) = self.seg.as_mut() else {
            return Ok((0, Vec::new()));
        };
        // In place, by a head trim: same segment, same bytes, new
        // coordinates.
        if let Some(base) = read_base(&f) {
            seg.base = base;
        }
        let at = seg.base + seg.served;
        // fstat, so the length describes the inode we are holding — a
        // rename between the open and here cannot mislead it.
        let len = f.metadata()?.len();
        if len <= seg.off {
            // Nothing new. `<` is not reachable in a live segment (it is
            // only ever appended to, and the writer's own resume
            // truncation cannot cut into a CRC-valid prefix) — clamping
            // rather than re-reading keeps that impossible case from
            // turning into duplicates.
            seg.off = seg.off.min(len);
            return Ok((at, Vec::new()));
        }
        let mut buf = vec![0u8; (len - seg.off) as usize];
        f.read_exact_at(&mut buf, seg.off)?;
        let (entries, used) = sap::parse_records(&buf);
        seg.off += used as u64;
        seg.served += entries.iter().map(|e| e.payload.len() as u64).sum::<u64>();
        Ok((at, entries))
    }

    /// Bind to the segment `f` refers to, rolling first if it is not the
    /// one we were reading.
    fn attach(&mut self, f: &File) -> io::Result<()> {
        let m = f.metadata()?;
        let (dev, ino) = (m.dev(), m.ino());
        if let Some(seg) = &self.seg {
            if seg.dev == dev && seg.ino == ino {
                return Ok(()); // same segment, base rewrites included
            }
            self.roll();
        }
        // A segment whose header is not there (or not ours) yet: the
        // writer creates the file and writes the header in one step, but
        // a reader can still land between them.
        if m.len() < HEADER_LEN {
            return Ok(());
        }
        let mut magic = [0u8; 8];
        if f.read_exact_at(&mut magic, 0).is_err() || &magic != SAP_MAGIC {
            return Ok(());
        }
        // No base, no attachment: a zero would be an address, and a
        // wrong one. The same shape as the magic check above — a reader
        // that landed mid-header simply tries again.
        let Some(base) = read_base(f) else {
            return Ok(());
        };
        self.seg = Some(Segment {
            dev,
            ino,
            off: HEADER_LEN,
            served: 0,
            base,
        });
        Ok(())
    }

    /// The segment we were reading is gone: hand what we served out of it
    /// to the chunk it became.
    fn roll(&mut self) {
        if let Some(seg) = self.seg.take() {
            self.skip += seg.served;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sap::Sap;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> TempDir {
            let p = std::env::temp_dir().join(format!(
                "timberfs-live-test-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn payloads(entries: &[SapEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.payload).to_string())
            .collect()
    }

    #[test]
    fn tails_a_segment_as_it_grows() {
        let d = TempDir::new();
        let p = crate::format::sap_path(d.path(), "app");
        let mut w = Sap::create(&p, 0, 0).unwrap();
        let mut t = LiveTail::open(d.path(), "app", true);
        assert!(t.live());

        w.append(1, 1, b"one\n").unwrap();
        w.sync().unwrap();
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["one\n"]);
        // Nothing new is not an error, and does not re-emit.
        assert!(t.poll().unwrap().1.is_empty());

        w.append(2, 2, b"two\n").unwrap();
        w.sync().unwrap();
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["two\n"]);
    }

    #[test]
    fn from_now_passes_over_what_was_already_there() {
        let d = TempDir::new();
        let p = crate::format::sap_path(d.path(), "app");
        let mut w = Sap::create(&p, 0, 0).unwrap();
        w.append(1, 1, b"before\n").unwrap();
        w.sync().unwrap();

        let mut t = LiveTail::open(d.path(), "app", true);
        assert!(t.poll().unwrap().1.is_empty(), "already-there is not new");
        // …but it still belongs to the chunk that segment becomes, so it
        // must be skipped there too.
        assert_eq!(t.skip_for_chunk(100), 0, "not yet sealed");

        w.append(2, 2, b"after\n").unwrap();
        w.sync().unwrap();
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["after\n"]);
    }

    #[test]
    fn tail_mode_reads_what_is_already_buffered() {
        let d = TempDir::new();
        let p = crate::format::sap_path(d.path(), "app");
        let mut w = Sap::create(&p, 0, 0).unwrap();
        w.append(1, 1, b"buffered\n").unwrap();
        w.sync().unwrap();

        let mut t = LiveTail::open(d.path(), "app", false);
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["buffered\n"]);
    }

    #[test]
    fn a_torn_tail_is_re_read_not_lost() {
        let d = TempDir::new();
        let p = crate::format::sap_path(d.path(), "app");
        let mut w = Sap::create(&p, 0, 0).unwrap();
        let mut t = LiveTail::open(d.path(), "app", true);

        w.append(1, 1, b"whole\n").unwrap();
        w.sync().unwrap();
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["whole\n"]);

        // A record only half on disk: the reader must not consume it…
        let full = {
            let mut w2 = Sap::create(&d.path().join("scratch.sap"), 0, 0).unwrap();
            w2.append(2, 2, b"torn\n").unwrap();
            w2.sync().unwrap();
            std::fs::read(d.path().join("scratch.sap")).unwrap()
        };
        let rec = &full[HEADER_LEN as usize..];
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        let end = std::fs::metadata(&p).unwrap().len();
        f.write_all_at(&rec[..rec.len() - 3], end).unwrap();
        assert!(
            t.poll().unwrap().1.is_empty(),
            "a torn record is not a record"
        );

        // …and must see it once the rest lands.
        let end = std::fs::metadata(&p).unwrap().len();
        f.write_all_at(&rec[rec.len() - 3..], end).unwrap();
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["torn\n"]);
    }

    #[test]
    fn a_sealed_segment_becomes_a_skip_for_its_chunk() {
        let d = TempDir::new();
        let p = crate::format::sap_path(d.path(), "app");
        let mut w = Sap::create(&p, 0, 0).unwrap();
        let mut t = LiveTail::open(d.path(), "app", true);
        w.append(1, 1, b"served-live\n").unwrap();
        w.sync().unwrap();
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["served-live\n"]);

        // The flush: seal (rename), then a fresh segment.
        std::fs::rename(&p, crate::format::sap_seal_path(d.path(), "app")).unwrap();
        let mut w2 = Sap::create(&p, 40, 12).unwrap();
        t.reconcile().unwrap();
        assert_eq!(
            t.skip_for_chunk(12),
            12,
            "the chunk repeats what was served live"
        );
        assert_eq!(t.skip_for_chunk(12), 0, "…exactly once");

        w2.append(3, 3, b"next\n").unwrap();
        w2.sync().unwrap();
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["next\n"]);
    }

    #[test]
    fn a_chunk_lost_to_retention_takes_its_skip_with_it() {
        let d = TempDir::new();
        let p = crate::format::sap_path(d.path(), "app");
        let mut w = Sap::create(&p, 0, 0).unwrap();
        let mut t = LiveTail::open(d.path(), "app", true);
        w.append(1, 1, b"served-live\n").unwrap();
        w.sync().unwrap();
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["served-live\n"]);
        std::fs::rename(&p, crate::format::sap_seal_path(d.path(), "app")).unwrap();
        Sap::create(&p, 40, 12).unwrap();
        t.reconcile().unwrap();

        // The chunk that segment became is gone before the reader got to
        // it. Its skip must go too, or it would cut the front off the
        // next chunk — bytes nobody has seen.
        t.forget_skip();
        assert_eq!(t.skip_for_chunk(12), 0);
    }

    #[test]
    fn a_head_trims_base_rewrite_is_not_a_roll() {
        let d = TempDir::new();
        let p = crate::format::sap_path(d.path(), "app");
        let mut w = Sap::create(&p, 900, 900).unwrap();
        let mut t = LiveTail::open(d.path(), "app", true);
        w.append(1, 1, b"kept\n").unwrap();
        w.sync().unwrap();
        let (at, got) = t.poll().unwrap();
        assert_eq!(payloads(&got), vec!["kept\n"]);
        assert_eq!(at, 900, "the segment's own base");

        // Retention rebases the store under the live segment: same file,
        // new bases. Nothing was sealed, so nothing may be re-emitted or
        // charged to a chunk.
        w.refresh_base(100, 100).unwrap();
        t.reconcile().unwrap();
        let (rebased, got) = t.poll().unwrap();
        assert!(got.is_empty());
        // The address followed the rebase, by the 800 that left the
        // store — so adding what left it back leaves the TAPE offset
        // where it was, which is the axis a cursor is on.
        assert_eq!(rebased, 100 + b"kept\n".len() as u64);
        assert_eq!(rebased + 800, at + b"kept\n".len() as u64);
        assert_eq!(t.skip_for_chunk(1000), 0);
    }

    #[test]
    fn a_store_with_no_wal_is_simply_not_live() {
        let d = TempDir::new();
        let mut t = LiveTail::open(d.path(), "app", true);
        assert!(!t.live());
        assert!(t.poll().unwrap().1.is_empty());
        assert_eq!(t.skip_for_chunk(100), 0);

        // …until one is declared under it, which needs no restart.
        let mut w = Sap::create(&crate::format::sap_path(d.path(), "app"), 0, 0).unwrap();
        w.append(1, 1, b"now live\n").unwrap();
        w.sync().unwrap();
        assert_eq!(payloads(&t.poll().unwrap().1), vec!["now live\n"]);
        assert!(t.live());
    }
}
