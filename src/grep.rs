//! Shared read-side helpers, once the heart of `timberfs grep` (now
//! retired — matching lives in timber-filter(1), selection in
//! `timberfs query`, artifacts in `timber-filter --records | timberfs
//! import --records`): entry grouping over any BufRead (Entries),
//! the word-anchored literal pattern that mirrors the token index's
//! semantics (word_pattern), the interior-token theorem for substring
//! acceleration (interior_tokens), store-name detection for CLI
//! disambiguation (names_timberfs_source), and the command-line echo
//! written into artifact manifests (command_line).

use std::io::{self, BufRead};
use std::path::Path;

use crate::import::Extractor;
use crate::query::{is_bundle, resolve_backing};

/// A timestamp-less flood can't balloon memory: entries are split here.
const ENTRY_CAP: usize = 16 << 20;

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

/// The invocation as the user typed it (argv, shell-quoted, argv[0]
/// normalized to "timberfs") — the most informative operation fact an
/// investigation artifact can carry: what question produced it.
pub fn command_line() -> String {
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

/// One predicate: what to look for, how to compare it, and (by which list
/// it sits in) whether it must, may, or must not match.
///
/// The CONTRACT is what a caller wants to ask. Whether the token index can
/// help answer it is an execution detail — see `Preds::pushdown`, which
/// finds the part of a predicate set the index can prove and leaves the
/// rest to a full read. A predicate the index cannot ride is not a
/// second-class predicate; it is one that costs more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pred {
    pub kind: PredKind,
    pub text: String,
    /// Compared caselessly. The index is exact-case, so a caseless
    /// predicate can never ride it.
    pub caseless: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredKind {
    /// A word-anchored phrase — the form the token index holds, and the
    /// only one that can skip a chunk whole.
    Has,
    /// A literal anywhere, even inside a longer word. Rides the index on
    /// its INTERIOR whole words when it has any.
    Substring,
    /// A regular expression, as given. Never rides the index; write
    /// `(?i)` inside the pattern for caselessness rather than asking for
    /// `caseless`, which is refused on a regex.
    Regex,
}

impl Pred {
    pub fn has(t: &str) -> Pred {
        Pred {
            kind: PredKind::Has,
            text: t.to_string(),
            caseless: false,
        }
    }
    fn regex(&self) -> anyhow::Result<regex::bytes::Regex> {
        let pattern = match self.kind {
            PredKind::Has => word_pattern(&self.text),
            PredKind::Substring => regex::escape(&self.text),
            PredKind::Regex => self.text.clone(),
        };
        regex::bytes::RegexBuilder::new(&pattern)
            .case_insensitive(self.caseless)
            .multi_line(true)
            .build()
            .map_err(|e| anyhow::anyhow!("bad {:?} predicate {:?}: {e}", self.kind, self.text))
    }
    /// The exact whole words this predicate proves must be present, which
    /// is what the index can skip chunks on. Empty when it can prove
    /// none.
    fn index_terms(&self) -> Vec<String> {
        if self.caseless {
            return Vec::new(); // the index is exact-case
        }
        match self.kind {
            PredKind::Has => vec![self.text.clone()],
            // A literal's INTERIOR words are whole words in the text even
            // though its edges may not be.
            PredKind::Substring => interior_tokens(&self.text),
            PredKind::Regex => Vec::new(),
        }
    }
}

/// The predicate set an ENTRY is judged against: every `all` must match,
/// at least one `any` must (when any are given), and no `none` may.
///
/// This is the exact half of a two-stage search. The index skips chunks it
/// can PROVE cannot match (see `pushdown`), and then every surviving entry
/// is judged here. Skipping is an optimisation; THIS is the meaning — so a
/// predicate the index cannot ride still works, it just reads more.
///
/// One implementation, used by `timber-filter` and by the query document,
/// because two would answer differently the first time one was changed.
#[derive(Debug, Clone, Default)]
pub struct Preds {
    all: Vec<regex::bytes::Regex>,
    any: Option<regex::bytes::Regex>,
    none: Vec<regex::bytes::Regex>,
    /// Kept so `pushdown` can be asked after compilation.
    spec: PredSpec,
}

/// The predicates as declared, before compilation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PredSpec {
    pub all: Vec<Pred>,
    pub any: Vec<Pred>,
    pub none: Vec<Pred>,
}

impl PredSpec {
    pub fn is_empty(&self) -> bool {
        self.all.is_empty() && self.any.is_empty() && self.none.is_empty()
    }

    /// Every predicate here, whatever list it is in.
    pub fn iter(&self) -> impl Iterator<Item = &Pred> {
        self.all.iter().chain(&self.any).chain(&self.none)
    }

    /// What the token index can prove, as (`--has` terms, `--any` terms).
    ///
    /// Uniform by SHAPE rather than by rule:
    ///
    /// - every exact `all` predicate contributes its index terms, because
    ///   narrowing a CONJUNCTION by any subset of it is safe
    /// - `any` contributes only when EVERY alternative is indexable: one
    ///   caseless or regex branch could live in a chunk the index would
    ///   skip, and skipping it would lose a real match
    /// - `none` contributes nothing, ever — a Bloom filter can say a term
    ///   may be present, never that it is absent
    pub fn pushdown(&self) -> (Vec<String>, Vec<String>) {
        let has: Vec<String> = self.all.iter().flat_map(|p| p.index_terms()).collect();
        let any = if !self.any.is_empty() && self.any.iter().all(|p| !p.index_terms().is_empty()) {
            // Each alternative must reduce to exactly one term: a
            // substring with two interior words would need an AND inside
            // an OR, which `--any` cannot express.
            let terms: Vec<Vec<String>> = self.any.iter().map(|p| p.index_terms()).collect();
            if terms.iter().all(|t| t.len() == 1) {
                terms.into_iter().flatten().collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        (has, any)
    }
}

impl Preds {
    pub fn compile(spec: PredSpec) -> anyhow::Result<Preds> {
        let all = spec
            .all
            .iter()
            .map(|p| p.regex())
            .collect::<anyhow::Result<Vec<_>>>()?;
        let none = spec
            .none
            .iter()
            .map(|p| p.regex())
            .collect::<anyhow::Result<Vec<_>>>()?;
        // One alternation rather than N regexes: `any` is a disjunction,
        // and the engine is better at that than a loop is. Caseless
        // branches carry their own inline flag so the group can mix.
        let any = if spec.any.is_empty() {
            None
        } else {
            let branches: Vec<String> = spec
                .any
                .iter()
                .map(|p| {
                    let body = match p.kind {
                        PredKind::Has => word_pattern(&p.text),
                        PredKind::Substring => regex::escape(&p.text),
                        PredKind::Regex => p.text.clone(),
                    };
                    if p.caseless {
                        format!("(?i:{body})")
                    } else {
                        body
                    }
                })
                .collect();
            Some(
                regex::bytes::RegexBuilder::new(&branches.join("|"))
                    .multi_line(true)
                    .build()
                    .map_err(|e| anyhow::anyhow!("bad alternation: {e}"))?,
            )
        };
        Ok(Preds {
            all,
            any,
            none,
            spec,
        })
    }

    pub fn keep(&self, entry: &[u8]) -> bool {
        self.all.iter().all(|r| r.is_match(entry))
            && self.any.as_ref().is_none_or(|r| r.is_match(entry))
            && !self.none.iter().any(|r| r.is_match(entry))
    }

    pub fn spec(&self) -> &PredSpec {
        &self.spec
    }

    pub fn is_empty(&self) -> bool {
        self.spec.is_empty()
    }
}
