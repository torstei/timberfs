//! The backing store: per-file append buffers, chunk flushing (compress +
//! index), and random-access reads through chunk decompression.
//!
//! Write path: appended bytes accumulate in an in-memory buffer. The buffer
//! becomes a chunk (one zstd frame + one index record) when it reaches
//! `chunk_size`, when the file is fsync'ed/closed, or when the oldest
//! buffered byte exceeds `flush_age_ms` (enforced by a background thread).
//! The flush age bounds the time granularity of the index for slow writers.
//!
//! Crash consistency: a chunk is written data-first, index-record-second.
//! On open, index records pointing past the end of the data file are
//! dropped, and orphan data bytes past the last indexed chunk are
//! overwritten by the next flush. fsync() through the mount flushes the
//! current buffer as a chunk and syncs both backing files, so fsync means
//! durable — buffered-but-unsynced data can be lost on a crash, bounded by
//! the flush age.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::format::{self, ChunkRecord, RECORD_LEN, RINGS_HEADER_LEN};

fn invalid_input(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

/// zstd skippable-frame magic range is 0x184D2A50..=0x184D2A5F (the low
/// nibble is a frame-type tag decoders ignore); any value in range works.
const ZSTD_SKIPPABLE_MAGIC: u32 = 0x184D2A50;

/// Read a store's collapse-head seqlock counter: even means idle, odd
/// means a collapse is in flight (readers must not trust what they read
/// while it's odd, or if it changed underneath them). Missing reads as 0
/// — a store never collapsed has never bumped it.
pub fn read_seq(dir: &Path, name: &str) -> u64 {
    match fs::read(format::seq_path(dir, name)) {
        Ok(b) if b.len() >= 8 => u64::from_le_bytes(b[..8].try_into().unwrap()),
        _ => 0,
    }
}

fn write_seq(dir: &Path, name: &str, v: u64) -> io::Result<()> {
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(format::seq_path(dir, name))?;
    f.write_all_at(&v.to_le_bytes(), 0)?;
    f.sync_all()?;
    Ok(())
}

fn fstatvfs_bsize(f: &File) -> io::Result<u64> {
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::fstatvfs(f.as_raw_fd(), &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st.f_bsize as u64)
    }
}

/// The block-aligned cut point for `FALLOC_FL_COLLAPSE_RANGE`, and the
/// "sliver" of bytes left before it — the tail of the last dropped frame,
/// which collapse can't remove (it isn't block-aligned) and which gets
/// overwritten with a zstd skippable frame instead. `None` when there
/// isn't even one whole block to cut, so the caller must fall back to
/// `remove_head`.
fn collapse_alignment(comp_cut: u64, bsize: u64) -> Option<(u64, u64)> {
    let aligned = (comp_cut / bsize) * bsize;
    if aligned == 0 {
        return None;
    }
    let sliver = comp_cut - aligned;
    if sliver == 0 || sliver >= 8 {
        return Some((aligned, sliver));
    }
    // 0 < sliver < 8: no room for the 8-byte skippable-frame header.
    // Collapse one block fewer so the sliver grows past it.
    let aligned = aligned - bsize;
    if aligned == 0 {
        return None;
    }
    Some((aligned, comp_cut - aligned))
}

/// Overwrite the leading `sliver` bytes of a post-collapse trunk with a
/// zstd skippable frame, so `zstd -dc` (and our own chunk_data) can keep
/// decoding straight through the leftover tail of the dropped frame and
/// into the real ones that follow. `sliver` must be 0 (nothing to do) or
/// >= 8 (room for the header) — see `collapse_alignment`.
fn stamp_skippable_frame(trunk: &File, sliver: u64) -> io::Result<()> {
    if sliver == 0 {
        return Ok(());
    }
    debug_assert!(sliver >= 8);
    let mut hdr = [0u8; 8];
    hdr[0..4].copy_from_slice(&ZSTD_SKIPPABLE_MAGIC.to_le_bytes());
    hdr[4..8].copy_from_slice(&((sliver - 8) as u32).to_le_bytes());
    trunk.write_all_at(&hdr, 0)?;
    Ok(())
}

fn write_trim_marker(path: &Path, pre_comp_size: u64, aligned: u64, sliver: u64) -> io::Result<()> {
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all_at(
        format!("{pre_comp_size} {aligned} {sliver}\n").as_bytes(),
        0,
    )?;
    f.sync_all()?;
    Ok(())
}

/// Parse a `.trim` marker's `"pre_comp_size aligned sliver"` text.
fn parse_trim_marker(text: &str) -> Option<(u64, u64, u64)> {
    let mut it = text.split_whitespace();
    let pre_comp_size = it.next()?.parse().ok()?;
    let aligned = it.next()?.parse().ok()?;
    let sliver = it.next()?.parse().ok()?;
    Some((pre_comp_size, aligned, sliver))
}

/// Reconcile a lingering `<name>.trim` marker before a store is opened —
/// a collapse that started but never finished (a crash between the
/// `fallocate` and the final rename, or a standalone reader observing a
/// writer mid-collapse). Compare the trunk's actual size against the
/// marker's recorded before/after sizes to tell which side of the
/// `fallocate` we're on:
///
///   - still `pre_comp_size`: the collapse never landed — roll back
///     (drop the staged rings and the marker; the committed rings are
///     untouched and already correct).
///   - `pre_comp_size - aligned`: the collapse landed — roll forward
///     (re-stamp the skippable frame, idempotent, then promote the
///     staged rings over the committed ones and drop the sidecar grain,
///     same as a normal collapse's tail).
///
/// Best-effort by design: a read-only caller without write access to the
/// directory (a non-root `query`/`info`) just leaves the marker for the
/// next writer to reconcile, rather than erroring out of a read.
pub fn reconcile_trim(dir: &Path, name: &str) -> io::Result<()> {
    let trim_p = format::trim_path(dir, name);
    let text = match fs::read_to_string(&trim_p) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let rings_tmp = dir.join(format!("{name}.{}.tmp", format::RINGS_EXT));
    let Some((pre_comp_size, aligned, sliver)) = parse_trim_marker(&text) else {
        // Unparseable marker: nothing safe to redo. Drop it and any
        // staged rings; the last committed rings are untouched.
        let _ = fs::remove_file(&rings_tmp);
        let _ = fs::remove_file(&trim_p);
        return Ok(());
    };
    let trunk_len = fs::metadata(format::trunk_path(dir, name))?.len();
    if trunk_len == pre_comp_size {
        let _ = fs::remove_file(&rings_tmp);
        let _ = fs::remove_file(&trim_p);
    } else if trunk_len == pre_comp_size.saturating_sub(aligned) {
        if sliver >= 8 {
            let trunk = OpenOptions::new()
                .write(true)
                .open(format::trunk_path(dir, name))?;
            stamp_skippable_frame(&trunk, sliver)?;
        }
        fs::rename(&rings_tmp, format::rings_path(dir, name))?;
        let _ = fs::remove_file(format::grain_path(dir, name));
        let _ = fs::remove_file(&trim_p);
    } else {
        eprintln!(
            "timberfs: {name}: .trim marker doesn't match the trunk size \
             ({trunk_len} bytes; expected {pre_comp_size} or {}) — leaving it \
             for manual recovery",
            pre_comp_size.saturating_sub(aligned)
        );
    }
    Ok(())
}

fn copy_range(from: &File, from_off: u64, len: u64, to: &File, to_off: u64) -> io::Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    let mut copied = 0u64;
    while copied < len {
        let n = ((len - copied) as usize).min(buf.len());
        from.read_exact_at(&mut buf[..n], from_off + copied)?;
        to.write_all_at(&buf[..n], to_off + copied)?;
        copied += n as u64;
    }
    Ok(())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Exit code a long-running daemon uses to say "my binary was replaced on
/// disk — re-exec me on the new one." The systemd units pair it with
/// SuccessExitStatus + RestartForceExitStatus so the supervisor restarts
/// on it cleanly, regardless of the unit's normal Restart= policy. Chosen
/// outside the sysexits.h range (64–78) so it can't be confused with a
/// real failure.
pub const EXIT_BINARY_UPGRADED: i32 = 85;

/// Watches the running executable so a supervised daemon can notice its
/// own package being upgraded (dpkg replaces /usr/bin/timberfs with a new
/// inode) and exit for a clean re-exec. Only acted on when the operator
/// opted in (the units pass --exit-on-upgrade); an interactive run keeps
/// going on the old binary until the user restarts it.
pub struct BinaryWatch {
    path: PathBuf,
    ino: u64,
}

impl BinaryWatch {
    /// Capture the running executable's install path and inode. None if
    /// /proc/self/exe can't be resolved (non-Linux, unusual sandbox) —
    /// then upgrade-detection is simply disabled.
    ///
    /// `metadata` resolves /proc/self/exe to the running inode even after the
    /// file is unlinked, so `ino` is always the binary we are actually
    /// executing. `read_link`, though, appends " (deleted)" to the path text
    /// when the old inode has already been unlinked — which happens if a
    /// package swap lands during our own startup, before we get here. Left
    /// intact that bogus path never stats, so `changed()` would return false
    /// forever and we'd be blind to this upgrade and every one after it.
    /// Strip the suffix so we watch the real install path.
    pub fn current() -> Option<BinaryWatch> {
        let ino = fs::metadata("/proc/self/exe").ok()?.ino();
        let raw = fs::read_link("/proc/self/exe").ok()?;
        Some(BinaryWatch {
            path: strip_deleted(raw),
            ino,
        })
    }

    /// True once a DIFFERENT binary is in place at the original path —
    /// i.e. the package was upgraded under us and the new file is ready.
    /// If the path is momentarily absent or unreadable (an upgrade in
    /// progress, mid-rename), we return false and keep running: never
    /// exit into a gap where there is no binary to re-exec into — wait
    /// until the replacement actually lands.
    pub fn changed(&self) -> bool {
        match fs::metadata(&self.path) {
            Ok(m) => m.ino() != self.ino,
            Err(_) => false,
        }
    }
}

/// A `/proc/self/exe` link target that the kernel has marked with the
/// trailing " (deleted)" (the original inode was unlinked) points at no
/// real file. Strip that suffix to recover the install path we should watch;
/// leave any other path untouched.
fn strip_deleted(raw: PathBuf) -> PathBuf {
    const DELETED: &[u8] = b" (deleted)";
    match raw.as_os_str().as_bytes().strip_suffix(DELETED) {
        Some(real) => PathBuf::from(OsStr::from_bytes(real)),
        None => raw,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Uncompressed buffer size that triggers a chunk flush.
    pub chunk_size: usize,
    /// zstd compression level.
    pub level: i32,
    /// Max age of buffered data before the background flusher forces a
    /// chunk. This bounds the write-time granularity of the index.
    pub flush_age_ms: u64,
}

struct StageBaseline {
    chunks: usize,
    comp_size: u64,
    buffer_start: u64,
}

pub struct FileStore {
    trunk: File,
    rings: File,
    /// Atomic-sink staging: baseline to commit from or roll back to.
    /// While staged, flushed chunks write trunk frames but hold their
    /// ring records in memory — readers see the store unchanged until
    /// commit_stage, and abort_stage truncates the trunk back.
    staged: Option<StageBaseline>,
    pub chunks: Vec<ChunkRecord>,
    /// Total bytes of indexed (compressed) data in the .trunk.
    pub comp_size: u64,
    /// Appended bytes not yet flushed into a chunk.
    buffer: Vec<u8>,
    /// Uncompressed offset of buffer[0] == total indexed uncompressed bytes.
    buffer_start: u64,
    buffer_first_ms: Option<u64>,
    buffer_last_ms: u64,
    /// Single-entry decompression cache: (chunk index, uncompressed data).
    /// Enough to make sequential scans (cat/grep) decompress each chunk once.
    cache: Option<(usize, Vec<u8>)>,
}

impl FileStore {
    /// Open (or create) the backing pair for a logical file and reconcile
    /// index and data after a possible crash.
    pub fn open(dir: &Path, name: &str) -> io::Result<FileStore> {
        // A lingering .trim marker means a collapse started but never
        // finished (crash between the fallocate and the final rename);
        // reconcile it before anything below reads the trunk/rings, so
        // neither the truncation check nor a caller sees a half-landed cut.
        reconcile_trim(dir, name)?;
        let trunk_p = format::trunk_path(dir, name);
        let trunk = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&trunk_p)
            .map_err(|e| io::Error::new(e.kind(), format!("opening {}: {e}", trunk_p.display())))?;
        let rings_p = format::rings_path(dir, name);
        let rings = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&rings_p)
            .map_err(|e| io::Error::new(e.kind(), format!("opening {}: {e}", rings_p.display())))?;

        let mut chunks = Vec::new();
        if rings.metadata()?.len() == 0 {
            rings.write_all_at(format::RINGS_MAGIC, 0)?;
        } else {
            chunks = format::read_index_file(&rings)?;
        }

        let trunk_len = trunk.metadata()?.len();
        while let Some(last) = chunks.last() {
            if last.comp_end() > trunk_len {
                eprintln!("timberfs: {name}: dropping index record for truncated chunk");
                chunks.pop();
            } else {
                break;
            }
        }
        // Trim dropped/partial trailing records from the index file.
        rings.set_len(RINGS_HEADER_LEN + (chunks.len() * RECORD_LEN) as u64)?;

        let comp_size = chunks.last().map(|c| c.comp_end()).unwrap_or(0);
        let buffer_start = chunks.last().map(|c| c.uncomp_end()).unwrap_or(0);
        Ok(FileStore {
            trunk,
            rings,
            chunks,
            comp_size,
            buffer: Vec::new(),
            buffer_start,
            buffer_first_ms: None,
            buffer_last_ms: 0,
            cache: None,
            staged: None,
        })
    }

    /// Logical (uncompressed) size of the file, including buffered bytes.
    pub fn size(&self) -> u64 {
        self.buffer_start + self.buffer.len() as u64
    }

    pub fn append(&mut self, data: &[u8], cfg: &Config) -> io::Result<()> {
        self.append_stamped(data, now_ms(), cfg)
    }

    /// Append with an explicit timestamp (`timberfs import`: the parsed
    /// log-line time rather than the wall clock). The chunk window is the
    /// min/max of the stamps it saw, so mildly out-of-order input simply
    /// widens windows — it never loses data.
    pub fn append_stamped(&mut self, data: &[u8], ts_ms: u64, cfg: &Config) -> io::Result<()> {
        self.append_windowed(data, ts_ms, ts_ms, cfg)
    }

    /// Append with an explicit write WINDOW (the records sink: an entry
    /// arriving with its original wf/wl keeps its write history). The
    /// chunk window is the min/max over everything buffered.
    pub fn append_windowed(
        &mut self,
        data: &[u8],
        first_ms: u64,
        last_ms: u64,
        cfg: &Config,
    ) -> io::Result<()> {
        if self.buffer.is_empty() {
            self.buffer_first_ms = Some(first_ms);
            self.buffer_last_ms = last_ms;
        } else {
            self.buffer_first_ms = Some(self.buffer_first_ms.unwrap_or(first_ms).min(first_ms));
            self.buffer_last_ms = self.buffer_last_ms.max(last_ms);
        }
        self.buffer.extend_from_slice(data);
        if self.buffer.len() >= cfg.chunk_size {
            self.flush_chunk(cfg)?;
        }
        Ok(())
    }

    /// Begin atomic staging (see the `staged` field).
    pub fn stage(&mut self) {
        self.staged = Some(StageBaseline {
            chunks: self.chunks.len(),
            comp_size: self.comp_size,
            buffer_start: self.buffer_start,
        });
    }

    /// Make everything appended since stage() visible: write the held
    /// ring records (data-first ordering, as ever), then sync both files.
    pub fn commit_stage(&mut self, cfg: &Config) -> io::Result<()> {
        self.flush_chunk(cfg)?;
        let Some(b) = self.staged.take() else {
            return Ok(());
        };
        for (i, rec) in self.chunks[b.chunks..].iter().enumerate() {
            let rec_off = RINGS_HEADER_LEN + ((b.chunks + i) * RECORD_LEN) as u64;
            self.rings.write_all_at(&rec.to_bytes(), rec_off)?;
        }
        self.trunk.sync_all()?;
        self.rings.sync_all()?;
        Ok(())
    }

    /// Roll back to the stage() baseline: truncate the trunk, forget the
    /// held records. Readers never saw any of it.
    pub fn abort_stage(&mut self) -> io::Result<()> {
        let Some(b) = self.staged.take() else {
            return Ok(());
        };
        self.trunk.set_len(b.comp_size)?;
        self.comp_size = b.comp_size;
        self.buffer_start = b.buffer_start;
        self.chunks.truncate(b.chunks);
        self.buffer.clear();
        self.buffer_first_ms = None;
        Ok(())
    }

    /// Compress the buffer into a zstd frame, append it to the .trunk, then
    /// append the index record. Data-first ordering is what makes crash
    /// recovery in open() safe.
    pub fn flush_chunk(&mut self, cfg: &Config) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let comp = zstd::stream::encode_all(&self.buffer[..], cfg.level)?;
        self.trunk.write_all_at(&comp, self.comp_size)?;
        let rec = ChunkRecord {
            uncomp_start: self.buffer_start,
            uncomp_len: self.buffer.len() as u64,
            comp_start: self.comp_size,
            comp_len: comp.len() as u64,
            first_write_ms: self.buffer_first_ms.unwrap_or(self.buffer_last_ms),
            last_write_ms: self.buffer_last_ms,
        };
        if self.staged.is_none() {
            let rec_off = RINGS_HEADER_LEN + (self.chunks.len() * RECORD_LEN) as u64;
            self.rings.write_all_at(&rec.to_bytes(), rec_off)?;
        }
        self.comp_size += comp.len() as u64;
        self.buffer_start += self.buffer.len() as u64;
        self.buffer.clear();
        self.buffer_first_ms = None;
        self.chunks.push(rec);
        Ok(())
    }

    pub fn read(&mut self, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        let end = offset.saturating_add(size as u64).min(self.size());
        if offset >= end {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            if pos >= self.buffer_start {
                let from = (pos - self.buffer_start) as usize;
                let to = (end - self.buffer_start) as usize;
                out.extend_from_slice(&self.buffer[from..to]);
                pos = end;
            } else {
                let idx = self.chunks.partition_point(|c| c.uncomp_end() <= pos);
                let chunk = self.chunks[idx];
                let stop = end.min(chunk.uncomp_end());
                let data = self.chunk_data(idx)?;
                let from = (pos - chunk.uncomp_start) as usize;
                let to = (stop - chunk.uncomp_start) as usize;
                out.extend_from_slice(&data[from..to]);
                pos = stop;
            }
        }
        Ok(out)
    }

    fn chunk_data(&mut self, idx: usize) -> io::Result<&Vec<u8>> {
        if self.cache.as_ref().map(|(i, _)| *i) != Some(idx) {
            let c = self.chunks[idx];
            let mut comp = vec![0u8; c.comp_len as usize];
            self.trunk.read_exact_at(&mut comp, c.comp_start)?;
            let data = zstd::stream::decode_all(&comp[..])?;
            if data.len() as u64 != c.uncomp_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chunk uncompressed length does not match index",
                ));
            }
            self.cache = Some((idx, data));
        }
        Ok(&self.cache.as_ref().unwrap().1)
    }

    /// fsync semantics: flush the buffer as a chunk and sync both files.
    pub fn sync(&mut self, cfg: &Config) -> io::Result<()> {
        self.flush_chunk(cfg)?;
        self.trunk.sync_all()?;
        self.rings.sync_all()?;
        Ok(())
    }

    /// Truncate-to-zero, i.e. copytruncate-style rotation: start over.
    pub fn reset(&mut self, dir: &Path, name: &str) -> io::Result<()> {
        let _ = fs::remove_file(format::grain_path(dir, name));
        self.trunk.set_len(0)?;
        self.rings.set_len(RINGS_HEADER_LEN)?;
        self.chunks.clear();
        self.comp_size = 0;
        self.buffer.clear();
        self.buffer_start = 0;
        self.buffer_first_ms = None;
        self.cache = None;
        Ok(())
    }

    pub fn first_write_ms(&self) -> Option<u64> {
        self.chunks
            .first()
            .map(|c| c.first_write_ms)
            .or(self.buffer_first_ms)
    }

    pub fn last_write_ms(&self) -> Option<u64> {
        if self.buffer.is_empty() {
            self.chunks.last().map(|c| c.last_write_ms)
        } else {
            Some(self.buffer_last_ms)
        }
    }

    fn buffer_age_ms(&self, now: u64) -> Option<u64> {
        self.buffer_first_ms.map(|t| now.saturating_sub(t))
    }

    /// Number of leading chunks written entirely before the cutoff. An
    /// explicit prefix scan, not a binary search: imported files carry
    /// logged timestamps whose chunk windows are only mostly sorted.
    fn rotation_split(&self, cutoff_ms: u64) -> usize {
        self.chunks
            .iter()
            .take_while(|c| c.last_write_ms < cutoff_ms)
            .count()
    }

    fn has_buffer_before(&self, cutoff_ms: u64) -> bool {
        self.buffer_first_ms.map(|t| t < cutoff_ms).unwrap_or(false)
    }

    /// Append another timberfs file's chunks verbatim: the compressed
    /// frames are copied as-is (no recompression) and the index records
    /// are rebased into this file's offset space. Used by rotation and by
    /// timberfs-to-timberfs import. The records must be one contiguous
    /// run in their trunk; the time ordering of this file's index is
    /// protected.
    pub fn append_frames(
        &mut self,
        src_trunk: &File,
        records: &[ChunkRecord],
        cfg: &Config,
    ) -> io::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        self.flush_chunk(cfg)?;
        if let Some(last_ms) = self.last_write_ms() {
            if last_ms > records[0].first_write_ms {
                return Err(invalid_input(
                    "target already contains data newer than the incoming chunks \
                     (would break the index time ordering)",
                ));
            }
        }
        let uncomp_base = self.buffer_start;
        let comp_base = self.comp_size;
        let src_comp_start = records[0].comp_start;
        let src_uncomp_start = records[0].uncomp_start;
        let total_comp = records.last().unwrap().comp_end() - src_comp_start;
        copy_range(
            src_trunk,
            src_comp_start,
            total_comp,
            &self.trunk,
            comp_base,
        )?;
        for c in records {
            let rec = ChunkRecord {
                uncomp_start: uncomp_base + (c.uncomp_start - src_uncomp_start),
                comp_start: comp_base + (c.comp_start - src_comp_start),
                ..*c
            };
            let off = RINGS_HEADER_LEN + (self.chunks.len() * RECORD_LEN) as u64;
            self.rings.write_all_at(&rec.to_bytes(), off)?;
            self.chunks.push(rec);
        }
        self.comp_size = comp_base + total_comp;
        self.buffer_start = uncomp_base + (records.last().unwrap().uncomp_end() - src_uncomp_start);
        self.cache = None;
        self.trunk.sync_all()?;
        self.rings.sync_all()?;
        Ok(())
    }

    /// Cut the first `k` chunks off this file: the remaining frames and a
    /// rebased index are written to temp files which are renamed over the
    /// originals, then the in-memory state is rebased to match. The
    /// unflushed buffer (data newer than any chunk) is untouched.
    fn remove_head(&mut self, k: usize, dir: &Path, name: &str) -> io::Result<()> {
        if k == 0 {
            return Ok(());
        }
        let comp_cut = self.chunks[k - 1].comp_end();
        let uncomp_cut = self.chunks[k - 1].uncomp_end();
        let trunk_p = format::trunk_path(dir, name);
        let rings_p = format::rings_path(dir, name);
        let trunk_tmp = dir.join(format!("{name}.{}.tmp", format::TRUNK_EXT));
        let rings_tmp = dir.join(format!("{name}.{}.tmp", format::RINGS_EXT));
        // Fallible section only builds the temp files; nothing here has
        // touched the live trunk/rings yet, so an error (ENOSPC, most
        // likely — exactly when this rewrite is tightest on space) just
        // needs the partial temps cleaned up, not a rollback.
        let staged: io::Result<()> = (|| {
            let new_trunk = File::create(&trunk_tmp)?;
            copy_range(
                &self.trunk,
                comp_cut,
                self.comp_size - comp_cut,
                &new_trunk,
                0,
            )?;
            new_trunk.sync_all()?;
            let mut idx = Vec::with_capacity(
                RINGS_HEADER_LEN as usize + (self.chunks.len() - k) * RECORD_LEN,
            );
            idx.extend_from_slice(format::RINGS_MAGIC);
            for c in &self.chunks[k..] {
                let rec = ChunkRecord {
                    uncomp_start: c.uncomp_start - uncomp_cut,
                    comp_start: c.comp_start - comp_cut,
                    ..*c
                };
                idx.extend_from_slice(&rec.to_bytes());
            }
            let new_rings = File::create(&rings_tmp)?;
            new_rings.write_all_at(&idx, 0)?;
            new_rings.sync_all()?;
            Ok(())
        })();
        if let Err(e) = staged {
            let _ = fs::remove_file(&trunk_tmp);
            let _ = fs::remove_file(&rings_tmp);
            return Err(e);
        }
        fs::rename(&trunk_tmp, &trunk_p)?;
        fs::rename(&rings_tmp, &rings_p)?;
        // Sidecar contract: a rings rewrite invalidates chunk numbering,
        // so derived indexes are deleted (rebuild with `timberfs reindex`).
        let _ = fs::remove_file(format::grain_path(dir, name));
        self.trunk = OpenOptions::new().read(true).write(true).open(&trunk_p)?;
        self.rings = OpenOptions::new().read(true).write(true).open(&rings_p)?;
        self.chunks.drain(..k);
        for c in &mut self.chunks {
            c.uncomp_start -= uncomp_cut;
            c.comp_start -= comp_cut;
        }
        self.comp_size -= comp_cut;
        self.buffer_start -= uncomp_cut;
        self.cache = None;
        Ok(())
    }

    /// Cut the first `k` chunks off this file via
    /// `FALLOC_FL_COLLAPSE_RANGE`: the kernel shifts the surviving
    /// compressed bytes down IN the existing trunk inode, so peak disk
    /// usage is ~1x the store rather than `remove_head`'s ~2x (a full
    /// rewrite briefly coexisting with the original). Returns `Ok(false)`
    /// when collapse isn't applicable here — too little data to cut a
    /// whole filesystem block, or the filesystem doesn't support
    /// `COLLAPSE_RANGE` (tmpfs, btrfs, NFS, older ext4/xfs) — so the
    /// caller can fall back to `remove_head`; `Ok(true)` once the cut has
    /// landed and in-memory state is rebased to match.
    fn collapse_head(&mut self, k: usize, dir: &Path, name: &str) -> io::Result<bool> {
        if k == 0 {
            return Ok(true);
        }
        let comp_cut = self.chunks[k - 1].comp_end();
        let uncomp_cut = self.chunks[k - 1].uncomp_end();
        let bsize = fstatvfs_bsize(&self.trunk)?;
        let Some((aligned, sliver)) = collapse_alignment(comp_cut, bsize) else {
            return Ok(false);
        };

        let rings_p = format::rings_path(dir, name);
        let rings_tmp = dir.join(format!("{name}.{}.tmp", format::RINGS_EXT));
        let trim_p = format::trim_path(dir, name);
        let cleanup_staged = || {
            let _ = fs::remove_file(&rings_tmp);
            let _ = fs::remove_file(&trim_p);
        };

        // Stage the rebased rings under a temp name — not yet the live
        // index — before touching the trunk at all.
        let staged: io::Result<()> = (|| {
            let mut idx = Vec::with_capacity(
                RINGS_HEADER_LEN as usize + (self.chunks.len() - k) * RECORD_LEN,
            );
            idx.extend_from_slice(format::RINGS_MAGIC);
            for c in &self.chunks[k..] {
                let rec = ChunkRecord {
                    uncomp_start: c.uncomp_start - uncomp_cut,
                    comp_start: c.comp_start - aligned,
                    ..*c
                };
                idx.extend_from_slice(&rec.to_bytes());
            }
            let new_rings = File::create(&rings_tmp)?;
            new_rings.write_all_at(&idx, 0)?;
            new_rings.sync_all()?;
            Ok(())
        })();
        if let Err(e) = staged {
            cleanup_staged();
            return Err(e);
        }

        // The crash marker, written (and fsynced, with the staged rings)
        // BEFORE the fallocate: if we die before the final rename below,
        // FileStore::open's reconcile_trim tells landed from not-landed
        // by comparing the trunk's actual size against these two values,
        // rather than misreading a shorter trunk as truncated writes.
        if let Err(e) = write_trim_marker(&trim_p, self.comp_size, aligned, sliver) {
            cleanup_staged();
            return Err(e);
        }

        // Odd = a collapse is in flight: a concurrent standalone reader
        // (query/info in their own process) must not trust an offset
        // resolved while this is odd, since the trunk can be mutated out
        // from under them mid-read. See query.rs's seqlock guard.
        let seq0 = read_seq(dir, name);
        if let Err(e) = write_seq(dir, name, seq0 + 1) {
            cleanup_staged();
            return Err(e);
        }

        let rc = unsafe {
            libc::fallocate(
                self.trunk.as_raw_fd(),
                libc::FALLOC_FL_COLLAPSE_RANGE,
                0,
                aligned as libc::off_t,
            )
        };
        if rc != 0 {
            let e = io::Error::last_os_error();
            cleanup_staged();
            let _ = write_seq(dir, name, seq0);
            return match e.raw_os_error() {
                Some(libc::EOPNOTSUPP) | Some(libc::EINVAL) => Ok(false),
                _ => Err(e),
            };
        }

        // The fallocate has committed the cut to the trunk — there is no
        // going back. From here the store MUST end consistent, or a still-
        // running writer would append at a stale offset and corrupt it. The
        // stamp and the seqlock reset don't touch in-memory offsets (a
        // missing stamp only degrades `zstd -dc` until the next reindex; an
        // unreset seqlock only makes readers retry), so those are
        // best-effort. The index rename and reopen DO define the offset
        // space, so a failure there is fatal: exit before the maintenance
        // thread can append onto a divergent store — the .trim marker makes
        // the next startup reconcile the landed cut (mirrors the
        // binary-upgrade exit in the same thread).
        if let Err(e) = stamp_skippable_frame(&self.trunk, sliver) {
            eprintln!(
                "timberfs: {name}: skippable-frame stamp failed after collapse ({e}); \
                 `zstd -dc` recovery needs a `timberfs reindex` until then"
            );
        }
        if let Err(e) = fs::rename(&rings_tmp, &rings_p) {
            eprintln!(
                "timberfs: {name}: FATAL: collapse landed but committing the rebased \
                 index failed ({e}); exiting so no write lands at a stale offset — \
                 the .trim marker reconciles it on restart"
            );
            std::process::exit(1);
        }
        // Sidecar contract: a rings rewrite invalidates chunk numbering,
        // so derived indexes are deleted (rebuild with `timberfs reindex`).
        let _ = fs::remove_file(format::grain_path(dir, name));
        let _ = fs::remove_file(&trim_p);
        if let Err(e) = write_seq(dir, name, seq0 + 2) {
            eprintln!(
                "timberfs: {name}: resetting the collapse seqlock failed ({e}); \
                 readers retry until the next collapse or reconcile"
            );
        }

        self.rings = match OpenOptions::new().read(true).write(true).open(&rings_p) {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "timberfs: {name}: FATAL: collapse landed but reopening the rebased \
                     index failed ({e}); exiting so no write lands at a stale offset — \
                     the .trim marker reconciles it on restart"
                );
                std::process::exit(1);
            }
        };
        self.chunks.drain(..k);
        for c in &mut self.chunks {
            c.uncomp_start -= uncomp_cut;
            c.comp_start -= aligned;
        }
        self.comp_size -= aligned;
        self.buffer_start -= uncomp_cut;
        self.cache = None;
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RotateStats {
    pub chunks_moved: usize,
    pub uncomp_bytes: u64,
    pub comp_bytes: u64,
    pub first_write_ms: u64,
    pub last_write_ms: u64,
    pub chunks_remaining: usize,
}

pub struct Store {
    pub dir: PathBuf,
    pub cfg: Config,
    pub files: BTreeMap<String, FileStore>,
}

impl Store {
    /// Open a backing directory, loading every `<name>.rings` found in it.
    pub fn open(dir: &Path, cfg: Config) -> io::Result<Store> {
        fs::create_dir_all(dir)?;
        let mut files = BTreeMap::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some(format::RINGS_EXT) {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    files.insert(stem.to_string(), FileStore::open(dir, stem)?);
                }
            }
        }
        Ok(Store {
            dir: dir.to_path_buf(),
            cfg,
            files,
        })
    }

    pub fn create(&mut self, name: &str) -> io::Result<()> {
        if !self.files.contains_key(name) {
            let f = FileStore::open(&self.dir, name)?;
            self.files.insert(name.to_string(), f);
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> io::Result<()> {
        if self.files.remove(name).is_none() {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        let _ = fs::remove_file(format::trunk_path(&self.dir, name));
        let _ = fs::remove_file(format::rings_path(&self.dir, name));
        let _ = fs::remove_file(format::grain_path(&self.dir, name));
        let _ = fs::remove_file(format::bark_path(&self.dir, name));
        Ok(())
    }

    /// Rename, the normal log rotation path (mv app.log app.log.1). The
    /// open file handles keep working across the backing-file rename.
    pub fn rename(&mut self, old: &str, new: &str) -> io::Result<()> {
        let cfg = self.cfg;
        let mut f = self
            .files
            .remove(old)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        if let Err(e) = f.flush_chunk(&cfg) {
            self.files.insert(old.to_string(), f);
            return Err(e);
        }
        // Rename-over semantics: drop any existing target.
        self.files.remove(new);
        let _ = fs::remove_file(format::trunk_path(&self.dir, new));
        let _ = fs::remove_file(format::rings_path(&self.dir, new));
        fs::rename(
            format::trunk_path(&self.dir, old),
            format::trunk_path(&self.dir, new),
        )?;
        fs::rename(
            format::rings_path(&self.dir, old),
            format::rings_path(&self.dir, new),
        )?;
        let _ = fs::rename(
            format::grain_path(&self.dir, old),
            format::grain_path(&self.dir, new),
        );
        let _ = fs::rename(
            format::bark_path(&self.dir, old),
            format::bark_path(&self.dir, new),
        );
        self.files.insert(new.to_string(), f);
        Ok(())
    }

    /// Called by the background flusher thread: force out buffers whose
    /// oldest byte is older than the configured flush age.
    pub fn flush_aged(&mut self) {
        let now = now_ms();
        let cfg = self.cfg;
        for (name, f) in self.files.iter_mut() {
            if let Some(age) = f.buffer_age_ms(now) {
                if age >= cfg.flush_age_ms {
                    if let Err(e) = f.flush_chunk(&cfg) {
                        eprintln!("timberfs: {name}: background flush failed: {e}");
                    }
                }
            }
        }
    }

    /// Time-based rotation: move every chunk of `source` written entirely
    /// before `cutoff_ms` into `target` (appending if it exists), or drop
    /// them when `target` is None (retention). Compressed frames move
    /// verbatim — nothing is recompressed. Chunk-granular like queries: a
    /// chunk straddling the cutoff stays in the source.
    pub fn rotate_head(
        &mut self,
        source: &str,
        target: Option<&str>,
        cutoff_ms: u64,
    ) -> io::Result<RotateStats> {
        let cfg = self.cfg;
        if target == Some(source) {
            return Err(invalid_input("rotation target equals source"));
        }
        {
            let src = self
                .files
                .get_mut(source)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            if src.has_buffer_before(cutoff_ms) {
                src.flush_chunk(&cfg)?;
            }
        }
        let moved: Vec<ChunkRecord> = {
            let src = self.files.get(source).unwrap();
            let k = src.rotation_split(cutoff_ms);
            if k == 0 {
                return Ok(RotateStats {
                    chunks_moved: 0,
                    uncomp_bytes: 0,
                    comp_bytes: 0,
                    first_write_ms: 0,
                    last_write_ms: 0,
                    chunks_remaining: src.chunks.len(),
                });
            }
            src.chunks[..k].to_vec()
        };
        if let Some(tname) = target {
            self.create(tname)?;
            // Take the target out of the map so we can hold it mutably
            // alongside an immutable borrow of the source.
            let mut tgt = self.files.remove(tname).unwrap();
            let src = self.files.get(source).unwrap();
            let res = tgt.append_frames(&src.trunk, &moved, &cfg);
            self.files.insert(tname.to_string(), tgt);
            res?;
        }
        let src = self.files.get_mut(source).unwrap();
        src.remove_head(moved.len(), &self.dir, source)?;
        Ok(RotateStats {
            chunks_moved: moved.len(),
            uncomp_bytes: moved.last().unwrap().uncomp_end(),
            comp_bytes: moved.last().unwrap().comp_end(),
            first_write_ms: moved.first().unwrap().first_write_ms,
            last_write_ms: moved.last().unwrap().last_write_ms,
            chunks_remaining: src.chunks.len(),
        })
    }

    /// Final flush + sync of everything, used on unmount.
    pub fn flush_all(&mut self) {
        let cfg = self.cfg;
        for (name, f) in self.files.iter_mut() {
            if let Err(e) = f.sync(&cfg) {
                eprintln!("timberfs: {name}: flush on unmount failed: {e}");
            }
        }
    }

    /// Continuous retention (the appender's --retain / --retain-size):
    /// drop head chunks older than `max_age_ms`, and keep the compressed
    /// size at or under `max_comp_bytes`. Dropping the head compacts the
    /// backing pair by rewriting what remains, so it triggers with
    /// hysteresis: age-expired data goes once it makes up at least a tenth
    /// of the file, a size overrun drops down to 95% of the budget. The
    /// unflushed buffer (newest data) is never touched. Returns stats when
    /// something was dropped.
    pub fn enforce_retention(
        &mut self,
        name: &str,
        max_age_ms: Option<u64>,
        max_comp_bytes: Option<u64>,
    ) -> io::Result<Option<RotateStats>> {
        let f = self
            .files
            .get_mut(name)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        if f.chunks.is_empty() {
            return Ok(None);
        }
        let mut k = 0usize;
        if let Some(age) = max_age_ms {
            let cutoff = now_ms().saturating_sub(age);
            let kt = f.rotation_split(cutoff);
            if kt > 0 && f.chunks[kt - 1].comp_end() * 10 >= f.comp_size {
                k = k.max(kt);
            }
        }
        if let Some(budget) = max_comp_bytes {
            if f.comp_size > budget {
                let low_water = budget.saturating_sub(budget / 20);
                // collapse_head can only cut on a filesystem-block
                // boundary, rounding the cut DOWN — up to ~2 blocks of the
                // dropped range's tail survives as an inert skippable
                // frame (one block from the alignment itself, up to one
                // more if the sliver was too small for the header and it
                // backed off a further block). Aim a couple of blocks
                // further below the low-water mark so that slack still
                // lands at or under it, never over the hard budget.
                let margin = fstatvfs_bsize(&f.trunk).unwrap_or(4096) * 2;
                let target = low_water.saturating_sub(margin);
                let ks = f
                    .chunks
                    .partition_point(|c| f.comp_size - c.comp_start > target);
                k = k.max(ks);
            }
        }
        if k == 0 {
            return Ok(None);
        }
        let k = k.min(f.chunks.len());
        let stats = RotateStats {
            chunks_moved: k,
            uncomp_bytes: f.chunks[k - 1].uncomp_end(),
            comp_bytes: f.chunks[k - 1].comp_end(),
            first_write_ms: f.chunks[0].first_write_ms,
            last_write_ms: f.chunks[k - 1].last_write_ms,
            chunks_remaining: f.chunks.len() - k,
        };
        // Prefer the in-place collapse (peak disk ~1x the store) and only
        // fall back to the rewrite (peak ~2x) when collapse doesn't apply
        // here (too little to cut a whole block, or the filesystem
        // doesn't support COLLAPSE_RANGE).
        if !f.collapse_head(k, &self.dir, name)? {
            f.remove_head(k, &self.dir, name)?;
        }
        Ok(Some(stats))
    }
}

/// Locking, two levels, all flock-based (locks die with their process):
///
/// - the directory lock `.timberfs.lock`: the mount daemon holds it
///   EXCLUSIVE (it owns in-memory state for every file in the directory);
///   appenders and offline rotation hold it SHARED — they coexist with
///   each other but never with a mount.
/// - a per-file lock `<name>.lock`: held exclusive by the writer of that
///   one file (an appender, or rotation for its source/destination). A
///   separate always-stable file, never the .rings itself, because
///   head-removal replaces the .rings inode by rename and a lock on a
///   renamed-over inode would silently stop excluding anyone.
///
/// Lock files are never deleted (unlink+recreate would let two processes
/// hold "the" lock on different inodes).
pub const LOCK_FILE_NAME: &str = ".timberfs.lock";

pub fn file_lock_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.lock"))
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "opening lock {} (need write access to the backing directory)",
                    path.display()
                ),
            )
        })
}

/// Ok(Some(file)) = lock acquired, keep the File alive to hold it.
/// Ok(None) = held by someone else in a conflicting mode.
fn try_flock(f: File, op: libc::c_int) -> io::Result<Option<File>> {
    let rc = unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(f))
    } else {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(e)
        }
    }
}

/// Directory lock, exclusive: the mount daemon.
pub fn lock_backing_exclusive(dir: &Path) -> io::Result<Option<File>> {
    try_flock(open_lock_file(&dir.join(LOCK_FILE_NAME))?, libc::LOCK_EX)
}

/// Directory lock, shared: appenders and offline rotation. Fails only
/// while a mount daemon holds the directory exclusively.
pub fn lock_backing_shared(dir: &Path) -> io::Result<Option<File>> {
    try_flock(open_lock_file(&dir.join(LOCK_FILE_NAME))?, libc::LOCK_SH)
}

/// Per-file writer lock, exclusive.
pub fn lock_file_exclusive(dir: &Path, name: &str) -> io::Result<Option<File>> {
    try_flock(open_lock_file(&file_lock_path(dir, name))?, libc::LOCK_EX)
}

/// The result of a READ-ONLY lock probe. Read-only commands (`info`)
/// must be able to inspect a store they can only read — a root-owned,
/// world-readable backing directory must not require write access just
/// to report who is writing. So the probe OPENS the lock file read-only
/// and never creates it (flock works fine on an O_RDONLY fd); it is an
/// observation, not an acquisition.
pub enum LockProbe {
    /// The lock file does not exist — no one ever took this lock.
    Absent,
    /// We could take the tested lock — no conflicting holder is alive.
    Free,
    /// A conflicting holder is alive (an active writer, or a mount).
    Held,
    /// The lock file exists but we could not open it (permissions) —
    /// we cannot tell.
    Unreadable,
}

fn probe_lock(path: &Path, op: libc::c_int) -> LockProbe {
    let f = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return LockProbe::Absent,
        Err(_) => return LockProbe::Unreadable,
    };
    let rc = unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) };
    if rc == 0 {
        // Acquired (and released when `f` drops): nobody held it — and
        // because flock is released on process death, this reflects a
        // LIVE holder, not stale lock-file contents.
        LockProbe::Free
    } else if io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
        LockProbe::Held
    } else {
        LockProbe::Unreadable
    }
}

/// Read-only probe: is the backing directory held EXCLUSIVELY (by a
/// mount daemon)? Tested by trying a SHARED lock — failure to get it
/// means a live exclusive holder. Absent/Free => no mount.
pub fn probe_backing_exclusive(dir: &Path) -> LockProbe {
    probe_lock(&dir.join(LOCK_FILE_NAME), libc::LOCK_SH)
}

/// Read-only probe: is a file's writer lock held — a live appender,
/// import or rotation?
pub fn probe_file_writer(dir: &Path, name: &str) -> LockProbe {
    probe_lock(&file_lock_path(dir, name), libc::LOCK_EX)
}

/// Names of files in the directory whose per-file writer lock is currently
/// held (probed non-destructively), for diagnostics in refusal messages.
pub fn active_file_locks(dir: &Path) -> Vec<String> {
    let mut active = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return active;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(base) = file_name.strip_suffix(".lock") else {
            continue;
        };
        if file_name == LOCK_FILE_NAME || base.is_empty() {
            continue;
        }
        if let Ok(f) = File::open(entry.path()) {
            let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                active.push(base.to_string());
            }
            // drop(f) releases the probe lock if we got it
        }
    }
    active.sort();
    active
}

/// Record who holds the lock (`mountpoint=...` for the mount daemon,
/// `appender=...` for a pipe appender), so tools can route or explain.
pub fn write_lock_info(f: &File, info: &str) -> io::Result<()> {
    f.set_len(0)?;
    f.write_all_at(info.as_bytes(), 0)?;
    f.sync_all()?;
    Ok(())
}

pub fn read_lock_mountpoint(dir: &Path) -> Option<PathBuf> {
    let s = fs::read_to_string(dir.join(LOCK_FILE_NAME)).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("mountpoint=").map(PathBuf::from))
}

/// Raw lock-file content, for describing the current holder in messages.
pub fn read_lock_raw(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join(LOCK_FILE_NAME)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_deleted_recovers_install_path() {
        // The kernel-marked "(deleted)" target must reduce to the real path,
        // so BinaryWatch watches /usr/bin/timberfs even when a swap landed
        // during our startup (else changed() stats a bogus path forever).
        assert_eq!(
            strip_deleted(PathBuf::from("/usr/bin/timberfs (deleted)")),
            PathBuf::from("/usr/bin/timberfs")
        );
        // A normal path is untouched...
        assert_eq!(
            strip_deleted(PathBuf::from("/usr/bin/timberfs")),
            PathBuf::from("/usr/bin/timberfs")
        );
        // ...including one that merely contains the word deleted, or a path
        // that legitimately ends in "(deleted)" without the leading space.
        assert_eq!(
            strip_deleted(PathBuf::from("/opt/deleted/timberfs")),
            PathBuf::from("/opt/deleted/timberfs")
        );
        assert_eq!(
            strip_deleted(PathBuf::from("/opt/timberfs(deleted)")),
            PathBuf::from("/opt/timberfs(deleted)")
        );
    }

    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique scratch directory that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("timberfs-store-test-{}-{n}", std::process::id()));
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
    fn collapse_alignment_falls_back_below_one_block() {
        // comp_cut doesn't even reach one whole block: nothing to collapse.
        assert_eq!(collapse_alignment(100, 4096), None);
    }

    #[test]
    fn collapse_alignment_aligns_down_to_the_block() {
        assert_eq!(
            collapse_alignment(4096 * 2 + 50, 4096),
            Some((4096 * 2, 50))
        );
    }

    #[test]
    fn collapse_alignment_exact_multiple_has_no_sliver() {
        assert_eq!(collapse_alignment(4096 * 3, 4096), Some((4096 * 3, 0)));
    }

    #[test]
    fn collapse_alignment_backs_off_a_block_when_sliver_too_small() {
        // A 5-byte sliver has no room for the 8-byte skippable-frame
        // header, so collapse one block fewer — growing the sliver past it.
        assert_eq!(
            collapse_alignment(4096 * 2 + 5, 4096),
            Some((4096, 4096 + 5))
        );
    }

    #[test]
    fn collapse_alignment_gives_up_when_backing_off_hits_zero() {
        // Only one block is available and its sliver is < 8: backing off
        // to fit the header leaves nothing left to collapse at all.
        assert_eq!(collapse_alignment(4096 + 5, 4096), None);
    }

    #[test]
    fn stamped_sliver_is_a_valid_skippable_frame() {
        let dir = TempDir::new();
        let trunk_path = dir.path().join("t.trunk");
        let kept = zstd::stream::encode_all(&b"world\n"[..], 3).unwrap();
        let sliver_len = 20u64;
        // The leftover tail of the dropped frame: arbitrary bytes, since
        // the skippable-frame length field is what makes zstd skip them,
        // not their content.
        let mut buf = vec![0xABu8; sliver_len as usize];
        buf.extend_from_slice(&kept);
        fs::write(&trunk_path, &buf).unwrap();

        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&trunk_path)
            .unwrap();
        stamp_skippable_frame(&f, sliver_len).unwrap();
        drop(f);

        let all = fs::read(&trunk_path).unwrap();
        let magic = u32::from_le_bytes(all[0..4].try_into().unwrap());
        assert!((0x184D2A50..=0x184D2A5F).contains(&magic));
        let declared_len = u32::from_le_bytes(all[4..8].try_into().unwrap()) as u64;
        // The frame occupies exactly [0, sliver_len): header + declared
        // user-data length adds back up to the whole sliver.
        assert_eq!(8 + declared_len, sliver_len);
        // zstd -dc skips the stamped frame and decodes straight into the
        // real one that follows.
        let decoded = zstd::stream::decode_all(&all[..]).unwrap();
        assert_eq!(decoded, b"world\n");
    }

    #[test]
    fn stamp_skippable_frame_is_a_noop_for_a_zero_sliver() {
        let dir = TempDir::new();
        let trunk_path = dir.path().join("t.trunk");
        let kept = zstd::stream::encode_all(&b"exact\n"[..], 3).unwrap();
        fs::write(&trunk_path, &kept).unwrap();
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&trunk_path)
            .unwrap();
        stamp_skippable_frame(&f, 0).unwrap();
        drop(f);
        assert_eq!(fs::read(&trunk_path).unwrap(), kept);
    }

    #[test]
    fn reconcile_trim_rolls_back_when_the_collapse_never_landed() {
        let dir = TempDir::new();
        let name = "app";
        // Trunk is still at its pre-collapse size: the fallocate never
        // actually happened before the crash.
        fs::write(format::trunk_path(dir.path(), name), vec![0u8; 100]).unwrap();
        fs::write(format::rings_path(dir.path(), name), b"old-rings").unwrap();
        let rings_tmp = dir.path().join(format!("{name}.{}.tmp", format::RINGS_EXT));
        fs::write(&rings_tmp, b"staged-rings").unwrap();
        write_trim_marker(&format::trim_path(dir.path(), name), 100, 40, 0).unwrap();

        reconcile_trim(dir.path(), name).unwrap();

        assert!(!format::trim_path(dir.path(), name).exists());
        assert!(!rings_tmp.exists());
        assert_eq!(
            fs::read(format::rings_path(dir.path(), name)).unwrap(),
            b"old-rings"
        );
    }

    #[test]
    fn reconcile_trim_rolls_forward_when_the_collapse_landed() {
        let dir = TempDir::new();
        let name = "app";
        let sliver = 20u64;
        let mut trunk_bytes = vec![0xCDu8; sliver as usize];
        trunk_bytes.extend_from_slice(&zstd::stream::encode_all(&b"kept\n"[..], 3).unwrap());
        fs::write(format::trunk_path(dir.path(), name), &trunk_bytes).unwrap();
        fs::write(format::rings_path(dir.path(), name), b"old-rings").unwrap();
        let rings_tmp = dir.path().join(format!("{name}.{}.tmp", format::RINGS_EXT));
        fs::write(&rings_tmp, b"staged-rings").unwrap();
        fs::write(format::grain_path(dir.path(), name), b"stale-grain").unwrap();
        // aligned = 40: the trunk's actual size is pre_comp_size - aligned,
        // proving the fallocate landed before the crash.
        let pre_comp_size = trunk_bytes.len() as u64 + 40;
        write_trim_marker(
            &format::trim_path(dir.path(), name),
            pre_comp_size,
            40,
            sliver,
        )
        .unwrap();

        reconcile_trim(dir.path(), name).unwrap();

        assert!(!format::trim_path(dir.path(), name).exists());
        assert!(!rings_tmp.exists());
        assert!(!format::grain_path(dir.path(), name).exists());
        assert_eq!(
            fs::read(format::rings_path(dir.path(), name)).unwrap(),
            b"staged-rings"
        );
        let all = fs::read(format::trunk_path(dir.path(), name)).unwrap();
        assert_eq!(zstd::stream::decode_all(&all[..]).unwrap(), b"kept\n");
    }

    #[test]
    fn reconcile_trim_is_a_noop_with_no_marker() {
        let dir = TempDir::new();
        // No .trim file at all: nothing to reconcile, no error.
        reconcile_trim(dir.path(), "app").unwrap();
    }
}
