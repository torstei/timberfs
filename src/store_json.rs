//! ONE JSON shape for a store, for every surface that emits one.
//!
//! `info --json` writes this object, `list --json` an array of it, and the
//! query document's `kind: "stores"` the same array. There is no per-surface
//! projection, because a projection is how a third arrangement of the same
//! facts appears next time.
//!
//! It is not a hypothetical worry. Before this existed the two surfaces had
//! 39 distinct keys between them and shared 10, with the same data under
//! different names (`labels`/`provenance`, `size_bytes`/`compressed_bytes`,
//! `from_ms`/`first_write_ms`) — and worse, `name` carried a DIFFERENT VALUE
//! in each: the file's name in one, what the store calls itself in the other.
//! A VM test existed whose whole job was to assert that two of those names
//! held equal values.
//!
//! Absent means ABSENT: an optional field is omitted rather than written as
//! null, so a consumer tests for the key's presence and the schema marks it
//! not-required. One rule, stated once, rather than per field.

use serde::Serialize;

use crate::query::{StoreSummary, WriterState};

/// Where a store was found. Facts about the lookup rather than about the
/// store — `info` knows them too, so it reports them too.
pub struct Location {
    /// The forest it was found in, or the directory itself for an ad-hoc one.
    pub forest: Option<String>,
    /// Its short name within a forest: the file name minus `.rings`/`.log`.
    pub handle: String,
    pub dir: String,
    pub path: String,
    pub kind: Kind,
    /// Size of the bundle file, for `kind: "bundle"`.
    pub bundle_bytes: Option<u64>,
}

/// What the thing on disk IS. A `.timber` bundle is a snapshot, so the
/// questions a live pair answers — who is writing it, who is following it
/// — do not arise, and their fields are ABSENT rather than answered with
/// an empty one. Absent is "not applicable"; empty is "none".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A backing pair: `.trunk` + `.rings`, plus sidecars.
    Pair,
    /// A `.timber` transfer bundle.
    Bundle,
}

/// A store, as JSON.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Store {
    // ---- identity -------------------------------------------------------
    /// The store's id. Absent only for a store that has no manifest, which
    /// a plain `append` produces; every other surface treats that as broken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// What the store CALLS ITSELF: its declared name, or its handle when it
    /// declares none. Never the file's name — that is in `path`, and a store
    /// whose path is opaque has a readable name only here.
    pub name: String,
    /// The path's own word for it, which an opaque path makes a uuid.
    pub handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// The store these entries came FROM, one hop back. A field rather than
    /// a label: selecting on it would be selecting on one hop's bookkeeping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_id: Option<String>,

    // ---- where it was found ---------------------------------------------
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forest: Option<String>,
    pub dir: String,
    pub path: String,
    pub kind: Kind,

    /// The manifest's provenance, verbatim — what a fleet view selects on.
    /// Nested so a free-form key can never collide with a field of this
    /// object.
    pub labels: serde_json::Map<String, serde_json::Value>,

    // ---- what it holds ---------------------------------------------------
    pub chunks: usize,
    /// Uncompressed. What a reader will actually get.
    pub logical_bytes: u64,
    /// On disk.
    pub compressed_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_write_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_write_ms: Option<u64>,

    // ---- numbering and what has left it ----------------------------------
    /// The oldest chunk number still held, and the newest. Absent for a
    /// store holding nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
    /// The number the next chunk will take. Never restarts, so it is the
    /// lifetime count of chunks this store has ever held.
    pub next_seq: u64,
    pub dropped_chunks: u64,
    pub dropped_bytes: u64,
    pub dropped_uncompressed_bytes: u64,

    // ---- sidecars ---------------------------------------------------------
    /// Whether the manifest DECLARES a token index — which is not the same
    /// as holding one. `grain_chunks` says how much of the store one
    /// actually covers, and the two were previously flattened into a single
    /// `indexed` boolean that could not tell them apart.
    pub index_declared: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grain_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grain_chunks: Option<usize>,
    pub wal_declared: bool,
    /// Bytes in the write-ahead sidecar not yet in a sealed chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sap_pending_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rings_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_bytes: Option<u64>,

    // ---- declared policy ---------------------------------------------------
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retain_size: Option<String>,
    pub retain_unconsumed: bool,

    // ---- lineage, for a store some other command derived -------------------
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_op: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,

    // ---- who is touching it -------------------------------------------------
    /// The writer holding this store, ABSENT when nobody does. Presence is
    /// liveness — there is no separate boolean to disagree with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writer: Option<String>,
    /// Registered followers: empty means none are registered, because the
    /// registry knows every follower of every store. ABSENT for a bundle,
    /// where the question does not arise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub followers: Option<Vec<serde_json::Value>>,
    /// The declared cursor directory, and what stands in it. Absent means no
    /// directory is declared, which is not the same as one declared with
    /// nobody reading — that is an empty `consumers`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursors_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumers: Option<serde_json::Value>,
    /// Bytes the slowest consumer is holding back from retention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_bytes: Option<u64>,
    /// Cursor files that could not be read. Absent means none were.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursors_unreadable: Option<usize>,
    /// Marks the `cursors_*` block as superseded by `followers`. Always
    /// `true` when present, so it carries nothing `consumers` does not —
    /// kept because removing it is a separate decision from unifying the
    /// shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursors_superseded: Option<bool>,
}

/// How a writer is named in JSON. `Idle` has no name — the field is simply
/// absent, so `writer` present IS "somebody holds it".
fn writer_text(w: &WriterState) -> Option<String> {
    match w {
        WriterState::Mounted(Some(p)) => Some(format!("mount {}", p.display())),
        WriterState::Mounted(None) => Some("mount".to_string()),
        WriterState::Active(Some(d)) => Some(d.clone()),
        WriterState::Active(None) => Some("active".to_string()),
        WriterState::Unreadable => Some("unreadable".to_string()),
        WriterState::Idle => None,
    }
}

impl Store {
    pub fn new(s: &StoreSummary, loc: &Location) -> Store {
        let (first_seq, last_seq) = match s.chunk_seq {
            Some((a, b)) => (Some(a), Some(b)),
            None => (None, None),
        };
        Store {
            id: s.id.clone(),
            name: s
                .declared_name
                .clone()
                .unwrap_or_else(|| loc.handle.clone()),
            handle: loc.handle.clone(),
            created: s.created.clone(),
            origin_id: s.origin_id.clone(),
            forest: loc.forest.clone(),
            dir: loc.dir.clone(),
            path: loc.path.clone(),
            kind: loc.kind,
            labels: s.labels.clone(),
            chunks: s.chunks,
            logical_bytes: s.logical_bytes,
            compressed_bytes: s.compressed_bytes,
            first_write_ms: s.first_write_ms,
            last_write_ms: s.last_write_ms,
            first_seq,
            last_seq,
            next_seq: s.next_seq,
            dropped_chunks: s.dropped_chunks(),
            dropped_bytes: s.dropped.comp_bytes,
            dropped_uncompressed_bytes: s.dropped.uncomp_bytes,
            index_declared: s.index_declared,
            grain_bytes: s.grain.map(|(b, _)| b),
            grain_chunks: s.grain.map(|(_, n)| n),
            wal_declared: s.wal_declared,
            sap_pending_bytes: s.sap_pending_bytes,
            rings_bytes: (s.rings_bytes > 0).then_some(s.rings_bytes),
            bundle_bytes: loc.bundle_bytes,
            retain: s.retain.clone(),
            retain_size: s.retain_size.clone(),
            retain_unconsumed: s.retain_unconsumed,
            derived_from: s.derived_from.clone(),
            derived_op: s.derived_op.clone(),
            window_from: s.window_from.clone(),
            window_to: s.window_to.clone(),
            command: s.command.clone(),
            pattern: s.pattern.clone(),
            writer: writer_text(&s.writer),
            followers: (loc.kind == Kind::Pair)
                .then(|| s.followers.iter().map(crate::follower::to_json).collect()),
            cursors_dir: s.consumers.as_ref().map(|sv| sv.dir.display().to_string()),
            consumers: s.consumers.as_ref().map(crate::query::consumers_json),
            held_bytes: s.consumers.as_ref().map(|sv| sv.held_bytes()),
            cursors_unreadable: s
                .consumers
                .as_ref()
                .and_then(|sv| (sv.unreadable > 0).then_some(sv.unreadable)),
            cursors_superseded: s.consumers.as_ref().map(|_| true),
        }
    }
}
