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
    ///
    /// ⚠ That locality is a current CHOICE with a stated precondition, not
    /// a law — see ROADMAP's "Globally addressable chunks". A single source
    /// delivered in order could keep its numbering, which is what makes
    /// `(origin, seq)` a citation that survives the network. If it ever
    /// does, one invariant decides it: **never claim an origin and
    /// renumber** — that produces an address that lies. Preserving the
    /// number without claiming an origin is legal but weaker (gap evidence
    /// survives, addressing does not), and preserving it without
    /// preserving CHUNK BOUNDARIES is not preserving an address at all.
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
/// 32..40  dropped_uncomp  — uncompressed bytes that have LEFT the store
/// 40..48  dropped_comp    — the same, compressed
/// 48..64  store id        — the 16 raw bytes of the store's UUID, or
///                           all-zero where it has none
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
/// The **store id** is the first tenant of that reserved space, and it is
/// what the space was reserved for: an OPTIONAL field whose absence reads
/// as zero, needing no `incompat_flags` bit, because a reader that does not
/// know about it is not misled by ignoring it. It lives here rather than in
/// the `.bark` alone because the backing PAIR is the store — lose the
/// manifest and the data should still say what it is — and rather than in
/// the trunk because the trunk has no header and its HEAD is the mutable
/// end: retention's head-drop collapses from offset 0, so a leading
/// identity frame is exactly what it would eat first.
///
/// ⚠ The id written here is always the store's OWN, read from its manifest.
/// It must never be copied from a sender or a source: replication and
/// `export` mint a fresh identity at the destination and record lineage in
/// `derived_from`, so carrying a source's id across would give two stores
/// one identity and silently rebind every cursor keyed on it.
///
/// `next_seq` exists for one case: retention can drop EVERY chunk, and a
/// store whose record set is empty would otherwise restart numbering at 0
/// and hand a fresh chunk a number some cursor already considers consumed.
/// It is not a base for index arithmetic — records carry their own numbers —
/// so it only ever forbids reuse, and only the paths that rewrite the whole
/// file (head-drop) keep it current. On the append path the last record is
/// the better source, so nothing extra is written there.
pub fn rings_header(
    next_seq: u64,
    dropped: Dropped,
    id: Option<[u8; 16]>,
) -> [u8; RINGS_HEADER_LEN as usize] {
    let mut h = [0u8; RINGS_HEADER_LEN as usize];
    h[0..8].copy_from_slice(RINGS_MAGIC);
    h[8..16].copy_from_slice(&RINGS_HEADER_LEN.to_le_bytes());
    h[16..24].copy_from_slice(&0u64.to_le_bytes());
    h[24..32].copy_from_slice(&next_seq.to_le_bytes());
    h[32..40].copy_from_slice(&dropped.uncomp_bytes.to_le_bytes());
    h[40..48].copy_from_slice(&dropped.comp_bytes.to_le_bytes());
    if let Some(id) = id {
        h[STORE_ID_OFF..STORE_ID_OFF + 16].copy_from_slice(&id);
    }
    h
}

/// Where the store id sits in a v2 header.
pub const STORE_ID_OFF: usize = 48;

/// The store id a `.rings` header carries, or None where it carries none:
/// a v1 header, a header too short to reach the field, or the all-zero
/// that means "not set". An id of all zeros is not representable, which
/// is what makes zero a safe absent.
pub fn header_store_id(buf: &[u8]) -> Option<[u8; 16]> {
    if buf.len() < STORE_ID_OFF + 16 || &buf[..8] != RINGS_MAGIC {
        return None;
    }
    // A header may declare itself shorter than the field it would contain.
    let declared = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    if declared < (STORE_ID_OFF + 16) as u64 {
        return None;
    }
    let id: [u8; 16] = buf[STORE_ID_OFF..STORE_ID_OFF + 16].try_into().ok()?;
    (id != [0u8; 16]).then_some(id)
}

/// A hyphenated UUID as its 16 raw bytes. None for anything that is not
/// one — an id is minted by us, so a manifest holding something else is a
/// fact to report, never something to reshape into 16 bytes.
pub fn uuid_bytes(s: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = s.bytes().filter(|b| *b != b'-').collect();
    if hex.len() != 32 || !hex.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(text, 16).ok()?;
    }
    Some(out)
}

/// The hyphenated form of 16 raw bytes — the spelling `.bark` holds and
/// `list` prints, so the two views of one identity are comparable as text.
pub fn uuid_text(b: &[u8; 16]) -> String {
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// What has LEFT this store over its whole life — an optional header field,
/// so it uses the reserved space rather than a version bump.
///
/// Not derivable after the fact, which is why it is recorded: a head-drop
/// REBASES the survivors' offsets, so the bytes that went leave no trace in
/// the index.
///
/// Bytes only. The chunk COUNT is not recorded, because the numbering
/// already carries it exactly — dense from 0, and only a prefix ever drops,
/// so the oldest surviving number IS the lifetime count. A counter beside it
/// could only ever be a subset of that (it misses whatever dropped before
/// the counter existed), so it never changes an answer. Should numbering
/// ever stop starting at 0 (see ROADMAP, "Globally addressable chunks")
/// what that needs is a numbering BASE — `first_seq - base` — and not a
/// count; the reserved space is there for it.
///
/// **Lengths, never offsets.** `collapse_head` cuts on a filesystem-block
/// boundary and leaves up to ~2 blocks of the dropped range as an inert
/// skippable frame, rebasing survivors by the ALIGNED amount — so
/// `comp_start` carries that sliver forward and summing offsets would
/// double-count it on the next drop. Summing `comp_len`/`uncomp_len` over
/// the dropped chunks is immune, identical in both the collapse and rewrite
/// paths, and means "what left the store" rather than "what the filesystem
/// reclaimed" — the two genuinely differ by the sliver.
///
/// ⚠ Zero reads as ABSENT, per the reserved-space contract, which for a
/// byte count collides with "nothing dropped". The numbering resolves it:
/// a store whose oldest chunk is number 0 has dropped nothing, so zero
/// bytes alongside a non-zero oldest number means the field was never
/// maintained — a store written before this existed — and not that nothing
/// went. A real chunk carries a frame header and cannot compress to
/// nothing, so the two are never confusable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    pub uncomp_bytes: u64,
    pub comp_bytes: u64,
}

/// The header's drop counters, or all-zero when this file is too old to
/// carry them. Gated on the DECLARED header length, not on the compiled-in
/// constant: that is what the length field is for.
pub fn header_dropped(buf: &[u8]) -> Dropped {
    const NEEDED: usize = 48;
    if buf.len() < NEEDED || &buf[..8] != RINGS_MAGIC {
        return Dropped::default();
    }
    let declared = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    if declared < NEEDED as u64 {
        return Dropped::default();
    }
    let at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
    Dropped {
        uncomp_bytes: at(32),
        comp_bytes: at(40),
    }
}

pub fn read_header_dropped(f: &File) -> io::Result<Dropped> {
    let mut h = [0u8; RINGS_HEADER_LEN as usize];
    match f.read_exact_at(&mut h, 0) {
        Ok(()) => Ok(header_dropped(&h)),
        // Shorter than a v2 header: a v1 file, or an empty one — nothing
        // recorded, which is exactly what the default says.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(Dropped::default()),
        Err(e) => Err(e),
    }
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
        let h = rings_header(42, Dropped::default(), None);
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
    fn the_header_carries_the_store_id_and_zero_means_none() {
        let id = uuid_bytes("0d01f72b-cc35-4da6-aa3a-77a6ced1b996").unwrap();
        let h = rings_header(7, Dropped::default(), Some(id));
        assert_eq!(header_store_id(&h), Some(id));
        assert_eq!(uuid_text(&id), "0d01f72b-cc35-4da6-aa3a-77a6ced1b996");
        // Absent reads as zero, which is safe precisely because an
        // all-zero UUID is not something we ever mint.
        assert_eq!(
            header_store_id(&rings_header(7, Dropped::default(), None)),
            None
        );
        // The id sits past every field an older reader knows, so adding it
        // changes nothing else in the header.
        let plain = rings_header(7, Dropped::default(), None);
        assert_eq!(h[..STORE_ID_OFF], plain[..STORE_ID_OFF]);
    }

    #[test]
    fn a_header_that_cannot_hold_an_id_reports_none_rather_than_reading_past_itself() {
        let h = rings_header(7, Dropped::default(), Some([9u8; 16]));
        // Truncated to the v1-compatible minimum: the field is not there.
        assert_eq!(header_store_id(&h[..RINGS_HEADER_MIN as usize]), None);
        // A header DECLARING itself shorter than the field must not be
        // read past its own declaration, even when the bytes happen to be
        // present — that declaration is what lets the header grow.
        let mut short = h;
        short[8..16].copy_from_slice(&32u64.to_le_bytes());
        assert_eq!(header_store_id(&short), None);
        // Not a rings file at all.
        assert_eq!(header_store_id(&[0u8; 64]), None);
    }

    #[test]
    fn only_a_real_uuid_becomes_sixteen_bytes() {
        assert!(uuid_bytes("0d01f72b-cc35-4da6-aa3a-77a6ced1b996").is_some());
        // Hyphens are cosmetic, so the unhyphenated spelling is the same id.
        assert_eq!(
            uuid_bytes("0d01f72bcc354da6aa3a77a6ced1b996"),
            uuid_bytes("0d01f72b-cc35-4da6-aa3a-77a6ced1b996")
        );
        // An id is minted by us: anything else in a manifest is a fact to
        // report, never something to reshape into 16 bytes.
        assert!(uuid_bytes("").is_none());
        assert!(uuid_bytes("not-a-uuid").is_none());
        assert!(
            uuid_bytes("0d01f72b-cc35-4da6-aa3a-77a6ced1b99").is_none(),
            "short"
        );
        assert!(
            uuid_bytes("zd01f72b-cc35-4da6-aa3a-77a6ced1b996").is_none(),
            "non-hex"
        );
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
