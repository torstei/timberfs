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

/// One series' identity, written once per block. Which DEFINITION
/// produced it is on `Block::definitions`, keyed by metric.
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

impl Series {
    /// The series as a LABEL MAP, so the selector that picks stores
    /// picks series too.
    ///
    /// ⚠ The metric goes in under the key `metric`, which is what
    /// [tally.md](../docs/plans/tally.md) already said the read side
    /// would do: the name is POSITIONAL in a line and a selectable KEY
    /// in a parsed one, the same relation a store's `name` and `id`
    /// already have. So `[metric=http_requests,status=500]` works with
    /// the operators `--select` already has — regex, negation, substring
    /// — rather than a second predicate language for series.
    pub fn as_fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert(
            "metric".to_string(),
            serde_json::Value::String(self.metric.clone()),
        );
        for (k, v) in &self.labels {
            m.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        m
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
    /// Which definition produced each metric, by the id assigned in the
    /// tally's own definitions (docs/plans/tally-series-identity.md).
    ///
    /// ⚠ Keyed by metric and not by series because a metric maps to one
    /// definition, so a field on `Series` would have many homes for one
    /// fact. The wire form is a metric table the series index into,
    /// DERIVED at encode time rather than stored beside them, so the two
    /// cannot disagree.
    ///
    /// ⚠ A metric may legitimately be ABSENT from this, permanently: a
    /// block packed from tally lines has no definition to record, the
    /// interchange form carrying none. So absence is not definition 0 —
    /// zero and unknown are different, as they are for a cell.
    pub definitions: BTreeMap<String, u32>,
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
            out.push(Block::at(&group, t0, width, range_buckets));
        }
        Ok(out)
    }

    /// One block over an explicit range, which `pack` uses per group and
    /// the open region uses to cover exactly its own span rather than a
    /// floored block it might straddle.
    fn at(group: &[&Sample], t0: u64, width: u64, range_buckets: usize) -> Block {
        {
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
            Block {
                width_ms: width,
                t0,
                n_buckets: range_buckets,
                generation: 0,
                series,
                definitions: BTreeMap::new(),
                cells,
                cites,
            }
        }
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
        // The metric table, then the series that index into it. Derived
        // here rather than held beside the series so the two cannot
        // disagree; first-appearance order, the series being sorted.
        let mut order: Vec<&str> = Vec::new();
        let mut index: BTreeMap<&str, usize> = BTreeMap::new();
        for s in &self.series {
            index.entry(&s.metric).or_insert_with(|| {
                order.push(&s.metric);
                order.len() - 1
            });
        }
        put_uvarint(&mut body, order.len() as u64);
        for m in &order {
            put_str(&mut body, m);
            // Biased by one, so 0 can mean "not recorded" and stay
            // distinguishable from definition 0.
            let d = self.definitions.get(*m).map_or(0, |d| *d as u64 + 1);
            put_uvarint(&mut body, d);
        }
        for s in &self.series {
            put_uvarint(&mut body, index[s.metric.as_str()] as u64);
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

    /// How many bytes the block at the front of `b` occupies — read
    /// from its header, so several can sit back to back in one file.
    pub fn size_of(b: &[u8]) -> anyhow::Result<usize> {
        Block::peek(b)?;
        let mut at = 5;
        for _ in 0..5 {
            get_uvarint(b, &mut at)?; // width, t0, n_buckets, generation, n_series
        }
        let frame = get_uvarint(b, &mut at)? as usize;
        at.checked_add(frame).context("a frame length overflows")
    }

    /// Decode only the series a selector picks, and only the buckets in
    /// `[from, to]`.
    ///
    /// ⚠ **The point is what is NOT done.** The series table is at the
    /// front, so the identities are read, matched, and then every
    /// non-matching series' columns are STEPPED OVER rather than
    /// materialised — one selective read of a 992-series day touches the
    /// columns of the series asked for and no others. The frame is still
    /// decompressed whole, which is the floor: zstd has no seek, and
    /// framing per series to get one would cost compression on every
    /// block to speed up a minority of reads. Measured either way before
    /// choosing (see docs/plans/tally-as-a-tally.md).
    pub fn select(
        b: &[u8],
        sel: &crate::select::Selector,
        from: u64,
        to: u64,
    ) -> anyhow::Result<Vec<Sample>> {
        let (block, _, _) = Block::decode_matching(b, sel)?;
        Ok(block
            .samples()
            .into_iter()
            .filter(|s| s.ts >= from && s.ts <= to)
            .collect())
    }

    /// The block with only the matching series' cells populated, and
    /// how many series were matched and stepped over.
    fn decode_matching(
        b: &[u8],
        sel: &crate::select::Selector,
    ) -> anyhow::Result<(Block, usize, usize)> {
        Block::decode_inner(b, Some(sel))
    }

    pub fn decode(b: &[u8]) -> anyhow::Result<Block> {
        Ok(Block::decode_inner(b, None)?.0)
    }

    fn decode_inner(
        b: &[u8],
        sel: Option<&crate::select::Selector>,
    ) -> anyhow::Result<(Block, usize, usize)> {
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
        let n_metrics = get_uvarint(&body, &mut p)? as usize;
        let mut names: Vec<String> = Vec::with_capacity(n_metrics);
        let mut definitions: BTreeMap<String, u32> = BTreeMap::new();
        for _ in 0..n_metrics {
            let name = get_str(&body, &mut p)?;
            let d = get_uvarint(&body, &mut p)?;
            if let Some(d) = d.checked_sub(1) {
                definitions.insert(
                    name.clone(),
                    u32::try_from(d).context("a definition id does not fit")?,
                );
            }
            names.push(name);
        }
        let mut series = Vec::with_capacity(n_series);
        for _ in 0..n_series {
            let mi = get_uvarint(&body, &mut p)? as usize;
            let metric = names
                .get(mi)
                .context("a series names a metric outside the block's own table")?
                .clone();
            let n_labels = get_uvarint(&body, &mut p)? as usize;
            let mut labels = Vec::with_capacity(n_labels);
            for _ in 0..n_labels {
                let k = get_str(&body, &mut p)?;
                let v = get_str(&body, &mut p)?;
                labels.push((k, v));
            }
            series.push(Series { metric, labels });
        }
        // ⚠ Matched BEFORE the columns are read, which is the whole
        // saving: a series the selector did not pick has its columns
        // stepped over — the varints are counted off the presence bits
        // and discarded — rather than decoded into a vector nobody
        // asked for.
        let wanted: Vec<bool> = match sel {
            None => vec![true; n_series],
            Some(sel) => series.iter().map(|s| sel.matches(&s.as_fields())).collect(),
        };
        let mut cells = Vec::with_capacity(n_series);
        let bitmap_len = n_buckets.div_ceil(8);
        for want in wanted.iter().copied() {
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
                    .context("a block ended inside a presence bitmap")?;
                let present = bits.iter().map(|b| b.count_ones() as usize).sum::<usize>();
                let bits = bits.to_vec();
                p += bitmap_len;
                if !want {
                    // Step over: the column holds one varint per present
                    // bit, and a varint's end is its own high bit.
                    for _ in 0..present {
                        get_uvarint(&body, &mut p)?;
                    }
                    continue;
                }
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
        let matched = wanted.iter().filter(|w| **w).count();
        Ok((
            Block {
                width_ms,
                t0,
                n_buckets,
                generation,
                series,
                definitions,
                cells,
                cites,
            },
            matched,
            wanted.len() - matched,
        ))
    }
}

// -------------------------------------------------------------- the manifest

/// The manifest's file name, beside the store's `.bark`.
pub const MANIFEST: &str = "manifest.json";

/// One block, as the manifest knows it — enough to plan a query and to
/// diff against another holder, without opening a block.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// The first bucket the block covers.
    pub t0: u64,
    pub n_buckets: usize,
    /// Which derivation of this range. The address is
    /// `(t0, generation)`, never a position in a sequence.
    pub generation: u32,
    pub bytes: u64,
    /// ⚠ A crc32, and deliberately not a cryptographic digest. Its job
    /// is "does the far end hold THESE bytes" and "did this file rot",
    /// not resisting an adversary who is already writing to the store
    /// directory. At a 730-block retention the collision chance is
    /// ~4e-6, and the tree already has this crc32 with a check-value
    /// test rather than a dependency to add.
    pub crc32: u32,
}

impl Entry {
    pub fn file_name(&self) -> String {
        block_name(self.t0, self.generation)
    }

    /// Does this block hold any bucket in `[from, to]`?
    fn covers(&self, from: u64, to: u64, width: u64) -> bool {
        let end = self.t0 + self.n_buckets as u64 * width;
        self.t0 <= to && end > from
    }
}

/// What a tally store IS: the blocks it has, and where its history stops.
///
/// ⚠ **The manifest is the COMMIT POINT**, and that is the whole reason
/// it exists rather than the directory being the truth. Writing a block
/// then rewriting the manifest then unlinking the superseded one means a
/// crash in either window leaves a file NOTHING REFERENCES — collectable
/// debris — instead of a store that lies. With the directory as truth,
/// every partial write would be a corrupt store, which is the trade
/// `.bark`'s temp-plus-rename already makes in this tree.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub v: u32,
    /// This store's own identity, minted once and never touched after —
    /// bark's rule, and for bark's reason: a path is an address and the
    /// id is what the store IS, across renames, moves and copies.
    ///
    /// ⚠ `Option` because a manifest written before identity existed has
    /// none, and minting one on read would hand an old store a new
    /// identity in silence. Missing is a defect to repair deliberately,
    /// which is how `timberfs identity` treats the same absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// When that identity was established, RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// The SOURCE store's id, which is load-bearing rather than
    /// provenance: a citation is an offset into that store's tape and
    /// means nothing without knowing which tape. Absent for a store
    /// packed from tally lines, which came from no store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The source's labels, copied when this store was created.
    ///
    /// ⚠ A record of what the source said THEN, never a live view: the
    /// source may be relabelled, renamed or deleted long before a
    /// two-year tally, and when it is deleted this is the only surviving
    /// witness of what was measured.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub labels: serde_json::Map<String, serde_json::Value>,
    /// Retention: how long, and how much.
    ///
    /// ⚠ Numbers where a `.bark` holds the strings an operator wrote,
    /// because that file holds a DECLARATION and this holds the resolved
    /// policy. And `retain_ms` is checked to be a whole number of BLOCKS
    /// (`set_retain`), retention dropping whole blocks and nothing
    /// finer — so a value between two of them would be a rounding
    /// dressed as a setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_bytes: Option<u64>,
    pub width_ms: u64,
    pub block_buckets: usize,
    /// The oldest bucket this store still claims to know about.
    ///
    /// ⚠ Not decoration: it separates DROPPED from NEVER WRITTEN, which
    /// is tally's "zero and unknown are different" one level up. Without
    /// it a window retention has taken away and a window nothing ever
    /// measured are the same empty answer, and only one of them means
    /// the numbers were there once.
    pub floor: u64,
    pub blocks: Vec<Entry>,
}

impl Manifest {
    pub fn new(width_ms: u64, block_buckets: usize) -> Manifest {
        Manifest {
            v: 1,
            id: crate::bark::new_uuid().ok(),
            created: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            source: None,
            labels: serde_json::Map::new(),
            retain_ms: None,
            retain_bytes: None,
            width_ms,
            block_buckets,
            floor: 0,
            blocks: Vec::new(),
        }
    }

    /// How long one block covers, which is retention's granularity.
    pub fn block_span_ms(&self) -> u64 {
        self.width_ms * self.block_buckets as u64
    }

    /// Declare how long to keep. ⚠ Refused unless it is a whole number
    /// of blocks: `drop_before` removes blocks whole, so a finer value
    /// would be a promise this store cannot keep.
    pub fn set_retain(&mut self, ms: Option<u64>) -> anyhow::Result<()> {
        if let Some(ms) = ms {
            let span = self.block_span_ms();
            if span == 0 || ms == 0 || ms % span != 0 {
                bail!(
                    "a retention of {} is not a whole number of blocks — this store's block \
                     covers {}, and retention drops blocks whole",
                    crate::tally::render_width(ms),
                    crate::tally::render_width(span)
                );
            }
        }
        self.retain_ms = ms;
        Ok(())
    }

    /// Record which store the numbers came from.
    ///
    /// ⚠ Refused when it disagrees with what is recorded: a store's
    /// source does not change, and feeding one store's records into
    /// another's blocks would leave every citation pointing into the
    /// wrong tape — silently, the offsets being plausible either way.
    pub fn set_source(&mut self, id: &str) -> anyhow::Result<bool> {
        match &self.source {
            Some(had) if had == id => Ok(false),
            Some(had) => bail!(
                "these blocks were derived from store {had} and this run reads {id} — a \
                 citation is an offset into ONE tape"
            ),
            None => {
                self.source = Some(id.to_string());
                Ok(true)
            }
        }
    }

    /// Read it, or `None` where a store has none yet.
    pub fn load(dir: &std::path::Path) -> anyhow::Result<Option<Manifest>> {
        let path = dir.join(MANIFEST);
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let m: Manifest = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a tally manifest", path.display()))?;
        if m.v != 1 {
            bail!(
                "{} is a manifest of version {}; this build reads 1",
                path.display(),
                m.v
            );
        }
        Ok(Some(m))
    }

    /// Write it. **This is the commit** — temp-plus-rename, so a reader
    /// sees the old manifest or the new one and never half of either.
    pub fn save(&self, dir: &std::path::Path) -> anyhow::Result<()> {
        let path = dir.join(MANIFEST);
        let tmp = dir.join(format!("{MANIFEST}.tmp"));
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
        Ok(())
    }

    /// The blocks holding any bucket in `[from, to]`.
    ///
    /// ⚠ The query-planning property, and it costs no data: a block's
    /// coverage is in the manifest, so a window selects files before
    /// anything is decompressed — and the columns of the series that did
    /// not match are never read at all.
    pub fn covering(&self, from: u64, to: u64) -> Vec<&Entry> {
        self.blocks
            .iter()
            .filter(|e| e.covers(from, to, self.width_ms))
            .collect()
    }

    /// Put this block in, replacing any other generation of its range.
    ///
    /// Returns the file names now superseded, for the caller to unlink
    /// AFTER the manifest is saved — which is the order that makes a
    /// crash benign.
    pub fn put(&mut self, e: Entry) -> Vec<String> {
        let mut superseded = Vec::new();
        self.blocks.retain(|had| {
            if had.t0 == e.t0 && had.generation != e.generation {
                superseded.push(had.file_name());
                false
            } else {
                had.t0 != e.t0
            }
        });
        self.blocks.push(e);
        self.blocks.sort_by_key(|e| (e.t0, e.generation));
        superseded
    }

    /// Head-drop: forget every block ending at or before `t`, and raise
    /// the floor to say so. Returns what to unlink after the save.
    pub fn drop_before(&mut self, t: u64) -> Vec<String> {
        let width = self.width_ms;
        let mut gone = Vec::new();
        self.blocks.retain(|e| {
            if e.t0 + e.n_buckets as u64 * width <= t {
                gone.push(e.file_name());
                false
            } else {
                true
            }
        });
        // ⚠ Raised even when nothing was dropped: the floor is a claim
        // about what this store no longer answers for, and a retention
        // sweep that found nothing to drop has still moved that line.
        self.floor = self.floor.max(t);
        gone
    }

    /// Files in the directory that the manifest does not name — crash
    /// debris from either window, and safe to remove.
    ///
    /// ⚠ Never the open region or the manifest itself, which are not
    /// blocks; and never a `.tmp`, which a concurrent writer may be
    /// mid-rename on.
    pub fn unreferenced(&self, dir: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
        let named: std::collections::BTreeSet<String> =
            self.blocks.iter().map(|e| e.file_name()).collect();
        let mut out = Vec::new();
        for ent in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = ent?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // ⚠ Every other entry is taken for a block, so a file this
            // store grows later must be named here or be reported as
            // debris — the definitions set is the one already designed
            // (docs/plans/tally-series-identity.md).
            if name == MANIFEST || name.ends_with(".tmp") {
                continue;
            }
            if !named.contains(name) {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }
}

impl Block {
    /// This block with `other`'s cells folded in — for a range that is
    /// still being filled.
    ///
    /// ⚠ A cell present in both takes the NEW value, which is the same
    /// newest-wins rule a tape already had for two lines of one bucket.
    /// Both blocks must cover the same range: merging across ranges
    /// would silently move numbers between buckets.
    pub fn merge(&self, other: &Block) -> anyhow::Result<Block> {
        if (self.t0, self.n_buckets, self.width_ms) != (other.t0, other.n_buckets, other.width_ms) {
            bail!(
                "two blocks over different ranges cannot merge ({}+{} against {}+{})",
                self.t0,
                self.n_buckets,
                other.t0,
                other.n_buckets
            );
        }
        let mut out = self.clone();
        for (m, d) in &other.definitions {
            // Within one tally a metric has one definition, and both
            // blocks are of one tally — `commit` finds the other
            // through this store's own manifest. So a disagreement is a
            // bug in whatever built them, not a case to pick a winner
            // for. ⚠ It is NOT a guard against merging two tallies: an
            // id is local to a store, so two of them can agree on the
            // number and mean different definitions.
            match out.definitions.get(m) {
                Some(had) if had != d => bail!(
                    "{m} is definition {had} in one block and {d} in the other, which cannot \
                     both be true of one tally"
                ),
                _ => out.definitions.insert(m.clone(), *d),
            };
        }
        for (i, s) in other.series.iter().enumerate() {
            let at = match out.series.iter().position(|had| had == s) {
                Some(at) => at,
                None => {
                    out.series.push(s.clone());
                    out.cells.push(BTreeMap::new());
                    out.series.len() - 1
                }
            };
            for (f, col) in &other.cells[i] {
                let mine = out.cells[at]
                    .entry(*f)
                    .or_insert_with(|| vec![None; out.n_buckets]);
                for (b, v) in col.iter().enumerate() {
                    if v.is_some() {
                        mine[b] = *v;
                    }
                }
            }
        }
        for (b, c) in other.cites.iter().enumerate() {
            if let Some((off, len)) = c {
                out.cites[b] = match out.cites[b] {
                    None => Some((*off, *len)),
                    Some((l, n)) => {
                        let (lo, hi) = (l.min(*off), (l + n).max(off + len));
                        Some((lo, hi - lo))
                    }
                };
            }
        }
        Ok(out)
    }
}

/// What a commit into a range that already has a block should do.
///
/// ⚠ **Never inferred, and this enum exists because inferring it lost
/// data.** A first version treated "I already hold this range" as a
/// regeneration and superseded the block. Real logs do not rotate on UTC
/// midnight, so each day's tally spills a few buckets into the previous
/// day — and that spill replaced a full 900 KB block with 22 KB of
/// overlap, silently. A partial write to a range is an ADDITION; only a
/// caller knows when it is a re-derivation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// Fold into whatever the range holds, at the same generation. What
    /// a range still being filled wants.
    Merge,
    /// Replace the range: a new generation, the old block unlinked once
    /// the manifest no longer names it.
    Regenerate,
}

/// Write a block and commit it, in the order that makes a crash benign.
///
/// 1. the block file, temp-plus-renamed;
/// 2. the manifest — **the commit**;
/// 3. the superseded block, unlinked.
///
/// ⚠ A crash between 1 and 2 leaves a block nothing references and a
/// store still answering from the previous generation. A crash between 2
/// and 3 leaves the previous generation unreferenced and the store
/// answering from the new one. Both are `unreferenced` debris; neither
/// is a store that lies, and that is the only property this order buys.
pub fn commit(
    dir: &std::path::Path,
    m: &mut Manifest,
    block: &Block,
    level: i32,
    how: How,
) -> anyhow::Result<Entry> {
    // What the range already holds decides what this write IS.
    let existing = m.blocks.iter().find(|e| e.t0 == block.t0).cloned();
    let block = match (&existing, how) {
        (Some(e), How::Merge) => {
            // ⚠ Read it back and fold, at the SAME generation: a range
            // being filled is one derivation arriving in pieces, not
            // several derivations.
            let had = read_block(dir, e)?;
            let mut merged = had.merge(block)?;
            merged.generation = e.generation;
            std::borrow::Cow::Owned(merged)
        }
        (Some(e), How::Regenerate) => {
            let mut next = block.clone();
            next.generation = e.generation + 1;
            std::borrow::Cow::Owned(next)
        }
        (None, _) => std::borrow::Cow::Borrowed(block),
    };
    let block = block.as_ref();
    let bytes = block.encode(level)?;
    let e = Entry {
        t0: block.t0,
        n_buckets: block.n_buckets,
        generation: block.generation,
        bytes: bytes.len() as u64,
        crc32: crate::sap::crc32(&bytes),
    };
    let path = dir.join(e.file_name());
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
    let superseded = m.put(e.clone());
    m.save(dir)?;
    for name in superseded {
        let old = dir.join(name);
        match std::fs::remove_file(&old) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("removing {}", old.display())),
        }
    }
    Ok(e)
}

/// Read a block the manifest names, checking it is the bytes the
/// manifest recorded.
pub fn read_block(dir: &std::path::Path, e: &Entry) -> anyhow::Result<Block> {
    let path = dir.join(e.file_name());
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let had = crate::sap::crc32(&bytes);
    if had != e.crc32 {
        bail!(
            "{}: the manifest records crc32 {:#010x} and the file is {:#010x} — the block \
             changed under the manifest, which a sealed block may not do",
            path.display(),
            e.crc32,
            had
        );
    }
    Block::decode(&bytes).with_context(|| format!("decoding {}", path.display()))
}

// ------------------------------------------------------------ the open edge

// ------------------------------------------------------------------ reading

/// What a read touched, so a caller can say what it did NOT do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Touched {
    pub blocks_in_manifest: usize,
    pub blocks_opened: usize,
    pub bytes_read: u64,
    pub series_matched: usize,
    pub series_skipped: usize,
}

/// Answer a window and a series selector out of a tally store.
///
/// Two prunings, and neither reads a number it does not need:
///
/// 1. **the manifest** says which blocks hold a bucket in the window, so
///    the rest are never opened — no bytes, no decompression;
/// 2. **the series table** at the front of each opened block says which
///    series it holds, so the columns of those the selector did not pick
///    are stepped over.
///
/// ⚠ A bucket still filling is in a block like any other, so there is
/// one source here and not two: a block is rewritten as more of its
/// range arrives, up to the manifest's floor.
pub fn query(
    dir: &std::path::Path,
    sel: &crate::select::Selector,
    from: u64,
    to: u64,
) -> anyhow::Result<(Vec<Sample>, Touched)> {
    let m =
        Manifest::load(dir)?.with_context(|| format!("{} holds no {MANIFEST}", dir.display()))?;
    let mut out = Vec::new();
    let mut t = Touched {
        blocks_in_manifest: m.blocks.len(),
        ..Default::default()
    };
    for e in m.covering(from, to) {
        let path = dir.join(e.file_name());
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if crate::sap::crc32(&bytes) != e.crc32 {
            bail!(
                "{}: the manifest records crc32 {:#010x} and the file is {:#010x}",
                path.display(),
                e.crc32,
                crate::sap::crc32(&bytes)
            );
        }
        t.blocks_opened += 1;
        t.bytes_read += bytes.len() as u64;
        // ⚠ ONE pass. A first version decoded the block whole to count
        // its series and then decoded it again selectively, which did
        // exactly the work the selection exists to avoid — so the pass
        // reports what it matched and stepped over.
        let (block, matched, skipped) = Block::decode_matching(&bytes, sel)?;
        t.series_matched += matched;
        t.series_skipped += skipped;
        out.extend(
            block
                .samples()
                .into_iter()
                .filter(|s| s.ts >= from && s.ts <= to),
        );
    }
    out.sort_by(|a, b| (a.ts, &a.metric, &a.labels).cmp(&(b.ts, &b.metric, &b.labels)));
    Ok((out, t))
}

/// Writes sealed samples into a store's blocks as they arrive.
///
/// ⚠ It BUFFERS, and the buffer is the write-amplification control. A
/// block is a day, so committing one sample rewrites up to a megabyte;
/// buffering `limit` samples makes that once per `limit` instead. Two
/// costs: a sample is not in a block until it is flushed, and the
/// buffer is not durable — losing it to a crash costs a re-read, the
/// position not having moved.
pub struct Writer {
    dir: std::path::PathBuf,
    m: Manifest,
    block_buckets: usize,
    level: i32,
    limit: usize,
    held: Vec<Sample>,
    pub committed: usize,
    pub blocks_written: usize,
    pub markers: usize,
}

impl Writer {
    /// Open a store to write into, creating its manifest if there is
    /// none. ⚠ The width is the WINDOW's, and a manifest that disagrees
    /// is refused rather than adopted: a store holds one bucket width,
    /// and coarsening is a different operation on a column.
    pub fn open(
        dir: &std::path::Path,
        width_ms: u64,
        block_buckets: usize,
        limit: usize,
        level: i32,
    ) -> anyhow::Result<Writer> {
        if block_buckets == 0 {
            bail!("a block spans at least one bucket");
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let m = match Manifest::load(dir)? {
            Some(m) => {
                if m.width_ms != width_ms {
                    bail!(
                        "{} holds {} buckets and this run makes {}",
                        dir.display(),
                        crate::tally::render_width(m.width_ms),
                        crate::tally::render_width(width_ms)
                    );
                }
                m
            }
            None => Manifest::new(width_ms, block_buckets),
        };
        Ok(Writer {
            dir: dir.to_path_buf(),
            m,
            block_buckets,
            level,
            limit: limit.max(1),
            held: Vec::new(),
            committed: 0,
            blocks_written: 0,
            markers: 0,
        })
    }

    /// Record which store these numbers come from, refusing a second
    /// one — see `Manifest::set_source`. It reaches disk with the next
    /// commit, the manifest being the commit point: a run that learns a
    /// source and writes no block has nothing to attribute.
    pub fn source_is(&mut self, id: &str) -> anyhow::Result<()> {
        self.m.set_source(id)?;
        Ok(())
    }

    /// Take a batch. Markers are counted and not stored — a marker
    /// states something about a RUN and the grid holds numbers
    /// (docs/plans/tally-design.md).
    pub fn take(&mut self, batch: Vec<Sample>) -> anyhow::Result<()> {
        for s in batch {
            if s.is_marker() {
                self.markers += 1;
            } else {
                self.held.push(s);
            }
        }
        if self.held.len() >= self.limit {
            self.flush()?;
        }
        Ok(())
    }

    /// Commit what is held. ⚠ `How::Merge`, always: a range being
    /// filled is one derivation arriving in pieces, and inferring a
    /// regeneration from having seen the range before is the defect that
    /// silently replaced three of six real days.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        if self.held.is_empty() {
            return Ok(());
        }
        let held = std::mem::take(&mut self.held);
        let n = held.len();
        for b in Block::pack(&held, self.block_buckets)? {
            commit(&self.dir, &mut self.m, &b, self.level, How::Merge)?;
            self.blocks_written += 1;
        }
        self.committed += n;
        Ok(())
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
    let width = blocks[0].width_ms;
    // An existing manifest is EXTENDED rather than replaced: packing a
    // second day into a store must not forget the first, and packing the
    // same day again must supersede it rather than sit beside it.
    let mut m = match Manifest::load(dir)? {
        Some(m) => m,
        None => Manifest::new(width, range_buckets),
    };
    let mut on_disk = 0usize;
    let (mut present, mut room) = (0usize, 0usize);
    let mut series = 0usize;
    let mut merged = 0usize;
    for b in &blocks {
        let (p, r) = b.occupancy();
        present += p;
        room += r;
        series += b.series.len();
        // ⚠ Re-packing a range it already holds is a REGENERATION, so
        // the generation goes up and the old block is superseded — which
        // is the whole point of addressing a block by
        // `(range, generation)` instead of a position.
        // ⚠ MERGE, never a regeneration inferred from having seen the
        // range: a log that does not rotate on UTC midnight spills into
        // the previous day, and treating that spill as a re-derivation
        // replaced a full day with the overlap.
        if m.blocks.iter().any(|e| e.t0 == b.t0) {
            merged += 1;
        }
        let e = commit(dir, &mut m, b, 3, How::Merge)?;
        on_disk += e.bytes as usize;
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
    if merged > 0 {
        crate::note!(
            "timberfs: {merged} range(s) already held a block and were MERGED into — a range \
             fills in pieces, and re-deriving one is a different operation asked for outright"
        );
    }
    crate::note!(
        "timberfs: {} of tally lines -> {} in the block(s) touched ({:.1}x)",
        human(text),
        human(on_disk),
        text as f64 / on_disk.max(1) as f64
    );
    let debris = m.unreferenced(dir)?;
    if !debris.is_empty() {
        crate::note!(
            "timberfs: {} file(s) here are named by no manifest entry — debris from an              interrupted commit, safe to remove",
            debris.len()
        );
    }
    Ok(())
}

/// `tally --query DIR --series '[...]'`: the two prunings, with an
/// account of what was NOT touched.
///
/// ⚠ The account is the interesting half. Block selection precedes
/// reading, so this can state what it kept shut — which most log stores
/// cannot answer without doing the work first.
pub fn cmd_query(
    dir: &std::path::Path,
    expr: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> anyhow::Result<()> {
    use std::io::Write;
    let sel = crate::select::Selector::parse(expr)?;
    let from_ms = match from {
        Some(t) => crate::tally::parse_stamp(t)?,
        None => 0,
    };
    let to_ms = match to {
        Some(t) => crate::tally::parse_stamp(t)?,
        None => u64::MAX,
    };
    let began = std::time::Instant::now();
    let (rows, touched) = query(dir, &sel, from_ms, to_ms)?;
    let took = began.elapsed();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for s in &rows {
        writeln!(out, "{}", s.render())?;
    }
    out.flush()?;
    crate::note!(
        "timberfs: {} line(s) in {:?} — opened {} of {} block(s), {} read; {} series matched, \
         {} stepped over",
        rows.len(),
        took,
        touched.blocks_opened,
        touched.blocks_in_manifest,
        human(touched.bytes_read as usize),
        touched.series_matched,
        touched.series_skipped
    );
    Ok(())
}

/// `tally --unpack DIR`: blocks back to lines.
///
/// The claim that the line format is the INTERCHANGE form and not the
/// storage — whatever read a tally store's lines still can.
pub fn cmd_unpack(dir: &std::path::Path) -> anyhow::Result<()> {
    use std::io::Write;
    // ⚠ Through the MANIFEST and not the directory, which is what makes
    // an interrupted commit invisible: a block the manifest does not
    // name is debris, and reading the directory would serve it.
    let m =
        Manifest::load(dir)?.with_context(|| format!("{} holds no {MANIFEST}", dir.display()))?;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for e in &m.blocks {
        let block = read_block(dir, e)?;
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

    /// A metric's identity is written once for the block, not once per
    /// series, and the definition that produced it rides on that table.
    /// The table is derived at encode time, so this is also the test
    /// that a series can still name its metric after a round trip.
    #[test]
    fn a_metric_is_named_once_and_carries_its_definition() {
        let mut block = pack1(
            &[
                "2026-09-06T13:37:00.000Z 60s m a=1 count=1",
                "2026-09-06T13:37:00.000Z 60s m a=2 count=2",
                "2026-09-06T13:37:00.000Z 60s other a=1 count=3",
            ],
            60,
        );
        block.definitions.insert("m".to_string(), 0);
        block.definitions.insert("other".to_string(), 7);
        let back = Block::decode(&block.encode(3).unwrap()).unwrap();
        assert_eq!(back, block);
        assert_eq!(back.definitions.get("m"), Some(&0));
        assert_eq!(back.definitions.get("other"), Some(&7));
        assert_eq!(
            back.series
                .iter()
                .map(|x| x.metric.as_str())
                .collect::<Vec<_>>(),
            ["m", "m", "other"],
        );
    }

    /// A metric with no recorded definition is a permanent state of the
    /// format rather than a stage of it — a block packed from tally
    /// lines has none to record — so absence must survive the round trip
    /// and stay distinguishable from definition 0. Tally's "zero and
    /// unknown are different", one level up from the presence bitmap.
    #[test]
    fn an_unrecorded_definition_is_absent_and_not_zero() {
        let block = pack1(&["2026-09-06T13:37:00.000Z 60s m a=1 count=1"], 60);
        assert!(block.definitions.is_empty());
        let back = Block::decode(&block.encode(3).unwrap()).unwrap();
        assert!(
            back.definitions.is_empty(),
            "an absent definition came back as {:?}",
            back.definitions
        );
    }

    /// Both blocks of a merge are of one tally, so a metric cannot have
    /// two definitions between them: refused rather than resolved by
    /// picking a side. ⚠ Not a cross-tally check — an id is local, so
    /// two tallies can agree on the number and mean different things.
    #[test]
    fn merging_blocks_that_disagree_about_a_definition_is_refused() {
        let mut a = pack1(&["2026-09-06T13:37:00.000Z 60s m a=1 count=1"], 60);
        let mut b = pack1(&["2026-09-06T13:38:00.000Z 60s m a=2 count=2"], 60);
        a.definitions.insert("m".to_string(), 1);
        b.definitions.insert("m".to_string(), 2);
        let err = a.merge(&b).unwrap_err().to_string();
        assert!(err.contains("cannot both be true of one tally"), "{err}");
        b.definitions.insert("m".to_string(), 1);
        let merged = a.merge(&b).unwrap();
        assert_eq!(merged.definitions.get("m"), Some(&1));
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

    fn sel(expr: &str) -> crate::select::Selector {
        crate::select::Selector::parse(expr).expect(expr)
    }

    /// ⚠ THE TWO PRUNINGS, and the point is what is NOT touched: the
    /// manifest keeps blocks outside the window shut, and the series
    /// table keeps the columns of series the selector did not pick from
    /// being decoded.
    #[test]
    fn a_query_opens_only_the_blocks_and_series_it_needs() {
        let dir = tmpdir("q1");
        let mut m = Manifest::new(60_000, 60);
        // Three hours, three series each.
        for h in 0..3u64 {
            let mut rows = Vec::new();
            for which in ["a", "b", "c"] {
                let mut x = Sample::new(
                    crate::tally::parse_stamp("2026-09-06T00:00:00.000Z").unwrap() + h * 3_600_000,
                    60_000,
                    "m",
                );
                x = x.label("who", which).field(Field::Count, 1.0);
                rows.push(x);
            }
            for b in Block::pack(&rows, 60).unwrap() {
                commit(&dir, &mut m, &b, 3, How::Merge).unwrap();
            }
        }
        let t0 = crate::tally::parse_stamp("2026-09-06T00:00:00.000Z").unwrap();

        // The middle hour, one series of the three.
        let (got, touched) = query(&dir, &sel("[who=b]"), t0 + 3_600_000, t0 + 3_600_000).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].labels, vec![("who".to_string(), "b".to_string())]);
        assert_eq!(touched.blocks_in_manifest, 3);
        assert_eq!(touched.blocks_opened, 1, "the other two were never read");
        assert_eq!(touched.series_matched, 1);
        assert_eq!(touched.series_skipped, 2, "their columns were stepped over");
    }

    /// A selective read must produce EXACTLY what a full read would,
    /// filtered — or the pruning is not pruning, it is losing.
    #[test]
    fn a_selective_read_agrees_with_a_full_one() {
        let dir = tmpdir("q2");
        let mut m = Manifest::new(60_000, 60);
        let mut rows = Vec::new();
        for i in 0..20u64 {
            let mut x = Sample::new(
                crate::tally::parse_stamp("2026-09-06T00:00:00.000Z").unwrap() + (i % 5) * 60_000,
                60_000,
                if i % 2 == 0 { "even" } else { "odd" },
            );
            x = x
                .label("n", &format!("{}", i))
                .field(Field::Count, i as f64)
                .field(Field::Sum, (i * 3) as f64);
            rows.push(x);
        }
        for b in Block::pack(&rows, 60).unwrap() {
            commit(&dir, &mut m, &b, 3, How::Merge).unwrap();
        }
        let (all, _) = query(&dir, &sel("[]"), 0, u64::MAX).unwrap();
        let (some, t) = query(&dir, &sel("[metric=even]"), 0, u64::MAX).unwrap();
        let want: Vec<String> = all
            .iter()
            .filter(|s| s.metric == "even")
            .map(|s| s.render())
            .collect();
        assert_eq!(some.iter().map(|s| s.render()).collect::<Vec<_>>(), want);
        assert_eq!(t.series_matched, 10);
        assert_eq!(t.series_skipped, 10);
    }

    /// The metric is a selectable KEY on a parsed line even though it is
    /// POSITIONAL in a rendered one — so the predicate that picks stores
    /// picks series, with the operators it already has.
    #[test]
    fn the_metric_is_a_selectable_key_with_the_usual_operators() {
        let s1 = Series {
            metric: "http_requests".to_string(),
            labels: vec![("status".to_string(), "500".to_string())],
        };
        assert!(sel("[metric=http_requests]").matches(&s1.as_fields()));
        assert!(sel("[metric=~http_.*]").matches(&s1.as_fields()));
        assert!(sel("[status=500]").matches(&s1.as_fields()));
        assert!(sel("[metric=http_requests,status!=200]").matches(&s1.as_fields()));
        assert!(!sel("[metric=http_bytes]").matches(&s1.as_fields()));
        assert!(
            sel("[]").matches(&s1.as_fields()),
            "the empty predicate is everything"
        );
    }

    /// The manifest selects a block, and then the WINDOW still has to
    /// bound the buckets inside it — a block is a range, so selecting it
    /// is not the same as wanting all of it.
    #[test]
    fn a_window_bounds_the_buckets_inside_a_selected_block() {
        let dir = tmpdir("q3");
        let mut m = Manifest::new(60_000, 60);
        let t0 = crate::tally::parse_stamp("2026-09-06T00:00:00.000Z").unwrap();
        let block = pack1(
            &[
                "2026-09-06T00:00:00.000Z 60s m count=1",
                "2026-09-06T00:30:00.000Z 60s m count=99",
            ],
            60,
        );
        commit(&dir, &mut m, &block, 3, How::Merge).unwrap();

        let (whole, _) = query(&dir, &sel("[]"), 0, u64::MAX).unwrap();
        assert_eq!(whole.len(), 2);

        let (early, t) = query(&dir, &sel("[]"), t0, t0 + 60_000).unwrap();
        assert_eq!(early.len(), 1, "the later bucket is outside this window");
        assert_eq!(early[0].render(), "2026-09-06T00:00:00.000Z 60s m count=1");
        assert_eq!(
            t.blocks_opened, 1,
            "and the block it was in was opened once"
        );
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A window selects blocks from the MANIFEST, with no block opened —
    /// which is the query-planning property, and the reason a block's
    /// coverage is recorded rather than discovered.
    #[test]
    fn a_window_selects_blocks_without_opening_one() {
        let mut m = Manifest::new(60_000, 60);
        for day in 0..3u64 {
            m.put(Entry {
                t0: day * 60 * 60_000,
                n_buckets: 60,
                generation: 0,
                bytes: 1,
                crc32: 0,
            });
        }
        // The middle hour only.
        let got = m.covering(60 * 60_000, 60 * 60_000 + 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].t0, 60 * 60_000);
        // A window spanning the seam takes both.
        assert_eq!(m.covering(59 * 60_000, 61 * 60_000).len(), 2);
        // And one before everything takes nothing.
        assert!(m.covering(0, 0).len() <= 1);
    }

    /// ⚠ THE BUG THIS ENUM EXISTS FOR, and it lost data. A first
    /// version inferred a regeneration from "I already hold this range",
    /// so a log that does not rotate on UTC midnight — every real one —
    /// spilled a few buckets into the previous day and REPLACED that
    /// day's full block with the overlap. Measured on real logs: three
    /// of six days went from ~900 KB to ~22 KB, silently.
    #[test]
    fn a_later_spill_into_a_day_must_not_replace_it() {
        let dir = tmpdir("mfspill");
        let mut m = Manifest::new(60_000, 1440);

        // A full day: a bucket every hour, 24 of them.
        let mut full: Vec<Sample> = Vec::new();
        for h in 0..24u64 {
            let mut x = Sample::new(
                crate::tally::parse_stamp("2026-09-06T00:00:00.000Z").unwrap() + h * 3_600_000,
                60_000,
                "m",
            );
            x = x.field(Field::Count, h as f64 + 1.0);
            full.push(x);
        }
        for b in Block::pack(&full, 1440).unwrap() {
            commit(&dir, &mut m, &b, 3, How::Merge).unwrap();
        }
        assert_eq!(read_all(&dir, &m).len(), 24, "the full day went in");

        // The next day's log spills two buckets back into this one —
        // which is what a rotation at 03:00 looks like.
        let spill: Vec<Sample> = [1u64, 2]
            .iter()
            .map(|h| {
                let mut x = Sample::new(
                    crate::tally::parse_stamp("2026-09-06T00:00:00.000Z").unwrap()
                        + h * 3_600_000
                        + 60_000,
                    60_000,
                    "m",
                );
                x = x.field(Field::Count, 99.0);
                x
            })
            .collect();
        for b in Block::pack(&spill, 1440).unwrap() {
            commit(&dir, &mut m, &b, 3, How::Merge).unwrap();
        }

        let after = read_all(&dir, &m);
        assert_eq!(
            after.len(),
            26,
            "the day must still hold its 24 buckets plus the 2 that spilled in, not 2"
        );
        assert_eq!(m.blocks.len(), 1, "still one block for the range");
        assert_eq!(m.blocks[0].generation, 0, "a merge is not a new generation");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And a regeneration, asked for explicitly, still replaces.
    #[test]
    fn an_explicit_regeneration_replaces_the_range() {
        let dir = tmpdir("mfregen");
        let mut m = Manifest::new(60_000, 60);
        let a = pack1(
            &[
                "2026-09-06T13:37:00.000Z 60s m count=1",
                "2026-09-06T13:38:00.000Z 60s m count=2",
            ],
            60,
        );
        commit(&dir, &mut m, &a, 3, How::Merge).unwrap();
        let b = pack1(&["2026-09-06T13:37:00.000Z 60s m count=999"], 60);
        commit(&dir, &mut m, &b, 3, How::Regenerate).unwrap();
        let after = read_all(&dir, &m);
        assert_eq!(after.len(), 1, "a regeneration REPLACES, so 13:38 is gone");
        assert_eq!(
            after[0].render(),
            "2026-09-06T13:37:00.000Z 60s m count=999"
        );
        assert_eq!(m.blocks[0].generation, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cell in both takes the new value, which is the newest-wins rule
    /// a tape already had for two lines of one bucket.
    #[test]
    fn a_merge_takes_the_newer_value_for_a_cell_in_both() {
        let dir = tmpdir("mfnewer");
        let mut m = Manifest::new(60_000, 60);
        let a = pack1(&["2026-09-06T13:37:00.000Z 60s m count=1"], 60);
        commit(&dir, &mut m, &a, 3, How::Merge).unwrap();
        let b = pack1(&["2026-09-06T13:37:00.000Z 60s m count=7"], 60);
        commit(&dir, &mut m, &b, 3, How::Merge).unwrap();
        let after = read_all(&dir, &m);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].render(), "2026-09-06T13:37:00.000Z 60s m count=7");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_all(dir: &std::path::Path, m: &Manifest) -> Vec<Sample> {
        let mut out = Vec::new();
        for e in &m.blocks {
            out.extend(read_block(dir, e).unwrap().samples());
        }
        out
    }

    /// ⚠ THE COMMIT ORDER, window one: a block written and NOT yet in
    /// the manifest. The store must still answer from the previous
    /// generation, and the new file must be collectable debris.
    #[test]
    fn a_crash_before_the_commit_leaves_debris_not_a_lie() {
        let dir = tmpdir("mf1");
        let mut m = Manifest::new(60_000, 60);
        let v1 = pack1(&["2026-09-06T13:37:00.000Z 60s m count=1"], 60);
        commit(&dir, &mut m, &v1, 3, How::Regenerate).unwrap();

        // Now the first step of a regeneration, and then a "crash".
        let mut v2 = pack1(&["2026-09-06T13:37:00.000Z 60s m count=999"], 60);
        v2.generation = 1;
        let bytes = v2.encode(3).unwrap();
        std::fs::write(dir.join(block_name(v2.t0, 1)), &bytes).unwrap();

        // A reader loads the manifest, which still names generation 0.
        let seen = Manifest::load(&dir).unwrap().unwrap();
        assert_eq!(seen.blocks.len(), 1);
        assert_eq!(seen.blocks[0].generation, 0, "the commit had not happened");
        let back = read_block(&dir, &seen.blocks[0]).unwrap();
        assert_eq!(
            back.samples()[0].render(),
            "2026-09-06T13:37:00.000Z 60s m count=1",
            "the store answers from the generation the manifest names"
        );
        // And the half-done generation is collectable.
        let debris = seen.unreferenced(&dir).unwrap();
        assert_eq!(debris.len(), 1);
        assert!(debris[0].ends_with(block_name(v2.t0, 1)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⚠ Window two: the manifest committed and the superseded block NOT
    /// yet unlinked. The store must answer from the NEW generation, and
    /// the old file must be collectable.
    #[test]
    fn a_crash_after_the_commit_leaves_debris_not_a_lie() {
        let dir = tmpdir("mf2");
        let mut m = Manifest::new(60_000, 60);
        let v1 = pack1(&["2026-09-06T13:37:00.000Z 60s m count=1"], 60);
        commit(&dir, &mut m, &v1, 3, How::Regenerate).unwrap();

        // The commit, by hand, stopping before the unlink.
        let mut v2 = pack1(&["2026-09-06T13:37:00.000Z 60s m count=999"], 60);
        v2.generation = 1;
        let bytes = v2.encode(3).unwrap();
        std::fs::write(dir.join(block_name(v2.t0, 1)), &bytes).unwrap();
        let superseded = m.put(Entry {
            t0: v2.t0,
            n_buckets: v2.n_buckets,
            generation: 1,
            bytes: bytes.len() as u64,
            crc32: crate::sap::crc32(&bytes),
        });
        m.save(&dir).unwrap();
        assert_eq!(superseded.len(), 1, "generation 0 is superseded");

        let seen = Manifest::load(&dir).unwrap().unwrap();
        assert_eq!(seen.blocks.len(), 1);
        assert_eq!(seen.blocks[0].generation, 1);
        assert_eq!(
            read_block(&dir, &seen.blocks[0]).unwrap().samples()[0].render(),
            "2026-09-06T13:37:00.000Z 60s m count=999",
            "the store answers from the new generation the moment the manifest names it"
        );
        // The old generation is now debris.
        let debris = seen.unreferenced(&dir).unwrap();
        assert_eq!(debris.len(), 1);
        assert!(debris[0].ends_with(block_name(v1.t0, 0)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A full regeneration leaves nothing behind, which is the case the
    /// two windows above bracket.
    #[test]
    fn a_completed_regeneration_leaves_no_debris() {
        let dir = tmpdir("mf3");
        let mut m = Manifest::new(60_000, 60);
        let v1 = pack1(&["2026-09-06T13:37:00.000Z 60s m count=1"], 60);
        commit(&dir, &mut m, &v1, 3, How::Regenerate).unwrap();
        let mut v2 = pack1(&["2026-09-06T13:37:00.000Z 60s m count=999"], 60);
        v2.generation = 1;
        commit(&dir, &mut m, &v2, 3, How::Regenerate).unwrap();
        assert_eq!(m.blocks.len(), 1);
        assert_eq!(m.blocks[0].generation, 1);
        assert!(m.unreferenced(&dir).unwrap().is_empty());
        assert!(!dir.join(block_name(v1.t0, 0)).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⚠ A sealed block may not change under its manifest, and the crc32
    /// is what says so — the property every cache and every replica
    /// downstream relies on.
    #[test]
    fn a_block_that_changed_under_the_manifest_is_refused() {
        let dir = tmpdir("mf4");
        let mut m = Manifest::new(60_000, 60);
        let b = pack1(&["2026-09-06T13:37:00.000Z 60s m count=1"], 60);
        let e = commit(&dir, &mut m, &b, 3, How::Regenerate).unwrap();
        let mut bytes = std::fs::read(dir.join(e.file_name())).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xff;
        std::fs::write(dir.join(e.file_name()), &bytes).unwrap();
        let err = read_block(&dir, &e).unwrap_err().to_string();
        assert!(err.contains("crc32"), "unhelpful: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⚠ The FLOOR separates dropped from never-written, which is "zero
    /// and unknown are different" one level up: a window retention took
    /// away and a window nothing ever measured are both empty, and only
    /// one of them means the numbers were once there.
    #[test]
    fn the_floor_says_dropped_rather_than_never_written() {
        let dir = tmpdir("mf5");
        let mut m = Manifest::new(60_000, 60);
        for h in 0..3u64 {
            let mut b = pack1(&["2026-09-06T13:37:00.000Z 60s m count=1"], 60);
            b.t0 = h * 60 * 60_000;
            commit(&dir, &mut m, &b, 3, How::Regenerate).unwrap();
        }
        assert_eq!(m.floor, 0, "nothing dropped yet");
        let gone = m.drop_before(2 * 60 * 60_000);
        m.save(&dir).unwrap();
        for name in &gone {
            std::fs::remove_file(dir.join(name)).unwrap();
        }
        assert_eq!(gone.len(), 2);
        assert_eq!(m.blocks.len(), 1);
        assert_eq!(
            m.floor,
            2 * 60 * 60_000,
            "the floor records what was let go"
        );
        // The dropped window is empty AND below the floor, so a reader
        // can tell it apart from one that never existed.
        assert!(m.covering(0, 60_000).is_empty());
        assert!(60_000 < m.floor, "below the floor: dropped, not unknown");
        let far = 99 * 60 * 60_000;
        assert!(m.covering(far, far + 1).is_empty());
        assert!(far > m.floor, "above the floor: never written");
        assert!(m.unreferenced(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The manifest survives a round trip, and a future version is
    /// refused rather than read under rules it may not share.
    #[test]
    fn a_manifest_round_trips_and_a_future_version_is_refused() {
        let dir = tmpdir("mf6");
        let mut m = Manifest::new(60_000, 1440);
        m.put(Entry {
            t0: 1_000_000,
            n_buckets: 1440,
            generation: 3,
            bytes: 42,
            crc32: 7,
        });
        m.floor = 999;
        m.save(&dir).unwrap();
        assert_eq!(Manifest::load(&dir).unwrap().unwrap(), m);

        let text = std::fs::read_to_string(dir.join(MANIFEST)).unwrap();
        std::fs::write(dir.join(MANIFEST), text.replace("\"v\": 1", "\"v\": 2")).unwrap();
        let err = Manifest::load(&dir).unwrap_err().to_string();
        assert!(err.contains("version 2"), "unhelpful: {err}");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A store has an identity, minted once, and reloading never
    /// touches it — bark's rule, for bark's reason: a path is an
    /// address and the id is what the store IS.
    #[test]
    fn a_store_is_minted_an_identity_that_a_reload_never_changes() {
        let dir = tmpdir("id1");
        let m = Manifest::new(60_000, 60);
        let id = m.id.clone().expect("minted");
        let created = m.created.clone().expect("stamped");
        m.save(&dir).unwrap();
        let back = Manifest::load(&dir).unwrap().unwrap();
        assert_eq!(back.id.as_deref(), Some(id.as_str()));
        assert_eq!(back.created, Some(created));
        // And a second store is a different store.
        assert_ne!(Manifest::new(60_000, 60).id, Some(id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⚠ A manifest written before identity existed loads, and is NOT
    /// handed one on the way in: minting on read would give an old
    /// store a new identity in silence, where absence is a defect to
    /// repair deliberately.
    #[test]
    fn a_manifest_without_an_identity_is_not_given_one_by_reading_it() {
        let dir = tmpdir("id2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST),
            r#"{"v":1,"width_ms":60000,"block_buckets":60,"floor":0,"blocks":[]}"#,
        )
        .unwrap();
        let m = Manifest::load(&dir).unwrap().unwrap();
        assert_eq!(m.id, None, "reading minted an identity");
        assert_eq!(m.source, None);
        assert!(m.labels.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store's source does not change. Feeding one store's records
    /// into another's blocks would leave every citation pointing into
    /// the wrong tape, and the offsets are plausible either way — so it
    /// is refused rather than noticed later.
    #[test]
    fn a_second_source_for_one_block_store_is_refused() {
        let mut m = Manifest::new(60_000, 60);
        assert!(m.set_source("aaaa-1111").unwrap(), "the first is recorded");
        assert!(
            !m.set_source("aaaa-1111").unwrap(),
            "the same one again is a no-op"
        );
        let err = m.set_source("bbbb-2222").unwrap_err().to_string();
        assert!(
            err.contains("aaaa-1111") && err.contains("bbbb-2222"),
            "{err}"
        );
        assert_eq!(
            m.source.as_deref(),
            Some("aaaa-1111"),
            "and it did not move"
        );
    }

    /// Retention drops whole blocks, so a value between two of them is
    /// a promise the store cannot keep — refused rather than rounded.
    #[test]
    fn a_retention_finer_than_a_block_is_refused() {
        let mut m = Manifest::new(60_000, 1440);
        assert_eq!(m.block_span_ms(), 86_400_000, "a day of 60s buckets");
        m.set_retain(Some(730 * 86_400_000)).unwrap();
        assert_eq!(m.retain_ms, Some(730 * 86_400_000));
        let err = m.set_retain(Some(36 * 3_600_000)).unwrap_err().to_string();
        assert!(err.contains("whole number of blocks"), "{err}");
        assert_eq!(
            m.retain_ms,
            Some(730 * 86_400_000),
            "a refusal must not have moved it"
        );
        m.set_retain(None).unwrap();
        assert_eq!(m.retain_ms, None, "and it can be cleared");
    }

    /// What the writer stores is what the lines would have said. If this
    /// does not hold, the block path is a different tally rather than
    /// the same one stored differently — which is the whole claim.
    /// ⚠ A flush limit of 2 forces several commits over one range, so
    /// this also exercises the merge every commit after the first is.
    #[test]
    fn what_the_writer_stores_is_what_the_lines_said() {
        let dir = tmpdir("w1");
        let lines = [
            "2026-09-06T13:37:00.000Z 60s m a=1 count=1 sum=10",
            "2026-09-06T13:38:00.000Z 60s m a=1 count=2 sum=20",
            "2026-09-06T13:38:00.000Z 60s m a=2 count=3 sum=30",
            "2026-09-06T13:39:00.000Z 60s other b=1 count=4",
        ];
        let mut w = Writer::open(&dir, 60_000, 60, 2, 3).unwrap();
        for l in lines {
            w.take(vec![s(l)]).unwrap();
        }
        w.flush().unwrap();
        assert_eq!(w.committed, 4);
        assert!(w.blocks_written > 1, "the limit should have forced commits");

        let (got, _) = query(&dir, &sel("[]"), 0, u64::MAX).unwrap();
        assert_eq!(
            got.iter().map(|x| x.render()).collect::<Vec<_>>(),
            lines,
            "the grid must render back to the lines it was given"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Markers are counted and not stored: a marker states something
    /// about a RUN and the grid holds numbers
    /// (docs/plans/tally-design.md).
    #[test]
    fn the_writer_counts_markers_and_stores_none() {
        let dir = tmpdir("w2");
        let mut w = Writer::open(&dir, 60_000, 60, 100, 3).unwrap();
        w.take(vec![
            s("2026-09-06T13:37:00.000Z 60s m a=1 count=1"),
            s("2026-09-06T13:37:00.000Z 60s !drop metric=m reason=unreadable count=2"),
            s("2026-09-06T13:37:00.000Z 0s !meta metric=m unit=calls"),
        ])
        .unwrap();
        w.flush().unwrap();
        assert_eq!((w.committed, w.markers), (1, 2));
        let (got, _) = query(&dir, &sel("[]"), 0, u64::MAX).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].metric, "m");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store holds ONE bucket width, so reopening it with another is
    /// refused rather than adopted — coarsening is a different
    /// operation on a column, not something a second run may do by
    /// arriving with a different window.
    #[test]
    fn a_writer_will_not_change_a_stores_width() {
        let dir = tmpdir("w3");
        let mut w = Writer::open(&dir, 60_000, 60, 100, 3).unwrap();
        w.take(vec![s("2026-09-06T13:37:00.000Z 60s m a=1 count=1")])
            .unwrap();
        w.flush().unwrap();
        let err = match Writer::open(&dir, 300_000, 60, 100, 3) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a second width was accepted"),
        };
        assert!(err.contains("60s") && err.contains("300s"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Garbage in must not panic. Defence in depth rather than a
    /// scenario: every production path reaches `decode` through
    /// `read_block`, which checks the manifest's crc32 first, so this
    /// asserts the parser is safe for a caller that does not.
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
