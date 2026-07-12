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
//!     timberfs grep 'tenantId=FOO' backing/app.log --from 13:00 --into case.timber
//!
//! Input is stdin (raw log bytes), a plain log file, or a timberfs
//! log/bundle — in the timberfs case --from/--to/--has pre-select chunks
//! first (time index + .grain Bloom filters), then entries are matched
//! exactly. Piping several greps gives entry-level AND, as with grep.
//!
//! Matching modes — the DEFAULT is a literal at token boundaries ("word
//! mode": ERROR finds the word ERROR, not ERRORS). That is the .grain's
//! own whole-token semantics, so on an indexed log the pattern's tokens
//! pre-filter chunks automatically and EXACTLY: grep 'FOO BAR ERROR' is
//! --has FOO --has BAR --has ERROR at chunk level, then "the entry
//! contains the anchored phrase" at entry level — an entry matching the
//! phrase must contain every token whole, so the skip can only be
//! conservative, never wrong. (Scattered word-AND instead of a phrase:
//! the pattern-less --has form, or piped greps.) The mode space is a
//! 2x2 — interpretation x boundaries — with the default in the fast
//! cell:
//!
//!            | whole words                | anywhere (inside words too)
//!   literal  | (default / --any / --has)  | --substring
//!   regexp   |            —               | --regex
//!
//! --substring reaches inside tokens (partial ids) and --regex is a full
//! regex; both read every chunk (a multi-word --substring still
//! pre-filters on its INTERIOR words), as must -i (the grain is
//! exact-case) and -v (non-matches must be READ to be printed). When a
//! grain exists but cannot be used, a one-line note says why.
//!
//! Predicates compose in two vocabularies, and the combinator is
//! readable from the flag's name. PATTERN flags (--any, --regex,
//! --substring): repeating one ORs within it (--any A --any B matches
//! either, and stays INDEXED — the union of exact branches is exact),
//! different kinds AND, and -v inverts the whole pattern conjunction.
//! REQUIREMENTS (--has): every --has must hold — the AND side;
//! multi-word arguments are contiguous word-anchored PHRASES (same
//! matching as --any; only the combinator differs, and the names say
//! which). "A AND NOT B" is `--has A -v B`. The bare positional is grep
//! legacy — tolerated, one word-anchored phrase on the pattern side.
//!
//! `--into DEST` writes the matching entries into a NEW timberfs log (or
//! .timber bundle) instead of printing them — an investigation shipped as
//! an artifact. The third derivation op after export and rotate: the bark
//! records lineage plus the full command line, pattern and window as
//! operation facts, so the artifact says what question produced it.
//! Unlike export (verbatim chunk copies), entries are decompressed,
//! matched and recompressed — cost follows the uncompressed window. An
//! empty result is a result: it yields an (empty) artifact unless
//! --fail-on-empty.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{bail, Context};
use regex::bytes::{Regex, RegexBuilder};
use serde_json::Value;

use crate::format::ChunkRecord;
use crate::import::Extractor;
use crate::query::{fmt_ms, is_bundle, open_source, resolve_backing, select_chunks};
use crate::store;

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

pub struct Entries<R: BufRead> {
    pub reader: R,
    pub extractor: Extractor,
    pub pending: Option<Vec<u8>>,
    pub warned_cap: bool,
}

impl<R: BufRead> Entries<R> {
    pub fn next_entry(&mut self) -> io::Result<Option<Vec<u8>>> {
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

/// An entry matches when EVERY regex matches it (an empty list matches
/// everything — the pure-window case). One user pattern is a list of one;
/// --has-derived token matching is a list of escaped tokens, ANDed.
fn is_match(res: &[Regex], entry: &[u8]) -> bool {
    res.iter().all(|re| re.is_match(entry))
}

#[allow(clippy::too_many_arguments)]
fn run<R: BufRead>(
    mut entries: Entries<R>,
    res: &[Regex],
    required: &[Regex],
    invert: bool,
    count: bool,
    prefix: Option<&[u8]>,
    window: Option<(u64, u64)>,
    null_sep: bool,
) -> anyhow::Result<u64> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut matched: u64 = 0;
    let mut last_ts: Option<u64> = None;
    while let Some(entry) = entries.next_entry()? {
        // --has is verified on every ENTRY, not just at chunk level: the
        // Bloom skip is an optimization, never a semantics change. (-v
        // inverts the PATTERN only; the --has requirement always holds.)
        if !required.is_empty() && !is_match(required, &entry) {
            continue;
        }
        // The DEFAULT verifies entries against --from/--to by the
        // timestamps the lines themselves carry; entries we cannot place
        // in time are included, never hidden.
        if let Some((from, to)) = window {
            let head = String::from_utf8_lossy(&entry[..entry.len().min(256)]);
            let ts = entries.extractor.extract(&head);
            if let Some(t) = ts {
                last_ts = Some(t);
            }
            if let Some(t) = ts.or(last_ts) {
                if t < from || t > to {
                    continue;
                }
            }
        }
        if is_match(res, &entry) != invert {
            matched += 1;
            if !count {
                if null_sep {
                    if let Some(label) = prefix {
                        out.write_all(label)?;
                        out.write_all(b":")?;
                    }
                    let body = entry.strip_suffix(b"\n").unwrap_or(&entry);
                    out.write_all(body)?;
                    out.write_all(b"\0")?;
                } else {
                    match prefix {
                        None => out.write_all(&entry)?,
                        Some(label) => {
                            for line in entry.split_inclusive(|&b| b == b'\n') {
                                out.write_all(label)?;
                                out.write_all(b":")?;
                                out.write_all(line)?;
                            }
                        }
                    }
                }
            }
        }
    }
    out.flush()?;
    Ok(matched)
}

/// The window/has chunk selection, padded by one chunk on each side: an
/// entry whose timestamped first line sits at the tail of the previous
/// chunk arrives whole. (Entries spanning further are subject to the
/// usual chunk-granularity slop.)
fn padded_chunks(
    p: &Path,
    records: &[ChunkRecord],
    from_ms: u64,
    to_ms: u64,
    has: &[String],
    any_of: &[String],
) -> anyhow::Result<(Vec<ChunkRecord>, usize, usize)> {
    let (selected, in_window) = select_chunks(p, records, from_ms, to_ms, has, any_of)?;
    let kept = selected.len();
    let mut padded = std::collections::BTreeSet::new();
    for (i, _) in &selected {
        padded.insert(i.saturating_sub(1));
        padded.insert(*i);
        padded.insert(i + 1);
    }
    Ok((
        padded
            .into_iter()
            .filter_map(|i| records.get(i).copied())
            .collect(),
        in_window,
        kept,
    ))
}

/// One honest line about what a narrowed scan will actually read —
/// printed AFTER selection, with the numbers the operation used. A
/// window that selects nothing says so, with the store's real coverage:
/// that is usually the whole answer to "why did I get 0 matches".
#[allow(clippy::too_many_arguments)]
fn report_scan(
    p: &Path,
    records: &[ChunkRecord],
    windowed: bool,
    has: &[String],
    auto: bool,
    idle: Option<&str>,
    scanning: usize,
    in_window: usize,
    kept: usize,
) {
    if !windowed && has.is_empty() {
        return; // a plain full scan needs no narration
    }
    if windowed && in_window == 0 {
        let (mut a, mut b) = (u64::MAX, 0u64);
        for r in records {
            a = a.min(r.first_write_ms);
            b = b.max(r.last_write_ms);
        }
        if records.is_empty() {
            crate::note!(
                "timberfs: {}: --from/--to select nothing — the store is empty",
                p.display()
            );
        } else {
            crate::note!(
                "timberfs: {}: --from/--to select nothing — the store covers {} .. {}",
                p.display(),
                fmt_ms(a),
                fmt_ms(b)
            );
        }
        return;
    }
    let mut parts: Vec<String> = Vec::new();
    if windowed {
        parts.push(format!("time window keeps {in_window}"));
    }
    if !has.is_empty() {
        let toks = crate::grain::tokenize_query(&has.join(" "))
            .iter()
            .map(|t| String::from_utf8_lossy(t).into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("tokens ({toks}) keep {kept}"));
    }
    let padding = if scanning > kept {
        ", plus entry-boundary padding"
    } else {
        ""
    };
    crate::note!(
        "timberfs: {}: scanning {scanning} of {} chunk(s) ({}{padding}){}{}",
        p.display(),
        records.len(),
        parts.join(", "),
        match idle {
            Some(r) if windowed => format!("; token index idle — {r}"),
            _ => String::new(),
        },
        if auto {
            "; --scan reads everything"
        } else {
            ""
        }
    );
}

/// Does this string name an EXISTING timberfs source (backing pair by
/// any of its names, or a bundle file)? Used to catch the forgotten-
/// PATTERN footgun: grep's first positional is the pattern, so a missing
/// pattern silently promotes the file into it.
pub fn names_timberfs_source(s: &str) -> bool {
    let p = Path::new(s);
    if is_bundle(p) {
        return p.is_file();
    }
    match resolve_backing(p) {
        Ok((dir, name)) => crate::format::rings_path(&dir, &name).exists(),
        Err(_) => false,
    }
}

/// Tokens a SUBSTRING match provably requires whole in any matching
/// entry: the alphanumeric runs strictly INSIDE the literal, bounded by
/// non-alphanumerics on both sides within it. Edge runs may extend in
/// the entry ("needle" can be "needles", "this" can be "Xthis") and
/// prove nothing — but "this is the needle" requires the word "the".
pub fn interior_tokens(lit: &str) -> Vec<String> {
    let b = lit.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphanumeric() {
            let start = i;
            while i < b.len() && b[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let bounded = start > 0 && i < b.len();
            if bounded && (3..=64).contains(&(i - start)) {
                out.push(lit[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A literal matched at token boundaries — the default mode. "ERROR"
/// matches the WORD ERROR ([ERROR], "ERROR:"), not ERRORS or
/// PROTOCOLERROR: the same whole-token semantics as the .grain, which is
/// exactly what makes the index pre-filter exact rather than
/// approximate. (?-u): entries are raw bytes, boundaries are ASCII.
pub fn word_pattern(lit: &str) -> String {
    format!(
        r"(?:\A|(?-u:[^0-9A-Za-z])){}(?:(?-u:[^0-9A-Za-z])|\z)",
        regex::escape(lit)
    )
}

fn word_regex(lit: &str, ignore_case: bool) -> anyhow::Result<Regex> {
    RegexBuilder::new(&word_pattern(lit))
        .case_insensitive(ignore_case)
        .build()
        .with_context(|| format!("bad pattern {lit:?}"))
}

/// Engage the automatic token pre-filter: when the pattern is a plain
/// literal and THIS source has a .grain, the pattern's tokens select
/// chunks exactly as --has would (any entry matching a literal must
/// contain all its tokens) — JOINING any explicit --has requirements,
/// since both are entry-level ANDs and so compose as a chunk-level AND. Silent no-op when
/// there is no grain — nothing to accelerate with, and the full scan is
/// the answer, not a warning.
fn engage_auto_has(
    p: &Path,
    has: &[String],
    auto_has: &[String],
    any_of: &[String],
    scan_reason: Option<&str>,
    windowed: bool,
    records: &[ChunkRecord],
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    if is_bundle(p) {
        return Ok((has.to_vec(), Vec::new()));
    }
    let Ok((dir, base)) = resolve_backing(p) else {
        return Ok((has.to_vec(), Vec::new()));
    };
    if !crate::format::grain_path(&dir, &base).exists() {
        // select_chunks warns about an explicit --has with no grain.
        return Ok((has.to_vec(), Vec::new()));
    }
    if auto_has.is_empty() && any_of.is_empty() && !has.is_empty() {
        // Only the explicit requirements filter chunks.
        return Ok((has.to_vec(), Vec::new()));
    }
    if auto_has.is_empty() && !any_of.is_empty() {
        // OR'd word alternatives: the union of exact branches.
        crate::note!(
            "timberfs: {}: pre-filtering on word alternatives ({})",
            p.display(),
            any_of.join(" | ")
        );
        return Ok((has.to_vec(), any_of.to_vec()));
    }
    if auto_has.is_empty() {
        // There IS an index here, and we are about to ignore it — say why
        // once, so the cost is never a mystery.
        if let Some(reason) = scan_reason {
            // With a time window in play this is NOT a full scan (the
            // window still narrows); the reason then rides on the scan
            // report line instead.
            if !windowed {
                crate::note!(
                    "timberfs: {}: full scan of {} chunk(s): {reason} cannot use the .grain \
                     token index",
                    p.display(),
                    records.len()
                );
            }
        }
        return Ok((has.to_vec(), Vec::new()));
    }
    // Pattern tokens JOIN the explicit requirements (AND at chunk level,
    // exactly as the predicates AND at entry level).
    let mut merged = has.to_vec();
    merged.extend(auto_has.iter().cloned());
    Ok((merged, any_of.to_vec()))
}

/// The invocation as the user typed it (argv, shell-quoted, argv[0]
/// normalized to "timberfs") — the most informative operation fact an
/// investigation artifact can carry: what question produced it.
fn command_line() -> String {
    fn quote(a: &str) -> String {
        let plain = !a.is_empty()
            && a.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_./=:%+,@^".contains(&b));
        if plain {
            a.to_string()
        } else {
            format!("'{}'", a.replace('\'', "'\\''"))
        }
    }
    std::iter::once("timberfs".to_string())
        .chain(std::env::args().skip(1).map(|a| quote(&a)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `grep --into DEST`: write the matching entries into a NEW store, with
/// a bark carrying lineage and the operation facts. Streaming: entries
/// are re-stamped from their own timestamps (continuation lines stay
/// glued to their entry) and appended as the match runs.
#[allow(clippy::too_many_arguments)]
fn grep_into(
    p: &Path,
    res: &[Regex],
    required: &[Regex],
    extractor: Extractor,
    from: Option<u64>,
    to: Option<u64>,
    has: &[String],
    auto_has: &[String],
    any_of: &[String],
    scan_reason: Option<&str>,
    invert: bool,
    pattern: &str,
    dest: &Path,
    fail_on_empty: bool,
) -> anyhow::Result<()> {
    let source = open_source(p)?;
    let from_ms = from.unwrap_or(0);
    let to_ms = to.unwrap_or(u64::MAX);
    if from_ms > to_ms {
        bail!("--from is after --to");
    }
    let auto = has.is_empty() && !auto_has.is_empty();
    let (has, any_of) = engage_auto_has(
        p,
        has,
        auto_has,
        any_of,
        scan_reason,
        from.is_some() || to.is_some(),
        &source.records,
    )?;
    let window = if from.is_none() && to.is_none() {
        None
    } else {
        Some((from_ms, to_ms))
    };
    let (sel_from, sel_to) = match window {
        Some(_) => (
            from_ms.saturating_sub(crate::query::WIDEN_MS),
            to_ms.saturating_add(crate::query::WIDEN_MS),
        ),
        None => (from_ms, to_ms),
    };
    let (chunks, in_window, kept) =
        padded_chunks(p, &source.records, sel_from, sel_to, &has, &any_of)?;
    report_scan(
        p,
        &source.records,
        from.is_some() || to.is_some(),
        &has,
        auto && !has.is_empty(),
        scan_reason,
        chunks.len(),
        in_window,
        kept,
    );

    // The pair is built in place for a pair destination, or in a scratch
    // directory for a .timber bundle (assembled into the tar afterwards).
    let bundled = is_bundle(dest);
    let (pair_dir, pair_name, _locks) = if bundled {
        if dest.exists() {
            bail!(
                "{} already exists — grep --into always creates",
                dest.display()
            );
        }
        let parent = match dest.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        };
        let stem = dest
            .file_stem()
            .and_then(|s| s.to_str())
            .context("bad bundle name")?
            .to_string();
        let tmp = parent.join(format!(".{stem}.grep-tmp.{}", std::process::id()));
        fs::create_dir_all(&tmp)?;
        (tmp, stem, None)
    } else {
        crate::query::ensure_dest_is_not_plain_file(dest, "grep")?;
        let (d, n) = resolve_backing(dest)?;
        fs::create_dir_all(&d)?;
        if crate::format::rings_path(&d, &n).exists() || crate::format::trunk_path(&d, &n).exists()
        {
            bail!(
                "{n} already exists in {} — grep --into always creates; merge with import",
                d.display()
            );
        }
        let dir_lock = store::lock_backing_shared(&d)?.with_context(|| {
            format!(
                "destination directory {} is served by a timberfs mount",
                d.display()
            )
        })?;
        let file_lock = store::lock_file_exclusive(&d, &n)?
            .with_context(|| format!("{n} already has a writer"))?;
        (d, n, Some((dir_lock, file_lock)))
    };
    let cleanup = |dir: &Path| {
        if bundled {
            let _ = fs::remove_dir_all(dir);
        } else {
            let _ = fs::remove_file(crate::format::trunk_path(dir, &pair_name));
            let _ = fs::remove_file(crate::format::rings_path(dir, &pair_name));
        }
    };

    let cfg = store::Config {
        chunk_size: 256 * 1024,
        level: 3,
        flush_age_ms: 5000,
    };
    let mut st = store::Store {
        dir: pair_dir.clone(),
        cfg,
        files: std::collections::BTreeMap::new(),
    };
    st.create(&pair_name)?;

    let mut entries = Entries {
        reader: BufReader::with_capacity(
            1 << 20,
            ChunkStream {
                file: source.file,
                chunks,
                idx: 0,
                buf: Vec::new(),
                pos: 0,
            },
        ),
        extractor,
        pending: None,
        warned_cap: false,
    };
    let mut matched: u64 = 0;
    let mut seen: u64 = 0;
    let mut last_ts: Option<u64> = None;
    let mut leading: Vec<Vec<u8>> = Vec::new(); // matches before the first stamp
    let mut span: Option<(u64, u64)> = None;
    while let Some(entry) = entries.next_entry()? {
        seen += 1;
        let head = String::from_utf8_lossy(&entry[..entry.len().min(256)]);
        let ts = entries.extractor.extract(&head);
        // Same default as printing: the artifact holds only entries whose
        // own timestamps fall inside the asked window (unknowable stays).
        let in_time = match (window, ts.or(last_ts)) {
            (Some((f, t)), Some(e)) => e >= f && e <= t,
            _ => true,
        };
        let has_ok = required.is_empty() || is_match(required, &entry);
        if in_time && has_ok && is_match(res, &entry) != invert {
            matched += 1;
            match ts.or(last_ts) {
                Some(t) => {
                    let f = st.files.get_mut(&pair_name).unwrap();
                    for l in leading.drain(..) {
                        f.append_stamped(&l, t, &cfg)?;
                    }
                    f.append_stamped(&entry, t, &cfg)?;
                    span = Some(match span {
                        None => (t, t),
                        Some((a, b)) => (a.min(t), b.max(t)),
                    });
                }
                None => leading.push(entry.clone()),
            }
        }
        if let Some(t) = ts {
            last_ts = Some(t);
        }
    }
    if !leading.is_empty() {
        cleanup(&pair_dir);
        bail!(
            "{} matching entr{} carried no timestamp and none followed; \
             try --timestamp-regex/--timestamp-format",
            leading.len(),
            if leading.len() == 1 { "y" } else { "ies" }
        );
    }
    if matched == 0 && fail_on_empty {
        cleanup(&pair_dir);
        bail!("no entries matched (--fail-on-empty)");
    }
    st.flush_all();

    // The bark: lineage + the operation, fully told.
    let mut derived = crate::bark::derived_map(source.bark.as_ref(), "grep");
    if from.is_some() {
        derived.insert(
            "window_from".to_string(),
            Value::String(crate::bark::ms_rfc3339(from_ms)),
        );
    }
    if to.is_some() {
        derived.insert(
            "window_to".to_string(),
            Value::String(crate::bark::ms_rfc3339(to_ms)),
        );
    }
    derived.insert("pattern".to_string(), Value::String(pattern.to_string()));
    derived.insert("command".to_string(), Value::String(command_line()));
    let bark_map = crate::bark::with_identity(derived)?;
    let bark_text = serde_json::to_string_pretty(&Value::Object(bark_map))? + "\n";

    if bundled {
        let res = assemble_bundle(&pair_dir, &pair_name, dest, &bark_text, span);
        cleanup(&pair_dir);
        res?;
    } else {
        fs::write(crate::format::bark_path(&pair_dir, &pair_name), bark_text)?;
    }
    match span {
        Some((a, b)) => crate::note!(
            "timberfs: grep: {matched} of {seen} entr{} matched, spanning {} .. {} -> {}{}",
            if matched == 1 { "y" } else { "ies" },
            fmt_ms(a),
            fmt_ms(b),
            dest.display(),
            if bundled { " (bundle)" } else { "" }
        ),
        None => crate::note!(
            "timberfs: grep: 0 of {seen} entries matched — empty result; the artifact \
             attests it (--fail-on-empty to error instead) -> {}{}",
            dest.display(),
            if bundled { " (bundle)" } else { "" }
        ),
    }
    Ok(())
}

/// Tar the scratch pair into `dest` as a bundle: rings, bark, trunk —
/// metadata first, bulk last, the same layout export writes.
fn assemble_bundle(
    pair_dir: &Path,
    name: &str,
    dest: &Path,
    bark_text: &str,
    span: Option<(u64, u64)>,
) -> anyhow::Result<()> {
    let mtime_secs = span.map(|(_, b)| b / 1000).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });
    let out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut builder = tar::Builder::new(out);
    let mut add = |member: String, bytes_len: u64, reader: &mut dyn Read| -> anyhow::Result<()> {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_mtime(mtime_secs);
        header.set_size(bytes_len);
        builder.append_data(&mut header, member, reader)?;
        Ok(())
    };
    let rings = fs::read(crate::format::rings_path(pair_dir, name))?;
    add(format!("{name}.rings"), rings.len() as u64, &mut &rings[..])?;
    add(
        format!("{name}.bark"),
        bark_text.len() as u64,
        &mut bark_text.as_bytes(),
    )?;
    let trunk_path = crate::format::trunk_path(pair_dir, name);
    let trunk_len = fs::metadata(&trunk_path)?.len();
    let mut trunk = File::open(&trunk_path)?;
    add(format!("{name}.trunk"), trunk_len, &mut trunk)?;
    let out = builder.into_inner()?;
    out.sync_all()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn grep_one(
    p: &Path,
    res: &[Regex],
    required: &[Regex],
    extractor: Extractor,
    from: Option<u64>,
    to: Option<u64>,
    has: &[String],
    auto_has: &[String],
    any_of: &[String],
    scan_reason: Option<&str>,
    invert: bool,
    count: bool,
    prefix: Option<&[u8]>,
    null_sep: bool,
    by_write_time: bool,
) -> anyhow::Result<u64> {
    let is_timberfs_source = is_bundle(p)
        || matches!(
            p.extension().and_then(|e| e.to_str()),
            Some(crate::format::TRUNK_EXT) | Some(crate::format::RINGS_EXT)
        )
        || !p.is_file();

    if !is_timberfs_source {
        // a plain log file at the exact path
        if from.is_some() || to.is_some() {
            bail!(
                "--from/--to need a timberfs log or bundle; {} is a plain file",
                p.display()
            );
        }
        return run(
            Entries {
                reader: BufReader::new(
                    File::open(p).with_context(|| format!("opening {}", p.display()))?,
                ),
                extractor,
                pending: None,
                warned_cap: false,
            },
            res,
            required,
            invert,
            count,
            prefix,
            None,
            null_sep,
        );
    }
    let source = open_source(p)?;
    let from_ms = from.unwrap_or(0);
    let to_ms = to.unwrap_or(u64::MAX);
    if from_ms > to_ms {
        bail!("--from is after --to");
    }
    let auto = has.is_empty() && !auto_has.is_empty();
    let (has, any_of) = engage_auto_has(
        p,
        has,
        auto_has,
        any_of,
        scan_reason,
        from.is_some() || to.is_some(),
        &source.records,
    )?;
    // The default verifies entries against the window by their own
    // timestamps, so the SELECTION widens to catch buffered stragglers;
    // --by-write-time keeps the raw chunk-window behavior.
    let window = if by_write_time || (from.is_none() && to.is_none()) {
        None
    } else {
        Some((from_ms, to_ms))
    };
    let (sel_from, sel_to) = match window {
        Some(_) => (
            from_ms.saturating_sub(crate::query::WIDEN_MS),
            to_ms.saturating_add(crate::query::WIDEN_MS),
        ),
        None => (from_ms, to_ms),
    };
    let (chunks, in_window, kept) =
        padded_chunks(p, &source.records, sel_from, sel_to, &has, &any_of)?;
    report_scan(
        p,
        &source.records,
        from.is_some() || to.is_some(),
        &has,
        auto && !has.is_empty(),
        scan_reason,
        chunks.len(),
        in_window,
        kept,
    );
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
        res,
        required,
        invert,
        count,
        prefix,
        window,
        null_sep,
    )
}

#[allow(clippy::too_many_arguments)]
/// The pattern side of a grep invocation: an optional positional pattern
/// (word mode unless a bare --regex/--substring flags it) plus any number of
/// attached predicates. The composition rule, stated once: repeating a
/// flag ORs, different kinds AND, --has always ANDs, and -v inverts the
/// whole pattern conjunction.
pub struct MatchSpec {
    pub positional: Option<String>,
    pub word_alts: Vec<String>,
    pub regex_alts: Vec<String>,
    pub substring_alts: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_grep(
    spec: MatchSpec,
    files: &[std::path::PathBuf],
    from: Option<u64>,
    to: Option<u64>,
    has: &[String],
    ignore_case: bool,
    invert: bool,
    count: bool,
    no_filename: bool,
    ts_regex: Option<&str>,
    ts_format: Option<&str>,
    into: Option<&Path>,
    fail_on_empty: bool,
    scan: bool,
    null_sep: bool,
    by_write_time: bool,
) -> anyhow::Result<()> {
    let mut files: Vec<std::path::PathBuf> = files.to_vec();
    // Repeated --has arguments are a set, not a list: dedup up front so
    // matchers, notes and the chunk filter all agree (exact-case,
    // first-occurrence order).
    let has: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        has.iter()
            .filter(|h| seen.insert(h.as_str()))
            .cloned()
            .collect()
    };
    let has: &[String] = &has;
    let selection_given = from.is_some() || to.is_some() || !has.is_empty();
    let has_alts = !spec.word_alts.is_empty()
        || !spec.regex_alts.is_empty()
        || !spec.substring_alts.is_empty()
        || !has.is_empty();
    let mut positional = spec.positional;

    // Positional disambiguation. With attached predicates in play, a
    // positional that names an existing store is a FILE
    // (--regex='a|b' file.log); otherwise it is a word-mode predicate
    // (needle --substring='exact' file.log).
    if has_alts {
        if let Some(p) = &positional {
            // ...an existing store OR plain file (grep reads both).
            if names_timberfs_source(p) || std::path::Path::new(p).is_file() {
                files.insert(0, std::path::PathBuf::from(p));
                positional = None;
            }
        }
    }
    // The forgotten-PATTERN shift: no predicates at all, the "pattern"
    // names a store, and selection flags are given — it IS the file, and
    // the selection matches instead.
    if !has_alts {
        if let Some(p) = positional.clone() {
            if selection_given && names_timberfs_source(&p) {
                files.insert(0, std::path::PathBuf::from(&p));
                positional = None;
            } else if files.is_empty() && !selection_given && names_timberfs_source(&p) {
                use std::io::IsTerminal;
                if io::stdin().is_terminal() {
                    bail!(
                        "{p} names a timberfs log, but it was read as the PATTERN \
                         (which comes first: timberfs grep PATTERN {p}); to select \
                         without a pattern, give --has/--from/--to"
                    );
                }
            }
        }
    }

    // Assemble the AND-list of predicates.
    let mut res: Vec<Regex> = Vec::new();
    let mut desc: Vec<String> = Vec::new();
    {
        if let Some(pat) = &positional {
            // the bare positional is grep legacy: always word mode
            res.push(word_regex(pat, ignore_case)?);
            desc.push(pat.clone());
        }
        if !spec.word_alts.is_empty() {
            let joined = spec
                .word_alts
                .iter()
                .map(|w| format!("(?:{})", word_pattern(w)))
                .collect::<Vec<_>>()
                .join("|");
            res.push(
                RegexBuilder::new(&joined)
                    .case_insensitive(ignore_case)
                    .build()
                    .with_context(|| "bad --any text".to_string())?,
            );
            desc.push(format!("({})", spec.word_alts.join("|")));
        }
        if !spec.regex_alts.is_empty() {
            let joined = spec
                .regex_alts
                .iter()
                .map(|p| format!("(?:{p})"))
                .collect::<Vec<_>>()
                .join("|");
            res.push(
                RegexBuilder::new(&joined)
                    .case_insensitive(ignore_case)
                    .multi_line(true)
                    .build()
                    .with_context(|| format!("bad --regex pattern {joined:?}"))?,
            );
            desc.push(format!("({})", spec.regex_alts.join("|")));
        }
        if !spec.substring_alts.is_empty() {
            let joined = spec
                .substring_alts
                .iter()
                .map(|p| format!("(?:{})", regex::escape(p)))
                .collect::<Vec<_>>()
                .join("|");
            res.push(
                RegexBuilder::new(&joined)
                    .case_insensitive(ignore_case)
                    .build()
                    .with_context(|| "bad --substring text".to_string())?,
            );
            desc.push(format!("({})", spec.substring_alts.join("|")));
        }
    }
    // No patterns at all: the requirements (or the window) select instead
    // — on ANY source; the chunk skip is just the timberfs acceleration.
    let matchless = res.is_empty();
    if matchless {
        if invert {
            bail!(
                "-v inverts patterns, and none were given (--has requirements are \
                 never inverted); \"A and not B\" is --has A -v B"
            );
        }
        if !has.is_empty() {
            crate::note!(
                "timberfs: no PATTERN given; matching entries that contain: {}",
                has.join(", ")
            );
        } else if selection_given {
            crate::note!("timberfs: no PATTERN given; every entry in the window matches");
        } else {
            bail!(
                "PATTERN required (positionally, or as --any TEXT / --regex PATTERN / \
                 --substring TEXT); or select with --has/--from/--to"
            );
        }
    }
    let pattern_desc = if desc.is_empty() {
        has.join(" AND ")
    } else {
        desc.join(" AND ")
    };

    // --has is verified on every entry, always — EXACT-case, like the
    // index it rides on (-i loosens patterns, not requirements: a looser
    // entry check than the chunk skip would miss).
    let required: Vec<Regex> = if !has.is_empty() {
        has.iter()
            .map(|t| word_regex(t, false))
            .collect::<anyhow::Result<_>>()?
    } else {
        Vec::new()
    };
    if ignore_case && !has.is_empty() {
        crate::note!(
            "timberfs: -i applies to patterns; --has requirements stay \
             exact-case (they ride the exact-case token index)"
        );
    }

    // Index acceleration: a WORD-mode positional's tokens are required in
    // any matching entry, and extra AND-predicates only narrow the result
    // — so the chunk skip stays exact even in mixed queries, and the
    // tokens simply join any explicit --has requirements. Off for -i
    // (the grain is exact-case), -v (non-matches must be read to be
    // printed), --scan.
    let word_pat: Option<&String> = positional.as_ref();
    // A single SUBSTRING literal contributes its interior tokens the
    // same way (a substring match requires them whole); OR'd substring
    // alternatives contribute nothing (each branch may lack them).
    let substring_lit: Option<&String> = if spec.substring_alts.len() == 1 {
        spec.substring_alts.first()
    } else {
        None
    };
    let mut accel: Vec<String> = Vec::new();
    if let Some(p) = word_pat {
        if !crate::grain::tokenize_query(p).is_empty() {
            accel.push(p.clone());
        }
    }
    if spec.word_alts.len() == 1 && !crate::grain::tokenize_query(&spec.word_alts[0]).is_empty() {
        accel.push(spec.word_alts[0].clone());
    }
    if let Some(l) = substring_lit {
        accel.extend(interior_tokens(l));
    }
    // Explicit --has does NOT disable pattern-token acceleration: chunk
    // requirements compose by AND exactly like the entry predicates do.
    let gates_open = !scan && !invert && !ignore_case;
    // OR'd words stay indexed: a chunk survives if ANY alternative's
    // tokens are all present — each branch is exact, so the union is.
    let any_of: Vec<String> = if gates_open
        && spec.word_alts.len() > 1
        && spec
            .word_alts
            .iter()
            .all(|w| !crate::grain::tokenize_query(w).is_empty())
    {
        spec.word_alts.clone()
    } else {
        Vec::new()
    };
    let auto_has: Vec<String> = if gates_open && !accel.is_empty() {
        accel
    } else {
        Vec::new()
    };
    // Why a grain-ful store will still be fully scanned (for the hint).
    let scan_reason: Option<String> = if auto_has.is_empty() && has.is_empty() && !scan {
        if ignore_case {
            Some("-i (the index is exact-case)".into())
        } else if invert {
            Some("-v (non-matches must be read to be printed)".into())
        } else if word_pat.is_none() && spec.word_alts.is_empty() && !spec.regex_alts.is_empty() {
            Some("--regex".into())
        } else if word_pat.is_none() && spec.word_alts.is_empty() && !spec.substring_alts.is_empty()
        {
            // A one-word literal deserves the real lesson: without
            // --substring, this exact search is index-accelerated; --substring
            // only adds matches INSIDE longer tokens.
            match substring_lit {
                Some(l)
                    if l.bytes().all(|b| b.is_ascii_alphanumeric())
                        && (3..=64).contains(&l.len()) =>
                {
                    Some(format!(
                        "--substring ({l:?} is one whole word — as --any {l} this is an \
                         indexed search; --substring adds only matches inside longer tokens)"
                    ))
                }
                _ => Some("--substring (raw text with no interior words to pre-filter on)".into()),
            }
        } else {
            None
        }
    } else {
        None
    };
    let scan_reason: Option<&str> = scan_reason.as_deref();

    if let Some(dest) = into {
        // The artifact derives from ONE timberfs source: lineage stays
        // unambiguous, and window/--has pre-selection applies. (Grep
        // across a fleet is the future entry-aware merge's job.)
        let [p] = &files[..] else {
            bail!(
                "grep --into wants exactly one timberfs source \
                 (got {} — merge or import first)",
                if files.is_empty() {
                    "stdin".to_string()
                } else {
                    format!("{} files", files.len())
                }
            );
        };
        let is_timberfs_source = is_bundle(p)
            || matches!(
                p.extension().and_then(|e| e.to_str()),
                Some(crate::format::TRUNK_EXT) | Some(crate::format::RINGS_EXT)
            )
            || !p.is_file();
        if !is_timberfs_source {
            bail!(
                "{} is a plain file — grep --into derives from a timberfs log or \
                 bundle (import it first)",
                p.display()
            );
        }
        let extractor = Extractor::new(ts_regex, ts_format, false)?;
        return grep_into(
            p,
            &res,
            &required,
            extractor,
            from,
            to,
            has,
            &auto_has,
            &any_of,
            scan_reason,
            invert,
            &pattern_desc,
            dest,
            fail_on_empty,
        );
    }

    if files.is_empty() {
        if from.is_some() || to.is_some() {
            bail!(
                "--from/--to need a timberfs log or bundle, not stdin \
                 (PATTERN comes first: timberfs grep PATTERN FILE)"
            );
        }
        let extractor = Extractor::new(ts_regex, ts_format, false)?;
        let stdin = io::stdin();
        let matched = run(
            Entries {
                reader: stdin.lock(),
                extractor,
                pending: None,
                warned_cap: false,
            },
            &res,
            &required,
            invert,
            count,
            None,
            None,
            null_sep,
        )?;
        if count {
            println!("{matched}");
        }
        return Ok(());
    }

    // Files process in argument order (grep semantics); multiple files get
    // grep-style "path:" line prefixes and per-file -c counts.
    let multi = files.len() > 1 && !no_filename;
    let mut total: u64 = 0;
    for p in &files {
        // A "file" that is neither a plain file nor a timberfs store is
        // usually a stray pattern (the pattern comes FIRST, or rides in
        // --regex PATTERN) — say so instead of blaming a missing .rings.
        if !p.is_file() && !names_timberfs_source(&p.display().to_string()) {
            bail!(
                "{} is neither a file nor a timberfs log — if it was meant as a \
                 pattern, the pattern comes first (or use --regex PATTERN)",
                p.display()
            );
        }
        let extractor = Extractor::new(ts_regex, ts_format, false)?;
        let label = p.display().to_string().into_bytes();
        let matched = grep_one(
            p,
            &res,
            &required,
            extractor,
            from,
            to,
            has,
            &auto_has,
            &any_of,
            scan_reason,
            invert,
            count,
            if multi { Some(&label) } else { None },
            null_sep,
            by_write_time,
        )?;
        total += matched;
        if count && multi {
            println!("{}:{matched}", p.display());
        }
    }
    if count && !multi {
        println!("{total}");
    }
    Ok(())
}
