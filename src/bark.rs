//! `.bark`: the log's manifest — declared properties and provenance, as
//! one flat, optional, human-editable JSON object next to the pair:
//!
//! ```text
//! {"index": true, "host": "foo.bar.com", "path": "/var/log/app.log"}
//! ```
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
pub fn new_uuid() -> anyhow::Result<String> {
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
    // Atomic (tmp + rename): live writers re-read the manifest on their
    // retention tick, and a torn read must be impossible.
    let path = format::bark_path(dir, name);
    let tmp = dir.join(format!("{name}.{}.tmp", format::BARK_EXT));
    fs::write(&tmp, text + "\n").with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Like `load`, but an EXISTING-yet-invalid manifest is an Err instead of
/// a warn-and-None — callers with retention at stake must distinguish
/// "no declaration" (fine: no limits) from "declaration unreadable"
/// (keep the last good policy; never silently drop to unbounded).
pub fn try_load(dir: &Path, name: &str) -> anyhow::Result<Option<Map<String, Value>>> {
    let path = format::bark_path(dir, name);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        _ => bail!("{} is not a JSON object", path.display()),
    }
}

/// A declared retention policy, parsed and validated. Absent keys mean
/// no limit on that axis; an entirely absent manifest means no limits at
/// all (a case file carved by grep has no business expiring).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Retention {
    pub max_age_ms: Option<u64>,
    pub max_comp_bytes: Option<u64>,
    /// `retain_unconsumed`: keep what this store's RETAINING followers
    /// have not read, on top of the other two axes. The polarity is
    /// deliberate — every `retain_*` key names what is KEPT, so
    /// `retain_consumed` would have read as exactly the opposite.
    ///
    /// Never a cap: it only ever holds MORE than age and size would, and
    /// `max_comp_bytes` is required alongside as the backstop. See
    /// `Store::enforce_retention`.
    pub unconsumed: bool,
}

impl Retention {
    pub fn is_some(&self) -> bool {
        self.max_age_ms.is_some() || self.max_comp_bytes.is_some() || self.unconsumed
    }
}

pub fn retention_from_map(map: &Map<String, Value>) -> anyhow::Result<Retention> {
    let get = |k: &str| -> anyhow::Result<Option<&str>> {
        match map.get(k) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(v) => bail!("\"{k}\" must be a string, got {v}"),
        }
    };
    let unconsumed = match map.get("retain_unconsumed") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(v) => bail!("\"retain_unconsumed\" must be true or false, got {v}"),
    };
    let retention = Retention {
        max_age_ms: get("retain")?
            .map(crate::append::parse_duration_ms)
            .transpose()?,
        max_comp_bytes: get("retain_size")?
            .map(crate::append::parse_size_bytes)
            .transpose()?,
        unconsumed,
    };
    // The backstop is not optional. Interest is additive, so without a
    // size budget one stalled follower pins the store until the disk
    // fills — which kills the PRODUCER, losing the newest data to protect
    // the oldest. An Err here means callers keep their last good policy
    // and warn, which is the right failure: never a silent drop to
    // unbounded, and never an unbounded hold either.
    if retention.unconsumed && retention.max_comp_bytes.is_none() {
        bail!(
            "\"retain_unconsumed\" needs a \"retain_size\" alongside it: interest only ever \
             holds MORE, so without a budget one stalled follower fills the disk and kills the \
             producer"
        );
    }
    Ok(retention)
}

/// Declared line-timestamp format — a CONTENT description (unlike
/// settings it inherits through derivation: an exported slice contains
/// the same lines in the same format). Consumed by the read path's
/// entry filtering and by import (flag-free exotic formats).
#[derive(Clone, Default)]
pub struct TimeFormat {
    pub regex: Option<String>,
    pub format: Option<String>,
    pub utc: bool,
}

pub fn time_format(map: Option<&Map<String, Value>>) -> TimeFormat {
    let Some(map) = map else {
        return TimeFormat::default();
    };
    let get = |k: &str| map.get(k).and_then(|v| v.as_str()).map(str::to_string);
    TimeFormat {
        regex: get("timestamp_regex"),
        format: get("timestamp_format"),
        utc: map
            .get("timestamp_utc")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// The store's declared retention. Err = a manifest exists but cannot be
/// read/parsed (the caller decides: warn + last-good, never unbounded).
pub fn declared_retention(dir: &Path, name: &str) -> anyhow::Result<Retention> {
    match try_load(dir, name)? {
        None => Ok(Retention::default()),
        Some(map) => retention_from_map(&map),
    }
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

/// Is the write-ahead sidecar (`.sap`) declared for this log? A property
/// of the STORE (like `index`), not of whoever happens to be writing it
/// right now: once declared, every streaming writer maintains and syncs
/// it with no flag of its own (`FileStore::open`, store.rs).
pub fn wal_declared(dir: &Path, name: &str) -> bool {
    load(dir, name)
        .and_then(|m| m.get("wal").cloned())
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Persist `"wal": true` (creating the bark if needed). Used by
/// `create --wal`, `append --wal` and `import --wal` — any road into a
/// wal-backed log converges on the same declared state.
pub fn declare_wal(dir: &Path, name: &str) -> anyhow::Result<()> {
    let mut map = load(dir, name).unwrap_or_default();
    if map.get("wal").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(());
    }
    map.insert("wal".to_string(), Value::Bool(true));
    save(dir, name, &map)
}

/// Keys that describe the STORE rather than its content: identity, lineage,
/// operational settings, and content-format declarations. Everything else in
/// a manifest is PROVENANCE — where the entries came from and what produced
/// them.
///
/// This is what counts as a LABEL: what `list` shows in that column, what
/// `frames` routes on, what a fleet view groups by. It is NOT a limit on
/// what a selector may match — `--select` matches the whole manifest, name
/// and settings included, because a rule that forbids asking a question
/// only because we filed the answer under a different heading is a rule
/// that helps nobody.
///
/// The split lives here because bark owns what its keys mean; views that
/// re-guessed it drifted, which is why `info` once showed `wal` as a label.
pub const NOT_PROVENANCE: &[&str] = &[
    "id",
    // What the store is CALLED. Rendered in its own column by `list` and
    // in its own line by `info`, so showing it among the labels would say
    // it twice — and it is not where the entries came from. Still fully
    // matchable: `--select` reads the manifest, not this list.
    "name",
    "created",
    "derived_from",
    "derived_op",
    "window_from",
    "window_to",
    "index",
    "wal",
    "retain",
    "retain_size",
    "retain_unconsumed",
    "cursors",
    "timestamp_regex",
    "timestamp_format",
    "timestamp_utc",
    "command",
    "pattern",
    // Lineage arriving over the wire, not provenance. `timber-otlp` sends
    // the ORIGIN store's id and path as OTLP resource attributes, and the
    // receiving intake seeds every attribute it is given — so these name
    // the store the entries came FROM. Selecting on them would be
    // selecting on one hop's bookkeeping, and under fan-in (several
    // senders routed into one store) they name only one of the origins,
    // which makes them actively wrong rather than merely useless.
    "timberfs.store.id",
    "timberfs.store.path",
    // Which routed value opened this store. One hop's bookkeeping, kept so
    // a SECOND value that sanitizes to the same store name is refused
    // rather than merged into it — see `intake::ensure_store`.
    ROUTED_FROM,
];

/// The routed value a store was opened by. Recorded because store names are
/// SANITIZED: `/` becomes `_`, so `checkout/v2` and `checkout_v2` produce
/// one name, and without this the second silently appends to the first's
/// store and the manifest describes only one of them.
pub const ROUTED_FROM: &str = "timberfs.routed_from";

/// The ORIGIN store's identity, when the entries arrived from another
/// timberfs store over the wire — `timber-otlp` sends it as an OTLP
/// resource attribute and the receiving intake seeds it.
///
/// ⚠ Trustworthy only where routing gives one store per origin. Under
/// fan-in it names whichever sender created the store, which is why the
/// receiving side's routing decides whether this means anything (see
/// ROADMAP, "Globally addressable chunks").
pub fn origin_id(map: &Map<String, Value>) -> Option<String> {
    map.get("timberfs.store.id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A manifest's provenance keys, in sorted order. The values are left
/// exactly as declared — flattening a dotted key like `service.name` is a
/// consumer's concern (Loki requires it, timberfs does not), and doing it
/// here would lose the key the operator actually wrote.
pub fn provenance(map: &Map<String, Value>) -> Map<String, Value> {
    map.iter()
        .filter(|(k, _)| !NOT_PROVENANCE.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
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
    "wal",
    "retain",
    "retain_size",
    "retain_unconsumed",
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

/// Which side of a disagreement to keep.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdentitySide {
    /// The `.rings` header — the backing pair, which IS the store.
    Index,
    /// The `.bark` manifest.
    Manifest,
}

/// What the two sides say about a store's identity.
fn identity_sides(dir: &Path, name: &str) -> anyhow::Result<(Option<String>, Option<String>)> {
    let manifest = try_load(dir, name)
        .with_context(|| {
            format!(
                "the existing manifest is unreadable — fix or remove {} first",
                format::bark_path(dir, name).display()
            )
        })?
        .unwrap_or_default()
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let index = fs::read(format::rings_path(dir, name))
        .ok()
        .and_then(|b| format::header_store_id(&b))
        .map(|id| format::uuid_text(&id));
    Ok((manifest, index))
}

/// Report or repair a store's identity. A store's id is not a setting, so
/// it is not `set`-table; but the three ways it can be BROKEN each have an
/// obvious intended fix, and an operator who knows which one applies needs
/// a way to say so. Without a flag this only reports, and exits non-zero
/// when the store is not in one consistent state — so it is also the check
/// a script runs.
pub fn cmd_identity(store: &Path, mint: bool, keep: Option<IdentitySide>) -> anyhow::Result<()> {
    let (dir, name) = resolve_backing(store)?;
    if !format::rings_path(&dir, &name).exists() {
        bail!("no timberfs log {name} in {}", dir.display());
    }
    if mint && keep.is_some() {
        bail!("--mint makes an identity where there is none; --keep chooses between two. Not both");
    }
    let (manifest, index) = identity_sides(&dir, &name)?;

    // Any write here touches the rings header, which a live writer also
    // rewrites (head-drop). Repair is not a thing to race.
    let _lock = if mint || keep.is_some() {
        Some(
            store::lock_file_exclusive(&dir, &name)?
                .ok_or_else(|| anyhow::anyhow!("{name} has a live writer — stop it first"))?,
        )
    } else {
        None
    };

    let chosen: Option<String> = match (mint, keep, &manifest, &index) {
        (true, _, None, None) => None, // mint below
        (true, _, _, _) => bail!(
            "--mint is for a store with no identity at all; this one already has {}",
            manifest.as_deref().or(index.as_deref()).unwrap_or("one")
        ),
        (_, Some(IdentitySide::Index), _, Some(id)) => Some(id.clone()),
        (_, Some(IdentitySide::Index), _, None) => {
            bail!("--keep index: the index carries no identity")
        }
        (_, Some(IdentitySide::Manifest), Some(id), _) => Some(id.clone()),
        (_, Some(IdentitySide::Manifest), None, _) => {
            bail!("--keep manifest: the manifest carries no identity")
        }
        (false, None, _, _) => {
            // Report only.
            println!("{name} — timberfs log in {}/", dir.display());
            println!("  manifest  {}", manifest.as_deref().unwrap_or("none"));
            println!("  index     {}", index.as_deref().unwrap_or("none"));
            let (verdict, ok) = match (&manifest, &index) {
                (Some(a), Some(b)) if a == b => ("consistent", true),
                (Some(_), Some(_)) => (
                    "DISAGREE — two identities for one store; \
                     pick one with --keep index or --keep manifest",
                    false,
                ),
                (Some(_), None) => (
                    "manifest only — the index is stamped on the next write",
                    true,
                ),
                (None, Some(_)) => (
                    "index only — `create --if-not-exists` recovers it into the manifest",
                    true,
                ),
                (None, None) => (
                    "NONE — this pair is not a store; make it one with --mint \
                     (or `create --if-not-exists`)",
                    false,
                ),
            };
            println!("  verdict   {verdict}");
            if !ok {
                std::process::exit(1);
            }
            return Ok(());
        }
    };

    let mut map = try_load(&dir, &name)?.unwrap_or_default();
    match chosen {
        // `save` mints identity for anything lacking one.
        None => {
            map.remove("id");
        }
        Some(id) => {
            map.insert("id".to_string(), Value::String(id));
        }
    }
    save(&dir, &name, &map)?;
    // Opening the store mirrors the manifest into the header — but only
    // where the header has none. A resolved DISAGREEMENT has to overwrite,
    // so stamp it here rather than relying on that.
    let settled = load(&dir, &name)
        .and_then(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .ok_or_else(|| anyhow::anyhow!("the manifest did not take an identity"))?;
    let bytes =
        format::uuid_bytes(&settled).ok_or_else(|| anyhow::anyhow!("{settled:?} is not a uuid"))?;
    let rings = fs::OpenOptions::new()
        .write(true)
        .open(format::rings_path(&dir, &name))?;
    std::os::unix::fs::FileExt::write_all_at(&rings, &bytes, format::STORE_ID_OFF as u64)?;
    rings.sync_all()?;
    crate::note!("timberfs: {name} identity is now {settled} (manifest and index agree)");
    Ok(())
}

/// Give an existing pair the identity it lacks, and report whether it
/// needed one. Nothing else about the store is touched: a declaration it
/// disagrees with is still left alone and warned about, because that is a
/// property someone chose, where a missing identity is a store that is not
/// yet a store.
fn mint_missing_identity(dir: &Path, name: &str) -> anyhow::Result<Option<&'static str>> {
    let existing = load(dir, name).unwrap_or_default();
    if existing.get("id").and_then(|v| v.as_str()).is_some() {
        return Ok(None);
    }
    // The pair may already carry one where the manifest was lost — the
    // pair is the store, so that is the identity, not a fresh mint.
    let rings = format::rings_path(dir, name);
    let carried = fs::read(&rings)
        .ok()
        .and_then(|b| format::header_store_id(&b))
        .map(|id| format::uuid_text(&id));
    let mut map = existing;
    let what = if let Some(id) = carried {
        map.insert("id".to_string(), Value::String(id));
        "recovered its identity from the index"
    } else {
        "had no identity; minted one"
    };
    // `save` mints identity for anything that still lacks one, and opening
    // the store mirrors it into the header.
    save(dir, name, &map)?;
    let mut st = store::Store {
        dir: dir.to_path_buf(),
        cfg: store::Config {
            chunk_size: 256 * 1024,
            level: 3,
            flush_age_ms: 5000,
        },
        files: std::collections::BTreeMap::new(),
    };
    st.create(name)?;
    Ok(Some(what))
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
#[allow(clippy::too_many_arguments)]
pub fn cmd_create(
    dest: &Path,
    index: bool,
    wal: bool,
    retain: Option<&str>,
    retain_size: Option<&str>,
    retain_unconsumed: bool,
    sets: &[String],
    if_not_exists: bool,
) -> anyhow::Result<()> {
    ensure_dest_is_not_plain_file(dest, "create")?;
    let (dir, name) = resolve_backing(dest)?;

    // Build (and thereby validate) the declaration before deciding
    // anything: a malformed --retain is an error even when the store is
    // already there and nothing will be written.
    let mut map = Map::new();
    if index {
        map.insert("index".to_string(), Value::Bool(true));
    }
    if wal {
        map.insert("wal".to_string(), Value::Bool(true));
    }
    if let Some(r) = retain {
        crate::append::parse_duration_ms(r)?;
        map.insert("retain".to_string(), Value::String(r.to_string()));
    }
    if let Some(r) = retain_size {
        crate::append::parse_size_bytes(r)?;
        map.insert("retain_size".to_string(), Value::String(r.to_string()));
    }
    if retain_unconsumed {
        map.insert("retain_unconsumed".to_string(), Value::Bool(true));
    }
    for kv in sets {
        let Some((k, v)) = kv.split_once('=') else {
            bail!("--set wants key=value, got {kv:?}");
        };
        let k = k.trim();
        if k == "cursors" {
            validate_cursors_dir(v)?;
        }
        map.insert(k.to_string(), declared_value(k, v)?);
    }
    // Same check `set` makes, on the same whole manifest: a declaration
    // that no writer could act on must fail before the store exists.
    retention_from_map(&map)?;

    fs::create_dir_all(&dir)?;
    if format::rings_path(&dir, &name).exists() || format::trunk_path(&dir, &name).exists() {
        if !if_not_exists {
            bail!("{name} already exists in {}", dir.display());
        }
        // CREATE IF NOT EXISTS: the store stands as it is, declaration
        // included — so say so when it declares something else.
        warn_declaration_drift(&dir, &name, &map);
        // ...but identity is not a declaration, it is what makes the pair
        // a store at all. A pair carrying none has not been created yet in
        // the only sense that matters, so CREATE IF NOT EXISTS creates the
        // missing part rather than reporting success at doing nothing.
        if let Some(what) = mint_missing_identity(&dir, &name)? {
            crate::note!("timberfs: {name} in {} {what}", dir.display());
            return Ok(());
        }
        crate::note!(
            "timberfs: {name} already exists in {}; nothing created",
            dir.display()
        );
        return Ok(());
    }
    let _dir_lock = store::lock_backing_shared(&dir)?.with_context(|| {
        format!(
            "backing directory {} is served by a timberfs mount",
            dir.display()
        )
    })?;
    let _file_lock = store::lock_file_exclusive(&dir, &name)?
        .with_context(|| format!("{name} already has a writer"))?;

    // The manifest FIRST, then the empty pair: the rings header mirrors
    // the store's identity, and `FileStore::open` reads it from the
    // manifest — so a store created with anything declared carries its id
    // in the pair from the very first byte, rather than from its first
    // write. (A bare `create` declares nothing, writes no manifest, and so
    // still has no identity; that is a separate question.)
    if !map.is_empty() {
        save(&dir, &name, &map)?;
    }
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
    crate::note!(
        "timberfs: created {}/{}{}{}{}",
        dir.display(),
        name,
        if index { " (indexed)" } else { "" },
        if wal { " (wal)" } else { "" },
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

/// A skipped `create --if-not-exists` writes nothing, so anything it
/// declared differently from the standing manifest is a discrepancy the
/// operator has to know about — silence would read as "declared".
fn warn_declaration_drift(dir: &Path, name: &str, declared: &Map<String, Value>) {
    if declared.is_empty() {
        return;
    }
    let existing = load(dir, name).unwrap_or_default();
    let render = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let drift: Vec<String> = declared
        .iter()
        .filter(|(k, v)| existing.get(*k) != Some(*v))
        .map(|(k, v)| match existing.get(k) {
            Some(cur) => format!("{k} is {}, not {}", render(cur), render(v)),
            None => format!("{k} is undeclared, not {}", render(v)),
        })
        .collect();
    if !drift.is_empty() {
        eprintln!(
            "timberfs: warning: {name} already exists and {} — left as it is \
             (timberfs set to change it)",
            drift.join(", ")
        );
    }
}

/// Identity and lineage are facts, not settings — never user-settable.
const PROTECTED: &[&str] = &["id", "created", "derived_from", "derived_op"];

/// Keys whose value is a JSON boolean, not a string. Shared by `create
/// --set` and `set` so the two spell the same manifest: writing
/// `"index": "true"` produces a key every reader evaluates as FALSE, which
/// is the worst kind of wrong — silently declared and silently ignored.
const BOOLEAN_KEYS: &[&str] = &["index", "wal", "timestamp_utc", "retain_unconsumed"];

/// A `KEY=VALUE` from the command line, as the manifest should hold it.
fn declared_value(k: &str, v: &str) -> anyhow::Result<Value> {
    if !BOOLEAN_KEYS.contains(&k) {
        return Ok(Value::String(v.to_string()));
    }
    match v {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        _ => bail!("\"{k}\" is true or false"),
    }
}

/// `cursors`: the directory this store's consumers keep their positions
/// in. Validated at declaration time because a wrong value is SILENT —
/// the store simply appears to have no consumers, which is also the
/// state in which nothing holds retention back. Absolute is required: a
/// relative path resolves against the cwd of whichever daemon reads it
/// next. A missing directory is only a warning — systemd creates a
/// `StateDirectory=` at unit start, which can be after the store is
/// declared.
fn validate_cursors_dir(v: &str) -> anyhow::Result<()> {
    let path = Path::new(v);
    if !path.is_absolute() {
        bail!("\"cursors\" must be an absolute path (it is read by daemons, from any cwd)");
    }
    if !path.is_dir() {
        eprintln!(
            "timberfs: warning: {v} is not a directory (yet) — nothing will be \
             found there until it exists"
        );
    }
    Ok(())
}

/// `timberfs set`: declare or change a store's properties in its manifest
/// — validated and atomic, which hand-editing the JSON is not. Known
/// settings are parse-checked (retain/retain_size/index); everything else
/// is free-form provenance. Works on live stores: writers re-read the
/// manifest on their retention tick, so a change takes effect within
/// seconds, no restart.
pub fn cmd_set(store: &Path, sets: &[String], unsets: &[String]) -> anyhow::Result<()> {
    if crate::query::is_bundle(store) {
        bail!(
            "{} is a .timber transfer bundle — bundles are read-only",
            store.display()
        );
    }
    if sets.is_empty() && unsets.is_empty() {
        bail!("nothing to do — give KEY=VALUE to set, or --unset KEY");
    }
    let (dir, name) = resolve_backing(store)?;
    if !format::rings_path(&dir, &name).exists() {
        bail!("no timberfs log {name} in {}", dir.display());
    }
    let mut map = try_load(&dir, &name)
        .with_context(|| {
            format!(
                "the existing manifest is unreadable — fix or remove {} first \
                 (rewriting it here would mint a NEW identity)",
                format::bark_path(&dir, &name).display()
            )
        })?
        .unwrap_or_default();

    for kv in sets {
        let Some((k, v)) = kv.split_once('=') else {
            bail!("set wants KEY=VALUE, got {kv:?}");
        };
        let (k, v) = (k.trim(), v.to_string());
        if PROTECTED.contains(&k) {
            bail!("\"{k}\" is identity/lineage — a fact, not a setting");
        }
        let value = match k {
            "retain" => {
                crate::append::parse_duration_ms(&v)?;
                Value::String(v)
            }
            "retain_size" => {
                crate::append::parse_size_bytes(&v)?;
                Value::String(v)
            }
            _ if BOOLEAN_KEYS.contains(&k) => declared_value(k, &v)?,
            "timestamp_regex" => {
                let re = regex::Regex::new(&v)
                    .with_context(|| "\"timestamp_regex\" does not compile".to_string())?;
                if re.captures_len() < 2 {
                    bail!("\"timestamp_regex\" needs one capture group around the timestamp");
                }
                Value::String(v)
            }
            "timestamp_format" => {
                if chrono::format::StrftimeItems::new(&v)
                    .any(|i| matches!(i, chrono::format::Item::Error))
                {
                    bail!("\"timestamp_format\" is not a valid chrono format string");
                }
                Value::String(v)
            }
            "cursors" => {
                validate_cursors_dir(&v)?;
                Value::String(v)
            }
            _ => Value::String(v),
        };
        map.insert(k.to_string(), value);
    }
    for k in unsets {
        let k = k.trim();
        if PROTECTED.contains(&k) {
            bail!("\"{k}\" is identity/lineage — a fact, not a setting");
        }
        map.remove(k);
    }

    if map.contains_key("timestamp_regex") != map.contains_key("timestamp_format") {
        bail!("timestamp_regex and timestamp_format go together (set both, or unset both)");
    }
    // Checked against the WHOLE resulting manifest, not just what this
    // invocation set: `set retain_unconsumed=true` on a store that already
    // declares a budget is fine, and `unset retain_size` on one that
    // retains unconsumed data is not. Refused here so the failure is at
    // the keyboard rather than in a writer's log an hour later.
    retention_from_map(&map)?;

    save(&dir, &name, &map)?;
    let saved = load(&dir, &name).context("re-reading the manifest")?;
    println!("{}", serde_json::to_string_pretty(&Value::Object(saved))?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn provenance_is_where_the_entries_came_from_not_what_the_store_is() {
        let m = map(&[
            // identity, lineage, settings, content format — the store
            // describing itself. None of these is a label, and a view that
            // leaked one would invite selecting on an operational setting.
            ("id", Value::String("abc".into())),
            ("created", Value::String("2026-08-22T00:00:00Z".into())),
            ("derived_from", Value::String("xyz".into())),
            ("index", Value::Bool(true)),
            ("retain", Value::String("90d".into())),
            ("retain_unconsumed", Value::Bool(true)),
            ("cursors", Value::String("/var/lib/timberfs".into())),
            ("timestamp_utc", Value::Bool(true)),
            ("wal", Value::Bool(true)),
            // One hop's bookkeeping, not provenance: under fan-in these
            // name only ONE of the origins.
            ("timberfs.store.id", Value::String("def".into())),
            ("timberfs.store.path", Value::String("/var/log/x".into())),
            // ...and where the entries came from, which is.
            ("host", Value::String("apache01".into())),
            ("service", Value::String("apache".into())),
            ("service.name", Value::String("apache".into())),
            ("datacentre", Value::String("osl1".into())),
        ]);
        let p = provenance(&m);
        assert_eq!(
            p.keys().cloned().collect::<Vec<_>>(),
            ["datacentre", "host", "service", "service.name"]
        );
        // Verbatim: a dotted key is NOT flattened here. Loki's label names
        // forbid the dot and timberfs does not, so flattening would lose the
        // key the operator wrote — and would silently merge `service.name`
        // with a `service_name` that flattens onto it.
        assert!(p.contains_key("service.name"));
        // And a manifest that is nothing but settings yields no labels,
        // rather than an error or a guess.
        assert!(provenance(&map(&[("index", Value::Bool(true))])).is_empty());
    }

    #[test]
    fn the_backstop_is_not_optional() {
        // Interest only ever holds MORE, so without a size budget one
        // stalled follower fills the disk and kills the producer. Refused
        // at parse time, which is what makes it refused at `set` too.
        let err = retention_from_map(&map(&[("retain_unconsumed", Value::Bool(true))]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("retain_size"), "{err}");
        // An age window is not a backstop: it is a bet on how long the
        // link stays down, which is the thing this feature exists to stop
        // making.
        assert!(retention_from_map(&map(&[
            ("retain_unconsumed", Value::Bool(true)),
            ("retain", Value::String("90d".into())),
        ]))
        .is_err());
        // With a budget it parses, and carries all three axes.
        let p = retention_from_map(&map(&[
            ("retain_unconsumed", Value::Bool(true)),
            ("retain_size", Value::String("50G".into())),
            ("retain", Value::String("90d".into())),
        ]))
        .unwrap();
        assert!(p.unconsumed);
        assert_eq!(p.max_comp_bytes, Some(50 * 1024 * 1024 * 1024));
        assert!(p.is_some());
    }

    #[test]
    fn retain_unconsumed_false_needs_nothing_and_declares_nothing() {
        let p = retention_from_map(&map(&[("retain_unconsumed", Value::Bool(false))])).unwrap();
        assert!(!p.unconsumed);
        assert!(!p.is_some(), "a store with no limits at all");
        // A non-boolean is an error, not a truthy string: `toBoolean` on
        // anything else is the kind of surprise a declaration must not
        // hold.
        assert!(
            retention_from_map(&map(&[("retain_unconsumed", Value::String("yes".into()))]))
                .is_err()
        );
    }

    #[test]
    fn a_declared_boolean_is_a_boolean_not_the_word() {
        // `--set index=true` writing "index": "true" declares a key every
        // reader evaluates as FALSE: silently declared, silently ignored.
        // `create --set` and `set` share one rule so they cannot diverge.
        for k in BOOLEAN_KEYS {
            assert_eq!(declared_value(k, "true").unwrap(), Value::Bool(true));
            assert_eq!(declared_value(k, "false").unwrap(), Value::Bool(false));
            assert!(
                declared_value(k, "yes").is_err(),
                "{k}=yes must be refused, not stored as a truthy string"
            );
        }
        // Everything else is free-form provenance and stays a string.
        assert_eq!(
            declared_value("host", "edge01").unwrap(),
            Value::String("edge01".into())
        );
    }

    #[test]
    fn the_axis_does_not_inherit_into_a_derived_artifact() {
        // Settings are per-store operational choices, and a rotated
        // segment has no followers: inheriting the axis would make an
        // archive wait on a consumer that reads the live store.
        let src = map(&[
            ("id", Value::String("abc".into())),
            ("retain_unconsumed", Value::Bool(true)),
            ("retain_size", Value::String("50G".into())),
            ("host", Value::String("edge01".into())),
        ]);
        let derived = derived_map(Some(&src), "rotate");
        assert!(!derived.contains_key("retain_unconsumed"));
        assert!(!derived.contains_key("retain_size"));
        // Provenance still travels: the lines survive extraction unchanged.
        assert_eq!(derived.get("host"), Some(&Value::String("edge01".into())));
        assert_eq!(
            derived.get("derived_from"),
            Some(&Value::String("abc".into()))
        );
    }
}
