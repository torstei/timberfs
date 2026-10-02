//! `.grain`: the per-chunk token index — one Bloom filter per chunk over
//! every token in it, enabling `query --has TOKEN` to skip chunks that
//! definitely don't mention something (the killer case: finding a unique
//! identifier with no known time range).
//!
//! Config-free by design: tokens are ASCII-alphanumeric runs of 3..=64
//! bytes, exact case, deduplicated per chunk. Rare tokens (request keys,
//! message ids, small tenants, ERROR in a healthy log) skip almost every
//! chunk; ubiquitous tokens skip nothing and cost only the test. Filters
//! are sized at ~10 bits per distinct token with k=7 hashes: ~1% false
//! positives, and a false positive costs one needless chunk decompression.
//!
//! This is a sidecar under the contract in the README: derived and
//! rebuildable (`timberfs reindex`), and a chunk without an entry means
//! "scan it". A rings rewrite renumbers chunks, so it must not leave the
//! file as it was: a head-drop (retention, rotation's source) rebases it
//! to match, anything else deletes it.
//!
//! On disk: magic "GRAIN001", 16-byte header carrying the tokenizer and
//! hash parameters, then per chunk (in rings order): u32 LE filter length
//! in bytes, followed by the filter bits. Hashing is two-seed FNV-1a with
//! Kirsch-Mitzenmacher double hashing — dependency-free and stable.
//!
//! A record carries no chunk id: its POSITION is the chunk index. That is
//! what makes appending a chunk cost one appended record, and what makes a
//! retention head-drop hostile — see `rebase_head`.
//!
//! The file states neither how many records it holds nor where the last
//! one ends, so every write would have to read it whole to find out. The
//! `.grain.commit` beside it records both after each completed write,
//! bound to the grain's inode and the bytes at its two ends: a grain
//! replaced or rebased by anyone else no longer matches and is walked
//! once instead. Whatever lies past the committed end is a torn write and
//! is cut before the next append.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use anyhow::{bail, Context};

use crate::format::{self};
use crate::store;

pub const GRAIN_MAGIC: &[u8; 8] = b"GRAIN001";
/// As GRAIN001, but `header[12..16]` holds the byte offset of the first
/// record, because a head-drop collapsed whole blocks off the front and
/// left dead bytes behind them (`rebase_head`). Written ONLY there, so a
/// grain that has never been head-dropped stays GRAIN001 byte for byte
/// and an older binary keeps using it. Reading one of these with a
/// GRAIN001-only binary fails the magic check, which means "no index,
/// scan the chunks" — slower, never wrong.
pub const GRAIN_MAGIC_V2: &[u8; 8] = b"GRAIN002";
const HEADER_LEN: usize = 16;
const K: u64 = 7;
const MIN_TOKEN: usize = 3;
const MAX_TOKEN: usize = 64;
/// ~1% false positives at k=7.
const BITS_PER_TOKEN: u64 = 10;

fn fnv1a(seed: u64, data: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64 ^ seed.wrapping_mul(0x100000001b3);
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn bit_positions(token: &[u8], m_bits: u64) -> impl Iterator<Item = u64> + '_ {
    let h1 = fnv1a(0, token);
    let h2 = fnv1a(0x9e3779b97f4a7c15, token) | 1;
    (0..K).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) % m_bits)
}

/// Distinct ASCII-alphanumeric runs of MIN..=MAX bytes.
fn tokenize(data: &[u8]) -> HashSet<&[u8]> {
    let mut out = HashSet::new();
    let mut start: Option<usize> = None;
    for (i, &b) in data.iter().enumerate() {
        if b.is_ascii_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            if (MIN_TOKEN..=MAX_TOKEN).contains(&(i - s)) {
                out.insert(&data[s..i]);
            }
        }
    }
    if let Some(s) = start {
        if (MIN_TOKEN..=MAX_TOKEN).contains(&(data.len() - s)) {
            out.insert(&data[s..]);
        }
    }
    out
}

/// A --has argument may contain separators ("req-8f3a" -> ["req","8f3a"]);
/// every produced token must be present (AND).
pub fn tokenize_query(arg: &str) -> Vec<Vec<u8>> {
    let mut tokens: Vec<Vec<u8>> = tokenize(arg.as_bytes())
        .into_iter()
        .map(|t| t.to_vec())
        .collect();
    tokens.sort();
    tokens
}

/// The 16-byte header for a grain whose records start at `first_rec`.
/// `HEADER_LEN` (the never-rebased case) writes GRAIN001 with the offset
/// field left zero, so such a file is byte-identical to what every
/// previous release wrote.
fn header_bytes(first_rec: usize) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    if first_rec == HEADER_LEN {
        h[..8].copy_from_slice(GRAIN_MAGIC);
    } else {
        h[..8].copy_from_slice(GRAIN_MAGIC_V2);
        h[12..16].copy_from_slice(&(first_rec as u32).to_le_bytes());
    }
    h[8] = 0; // case folding: none
    h[9] = MIN_TOKEN as u8;
    h[10] = MAX_TOKEN as u8;
    h[11] = K as u8;
    h
}

/// Where this grain's records start, or None if `buf` is not a grain we
/// understand — in which case the caller scans (readers) or rebuilds
/// (writers), never guesses.
fn first_record_offset(buf: &[u8]) -> Option<usize> {
    header_first_rec(buf, buf.len() as u64).map(|o| o as usize)
}

fn header_first_rec(h: &[u8], file_len: u64) -> Option<u64> {
    if h.len() < HEADER_LEN {
        return None;
    }
    match &h[..8] {
        m if m == GRAIN_MAGIC => Some(HEADER_LEN as u64),
        m if m == GRAIN_MAGIC_V2 => {
            let off = u32::from_le_bytes(h[12..16].try_into().unwrap()) as u64;
            (HEADER_LEN as u64..=file_len).contains(&off).then_some(off)
        }
        _ => None,
    }
}

const LEAD_LEN: usize = 16;
const WALK_BUF: usize = 256 * 1024;

/// What a commit is bound to: the file itself, its header, and the bytes
/// where its first record starts. A grain replaced or head-dropped by
/// anyone else no longer matches, whatever its length.
struct Stamp {
    ino: u64,
    header: [u8; HEADER_LEN],
    lead: [u8; LEAD_LEN],
    first: u64,
    len: u64,
}

fn stamp(f: &File) -> io::Result<Option<Stamp>> {
    let md = f.metadata()?;
    let len = md.len();
    if len < HEADER_LEN as u64 {
        return Ok(None);
    }
    let mut header = [0u8; HEADER_LEN];
    f.read_exact_at(&mut header, 0)?;
    let Some(first) = header_first_rec(&header, len) else {
        return Ok(None);
    };
    let mut lead = [0u8; LEAD_LEN];
    let n = (len - first).min(LEAD_LEN as u64) as usize;
    f.read_exact_at(&mut lead[..n], first)?;
    Ok(Some(Stamp {
        ino: md.ino(),
        header,
        lead,
        first,
        len,
    }))
}

/// How many whole records the grain holds and where the last one ends.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tail {
    count: u64,
    end: u64,
}

/// Walk up to `max` length-prefixed records from `first`, stopping at the
/// last whole one: a partial tail is where a crash left off.
fn walk(f: &File, first: u64, len: u64, max: u64) -> io::Result<Tail> {
    let mut r = BufReader::with_capacity(WALK_BUF, f);
    r.seek(SeekFrom::Start(first))?;
    let mut tail = Tail {
        count: 0,
        end: first,
    };
    while tail.count < max && tail.end + 4 <= len {
        let mut p = [0u8; 4];
        r.read_exact(&mut p)?;
        let body = u32::from_le_bytes(p) as u64;
        if tail.end + 4 + body > len {
            break;
        }
        r.seek_relative(body as i64)?;
        tail.end += 4 + body;
        tail.count += 1;
    }
    Ok(tail)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The recorded tail, if it still describes THIS file. A commit that is
/// stale because it lags the file is safe to trust (the caller truncates
/// to it and re-derives the rest); one bound to a different file is not.
fn load_commit(dir: &Path, name: &str, s: &Stamp) -> Option<Tail> {
    let text = fs::read_to_string(format::grain_commit_path(dir, name)).ok()?;
    let mut it = text.split_whitespace();
    let ino: u64 = it.next()?.parse().ok()?;
    let header = unhex::<HEADER_LEN>(it.next()?)?;
    let lead = unhex::<LEAD_LEN>(it.next()?)?;
    let count: u64 = it.next()?.parse().ok()?;
    let end: u64 = it.next()?.parse().ok()?;
    (ino == s.ino && header == s.header && lead == s.lead && end >= s.first && end <= s.len)
        .then_some(Tail { count, end })
}

/// Atomic by rename and deliberately not fsynced: a lost or older commit
/// describes a prefix of the file, which costs re-deriving the difference
/// and never correctness.
fn save_commit(dir: &Path, name: &str, f: &File, t: Tail) -> io::Result<()> {
    let path = format::grain_commit_path(dir, name);
    let Some(s) = stamp(f)? else {
        let _ = fs::remove_file(&path);
        return Ok(());
    };
    let tmp = dir.join(format!("{name}.{}.tmp", format::GRAIN_COMMIT_EXT));
    fs::write(
        &tmp,
        format!(
            "{} {} {} {} {}\n",
            s.ino,
            hex(&s.header),
            hex(&s.lead),
            t.count,
            t.end
        ),
    )?;
    fs::rename(&tmp, &path)
}

fn build_filter(tokens: &HashSet<&[u8]>) -> Vec<u8> {
    let n = tokens.len().max(1) as u64;
    let m_bits = (n * BITS_PER_TOKEN).next_multiple_of(64).max(64);
    let mut bits = vec![0u8; (m_bits / 8) as usize];
    for t in tokens {
        for p in bit_positions(t, m_bits) {
            bits[(p / 8) as usize] |= 1 << (p % 8);
        }
    }
    bits
}

fn filter_contains(filter: &[u8], token: &[u8]) -> bool {
    let m_bits = (filter.len() * 8) as u64;
    if m_bits == 0 {
        return true;
    }
    bit_positions(token, m_bits).all(|p| filter[(p / 8) as usize] & (1 << (p % 8)) != 0)
}

pub struct Grain {
    filters: Vec<Vec<u8>>,
}

impl Grain {
    /// How many chunks this grain has entries for (an index lagging its
    /// log — appender writes, partial extends — covers fewer than the
    /// rings; the gap is scanned, per the contract).
    pub fn chunk_count(&self) -> usize {
        self.filters.len()
    }

    /// One chunk's filter bytes, for shipping it alongside its chunk: the
    /// receiver adopts a page it recognises instead of decompressing to
    /// re-tokenize. `None` beyond the grain's coverage, which means the
    /// destination rebuilds — the same contract as a missing entry.
    pub fn page(&self, idx: usize) -> Option<&[u8]> {
        self.filters.get(idx).map(|v| &v[..])
    }

    /// May chunk `idx` contain ALL the tokens? A chunk beyond the grain's
    /// coverage answers yes — missing means scan, per the contract.
    pub fn may_contain_all(&self, idx: usize, tokens: &[Vec<u8>]) -> bool {
        match self.filters.get(idx) {
            Some(f) => tokens.iter().all(|t| filter_contains(f, t)),
            None => true,
        }
    }
}

/// The filters of records `from..from + count`, read by walking the length
/// prefixes with a bounded buffer: memory is the pages asked for, not the
/// grain. Shorter than `count` where the grain does not reach, which is
/// "missing means scan" for the rest.
pub fn read_pages(path: &Path, from: usize, count: usize) -> Vec<Vec<u8>> {
    let mut pages = Vec::new();
    let Ok(f) = File::open(path) else {
        return pages;
    };
    let Ok(Some(s)) = stamp(&f) else {
        return pages;
    };
    let mut r = BufReader::with_capacity(WALK_BUF, &f);
    if r.seek(SeekFrom::Start(s.first)).is_err() {
        return pages;
    }
    let (mut off, mut idx) = (s.first, 0usize);
    while idx < from.saturating_add(count) && off + 4 <= s.len {
        let mut p = [0u8; 4];
        if r.read_exact(&mut p).is_err() {
            break;
        }
        let len = u32::from_le_bytes(p) as u64;
        if off + 4 + len > s.len {
            break;
        }
        if idx >= from {
            let mut page = vec![0u8; len as usize];
            if r.read_exact(&mut page).is_err() {
                break;
            }
            pages.push(page);
        } else if r.seek_relative(len as i64).is_err() {
            break;
        }
        off += 4 + len;
        idx += 1;
    }
    pages
}

/// The grain's size and how many chunks it covers, without loading it:
/// the commit when it still describes the file, otherwise one walk. Opens
/// nothing for writing, so a caller that may only read the store can use it.
pub fn coverage(dir: &Path, name: &str) -> Option<(u64, usize)> {
    let f = File::open(format::grain_path(dir, name)).ok()?;
    let s = stamp(&f).ok()??;
    let tail = match load_commit(dir, name, &s) {
        Some(t) => t,
        None => walk(&f, s.first, s.len, u64::MAX).ok()?,
    };
    Some((s.len, tail.count as usize))
}

pub fn load(path: &Path) -> anyhow::Result<Grain> {
    let buf = fs::read(path).with_context(|| format!("reading grain index {}", path.display()))?;
    let Some(first) = first_record_offset(&buf) else {
        bail!("{} is not a grain index (bad magic)", path.display());
    };
    let mut filters = Vec::new();
    let mut off = first;
    while off + 4 <= buf.len() {
        let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        if off + len > buf.len() {
            break; // truncated tail: those chunks fall back to scanning
        }
        filters.push(buf[off..off + len].to_vec());
        off += len;
    }
    Ok(Grain { filters })
}

/// Build (or rebuild) the .grain for a backing pair by streaming the trunk.
pub fn cmd_reindex(file: &Path) -> anyhow::Result<()> {
    if crate::query::is_bundle(file) {
        bail!(
            "{} is a .timber bundle (read-only); reindex the log before exporting it",
            file.display()
        );
    }
    let (dir, name) = crate::query::resolve_backing(file)?;
    let rings_p = format::rings_path(&dir, &name);
    if !rings_p.exists() {
        bail!("no index file {}", rings_p.display());
    }
    // The same writer locks as rotation: don't race an appender whose
    // chunk numbering could move under us (head drops).
    let _dir_lock = store::lock_backing_shared(&dir)?.with_context(|| {
        format!(
            "backing directory {} is served by a timberfs mount",
            dir.display()
        )
    })?;
    let _file_lock = store::lock_file_exclusive(&dir, &name)?
        .with_context(|| format!("{name} has an active writer; stop it and retry"))?;
    crate::bark::declare_index(&dir, &name)?;
    build_grain(&dir, &name)
}

/// Extend an existing grain to cover chunks appended since it was built —
/// only the new chunks are tokenized, so maintenance is proportional to
/// the new data, like a database index. Anything unexpected (bad magic,
/// more grain records than rings) falls back to a full rebuild. The
/// caller holds the writer locks.
pub fn extend_grain(dir: &Path, name: &str) -> anyhow::Result<()> {
    let gpath = format::grain_path(dir, name);
    let Ok(out) = OpenOptions::new().read(true).write(true).open(&gpath) else {
        return build_grain(dir, name);
    };
    let Some((tail, known)) = locate(dir, name, &out)? else {
        return build_grain(dir, name);
    };
    let covered = tail.count as usize;
    let (chunks, new) = format::read_index_tail(&format::rings_path(dir, name), covered)?;
    if covered > chunks {
        return build_grain(dir, name);
    }
    if out.metadata()?.len() > tail.end {
        out.set_len(tail.end)?;
    }
    if covered == chunks {
        if !known {
            save_commit(dir, name, &out, tail)?;
        }
        return Ok(());
    }
    let trunk = File::open(format::trunk_path(dir, name))
        .with_context(|| format!("opening {}", format::trunk_path(dir, name).display()))?;
    let mut woff = tail.end;
    for c in &new {
        let mut comp = vec![0u8; c.comp_len as usize];
        trunk.read_exact_at(&mut comp, c.comp_start)?;
        let data = zstd::stream::decode_all(&comp[..])
            .with_context(|| "decompressing a stored chunk — the .trunk may be corrupt")?;
        let tokens = tokenize(&data);
        let filter = build_filter(&tokens);
        out.write_all_at(&(filter.len() as u32).to_le_bytes(), woff)?;
        woff += 4;
        out.write_all_at(&filter, woff)?;
        woff += filter.len() as u64;
    }
    out.sync_all()?;
    save_commit(
        dir,
        name,
        &out,
        Tail {
            count: chunks as u64,
            end: woff,
        },
    )?;
    Ok(())
}

/// Where the records end: the commit when it still describes this file,
/// otherwise one walk. The flag says which. None when the file is not a
/// grain.
fn locate(dir: &Path, name: &str, f: &File) -> io::Result<Option<(Tail, bool)>> {
    let Some(s) = stamp(f)? else {
        return Ok(None);
    };
    Ok(Some(match load_commit(dir, name, &s) {
        Some(t) => (t, true),
        None => (walk(f, s.first, s.len, u64::MAX)?, false),
    }))
}

/// Does a grain header from elsewhere describe the tokenizer THIS build
/// uses? Parameters live in bytes 8..12 (case folding, MIN_TOKEN,
/// MAX_TOKEN, K) and a page built under different ones, read under ours,
/// gives FALSE NEGATIVES — the single answer a search index must never
/// give. So a mismatch means "do not adopt", and the destination rebuilds.
pub fn header_matches(bytes: &[u8]) -> bool {
    if bytes.len() < HEADER_LEN {
        return false;
    }
    let known = &bytes[..8] == GRAIN_MAGIC || &bytes[..8] == GRAIN_MAGIC_V2;
    known && bytes[8..12] == header_bytes(HEADER_LEN)[8..12]
}

/// Adopt one filter page computed elsewhere as the next chunk's record.
/// The caller has just appended the corresponding chunk, so position and
/// chunk index stay in step — which is the grain's whole indexing scheme.
///
/// Best-effort like the rest of the sidecar: a grain that cannot be
/// extended is removed and the next `extend_grain` rebuilds it.
pub fn append_page(dir: &Path, name: &str, page: &[u8]) -> anyhow::Result<()> {
    let gpath = format::grain_path(dir, name);
    let open = || OpenOptions::new().read(true).write(true).open(&gpath);
    let (f, tail) = match open() {
        Ok(f) => match locate(dir, name, &f)? {
            Some((tail, _)) => (f, tail),
            None => (fresh_grain(dir, name, &open)?, EMPTY),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => (fresh_grain(dir, name, &open)?, EMPTY),
        Err(e) => return Err(e).with_context(|| format!("opening {}", gpath.display())),
    };
    let mut record = Vec::with_capacity(4 + page.len());
    record.extend_from_slice(&(page.len() as u32).to_le_bytes());
    record.extend_from_slice(page);
    if f.metadata()?.len() > tail.end {
        f.set_len(tail.end)?;
    }
    f.write_all_at(&record, tail.end)
        .with_context(|| format!("appending to {}", gpath.display()))?;
    save_commit(
        dir,
        name,
        &f,
        Tail {
            count: tail.count + 1,
            end: tail.end + record.len() as u64,
        },
    )?;
    Ok(())
}

const EMPTY: Tail = Tail {
    count: 0,
    end: HEADER_LEN as u64,
};

/// A header-only grain installed by rename, so a reader sees the old file
/// or the new one and never a file with half a header.
fn fresh_grain(
    dir: &Path,
    name: &str,
    open: &dyn Fn() -> io::Result<File>,
) -> anyhow::Result<File> {
    let gpath = format::grain_path(dir, name);
    let tmp = gpath.with_extension("grain.tmp");
    fs::write(&tmp, header_bytes(HEADER_LEN))
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &gpath).with_context(|| format!("renaming onto {}", gpath.display()))?;
    Ok(open()?)
}

/// Drop the first `k` chunks' filters, after retention has cut the same
/// `k` chunks off the head of the store.
///
/// A record's position IS its chunk index, so a rings rebase renumbers
/// every chunk and leaves each filter answering for the wrong one — a
/// FALSE NEGATIVE, the single answer a search index must never give. The
/// cut is always a prefix, so the fix is a prefix too, and it is cheap in
/// the same way the trunk's own head-drop is cheap: whole blocks come off
/// the front with `COLLAPSE_RANGE`, the dead bytes left by the alignment
/// are skipped via the header's first-record offset, and nothing is
/// decompressed or re-tokenized. Where collapse doesn't apply, the tail is
/// rewritten instead (still no decompression).
///
/// The caller holds the writer locks, has already rebased the rings, and
/// keeps a seqlock window open around both. Best-effort by contract: a
/// grain that is missing, unreadable or shorter than the drop is simply
/// removed, and the next extend rebuilds it.
pub fn rebase_head(dir: &Path, name: &str, k: usize) -> io::Result<()> {
    let gpath = format::grain_path(dir, name);
    if k == 0 {
        return Ok(());
    }
    let f = match OpenOptions::new().read(true).write(true).open(&gpath) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let k64 = k as u64;
    let s = stamp(&f)?;
    let survivors = match &s {
        Some(s) => {
            let t = walk(&f, s.first, s.len, k64)?;
            (t.count == k64).then_some(t.end)
        }
        None => None,
    };
    let (Some(s), Some(survivors)) = (s, survivors) else {
        // Not a grain we understand, or it covered fewer chunks than were
        // dropped: nothing left worth rebasing.
        let _ = fs::remove_file(&gpath);
        let _ = fs::remove_file(format::grain_commit_path(dir, name));
        return Ok(());
    };
    let known = load_commit(dir, name, &s).filter(|t| t.count >= k64 && survivors <= t.end);

    // Keep room to re-stamp the header over dead bytes: the cut can never
    // reach past `survivors - HEADER_LEN`.
    let bsize = crate::store::fstatvfs_bsize(&f)?;
    let aligned = ((survivors - HEADER_LEN as u64) / bsize) * bsize;
    if aligned > 0 {
        let rc = unsafe {
            libc::fallocate(
                std::os::fd::AsRawFd::as_raw_fd(&f),
                libc::FALLOC_FL_COLLAPSE_RANGE,
                0,
                aligned as libc::off_t,
            )
        };
        if rc == 0 {
            let first_rec = (survivors - aligned) as usize;
            f.write_all_at(&header_bytes(first_rec), 0)?;
            f.sync_all()?;
            recommit(
                dir,
                name,
                &f,
                known.map(|t| Tail {
                    count: t.count - k64,
                    end: t.end - aligned,
                }),
            );
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            // No COLLAPSE_RANGE here (tmpfs, btrfs, NFS, older ext4/xfs):
            // rewrite instead, below.
            Some(libc::EOPNOTSUPP) | Some(libc::EINVAL) => {}
            _ => return Err(e),
        }
    }
    // Rewrite: a fresh GRAIN001 (records back at HEADER_LEN) staged and
    // renamed, so a reader sees the whole old file or the whole new one.
    // A store on a filesystem without COLLAPSE_RANGE therefore never
    // upgrades its magic at all.
    let tmp = dir.join(format!("{name}.{}.tmp", format::GRAIN_EXT));
    let out = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    let src_end = known.map_or(s.len, |t| t.end);
    {
        let mut w = BufWriter::with_capacity(WALK_BUF, &out);
        w.write_all(&header_bytes(HEADER_LEN))?;
        let mut r = BufReader::with_capacity(WALK_BUF, &f);
        r.seek(SeekFrom::Start(survivors))?;
        io::copy(&mut r.take(src_end - survivors), &mut w)?;
        w.flush()?;
    }
    out.sync_all()?;
    fs::rename(&tmp, &gpath)?;
    recommit(
        dir,
        name,
        &out,
        known.map(|t| Tail {
            count: t.count - k64,
            end: HEADER_LEN as u64 + (t.end - survivors),
        }),
    );
    Ok(())
}

/// Carry a commit across a rebase, or drop it so the next extend walks.
fn recommit(dir: &Path, name: &str, f: &File, t: Option<Tail>) {
    match t {
        Some(t) => {
            let _ = save_commit(dir, name, f, t);
        }
        None => {
            let _ = fs::remove_file(format::grain_commit_path(dir, name));
        }
    }
}

/// The grain build itself; the caller holds the writer locks.
pub fn build_grain(dir: &Path, name: &str) -> anyhow::Result<()> {
    let rings_p = format::rings_path(dir, name);
    let records = format::read_index(&rings_p)?;
    let trunk = File::open(format::trunk_path(dir, name))
        .with_context(|| format!("opening {}", format::trunk_path(dir, name).display()))?;
    let tmp = dir.join(format!("{name}.{}.tmp", format::GRAIN_EXT));
    let out = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    out.write_all_at(&header_bytes(HEADER_LEN), 0)?;

    let mut off = HEADER_LEN as u64;
    let mut total_tokens: u64 = 0;
    let mut next_progress = records.len() / 10;
    for (i, c) in records.iter().enumerate() {
        let mut comp = vec![0u8; c.comp_len as usize];
        trunk.read_exact_at(&mut comp, c.comp_start)?;
        let data = zstd::stream::decode_all(&comp[..])
            .with_context(|| "decompressing a stored chunk — the .trunk may be corrupt")?;
        let tokens = tokenize(&data);
        total_tokens += tokens.len() as u64;
        let filter = build_filter(&tokens);
        out.write_all_at(&(filter.len() as u32).to_le_bytes(), off)?;
        off += 4;
        out.write_all_at(&filter, off)?;
        off += filter.len() as u64;
        if records.len() >= 10 && i + 1 >= next_progress && i + 1 < records.len() {
            crate::note!(
                "timberfs: reindex {}% ({} of {} chunks)",
                (i + 1) * 100 / records.len(),
                i + 1,
                records.len()
            );
            next_progress += records.len() / 10;
        }
    }
    out.sync_all()?;
    fs::rename(&tmp, format::grain_path(dir, name)).with_context(|| {
        format!(
            "installing grain index {}",
            format::grain_path(dir, name).display()
        )
    })?;
    let _ = save_commit(
        dir,
        name,
        &out,
        Tail {
            count: records.len() as u64,
            end: off,
        },
    );
    crate::note!(
        "timberfs: indexed {} chunk(s), {} distinct tokens ({} avg/chunk), grain is {} bytes \
         ({} bytes/chunk avg)",
        records.len(),
        total_tokens,
        total_tokens / records.len().max(1) as u64,
        off,
        off / records.len().max(1) as u64
    );
    Ok(())
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
            let dir = std::env::temp_dir()
                .join(format!("timberfs-grain-test-{}-{n}", std::process::id()));
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

    /// A grain whose chunk `i` contains exactly the token `tok<i>`, built
    /// by hand so the test doesn't need a store behind it.
    fn write_grain(dir: &Path, name: &str, chunks: usize) -> Vec<Vec<u8>> {
        let mut out = header_bytes(HEADER_LEN).to_vec();
        let mut tokens = Vec::new();
        for i in 0..chunks {
            let tok = format!("tok{i:04}").into_bytes();
            // Pad each filter out so the file spans several blocks and the
            // collapse path is exercised, not just the rewrite fallback.
            let mut set: HashSet<&[u8]> = HashSet::new();
            set.insert(&tok);
            // Big enough that dropping a few records spans a whole
            // filesystem block, so the COLLAPSE_RANGE branch is the one
            // under test wherever the filesystem supports it (tmpfs does
            // not, and takes the rewrite fallback — both are asserted
            // through the same semantic checks below).
            let padding: Vec<Vec<u8>> = (0..4000)
                .map(|p| format!("pad{i}x{p:04}").into_bytes())
                .collect();
            for p in &padding {
                set.insert(p);
            }
            let filter = build_filter(&set);
            out.extend_from_slice(&(filter.len() as u32).to_le_bytes());
            out.extend_from_slice(&filter);
            tokens.push(tok);
        }
        fs::write(format::grain_path(dir, name), &out).unwrap();
        tokens
    }

    #[test]
    fn a_fresh_grain_is_still_v1_on_disk() {
        // The V2 header exists only for rebased files: a store that never
        // hits retention must stay byte-compatible with older readers.
        let d = TempDir::new();
        write_grain(d.path(), "a.log", 3);
        let buf = fs::read(format::grain_path(d.path(), "a.log")).unwrap();
        assert_eq!(&buf[..8], GRAIN_MAGIC);
        assert_eq!(&buf[12..16], &[0, 0, 0, 0], "the offset field stays zero");
        assert_eq!(first_record_offset(&buf), Some(HEADER_LEN));
    }

    #[test]
    fn rebase_head_drops_a_prefix_and_keeps_the_rest_aligned() {
        let d = TempDir::new();
        let tokens = write_grain(d.path(), "a.log", 12);
        let before = load(&format::grain_path(d.path(), "a.log")).unwrap();
        assert_eq!(before.chunk_count(), 12);

        rebase_head(d.path(), "a.log", 5).unwrap();

        let after = load(&format::grain_path(d.path(), "a.log")).unwrap();
        assert_eq!(after.chunk_count(), 7, "12 chunks minus the 5 dropped");
        // Chunk i of the rebased grain must answer for what was chunk i+5:
        // the whole point, since a stale mapping is a FALSE NEGATIVE.
        for (i, t) in tokens.iter().enumerate().skip(5) {
            assert!(
                after.may_contain_all(i - 5, std::slice::from_ref(t)),
                "token of old chunk {i} lost from new chunk {}",
                i - 5
            );
        }
        // And the dropped ones are gone rather than shifted into place.
        let survivors_claim_dropped =
            (0..7).any(|i| after.may_contain_all(i, std::slice::from_ref(&tokens[0])));
        assert!(
            !survivors_claim_dropped,
            "a dropped chunk's filter survived"
        );
    }

    #[test]
    fn a_rebased_grain_reads_back_through_both_paths() {
        let d = TempDir::new();
        write_grain(d.path(), "a.log", 10);
        rebase_head(d.path(), "a.log", 4).unwrap();
        let buf = fs::read(format::grain_path(d.path(), "a.log")).unwrap();
        // Either strategy is correct; both must leave a file `load` and
        // `extend_grain`'s walker agree on.
        let first = first_record_offset(&buf).expect("still a grain");
        if &buf[..8] == GRAIN_MAGIC_V2 {
            assert!(first > HEADER_LEN, "V2 means dead bytes were left behind");
        } else {
            assert_eq!(first, HEADER_LEN, "the rewrite path resets to V1");
        }
        let g = fs::File::open(format::grain_path(d.path(), "a.log")).unwrap();
        let t = walk(&g, first as u64, buf.len() as u64, u64::MAX).unwrap();
        assert_eq!((t.count, t.end), (6, buf.len() as u64));
    }

    #[test]
    fn rebasing_past_the_end_drops_the_grain() {
        // A grain lagging its log can cover fewer chunks than retention
        // just dropped: there is nothing left to rebase, so it goes and
        // the next extend rebuilds it.
        let d = TempDir::new();
        write_grain(d.path(), "a.log", 3);
        rebase_head(d.path(), "a.log", 9).unwrap();
        assert!(!format::grain_path(d.path(), "a.log").exists());
    }

    #[test]
    fn rebase_is_a_noop_without_a_grain() {
        let d = TempDir::new();
        rebase_head(d.path(), "missing.log", 3).unwrap();
        assert!(!format::grain_path(d.path(), "missing.log").exists());
    }

    fn cfg() -> crate::store::Config {
        crate::store::Config {
            chunk_size: 1 << 20,
            level: 1,
            flush_age_ms: u64::MAX,
        }
    }

    fn a_store(dir: &Path, chunks: usize) -> crate::store::Store {
        let mut st = crate::store::Store {
            dir: dir.to_path_buf(),
            cfg: cfg(),
            files: std::collections::BTreeMap::new(),
        };
        st.create("a.log").unwrap();
        add_chunks(&mut st, 0, chunks);
        st
    }

    fn add_chunks(st: &mut crate::store::Store, from: usize, to: usize) {
        let f = st.files.get_mut("a.log").unwrap();
        for i in from..to {
            f.append_windowed(
                format!("line {i} marker{i:05} padding\n").as_bytes(),
                1_000 + i as u64,
                1_000 + i as u64,
                &cfg(),
            )
            .unwrap();
            f.flush_chunk(&cfg()).unwrap();
        }
    }

    fn marker(i: usize) -> Vec<Vec<u8>> {
        vec![format!("marker{i:05}").into_bytes()]
    }

    fn grain_bytes(d: &Path) -> Vec<u8> {
        fs::read(format::grain_path(d, "a.log")).unwrap()
    }

    fn committed(d: &Path) -> Option<Tail> {
        let f = File::open(format::grain_path(d, "a.log")).ok()?;
        load_commit(d, "a.log", &stamp(&f).ok()??)
    }

    fn assert_indexes_every_chunk(d: &Path, chunks: usize) {
        let g = load(&format::grain_path(d, "a.log")).unwrap();
        assert_eq!(g.chunk_count(), chunks);
        for i in 0..chunks {
            assert!(g.may_contain_all(i, &marker(i)), "chunk {i} lost its token");
        }
    }

    #[test]
    fn extending_in_steps_equals_building_at_once() {
        let d = TempDir::new();
        let mut st = a_store(d.path(), 3);
        extend_grain(d.path(), "a.log").unwrap();
        assert_eq!(committed(d.path()).map(|t| t.count), Some(3));
        add_chunks(&mut st, 3, 8);
        extend_grain(d.path(), "a.log").unwrap();
        let stepped = grain_bytes(d.path());

        build_grain(d.path(), "a.log").unwrap();
        assert_eq!(stepped, grain_bytes(d.path()));
        assert_indexes_every_chunk(d.path(), 8);
        let t = committed(d.path()).expect("a build leaves a commit");
        assert_eq!((t.count, t.end), (8, stepped.len() as u64));
    }

    #[test]
    fn the_commit_governs_where_extending_resumes() {
        // A complete-looking record past the committed end is what a walk
        // would count as covered; the commit says it is not.
        let d = TempDir::new();
        let mut st = a_store(d.path(), 3);
        extend_grain(d.path(), "a.log").unwrap();
        let clean = grain_bytes(d.path());
        let g = OpenOptions::new()
            .append(true)
            .open(format::grain_path(d.path(), "a.log"))
            .unwrap();
        (&g).write_all(&8u32.to_le_bytes()).unwrap();
        (&g).write_all(&[0u8; 8]).unwrap();
        drop(g);

        add_chunks(&mut st, 3, 4);
        extend_grain(d.path(), "a.log").unwrap();
        assert_indexes_every_chunk(d.path(), 4);
        assert_eq!(&grain_bytes(d.path())[..clean.len()], &clean[..]);
    }

    #[test]
    fn a_torn_tail_is_cut_before_extending() {
        let d = TempDir::new();
        let mut st = a_store(d.path(), 3);
        extend_grain(d.path(), "a.log").unwrap();
        let g = OpenOptions::new()
            .append(true)
            .open(format::grain_path(d.path(), "a.log"))
            .unwrap();
        (&g).write_all(&100u32.to_le_bytes()).unwrap();
        (&g).write_all(&[1, 2, 3]).unwrap();
        drop(g);
        // Without the commit, the walk has to find the same boundary.
        fs::remove_file(format::grain_commit_path(d.path(), "a.log")).unwrap();

        add_chunks(&mut st, 3, 5);
        extend_grain(d.path(), "a.log").unwrap();
        assert_indexes_every_chunk(d.path(), 5);
        let stepped = grain_bytes(d.path());
        build_grain(d.path(), "a.log").unwrap();
        assert_eq!(stepped, grain_bytes(d.path()));
    }

    #[test]
    fn a_commit_for_another_file_is_not_believed() {
        let d = TempDir::new();
        let _st = a_store(d.path(), 3);
        extend_grain(d.path(), "a.log").unwrap();
        assert_eq!(committed(d.path()).map(|t| t.count), Some(3));

        // Someone else replaces the grain with one covering fewer chunks.
        let other = TempDir::new();
        let _ = a_store(other.path(), 2);
        build_grain(other.path(), "a.log").unwrap();
        fs::remove_file(format::grain_commit_path(other.path(), "a.log")).unwrap();
        fs::copy(
            format::grain_path(other.path(), "a.log"),
            d.path().join("swap"),
        )
        .unwrap();
        fs::rename(d.path().join("swap"), format::grain_path(d.path(), "a.log")).unwrap();

        extend_grain(d.path(), "a.log").unwrap();
        assert_indexes_every_chunk(d.path(), 3);
    }

    #[test]
    fn a_lagging_commit_re_derives_what_it_does_not_cover() {
        // The commit rename can be lost across a power cut while the
        // records it described survive.
        let d = TempDir::new();
        let mut st = a_store(d.path(), 3);
        extend_grain(d.path(), "a.log").unwrap();
        let old = fs::read(format::grain_commit_path(d.path(), "a.log")).unwrap();
        add_chunks(&mut st, 3, 6);
        extend_grain(d.path(), "a.log").unwrap();
        fs::write(format::grain_commit_path(d.path(), "a.log"), old).unwrap();

        extend_grain(d.path(), "a.log").unwrap();
        assert_indexes_every_chunk(d.path(), 6);
    }

    #[test]
    fn rebasing_carries_the_commit() {
        let d = TempDir::new();
        let _st = a_store(d.path(), 12);
        extend_grain(d.path(), "a.log").unwrap();

        rebase_head(d.path(), "a.log", 5).unwrap();

        let t = committed(d.path()).expect("the rebase kept the commit usable");
        assert_eq!(t.count, 7);
        assert_eq!(t.end, grain_bytes(d.path()).len() as u64);
        let g = load(&format::grain_path(d.path(), "a.log")).unwrap();
        assert_eq!(g.chunk_count(), 7);
        for i in 5..12 {
            assert!(g.may_contain_all(i - 5, &marker(i)));
        }
    }

    #[test]
    fn rebasing_without_a_commit_still_works_and_leaves_none() {
        let d = TempDir::new();
        let _st = a_store(d.path(), 12);
        extend_grain(d.path(), "a.log").unwrap();
        fs::remove_file(format::grain_commit_path(d.path(), "a.log")).unwrap();

        rebase_head(d.path(), "a.log", 4).unwrap();
        assert!(committed(d.path()).is_none());
        assert_eq!(
            load(&format::grain_path(d.path(), "a.log"))
                .unwrap()
                .chunk_count(),
            8
        );
    }

    #[test]
    fn adopted_pages_build_the_same_grain_and_commit() {
        let src = TempDir::new();
        let _st = a_store(src.path(), 6);
        build_grain(src.path(), "a.log").unwrap();
        let g = load(&format::grain_path(src.path(), "a.log")).unwrap();

        let dst = TempDir::new();
        for i in 0..6 {
            append_page(dst.path(), "a.log", g.page(i).unwrap()).unwrap();
        }
        assert_eq!(grain_bytes(dst.path()), grain_bytes(src.path()));
        let t = committed(dst.path()).expect("adoption commits");
        assert_eq!((t.count, t.end), (6, grain_bytes(dst.path()).len() as u64));
    }

    #[test]
    fn adopting_a_page_after_a_torn_tail_does_not_keep_the_debris() {
        let src = TempDir::new();
        let _st = a_store(src.path(), 3);
        build_grain(src.path(), "a.log").unwrap();
        let g = load(&format::grain_path(src.path(), "a.log")).unwrap();

        let dst = TempDir::new();
        append_page(dst.path(), "a.log", g.page(0).unwrap()).unwrap();
        let f = OpenOptions::new()
            .append(true)
            .open(format::grain_path(dst.path(), "a.log"))
            .unwrap();
        (&f).write_all(&50u32.to_le_bytes()).unwrap();
        drop(f);
        append_page(dst.path(), "a.log", g.page(1).unwrap()).unwrap();
        append_page(dst.path(), "a.log", g.page(2).unwrap()).unwrap();

        assert_eq!(grain_bytes(dst.path()), grain_bytes(src.path()));
    }

    #[test]
    fn rebasing_a_wide_grain_carries_the_commit_through_a_collapse() {
        // Filters big enough that the dropped prefix spans a filesystem
        // block, so COLLAPSE_RANGE is the branch taken wherever it exists.
        let d = TempDir::new();
        let mut st = crate::store::Store {
            dir: d.path().to_path_buf(),
            cfg: cfg(),
            files: std::collections::BTreeMap::new(),
        };
        st.create("a.log").unwrap();
        let f = st.files.get_mut("a.log").unwrap();
        for i in 0..10usize {
            let wide: String = (0..1500).map(|t| format!("w{i:02}t{t:05} ")).collect();
            f.append_windowed(
                format!("{wide}marker{i:05}\n").as_bytes(),
                1_000 + i as u64,
                1_000 + i as u64,
                &cfg(),
            )
            .unwrap();
            f.flush_chunk(&cfg()).unwrap();
        }
        extend_grain(d.path(), "a.log").unwrap();

        rebase_head(d.path(), "a.log", 6).unwrap();

        let buf = grain_bytes(d.path());
        eprintln!(
            "rebased grain magic: {}",
            String::from_utf8_lossy(&buf[..8])
        );
        let t = committed(d.path()).expect("the rebase kept the commit usable");
        assert_eq!((t.count, t.end), (4, buf.len() as u64));
        let g = load(&format::grain_path(d.path(), "a.log")).unwrap();
        assert_eq!(g.chunk_count(), 4);
        for i in 6..10 {
            assert!(g.may_contain_all(i - 6, &marker(i)));
        }
        assert!(!g.may_contain_all(0, &marker(0)));
    }

    #[test]
    fn coverage_agrees_with_loading_the_grain() {
        let d = TempDir::new();
        let _st = a_store(d.path(), 7);
        extend_grain(d.path(), "a.log").unwrap();
        let loaded = load(&format::grain_path(d.path(), "a.log")).unwrap();
        let len = grain_bytes(d.path()).len() as u64;

        assert_eq!(
            coverage(d.path(), "a.log"),
            Some((len, loaded.chunk_count()))
        );
        // Without the commit it walks, and gets the same answer without
        // writing one: the caller may be unable to write the store at all.
        fs::remove_file(format::grain_commit_path(d.path(), "a.log")).unwrap();
        assert_eq!(coverage(d.path(), "a.log"), Some((len, 7)));
        assert!(!format::grain_commit_path(d.path(), "a.log").exists());
    }

    #[test]
    fn coverage_does_not_count_a_torn_tail_or_a_missing_grain() {
        let d = TempDir::new();
        assert_eq!(coverage(d.path(), "a.log"), None);
        let _st = a_store(d.path(), 4);
        extend_grain(d.path(), "a.log").unwrap();
        let g = OpenOptions::new()
            .append(true)
            .open(format::grain_path(d.path(), "a.log"))
            .unwrap();
        (&g).write_all(&100u32.to_le_bytes()).unwrap();
        drop(g);
        fs::remove_file(format::grain_commit_path(d.path(), "a.log")).unwrap();
        assert_eq!(coverage(d.path(), "a.log").map(|c| c.1), Some(4));

        fs::write(
            format::grain_path(d.path(), "a.log"),
            b"not a grain at all, no",
        )
        .unwrap();
        assert_eq!(coverage(d.path(), "a.log"), None);
    }

    #[test]
    fn pages_read_by_range_are_the_loaded_ones() {
        let d = TempDir::new();
        write_grain(d.path(), "a.log", 9);
        let path = format::grain_path(d.path(), "a.log");
        let loaded = load(&path).unwrap();
        for (from, count) in [(0, 9), (0, 3), (2, 4), (7, 2), (8, 50), (0, 0)] {
            let got = read_pages(&path, from, count);
            let want: Vec<Vec<u8>> = (from..(from + count).min(9))
                .map(|i| loaded.page(i).unwrap().to_vec())
                .collect();
            assert_eq!(got, want, "pages {from}..{}", from + count);
        }
        assert!(read_pages(&path, 40, 3).is_empty(), "past the end");
        assert!(read_pages(&d.path().join("none"), 0, 3).is_empty());
    }

    #[test]
    fn pages_stop_at_a_torn_tail() {
        let d = TempDir::new();
        write_grain(d.path(), "a.log", 4);
        let path = format::grain_path(d.path(), "a.log");
        let g = OpenOptions::new().append(true).open(&path).unwrap();
        (&g).write_all(&90u32.to_le_bytes()).unwrap();
        (&g).write_all(&[7u8; 5]).unwrap();
        drop(g);
        assert_eq!(read_pages(&path, 0, 10).len(), 4);
        assert_eq!(read_pages(&path, 3, 10).len(), 1);
    }
}
