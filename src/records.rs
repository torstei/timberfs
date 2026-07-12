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
    pub payload: Vec<u8>,
}

pub enum Rec {
    /// stream-start fields (excluding the kind), in stream order.
    Start(Vec<(String, String)>),
    Source(Vec<(String, String)>),
    Entry(EntryRec),
    /// stream-end fields; its arrival is the completeness marker.
    End(Vec<(String, String)>),
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
                        payload,
                    })));
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
