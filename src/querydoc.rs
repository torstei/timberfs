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
    /// Where a previous answer stopped, per store. One entry for every
    /// store that answer EXAMINED, including the ones that matched
    /// nothing — leave those out and the next page re-scans them from the
    /// start of the window, which on a fleet is most of the cost.
    ///
    /// The response's `position` records are exactly this shape, so a
    /// client hands back what it was given.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cursor: Vec<Position>,
    /// A cap counting forward from the start.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max: Option<Bound>,
    /// The last N. A different operation from `max`, not the same one with
    /// a sign, which is why they conflict rather than compose.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tail: Option<Bound>,
    /// How long the search may take. Composes with `max` rather than
    /// conflicting: they bound different things, and a fleet read is slow
    /// because it READS a lot, not because it matches a lot.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub deadline: Option<Deadline>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response_format: Option<ResponseFormat>,
}

/// Which stores to read: a predicate over what they declare.
///
/// There is deliberately no way to name a store by PATH. A path is
/// neither unique-and-stable (its identity is) nor a way to find things
/// (its labels are) — it is where the store happens to sit, and it is
/// heading towards being opaque. A generator that keys on it is coupled
/// to a layout that is meant to stop meaning anything.
///
/// Enumerating is not a separate idea: it is this predicate with nothing
/// in it. An absent or empty `select` is every store.
///
/// The predicate is a CONJUNCTION, so naming several specific stores is
/// an alternation over their ids — `id=~(A|B)`, which is what
/// `--dump-json` writes. Ugly to read, exact, and it round-trips; if
/// that becomes a real irritation the fix is OR in the selector, not a
/// second way to name a store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Stores {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub select: Vec<Term>,
}

/// Where one store was left off. The offset is absolute on that store's
/// TAPE — what has ever left it, plus where the next undelivered entry
/// sits in what remains — so retention cannot move it, and it cannot
/// collide the way a timestamp does: on a fleet whose entries share a
/// second, paging by clock loses everything that shared the last one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Position {
    /// The store, by identity. Never by path.
    pub id: String,
    /// Resume just here. Absent means that store delivered nothing, so
    /// the next read starts where the window does — not at zero.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub offset: Option<u64>,
}

/// One term of the store predicate, matched against the whole manifest —
/// labels, `name`, `id` and settings alike.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Term {
    pub key: String,
    /// `=`, `!=`, `=~`, `!~`, `=*` or `!*`.
    ///
    /// Regexes are anchored at both ends, so `=~` is a whole-value match.
    /// `=*` is a LITERAL substring — what a person means by "the store
    /// with apache in its name", and not the same as `=~.*apache.*`,
    /// which would read a `.` in the text as a pattern.
    ///
    /// Enumerated in the schema rather than described as a string, so a
    /// generator can be told which operators exist by the contract instead
    /// of discovering it from an answer that came back wrong.
    #[cfg_attr(test, schemars(extend("enum" = crate::select::OPS)))]
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
    /// Where to START, by chunk NUMBER: a place on the tape rather than a
    /// time, and exact where a timestamp can match two chunks that share a
    /// boundary millisecond. Unaffected by `axis`, which measures the
    /// window's `from`/`to` — a chunk number is not a clock. Refused
    /// beside anything else that names a start (`from`, `tail`, `cursor`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from_chunk: Option<u64>,
}

/// The predicates, flat: every `all` must match, at least one `any` must,
/// and no `none` may.
///
/// Flat rather than a boolean tree. An arbitrary tree is expressible in
/// JSON but not in flags, and then `--dump-json` could not reproduce a
/// server's query — that waits for a version 2. Three lists cover what a
/// log search actually asks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DocMatch {
    /// What these predicates SELECT, and it is REQUIRED for the same
    /// reason `window.axis` is: the two answers differ by orders of
    /// magnitude, and a default is an assumption the next reader makes
    /// wrongly. This member exists because the format shipped without it
    /// and called itself an entry predicate while selecting chunks.
    pub granularity: DocGranularity,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub all: Vec<Predicate>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub any: Vec<Predicate>,
    /// None of these may match. Only meaningful at ENTRY granularity: a
    /// Bloom filter can say a term may be present, never that it is
    /// absent, so a chunk sweep cannot narrow on one and asking for it
    /// there is refused rather than silently ignored.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub none: Vec<Predicate>,
}

/// Whether a predicate names entries or chunks.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum DocGranularity {
    /// The entries that actually contain the terms. What a reader almost
    /// always means. The token index still skips chunks it can prove are
    /// irrelevant, so this is not the slow option — it is the same read
    /// with the survivors judged one entry at a time.
    #[default]
    Entries,
    /// The chunks that MAY contain them, emitted whole — a superset, and
    /// the cheapest thing the store can answer, because the index alone
    /// decides it and nothing is decompressed to find out. Ask for this
    /// when the next stage does its own matching.
    Chunks,
}

/// One predicate. Exactly one matcher is set.
///
/// The contract says what the caller wants to ASK, not what timberfs can
/// answer cheaply. `has` rides the token index; `substring` rides it on
/// its interior whole words; `regex` cannot ride it at all — and none of
/// that changes what any of them MEAN. The index decides how much gets
/// read, never what matches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Predicate {
    /// A word-anchored phrase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has: Option<String>,
    /// A literal anywhere, even inside a longer word.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub substring: Option<String>,
    /// A regular expression, as given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    /// Compare caselessly. Refused on `regex`, which says `(?i)` itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub caseless: bool,
}

impl Predicate {
    /// Exactly one matcher, or a refusal that says so. A predicate with
    /// none matches everything and one with two is two questions.
    fn compile(&self) -> anyhow::Result<crate::grep::Pred> {
        use crate::grep::PredKind;
        let set: Vec<(PredKind, &String)> = [
            self.has.as_ref().map(|t| (PredKind::Has, t)),
            self.substring.as_ref().map(|t| (PredKind::Substring, t)),
            self.regex.as_ref().map(|t| (PredKind::Regex, t)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let (kind, text) = match set.len() {
            1 => set[0],
            0 => bail!(
                "a predicate needs a matcher: `has` (a word-anchored phrase), \
                 `substring` (a literal anywhere) or `regex`"
            ),
            _ => bail!(
                "a predicate sets more than one matcher, which is more than one \
                 question — give each its own entry in the list"
            ),
        };
        if self.caseless && kind == PredKind::Regex {
            bail!(
                "`caseless` is refused on `regex` — a pattern says `(?i)` itself, and two \
                 ways to say it could disagree"
            );
        }
        Ok(crate::grep::Pred {
            kind,
            text: text.clone(),
            caseless: self.caseless,
        })
    }

    fn of(p: &crate::grep::Pred) -> Predicate {
        use crate::grep::PredKind;
        Predicate {
            has: (p.kind == PredKind::Has).then(|| p.text.clone()),
            substring: (p.kind == PredKind::Substring).then(|| p.text.clone()),
            regex: (p.kind == PredKind::Regex).then(|| p.text.clone()),
            caseless: p.caseless,
        }
    }
}

/// A bound, which must say WHAT it counts, because "stop after 10" of
/// what decides both the answer and its cost. Exactly one of `entries`
/// and `chunks`; neither, or both, is refused rather than guessed at.
///
/// ⚠ That is checked at conversion, not by the shape: both members must
/// be optional for either to be given, so a JSON Schema validator cannot
/// catch those two on its own.
///
/// A `scope` (total versus per-store) belongs here and is absent until
/// there is more than one to choose between — the difference matters, but
/// a field with a single legal value is noise, and adding it later is
/// additive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Bound {
    /// Entries. Needs a full read: chunks are decompressed and framed to
    /// know where one entry ends.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub entries: Option<u64>,
    /// Chunks. Nearly free — the index alone answers it, and nothing is
    /// decompressed to count. The unit to bound a chunk-granular search
    /// in, since an entry count there caps entries nobody asked about.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub chunks: Option<u64>,
}

/// A bound on TIME rather than on volume.
///
/// Why the service and not the client: a client-side timeout drops the
/// connection and everything that had already arrived with it. A deadline
/// is answered — the stores read completely are complete, the one it
/// stopped in has a position to resume from, and the ones never reached
/// say so.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Deadline {
    /// Milliseconds, as every other time in this document is.
    pub ms: u64,
}

impl Bound {
    /// Exactly one unit, named. "Stop after 10" of WHAT decides both the
    /// answer and its cost, so neither a missing unit nor two of them is
    /// something to pick between on the caller's behalf.
    fn one(&self, member: &str) -> anyhow::Result<()> {
        match (self.entries, self.chunks) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (None, None) => bail!(
                "`{member}` needs a unit: `entries` (a full read) or `chunks` (free from \
                 the index) — \"stop after 10\" of what?"
            ),
            (Some(_), Some(_)) => bail!(
                "`{member}` names both `entries` and `chunks`; they are different \
                 questions with different costs, so give one"
            ),
        }
    }
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
    /// Which stores matched and what they are — no entries at all.
    /// Listing is not a different question from searching: it is the
    /// same document asking for a different answer.
    Stores,
    /// Compressed chunks, verbatim — nothing decompressed at either end.
    ///
    /// Framed as `timberfs-records(5)`: one `chunk` record per chunk,
    /// carrying its RING — number, offsets, lengths, write window — and
    /// the zstd frame as its payload. The ring is what makes the answer
    /// usable: without it a consumer has bytes it cannot bound, number,
    /// place in time, or resume from.
    ///
    /// ⚠ Chunks do not align to a window, so the answer is a SUPERSET of
    /// what was asked for. Each record says which window its chunk
    /// actually covers, so the widening is visible per chunk rather than
    /// asserted once.
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

/// The sources of a query, as a predicate naming exactly those stores.
///
/// One store is an equality on its id; several are an anchored
/// alternation, because the predicate is a conjunction and `id=A AND
/// id=B` matches nothing. Generated rather than typed, so its ugliness
/// costs a reader little — and if it comes to matter, OR in the selector
/// is the fix, not a second way to name a store.
fn sources_as_identity(sources: &[std::path::PathBuf]) -> anyhow::Result<Vec<Term>> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for p in sources {
        let (dir, name) = crate::query::resolve_backing(p)?;
        let id = crate::bark::load(&dir, &name)
            .and_then(|b| b.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} has no identity, so a document cannot name it — \
                     `timberfs identity {} --mint` gives it one",
                    p.display(),
                    p.display()
                )
            })?;
        ids.push(id);
    }
    let value = if ids.len() == 1 {
        return Ok(vec![Term {
            key: "id".into(),
            op: "=".into(),
            value: ids.pop().unwrap(),
        }]);
    } else {
        format!("({})", ids.join("|"))
    };
    Ok(vec![Term {
        key: "id".into(),
        op: "=~".into(),
        value,
    }])
}

/// The envelope a JSON answer carries.
///
/// `server_version` belongs to the ANSWER, not to any one kind of it. The
/// records stream carries the same field in `stream-start`; this is where
/// it goes when the answer is JSON. `loglines` and `chunks` have nowhere
/// to put it, which is what makes them the raw kinds.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Answer {
    /// What produced this: `"timberfs, 0.24.0"`.
    pub server_version: String,
    /// What was asked for — `response_format.kind: "stores"` today.
    ///
    /// Typed as the store objects themselves rather than as opaque JSON,
    /// because a contract a client cannot validate against is prose with
    /// extra punctuation.
    #[cfg_attr(test, schemars(with = "Vec<crate::store_json::Store>"))]
    pub stores: serde_json::Value,
    /// What this machine will let one request ask for. Absent where it
    /// declares nothing, which is the default install.
    ///
    /// Here because a store listing is the FIRST thing a client asks —
    /// it has to know which stores exist before it reads any — so the
    /// ceilings arrive in time to size a page, rather than as the reason
    /// an answer came back short.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::limits::Limits>,
}

impl Answer {
    pub fn with_stores(stores: serde_json::Value) -> anyhow::Result<Self> {
        let limits = crate::limits::configured()?;
        Ok(Answer {
            server_version: server_version(),
            stores,
            limits: (!limits.is_empty()).then_some(limits),
        })
    }
}

/// What is answering: `"timberfs, <version>"`.
///
/// Product AND version, because the thing answering need not be a
/// timberfs — a relay, a wrapper or another implementation says what it
/// is in the same field, the way an HTTP `Server:` header does.
pub fn server_version() -> String {
    format!("timberfs, {}", env!("CARGO_PKG_VERSION"))
}

/// The stores a predicate names. An empty predicate is every store —
/// enumerating is not a different question from searching, it is this one
/// with nothing in it.
fn selector_expr(stores: &Stores) -> String {
    if stores.select.is_empty() {
        "*".to_string()
    } else {
        stores
            .select
            .iter()
            .map(|t| {
                let v = &t.value;
                if v.contains(',') || v.contains('"') {
                    format!("{}{}\"{}\"", t.key, t.op, v.replace('"', ""))
                } else {
                    format!("{}{}{}", t.key, t.op, v)
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn resolve_stores(stores: &Stores) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let expr = selector_expr(stores);
    let sel = crate::select::Selector::parse(&expr)
        .with_context(|| format!("`stores.select` produced the selector {expr:?}"))?;
    Ok(crate::select::resolve(&[], &sel)
        .into_iter()
        .map(|m| m.dir.join(&m.name))
        .collect())
}

impl Document {
    /// The document a `Query` describes.
    ///
    /// The sources become a predicate over their IDENTITIES. What the
    /// caller typed — a path, a handle — is how a person names a store,
    /// not how a document does: the document says which store, and the
    /// store says where it is. A store with no identity cannot be named
    /// this way, and that is an error rather than a path smuggled back in.
    pub fn of(q: &Query) -> anyhow::Result<Document> {
        // A following read is a PROCESS holding a stream open, not a
        // search. The document describes a search, and one that never
        // ends is a subscription — which is transport-shaped, so it
        // belongs to whatever protocol serves the document rather than
        // to the document.
        //
        // The document's equivalent is to page: read to
        // `status=exhausted`, keep the cursor, ask again later and get
        // what arrived since. Refused rather than dropped, because
        // `--dump-json` claims to be the search these flags describe and
        // silently losing one is how a caller ends up running a
        // different query than it printed.
        if q.follow.follow {
            bail!(
                "a following read cannot be written as a query document: `--follow` holds a \
                 stream open, where a document describes one search. Ask repeatedly from \
                 where the last answer stopped instead — or keep using `--follow`, which is \
                 what it is for"
            );
        }
        let windowed = q.window.from.is_some() || q.window.to.is_some();
        Ok(Document {
            // A cursor is a POSITION, not part of the search, so nothing
            // the flags describe can produce one — `--dump-json` renders
            // the query, and where you are in it is the caller's.
            cursor: Vec::new(),
            v: VERSION.to_string(),
            stores: Stores {
                select: sources_as_identity(&q.sources)?,
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
                granularity: match q.matching.granularity {
                    crate::query::Granularity::Entries => DocGranularity::Entries,
                    crate::query::Granularity::Chunks => DocGranularity::Chunks,
                },
                all: q.matching.all.iter().map(Predicate::of).collect(),
                any: q.matching.any.iter().map(Predicate::of).collect(),
                none: q.matching.none.iter().map(Predicate::of).collect(),
            }),
            max: bound_of(q.limit.max, q.limit.max_chunks),
            tail: bound_of(q.limit.tail, q.limit.tail_chunks),
            deadline: q.limit.deadline_ms.map(|ms| Deadline { ms }),
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
        })
    }

    /// Does this document ask for stores rather than entries? Listing is
    /// the same question with a different answer, so it is a response
    /// kind rather than a second document type.
    pub fn lists_stores(&self) -> bool {
        self.response_format
            .as_ref()
            .is_some_and(|f| f.kind == Kind::Stores)
    }

    /// The store predicate as a `--select` expression.
    pub fn store_selector(&self) -> String {
        selector_expr(&self.stores)
    }

    /// The `Query` this document describes.
    pub fn to_query(&self) -> anyhow::Result<Query> {
        self.to_query_under(crate::limits::configured()?)
    }

    /// The same, against ceilings handed in rather than read from this
    /// machine — the seam a query server sits on, and what lets the
    /// ceilings be tested without a process-wide environment.
    pub fn to_query_under(&self, ceilings: crate::limits::Limits) -> anyhow::Result<Query> {
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
        // An operator this build does not know must be REFUSED, never
        // formatted into a selector string. The parser tries operators
        // longest-first, so an unknown one is not rejected there — a
        // shorter operator is found inside it and the rest becomes part of
        // the value. `=?` reads as `=` against `?value` and matches
        // nothing; `!=X` reads as `!=` against `Xvalue` and matches nearly
        // everything. Both answer 200 with a confident wrong result.
        //
        // Which is how a newer generator meets an older timberfs: `=*`
        // shipped after v0.23.1, and against a build without it
        // `name=*auth01` silently became `name` equals `*auth01`.
        for t in &self.stores.select {
            if !crate::select::OPS.contains(&t.op.as_str()) {
                bail!(
                    "`stores.select` uses the operator {:?}, which this timberfs does not \
                     know — it has {}. An operator is not guessed at: a near miss would be \
                     read as a shorter one with the rest of it stuck to the value, and \
                     answered as though it had been understood",
                    t.op,
                    crate::select::OPS
                        .iter()
                        .map(|o| format!("`{o}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        if self.max.is_some() && self.tail.is_some() {
            bail!("`max` and `tail` are different operations: give one, not both");
        }
        let fmt = self.response_format.clone().unwrap_or(ResponseFormat {
            kind: Kind::Loglines,
            options: FormatOptions::default(),
        });
        if self.lists_stores() {
            // A store listing has no entries, so members that describe
            // entries have nothing to act on. Ignoring them would be the
            // failure this format refuses everywhere else.
            for (name, present) in [
                ("window", self.window.is_some()),
                ("match", self.matching.is_some()),
                ("max", self.max.is_some()),
                ("tail", self.tail.is_some()),
            ] {
                if present {
                    bail!(
                        "`{name}` describes entries, and `response_format.kind: \"stores\"` \
                         answers with stores — drop one or the other"
                    );
                }
            }
        }
        // The axis and the response kind are not independent yet, and
        // saying so is better than accepting a document whose axis is
        // then ignored — which is the "parses and does nothing" failure
        // the absent members were removed for.
        //
        // Underneath, ONE flag (`--by-write-time`) decides both: raw
        // chunks ARE the write-axis read, and there is no write-axis
        // read that yields entries. The document keeps the two members
        // apart because they are two questions; until the read path can
        // answer them separately, they have to agree.
        let axis = self.window.as_ref().map(|w| w.axis);
        match (axis, fmt.kind) {
            (Some(Axis::Logline) | None, Kind::Chunks) => bail!(
                "`chunks` are selected on the write axis and carry no entry parsing, \
                 so they need `window.axis: \"write\"` — say which axis you meant"
            ),
            (_, Kind::Stores) => {}
            (Some(Axis::Write), Kind::Loglines | Kind::Records) => bail!(
                "the write axis yields raw chunks today, so it needs \
                 `response_format.kind: \"chunks\"`; for entries, ask on the logline axis"
            ),
            _ => {}
        }
        if let Some(b) = &self.max {
            b.one("max")?;
        }
        if let Some(b) = &self.tail {
            b.one("tail")?;
        }
        if let Some(m) = &self.matching {
            // A chunk sweep narrows with the token index and nothing else,
            // so a predicate the index cannot prove would parse and do
            // nothing there — the failure this format refuses everywhere.
            if m.granularity == DocGranularity::Chunks {
                if !m.none.is_empty() {
                    bail!(
                        "`none` cannot narrow a chunk sweep: the token index says a term MAY \
                         be in a chunk, never that it is absent, so an exclusion there would \
                         return the chunks it was meant to remove. Ask for \
                         `granularity: \"entries\"`"
                    );
                }
                for p in m.all.iter().chain(&m.any) {
                    let c = p.compile()?;
                    if c.caseless || c.kind == crate::grep::PredKind::Regex {
                        bail!(
                            "a chunk sweep narrows with the token index, which is exact-case \
                             and holds whole words — {} cannot ride it and would narrow \
                             nothing. Ask for `granularity: \"entries\"`, where it is judged \
                             on the entry itself",
                            if c.caseless {
                                "a caseless predicate"
                            } else {
                                "`regex`"
                            }
                        );
                    }
                }
            }
            if m.granularity == DocGranularity::Entries && fmt.kind == Kind::Chunks {
                bail!(
                    "`response_format.kind: \"chunks\"` moves compressed chunks verbatim, so \
                     nothing decompresses them to judge an entry — ask for \
                     `granularity: \"chunks\"` and accept the superset, or for entries in a \
                     kind that has them"
                );
            }
        }
        let mut q = Query {
            sources: resolve_stores(&self.stores)?,
            cursor: self
                .cursor
                .iter()
                .filter_map(|p| p.offset.map(|o| (p.id.clone(), o)))
                .collect(),
            window: Window {
                from: self.window.as_ref().and_then(|w| w.from),
                to: self.window.as_ref().and_then(|w| w.to),
                from_chunk: self.window.as_ref().and_then(|w| w.from_chunk),
            },
            matching: Match {
                all: compile_all(self.matching.as_ref().map(|m| &m.all[..]))?,
                any: compile_all(self.matching.as_ref().map(|m| &m.any[..]))?,
                none: compile_all(self.matching.as_ref().map(|m| &m.none[..]))?,
                granularity: match self.matching.as_ref().map(|m| m.granularity) {
                    Some(DocGranularity::Chunks) | None => crate::query::Granularity::Chunks,
                    Some(DocGranularity::Entries) => crate::query::Granularity::Entries,
                },
            },
            limit: Limit {
                max: self.max.as_ref().and_then(|b| b.entries),
                max_chunks: self.max.as_ref().and_then(|b| b.chunks),
                tail_chunks: self.tail.as_ref().and_then(|b| b.chunks),
                tail: self.tail.as_ref().and_then(|b| b.entries),
                deadline_ms: self.deadline.as_ref().map(|d| d.ms),
                imposed: Default::default(),
            },
            output: Output {
                no_filename: fmt.options.no_filename,
                show_write_time: fmt.options.show_write_time,
                null_sep: fmt.options.null_separated,
                records: fmt.kind == Kind::Records,
                by_write_time: fmt.kind == Kind::Chunks,
                // The document's `chunks` is for a machine: framed, with
                // the rings, compressed. `--by-write-time` stays the text
                // dump it has always been.
                chunk_records: fmt.kind == Kind::Chunks,
            },
            follow: Follow::default(),
        };
        // A document that parses can still describe an impossible search
        // (a window that ends before it starts, a chunk predicate on a
        // following read). Checking here means `to_query` yields a query
        // that RUNS, rather than one that fails later somewhere else.
        q.validate()?;
        // This machine's ceilings, last: they lower what the request
        // asked for, and lowering a bound the document itself would have
        // been refused for would hide the refusal.
        //
        // The DOCUMENT only, because the document is the TRUST BOUNDARY
        // and the flags are not: the CLI runs on the host, where whoever
        // can type it can already read the files. A document is the one
        // shape a caller who is not here can hand you.
        //
        // A store listing reads no entries — `window`, `match`, `max` and
        // `tail` are refused with it — so there is no bound to put one on.
        if !self.lists_stores() {
            ceilings.impose(fmt.kind == Kind::Chunks, &mut q.limit)?;
        }
        Ok(q)
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

/// The bound a `Query`'s two counters describe. They are exclusive by
/// construction — nothing sets both — so this picks whichever is set.
fn bound_of(entries: Option<u64>, chunks: Option<u64>) -> Option<Bound> {
    match (entries, chunks) {
        (None, None) => None,
        _ => Some(Bound { entries, chunks }),
    }
}

/// Compile a list of document predicates, or nothing.
fn compile_all(ps: Option<&[Predicate]>) -> anyhow::Result<Vec<crate::grep::Pred>> {
    ps.unwrap_or(&[]).iter().map(|p| p.compile()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> Query {
        Query {
            cursor: Default::default(),
            // No sources: "every store". Naming one would need a real
            // store on disk, because a document names stores by their
            // IDENTITY and only a store can supply that — which is the
            // point, and is covered end to end in the VM suite.
            sources: vec![],
            window: Window {
                from: Some(1_000),
                to: Some(2_000),
                from_chunk: None,
            },
            matching: Match {
                all: vec![crate::grep::Pred::has("ERROR")],
                any: vec![
                    crate::grep::Pred::has("timeout"),
                    crate::grep::Pred::has("refused"),
                ],
                none: Vec::new(),
                granularity: crate::query::Granularity::Entries,
            },
            limit: Limit {
                max: Some(100),
                tail: None,
                max_chunks: None,
                tail_chunks: None,
                deadline_ms: Some(5_000),
                imposed: Default::default(),
            },
            output: Output {
                no_filename: true,
                show_write_time: false,
                null_sep: true,
                records: true,
                by_write_time: false,
                chunk_records: false,
            },
            follow: Follow::default(),
        }
    }

    #[test]
    fn everything_but_the_stores_survives_the_round_trip() {
        // What the flags build must come back identical, or the document
        // is a second dialect. The STORES half is deliberately not here:
        // a document names them by identity, only a real store can supply
        // one, and an empty predicate means EVERY store where an empty
        // source list means none — so that half is exercised end to end
        // in the VM suite, against stores that exist.
        let before = q();
        let doc = Document::of(&before).unwrap();
        // Under NO ceilings: a machine's policy is a layer OVER the
        // document, so mixing it in here would test the policy rather
        // than the round trip.
        let after = doc
            .to_query_under(crate::limits::Limits::default())
            .unwrap();
        for (name, a, b) in [
            (
                "window",
                format!("{:?}", before.window),
                format!("{:?}", after.window),
            ),
            (
                "match",
                format!("{:?}", before.matching),
                format!("{:?}", after.matching),
            ),
            (
                "limit",
                format!("{:?}", before.limit),
                format!("{:?}", after.limit),
            ),
            (
                "output",
                format!("{:?}", before.output),
                format!("{:?}", after.output),
            ),
        ] {
            assert_eq!(a, b, "{name} did not survive");
        }
        // ...and the document itself round-trips through JSON.
        let text = serde_json::to_string(&doc).unwrap();
        assert_eq!(serde_json::from_str::<Document>(&text).unwrap(), doc);
    }

    /// The job `--dump-json` is uniquely good for: telling a client what a
    /// typed time MEANS, so it does not write a second date parser. The
    /// answer is a document, so the same call also shows that naming no
    /// store is every store rather than an error.
    #[test]
    fn a_typed_time_becomes_a_number_a_client_can_read_back() {
        let mut q = q();
        q.window.from = Some(crate::query::parse_time("2026-08-28 11:10").unwrap());
        q.window.to = None;
        q.sources = vec![];
        q.matching = Default::default();
        let doc = Document::of(&q).unwrap();
        let from = doc.window.as_ref().unwrap().from.unwrap();
        // Milliseconds since the epoch, and a 2026 date is a 13-digit
        // number: the check is that a wall-clock string came back as one
        // scale, not that a particular zone was applied.
        assert!(
            (1_700_000_000_000..2_000_000_000_000).contains(&from),
            "not epoch milliseconds: {from}"
        );
        let text = serde_json::to_string(&doc).unwrap();
        assert!(text.contains(&from.to_string()), "{text}");
        assert!(
            doc.stores.select.is_empty(),
            "no store named is EVERY store, which is what an empty predicate is"
        );
        // A time that means nothing is refused, and says which one — not
        // reported as a missing argument for one that was given.
        let e = crate::query::parse_time("not a time")
            .unwrap_err()
            .to_string();
        assert!(e.contains("unrecognized time"), "{e}");
    }

    #[test]
    fn a_store_cannot_be_named_by_its_path() {
        // A path is neither a store's identity nor a way to find one. It
        // is where the store happens to sit, and it is heading towards
        // being opaque — a generator keyed on it is coupled to a layout
        // that is meant to stop meaning anything.
        let e = serde_json::from_str::<Document>(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[{"file_path":"/x"}]}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("paths"), "{e}");
    }

    #[test]
    fn enumerating_is_a_search_with_nothing_in_it() {
        // Not a separate idea needing its own member: an absent or empty
        // predicate is every store.
        let doc: Document =
            serde_json::from_str(r#"{"v":"1.0-EXPERIMENTAL","stores":{}}"#).unwrap();
        assert!(doc.stores.select.is_empty());
        let with: Document = serde_json::from_str(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"select":[{"key":"type","op":"=","value":"console"}]}}"#,
        )
        .unwrap();
        assert_eq!(with.stores.select.len(), 1);
        assert_eq!(with.stores.select[0].key, "type");
    }

    #[test]
    fn a_misspelled_member_is_refused_not_ignored() {
        // The failure this exists to stop: `"form"` silently ignored means
        // an unbounded search returning results that look right.
        let bad = r#"{"v":"1.0-EXPERIMENTAL","stores":{},
                      "window":{"axis":"logline","form":1}}"#;
        let e = serde_json::from_str::<Document>(bad)
            .unwrap_err()
            .to_string();
        assert!(e.contains("form"), "{e}");
        // A member at the top level, too.
        let bad = r#"{"v":"1.0-EXPERIMENTAL","stores":{},"limits":{"entries":1}}"#;
        assert!(serde_json::from_str::<Document>(bad).is_err());
    }

    #[test]
    fn an_operator_this_build_does_not_know_is_refused_not_truncated() {
        // The failure this exists to stop, measured against v0.23.1: `=*`
        // shipped after it, and a build without that operator answered
        // `name=*auth01` with an empty list and exit 0 — the parser found
        // `=` inside `=*` and compared against the value `*auth01`.
        //
        // The direction that hurts is the other one. `!=` found inside a
        // mistyped negation matches nearly every store, so a typo WIDENS
        // the answer and nothing in it says so.
        let doc = |op: &str| {
            format!(
                r#"{{"v":"1.0-EXPERIMENTAL","stores":{{"select":[
                     {{"key":"name","op":"{op}","value":"s1"}}]}}}}"#
            )
        };
        for op in ["=?", "=X", "!=Y", "LIKE", "~~", ""] {
            let d: Document = serde_json::from_str(&doc(op)).unwrap();
            let e = d.to_query().unwrap_err().to_string();
            assert!(e.contains(op) || op.is_empty(), "{op:?} was not named: {e}");
            assert!(e.contains("`=*`"), "{op:?} did not list the real ones: {e}");
        }
        // ...and every operator that does exist still parses.
        for op in crate::select::OPS {
            let d: Document = serde_json::from_str(&doc(op)).unwrap();
            assert!(d.to_query().is_ok(), "{op:?} was refused");
        }
    }

    #[test]
    fn every_answer_says_what_produced_it() {
        // The gap this closes: `=*` shipped after v0.23.1, and a build
        // without it does NOT refuse the operator — `name=*apache`
        // silently became `name` equal to `*apache`. A client could only
        // find out by being answered wrongly.
        //
        // It rides on the ANSWER rather than on a request of its own,
        // because a client that has to ask separately is a client that
        // can forget to, and the version is wanted exactly when something
        // looks wrong — which is while reading an answer, not before.
        let v = server_version();
        assert!(v.starts_with("timberfs, "), "{v}");
        assert!(v.ends_with(env!("CARGO_PKG_VERSION")), "{v}");

        let a = Answer::with_stores(serde_json::json!([])).unwrap();
        let rendered = serde_json::to_value(&a).unwrap();
        assert_eq!(rendered["server_version"], v);
        assert!(rendered["stores"].is_array());
    }

    #[test]
    fn the_axis_is_required() {
        // Optional, it would be omitted, and the document would inherit
        // the CLI's defect of an axis that switches with the mode.
        let no_axis = r#"{"v":"1.0-EXPERIMENTAL","stores":{},"window":{"from":1}}"#;
        assert!(serde_json::from_str::<Document>(no_axis).is_err());
        let with_axis =
            r#"{"v":"1.0-EXPERIMENTAL","stores":{},"window":{"axis":"write","from":1}}"#;
        assert!(serde_json::from_str::<Document>(with_axis).is_ok());
    }

    #[test]
    fn an_unknown_version_is_refused_rather_than_guessed_at() {
        let doc: Document = serde_json::from_str(r#"{"v":"0.9-OLD","stores":{}}"#).unwrap();
        let e = doc.to_query().unwrap_err().to_string();
        assert!(e.contains("1.0-EXPERIMENTAL"), "{e}");
    }

    #[test]
    fn an_omitted_member_widens_rather_than_empties() {
        // The semantics the whole format rests on. A document naming only
        // stores is "every entry of those stores".
        let doc: Document =
            serde_json::from_str(r#"{"v":"1.0-EXPERIMENTAL","stores":{}}"#).unwrap();
        // Under NO ceilings: this is about what the DOCUMENT means, and a
        // machine's ceilings are a layer over it that a bare `to_query`
        // would mix in.
        let q = doc
            .to_query_under(crate::limits::Limits::default())
            .unwrap();
        assert!(q.window.from.is_none() && q.window.to.is_none());
        assert!(q.matching.is_empty());
        assert!(q.limit.max.is_none() && q.limit.tail.is_none());
    }

    #[test]
    fn max_and_tail_together_are_refused() {
        let doc: Document = serde_json::from_str(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{},
                "max":{"entries":1},"tail":{"entries":1}}"#,
        )
        .unwrap();
        assert!(doc.to_query().unwrap_err().to_string().contains("not both"));
    }

    #[test]
    fn a_match_says_what_it_selects_and_will_not_assume() {
        // The defect this member exists for: the format shipped calling
        // `match` an entry predicate while SELECTING CHUNKS, so a term in
        // one entry returned every entry of every chunk that might hold
        // it. Nothing caught it, because every test asserted the document
        // round-trips and none asserted that a match matches.
        //
        // Omitting it is refused by the shape — `granularity` has no
        // serde default, so a document that does not say cannot parse.
        assert!(serde_json::from_str::<Document>(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{},"match":{"all":[{"has":"ERROR"}]}}"#
        )
        .is_err());

        for (g, want) in [
            ("entries", crate::query::Granularity::Entries),
            ("chunks", crate::query::Granularity::Chunks),
        ] {
            let d: Document = serde_json::from_str(&format!(
                r#"{{"v":"1.0-EXPERIMENTAL","stores":{{}},
                    "window":{{"axis":"logline"}},
                    "match":{{"granularity":"{g}","all":[{{"has":"ERROR"}}]}}}}"#
            ))
            .unwrap();
            assert_eq!(d.to_query().unwrap().matching.granularity, want);
        }

        // Entries cannot be judged inside chunks that never get
        // decompressed, so the combination is refused rather than
        // silently answered with the superset.
        let d: Document = serde_json::from_str(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{},"window":{"axis":"write"},
                "match":{"granularity":"entries","all":[{"has":"ERROR"}]},
                "response_format":{"kind":"chunks"}}"#,
        )
        .unwrap();
        assert!(d.to_query().unwrap_err().to_string().contains("verbatim"));
    }

    #[test]
    fn an_entry_predicate_composes_with_a_following_read_where_a_chunk_one_cannot() {
        // The index has nothing to skip on a live stream, which is why a
        // CHUNK predicate is refused there. An entry predicate is just a
        // filter, and filtering a tail is what a live search IS.
        let doc = |g: &str| -> anyhow::Result<crate::query::Query> {
            let d: Document = serde_json::from_str(&format!(
                r#"{{"v":"1.0-EXPERIMENTAL","stores":{{}},
                    "window":{{"axis":"logline"}},
                    "match":{{"granularity":"{g}","all":[{{"has":"ERROR"}}]}},
                    "tail":{{"entries":10}}}}"#
            ))
            .unwrap();
            d.to_query()
        };
        assert!(doc("entries").is_ok());
        let e = doc("chunks").unwrap_err().to_string();
        assert!(e.contains("token index"), "{e}");
    }

    #[test]
    fn a_predicate_says_what_a_caller_wants_not_what_the_index_can_do() {
        // The contract carries `timber-filter`'s whole set, because the
        // point of a search input is to express the search. Whether the
        // index can help is an EXECUTION detail, and it decides how much
        // gets read rather than what matches.
        let doc = |m: &str| -> anyhow::Result<crate::query::Query> {
            let d: Document = serde_json::from_str(&format!(
                r#"{{"v":"1.0-EXPERIMENTAL","stores":{{}},
                    "window":{{"axis":"logline"}},"match":{m}}}"#
            ))
            .unwrap();
            d.to_query()
        };
        // Every matcher, in every list, at entry granularity.
        for m in [
            r#"{"granularity":"entries","all":[{"has":"ERROR"}]}"#,
            r#"{"granularity":"entries","all":[{"substring":"req-8f"}]}"#,
            r#"{"granularity":"entries","all":[{"regex":"^\\d+ ERROR"}]}"#,
            r#"{"granularity":"entries","all":[{"has":"error","caseless":true}]}"#,
            r#"{"granularity":"entries","none":[{"has":"healthcheck"}]}"#,
            r#"{"granularity":"entries","any":[{"has":"a"},{"substring":"b"}]}"#,
        ] {
            assert!(doc(m).is_ok(), "{m}");
        }

        // Exactly one matcher: none matches everything, two is two
        // questions.
        assert!(doc(r#"{"granularity":"entries","all":[{}]}"#)
            .unwrap_err()
            .to_string()
            .contains("needs a matcher"));
        assert!(
            doc(r#"{"granularity":"entries","all":[{"has":"a","substring":"b"}]}"#)
                .unwrap_err()
                .to_string()
                .contains("more than one")
        );
        // A regex says `(?i)` itself; two ways to say it could disagree.
        assert!(
            doc(r#"{"granularity":"entries","all":[{"regex":"x","caseless":true}]}"#)
                .unwrap_err()
                .to_string()
                .contains("caseless")
        );
    }

    #[test]
    fn a_chunk_sweep_refuses_what_the_index_cannot_prove() {
        // A chunk sweep narrows with the token index and nothing else, so
        // a predicate the index cannot ride would parse and narrow
        // nothing — accepted, it would read as a search that ran.
        let doc = |m: &str| -> anyhow::Result<crate::query::Query> {
            let d: Document = serde_json::from_str(&format!(
                r#"{{"v":"1.0-EXPERIMENTAL","stores":{{}},
                    "window":{{"axis":"logline"}},"match":{m}}}"#
            ))
            .unwrap();
            d.to_query()
        };
        // An exclusion is the sharpest case: a Bloom filter can say a term
        // MAY be in a chunk, never that it is absent, so honouring `none`
        // there would return exactly the chunks it was meant to remove.
        let e = doc(r#"{"granularity":"chunks","none":[{"has":"x"}]}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("never that it is absent"), "{e}");

        for m in [
            r#"{"granularity":"chunks","all":[{"regex":"x"}]}"#,
            r#"{"granularity":"chunks","all":[{"has":"x","caseless":true}]}"#,
        ] {
            assert!(doc(m).is_err(), "{m}");
        }
        // What the index CAN prove is fine, including a substring, which
        // rides it on its interior whole words.
        assert!(doc(r#"{"granularity":"chunks","all":[{"has":"x"}]}"#).is_ok());
        assert!(doc(r#"{"granularity":"chunks","all":[{"substring":"a req b"}]}"#).is_ok());
    }

    #[test]
    fn the_pushdown_narrows_only_where_it_can_prove_something() {
        use crate::grep::{Pred, PredKind, PredSpec};
        let p = |k, t: &str, c| Pred {
            kind: k,
            text: t.to_string(),
            caseless: c,
        };
        // A conjunction may be narrowed by ANY subset of itself, so each
        // indexable term contributes and the rest is judged on the entry.
        let spec = PredSpec {
            all: vec![
                p(PredKind::Has, "ERROR", false),
                p(PredKind::Regex, "x+", false),
                p(PredKind::Has, "nope", true),
            ],
            ..Default::default()
        };
        assert_eq!(spec.pushdown().0, vec!["ERROR".to_string()]);

        // A DISJUNCTION may not: one branch the index cannot prove could
        // live in a chunk the others would let it skip, and skipping it
        // would lose a real match.
        let mixed = PredSpec {
            any: vec![p(PredKind::Has, "a", false), p(PredKind::Has, "b", true)],
            ..Default::default()
        };
        assert!(
            mixed.pushdown().1.is_empty(),
            "one caseless branch disables it"
        );
        let exact = PredSpec {
            any: vec![p(PredKind::Has, "a", false), p(PredKind::Has, "b", false)],
            ..Default::default()
        };
        assert_eq!(exact.pushdown().1, vec!["a".to_string(), "b".to_string()]);

        // An exclusion never narrows, whatever it is.
        let excl = PredSpec {
            none: vec![p(PredKind::Has, "ERROR", false)],
            ..Default::default()
        };
        assert_eq!(excl.pushdown(), (Vec::new(), Vec::new()));

        // A substring rides on its INTERIOR whole words — its edges may
        // sit inside longer words, but `req` here cannot.
        let sub = PredSpec {
            all: vec![p(PredKind::Substring, "id=req done", false)],
            ..Default::default()
        };
        assert!(
            sub.pushdown().0.contains(&"req".to_string()),
            "{:?}",
            sub.pushdown()
        );
    }

    #[test]
    fn a_bound_needs_exactly_one_unit() {
        // "stop after 10" of WHAT: entries need a full read where chunks
        // are nearly free from the index, so a bare number would hide the
        // cost. Both units at once is two questions, not a refinement.
        //
        // ⚠ Checked at conversion, not by the SHAPE, because both members
        // are optional so that either may be given. A JSON Schema
        // validator therefore cannot catch these two on its own — the
        // schema says so in the description, and timberfs refuses them.
        let bad = [
            (
                r#"{"v":"1.0-EXPERIMENTAL","stores":{},"max":{}}"#,
                "needs a unit",
            ),
            (
                r#"{"v":"1.0-EXPERIMENTAL","stores":{},"max":{"entries":5,"chunks":2}}"#,
                "both",
            ),
            (
                r#"{"v":"1.0-EXPERIMENTAL","stores":{},"tail":{}}"#,
                "needs a unit",
            ),
        ];
        for (doc, want) in bad {
            let d: Document = serde_json::from_str(doc).unwrap();
            let e = d.to_query().unwrap_err().to_string();
            assert!(e.contains(want), "{doc} gave {e}");
        }
        // A bare number is still refused by the shape: a bound is an
        // object, and nothing about that changed.
        assert!(serde_json::from_str::<Document>(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{},"max":10}"#
        )
        .is_err());
    }

    /// The ceilings bound the DOCUMENT, and every case has to be told
    /// apart in the answer: a bound the request asked for, one this
    /// machine lowered onto it, and one it supplied where the request
    /// named none.
    #[test]
    fn this_machines_ceilings_bound_a_request_that_asked_for_more() {
        let ceilings = crate::limits::Limits {
            max_entries: Some(100),
            deadline_ms: Some(5_000),
            ..Default::default()
        };
        let of = |doc: &str| -> Query {
            serde_json::from_str::<Document>(doc)
                .unwrap()
                .to_query_under(ceilings)
                .unwrap()
        };
        let bare = of(r#"{"v":"1.0-EXPERIMENTAL","stores":{}}"#);
        assert_eq!(
            (bare.limit.max, bare.limit.deadline_ms),
            (Some(100), Some(5_000))
        );
        assert!(bare.limit.imposed.max && bare.limit.imposed.deadline);

        let over = of(r#"{"v":"1.0-EXPERIMENTAL","stores":{},"max":{"entries":900}}"#);
        assert_eq!(over.limit.max, Some(100));
        assert!(over.limit.imposed.max);

        let under = of(r#"{"v":"1.0-EXPERIMENTAL","stores":{},"max":{"entries":9}}"#);
        assert_eq!(under.limit.max, Some(9));
        assert!(!under.limit.imposed.max);

        // Declared on the read, so the answer carries them whether or not
        // one of them bit.
        assert_eq!(under.limit.imposed.declared, ceilings);
    }

    /// A store listing reads no entries — `max` and `tail` are refused
    /// with it — so there is no bound for a ceiling to sit on. Putting
    /// one there would contradict the refusal one line above it.
    #[test]
    fn a_store_listing_gets_no_ceiling_because_it_has_no_bound() {
        let q = serde_json::from_str::<Document>(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{},"response_format":{"kind":"stores"}}"#,
        )
        .unwrap()
        .to_query_under(crate::limits::Limits {
            max_entries: Some(100),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(q.limit.max, None);
        assert!(!q.limit.imposed.max);
        // ...including under the built-in ceilings, which is the path a
        // machine nobody configured actually takes.
        let q = serde_json::from_str::<Document>(
            r#"{"v":"1.0-EXPERIMENTAL","stores":{},"response_format":{"kind":"stores"}}"#,
        )
        .unwrap()
        .to_query_under(crate::limits::Limits::builtin())
        .unwrap();
        assert_eq!(q.limit.max, None);
    }

    /// A machine nobody configured still bounds a request, because the
    /// ceilings are ON and the file overrides them rather than switching
    /// them on. A bounded answer is a PAGE — it carries the positions
    /// that resume it — so this costs a caller nothing it cannot take
    /// back, which is what makes a default defensible.
    #[test]
    fn an_unconfigured_machine_still_bounds_a_document() {
        let q = serde_json::from_str::<Document>(r#"{"v":"1.0-EXPERIMENTAL","stores":{}}"#)
            .unwrap()
            .to_query_under(crate::limits::Limits::builtin())
            .unwrap();
        assert_eq!(q.limit.max, crate::limits::Limits::builtin().max_entries);
        assert!(q.limit.imposed.max);
        assert_eq!(
            q.limit.imposed.declared,
            crate::limits::Limits::builtin(),
            "and says so in the answer"
        );
    }

    #[test]
    fn a_member_the_code_would_ignore_is_not_in_the_contract() {
        // A contract that names a member the code ignores is the failure
        // strictness exists to prevent, so a member is absent until it
        // works. `paths` is absent for a different reason: a path is
        // neither a store's identity nor a way to find one, so naming a
        // store by it is refused outright rather than supported.
        for doc in [
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"paths":[{"file_path":"/x"}]}}"#,
            r#"{"v":"1.0-EXPERIMENTAL","stores":{"forests":[{"file_path":"/x"}]}}"#,
            r#"{"v":"1.0-EXPERIMENTAL","stores":{},"match":{"none":[{"has":"x"}]}}"#,
            r#"{"v":"1.0-EXPERIMENTAL","stores":{},"max":{"entries":1,"scope":"store"}}"#,
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
        // One stable path, describing THE VERSION THIS BUILD ACCEPTS
        // rather than "the format": when a build accepts more than one,
        // there is a schema per version and this name follows the current
        // one. Said in the man page too, so nobody bookmarks it expecting
        // a fixed target.
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

    /// The ANSWER is a contract too, and was prose while the request had
    /// a schema. That asymmetry is the one a client feels: timberfs
    /// refuses a request member it does not know, and offered no way to
    /// check what came back.
    ///
    /// The records stream is deliberately NOT here — it is a NUL-framed
    /// byte format, which a JSON Schema cannot describe and
    /// `timberfs-records(5)` can.
    #[test]
    fn the_committed_answer_schema_matches_the_types() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/query-answer.schema.json");
        let generated =
            serde_json::to_string_pretty(&schemars::schema_for!(Answer)).unwrap() + "\n";
        if std::env::var("UPDATE_SCHEMA").is_ok() {
            std::fs::write(path, &generated).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            committed, generated,
            "the answer's contract has changed. Review the diff, then:\n  \
             UPDATE_SCHEMA=1 cargo test --lib the_committed_answer_schema_matches_the_types"
        );
    }

    /// An answer this build actually emits must satisfy the schema it
    /// publishes — a generated contract nothing is checked against is a
    /// contract that drifts on the first field somebody adds by hand.
    #[test]
    fn a_real_answer_satisfies_the_published_answer_schema() {
        let schema = schemars::schema_for!(Answer);
        let compiled = jsonschema::validator_for(&serde_json::to_value(&schema).unwrap())
            .expect("the generated schema must itself be valid");
        let a = Answer::with_stores(serde_json::json!([])).unwrap();
        let v = serde_json::to_value(&a).unwrap();
        assert!(
            compiled.is_valid(&v),
            "{v} does not satisfy the answer schema"
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
        let doc: Document = serde_json::from_str(r#"{"v":"0.9-OLD","stores":{}}"#).unwrap();
        let e = doc.to_query().unwrap_err().to_string();
        assert!(e.contains("1.0-EXPERIMENTAL"), "{e}");
        assert!(e.contains("EXPERIMENTAL"), "{e}");
    }
    #[test]
    fn the_axis_and_the_kind_have_to_agree() {
        // `axis` was REQUIRED and then dropped on the floor: a document
        // asking for the write axis got logline semantics with nothing
        // saying so. Underneath, one flag decides both, so until the read
        // path separates them the document refuses the combinations it
        // cannot honour rather than silently picking one.
        let doc = |w: &str, k: &str| -> anyhow::Result<Query> {
            let text = format!(r#"{{"v":"1.0-EXPERIMENTAL","stores":{{}}{w}{k}}}"#);
            serde_json::from_str::<Document>(&text).unwrap().to_query()
        };
        // The pair that works, and is what --by-write-time is.
        assert!(doc(
            r#","window":{"axis":"write"}"#,
            r#","response_format":{"kind":"chunks"}"#
        )
        .is_ok());
        // Entries on the logline axis: the ordinary read.
        assert!(doc(r#","window":{"axis":"logline"}"#, "").is_ok());
        assert!(doc("", "").is_ok(), "no window at all is the logline read");
        // Chunks without saying which axis: refused, because the answer
        // would be selected on an axis the caller did not name.
        let e = doc("", r#","response_format":{"kind":"chunks"}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("write"), "{e}");
        assert!(doc(
            r#","window":{"axis":"logline"}"#,
            r#","response_format":{"kind":"chunks"}"#
        )
        .is_err());
        // The write axis with entry output: no such read exists yet.
        let e = doc(r#","window":{"axis":"write"}"#, "")
            .unwrap_err()
            .to_string();
        assert!(e.contains("chunks"), "{e}");
        assert!(doc(
            r#","window":{"axis":"write"}"#,
            r#","response_format":{"kind":"records"}"#
        )
        .is_err());
    }
}
