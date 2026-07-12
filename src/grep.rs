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
