//! Service-imposed limits: what the machine ANSWERING will let one
//! request ask for.
//!
//! THE DOCUMENT IS THE TRUST BOUNDARY, and the flags are not. The CLI
//! runs ON the host: whoever can type it can already read the files, so
//! there is nothing for a ceiling to protect there. A document is the one
//! shape a caller who is NOT on the host can hand you — a relay execs
//! `timberfs query --query -` on their behalf — so that is where the
//! ceilings apply.
//!
//! ⚠ Which is why the same search bounded two ways is not two dialects of
//! one question, and must not be "fixed" into one: the SEARCH is identical
//! either way, and what differs is what this machine will spend on it for
//! someone who is not here.
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

/// The ceilings in force. `None` is no ceiling — which is what
/// `Default` gives, because that is what a read the ceilings do not bound
/// declares. What an unconfigured MACHINE has is `builtin`.
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
    /// What an unconfigured timberfs answers under.
    ///
    /// ON by default, because the file exists to OVERRIDE these rather
    /// than to switch protection on: a machine nobody configured is the
    /// one most likely to be asked for everything, and a ceiling only the
    /// careful operator gets is not a ceiling. It costs an unbounded
    /// caller nothing it cannot take back — a bounded answer is a PAGE,
    /// carrying the positions that resume it.
    ///
    /// The numbers are a judgement, not physics, and each is one line to
    /// change:
    ///
    /// - **100_000 entries** — an answer a client can hold (measured at
    ///   ~3.4x its own size in a reader, so ~13 MB of apache lines here),
    ///   and far above any interactive search. "Give me everything" is
    ///   what it bounds, and that is the request that should page.
    /// - **1_000 chunks** — the same size class in the unit a chunk sweep
    ///   is bounded in: frames move compressed and verbatim, so this is
    ///   what a caller receives rather than what it decompresses to.
    /// - **30_000 ms** — the only bound on the WAIT. A relay's own
    ///   timeout drops the connection and everything already on it, where
    ///   a deadline is ANSWERED: the stores read are complete and the one
    ///   it stopped in carries a position. Under any relay timeout worth
    ///   the name, so the answered partial arrives first.
    pub fn builtin() -> Self {
        Limits {
            max_entries: Some(100_000),
            max_chunks: Some(1_000),
            deadline_ms: Some(30_000),
        }
    }

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
        l.imposed.declared = *self;
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
        if l.tail.is_some() || l.tail_chunks.is_some() {
            // A tail says how much itself and the check above held it to
            // the ceiling. Nothing further is put on one: `max` beside a
            // tail is two starts, and the tail path reads no deadline at
            // all — a ceiling that does nothing is worse than none.
            return Ok(());
        }
        // LOWERED in whichever unit the request named — both, if it
        // somehow named both. The two bound different things (how much is
        // delivered, how much is read) and the engine stops at whichever
        // comes first, so a request bounded in chunks does not escape the
        // entries ceiling by changing units.
        for (asked, ceiling, flag) in [
            (
                &mut l.max_chunks,
                self.max_chunks,
                &mut l.imposed.max_chunks,
            ),
            (&mut l.max, self.max_entries, &mut l.imposed.max),
        ] {
            if let (Some(n), Some(c)) = (*asked, ceiling) {
                if n > c {
                    (*asked, *flag) = (Some(c), true);
                }
            }
        }
        // ...and SUPPLIED where the request named no bound, in the unit
        // this answer is counted in. A `chunks` answer moves frames
        // verbatim, so nothing decompresses to count an entry there; and
        // an entries answer is not given a chunk bound it did not ask for,
        // which would stop a search that reads a lot and matches little —
        // the case the deadline is for, in the unit that fits it.
        let (asked, ceiling, flag) = if chunk_answer {
            (
                &mut l.max_chunks,
                self.max_chunks,
                &mut l.imposed.max_chunks,
            )
        } else {
            (&mut l.max, self.max_entries, &mut l.imposed.max)
        };
        if asked.is_none() {
            if let Some(c) = ceiling {
                (*asked, *flag) = (Some(c), true);
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

/// What this machine declares, warning about any line it could not use.
///
/// ⚠ A line this build cannot apply is SKIPPED, not fatal, and the reason
/// goes to stderr. That is the opposite disposition from the rest of this
/// format, and the reason is that `timberfs query` has no STARTUP: a relay
/// execs it once per request, so a policy file it refused would answer
/// every caller with a config error the caller cannot fix, for a mistake
/// the operator would never see. A server reads its policy once and
/// refuses to start, which is the same strictness landing on the person
/// who can act on it — see `describe` and `timberfs limits`, which is that
/// check for a thing that has no startup.
///
/// Skipping is not silent either way: the ceilings actually in force are
/// declared in every answer, so a ceiling that did not survive the file is
/// visible to the caller as well as to the operator.
pub fn configured() -> anyhow::Result<Limits> {
    let (limits, problems, _) = describe()?;
    for why in &problems {
        eprintln!("timberfs: {why}");
    }
    Ok(limits)
}

/// The ceilings, everything unusable about the file, and where it came
/// from — what `timberfs limits` prints and what `configured` warns with.
pub fn describe() -> anyhow::Result<(Limits, Vec<String>, String)> {
    if let Some(env) = std::env::var_os(ENV) {
        let (l, p) = parse(&env.to_string_lossy(), ENV);
        return Ok((l, p, format!("${ENV}")));
    }
    match std::fs::read_to_string(FILE) {
        Ok(text) => {
            let (l, p) = parse(&text, FILE);
            Ok((l, p, FILE.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((
            Limits::builtin(),
            Vec::new(),
            "the built-in defaults".into(),
        )),
        // Present and unreadable is not "no ceilings": something is there
        // and this build cannot see it, which is worth stopping for.
        Err(e) => Err(e).with_context(|| format!("reading {FILE}")),
    }
}

/// `KEY=VALUE`, one per line or space-separated; `#` comments and blank
/// lines ignored. Returns what it understood and one sentence per line it
/// did not — an unknown key (an older timberfs meeting a newer policy, or
/// a typo) and a value that is not a number are both that.
///
/// It starts from the BUILT-IN ceilings and overrides them, so a file
/// naming one key changes that one and leaves the rest in force. `none`
/// removes a ceiling; there is no other way to say it, and `0` is refused
/// because a ceiling of zero entries would answer nothing while reading
/// as "off".
fn parse(text: &str, from: &str) -> (Limits, Vec<String>) {
    let (mut l, mut bad) = (Limits::builtin(), Vec::new());
    // `#` runs to the end of the LINE, so a comment cannot be mistaken for
    // a field — the fields themselves are whitespace-separated, which is
    // what lets the env var carry the same text as the file.
    let fields = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .flat_map(str::split_whitespace);
    for field in fields {
        let Some((key, value)) = field.split_once('=') else {
            bad.push(format!(
                "{from}: {field:?} is not KEY=VALUE, so it sets no ceiling"
            ));
            continue;
        };
        let n = if value.eq_ignore_ascii_case("none") {
            None
        } else {
            match value.parse::<u64>() {
                Ok(0) => {
                    bad.push(format!(
                        "{from}: `{key}=0` would answer nothing at all — write `none` to \
                         remove the ceiling. Left at the default"
                    ));
                    continue;
                }
                Ok(n) => Some(n),
                Err(_) => {
                    bad.push(format!(
                        "{from}: `{key}` wants a number or `none`, not {value:?}. Left at \
                         the default"
                    ));
                    continue;
                }
            }
        };
        match key {
            "MAX_ENTRIES" => l.max_entries = n,
            "MAX_CHUNKS" => l.max_chunks = n,
            "DEADLINE_MS" => l.deadline_ms = n,
            _ => bad.push(format!(
                "{from}: `{key}` is not a limit this timberfs knows (MAX_ENTRIES, \
                 MAX_CHUNKS, DEADLINE_MS). The ceilings it does know are unaffected"
            )),
        }
    }
    (l, bad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Limit;

    /// A machine nobody configured is the one most likely to be asked for
    /// everything, so the ceilings are ON and the file overrides them.
    /// `Default` stays empty because that is what a read the ceilings do
    /// not bound — one from the flags — declares.
    #[test]
    fn an_empty_file_is_the_built_in_ceilings_not_none() {
        let (l, bad) = parse("", "test");
        assert_eq!(l, Limits::builtin());
        assert!(bad.is_empty());
        assert!(Limits::default().is_empty());
        assert_eq!(Limits::default().record_fields(), "");
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let (l, bad) = parse(
            "# a policy\n\nMAX_ENTRIES=100\n\nDEADLINE_MS=5000\n",
            "test",
        );
        assert_eq!(
            (l.max_entries, l.deadline_ms, l.max_chunks),
            (Some(100), Some(5000), Limits::builtin().max_chunks)
        );
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// The file OVERRIDES the built-in ceilings, so a line naming one key
    /// changes that one and leaves the rest standing.
    #[test]
    fn the_file_overrides_the_built_in_ceilings_key_by_key() {
        let (l, bad) = parse("MAX_ENTRIES=5", "test");
        assert_eq!(l.max_entries, Some(5));
        assert_eq!(l.max_chunks, Limits::builtin().max_chunks);
        assert_eq!(l.deadline_ms, Limits::builtin().deadline_ms);
        assert!(bad.is_empty(), "{bad:?}");

        // `none` is the only way to remove one, because `0` would answer
        // nothing while reading as "off".
        let (l, bad) = parse("DEADLINE_MS=none", "test");
        assert_eq!(l.deadline_ms, None);
        assert!(bad.is_empty(), "{bad:?}");
        let (l, bad) = parse("DEADLINE_MS=0", "test");
        assert_eq!(l.deadline_ms, Limits::builtin().deadline_ms);
        assert!(bad[0].contains("none"), "{bad:?}");
    }

    /// `timberfs query` has no startup: a relay execs it once per request,
    /// so a policy file it REFUSED would answer every caller with a config
    /// error the caller cannot fix, for a mistake the operator would never
    /// see. An override naming something that does not exist is the
    /// operator's mistake and must not make the logs unavailable — the
    /// line is skipped, said out loud, and every ceiling this build DOES
    /// know stays in force. `timberfs limits` is where the strictness
    /// lives, and a server would move it to its own startup.
    #[test]
    fn a_line_it_cannot_use_is_skipped_and_named_not_fatal() {
        // The realistic one: an older timberfs meeting a newer policy.
        let (l, bad) = parse("MAX_ENTRIES=100\nMAX_BYTES=99\n", "test");
        assert_eq!(l.max_entries, Some(100));
        assert_eq!(l.max_chunks, Limits::builtin().max_chunks);
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert!(
            bad[0].contains("MAX_BYTES") && bad[0].contains("MAX_ENTRIES"),
            "{bad:?}"
        );

        // A typo leaves the DEFAULT standing, which is the difference
        // between this and a file that switched protection on.
        for text in ["MAX_ENTRIES=lots", "MAX_ENTRIES"] {
            let (l, bad) = parse(text, "test");
            assert_eq!(l, Limits::builtin(), "{text}");
            assert_eq!(bad.len(), 1, "{text} gave {bad:?}");
        }
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

    /// The hole this closes: `max` names ONE unit, so a request could
    /// bound itself in chunks and meet no entries ceiling at all — 200
    /// entries came back where the ceiling said 5. The two bound
    /// different things and the engine stops at whichever comes first,
    /// so both are put on a request that named either.
    #[test]
    fn a_request_cannot_escape_a_ceiling_by_changing_units() {
        let c = Limits {
            max_entries: Some(5),
            max_chunks: Some(1000),
            ..Default::default()
        };
        let mut l = Limit {
            max_chunks: Some(9),
            ..Default::default()
        };
        c.impose(false, &mut l).unwrap();
        assert_eq!((l.max_chunks, l.max), (Some(9), Some(5)));
        assert!(l.imposed.max && !l.imposed.max_chunks);

        // ...but an entries answer is not GIVEN a chunk bound it did not
        // ask for: that would stop a search which reads a lot and matches
        // little, and the deadline is the bound for that.
        let mut plain = Limit::default();
        c.impose(false, &mut plain).unwrap();
        assert_eq!((plain.max, plain.max_chunks), (Some(5), None));

        // A chunks answer counts no entries, so it gets only its own.
        let mut chunks = Limit::default();
        c.impose(true, &mut chunks).unwrap();
        assert_eq!((chunks.max, chunks.max_chunks), (None, Some(1000)));
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
