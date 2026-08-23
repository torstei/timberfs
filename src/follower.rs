//! The follower registry: a store's consumers as DECLARED objects.
//!
//! A follower is a registered reader of one store — a name, a type, a
//! `retaining` flag and a position. Which is a Postgres replication slot,
//! and the parallel holds all the way down: an operator-chosen name
//! unique per host while the registration itself records which store it
//! belongs to, and an unused registration pinning data forever with a
//! size budget as the backstop.
//!
//! The alternative was cursors found by CONVENTION — a store declaring a
//! directory, every cursor in it holding the head back — and it fails
//! three ways, all of them the same failure: policy living in the
//! filesystem rather than in a declared object.
//!
//!   * Being in a directory cannot express INTENT. An ad-hoc `--cursor`
//!     left behind there pins the store, so a reader changes retention by
//!     leaving a file around; putting `retaining` in the path only moves
//!     that, since policy then changes by `mv`.
//!   * A directory cannot tell "nobody has ever read this" from "the
//!     consumer's file was deleted" — both are zero cursors. That is
//!     exactly the follower deployed before it first runs, the case most
//!     worth protecting.
//!   * Deriving a cursor's path from a program name, which is what made
//!     the convention convenient, COLLIDES the moment two shippers of one
//!     type follow one store: last writer wins, one advances past data the
//!     other never sent. Deriving a path removes misconfiguration and
//!     introduces collision.
//!
//! ```text
//! /var/lib/timberfs/followers/<name>/
//!     follower.json    store, type, retaining, config   (the operator writes)
//!     cursor.json      seq, n, delivered                (the follower writes)
//!     follower.lock    held while it runs               (`run` acquires)
//! ```
//!
//! Declaration and position are SEPARATE FILES because they have separate
//! owners: a cursor save is a whole-file tmp+rename that deliberately
//! drops keys it does not own (cursor.rs). One file would make every
//! position write preserve operator fields, and would race `update`.
//!
//! The lock is a third file rather than the cursor because a cursor save
//! REPLACES the inode by rename, and a lock on a renamed-over inode
//! silently stops excluding anyone — the same reason the store's writer
//! lock is never the `.rings` (store.rs).
//!
//! `<name>` is host-unique and constrained to `[A-Za-z0-9_.-]`, so it
//! needs no `systemd-escape` and is a legal directory name as it stands.
//! A UUID is the first instinct and is unusable in `systemctl status
//! timberfs-follower@…`, which is where these names actually get typed.
//!
//! **The store is recorded by IDENTITY, and the follower records it.**
//! Flat names mean the relation is stated once, by the party that knows
//! it — like a slot recording its database — so the store keeps no
//! follower list and there is no reverse index to fall out of sync. A
//! path would not do, a store being movable, so `create` mints the
//! store's `.bark` id when it has none.
//!
//! **systemd runs them; timberfs only dispatches.** `follower run <name>`
//! reads the declaration and EXECs the right binary for its type,
//! replacing its own process — so systemd keeps the lifecycle, the
//! restarts and the journal. A dispatcher, not a supervisor, which is
//! also what makes it safe for the registry to hold configuration at all:
//! the objection to storing a type and an endpoint is that something must
//! then run them, and `exec` is that something at zero supervisory cost.
//!
//! Note the direction. A follower here reads OUT of a store; `import
//! --follow` (follow.rs) reads a producer's file INTO one. Both run
//! `--follow`, so the verb is honestly shared, but the units are not the
//! same thing: `timberfs-follower@` ships a store off the box,
//! `timberfs-follow@` brings a file in.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};

use crate::cursor::{self, Cursor, Standing};
use crate::format;
use crate::query::resolve_backing;
use crate::store::{self, LockProbe};

/// Where the registry lives. One directory per host, holding one
/// directory per follower.
pub const DEFAULT_REGISTRY: &str = "/var/lib/timberfs/followers";

/// Replaces the registry path wholesale — the same idiom as
/// TIMBERFS_FORESTS: a test/one-off override that keeps this a pure
/// function of the environment with no clap plumbing, and lets a
/// follower run as a user who owns no /var/lib.
pub const REGISTRY_ENV: &str = "TIMBERFS_FOLLOWERS";

/// The systemd template that runs one. Named in messages so the unit an
/// operator has to type is never guessed at.
pub const UNIT_TEMPLATE: &str = "timberfs-follower@";

const DECL_FILE: &str = "follower.json";
const CURSOR_FILE: &str = "cursor.json";
const LOCK_FILE: &str = "follower.lock";

/// The follower types `run` knows how to exec. One per shipper binary:
/// the type names a DESTINATION SHAPE, which is also how the tool
/// boundary is drawn everywhere else here.
pub const TYPES: &[&str] = &["otlp", "frames"];

pub fn registry_dir() -> PathBuf {
    match std::env::var_os(REGISTRY_ENV) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(DEFAULT_REGISTRY),
    }
}

pub fn follower_dir(reg: &Path, name: &str) -> PathBuf {
    reg.join(name)
}

pub fn decl_path(reg: &Path, name: &str) -> PathBuf {
    follower_dir(reg, name).join(DECL_FILE)
}

pub fn cursor_path(reg: &Path, name: &str) -> PathBuf {
    follower_dir(reg, name).join(CURSOR_FILE)
}

pub fn lock_path(reg: &Path, name: &str) -> PathBuf {
    follower_dir(reg, name).join(LOCK_FILE)
}

pub fn unit_name(name: &str) -> String {
    format!("{UNIT_TEMPLATE}{name}.service")
}

/// A follower name is a directory name AND a systemd instance AND a
/// command-line argument, so it is validated once, here, against all
/// three. Rejected rather than escaped: `systemd-escape` would make the
/// name in `systemctl status` differ from the name the operator typed,
/// which is the whole reason this is not a UUID.
pub fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("a follower needs a name");
    }
    if name.len() > 64 {
        bail!("follower name {name:?} is too long (64 characters at most)");
    }
    if name == "." || name == ".." {
        bail!("{name:?} is not a name");
    }
    if name.starts_with('-') {
        bail!("follower name {name:?} may not start with `-` (it would read as a flag)");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
    {
        bail!(
            "follower name {name:?} contains {bad:?} — names are [A-Za-z0-9_.-], so that they \
             need no systemd-escape and read the same in `systemctl status` as here"
        );
    }
    Ok(())
}

/// A follower's declaration: the operator's half of the pair. Unknown
/// keys are PRESERVED across an `update`, like a `.bark` — a declaration
/// is a label, not a schema, and a key this version does not know may be
/// one the next does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// The registry directory name, which is authoritative. Written into
    /// the file too, so a human reading it alone knows whose it is — and
    /// so a copied directory can be caught.
    pub name: String,
    /// What the store is identified BY: its `.bark` id. The same anchor a
    /// cursor holds (`cursor::store_anchor`), so a store recognises its
    /// own followers by exactly the rule its own consumers wrote.
    pub store: String,
    /// The store's path when last declared — a HINT, since identity is
    /// what decides. `run` tries it first and falls back to a forest scan.
    pub path: String,
    /// Which binary `run` execs. `type` in the file; `kind` here, `type`
    /// being a keyword.
    pub kind: String,
    /// Does this follower hold the store's head back? Declared, so it
    /// cannot be acquired by leaving a file somewhere.
    pub retaining: bool,
    /// The destination, promoted out of `args` because every follower type
    /// has one and it is what an operator reads first in `list`.
    pub endpoint: Option<String>,
    /// Everything else, verbatim, in the shipper's own spelling. The
    /// registry deliberately does not re-declare `timber-otlp`'s surface:
    /// a second spelling of the same flags is a second thing to keep in
    /// step, and it would drift.
    pub args: Vec<String>,
    pub created: String,
    /// Keys this version does not know, kept so an `update` cannot eat
    /// them.
    pub extra: Map<String, Value>,
}

impl Declaration {
    /// Read the declaration of `name`. A missing or unreadable one is an
    /// error, never a default: a follower with no declaration is not a
    /// follower whose retention interest is "nothing", it is a registry
    /// that cannot be trusted to answer at all.
    pub fn load(reg: &Path, name: &str) -> anyhow::Result<Declaration> {
        let path = decl_path(reg, name);
        let text = fs::read_to_string(&path).with_context(|| {
            format!(
                "reading {} (is there a follower named {name}? `timberfs follower list`)",
                path.display()
            )
        })?;
        let mut map: Map<String, Value> = match serde_json::from_str(&text) {
            Ok(Value::Object(m)) => m,
            _ => bail!("{} is not a JSON object", path.display()),
        };
        let take_str = |m: &mut Map<String, Value>, k: &str| -> Option<String> {
            m.remove(k).and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            })
        };
        let declared_name = take_str(&mut map, "name");
        // A name disagreeing with the directory means the directory was
        // COPIED, and two followers sharing one position is precisely the
        // collision registration exists to prevent — one would advance
        // past data the other never sent. Refuse rather than pick.
        if let Some(declared) = &declared_name {
            if declared != name {
                bail!(
                    "{} declares the name {declared:?} but sits in {name:?} — a copied registry \
                     directory, and two followers sharing one position is the collision a \
                     registry exists to prevent. Fix the name, or remove the copy",
                    path.display()
                );
            }
        }
        let store = take_str(&mut map, "store").unwrap_or_default();
        if store.is_empty() {
            bail!(
                "{} names no \"store\" — a follower is a position in ONE store, recorded by \
                 identity",
                path.display()
            );
        }
        let kind = take_str(&mut map, "type").unwrap_or_default();
        if kind.is_empty() {
            bail!(
                "{} declares no \"type\", so nothing can be run for it (known: {})",
                path.display(),
                TYPES.join(", ")
            );
        }
        let retaining = map
            .remove("retaining")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let args = match map.remove("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s),
                    other => bail!("\"args\" holds {other}, which is not a string"),
                })
                .collect::<anyhow::Result<Vec<String>>>()
                .with_context(|| format!("in {}", path.display()))?,
            Some(other) => bail!("{}: \"args\" must be an array, got {other}", path.display()),
        };
        Ok(Declaration {
            name: name.to_string(),
            store,
            path: take_str(&mut map, "path").unwrap_or_default(),
            kind,
            retaining,
            endpoint: take_str(&mut map, "endpoint").filter(|e| !e.is_empty()),
            args,
            created: take_str(&mut map, "created").unwrap_or_default(),
            extra: map,
        })
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("name".into(), Value::String(self.name.clone()));
        map.insert("store".into(), Value::String(self.store.clone()));
        map.insert("path".into(), Value::String(self.path.clone()));
        map.insert("type".into(), Value::String(self.kind.clone()));
        map.insert("retaining".into(), Value::Bool(self.retaining));
        match &self.endpoint {
            Some(e) => map.insert("endpoint".into(), Value::String(e.clone())),
            None => map.insert("endpoint".into(), Value::Null),
        };
        map.insert(
            "args".into(),
            Value::Array(self.args.iter().cloned().map(Value::String).collect()),
        );
        map.insert("created".into(), Value::String(self.created.clone()));
        for (k, v) in &self.extra {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        map
    }

    /// Atomic and durable, for the same reason a cursor save is: a torn
    /// declaration after a crash is an unreadable registry, and an
    /// unreadable registry is a follower that cannot be started.
    pub fn save(&self, reg: &Path) -> anyhow::Result<()> {
        let dir = follower_dir(reg, &self.name);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = decl_path(reg, &self.name);
        let text = serde_json::to_string_pretty(&Value::Object(self.to_map()))? + "\n";
        let tmp = dir.join(format!("{DECL_FILE}.tmp"));
        {
            let mut f =
                fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
        if let Ok(d) = fs::File::open(&dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// The store this follower reads, as a path a shipper can be handed.
    ///
    /// Identity decides and the recorded path is only a hint, so the hint
    /// is CHECKED rather than trusted: a path that now holds a different
    /// store is exactly the case a path-keyed registry gets wrong.
    pub fn resolve_store(&self) -> anyhow::Result<PathBuf> {
        if !self.path.is_empty() {
            let p = PathBuf::from(&self.path);
            if let Ok((dir, name)) = resolve_backing(&p) {
                if format::rings_path(&dir, &name).exists() {
                    let bark = crate::bark::load(&dir, &name);
                    if cursor::store_anchor(&dir, &name, bark.as_ref()) == self.store {
                        return Ok(p);
                    }
                }
            }
        }
        match crate::forest::stores_by_anchor(&self.store).as_slice() {
            [one] => Ok(one.clone()),
            [] => bail!(
                "follower {} follows store {} — not at {} any more, and no configured forest \
                 holds it. Point it at the store again with `timberfs follower update {} \
                 store=<path>`",
                self.name,
                self.store,
                if self.path.is_empty() {
                    "(no recorded path)"
                } else {
                    &self.path
                },
                self.name
            ),
            // ⚠ Load-bearing beyond this message: an `id` names ONE
            // store's bytes, which is why a duplicate is corruption here
            // and not a replica. Any future addressable-replica work
            // (ROADMAP, "Globally addressable chunks") must therefore
            // travel a SEPARATE lineage key rather than share `id`, or it
            // turns this refusal — and `cursor::check_store`, which rests
            // on the same assumption — into a false alarm.
            several => bail!(
                "store {} is claimed by several stores, so which one follower {} means cannot \
                 be decided — a copied .bark gives two stores one identity:\n{}",
                self.store,
                self.name,
                several
                    .iter()
                    .map(|p| format!("  {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        }
    }

    /// The argv `run` execs: the shipper, the registry's own cursor path,
    /// and the operator's arguments last so an explicit flag always wins
    /// over a derived one.
    ///
    /// `--start` is derived from `retaining`, and that is the one
    /// non-obvious mapping here. `timber-otlp` defaults to `end` — only
    /// new entries — which for a RETAINING follower would skip on its
    /// first run exactly the backlog it was registered to protect, and
    /// then let retention drop it. Retaining says "this data is not lost
    /// until this follower has it", so `begin` is the only consistent
    /// reading of it.
    pub fn argv(&self, cursor: &Path, store: &Path) -> anyhow::Result<Vec<String>> {
        match self.kind.as_str() {
            "otlp" => {
                let mut argv = vec![
                    binary("timber-otlp").display().to_string(),
                    "--follow".to_string(),
                    "--cursor".to_string(),
                    cursor.display().to_string(),
                ];
                if !declares(&self.args, "--start") {
                    argv.push("--start".to_string());
                    argv.push(if self.retaining { "begin" } else { "end" }.to_string());
                }
                if let (Some(e), false) = (&self.endpoint, declares(&self.args, "--endpoint")) {
                    argv.push("--endpoint".to_string());
                    argv.push(e.clone());
                }
                argv.extend(self.args.iter().cloned());
                argv.push(store.display().to_string());
                Ok(argv)
            }
            "frames" => {
                // The native wire. No `--start`: a frames sender resumes
                // from the RECEIVER's coverage, which is authoritative, so
                // there is no local decision about where to begin — and no
                // way to accidentally re-ship a whole store.
                let mut argv = vec![
                    binary("timberfs").display().to_string(),
                    "frames-send".to_string(),
                    "--follow".to_string(),
                    "--cursor".to_string(),
                    cursor.display().to_string(),
                ];
                if let (Some(e), false) = (&self.endpoint, declares(&self.args, "--endpoint")) {
                    argv.push("--endpoint".to_string());
                    argv.push(e.clone());
                }
                argv.extend(self.args.iter().cloned());
                argv.push(store.display().to_string());
                Ok(argv)
            }
            other => bail!(
                "follower {} declares type {other:?}, which this timberfs cannot run \
                 (known: {})",
                self.name,
                TYPES.join(", ")
            ),
        }
    }
}

/// Is `flag` already spelled in the operator's own arguments, in either
/// `--flag value` or `--flag=value` form? A derived flag must never be
/// passed twice — clap would take the first and the operator would be
/// quietly overridden by a default.
fn declares(args: &[String], flag: &str) -> bool {
    let eq = format!("{flag}=");
    args.iter().any(|a| a == flag || a.starts_with(&eq))
}

/// The shipper binary: our OWN directory first, then `$PATH`. A dev build
/// must exec its SIBLING — otherwise `target/debug/timberfs follower run`
/// silently drives the installed shipper, which is a different version
/// against the same registry.
fn binary(name: &str) -> PathBuf {
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let sibling = dir.join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from(name)
}

/// Is a follower's process live? From the lock, never from systemd: a
/// lock is released by the kernel on process death, so it cannot go
/// stale, and asking systemd would answer about the unit rather than
/// about the thing holding a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Running,
    Stopped,
    /// The lock exists but could not be opened — a follower registered by
    /// another user. Never rendered as "stopped": the difference matters
    /// to anything that refuses on liveness.
    Unknown,
}

impl Liveness {
    /// For the RUNNING column, where the header supplies the question.
    pub fn text(&self) -> &'static str {
        match self {
            Liveness::Running => "yes",
            Liveness::Stopped => "no",
            Liveness::Unknown => "?",
        }
    }

    /// For a line with no column header to lean on.
    pub fn word(&self) -> &'static str {
        match self {
            Liveness::Running => "running",
            Liveness::Stopped => "stopped",
            Liveness::Unknown => "liveness unknown",
        }
    }
}

/// Two facts, and each covers the other's hole.
///
/// The LOCK says somebody holds it. It cannot say who, because a
/// descriptor is inherited: `run` clears FD_CLOEXEC so the lock survives
/// the exec, and the shipper then spawns its own reader (`timber-otlp`
/// runs `timberfs query --records --follow`), which inherits it too. Such
/// a child can outlive its parent — a shipper killed while its reader
/// sits idle leaves a grandchild holding the lock with no follower behind
/// it, and the lock alone reads that as running.
///
/// The recorded PID says whether that somebody is the follower. `exec`
/// preserves the pid, so the pid `run` writes before exec'ing IS the
/// shipper's for its whole life — which is what makes this a proof rather
/// than a pid-file heuristic. Held plus that pid alive is the follower;
/// held with it gone is an inherited descriptor.
///
/// ⚠ Residual: a recycled pid reads as running. It fails CLOSED — the
/// only thing liveness gates is a refusal to delete — and never the other
/// way, which is what would matter.
pub fn liveness(reg: &Path, name: &str) -> Liveness {
    match store::probe_path_exclusive(&lock_path(reg, name)) {
        LockProbe::Absent | LockProbe::Free => Liveness::Stopped,
        LockProbe::Unreadable => Liveness::Unknown,
        LockProbe::Held => match recorded_pid(reg, name) {
            Some(pid) if Path::new(&format!("/proc/{pid}")).exists() => Liveness::Running,
            Some(_) => Liveness::Stopped,
            // Held, with nothing recorded to attribute it to. Never
            // "stopped": a delete must not proceed on a lock nobody can
            // account for.
            None => Liveness::Unknown,
        },
    }
}

/// The pid `run` wrote into the lock file before exec'ing. Not trusted on
/// its own — it is only ever read for a lock that IS held.
fn recorded_pid(reg: &Path, name: &str) -> Option<u32> {
    let raw = fs::read_to_string(lock_path(reg, name)).ok()?;
    raw.lines()
        .next()?
        .rsplit_once("pid=")
        .and_then(|(_, p)| p.trim().parse::<u32>().ok())
}

/// A registered follower, read: its declaration, its position, whether it
/// is running, and — when its store can be found and read — where that
/// position stands against the store's chunks.
pub struct Registered {
    pub decl: Declaration,
    /// `None` when it has never delivered anything. Deliberately
    /// distinguished from a position of zero: "never run" is the state a
    /// registry exists to be able to express.
    pub cursor: Option<Cursor>,
    pub live: Liveness,
    /// The store as found now, and the standing of the position in it.
    /// `None` when the store cannot be resolved or read — reported as
    /// such, never as "caught up".
    pub store_path: Option<PathBuf>,
    pub standing: Option<Standing>,
}

impl Registered {
    pub fn name(&self) -> &str {
        &self.decl.name
    }

    /// The store's handle, for a column: its logical name minus a single
    /// `.log`, the same shortening `forest` does — so the STORE column of
    /// `follower list` reads like the HANDLE column of `list`.
    pub fn store_handle(&self) -> String {
        let path = self
            .store_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(&self.decl.path));
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            return self.decl.store.clone();
        }
        name.strip_suffix(".log").unwrap_or(&name).to_string()
    }

    /// Does this follower hold the store's ENTIRE head back? True of a
    /// retaining follower with no position — which is the point (it
    /// protects one deployed before it first runs) and the footgun.
    /// Deliberately not "behind_bytes covers everything": a follower that
    /// has never run has no measured backlog at all, and treating that as
    /// zero is exactly the reading that hides it.
    pub fn holds_everything(&self) -> bool {
        self.decl.retaining && self.cursor.as_ref().and_then(|c| c.seq).is_none()
    }

    pub fn position_text(&self) -> String {
        match self.cursor.as_ref().and_then(|c| c.seq) {
            Some(seq) => format!("chunk {seq}"),
            None => "-".to_string(),
        }
    }

    /// One phrase for how this follower is doing. The order matters: a
    /// missing store outranks a missing position, which outranks a
    /// distance — each of those makes the next meaningless.
    pub fn lag_text(&self) -> String {
        if self.store_path.is_none() {
            return "store gone".to_string();
        }
        match (&self.cursor, &self.standing) {
            (None, _) => "never run".to_string(),
            (Some(c), _) if c.delivered == 0 => "never run".to_string(),
            (Some(_), Some(st)) => st.lag_text(),
            (Some(_), None) => "store unreadable".to_string(),
        }
    }
}

/// Read one follower: declaration, position, liveness, and its standing
/// in its store when that can be found. Never fatal past the
/// declaration — a follower whose store is gone is still a registration,
/// and hiding it would hide the very thing an operator has to clean up.
pub fn read(reg: &Path, name: &str) -> anyhow::Result<Registered> {
    // One place guards every road into the registry: a name is a path
    // component here, so anything that is not a legal follower name has no
    // business being joined onto the registry directory.
    validate_name(name)?;
    let decl = Declaration::load(reg, name)?;
    let cursor = Cursor::load(&cursor_path(reg, name)).unwrap_or(None);
    let live = liveness(reg, name);
    let store_path = decl.resolve_store().ok();
    let standing = match (&store_path, &cursor) {
        (Some(p), Some(c)) => resolve_backing(p)
            .ok()
            .and_then(|(dir, base)| format::read_index(&format::rings_path(&dir, &base)).ok())
            .map(|records| cursor::standing(c, &records)),
        _ => None,
    };
    Ok(Registered {
        decl,
        cursor,
        live,
        store_path,
        standing,
    })
}

/// What the registry holds, read.
pub struct Registry {
    /// Every follower it could account for, by name.
    pub followers: Vec<Registered>,
    /// `(name, why)` per directory whose declaration could not be read.
    /// REPORTED rather than skipped: the registry is the only place a
    /// ghost follower is discoverable, and swallowing one would make
    /// `list` the single command blind to the problem it exists to
    /// surface.
    pub broken: Vec<(String, String)>,
}

/// Every follower in the registry, by name.
pub fn read_all(reg: &Path) -> anyhow::Result<Registry> {
    let mut names: Vec<String> = match fs::read_dir(reg) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", reg.display())),
    };
    names.sort();
    let mut followers = Vec::new();
    let mut broken = Vec::new();
    for name in names {
        match read(reg, &name) {
            Ok(r) => followers.push(r),
            Err(e) => broken.push((name, e.to_string())),
        }
    }
    Ok(Registry { followers, broken })
}

/// Every follower that holds a position in the store `anchor` identifies.
///
/// A scan of the registry filtered by store id, rather than a read of one
/// per-store directory — the cost of recording the relation once, on the
/// side that knows it. Cheap in practice: declarations are small and
/// change rarely.
pub fn for_store(reg: &Path, anchor: &str) -> Vec<Registered> {
    let held = read_all(reg).map(|r| r.followers).unwrap_or_default();
    let mut mine: Vec<Registered> = held
        .into_iter()
        .filter(|r| r.decl.store == anchor)
        .collect();
    rank(&mut mine);
    mine
}

/// The whole registry grouped by store anchor — read ONCE, for a command
/// that then asks about many stores. `list` over a fleet would otherwise
/// rescan the registry per store, and every scan reads every follower's
/// rings to place its position.
///
/// Deliberately not cached behind the module: a long-running writer must
/// see a registration made a second ago, so who re-reads and when stays
/// the caller's decision rather than a hidden one.
pub fn by_store(reg: &Path) -> HashMap<String, Vec<Registered>> {
    let mut out: HashMap<String, Vec<Registered>> = HashMap::new();
    let followers = read_all(reg).map(|r| r.followers).unwrap_or_default();
    for r in followers {
        out.entry(r.decl.store.clone()).or_default().push(r);
    }
    for group in out.values_mut() {
        rank(group);
    }
    out
}

/// Furthest behind first: that follower decides how much of the store is
/// unread, so it is the one an operator needs named.
///
/// A retaining follower with NO POSITION outranks every backlog, because
/// it holds the whole store rather than a tail of it — the footgun state,
/// and the one that must not sort last just because zero bytes have been
/// measured for it. That is also why `holds_everything` is a first-class
/// question and not "behind_bytes == 0".
fn rank(followers: &mut [Registered]) {
    followers.sort_by(|a, b| {
        let bytes = |r: &Registered| r.standing.map(|s| s.behind_bytes).unwrap_or(0);
        b.holds_everything()
            .cmp(&a.holds_everything())
            .then_with(|| bytes(b).cmp(&bytes(a)))
            .then_with(|| a.name().cmp(b.name()))
    });
}

// ---------------------------------------------------------------------------
// the interest axis
// ---------------------------------------------------------------------------

/// What a store's RETAINING followers hold back, as a retention tick needs
/// it. Read from the registry alone: the caller is the store, so nothing
/// here resolves a store or reads a rings file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Interest {
    /// Chunks numbered strictly below this are consumed by EVERY retaining
    /// follower, and may be dropped.
    ///
    /// `None` means none may — and that is the value every fail-closed
    /// case lands on, together with the healthy-but-holding case of a
    /// follower that has never run. They are one value because they are
    /// one instruction: drop nothing by interest. Note what that does NOT
    /// mean — interest is additive, so age and size still apply, and a
    /// `None` here is a hold on this axis only, never a hold on the store.
    pub floor: Option<u64>,
    /// The retaining follower whose position IS the floor: the one to name
    /// when a size budget overrides it. `None` when no retaining follower
    /// was found at all, so there is nobody to name and no loss to record.
    pub holder: Option<String>,
    /// That follower's position — `None` when it has never run, and
    /// therefore holds everything.
    pub holder_at: Option<u64>,
    /// How many retaining followers were considered, for reporting.
    pub retaining: usize,
}

impl Interest {
    /// How many head chunks may be dropped by interest alone — for a
    /// PREVIEW (`trim --dry-run`, `info`) computed outside a writer. The
    /// writer itself is handed `floor` and partitions its own chunks, so
    /// this is never the thing that decides a drop.
    pub fn droppable(&self, records: &[crate::format::ChunkRecord]) -> usize {
        match self.floor {
            Some(floor) => records.partition_point(|c| c.seq < floor),
            None => 0,
        }
    }
}

/// One retaining follower, as the interest axis sees it: a name and a
/// position, and nothing else.
type Retaining = (String, Option<u64>);

/// The interest axis for every store, from ONE read of the registry — for
/// a writer that then asks about each store it holds.
///
/// ⚠ Read afresh every tick, deliberately NOT gated on the registry
/// directory's mtime. That gate is the obvious optimisation and it is
/// wrong: a position save and an `update` are both a tmp+rename INSIDE
/// `followers/<name>/`, which leaves `followers/`'s own mtime untouched
/// (measured). So the gate would miss every position advance — the floor
/// would never move, and the axis would silently do nothing — and, worse,
/// it would miss an `update retaining=true`, leaving the store dropping
/// data a newly-retaining follower should be holding. That is the EARLY
/// direction, which the "dropping late is harmless" licence does not
/// cover. The scan it saves is one `read_dir` plus two small reads per
/// follower, page-cached, once per tick for all stores at once.
pub struct InterestIndex {
    /// Store anchor -> its retaining followers. `None` means fail closed
    /// for EVERY store: see `read`.
    per_store: Option<HashMap<String, Vec<Retaining>>>,
}

impl InterestIndex {
    /// Fail closed for every store, without reading anything — what a
    /// caller uses when it has no registry to consult.
    pub fn closed() -> InterestIndex {
        InterestIndex { per_store: None }
    }

    /// Read the registry.
    ///
    /// A declaration that cannot be read fails closed for ALL stores, not
    /// just its own: an unreadable declaration might have been a retaining
    /// follower of any store, and there is no way to know which. Harsh,
    /// and bounded by design — interest is additive, so age and size keep
    /// working and the only cost is dropping late, which is the harmless
    /// direction. It is also loud: `timberfs follower list` reports the
    /// same declaration as unreadable.
    pub fn read(reg: &Path) -> InterestIndex {
        let entries = match fs::read_dir(reg) {
            Ok(e) => e,
            // No registry at all: nothing is registered, so nothing is
            // held. Distinct from unreadable — this is a fact, not a gap.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return InterestIndex {
                    per_store: Some(HashMap::new()),
                }
            }
            Err(_) => return InterestIndex::closed(),
        };
        let mut per_store: HashMap<String, Vec<Retaining>> = HashMap::new();
        for entry in entries {
            let Ok(entry) = entry else {
                return InterestIndex::closed();
            };
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                return InterestIndex::closed();
            };
            if validate_name(&name).is_err() {
                // Not a name a follower can have, so not a follower —
                // some other directory somebody put here.
                continue;
            }
            let decl = match Declaration::load(reg, &name) {
                Ok(d) => d,
                Err(_) => return InterestIndex::closed(),
            };
            if !decl.retaining {
                continue;
            }
            // A position that cannot be read is not a position: the
            // follower holds everything until it can be read again.
            let at = match Cursor::load(&cursor_path(reg, &name)) {
                Ok(c) => c.and_then(|c| c.seq),
                Err(_) => None,
            };
            per_store.entry(decl.store).or_default().push((name, at));
        }
        InterestIndex {
            per_store: Some(per_store),
        }
    }

    /// What the store `anchor` may drop by interest. `next_seq` is the
    /// number the store will give its NEXT chunk — everything it has ever
    /// written is below it.
    pub fn for_store(&self, anchor: &str, next_seq: u64) -> Interest {
        let Some(per_store) = &self.per_store else {
            return Interest::default();
        };
        let Some(followers) = per_store.get(anchor) else {
            // Nothing retains this store: the axis holds nothing, and
            // there is nobody to name in a loss record.
            return Interest::default();
        };
        let retaining = followers.len();
        // The minimum over the set, with two ways to be zero. A follower
        // that has never run holds everything, and one claiming a chunk
        // the store has never written is a wrong anchor or a hand-edit --
        // newly PROVABLE, where a future timestamp was indistinguishable
        // from clock skew. Both mean: drop nothing, and name that one.
        let mut floor: Option<u64> = None;
        for (name, at) in followers {
            match at {
                None => {
                    return Interest {
                        floor: None,
                        holder: Some(name.clone()),
                        holder_at: None,
                        retaining,
                    }
                }
                Some(seq) if *seq >= next_seq => {
                    return Interest {
                        floor: None,
                        holder: Some(name.clone()),
                        holder_at: Some(*seq),
                        retaining,
                    }
                }
                Some(seq) => {
                    if floor.is_none_or(|f| *seq < f) {
                        floor = Some(*seq);
                    }
                }
            }
        }
        let holder = floor.and_then(|f| {
            followers
                .iter()
                .find(|(_, at)| *at == Some(f))
                .map(|(n, _)| n.clone())
        });
        Interest {
            floor,
            holder,
            holder_at: floor,
            retaining,
        }
    }
}

/// The anchor a store's followers are matched by. Derived, not cached
/// here: it can change exactly once, when `follower create` mints an
/// identity for a store that had none, and a tick that cached the
/// pre-minting value would then find no followers — the harmless
/// direction, but the axis would silently never start working. Callers
/// pair it with the policy, which is re-read on the same manifest change.
pub fn anchor_of(dir: &Path, name: &str) -> String {
    cursor::store_anchor(dir, name, crate::bark::load(dir, name).as_ref())
}

/// The interest axis for ONE retention tick: the registry is read at most
/// once, and only if some store actually declares the axis — so a host
/// where nothing declares `retain_unconsumed` never touches it, and a
/// writer holding many stores reads it once rather than once per store.
#[derive(Default)]
pub struct TickInterest {
    index: Option<InterestIndex>,
}

impl TickInterest {
    /// What the store `anchor` may drop by interest on this tick.
    /// `next_seq` is the number its next chunk will get.
    pub fn floor(
        &mut self,
        policy: &crate::bark::Retention,
        anchor: &str,
        next_seq: u64,
    ) -> Interest {
        if !policy.unconsumed {
            return Interest::default();
        }
        self.index
            .get_or_insert_with(|| InterestIndex::read(&registry_dir()))
            .for_store(anchor, next_seq)
    }
}

/// The exact loss, when a declared budget dropped chunks a retaining
/// follower had not read.
///
/// This is a REQUIREMENT rather than a nicety. With finite disk, bounded
/// loss is a choice already made — the alternative is blocking the
/// producer, which for telemetry is worse than losing an hour of access
/// logs — so what is owed is precise accounting at the moment it happens,
/// and the writer holds both halves of the comparison right there. The
/// shipper's GAP warning is the same fact inferred later, from the other
/// side; this one is exact.
///
/// `None` when nothing unconsumed was dropped (the healthy case) or when
/// no retaining follower was found (nothing to account to).
pub fn override_record(
    store: &str,
    policy: &crate::bark::Retention,
    stats: &crate::store::RotateStats,
    interest: &Interest,
) -> Option<String> {
    let holder = interest.holder.as_ref()?;
    let first = match interest.floor {
        // It held everything, so everything dropped was unread.
        None => stats.first_seq,
        // Chunks at or past the floor were dropped despite being unread.
        Some(floor) if stats.last_seq >= floor => floor.max(stats.first_seq),
        Some(_) => return None,
    };
    let budget = policy
        .max_comp_bytes
        .map(crate::rotate::human_bytes)
        .unwrap_or_else(|| "the size budget".to_string());
    let position = match interest.holder_at {
        Some(seq) => format!("at chunk {seq}"),
        None => "which has never run".to_string(),
    };
    Some(format!(
        "timberfs: {store}: retain_size ({budget}) reached with follower {holder} {position} — \
         dropped chunks {first}..{} it had not read",
        stats.last_seq
    ))
}

// ---------------------------------------------------------------------------
// systemd
// ---------------------------------------------------------------------------

/// `systemctl <verb> timberfs-follower@<name>` — for the four verbs an
/// operator asked for by flag. Passive state is never read this way (see
/// `liveness`); these are actions, and an action that cannot be performed
/// must say so rather than be inferred away.
fn systemctl(verb: &str, name: &str) -> anyhow::Result<()> {
    let unit = unit_name(name);
    let status = Command::new("systemctl")
        .arg(verb)
        .arg(&unit)
        .status()
        .with_context(|| format!("running `systemctl {verb} {unit}`"))?;
    if !status.success() {
        bail!("`systemctl {verb} {unit}` failed ({status})");
    }
    crate::note!("timberfs: systemctl {verb} {unit}");
    Ok(())
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

pub struct CreateOpts {
    /// The store to follow: a path, or a forest handle.
    pub store: PathBuf,
    pub kind: String,
    pub endpoint: Option<String>,
    pub retaining: bool,
    pub enable: bool,
    pub start: bool,
    pub dry_run: bool,
    /// The shipper's own arguments, verbatim (everything after `--`).
    pub args: Vec<String>,
}

/// `timberfs follower create`: register a follower.
///
/// Refused if the name is taken, so a collision is a registration error
/// rather than two processes overwriting one position.
pub fn cmd_create(name: &str, opts: CreateOpts) -> anyhow::Result<()> {
    validate_name(name)?;
    if !TYPES.contains(&opts.kind.as_str()) {
        bail!(
            "unknown follower type {:?} (known: {})",
            opts.kind,
            TYPES.join(", ")
        );
    }
    let reg = registry_dir();
    let dir = follower_dir(&reg, name);
    if dir.exists() {
        bail!(
            "follower {name} already exists ({}) — names are host-unique, and two followers \
             sharing one position would let each advance past data the other never sent. \
             `timberfs follower status {name}` to see whose it is",
            dir.display()
        );
    }

    // Identity, not address: a store can move, so the declaration records
    // the `.bark` id and mints one when the store has none.
    let store = crate::forest::resolve_source(&opts.store)?;
    let (sdir, sname) = resolve_backing(&store)?;
    if !format::rings_path(&sdir, &sname).exists() {
        bail!(
            "no timberfs store {sname} in {} — a follower is a position in a store that exists",
            sdir.display()
        );
    }
    let bark = crate::bark::ensure_identified(&sdir, &sname).with_context(|| {
        format!(
            "declaring an identity for {} (needs write access to its backing directory): a \
             follower records its store by identity, not by path, because a store can move",
            store.display()
        )
    })?;
    let anchor = cursor::store_anchor(&sdir, &sname, Some(&bark));
    if anchor.starts_with("path:") {
        bail!(
            "{} has no declared identity and one could not be minted — a follower cannot be \
             anchored to a path, a store being movable",
            store.display()
        );
    }

    let decl = Declaration {
        name: name.to_string(),
        store: anchor,
        path: fs::canonicalize(&sdir)
            .unwrap_or(sdir)
            .join(&sname)
            .display()
            .to_string(),
        kind: opts.kind,
        retaining: opts.retaining,
        endpoint: opts.endpoint,
        args: opts.args,
        created: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        extra: Map::new(),
    };
    // Built before anything is written, so a bad type or an unrunnable
    // combination fails at registration rather than at the first start.
    // Against the declaration's OWN path, not the argument as typed, so
    // the preview is the command `run` will actually exec.
    let argv = decl.argv(&cursor_path(&reg, name), Path::new(&decl.path))?;

    if opts.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(decl.to_map()))?
        );
        println!("would run: {}", argv.join(" "));
        println!("dry run: nothing registered");
        return Ok(());
    }
    decl.save(&reg)?;
    crate::note!(
        "timberfs: registered follower {name} on {} ({})",
        decl.path,
        decl.kind
    );
    // The footgun, in one line, at the moment it is created — the same
    // one Postgres has with an unused slot. A retaining follower with no
    // position holds EVERYTHING, which is the point (it protects a
    // follower deployed before it first runs) and also the trap.
    if decl.retaining {
        crate::note!(
            "timberfs: {name} is retaining: once a writer honours it, it holds the whole store \
             until it first runs. `--start` (or `systemctl start {}`) makes the safe path the \
             easy one",
            unit_name(name)
        );
    }
    if opts.enable {
        systemctl("enable", name)?;
    }
    if opts.start {
        systemctl("start", name)?;
    }
    if !opts.enable && !opts.start {
        crate::note!(
            "timberfs: nothing runs it yet — `systemctl enable --now {}`",
            unit_name(name)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

const COLUMNS: [&str; 7] = [
    "NAME",
    "STORE",
    "TYPE",
    "RETAINING",
    "POSITION",
    "LAG",
    "RUNNING",
];

fn row_cells(r: &Registered) -> Vec<String> {
    vec![
        r.name().to_string(),
        r.store_handle(),
        r.decl.kind.clone(),
        if r.decl.retaining { "yes" } else { "no" }.to_string(),
        r.position_text(),
        r.lag_text(),
        r.live.text().to_string(),
    ]
}

pub fn to_json(r: &Registered) -> Value {
    let mut o = Map::new();
    o.insert("name".into(), r.name().into());
    o.insert("store".into(), r.decl.store.clone().into());
    o.insert(
        "store_path".into(),
        match &r.store_path {
            Some(p) => p.display().to_string().into(),
            None => Value::Null,
        },
    );
    o.insert("declared_path".into(), r.decl.path.clone().into());
    o.insert("type".into(), r.decl.kind.clone().into());
    o.insert("retaining".into(), r.decl.retaining.into());
    o.insert(
        "endpoint".into(),
        match &r.decl.endpoint {
            Some(e) => e.clone().into(),
            None => Value::Null,
        },
    );
    o.insert(
        "args".into(),
        Value::Array(r.decl.args.iter().cloned().map(Value::String).collect()),
    );
    o.insert("created".into(), r.decl.created.clone().into());
    o.insert(
        "running".into(),
        match r.live {
            Liveness::Running => Value::Bool(true),
            Liveness::Stopped => Value::Bool(false),
            Liveness::Unknown => Value::Null,
        },
    );
    o.insert("unit".into(), unit_name(r.name()).into());
    // The same keys whether or not there is a position, so a consumer
    // tests for a VALUE rather than for a key's presence.
    match &r.cursor {
        None => {
            for k in ["seq", "n", "delivered", "wl"] {
                o.insert(k.into(), Value::Null);
            }
        }
        Some(c) => {
            o.insert("seq".into(), c.seq.map(Into::into).unwrap_or(Value::Null));
            o.insert("n".into(), c.n.into());
            o.insert("delivered".into(), c.delivered.into());
            o.insert("wl".into(), c.wl.into());
        }
    }
    match &r.standing {
        None => {
            o.insert("behind_chunks".into(), Value::Null);
            o.insert("behind_bytes".into(), Value::Null);
            o.insert("gap_chunks".into(), Value::Null);
        }
        Some(st) => {
            o.insert("consumed_chunks".into(), st.consumed_chunks.into());
            o.insert("behind_chunks".into(), st.behind_chunks.into());
            o.insert("behind_bytes".into(), st.behind_bytes.into());
            o.insert("behind_ms".into(), st.behind_ms.into());
            o.insert(
                "gap_chunks".into(),
                st.gap_chunks.map(Into::into).unwrap_or(Value::Null),
            );
        }
    }
    o.insert("lag".into(), r.lag_text().into());
    Value::Object(o)
}

/// `timberfs follower list [--store STORE]`.
pub fn cmd_list(store: Option<&Path>, names_only: bool, json: bool) -> anyhow::Result<()> {
    let reg = registry_dir();
    let Registry {
        mut followers,
        broken,
    } = read_all(&reg)?;
    if let Some(s) = store {
        let path = crate::forest::resolve_source(s)?;
        let (dir, name) = resolve_backing(&path)?;
        let bark = crate::bark::load(&dir, &name);
        let anchor = cursor::store_anchor(&dir, &name, bark.as_ref());
        followers.retain(|r| r.decl.store == anchor);
    }
    if names_only {
        // What shell completion consumes: bare names, no header, no
        // columns — the same contract as `timberfs list --names`.
        for r in &followers {
            println!("{}", r.name());
        }
        return Ok(());
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Array(followers.iter().map(to_json).collect()))?
        );
    } else if followers.is_empty() {
        crate::note!(
            "timberfs: no followers registered in {} (`timberfs follower create` registers one)",
            reg.display()
        );
    } else {
        let data: Vec<Vec<String>> = followers.iter().map(row_cells).collect();
        println!("{}", crate::list::format_table(&COLUMNS, &data));
    }
    for (name, why) in &broken {
        eprintln!("timberfs: warning: follower {name} is not readable: {why}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// `timberfs follower status NAME`: one follower on one screen — what it
/// declares, where it stands, and whether anything is honouring it.
pub fn cmd_status(name: &str, json: bool) -> anyhow::Result<()> {
    let reg = registry_dir();
    let r = read(&reg, name)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&to_json(&r))?);
        return Ok(());
    }
    println!("follower  {}", r.name());
    match &r.store_path {
        Some(p) => println!("store     {}  (id {})", p.display(), r.decl.store),
        None => println!(
            "store     {} — NOT FOUND (declared at {})",
            r.decl.store,
            if r.decl.path.is_empty() {
                "no path"
            } else {
                &r.decl.path
            }
        ),
    }
    println!("type      {}", r.decl.kind);
    if let Some(e) = &r.decl.endpoint {
        println!("endpoint  {e}");
    }
    if !r.decl.args.is_empty() {
        println!("args      {}", r.decl.args.join(" "));
    }
    print_retaining(&r);
    match &r.cursor {
        None => println!("position  none — it has never delivered anything"),
        Some(c) => {
            println!(
                "position  {}, {} entries in; {} delivered",
                r.position_text(),
                c.n,
                c.delivered
            );
        }
    }
    println!("lag       {}", r.lag_text());
    if let Some(st) = &r.standing {
        if let Some(n) = st.gap_chunks {
            println!(
                "          GAP — {n} chunk(s) were dropped before it read them; it resumes at \
                 the oldest one still here"
            );
        } else if st.behind_chunks > 0 {
            println!(
                "          {} unread in {} chunk(s)",
                crate::rotate::human_bytes(st.behind_bytes),
                st.behind_chunks
            );
        }
    }
    let holder = match r.live {
        Liveness::Running => store::describe_lock_holder(&lock_path(&reg, name))
            .map(|w| format!("yes ({w})"))
            .unwrap_or_else(|| "yes".to_string()),
        Liveness::Stopped => "no".to_string(),
        Liveness::Unknown => "unknown (the lock is not readable from here)".to_string(),
    };
    println!("running   {holder}");
    println!("unit      {}", unit_name(name));
    Ok(())
}

/// What `retaining` currently means for this follower — which depends on
/// whether the store it follows declares that it honours it. Stating the
/// flag alone would be a half-truth: a declared interest no writer reads
/// holds nothing back.
fn print_retaining(r: &Registered) {
    if !r.decl.retaining {
        println!("retaining no — it holds nothing back; retention ignores its position");
        return;
    }
    let honoured = r
        .store_path
        .as_ref()
        .and_then(|p| resolve_backing(p).ok())
        .and_then(|(dir, name)| crate::bark::load(&dir, &name))
        .and_then(|m| m.get("retain_unconsumed").and_then(Value::as_bool))
        .unwrap_or(false);
    if honoured {
        println!("retaining yes — its position holds the store's head back");
    } else {
        println!(
            "retaining yes — declared, but no writer honours it yet (the store does not declare \
             retain_unconsumed)"
        );
    }
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

/// The keys `update` will change. `store` takes a path or handle and is
/// re-resolved to an identity; the rest are scalars.
const SETTABLE: &[&str] = &["retaining", "endpoint", "type", "store"];

/// `timberfs follower update NAME KEY=VALUE...`
///
/// `retaining=false` is the first half of retiring a follower, and it is
/// separate from `delete` on purpose: the destructive act deserves its
/// own command. Both refusals are about deliberateness rather than
/// prevention — `update && delete` is still one line — so there is no
/// `--force`: the two-step IS the force.
pub fn cmd_update(
    name: &str,
    sets: &[String],
    unsets: &[String],
    args: Option<Vec<String>>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let reg = registry_dir();
    let before = read(&reg, name)?;
    let mut decl = before.decl.clone();
    if sets.is_empty() && unsets.is_empty() && args.is_none() {
        bail!(
            "nothing to do — give KEY=VALUE to set ({}), --unset KEY, or `-- ARGS...` to \
             replace the shipper's arguments",
            SETTABLE.join("/")
        );
    }
    for kv in sets {
        let Some((k, v)) = kv.split_once('=') else {
            bail!("update wants KEY=VALUE, got {kv:?}");
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "retaining" => {
                decl.retaining = match v {
                    "true" => true,
                    "false" => false,
                    _ => bail!("\"retaining\" is true or false"),
                }
            }
            "endpoint" => decl.endpoint = Some(v.to_string()).filter(|e| !e.is_empty()),
            "type" => {
                if !TYPES.contains(&v) {
                    bail!("unknown follower type {v:?} (known: {})", TYPES.join(", "));
                }
                decl.kind = v.to_string();
            }
            "store" => {
                let store = crate::forest::resolve_source(Path::new(v))?;
                let (sdir, sname) = resolve_backing(&store)?;
                // Checked before minting: `ensure_identified` WRITES, so a
                // typo would otherwise leave a stray manifest next to no
                // store, and the follower would point at it.
                if !format::rings_path(&sdir, &sname).exists() {
                    bail!(
                        "no timberfs store {sname} in {} — a follower is a position in a store \
                         that exists",
                        sdir.display()
                    );
                }
                let bark = crate::bark::ensure_identified(&sdir, &sname)?;
                let anchor = cursor::store_anchor(&sdir, &sname, Some(&bark));
                // Re-pointing a follower at a DIFFERENT store leaves its
                // position anchored to the old one, and cursor.rs refuses
                // to resume across that — say so here rather than let the
                // shipper fail at its next start.
                if anchor != decl.store && before.cursor.is_some() {
                    crate::note!(
                        "timberfs: warning: {name} has a position in store {} — remove {} to \
                         start over in the new store, or the shipper will refuse to resume",
                        decl.store,
                        cursor_path(&reg, name).display()
                    );
                }
                decl.store = anchor;
                decl.path = fs::canonicalize(&sdir)
                    .unwrap_or(sdir)
                    .join(&sname)
                    .display()
                    .to_string();
            }
            "name" | "path" | "created" => {
                bail!("\"{k}\" is identity, not configuration — it is not settable")
            }
            _ => bail!(
                "unknown key {k:?} (settable: {}); a shipper flag goes after `--`",
                SETTABLE.join(", ")
            ),
        }
    }
    for k in unsets {
        match k.trim() {
            "endpoint" => decl.endpoint = None,
            "retaining" => decl.retaining = false,
            other => bail!("{other:?} cannot be unset"),
        }
    }
    if let Some(a) = args {
        decl.args = a;
    }
    // Validated before it is written, exactly as at create: an update that
    // makes a follower unrunnable must fail now, not at its next start.
    let store_for_argv = decl.resolve_store().unwrap_or_else(|_| PathBuf::from("?"));
    decl.argv(&cursor_path(&reg, name), &store_for_argv)?;

    if decl == before.decl {
        crate::note!("timberfs: {name} already declares that; nothing written");
        return Ok(());
    }
    // Releasing the head is the part worth quantifying, and the part
    // whose asymmetry is easy to miss: the FLAG toggles, its EFFECT does
    // not. Setting it back to true will not bring dropped data back.
    let releasing = before.decl.retaining && !decl.retaining;
    if releasing {
        match (&before.standing, &before.cursor) {
            (Some(st), Some(c)) if st.consumed_chunks > 0 || st.behind_bytes > 0 => {
                crate::note!(
                    "timberfs: {name} releases the head at chunk {} — {} in {} chunk(s) it \
                     alone was holding become droppable",
                    c.seq.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
                    crate::rotate::human_bytes(st.behind_bytes),
                    st.behind_chunks
                );
            }
            _ => crate::note!("timberfs: {name} releases the head (it held no position)"),
        }
        crate::note!(
            "timberfs: this does not undo: setting retaining=true again will not bring dropped \
             data back, and {name} then resumes at a position that may be gapped"
        );
    }
    if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(decl.to_map()))?
        );
        println!("dry run: nothing written");
        return Ok(());
    }
    decl.save(&reg)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Value::Object(decl.to_map()))?
    );
    if before.live == Liveness::Running {
        crate::note!(
            "timberfs: {name} is running with the OLD declaration — `systemctl restart {}` to \
             pick this up",
            unit_name(name)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

/// `timberfs follower delete NAME`: bookkeeping, once the head has been
/// released.
///
/// Refused while `retaining=true` (set it false first, and see what that
/// frees) and while the follower is RUNNING — deleting under a live
/// process would leave it writing an unlinked cursor, silently doing
/// nothing. One escape: a follower whose store no longer exists deletes
/// freely, there being nothing to release.
pub fn cmd_delete(name: &str, stop: bool, disable: bool) -> anyhow::Result<()> {
    validate_name(name)?;
    let reg = registry_dir();
    let dir = follower_dir(&reg, name);
    if !dir.exists() {
        bail!("no follower {name} in {}", reg.display());
    }
    // A declaration too broken to read cannot be holding anything back
    // either, so it deletes — a ghost must be removable by the command
    // that finds it.
    let r = read(&reg, name).ok();
    // Every refusal comes BEFORE anything is touched, `--stop`/`--disable`
    // included: a command that declines must leave nothing behind, and
    // stopping the unit of a follower we are about to refuse to delete
    // would be exactly the silent release the refusal exists to prevent.
    if let Some(r) = &r {
        if r.decl.retaining && r.store_path.is_some() {
            bail!(
                "{name} is retaining, so deleting it would silently release the store's head. \
                 Release it deliberately first:\n  \
                 timberfs follower update {name} retaining=false\n  \
                 timberfs follower delete {name}"
            );
        }
    }
    if stop {
        systemctl("stop", name)?;
    }
    // Probed after the stop, since that is what a `--stop` is for, and it
    // is the LIVE state that decides — a follower deleted from under its
    // own process would leave it writing an unlinked position file,
    // silently doing nothing at all.
    match liveness(&reg, name) {
        Liveness::Running => bail!(
            "{name} is running{} — deleting it now would leave it writing an unlinked \
             cursor, doing nothing at all. Stop it first (`--stop`, or `systemctl stop {}`)",
            store::describe_lock_holder(&lock_path(&reg, name))
                .map(|w| format!(" ({w})"))
                .unwrap_or_default(),
            unit_name(name)
        ),
        Liveness::Unknown => bail!(
            "cannot tell whether {name} is running: {} is not readable from here",
            lock_path(&reg, name).display()
        ),
        Liveness::Stopped => {}
    }
    if disable {
        systemctl("disable", name)?;
    }
    fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    crate::note!("timberfs: deleted follower {name} ({})", dir.display());
    if !disable {
        crate::note!(
            "timberfs: the unit is untouched — `systemctl disable {}` if it was enabled",
            unit_name(name)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

/// `timberfs follower run NAME`: read the declaration, take the lock, and
/// EXEC the shipper.
///
/// The lock is acquired HERE and inherited across the exec (its
/// FD_CLOEXEC is cleared), so the shipper needs no lock code of its own
/// and the registry's liveness is a property of the registry. flock is
/// per open file description and survives exec, so the lock the new
/// process image holds is this one.
///
/// ⚠ The lock never gates retention. A follower that is temporarily down
/// holds no lock and must still pin the head — that is the entire purpose
/// of a spool. The lock detects collisions and reports liveness; it
/// decides nothing about what may be dropped. Inverting that would turn
/// "the shipper is down" into "drop everything it had not read".
pub fn cmd_run(name: &str) -> anyhow::Result<()> {
    validate_name(name)?;
    let reg = registry_dir();
    let decl = Declaration::load(&reg, name)?;
    let store = decl.resolve_store()?;
    let cursor = cursor_path(&reg, name);
    let argv = decl.argv(&cursor, &store)?;

    let lpath = lock_path(&reg, name);
    let lock = store::lock_path_exclusive(&lpath)
        .with_context(|| format!("locking {}", lpath.display()))?
        .with_context(|| {
            format!(
                "follower {name} is already running{} — one position, one process",
                store::describe_lock_holder(&lpath)
                    .map(|w| format!(" ({w})"))
                    .unwrap_or_default()
            )
        })?;
    store::write_lock_info(
        &lock,
        &format!("follower {name} pid={}\n", std::process::id()),
    )?;
    // Hand the lock to the process we are about to become. Rust opens
    // every file O_CLOEXEC, so without this the exec would drop it and
    // every follower would read as stopped while running.
    if unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_SETFD, 0) } == -1 {
        return Err(std::io::Error::last_os_error())
            .context("clearing FD_CLOEXEC on the follower lock");
    }

    crate::note!("timberfs: follower {name}: exec {}", argv.join(" "));
    let err = Command::new(&argv[0]).args(&argv[1..]).exec();
    // exec only returns on failure.
    Err(err).with_context(|| {
        format!(
            "exec {} for follower {name} (is the timberfs package complete?)",
            argv[0]
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "timberfs-follower-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn decl(name: &str, retaining: bool) -> Declaration {
        Declaration {
            name: name.to_string(),
            store: "store-id".into(),
            path: "/var/log/timberfs/app/app.log".into(),
            kind: "otlp".into(),
            retaining,
            endpoint: Some("http://127.0.0.1:4318".into()),
            args: vec!["--service".into(), "checkout".into()],
            created: "2026-08-21T00:00:00Z".into(),
            extra: Map::new(),
        }
    }

    #[test]
    fn a_name_is_a_directory_a_unit_and_an_argument() {
        for ok in ["central", "otlp-1", "a.b_c", "X9"] {
            validate_name(ok).unwrap_or_else(|e| panic!("{ok} should be legal: {e}"));
        }
        // Anything needing systemd-escape would make the name in
        // `systemctl status` differ from the one typed here.
        for bad in ["", "a/b", "a b", "a@b", "..", "-x", "æ"] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn a_declaration_roundtrips_and_keeps_keys_it_does_not_know() {
        let reg = scratch("roundtrip");
        let mut d = decl("central", true);
        d.extra.insert("note".into(), Value::String("mine".into()));
        d.save(&reg).unwrap();
        let back = Declaration::load(&reg, "central").unwrap();
        assert_eq!(back, d);
        // A declaration is a label, not a schema: an unknown key survives
        // a save so a newer timberfs's key is not eaten by an older one.
        let text = fs::read_to_string(decl_path(&reg, "central")).unwrap();
        assert!(text.contains("\"note\": \"mine\""), "{text}");
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn a_copied_registry_directory_is_refused() {
        // Two followers sharing one position is the collision the whole
        // registry exists to prevent, and `cp -r` is how it happens.
        let reg = scratch("copied");
        decl("central", true).save(&reg).unwrap();
        fs::create_dir_all(follower_dir(&reg, "clone")).unwrap();
        fs::copy(decl_path(&reg, "central"), decl_path(&reg, "clone")).unwrap();
        let err = Declaration::load(&reg, "clone").unwrap_err().to_string();
        assert!(err.contains("copied registry directory"), "{err}");
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn a_declaration_must_name_a_store_and_a_type() {
        let reg = scratch("incomplete");
        let write = |name: &str, body: &str| {
            fs::create_dir_all(follower_dir(&reg, name)).unwrap();
            fs::write(decl_path(&reg, name), body).unwrap();
        };
        write("nostore", r#"{"type":"otlp"}"#);
        assert!(Declaration::load(&reg, "nostore")
            .unwrap_err()
            .to_string()
            .contains("no \"store\""));
        write("notype", r#"{"store":"x"}"#);
        assert!(Declaration::load(&reg, "notype")
            .unwrap_err()
            .to_string()
            .contains("no \"type\""));
        write("garbage", "not json");
        assert!(Declaration::load(&reg, "garbage").is_err());
        // Missing is an error too: a follower with no declaration is a
        // registry that cannot be trusted to answer, not an empty policy.
        assert!(Declaration::load(&reg, "absent").is_err());
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn retaining_ships_from_the_beginning() {
        // The one derived flag that matters: --start defaults to `end` in
        // the shipper, which for a retaining follower would skip on its
        // first run exactly the backlog it exists to protect.
        let cursor = Path::new("/reg/central/cursor.json");
        let store = Path::new("/var/log/timberfs/app/app.log");
        let argv = decl("central", true).argv(cursor, store).unwrap();
        let joined = argv.join(" ");
        assert!(joined.contains("--start begin"), "{joined}");
        let argv = decl("tap", false).argv(cursor, store).unwrap();
        assert!(argv.join(" ").contains("--start end"));
    }

    #[test]
    fn the_operators_own_flags_win_over_derived_ones() {
        // A derived flag passed twice would be silently overridden by
        // clap taking the first, so a flag the operator spelled means the
        // derivation stands down — in either spelling.
        let cursor = Path::new("/reg/c/cursor.json");
        let store = Path::new("/s/app.log");
        let mut d = decl("central", true);
        d.args = vec!["--start".into(), "end".into()];
        let argv = d.argv(cursor, store).unwrap();
        assert_eq!(argv.iter().filter(|a| *a == "--start").count(), 1);
        assert!(argv.join(" ").ends_with("--start end /s/app.log"));

        d.args = vec!["--endpoint=http://elsewhere:4318".into()];
        let argv = d.argv(cursor, store).unwrap();
        assert_eq!(
            argv.iter().filter(|a| a.starts_with("--endpoint")).count(),
            1
        );
    }

    #[test]
    fn argv_ends_with_the_store_and_carries_the_registry_cursor() {
        let cursor = Path::new("/reg/central/cursor.json");
        let store = Path::new("/var/log/timberfs/app/app.log");
        let argv = decl("central", true).argv(cursor, store).unwrap();
        assert!(argv[0].ends_with("timber-otlp"), "{:?}", argv[0]);
        assert_eq!(argv.last().unwrap(), "/var/log/timberfs/app/app.log");
        assert!(argv.contains(&"--follow".to_string()));
        let i = argv.iter().position(|a| a == "--cursor").unwrap();
        assert_eq!(argv[i + 1], "/reg/central/cursor.json");
    }

    #[test]
    fn an_unknown_type_cannot_be_run() {
        let mut d = decl("central", true);
        d.kind = "kafka".into();
        let err = d
            .argv(Path::new("/c"), Path::new("/s"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("kafka"), "{err}");
        assert!(err.contains("otlp"), "{err}");
    }

    #[test]
    fn read_all_reports_a_broken_declaration_rather_than_hiding_it() {
        let reg = scratch("broken");
        decl("good", false).save(&reg).unwrap();
        fs::create_dir_all(follower_dir(&reg, "bad")).unwrap();
        fs::write(decl_path(&reg, "bad"), "{").unwrap();
        let held = read_all(&reg).unwrap();
        assert_eq!(held.followers.len(), 1);
        assert_eq!(held.followers[0].name(), "good");
        assert_eq!(held.broken.len(), 1);
        assert_eq!(held.broken[0].0, "bad");
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn a_missing_registry_is_empty_not_an_error() {
        // Nothing has registered yet is the normal state on a fresh host,
        // and every read path has to survive it.
        let empty = read_all(Path::new("/nonexistent/timberfs-followers")).unwrap();
        assert!(empty.followers.is_empty());
        assert!(empty.broken.is_empty());
    }

    #[test]
    fn for_store_filters_by_identity_and_ranks_the_worst_first() {
        let reg = scratch("bystore");
        let mut a = decl("ahead", true);
        a.store = "id-a".into();
        a.save(&reg).unwrap();
        let mut b = decl("behind", true);
        b.store = "id-a".into();
        b.save(&reg).unwrap();
        let mut other = decl("elsewhere", true);
        other.store = "id-b".into();
        other.save(&reg).unwrap();
        let mine = for_store(&reg, "id-a");
        let names: Vec<&str> = mine.iter().map(|r| r.name()).collect();
        // No stores on disk, so every standing is empty and the ranking
        // falls back to the name — this is about WHICH followers are ours.
        assert_eq!(names, ["ahead", "behind"]);
        assert!(for_store(&reg, "id-c").is_empty());
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn liveness_comes_from_the_lock_and_survives_a_cursor_rewrite() {
        let reg = scratch("liveness");
        decl("central", true).save(&reg).unwrap();
        assert_eq!(liveness(&reg, "central"), Liveness::Stopped);
        let held = store::lock_path_exclusive(&lock_path(&reg, "central"))
            .unwrap()
            .unwrap();
        // As `run` does before exec'ing: the pid is what says the holder
        // is the follower and not a descriptor its shipper leaked.
        store::write_lock_info(
            &held,
            &format!("follower central pid={}\n", std::process::id()),
        )
        .unwrap();
        // The lock is a file of its own precisely so a cursor save, which
        // replaces its inode by rename, cannot drop it.
        let c = Cursor::new("central", "store-id", "/p");
        c.save(&cursor_path(&reg, "central")).unwrap();
        c.save(&cursor_path(&reg, "central")).unwrap();
        assert_eq!(liveness(&reg, "central"), Liveness::Running);
        drop(held);
        assert_eq!(liveness(&reg, "central"), Liveness::Stopped);
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn an_inherited_lock_is_not_a_running_follower() {
        // The failure this exists for: `timber-otlp` spawns `timberfs
        // query --records --follow` to read the store, that child
        // inherits the lock, and it can outlive its parent — leaving the
        // lock held with no follower behind it. exec preserves the pid,
        // so the recorded pid is what tells the two apart.
        let reg = scratch("inherited");
        decl("central", true).save(&reg).unwrap();
        let held = store::lock_path_exclusive(&lock_path(&reg, "central"))
            .unwrap()
            .unwrap();
        // A pid that cannot be alive: /proc's own ceiling plus one.
        store::write_lock_info(&held, "follower central pid=4294967295\n").unwrap();
        assert_eq!(liveness(&reg, "central"), Liveness::Stopped);
        // Held, and nothing recorded to attribute it to: never "stopped",
        // since a delete must not proceed on a lock nobody can account for.
        store::write_lock_info(&held, "\n").unwrap();
        assert_eq!(liveness(&reg, "central"), Liveness::Unknown);
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn a_retaining_follower_that_never_ran_outranks_every_backlog() {
        // It holds the WHOLE store, not a tail of it, and has no measured
        // backlog at all — so ranking on bytes alone would sort the
        // worst case last.
        let reg = scratch("holdsall");
        let mut fresh = decl("fresh", true);
        fresh.store = "id".into();
        fresh.save(&reg).unwrap();
        let mut behind = decl("behind", true);
        behind.store = "id".into();
        behind.save(&reg).unwrap();
        Cursor {
            seq: Some(3),
            ..Cursor::new("behind", "id", "/p")
        }
        .save(&cursor_path(&reg, "behind"))
        .unwrap();
        let mine = for_store(&reg, "id");
        assert_eq!(
            mine.iter().map(|r| r.name()).collect::<Vec<_>>(),
            ["fresh", "behind"]
        );
        assert!(mine[0].holds_everything());
        assert!(!mine[1].holds_everything());
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn a_registered_follower_with_no_position_reads_as_never_run() {
        let reg = scratch("neverrun");
        decl("central", true).save(&reg).unwrap();
        let r = read(&reg, "central").unwrap();
        // The store does not exist here, and that outranks the position:
        // each of these makes the next meaningless.
        assert_eq!(r.lag_text(), "store gone");
        assert_eq!(r.position_text(), "-");
        assert!(r.cursor.is_none());
        fs::remove_dir_all(&reg).ok();
    }

    /// A retaining follower of `store`, standing at `at`.
    fn retaining(reg: &Path, name: &str, store: &str, at: Option<u64>) {
        let mut d = decl(name, true);
        d.store = store.to_string();
        d.save(reg).unwrap();
        if let Some(seq) = at {
            Cursor {
                seq: Some(seq),
                ..Cursor::new(name, store, "/p")
            }
            .save(&cursor_path(reg, name))
            .unwrap();
        }
    }

    #[test]
    fn the_floor_is_the_minimum_over_retaining_followers() {
        let reg = scratch("floor");
        retaining(&reg, "ahead", "id", Some(9));
        retaining(&reg, "behind", "id", Some(4));
        // A non-retaining follower of the same store decides nothing: the
        // flag is what expresses interest, not the presence of a position.
        let mut tap = decl("tap", false);
        tap.store = "id".into();
        tap.save(&reg).unwrap();
        Cursor {
            seq: Some(0),
            ..Cursor::new("tap", "id", "/p")
        }
        .save(&cursor_path(&reg, "tap"))
        .unwrap();

        let held = InterestIndex::read(&reg).for_store("id", 100);
        assert_eq!(held.floor, Some(4));
        assert_eq!(held.holder.as_deref(), Some("behind"));
        assert_eq!(held.retaining, 2, "the tap is not one of them");
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn a_follower_that_never_ran_holds_everything_and_is_named() {
        // The point of the whole design: "nobody has ever read this" is
        // expressible, and it holds the store rather than releasing it.
        let reg = scratch("neverran");
        retaining(&reg, "ahead", "id", Some(9));
        retaining(&reg, "fresh", "id", None);
        let held = InterestIndex::read(&reg).for_store("id", 100);
        assert_eq!(held.floor, None, "one of them holds everything");
        assert_eq!(held.holder.as_deref(), Some("fresh"));
        assert_eq!(held.holder_at, None);
        // And a floor of None drops nothing, whatever the store holds.
        assert_eq!(held.droppable(&three()), 0);
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn a_position_past_everything_written_is_impossible_not_merely_odd() {
        // Newly PROVABLE, where a future timestamp was indistinguishable
        // from clock skew: a chunk number beyond what the store has ever
        // written is a wrong anchor or a hand-edit. Fail closed and say
        // whose it is.
        let reg = scratch("impossible");
        retaining(&reg, "bogus", "id", Some(500));
        let held = InterestIndex::read(&reg).for_store("id", 10);
        assert_eq!(held.floor, None);
        assert_eq!(held.holder.as_deref(), Some("bogus"));
        assert_eq!(held.holder_at, Some(500));
        // One below next_seq is a legal position, and drops accordingly.
        let ok = InterestIndex::read(&reg).for_store("id", 501);
        assert_eq!(ok.floor, Some(500));
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn every_way_of_not_knowing_drops_nothing() {
        // Each of these is indistinguishable from "consumed" if read
        // wrong, so each has to land on the same instruction: drop nothing
        // by interest. Note what that is NOT — interest is additive, so
        // age and size keep working throughout.
        let reg = scratch("failclosed");

        // No registry at all.
        let absent = InterestIndex::read(Path::new("/nonexistent/timberfs-followers"));
        assert_eq!(absent.for_store("id", 100).floor, None);

        // A registry with nothing in it, and one with nothing for us.
        retaining(&reg, "elsewhere", "other-store", Some(3));
        let held = InterestIndex::read(&reg).for_store("id", 100);
        assert_eq!(held.floor, None);
        assert_eq!(held.holder, None, "nobody to name, so no loss record");

        // A follower of ours whose position cannot be read holds
        // everything: an unreadable position is not a position.
        retaining(&reg, "torn", "id", None);
        fs::write(cursor_path(&reg, "torn"), "{ not json").unwrap();
        assert_eq!(InterestIndex::read(&reg).for_store("id", 100).floor, None);

        // An unreadable DECLARATION fails closed for every store, not just
        // its own: it might have been a retaining follower of any of them,
        // and there is no way to know which.
        fs::write(decl_path(&reg, "torn"), "{ not json").unwrap();
        let broken = InterestIndex::read(&reg);
        assert_eq!(broken.for_store("id", 100).floor, None);
        assert_eq!(broken.for_store("other-store", 100).floor, None);
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn the_axis_is_not_consulted_unless_the_store_declares_it() {
        // A host where nothing declares `retain_unconsumed` must never
        // touch the registry — which is also what makes reading it afresh
        // every tick affordable.
        let mut tick = TickInterest::default();
        let off = crate::bark::Retention {
            max_comp_bytes: Some(1024),
            ..Default::default()
        };
        assert_eq!(tick.floor(&off, "id", 100), Interest::default());
        assert!(tick.index.is_none(), "nothing declared it, so nothing read");

        let on = crate::bark::Retention {
            max_comp_bytes: Some(1024),
            unconsumed: true,
            ..Default::default()
        };
        tick.floor(&on, "id", 100);
        assert!(tick.index.is_some(), "declared, so read once");
    }

    #[test]
    fn the_loss_record_names_the_follower_and_the_exact_range() {
        let policy = crate::bark::Retention {
            max_comp_bytes: Some(50 * 1024),
            unconsumed: true,
            ..Default::default()
        };
        let stats = |first: u64, last: u64| crate::store::RotateStats {
            chunks_moved: (last - first + 1) as usize,
            uncomp_bytes: 0,
            comp_bytes: 0,
            first_write_ms: 0,
            last_write_ms: 0,
            chunks_remaining: 0,
            first_seq: first,
            last_seq: last,
        };
        let held = Interest {
            floor: Some(4200),
            holder: Some("central".into()),
            holder_at: Some(4200),
            retaining: 1,
        };
        // The budget went past the floor: the overrun is recorded from the
        // floor, not from the start of the drop — chunks below it were
        // consumed and their loss is the healthy case.
        let record = override_record("app.log", &policy, &stats(4000, 4830), &held).unwrap();
        assert!(
            record.contains("follower central at chunk 4200"),
            "{record}"
        );
        assert!(record.contains("dropped chunks 4200..4830"), "{record}");
        assert!(record.contains("50.0 KiB"), "{record}");

        // A drop entirely below the floor is consumed data going, which is
        // the whole point of the feature and not a loss at all.
        assert!(override_record("app.log", &policy, &stats(4000, 4199), &held).is_none());

        // A follower that never ran held everything, so everything dropped
        // was unread — and the record says so rather than naming a chunk.
        let fresh = Interest {
            floor: None,
            holder: Some("fresh".into()),
            holder_at: None,
            retaining: 1,
        };
        let record = override_record("app.log", &policy, &stats(0, 12), &fresh).unwrap();
        assert!(
            record.contains("follower fresh which has never run"),
            "{record}"
        );
        assert!(record.contains("dropped chunks 0..12"), "{record}");

        // Nobody registered: nothing to account to, so no record.
        assert!(override_record("app.log", &policy, &stats(0, 12), &Interest::default()).is_none());
    }

    #[test]
    fn droppable_is_the_same_prefix_a_cursor_calls_consumed() {
        // The invariant tying the two halves together: what interest lets
        // retention drop must be exactly what a resume would never
        // deliver. `cursor::consumed_prefix` is the other side of it.
        let records = three();
        for seq in 0..=3u64 {
            let held = Interest {
                floor: Some(seq),
                holder: Some("c".into()),
                holder_at: Some(seq),
                retaining: 1,
            };
            assert_eq!(
                held.droppable(&records),
                cursor::consumed_prefix(&records, Some(seq)),
                "interest and the cursor disagree at chunk {seq}"
            );
        }
    }

    /// Three chunks numbered 0..2, as cursor.rs's own tests use.
    fn three() -> Vec<crate::format::ChunkRecord> {
        (0..3)
            .map(|i| crate::format::ChunkRecord {
                uncomp_start: i * 10,
                uncomp_len: 10,
                comp_start: i * 10,
                comp_len: 10,
                first_write_ms: 100 + i * 100,
                last_write_ms: 200 + i * 100,
                seq: i,
            })
            .collect()
    }

    #[test]
    fn the_registry_lays_out_one_directory_per_follower() {
        // The default is /var/lib, which no test may write and no
        // unprivileged follower owns.
        assert_eq!(registry_dir(), PathBuf::from(DEFAULT_REGISTRY));
        assert_eq!(
            follower_dir(Path::new("/reg"), "central"),
            PathBuf::from("/reg/central")
        );
        assert_eq!(unit_name("central"), "timberfs-follower@central.service");
    }
}
