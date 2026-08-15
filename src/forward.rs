//! `timberfs forward-intake`: a TCP receiver speaking the Fluentd Forward
//! protocol v1 — the wire protocol shared by Docker's `fluentd` log driver,
//! Fluent Bit, Fluentd, and the fluent-logger client libraries. One receiver
//! makes all of those valid timberfs producers, each tag landing in its own
//! store (one directory, many `FileStore`s, exactly like the mount).
//!
//! The motivating property is at-least-once delivery: a sender may attach a
//! `chunk` id to a batch and retry until it sees `{"ack": id}` back. We ack
//! only once the batch is durable — a guarantee a pipe into
//! `append` could never express, which is why this is its own subcommand
//! rather than a stream filter (only `timberfs` writes stores).
//!
//! Durable means fsynced into the store's `.sap` write-ahead sidecar
//! (every store this receiver touches declares `"wal": true`): an ack
//! costs one raw append + one fsync, immediately, while chunks keep their
//! own size/age cadence — per-message-ack senders (Docker's driver in
//! blocking mode acks every line) get wire-speed acks WITHOUT shredding
//! the store into one chunk per line.
//!
//! Stores are pre-created by the operator (`timberfs create --wal`) by
//! default: creation is the operator's decision, not the network's. An
//! unknown tag is refused — logged once, its events dropped, its `chunk`
//! ids never acked — so an acking sender simply buffers and retries until
//! the store exists, and provisioning converges with nothing lost.
//! `--auto-create` opts into minting a store per new tag instead: the
//! Docker-host mode, where tags are container names that come and go.
//!
//! Deliberate limitations, all because there is no auth/negotiation phase in
//! Forward v1 and we don't implement one:
//!   - no TLS — deploy on loopback or a private network;
//!   - no handshake (HELO/PING/PONG) — clients transition straight to event
//!     transport, which is spec-legal and how Docker's driver already works;
//!   - no UDP heartbeat listener;
//!   - `CompressedPackedForward` (gzip) is refused — we log which connection
//!     asked for it and close rather than silently drop data.
//!
//! The decoder half (`decode_message` and friends) is pure — no I/O — so it
//! is exercised directly by the unit tests below. The receiver half is the
//! shared intake core (see intake.rs: the directory of stores, their locks
//! and the flush/retention/grain/graceful-exit tick) plus one thread per TCP
//! connection and the ack completion that is Forward's own.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Context;
use rmpv::Value;
use serde_json::Value as Json;

use crate::store::{self, Config};

/// One decoded Forward-protocol message: the tag it addresses, the
/// (event-time-ms, record) pairs it carries, and the ack chunk id if the
/// sender wants one.
pub struct Decoded {
    pub tag: String,
    pub entries: Vec<(u64, Value)>,
    pub chunk: Option<String>,
}

pub enum DecodeError {
    /// A well-formed but unsupported mode (currently: gzip-compressed
    /// PackedForward) — named so the caller can log exactly what it refused.
    Unsupported(String),
    /// A malformed message. The msgpack stream itself may now be desynced
    /// (an array we thought was the whole message may only be a prefix), so
    /// the caller should not attempt to keep reading from this connection.
    Invalid(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Unsupported(s) => write!(f, "{s} is not supported"),
            DecodeError::Invalid(s) => write!(f, "{s}"),
        }
    }
}

fn value_to_tag(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(
            s.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| String::from_utf8_lossy(s.as_bytes()).into_owned()),
        ),
        _ => None,
    }
}

fn find_field<'a>(record: &'a Value, key: &str) -> Option<&'a Value> {
    match record {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| matches!(k, Value::String(s) if s.as_str() == Some(key)))
            .map(|(_, v)| v),
        _ => None,
    }
}

fn record_str<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    match find_field(record, key) {
        Some(Value::String(s)) => s.as_str(),
        _ => None,
    }
}

fn chunk_of(option: Option<&Value>) -> Option<String> {
    option
        .and_then(|o| record_str(o, "chunk"))
        .map(str::to_string)
}

fn is_gzip_compressed(option: Option<&Value>) -> bool {
    option.and_then(|o| record_str(o, "compressed")) == Some("gzip")
}

/// unix seconds/EventTime-ext/float -> unix ms; zero, absent, negative or
/// unparseable all fall back to the write-time wall clock (the honest
/// answer for anything we can't read as a real timestamp).
fn time_to_ms(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Integer(i)) => i
            .as_u64()
            .filter(|&s| s > 0)
            .map(|s| s.saturating_mul(1000))
            .unwrap_or_else(store::now_ms),
        Some(Value::F64(f)) if *f > 0.0 => (*f * 1000.0) as u64,
        Some(Value::F32(f)) if *f > 0.0 => (*f as f64 * 1000.0) as u64,
        Some(Value::Ext(tag, data)) if *tag == 0 && data.len() == 8 => {
            let secs = u32::from_be_bytes(data[0..4].try_into().unwrap()) as u64;
            let nanos = u32::from_be_bytes(data[4..8].try_into().unwrap()) as u64;
            if secs == 0 && nanos == 0 {
                store::now_ms()
            } else {
                secs * 1000 + nanos / 1_000_000
            }
        }
        _ => store::now_ms(),
    }
}

enum ElemKind {
    Time,
    Array,
    Bytes,
    Other,
}

fn classify(v: &Value) -> ElemKind {
    match v {
        Value::Array(_) => ElemKind::Array,
        Value::String(_) | Value::Binary(_) => ElemKind::Bytes,
        Value::Integer(_) | Value::Ext(_, _) | Value::F32(_) | Value::F64(_) => ElemKind::Time,
        _ => ElemKind::Other,
    }
}

/// Decode one top-level msgpack value (one Forward-protocol message) into
/// its entries. Pure — no I/O — so the unit tests drive it directly with
/// hand-encoded msgpack bytes.
pub fn decode_message(msg: Value) -> Result<Decoded, DecodeError> {
    let Value::Array(arr) = msg else {
        return Err(DecodeError::Invalid(
            "top-level msgpack value must be an array".to_string(),
        ));
    };
    if arr.len() < 2 {
        return Err(DecodeError::Invalid(format!(
            "message array has {} element(s), need at least [tag, ...]",
            arr.len()
        )));
    }
    let tag = value_to_tag(&arr[0])
        .ok_or_else(|| DecodeError::Invalid("tag (element 0) must be a string".to_string()))?;
    match classify(&arr[1]) {
        ElemKind::Time => decode_message_mode(tag, arr),
        ElemKind::Array => decode_forward_mode(tag, arr),
        ElemKind::Bytes => decode_packed_forward_mode(tag, arr),
        ElemKind::Other => Err(DecodeError::Invalid(
            "cannot classify message: element 1 is neither a time, an array, nor a string/bin"
                .to_string(),
        )),
    }
}

/// `[tag, time, record, option?]`
fn decode_message_mode(tag: String, mut arr: Vec<Value>) -> Result<Decoded, DecodeError> {
    if arr.len() < 3 {
        return Err(DecodeError::Invalid(
            "Message mode needs [tag, time, record]".to_string(),
        ));
    }
    let option = arr.get(3).cloned();
    let chunk = chunk_of(option.as_ref());
    let record = arr.remove(2);
    let time = time_to_ms(Some(&arr[1]));
    Ok(Decoded {
        tag,
        entries: vec![(time, record)],
        chunk,
    })
}

/// `[tag, [[time, record], ...], option?]`
fn decode_forward_mode(tag: String, mut arr: Vec<Value>) -> Result<Decoded, DecodeError> {
    let option = arr.get(2).cloned();
    let chunk = chunk_of(option.as_ref());
    let Value::Array(pairs) = arr.remove(1) else {
        unreachable!("classify() already confirmed element 1 is an array")
    };
    let mut entries = Vec::with_capacity(pairs.len());
    for pair in pairs {
        entries.push(decode_time_record_pair(pair)?);
    }
    Ok(Decoded {
        tag,
        entries,
        chunk,
    })
}

/// `[tag, blob, option?]`, blob = concatenated `[time, record]` pairs.
fn decode_packed_forward_mode(tag: String, mut arr: Vec<Value>) -> Result<Decoded, DecodeError> {
    let option = arr.get(2).cloned();
    if is_gzip_compressed(option.as_ref()) {
        return Err(DecodeError::Unsupported(
            "CompressedPackedForward (gzip)".to_string(),
        ));
    }
    let chunk = chunk_of(option.as_ref());
    let blob: Vec<u8> = match arr.remove(1) {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Binary(b) => b,
        _ => unreachable!("classify() already confirmed element 1 is a string/bin"),
    };
    let mut cursor = io::Cursor::new(blob);
    let len = cursor.get_ref().len() as u64;
    let mut entries = Vec::new();
    while cursor.position() < len {
        let pair = rmpv::decode::read_value(&mut cursor)
            .map_err(|e| DecodeError::Invalid(format!("PackedForward blob entry: {e}")))?;
        entries.push(decode_time_record_pair(pair)?);
    }
    Ok(Decoded {
        tag,
        entries,
        chunk,
    })
}

fn decode_time_record_pair(pair: Value) -> Result<(u64, Value), DecodeError> {
    let Value::Array(mut p) = pair else {
        return Err(DecodeError::Invalid(
            "entry must be [time, record]".to_string(),
        ));
    };
    if p.len() < 2 {
        return Err(DecodeError::Invalid(
            "entry needs [time, record]".to_string(),
        ));
    }
    let record = p.remove(1);
    let time = time_to_ms(Some(&p[0]));
    Ok((time, record))
}

/// Best-effort msgpack Value -> JSON, used only for the whole-record
/// fallback payload when the payload key is missing or not a string.
fn rmpv_to_json(v: &Value) -> Json {
    match v {
        Value::Nil => Json::Null,
        Value::Boolean(b) => Json::Bool(*b),
        Value::Integer(i) => i
            .as_u64()
            .map(Json::from)
            .or_else(|| i.as_i64().map(Json::from))
            .unwrap_or(Json::Null),
        Value::F32(f) => serde_json::Number::from_f64(*f as f64)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::F64(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::String(s) => Json::String(
            s.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| String::from_utf8_lossy(s.as_bytes()).into_owned()),
        ),
        Value::Binary(b) => Json::String(String::from_utf8_lossy(b).into_owned()),
        Value::Array(a) => Json::Array(a.iter().map(rmpv_to_json).collect()),
        Value::Map(m) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in m {
                let key = match k {
                    Value::String(s) => s
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| String::from_utf8_lossy(s.as_bytes()).into_owned()),
                    other => format!("{other:?}"),
                };
                obj.insert(key, rmpv_to_json(v));
            }
            Json::Object(obj)
        }
        Value::Ext(tag, data) => {
            serde_json::json!({"ext_type": tag, "ext_data_len": data.len()})
        }
    }
}

/// `record[payload_key]` if it's a string, else the whole record as compact
/// JSON — without the trailing newline, so partial fragments can be
/// concatenated before one is added at the end.
fn payload_no_newline(record: &Value, payload_key: &str) -> Vec<u8> {
    match find_field(record, payload_key) {
        Some(Value::String(s)) => s.as_bytes().to_vec(),
        _ => rmpv_to_json(record).to_string().into_bytes(),
    }
}

fn with_trailing_newline(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() != Some(&b'\n') {
        bytes.push(b'\n');
    }
    bytes
}

fn is_partial(record: &Value) -> bool {
    record_str(record, "partial_message") == Some("true")
}

const PARTIAL_MAX_BYTES: usize = 16 * 1024 * 1024;
const PARTIAL_MAX_OUTSTANDING: usize = 1000;

struct PartialBuf {
    first_time_ms: u64,
    bytes: Vec<u8>,
}

/// Per-connection reassembly of Docker's split-line partial messages:
/// fragments carrying `partial_message="true"` are buffered by `partial_id`
/// in arrival order and merged into one entry when `partial_last=="true"`.
/// Guarded so a sender that never completes a series (or opens too many at
/// once) can't grow this without bound.
#[derive(Default)]
pub struct PartialReassembler {
    buffers: HashMap<String, PartialBuf>,
}

impl PartialReassembler {
    /// Feed one decoded (time, record) entry; returns zero or more complete
    /// (time, payload-with-trailing-newline) entries ready to append.
    pub fn feed(&mut self, time_ms: u64, record: &Value, payload_key: &str) -> Vec<(u64, Vec<u8>)> {
        if !is_partial(record) {
            return vec![(
                time_ms,
                with_trailing_newline(payload_no_newline(record, payload_key)),
            )];
        }
        let Some(id) = record_str(record, "partial_id") else {
            // No id to key the buffer by: nothing sane to merge with, so
            // pass it through standalone rather than silently dropping it.
            return vec![(
                time_ms,
                with_trailing_newline(payload_no_newline(record, payload_key)),
            )];
        };
        let id = id.to_string();
        let is_last = record_str(record, "partial_last") == Some("true");
        let frag = payload_no_newline(record, payload_key);

        if !self.buffers.contains_key(&id) && self.buffers.len() >= PARTIAL_MAX_OUTSTANDING {
            eprintln!(
                "timberfs: forward-intake: {PARTIAL_MAX_OUTSTANDING} outstanding partial \
                 message ids reached; emitting {id} unbuffered"
            );
            return vec![(time_ms, with_trailing_newline(frag))];
        }

        let buf = self
            .buffers
            .entry(id.clone())
            .or_insert_with(|| PartialBuf {
                first_time_ms: time_ms,
                bytes: Vec::new(),
            });
        buf.bytes.extend_from_slice(&frag);

        if buf.bytes.len() > PARTIAL_MAX_BYTES {
            eprintln!(
                "timberfs: forward-intake: partial message {id} exceeded {PARTIAL_MAX_BYTES} \
                 bytes; emitting what's buffered and resetting it"
            );
            let buf = self.buffers.remove(&id).unwrap();
            return vec![(buf.first_time_ms, with_trailing_newline(buf.bytes))];
        }
        if is_last {
            let buf = self.buffers.remove(&id).unwrap();
            return vec![(buf.first_time_ms, with_trailing_newline(buf.bytes))];
        }
        Vec::new()
    }
}

// ---------------------------------------------------------------------
// The receiver. The directory of stores, their locks and the maintenance
// tick are the shared intake core; what is Forward's own is the ack.
// ---------------------------------------------------------------------

struct PendingAck {
    chunk: String,
    conn_id: u64,
    writer: TcpStream,
}

/// Chunk ids awaiting durability, per store. Under the intake's own lock
/// (as `extra`), because an ack may only be sent for what a sync that the
/// same lock covered has already made durable.
type AckMap = BTreeMap<String, Vec<PendingAck>>;
type Intake = crate::intake::Intake<AckMap>;

/// Per-store policy for the receiver, one field per CLI flag.
pub struct ForwardOpts {
    pub payload_key: String,
    pub retain: Option<String>,
    pub retain_size: Option<String>,
    pub index: bool,
    /// Mint a store for a never-seen tag. OFF by default: creation is the
    /// operator's decision (`timberfs create --wal`), not the network's —
    /// an unknown tag is refused, unacked, and logged once.
    pub auto_create: bool,
}

/// Seed a brand-new tag's `.bark` the same way every other writer declares
/// its properties up front: identity + lineage, the tag, the container
/// fields when the first record carried them, the declared retention/
/// index this receiver applies to everything it creates — and `"wal"`,
/// because the ack contract (durable before acked) is delivered by the
/// sap. Seeded BEFORE the store opens so the sap exists from entry one.
fn seed_bark(
    dir: &Path,
    name: &str,
    tag: &str,
    container_id: Option<&str>,
    container_name: Option<&str>,
    opts: &ForwardOpts,
) -> anyhow::Result<()> {
    let mut map = crate::bark::derived_map(None, "forward-intake");
    map.insert("tag".to_string(), Json::String(tag.to_string()));
    if let Some(id) = container_id {
        map.insert("container_id".to_string(), Json::String(id.to_string()));
    }
    if let Some(n) = container_name {
        map.insert("container_name".to_string(), Json::String(n.to_string()));
    }
    if let Some(r) = &opts.retain {
        map.insert("retain".to_string(), Json::String(r.clone()));
    }
    if let Some(r) = &opts.retain_size {
        map.insert("retain_size".to_string(), Json::String(r.clone()));
    }
    if opts.index {
        map.insert("index".to_string(), Json::Bool(true));
    }
    map.insert("wal".to_string(), Json::Bool(true));
    crate::bark::save(dir, name, &map)
}

/// Lazily create a tag's store on first use, seeding a brand-new one's
/// manifest with what this receiver knows about the tag. The locking,
/// the refusal of an unknown tag and the wal declaration are the shared
/// intake core's (see intake.rs); only the seed is Forward's own.
fn ensure_store(
    intake: &mut Intake,
    dir: &Path,
    name: &str,
    tag: &str,
    opts: &ForwardOpts,
    container_id: Option<&str>,
    container_name: Option<&str>,
) -> anyhow::Result<()> {
    crate::intake::ensure_store(
        intake,
        dir,
        name,
        &format!("unknown tag {tag:?}"),
        opts.auto_create,
        |dir, name| seed_bark(dir, name, tag, container_id, container_name, opts),
    )
}

/// Complete every pending ack for one store: make everything appended so
/// far durable, then send each `{"ack": id}`. With a live wal, durable =
/// one sap fsync — the cheap point of the whole design: chunks keep their
/// size/age cadence no matter how the sender acks, and every ack pending
/// when one fsync completes shares it (group commit — the store lock is
/// held across the sync, so anything in the list was appended before it).
/// Without a wal (degraded, e.g. ENOSPC at segment creation) fall back to
/// flushing + syncing the chunk itself: an ack must never outrun
/// durability. On failure the acks are put back — unacked and retried by
/// the sender and by the next tick's sweep, never sent on hope.
fn drain_acks(intake: &Mutex<Intake>, name: &str, cfg: &Config) {
    let acks = {
        let mut g = intake.lock().unwrap();
        let Some(acks) = g.extra.get_mut(name).map(std::mem::take) else {
            return;
        };
        if acks.is_empty() {
            return;
        }
        let durable = match g.store.files.get_mut(name) {
            Some(f) if f.has_wal() => f.sap_sync().is_ok(),
            Some(f) => f.flush_chunk(cfg).and_then(|()| f.sync(cfg)).is_ok(),
            None => false,
        };
        if !durable {
            g.extra.insert(name.to_string(), acks);
            return;
        }
        acks
    };
    for mut ack in acks {
        let msg = encode_ack(&ack.chunk);
        let _ = ack.writer.write_all(&msg);
    }
}

fn encode_ack(chunk: &str) -> Vec<u8> {
    let val = Value::Map(vec![(Value::from("ack"), Value::from(chunk))]);
    let mut buf = Vec::new();
    if let Err(e) = rmpv::encode::write_value(&mut buf, &val) {
        eprintln!("timberfs: forward-intake: encoding ack failed: {e}");
    }
    buf
}

fn is_clean_eof(e: &rmpv::decode::Error) -> bool {
    match e {
        rmpv::decode::Error::InvalidMarkerRead(e) | rmpv::decode::Error::InvalidDataRead(e) => {
            e.kind() == io::ErrorKind::UnexpectedEof
        }
        rmpv::decode::Error::DepthLimitExceeded => false,
    }
}

fn handle_connection(
    stream: TcpStream,
    intake: Arc<Mutex<Intake>>,
    dir: PathBuf,
    cfg: Config,
    opts: Arc<ForwardOpts>,
    conn_id: u64,
) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("timberfs: forward-intake: {peer}: cannot clone connection for acks: {e}");
            return;
        }
    };
    let mut reader = BufReader::new(stream);
    let mut reassembler = PartialReassembler::default();

    loop {
        let msg = match rmpv::decode::read_value(&mut reader) {
            Ok(v) => v,
            Err(e) => {
                if !is_clean_eof(&e) {
                    eprintln!(
                        "timberfs: forward-intake: {peer}: decode error ({e}); closing \
                         connection (a desynced msgpack stream is unrecoverable)"
                    );
                }
                break;
            }
        };
        let decoded = match decode_message(msg) {
            Ok(d) => d,
            Err(e @ DecodeError::Unsupported(_)) => {
                eprintln!("timberfs: forward-intake: {peer}: {e}; closing connection");
                break;
            }
            Err(e @ DecodeError::Invalid(_)) => {
                eprintln!(
                    "timberfs: forward-intake: {peer}: {e}; closing connection \
                     (a desynced msgpack stream is unrecoverable)"
                );
                break;
            }
        };
        let name = crate::intake::store_name(&decoded.tag);
        for (time_ms, record) in &decoded.entries {
            let container_id = record_str(record, "container_id").map(str::to_string);
            let container_name = record_str(record, "container_name").map(str::to_string);
            for (t, payload) in reassembler.feed(*time_ms, record, &opts.payload_key) {
                let mut g = intake.lock().unwrap();
                if let Err(e) = ensure_store(
                    &mut g,
                    &dir,
                    &name,
                    &decoded.tag,
                    &opts,
                    container_id.as_deref(),
                    container_name.as_deref(),
                ) {
                    if g.refused.insert(name.clone()) {
                        eprintln!("timberfs: forward-intake: {name}: {e}");
                    }
                    continue;
                }
                if let Some(f) = g.store.files.get_mut(&name) {
                    if let Err(e) = f.append_windowed(&payload, t, t, &cfg) {
                        eprintln!("timberfs: forward-intake: {name}: append failed: {e}");
                    }
                }
            }
        }
        if let Some(chunk) = decoded.chunk {
            // Never ack what has no store (refused tag, open failure):
            // the missing ack is the refusal's honest wire form — an
            // acking sender buffers and retries until the operator
            // creates the store, and then converges with nothing lost.
            if !intake.lock().unwrap().store.files.contains_key(&name) {
                continue;
            }
            match writer.try_clone() {
                Ok(w) => {
                    intake
                        .lock()
                        .unwrap()
                        .extra
                        .entry(name.clone())
                        .or_default()
                        .push(PendingAck {
                            chunk,
                            conn_id,
                            writer: w,
                        });
                    // Eagerly, not on the next tick: a blocking sender
                    // waits for this ack before its next message, so ack
                    // latency IS its throughput.
                    drain_acks(&intake, &name, &cfg);
                }
                Err(e) => {
                    eprintln!(
                        "timberfs: forward-intake: {peer}: cannot clone connection for an \
                         ack ({e}); the sender will retry the chunk"
                    );
                }
            }
        }
    }

    // EOF or a decode error: this connection's outstanding acks can never
    // be sent (the pending_acks list may be shared with other connections
    // writing the same tag, so only drop entries that are ours).
    let mut g = intake.lock().unwrap();
    for acks in g.extra.values_mut() {
        acks.retain(|a| a.conn_id != conn_id);
    }
}

/// `timberfs forward-intake`: receive Fluentd Forward protocol v1 over TCP
/// and write each tag into its own store under `into_dir`. See the module
/// doc comment for the supported subset and deliberate limitations.
pub fn cmd_forward_intake(
    listen: &str,
    into_dir: &Path,
    opts: ForwardOpts,
    exit_on_upgrade: bool,
) -> anyhow::Result<()> {
    opts.retain
        .as_deref()
        .map(crate::append::parse_duration_ms)
        .transpose()?;
    opts.retain_size
        .as_deref()
        .map(crate::append::parse_size_bytes)
        .transpose()?;
    let _dir_lock = crate::intake::open_backing_dir(into_dir)?;

    let cfg = Config {
        chunk_size: 256 * 1024,
        level: 3,
        flush_age_ms: 5000,
    };
    let intake = Arc::new(Mutex::new(Intake::new(into_dir, cfg, AckMap::new())));
    let opts = Arc::new(opts);

    crate::append::install_signal_handlers();

    let stop = Arc::new(AtomicBool::new(false));
    // The shared maintenance tick, plus what only Forward owes: acks are
    // completed eagerly in the connection threads (drain_acks after each
    // registered chunk), so this sweep only retries ones a transient
    // failure left pending there.
    let maint = crate::intake::spawn_maintenance(
        Arc::clone(&intake),
        into_dir.to_path_buf(),
        Arc::clone(&stop),
        exit_on_upgrade,
        move |intake, names| {
            for name in names {
                drain_acks(intake, name, &cfg);
            }
        },
    );

    let listener = match crate::intake::socket_activated_listener() {
        Some(l) => l,
        None => TcpListener::bind(listen)
            .with_context(|| format!("binding forward-intake listener on {listen}"))?,
    };
    eprintln!(
        "timberfs: forward-intake listening on {} -> {}",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| listen.to_string()),
        into_dir.display()
    );

    let conn_counter = Arc::new(AtomicU64::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("timberfs: forward-intake: accept failed: {e}");
                continue;
            }
        };
        let intake = Arc::clone(&intake);
        let dir = into_dir.to_path_buf();
        let opts = Arc::clone(&opts);
        let conn_id = conn_counter.fetch_add(1, Ordering::Relaxed);
        thread::spawn(move || handle_connection(stream, intake, dir, cfg, opts, conn_id));
    }

    stop.store(true, Ordering::Relaxed);
    let _ = maint.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::encode::write_value;

    fn encode(v: &Value) -> Vec<u8> {
        let mut buf = Vec::new();
        write_value(&mut buf, v).unwrap();
        buf
    }

    fn decode_bytes(bytes: &[u8]) -> Value {
        let mut cursor = io::Cursor::new(bytes);
        rmpv::decode::read_value(&mut cursor).unwrap()
    }

    fn record(pairs: &[(&str, &str)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (Value::from(*k), Value::from(*v)))
                .collect(),
        )
    }

    #[test]
    fn message_mode_decodes() {
        let msg = Value::Array(vec![
            Value::from("app.log"),
            Value::from(1_700_000_000u64),
            record(&[("log", "hello")]),
        ]);
        let decoded = decode_message(msg).ok().unwrap();
        assert_eq!(decoded.tag, "app.log");
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].0, 1_700_000_000_000);
        assert_eq!(record_str(&decoded.entries[0].1, "log"), Some("hello"));
        assert!(decoded.chunk.is_none());
    }

    #[test]
    fn forward_mode_decodes_batch() {
        let msg = Value::Array(vec![
            Value::from("app.log"),
            Value::Array(vec![
                Value::Array(vec![Value::from(100u64), record(&[("log", "one")])]),
                Value::Array(vec![Value::from(200u64), record(&[("log", "two")])]),
            ]),
        ]);
        let decoded = decode_message(msg).ok().unwrap();
        assert_eq!(decoded.entries.len(), 2);
        assert_eq!(decoded.entries[0].0, 100_000);
        assert_eq!(record_str(&decoded.entries[0].1, "log"), Some("one"));
        assert_eq!(decoded.entries[1].0, 200_000);
        assert_eq!(record_str(&decoded.entries[1].1, "log"), Some("two"));
    }

    /// A raw msgpack "str" header + bytes, for framing a blob whose content
    /// is NOT necessarily valid UTF-8 (msgpack-encoded bytes routinely
    /// aren't) — `rmpv::Value::String` has no public constructor for that,
    /// only the real decoder can produce one, so this builds the wire bytes
    /// by hand and decodes them for real, exactly like a live connection.
    fn msgpack_raw_str(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let len = bytes.len();
        if len <= 31 {
            out.push(0xa0 | len as u8);
        } else if len <= 255 {
            out.push(0xd9);
            out.push(len as u8);
        } else {
            out.push(0xda);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        out.extend_from_slice(bytes);
        out
    }

    fn msgpack_fixarray(n: usize) -> Vec<u8> {
        vec![0x90 | n as u8]
    }

    #[test]
    fn packed_forward_str_blob_decodes() {
        let mut blob = Vec::new();
        blob.extend(encode(&Value::Array(vec![
            Value::from(1u64),
            record(&[("log", "a")]),
        ])));
        blob.extend(encode(&Value::Array(vec![
            Value::from(2u64),
            record(&[("log", "b")]),
        ])));
        let mut wire = msgpack_fixarray(2);
        wire.extend(encode(&Value::from("app.log")));
        wire.extend(msgpack_raw_str(&blob));
        let msg = decode_bytes(&wire);
        let decoded = decode_message(msg).ok().unwrap();
        assert_eq!(decoded.entries.len(), 2);
        assert_eq!(record_str(&decoded.entries[0].1, "log"), Some("a"));
        assert_eq!(record_str(&decoded.entries[1].1, "log"), Some("b"));
    }

    #[test]
    fn packed_forward_bin_blob_decodes() {
        let mut blob = Vec::new();
        blob.extend(encode(&Value::Array(vec![
            Value::from(1u64),
            record(&[("log", "a")]),
        ])));
        let msg = Value::Array(vec![Value::from("app.log"), Value::Binary(blob)]);
        let decoded = decode_message(msg).ok().unwrap();
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(record_str(&decoded.entries[0].1, "log"), Some("a"));
    }

    #[test]
    fn event_time_ext_decodes() {
        let mut data = Vec::new();
        data.extend_from_slice(&1_700_000_000u32.to_be_bytes());
        data.extend_from_slice(&500_000_000u32.to_be_bytes());
        let msg = Value::Array(vec![
            Value::from("app.log"),
            Value::Ext(0, data),
            record(&[("log", "hi")]),
        ]);
        let decoded = decode_message(msg).ok().unwrap();
        assert_eq!(decoded.entries[0].0, 1_700_000_000_500);
    }

    #[test]
    fn float_time_tolerated() {
        let msg = Value::Array(vec![
            Value::from("app.log"),
            Value::F64(1_700_000_000.25),
            record(&[("log", "hi")]),
        ]);
        let decoded = decode_message(msg).ok().unwrap();
        assert_eq!(decoded.entries[0].0, 1_700_000_000_250);
    }

    #[test]
    fn zero_time_falls_back_to_now() {
        let before = store::now_ms();
        let msg = Value::Array(vec![
            Value::from("app.log"),
            Value::from(0u64),
            record(&[("log", "hi")]),
        ]);
        let decoded = decode_message(msg).ok().unwrap();
        assert!(decoded.entries[0].0 >= before);
    }

    #[test]
    fn ack_chunk_extracted_from_message_mode() {
        let option = Value::Map(vec![(Value::from("chunk"), Value::from("abc123"))]);
        let msg = Value::Array(vec![
            Value::from("app.log"),
            Value::from(1u64),
            record(&[("log", "hi")]),
            option,
        ]);
        let decoded = decode_message(msg).ok().unwrap();
        assert_eq!(decoded.chunk.as_deref(), Some("abc123"));
    }

    #[test]
    fn ack_chunk_extracted_from_forward_mode() {
        let option = Value::Map(vec![(Value::from("chunk"), Value::from("xyz"))]);
        let msg = Value::Array(vec![
            Value::from("app.log"),
            Value::Array(vec![Value::Array(vec![
                Value::from(1u64),
                record(&[("log", "hi")]),
            ])]),
            option,
        ]);
        let decoded = decode_message(msg).ok().unwrap();
        assert_eq!(decoded.chunk.as_deref(), Some("xyz"));
    }

    #[test]
    fn gzip_packed_forward_rejected() {
        let option = Value::Map(vec![(Value::from("compressed"), Value::from("gzip"))]);
        let msg = Value::Array(vec![
            Value::from("app.log"),
            Value::Binary(vec![1, 2, 3]),
            option,
        ]);
        match decode_message(msg) {
            Err(DecodeError::Unsupported(what)) => assert!(what.contains("gzip")),
            _ => panic!("expected Unsupported"),
        }
    }

    #[test]
    fn round_trip_through_the_wire() {
        let msg = Value::Array(vec![
            Value::from("wiretest"),
            Value::from(42u64),
            record(&[("log", "round-trip")]),
        ]);
        let bytes = encode(&msg);
        let decoded_val = decode_bytes(&bytes);
        let decoded = decode_message(decoded_val).ok().unwrap();
        assert_eq!(decoded.tag, "wiretest");
        assert_eq!(record_str(&decoded.entries[0].1, "log"), Some("round-trip"));
    }

    #[test]
    fn payload_key_fallback_to_json() {
        let rec = record(&[("source", "stdout")]);
        let bytes = payload_no_newline(&rec, "log");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["source"], "stdout");
    }

    #[test]
    fn payload_key_present_uses_it_directly() {
        let rec = record(&[("log", "the actual line"), ("source", "stdout")]);
        let bytes = payload_no_newline(&rec, "log");
        assert_eq!(bytes, b"the actual line");
    }

    #[test]
    fn payload_gets_exactly_one_trailing_newline() {
        assert_eq!(with_trailing_newline(b"abc".to_vec()), b"abc\n");
        assert_eq!(with_trailing_newline(b"abc\n".to_vec()), b"abc\n");
    }

    fn partial(id: &str, ordinal: &str, last: bool, log: &str) -> Value {
        record(&[
            ("partial_message", "true"),
            ("partial_id", id),
            ("partial_ordinal", ordinal),
            ("partial_last", if last { "true" } else { "false" }),
            ("log", log),
        ])
    }

    #[test]
    fn partial_reassembly_merges_in_arrival_order() {
        let mut r = PartialReassembler::default();
        assert!(r
            .feed(100, &partial("p1", "0", false, "hello "), "log")
            .is_empty());
        assert!(r
            .feed(101, &partial("p1", "1", false, "cruel "), "log")
            .is_empty());
        let out = r.feed(102, &partial("p1", "2", true, "world"), "log");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 100);
        assert_eq!(out[0].1, b"hello cruel world\n");
    }

    #[test]
    fn partial_reassembly_tracks_independent_ids() {
        let mut r = PartialReassembler::default();
        assert!(r.feed(1, &partial("a", "0", false, "A1"), "log").is_empty());
        assert!(r.feed(2, &partial("b", "0", false, "B1"), "log").is_empty());
        let out_a = r.feed(3, &partial("a", "1", true, "A2"), "log");
        assert_eq!(out_a[0].1, b"A1A2\n");
        let out_b = r.feed(4, &partial("b", "1", true, "B2"), "log");
        assert_eq!(out_b[0].1, b"B1B2\n");
    }

    #[test]
    fn non_partial_entries_pass_through_immediately() {
        let mut r = PartialReassembler::default();
        let out = r.feed(1, &record(&[("log", "plain line")]), "log");
        assert_eq!(out, vec![(1, b"plain line\n".to_vec())]);
    }

    #[test]
    fn partial_guard_caps_buffer_size() {
        let mut r = PartialReassembler::default();
        let big = "x".repeat(PARTIAL_MAX_BYTES + 1);
        let out = r.feed(1, &partial("huge", "0", false, &big), "log");
        // Over the cap on the very first fragment: emitted immediately,
        // with the id reset (not left dangling in the map).
        assert_eq!(out.len(), 1);
        assert!(!r.buffers.contains_key("huge"));
    }

    #[test]
    fn partial_guard_caps_outstanding_ids() {
        let mut r = PartialReassembler::default();
        for i in 0..PARTIAL_MAX_OUTSTANDING {
            let id = format!("id{i}");
            assert!(r.feed(1, &partial(&id, "0", false, "x"), "log").is_empty());
        }
        assert_eq!(r.buffers.len(), PARTIAL_MAX_OUTSTANDING);
        // The next NEW id can't get a slot: emitted unbuffered instead of
        // growing past the cap.
        let out = r.feed(1, &partial("one-too-many", "0", false, "y"), "log");
        assert_eq!(out.len(), 1);
        assert!(!r.buffers.contains_key("one-too-many"));
        assert_eq!(r.buffers.len(), PARTIAL_MAX_OUTSTANDING);
    }
}
