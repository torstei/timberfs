//! Reading a timberfs-records(5) stream: NUL-terminated records,
//! metadata marked by a leading RS byte with US-separated key=value
//! fields, entry payloads read by their authoritative len. Unknown
//! kinds and keys are ignored (the format grows additively); EOF
//! without stream-end is truncation — an error, never a short result.

use std::io::BufRead;

use anyhow::{bail, Context};

/// One entry, with whatever the stream said about it.
pub struct EntryRec {
    /// The entry's own logline timestamp, when it has one.
    pub ts: Option<u64>,
    /// The original write window, when the stream carries one.
    pub wf: Option<u64>,
    pub wl: Option<u64>,
    /// The number of the chunk this entry came from IN THE SOURCE STORE.
    /// `None` means the producer read it from the live edge, where no
    /// chunk exists yet — a fact about the container, not the address:
    /// such an entry still states the `offset` it sits at.
    ///
    /// ⚠ A position, not a fact about the entry: unlike `wf`/`wl` it is
    /// NOT carried into a destination store (see `sink.rs`).
    pub chunk: Option<u64>,
    /// Where this run of bytes BEGINS on the store's tape. With
    /// `payload.len()` it states both ends, and the runs CHAIN — so
    /// `offset + len` is the watermark a consumer reports for this
    /// entry, a number it was handed rather than one it computes.
    ///
    /// `None` only for a stream that states none. Every answer timberfs
    /// gives does.
    pub offset: Option<u64>,
    /// Which store this entry came from, in a MULTI-SOURCE stream: the
    /// path (`src`) and the identity (`id`). Both absent in a
    /// single-source stream, which names its source once in
    /// `stream-start`/`source` instead.
    ///
    /// The id is the durable half — a path names a store only within one
    /// answer — and it is what a per-store position is keyed by.
    pub src: Option<String>,
    pub id: Option<String>,
    pub payload: Vec<u8>,
}

/// Where one store got to, from a `position` record. Handed back as the
/// request's `cursor` to resume exactly here.
pub struct PositionRec {
    pub path: Option<String>,
    /// `None` for a store whose manifest declares no id, which therefore
    /// cannot be resumed by cursor at all.
    pub id: Option<String>,
    /// ⚠ Absent means there is no position — nothing delivered and
    /// nothing resumed from — which is NOT offset zero. Handing back a
    /// cursor entry without one asks for the start of the window.
    pub offset: Option<u64>,
}

pub enum Rec {
    /// stream-start fields (excluding the kind), in stream order.
    Start(Vec<(String, String)>),
    Source(Vec<(String, String)>),
    Entry(EntryRec),
    Position(PositionRec),
    /// stream-end fields; its arrival is the completeness marker.
    End(Vec<(String, String)>),
}

/// Walk a records stream by RECORD, keeping each one's bytes verbatim.
///
/// For a consumer that FORWARDS some records and drops others: what it
/// passes on is the producer's own bytes, so nothing can drift between
/// what timberfs wrote and what a downstream reader sees. A decode into
/// `Rec` and a re-encode would be two spellings of one format.
pub struct Frames<'a> {
    buf: &'a [u8],
    at: usize,
}

/// One record, undecoded: its kind, its whole bytes, and its header
/// fields on demand.
pub struct Frame<'a> {
    pub kind: &'a [u8],
    /// The record exactly as it was written, header and payload and both
    /// NULs — what a forwarder writes out.
    pub bytes: &'a [u8],
    hdr: &'a [u8],
}

impl<'a> Frame<'a> {
    /// A header field, scanned rather than collected: a forwarder wants
    /// three or four of them and allocating a map per record to get them
    /// is the wrong shape at a batch a time.
    pub fn field(&self, key: &str) -> Option<&'a str> {
        self.hdr
            .split(|&b| b == 0x1f)
            .skip(1)
            .filter_map(|p| std::str::from_utf8(p).ok())
            .find_map(|p| p.strip_prefix(key)?.strip_prefix('='))
    }

    pub fn number(&self, key: &str) -> Option<u64> {
        self.field(key)?.parse().ok()
    }
}

impl<'a> Frames<'a> {
    pub fn new(buf: &'a [u8]) -> Frames<'a> {
        Frames { buf, at: 0 }
    }
}

impl<'a> Iterator for Frames<'a> {
    type Item = anyhow::Result<Frame<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.buf.len() {
            return None;
        }
        let start = self.at;
        let rest = &self.buf[start..];
        let Some(nul) = rest.iter().position(|&b| b == 0) else {
            return Some(Err(anyhow::anyhow!("record stream truncated mid-record")));
        };
        let hdr = &rest[..nul];
        let Some(body) = hdr.strip_prefix(b"\x1e") else {
            return Some(Err(anyhow::anyhow!(
                "malformed record stream: unmarked record"
            )));
        };
        let kind_end = body.iter().position(|&b| b == 0x1f).unwrap_or(body.len());
        let kind = &body[..kind_end];
        // An entry's payload follows its header, authoritative by `len`
        // — the only way past it, since a zstd frame or a log line may
        // contain a NUL like any other byte.
        let mut end = nul + 1;
        if kind == b"entry" || kind == b"chunk" {
            let frame = Frame {
                kind,
                bytes: &[],
                hdr,
            };
            let Some(len) = frame.number("len") else {
                return Some(Err(anyhow::anyhow!("a {} record has no len", "payload")));
            };
            end += len as usize + 1;
            if start + end > self.buf.len() {
                return Some(Err(anyhow::anyhow!("record stream truncated mid-payload")));
            }
        }
        self.at = start + end;
        Some(Ok(Frame {
            kind,
            bytes: &self.buf[start..self.at],
            hdr,
        }))
    }
}

pub struct Reader<R: BufRead> {
    r: R,
    hdr: Vec<u8>,
    complete: bool,
}

impl<R: BufRead> Reader<R> {
    pub fn new(r: R) -> Reader<R> {
        Reader {
            r,
            hdr: Vec::new(),
            complete: false,
        }
    }

    /// The next meaningful record, or None at clean end-of-stream.
    /// Unknown metadata kinds are skipped here so every consumer gets
    /// forward compatibility for free.
    pub fn next_rec(&mut self) -> anyhow::Result<Option<Rec>> {
        loop {
            self.hdr.clear();
            if self.r.read_until(0, &mut self.hdr)? == 0 {
                if !self.complete {
                    bail!("record stream truncated — no stream-end (producer died or pipe broke)");
                }
                return Ok(None);
            }
            if self.hdr.pop() != Some(0) {
                bail!("record stream truncated mid-record");
            }
            let Some(body) = self.hdr.strip_prefix(b"\x1e") else {
                bail!(
                    "malformed record stream: unmarked record (raw text? \
                     produce it with --records upstream)"
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
            let get = |key: &str| -> Option<&String> {
                fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            };
            match kind.as_slice() {
                b"stream-start" => {
                    let v = get("v").cloned().unwrap_or_default();
                    if v != "1" {
                        bail!("record stream version {v:?} is newer than this timberfs — upgrade");
                    }
                    return Ok(Some(Rec::Start(fields)));
                }
                b"source" => return Ok(Some(Rec::Source(fields))),
                b"entry" => {
                    let len: usize = get("len")
                        .and_then(|v| v.parse().ok())
                        .context("entry record without len")?;
                    let ts = get("ts").and_then(|v| v.parse().ok());
                    let wf = get("wf").and_then(|v| v.parse().ok());
                    let wl = get("wl").and_then(|v| v.parse().ok());
                    let chunk = get("chunk").and_then(|v| v.parse().ok());
                    let mut payload = vec![0u8; len];
                    self.r.read_exact(&mut payload).context(
                        "record stream truncated mid-entry (producer died or pipe broke)",
                    )?;
                    let mut nul = [0u8; 1];
                    self.r.read_exact(&mut nul)?;
                    if nul[0] != 0 {
                        bail!("record stream framing error: payload not NUL-terminated");
                    }
                    return Ok(Some(Rec::Entry(EntryRec {
                        ts,
                        wf,
                        wl,
                        chunk,
                        offset: get("offset").and_then(|v| v.parse().ok()),
                        src: get("src").cloned(),
                        id: get("id").cloned(),
                        payload,
                    })));
                }
                b"position" => {
                    return Ok(Some(Rec::Position(PositionRec {
                        path: get("path").cloned(),
                        id: get("id").cloned(),
                        offset: get("offset").and_then(|v| v.parse().ok()),
                    })))
                }
                b"stream-end" => {
                    self.complete = true;
                    return Ok(Some(Rec::End(fields)));
                }
                _ => {} // forward compatibility: unknown kinds are ignored
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(stream: &[u8]) -> Vec<Rec> {
        let mut r = Reader::new(stream);
        let mut out = Vec::new();
        while let Some(rec) = r.next_rec().expect("well-formed stream") {
            out.push(rec);
        }
        out
    }

    /// A forwarder passes the producer's own bytes through, so what it
    /// keeps is byte-identical and only what it drops is gone. The
    /// walker must therefore find a payload's end by `len` and not by a
    /// delimiter, an entry being able to contain one like any other
    /// bytes.
    #[test]
    fn records_are_walked_verbatim_and_payloads_bound_by_len() {
        // The middle entry's payload holds every delimiter there is.
        let nasty = b"a\x1eb\x00c\x1fd";
        let mut stream = Vec::new();
        stream.extend_from_slice(b"\x1estream-start\x1fv=1\x1fsources=2\0");
        stream.extend_from_slice(b"\x1esource\x1fpath=/l/a.log\x1fid=aaa\0");
        stream.extend_from_slice(b"\x1eentry\x1flen=3\x1foffset=10\x1fchunk=4\x1fid=aaa\0one\0");
        stream.extend_from_slice(b"\x1eentry\x1flen=7\x1foffset=13\x1fid=aaa\0");
        stream.extend_from_slice(nasty);
        stream.extend_from_slice(b"\0");
        stream.extend_from_slice(b"\x1eposition\x1fid=aaa\x1foffset=21\0");
        stream.extend_from_slice(b"\x1estream-end\x1fentries=2\0");

        let frames: Vec<Frame> = Frames::new(&stream).map(|f| f.unwrap()).collect();
        let kinds: Vec<String> = frames
            .iter()
            .map(|f| String::from_utf8_lossy(f.kind).into_owned())
            .collect();
        assert_eq!(
            kinds,
            [
                "stream-start",
                "source",
                "entry",
                "entry",
                "position",
                "stream-end"
            ],
            "a payload containing RS, NUL and US did not desynchronise the walk"
        );

        // Fields are readable without decoding, and the bytes are the
        // producer's — a forwarder writing them out changes nothing.
        let e = &frames[2];
        assert_eq!(e.field("id"), Some("aaa"));
        assert_eq!((e.number("offset"), e.number("chunk")), (Some(10), Some(4)));
        assert_eq!(
            e.bytes,
            b"\x1eentry\x1flen=3\x1foffset=10\x1fchunk=4\x1fid=aaa\0one\0"
        );
        assert_eq!(frames[3].number("chunk"), None, "read from the live edge");

        // Concatenating the kept records is a stream again.
        let kept: Vec<u8> = frames
            .iter()
            .filter(|f| f.kind == b"entry")
            .flat_map(|f| f.bytes.iter().copied())
            .collect();
        let again: Vec<String> = Frames::new(&kept)
            .map(|f| String::from_utf8_lossy(f.unwrap().kind).into_owned())
            .collect();
        assert_eq!(again, ["entry", "entry"]);
    }

    /// A multi-source stream attributes every entry, and the `position`
    /// records at the end are the cursor a consumer hands back. Both were
    /// on the wire before anything in this file could read them, so a
    /// consumer merging several stores could neither tell them apart nor
    /// resume any of them.
    #[test]
    fn a_multi_source_stream_is_attributed_and_carries_positions() {
        let stream = b"\x1estream-start\x1fv=1\x1fsources=2\0\
                       \x1eentry\x1flen=3\x1fts=7\x1fwf=8\x1fwl=9\x1foffset=0\x1fchunk=4\
                       \x1fsrc=/l/a.log\x1fid=aaa\0one\0\
                       \x1eentry\x1flen=3\x1fwf=8\x1fwl=9\x1foffset=0\x1fsrc=/l/b.log\x1fid=bbb\0two\0\
                       \x1eposition\x1fpath=/l/a.log\x1fid=aaa\x1foffset=33\0\
                       \x1eposition\x1fpath=/l/b.log\x1fid=bbb\0\
                       \x1estream-end\x1fentries=2\x1fstatus=exhausted\0";
        let recs = read(&stream[..]);
        let entries: Vec<&EntryRec> = recs
            .iter()
            .filter_map(|r| match r {
                Rec::Entry(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id.as_deref(), Some("aaa"));
        assert_eq!(entries[0].src.as_deref(), Some("/l/a.log"));
        assert_eq!(entries[0].chunk, Some(4));
        // The watermark a consumer reports is this plus the payload's
        // length: both ends of what was served, from the wire.
        assert_eq!(entries[0].offset, Some(0));
        assert_eq!(entries[1].id.as_deref(), Some("bbb"));
        // Read from the live edge, so no chunk holds it yet — which does
        // not make it unattributed.
        assert_eq!(entries[1].chunk, None);

        let pos: Vec<&PositionRec> = recs
            .iter()
            .filter_map(|r| match r {
                Rec::Position(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(pos.len(), 2);
        assert_eq!(
            (pos[0].id.as_deref(), pos[0].offset),
            (Some("aaa"), Some(33))
        );
        // Nothing delivered and nothing resumed from: no position at all,
        // which is not offset zero.
        assert_eq!((pos[1].id.as_deref(), pos[1].offset), (Some("bbb"), None));
    }

    /// A single-source stream names its source once, so its entries carry
    /// no attribution — and a consumer must not read that as "unknown
    /// store".
    #[test]
    fn a_single_source_stream_does_not_attribute_each_entry() {
        let stream =
            b"\x1estream-start\x1fv=1\x1fsources=1\0\x1eentry\x1flen=3\x1fwf=1\x1fwl=1\0one\0\
              \x1estream-end\x1fentries=1\0";
        let recs = read(&stream[..]);
        let Rec::Entry(e) = &recs[1] else {
            panic!("expected an entry");
        };
        assert!(e.src.is_none() && e.id.is_none());
    }
}
