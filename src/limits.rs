//! Service-imposed limits: what the machine ANSWERING will let one
//! request ask for.
//!
//! A document is a request from somewhere else — a relay hands one to
//! `timberfs query --query -` for a caller who does not own this machine,
//! where the flags are the operator at a shell. So these bound the
//! document and leave the flags alone.
//!
//! DECLARED, not discovered. Every answer with somewhere to put them
//! carries the ceilings, so a caller sizes its pages before it asks
//! instead of learning them from an answer that came back short.
//!
//! Clamped where the answer can be continued and refused where it cannot.
//! A `max` that the ceiling lowered is the first page of the answer: the
//! `position` records say where it got to and the cursor resumes there. A
//! `tail` carries no position, so a smaller tail is a different answer
//! rather than the start of one — and a different answer nobody asked for
//! is what this format refuses everywhere else.
//!
//! ⚠ It bounds ACCIDENTS, not adversaries: whoever controls the argv or
//! the environment controls these too. What it protects is the machine
//! from one document that asks for everything.

use anyhow::{bail, Context};
use serde::Serialize;

/// Where the ceilings are declared: `KEY=VALUE`, the idiom of
/// `forests.d/*.conf` and the mount configs.
const FILE: &str = "/etc/timberfs/limits.conf";
/// Replaces the file wholesale, space-separated `KEY=VALUE` — the
/// one-off and the test override, which keeps this a pure function with
/// no clap plumbing. The same idiom as `TIMBERFS_FORESTS`.
const ENV: &str = "TIMBERFS_LIMITS";

/// The ceilings, as declared. An absent one is no ceiling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Limits {
    /// Most entries one answer may deliver. Supplied to a request that
    /// named no bound, and lowered onto one that asked for more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u64>,
    /// The same in chunks, which is the unit a `chunks` answer is bounded
    /// in: an entry count there caps entries nobody asked about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chunks: Option<u64>,
    /// Longest a search may run. It bounds what the other two cannot —
    /// a read is slow because it READS a lot, not because it matches a
    /// lot — and it is answered rather than abandoned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        *self == Limits::default()
    }

    /// The ceilings as `timberfs-records(5)` fields, for a `stream-start`.
    /// Empty when nothing is declared, so an unconfigured timberfs writes
    /// exactly the stream it always did.
    pub fn record_fields(&self) -> String {
        let mut s = String::new();
        if let Some(n) = self.max_entries {
            s.push_str(&format!("\x1flimits.max.entries={n}"));
        }
        if let Some(n) = self.max_chunks {
            s.push_str(&format!("\x1flimits.max.chunks={n}"));
        }
        if let Some(n) = self.deadline_ms {
            s.push_str(&format!("\x1flimits.deadline_ms={n}"));
        }
        s
    }

    /// Put the ceilings on one request's bounds.
    ///
    /// `chunk_answer` picks the unit a request that named no bound gets
    /// bounded in: chunks for a chunk sweep, entries for everything that
    /// reads them. Bounding a chunk sweep by entries would cap entries
    /// nobody asked about, which is the trap `max`'s own units exist to
    /// keep a caller out of.
    pub fn impose(&self, chunk_answer: bool, l: &mut crate::query::Limit) -> anyhow::Result<()> {
        // A tail is REFUSED rather than lowered: its answer carries no
        // position, so "the last 10000" in place of "the last 10000000"
        // is a different question answered silently. Named with the
        // ceiling, so the retry is obvious.
        for (asked, ceiling, member) in [
            (l.tail, self.max_entries, "tail.entries"),
            (l.tail_chunks, self.max_chunks, "tail.chunks"),
        ] {
            if let (Some(n), Some(c)) = (asked, ceiling) {
                if n > c {
                    bail!(
                        "`{member}: {n}` is over this timberfs's ceiling of {c}. A tail cannot \
                         be lowered to fit: it reports no position, so a shorter one is a \
                         different answer rather than the first page of yours. Ask for {c} or \
                         fewer — or for `max`, which pages"
                    );
                }
            }
        }
        // `max` is lowered, and supplied where nothing was asked. Either
        // way the answer is a PAGE: `stream-end` says `limited`, names
        // this ceiling, and the `position` records resume it.
        let bounded = l.max.is_some()
            || l.max_chunks.is_some()
            || l.tail.is_some()
            || l.tail_chunks.is_some();
        l.imposed.declared = *self;
        if let Some(c) = self.max_chunks {
            if l.max_chunks.is_some_and(|m| m > c) || (chunk_answer && !bounded) {
                l.max_chunks = Some(c);
                l.imposed.max_chunks = true;
            }
        }
        if let Some(c) = self.max_entries {
            if l.max.is_some_and(|m| m > c) || (!chunk_answer && !bounded) {
                l.max = Some(c);
                l.imposed.max = true;
            }
        }
        if let Some(c) = self.deadline_ms {
            if l.deadline_ms.is_none_or(|d| d > c) {
                l.deadline_ms = Some(c);
                l.imposed.deadline = true;
            }
        }
        Ok(())
    }
}

/// What this machine declares, from `TIMBERFS_LIMITS` if set, else
/// `/etc/timberfs/limits.conf`, else nothing.
pub fn configured() -> anyhow::Result<Limits> {
    if let Some(env) = std::env::var_os(ENV) {
        return parse(&env.to_string_lossy(), ENV);
    }
    match std::fs::read_to_string(FILE) {
        Ok(text) => parse(&text, FILE),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Limits::default()),
        Err(e) => Err(e).with_context(|| format!("reading {FILE}")),
    }
}

/// `KEY=VALUE`, one per line or space-separated; `#` comments and blank
/// lines ignored.
///
/// ⚠ An unknown key is an ERROR, where `forests.d` ignores one for
/// forward compatibility. The directions of failure are opposite: a
/// forest nobody reads is a store not found, and a CEILING nobody reads
/// is the unbounded read this file exists to prevent. So a typo, and an
/// older timberfs meeting a newer policy, both fail closed and say so.
fn parse(text: &str, from: &str) -> anyhow::Result<Limits> {
    let mut l = Limits::default();
    // `#` runs to the end of the LINE, so a comment cannot be mistaken for
    // a field — the fields themselves are whitespace-separated, which is
    // what lets the env var carry the same text as the file.
    let fields = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .flat_map(str::split_whitespace);
    for field in fields {
        let (key, value) = field
            .split_once('=')
            .with_context(|| format!("{from}: {field:?} is not KEY=VALUE"))?;
        let n = || -> anyhow::Result<u64> {
            value
                .parse::<u64>()
                .with_context(|| format!("{from}: `{key}` wants a number, not {value:?}"))
        };
        match key {
            "MAX_ENTRIES" => l.max_entries = Some(n()?),
            "MAX_CHUNKS" => l.max_chunks = Some(n()?),
            "DEADLINE_MS" => l.deadline_ms = Some(n()?),
            _ => bail!(
                "{from}: `{key}` is not a limit this timberfs knows — it has MAX_ENTRIES, \
                 MAX_CHUNKS and DEADLINE_MS. A ceiling that is not understood is not \
                 applied, so it is refused rather than ignored"
            ),
        }
    }
    Ok(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Limit;

    #[test]
    fn nothing_declared_is_no_ceiling() {
        let l = parse("", "test").unwrap();
        assert!(l.is_empty());
        assert_eq!(l.record_fields(), "");
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let l = parse(
            "# a policy\n\nMAX_ENTRIES=100\n\nDEADLINE_MS=5000\n",
            "test",
        )
        .unwrap();
        assert_eq!(l.max_entries, Some(100));
        assert_eq!(l.deadline_ms, Some(5000));
        assert_eq!(l.max_chunks, None);
    }

    /// A ceiling nobody understood is not applied, and an unapplied
    /// ceiling is the unbounded read the file exists to prevent — the
    /// opposite direction from `forests.d`, where an ignored key costs a
    /// lookup rather than the machine.
    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let e = parse("MAX_ENTRY=100", "test").unwrap_err().to_string();
        assert!(e.contains("MAX_ENTRIES"), "{e}");
        assert!(parse("MAX_ENTRIES=lots", "test").is_err());
        assert!(parse("MAX_ENTRIES", "test").is_err());
    }

    #[test]
    fn a_request_with_no_bound_is_given_the_ceiling() {
        let c = Limits {
            max_entries: Some(100),
            ..Default::default()
        };
        let mut l = Limit::default();
        c.impose(false, &mut l).unwrap();
        assert_eq!(l.max, Some(100));
        assert!(l.imposed.max);
    }

    /// A chunk sweep is bounded in CHUNKS. An entry count there caps
    /// entries nobody asked about, so "the first 5 entries" of a sweep
    /// can easily contain none of the matches.
    #[test]
    fn a_chunk_answer_is_bounded_in_chunks() {
        let c = Limits {
            max_entries: Some(100),
            max_chunks: Some(7),
            ..Default::default()
        };
        let mut l = Limit::default();
        c.impose(true, &mut l).unwrap();
        assert_eq!((l.max, l.max_chunks), (None, Some(7)));
        assert!(l.imposed.max_chunks && !l.imposed.max);
    }

    #[test]
    fn a_smaller_request_is_left_alone() {
        let c = Limits {
            max_entries: Some(100),
            deadline_ms: Some(5000),
            ..Default::default()
        };
        let mut l = Limit {
            max: Some(10),
            deadline_ms: Some(1000),
            ..Default::default()
        };
        c.impose(false, &mut l).unwrap();
        assert_eq!((l.max, l.deadline_ms), (Some(10), Some(1000)));
        assert!(!l.imposed.max && !l.imposed.deadline);
    }

    #[test]
    fn a_larger_request_is_lowered_and_says_which_bound_it_was() {
        let c = Limits {
            max_entries: Some(100),
            deadline_ms: Some(5000),
            ..Default::default()
        };
        let mut l = Limit {
            max: Some(1_000_000),
            deadline_ms: Some(600_000),
            ..Default::default()
        };
        c.impose(false, &mut l).unwrap();
        assert_eq!((l.max, l.deadline_ms), (Some(100), Some(5000)));
        assert!(l.imposed.max && l.imposed.deadline);
    }

    /// The asymmetry that matters: a lowered `max` is the first page of
    /// the answer, and a lowered `tail` is a different answer.
    #[test]
    fn a_tail_over_the_ceiling_is_refused_because_it_cannot_be_paged() {
        let c = Limits {
            max_entries: Some(100),
            ..Default::default()
        };
        let mut l = Limit {
            tail: Some(1000),
            ..Default::default()
        };
        let e = c.impose(false, &mut l).unwrap_err().to_string();
        assert!(e.contains("no position"), "{e}");
        assert!(e.contains("100"), "{e}");
        // Within the ceiling it is untouched, and gets no `max` either:
        // a tail already says how much.
        let mut ok = Limit {
            tail: Some(10),
            ..Default::default()
        };
        c.impose(false, &mut ok).unwrap();
        assert_eq!((ok.tail, ok.max), (Some(10), None));
    }

    #[test]
    fn the_ceilings_are_declared_as_stream_start_fields() {
        let c = Limits {
            max_entries: Some(100),
            deadline_ms: Some(5000),
            ..Default::default()
        };
        assert_eq!(
            c.record_fields(),
            "\x1flimits.max.entries=100\x1flimits.deadline_ms=5000"
        );
    }
}
