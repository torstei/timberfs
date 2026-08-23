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

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
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
    if buf.len() < HEADER_LEN {
        return None;
    }
    match &buf[..8] {
        m if m == GRAIN_MAGIC => Some(HEADER_LEN),
        m if m == GRAIN_MAGIC_V2 => {
            let off = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
            (HEADER_LEN..=buf.len()).contains(&off).then_some(off)
        }
        _ => None,
    }
}

/// Walk `n` records from `off`, returning where the next one starts.
/// None when the file ends first — a partial tail (crash debris) or a
/// grain that simply doesn't reach that far.
fn skip_records(buf: &[u8], mut off: usize, n: usize) -> Option<usize> {
    for _ in 0..n {
        if off + 4 > buf.len() {
            return None;
        }
        let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if off + 4 + len > buf.len() {
            return None;
        }
        off += 4 + len;
    }
    Some(off)
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
    let existing = match fs::read(&gpath) {
        Ok(b) => b,
        Err(_) => return build_grain(dir, name),
    };
    let Some(first) = first_record_offset(&existing) else {
        return build_grain(dir, name);
    };
    let mut off = first;
    let mut covered = 0usize;
    loop {
        if off + 4 > existing.len() {
            break;
        }
        let len = u32::from_le_bytes(existing[off..off + 4].try_into().unwrap()) as usize;
        if off + 4 + len > existing.len() {
            break; // partial tail (crash): overwritten below
        }
        off += 4 + len;
        covered += 1;
    }
    let records = format::read_index(&format::rings_path(dir, name))?;
    if covered > records.len() {
        return build_grain(dir, name);
    }
    if covered == records.len() {
        return Ok(());
    }
    let trunk = File::open(format::trunk_path(dir, name))
        .with_context(|| format!("opening {}", format::trunk_path(dir, name).display()))?;
    let out = OpenOptions::new().write(true).open(&gpath)?;
    out.set_len(off as u64)?;
    let mut woff = off as u64;
    let mut total_tokens = 0u64;
    for c in &records[covered..] {
        let mut comp = vec![0u8; c.comp_len as usize];
        trunk.read_exact_at(&mut comp, c.comp_start)?;
        let data = zstd::stream::decode_all(&comp[..])
            .with_context(|| "decompressing a stored chunk — the .trunk may be corrupt")?;
        let tokens = tokenize(&data);
        total_tokens += tokens.len() as u64;
        let filter = build_filter(&tokens);
        out.write_all_at(&(filter.len() as u32).to_le_bytes(), woff)?;
        woff += 4;
        out.write_all_at(&filter, woff)?;
        woff += filter.len() as u64;
    }
    out.sync_all()?;
    crate::note!(
        "timberfs: grain extended: {} new chunk(s) indexed ({} tokens), {} total",
        records.len() - covered,
        total_tokens,
        records.len()
    );
    Ok(())
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
    let mut buf = match fs::read(&gpath) {
        Ok(b) if first_record_offset(&b).is_some() => b,
        _ => header_bytes(HEADER_LEN).to_vec(),
    };
    buf.extend_from_slice(&(page.len() as u32).to_le_bytes());
    buf.extend_from_slice(page);
    let tmp = gpath.with_extension("grain.tmp");
    fs::write(&tmp, &buf).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &gpath).with_context(|| format!("renaming onto {}", gpath.display()))?;
    Ok(())
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
pub fn rebase_head(dir: &Path, name: &str, k: usize) -> std::io::Result<()> {
    let gpath = format::grain_path(dir, name);
    if k == 0 {
        return Ok(());
    }
    let buf = match fs::read(&gpath) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let drop_to = first_record_offset(&buf).and_then(|first| skip_records(&buf, first, k));
    let Some(survivors) = drop_to else {
        // Not a grain we understand, or it covered fewer chunks than were
        // dropped: nothing left worth rebasing.
        let _ = fs::remove_file(&gpath);
        return Ok(());
    };

    let f = OpenOptions::new().read(true).write(true).open(&gpath)?;
    // Keep room to re-stamp the header over dead bytes: the cut can never
    // reach past `survivors - HEADER_LEN`.
    let bsize = crate::store::fstatvfs_bsize(&f)?;
    let aligned = ((survivors - HEADER_LEN) as u64 / bsize) * bsize;
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
            let first_rec = survivors - aligned as usize;
            f.write_all_at(&header_bytes(first_rec), 0)?;
            return f.sync_all();
        }
        let e = std::io::Error::last_os_error();
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
    drop(f);
    let tmp = dir.join(format!("{name}.{}.tmp", format::GRAIN_EXT));
    let out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    out.write_all_at(&header_bytes(HEADER_LEN), 0)?;
    out.write_all_at(&buf[survivors..], HEADER_LEN as u64)?;
    out.sync_all()?;
    fs::rename(&tmp, &gpath)
}

/// The grain build itself; the caller holds the writer locks.
pub fn build_grain(dir: &Path, name: &str) -> anyhow::Result<()> {
    let rings_p = format::rings_path(dir, name);
    let records = format::read_index(&rings_p)?;
    let trunk = File::open(format::trunk_path(dir, name))
        .with_context(|| format!("opening {}", format::trunk_path(dir, name).display()))?;
    let tmp = dir.join(format!("{name}.{}.tmp", format::GRAIN_EXT));
    let out = OpenOptions::new()
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
        assert_eq!(skip_records(&buf, first, 6), Some(buf.len()));
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
}
