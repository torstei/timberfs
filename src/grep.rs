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
//! the pattern-less --has form, or piped greps.) -F (raw substring —
//! reaches inside tokens, for partial ids) and --regex (full regex) read
//! every chunk, as must -i (the grain is exact-case) and -v (non-matches
//! must be READ to be printed; the index proving "no matches here" means
//! print the whole chunk, not skip it). When a grain exists but cannot
//! be used, a one-line note says why — the cost is never a mystery.
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

/// An entry matches when EVERY regex matches it (an empty list matches
/// everything — the pure-window case). One user pattern is a list of one;
/// --has-derived token matching is a list of escaped tokens, ANDed.
fn is_match(res: &[Regex], entry: &[u8]) -> bool {
    res.iter().all(|re| re.is_match(entry))
}

fn run<R: BufRead>(
    mut entries: Entries<R>,
    res: &[Regex],
    invert: bool,
    count: bool,
    prefix: Option<&[u8]>,
) -> anyhow::Result<u64> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut matched: u64 = 0;
    while let Some(entry) = entries.next_entry()? {
        if is_match(res, &entry) != invert {
            matched += 1;
            if !count {
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
) -> anyhow::Result<(Vec<ChunkRecord>, usize, usize)> {
    let (selected, in_window) = select_chunks(p, records, from_ms, to_ms, has)?;
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
        "timberfs: {}: scanning {scanning} of {} chunk(s) ({}{padding}){}",
        p.display(),
        records.len(),
        parts.join(", "),
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
fn names_timberfs_source(s: &str) -> bool {
    let p = Path::new(s);
    if is_bundle(p) {
        return p.is_file();
    }
    match resolve_backing(p) {
        Ok((dir, name)) => crate::format::rings_path(&dir, &name).exists(),
        Err(_) => false,
    }
}

/// A literal matched at token boundaries — the default mode. "ERROR"
/// matches the WORD ERROR ([ERROR], "ERROR:"), not ERRORS or
/// PROTOCOLERROR: the same whole-token semantics as the .grain, which is
/// exactly what makes the index pre-filter exact rather than
/// approximate. (?-u): entries are raw bytes, boundaries are ASCII.
fn word_regex(lit: &str, ignore_case: bool) -> anyhow::Result<Regex> {
    let pat = format!(
        r"(?:\A|(?-u:[^0-9A-Za-z])){}(?:(?-u:[^0-9A-Za-z])|\z)",
        regex::escape(lit)
    );
    RegexBuilder::new(&pat)
        .case_insensitive(ignore_case)
        .build()
        .with_context(|| format!("bad pattern {lit:?}"))
}

/// Engage the automatic token pre-filter: when no explicit --has was
/// given, the pattern is a plain literal, and THIS source has a .grain,
/// the pattern's tokens select chunks exactly as --has would (any entry
/// matching a literal must contain all its tokens). Silent no-op when
/// there is no grain — nothing to accelerate with, and the full scan is
/// the answer, not a warning.
fn engage_auto_has(
    p: &Path,
    has: &[String],
    auto_has: &[String],
    scan_reason: Option<&str>,
    records: &[ChunkRecord],
) -> anyhow::Result<Vec<String>> {
    if !has.is_empty() || is_bundle(p) {
        return Ok(has.to_vec());
    }
    let Ok((dir, base)) = resolve_backing(p) else {
        return Ok(Vec::new());
    };
    if !crate::format::grain_path(&dir, &base).exists() {
        return Ok(Vec::new());
    }
    if auto_has.is_empty() {
        // There IS an index here, and we are about to ignore it — say why
        // once, so the cost is never a mystery.
        if let Some(reason) = scan_reason {
            crate::note!(
                "timberfs: {}: full scan of {} chunk(s): {reason} cannot use the .grain \
                 token index",
                p.display(),
                records.len()
            );
        }
        return Ok(Vec::new());
    }
    Ok(auto_has.to_vec())
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
    extractor: Extractor,
    from: Option<u64>,
    to: Option<u64>,
    has: &[String],
    auto_has: &[String],
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
    let has = engage_auto_has(p, has, auto_has, scan_reason, &source.records)?;
    let (chunks, in_window, kept) = padded_chunks(p, &source.records, from_ms, to_ms, &has)?;
    report_scan(
        p,
        &source.records,
        from.is_some() || to.is_some(),
        &has,
        auto && !has.is_empty(),
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
        if is_match(res, &entry) != invert {
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
    extractor: Extractor,
    from: Option<u64>,
    to: Option<u64>,
    has: &[String],
    auto_has: &[String],
    scan_reason: Option<&str>,
    invert: bool,
    count: bool,
    prefix: Option<&[u8]>,
) -> anyhow::Result<u64> {
    let is_timberfs_source = is_bundle(p)
        || matches!(
            p.extension().and_then(|e| e.to_str()),
            Some(crate::format::TRUNK_EXT) | Some(crate::format::RINGS_EXT)
        )
        || !p.is_file();

    if !is_timberfs_source {
        // a plain log file at the exact path
        if from.is_some() || to.is_some() || !has.is_empty() {
            bail!(
                "--from/--to/--has need a timberfs log or bundle; {} is a plain file",
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
            invert,
            count,
            prefix,
        );
    }
    let source = open_source(p)?;
    let from_ms = from.unwrap_or(0);
    let to_ms = to.unwrap_or(u64::MAX);
    if from_ms > to_ms {
        bail!("--from is after --to");
    }
    let auto = has.is_empty() && !auto_has.is_empty();
    let has = engage_auto_has(p, has, auto_has, scan_reason, &source.records)?;
    let (chunks, in_window, kept) = padded_chunks(p, &source.records, from_ms, to_ms, &has)?;
    report_scan(
        p,
        &source.records,
        from.is_some() || to.is_some(),
        &has,
        auto && !has.is_empty(),
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
        invert,
        count,
        prefix,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_grep(
    pattern: &str,
    files: &[std::path::PathBuf],
    from: Option<u64>,
    to: Option<u64>,
    has: &[String],
    ignore_case: bool,
    invert: bool,
    fixed: bool,
    count: bool,
    no_filename: bool,
    ts_regex: Option<&str>,
    ts_format: Option<&str>,
    into: Option<&Path>,
    fail_on_empty: bool,
    scan: bool,
    regex_mode: bool,
) -> anyhow::Result<()> {
    // A forgotten PATTERN puts the file first — the first positional
    // silently becomes the pattern, and the resulting "not stdin" error
    // lies to a user who plainly passed a file. When the "pattern" names
    // an existing timberfs source AND selection flags are given (today a
    // guaranteed error or a nonsense match), the intent is unambiguous:
    // it IS the file, and the selection matches instead — entries
    // containing every --has token, or every entry in a pure window.
    let mut files: Vec<std::path::PathBuf> = files.to_vec();
    let mut pattern = pattern.to_string();
    let selection_given = from.is_some() || to.is_some() || !has.is_empty();
    let pattern_is_matchless = if selection_given && names_timberfs_source(&pattern) {
        files.insert(0, std::path::PathBuf::from(&pattern));
        if has.is_empty() {
            crate::note!("timberfs: no PATTERN given; every entry in the window matches");
            pattern = String::new();
        } else {
            crate::note!(
                "timberfs: no PATTERN given; matching entries that contain: {}",
                has.join(", ")
            );
            pattern = has.join(" AND ");
        }
        true
    } else {
        false
    };
    if files.is_empty() && !selection_given && names_timberfs_source(&pattern) {
        use std::io::IsTerminal;
        if io::stdin().is_terminal() {
            bail!(
                "{pattern} names a timberfs log, but it was read as the PATTERN \
                 (which comes first: timberfs grep PATTERN {pattern}); to select \
                 without a pattern, give --has/--from/--to"
            );
        }
    }

    let res: Vec<Regex> = if pattern_is_matchless {
        // --has tokens, ANDed, word-anchored like everything else
        // (empty = match everything, the pure-window case).
        has.iter()
            .map(|t| word_regex(t, ignore_case))
            .collect::<anyhow::Result<_>>()?
    } else if regex_mode {
        vec![RegexBuilder::new(&pattern)
            .case_insensitive(ignore_case)
            .multi_line(true)
            .build()
            .with_context(|| format!("bad pattern {pattern:?}"))?]
    } else if fixed {
        // raw substring — matches INSIDE tokens (partial ids), so the
        // token index cannot help; full scan
        vec![RegexBuilder::new(&regex::escape(&pattern))
            .case_insensitive(ignore_case)
            .build()
            .with_context(|| format!("bad pattern {pattern:?}"))?]
    } else {
        // the default: a literal phrase at token boundaries
        vec![word_regex(&pattern, ignore_case)?]
    };

    // The default word mode is EXACTLY what the .grain answers: an entry
    // word-matching a literal must contain all its tokens whole, so the
    // pre-filter cannot change the answer — a store with a grain gets the
    // chunk skip for free. Off when the semantics genuinely need every
    // chunk: --regex/-F (substrings reach inside tokens), -i (the grain
    // is exact-case), -v (non-matches must be READ to be printed — the
    // index can only prove absence of matches, which for -v means "print
    // the whole chunk"), an explicit --has (the user's selection wins),
    // and --scan.
    let auto_has: Vec<String> = if !scan
        && !invert
        && !ignore_case
        && !regex_mode
        && !fixed
        && !pattern_is_matchless
        && has.is_empty()
        && !crate::grain::tokenize_query(&pattern).is_empty()
    {
        vec![pattern.clone()]
    } else {
        Vec::new()
    };
    // Why a grain-ful store will still be fully scanned (for the hint).
    let scan_reason: Option<&str> = if auto_has.is_empty() && has.is_empty() && !scan {
        if regex_mode {
            Some("--regex")
        } else if fixed {
            Some("-F (raw substring)")
        } else if ignore_case {
            Some("-i (the index is exact-case)")
        } else if invert {
            Some("-v (non-matches must be read to be printed)")
        } else {
            None
        }
    } else {
        None
    };

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
            extractor,
            from,
            to,
            has,
            &auto_has,
            scan_reason,
            invert,
            &pattern,
            dest,
            fail_on_empty,
        );
    }

    if files.is_empty() {
        if selection_given {
            bail!(
                "--from/--to/--has need a timberfs log or bundle, not stdin \
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
            invert,
            count,
            None,
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
        let extractor = Extractor::new(ts_regex, ts_format, false)?;
        let label = p.display().to_string().into_bytes();
        let matched = grep_one(
            p,
            &res,
            extractor,
            from,
            to,
            has,
            &auto_has,
            scan_reason,
            invert,
            count,
            if multi { Some(&label) } else { None },
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
