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
//!   `<name>.rings`  — the index: an 8-byte magic followed by fixed-size
//!                    48-byte records, one per chunk, appended in write
//!                    order. Records are therefore sorted both by
//!                    uncompressed offset and by write time, so byte-offset
//!                    reads and time-range queries are both a binary search.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub const RINGS_MAGIC: &[u8; 8] = b"RING0001";
pub const RINGS_HEADER_LEN: u64 = 8;
pub const RECORD_LEN: usize = 48;
pub const TRUNK_EXT: &str = "trunk";
pub const RINGS_EXT: &str = "rings";

pub fn trunk_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{TRUNK_EXT}"))
}

pub fn rings_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{RINGS_EXT}"))
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
        }
    }
}

pub fn read_index(path: &Path) -> io::Result<Vec<ChunkRecord>> {
    let f = File::open(path)?;
    read_index_file(&f)
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

/// Parse rings content wherever it came from (a file, a bundle member).
pub fn parse_index_bytes(buf: &[u8]) -> io::Result<Vec<ChunkRecord>> {
    if buf.len() < RINGS_HEADER_LEN as usize || &buf[..8] != RINGS_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a timberfs index (bad magic)",
        ));
    }
    let n = (buf.len() - RINGS_HEADER_LEN as usize) / RECORD_LEN;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = RINGS_HEADER_LEN as usize + i * RECORD_LEN;
        out.push(ChunkRecord::from_bytes(&buf[off..off + RECORD_LEN]));
    }
    Ok(out)
}
