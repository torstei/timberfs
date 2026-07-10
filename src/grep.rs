//! `timberfs grep`: entry-aware grep. A log ENTRY is a line carrying a
//! timestamp plus all its continuation lines (stack traces, wrapped
//! output) — the pattern is matched against the whole entry, and matching
//! entries are printed whole. Entry boundaries are detected with the same
//! timestamp auto-detection as import (--timestamp-regex/--format for
//! exotic formats).
//!
//!     cat any.log | timberfs grep 'tenantId=FOO' | timberfs grep -v DEBUG
//!     timberfs grep ERROR backing/app.log --from 13:00 --to 14:00
//!     timberfs grep 'req-8f3a' incident.timber --has req-8f3a
//!
//! Input is stdin (raw log bytes), a plain log file, or a timberfs
//! log/bundle — in the timberfs case --from/--to/--has pre-select chunks
//! first (time index + .grain Bloom filters), then entries are matched
//! exactly. Piping several greps gives entry-level AND, as with grep.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{bail, Context};
use regex::bytes::{Regex, RegexBuilder};

use crate::format::ChunkRecord;
use crate::import::Extractor;
use crate::query::{is_bundle, open_source, parse_time, select_chunks};

/// A timestamp-less flood can't balloon memory: entries are split here.
const ENTRY_CAP: usize = 16 << 20;

/// Streams decompressed content of the selected chunks in order.
struct ChunkStream {
    file: File,
    chunks: Vec<ChunkRecord>,
    idx: usize,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChunkStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.buf.len() {
            if self.idx == self.chunks.len() {
                return Ok(0);
            }
            let c = self.chunks[self.idx];
            self.idx += 1;
            let mut comp = vec![0u8; c.comp_len as usize];
            self.file.read_exact_at(&mut comp, c.comp_start)?;
            self.buf = zstd::stream::decode_all(&comp[..])?;
            self.pos = 0;
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct Entries<R: BufRead> {
    reader: R,
    extractor: Extractor,
    pending: Option<Vec<u8>>,
    warned_cap: bool,
}

impl<R: BufRead> Entries<R> {
    fn next_entry(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut entry = match self.pending.take() {
            Some(line) => line,
            None => {
                let mut line = Vec::new();
                if self.reader.read_until(b'\n', &mut line)? == 0 {
                    return Ok(None);
                }
                line
            }
        };
        loop {
            let mut line = Vec::new();
            if self.reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let head = String::from_utf8_lossy(&line[..line.len().min(256)]);
            if self.extractor.extract(&head).is_some() {
                self.pending = Some(line);
                break;
            }
            if entry.len() + line.len() > ENTRY_CAP {
                if !self.warned_cap {
                    eprintln!("timberfs: entry exceeds 16 MiB; splitting");
                    self.warned_cap = true;
                }
                self.pending = Some(line);
                break;
            }
            entry.extend_from_slice(&line);
        }
        Ok(Some(entry))
    }
}

fn run<R: BufRead>(
    mut entries: Entries<R>,
    re: &Regex,
    invert: bool,
    count: bool,
) -> anyhow::Result<u64> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut matched: u64 = 0;
    while let Some(entry) = entries.next_entry()? {
        if re.is_match(&entry) != invert {
            matched += 1;
            if !count {
                out.write_all(&entry)?;
            }
        }
    }
    out.flush()?;
    Ok(matched)
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_grep(
    pattern: &str,
    file: Option<&Path>,
    from: Option<&str>,
    to: Option<&str>,
    has: &[String],
    ignore_case: bool,
    invert: bool,
    fixed: bool,
    count: bool,
    ts_regex: Option<&str>,
    ts_format: Option<&str>,
) -> anyhow::Result<()> {
    let pat = if fixed {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let re = RegexBuilder::new(&pat)
        .case_insensitive(ignore_case)
        .multi_line(true)
        .build()
        .with_context(|| format!("bad pattern {pattern:?}"))?;
    let extractor = Extractor::new(ts_regex, ts_format, false)?;

    let is_timberfs_source = |p: &Path| {
        is_bundle(p)
            || matches!(
                p.extension().and_then(|e| e.to_str()),
                Some(crate::format::TRUNK_EXT) | Some(crate::format::RINGS_EXT)
            )
            || !p.is_file()
    };

    let matched = match file {
        None => {
            if from.is_some() || to.is_some() || !has.is_empty() {
                bail!("--from/--to/--has need a timberfs log or bundle, not stdin");
            }
            let stdin = io::stdin();
            run(
                Entries {
                    reader: stdin.lock(),
                    extractor,
                    pending: None,
                    warned_cap: false,
                },
                &re,
                invert,
                count,
            )?
        }
        Some(p) if !is_timberfs_source(p) => {
            // a plain log file at the exact path
            if from.is_some() || to.is_some() || !has.is_empty() {
                bail!(
                    "--from/--to/--has need a timberfs log or bundle; {} is a plain file",
                    p.display()
                );
            }
            run(
                Entries {
                    reader: BufReader::new(
                        File::open(p).with_context(|| format!("opening {}", p.display()))?,
                    ),
                    extractor,
                    pending: None,
                    warned_cap: false,
                },
                &re,
                invert,
                count,
            )?
        }
        Some(p) => {
            let source = open_source(p)?;
            let from_ms = from.map(parse_time).transpose()?.unwrap_or(0);
            let to_ms = to.map(parse_time).transpose()?.unwrap_or(u64::MAX);
            if from_ms > to_ms {
                bail!("--from is after --to");
            }
            let (selected, _) = select_chunks(p, &source.records, from_ms, to_ms, has)?;
            // Pad the selection by one chunk on each side: an entry whose
            // timestamped first line sits at the tail of the previous
            // chunk arrives whole. (Entries spanning further are subject
            // to the usual chunk-granularity slop.)
            let mut padded = std::collections::BTreeSet::new();
            for (i, _) in &selected {
                padded.insert(i.saturating_sub(1));
                padded.insert(*i);
                padded.insert(i + 1);
            }
            let chunks: Vec<ChunkRecord> = padded
                .into_iter()
                .filter_map(|i| source.records.get(i).copied())
                .collect();
            let stream = ChunkStream {
                file: source.file,
                chunks,
                idx: 0,
                buf: Vec::new(),
                pos: 0,
            };
            run(
                Entries {
                    reader: BufReader::with_capacity(1 << 20, stream),
                    extractor,
                    pending: None,
                    warned_cap: false,
                },
                &re,
                invert,
                count,
            )?
        }
    };
    if count {
        println!("{matched}");
    }
    Ok(())
}
