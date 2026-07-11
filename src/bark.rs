//! `.bark`: the log's manifest — declared properties and provenance, as
//! one flat, optional, human-editable JSON object next to the pair:
//!
//!     {"index": true, "host": "foo.bar.com", "path": "/var/log/app.log"}
//!
//! Unlike `.grain` (derived, rebuildable, dropped on rings rewrites),
//! bark is DECLARED: it survives head-drops (provenance and settings
//! don't change when old chunks are retained away), travels on rename,
//! and ships inside `.timber` bundles. Well-known key so far:
//!
//!   "index": true  — the CREATE INDEX declaration. Writers maintain the
//!   .grain automatically: imports extend it for new chunks and rebuild
//!   it when it is missing (e.g. after rotation/retention dropped it).
//!
//! Every manifest is minted with a durable identity on first write:
//! "id" (a random UUID — constant across renames, moves and hosts; the
//! identity of the STORE, where paths are merely its current address)
//! and "created" (RFC3339, when the identity was established).
//!
//! Unknown keys are preserved untouched — bark is a label, not a schema.
//!
//! `timberfs create --index --set host=foo ... DEST` creates an empty log
//! with its properties declared up front, database-style.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};

use crate::format;
use crate::query::{ensure_dest_is_not_plain_file, resolve_backing};
use crate::store;

pub fn load(dir: &Path, name: &str) -> Option<Map<String, Value>> {
    let text = fs::read_to_string(format::bark_path(dir, name)).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => {
            eprintln!(
                "timberfs: warning: {} is not a JSON object; ignoring it",
                format::bark_path(dir, name).display()
            );
            None
        }
    }
}

/// A random UUIDv4, dependency-free (we are Linux-only anyway).
fn new_uuid() -> anyhow::Result<String> {
    let mut b = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
    ))
}

/// Every store gets a durable identity the first time a manifest is
/// written, whichever path writes it: "id" stays constant across renames,
/// moves and hosts (paths change, identity does not), and "created"
/// records when the identity was established. Once present, neither is
/// ever touched.
pub fn with_identity(mut map: Map<String, Value>) -> anyhow::Result<Map<String, Value>> {
    if !map.contains_key("id") {
        map.insert("id".to_string(), Value::String(new_uuid()?));
    }
    if !map.contains_key("created") {
        map.insert(
            "created".to_string(),
            Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
        );
    }
    Ok(map)
}

pub fn save(dir: &Path, name: &str, map: &Map<String, Value>) -> anyhow::Result<()> {
    let map = with_identity(map.clone())?;
    let text = serde_json::to_string_pretty(&Value::Object(map))?;
    fs::write(format::bark_path(dir, name), text + "\n")
        .with_context(|| format!("writing {}", format::bark_path(dir, name).display()))?;
    Ok(())
}

/// Is the index declared for this log?
pub fn index_declared(dir: &Path, name: &str) -> bool {
    load(dir, name)
        .and_then(|m| m.get("index").cloned())
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Persist `"index": true` (creating the bark if needed). Used by
/// `create --index`, `import --index` and `reindex`, so any road into an
/// indexed log converges on the same declared state.
pub fn declare_index(dir: &Path, name: &str) -> anyhow::Result<()> {
    let mut map = load(dir, name).unwrap_or_default();
    if map.get("index").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(());
    }
    map.insert("index".to_string(), Value::Bool(true));
    save(dir, name, &map)
}

/// Reserved keys that never inherit into a derived artifact: fresh
/// identity and lineage are written instead, and settings ("index") are
/// per-store operational choices (a read-only bundle cannot maintain a
/// grain). Everything else — host, path, format, user keys — is data
/// provenance and inherits: the lines survive extraction unchanged.
const NON_INHERITED: &[&str] = &[
    "id",
    "created",
    "derived_from",
    "derived_op",
    "window_from",
    "window_to",
    "index",
];

/// Window bounds are operation facts, recorded as RFC3339 UTC.
pub fn ms_rfc3339(ms: u64) -> String {
    DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| ms.to_string())
}

/// The bark for an artifact derived from `source_bark` by `op`
/// ("rotate"/"export"): new identity (minted by save), lineage pointer
/// when the source is identified, inherited provenance. Content facts —
/// actual spans, sizes — are NOT recorded (the artifact's own rings state
/// those authoritatively); the REQUESTED window is different: it is a
/// fact about the operation, like derived_op, and callers add it as
/// window_from/window_to. Content can never state coverage — an artifact
/// whose last line is 17:00 doesn't say whether 17:00-24:00 was
/// covered-but-silent or not covered — and for an EMPTY artifact the
/// declared window is the entire meaning ("I cover Saturday, I contain
/// nothing").
pub fn derived_map(source_bark: Option<&Map<String, Value>>, op: &str) -> Map<String, Value> {
    let mut map = Map::new();
    if let Some(src) = source_bark {
        for (k, v) in src {
            if !NON_INHERITED.contains(&k.as_str()) {
                map.insert(k.clone(), v.clone());
            }
        }
        if let Some(id) = src.get("id").and_then(|v| v.as_str()) {
            map.insert("derived_from".to_string(), Value::String(id.to_string()));
        }
    }
    map.insert("derived_op".to_string(), Value::String(op.to_string()));
    map
}

/// Rotate holds exclusive writer locks on its source, so it may mint the
/// source's identity when missing — every rotation then leaves a complete
/// lineage chain. (Export never writes its source: it is read-only.)
pub fn ensure_identified(dir: &Path, name: &str) -> anyhow::Result<Map<String, Value>> {
    let map = load(dir, name).unwrap_or_default();
    if map.get("id").and_then(|v| v.as_str()).is_some() {
        return Ok(map);
    }
    save(dir, name, &map)?; // save mints id + created
    load(dir, name).context("re-reading freshly minted manifest")
}

/// `timberfs create`: make an empty log with declared properties.
pub fn cmd_create(dest: &Path, index: bool, sets: &[String]) -> anyhow::Result<()> {
    ensure_dest_is_not_plain_file(dest, "create")?;
    let (dir, name) = resolve_backing(dest)?;
    fs::create_dir_all(&dir)?;
    if format::rings_path(&dir, &name).exists() || format::trunk_path(&dir, &name).exists() {
        bail!("{name} already exists in {}", dir.display());
    }
    let _dir_lock = store::lock_backing_shared(&dir)?.with_context(|| {
        format!(
            "backing directory {} is served by a timberfs mount",
            dir.display()
        )
    })?;
    let _file_lock = store::lock_file_exclusive(&dir, &name)?
        .with_context(|| format!("{name} already has a writer"))?;

    let mut map = Map::new();
    if index {
        map.insert("index".to_string(), Value::Bool(true));
    }
    for kv in sets {
        let Some((k, v)) = kv.split_once('=') else {
            bail!("--set wants key=value, got {kv:?}");
        };
        map.insert(k.trim().to_string(), Value::String(v.to_string()));
    }

    // The empty pair (rings header included), then the manifest.
    let mut st = store::Store {
        dir: dir.clone(),
        cfg: store::Config {
            chunk_size: 256 * 1024,
            level: 3,
            flush_age_ms: 5000,
        },
        files: std::collections::BTreeMap::new(),
    };
    st.create(&name)?;
    if !map.is_empty() {
        save(&dir, &name, &map)?;
    }
    eprintln!(
        "timberfs: created {}/{}{}{}",
        dir.display(),
        name,
        if index { " (indexed)" } else { "" },
        if map.is_empty() {
            String::new()
        } else {
            format!(
                " with manifest {}",
                format::bark_path(&dir, &name).display()
            )
        }
    );
    Ok(())
}
