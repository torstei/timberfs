//! `.sap`: a write-ahead sidecar that decouples durability from
//! compression. The appender's buffer wants to grow large before it is
//! worth compressing; a caller wanting "durable when acked" wants every
//! byte on disk immediately. Coupling the two (as the plain `.trunk`/
//! `.rings` chunking does) forces a choice — flush tiny chunks for
//! durability, or lose up to `flush_age` on a crash.
//!
//! The sap breaks that coupling: entries are appended here RAW as they
//! enter the in-memory buffer, `sap_sync()` (store.rs) fsyncs it — the
//! durability point — and chunk flushing proceeds exactly as before, on
//! its own size/age schedule. The sap and the buffer hold the same bytes
//! by construction: it is write-only in steady state and is read exactly
//! once ever, on writer-open after a crash (see store.rs's `FileStore::
//! open` for the recovery matrix).
//!
//! On disk:
//!
//!   segment header (24 bytes): magic "SAP00001" (8) + u64 LE `base` +
//!   u64 LE `uncomp_base`. `base` is the trunk's compressed size
//!   (`comp_size`) when this segment was created — the value a
//!   `.sap.seal`'s crash reconciliation compares against the CURRENT
//!   `comp_size` to tell a landed flush from one that never happened
//!   (store.rs). `uncomp_base` is the store's logical (uncompressed)
//!   position at the same moment (`buffer_start`): it locates the segment
//!   in the uncompressed stream without a rings lookup — a recovery
//!   cross-check today, and the planned live-tail reader's realignment
//!   anchor.
//!
//!   record: u32 LE len | u64 LE wf | u64 LE wl | `len` payload bytes |
//!   u32 LE crc32 — crc32 (polynomial 0xEDB88320, the standard zlib/gzip
//!   CRC-32) covers the 20-byte record header and the payload.
//!
//! Replay reads the longest valid prefix — stopping at EOF, a short read,
//! or the first CRC mismatch — and the caller truncates the file to that
//! prefix before resuming appends onto it. A torn tail is expected crash
//! debris (the same discipline as `.rings`' trailing-partial-record
//! handling in format.rs), never an error.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

pub const SAP_MAGIC: &[u8; 8] = b"SAP00001";
pub const HEADER_LEN: u64 = 24;
/// len(4) + wf(8) + wl(8), not counting the payload or the trailing crc32.
const RECORD_HEADER_LEN: usize = 20;

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

const CRC32_TABLE: [u32; 256] = build_crc32_table();

/// Standard zlib/gzip CRC-32 (polynomial 0xEDB88320), hand-rolled and
/// table-driven — no new dependency, same discipline as the FNV-1a in
/// grain.rs.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = CRC32_TABLE[idx] ^ (crc >> 8);
    }
    !crc
}

fn header_bytes(base: u64, uncomp_base: u64) -> [u8; HEADER_LEN as usize] {
    let mut h = [0u8; HEADER_LEN as usize];
    h[..8].copy_from_slice(SAP_MAGIC);
    h[8..16].copy_from_slice(&base.to_le_bytes());
    h[16..24].copy_from_slice(&uncomp_base.to_le_bytes());
    h
}

fn encode_record(wf: u64, wl: u64, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RECORD_HEADER_LEN + payload.len() + 4);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&wf.to_le_bytes());
    buf.extend_from_slice(&wl.to_le_bytes());
    buf.extend_from_slice(payload);
    let crc = crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf
}

/// One replayed entry: the original write window and payload, exactly as
/// they were appended.
pub struct SapEntry {
    pub wf: u64,
    pub wl: u64,
    pub payload: Vec<u8>,
}

/// Parse the longest valid prefix of records out of `buf` (the file's
/// content AFTER the header). Stops at EOF, a short/torn record,
/// or the first CRC mismatch. Returns the entries and how many bytes of
/// `buf` they occupy — the caller truncates the file to
/// `HEADER_LEN + that` before resuming appends.
fn parse_records(buf: &[u8]) -> (Vec<SapEntry>, usize) {
    let mut entries = Vec::new();
    let mut off = 0usize;
    loop {
        if off + 4 > buf.len() {
            break;
        }
        let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        let total = RECORD_HEADER_LEN + len + 4;
        if off + total > buf.len() {
            break; // short/torn tail: expected crash debris
        }
        let head_and_payload = &buf[off..off + RECORD_HEADER_LEN + len];
        let stored_crc = u32::from_le_bytes(
            buf[off + RECORD_HEADER_LEN + len..off + total]
                .try_into()
                .unwrap(),
        );
        if crc32(head_and_payload) != stored_crc {
            break; // first corrupt/torn record: stop here
        }
        let wf = u64::from_le_bytes(buf[off + 4..off + 12].try_into().unwrap());
        let wl = u64::from_le_bytes(buf[off + 12..off + 20].try_into().unwrap());
        let payload = buf[off + RECORD_HEADER_LEN..off + RECORD_HEADER_LEN + len].to_vec();
        entries.push(SapEntry { wf, wl, payload });
        off += total;
    }
    (entries, off)
}

/// The result of replaying a sap (or seal) file: its declared bases, the
/// entries recovered from its longest valid prefix, and the byte length
/// of that valid prefix (header included) — what the file should be
/// truncated to before it is resumed.
pub struct SapReplay {
    pub base: u64,
    pub uncomp_base: u64,
    pub entries: Vec<SapEntry>,
    pub valid_len: u64,
}

/// Replay `path`. `Ok(None)` means there is nothing usable there: the
/// file doesn't exist, or its header is unparseable (a header write is
/// effectively atomic — the same "unparseable marker: nothing safe to
/// redo" discipline as store.rs's `.trim` marker — so an invalid header
/// implies no records were ever durably appended after it; the file is
/// removed best-effort so it doesn't linger).
pub fn replay(path: &Path) -> io::Result<Option<SapReplay>> {
    let buf = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if buf.len() < HEADER_LEN as usize || &buf[..8] != SAP_MAGIC {
        eprintln!(
            "timberfs: {} has no valid sap header — nothing to recover from it; removing",
            path.display()
        );
        let _ = fs::remove_file(path);
        return Ok(None);
    }
    let base = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let uncomp_base = u64::from_le_bytes(buf[16..24].try_into().unwrap());
    let (entries, body_len) = parse_records(&buf[HEADER_LEN as usize..]);
    Ok(Some(SapReplay {
        base,
        uncomp_base,
        entries,
        valid_len: HEADER_LEN + body_len as u64,
    }))
}

/// A live write-ahead segment: a buffered, appendable file plus the
/// bases (trunk `comp_size` and logical `buffer_start`) it was created
/// at. The buffer and this
/// file hold the same bytes by construction — every write here mirrors
/// one to the in-memory buffer (store.rs's `append_windowed`).
pub struct Sap {
    writer: BufWriter<File>,
    base: u64,
    uncomp_base: u64,
}

impl Sap {
    /// Start a fresh, empty segment at `path` (truncating anything that
    /// was there — the caller has already dealt with a prior segment,
    /// e.g. sealing it).
    pub fn create(path: &Path, base: u64, uncomp_base: u64) -> io::Result<Sap> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all_at(&header_bytes(base, uncomp_base), 0)?;
        let mut file = file;
        file.seek(SeekFrom::Start(HEADER_LEN))?;
        Ok(Sap {
            writer: BufWriter::new(file),
            base,
            uncomp_base,
        })
    }

    /// Resume an existing segment whose valid prefix is `valid_len` bytes
    /// (header included) — the file is truncated to that (dropping any
    /// torn tail) and appends continue from there.
    pub fn resume(path: &Path, base: u64, uncomp_base: u64, valid_len: u64) -> io::Result<Sap> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        file.set_len(valid_len)?;
        let mut file = file;
        file.seek(SeekFrom::Start(valid_len))?;
        Ok(Sap {
            writer: BufWriter::new(file),
            base,
            uncomp_base,
        })
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn uncomp_base(&self) -> u64 {
        self.uncomp_base
    }

    /// Mirror one append into the sap.
    pub fn append(&mut self, wf: u64, wl: u64, payload: &[u8]) -> io::Result<()> {
        self.writer.write_all(&encode_record(wf, wl, payload))
    }

    /// The durability point: push buffered writes to the OS and fsync.
    /// Cheap when nothing changed since the last call (flush on an empty
    /// buffer is a no-op; sync_all still costs one syscall).
    pub fn sync(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()
    }

    /// Refresh this segment's base headers in place, for the rare case
    /// where the store's coordinates move without a flush of THIS
    /// segment — `append_frames` (verbatim chunk merge) and
    /// retention/rotation's head trims (`remove_head`/`collapse_head`)
    /// all change `comp_size` (and the head trims `buffer_start` too)
    /// directly. `write_all_at` on a file NOT
    /// opened with O_APPEND is a real pwrite at the given offset (unlike
    /// O_APPEND fds, where Linux ignores the offset and always appends),
    /// so this never disturbs the writer's own append position.
    pub fn refresh_base(&mut self, new_base: u64, new_uncomp: u64) -> io::Result<()> {
        self.base = new_base;
        self.uncomp_base = new_uncomp;
        let mut both = [0u8; 16];
        both[..8].copy_from_slice(&new_base.to_le_bytes());
        both[8..].copy_from_slice(&new_uncomp.to_le_bytes());
        self.writer.get_ref().write_all_at(&both, 8)?;
        self.writer.get_ref().sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("timberfs-sap-test-{}-{n}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        // The canonical CRC-32 check value for the ASCII string "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn header_roundtrips() {
        let h = header_bytes(0x1122_3344_5566_7788, 0x99AA_BBCC_DDEE_FF00);
        assert_eq!(&h[..8], SAP_MAGIC);
        assert_eq!(
            u64::from_le_bytes(h[8..16].try_into().unwrap()),
            0x1122_3344_5566_7788
        );
        assert_eq!(
            u64::from_le_bytes(h[16..24].try_into().unwrap()),
            0x99AA_BBCC_DDEE_FF00
        );
    }

    #[test]
    fn record_roundtrips_through_parse_records() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&encode_record(10, 20, b"hello"));
        buf.extend_from_slice(&encode_record(20, 30, b"world!!"));
        let (entries, consumed) = parse_records(&buf);
        assert_eq!(consumed, buf.len());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].wf, 10);
        assert_eq!(entries[0].wl, 20);
        assert_eq!(entries[0].payload, b"hello");
        assert_eq!(entries[1].wf, 20);
        assert_eq!(entries[1].wl, 30);
        assert_eq!(entries[1].payload, b"world!!");
    }

    #[test]
    fn torn_tail_truncates_at_every_byte_offset() {
        // A 3-record sap: whatever byte offset a crash tears it at, replay
        // must recover exactly the records fully present before that
        // offset and never a partial/corrupt one.
        let mut full = Vec::new();
        full.extend_from_slice(&encode_record(1, 1, b"aaa"));
        full.extend_from_slice(&encode_record(2, 2, b"bbbbbb"));
        full.extend_from_slice(&encode_record(3, 3, b"c"));
        let bounds = [
            0,
            RECORD_HEADER_LEN + 3 + 4,
            RECORD_HEADER_LEN + 3 + 4 + RECORD_HEADER_LEN + 6 + 4,
            full.len(),
        ];
        for cut in 0..=full.len() {
            let (entries, consumed) = parse_records(&full[..cut]);
            let expected_full_records = bounds.iter().filter(|&&b| b <= cut).count() - 1;
            assert_eq!(
                entries.len(),
                expected_full_records,
                "cut at {cut} should yield {expected_full_records} record(s)"
            );
            assert_eq!(
                consumed, bounds[expected_full_records],
                "cut at {cut} should consume exactly its whole records"
            );
        }
    }

    #[test]
    fn replay_reports_absent_file_as_none() {
        let dir = TempDir::new();
        assert!(replay(&dir.path().join("missing.sap")).unwrap().is_none());
    }

    #[test]
    fn replay_drops_a_file_with_a_bad_header() {
        let dir = TempDir::new();
        let p = dir.path().join("bad.sap");
        fs::write(&p, b"not a sap file at all").unwrap();
        assert!(replay(&p).unwrap().is_none());
        assert!(!p.exists());
    }

    #[test]
    fn create_then_replay_roundtrips() {
        let dir = TempDir::new();
        let p = dir.path().join("x.sap");
        let mut sap = Sap::create(&p, 42, 7).unwrap();
        sap.append(1, 2, b"one").unwrap();
        sap.append(3, 4, b"two").unwrap();
        sap.sync().unwrap();
        drop(sap);

        let replayed = replay(&p).unwrap().unwrap();
        assert_eq!(replayed.base, 42);
        assert_eq!(replayed.uncomp_base, 7);
        assert_eq!(replayed.entries.len(), 2);
        assert_eq!(replayed.entries[0].payload, b"one");
        assert_eq!(replayed.entries[1].payload, b"two");
        assert_eq!(replayed.valid_len, fs::metadata(&p).unwrap().len());
    }

    #[test]
    fn resume_truncates_torn_tail_and_appends_correctly() {
        let dir = TempDir::new();
        let p = dir.path().join("x.sap");
        {
            let mut sap = Sap::create(&p, 0, 0).unwrap();
            sap.append(1, 1, b"first").unwrap();
            sap.sync().unwrap();
        }
        // Simulate torn crash debris appended after a clean record.
        let mut bytes = fs::read(&p).unwrap();
        bytes.extend_from_slice(&[0xAB; 5]);
        fs::write(&p, &bytes).unwrap();

        let replayed = replay(&p).unwrap().unwrap();
        assert_eq!(replayed.entries.len(), 1);
        let mut sap =
            Sap::resume(&p, replayed.base, replayed.uncomp_base, replayed.valid_len).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().len(), replayed.valid_len);
        sap.append(2, 2, b"second").unwrap();
        sap.sync().unwrap();

        let replayed2 = replay(&p).unwrap().unwrap();
        assert_eq!(replayed2.entries.len(), 2);
        assert_eq!(replayed2.entries[0].payload, b"first");
        assert_eq!(replayed2.entries[1].payload, b"second");
    }

    #[test]
    fn refresh_base_does_not_disturb_appends() {
        let dir = TempDir::new();
        let p = dir.path().join("x.sap");
        let mut sap = Sap::create(&p, 100, 1000).unwrap();
        sap.append(1, 1, b"a").unwrap();
        sap.refresh_base(200, 2000).unwrap();
        sap.append(2, 2, b"b").unwrap();
        sap.sync().unwrap();

        let replayed = replay(&p).unwrap().unwrap();
        assert_eq!(replayed.base, 200);
        assert_eq!(replayed.uncomp_base, 2000);
        assert_eq!(replayed.entries.len(), 2);
        assert_eq!(replayed.entries[0].payload, b"a");
        assert_eq!(replayed.entries[1].payload, b"b");
    }
}
