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
//! rebuildable (`timberfs reindex`), a chunk without an entry means "scan
//! it", and any rings rewrite (rotation, retention) deletes the file.
//!
//! On disk: magic "GRAIN001", 16-byte header carrying the tokenizer and
//! hash parameters, then per chunk (in rings order): u32 LE filter length
//! in bytes, followed by the filter bits. Hashing is two-seed FNV-1a with
//! Kirsch-Mitzenmacher double hashing — dependency-free and stable.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{bail, Context};

use crate::format::{self};
use crate::store;

pub const GRAIN_MAGIC: &[u8; 8] = b"GRAIN001";
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
    let buf = fs::read(path)?;
    if buf.len() < HEADER_LEN || &buf[..8] != GRAIN_MAGIC {
        bail!("{} is not a grain index (bad magic)", path.display());
    }
    let mut filters = Vec::new();
    let mut off = HEADER_LEN;
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

    let records = format::read_index(&rings_p)?;
    let trunk = File::open(format::trunk_path(&dir, &name))?;
    let tmp = dir.join(format!("{name}.{}.tmp", format::GRAIN_EXT));
    let out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    let mut header = [0u8; HEADER_LEN];
    header[..8].copy_from_slice(GRAIN_MAGIC);
    header[8] = 0; // case folding: none
    header[9] = MIN_TOKEN as u8;
    header[10] = MAX_TOKEN as u8;
    header[11] = K as u8;
    out.write_all_at(&header, 0)?;

    let mut off = HEADER_LEN as u64;
    let mut total_tokens: u64 = 0;
    let mut next_progress = records.len() / 10;
    for (i, c) in records.iter().enumerate() {
        let mut comp = vec![0u8; c.comp_len as usize];
        trunk.read_exact_at(&mut comp, c.comp_start)?;
        let data = zstd::stream::decode_all(&comp[..])?;
        let tokens = tokenize(&data);
        total_tokens += tokens.len() as u64;
        let filter = build_filter(&tokens);
        out.write_all_at(&(filter.len() as u32).to_le_bytes(), off)?;
        off += 4;
        out.write_all_at(&filter, off)?;
        off += filter.len() as u64;
        if records.len() >= 10 && i + 1 >= next_progress && i + 1 < records.len() {
            eprintln!(
                "timberfs: reindex {}% ({} of {} chunks)",
                (i + 1) * 100 / records.len(),
                i + 1,
                records.len()
            );
            next_progress += records.len() / 10;
        }
    }
    out.sync_all()?;
    fs::rename(&tmp, format::grain_path(&dir, &name))?;
    eprintln!(
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
