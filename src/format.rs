//! On-disk format of the backing store.
//!
//! Each logical file `<name>` in the mount is backed by two files in the
//! backing directory:
//!
//!   `<name>.trunk`  — the data: a plain concatenation of zstd frames, one
//!                    frame per chunk. Deliberately header-free so that
//!                    `zstd -dc <name>.trunk` recovers the full uncompressed
//!                    content with stock tools, even without timberfs.
//!
//!   `<name>.rings`  — the index: a 16-byte header (magic + the chunk-number
//!                    high-water mark) followed by fixed-size 56-byte
//!                    records, one per chunk, appended in write order.
//!                    Records are therefore sorted by uncompressed offset,
//!                    and by chunk number, so a byte-offset read is a binary
//!                    search. They are only MOSTLY sorted by write time —
//!                    `now_ms()` is the wall clock, so an NTP step or a
//!                    `date -s` can move a window backwards, and an intake
//!                    stamps the sender's event time on purpose. Time-range
//!                    reads cope with that by widening; anything needing a
//!                    single monotonic position uses the chunk number.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub const RINGS_MAGIC: &[u8; 8] = b"RING0002";
/// The header length THIS version writes. A reader must use the length the
/// file itself declares (`header_len` below), not this constant, or a later
/// version's longer header would shift every record offset underneath it.
pub const RINGS_HEADER_LEN: u64 = 64;
/// Below this a v2 header cannot hold the fields every reader requires.
pub const RINGS_HEADER_MIN: u64 = 32;
pub const RECORD_LEN: usize = 56;
/// The pre-chunk-number layout: an 8-byte header and 48-byte records. Read
/// for compatibility and by the migration; never written. Support for it is
/// meant to be dropped after a grace period, at which point the reader that
/// stays behind is a standalone converter.
pub const RINGS_MAGIC_V1: &[u8; 8] = b"RING0001";
pub const RINGS_HEADER_LEN_V1: u64 = 8;
pub const RECORD_LEN_V1: usize = 48;

/// Which layout a `.rings` file is in. A reader handles both; a writer
/// migrates v1 before it appends, since the two strides cannot be mixed in
/// one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingsVersion {
    V1,
    V2,
}
pub const TRUNK_EXT: &str = "trunk";
pub const RINGS_EXT: &str = "rings";
pub const GRAIN_EXT: &str = "grain";
pub const BARK_EXT: &str = "bark";
/// The collapse-head seqlock counter (store.rs): even means idle, odd
/// means a collapse is in flight. Missing reads as 0 (never collapsed).
pub const SEQ_EXT: &str = "seq";
/// The collapse-head crash marker (store.rs): present only mid-collapse,
/// so `FileStore::open` can tell whether a crashed collapse's
/// `fallocate(COLLAPSE_RANGE)` landed before reconciling the rings.
pub const TRIM_EXT: &str = "trim";
/// The write-ahead sidecar (sap.rs): raw copies of appended entries,
/// fsynced ahead of the chunk that eventually compresses them — the
/// durability point for a `"wal": true` store. `.seal` is the mid-flush
/// handoff name (store.rs's seal-and-swap).
pub const SAP_EXT: &str = "sap";
pub const SAP_SEAL_EXT: &str = "sap.seal";

pub fn trunk_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{TRUNK_EXT}"))
}

pub fn rings_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{RINGS_EXT}"))
}

pub fn grain_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{GRAIN_EXT}"))
}

pub fn bark_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{BARK_EXT}"))
}

pub fn seq_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{SEQ_EXT}"))
}

pub fn trim_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{TRIM_EXT}"))
}

pub fn sap_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{SAP_EXT}"))
}

pub fn sap_seal_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{SAP_SEAL_EXT}"))
}

/// One chunk: a contiguous run of appended bytes, compressed as a single
/// zstd frame, together with the wall-clock window in which those bytes
/// were written. All fields little-endian u64 on disk.
#[derive(Debug, Clone, Copy)]
pub struct ChunkRecord {
    /// Offset of this chunk in the uncompressed (logical) file.
    pub uncomp_start: u64,
    /// Uncompressed length of this chunk.
    pub uncomp_len: u64,
    /// Offset of the zstd frame in the .trunk file.
    pub comp_start: u64,
    /// Length of the zstd frame.
    pub comp_len: u64,
    /// Wall clock (unix ms) of the first write buffered into this chunk.
    pub first_write_ms: u64,
    /// Wall clock (unix ms) of the last write buffered into this chunk.
    pub last_write_ms: u64,
    /// This chunk's number in ITS OWN store: assigned at append, dense,
    /// never reused, and monotone by construction rather than by trusting a
    /// clock — which is what makes it the only axis a cursor can safely
    /// hold. Preserved verbatim by a head-drop (the numbering does not slide
    /// down when the oldest chunks go), and therefore NOT the record's index.
    /// Local to one store: a chunk shipped into another store is renumbered
    /// there, because the number says where it sits, not what it is.
    pub seq: u64,
}

impl ChunkRecord {
    pub fn uncomp_end(&self) -> u64 {
        self.uncomp_start + self.uncomp_len
    }

    pub fn comp_end(&self) -> u64 {
        self.comp_start + self.comp_len
    }

    pub fn to_bytes(self) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        b[0..8].copy_from_slice(&self.uncomp_start.to_le_bytes());
        b[8..16].copy_from_slice(&self.uncomp_len.to_le_bytes());
        b[16..24].copy_from_slice(&self.comp_start.to_le_bytes());
        b[24..32].copy_from_slice(&self.comp_len.to_le_bytes());
        b[32..40].copy_from_slice(&self.first_write_ms.to_le_bytes());
        b[40..48].copy_from_slice(&self.last_write_ms.to_le_bytes());
        b[48..56].copy_from_slice(&self.seq.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> ChunkRecord {
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        ChunkRecord {
            uncomp_start: u64_at(0),
            uncomp_len: u64_at(8),
            comp_start: u64_at(16),
            comp_len: u64_at(24),
            first_write_ms: u64_at(32),
            last_write_ms: u64_at(40),
            seq: u64_at(48),
        }
    }

    /// A v1 record, which carries no number. `seq` is supplied by the
    /// caller: the oldest surviving record is 0, which is a DEFINITION of
    /// where this store's numbering begins, not an attempt to recover how
    /// many chunks it dropped before anyone was counting.
    pub fn from_bytes_v1(b: &[u8], seq: u64) -> ChunkRecord {
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        ChunkRecord {
            uncomp_start: u64_at(0),
            uncomp_len: u64_at(8),
            comp_start: u64_at(16),
            comp_len: u64_at(24),
            first_write_ms: u64_at(32),
            last_write_ms: u64_at(40),
            seq,
        }
    }
}

/// The v2 header: 64 bytes, every field a little-endian u64 after the
/// magic, and deliberately larger than it needs to be.
///
/// ```text
///  0..8   magic "RING0002"
///  8..16  header_len      — where record 0 starts
/// 16..24  incompat_flags  — refuse on a bit you do not know
/// 24..32  next_seq        — the chunk-number high-water mark
/// 32..64  reserved, zero
/// ```
///
/// `header_len` is what makes the reserved space usable: without it a
/// reader computes record offsets from a compiled-in constant, so a later
/// version's longer header would shift every record underneath every binary
/// already deployed — and reserving bytes nobody can safely grow into buys
/// nothing. `incompat_flags` is the other half: reserved space is only safe
/// for OPTIONAL fields (0 reads as absent), and a field that changes how
/// records must be interpreted sets a bit instead, so an older reader stops
/// rather than guessing. The cost of both is 64 bytes per store, once.
///
/// `next_seq` exists for one case: retention can drop EVERY chunk, and a
/// store whose record set is empty would otherwise restart numbering at 0
/// and hand a fresh chunk a number some cursor already considers consumed.
/// It is not a base for index arithmetic — records carry their own numbers —
/// so it only ever forbids reuse, and only the paths that rewrite the whole
/// file (head-drop) keep it current. On the append path the last record is
/// the better source, so nothing extra is written there.
pub fn rings_header(next_seq: u64) -> [u8; RINGS_HEADER_LEN as usize] {
    let mut h = [0u8; RINGS_HEADER_LEN as usize];
    h[0..8].copy_from_slice(RINGS_MAGIC);
    h[8..16].copy_from_slice(&RINGS_HEADER_LEN.to_le_bytes());
    h[16..24].copy_from_slice(&0u64.to_le_bytes());
    h[24..32].copy_from_slice(&next_seq.to_le_bytes());
    h
}

/// The high-water mark a v2 header carries. A v1 header has none, and a
/// truncated one reads as 0 — both mean "trust the records".
pub fn header_next_seq(buf: &[u8]) -> u64 {
    if buf.len() < RINGS_HEADER_MIN as usize || &buf[..8] != RINGS_MAGIC {
        return 0;
    }
    u64::from_le_bytes(buf[24..32].try_into().unwrap())
}

pub fn read_index(path: &Path) -> io::Result<Vec<ChunkRecord>> {
    let f = File::open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("opening index {}: {e}", path.display())))?;
    read_index_file(&f)
        .map_err(|e| io::Error::new(e.kind(), format!("reading index {}: {e}", path.display())))
}

/// Parse a .rings file. A trailing partial record (crash mid-append) is
/// silently ignored — the corresponding data bytes in the .trunk are simply
/// overwritten by the next chunk.
pub fn read_index_file(f: &File) -> io::Result<Vec<ChunkRecord>> {
    let len = f.metadata()?.len() as usize;
    let mut buf = vec![0u8; len];
    f.read_exact_at(&mut buf, 0)?;
    parse_index_bytes(&buf)
}

/// The high-water mark from a rings file's header, without parsing records.
pub fn read_header_next_seq(f: &File) -> io::Result<u64> {
    let mut h = [0u8; RINGS_HEADER_LEN as usize];
    match f.read_exact_at(&mut h, 0) {
        Ok(()) => Ok(header_next_seq(&h)),
        // Shorter than a v2 header: a v1 file, or an empty one. Either way
        // the records are the only source, and the caller falls back to them.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
        Err(e) => Err(e),
    }
}

/// Parse rings content wherever it came from (a file, a bundle member).
pub fn parse_index_bytes(buf: &[u8]) -> io::Result<Vec<ChunkRecord>> {
    parse_index_versioned(buf).map(|(recs, _)| recs)
}

/// As `parse_index_bytes`, plus which layout it found — what a WRITER needs,
/// since it must migrate a v1 file before appending rather than mixing two
/// record strides in one file.
pub fn parse_index_versioned(buf: &[u8]) -> io::Result<(Vec<ChunkRecord>, RingsVersion)> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "not a timberfs index (bad magic)",
        )
    };
    if buf.len() < RINGS_HEADER_LEN_V1 as usize {
        return Err(bad());
    }
    let (version, header, rec_len) = match &buf[..8] {
        // The FILE's header length, not ours: a longer header from a later
        // version must shift record offsets for this reader too, which is
        // the whole point of declaring it.
        m if m == RINGS_MAGIC => {
            if buf.len() < RINGS_HEADER_MIN as usize {
                return Err(bad());
            }
            let declared = u64::from_le_bytes(buf[8..16].try_into().unwrap());
            if declared < RINGS_HEADER_MIN || declared > buf.len() as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "timberfs index declares a {declared}-byte header, which is impossible"
                    ),
                ));
            }
            // A bit set here changes how the records must be read, so an
            // older reader has to stop rather than answer from a layout it
            // does not know. Optional additions live in the reserved bytes
            // and set nothing, precisely so this stays rare.
            let incompat = u64::from_le_bytes(buf[16..24].try_into().unwrap());
            if incompat != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "timberfs index requires unsupported features (incompat 0x{incompat:016x}) \
                         — it was written by a newer timberfs"
                    ),
                ));
            }
            (RingsVersion::V2, declared as usize, RECORD_LEN)
        }
        m if m == RINGS_MAGIC_V1 => (
            RingsVersion::V1,
            RINGS_HEADER_LEN_V1 as usize,
            RECORD_LEN_V1,
        ),
        _ => return Err(bad()),
    };
    let n = (buf.len() - header) / rec_len;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = header + i * rec_len;
        let b = &buf[off..off + rec_len];
        out.push(match version {
            RingsVersion::V2 => ChunkRecord::from_bytes(b),
            RingsVersion::V1 => ChunkRecord::from_bytes_v1(b, i as u64),
        });
    }
    Ok((out, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(seq: u64) -> ChunkRecord {
        ChunkRecord {
            uncomp_start: seq * 10,
            uncomp_len: 10,
            comp_start: seq * 4,
            comp_len: 4,
            first_write_ms: 100 + seq,
            last_write_ms: 100 + seq,
            seq,
        }
    }

    /// Build a rings image with an arbitrary header length and flags, i.e.
    /// what a LATER version of timberfs would write.
    fn image(header_len: u64, incompat: u64, n: u64) -> Vec<u8> {
        let mut buf = vec![0u8; header_len as usize];
        buf[0..8].copy_from_slice(RINGS_MAGIC);
        buf[8..16].copy_from_slice(&header_len.to_le_bytes());
        buf[16..24].copy_from_slice(&incompat.to_le_bytes());
        buf[24..32].copy_from_slice(&n.to_le_bytes());
        for i in 0..n {
            buf.extend_from_slice(&rec(i).to_bytes());
        }
        buf
    }

    #[test]
    fn a_longer_header_from_a_later_version_still_parses() {
        // The reserved bytes are only worth having if a reader finds record
        // 0 where the FILE says it is rather than where this build's
        // constant says — otherwise a later, longer header shifts every
        // record underneath every binary already deployed.
        let buf = image(128, 0, 3);
        let (recs, ver) = parse_index_versioned(&buf).unwrap();
        assert_eq!(ver, RingsVersion::V2);
        assert_eq!(recs.len(), 3);
        assert_eq!(recs.iter().map(|c| c.seq).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(header_next_seq(&buf), 3);
    }

    #[test]
    fn an_unknown_incompat_bit_is_refused_not_guessed_at() {
        let buf = image(RINGS_HEADER_LEN, 0b10, 2);
        let e = parse_index_versioned(&buf).unwrap_err();
        assert!(e.to_string().contains("unsupported features"), "{e}");
        // Whereas no flags is the ordinary case.
        assert!(parse_index_versioned(&image(RINGS_HEADER_LEN, 0, 2)).is_ok());
    }

    #[test]
    fn an_impossible_header_length_is_refused() {
        // Too small to hold the fields every reader requires...
        let mut buf = image(RINGS_HEADER_LEN, 0, 1);
        buf[8..16].copy_from_slice(&8u64.to_le_bytes());
        assert!(parse_index_versioned(&buf).is_err());
        // ...and longer than the file itself.
        let mut buf = image(RINGS_HEADER_LEN, 0, 1);
        buf[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse_index_versioned(&buf).is_err());
    }

    #[test]
    fn the_header_this_version_writes_round_trips() {
        let h = rings_header(42);
        assert_eq!(&h[..8], RINGS_MAGIC);
        assert_eq!(
            u64::from_le_bytes(h[8..16].try_into().unwrap()),
            RINGS_HEADER_LEN
        );
        assert_eq!(u64::from_le_bytes(h[16..24].try_into().unwrap()), 0);
        assert_eq!(header_next_seq(&h), 42);
        // The reserved tail is zero, so a later version can tell "absent"
        // from "set" without a flag for every optional addition.
        assert!(h[32..].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_record_round_trips_including_its_number() {
        let r = rec(7);
        let back = ChunkRecord::from_bytes(&r.to_bytes());
        assert_eq!(
            (back.seq, back.uncomp_start, back.last_write_ms),
            (7, 70, 107)
        );
        // A v1 record is the same bytes minus the number, which is what
        // makes the migration a per-record append rather than a rewrite.
        let v1 = &r.to_bytes()[..RECORD_LEN_V1];
        let back = ChunkRecord::from_bytes_v1(v1, 99);
        assert_eq!((back.seq, back.uncomp_start), (99, 70));
    }
}
