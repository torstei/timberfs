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
//!     follower.json    select, retaining, command       (the operator writes)
//!     positions.json   a place per store it has read    (the follower writes)
//!     follower.lock    held while it runs               (`run` acquires)
//! ```
//!
//! Declaration and positions are SEPARATE FILES because they have separate
//! owners: a positions save is a whole-file tmp+rename that deliberately
//! drops keys it does not own (cursor.rs). One file would make every
//! position write preserve operator fields, and would race `update`.
//!
//! The lock is a third file rather than the positions because a positions
//! save REPLACES the inode by rename, and a lock on a renamed-over inode
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
/// A selection's places, one per store it has read. Separate from
/// `cursor.json` rather than a new shape inside it: the two are different
/// documents, and a reader must never mistake a single-store cursor for
/// an empty set of positions, which would read as "nothing consumed" and
/// re-ship every store.
const POSITIONS_FILE: &str = "positions.json";
const LOCK_FILE: &str = "follower.lock";

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

pub fn positions_path(reg: &Path, name: &str) -> PathBuf {
    follower_dir(reg, name).join(POSITIONS_FILE)
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

/// What to say about a declaration that names a TYPE, or nothing.
///
/// Nothing is migrated. A type named a destination shape and implied
/// that a new destination meant a new binary in this tree; a command
/// says the truth, which is that a destination is a program. Turning
/// `type: otlp` into `timber-otlp --endpoint …` was tried and is worse
/// than refusing: that program was rewritten by the same change, so an
/// upgraded host would have got a declaration that HANGS until the
/// hello times out. An error naming the one command that fixes it is
/// the better failure.
/// The extra directory a `--store` needs swept, if any: a store need not
/// be in a forest, and a follower sweeps forests. Empty where the
/// selection already reaches it, so an in-forest declaration carries no
/// path at all.
fn place_to_look(select: &str, dir: &Path) -> Vec<String> {
    match crate::select::Selector::parse(select) {
        Ok(p) if crate::select::resolve(&[], &p).is_empty() => vec![dir.display().to_string()],
        _ => Vec::new(),
    }
}

fn no_command(at: &Path, kind: &str) -> anyhow::Error {
    let what = match kind {
        "" => "declares no \"command\"".to_string(),
        other => format!("declares `type: {other}`, which no longer means anything"),
    };
    anyhow::anyhow!(
        "{} {what}. A follower is fed a PROGRAM, and that program says how far to move each \
         store's position — so give it one:\n  \
         timberfs follower update <name> -- timber-otlp --endpoint http://collector:4318",
        at.display()
    )
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
    /// WHICH stores this follows, as a selector — the same predicate
    /// `list --select` and the query document take.
    ///
    /// One store is the one-term case `id=<its id>`, which is what
    /// `--store` writes: a follower has always recorded its store by
    /// IDENTITY and `create` has always refused a path-anchored one, so
    /// every declaration ever written is expressible here and a legacy
    /// `store` member migrates to exactly the store it named.
    pub select: String,
    /// Does this follower hold the store's head back? Declared, so it
    /// cannot be acquired by leaving a file somewhere.
    pub retaining: bool,
    /// The CONSUMER: a program and its arguments, fed the records and
    /// asked how far to move the positions.
    ///
    /// A list, so nothing makes a quoting round trip on the way to it —
    /// the same reason a timbersh target's `cmd` is one. Recorded
    /// verbatim and never inspected: what is not ours to interpret is
    /// passed on unread, and a create-time check of it could not be
    /// enforced later anyway (see docs/plans/consumer-protocol.md).
    ///
    /// ⚠ This replaced `type` + `endpoint` + `args`. A TYPE per
    /// destination reads as a taxonomy that grows one entry per protocol
    /// and implies the answer to "mine is not listed" is a new binary in
    /// this tree; a command says the truth, which is that a destination
    /// is a program and timberfs does not need to know which. An older
    /// declaration's `type` and `endpoint` migrate to the command they
    /// were shorthand for.
    pub command: Vec<String>,
    /// Where a store this follower has never read is picked up.
    ///
    /// `None` means take the default, which DEPENDS on `retaining` — so
    /// it is derived rather than stored: turning retaining on later must
    /// change this with it, and a value written at create time would
    /// silently keep the old answer.
    pub follow_from: Option<crate::ship::FollowFrom>,
    /// Extra directories to sweep BESIDE the configured forests.
    ///
    /// ⚠ A place to LOOK, never an address. The selection is still what
    /// decides which store, so a store that moves inside a swept
    /// directory is still found and one that moves out of it is still
    /// found through its forest — nothing here is dereferenced.
    ///
    /// It exists because a store need not be in a forest: a backing
    /// directory under a mount is the normal shape, and
    /// `create --store <path>` there recorded an identity no sweep could
    /// reach, so the follower followed nothing forever while reporting
    /// that its store had not appeared YET.
    pub look_in: Vec<String>,
    pub created: String,
    /// Keys this version does not know, kept so an `update` cannot eat
    /// them.
    pub extra: Map<String, Value>,
}

impl Declaration {
    /// Every directory this follower's selection is resolved against:
    /// the configured forests, plus whatever `look_in` adds.
    ///
    /// The forests are expanded rather than left implied, because a
    /// non-empty directory list REPLACES them — so adding one place to
    /// look would otherwise remove every other.
    pub fn sweeps(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = crate::forest::forest_dirs();
        for d in &self.look_in {
            let d = PathBuf::from(d);
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        dirs
    }

    /// The policy in force. `retaining` implies `begin`: it promises
    /// that data is not lost until this follower has it, so skipping a
    /// backlog on the first read would contradict the declaration.
    pub fn picks_up(&self) -> crate::ship::FollowFrom {
        self.follow_from.unwrap_or(if self.retaining {
            crate::ship::FollowFrom::Begin
        } else {
            crate::ship::FollowFrom::Discovery
        })
    }

    /// The one store this follows, by identity, when it follows exactly
    /// one — the `id=<x>` case, which is every declaration `--store`
    /// wrote. `None` for a set, where nothing single can be resolved,
    /// named in an error, or given a legacy cursor.
    pub fn anchor(&self) -> Option<String> {
        crate::select::Selector::parse(&self.select)
            .ok()
            .and_then(|s| s.sole_id().map(str::to_string))
    }

    /// The selection, parsed. A declaration that cannot be parsed was
    /// refused at load, so this is infallible in practice and still
    /// returns the error rather than panicking on a hand-edit race.
    pub fn selector(&self) -> anyhow::Result<crate::select::Selector> {
        crate::select::Selector::parse(&self.select)
    }

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
        // The legacy member named ONE store by identity, so it is the
        // one-term selector for that store. Read, never rewritten: a
        // declaration says which stores it follows in one place.
        let legacy = take_str(&mut map, "store").unwrap_or_default();
        let select = match take_str(&mut map, "select") {
            // Not defaulted. An absent selection is a declaration that
            // does not say what it follows; `[]` is one that says «every
            // store», and the two must never be one value.
            Some(s) if !s.trim().is_empty() => s,
            _ if !legacy.is_empty() => format!("[id={legacy}]"),
            _ => bail!(
                "{} names no \"select\" — a follower follows the stores a predicate matches. \
                 One store is `[id=<its id>]`; every store is `[]`, which is a thing to have \
                 written rather than a member to leave out",
                path.display()
            ),
        };
        let select = crate::select::canonical(&select)
            .with_context(|| format!("{}: \"select\"", path.display()))?;
        let retaining = map
            .remove("retaining")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let strings = |m: &mut Map<String, Value>, k: &str| -> anyhow::Result<Vec<String>> {
            match m.remove(k) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Array(items)) => items
                    .into_iter()
                    .map(|v| match v {
                        Value::String(s) => Ok(s),
                        other => bail!("{k:?} holds {other}, which is not a string"),
                    })
                    .collect(),
                Some(other) => bail!("{k:?} must be an array, got {other}"),
            }
        };
        let command =
            strings(&mut map, "command").with_context(|| format!("in {}", path.display()))?;
        // Taken out rather than left in `extra`, so an `update` does
        // not carry them forward: a `type`, an `endpoint`, a `path` and
        // an `args` are what a command replaced, and keeping them would
        // leave a declaration describing two arrangements at once.
        let _ = strings(&mut map, "args");
        let legacy_type = take_str(&mut map, "type").unwrap_or_default();
        let _ = take_str(&mut map, "endpoint");
        let _ = take_str(&mut map, "path");
        if command.is_empty() {
            return Err(no_command(&path, &legacy_type));
        }
        let follow_from = match take_str(&mut map, "follow_from") {
            Some(v) => Some(
                crate::ship::FollowFrom::parse(&v)
                    .with_context(|| format!("{}: \"follow_from\"", path.display()))?,
            ),
            None => None,
        };
        Ok(Declaration {
            name: name.to_string(),
            select,
            retaining,
            command,
            follow_from,
            look_in: strings(&mut map, "look_in")?,
            created: take_str(&mut map, "created").unwrap_or_default(),
            extra: map,
        })
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("name".into(), Value::String(self.name.clone()));
        map.insert("select".into(), Value::String(self.select.clone()));
        map.insert("retaining".into(), Value::Bool(self.retaining));
        map.insert(
            "command".into(),
            Value::Array(
                self.command
                    .iter()
                    .map(|a| Value::String(a.clone()))
                    .collect(),
            ),
        );
        if let Some(from) = self.follow_from {
            map.insert(
                "follow_from".into(),
                Value::String(from.as_str().to_string()),
            );
        }
        if !self.look_in.is_empty() {
            map.insert(
                "look_in".into(),
                Value::Array(
                    self.look_in
                        .iter()
                        .map(|d| Value::String(d.clone()))
                        .collect(),
                ),
            );
        }
        map.insert("created".into(), Value::String(self.created.clone()));
        for (k, v) in &self.extra {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        map
    }

    /// Atomic and durable, for the same reason a positions save is: a torn
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
#[derive(Clone)]
/// One store this follower covers, as a view renders it.
pub struct Covered {
    pub id: String,
    pub path: PathBuf,
    /// Where it stands in that store. `None` when the store's index
    /// cannot be read — reported as such, never as "caught up".
    pub standing: Option<Standing>,
    /// Has this follower read anything from this store at all? A store
    /// it covers and has never read is the state a registry exists to
    /// express, and it is not a position of zero.
    pub read: bool,
    /// The consumer's last word about this store.
    pub note: Option<crate::cursor::Note>,
}

/// A follower's positions, as reading them can turn out.
///
/// ⚠ Three states and not two: a file that is MISSING says the follower
/// has never run, which is the state a registry exists to be able to
/// express, and one that is UNREADABLE says nothing at all — and must
/// not be rendered as "nothing consumed", which is the reading that
/// would let retention drop what it is holding.
#[derive(Clone)]
pub enum Places {
    Never,
    Unreadable,
    Held(crate::cursor::Positions),
}

impl Places {
    pub fn held(&self) -> Option<&crate::cursor::Positions> {
        match self {
            Places::Held(p) => Some(p),
            _ => None,
        }
    }
}

/// A registered follower, read: its declaration, its positions, whether
/// it is running, and where it stands in each store it covers.
#[derive(Clone)]
pub struct Registered {
    pub decl: Declaration,
    /// Where it stands, in three states rather than two.
    pub places: Places,
    pub live: Liveness,
    /// Every store the selection matches now, worst first. A selection
    /// is resolved to build this, which is a forest scan — so it is done
    /// where a view asks for it and not on the interest axis.
    pub covered: Vec<Covered>,
}

impl Registered {
    pub fn name(&self) -> &str {
        &self.decl.name
    }

    /// What it follows, for a column: one store's handle where it names
    /// one, else the predicate itself.
    pub fn follows_text(&self) -> String {
        match self.covered.as_slice() {
            [one] => {
                let name = one
                    .path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name.is_empty() {
                    self.decl.select.clone()
                } else {
                    name.strip_suffix(".log").unwrap_or(&name).to_string()
                }
            }
            _ => self.decl.select.clone(),
        }
    }

    /// Does this follower hold an ENTIRE store's head back? True of a
    /// retaining follower covering a store it has never read — which is
    /// the point (it protects one deployed before it first runs) and the
    /// footgun. Deliberately not "behind_bytes covers everything": a
    /// store never read has no measured backlog at all, and treating
    /// that as zero is exactly the reading that hides it.
    pub fn holds_everything(&self) -> bool {
        self.decl.retaining && self.covered.iter().any(|c| !c.read)
    }

    /// Where it stands in ONE store, by identity. What a view about a
    /// single store needs: `worst` is across the whole selection, which
    /// for one store's page would name somebody else's backlog.
    pub fn at(&self, id: &str) -> Option<&Covered> {
        self.covered.iter().find(|c| c.id == id)
    }

    /// The worst standing across the stores it covers: the one that
    /// decides how much is unread, and so the one an operator needs
    /// named.
    pub fn worst(&self) -> Option<&Covered> {
        self.covered
            .iter()
            .find(|c| !c.read)
            .or_else(|| self.covered.iter().max_by_key(|c| c.behind_bytes()))
    }

    pub fn behind_bytes(&self) -> u64 {
        self.covered.iter().map(Covered::behind_bytes).sum()
    }

    /// Recorded places for stores this follower no longer covers, each
    /// with whether its backing is still on disk.
    ///
    /// ⚠ These are not visible anywhere else, and there are three states
    /// in that file rather than one: covered, left the selection, and
    /// gone from disk. A place is KEPT when a store leaves — bringing it
    /// back resumes rather than re-ships — so they accumulate for as
    /// long as the selection churns, and nothing prunes them. Whether a
    /// store is covered is the SELECTOR's answer and cannot be read off
    /// the file, so this is derived here rather than recorded there.
    pub fn uncovered(&self) -> Vec<(String, bool)> {
        let Some(p) = self.places.held() else {
            return Vec::new();
        };
        p.at.iter()
            .filter(|(id, _)| !self.covered.iter().any(|c| &c.id == *id))
            .map(|(id, at)| {
                // ⚠ The recorded path is the store's LOGICAL name, which
                // is never a file — the pair is `<name>.rings` and
                // `<name>.trunk`. Testing the logical path answers false
                // for a perfectly live store.
                let here = !at.path.is_empty()
                    && resolve_backing(Path::new(&at.path))
                        .is_ok_and(|(dir, name)| format::rings_path(&dir, &name).exists());
                (id.clone(), here)
            })
            .collect()
    }

    /// One phrase for how this follower is doing, across its whole
    /// selection. The order matters: a selection that matches nothing
    /// outranks a store never read, which outranks a distance — each
    /// makes the next meaningless.
    pub fn lag_text(&self) -> String {
        if matches!(self.places, Places::Unreadable) {
            return "positions unreadable".to_string();
        }
        if self.covered.is_empty() {
            return "matches nothing".to_string();
        }
        let unread = self.covered.iter().filter(|c| !c.read).count();
        if unread == self.covered.len() {
            return "never run".to_string();
        }
        // ⚠ A store never read has NO position, so it has no distance
        // from one: its `behind_ms` is measured from zero and reads as
        // the whole unix epoch. The worst store here is an unread one by
        // construction — `worst` puts them first, because holding a
        // whole store outranks any measured backlog — so the phrase has
        // to be about that and not about a clock.
        if unread > 0 {
            return format!("{unread} of {} never read", self.covered.len());
        }
        match self.worst().and_then(|c| c.standing.as_ref()) {
            Some(st) => st.lag_text(),
            None => "store unreadable".to_string(),
        }
    }
}

impl Covered {
    pub fn behind_bytes(&self) -> u64 {
        self.standing.map(|s| s.behind_bytes).unwrap_or(0)
    }

    pub fn handle(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        name.strip_suffix(".log").unwrap_or(&name).to_string()
    }
}

/// Read one follower: declaration, positions, liveness, and where it
/// stands in each store its selection covers. Never fatal past the
/// declaration — a follower whose stores are gone is still a
/// registration, and hiding it would hide the very thing an operator has
/// to clean up.
pub fn read(reg: &Path, name: &str) -> anyhow::Result<Registered> {
    // One place guards every road into the registry: a name is a path
    // component here, so anything that is not a legal follower name has
    // no business being joined onto the registry directory.
    validate_name(name)?;
    let decl = Declaration::load(reg, name)?;
    let places = match crate::cursor::Positions::load(&positions_path(reg, name)) {
        Ok(Some(p)) => Places::Held(p),
        Err(_) => Places::Unreadable,
        // No positions file. A declaration written before they existed
        // kept a cursor for the one store it named, and that is still
        // that store's place until `run` carries it over.
        Ok(None) => match (decl.anchor(), Cursor::load(&cursor_path(reg, name))) {
            (Some(anchor), Ok(Some(c))) if c.delivered > 0 => {
                let mut p = crate::cursor::Positions::new(&c.consumer);
                p.advance(&anchor, &c.path, 0, c.seq, c.wl, c.delivered);
                Places::Held(p)
            }
            (_, Err(_)) => Places::Unreadable,
            _ => Places::Never,
        },
    };
    let positions = places.held();
    let live = liveness(reg, name);
    let mut covered = Vec::new();
    if let Ok(sel) = decl.selector() {
        for m in crate::select::resolve(&decl.sweeps(), &sel) {
            let Some(id) = m.id else { continue };
            let id_for_note = id.clone();
            let path = m.dir.join(&m.name);
            let at = positions.as_ref().and_then(|p| p.at.get(&id));
            let standing = format::read_index(&format::rings_path(&m.dir, &m.name))
                .ok()
                .map(|records| {
                    cursor::standing_at(
                        at.and_then(|a| a.chunk),
                        at.map(|a| a.wl).unwrap_or(0),
                        &records,
                    )
                });
            covered.push(Covered {
                id,
                path,
                standing,
                read: at.is_some(),
                // From the notes map, not from the position: the store
                // a consumer most needs to explain is one it has never
                // got past, which has no position at all.
                note: positions
                    .as_ref()
                    .and_then(|p| p.notes.get(&id_for_note).cloned()),
            });
        }
    }
    // Worst first, for the same reason `rank` puts the worst follower
    // first: that store decides how much is unread.
    covered.sort_by(|a, b| {
        a.read
            .cmp(&b.read)
            .then_with(|| b.behind_bytes().cmp(&a.behind_bytes()))
            .then_with(|| a.handle().cmp(&b.handle()))
    });
    Ok(Registered {
        decl,
        places,
        live,
        covered,
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

/// Every follower whose selection covers the store described by
/// `fields` — its manifest, its name and the pair's identity, as
/// `select::selectable_of` reads them.
///
/// A scan of the registry, rather than a read of one per-store directory:
/// the relation is recorded once, by the side that knows it. Cheap in
/// practice — declarations are small and change rarely — but note the
/// shape changed with selection: this is a MATCH per follower where it
/// used to be a comparison, so a caller asking about many stores reads
/// the registry once with `all` and matches per store.
pub fn for_store(reg: &Path, fields: &Map<String, Value>) -> Vec<Registered> {
    let mut mine = covering(&all(reg), fields);
    rank(&mut mine);
    mine
}

/// The whole registry, read ONCE, for a command that then asks about many
/// stores: `list` over a fleet would otherwise rescan it per store, and
/// every scan reads every follower's rings to place its position.
///
/// Deliberately not cached behind the module: a long-running writer must
/// see a registration made a second ago, so who re-reads and when stays
/// the caller's decision rather than a hidden one.
pub fn all(reg: &Path) -> Vec<Registered> {
    read_all(reg).map(|r| r.followers).unwrap_or_default()
}

/// Those of `held` whose selection covers this store, ranked. A follower
/// whose declaration will not parse covers nothing HERE — the interest
/// axis is where an unreadable declaration has to fail closed, and it
/// does; a listing that refused to render would just hide the rest.
pub fn covering(held: &[Registered], fields: &Map<String, Value>) -> Vec<Registered> {
    let mut mine: Vec<Registered> = held
        .iter()
        .filter(|r| {
            r.decl
                .selector()
                .map(|sel| sel.matches(fields))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    rank(&mut mine);
    mine
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
        let bytes = Registered::behind_bytes;
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
    /// True when the registry could not be read and the axis is holding
    /// everything as a consequence, rather than because nothing retains
    /// this store.
    ///
    /// ⚠ Both hold nothing back, so both were once reported as "nothing
    /// retains this store" — and an operator reading that while a
    /// corrupt declaration silently pinned every store on the host has
    /// been told the opposite of the truth.
    pub blind: bool,
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

/// One retaining follower, as the interest axis sees it: what it covers,
/// and where it stands in each store it has read.
struct Retaining {
    name: String,
    selector: crate::select::Selector,
    /// Store identity -> the chunk it has consumed up to. A store with no
    /// entry has never been read by this follower, so it holds all of it
    /// — the same rule as a follower that has never run, applied per
    /// store, and the state a registry exists to be able to express.
    at: HashMap<String, Option<u64>>,
}

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
    /// Every retaining follower. `None` means fail closed for EVERY
    /// store: see `read`.
    ///
    /// Not grouped by store any more, because a selection cannot be: the
    /// question "which followers hold this store" is answered by matching
    /// each selector against the store, which is why the axis is now
    /// handed the store's fields rather than its anchor.
    retaining: Option<Vec<Retaining>>,
}

impl InterestIndex {
    /// Fail closed for every store, without reading anything — what a
    /// caller uses when it has no registry to consult.
    pub fn closed() -> InterestIndex {
        InterestIndex { retaining: None }
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
                    retaining: Some(Vec::new()),
                }
            }
            Err(_) => return InterestIndex::closed(),
        };
        let mut retaining: Vec<Retaining> = Vec::new();
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
            // A selector that will not parse might have covered any
            // store, so it fails closed for all of them — the same rule
            // as an unreadable declaration, for the same reason.
            let Ok(selector) = decl.selector() else {
                return InterestIndex::closed();
            };
            let Some(at) = positions_of(reg, &name, &decl) else {
                return InterestIndex::closed();
            };
            retaining.push(Retaining { name, selector, at });
        }
        InterestIndex {
            retaining: Some(retaining),
        }
    }

    /// What the store described by `fields` may drop by interest.
    /// `next_seq` is the number the store will give its NEXT chunk —
    /// everything it has ever written is below it.
    ///
    /// `fields` is what a selector matches on, handed IN: the caller is
    /// the store and has just read its own manifest for its retention
    /// policy, so the axis stays a function of what it is given.
    pub fn for_store(&self, fields: &Map<String, Value>, next_seq: u64) -> Interest {
        let Some(all) = &self.retaining else {
            return Interest {
                blind: true,
                ..Interest::default()
            };
        };
        // A store's identity is what a position is keyed by, so a store
        // that has none can hold nothing back — there is no position that
        // could name it. Such a pair is not a store (see `bark`).
        let id = fields.get("id").and_then(Value::as_str);
        let followers: Vec<(&String, Option<u64>)> = all
            .iter()
            .filter(|r| r.selector.matches(fields))
            .map(|r| {
                (
                    &r.name,
                    // No entry for this store: never read, holds all of
                    // it. Same for a store with no identity to look up.
                    id.and_then(|i| r.at.get(i).copied()).flatten(),
                )
            })
            .collect();
        if followers.is_empty() {
            // Nothing retains this store: the axis holds nothing, and
            // there is nobody to name in a loss record.
            return Interest::default();
        }
        let retaining = followers.len();
        // The minimum over the set, with two ways to be zero. A follower
        // that has never run holds everything, and one claiming a chunk
        // the store has never written is a wrong anchor or a hand-edit --
        // newly PROVABLE, where a future timestamp was indistinguishable
        // from clock skew. Both mean: drop nothing, and name that one.
        let mut floor: Option<u64> = None;
        for (name, at) in &followers {
            match at {
                None => {
                    return Interest {
                        floor: None,
                        holder: Some((*name).clone()),
                        holder_at: None,
                        retaining,
                        blind: false,
                    }
                }
                Some(seq) if *seq >= next_seq => {
                    return Interest {
                        floor: None,
                        holder: Some((*name).clone()),
                        holder_at: Some(*seq),
                        retaining,
                        blind: false,
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
                .map(|(n, _)| (*n).clone())
        });
        Interest {
            floor,
            holder,
            holder_at: floor,
            retaining,
            blind: false,
        }
    }
}

/// Where one retaining follower stands in each store it has read:
/// identity -> the chunk consumed up to.
///
/// `positions.json` is a selection's places; a `cursor.json` is the one
/// store an older declaration named, so its position belongs to that
/// store. `None` means neither could be READ — which is not a position,
/// and the follower holds everything until it can be read again.
fn positions_of(
    reg: &Path,
    name: &str,
    decl: &Declaration,
) -> Option<HashMap<String, Option<u64>>> {
    let mut out = HashMap::new();
    let positions = positions_path(reg, name);
    if positions.exists() {
        let held = cursor::Positions::load(&positions).ok()?;
        for (id, at) in held.map(|p| p.at).unwrap_or_default() {
            out.insert(id, at.chunk);
        }
        return Some(out);
    }
    let cursor = match Cursor::load(&cursor_path(reg, name)) {
        Ok(c) => c,
        Err(_) => return None,
    };
    // No file at all is a follower that has never run: no entry for any
    // store, so it holds every store it covers. That is the state a
    // registry exists to express, and it is not a failure.
    if let (Some(c), Some(anchor)) = (cursor, decl.anchor()) {
        out.insert(anchor, c.seq);
    }
    Some(out)
}

/// The fields a store's followers are matched against. Derived, not
/// cached here: a label change is a manifest change, and callers pair
/// this with the retention policy, which is re-read on the same stat.
pub fn subject_of(dir: &Path, name: &str) -> Map<String, Value> {
    crate::select::selectable_of(dir, name)
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
        fields: &Map<String, Value>,
        next_seq: u64,
    ) -> Interest {
        if !policy.unconsumed {
            return Interest::default();
        }
        self.index
            .get_or_insert_with(|| InterestIndex::read(&registry_dir()))
            .for_store(fields, next_seq)
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
    /// WHICH stores, as a predicate. `--store` is the sugar that turns
    /// one store into `[id=<its id>]`, resolved and minted here.
    pub select: Option<String>,
    /// The store to follow: a path, or a forest handle. Sugar for a
    /// one-term selection.
    pub store: Option<PathBuf>,
    pub retaining: bool,
    pub follow_from: Option<crate::ship::FollowFrom>,
    pub enable: bool,
    pub start: bool,
    pub dry_run: bool,
    /// The consumer and its arguments, verbatim (everything after `--`).
    pub command: Vec<String>,
}

/// `timberfs follower create`: register a follower.
///
/// Refused if the name is taken, so a collision is a registration error
/// rather than two processes overwriting one position.
pub fn cmd_create(name: &str, opts: CreateOpts) -> anyhow::Result<()> {
    validate_name(name)?;
    if opts.command.is_empty() {
        bail!(
            "no consumer to feed: give the program and its arguments after `--`, e.g. \
             `-- timber-otlp --endpoint http://collector:4318`"
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

    let (select, look_in) = match (&opts.select, &opts.store) {
        (Some(_), Some(_)) => bail!(
            "--select names a SET and --store names one store, which is the one-term case of \
             it: give one of them"
        ),
        (None, None) => bail!(
            "no selection: --select '[k=v]' for the stores a predicate matches, `[]` for every \
             one, or --store <store> for exactly one"
        ),
        (Some(expr), None) => (crate::select::canonical(expr)?, Vec::new()),
        // Identity, not address: a store can move, so one named store is
        // recorded by its `.bark` id, and one minted here when it has
        // none. A SELECTION mints nothing — a store it merely matched is
        // not one the operator named.
        (None, Some(store)) => {
            let store = crate::forest::resolve_source(store)?;
            let (sdir, sname) = resolve_backing(&store)?;
            if !format::rings_path(&sdir, &sname).exists() {
                bail!(
                    "no timberfs store {sname} in {} — a follower follows stores that exist",
                    sdir.display()
                );
            }
            let bark = crate::bark::ensure_identified(&sdir, &sname).with_context(|| {
                format!(
                    "declaring an identity for {} (needs write access to its backing \
                     directory): a follower records its stores by identity, not by path, \
                     because a store can move",
                    store.display()
                )
            })?;
            let anchor = cursor::store_anchor(&sdir, &sname, Some(&bark));
            if anchor.starts_with("path:") {
                bail!(
                    "{} has no declared identity and one could not be minted — a follower \
                     cannot be anchored to a path, a store being movable",
                    store.display()
                );
            }
            // ⚠ A store need not be in a forest — a backing directory
            // under a mount is the normal shape — and a follower sweeps
            // forests. So where the named store is not reachable through
            // one, its DIRECTORY is recorded as a place to look, beside
            // the identity that says which store to look FOR. Without it
            // the follower resolves nothing and reports, wrongly, that
            // its store had not appeared yet.
            let sel = format!("[id={anchor}]");
            let look_in = place_to_look(&sel, &sdir);
            (sel, look_in)
        }
    };

    let decl = Declaration {
        name: name.to_string(),
        select,
        retaining: opts.retaining,
        command: opts.command,
        follow_from: opts.follow_from,
        look_in,
        created: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        extra: Map::new(),
    };

    // What it covers NOW, which is not what it will cover: a selection is
    // re-resolved every poll, so this is the operator's check on their
    // predicate rather than a list the declaration holds.
    let covers = decl
        .selector()
        .map(|sel| crate::select::resolve(&decl.sweeps(), &sel))
        .unwrap_or_default();
    let (identified, idless): (Vec<_>, Vec<_>) = covers.iter().partition(|m| m.id.is_some());

    if opts.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(decl.to_map()))?
        );
        report_coverage(&decl, &identified, &idless);
        println!("would run: {}", decl.command.join(" "));
        println!("dry run: nothing registered");
        return Ok(());
    }
    decl.save(&reg)?;
    crate::note!(
        "timberfs: registered follower {name} on {} ({} store(s) now)",
        decl.select,
        identified.len()
    );
    report_coverage(&decl, &identified, &idless);
    // The footgun, in one line, at the moment it is created — the same
    // one Postgres has with an unused slot. A retaining follower holds
    // EVERYTHING of a store it has not read, which is the point (it
    // protects a follower deployed before it first runs) and also the
    // trap.
    if decl.retaining {
        crate::note!(
            "timberfs: {name} is retaining: once a writer honours it, it holds each covered \
             store whole until it first reads it. `--start` (or `systemctl start {}`) makes \
             the safe path the easy one",
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

/// What a selection covers today, and what it cannot follow.
///
/// ⚠ A selection matching NOTHING is not refused: a follower is
/// routinely declared before the stores exist — an archive with
/// `--auto-create` receives a new sender's data and forwards it nowhere
/// until somebody registers one — so refusing would make the right order
/// impossible. It is said, not prevented.
fn report_coverage(
    decl: &Declaration,
    identified: &[&crate::select::Match],
    idless: &[&crate::select::Match],
) {
    if identified.is_empty() {
        crate::note!(
            "timberfs: {} matches no store with an identity yet — nothing is followed until \
             one appears, which is a legitimate order to do this in",
            decl.select
        );
    } else if decl.retaining {
        crate::note!(
            "timberfs: retaining {} store(s): {}",
            identified.len(),
            identified
                .iter()
                .take(6)
                .map(|m| m.handle.as_str())
                .collect::<Vec<_>>()
                .join(", ")
                + if identified.len() > 6 { ", …" } else { "" }
        );
    }
    if !idless.is_empty() {
        crate::note!(
            "timberfs: {} matched store(s) carry no identity, so they are NOT followed: {} \
             (`timberfs identity <store> --mint` gives one)",
            idless.len(),
            idless
                .iter()
                .take(6)
                .map(|m| m.handle.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

const COLUMNS: [&str; 6] = ["NAME", "FOLLOWS", "STORES", "RETAINING", "LAG", "RUNNING"];

fn row_cells(r: &Registered) -> Vec<String> {
    vec![
        r.name().to_string(),
        r.follows_text(),
        r.covered.len().to_string(),
        if r.decl.retaining { "yes" } else { "no" }.to_string(),
        r.lag_text(),
        r.live.text().to_string(),
    ]
}

pub fn to_json(r: &Registered) -> Value {
    let mut o = Map::new();
    o.insert("name".into(), r.name().into());
    o.insert("select".into(), r.decl.select.clone().into());
    o.insert(
        "command".into(),
        Value::Array(r.decl.command.iter().cloned().map(Value::String).collect()),
    );
    o.insert("retaining".into(), r.decl.retaining.into());
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
    o.insert("lag".into(), r.lag_text().into());
    o.insert("holds_everything".into(), r.holds_everything().into());
    // Recorded places for stores no longer covered. An ARRAY always, so
    // a consumer reads the same shape whether there are any or not, and
    // `on_disk` separates "left the selection" from "deleted".
    o.insert(
        "uncovered".into(),
        Value::Array(
            r.uncovered()
                .into_iter()
                .map(|(id, here)| {
                    let mut e = Map::new();
                    e.insert("id".into(), id.into());
                    e.insert("on_disk".into(), here.into());
                    Value::Object(e)
                })
                .collect(),
        ),
    );
    // ⚠ `None` is "the positions file could not be read", which is not
    // "nothing consumed" — so it is null and not an empty object, and a
    // consumer tests for a value rather than for a key.
    o.insert(
        "positions".into(),
        match &r.places {
            Places::Never => Value::String("never".into()),
            Places::Unreadable => Value::String("unreadable".into()),
            Places::Held(_) => Value::String("held".into()),
        },
    );
    o.insert(
        "stores".into(),
        Value::Array(r.covered.iter().map(covered_json).collect()),
    );
    Value::Object(o)
}

/// One covered store. The same keys whether or not it has been read, so
/// a consumer tests for a VALUE rather than for a key's presence.
fn covered_json(c: &Covered) -> Value {
    let mut o = Map::new();
    o.insert("id".into(), c.id.clone().into());
    o.insert("path".into(), c.path.display().to_string().into());
    o.insert("read".into(), c.read.into());
    match &c.standing {
        None => {
            for k in [
                "consumed_chunks",
                "behind_chunks",
                "behind_bytes",
                "behind_ms",
                "gap_chunks",
            ] {
                o.insert(k.into(), Value::Null);
            }
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
    match &c.note {
        None => o.insert("note".into(), Value::Null),
        Some(n) => {
            let mut nn = Map::new();
            nn.insert("text".into(), n.text.clone().into());
            nn.insert(
                "offset".into(),
                n.offset.map(Into::into).unwrap_or(Value::Null),
            );
            nn.insert("when".into(), n.when.clone().into());
            o.insert("note".into(), Value::Object(nn))
        }
    };
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
        let fields = subject_of(&dir, &name);
        followers.retain(|r| {
            r.decl
                .selector()
                .map(|sel| sel.matches(&fields))
                .unwrap_or(false)
        });
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
    println!("follows   {}", r.decl.select);
    println!("command   {}", r.decl.command.join(" "));
    print_retaining(&r);
    match &r.places {
        Places::Unreadable => println!(
            "positions UNREADABLE ({}) — which is not \"nothing consumed\": nothing below can \
             be trusted, and the interest axis fails closed for every store while it lasts",
            positions_path(&reg, name).display()
        ),
        Places::Never => println!("positions none — it has never read anything"),
        Places::Held(_) => {}
    }
    println!("stores    {}", r.covered.len());
    if r.covered.is_empty() {
        println!(
            "          the selection matches nothing right now, which is a legitimate state: \
             a follower may be declared before its stores exist"
        );
    }
    // Worst first, so the store that decides how much is unread is the
    // one an operator sees first.
    for c in &r.covered {
        let place = match (&c.standing, c.read) {
            (_, false) => "never read".to_string(),
            (Some(st), true) => st.lag_text(),
            (None, true) => "store unreadable".to_string(),
        };
        println!("  {:<24} {}", c.handle(), place);
        if let Some(st) = &c.standing {
            if let Some(n) = st.gap_chunks {
                println!(
                    "  {:<24} GAP — {n} chunk(s) were dropped before it read them; it resumes \
                     at the oldest one still here",
                    ""
                );
            // ⚠ Not at the live edge: the chunk a position sits INSIDE
            // counts as unread, because a chunk-granular floor cannot
            // say a chunk is finished while the position is in it. So a
            // follower that has read everything still shows its current
            // chunk as a backlog, and printing that beside "at the live
            // edge" reads as a contradiction. `info`'s own block has
            // guarded this since it was written.
            } else if st.behind_chunks > 0 && !st.at_live_edge() {
                println!(
                    "  {:<24} {} unread in {} chunk(s)",
                    "",
                    crate::rotate::human_bytes(st.behind_bytes),
                    st.behind_chunks
                );
            }
        }
        // The consumer's own words about this store, and the offset is
        // an ADDRESS — the same one `timberview` opens at, so a note
        // names a place an operator can go and look.
        //
        // ⚠ A store and an offset rather than a `timber://host/...` URL:
        // the host in such an address is whatever name the READER
        // reaches this machine by, and this machine does not know it.
        // `gethostname()` is conventionally the short name, which is why
        // two hosts in different environments present the same one —
        // docs/plans/receiving-end.md calls that door 4. Composing the
        // URL is the reader's job; ours is to say which store and where.
        if let Some(n) = &c.note {
            println!("  {:<24} note: {}", "", n.text);
            if let Some(off) = n.offset {
                println!("  {:<24}   at offset {off} of {}", "", c.handle());
            }
        }
    }
    // Every recorded place is accounted for, not just the covered ones:
    // the file holds a place per store this has EVER read, so a reader
    // comparing `stores` against it would otherwise find more entries
    // than are explained anywhere.
    let stale = r.uncovered();
    if !stale.is_empty() {
        let gone = stale.iter().filter(|(_, here)| !here).count();
        println!(
            "places    {} more, for store(s) this no longer covers{}",
            stale.len(),
            if gone > 0 {
                format!(" — {gone} of them no longer on disk at all")
            } else {
                String::new()
            }
        );
        println!(
            "          each keeps its place, so a store that comes back into the selection \
             resumes rather than re-ships; nothing prunes them"
        );
    }
    if let Some(n) = r.places.held().and_then(|p| p.note.as_ref()) {
        println!("note      {} ({})", n.text, n.when);
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
/// whether the stores it covers declare that they honour it. Stating the
/// flag alone would be a half-truth: a declared interest no writer reads
/// holds nothing back.
fn print_retaining(r: &Registered) {
    if !r.decl.retaining {
        println!("retaining no — it holds nothing back; retention ignores its positions");
        return;
    }
    let honouring = r
        .covered
        .iter()
        .filter(|c| {
            resolve_backing(&c.path)
                .ok()
                .and_then(|(dir, name)| crate::bark::load(&dir, &name))
                .and_then(|m| m.get("retain_unconsumed").and_then(Value::as_bool))
                .unwrap_or(false)
        })
        .count();
    if honouring == 0 {
        println!(
            "retaining yes — declared, but no covered store declares retain_unconsumed, so \
             nothing honours it yet"
        );
    } else if honouring == r.covered.len() {
        println!(
            "retaining yes — its positions hold every covered store's head back, until \
             retain_size overrides it"
        );
    } else {
        println!(
            "retaining yes — honoured by {honouring} of {} covered store(s); the rest do not \
             declare retain_unconsumed",
            r.covered.len()
        );
    }
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

/// The keys `update` will change. `store` takes a path or handle and is
/// re-resolved to a one-term selection; `select` takes a predicate. The
/// CONSUMER is replaced wholesale with what follows `--`, because a
/// command is an argv and editing one member of it by key is a shape
/// that reads correctly right up until the arguments matter.
const SETTABLE: &[&str] = &["retaining", "select", "store", "follow_from"];

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
            // ⚠ A place to look belonged to the STORE `--store` named.
            // A predicate is about forests, so keeping it would sweep a
            // directory nobody asked about — and silently pick up a
            // store there that no forest holds.
            "select" => {
                decl.select = crate::select::canonical(v)?;
                decl.look_in = Vec::new();
            }
            "follow_from" => decl.follow_from = Some(crate::ship::FollowFrom::parse(v)?),
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
                // Re-pointing a follower at a DIFFERENT store leaves the
                // old store's position in the file. Harmless — a
                // position is keyed by identity, so the new store simply
                // has none and is read from the start — but worth saying,
                // because the old entry then holds retention for a store
                // nothing follows any more.
                if Some(&anchor) != decl.anchor().as_ref()
                    && before.places.held().is_some_and(|p| !p.at.is_empty())
                {
                    crate::note!(
                        "timberfs: {name} keeps its place in the store(s) it used to follow, in \
                         {} — the new one is read from the start, and a stale entry there \
                         holds nothing (membership is the selection's, never the file's)",
                        positions_path(&reg, name).display()
                    );
                }
                decl.select = format!("[id={anchor}]");
                // Re-pointed, so the place to look is re-derived: the new
                // store may be outside every forest where the old one was
                // not, or the other way round.
                decl.look_in = place_to_look(&decl.select, &sdir);
            }
            "name" | "created" => {
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
            "retaining" => decl.retaining = false,
            // Back to the derived default, which depends on retaining.
            "follow_from" => decl.follow_from = None,
            other => bail!("{other:?} cannot be unset"),
        }
    }
    if let Some(a) = args {
        if a.is_empty() {
            bail!("`--` with nothing after it would leave no consumer to feed");
        }
        decl.command = a;
    }
    if decl == before.decl {
        crate::note!("timberfs: {name} already declares that; nothing written");
        return Ok(());
    }
    // Releasing the head is the part worth quantifying, and the part
    // whose asymmetry is easy to miss: the FLAG toggles, its EFFECT does
    // not. Setting it back to true will not bring dropped data back.
    let releasing = before.decl.retaining && !decl.retaining;
    if releasing {
        let held: u64 = before.behind_bytes();
        let unread = before.covered.iter().filter(|c| !c.read).count();
        if held > 0 || unread > 0 {
            crate::note!(
                "timberfs: {name} releases the head of {} store(s) — {} it alone was holding \
                 becomes droppable{}",
                before.covered.len(),
                crate::rotate::human_bytes(held),
                if unread > 0 {
                    format!(", including {unread} it had never read at all")
                } else {
                    String::new()
                }
            );
        } else {
            crate::note!("timberfs: {name} releases the head (it was holding nothing)");
        }
        crate::note!(
            "timberfs: this does not undo: setting retaining=true again will not bring dropped \
             data back, and {name} then resumes at a position that may be gapped"
        );
    }
    // A CHANGED SELECTION moves heads too, per store, and this was the
    // one edit that said nothing about it — where `retaining=false`
    // quantifies its release and `create --retaining` names what it
    // holds. A silent acquire fills a disk; a silent release drops data.
    if decl.select != before.decl.select {
        report_membership_change(&before, &decl);
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

/// What a changed selection does to the stores on either side of it.
///
/// Said for every follower, because which stores are read is the point of
/// a selection — and for a RETAINING one the leavers' bytes are quantified,
/// since that release is the same one `update retaining=false` is careful
/// to put a number on.
///
/// ⚠ A leaver KEEPS its recorded place: membership is the selection's,
/// never the file's, so bringing it back resumes rather than re-ships. Said
/// here because the opposite is the natural guess, and guessing wrong means
/// an operator avoids an edit that was safe.
fn report_membership_change(before: &Registered, decl: &Declaration) {
    let Ok(sel) = decl.selector() else { return };
    let after = crate::select::resolve(&decl.sweeps(), &sel);
    let (was, now): (Vec<&str>, Vec<&str>) = (
        before.covered.iter().map(|c| c.id.as_str()).collect(),
        after.iter().filter_map(|m| m.id.as_deref()).collect(),
    );
    let joining: Vec<&crate::select::Match> = after
        .iter()
        .filter(|m| m.id.as_deref().is_some_and(|i| !was.contains(&i)))
        .collect();
    let leaving: Vec<&Covered> = before
        .covered
        .iter()
        .filter(|c| !now.contains(&c.id.as_str()))
        .collect();
    if joining.is_empty() && leaving.is_empty() {
        crate::note!(
            "timberfs: {} still covers the same {} store(s)",
            decl.name,
            after.len()
        );
        return;
    }
    if !joining.is_empty() {
        // The handle, not the file name: `Covered::handle` strips `.log`
        // for the leavers, and one message must not spell a store two ways.
        let names: Vec<&str> = joining.iter().map(|m| m.handle.as_str()).collect();
        crate::note!(
            "timberfs: {} now also covers {} store(s): {}{}",
            decl.name,
            joining.len(),
            names.join(", "),
            if decl.retaining {
                " — and holds each of them whole until it has read it"
            } else {
                ""
            }
        );
    }
    if !leaving.is_empty() {
        let names: Vec<String> = leaving.iter().map(|c| c.handle()).collect();
        crate::note!(
            "timberfs: {} no longer covers {} store(s): {}",
            decl.name,
            leaving.len(),
            names.join(", ")
        );
        if before.decl.retaining && decl.retaining {
            let freed: u64 = leaving.iter().map(|c| c.behind_bytes()).sum();
            let unread = leaving.iter().filter(|c| !c.read).count();
            crate::note!(
                "timberfs: that releases their heads — {} it was holding becomes droppable{}",
                crate::rotate::human_bytes(freed),
                if unread > 0 {
                    format!(", including {unread} it had never read at all")
                } else {
                    String::new()
                }
            );
        }
        crate::note!(
            "timberfs: each keeps its recorded place, so bringing it back into the selection \
             resumes rather than re-ships"
        );
    }
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

/// `timberfs follower delete NAME`: bookkeeping, once the head has been
/// released.
///
/// Refused while `retaining=true` (set it false first, and see what that
/// frees) and while the follower is RUNNING — deleting under a live
/// process would leave it writing an unlinked positions file, silently doing
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
        if r.decl.retaining && !r.covered.is_empty() {
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
             positions file, doing nothing at all. Stop it first (`--stop`, or \
             `systemctl stop {}`)",
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

/// `timberfs follower run NAME`: read the declaration, take the lock,
/// and RUN THE LOOP — reading the selection, feeding the declared
/// consumer, and moving the positions as far as it says.
///
/// ⚠ It no longer EXECs. It used to become the shipper, which is why the
/// lock's FD_CLOEXEC was cleared: the lock had to survive the exec. Now
/// the consumer is a CHILD, and clearing it would hand that child a lock
/// on the follower for its whole life — the grandchild-holds-the-lock
/// hazard `liveness` exists to detect, created deliberately. So the flag
/// stays as Rust set it, and the lock dies with this process, which is
/// what it is a statement about.
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
    let selector = decl.selector()?;

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

    migrate_cursor(&reg, name, &decl)?;
    crate::note!(
        "timberfs: {name} following {} with {}",
        decl.select,
        decl.command.join(" ")
    );
    let held = lock;
    let result = crate::feed::run(crate::feed::Opts {
        selector,
        dirs: decl.sweeps(),
        positions: Some(positions_path(&reg, name)),
        batch_entries: crate::ship::BATCH_ENTRIES,
        poll: std::time::Duration::from_secs(1),
        follow: true,
        argv: decl.command.clone(),
        hello_wait: crate::feed::HELLO_WAIT,
        max_silence: crate::feed::MAX_SILENCE,
        follow_from: decl.picks_up(),
        // When the follower was DECLARED, not when its positions file
        // was first written: one declared before its stores exist is the
        // case `discovery` is for, and comparing against the file would
        // skip every store born between the declaration and the first
        // start.
        since: Some(decl.created.clone()),
    });
    // Held until the loop is done, so liveness is true for exactly as
    // long as something is following.
    drop(held);
    result
}

/// A declaration written before positions existed kept a `cursor.json`
/// for the one store it named. Seed the positions file from it once, so a
/// follower that has been shipping for months does not start again from
/// the beginning.
///
/// ⚠ The offset is the START of the chunk the cursor stood in, not the
/// exact entry: a cursor holds `(seq, n)` and an offset is a byte, and
/// there is no conversion between them that does not read the store. So
/// up to one chunk is re-delivered — at-least-once, which is what a
/// chunk-granular position always did across a restart anyway.
fn migrate_cursor(reg: &Path, name: &str, decl: &Declaration) -> anyhow::Result<()> {
    let positions = positions_path(reg, name);
    if positions.exists() {
        return Ok(());
    }
    let Some(anchor) = decl.anchor() else {
        return Ok(());
    };
    let Some(c) = Cursor::load(&cursor_path(reg, name))? else {
        return Ok(());
    };
    if c.delivered == 0 {
        return Ok(());
    }
    let Some(m) = crate::select::resolve(&decl.sweeps(), &decl.selector()?)
        .into_iter()
        .next()
    else {
        return Ok(());
    };
    let records = format::read_index(&format::rings_path(&m.dir, &m.name)).unwrap_or_default();
    let dropped = crate::query::dropped_bytes_of(&m.dir.join(&m.name));
    let offset = match c.seq.and_then(|seq| records.iter().find(|r| r.seq == seq)) {
        Some(chunk) => dropped + chunk.uncomp_start,
        // Its chunk is gone: retention overtook it, so it resumes at
        // whatever is now oldest, which is what it would have done
        // before this too.
        None => dropped + records.first().map(|r| r.uncomp_start).unwrap_or(0),
    };
    let mut held = crate::cursor::Positions::new(&c.consumer);
    held.advance(
        &anchor,
        &m.dir.join(&m.name).display().to_string(),
        offset,
        c.seq,
        c.wl,
        c.delivered,
    );
    held.save(&positions)?;
    crate::note!(
        "timberfs: {name}: carried its cursor into {} at offset {offset} (chunk {}) — up to \
         one chunk may be re-delivered, which is what a restart always did",
        positions.display(),
        c.seq.map(|s| s.to_string()).unwrap_or_else(|| "-".into())
    );
    Ok(())
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

    /// A store as the interest axis and a listing see it: what a
    /// selector matches against.
    fn store_fields(id: &str, labels: &[(&str, &str)]) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(id.to_string()));
        for (k, v) in labels {
            m.insert(k.to_string(), Value::String(v.to_string()));
        }
        m
    }

    fn decl(name: &str, retaining: bool) -> Declaration {
        Declaration {
            name: name.to_string(),
            select: "[id=store-id]".into(),
            retaining,
            command: vec![
                "timber-otlp".into(),
                "--endpoint".into(),
                "http://127.0.0.1:4318".into(),
            ],
            follow_from: None,
            look_in: Vec::new(),
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
    fn a_declaration_must_name_a_selection_and_a_consumer() {
        let reg = scratch("incomplete");
        let write = |name: &str, body: &str| {
            fs::create_dir_all(follower_dir(&reg, name)).unwrap();
            fs::write(decl_path(&reg, name), body).unwrap();
        };
        write("nostore", r#"{"type":"otlp"}"#);
        assert!(Declaration::load(&reg, "nostore")
            .unwrap_err()
            .to_string()
            .contains("no \"select\""));
        // A selector that will not parse is refused where it is READ, so
        // nothing downstream has to cope with one.
        write("bad", r#"{"type":"otlp","select":"host~web01"}"#);
        assert!(Declaration::load(&reg, "bad").is_err());
        // A store and no command: nothing to run, and the message says
        // what to give it rather than naming a type list.
        write("notype", r#"{"store":"x"}"#);
        let err = Declaration::load(&reg, "notype").unwrap_err().to_string();
        assert!(err.contains("command"), "{err}");
        write("garbage", "not json");
        assert!(Declaration::load(&reg, "garbage").is_err());
        // Missing is an error too: a follower with no declaration is a
        // registry that cannot be trusted to answer, not an empty policy.
        assert!(Declaration::load(&reg, "absent").is_err());
        fs::remove_dir_all(&reg).ok();
    }

    /// ⚠ The positions file holds a place per store the follower has EVER
    /// read, and nothing prunes it — so a reader comparing `stores`
    /// against that file finds entries nothing explains. Three states live
    /// in there and only one of them is `covered`; the other two are
    /// derived here, because whether a store is covered is the SELECTOR's
    /// answer and a changed selector does not touch the file.
    #[test]
    fn every_recorded_place_is_accounted_for_not_just_the_covered_ones() {
        let reg = scratch("uncovered");
        let root = std::env::temp_dir().join(format!("tfs-unc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let live = root.join("live");
        fs::create_dir_all(&live).unwrap();
        // A pair on disk, so `on_disk` has something true to report. The
        // recorded path is the store's LOGICAL name, which is never a
        // file — testing it directly answers false for a live store.
        fs::write(live.join("a.log.rings"), b"").unwrap();
        fs::write(live.join("a.log.trunk"), b"").unwrap();

        let mut held = crate::cursor::Positions::new("c");
        held.advance(
            "kept-and-here",
            &live.join("a.log").display().to_string(),
            10,
            Some(0),
            1,
            1,
        );
        held.advance("kept-but-gone", "/nonexistent/b.log", 20, Some(0), 1, 1);
        held.advance("covered", "/wherever/c.log", 30, Some(0), 1, 1);
        let r = Registered {
            decl: decl("f", false),
            places: Places::Held(held),
            live: Liveness::Stopped,
            covered: vec![Covered {
                id: "covered".into(),
                path: PathBuf::from("/wherever/c.log"),
                standing: None,
                read: true,
                note: None,
            }],
        };
        let mut unc = r.uncovered();
        unc.sort();
        assert_eq!(
            unc,
            vec![
                ("kept-and-here".to_string(), true),
                ("kept-but-gone".to_string(), false)
            ],
            "the covered store must not be listed, and the pair decides on_disk"
        );
        // Unreadable places say nothing about anything, so they list none
        // rather than listing every store as uncovered.
        let blind = Registered {
            places: Places::Unreadable,
            ..r
        };
        assert!(blind.uncovered().is_empty());
        let _ = fs::remove_dir_all(&root);
        fs::remove_dir_all(&reg).ok();
    }

    /// ⚠ A store need NOT be in a forest, and a follower sweeps forests.
    /// So `--store` on one outside every forest records its directory as
    /// a place to look; without that the follower resolved nothing and
    /// reported that its store had not appeared *yet*, forever — while a
    /// `--retaining` one silently deleted freely, there being nothing it
    /// could see to release. Measured against the VM suite, whose stores
    /// live in a backing directory under a mount.
    #[test]
    fn a_store_outside_every_forest_is_still_reachable() {
        let reg = scratch("outside");
        let root = std::env::temp_dir().join(format!("tfs-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (backing, forest) = (root.join("backing"), root.join("forest"));
        fs::create_dir_all(&backing).unwrap();
        fs::create_dir_all(&forest).unwrap();

        let outside = Declaration {
            name: "out".into(),
            select: "[id=x]".into(),
            retaining: false,
            command: vec!["true".into()],
            follow_from: None,
            look_in: vec![backing.display().to_string()],
            created: String::new(),
            extra: Map::new(),
        };
        // The recorded place is swept BESIDE the configured forests, not
        // instead of them: a non-empty directory list replaces them, so
        // adding one place to look would otherwise remove every other.
        let swept = crate::forest::forest_dirs();
        let with = outside.sweeps();
        for d in &swept {
            assert!(
                with.contains(d),
                "{} was dropped from the sweep",
                d.display()
            );
        }
        assert!(with.contains(&backing));

        // And a declaration with no `look_in` sweeps exactly the forests,
        // so the common case carries no path at all.
        let inside = Declaration {
            look_in: Vec::new(),
            ..outside
        };
        assert_eq!(inside.sweeps(), swept);
        // It is not written when it is empty, so an in-forest
        // declaration reads as one.
        assert!(!inside.to_map().contains_key("look_in"));
        let _ = fs::remove_dir_all(&root);
        fs::remove_dir_all(&reg).ok();
    }

    /// A TYPE is not migrated into a command, and the reason is the
    /// failure it would have caused: `type: otlp` would become
    /// `timber-otlp --endpoint …`, and that program was rewritten by the
    /// same change into a consumer — so an upgraded host would have got
    /// a declaration that HANGS until the hello times out. Every kind is
    /// refused by name, with the one command that fixes it.
    #[test]
    fn a_type_is_refused_by_name_and_never_migrated() {
        let reg = scratch("typed");
        for (name, body) in [
            (
                "otlp",
                r#"{"store":"id-a","type":"otlp","endpoint":"http://c:4318"}"#,
            ),
            (
                "repl",
                r#"{"store":"id-b","type":"frames","endpoint":"archive:4319"}"#,
            ),
            ("kafka", r#"{"store":"id-c","type":"kafka"}"#),
        ] {
            fs::create_dir_all(follower_dir(&reg, name)).unwrap();
            fs::write(decl_path(&reg, name), body).unwrap();
            let err = Declaration::load(&reg, name).unwrap_err().to_string();
            assert!(err.contains("type"), "{name}: {err}");
            assert!(
                err.contains("follower update"),
                "{name}: it should name the fix: {err}"
            );
        }
        fs::remove_dir_all(&reg).ok();
    }

    /// A declaration with neither a command nor a type has nothing to
    /// run, which is a broken registration and not an empty policy.
    #[test]
    fn a_declaration_with_no_command_is_refused() {
        let reg = scratch("nocmd");
        fs::create_dir_all(follower_dir(&reg, "empty")).unwrap();
        fs::write(decl_path(&reg, "empty"), r#"{"select":"[]"}"#).unwrap();
        let err = Declaration::load(&reg, "empty").unwrap_err().to_string();
        assert!(err.contains("command"), "{err}");
        fs::remove_dir_all(&reg).ok();
    }

    /// The command survives a round trip as an ARGV, not as a string: a
    /// consumer's argument holding a space must not become two.
    #[test]
    fn a_command_round_trips_as_a_list() {
        let reg = scratch("cmdlist");
        let mut d = decl("central", false);
        d.command = vec![
            "ssh".into(),
            "archive01".into(),
            "my-consumer --flag 'a b'".into(),
        ];
        d.save(&reg).unwrap();
        let back = Declaration::load(&reg, "central").unwrap();
        assert_eq!(back.command, d.command);
        fs::remove_dir_all(&reg).ok();
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
        a.select = "[id=id-a]".into();
        a.save(&reg).unwrap();
        let mut b = decl("behind", true);
        b.select = "[id=id-a]".into();
        b.save(&reg).unwrap();
        let mut other = decl("elsewhere", true);
        other.select = "[id=id-b]".into();
        other.save(&reg).unwrap();
        let mine = for_store(&reg, &store_fields("id-a", &[]));
        let names: Vec<&str> = mine.iter().map(|r| r.name()).collect();
        // No stores on disk, so every standing is empty and the ranking
        // falls back to the name — this is about WHICH followers are ours.
        assert_eq!(names, ["ahead", "behind"]);
        assert!(for_store(&reg, &store_fields("id-c", &[])).is_empty());
        fs::remove_dir_all(&reg).ok();
    }

    #[test]
    fn liveness_comes_from_the_lock_and_survives_a_cursor_rewrite() {
        let _no_forks = crate::store::fork_guard();
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
        let _no_forks = crate::store::fork_guard();
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

    /// A store never read has no position, so it has no distance from
    /// one: `behind_ms` is measured from zero and formats as the whole
    /// unix epoch. The phrase for that must be about the store and not
    /// about a clock — "20698d behind" is what an operator saw before
    /// this, on a follower that had simply not started.
    #[test]
    fn a_store_never_read_is_not_reported_as_decades_behind() {
        let mixed = Registered {
            decl: decl("central", true),
            places: Places::Held(crate::cursor::Positions::new("c")),
            live: Liveness::Stopped,
            covered: vec![
                covering_one("x", true, false, 76).covered.remove(0),
                covering_one("y", true, true, 0).covered.remove(0),
            ],
        };
        let text = mixed.lag_text();
        assert_eq!(text, "1 of 2 never read");
        // What the time formatter says, which is what must not appear.
        assert!(
            !text.contains("behind"),
            "a distance measured from a zero clock: {text}"
        );
    }

    /// A `Registered` covering a store it has never read, ranked and
    /// asked about directly — no filesystem, because the rule is about
    /// the ranking and not about where the facts came from.
    fn covering_one(name: &str, retaining: bool, read: bool, behind: u64) -> Registered {
        let mut d = decl(name, retaining);
        d.select = "[id=id]".into();
        Registered {
            decl: d,
            places: if read {
                Places::Held(crate::cursor::Positions::new(name))
            } else {
                Places::Never
            },
            live: Liveness::Stopped,
            covered: vec![Covered {
                id: "id".into(),
                path: PathBuf::from("/var/log/timberfs/app/app.log"),
                standing: Some(crate::cursor::Standing {
                    consumed_chunks: 0,
                    behind_chunks: 1,
                    behind_bytes: behind,
                    behind_ms: 0,
                    gap_chunks: None,
                }),
                read,
                note: None,
            }],
        }
    }

    #[test]
    fn a_retaining_follower_that_never_read_a_store_outranks_every_backlog() {
        // It holds the WHOLE store, not a tail of it, and has no
        // measured backlog at all — so ranking on bytes alone would sort
        // the worst case last.
        let mut both = vec![
            covering_one("behind", true, true, 9_000_000),
            covering_one("fresh", true, false, 0),
        ];
        rank(&mut both);
        assert_eq!(
            both.iter().map(|r| r.name()).collect::<Vec<_>>(),
            ["fresh", "behind"]
        );
        assert!(both[0].holds_everything());
        assert!(!both[1].holds_everything());

        // And a follower covering NOTHING holds nothing: there is no
        // store to hold, whatever the flag says.
        let mut empty = covering_one("idle", true, false, 0);
        empty.covered.clear();
        assert!(!empty.holds_everything());
        assert_eq!(empty.lag_text(), "matches nothing");
    }

    /// A selection that matches nothing outranks every other reading:
    /// "never run" would suggest a store waiting to be read, and there
    /// is none. Both are legitimate states a registry must be able to
    /// tell apart.
    #[test]
    fn a_selection_that_matches_nothing_says_so_rather_than_never_run() {
        let reg = scratch("neverrun");
        decl("central", true).save(&reg).unwrap();
        let r = read(&reg, "central").unwrap();
        assert_eq!(r.lag_text(), "matches nothing");
        assert!(r.covered.is_empty());
        assert!(
            matches!(r.places, Places::Never),
            "no positions file is NEVER RUN, which is not the same as unreadable"
        );
        // And nothing is held: there is no store to hold.
        assert!(!r.holds_everything());
        fs::remove_dir_all(&reg).ok();
    }

    /// A retaining follower of `store`, standing at `at`.
    fn retaining(reg: &Path, name: &str, store: &str, at: Option<u64>) {
        let mut d = decl(name, true);
        d.select = format!("[id={store}]");
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
        tap.select = "[id=id]".into();
        tap.save(&reg).unwrap();
        Cursor {
            seq: Some(0),
            ..Cursor::new("tap", "id", "/p")
        }
        .save(&cursor_path(&reg, "tap"))
        .unwrap();

        let held = InterestIndex::read(&reg).for_store(&store_fields("id", &[]), 100);
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
        let held = InterestIndex::read(&reg).for_store(&store_fields("id", &[]), 100);
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
        let held = InterestIndex::read(&reg).for_store(&store_fields("id", &[]), 10);
        assert_eq!(held.floor, None);
        assert_eq!(held.holder.as_deref(), Some("bogus"));
        assert_eq!(held.holder_at, Some(500));
        // One below next_seq is a legal position, and drops accordingly.
        let ok = InterestIndex::read(&reg).for_store(&store_fields("id", &[]), 501);
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
        assert_eq!(absent.for_store(&store_fields("id", &[]), 100).floor, None);

        // A registry with nothing in it, and one with nothing for us.
        retaining(&reg, "elsewhere", "other-store", Some(3));
        let held = InterestIndex::read(&reg).for_store(&store_fields("id", &[]), 100);
        assert_eq!(held.floor, None);
        assert_eq!(held.holder, None, "nobody to name, so no loss record");

        // A follower of ours whose position cannot be read holds
        // everything: an unreadable position is not a position.
        retaining(&reg, "torn", "id", None);
        fs::write(cursor_path(&reg, "torn"), "{ not json").unwrap();
        assert_eq!(
            InterestIndex::read(&reg)
                .for_store(&store_fields("id", &[]), 100)
                .floor,
            None
        );

        // An unreadable DECLARATION fails closed for every store, not just
        // its own: it might have been a retaining follower of any of them,
        // and there is no way to know which.
        fs::write(decl_path(&reg, "torn"), "{ not json").unwrap();
        let broken = InterestIndex::read(&reg);
        assert_eq!(broken.for_store(&store_fields("id", &[]), 100).floor, None);
        assert_eq!(
            broken
                .for_store(&store_fields("other-store", &[]), 100)
                .floor,
            None
        );
        fs::remove_dir_all(&reg).ok();
    }

    /// ⚠ Fail-closed and nothing-retains-this hold the same amount back
    /// — nothing — so they were once one message, and an operator reading
    /// «nothing retains this store» while a corrupt declaration silently
    /// pinned every store on the host was told the opposite of the truth.
    /// `blind` is the difference, and it is what `trim` reports.
    #[test]
    fn a_registry_that_cannot_be_read_says_so_rather_than_nothing_retains_this() {
        let fields = store_fields("store-id", &[]);
        let blind = InterestIndex::closed().for_store(&fields, 10);
        assert!(blind.blind, "a closed index is blind, not empty");
        assert_eq!(blind.floor, None, "and it still holds everything back");
        assert_eq!(blind.holder, None, "with nobody to name");

        // An empty registry is a FACT, not a gap: nothing is registered,
        // so nothing is held, and that is not blindness.
        let reg = scratch("empty-registry");
        fs::remove_dir_all(&reg).ok();
        let seeing = InterestIndex::read(&reg).for_store(&fields, 10);
        assert!(!seeing.blind);
        assert_eq!(seeing.floor, None);
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
        let fields = store_fields("id", &[]);
        assert_eq!(tick.floor(&off, &fields, 100), Interest::default());
        assert!(tick.index.is_none(), "nothing declared it, so nothing read");

        let on = crate::bark::Retention {
            max_comp_bytes: Some(1024),
            unconsumed: true,
            ..Default::default()
        };
        tick.floor(&on, &fields, 100);
        assert!(tick.index.is_some(), "declared, so read once");
    }

    /// One retaining declaration holds back every store its selector
    /// covers, each at its own position — the whole point of a selection,
    /// and the case an anchor comparison could not express.
    #[test]
    fn a_selection_holds_every_store_it_covers_at_its_own_position() {
        let reg = scratch("selectionfloor");
        let mut d = decl("central", true);
        d.select = "[service=apache]".into();
        d.save(&reg).unwrap();
        let mut held = cursor::Positions::new("central");
        held.advance("id-a", "/p/a", 100, Some(7), 0, 1);
        held.advance("id-b", "/p/b", 100, Some(2), 0, 1);
        held.save(&positions_path(&reg, "central")).unwrap();
        let index = InterestIndex::read(&reg);

        let apache = |id: &str| store_fields(id, &[("service", "apache")]);
        assert_eq!(index.for_store(&apache("id-a"), 100).floor, Some(7));
        assert_eq!(index.for_store(&apache("id-b"), 100).floor, Some(2));
        // Covered but never read: it holds the whole store, exactly as a
        // follower that has never run does.
        let fresh = index.for_store(&apache("id-c"), 100);
        assert_eq!(fresh.floor, None);
        assert_eq!(fresh.holder.as_deref(), Some("central"));
        // Not covered: nothing holds it, and there is nobody to name.
        let other = index.for_store(&store_fields("id-d", &[("service", "postgres")]), 100);
        assert_eq!(other.floor, None);
        assert_eq!(other.holder, None);
        assert_eq!(other.retaining, 0);
        fs::remove_dir_all(&reg).ok();
    }

    /// A declaration written before selections named ONE store in a
    /// `store` member, and `create` always refused a path-anchored one —
    /// so it is exactly the predicate `id=<it>` and keeps its cursor.
    #[test]
    fn a_legacy_declaration_reads_as_the_store_it_named() {
        let reg = scratch("legacy");
        fs::create_dir_all(follower_dir(&reg, "old")).unwrap();
        fs::write(
            decl_path(&reg, "old"),
            r#"{"store":"id-a","retaining":true,"command":["/bin/true"]}"#,
        )
        .unwrap();
        let d = Declaration::load(&reg, "old").unwrap();
        assert_eq!(d.select, "[id=id-a]");
        assert_eq!(d.anchor().as_deref(), Some("id-a"));

        // And its position is still its position: a `cursor.json` belongs
        // to the one store the declaration named.
        Cursor {
            seq: Some(5),
            ..Cursor::new("old", "id-a", "/p")
        }
        .save(&cursor_path(&reg, "old"))
        .unwrap();
        let index = InterestIndex::read(&reg);
        assert_eq!(
            index.for_store(&store_fields("id-a", &[]), 100).floor,
            Some(5)
        );
        assert_eq!(
            index.for_store(&store_fields("id-b", &[]), 100).holder,
            None
        );
        fs::remove_dir_all(&reg).ok();
    }

    /// A store with no identity can hold nothing back: a position is
    /// keyed by identity, so there is none that could name it.
    #[test]
    fn a_store_with_no_identity_holds_nothing_back() {
        let reg = scratch("noid");
        let mut d = decl("central", true);
        d.select = "[]".into();
        d.save(&reg).unwrap();
        let mut fields = Map::new();
        fields.insert("name".into(), Value::String("plain".into()));
        let held = InterestIndex::read(&reg).for_store(&fields, 100);
        // Covered by `*`, and still nothing to hold: the follower cannot
        // have a position in a store that cannot be addressed.
        assert_eq!(held.floor, None);
        assert_eq!(held.holder.as_deref(), Some("central"));
        fs::remove_dir_all(&reg).ok();
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
            blind: false,
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
            blind: false,
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
                blind: false,
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
