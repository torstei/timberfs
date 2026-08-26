//! The query document: a whole search as JSON.
//!
//! This is the WIRE type, kept separate from `query::Query`, which is the
//! internal one. Two types rather than one set of serde attributes on the
//! internal type, because the document has members the internal value does
//! not (a version, an explicit time axis) and shapes some of them
//! differently — and because a wire format should be free to stay still
//! while the internals move. The conversions are explicit and the
//! round trip is tested, which is the guarantee that matters.
//!
//! **`deny_unknown_fields` everywhere is deliberate.** A misspelled
//! `"form"` for `"from"` must be an error, not a silently unbounded
//! search: a query language that ignores a typo returns wrong results
//! that look right, and laxness cannot be tightened once callers rely on
//! it.
//!
//! Deserialization is into statically known types. Nothing here lets the
//! payload choose what to construct, so the polymorphic-deserialization
//! problem that makes untrusted input dangerous elsewhere does not arise.
//! What does arise is denial of service, and the defence is the same one
//! `otlp-intake` already uses: bound the input BEFORE parsing.

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::query::{Follow, Limit, Match, Output, Query, Window};

/// The only version this build speaks. An unknown one is refused rather
/// than interpreted optimistically: a document from a future timberfs may
/// mean something different by the same member, and guessing which would
/// be worse than saying no.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub v: u32,
    pub stores: Stores,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub window: Option<DocWindow>,
    #[serde(rename = "match", skip_serializing_if = "Option::is_none", default)]
    pub matching: Option<DocMatch>,
    /// A cap counting forward from the start.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max: Option<Bound>,
    /// The last N. A different operation from `max`, not the same one with
    /// a sign, which is why they conflict rather than compose.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tail: Option<Bound>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response_format: Option<ResponseFormat>,
}

/// Which stores to read.
///
/// `paths` is what the CLI has today. `select` — a label predicate — is
/// what a server will take, and the member is here now so that adding it
/// is not a shape change. Objects rather than bare strings for the same
/// reason: a forest will want attributes of its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Stores {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub paths: Vec<StorePath>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub select: Vec<Term>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub forests: Vec<Forest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StorePath {
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Forest {
    pub file_path: String,
}

/// One label predicate, as `select` takes them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Term {
    pub key: String,
    pub op: String,
    pub value: String,
}

/// Which axis a window is measured on.
///
/// Required, with no default, on purpose. The CLI's `--from` means
/// logline time in a windowed read and write time under `--follow` — the
/// axis switches with the mode instead of being chosen, which is a known
/// defect. Made optional here it would be omitted, and the document would
/// inherit the same ambiguity.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// The timestamps the lines themselves carry.
    Logline,
    /// When the data arrived.
    Write,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DocWindow {
    pub axis: Axis,
    /// Milliseconds since the epoch. Absent = unbounded on that side; an
    /// omitted member WIDENS the search rather than emptying it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub to: Option<u64>,
    /// A resume position by chunk NUMBER: exact, where a timestamp can
    /// match two chunks that share a boundary millisecond.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from_chunk: Option<u64>,
}

/// Entry predicates. Flat, mirroring the flags: `all` must all match,
/// at least one of `any` must, and none of `none` may. Arbitrary boolean
/// trees would be expressible in JSON but not in flags, and then the CLI
/// could not reproduce a server's query — that waits for a version 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct DocMatch {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub all: Vec<Predicate>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub any: Vec<Predicate>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub none: Vec<Predicate>,
}

/// One predicate. Exactly one matcher is set; the shape leaves room for
/// `substring` and `regex` without changing it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Predicate {
    /// A word-anchored phrase, which is the form that can ride the token
    /// index and so skip chunks.
    pub has: String,
}

/// A bound, which must say what it counts and over what scope: `entries`
/// costs a full read, `chunks` and `bytes` are nearly free from the
/// index, and per-store versus total is the difference between an O(N)
/// query and one that can stop the moment it is satisfied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Bound {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub entries: Option<u64>,
    #[serde(default = "Scope::total")]
    pub scope: Scope,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Across the whole selection: the engine may stop the moment it is
    /// satisfied, anywhere.
    Total,
    /// Per matching store: answers "which stores", at the cost of
    /// touching all of them.
    Store,
}

impl Scope {
    fn total() -> Scope {
        Scope::Total
    }
}

/// What comes back. A kind plus options rather than a bare name, because
/// the flags it stands for are orthogonal to it: `no_filename`,
/// `show_write_time` and NUL separation shape any of the kinds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResponseFormat {
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "FormatOptions::is_default")]
    pub options: FormatOptions,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The log as a person reads it.
    Loglines,
    /// The typed record stream: entries with their timestamps and chunk
    /// windows attached.
    Records,
    /// Compressed chunks, verbatim — nothing decompressed at either end.
    /// ⚠ Chunks do not align to a window, so the answer is a SUPERSET of
    /// what was asked for and the response must say what window it
    /// actually widened to.
    Chunks,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct FormatOptions {
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_filename: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub show_write_time: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub null_separated: bool,
}

impl FormatOptions {
    fn is_default(&self) -> bool {
        *self == FormatOptions::default()
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Document {
    /// The document a `Query` describes.
    pub fn of(q: &Query) -> Document {
        let windowed = q.window.from.is_some() || q.window.to.is_some();
        Document {
            v: VERSION,
            stores: Stores {
                paths: q
                    .sources
                    .iter()
                    .map(|p| StorePath {
                        file_path: p.display().to_string(),
                    })
                    .collect(),
                select: Vec::new(),
                forests: Vec::new(),
            },
            window: if windowed || q.window.from_chunk.is_some() {
                Some(DocWindow {
                    // The axis the flags MEANT, made explicit: a
                    // windowed read verifies each entry's own timestamp,
                    // while follow reads forward on the write axis.
                    // Writing it down is the whole point — the document
                    // does not inherit an axis that switches with the
                    // mode.
                    axis: if q.follow.follow || q.output.by_write_time {
                        Axis::Write
                    } else {
                        Axis::Logline
                    },
                    from: q.window.from,
                    to: q.window.to,
                    from_chunk: q.window.from_chunk,
                })
            } else {
                None
            },
            matching: (!q.matching.is_empty()).then(|| DocMatch {
                all: q
                    .matching
                    .has
                    .iter()
                    .map(|h| Predicate { has: h.clone() })
                    .collect(),
                any: q
                    .matching
                    .any
                    .iter()
                    .map(|h| Predicate { has: h.clone() })
                    .collect(),
                none: Vec::new(),
            }),
            max: q.limit.max.map(|n| Bound {
                entries: Some(n),
                scope: Scope::Total,
            }),
            tail: q.limit.tail.map(|n| Bound {
                entries: Some(n),
                scope: Scope::Total,
            }),
            response_format: Some(ResponseFormat {
                kind: if q.output.by_write_time {
                    Kind::Chunks
                } else if q.output.records {
                    Kind::Records
                } else {
                    Kind::Loglines
                },
                options: FormatOptions {
                    no_filename: q.output.no_filename,
                    show_write_time: q.output.show_write_time,
                    null_separated: q.output.null_sep,
                },
            }),
        }
    }

    /// The `Query` this document describes.
    pub fn to_query(&self) -> anyhow::Result<Query> {
        if self.v != VERSION {
            bail!(
                "this timberfs speaks query document version {VERSION}, not {}",
                self.v
            );
        }
        if !self.stores.select.is_empty() {
            bail!("`stores.select` is not supported yet — name stores by `paths` for now");
        }
        if self.max.is_some() && self.tail.is_some() {
            bail!("`max` and `tail` are different operations: give one, not both");
        }
        for (what, b) in [("max", &self.max), ("tail", &self.tail)] {
            if let Some(b) = b {
                if b.entries.is_none() {
                    bail!("`{what}` needs a unit — `entries` is the one this build counts");
                }
                if b.scope != Scope::Total {
                    bail!("`{what}.scope` other than `total` is not supported yet");
                }
            }
        }
        let fmt = self.response_format.clone().unwrap_or(ResponseFormat {
            kind: Kind::Loglines,
            options: FormatOptions::default(),
        });
        Ok(Query {
            sources: self
                .stores
                .paths
                .iter()
                .map(|p| std::path::PathBuf::from(&p.file_path))
                .collect(),
            window: Window {
                from: self.window.as_ref().and_then(|w| w.from),
                to: self.window.as_ref().and_then(|w| w.to),
                from_chunk: self.window.as_ref().and_then(|w| w.from_chunk),
            },
            matching: Match {
                has: self
                    .matching
                    .as_ref()
                    .map(|m| m.all.iter().map(|p| p.has.clone()).collect())
                    .unwrap_or_default(),
                any: self
                    .matching
                    .as_ref()
                    .map(|m| m.any.iter().map(|p| p.has.clone()).collect())
                    .unwrap_or_default(),
            },
            limit: Limit {
                max: self.max.as_ref().and_then(|b| b.entries),
                tail: self.tail.as_ref().and_then(|b| b.entries),
            },
            output: Output {
                no_filename: fmt.options.no_filename,
                show_write_time: fmt.options.show_write_time,
                null_sep: fmt.options.null_separated,
                records: fmt.kind == Kind::Records,
                by_write_time: fmt.kind == Kind::Chunks,
            },
            follow: Follow::default(),
        })
    }
}

/// Read a document from a path, or from stdin for `-`.
pub fn read(path: &str) -> anyhow::Result<Document> {
    let text = if path == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
    };
    serde_json::from_str(&text).context(
        "the query document is not valid JSON, or names a member this timberfs does not know",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> Query {
        Query {
            sources: vec!["/var/log/timberfs/a/a.log".into()],
            window: Window {
                from: Some(1_000),
                to: Some(2_000),
                from_chunk: None,
            },
            matching: Match {
                has: vec!["ERROR".into()],
                any: vec!["timeout".into(), "refused".into()],
            },
            limit: Limit {
                max: Some(100),
                tail: None,
            },
            output: Output {
                no_filename: true,
                show_write_time: false,
                null_sep: true,
                records: true,
                by_write_time: false,
            },
            follow: Follow::default(),
        }
    }

    #[test]
    fn a_query_survives_the_round_trip() {
        // The guarantee that keeps the two surfaces one question: what the
        // flags build must serialize and come back identical, or the
        // document is a second dialect.
        let before = q();
        let doc = Document::of(&before);
        let after = doc.to_query().unwrap();
        assert_eq!(format!("{before:?}"), format!("{after:?}"));
        // ...and the document itself round-trips through JSON.
        let text = serde_json::to_string(&doc).unwrap();
        assert_eq!(serde_json::from_str::<Document>(&text).unwrap(), doc);
    }

    #[test]
    fn a_misspelled_member_is_refused_not_ignored() {
        // The failure this exists to stop: `"form"` silently ignored means
        // an unbounded search returning results that look right.
        let bad = r#"{"v":1,"stores":{"paths":[{"file_path":"/x"}]},
                      "window":{"axis":"logline","form":1}}"#;
        let e = serde_json::from_str::<Document>(bad)
            .unwrap_err()
            .to_string();
        assert!(e.contains("form"), "{e}");
        // A member at the top level, too.
        let bad = r#"{"v":1,"stores":{"paths":[]},"limits":{"entries":1}}"#;
        assert!(serde_json::from_str::<Document>(bad).is_err());
    }

    #[test]
    fn the_axis_is_required() {
        // Optional, it would be omitted, and the document would inherit
        // the CLI's defect of an axis that switches with the mode.
        let no_axis = r#"{"v":1,"stores":{"paths":[]},"window":{"from":1}}"#;
        assert!(serde_json::from_str::<Document>(no_axis).is_err());
        let with_axis = r#"{"v":1,"stores":{"paths":[]},"window":{"axis":"write","from":1}}"#;
        assert!(serde_json::from_str::<Document>(with_axis).is_ok());
    }

    #[test]
    fn an_unknown_version_is_refused_rather_than_guessed_at() {
        let doc: Document = serde_json::from_str(r#"{"v":99,"stores":{"paths":[]}}"#).unwrap();
        let e = doc.to_query().unwrap_err().to_string();
        assert!(e.contains("version 1"), "{e}");
    }

    #[test]
    fn an_omitted_member_widens_rather_than_empties() {
        // The semantics the whole format rests on. A document naming only
        // stores is "every entry of those stores".
        let doc: Document =
            serde_json::from_str(r#"{"v":1,"stores":{"paths":[{"file_path":"/x"}]}}"#).unwrap();
        let q = doc.to_query().unwrap();
        assert!(q.window.from.is_none() && q.window.to.is_none());
        assert!(q.matching.is_empty());
        assert!(q.limit.max.is_none() && q.limit.tail.is_none());
    }

    #[test]
    fn max_and_tail_together_are_refused() {
        let doc: Document = serde_json::from_str(
            r#"{"v":1,"stores":{"paths":[]},
                "max":{"entries":1},"tail":{"entries":1}}"#,
        )
        .unwrap();
        assert!(doc.to_query().unwrap_err().to_string().contains("not both"));
    }

    #[test]
    fn a_bound_without_a_unit_is_refused() {
        // "stop after 10" of WHAT: entries cost a full read, chunks and
        // bytes are nearly free. A bare number would hide that.
        let doc: Document =
            serde_json::from_str(r#"{"v":1,"stores":{"paths":[]},"max":{"scope":"total"}}"#)
                .unwrap();
        assert!(doc.to_query().unwrap_err().to_string().contains("unit"));
    }
    /// Every example in docs/examples must parse and convert. Prose can
    /// go stale quietly; an example that stops being true fails the
    /// build, which is the only kind of documentation that stays honest.
    #[test]
    fn the_documented_examples_are_real() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/examples");
        let mut seen = 0;
        for e in std::fs::read_dir(dir).expect("docs/examples") {
            let path = e.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let doc: Document =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            doc.to_query()
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            seen += 1;
        }
        assert!(seen >= 3, "expected the documented examples, found {seen}");
    }
}
