//! The native replication wire: the codec, and nothing else — no I/O, no
//! transport, no store access. See `docs/plans/native-replication.md`.
//!
//! One hello per connection, then typed frames each carrying a stream id
//! and their own payload length. That length is the load-bearing part: a
//! reader can skip a frame it does not understand, which is what lets new
//! frame types and new sidecar kinds be additive rather than a version
//! bump. `incompat_flags` in the hello is the other half — it guards
//! changes to how a CHUNK must be interpreted, where skipping is not an
//! option.
//!
//! Everything is little-endian, as in `format.rs`.

use std::fmt;

pub const MAGIC: &[u8; 8] = b"TIMBSTR1";
pub const VERSION: u32 = 1;
/// magic + version + incompat_flags.
pub const HELLO_LEN: usize = 16;
/// stream id + kind + payload length.
pub const FRAME_HEADER_LEN: usize = 12;
/// A sidecar table entry: an 8-byte kind tag and a u32 length.
pub const SIDECAR_ENTRY_LEN: usize = 12;

/// Largest payload this codec will accept. A length field arrives from the
/// network, so it is bounded BEFORE anything is allocated on the strength
/// of it. Generous next to a 256 KiB chunk plus its sidecars.
pub const MAX_PAYLOAD: u32 = 64 << 20;

/// `last_seq` when the sender does not know where the stream ends —
/// deliberately not 0, which is a legitimate last seq for a stream
/// carrying only chunk 0.
pub const OPEN_ENDED: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    /// A bit set here that the reader does not know means STOP: something
    /// about the chunks themselves changed, so skipping is not safe.
    pub incompat: u32,
}

/// What a `stream-open` asks for. `Coverage` answers with a run list and
/// no chunk frames, `Index` sends chunk frames carrying metadata and their
/// true `comp_len` but no bytes, `Frames` sends the bytes too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Coverage,
    Index,
    Frames,
}

impl Mode {
    fn wire(self) -> u32 {
        match self {
            Mode::Coverage => 1,
            Mode::Index => 2,
            Mode::Frames => 3,
        }
    }

    fn from_wire(v: u32) -> Result<Mode, FrameError> {
        match v {
            1 => Ok(Mode::Coverage),
            2 => Ok(Mode::Index),
            3 => Ok(Mode::Frames),
            // 0 is reserved so an unset field cannot read as a valid mode.
            other => Err(FrameError::BadMode(other)),
        }
    }
}

/// One run of a tape a peer holds, both ends INCLUSIVE. A contiguous store
/// is one run; the empty list means "nothing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub start: u64,
    pub end: u64,
}

/// Data riding alongside a chunk (or alongside a stream, in the hello
/// frame): a `.grain` page, a digest, a future zone-map. The tag names the
/// kind AND its parameters, so a reader with different parameters simply
/// does not recognise the kind and drops it — a rebuild, rather than a
/// filter read under the wrong constants.
///
/// An unknown kind is dropped by the CALLER; the codec round-trips it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    pub kind: [u8; 8],
    pub bytes: Vec<u8>,
}

impl Sidecar {
    /// A tag from a string, padded with NULs. Longer than 8 bytes is a
    /// programming error, not a wire condition.
    pub fn tag(name: &str) -> [u8; 8] {
        let mut t = [0u8; 8];
        let b = name.as_bytes();
        let n = b.len().min(8);
        t[..n].copy_from_slice(&b[..n]);
        t
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Opens a logical stream on this connection and says what it is.
    /// `origin_id` is the travelling half of the address, copied verbatim;
    /// `sender_id` is the sender's own store, which becomes the
    /// destination's `derived_from`. Neither is ever the destination's own
    /// identity — it mints that itself.
    StreamOpen {
        origin_id: [u8; 16],
        sender_id: [u8; 16],
        first_seq: u64,
        /// `OPEN_ENDED` for a live stream with no known end.
        last_seq: u64,
        mode: Mode,
        /// The store's labels as JSON — `bark::provenance()`, not the whole
        /// manifest: the receiver keeps its own retention and index policy.
        provenance: Vec<u8>,
        sidecars: Vec<Sidecar>,
    },
    /// One chunk. `comp_len` is the chunk's TRUE compressed size even when
    /// the bytes are absent, because half of what a catalogue answers is
    /// how big the thing is. Offsets never travel: the receiver derives
    /// its own by accumulation.
    Chunk {
        seq: u64,
        uncomp_len: u64,
        comp_len: u64,
        /// `None` in `Index` mode — metadata without the payload.
        comp: Option<Vec<u8>>,
        first_write_ms: u64,
        last_write_ms: u64,
        sidecars: Vec<Sidecar>,
    },
    /// What a peer holds. Also serves as the ack: a contiguous receiver
    /// acking a store it holds to 424242 sends one run, which is the
    /// degenerate case of the same answer rather than a second frame type.
    Coverage { runs: Vec<Run> },
    /// The server's side of a registration: a handle it controls, plus
    /// what it already holds for this origin.
    Accepted {
        registration_id: [u8; 16],
        runs: Vec<Run>,
    },
    /// Refused: something else already holds this identity or name.
    /// `holder_origin` and its coverage are what make the conflict
    /// actionable rather than merely reported.
    Conflict {
        holder_origin: [u8; 16],
        runs: Vec<Run>,
        reason: String,
    },
    /// A kind this build does not know, kept whole. The payload length is
    /// what made that possible, and dropping it is safe by construction:
    /// an additive type cannot be load-bearing for an older peer.
    Unknown { kind: u32, payload: Vec<u8> },
}

impl Frame {
    fn wire_kind(&self) -> u32 {
        match self {
            Frame::StreamOpen { .. } => 1,
            Frame::Chunk { .. } => 2,
            Frame::Coverage { .. } => 3,
            Frame::Accepted { .. } => 4,
            Frame::Conflict { .. } => 5,
            Frame::Unknown { kind, .. } => *kind,
        }
    }
}

/// A frame and the stream it belongs to. Stream 0 is the only stream on a
/// connection that never multiplexes, so a 1:1 transport pays 4 bytes and
/// no design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framed {
    pub stream: u32,
    pub frame: Frame,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    BadMagic,
    /// A version this build does not implement. Refuse rather than guess.
    BadVersion(u32),
    /// An incompat bit we do not understand: the chunks themselves may not
    /// mean what we think.
    Incompatible(u32),
    TooLarge(u32),
    BadMode(u32),
    /// The payload's declared length does not match what its own fields
    /// describe — a self-check the fixed prefix makes possible.
    Inconsistent(&'static str),
    BadUtf8,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::BadMagic => write!(f, "not a timberfs replication stream"),
            FrameError::BadVersion(v) => {
                write!(f, "stream version {v} is newer than this build ({VERSION})")
            }
            FrameError::Incompatible(bits) => write!(
                f,
                "stream sets incompatible flags {bits:#x} this build does not implement"
            ),
            FrameError::TooLarge(n) => {
                write!(
                    f,
                    "frame payload of {n} bytes exceeds the {MAX_PAYLOAD} cap"
                )
            }
            FrameError::BadMode(v) => write!(f, "unknown stream mode {v}"),
            FrameError::Inconsistent(what) => write!(f, "malformed frame: {what}"),
            FrameError::BadUtf8 => write!(f, "malformed frame: text is not UTF-8"),
        }
    }
}

impl std::error::Error for FrameError {}

// ---------------------------------------------------------------- encoding

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_runs(out: &mut Vec<u8>, runs: &[Run]) {
    put_u32(out, runs.len() as u32);
    for r in runs {
        put_u64(out, r.start);
        put_u64(out, r.end);
    }
}

/// The sidecar table, then a callback for the fixed remainder, then the
/// sidecar bodies — so every length a reader needs sits in the prefix.
fn put_sidecar_table(out: &mut Vec<u8>, sidecars: &[Sidecar]) {
    put_u32(out, sidecars.len() as u32);
    for s in sidecars {
        out.extend_from_slice(&s.kind);
        put_u32(out, s.bytes.len() as u32);
    }
}

fn put_sidecar_bodies(out: &mut Vec<u8>, sidecars: &[Sidecar]) {
    for s in sidecars {
        out.extend_from_slice(&s.bytes);
    }
}

pub fn encode_hello(h: Hello) -> [u8; HELLO_LEN] {
    let mut b = [0u8; HELLO_LEN];
    b[0..8].copy_from_slice(MAGIC);
    b[8..12].copy_from_slice(&h.version.to_le_bytes());
    b[12..16].copy_from_slice(&h.incompat.to_le_bytes());
    b
}

/// Append one frame to `out`.
pub fn encode(f: &Framed, out: &mut Vec<u8>) {
    let mut payload = Vec::new();
    match &f.frame {
        Frame::StreamOpen {
            origin_id,
            sender_id,
            first_seq,
            last_seq,
            mode,
            provenance,
            sidecars,
        } => {
            payload.extend_from_slice(origin_id);
            payload.extend_from_slice(sender_id);
            put_u64(&mut payload, *first_seq);
            put_u64(&mut payload, *last_seq);
            put_u32(&mut payload, mode.wire());
            put_u32(&mut payload, provenance.len() as u32);
            put_sidecar_table(&mut payload, sidecars);
            payload.extend_from_slice(provenance);
            put_sidecar_bodies(&mut payload, sidecars);
        }
        Frame::Chunk {
            seq,
            uncomp_len,
            comp_len,
            comp,
            first_write_ms,
            last_write_ms,
            sidecars,
        } => {
            put_u64(&mut payload, *seq);
            put_u64(&mut payload, *uncomp_len);
            put_u64(&mut payload, *comp_len);
            put_u64(&mut payload, *first_write_ms);
            put_u64(&mut payload, *last_write_ms);
            put_sidecar_table(&mut payload, sidecars);
            if let Some(bytes) = comp {
                payload.extend_from_slice(bytes);
            }
            put_sidecar_bodies(&mut payload, sidecars);
        }
        Frame::Coverage { runs } => put_runs(&mut payload, runs),
        Frame::Accepted {
            registration_id,
            runs,
        } => {
            payload.extend_from_slice(registration_id);
            put_runs(&mut payload, runs);
        }
        Frame::Conflict {
            holder_origin,
            runs,
            reason,
        } => {
            payload.extend_from_slice(holder_origin);
            put_runs(&mut payload, runs);
            put_u32(&mut payload, reason.len() as u32);
            payload.extend_from_slice(reason.as_bytes());
        }
        Frame::Unknown { payload: p, .. } => payload.extend_from_slice(p),
    }
    put_u32(out, f.stream);
    put_u32(out, f.frame.wire_kind());
    put_u32(out, payload.len() as u32);
    out.extend_from_slice(&payload);
}

// ---------------------------------------------------------------- decoding

/// A cursor over one frame's payload that cannot read past its end.
struct Cur<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, at: 0 }
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], FrameError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(FrameError::Inconsistent(what))?;
        if end > self.b.len() {
            return Err(FrameError::Inconsistent(what));
        }
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, FrameError> {
        Ok(u32::from_le_bytes(self.take(4, what)?.try_into().unwrap()))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, FrameError> {
        Ok(u64::from_le_bytes(self.take(8, what)?.try_into().unwrap()))
    }

    fn id(&mut self, what: &'static str) -> Result<[u8; 16], FrameError> {
        Ok(self.take(16, what)?.try_into().unwrap())
    }

    fn remaining(&self) -> usize {
        self.b.len() - self.at
    }
}

fn get_runs(c: &mut Cur<'_>) -> Result<Vec<Run>, FrameError> {
    let n = c.u32("run count")? as usize;
    // Each run is 16 bytes, so a count the payload cannot hold is refused
    // before it is allocated for.
    if n.saturating_mul(16) > c.remaining() {
        return Err(FrameError::Inconsistent("run count exceeds payload"));
    }
    let mut runs = Vec::with_capacity(n);
    for _ in 0..n {
        let start = c.u64("run start")?;
        let end = c.u64("run end")?;
        if end < start {
            return Err(FrameError::Inconsistent("run ends before it starts"));
        }
        runs.push(Run { start, end });
    }
    Ok(runs)
}

/// The sidecar TABLE: kinds and lengths, no bodies yet.
fn get_sidecar_table(c: &mut Cur<'_>) -> Result<Vec<([u8; 8], usize)>, FrameError> {
    let n = c.u32("sidecar count")? as usize;
    if n.saturating_mul(SIDECAR_ENTRY_LEN) > c.remaining() {
        return Err(FrameError::Inconsistent("sidecar count exceeds payload"));
    }
    let mut table = Vec::with_capacity(n);
    for _ in 0..n {
        let kind: [u8; 8] = c.take(8, "sidecar kind")?.try_into().unwrap();
        let len = c.u32("sidecar length")? as usize;
        table.push((kind, len));
    }
    Ok(table)
}

fn get_sidecars(c: &mut Cur<'_>, table: &[([u8; 8], usize)]) -> Result<Vec<Sidecar>, FrameError> {
    let mut out = Vec::with_capacity(table.len());
    for (kind, len) in table {
        out.push(Sidecar {
            kind: *kind,
            bytes: c.take(*len, "sidecar body")?.to_vec(),
        });
    }
    Ok(out)
}

pub fn decode_hello(buf: &[u8]) -> Result<Option<(Hello, usize)>, FrameError> {
    if buf.len() < HELLO_LEN {
        return Ok(None);
    }
    if &buf[..8] != MAGIC {
        return Err(FrameError::BadMagic);
    }
    let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    if version != VERSION {
        return Err(FrameError::BadVersion(version));
    }
    let incompat = u32::from_le_bytes(buf[12..16].try_into().unwrap());
    if incompat != 0 {
        return Err(FrameError::Incompatible(incompat));
    }
    Ok(Some((Hello { version, incompat }, HELLO_LEN)))
}

/// Decode one frame. `Ok(None)` means the buffer does not hold a whole
/// frame yet — the caller reads more and asks again. Errors are permanent:
/// the stream is malformed and cannot be resynchronised, because framing
/// IS the length field.
pub fn decode(buf: &[u8]) -> Result<Option<(Framed, usize)>, FrameError> {
    if buf.len() < FRAME_HEADER_LEN {
        return Ok(None);
    }
    let stream = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let kind = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let len = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    if len > MAX_PAYLOAD {
        return Err(FrameError::TooLarge(len));
    }
    let total = FRAME_HEADER_LEN + len as usize;
    if buf.len() < total {
        return Ok(None);
    }
    let payload = &buf[FRAME_HEADER_LEN..total];
    let mut c = Cur::new(payload);
    let frame = match kind {
        1 => {
            let origin_id = c.id("origin id")?;
            let sender_id = c.id("sender id")?;
            let first_seq = c.u64("first seq")?;
            let last_seq = c.u64("last seq")?;
            let mode = Mode::from_wire(c.u32("mode")?)?;
            let prov_len = c.u32("provenance length")? as usize;
            let table = get_sidecar_table(&mut c)?;
            let provenance = c.take(prov_len, "provenance")?.to_vec();
            let sidecars = get_sidecars(&mut c, &table)?;
            Frame::StreamOpen {
                origin_id,
                sender_id,
                first_seq,
                last_seq,
                mode,
                provenance,
                sidecars,
            }
        }
        2 => {
            let seq = c.u64("seq")?;
            let uncomp_len = c.u64("uncomp_len")?;
            let comp_len = c.u64("comp_len")?;
            let first_write_ms = c.u64("first write")?;
            let last_write_ms = c.u64("last write")?;
            let table = get_sidecar_table(&mut c)?;
            let sidecar_bytes: usize = table.iter().map(|(_, l)| *l).sum();
            // Whether the bytes came along is decidable from the lengths
            // already read, and self-checking: the remainder is either the
            // whole chunk or nothing. Anything else is a malformed frame
            // rather than a guess about which mode the sender was in.
            let rest = c
                .remaining()
                .checked_sub(sidecar_bytes)
                .ok_or(FrameError::Inconsistent("sidecar bodies exceed payload"))?;
            let comp = if rest == 0 {
                None
            } else if rest as u64 == comp_len {
                Some(c.take(rest, "chunk payload")?.to_vec())
            } else {
                return Err(FrameError::Inconsistent(
                    "chunk payload is neither absent nor comp_len bytes",
                ));
            };
            let sidecars = get_sidecars(&mut c, &table)?;
            Frame::Chunk {
                seq,
                uncomp_len,
                comp_len,
                comp,
                first_write_ms,
                last_write_ms,
                sidecars,
            }
        }
        3 => Frame::Coverage {
            runs: get_runs(&mut c)?,
        },
        4 => Frame::Accepted {
            registration_id: c.id("registration id")?,
            runs: get_runs(&mut c)?,
        },
        5 => {
            let holder_origin = c.id("holder origin")?;
            let runs = get_runs(&mut c)?;
            let n = c.u32("reason length")? as usize;
            let reason = String::from_utf8(c.take(n, "reason")?.to_vec())
                .map_err(|_| FrameError::BadUtf8)?;
            Frame::Conflict {
                holder_origin,
                runs,
                reason,
            }
        }
        other => Frame::Unknown {
            kind: other,
            payload: payload.to_vec(),
        },
    };
    Ok(Some((Framed { stream, frame }, total)))
}

// ------------------------------------------------------------------- uuids

/// A hyphenated UUID string to its 16 bytes. The manifest stores ids as
/// text; the wire carries them as bytes.
pub fn uuid_bytes(s: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = s.bytes().filter(|b| *b != b'-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// The inverse, in the form `bark` writes.
pub fn uuid_string(b: &[u8; 16]) -> String {
    let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(&b[0..4]),
        h(&b[4..6]),
        h(&b[6..8]),
        h(&b[8..10]),
        h(&b[10..16])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> [u8; 16] {
        [n; 16]
    }

    fn roundtrip(f: Framed) -> Framed {
        let mut buf = Vec::new();
        encode(&f, &mut buf);
        let (got, used) = decode(&buf).unwrap().expect("a whole frame");
        assert_eq!(used, buf.len(), "decode consumed the whole frame");
        assert_eq!(got, f);
        got
    }

    #[test]
    fn every_frame_round_trips() {
        roundtrip(Framed {
            stream: 0,
            frame: Frame::StreamOpen {
                origin_id: id(1),
                sender_id: id(2),
                first_seq: 0,
                last_seq: OPEN_ENDED,
                mode: Mode::Frames,
                provenance: br#"{"host":"apache01"}"#.to_vec(),
                sidecars: vec![Sidecar {
                    kind: Sidecar::tag("GRAIN001"),
                    bytes: vec![0xab; 16],
                }],
            },
        });
        roundtrip(Framed {
            stream: 7,
            frame: Frame::Chunk {
                seq: 424_242,
                uncomp_len: 262_144,
                comp_len: 5,
                comp: Some(vec![1, 2, 3, 4, 5]),
                first_write_ms: 1_700_000_000_000,
                last_write_ms: 1_700_000_005_000,
                sidecars: vec![
                    Sidecar {
                        kind: Sidecar::tag("GRAIN001"),
                        bytes: vec![9; 8],
                    },
                    Sidecar {
                        kind: Sidecar::tag("XXH3"),
                        bytes: vec![7; 8],
                    },
                ],
            },
        });
        roundtrip(Framed {
            stream: 1,
            frame: Frame::Coverage {
                runs: vec![Run { start: 0, end: 5 }, Run { start: 7, end: 7 }],
            },
        });
        roundtrip(Framed {
            stream: 1,
            frame: Frame::Accepted {
                registration_id: id(3),
                runs: vec![Run {
                    start: 0,
                    end: 424_242,
                }],
            },
        });
        roundtrip(Framed {
            stream: 1,
            frame: Frame::Conflict {
                holder_origin: id(4),
                runs: vec![Run {
                    start: 0,
                    end: 424_242,
                }],
                reason: "held by another origin".to_string(),
            },
        });
        // Empty everywhere: no runs, no sidecars, no provenance.
        roundtrip(Framed {
            stream: 0,
            frame: Frame::Coverage { runs: vec![] },
        });
    }

    #[test]
    fn an_unknown_kind_is_skippable_which_is_the_whole_point() {
        // The property every additive change rests on: a reader that does
        // not know a frame type still knows where it ends, so a new type
        // needs no version bump and no negotiation.
        let mut buf = Vec::new();
        encode(
            &Framed {
                stream: 3,
                frame: Frame::Unknown {
                    kind: 4242,
                    payload: vec![0xde; 300],
                },
            },
            &mut buf,
        );
        // ...and a frame we DO understand follows it, still readable.
        let known = Framed {
            stream: 3,
            frame: Frame::Coverage {
                runs: vec![Run { start: 1, end: 2 }],
            },
        };
        encode(&known, &mut buf);

        let (first, used) = decode(&buf).unwrap().unwrap();
        match &first.frame {
            Frame::Unknown { kind, payload } => {
                assert_eq!(*kind, 4242);
                assert_eq!(payload.len(), 300);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        let (second, _) = decode(&buf[used..]).unwrap().unwrap();
        assert_eq!(second, known, "the stream resynchronises after a skip");
    }

    #[test]
    fn a_partial_buffer_asks_for_more_rather_than_failing() {
        let mut buf = Vec::new();
        encode(
            &Framed {
                stream: 0,
                frame: Frame::Chunk {
                    seq: 1,
                    uncomp_len: 10,
                    comp_len: 4,
                    comp: Some(vec![1, 2, 3, 4]),
                    first_write_ms: 5,
                    last_write_ms: 6,
                    sidecars: vec![],
                },
            },
            &mut buf,
        );
        // Every prefix short of the whole frame is "not yet", never an
        // error: a stream reader must be able to accumulate.
        for n in 0..buf.len() {
            assert_eq!(decode(&buf[..n]), Ok(None), "prefix of {n} bytes");
        }
        assert!(decode(&buf).unwrap().is_some());
    }

    #[test]
    fn index_mode_carries_the_true_size_without_the_bytes() {
        // Half of what a catalogue answers is how big the thing is, so
        // comp_len is the chunk's real size even with no payload attached.
        let f = roundtrip(Framed {
            stream: 0,
            frame: Frame::Chunk {
                seq: 9,
                uncomp_len: 262_144,
                comp_len: 25_919,
                comp: None,
                first_write_ms: 1,
                last_write_ms: 2,
                sidecars: vec![Sidecar {
                    kind: Sidecar::tag("GRAIN001"),
                    bytes: vec![3; 44],
                }],
            },
        });
        match &f.frame {
            Frame::Chunk { comp, comp_len, .. } => {
                assert!(comp.is_none());
                assert_eq!(*comp_len, 25_919, "the size survives without the bytes");
            }
            other => panic!("{other:?}"),
        }
        // And a metadata-only chunk frame is small regardless of the chunk.
        let mut buf = Vec::new();
        encode(&f, &mut buf);
        assert!(buf.len() < 128, "index frame was {} bytes", buf.len());
    }

    #[test]
    fn a_chunk_payload_that_is_neither_absent_nor_whole_is_refused() {
        // Present-or-absent is decidable from the lengths in the prefix,
        // so a truncated body is a malformed frame rather than a guess
        // about which mode the sender was in.
        let mut buf = Vec::new();
        encode(
            &Framed {
                stream: 0,
                frame: Frame::Chunk {
                    seq: 1,
                    uncomp_len: 10,
                    comp_len: 8,
                    comp: Some(vec![0; 8]),
                    first_write_ms: 1,
                    last_write_ms: 2,
                    sidecars: vec![],
                },
            },
            &mut buf,
        );
        // Claim one more byte of payload than the chunk actually holds.
        let len = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        buf[8..12].copy_from_slice(&(len - 1).to_le_bytes());
        buf.pop();
        assert!(matches!(decode(&buf), Err(FrameError::Inconsistent(_))));
    }

    #[test]
    fn declared_lengths_are_bounded_before_anything_is_allocated() {
        // Every count comes off the network, so each is checked against
        // what the payload can actually hold. Left unchecked these are an
        // allocation of gigabytes from twelve bytes of input.
        let mut buf = Vec::new();
        put_u32(&mut buf, 0);
        put_u32(&mut buf, 3); // Coverage
        put_u32(&mut buf, 4);
        put_u32(&mut buf, u32::MAX); // run count
        assert!(matches!(decode(&buf), Err(FrameError::Inconsistent(_))));

        let mut buf = Vec::new();
        put_u32(&mut buf, 0);
        put_u32(&mut buf, 2); // Chunk
        put_u32(&mut buf, 44);
        for _ in 0..5 {
            put_u64(&mut buf, 0);
        }
        put_u32(&mut buf, u32::MAX); // sidecar count
        assert!(matches!(decode(&buf), Err(FrameError::Inconsistent(_))));

        // And the payload length itself is capped before the buffer is
        // even consulted.
        let mut buf = Vec::new();
        put_u32(&mut buf, 0);
        put_u32(&mut buf, 3);
        put_u32(&mut buf, MAX_PAYLOAD + 1);
        assert_eq!(decode(&buf), Err(FrameError::TooLarge(MAX_PAYLOAD + 1)));
    }

    #[test]
    fn open_ended_is_not_seq_zero() {
        // A stream carrying only chunk 0 has last_seq 0, which must not
        // read as "I do not know where this ends".
        let f = roundtrip(Framed {
            stream: 0,
            frame: Frame::StreamOpen {
                origin_id: id(1),
                sender_id: id(2),
                first_seq: 0,
                last_seq: 0,
                mode: Mode::Index,
                provenance: vec![],
                sidecars: vec![],
            },
        });
        match f.frame {
            Frame::StreamOpen { last_seq, .. } => {
                assert_eq!(last_seq, 0);
                assert_ne!(last_seq, OPEN_ENDED);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_hello_refuses_what_it_cannot_honour() {
        let h = Hello {
            version: VERSION,
            incompat: 0,
        };
        let b = encode_hello(h);
        assert_eq!(decode_hello(&b).unwrap(), Some((h, HELLO_LEN)));
        // Short of a whole hello: ask for more.
        assert_eq!(decode_hello(&b[..8]), Ok(None));
        // Not our stream at all.
        assert_eq!(decode_hello(b"NOPE0000________"), Err(FrameError::BadMagic));
        // A newer version, and any incompat bit, are refused rather than
        // guessed at: the second says the CHUNKS may not mean what we
        // think, which no amount of skipping makes safe.
        let mut newer = b;
        newer[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert_eq!(
            decode_hello(&newer),
            Err(FrameError::BadVersion(VERSION + 1))
        );
        let mut flagged = b;
        flagged[12..16].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(decode_hello(&flagged), Err(FrameError::Incompatible(1)));
    }

    #[test]
    fn mode_zero_is_reserved_so_an_unset_field_cannot_pass() {
        let mut buf = Vec::new();
        encode(
            &Framed {
                stream: 0,
                frame: Frame::StreamOpen {
                    origin_id: id(1),
                    sender_id: id(2),
                    first_seq: 0,
                    last_seq: 0,
                    mode: Mode::Frames,
                    provenance: vec![],
                    sidecars: vec![],
                },
            },
            &mut buf,
        );
        // mode sits at 12 (header) + 16 + 16 + 8 + 8.
        buf[60..64].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode(&buf), Err(FrameError::BadMode(0)));
    }

    #[test]
    fn several_frames_decode_from_one_buffer() {
        let frames = vec![
            Framed {
                stream: 0,
                frame: Frame::Coverage {
                    runs: vec![Run { start: 0, end: 1 }],
                },
            },
            Framed {
                stream: 1,
                frame: Frame::Chunk {
                    seq: 2,
                    uncomp_len: 3,
                    comp_len: 2,
                    comp: Some(vec![8, 9]),
                    first_write_ms: 4,
                    last_write_ms: 5,
                    sidecars: vec![],
                },
            },
            Framed {
                stream: 0,
                frame: Frame::Coverage { runs: vec![] },
            },
        ];
        let mut buf = Vec::new();
        for f in &frames {
            encode(f, &mut buf);
        }
        let mut at = 0;
        for want in &frames {
            let (got, used) = decode(&buf[at..]).unwrap().unwrap();
            assert_eq!(&got, want);
            at += used;
        }
        assert_eq!(at, buf.len(), "no trailing bytes");
        assert_eq!(decode(&buf[at..]), Ok(None));
    }

    #[test]
    fn the_per_chunk_overhead_is_what_the_design_claims() {
        // The note puts the frame's cost at 0.17% of a 25 KB chunk with no
        // sidecars, and 0.06% more per sidecar. Worth asserting, because a
        // field added carelessly is invisible until it is on every chunk.
        let comp = vec![0u8; 25_919]; // the measured mean frame size
        let bare = Framed {
            stream: 0,
            frame: Frame::Chunk {
                seq: 1,
                uncomp_len: 262_144,
                comp_len: comp.len() as u64,
                comp: Some(comp.clone()),
                first_write_ms: 1,
                last_write_ms: 2,
                sidecars: vec![],
            },
        };
        let mut buf = Vec::new();
        encode(&bare, &mut buf);
        let overhead = buf.len() - comp.len();
        assert_eq!(
            overhead,
            FRAME_HEADER_LEN + 44,
            "12 header + 40 fixed + 4 count"
        );
        assert!(
            (overhead as f64) / (comp.len() as f64) < 0.003,
            "overhead {overhead} B on a {} B chunk",
            comp.len()
        );

        let with_grain = Framed {
            stream: 0,
            frame: Frame::Chunk {
                seq: 1,
                uncomp_len: 262_144,
                comp_len: comp.len() as u64,
                comp: Some(comp.clone()),
                first_write_ms: 1,
                last_write_ms: 2,
                sidecars: vec![Sidecar {
                    kind: Sidecar::tag("GRAIN001"),
                    bytes: vec![0; 44],
                }],
            },
        };
        let mut buf2 = Vec::new();
        encode(&with_grain, &mut buf2);
        // One sidecar costs its table entry plus its bytes, and nothing else.
        assert_eq!(buf2.len(), buf.len() + SIDECAR_ENTRY_LEN + 44);
    }

    #[test]
    fn uuids_round_trip_in_the_form_the_manifest_writes() {
        let s = "899c8aad-4d3d-4c60-b125-ee5ce7e1cc86";
        let b = uuid_bytes(s).expect("parses");
        assert_eq!(uuid_string(&b), s);
        assert_eq!(uuid_bytes("not-a-uuid"), None);
        assert_eq!(uuid_bytes(""), None);
        // A well-formed length with a non-hex digit is still not a uuid.
        assert_eq!(uuid_bytes("899c8aad-4d3d-4c60-b125-ee5ce7e1ccZZ"), None);
    }

    #[test]
    fn a_sidecar_kind_we_do_not_know_survives_the_trip_intact() {
        // The codec does not judge kinds; dropping an unknown one is the
        // caller's decision, and it can only make it if the bytes arrived.
        let f = roundtrip(Framed {
            stream: 0,
            frame: Frame::Chunk {
                seq: 1,
                uncomp_len: 2,
                comp_len: 0,
                comp: None,
                first_write_ms: 3,
                last_write_ms: 4,
                sidecars: vec![Sidecar {
                    kind: Sidecar::tag("FUTURE99"),
                    bytes: vec![1, 2, 3],
                }],
            },
        });
        match f.frame {
            Frame::Chunk { sidecars, .. } => {
                assert_eq!(sidecars[0].kind, Sidecar::tag("FUTURE99"));
                assert_eq!(sidecars[0].bytes, vec![1, 2, 3]);
            }
            other => panic!("{other:?}"),
        }
    }
}
