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

/// The one version string this build accepts, matched EXACTLY.
///
/// A string rather than a number so the warning travels in the document
/// itself: a generator writing `"1.0-EXPERIMENTAL"` has typed the word,
/// where a bare `1` puts the warning in documentation its author may
/// never open.
///
/// Exact match, deliberately. The structure is meaningful to people and
/// opaque to this code — no parsing, no ordering, no arguing about
/// whether `"1.0"` and `"1"` are the same thing. When the format
/// stabilises the accepted string changes, and every generator using the
/// old one is refused, which is what "experimental" means.
///
/// **While the string carries `EXPERIMENTAL`, the format may be broken in
/// place without changing it.** That costs less than it sounds:
/// `deny_unknown_fields` and strict typing already turn most breaking
/// changes into a specific error at the client — a removed or renamed
/// member is an unknown field, a newly required one is a missing field, a
/// changed type is a type error. Break those freely.
///
/// The one change worth a new string even here is the one nothing
/// catches: **a member that keeps its name and its type but changes
/// MEANING** — `from` in seconds where it was milliseconds, `entries`
/// counting chunks. That parses, and answers a different question than
/// the caller asked, with nothing anywhere saying so.
pub const VERSION: &str = "1.0-EXPERIMENTAL";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// The format version, matched exactly against `VERSION`. Carries
    /// its own stability marker while the format is still being learned.
    ///
    /// Published as a `const` so the schema states the accepted string
    /// rather than merely "a string" — a generator validating against the
    /// contract should learn the version from it, not from prose.
    #[cfg_attr(test, schemars(extend("const" = VERSION)))]
    pub v: String,
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
/// Only what works: a label `select` and a `forests` list are both
/// coming, and both are ABSENT rather than present-and-refused. A member
/// the contract advertises but the code ignores is the failure this
/// format's strictness exists to prevent — and adding an optional member
/// later is allowed within v1, so there is nothing to reserve.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Stores {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub paths: Vec<StorePath>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StorePath {
    pub file_path: String,
}

/// Which axis a window is measured on.
///
/// Required, with no default, on purpose. The CLI's `--from` means
/// logline time in a windowed read and write time under `--follow` — the
/// axis switches with the mode instead of being chosen, which is a known
/// defect. Made optional here it would be omitted, and the document would
/// inherit the same ambiguity.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// The timestamps the lines themselves carry.
    Logline,
    /// When the data arrived.
    Write,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
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

/// Entry predicates. Flat, mirroring the flags: `all` must all match and
/// at least one of `any` must. A `none` list belongs here too and is
/// absent for the same reason as `select`: `query` has no flag that
/// produces one, and a member the contract names but the code drops is
/// worse than one it does not name yet. Arbitrary boolean
/// trees would be expressible in JSON but not in flags, and then the CLI
/// could not reproduce a server's query — that waits for a version 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DocMatch {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub all: Vec<Predicate>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub any: Vec<Predicate>,
}

/// One predicate. Exactly one matcher is set; the shape leaves room for
/// `substring` and `regex` without changing it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Predicate {
    /// A word-anchored phrase, which is the form that can ride the token
    /// index and so skip chunks.
    pub has: String,
}

/// A bound, which must say WHAT it counts: `entries` needs a full read
/// and decompression where chunks and bytes are nearly free from the
/// index, so a bare number would hide the cost. `entries` is the only
/// unit this build counts, and is therefore required rather than
/// optional-and-then-refused; when another arrives it becomes one of
/// several, which relaxes the contract rather than breaking it.
///
/// A `scope` (total versus per-store) belongs here and is absent until
/// there is more than one to choose between — the difference matters, but
/// a field with a single legal value is noise, and adding it later is
/// additive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Bound {
    pub entries: u64,
}

/// What comes back. A kind plus options rather than a bare name, because
/// the flags it stands for are orthogonal to it: `no_filename`,
/// `show_write_time` and NUL separation shape any of the kinds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResponseFormat {
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "FormatOptions::is_default")]
    pub options: FormatOptions,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
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
#[cfg_attr(test, derive(schemars::JsonSchema))]
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
            v: VERSION.to_string(),
            stores: Stores {
                paths: q
                    .sources
                    .iter()
                    .map(|p| StorePath {
                        file_path: p.display().to_string(),
                    })
                    .collect(),
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
            }),
            max: q.limit.max.map(|n| Bound { entries: n }),
            tail: q.limit.tail.map(|n| Bound { entries: n }),
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
            // Whether a compatible build exists at all is the thing a
            // generator's author most needs to know here, and while the
            // accepted string says EXPERIMENTAL the answer is no: the
            // version it knew is gone rather than still served.
            let note = if VERSION.contains("EXPERIMENTAL") {
                " — and that version is EXPERIMENTAL: a superseded one is not kept \
                 working, so a generator and its timberfs are upgraded together"
            } else {
                ""
            };
            bail!(
                "this timberfs speaks query document version {VERSION:?}, not {:?}{note}",
                self.v
            );
        }
        if self.max.is_some() && self.tail.is_some() {
            bail!("`max` and `tail` are different operations: give one, not both");
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
                max: self.max.as_ref().map(|b| b.entries),
                tail: self.tail.as_ref().map(|b| b.entries),
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
        let bad = r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[{"file_path":"/x"}]},
                      "window":{"axis":"logline","form":1}}"#;
        let e = serde_json::from_str::<Document>(bad)
            .unwrap_err()
            .to_string();
        assert!(e.contains("form"), "{e}");
        // A member at the top level, too.
        let bad = r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[]},"limits":{"entries":1}}"#;
        assert!(serde_json::from_str::<Document>(bad).is_err());
    }

    #[test]
    fn the_axis_is_required() {
        // Optional, it would be omitted, and the document would inherit
        // the CLI's defect of an axis that switches with the mode.
        let no_axis = r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[]},"window":{"from":1}}"#;
        assert!(serde_json::from_str::<Document>(no_axis).is_err());
        let with_axis =
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[]},"window":{"axis":"write","from":1}}"#;
        assert!(serde_json::from_str::<Document>(with_axis).is_ok());
    }

    #[test]
    fn an_unknown_version_is_refused_rather_than_guessed_at() {
        let doc: Document =
            serde_json::from_str(r#"{"v":"0.9-OLD","stores":{"paths":[]}}"#).unwrap();
        let e = doc.to_query().unwrap_err().to_string();
        assert!(e.contains("1.0-EXPERIMENTAL"), "{e}");
    }

    #[test]
    fn an_omitted_member_widens_rather_than_empties() {
        // The semantics the whole format rests on. A document naming only
        // stores is "every entry of those stores".
        let doc: Document = serde_json::from_str(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[{"file_path":"/x"}]}}"#,
        )
        .unwrap();
        let q = doc.to_query().unwrap();
        assert!(q.window.from.is_none() && q.window.to.is_none());
        assert!(q.matching.is_empty());
        assert!(q.limit.max.is_none() && q.limit.tail.is_none());
    }

    #[test]
    fn max_and_tail_together_are_refused() {
        let doc: Document = serde_json::from_str(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[]},
                "max":{"entries":1},"tail":{"entries":1}}"#,
        )
        .unwrap();
        assert!(doc.to_query().unwrap_err().to_string().contains("not both"));
    }

    #[test]
    fn a_bound_without_a_unit_is_refused() {
        // "stop after 10" of WHAT: entries need a full read where chunks
        // and bytes are nearly free from the index, so a bare number
        // would hide the cost. Refused by the SHAPE now rather than at
        // conversion, which is what makes it visible in the schema.
        assert!(serde_json::from_str::<Document>(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[]},"max":{}}"#
        )
        .is_err());
        assert!(serde_json::from_str::<Document>(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[]},"max":10}"#
        )
        .is_err());
    }

    #[test]
    fn a_member_the_code_would_ignore_is_not_in_the_contract() {
        // `forests`, `select` and `none` all parsed and were then either
        // refused or — worse — silently dropped. A contract that names a
        // member the code ignores is the failure strictness exists to
        // prevent, so they are absent until they work. Adding an optional
        // member later is allowed within v1; advertising a lie is not.
        for doc in [
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[],"select":[{"key":"a","op":"=","value":"b"}]}}"#,
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[],"forests":[{"file_path":"/x"}]}}"#,
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[]},"match":{"none":[{"has":"x"}]}}"#,
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[]},"max":{"entries":1,"scope":"store"}}"#,
        ] {
            assert!(
                serde_json::from_str::<Document>(doc).is_err(),
                "should be refused, not accepted: {doc}"
            );
        }
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
    /// The contract is a FILE in the repository, not something a build
    /// produces: it has to show up in a diff so a change to it is
    /// reviewed rather than discovered by a client. This test is what
    /// keeps that file honest — change the types without regenerating and
    /// the build fails here, naming the command.
    ///
    /// Generated from `Document` rather than the other way round, which is
    /// safe only because `Document` is a WIRE type with no other job: it
    /// never reaches the read paths, so the internal model stays free to
    /// move. Generating a contract from a type that also serves the
    /// internals is how you end up with neither.
    #[test]
    fn the_committed_schema_matches_the_types() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/query-document.schema.json"
        );
        let generated =
            serde_json::to_string_pretty(&schemars::schema_for!(Document)).unwrap() + "\n";
        if std::env::var("UPDATE_SCHEMA").is_ok() {
            std::fs::write(path, &generated).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            committed, generated,
            "the query document's contract has changed. Review the diff, then:\n  \
             UPDATE_SCHEMA=1 cargo test --lib the_committed_schema_matches_the_types"
        );
    }

    /// Every documented example must satisfy the published contract, not
    /// merely happen to deserialize.
    #[test]
    fn the_examples_satisfy_the_published_schema() {
        let schema = schemars::schema_for!(Document);
        let compiled = jsonschema::validator_for(&serde_json::to_value(&schema).unwrap())
            .expect("the generated schema must itself be valid");
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/examples");
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(
                compiled.is_valid(&v),
                "{} does not satisfy the published schema",
                path.display()
            );
        }
    }
    #[test]
    fn the_experimental_regime_is_stated_in_the_refusal() {
        // A version mismatch is the moment a client author most needs to
        // know whether a compatible build exists — during the
        // experimental phase it does not, and the error has to say so
        // rather than reading like a transient problem to retry.
        let doc: Document =
            serde_json::from_str(r#"{"v":"0.9-OLD","stores":{"paths":[]}}"#).unwrap();
        let e = doc.to_query().unwrap_err().to_string();
        assert!(e.contains("1.0-EXPERIMENTAL"), "{e}");
        assert!(e.contains("EXPERIMENTAL"), "{e}");
    }
}
