//! A tally BLOCK: the grid of series × buckets, stored as columns.
//!
//! A tally is not a log. A log entry is a fact — written once, never
//! revised — which is why a chunk can be immutable and why the tape
//! model fits. A tally bucket is a CONCLUSION: derived, recomputable,
//! and provisional until its bucket seals. Storing it as a tape of text
//! lines costs the difference: measured on one real day, the series
//! identity is 44% of the bytes, the bucket stamp 24%, and the numbers
//! 17%, because a tape has one verb and the row key goes in every cell.
//!
//! So this stores the grid instead. See docs/plans/tally-as-a-tally.md
//! for the whole argument and the measurements; the parts that decide
//! the code:
//!
//! * **A series is an object**, written once per block, and the cells
//!   reference it by index. That is the 44%.
//! * **A bucket start is a POSITION** — cell `i` of a series is bucket
//!   `t0 + i * width_ms` — so no stamp is stored at all. That is the
//!   24%.
//! * **A measure is a COLUMN**, which is what makes coarsening the
//!   column operation the field name already names.
//!
//! ⚠ The text line format is the INTERCHANGE form and not the storage:
//! `Block::samples` renders `tally::Sample`s back, so `timbergraph`,
//! `--fold` and a human reading a pipe keep working. The line format
//! was doing two jobs and only one of them wanted a tape.
//!
//! ⚠ **Zero and unknown are different**, which is tally's second
//! invariant and a convention nothing could enforce on a tape. Here a
//! presence bit per cell carries it: a bucket a series was absent from
//! has no bit and no value, which is not the same as a bucket where it
//! counted nothing. A metric added today therefore leaves last week
//! UNKNOWN rather than zero, by construction.
//!
//! EXPERIMENTAL: the on-disk shape may still move, and nothing reads it
//! but the tests and `tally --pack`/`--unpack`.

use std::collections::BTreeMap;

use anyhow::{bail, Context};

use crate::tally::{Field, Sample};

/// The format's magic and version. A block is self-describing because a
/// manifest diff ships one on its own — see the plan note.
const MAGIC: &[u8; 4] = b"TBLK";
const VERSION: u8 = 1;

/// One series' identity, written once per block.
///
/// ⚠ The DEFINITION belongs here too, and is why a name needs no
/// prefix: what a number means is a property of the series, not of a
/// string somebody chose. Not carried yet — the extractor has no id to
/// put in it (docs/plans/tally-series-identity.md), so the field is
/// deliberately absent rather than filled with the document's name,
/// which is a handle and not an identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Series {
    pub metric: String,
    /// Sorted by key, as a `Sample`'s are: one series is one run of
    /// bytes, and two blocks agree on the order.
    pub labels: Vec<(String, String)>,
}

impl Series {
    fn of(s: &Sample) -> Series {
        Series {
            metric: s.metric.clone(),
            labels: s.labels.clone(),
        }
    }
}

/// A block: one time range of one tally, every series in it, columnar.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub width_ms: u64,
    /// The first bucket's start. Cell `i` is `t0 + i * width_ms`.
    pub t0: u64,
    pub n_buckets: usize,
    /// Bumped when a range is re-derived. The address is
    /// `(range, generation)` and never a position in a sequence, which
    /// is what makes regeneration safe where a chunk number is not.
    pub generation: u32,
    pub series: Vec<Series>,
    /// `cells[s][f]` is series `s`'s column for field `f`, as
    /// `Option<f64>` per bucket — `None` being ABSENT and not zero.
    cells: Vec<BTreeMap<Field, Vec<Option<f64>>>>,
    /// The CITATION, one span of source tape PER BUCKET rather than per
    /// cell: the offset range every series in that bucket counted
    /// within.
    ///
    /// ⚠ Kept, and kept coarse, and both halves matter.
    ///
    /// Kept, because a bucket's TIME does not bound the entries it
    /// counted. Displacement puts an entry whose own bucket had already
    /// sealed into the CURRENT one — that is what `!late` records — so a
    /// time-range lookup on the source would miss exactly the entries
    /// that made a number surprising. An offset range is exact whatever
    /// the clock did and whatever arrived late. It is also why a
    /// citation cannot be replaced by "the bucket's minute": that
    /// answer is wrong in the one case anybody looks.
    ///
    /// Coarse, because per-cell it cost 60% of a block — 1.94 bytes a
    /// cell, an offset delta being ~650 KB of tape per bucket where a
    /// count is one byte — and the citation was ALREADY approximate:
    /// "a range to READ, not the set of entries — a metric matching one
    /// line in a hundred cites the ninety-nine between them". One span
    /// per bucket widens an approximation rather than introducing one.
    ///
    /// ⚠ So a citation does NOT round-trip byte-identically: it comes
    /// back widened, CONTAINING what it went in as. `samples` says so
    /// and the tests assert containment rather than equality.
    cites: Vec<Option<(u64, u64)>>,
}

impl Block {
    /// Build blocks from samples, one per `range_buckets` of time.
    ///
    /// ⚠ Markers (`!cap`, `!late`, `!meta`) are NOT taken: they are
    /// statements about a bucket's quality rather than values in the
    /// grid, and where they belong is still open. Passing them here
    /// would silently drop them, so they are refused.
    pub fn pack(samples: &[Sample], range_buckets: usize) -> anyhow::Result<Vec<Block>> {
        if range_buckets == 0 {
            bail!("a block spans at least one bucket");
        }
        if let Some(m) = samples.iter().find(|s| s.is_marker()) {
            bail!(
                "{:?} is a marker, and a block holds the grid — where markers live is not \
                 settled (docs/plans/tally-as-a-tally.md). Filter them out and keep them",
                m.metric
            );
        }
        let width = match samples.iter().map(|s| s.width_ms).max() {
            None | Some(0) => bail!(
                "a block holds BUCKETS, and these are observations (width 0s) — fold them first"
            ),
            Some(w) => w,
        };
        if let Some(s) = samples.iter().find(|s| s.width_ms != width) {
            bail!(
                "two widths in one block ({} and {}) — a cell's position IS its bucket, so one \
                 block is one width",
                crate::tally::render_width(width),
                crate::tally::render_width(s.width_ms)
            );
        }

        // Which block a bucket falls in: floor to a multiple of the
        // range, in bucket counts, so a block boundary is stable under
        // any subset of the input rather than depending on what arrived.
        let span = width * range_buckets as u64;
        let mut by_block: BTreeMap<u64, Vec<&Sample>> = BTreeMap::new();
        for s in samples {
            by_block.entry(s.ts - s.ts % span).or_default().push(s);
        }

        let mut out = Vec::new();
        for (t0, group) in by_block {
            let mut series: Vec<Series> = group.iter().map(|s| Series::of(s)).collect();
            series.sort();
            series.dedup();
            let index: BTreeMap<&Series, usize> =
                series.iter().enumerate().map(|(i, s)| (s, i)).collect();
            let mut cells: Vec<BTreeMap<Field, Vec<Option<f64>>>> =
                vec![BTreeMap::new(); series.len()];
            let mut cites: Vec<Option<(u64, u64)>> = vec![None; range_buckets];
            for s in group {
                let si = index[&Series::of(s)];
                let bucket = ((s.ts - t0) / width) as usize;
                for (f, v) in &s.fields {
                    let col = cells[si]
                        .entry(*f)
                        .or_insert_with(|| vec![None; range_buckets]);
                    col[bucket] = Some(*v);
                }
                if let Some((off, len)) = s.cite {
                    // The bucket's span is the union of its series'.
                    let (lo, hi) = match cites[bucket] {
                        None => (off, off + len),
                        Some((l, n)) => (l.min(off), (l + n).max(off + len)),
                    };
                    cites[bucket] = Some((lo, hi - lo));
                }
            }
            out.push(Block {
                width_ms: width,
                t0,
                n_buckets: range_buckets,
                generation: 0,
                series,
                cells,
                cites,
            });
        }
        Ok(out)
    }

    /// Render the grid back to lines — the INTERCHANGE form.
    ///
    /// Ordered by bucket then series, which is the order a tape had, so
    /// a consumer downstream cannot tell the difference.
    pub fn samples(&self) -> Vec<Sample> {
        let mut out = Vec::new();
        for b in 0..self.n_buckets {
            for (si, s) in self.series.iter().enumerate() {
                let mut fields: Vec<(Field, f64)> = Vec::new();
                for (f, col) in &self.cells[si] {
                    if let Some(v) = col[b] {
                        fields.push((*f, v));
                    }
                }
                if fields.is_empty() {
                    continue; // absent, which is not zero
                }
                fields.sort_by_key(|(f, _)| *f);
                out.push(Sample {
                    ts: self.t0 + b as u64 * self.width_ms,
                    width_ms: self.width_ms,
                    metric: s.metric.clone(),
                    labels: s.labels.clone(),
                    fields,
                    cite: self.cites[b],
                });
            }
        }
        out
    }

    /// How many cells carry a value, against how many the grid has room
    /// for. The measured density of a real tally is ~21%, which is what
    /// the presence bitmap is for.
    pub fn occupancy(&self) -> (usize, usize) {
        let present = self
            .cells
            .iter()
            .flat_map(|m| m.values())
            .flat_map(|c| c.iter())
            .filter(|v| v.is_some())
            .count();
        let room = self
            .cells
            .iter()
            .map(|m| m.len() * self.n_buckets)
            .sum::<usize>();
        (present, room)
    }
}

// ------------------------------------------------------------- encoding

fn put_uvarint(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn get_uvarint(b: &[u8], at: &mut usize) -> anyhow::Result<u64> {
    let mut n = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *b.get(*at).context("a varint ran off the end of a block")?;
        *at += 1;
        n |= ((byte & 0x7f) as u64)
            .checked_shl(shift)
            .context("a varint is longer than 64 bits")?;
        if byte & 0x80 == 0 {
            return Ok(n);
        }
        shift += 7;
        if shift > 63 {
            bail!("a varint is longer than 64 bits");
        }
    }
}

fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

fn unzigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_uvarint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn get_str(b: &[u8], at: &mut usize) -> anyhow::Result<String> {
    let n = get_uvarint(b, at)? as usize;
    let end = at.checked_add(n).context("a string length overflows")?;
    let s = b
        .get(*at..end)
        .context("a string ran off the end of a block")?;
    *at = end;
    String::from_utf8(s.to_vec()).context("a block holds a non-UTF-8 string")
}

impl Block {
    /// The block's bytes: a plain header, then a zstd frame holding
    /// everything else.
    ///
    /// ⚠ The header is OUTSIDE the frame so that a reader can tell what
    /// a block covers without decompressing it — which is what makes a
    /// query select blocks by range for free, and what lets a manifest
    /// be rebuilt from a directory if it is ever lost.
    pub fn encode(&self, level: i32) -> anyhow::Result<Vec<u8>> {
        let mut head = Vec::with_capacity(32);
        head.extend_from_slice(MAGIC);
        head.push(VERSION);
        put_uvarint(&mut head, self.width_ms);
        put_uvarint(&mut head, self.t0);
        put_uvarint(&mut head, self.n_buckets as u64);
        put_uvarint(&mut head, self.generation as u64);
        put_uvarint(&mut head, self.series.len() as u64);

        let mut body = Vec::new();
        // The series table: identities, once.
        for s in &self.series {
            put_str(&mut body, &s.metric);
            put_uvarint(&mut body, s.labels.len() as u64);
            for (k, v) in &s.labels {
                put_str(&mut body, k);
                put_str(&mut body, v);
            }
        }
        // Then, per series, its columns: which fields, a presence bitmap
        // each, and the present values delta-coded.
        for si in 0..self.series.len() {
            let cols = &self.cells[si];
            put_uvarint(&mut body, cols.len() as u64);
            for (f, col) in cols {
                body.push(*f as u8);
                let mut bits = vec![0u8; self.n_buckets.div_ceil(8)];
                for (i, v) in col.iter().enumerate() {
                    if v.is_some() {
                        bits[i >> 3] |= 1 << (i & 7);
                    }
                }
                body.extend_from_slice(&bits);
                // ⚠ Delta over the PRESENT values only, so a sparse
                // series pays for what it has rather than for the
                // buckets it is missing. Scaled to millis and stored as
                // an integer: every measured value in a real day's
                // tally was an integer, and a float column would cost
                // 8 bytes a cell where this costs one or two.
                let mut prev = 0i64;
                for v in col.iter().flatten() {
                    let scaled = (v * 1000.0).round() as i64;
                    put_uvarint(&mut body, zigzag(scaled - prev));
                    prev = scaled;
                }
            }
        }
        // Then ONE citation section for the block: a bitmap over buckets
        // and two columns. ⚠ The offset is delta-coded because a tape
        // offset only grows, so the deltas are small where the absolute
        // numbers are not — and there are now n_buckets of them rather
        // than one per occupied cell, which is the whole saving.
        {
            let mut bits = vec![0u8; self.n_buckets.div_ceil(8)];
            for (i, c) in self.cites.iter().enumerate() {
                if c.is_some() {
                    bits[i >> 3] |= 1 << (i & 7);
                }
            }
            body.extend_from_slice(&bits);
            let mut prev = 0i64;
            for (off, _) in self.cites.iter().flatten() {
                put_uvarint(&mut body, zigzag(*off as i64 - prev));
                prev = *off as i64;
            }
            for (_, len) in self.cites.iter().flatten() {
                put_uvarint(&mut body, *len);
            }
        }
        let mut out = head;
        let frame =
            zstd::stream::encode_all(&body[..], level).context("compressing a tally block")?;
        put_uvarint(&mut out, frame.len() as u64);
        out.extend_from_slice(&frame);
        Ok(out)
    }

    /// What a block covers, without decompressing it.
    pub fn peek(b: &[u8]) -> anyhow::Result<(u64, u64, usize, u32)> {
        if b.len() < 5 || &b[..4] != MAGIC {
            bail!(
                "not a tally block: no {} magic",
                String::from_utf8_lossy(MAGIC)
            );
        }
        if b[4] != VERSION {
            bail!(
                "a tally block of version {}; this build reads {VERSION}",
                b[4]
            );
        }
        let mut at = 5;
        let width = get_uvarint(b, &mut at)?;
        let t0 = get_uvarint(b, &mut at)?;
        let n = get_uvarint(b, &mut at)? as usize;
        let gen = get_uvarint(b, &mut at)? as u32;
        Ok((t0, width, n, gen))
    }

    pub fn decode(b: &[u8]) -> anyhow::Result<Block> {
        let (t0, width_ms, n_buckets, generation) = Block::peek(b)?;
        let mut at = 5;
        let _ = get_uvarint(b, &mut at)?; // width
        let _ = get_uvarint(b, &mut at)?; // t0
        let _ = get_uvarint(b, &mut at)?; // n_buckets
        let _ = get_uvarint(b, &mut at)?; // generation
        let n_series = get_uvarint(b, &mut at)? as usize;
        let frame_len = get_uvarint(b, &mut at)? as usize;
        let end = at
            .checked_add(frame_len)
            .context("a frame length overflows")?;
        let frame = b
            .get(at..end)
            .context("a tally block is shorter than its frame says")?;
        let body = zstd::stream::decode_all(frame).context("decompressing a tally block")?;

        let mut p = 0usize;
        let mut series = Vec::with_capacity(n_series);
        for _ in 0..n_series {
            let metric = get_str(&body, &mut p)?;
            let n_labels = get_uvarint(&body, &mut p)? as usize;
            let mut labels = Vec::with_capacity(n_labels);
            for _ in 0..n_labels {
                let k = get_str(&body, &mut p)?;
                let v = get_str(&body, &mut p)?;
                labels.push((k, v));
            }
            series.push(Series { metric, labels });
        }
        let mut cells = Vec::with_capacity(n_series);
        let bitmap_len = n_buckets.div_ceil(8);
        for _ in 0..n_series {
            let n_cols = get_uvarint(&body, &mut p)? as usize;
            let mut cols = BTreeMap::new();
            for _ in 0..n_cols {
                let tag = *body.get(p).context("a block ended before a field tag")?;
                p += 1;
                let f = Field::ALL
                    .get(tag as usize)
                    .copied()
                    .with_context(|| format!("a block names field {tag}, which is not one"))?;
                let bits = body
                    .get(p..p + bitmap_len)
                    .context("a block ended inside a presence bitmap")?
                    .to_vec();
                p += bitmap_len;
                let mut col = vec![None; n_buckets];
                let mut prev = 0i64;
                for i in 0..n_buckets {
                    if bits[i >> 3] >> (i & 7) & 1 == 1 {
                        let d = unzigzag(get_uvarint(&body, &mut p)?);
                        prev += d;
                        col[i] = Some(prev as f64 / 1000.0);
                    }
                }
                cols.insert(f, col);
            }
            cells.push(cols);
        }
        let cites = {
            let bits = body
                .get(p..p + bitmap_len)
                .context("a block ended inside its citation bitmap")?
                .to_vec();
            p += bitmap_len;
            let live: Vec<usize> = (0..n_buckets)
                .filter(|i| bits[i >> 3] >> (i & 7) & 1 == 1)
                .collect();
            let mut offs = Vec::with_capacity(live.len());
            let mut prev = 0i64;
            for _ in 0..live.len() {
                prev += unzigzag(get_uvarint(&body, &mut p)?);
                offs.push(prev as u64);
            }
            let mut col: Vec<Option<(u64, u64)>> = vec![None; n_buckets];
            for (n, i) in live.iter().enumerate() {
                col[*i] = Some((offs[n], get_uvarint(&body, &mut p)?));
            }
            col
        };
        Ok(Block {
            width_ms,
            t0,
            n_buckets,
            generation,
            series,
            cells,
            cites,
        })
    }
}

// ------------------------------------------------------------- the verbs

/// A block's file name: sortable by time, generation visible, derivable
/// from `(range, generation)`.
///
/// ⚠ Deliberately not content-addressed. A digest as the filename makes
/// replication idempotent and dedupes identical blocks, and makes a
/// directory an operator inspects completely opaque; the digest belongs
/// in the manifest, where the replication protocol wants it anyway.
pub fn block_name(t0_ms: u64, generation: u32) -> String {
    // Derived from the one UTC formatter this tree already has, rather
    // than a second conversion that could disagree with it: a naive
    // stamp read in the reader's zone is a bug this repository has had
    // once already.
    let iso = crate::bark::ms_rfc3339(t0_ms);
    let compact: String = iso
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'T')
        .collect();
    // yyyymmddThhmmss — the milliseconds go, a block start being a
    // bucket start and buckets not being sub-second.
    format!("{}.g{generation}", &compact[..compact.len() - 3])
}

/// `tally --pack DIR`: tally lines in, columnar blocks out.
///
/// A measurement rather than a feature — it reports what the grid cost
/// against the lines it was given, which is the only honest way to
/// argue about a format.
pub fn cmd_pack(dir: &std::path::Path, range_buckets: usize) -> anyhow::Result<()> {
    use std::io::BufRead;
    let mut samples = Vec::new();
    let mut markers = 0usize;
    let mut text = 0usize;
    let stdin = std::io::stdin();
    for (n, line) in stdin.lock().lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        text += line.len() + 1;
        let s = Sample::parse(&line).with_context(|| format!("stdin line {}", n + 1))?;
        // ⚠ Kept out rather than dropped: a marker says a bucket's
        // numbers are WRONG or short, and where they belong in a block
        // is not settled. Counted so the report cannot hide them.
        if s.is_marker() {
            markers += 1;
            continue;
        }
        samples.push(s);
    }
    if samples.is_empty() {
        bail!("no tally lines on stdin");
    }
    let blocks = Block::pack(&samples, range_buckets)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut on_disk = 0usize;
    let (mut present, mut room) = (0usize, 0usize);
    let mut series = 0usize;
    for b in &blocks {
        let bytes = b.encode(3)?;
        on_disk += bytes.len();
        let (p, r) = b.occupancy();
        present += p;
        room += r;
        series += b.series.len();
        let path = dir.join(block_name(b.t0, b.generation));
        // Temp-plus-rename, so a reader never sees half a block and a
        // crash leaves a file nothing references rather than a truncated
        // one something does.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
    }
    crate::note!(
        "timberfs: {} block(s), {} series, {}/{} cells present ({}%)",
        blocks.len(),
        series,
        present,
        room,
        present
            .checked_mul(100)
            .and_then(|n| n.checked_div(room))
            .unwrap_or(0)
    );
    if markers > 0 {
        crate::note!(
            "timberfs: {markers} marker(s) NOT packed — a block holds the grid, and where a              marker belongs is not settled. They are still in your input"
        );
    }
    crate::note!(
        "timberfs: {} of tally lines -> {} on disk ({:.1}x)",
        human(text),
        human(on_disk),
        text as f64 / on_disk.max(1) as f64
    );
    Ok(())
}

/// `tally --unpack DIR`: blocks back to lines.
///
/// The claim that the line format is the INTERCHANGE form and not the
/// storage — whatever read a tally store's lines still can.
pub fn cmd_unpack(dir: &std::path::Path) -> anyhow::Result<()> {
    use std::io::Write;
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_none_or(|e| e != "tmp"))
        .collect();
    // Sortable by name IS sortable by time, which is the whole reason
    // for the naming.
    names.sort();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for path in names {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let block =
            Block::decode(&bytes).with_context(|| format!("decoding {}", path.display()))?;
        for s in block.samples() {
            writeln!(out, "{}", s.render())?;
        }
    }
    out.flush()?;
    Ok(())
}

fn human(n: usize) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(line: &str) -> Sample {
        Sample::parse(line).expect(line)
    }

    fn pack1(lines: &[&str], range: usize) -> Block {
        let samples: Vec<Sample> = lines.iter().map(|l| s(l)).collect();
        let mut blocks = Block::pack(&samples, range).unwrap();
        assert_eq!(blocks.len(), 1, "expected one block");
        blocks.remove(0)
    }

    /// The whole claim: the grid renders back to the lines it was built
    /// from. If this does not hold, no size measurement means anything.
    /// ⚠ Values, identities and buckets are exact; a CITATION is not,
    /// so these lines carry none — it has its own test, asserting
    /// containment.
    #[test]
    fn a_block_renders_back_the_lines_it_was_packed_from() {
        let lines = [
            "2026-09-06T13:37:00.000Z 60s http_requests method=GET status=200 count=90",
            "2026-09-06T13:37:00.000Z 60s http_requests method=GET status=500 count=10",
            "2026-09-06T13:38:00.000Z 60s http_requests method=GET status=200 count=80 sum=4.5",
            "2026-09-06T13:39:00.000Z 60s http_bytes status=200 count=2 sum=8419221",
        ];
        let block = pack1(&lines, 60);
        let back = block.samples();
        let mut want: Vec<String> = lines.iter().map(|l| s(l).render()).collect();
        let mut got: Vec<String> = back.iter().map(|x| x.render()).collect();
        want.sort();
        got.sort();
        assert_eq!(want, got);
    }

    /// And through the bytes, which is the part a size claim rests on.
    #[test]
    fn a_block_round_trips_through_its_own_encoding() {
        let lines = [
            "2026-09-06T13:37:00.000Z 60s m a=1 count=90 sum=1.5 min=0.25 max=9 last=3",
            "2026-09-06T13:38:00.000Z 60s m a=1 count=80",
            "2026-09-06T13:39:00.000Z 60s m a=2 count=1",
        ];
        let block = pack1(&lines, 60);
        let bytes = block.encode(3).unwrap();
        let back = Block::decode(&bytes).unwrap();
        assert_eq!(block, back);
        assert_eq!(
            back.samples()
                .iter()
                .map(|x| x.render())
                .collect::<Vec<_>>(),
            block
                .samples()
                .iter()
                .map(|x| x.render())
                .collect::<Vec<_>>()
        );
    }

    /// ⚠ Tally's second invariant, and the reason for the bitmap: a
    /// bucket a series was absent from must not come back as a zero. A
    /// metric added today leaves last week UNKNOWN.
    #[test]
    fn an_absent_bucket_is_not_a_zero() {
        let block = pack1(
            &[
                "2026-09-06T13:37:00.000Z 60s m count=5",
                "2026-09-06T13:39:00.000Z 60s m count=7",
            ],
            60,
        );
        let back = Block::decode(&block.encode(3).unwrap()).unwrap();
        let got: Vec<String> = back.samples().iter().map(|x| x.render()).collect();
        assert_eq!(
            got,
            vec![
                "2026-09-06T13:37:00.000Z 60s m count=5",
                "2026-09-06T13:39:00.000Z 60s m count=7",
            ],
            "13:38 was absent and must stay absent, not become count=0"
        );
        let (present, room) = back.occupancy();
        assert_eq!((present, room), (2, 60));
    }

    /// A block's coverage is readable without decompressing it, which is
    /// what lets a query select blocks and a manifest be rebuilt.
    #[test]
    fn what_a_block_covers_is_readable_without_decompressing_it() {
        let block = pack1(&["2026-09-06T13:37:00.000Z 60s m count=1"], 60);
        let bytes = block.encode(3).unwrap();
        let (t0, width, n, gen) = Block::peek(&bytes).unwrap();
        assert_eq!((t0, width, n, gen), (block.t0, 60_000, 60, 0));
    }

    /// A boundary is a multiple of the range, so which block a bucket
    /// lands in does not depend on what else arrived with it.
    #[test]
    fn a_block_boundary_does_not_move_with_its_input() {
        let all = [
            "2026-09-06T13:00:00.000Z 60s m count=1",
            "2026-09-06T14:30:00.000Z 60s m count=2",
        ];
        let both = Block::pack(&all.iter().map(|l| s(l)).collect::<Vec<_>>(), 60).unwrap();
        assert_eq!(both.len(), 2, "an hour apart, one block an hour");
        let second_alone = Block::pack(&[s(all[1])], 60).unwrap();
        assert_eq!(
            second_alone[0].t0, both[1].t0,
            "the same bucket must land at the same t0 alone as in company"
        );
    }

    /// Markers are refused rather than dropped: they say a bucket's
    /// numbers are wrong or short, which is the last thing to lose
    /// quietly.
    #[test]
    fn a_marker_is_refused_rather_than_swallowed() {
        let e = Block::pack(
            &[s("2026-09-06T13:37:00.000Z 60s !cap metric=m count=3")],
            60,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("marker"), "unhelpful: {e}");
    }

    /// Observations are not buckets. Folding is a separate operation and
    /// packing must not appear to do it.
    #[test]
    fn observations_are_refused_because_a_cell_is_a_bucket() {
        let e = Block::pack(&[s("2026-09-06T13:37:10.000Z 0s m count=1")], 60)
            .unwrap_err()
            .to_string();
        assert!(e.contains("fold"), "unhelpful: {e}");
    }

    /// One block is one width, because a cell's POSITION is its bucket.
    #[test]
    fn two_widths_in_one_block_are_refused() {
        let e = Block::pack(
            &[
                s("2026-09-06T13:37:00.000Z 60s m count=1"),
                s("2026-09-06T13:40:00.000Z 300s m count=1"),
            ],
            60,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("one width"), "unhelpful: {e}");
    }

    /// ⚠ A citation comes back WIDENED to its bucket's span, and the
    /// property is therefore CONTAINMENT rather than equality. Asserted
    /// with TWO series in one bucket, because with one series a bucket's
    /// union is that series' own span and the test would pass whether
    /// the widening happened or not.
    #[test]
    fn a_citation_is_widened_to_its_bucket_and_still_contains() {
        let lines = [
            "2026-09-06T13:37:00.000Z 60s m a=1 count=5 @1000+100",
            "2026-09-06T13:37:00.000Z 60s m a=2 count=5 @5000+100",
            "2026-09-06T13:38:00.000Z 60s m a=1 count=7 @9000+50",
            "2026-09-06T13:40:00.000Z 60s m a=1 count=1",
        ];
        let block = pack1(&lines, 60);
        let got = Block::decode(&block.encode(3).unwrap()).unwrap().samples();

        // Both series in 13:37 cite the UNION, 1000..5100.
        let at37: Vec<_> = got.iter().filter(|x| x.ts == s(lines[0]).ts).collect();
        assert_eq!(at37.len(), 2, "both series came back");
        for x in &at37 {
            assert_eq!(x.cite, Some((1000, 4100)), "the bucket's union, widened");
        }

        // Containment, for every line that carried one.
        for want in lines.iter().map(|l| s(l)) {
            let Some((wo, wl)) = want.cite else { continue };
            let had = got
                .iter()
                .find(|x| x.ts == want.ts && x.labels == want.labels)
                .expect("the line came back");
            let (go, gl) = had.cite.expect("its bucket cited something");
            assert!(
                go <= wo && go + gl >= wo + wl,
                "the widened span {go}+{gl} must contain {wo}+{wl}"
            );
        }

        // And a bucket nothing cited cites nothing — absent is not zero
        // here either.
        let at40 = got.iter().find(|x| x.ts == s(lines[3]).ts).unwrap();
        assert_eq!(at40.cite, None);
    }

    /// The name is the address: sortable by time, generation visible.
    #[test]
    fn a_block_name_sorts_by_time_and_shows_its_generation() {
        let a = block_name(
            crate::tally::parse_stamp("2026-09-06T00:00:00.000Z").unwrap(),
            0,
        );
        let b = block_name(
            crate::tally::parse_stamp("2026-09-07T00:00:00.000Z").unwrap(),
            2,
        );
        assert_eq!(a, "20260906T000000.g0");
        assert_eq!(b, "20260907T000000.g2");
        assert!(a < b, "directory order must be time order");
    }

    /// Garbage in must not panic: a block arrives over a wire.
    #[test]
    fn a_truncated_or_foreign_block_is_an_error_not_a_panic() {
        assert!(Block::decode(b"").is_err());
        assert!(Block::decode(b"NOPE1234").is_err());
        let good = pack1(&["2026-09-06T13:37:00.000Z 60s m count=1"], 60)
            .encode(3)
            .unwrap();
        for cut in 0..good.len() {
            let _ = Block::decode(&good[..cut]); // must not panic
        }
        let mut bad = good.clone();
        bad[4] = 99;
        assert!(Block::decode(&bad).is_err(), "a future version is refused");
    }
}
