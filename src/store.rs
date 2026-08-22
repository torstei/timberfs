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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::format::{self, ChunkRecord, RECORD_LEN, RINGS_HEADER_LEN};
use crate::sap;

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

pub(crate) fn fstatvfs_bsize(f: &File) -> io::Result<u64> {
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
///     same as a normal collapse's tail). The rename itself must be
///     idempotent too: a crash can land after the rename already
///     committed (between it and the `.trim` removal), in which case
///     `rings.tmp` is already gone and `rings` already holds the rebased
///     index — that's success, not an error, so a missing `rings.tmp` here
///     just means "already renamed" and the rename is skipped.
///
/// Either finalizing branch also resets the collapse seqlock (store.rs) to
/// even if it's odd: a writer that died after bumping it but before
/// resetting it would otherwise leave standalone readers spinning forever
/// against a store that looks permanently mid-collapse. Best-effort — a
/// read-only caller without write access can't write it, but the marker
/// stays gone either way, so the next writer's own `open` clears it.
///
/// Best-effort by design: a read-only caller without write access to the
/// directory (a non-root `query`/`info`) just leaves the marker for the
/// next writer to reconcile, rather than erroring out of a read.
/// Rings v1 -> v2: the same records, each gaining its chunk number, plus a
/// header carrying the high-water mark. Numbering starts at 0 for the oldest
/// SURVIVING record — a definition of where this store's numbering begins,
/// not an attempt to recover how many it dropped before anyone counted.
///
/// Lazy and idempotent: it runs when a writer opens the store, so no
/// operator step exists, and temp + rename means a crash leaves the v1 file
/// intact and the migration simply runs again. Readers need no migration at
/// all — they synthesize the same numbers when parsing v1.
fn migrate_rings(dir: &Path, name: &str) -> io::Result<()> {
    let p = format::rings_path(dir, name);
    let buf = match fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // Absent, empty, already v2, or something else entirely: not ours to
    // touch. A bad magic is left for `open` to report as it always has.
    if buf.len() < 8 || &buf[..8] != format::RINGS_MAGIC_V1 {
        return Ok(());
    }
    let (records, _) = format::parse_index_versioned(&buf)?;
    let next_seq = records.last().map(|c| c.seq + 1).unwrap_or(0);
    let mut idx =
        Vec::with_capacity(format::RINGS_HEADER_LEN as usize + records.len() * format::RECORD_LEN);
    // A v1 file never carried the drop counters, so there is nothing
    // to migrate and zero is the honest answer.
    idx.extend_from_slice(&format::rings_header(next_seq, format::Dropped::default()));
    for c in &records {
        idx.extend_from_slice(&c.to_bytes());
    }
    let tmp = dir.join(format!("{name}.{}.migrate", format::RINGS_EXT));
    {
        let f = File::create(&tmp)?;
        f.write_all_at(&idx, 0)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &p)?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    eprintln!(
        "timberfs: {name}: rings migrated to {} — {} chunk(s) numbered from 0",
        String::from_utf8_lossy(format::RINGS_MAGIC),
        records.len()
    );
    Ok(())
}

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
        reset_seq_if_odd(dir, name);
    } else if trunk_len == pre_comp_size.saturating_sub(aligned) {
        if sliver >= 8 {
            let trunk = OpenOptions::new()
                .write(true)
                .open(format::trunk_path(dir, name))?;
            stamp_skippable_frame(&trunk, sliver)?;
        }
        match fs::rename(&rings_tmp, format::rings_path(dir, name)) {
            Ok(()) => {}
            // Already renamed by a prior attempt that crashed between the
            // rename and the marker removal below: the live rings already
            // holds the rebased index, nothing left to redo.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let _ = fs::remove_file(format::grain_path(dir, name));
        let _ = fs::remove_file(&trim_p);
        reset_seq_if_odd(dir, name);
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

/// Reset the collapse seqlock to even if a crash left it odd (see
/// `reconcile_trim`). Best-effort: a failed write here just means readers
/// keep retrying until the next writer's `open` gets a chance.
fn reset_seq_if_odd(dir: &Path, name: &str) {
    let seq = read_seq(dir, name);
    if seq % 2 == 1 {
        if let Err(e) = write_seq(dir, name, seq + 1) {
            eprintln!(
                "timberfs: {name}: resetting the collapse seqlock during \
                 reconcile failed ({e}); readers retry until the next writer opens it"
            );
        }
    }
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
    dir: PathBuf,
    name: String,
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
    /// The write-ahead sidecar (sap.rs), live only when `"wal": true` is
    /// declared in `.bark` — `None` costs nothing, the default for every
    /// store today. Its content always equals `buffer` by construction;
    /// see `append_windowed` and `flush_chunk`'s seal-and-swap.
    wal: Option<sap::Sap>,
    /// The number the next chunk gets. Monotone for the life of the store,
    /// never derived from `chunks.len()`: a head-drop removes records
    /// without moving the numbering down, and retention can remove ALL of
    /// them — a store that renumbered from 0 there would hand a fresh chunk
    /// a number a cursor already counts as consumed. Recovered at open as
    /// the greater of the header's high-water mark and one past the newest
    /// surviving record.
    next_seq: u64,
    /// What has left this store over its whole life — see `format::Dropped`.
    /// Carried in memory because the head-drop paths, which are the only
    /// ones that change it, need the running total to write.
    dropped: format::Dropped,
}

impl FileStore {
    /// Open (or create) the backing pair for a logical file and reconcile
    /// index, data and the write-ahead sidecar after a possible crash.
    ///
    /// Sap recovery MUTATES the store (it can rebuild the buffer, or force
    /// a flush to complete an interrupted one), so it must only run for a
    /// writer already holding this file's exclusive lock. Verified: every
    /// read-only path (`query`/`info`/`grep`/`timber-filter`) resolves
    /// sources through `query::open_source`, which never calls
    /// `FileStore::open`; every writer (`append`, the records sink,
    /// `import`, `rotate`, the mount's `create`/`setxattr` handlers) calls
    /// this only after acquiring the file's exclusive lock, via
    /// `Store::create`. The one exception is the mount daemon's own
    /// startup enumeration (`Store::open`, below), which lists every
    /// `.rings` file in the directory before `fs::mount` acquires the
    /// directory's exclusive lock — the same pre-existing window
    /// `reconcile_trim` already runs in unguarded (a narrow, accepted race
    /// against a directory no other writer should be touching while a
    /// mount is starting up).
    pub fn open(dir: &Path, name: &str, cfg: &Config) -> io::Result<FileStore> {
        // A lingering .trim marker means a collapse started but never
        // finished (crash between the fallocate and the final rename);
        // reconcile it before anything below reads the trunk/rings, so
        // neither the truncation check nor a caller sees a half-landed cut.
        reconcile_trim(dir, name)?;
        // Before the rings are opened for writing: a v1 file cannot take a
        // v2 record (two strides in one file), and migrating in place after
        // the handle exists would leave that handle on the pre-rename inode.
        migrate_rings(dir, name)?;
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
        let mut next_seq = 0u64;
        let mut dropped = format::Dropped::default();
        if rings.metadata()?.len() == 0 {
            rings.write_all_at(&format::rings_header(0, format::Dropped::default()), 0)?;
        } else {
            chunks = format::read_index_file(&rings)?;
            next_seq = format::read_header_next_seq(&rings)?
                .max(chunks.last().map(|c| c.seq + 1).unwrap_or(0));
            dropped = format::read_header_dropped(&rings)?;
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

        let mut comp_size = chunks.last().map(|c| c.comp_end()).unwrap_or(0);
        let mut buffer_start = chunks.last().map(|c| c.uncomp_end()).unwrap_or(0);
        let mut buffer = Vec::new();
        let mut buffer_first_ms: Option<u64> = None;
        let mut buffer_last_ms = 0u64;

        let apply_entries = |entries: Vec<sap::SapEntry>,
                             buffer: &mut Vec<u8>,
                             first: &mut Option<u64>,
                             last: &mut u64| {
            for e in entries {
                if buffer.is_empty() {
                    *first = Some(e.wf);
                    *last = e.wl;
                } else {
                    *first = Some(first.unwrap_or(e.wf).min(e.wf));
                    *last = (*last).max(e.wl);
                }
                buffer.extend_from_slice(&e.payload);
            }
        };

        // --- sap reconciliation (see docs/design.md's ".sap" chapter for
        // the full crash matrix; this mirrors it directly) ---
        let seal_p = format::sap_seal_path(dir, name);
        let sap_p = format::sap_path(dir, name);
        if let Some(seal) = sap::replay(&seal_p)? {
            if seal.base < comp_size {
                // The flush landed (its frame is already in the trunk):
                // the seal is stale debris.
                let _ = fs::remove_file(&seal_p);
            } else {
                if seal.base > comp_size {
                    eprintln!(
                        "timberfs: {name}: .sap.seal's base ({}) exceeds the trunk's \
                         compressed size ({comp_size}) — the trunk shrank underneath an \
                         interrupted flush; replaying the sealed entries anyway \
                         (preserving data wins over tidiness)",
                        seal.base
                    );
                }
                // The flush never landed: replay through the normal append
                // path and complete it now, exactly where it would have
                // landed (comp_size is unchanged since the frame was never
                // written).
                apply_entries(
                    seal.entries,
                    &mut buffer,
                    &mut buffer_first_ms,
                    &mut buffer_last_ms,
                );
                if !buffer.is_empty() {
                    let comp = zstd::stream::encode_all(&buffer[..], cfg.level)?;
                    trunk.write_all_at(&comp, comp_size)?;
                    let rec = ChunkRecord {
                        uncomp_start: buffer_start,
                        uncomp_len: buffer.len() as u64,
                        comp_start: comp_size,
                        comp_len: comp.len() as u64,
                        first_write_ms: buffer_first_ms.unwrap_or(buffer_last_ms),
                        last_write_ms: buffer_last_ms,
                        seq: next_seq,
                    };
                    next_seq += 1;
                    let rec_off = RINGS_HEADER_LEN + (chunks.len() * RECORD_LEN) as u64;
                    rings.write_all_at(&rec.to_bytes(), rec_off)?;
                    trunk.sync_all()?;
                    rings.sync_all()?;
                    comp_size += comp.len() as u64;
                    buffer_start += buffer.len() as u64;
                    chunks.push(rec);
                    buffer.clear();
                    buffer_first_ms = None;
                    buffer_last_ms = 0;
                }
                let _ = fs::remove_file(&seal_p);
            }
        }

        let wal_declared = crate::bark::wal_declared(dir, name);
        let mut wal: Option<sap::Sap> = None;
        if let Some(live) = sap::replay(&sap_p)? {
            apply_entries(
                live.entries,
                &mut buffer,
                &mut buffer_first_ms,
                &mut buffer_last_ms,
            );
            if wal_declared {
                let mut live_sap =
                    sap::Sap::resume(&sap_p, live.base, live.uncomp_base, live.valid_len)?;
                // The header is a witness, the store is the truth: bases
                // left stale by a crash inside a refresh (append_frames or
                // a head trim) are re-stamped here, not trusted.
                if live.base != comp_size || live.uncomp_base != buffer_start {
                    eprintln!(
                        "timberfs: {name}: the sap's recorded bases ({}, {}) disagree \
                         with the store ({comp_size}, {buffer_start}); trusting the \
                         store and refreshing the header",
                        live.base, live.uncomp_base
                    );
                    live_sap.refresh_base(comp_size, buffer_start)?;
                }
                wal = Some(live_sap);
            } else {
                // "set wal=false" with a leftover sap: the entries above
                // are already folded into the buffer (preserved), so it's
                // safe to drop the file — it won't be recreated.
                let _ = fs::remove_file(&sap_p);
            }
        } else if wal_declared {
            wal = Some(sap::Sap::create(&sap_p, comp_size, buffer_start)?);
        }

        Ok(FileStore {
            dir: dir.to_path_buf(),
            name: name.to_string(),
            trunk,
            rings,
            chunks,
            comp_size,
            buffer,
            buffer_start,
            buffer_first_ms,
            buffer_last_ms,
            cache: None,
            staged: None,
            wal,
            next_seq,
            dropped,
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
        // Staged (atomic) delivery bypasses the sap entirely: nothing is
        // durable — or even visible — before commit_stage, by design, so
        // there is nothing for the wal to add. append_frames never reaches
        // here (it copies compressed frames directly, never through the
        // buffer).
        if self.staged.is_none() {
            if let Some(wal) = &mut self.wal {
                wal.append(first_ms, last_ms, data)?;
            }
        }
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
    ///
    /// When a wal is live and unstaged, this is also the sap's
    /// seal-and-swap handoff: the segment about to be superseded by this
    /// flush is sealed (renamed to `.sap.seal`) before the frame lands, so
    /// a crash between "compressed" and "indexed" is decidable on the next
    /// open (base < comp_size => landed => the seal is stale; base ==
    /// comp_size => never landed => replay it). Every flush rotates the
    /// segment, so a segment's lifetime spans exactly one chunk cycle and
    /// its eventual flush lands its frame at exactly its `base`.
    pub fn flush_chunk(&mut self, cfg: &Config) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        // Compress first: it's the step most likely to be skipped if
        // anything below fails, and it doesn't touch any on-disk state, so
        // failing here leaves the sap untouched and still live.
        let comp = zstd::stream::encode_all(&self.buffer[..], cfg.level)?;
        let sealing = self.staged.is_none() && self.wal.is_some();
        let sap_p = format::sap_path(&self.dir, &self.name);
        let seal_p = format::sap_seal_path(&self.dir, &self.name);
        if sealing {
            self.wal.as_mut().unwrap().sync()?;
            fs::rename(&sap_p, &seal_p)?;
        }
        // Before the frame + index are durable, a failure here means the
        // flush never actually happened: undo the rename so the seal is
        // live again, unchanged — exactly the state it was in before this
        // call, with a `base` that is still correct.
        let unseal_before_landed = |e: io::Error| -> io::Error {
            if sealing {
                let _ = fs::rename(&seal_p, &sap_p);
            }
            e
        };
        self.trunk
            .write_all_at(&comp, self.comp_size)
            .map_err(unseal_before_landed)?;
        let rec = ChunkRecord {
            uncomp_start: self.buffer_start,
            uncomp_len: self.buffer.len() as u64,
            comp_start: self.comp_size,
            comp_len: comp.len() as u64,
            first_write_ms: self.buffer_first_ms.unwrap_or(self.buffer_last_ms),
            last_write_ms: self.buffer_last_ms,
            seq: self.next_seq,
        };
        self.next_seq += 1;
        if self.staged.is_none() {
            let rec_off = RINGS_HEADER_LEN + (self.chunks.len() * RECORD_LEN) as u64;
            self.rings
                .write_all_at(&rec.to_bytes(), rec_off)
                .map_err(unseal_before_landed)?;
        }
        self.comp_size += comp.len() as u64;
        self.buffer_start += self.buffer.len() as u64;
        self.buffer.clear();
        self.buffer_first_ms = None;
        self.chunks.push(rec);
        if sealing {
            // The frame + index must be durable BEFORE the seal is
            // unlinked: a plain (unsynced) flush_chunk is fine for a
            // non-wal store (bounded by --flush-age, as today), but here
            // the seal is the only record of this data until the frame
            // lands, so unlinking it early on an unsynced write would be
            // able to lose data a wal store promises not to.
            self.trunk.sync_all().map_err(unseal_before_landed)?;
            self.rings.sync_all().map_err(unseal_before_landed)?;
            // The flush is now durable regardless of what happens next, so
            // a failure from here on must NOT resurrect the (now stale)
            // seal — replaying it later would re-introduce data that is
            // already safely in the trunk. Degrade instead: drop the wal
            // for this run (recovered on the next open, once the
            // transient error — almost certainly ENOSPC — has passed).
            match sap::Sap::create(&sap_p, self.comp_size, self.buffer_start) {
                Ok(fresh) => self.wal = Some(fresh),
                Err(e) => {
                    eprintln!(
                        "timberfs: {}: starting the next wal segment failed ({e}); \
                         wal durability is off for this store until it is reopened",
                        self.name
                    );
                    self.wal = None;
                }
            }
            let _ = fs::remove_file(&seal_p);
        }
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

    /// The wal's own durability point, decoupled from chunk flushing: push
    /// the sap's buffered writes to disk and fsync, WITHOUT touching the
    /// trunk/rings or the chunk-size/age schedule. A no-op when wal isn't
    /// declared, or while staged (atomic delivery bypasses the sap
    /// entirely, so there is nothing here to sync). This is the primitive
    /// a future "ack when durable" ingestion path calls per message; the
    /// 1-second maintenance loops (append.rs, sink.rs, fs.rs) call it every
    /// tick so a plain writer's crash window shrinks from `flush_age` to
    /// that tick interval.
    /// Whether a live write-ahead segment backs this file — i.e. whether
    /// `sap_sync` is a real durability point (it is a silent no-op
    /// without one, e.g. undeclared, or degraded after ENOSPC).
    pub fn has_wal(&self) -> bool {
        self.wal.is_some()
    }

    pub fn sap_sync(&mut self) -> io::Result<()> {
        if self.staged.is_some() {
            return Ok(());
        }
        match &mut self.wal {
            Some(wal) => wal.sync(),
            None => Ok(()),
        }
    }

    /// Bring the live sap in line with what the manifest declares, for
    /// `timberfs set wal=true|false` on a RUNNING writer — the same
    /// no-restart contract retention already has, and the one that
    /// matters mid-incident, when restarting a writer means restarting
    /// whatever produces its lines.
    ///
    /// Turning it on FLUSHES first: a segment's content must be exactly
    /// the next chunk's bytes (live.rs and the crash matrix both rest on
    /// that), and one started mid-buffer would describe a chunk it holds
    /// only the tail of. Turning it off keeps the buffer and drops the
    /// file — what is buffered is still flushed as usual.
    pub fn sync_wal_declaration(&mut self, declared: bool, cfg: &Config) -> io::Result<bool> {
        if self.staged.is_some() || declared == self.wal.is_some() {
            return Ok(false);
        }
        let sap_p = format::sap_path(&self.dir, &self.name);
        if declared {
            self.flush_chunk(cfg)?;
            self.wal = Some(sap::Sap::create(&sap_p, self.comp_size, self.buffer_start)?);
        } else {
            self.wal = None;
            let _ = fs::remove_file(&sap_p);
        }
        Ok(true)
    }

    /// Make what has been appended VISIBLE to a reader tailing the sap
    /// (live.rs), without the fsync `sap_sync` pays. Writers call it once
    /// per batch of appends: the batch is what a follower read in one go
    /// or what an appender got from one read(2), so this costs one
    /// syscall per batch, not per line, and it is what puts a written
    /// line in front of `query --follow` in a poll interval rather than
    /// in a flush age.
    pub fn sap_flush(&mut self) -> io::Result<()> {
        if self.staged.is_some() {
            return Ok(());
        }
        match &mut self.wal {
            Some(wal) => wal.flush(),
            None => Ok(()),
        }
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
        let _ = fs::remove_file(format::sap_seal_path(dir, name));
        if self.wal.is_some() {
            self.wal = Some(sap::Sap::create(&format::sap_path(dir, name), 0, 0)?);
        }
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
            // The incoming number is the SOURCE's position and is dropped:
            // it says where the chunk sat there, not what it is, and two
            // sources fanning in would interleave into a sequence that is
            // neither dense nor monotone. ⚠ That reason is a PRECONDITION,
            // not a law: a single source delivered in order could keep its
            // numbering (ROADMAP, "Globally addressable chunks"). Relaxing
            // it here would also mean this destination inherits the
            // source's chunk BOUNDARIES, since a number only addresses
            // anything while those hold — and rotate deliberately owns its
            // own chunking.
            let rec = ChunkRecord {
                uncomp_start: uncomp_base + (c.uncomp_start - src_uncomp_start),
                comp_start: comp_base + (c.comp_start - src_comp_start),
                seq: self.next_seq,
                ..*c
            };
            self.next_seq += 1;
            let off = RINGS_HEADER_LEN + (self.chunks.len() * RECORD_LEN) as u64;
            self.rings.write_all_at(&rec.to_bytes(), off)?;
            self.chunks.push(rec);
        }
        self.comp_size = comp_base + total_comp;
        self.buffer_start = uncomp_base + (records.last().unwrap().uncomp_end() - src_uncomp_start);
        self.cache = None;
        self.trunk.sync_all()?;
        self.rings.sync_all()?;
        // append_frames copies compressed frames directly and never
        // touches the buffer (the flush_chunk call above already emptied
        // it), so the sap itself has nothing new to record — but its base
        // headers, minted at the store's coordinates when the segment
        // was created, are now stale: both moved without a flush
        // of this segment. Refresh them so a later seal's landed/never-landed
        // comparison stays correct.
        if let Some(wal) = &mut self.wal {
            wal.refresh_base(self.comp_size, self.buffer_start)?;
        }
        Ok(())
    }

    /// Cut the first `k` chunks off this file: the remaining frames and a
    /// rebased index are written to temp files which are renamed over the
    /// originals, then the in-memory state is rebased to match. The
    /// unflushed buffer (data newer than any chunk) is untouched.
    /// The running drop total after dropping the first `k` chunks.
    ///
    /// LENGTHS, never offsets: `collapse_head` rebases survivors by the
    /// block-ALIGNED cut and leaves the sliver in `comp_start`, so summing
    /// offsets would count that sliver again on the next drop. This also
    /// makes the two head-drop paths agree, and makes the number mean "what
    /// left the store" rather than "what the filesystem reclaimed".
    fn dropped_after(&self, k: usize) -> format::Dropped {
        let gone = &self.chunks[..k];
        format::Dropped {
            chunks: self.dropped.chunks + k as u64,
            uncomp_bytes: self.dropped.uncomp_bytes
                + gone.iter().map(|c| c.uncomp_len).sum::<u64>(),
            comp_bytes: self.dropped.comp_bytes + gone.iter().map(|c| c.comp_len).sum::<u64>(),
        }
    }

    fn remove_head(&mut self, k: usize, dir: &Path, name: &str) -> io::Result<()> {
        if k == 0 {
            return Ok(());
        }
        let dropped = self.dropped_after(k);
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
            idx.extend_from_slice(&format::rings_header(self.next_seq, dropped));
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
        // Odd for the same reason as collapse_head's window, though this
        // path swaps whole inodes rather than mutating in place: the rings
        // and the positional grain are renumbered together, and a reader
        // that sampled one before and the other after would pair two
        // numberings and skip chunks. It has no .trim marker to reconcile
        // from, so a crash inside the window leaves the counter odd until
        // the next writer opens the store — the same as a crash mid-
        // collapse, and readers retry rather than read stale offsets.
        let seq0 = read_seq(dir, name);
        let _ = write_seq(dir, name, seq0 + 1);
        fs::rename(&trunk_tmp, &trunk_p)?;
        fs::rename(&rings_tmp, &rings_p)?;
        if let Err(e) = crate::grain::rebase_head(dir, name, k) {
            eprintln!(
                "timberfs: {name}: rebasing the token index after a head-drop failed \
                 ({e}); dropping it — the next write rebuilds it"
            );
            let _ = fs::remove_file(format::grain_path(dir, name));
        }
        let _ = write_seq(dir, name, seq0 + 2);
        self.trunk = OpenOptions::new().read(true).write(true).open(&trunk_p)?;
        self.rings = OpenOptions::new().read(true).write(true).open(&rings_p)?;
        self.chunks.drain(..k);
        for c in &mut self.chunks {
            c.uncomp_start -= uncomp_cut;
            c.comp_start -= comp_cut;
        }
        self.comp_size -= comp_cut;
        self.buffer_start -= uncomp_cut;
        // Only now: the staged header already carries this, and it rode the
        // same rename, so on-disk and in-memory move together.
        self.dropped = dropped;
        self.cache = None;
        // Retention/rotation touch only already-flushed chunks — the
        // buffer (and thus the sap's entries) are untouched — but this
        // just moved both coordinates out from under the sap's bases, same
        // as append_frames: refresh them.
        if let Some(wal) = &mut self.wal {
            wal.refresh_base(self.comp_size, self.buffer_start)?;
        }
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
        let dropped = self.dropped_after(k);
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
            idx.extend_from_slice(&format::rings_header(self.next_seq, dropped));
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
        // The grain is positional, so the rebased rings just renumbered
        // every filter: drop its first k records to match. INSIDE the odd
        // seqlock window, with the rings rename above — the two must never
        // be observable apart, or a reader pairs one numbering with the
        // other and skips chunks it should have read.
        if let Err(e) = crate::grain::rebase_head(dir, name, k) {
            eprintln!(
                "timberfs: {name}: rebasing the token index after a head-drop failed \
                 ({e}); dropping it — the next write rebuilds it"
            );
            let _ = fs::remove_file(format::grain_path(dir, name));
        }
        // Reset the seqlock to even BEFORE the .trim marker goes away: if
        // we die between here and the marker removal, the marker is still
        // there to make the next open's reconcile_trim finalize the
        // collapse, and a still-odd counter would otherwise wedge every
        // standalone reader until that happens. A crash before this point
        // leaves the counter odd with the marker present — reconcile_trim
        // resets it too, so the next writer open still clears it.
        if let Err(e) = write_seq(dir, name, seq0 + 2) {
            eprintln!(
                "timberfs: {name}: resetting the collapse seqlock failed ({e}); \
                 readers retry until the next collapse or reconcile"
            );
        }
        let _ = fs::remove_file(&trim_p);

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
        // Only now, as in remove_head: the staged header already carries
        // this and rode the same rename.
        self.dropped = dropped;
        self.cache = None;
        // Same reasoning as remove_head: the collapse just moved comp_size
        // without touching the buffer/sap, so the sap's `base` is stale.
        // Best-effort (like the skippable-frame stamp above): a failure
        // here only risks a spurious "external damage" warning on a very
        // narrow future crash window, never data loss (replay still wins).
        if let Some(wal) = &mut self.wal {
            if let Err(e) = wal.refresh_base(self.comp_size, self.buffer_start) {
                eprintln!(
                    "timberfs: {name}: refreshing the wal segment's base after collapse \
                     failed ({e}); harmless unless a crash lands before the next flush"
                );
            }
        }
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
    /// The chunk NUMBERS the range covered, first and last inclusive.
    /// Reported rather than derived from the count: numbering survives a
    /// head-drop, so a count says nothing about which chunks these were —
    /// and a cursor holds exactly this axis, which is what lets a loss
    /// record name a range a follower can be compared against.
    pub first_seq: u64,
    pub last_seq: u64,
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
                    files.insert(stem.to_string(), FileStore::open(dir, stem, &cfg)?);
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
            let f = FileStore::open(&self.dir, name, &self.cfg)?;
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
        let _ = fs::remove_file(format::sap_path(&self.dir, name));
        let _ = fs::remove_file(format::sap_seal_path(&self.dir, name));
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
        let _ = fs::rename(
            format::sap_path(&self.dir, old),
            format::sap_path(&self.dir, new),
        );
        let _ = fs::rename(
            format::sap_seal_path(&self.dir, old),
            format::sap_seal_path(&self.dir, new),
        );
        f.name = new.to_string();
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

    /// Called by the same 1-second maintenance tick as `flush_aged`: the
    /// wal's own durability point, independent of the chunk flush
    /// schedule — a plain wal-declared writer's power-loss window shrinks
    /// from `flush_age` to this tick interval.
    /// Apply `set wal=…` to every file this store holds; announces each
    /// change, because a writer quietly changing its durability and
    /// visibility properties is exactly what an operator needs to see
    /// having happened.
    pub fn sync_wal_declarations(&mut self) {
        let cfg = self.cfg;
        for (name, f) in self.files.iter_mut() {
            let declared = crate::bark::wal_declared(&f.dir, name);
            match f.sync_wal_declaration(declared, &cfg) {
                Ok(true) => crate::note!(
                    "timberfs: {name}: wal {} (declared in the manifest)",
                    if declared {
                        "started — new entries are visible to a live follower as they arrive"
                    } else {
                        "stopped"
                    }
                ),
                Ok(false) => {}
                Err(e) => eprintln!("timberfs: {name}: applying the wal declaration failed: {e}"),
            }
        }
    }

    pub fn sap_sync_all(&mut self) {
        for (name, f) in self.files.iter_mut() {
            if let Err(e) = f.sap_sync() {
                eprintln!("timberfs: {name}: wal sync failed: {e}");
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
                    first_seq: 0,
                    last_seq: 0,
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
            first_seq: moved.first().unwrap().seq,
            last_seq: moved.last().unwrap().seq,
        })
    }

    /// The number this store will give its NEXT chunk — so everything it
    /// has ever written is strictly below it. What the interest axis needs
    /// to call a follower's claimed position IMPOSSIBLE rather than merely
    /// suspicious.
    pub fn next_seq(&self, name: &str) -> Option<u64> {
        self.files.get(name).map(|f| f.next_seq)
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

    /// Continuous retention (the appender's --retain / --retain-size, plus
    /// the interest axis): drop head chunks older than `max_age_ms`, keep
    /// the compressed size at or under `max_comp_bytes`, and drop what
    /// `interest_droppable` says every retaining follower has consumed.
    ///
    /// The three axes combine with `max`, never `min`: each one names a
    /// prefix it would be happy to see gone, and the largest wins. For
    /// interest that is the whole design — letting it CAP the drop would
    /// let one stalled follower pin the store until the disk fills, which
    /// kills the PRODUCER, losing the newest data to protect the oldest.
    /// So a caller that cannot determine the interest floor passes `None`,
    /// and the axis simply contributes nothing.
    ///
    /// Age and size trigger with hysteresis, because dropping the head
    /// compacts the pair: age-expired data goes once it makes up at least
    /// a tenth of the file, a size overrun drops down to 95% of the
    /// budget. Interest has NONE, deliberately — promptness is the entire
    /// point of it ("what remains on the box after a successful ship is
    /// one chunk"), and the in-place collapse makes a per-chunk cut cheap.
    ///
    /// The unflushed buffer (newest data) is never touched. Returns stats
    /// when something was dropped.
    pub fn enforce_retention(
        &mut self,
        name: &str,
        max_age_ms: Option<u64>,
        max_comp_bytes: Option<u64>,
        interest_floor: Option<u64>,
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
        // Additive, and last only for readability: `max` does not care.
        // A partition over chunk NUMBERS, which is exact where a partition
        // over write windows could not be: numbers are dense and only
        // increase, and they survive a head-drop unchanged.
        if let Some(floor) = interest_floor {
            k = k.max(f.chunks.partition_point(|c| c.seq < floor));
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
            first_seq: f.chunks[0].seq,
            last_seq: f.chunks[k - 1].seq,
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
/// One non-blocking flock attempt: Ok(false) = someone else holds it.
fn flock_nb(f: &File, op: libc::c_int) -> io::Result<bool> {
    let rc = unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(e)
    }
}

fn try_flock(f: File, op: libc::c_int) -> io::Result<Option<File>> {
    Ok(flock_nb(&f, op)?.then_some(f))
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
    lock_path_exclusive(&file_lock_path(dir, name))
}

/// The same lock on an arbitrary path. The per-file helpers are this with
/// the path spelled by the store layout; the follower registry's lock
/// lives outside any backing directory and spells its own.
pub fn lock_path_exclusive(path: &Path) -> io::Result<Option<File>> {
    try_flock(open_lock_file(path)?, libc::LOCK_EX)
}

/// The same lock, retried until `timeout` runs out.
///
/// A supervised streaming writer is routinely started while the writer it
/// replaces is still exiting: Apache spawns a new piped-log program on
/// reload before the old one has drained its pipe and released the lock.
/// That handoff is a normal event, so failing on the first attempt turns
/// every reload into an error per store; a bounded wait lets it complete,
/// and a lock still held at the deadline is the real conflict it looks
/// like. Nothing is read from stdin while waiting — the kernel pipe
/// buffers for the producer, which is why the wait must stay short.
pub fn lock_file_exclusive_waiting(
    dir: &Path,
    name: &str,
    timeout: Duration,
) -> io::Result<Option<File>> {
    // The lock file is never unlinked, so one open fd serves every
    // attempt (flock is per open file description, not per path).
    let f = open_lock_file(&file_lock_path(dir, name))?;
    let deadline = Instant::now() + timeout;
    loop {
        if flock_nb(&f, libc::LOCK_EX)? {
            return Ok(Some(f));
        }
        // A stop signal during the wait is an answer too: give up now
        // rather than keeping the supervisor waiting on a lock we are
        // about to stop wanting.
        if crate::append::stopping() {
            return Ok(None);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// Who holds a file's writer lock, for the message when we cannot get it.
/// Writers record themselves in the lock file, but that text is never
/// cleared on exit — so the pid is checked against /proc rather than
/// repeated as fact.
pub fn describe_file_writer(dir: &Path, name: &str) -> Option<String> {
    describe_lock_holder(&file_lock_path(dir, name))
}

/// The same, for a lock at an arbitrary path.
pub fn describe_lock_holder(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let who = raw.lines().next()?.trim();
    if who.is_empty() {
        return None;
    }
    let Some(pid) = who
        .rsplit_once("pid=")
        .and_then(|(_, p)| p.trim().parse::<u32>().ok())
    else {
        return Some(who.to_string());
    };
    match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw) if !raw.is_empty() => {
            let cmd: Vec<String> = raw
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            Some(format!("{who} ({})", cmd.join(" ")))
        }
        // Gone, or a process we may not inspect: say which, and never
        // pin the blame on a pid that has been recycled.
        _ if !Path::new(&format!("/proc/{pid}")).exists() => Some(format!(
            "{who}, but that process is gone — the live holder did not record itself"
        )),
        _ => Some(who.to_string()),
    }
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
    probe_path_exclusive(&file_lock_path(dir, name))
}

/// Read-only probe: is a live holder on the exclusive lock at `path`?
/// The arbitrary-path form, for locks outside a backing directory.
pub fn probe_path_exclusive(path: &Path) -> LockProbe {
    probe_lock(path, libc::LOCK_EX)
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
    fn a_recorded_writer_is_named_only_while_it_lives() {
        let dir = std::env::temp_dir().join(format!("tfs-holder-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        // Our own pid: the description names the process behind it.
        fs::write(
            file_lock_path(&dir, "a.log"),
            format!("appender pid={}\n", std::process::id()),
        )
        .unwrap();
        let live = describe_file_writer(&dir, "a.log").unwrap();
        assert!(live.starts_with("appender pid="), "{live}");
        assert!(live.contains('('), "the live holder's command line: {live}");

        // A pid that cannot exist: the lock file's word is not taken for
        // it — the text is never cleared on exit, so it goes stale.
        fs::write(file_lock_path(&dir, "b.log"), "appender pid=999999999\n").unwrap();
        let stale = describe_file_writer(&dir, "b.log").unwrap();
        assert!(stale.contains("that process is gone"), "{stale}");

        // No lock file at all, and one that records nothing: no claim.
        assert!(describe_file_writer(&dir, "c.log").is_none());
        fs::write(file_lock_path(&dir, "d.log"), "").unwrap();
        assert!(describe_file_writer(&dir, "d.log").is_none());

        fs::remove_dir_all(&dir).ok();
    }

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
        // Simulate a crash after the writer bumped the seqlock odd but
        // before the fallocate — bug B: rollback must clear it too.
        write_seq(dir.path(), name, 7).unwrap();

        reconcile_trim(dir.path(), name).unwrap();

        assert!(!format::trim_path(dir.path(), name).exists());
        assert!(!rings_tmp.exists());
        assert_eq!(
            fs::read(format::rings_path(dir.path(), name)).unwrap(),
            b"old-rings"
        );
        assert_eq!(
            read_seq(dir.path(), name) % 2,
            0,
            "seqlock left odd after rollback"
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
        // Simulate a crash after the fallocate landed but before the
        // seqlock reset — bug B: roll-forward must clear it too.
        write_seq(dir.path(), name, 5).unwrap();

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
        assert_eq!(
            read_seq(dir.path(), name) % 2,
            0,
            "seqlock left odd after roll-forward"
        );
    }

    #[test]
    fn reconcile_trim_rolls_forward_when_the_rename_already_committed() {
        // Bug A: a crash between the rename (rings.tmp -> rings) and the
        // .trim removal leaves rings.tmp already gone and rings already
        // holding the rebased index. The old code unconditionally
        // re-ran the rename and errored with NotFound, making the store
        // unopenable; reconcile_trim must treat this as "already done".
        let dir = TempDir::new();
        let name = "app";
        let sliver = 20u64;
        let mut trunk_bytes = vec![0xCDu8; sliver as usize];
        trunk_bytes.extend_from_slice(&zstd::stream::encode_all(&b"kept\n"[..], 3).unwrap());
        fs::write(format::trunk_path(dir.path(), name), &trunk_bytes).unwrap();
        // rings already holds the rebased (post-rename) content; no
        // rings.tmp left behind, no stale grain either (already removed).
        fs::write(format::rings_path(dir.path(), name), b"rebased-rings").unwrap();
        let pre_comp_size = trunk_bytes.len() as u64 + 40;
        write_trim_marker(
            &format::trim_path(dir.path(), name),
            pre_comp_size,
            40,
            sliver,
        )
        .unwrap();
        write_seq(dir.path(), name, 5).unwrap();

        reconcile_trim(dir.path(), name).unwrap();

        assert!(!format::trim_path(dir.path(), name).exists());
        assert_eq!(
            fs::read(format::rings_path(dir.path(), name)).unwrap(),
            b"rebased-rings"
        );
        let all = fs::read(format::trunk_path(dir.path(), name)).unwrap();
        assert_eq!(zstd::stream::decode_all(&all[..]).unwrap(), b"kept\n");
        assert_eq!(
            read_seq(dir.path(), name) % 2,
            0,
            "seqlock left odd after reconcile"
        );
    }

    #[test]
    fn reconcile_trim_ignores_an_orphan_staged_rings_with_no_marker() {
        // Crash window 1: the writer staged rings.tmp but died before
        // writing the .trim marker — nothing was ever committed, so
        // there's no marker to reconcile. The orphan rings.tmp is
        // harmless: a later collapse truncates and reuses it, and
        // FileStore::open never looks at it.
        let dir = TempDir::new();
        let name = "app";
        fs::write(format::trunk_path(dir.path(), name), vec![0u8; 100]).unwrap();
        fs::write(format::rings_path(dir.path(), name), b"old-rings").unwrap();
        let rings_tmp = dir.path().join(format!("{name}.{}.tmp", format::RINGS_EXT));
        fs::write(&rings_tmp, b"staged-rings").unwrap();

        reconcile_trim(dir.path(), name).unwrap();

        // No marker means reconcile_trim is a no-op: the orphan is left
        // for the next collapse attempt to overwrite.
        assert!(rings_tmp.exists());
        assert_eq!(
            fs::read(format::rings_path(dir.path(), name)).unwrap(),
            b"old-rings"
        );
    }

    #[test]
    fn reconcile_trim_is_a_noop_with_no_marker() {
        let dir = TempDir::new();
        // No .trim file at all: nothing to reconcile, no error.
        reconcile_trim(dir.path(), "app").unwrap();
    }

    // --- .sap / wal integration ---

    fn test_cfg() -> Config {
        // A large chunk_size so appends in these tests never auto-flush
        // unless the test calls flush_chunk itself.
        Config {
            chunk_size: 1 << 20,
            level: 1,
            flush_age_ms: u64::MAX,
        }
    }

    /// Build a one-chunk trunk+rings pair from scratch, bypassing
    /// FileStore entirely — a fixture for the seal-reconcile matrix below,
    /// which needs full control over `comp_size` independent of any sap.
    fn write_one_chunk(
        dir: &Path,
        name: &str,
        data: &[u8],
        first_ms: u64,
        last_ms: u64,
    ) -> ChunkRecord {
        let comp = zstd::stream::encode_all(data, 1).unwrap();
        fs::write(format::trunk_path(dir, name), &comp).unwrap();
        let rec = ChunkRecord {
            uncomp_start: 0,
            uncomp_len: data.len() as u64,
            comp_start: 0,
            comp_len: comp.len() as u64,
            first_write_ms: first_ms,
            last_write_ms: last_ms,
            seq: 0,
        };
        let mut idx = Vec::new();
        idx.extend_from_slice(&format::rings_header(1, format::Dropped::default()));
        idx.extend_from_slice(&rec.to_bytes());
        fs::write(format::rings_path(dir, name), &idx).unwrap();
        rec
    }

    fn write_empty_pair(dir: &Path, name: &str) {
        fs::write(
            format::rings_path(dir, name),
            format::rings_header(0, format::Dropped::default()),
        )
        .unwrap();
        fs::write(format::trunk_path(dir, name), []).unwrap();
    }

    #[test]
    fn open_without_wal_declared_creates_no_sap() {
        let dir = TempDir::new();
        let f = FileStore::open(dir.path(), "app", &test_cfg()).unwrap();
        assert!(f.wal.is_none());
        assert!(!format::sap_path(dir.path(), "app").exists());
    }

    #[test]
    fn open_creates_a_fresh_sap_when_wal_is_declared() {
        let dir = TempDir::new();
        crate::bark::declare_wal(dir.path(), "app").unwrap();
        let f = FileStore::open(dir.path(), "app", &test_cfg()).unwrap();
        assert!(f.wal.is_some());
        let replayed = sap::replay(&format::sap_path(dir.path(), "app"))
            .unwrap()
            .unwrap();
        assert_eq!(replayed.base, 0);
        assert!(replayed.entries.is_empty());
    }

    #[test]
    fn flush_seals_and_rotates_the_sap() {
        let dir = TempDir::new();
        let name = "app";
        crate::bark::declare_wal(dir.path(), name).unwrap();
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        f.append_windowed(b"hello\n", 100, 100, &cfg).unwrap();
        // append_windowed only mirrors into the sap's BufWriter; sap_sync
        // (the actual durability point) is what pushes it to the OS and
        // makes it visible to an independent fs::read.
        f.sap_sync().unwrap();

        let sap_p = format::sap_path(dir.path(), name);
        let before = sap::replay(&sap_p).unwrap().unwrap();
        assert_eq!(before.entries.len(), 1);
        assert_eq!(before.base, 0);

        f.flush_chunk(&cfg).unwrap();

        assert!(!format::sap_seal_path(dir.path(), name).exists());
        let after = sap::replay(&sap_p).unwrap().unwrap();
        assert!(after.entries.is_empty());
        assert_eq!(after.base, f.comp_size);
        assert!(f.comp_size > 0);
    }

    #[test]
    fn seal_reconcile_landed_just_removes_the_seal() {
        let dir = TempDir::new();
        let name = "app";
        write_one_chunk(dir.path(), name, b"hello\n", 10, 10);
        crate::bark::declare_wal(dir.path(), name).unwrap();
        // base(0) < the trunk's actual comp_size => the flush landed.
        let mut sap = sap::Sap::create(&format::sap_path(dir.path(), name), 0, 0).unwrap();
        sap.append(10, 10, b"hello\n").unwrap();
        sap.sync().unwrap();
        drop(sap);
        let seal_p = format::sap_seal_path(dir.path(), name);
        fs::rename(format::sap_path(dir.path(), name), &seal_p).unwrap();

        let f = FileStore::open(dir.path(), name, &test_cfg()).unwrap();
        assert!(!seal_p.exists());
        assert_eq!(
            f.chunks.len(),
            1,
            "the seal's entries must not be re-flushed"
        );
        assert!(f.buffer.is_empty());
        // A fresh sap must be live at the current comp_size.
        let fresh = sap::replay(&format::sap_path(dir.path(), name))
            .unwrap()
            .unwrap();
        assert!(fresh.entries.is_empty());
        assert_eq!(fresh.base, f.comp_size);
    }

    #[test]
    fn seal_reconcile_never_landed_replays_and_completes_the_flush() {
        let dir = TempDir::new();
        let name = "app";
        write_empty_pair(dir.path(), name);
        crate::bark::declare_wal(dir.path(), name).unwrap();
        // base == comp_size (both 0): the flush was staged but never landed.
        let mut sap = sap::Sap::create(&format::sap_path(dir.path(), name), 0, 0).unwrap();
        sap.append(5, 5, b"abc").unwrap();
        sap.append(6, 7, b"def").unwrap();
        sap.sync().unwrap();
        drop(sap);
        let seal_p = format::sap_seal_path(dir.path(), name);
        fs::rename(format::sap_path(dir.path(), name), &seal_p).unwrap();

        let f = FileStore::open(dir.path(), name, &test_cfg()).unwrap();
        assert!(!seal_p.exists());
        assert_eq!(f.chunks.len(), 1, "the interrupted flush must be completed");
        assert_eq!(f.chunks[0].first_write_ms, 5);
        assert_eq!(f.chunks[0].last_write_ms, 7);
        assert!(f.buffer.is_empty());
        assert_eq!(f.size(), 6);
        let comp = fs::read(format::trunk_path(dir.path(), name)).unwrap();
        assert_eq!(zstd::stream::decode_all(&comp[..]).unwrap(), b"abcdef");
        let fresh = sap::replay(&format::sap_path(dir.path(), name))
            .unwrap()
            .unwrap();
        assert!(fresh.entries.is_empty());
        assert_eq!(fresh.base, f.comp_size);
    }

    #[test]
    fn seal_reconcile_shrank_still_replays_preserving_data_over_tidiness() {
        let dir = TempDir::new();
        let name = "app";
        write_empty_pair(dir.path(), name);
        crate::bark::declare_wal(dir.path(), name).unwrap();
        // base(100) > comp_size(0): the trunk shrank underneath the seal
        // (external damage) — still replay rather than lose the entries.
        let mut sap = sap::Sap::create(&format::sap_path(dir.path(), name), 100, 0).unwrap();
        sap.append(1, 1, b"x").unwrap();
        sap.sync().unwrap();
        drop(sap);
        let seal_p = format::sap_seal_path(dir.path(), name);
        fs::rename(format::sap_path(dir.path(), name), &seal_p).unwrap();

        let f = FileStore::open(dir.path(), name, &test_cfg()).unwrap();
        assert!(!seal_p.exists());
        assert_eq!(f.chunks.len(), 1);
        assert_eq!(f.size(), 1);
    }

    #[test]
    fn replay_rebuilds_the_buffer_byte_identical_to_an_uncrashed_run() {
        let crashed = TempDir::new();
        let baseline = TempDir::new();
        let name = "app";
        crate::bark::declare_wal(crashed.path(), name).unwrap();
        crate::bark::declare_wal(baseline.path(), name).unwrap();
        let cfg = test_cfg();

        {
            let mut f = FileStore::open(crashed.path(), name, &cfg).unwrap();
            f.append_windowed(b"line one\n", 10, 10, &cfg).unwrap();
            f.append_windowed(b"line two\n", 20, 25, &cfg).unwrap();
            // Durable up to here (fsynced), then "crash": dropped with no
            // chunk flush — the buffer only ever lived in memory and the
            // sap.
            f.sap_sync().unwrap();
        }
        {
            let mut g = FileStore::open(baseline.path(), name, &cfg).unwrap();
            g.append_windowed(b"line one\n", 10, 10, &cfg).unwrap();
            g.append_windowed(b"line two\n", 20, 25, &cfg).unwrap();
            g.flush_chunk(&cfg).unwrap();
        }

        let mut f2 = FileStore::open(crashed.path(), name, &cfg).unwrap();
        assert_eq!(f2.buffer, b"line one\nline two\n");
        assert_eq!(f2.buffer_first_ms, Some(10));
        assert_eq!(f2.buffer_last_ms, 25);
        f2.flush_chunk(&cfg).unwrap();

        assert_eq!(
            fs::read(format::trunk_path(crashed.path(), name)).unwrap(),
            fs::read(format::trunk_path(baseline.path(), name)).unwrap(),
        );
        assert_eq!(
            fs::read(format::rings_path(crashed.path(), name)).unwrap(),
            fs::read(format::rings_path(baseline.path(), name)).unwrap(),
        );
    }

    #[test]
    fn undeclaring_wal_replays_then_deletes_the_leftover_sap() {
        let dir = TempDir::new();
        let name = "app";
        crate::bark::declare_wal(dir.path(), name).unwrap();
        let cfg = test_cfg();
        {
            let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
            f.append_windowed(b"still here\n", 1, 1, &cfg).unwrap();
            f.sap_sync().unwrap();
            // "crash": dropped with no chunk flush.
        }

        // `timberfs set wal=false`.
        let mut map = crate::bark::load(dir.path(), name).unwrap();
        map.insert("wal".to_string(), serde_json::Value::Bool(false));
        crate::bark::save(dir.path(), name, &map).unwrap();

        let f2 = FileStore::open(dir.path(), name, &cfg).unwrap();
        assert!(f2.wal.is_none());
        assert!(!format::sap_path(dir.path(), name).exists());
        assert_eq!(
            f2.buffer, b"still here\n",
            "data must survive undeclaring wal"
        );
    }

    #[test]
    fn staged_appends_never_touch_the_sap() {
        let dir = TempDir::new();
        let name = "app";
        crate::bark::declare_wal(dir.path(), name).unwrap();
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        f.stage();
        f.append_windowed(b"staged\n", 1, 1, &cfg).unwrap();
        f.flush_chunk(&cfg).unwrap();
        // Staged delivery bypasses the sap entirely: the live segment
        // (still base=0, no entries) must be untouched by any of this.
        let sap_p = format::sap_path(dir.path(), name);
        let replayed = sap::replay(&sap_p).unwrap().unwrap();
        assert_eq!(replayed.base, 0);
        assert!(replayed.entries.is_empty());
        assert!(!format::sap_seal_path(dir.path(), name).exists());
        f.commit_stage(&cfg).unwrap();
    }

    #[test]
    fn append_frames_refreshes_the_sap_base() {
        let dir = TempDir::new();
        let src_dir = TempDir::new();
        let name = "app";
        let rec = write_one_chunk(src_dir.path(), "src", b"verbatim\n", 5, 5);
        let src_trunk = File::open(format::trunk_path(src_dir.path(), "src")).unwrap();

        crate::bark::declare_wal(dir.path(), name).unwrap();
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        assert_eq!(f.wal.as_ref().unwrap().base(), 0);

        f.append_frames(&src_trunk, &[rec], &cfg).unwrap();

        // append_frames bumps both coordinates directly (never through the
        // buffer), so the sap's bases — stale at 0 — must be refreshed to
        // match, or a later seal's landed/never-landed comparison lies.
        assert_eq!(f.wal.as_ref().unwrap().base(), f.comp_size);
        assert_eq!(f.wal.as_ref().unwrap().uncomp_base(), f.buffer_start);
        assert!(f.comp_size > 0);
    }

    /// Rewrite a store's rings in the pre-chunk-number layout, so the
    /// migration can be tested against a file it did not write. Works by
    /// truncation because `seq` is the LAST field of a record — which is
    /// also what makes the real migration a per-record append of 8 bytes.
    fn downgrade_rings_to_v1(dir: &Path, name: &str) {
        let recs = format::read_index(&format::rings_path(dir, name)).unwrap();
        let mut idx = Vec::new();
        idx.extend_from_slice(format::RINGS_MAGIC_V1);
        for c in &recs {
            idx.extend_from_slice(&c.to_bytes()[..format::RECORD_LEN_V1]);
        }
        fs::write(format::rings_path(dir, name), &idx).unwrap();
    }

    fn write_n_chunks(f: &mut FileStore, cfg: &Config, n: u64) {
        for i in 0..n {
            f.append_windowed(format!("line {i}\n").as_bytes(), 100 + i, 100 + i, cfg)
                .unwrap();
            f.flush_chunk(cfg).unwrap();
        }
    }

    #[test]
    fn chunk_numbers_are_dense_and_start_at_zero() {
        let dir = TempDir::new();
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), "app", &cfg).unwrap();
        write_n_chunks(&mut f, &cfg, 4);
        assert_eq!(
            f.chunks.iter().map(|c| c.seq).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
    }

    /// A Store holding one FileStore, for the whole-store retention API.
    fn store_with(dir: &Path, name: &str, chunks: u64) -> Store {
        let cfg = test_cfg();
        let mut st = Store {
            dir: dir.to_path_buf(),
            cfg,
            files: BTreeMap::new(),
        };
        st.create(name).unwrap();
        let f = st.files.get_mut(name).unwrap();
        write_n_chunks(f, &cfg, chunks);
        st
    }

    #[test]
    fn dropped_bytes_accumulate_and_survive_a_reopen() {
        let dir = TempDir::new();
        let name = "app";
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        write_n_chunks(&mut f, &cfg, 5);
        let gone: u64 = f.chunks[..2].iter().map(|c| c.comp_len).sum();
        let gone_u: u64 = f.chunks[..2].iter().map(|c| c.uncomp_len).sum();
        assert_eq!(f.dropped, format::Dropped::default(), "nothing yet");

        f.remove_head(2, dir.path(), name).unwrap();
        assert_eq!(f.dropped.chunks, 2);
        assert_eq!(f.dropped.comp_bytes, gone);
        assert_eq!(f.dropped.uncomp_bytes, gone_u);

        // A second drop ADDS, and the offsets having been rebased must not
        // disturb it — the totals are sums of LENGTHS, not of offsets.
        let gone2: u64 = f.chunks[..1].iter().map(|c| c.comp_len).sum();
        f.remove_head(1, dir.path(), name).unwrap();
        assert_eq!(f.dropped.chunks, 3);
        assert_eq!(f.dropped.comp_bytes, gone + gone2);

        // And it is on disk, not just in memory: the header rode the same
        // rename as the records.
        let before = f.dropped;
        drop(f);
        let f = FileStore::open(dir.path(), name, &cfg).unwrap();
        assert_eq!(f.dropped, before);
    }

    #[test]
    fn a_store_that_never_dropped_records_nothing() {
        let dir = TempDir::new();
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), "app", &cfg).unwrap();
        write_n_chunks(&mut f, &cfg, 3);
        drop(f);
        let f = FileStore::open(dir.path(), "app", &cfg).unwrap();
        // Zero here is genuine, and the oldest surviving number being 0 is
        // what tells it apart from a header that predates the counters.
        assert_eq!(f.dropped, format::Dropped::default());
        assert_eq!(f.chunks[0].seq, 0);
    }

    #[test]
    fn a_header_without_the_counters_reads_as_absent_not_as_zero() {
        // A v1 rings file, and a v2 one truncated to just its next_seq: both
        // carry no counters, and `header_dropped` must say so rather than
        // read whatever bytes happen to be there.
        assert_eq!(format::header_dropped(&[]), format::Dropped::default());
        let short = &format::rings_header(7, format::Dropped::default())[..32];
        assert_eq!(format::header_dropped(short), format::Dropped::default());
        // A full header round-trips.
        let d = format::Dropped {
            chunks: 4200,
            uncomp_bytes: 9_000_000,
            comp_bytes: 600_000,
        };
        let h = format::rings_header(4831, d);
        assert_eq!(format::header_dropped(&h), d);
        // ...and the field it shares the header with is untouched.
        assert_eq!(format::header_next_seq(&h), 4831);
    }

    #[test]
    fn interest_is_additive_never_a_cap() {
        // The rule the whole axis rests on. Letting interest CAP the drop
        // would let one stalled follower pin the store until the disk
        // fills, which kills the PRODUCER — losing the newest data to
        // protect the oldest, strictly the worse trade.
        let dir = TempDir::new();
        let mut st = store_with(dir.path(), "app", 6);
        let comp = st.files.get("app").unwrap().comp_size;

        // A budget that demands more than the follower's position allows:
        // the budget wins, because `max` and not `min`.
        let stats = st
            .enforce_retention("app", None, Some(comp / 6), Some(1))
            .unwrap()
            .expect("the budget forces a drop");
        assert!(
            stats.chunks_moved > 1,
            "interest capped the size axis at 1 chunk: {} moved",
            stats.chunks_moved
        );
        assert_eq!(stats.first_seq, 0, "and it names the range it took");
    }

    #[test]
    fn interest_alone_drops_the_consumed_prefix() {
        let dir = TempDir::new();
        let mut st = store_with(dir.path(), "app", 5);

        // No age, no budget: interest is the only axis with an opinion,
        // and it drops exactly what is below the floor.
        let stats = st
            .enforce_retention("app", None, None, Some(3))
            .unwrap()
            .expect("chunks 0..2 are consumed");
        assert_eq!(stats.chunks_moved, 3);
        assert_eq!((stats.first_seq, stats.last_seq), (0, 2));
        assert_eq!(
            st.files
                .get("app")
                .unwrap()
                .chunks
                .iter()
                .map(|c| c.seq)
                .collect::<Vec<_>>(),
            [3, 4],
            "the follower's own chunk stays: `n` counts inside it"
        );
        // Idempotent — the floor has not moved, so neither does the head.
        assert!(st
            .enforce_retention("app", None, None, Some(3))
            .unwrap()
            .is_none());
    }

    #[test]
    fn no_floor_means_no_interest_drop_at_all() {
        // Every fail-closed case arrives here as `None`, and it must be
        // inert rather than clamping: age and size go on working.
        let dir = TempDir::new();
        let mut st = store_with(dir.path(), "app", 4);
        assert!(st
            .enforce_retention("app", None, None, None)
            .unwrap()
            .is_none());
        // A floor of 0 is the same statement: nothing is below chunk 0.
        assert!(st
            .enforce_retention("app", None, None, Some(0))
            .unwrap()
            .is_none());
        assert_eq!(st.files.get("app").unwrap().chunks.len(), 4);
    }

    #[test]
    fn interest_has_no_hysteresis_where_age_does() {
        // Promptness IS the point of this axis — "what remains on the box
        // after a successful ship is one chunk" — so a single consumed
        // chunk goes at once, where the age axis deliberately waits until
        // expired data is a tenth of the file.
        let dir = TempDir::new();
        let mut st = store_with(dir.path(), "app", 20);
        let stats = st
            .enforce_retention("app", None, None, Some(1))
            .unwrap()
            .expect("one consumed chunk is enough");
        assert_eq!(stats.chunks_moved, 1);
        assert_eq!((stats.first_seq, stats.last_seq), (0, 0));
    }

    #[test]
    fn dropped_chunk_numbers_are_reported_not_inferred() {
        // A count says nothing about WHICH chunks went: numbering survives
        // a head-drop, so after one the record at index 0 is not chunk 0.
        // The loss record compares against a cursor, which holds numbers.
        let dir = TempDir::new();
        let mut st = store_with(dir.path(), "app", 6);
        st.enforce_retention("app", None, None, Some(2))
            .unwrap()
            .unwrap();
        let stats = st
            .enforce_retention("app", None, None, Some(4))
            .unwrap()
            .expect("two more are consumed now");
        assert_eq!(stats.chunks_moved, 2);
        assert_eq!(
            (stats.first_seq, stats.last_seq),
            (2, 3),
            "the numbers the survivors actually carry, not 0..1"
        );
    }

    #[test]
    fn chunk_numbers_survive_a_head_drop() {
        let dir = TempDir::new();
        let name = "app";
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        write_n_chunks(&mut f, &cfg, 4);

        f.remove_head(2, dir.path(), name).unwrap();

        // The numbering does not slide down: the survivors keep the names a
        // cursor already holds. This is the whole reason the number is
        // stored rather than derived from the record's position.
        assert_eq!(f.chunks.iter().map(|c| c.seq).collect::<Vec<_>>(), [2, 3]);
        // And it is durable, not just in memory.
        drop(f);
        let f = FileStore::open(dir.path(), name, &cfg).unwrap();
        assert_eq!(f.chunks.iter().map(|c| c.seq).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(f.next_seq, 4, "the next chunk continues past the survivors");
    }

    #[test]
    fn numbering_does_not_restart_when_retention_empties_the_store() {
        let dir = TempDir::new();
        let name = "app";
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        write_n_chunks(&mut f, &cfg, 3);

        // Retention is allowed to drop every chunk. With no record left to
        // read the next number from, a store that renumbered from 0 here
        // would hand a fresh chunk a number some cursor counts as consumed
        // — which is silent data loss, so the header carries a high-water
        // mark for exactly this case.
        f.remove_head(3, dir.path(), name).unwrap();
        assert!(f.chunks.is_empty());
        drop(f);

        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        assert!(f.chunks.is_empty(), "reopened empty");
        assert_eq!(f.next_seq, 3, "recovered from the header, not from records");
        write_n_chunks(&mut f, &cfg, 1);
        assert_eq!(f.chunks[0].seq, 3, "continues rather than reusing 0");
    }

    #[test]
    fn a_v1_index_reads_with_synthesized_numbers() {
        let dir = TempDir::new();
        let name = "app";
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        write_n_chunks(&mut f, &cfg, 3);
        drop(f);
        downgrade_rings_to_v1(dir.path(), name);

        let buf = fs::read(format::rings_path(dir.path(), name)).unwrap();
        let (recs, ver) = format::parse_index_versioned(&buf).unwrap();
        assert_eq!(ver, format::RingsVersion::V1);
        assert_eq!(recs.len(), 3);
        // A reader needs no migration: the oldest surviving record is 0 by
        // definition, which is what the migration will write too.
        assert_eq!(recs.iter().map(|c| c.seq).collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn a_writer_migrates_a_v1_index_on_open() {
        let dir = TempDir::new();
        let name = "app";
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        write_n_chunks(&mut f, &cfg, 3);
        let before: Vec<(u64, u64)> = f
            .chunks
            .iter()
            .map(|c| (c.uncomp_start, c.comp_start))
            .collect();
        drop(f);
        downgrade_rings_to_v1(dir.path(), name);

        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        let buf = fs::read(format::rings_path(dir.path(), name)).unwrap();
        assert_eq!(&buf[..8], format::RINGS_MAGIC, "migrated in place");
        assert_eq!(format::header_next_seq(&buf), 3);
        assert_eq!(
            f.chunks.iter().map(|c| c.seq).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        // Everything else about the records is untouched — the migration
        // adds a field, it does not recompute the index.
        let after: Vec<(u64, u64)> = f
            .chunks
            .iter()
            .map(|c| (c.uncomp_start, c.comp_start))
            .collect();
        assert_eq!(before, after);
        // And the store keeps working: appending continues the numbering
        // rather than colliding with a migrated record.
        write_n_chunks(&mut f, &cfg, 1);
        assert_eq!(f.chunks.last().unwrap().seq, 3);
    }

    #[test]
    fn migrating_is_idempotent_and_leaves_v2_alone() {
        let dir = TempDir::new();
        let name = "app";
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        write_n_chunks(&mut f, &cfg, 2);
        f.remove_head(1, dir.path(), name).unwrap();
        drop(f);
        let before = fs::read(format::rings_path(dir.path(), name)).unwrap();

        migrate_rings(dir.path(), name).unwrap();
        migrate_rings(dir.path(), name).unwrap();

        let after = fs::read(format::rings_path(dir.path(), name)).unwrap();
        assert_eq!(before, after, "a v2 index is not rewritten");
        // Crucially the high-water mark of a store that HAS dropped its
        // head is not reset by a migration pass over it.
        assert_eq!(format::header_next_seq(&after), 2);
    }

    #[test]
    fn appended_frames_are_renumbered_by_the_destination() {
        let dir = TempDir::new();
        let cfg = test_cfg();
        let mut src = FileStore::open(dir.path(), "src", &cfg).unwrap();
        write_n_chunks(&mut src, &cfg, 3);
        src.remove_head(2, dir.path(), "src").unwrap();
        assert_eq!(src.chunks[0].seq, 2, "the source's own numbering");

        let mut dst = FileStore::open(dir.path(), "dst", &cfg).unwrap();
        dst.append_windowed(b"local\n", 1, 1, &cfg).unwrap();
        dst.flush_chunk(&cfg).unwrap();
        let src_trunk = fs::File::open(format::trunk_path(dir.path(), "src")).unwrap();
        let records = src.chunks.clone();
        dst.append_frames(&src_trunk, &records, &cfg).unwrap();

        // The incoming number said where the chunk sat in `src`; here it
        // gets this store's next number. Otherwise a fan-in would produce a
        // sequence that is neither dense nor monotone.
        assert_eq!(dst.chunks.iter().map(|c| c.seq).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn remove_head_refreshes_the_sap_base() {
        let dir = TempDir::new();
        let name = "app";
        crate::bark::declare_wal(dir.path(), name).unwrap();
        let cfg = test_cfg();
        let mut f = FileStore::open(dir.path(), name, &cfg).unwrap();
        f.append_windowed(b"first\n", 1, 1, &cfg).unwrap();
        f.flush_chunk(&cfg).unwrap();
        f.append_windowed(b"second\n", 2, 2, &cfg).unwrap();
        f.flush_chunk(&cfg).unwrap();
        assert_eq!(f.chunks.len(), 2);

        f.remove_head(1, dir.path(), name).unwrap();

        assert_eq!(f.chunks.len(), 1);
        assert_eq!(
            f.wal.as_ref().unwrap().base(),
            f.comp_size,
            "the sap's base must track comp_size after a head trim"
        );
        assert_eq!(
            f.wal.as_ref().unwrap().uncomp_base(),
            f.buffer_start,
            "the sap's logical base must track buffer_start after a head trim"
        );
    }
}
