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
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::format::{self, ChunkRecord, RECORD_LEN, RINGS_HEADER_LEN};

fn invalid_input(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
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
        let trunk = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(format::trunk_path(dir, name))?;
        let rings = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(format::rings_path(dir, name))?;

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
        {
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
                let ks = f
                    .chunks
                    .partition_point(|c| f.comp_size - c.comp_start > low_water);
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
        f.remove_head(k, &self.dir, name)?;
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
