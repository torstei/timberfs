//! The consumer protocol: what a consumer says back.
//!
//! A follower reads its selection, feeds a consumer, and holds the
//! position. The consumer says how far to move it. That is the whole
//! contract, and it travels ONE WAY — nothing goes to the consumer but
//! the records stream itself.
//!
//! Framed as `timberfs-records(5)` is — RS-marked kind, US-separated
//! `k=v`, NUL-terminated — so there is one framing discipline and one
//! parser shape at both ends of the pipe.
//!
//! **A watermark means "do not send me these again", not "these are
//! safe."** A receiver refusing an entry for being outside its ingestion
//! window refuses it permanently, so advancing only on confirmed
//! delivery would wedge a follower on one bad batch forever. So: the
//! receiver is down, report nothing and the same entries come again; an
//! entry is too old or malformed, report PAST it and let the consumer's
//! own log say why. What it did with the data is its business, which is
//! why no error taxonomy crosses this boundary.
//!
//! **One unit: the absolute offset on the store's tape.** An entry
//! record states `offset` and `len` and the runs chain, so a watermark
//! is the last accepted entry's `offset + len` — a number the consumer
//! was handed rather than one it computes. A chunk boundary is the same
//! kind of number, one that happens to land on a boundary.
//!
//! ⚠ **Any field whose value this protocol does not constrain is a JSON
//! string** (`holds`, a note's `text`). A JSON string cannot contain a
//! raw control character, so content cannot break the framing — by
//! construction rather than by hoping nobody puts a tab in an error
//! message.
//!
//! Deliberately small enough to implement in a shell script, which is
//! the point of it existing at all — three `printf`s produce a
//! conforming consumer, byte for byte what the emitters here write:
//!
//! ```text
//! printf '\036hello\037v=1\037reads=records\000'
//! printf '\036progress\037id=%s\037offset=%s\000' "$id" "$off"
//! printf '\036note\037id=%s\037text="%s"\000' "$id" "$why"
//! ```

use std::io::BufRead;

use anyhow::{bail, Context};
use serde_json::Value;

/// The version this build speaks. A consumer declaring another is
/// refused, rather than read under rules it may not share.
pub const VERSION: &str = "1";

/// What a consumer reads. Declared by the consumer, not configured by
/// the operator: it is a property of the program, and asking an operator
/// to state it is a way to get it wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Diet {
    Records,
    Chunks,
}

impl Diet {
    pub fn as_str(&self) -> &'static str {
        match self {
            Diet::Records => "records",
            Diet::Chunks => "chunks",
        }
    }

    fn parse(s: &str) -> anyhow::Result<Diet> {
        match s {
            "records" => Ok(Diet::Records),
            "chunks" => Ok(Diet::Chunks),
            // Not "unknown": a consumer asking for something this build
            // does not serve has to be told which it was, or the operator
            // is left comparing versions by hand.
            other => bail!(
                "a consumer asked to read {other:?}, which is not a diet this timberfs knows \
                 (records, chunks)"
            ),
        }
    }
}

/// One message from a consumer.
#[derive(Debug)]
pub enum Report {
    /// Once, before anything else. `holds` is what the consumer's
    /// destination ALREADY has — a hint, honoured only where the
    /// follower has no position of its own for that store.
    Hello {
        reads: Diet,
        holds: Vec<(String, u64)>,
    },
    /// Move this store's position here.
    Progress { id: String, offset: u64 },
    /// Why nothing is moving, for an operator. Opaque to timberfs and
    /// rendered verbatim. `id` absent means "about me, not a store".
    Note {
        id: Option<String>,
        offset: Option<u64>,
        text: String,
    },
}

pub struct Reader<R: BufRead> {
    r: R,
    buf: Vec<u8>,
    said_hello: bool,
}

impl<R: BufRead> Reader<R> {
    pub fn new(r: R) -> Reader<R> {
        Reader {
            r,
            buf: Vec::new(),
            said_hello: false,
        }
    }

    /// The next message, or None once the consumer's end is closed.
    ///
    /// An unknown KIND is skipped, so the protocol can grow additively;
    /// an unknown FIELD likewise. A malformed record is an error and not
    /// a skip: a consumer that speaks garbage cannot be trusted with a
    /// position, and the loud failure is the safe one.
    pub fn next_report(&mut self) -> anyhow::Result<Option<Report>> {
        loop {
            self.buf.clear();
            if self.r.read_until(0, &mut self.buf)? == 0 {
                return Ok(None);
            }
            if self.buf.pop() != Some(0) {
                bail!("consumer report truncated mid-record");
            }
            let Some(body) = self.buf.strip_prefix(b"\x1e") else {
                bail!(
                    "malformed consumer report: unmarked record. Every message begins with \
                     RS (0x1e), as timberfs-records(5) does"
                );
            };
            let mut parts = body.split(|&b| b == 0x1f);
            let kind = parts.next().unwrap_or_default().to_vec();
            let fields: Vec<(String, String)> = parts
                .filter_map(|p| {
                    let s = String::from_utf8_lossy(p);
                    s.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect();
            let get = |k: &str| fields.iter().find(|(f, _)| f == k).map(|(_, v)| v.as_str());
            match kind.as_slice() {
                b"hello" => {
                    let v = get("v").unwrap_or_default();
                    if v != VERSION {
                        bail!(
                            "a consumer speaks protocol version {v:?}; this timberfs speaks \
                             {VERSION:?}"
                        );
                    }
                    let reads = Diet::parse(get("reads").unwrap_or("records"))?;
                    let holds = match get("holds") {
                        None => Vec::new(),
                        Some(json) => parse_holds(json)?,
                    };
                    self.said_hello = true;
                    return Ok(Some(Report::Hello { reads, holds }));
                }
                b"progress" => {
                    self.require_hello("progress")?;
                    let id = get("id")
                        .context("a progress report names no store (id=)")?
                        .to_string();
                    let offset: u64 = get("offset")
                        .context("a progress report carries no offset")?
                        .parse()
                        .context("a progress report's offset is not a number")?;
                    return Ok(Some(Report::Progress { id, offset }));
                }
                b"note" => {
                    self.require_hello("note")?;
                    let text = match get("text") {
                        Some(json) => parse_text(json)?,
                        None => bail!("a note carries no text"),
                    };
                    return Ok(Some(Report::Note {
                        id: get("id").map(str::to_string),
                        offset: get("offset").and_then(|v| v.parse().ok()),
                        text,
                    }));
                }
                _ => {} // additive growth: an unknown kind is not an error
            }
        }
    }

    /// A message before `hello` is refused rather than acted on: the
    /// hello is what proves the far end implements this protocol at all,
    /// and a position must not move on the word of something that never
    /// said so.
    fn require_hello(&self, kind: &str) -> anyhow::Result<()> {
        if self.said_hello {
            return Ok(());
        }
        bail!("a consumer sent {kind} before saying hello")
    }
}

/// `holds={"<store-id>": <offset>, …}` — one field, because the pairs
/// are a map and positional `k=v` repetition is a shape that reads
/// correctly right up until one member is missing.
fn parse_holds(json: &str) -> anyhow::Result<Vec<(String, u64)>> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(json) else {
        bail!("a hello's `holds` is not a JSON object");
    };
    map.into_iter()
        .map(|(id, v)| {
            v.as_u64()
                .map(|off| (id.clone(), off))
                .with_context(|| format!("`holds` gives store {id} an offset that is not a number"))
        })
        .collect()
}

fn parse_text(json: &str) -> anyhow::Result<String> {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::String(s)) => Ok(s),
        _ => bail!("a note's `text` is not a JSON string"),
    }
}

// ---------------------------------------------------------------------------
// the consumer's side: emitting
// ---------------------------------------------------------------------------

/// The hello a consumer opens with. `holds` may be empty — most
/// consumers know nothing about what their destination already has.
pub fn hello(reads: Diet, holds: &[(String, u64)]) -> Vec<u8> {
    let mut out = format!("\x1ehello\x1fv={VERSION}\x1freads={}", reads.as_str()).into_bytes();
    if !holds.is_empty() {
        let map: serde_json::Map<String, Value> = holds
            .iter()
            .map(|(id, off)| (id.clone(), Value::from(*off)))
            .collect();
        out.extend_from_slice(b"\x1fholds=");
        out.extend_from_slice(Value::Object(map).to_string().as_bytes());
    }
    out.push(0);
    out
}

pub fn progress(id: &str, offset: u64) -> Vec<u8> {
    let mut out = format!("\x1eprogress\x1fid={id}\x1foffset={offset}").into_bytes();
    out.push(0);
    out
}

pub fn note(id: Option<&str>, offset: Option<u64>, text: &str) -> Vec<u8> {
    let mut out = b"\x1enote".to_vec();
    if let Some(id) = id {
        out.extend_from_slice(format!("\x1fid={id}").as_bytes());
    }
    if let Some(offset) = offset {
        out.extend_from_slice(format!("\x1foffset={offset}").as_bytes());
    }
    out.extend_from_slice(b"\x1ftext=");
    out.extend_from_slice(Value::String(text.to_string()).to_string().as_bytes());
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(stream: &[u8]) -> anyhow::Result<Vec<Report>> {
        let mut r = Reader::new(stream);
        let mut out = Vec::new();
        while let Some(rep) = r.next_report()? {
            out.push(rep);
        }
        Ok(out)
    }

    /// The round trip both ends depend on: what a Rust consumer emits is
    /// what the follower reads, field for field.
    #[test]
    fn what_a_consumer_emits_is_what_the_follower_reads() {
        let mut stream = hello(Diet::Records, &[("aaa".into(), 424242)]);
        stream.extend(progress("aaa", 33724753900));
        stream.extend(note(Some("aaa"), Some(33724753900), "400: entry refused"));
        stream.extend(note(None, None, "collector.internal unreachable"));

        let reports = read(&stream).unwrap();
        assert_eq!(reports.len(), 4);
        match &reports[0] {
            Report::Hello { reads, holds } => {
                assert_eq!(*reads, Diet::Records);
                assert_eq!(holds, &[("aaa".to_string(), 424242)]);
            }
            _ => panic!("expected a hello"),
        }
        match &reports[1] {
            Report::Progress { id, offset } => {
                assert_eq!((id.as_str(), *offset), ("aaa", 33724753900));
            }
            _ => panic!("expected progress"),
        }
        match (&reports[2], &reports[3]) {
            (
                Report::Note {
                    id: Some(id),
                    offset: Some(off),
                    text,
                },
                Report::Note {
                    id: None,
                    offset: None,
                    text: about_me,
                },
            ) => {
                assert_eq!((id.as_str(), *off), ("aaa", 33724753900));
                assert_eq!(text, "400: entry refused");
                assert_eq!(about_me, "collector.internal unreachable");
            }
            _ => panic!("expected two notes, one of them about the consumer itself"),
        }
    }

    /// A hello is what proves the far end implements this protocol, so a
    /// position must not move on the word of something that never said
    /// so — and "no hello, no run" is what makes silence unambiguous.
    #[test]
    fn nothing_is_accepted_before_a_hello() {
        let err = read(&progress("aaa", 5)).unwrap_err().to_string();
        assert!(err.contains("before saying hello"), "{err}");
        let err = read(&note(Some("aaa"), None, "hi"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("before saying hello"), "{err}");
    }

    /// A free-form value is a JSON string, so content cannot break the
    /// framing. The characters that would are exactly the delimiters.
    #[test]
    fn a_note_carrying_delimiters_survives_them() {
        let nasty = "US \u{1f} NUL \u{0} RS \u{1e} newline \n tab \t quote \" done";
        let mut stream = hello(Diet::Records, &[]);
        stream.extend(note(None, None, nasty));
        // The wire itself must contain no bare delimiter inside the text.
        let text_field = stream.split(|&b| b == 0x1f).next_back().unwrap();
        assert!(text_field.starts_with(b"text="));
        assert!(
            !text_field[5..].contains(&0x1e),
            "a raw RS reached the wire"
        );

        let reports = read(&stream).unwrap();
        match &reports[1] {
            Report::Note { text, .. } => assert_eq!(text, nasty),
            _ => panic!("expected a note"),
        }
    }

    /// Additive growth: a kind or a field this build does not know is
    /// skipped, so a newer consumer talking to an older timberfs loses
    /// the message it did not understand rather than the whole stream.
    #[test]
    fn an_unknown_kind_or_field_is_skipped_not_fatal() {
        let mut stream = hello(Diet::Records, &[]);
        stream.extend_from_slice(b"\x1ewhatever\x1fx=1\0");
        stream.extend_from_slice(b"\x1eprogress\x1fid=aaa\x1foffset=7\x1fsomething=new\0");
        let reports = read(&stream).unwrap();
        assert_eq!(
            reports.len(),
            2,
            "the unknown kind is gone, the rest is not"
        );
        match &reports[1] {
            Report::Progress { id, offset } => assert_eq!((id.as_str(), *offset), ("aaa", 7)),
            _ => panic!("expected progress"),
        }
    }

    /// A version this build does not speak, and a diet it does not know,
    /// are each refused by name — an operator comparing versions by hand
    /// is what a vague error costs.
    #[test]
    fn a_version_or_diet_this_build_does_not_know_is_refused_by_name() {
        let err = read(b"\x1ehello\x1fv=2\x1freads=records\0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("version \"2\""), "{err}");
        let err = read(b"\x1ehello\x1fv=1\x1freads=interpretive-dance\0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("interpretive-dance"), "{err}");
    }

    /// Garbage is an error and not a skip: a consumer that cannot frame
    /// a message cannot be trusted with a position either.
    #[test]
    fn garbage_is_refused_rather_than_skipped() {
        assert!(read(b"not a record at all\0").is_err());
        assert!(
            read(b"\x1ehello\x1fv=1\0\x1eprogress\x1fid=aaa\0").is_err(),
            "no offset"
        );
        assert!(
            read(b"\x1ehello\x1fv=1\x1fholds=[1,2]\0").is_err(),
            "holds is not an object"
        );
        assert!(
            read(b"\x1ehello\x1fv=1\0\x1enote\x1ftext=bare\0").is_err(),
            "text is not a JSON string"
        );
    }
}
