//! timber-filter — pass whole log ENTRIES (a stamped line plus its
//! continuation lines: stack traces stay attached) matching named
//! predicates; the first consumer of the timberfs record stream
//! (timberfs-records(5)).
//!
//! Nothing shape-shifts and no flag modifies another: positionals are
//! always files, and every predicate flag carries its kind, its
//! polarity and its case rule in its own name. Select flags (--has,
//! --substring, --regex) AND on repetition; --any alternatives OR;
//! --not-* exclude; -caseless variants compare caselessly ((?i)
//! inside a --regex does the same there). Zero predicates is cat.
//! Store arguments are searched by spawning the sibling `timberfs
//! query --records` with --from/--to/--has passed through; a record
//! stream or raw log on stdin (or raw files) is matched as-is.
//!
//! Index acceleration is uniform by SHAPE: exact-case positives ride
//! the token index (--has whole, every --substring on its interior
//! words, a single --any whole); caseless variants cannot (the index
//! is exact-case) and excludes cannot (a Bloom filter cannot prove
//! absence). Everything else reads what the selection produced —
//! with a note when that is the whole store.

use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};
use clap::Parser;
use regex::bytes::{Regex, RegexBuilder};

use timberfs::grep::{interior_tokens, names_timberfs_source, word_pattern, Entries};
use timberfs::import::Extractor;
use timberfs::note;

const HEAD_SELECT: &str = "Select (every one must hold — repeat to AND)";
const HEAD_ANY: &str = "Alternatives (at least one must hold — repeat to OR)";
const HEAD_EXCLUDE: &str = "Exclude (none may hold)";
const HEAD_WINDOW: &str = "Time window (stores only; handed to the query layer)";
const HEAD_OUTPUT: &str = "Output";

/// Pass log entries matching named predicates — a stamped line plus
/// its continuation lines stays one entry. Positionals are always
/// files: timberfs stores (selected via the query layer), raw logs,
/// or record streams; default stdin, sniffed. Patterns are -e,
/// requirements are --has; with no -e, the requirements and the
/// window select
#[derive(Parser)]
#[command(name = "timber-filter", version)]
struct Cli {
    /// timberfs stores (selected via the query layer) or raw log
    /// files; default: stdin — a record stream or raw text, sniffed
    files: Vec<PathBuf>,

    /// Word-anchored phrase the entry must contain; rides the token
    /// index on stores
    #[arg(long, value_name = "TEXT", help_heading = HEAD_SELECT)]
    has: Vec<String>,
    /// As --has, compared caselessly (the exact-case index sits out)
    #[arg(long, value_name = "TEXT", help_heading = HEAD_SELECT)]
    has_caseless: Vec<String>,
    /// Literal the entry must contain, even inside longer words; a
    /// multi-word literal rides the index on its interior words
    #[arg(long, value_name = "TEXT", help_heading = HEAD_SELECT)]
    substring: Vec<String>,
    /// As --substring, compared caselessly
    #[arg(long, value_name = "TEXT", help_heading = HEAD_SELECT)]
    substring_caseless: Vec<String>,
    /// Regular expression the entry must match ((?i)... inside the
    /// pattern for caseless)
    #[arg(long, value_name = "PATTERN", help_heading = HEAD_SELECT)]
    regex: Vec<String>,

    /// Word-anchored phrase; at least one --any/--any-caseless must
    /// match. A single exact --any rides the token index
    #[arg(long, value_name = "TEXT", help_heading = HEAD_ANY)]
    any: Vec<String>,
    /// As --any, compared caselessly
    #[arg(long, value_name = "TEXT", help_heading = HEAD_ANY)]
    any_caseless: Vec<String>,

    /// Word-anchored phrase the entry must NOT contain
    #[arg(long, value_name = "TEXT", help_heading = HEAD_EXCLUDE)]
    not_has: Vec<String>,
    /// As --not-has, compared caselessly
    #[arg(long, value_name = "TEXT", help_heading = HEAD_EXCLUDE)]
    not_has_caseless: Vec<String>,
    /// Literal the entry must NOT contain anywhere
    #[arg(long, value_name = "TEXT", help_heading = HEAD_EXCLUDE)]
    not_substring: Vec<String>,
    /// As --not-substring, compared caselessly
    #[arg(long, value_name = "TEXT", help_heading = HEAD_EXCLUDE)]
    not_substring_caseless: Vec<String>,
    /// Regular expression the entry must NOT match
    #[arg(long, value_name = "PATTERN", help_heading = HEAD_EXCLUDE)]
    not_regex: Vec<String>,

    /// Start of the time window (formats as timberfs query)
    #[arg(long, value_name = "TIME", help_heading = HEAD_WINDOW)]
    from: Option<String>,
    /// End of the time window
    #[arg(long, value_name = "TIME", help_heading = HEAD_WINDOW)]
    to: Option<String>,

    /// Print only the number of matching entries
    #[arg(short = 'c', long, help_heading = HEAD_OUTPUT)]
    count: bool,
    /// Stop after at most N matching entries (a hard cap, like head -n)
    #[arg(long, value_name = "N", help_heading = HEAD_OUTPUT)]
    max: Option<u64>,
    /// NUL-terminated entry records (multiline entries stay one record)
    #[arg(short = '0', long = "null", help_heading = HEAD_OUTPUT)]
    null_sep: bool,
    /// Never prefix output lines with the source name
    #[arg(long, help_heading = HEAD_OUTPUT)]
    no_filename: bool,
    /// Typed record stream out, for the next timber-aware stage:
    /// entry metadata passes through verbatim, and this stage appends
    /// its command line to the stream-start lineage (stage=...). See
    /// timberfs-records(5)
    #[arg(long, help_heading = HEAD_OUTPUT, conflicts_with_all = ["count", "null_sep", "no_filename"])]
    records: bool,
    /// Suppress informational notes on stderr; errors still print
    #[arg(long)]
    quiet: bool,
    /// Custom entry-boundary timestamp for raw input: regex with one
    /// capture group
    #[arg(long, requires = "timestamp_format")]
    timestamp_regex: Option<String>,
    /// chrono format string for the captured timestamp
    #[arg(long, requires = "timestamp_regex")]
    timestamp_format: Option<String>,
}

fn main() {
    // Die quietly when a pipe closes (timber-filter | head), like any
    // Unix tool, instead of Rust's default panic-on-EPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    timberfs::note::set_quiet(cli.quiet);
    if let Err(e) = run(cli) {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let files = cli.files.clone();

    // Positionals are always files — a name that is neither a store
    // nor an existing file is the one first-contact mistake, so the
    // error teaches the grammar.
    for f in &files {
        let name = f.display().to_string();
        if !names_timberfs_source(&name) && !f.is_file() {
            bail!(
                "{name}: no such file (positionals are always files; \
                 predicates are named: --substring {name:?}, --has, --any, --regex, ...)"
            );
        }
    }

    // Repeated --has arguments are a set (exact-case, first-occurrence
    // order) — matchers, pushdown and the child's arguments must agree.
    let has: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        cli.has
            .iter()
            .filter(|h| seen.insert(h.as_str()))
            .cloned()
            .collect()
    };

    // The predicate algebra. Every flag carries its kind, polarity and
    // case rule in its name; nothing here is modal. Zero predicates is
    // not an error — a filter with an empty program is cat (grep '',
    // sed '', awk 1), here entry-aware, which also makes predicate-less
    // timber-filter the records-to-text converter.
    let mut select: Vec<Regex> = Vec::new();
    for t in &has {
        select.push(word_rx(t, false)?);
    }
    for t in &cli.has_caseless {
        select.push(word_rx(t, true)?);
    }
    for t in &cli.substring {
        select.push(sub_rx(t, false)?);
    }
    for t in &cli.substring_caseless {
        select.push(sub_rx(t, true)?);
    }
    for p in &cli.regex {
        select.push(user_rx(p, "--regex")?);
    }
    let any: Option<Regex> = if cli.any.is_empty() && cli.any_caseless.is_empty() {
        None
    } else {
        let mut branches: Vec<String> = cli.any.iter().map(|w| word_pattern(w)).collect();
        branches.extend(
            cli.any_caseless
                .iter()
                .map(|w| format!("(?i:{})", word_pattern(w))),
        );
        Some(
            RegexBuilder::new(&branches.join("|"))
                .multi_line(true)
                .build()
                .with_context(|| "bad --any".to_string())?,
        )
    };
    let mut exclude: Vec<Regex> = Vec::new();
    for t in &cli.not_has {
        exclude.push(word_rx(t, false)?);
    }
    for t in &cli.not_has_caseless {
        exclude.push(word_rx(t, true)?);
    }
    for t in &cli.not_substring {
        exclude.push(sub_rx(t, false)?);
    }
    for t in &cli.not_substring_caseless {
        exclude.push(sub_rx(t, true)?);
    }
    for p in &cli.not_regex {
        exclude.push(user_rx(p, "--not-regex")?);
    }
    let preds = Preds {
        select,
        any,
        exclude,
    };

    // Classify inputs: stores go through the selection layer, raw
    // files are read as they are. Mixing the two in one invocation
    // would mean two different --from/--to semantics — refused.
    let stores: Vec<&PathBuf> = files
        .iter()
        .filter(|f| names_timberfs_source(&f.display().to_string()))
        .collect();
    if !stores.is_empty() && stores.len() != files.len() {
        bail!(
            "mix of timberfs stores and raw files; search them in two \
             invocations (selection applies only to stores)"
        );
    }
    let windowed = cli.from.is_some() || cli.to.is_some();
    if windowed && stores.is_empty() {
        bail!(
            "--from/--to select on a timberfs store; this input is already \
             a stream (pipe from timberfs query to select first)"
        );
    }

    // The exact pushdown — uniform by SHAPE, not by rule: exact-case
    // positives ride the index (--has whole, every --substring on its
    // interior words, a single --any whole); caseless can't (the index
    // is exact-case) and excludes can't (a Bloom cannot prove absence).
    let mut pushdown: Vec<String> = Vec::new();
    if !stores.is_empty() {
        for t in &cli.substring {
            let toks = interior_tokens(t);
            if !toks.is_empty() {
                note!(
                    "timber-filter: --substring {t:?} rides the token index on its \
                     interior words ({})",
                    toks.join(", ")
                );
                pushdown.extend(toks);
            }
        }
    }
    // --any alternatives push down as query --any (the union of exact
    // branches is exact) — but only when ALL alternatives are exact:
    // one caseless branch could live in a chunk the index would skip.
    let mut any_pushdown: Vec<String> = Vec::new();
    if !stores.is_empty() && !cli.any.is_empty() && cli.any_caseless.is_empty() {
        note!(
            "timber-filter: --any alternative{} ride{} the token index (union of exact branches)",
            if cli.any.len() == 1 { "" } else { "s" },
            if cli.any.len() == 1 { "s" } else { "" }
        );
        any_pushdown = cli.any.clone();
    }

    let mut sink = Sink {
        count: cli.count,
        null_sep: cli.null_sep,
        no_filename: cli.no_filename,
        records: cli.records,
        started: false,
        sources: files.len().max(1),
        matched: 0,
        filtered: 0,
        max: cli.max,
        end_extra: String::new(),
        out: io::BufWriter::new(io::stdout().lock()),
    };

    if !stores.is_empty() {
        // Spawn the sibling selection layer and read its record stream.
        let mut cmd = Command::new(sibling_timberfs());
        cmd.args(["query", "--records"]);
        if cli.quiet {
            cmd.arg("--quiet");
        }
        if let Some(f) = &cli.from {
            cmd.args(["--from", f]);
        }
        if let Some(t) = &cli.to {
            cmd.args(["--to", t]);
        }
        for h in has.iter().chain(&pushdown) {
            cmd.args(["--has", h]);
        }
        for a in &any_pushdown {
            cmd.args(["--any", a]);
        }
        cmd.args(&files).stdout(Stdio::piped());
        let mut child = cmd
            .spawn()
            .context("spawning timberfs (is it installed next to timber-filter?)")?;
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let note_unselected =
            has.is_empty() && pushdown.is_empty() && any_pushdown.is_empty() && !windowed;
        grep_records(stdout, &preds, note_unselected, &mut sink)?;
        if sink.at_cap() {
            // --max satisfied: we stopped reading early, so the child sees a
            // broken pipe and exits non-zero. That's expected, not a failure.
            let _ = child.kill();
            let _ = child.wait();
        } else {
            let status = child.wait()?;
            if !status.success() {
                bail!("timberfs query failed");
            }
        }
    } else if files.is_empty() {
        // stdin: a record stream or raw text — sniffed by the only
        // bytes that can start a v1 record stream.
        let stdin = io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let head = reader.fill_buf()?;
        if head.starts_with(b"\x1estream-start") {
            grep_records(reader, &preds, false, &mut sink)?;
        } else {
            let extractor = extractor(&cli)?;
            grep_raw(reader, extractor, None, &preds, &mut sink)?;
        }
    } else {
        let multi = files.len() > 1;
        for f in &files {
            let file =
                std::fs::File::open(f).with_context(|| format!("opening {}", f.display()))?;
            let label = if multi {
                Some(f.display().to_string().into_bytes())
            } else {
                None
            };
            let extractor = extractor(&cli)?;
            grep_raw(BufReader::new(file), extractor, label, &preds, &mut sink)?;
            if sink.at_cap() {
                break;
            }
        }
    }

    sink.finish()
}

fn extractor(cli: &Cli) -> anyhow::Result<Extractor> {
    Extractor::new(
        cli.timestamp_regex.as_deref(),
        cli.timestamp_format.as_deref(),
        false,
    )
}

/// The sibling binary: next to our own executable first (same package,
/// same version), PATH as the fallback.
fn sibling_timberfs() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("timberfs");
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from("timberfs")
}

struct Sink<W: Write> {
    count: bool,
    null_sep: bool,
    no_filename: bool,
    records: bool,
    started: bool,
    sources: usize,
    matched: u64,
    filtered: u64,
    /// --max: stop after this many matching entries (a hard cap).
    max: Option<u64>,
    /// chunks_read/chunks_total passthrough from an upstream
    /// stream-end, preformatted (leading US), when known.
    end_extra: String,
    out: W,
}

/// This stage's own command line, for the stream-start lineage.
fn stage_echo() -> String {
    let mut args = std::env::args();
    let bin = args
        .next()
        .map(|a| {
            std::path::Path::new(&a)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or(a.clone())
        })
        .unwrap_or_else(|| "timber-filter".into());
    let mut echo = bin;
    for a in args {
        echo.push(' ');
        echo.push_str(&a);
    }
    // Values must not contain US or NUL (timberfs-records(5)).
    echo.replace(['\x1f', '\0'], "?")
}

impl<W: Write> Sink<W> {
    /// --max reached: the caller should stop feeding entries.
    fn at_cap(&self) -> bool {
        self.max.is_some_and(|m| self.matched >= m)
    }

    /// Open the outgoing record stream: pass an upstream stream-start's
    /// fields through verbatim (or synthesize v=1 for raw input), then
    /// append this stage's own command line to the lineage.
    fn stream_start(&mut self, upstream_fields: Option<&[u8]>) -> io::Result<()> {
        if !self.records || self.started {
            return Ok(());
        }
        self.started = true;
        self.out.write_all(b"\x1estream-start")?;
        match upstream_fields {
            Some(fields) => self.out.write_all(fields)?,
            None => write!(self.out, "\x1fv=1\x1fsources={}", self.sources)?,
        }
        write!(self.out, "\x1fstage={}", stage_echo())?;
        self.out.write_all(b"\0")?;
        Ok(())
    }

    /// Pass a metadata record (e.g. source) through verbatim.
    fn passthrough(&mut self, body: &[u8]) -> io::Result<()> {
        if !self.records {
            return Ok(());
        }
        self.out.write_all(b"\x1e")?;
        self.out.write_all(body)?;
        self.out.write_all(b"\0")
    }

    /// meta: an upstream entry-record body to pass through verbatim
    /// (payload unmodified, so len stays true); ts: the entry's own
    /// stamp for synthesized headers (raw input).
    fn emit_meta(
        &mut self,
        entry: &[u8],
        label: Option<&[u8]>,
        meta: Option<&[u8]>,
        ts: Option<u64>,
    ) -> io::Result<()> {
        self.matched += 1;
        if self.count {
            return Ok(());
        }
        if self.records {
            self.stream_start(None)?;
            match meta {
                Some(body) => {
                    self.out.write_all(b"\x1e")?;
                    self.out.write_all(body)?;
                }
                None => {
                    write!(self.out, "\x1eentry\x1flen={}", entry.len())?;
                    if let Some(t) = ts {
                        write!(self.out, "\x1fts={t}")?;
                    }
                    if let Some(l) = label {
                        self.out.write_all(b"\x1fsrc=")?;
                        self.out.write_all(l)?;
                    }
                }
            }
            self.out.write_all(b"\0")?;
            self.out.write_all(entry)?;
            self.out.write_all(b"\0")?;
            return Ok(());
        }
        let label = if self.no_filename { None } else { label };
        if self.null_sep {
            if let Some(l) = label {
                self.out.write_all(l)?;
                self.out.write_all(b":")?;
            }
            let body = entry.strip_suffix(b"\n").unwrap_or(entry);
            self.out.write_all(body)?;
            self.out.write_all(b"\0")?;
        } else {
            for line in entry.split_inclusive(|&b| b == b'\n') {
                if let Some(l) = label {
                    self.out.write_all(l)?;
                    self.out.write_all(b":")?;
                }
                self.out.write_all(line)?;
            }
            if !entry.ends_with(b"\n") {
                self.out.write_all(b"\n")?;
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> anyhow::Result<()> {
        if self.count {
            writeln!(self.out, "{}", self.matched)?;
        }
        if self.records {
            self.stream_start(None)?;
            write!(
                self.out,
                "\x1estream-end\x1fentries={}\x1fdropped={}{}",
                self.matched, self.filtered, self.end_extra
            )?;
            self.out.write_all(b"\0")?;
        }
        self.out.flush()?;
        Ok(())
    }
}

/// The predicate sets an entry is judged against. All Selects must
/// hold, at least one alternative (when any were given), no Exclude.
struct Preds {
    select: Vec<Regex>,
    any: Option<Regex>,
    exclude: Vec<Regex>,
}

fn keep(preds: &Preds, entry: &[u8]) -> bool {
    preds.select.iter().all(|r| r.is_match(entry))
        && preds.any.as_ref().is_none_or(|r| r.is_match(entry))
        && !preds.exclude.iter().any(|r| r.is_match(entry))
}

/// Word-anchored literal (the token index's own semantics).
fn word_rx(t: &str, caseless: bool) -> anyhow::Result<Regex> {
    RegexBuilder::new(&word_pattern(t))
        .case_insensitive(caseless)
        .multi_line(true)
        .build()
        .with_context(|| format!("bad phrase {t:?}"))
}

/// Literal anywhere, even inside longer words.
fn sub_rx(t: &str, caseless: bool) -> anyhow::Result<Regex> {
    RegexBuilder::new(&regex::escape(t))
        .case_insensitive(caseless)
        .multi_line(true)
        .build()
        .with_context(|| format!("bad literal {t:?}"))
}

/// A user regex, as given.
fn user_rx(p: &str, flag: &str) -> anyhow::Result<Regex> {
    RegexBuilder::new(p)
        .multi_line(true)
        .build()
        .with_context(|| format!("bad {flag} pattern {p:?}"))
}

fn grep_raw<R: BufRead, W: Write>(
    reader: R,
    extractor: Extractor,
    label: Option<Vec<u8>>,
    preds: &Preds,
    sink: &mut Sink<W>,
) -> anyhow::Result<()> {
    let mut entries = Entries {
        reader,
        extractor,
        pending: None,
        warned_cap: false,
    };
    while let Some(entry) = entries.next_entry()? {
        if keep(preds, &entry) {
            let ts = if sink.records {
                let head = String::from_utf8_lossy(&entry[..entry.len().min(256)]);
                entries.extractor.extract(&head)
            } else {
                None
            };
            sink.emit_meta(&entry, label.as_deref(), None, ts)?;
            if sink.at_cap() {
                break;
            }
        } else {
            sink.filtered += 1;
        }
    }
    Ok(())
}

/// Read a timberfs-records(5) stream: metadata records are RS-marked;
/// entry payloads are read by their authoritative len. Unknown kinds
/// and keys are ignored (the format grows additively). EOF without
/// stream-end is truncation — an error, never a short result.
fn grep_records<R: BufRead, W: Write>(
    mut reader: R,
    preds: &Preds,
    note_unselected: bool,
    sink: &mut Sink<W>,
) -> anyhow::Result<()> {
    let mut complete = false;
    let mut noted = false;
    let (mut kept_sum, mut total_sum) = (0u64, 0u64);
    let mut hdr: Vec<u8> = Vec::new();
    loop {
        hdr.clear();
        if reader.read_until(0, &mut hdr)? == 0 {
            break;
        }
        if hdr.pop() != Some(0) {
            bail!("record stream truncated mid-record");
        }
        let Some(body) = hdr.strip_prefix(b"\x1e") else {
            bail!("malformed record stream: unmarked record (raw text? omit --records upstream)");
        };
        let kind = body
            .split(|&b| b == 0x1f)
            .next()
            .unwrap_or_default()
            .to_vec();
        let kv = |key: &[u8]| -> Option<String> {
            body.split(|&b| b == 0x1f).skip(1).find_map(|p| {
                p.strip_prefix(key)
                    .and_then(|r| r.strip_prefix(b"="))
                    .map(|v| String::from_utf8_lossy(v).into_owned())
            })
        };
        match kind.as_slice() {
            b"stream-start" => {
                let v = kv(b"v").unwrap_or_default();
                if v != "1" {
                    bail!(
                        "record stream version {v:?} is newer than this timber-filter — upgrade it"
                    );
                }
                sink.stream_start(Some(&body["stream-start".len()..]))?;
            }
            b"source" => {
                kept_sum += kv(b"kept").and_then(|v| v.parse().ok()).unwrap_or(0);
                total_sum += kv(b"total").and_then(|v| v.parse().ok()).unwrap_or(0);
                sink.passthrough(body)?;
            }
            b"entry" => {
                if note_unselected && !noted && total_sum > 64 && kept_sum == total_sum {
                    note!(
                        "timber-filter: nothing narrows this search — matching all \
                         {total_sum} chunks (-w patterns, --has and --from/--to \
                         ride the indexes)"
                    );
                }
                noted = true;
                let len: usize = kv(b"len")
                    .and_then(|v| v.parse().ok())
                    .context("entry record without len")?;
                let mut payload = vec![0u8; len];
                reader
                    .read_exact(&mut payload)
                    .context("record stream truncated mid-entry (producer died or pipe broke)")?;
                let mut nul = [0u8; 1];
                reader.read_exact(&mut nul)?;
                if nul[0] != 0 {
                    bail!("record stream framing error: payload not NUL-terminated");
                }
                if keep(preds, &payload) {
                    let src = kv(b"src");
                    sink.emit_meta(
                        &payload,
                        src.as_deref().map(str::as_bytes),
                        Some(body),
                        None,
                    )?;
                    if sink.at_cap() {
                        break;
                    }
                } else {
                    sink.filtered += 1;
                }
            }
            b"stream-end" => {
                complete = true;
                if let (Some(r), Some(t)) = (kv(b"chunks_read"), kv(b"chunks_total")) {
                    sink.end_extra = format!("\x1fchunks_read={r}\x1fchunks_total={t}");
                }
            }
            _ => {} // forward compatibility: unknown kinds are ignored
        }
    }
    // A missing stream-end normally means truncation — but not when WE
    // stopped early to satisfy --max; that's a deliberate, complete result.
    if !complete && !sink.at_cap() {
        bail!(
            "record stream truncated — no stream-end (producer died or pipe \
             broke); the result above is incomplete"
        );
    }
    Ok(())
}
