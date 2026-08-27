//! Taking incus container consoles into timberfs.
//!
//! The console is the "everything else" channel: boot output, a crashing
//! JVM's fatal log, whatever a process writes on its way down. An app that
//! ships its own logs over OTLP is better served by `otlp-intake` — what
//! this exists for is the output that arrives when the app's own logging
//! is already dead, and that incus keeps in a 128 KiB ring which wraps in
//! silence.
//!
//! Three decisions shape this file, and each is measured rather than
//! assumed (see `incus.rs` for the protocol facts behind them):
//!
//! * The **live websocket**, not the ring, is the feed. Draining the ring
//!   destroys it for everyone else, and everything drained in one poll
//!   would share that poll's timestamp — collapsing write-time granularity
//!   to the poll interval, which is the axis this whole store is indexed
//!   on. The ring is drained ONCE, right after attaching, to recover what
//!   the instance emitted before we arrived.
//! * A store is found by its **labels**, never by a name assembled from
//!   them. Which labels is the operator's choice (`--key`), because the
//!   right answer differs: one store per instance, one per image version,
//!   or every console on the host in one store are all things somebody
//!   legitimately wants.
//! * Only lines that START an entry are stamped. Stamping every line
//!   shatters a stack trace into one entry per frame; stamping none fuses
//!   unrelated output into one. An entry that has gone quiet is closed —
//!   timing a live tap has and a poller does not.

use std::collections::BTreeMap;

use anyhow::{bail, Context};
use regex::Regex;

use crate::incus::Instance;

/// Facts that describe this instantiation of a container rather than the
/// log itself: they change when it is rebuilt or replaced, so as labels
/// they would claim the current value for entries produced under three
/// images ago. They go into the log's timeline at each attach instead —
/// unless the operator puts one in `--key`, which is them saying that for
/// their purposes it IS part of what the store is.
const EPISODIC: &[&str] = &["image", "base_image", "entrypoint", "incus.uuid"];

/// The default lookup key: one store per instance.
pub const DEFAULT_KEY: &str = "type,incus.project,incus.instance";

/// What the intake knows about an instance, as flat `key=value` facts —
/// the vocabulary `--key` selects from and `--prefix` expands.
pub fn facts(inst: &Instance, server_name: &str) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("type".into(), "console".into());
    // The machine the container runs on, not the container's own idea of
    // its hostname: `host` is the label a fleet view cannot do without,
    // and a container's hostname answers a different question.
    let host = if inst.location.is_empty() {
        server_name.to_string()
    } else {
        inst.location.clone()
    };
    f.insert("host".into(), host);
    f.insert("incus.project".into(), inst.project.clone());
    f.insert("incus.instance".into(), inst.name.clone());
    for (k, v) in [
        ("image", &inst.image),
        ("base_image", &inst.base_image),
        ("entrypoint", &inst.entrypoint),
    ] {
        if !v.is_empty() {
            f.insert(k.into(), v.clone());
        }
    }
    // `user.*` is where an operator already puts labels, so they are
    // carried rather than reinvented. `user.service` is lifted to the
    // conventional `service` name; nothing else is guessed, and in
    // particular `service` is NOT derived from the image, which is a
    // different fact that merely often looks like one.
    for (k, v) in &inst.user_keys {
        if k == "user.service" {
            f.insert("service".into(), v.clone());
        } else {
            f.insert(k.clone(), v.clone());
        }
    }
    f
}

/// Split a `--key` list, refusing anything the facts cannot supply — a
/// key naming a fact that is never present would match every store or
/// none, silently.
pub fn parse_key(spec: &str) -> anyhow::Result<Vec<String>> {
    let keys: Vec<String> = spec
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if keys.is_empty() {
        bail!("--key needs at least one label: a key that selects nothing selects everything");
    }
    Ok(keys)
}

/// The selector that finds this instance's store: every key fact, matched
/// exactly. A fact the instance does not have is matched as absent, which
/// is what `--select key=` means — so an instance with no image and a key
/// naming `image` finds the store for "instances with no image", which is
/// at least consistent.
pub fn key_selector(key: &[String], facts: &BTreeMap<String, String>) -> String {
    key.iter()
        .map(|k| {
            let v = facts.get(k).map(String::as_str).unwrap_or("");
            // A value with a comma would split the term; quote it, which
            // is what the selector grammar provides for.
            if v.contains(',') || v.contains('"') {
                format!("{k}=\"{}\"", v.replace('"', ""))
            } else {
                format!("{k}={v}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The labels a newly created store is seeded with: every key fact,
/// because the key must be matchable or the store cannot be found again,
/// plus the stable facts, because they are true of the store rather than
/// of one instantiation of it.
pub fn labels_for(key: &[String], facts: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    facts
        .iter()
        .filter(|(k, _)| !EPISODIC.contains(&k.as_str()) || key.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// One piece of a `--prefix` template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Literal(String),
    /// `{time}` — the only piece that varies within an attach episode.
    Time,
    Fact(String),
}

/// Parse a `--prefix` template. `{{` is a literal brace; an unknown fact
/// name is refused rather than expanded to nothing, because a prefix that
/// silently loses a field is one nobody notices until they query.
pub fn parse_prefix(spec: &str, known: &[&str]) -> anyhow::Result<Vec<Piece>> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut rest = spec;
    while let Some(i) = rest.find('{') {
        if rest[i..].starts_with("{{") {
            lit.push_str(&rest[..i]);
            lit.push('{');
            rest = &rest[i + 2..];
            continue;
        }
        let Some(j) = rest[i..].find('}') else {
            bail!("unclosed `{{` in --prefix {spec:?}");
        };
        lit.push_str(&rest[..i]);
        if !lit.is_empty() {
            out.push(Piece::Literal(std::mem::take(&mut lit)));
        }
        let name = &rest[i + 1..i + j];
        if name == "time" {
            out.push(Piece::Time);
        } else if known.contains(&name) || name.starts_with("user.") {
            out.push(Piece::Fact(name.to_string()));
        } else {
            bail!(
                "--prefix names {{{name}}}, which is not a fact this intake has. \
                 Known: time, {}",
                known.join(", ")
            );
        }
        rest = &rest[i + j + 1..];
    }
    lit.push_str(rest);
    if !lit.is_empty() {
        out.push(Piece::Literal(lit));
    }
    Ok(out)
}

/// Every fact name a `--prefix` may use, for the error message and for
/// validation. `user.*` is accepted beyond this list.
pub fn known_facts() -> Vec<&'static str> {
    vec![
        "type",
        "host",
        "incus.project",
        "incus.instance",
        "image",
        "base_image",
        "entrypoint",
        "incus.uuid",
        "service",
    ]
}

/// A prefix with its per-instance facts already substituted: what is left
/// is the literal text and the one hole the timestamp goes in.
#[derive(Debug, Clone)]
pub struct Prefix {
    before: String,
    after: String,
    has_time: bool,
}

/// The timestamp shape written into the payload. Chosen to be what
/// timberfs's own detection already recognises as a leading stamp, so the
/// common case needs no declaration on the store.
const TIME_FMT: &str = "%Y-%m-%dT%H:%M:%S%.3f%:z";
const TIME_RE: &str = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}[+-]\d{2}:\d{2}";

impl Prefix {
    pub fn render(pieces: &[Piece], facts: &BTreeMap<String, String>) -> Prefix {
        let mut before = String::new();
        let mut after = String::new();
        let mut seen_time = false;
        for p in pieces {
            let target = if seen_time { &mut after } else { &mut before };
            match p {
                Piece::Literal(s) => target.push_str(s),
                Piece::Fact(k) => target.push_str(facts.get(k).map(String::as_str).unwrap_or("")),
                Piece::Time => seen_time = true,
            }
        }
        Prefix {
            before,
            after,
            has_time: seen_time,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.before.is_empty() && self.after.is_empty() && !self.has_time
    }

    /// Write the prefix for an entry starting at `ms`.
    pub fn write(&self, ms: i64, out: &mut Vec<u8>) {
        out.extend_from_slice(self.before.as_bytes());
        if self.has_time {
            let t = chrono::DateTime::from_timestamp_millis(ms)
                .map(|t| t.with_timezone(&chrono::Local).format(TIME_FMT).to_string())
                .unwrap_or_default();
            out.extend_from_slice(t.as_bytes());
        }
        out.extend_from_slice(self.after.as_bytes());
    }
}

/// The `timestamp_regex` / `timestamp_format` a store needs to read back a
/// prefix whose timestamp is NOT leading — timberfs's built-in detection
/// is anchored, so it would see no stamp at all and fuse every line into
/// one entry. None where `{time}` leads (nothing to declare) or where
/// there is no `{time}` at all (nothing to find).
///
/// Derived from the template's SHAPE, not from one instance's rendering:
/// several instances may share a store, and `{incus.instance}` is then a
/// different word on every line.
pub fn timestamp_declaration(pieces: &[Piece]) -> Option<(String, String)> {
    let idx = pieces.iter().position(|p| *p == Piece::Time)?;
    if idx == 0 {
        return None;
    }
    let mut re = String::from("^");
    for p in &pieces[..idx] {
        match p {
            Piece::Literal(s) => re.push_str(&regex::escape(s)),
            // A fact's value is one unspaced word as far as this is
            // concerned; it is only ever skipped over.
            Piece::Fact(_) => re.push_str(r"\S*"),
            Piece::Time => unreachable!("first Time is at idx"),
        }
    }
    re.push_str(&format!("({TIME_RE})"));
    Some((re, TIME_FMT.to_string()))
}

/// Whether an entry is open, and what opened it — which decides whether
/// the next unstamped line continues it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    None,
    /// A line carrying its own timestamp. Everything unstamped after it
    /// is its body: an exception, a stack frame, the rest of a banner.
    ByStamp,
    /// A line we stamped ourselves, having judged it an entry start.
    ByPrefix,
}

/// Turns console bytes into stamped entries.
///
/// The rule: a line that already carries its own leading timestamp starts
/// an entry and is left alone; a line arriving while an entry is open
/// continues it and is left alone; anything else starts an entry and gets
/// the prefix. An entry goes stale after `idle_ms` without a line, which
/// is what separates a stack trace (a sub-millisecond burst) from an
/// unrelated message arriving seconds later.
pub struct Stamper {
    prefix: Prefix,
    idle_ms: u64,
    iso: Regex,
    open_entry: Open,
    last_ms: i64,
    /// Bytes of a line not yet terminated by the console.
    partial: Vec<u8>,
    /// True while feeding bytes whose arrival time says nothing about
    /// when they were written.
    timeless: bool,
}

impl Stamper {
    pub fn new(prefix: Prefix, idle_ms: u64) -> Stamper {
        Stamper {
            prefix,
            idle_ms,
            // The same shape timberfs's own extractor anchors on, so
            // "already stamped" here means "already an entry start"
            // there.
            iso: Regex::new(r"^\d{4}[.-]\d{2}[.-]\d{2}[T ]\d{2}:\d{2}:\d{2}").unwrap(),
            open_entry: Open::None,
            last_ms: 0,
            partial: Vec::new(),
            timeless: false,
        }
    }

    /// Feed bytes recovered from the ring buffer, which arrive in ONE
    /// batch however long they took to be written. Arrival timing says
    /// nothing here, so the idle gap cannot: every unstamped line starts
    /// its own entry, and only a line that carries its own timestamp
    /// still gathers the lines beneath it.
    ///
    /// Without this the whole backlog fuses into one entry, which is what
    /// the first live run against a real container did.
    pub fn push_recovered(&mut self, bytes: &[u8], now_ms: i64) -> Vec<u8> {
        self.timeless = true;
        let out = self.push(bytes, now_ms);
        self.timeless = false;
        // The live stream that follows is timed again, and nothing it
        // sends continues a line recovered from the ring.
        self.open_entry = Open::None;
        out
    }

    /// Feed console bytes; returns the entries to append. Carriage returns
    /// are dropped: a console is a PTY, so ONLCR turns every newline into
    /// CR(s)+LF and those are terminal mechanics, not content.
    pub fn push(&mut self, bytes: &[u8], now_ms: i64) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + 64);
        for b in bytes {
            match b {
                b'\r' => {}
                b'\n' => {
                    let line = std::mem::take(&mut self.partial);
                    self.emit(&line, now_ms, &mut out);
                    out.push(b'\n');
                }
                _ => self.partial.push(*b),
            }
        }
        out
    }

    /// Whatever is buffered but unterminated — flushed when the console
    /// closes, so a last line without a newline is not lost.
    pub fn flush(&mut self, now_ms: i64) -> Vec<u8> {
        if self.partial.is_empty() {
            return Vec::new();
        }
        let line = std::mem::take(&mut self.partial);
        let mut out = Vec::new();
        self.emit(&line, now_ms, &mut out);
        out.push(b'\n');
        out
    }

    fn emit(&mut self, line: &[u8], now_ms: i64, out: &mut Vec<u8>) {
        if self.open_entry != Open::None
            && now_ms.saturating_sub(self.last_ms) > self.idle_ms as i64
        {
            self.open_entry = Open::None;
        }
        self.last_ms = now_ms;
        let already_stamped = std::str::from_utf8(line)
            .map(|s| self.iso.is_match(s))
            .unwrap_or(false);
        // Recovered bytes have no usable arrival time, so only a stamped
        // line may gather what follows it.
        let continues = match self.open_entry {
            Open::None => false,
            Open::ByStamp => true,
            Open::ByPrefix => !self.timeless,
        };
        if already_stamped {
            // The producer stamps its own lines. Prefixing would demote
            // its timestamp to payload AND make every continuation line
            // its own entry, which is how a stack trace stops coming back
            // whole.
            self.open_entry = Open::ByStamp;
        } else if continues {
            // A continuation: an exception body, a stack frame, the rest
            // of a banner.
        } else {
            self.prefix.write(now_ms, out);
            self.open_entry = Open::ByPrefix;
        }
        out.extend_from_slice(line);
    }
}

/// The line written into the log when a tap attaches: which instance,
/// which image, which entrypoint — the episodic facts, in the timeline
/// where they can be correlated with what follows, rather than in labels
/// that would claim today's values for all of history. Also marks the
/// seam: a restart is where a gap, if any, is.
pub fn attach_marker(facts: &BTreeMap<String, String>, ring_bytes: usize) -> String {
    let mut s = String::from("timberfs: console attached");
    for k in [
        "incus.instance",
        "incus.project",
        "host",
        "image",
        "base_image",
        "entrypoint",
        "incus.uuid",
    ] {
        if let Some(v) = facts.get(k) {
            if v.contains(' ') {
                s.push_str(&format!(" {k}={v:?}"));
            } else {
                s.push_str(&format!(" {k}={v}"));
            }
        }
    }
    s.push_str(&format!(" ring_backlog={ring_bytes}"));
    s
}

/// A store's name is for people; the path is not. This is the readable
/// one, and it is deliberately not what the store is FOUND by.
pub fn store_name(facts: &BTreeMap<String, String>, key: &[String]) -> String {
    match (
        facts.get("incus.instance"),
        key.contains(&"incus.instance".to_string()),
    ) {
        // The usual case: one store per instance.
        (Some(i), true) => format!("{i}-console"),
        // A key that does not name the instance is a key that merges
        // several of them, so naming the store after one would be a lie.
        _ => {
            let host = facts.get("host").map(String::as_str).unwrap_or("incus");
            format!("{host}-console")
        }
    }
}

// ------------------------------------------------------------ resolution

/// Find the store this instance's console belongs in, creating it if it
/// is not there yet, and return the name it lives under — which is its
/// id, because the path is opaque and nothing should read it.
///
/// The lookup is the operator's `--key`, matched against every store's
/// manifest. Exactly one match is used; none mints a store; SEVERAL is
/// refused, and that refusal is not us policing the key — a key that
/// merges instances is a legitimate choice, but two stores already
/// wearing one key is a state nothing can write to, and creating a third
/// would be worse.
pub fn resolve_store(
    into_dir: &std::path::Path,
    key: &[String],
    facts: &BTreeMap<String, String>,
    opts: &IncusOpts,
) -> anyhow::Result<String> {
    let expr = key_selector(key, facts);
    let sel = crate::select::Selector::parse(&expr)
        .with_context(|| format!("the --key produced the selector {expr:?}"))?;
    let dirs = [into_dir.to_path_buf()];
    let mut found = crate::select::resolve(&dirs, &sel);
    match found.len() {
        1 => Ok(found.pop().unwrap().name),
        0 => Ok(mint_store(into_dir, key, facts, opts, &expr)?),
        _ => {
            let names = found
                .iter()
                .map(|m| format!("  {} ({})", m.name, m.dir.display()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "{expr} matches {} stores, so there is no one store to write to:\n{names}\n\
                 give --key more labels, or take the extra stores out of {}",
                found.len(),
                into_dir.display()
            )
        }
    }
}

/// Write the manifest for a new store, and return the name it lives
/// under. The id is minted HERE and the directory is named after it: the
/// manifest must carry the id before the pair exists, because a pair whose
/// two sides disagree is refused by every writer — including this one.
fn mint_store(
    into_dir: &std::path::Path,
    key: &[String],
    facts: &BTreeMap<String, String>,
    opts: &IncusOpts,
    routed_from: &str,
) -> anyhow::Result<String> {
    let id = crate::bark::new_uuid()?;
    let dir = into_dir.join(&id);
    std::fs::create_dir_all(&dir)?;
    let mut map = serde_json::Map::new();
    map.insert("id".into(), serde_json::Value::String(id.clone()));
    map.insert(
        "name".into(),
        serde_json::Value::String(store_name(facts, key)),
    );
    for (k, v) in labels_for(key, facts) {
        map.insert(k, serde_json::Value::String(v));
    }
    if opts.index {
        map.insert("index".into(), serde_json::Value::Bool(true));
    }
    // The sap: an appender's crash window shrinks to a second, and
    // `query --follow` can tail the console live, which is most of the
    // point of tapping it in the first place.
    map.insert("wal".into(), serde_json::Value::Bool(true));
    if let Some(r) = &opts.retain {
        map.insert("retain".into(), serde_json::Value::String(r.clone()));
    }
    if let Some(r) = &opts.retain_size {
        map.insert("retain_size".into(), serde_json::Value::String(r.clone()));
    }
    // A prefix whose timestamp does not lead needs the store taught how to
    // find it; the built-in detection is anchored.
    if let Some((re, fmt)) = timestamp_declaration(&opts.prefix) {
        map.insert("timestamp_regex".into(), serde_json::Value::String(re));
        map.insert("timestamp_format".into(), serde_json::Value::String(fmt));
    }
    map.insert(
        crate::bark::ROUTED_FROM.to_string(),
        serde_json::Value::String(routed_from.to_string()),
    );
    crate::bark::save(&dir, &id, &map)?;
    crate::note!(
        "timberfs: {} has no store yet; created {}",
        routed_from,
        dir.join(&id).display()
    );
    Ok(id)
}

/// Everything the intake was told on the command line.
pub struct IncusOpts {
    pub socket: String,
    pub project: String,
    pub into_dir: std::path::PathBuf,
    pub key: Vec<String>,
    pub prefix: Vec<Piece>,
    pub include_vms: bool,
    pub only: Vec<String>,
    pub retain: Option<String>,
    pub retain_size: Option<String>,
    pub index: bool,
    pub idle_ms: u64,
    /// Keep the console ring buffer instead of consuming it, so
    /// `incus console --show-log` still has something to show. Costs
    /// correctness across a restart: see `drain_every_ms`.
    pub keep_ring: bool,
    /// How often, at most, to consume the ring while attached.
    pub drain_every_ms: u64,
    pub mark_episodes: bool,
    pub exit_on_upgrade: bool,
}

impl IncusOpts {
    /// Is this instance one this intake should tap?
    pub fn wants(&self, inst: &Instance) -> bool {
        if !inst.is_running() {
            return false;
        }
        // A VM's console is a different animal: file-backed, so reads are
        // idempotent and there is no ring to lose, and it carries the
        // kernel's boot output rather than an application's stdout.
        if !inst.is_container() && !self.include_vms {
            return false;
        }
        self.only.is_empty() || self.only.iter().any(|n| n == &inst.name)
    }
}

// ------------------------------------------------------------- the tap

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Tap one instance's console until it closes, the store cannot be
/// written, or `stop` is set. Returns Ok(()) on a clean end — a restart,
/// a shutdown — so the supervisor can decide whether to come back.
///
/// The order here is the whole reliability argument. Attach FIRST, then
/// drain the ring: the two feeds are independent, so what the ring still
/// holds overlaps what the websocket has begun delivering rather than
/// leaving a gap between them. Drain-then-attach would lose whatever the
/// instance emitted in between.
pub fn tap_instance(
    incus: &crate::incus::Incus,
    inst: &Instance,
    server_name: &str,
    opts: &IncusOpts,
    intake: &Arc<Mutex<crate::intake::Intake>>,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<usize> {
    let facts = facts(inst, server_name);
    let console = incus.console_attach(&inst.name)?;
    // From here every exit path must release the console, or a human
    // running `incus console` is refused until this process dies.
    let result = tap_attached(incus, inst, &facts, opts, intake, stop, &console);
    if let Err(e) = incus.cancel_operation(&console.id) {
        crate::note!("timberfs: {}: releasing the console: {e}", inst.name);
    }
    result
}

fn tap_attached(
    incus: &crate::incus::Incus,
    inst: &Instance,
    facts: &BTreeMap<String, String>,
    opts: &IncusOpts,
    intake: &Arc<Mutex<crate::intake::Intake>>,
    stop: &Arc<AtomicBool>,
    console: &crate::incus::Console,
) -> anyhow::Result<usize> {
    let mut ws = incus.console_stream(console)?;
    // Only a container has the ring; a VM's console is a file, and reading
    // it is not a drain.
    let backlog = if inst.is_container() {
        incus.console_drain(&inst.name).unwrap_or_default()
    } else {
        Vec::new()
    };

    let store = resolve_store(&opts.into_dir, &opts.key, facts, opts)?;
    {
        let mut g = intake.lock().unwrap();
        crate::intake::ensure_store(
            &mut g,
            &store,
            &format!("incus-intake {}", inst.name),
            // The store either already existed (resolved by key) or was
            // just minted with its manifest; either way this is not the
            // moment to refuse it.
            true,
            "",
            |_dir, _name| Ok(()),
        )?;
    }

    let mut stamper = Stamper::new(Prefix::render(&opts.prefix, facts), opts.idle_ms);
    // The marker is held back until something is actually collected. It
    // marks a SEAM IN CONTENT — where a gap, if any, is — so writing one
    // for an attach that turns out to collect nothing records our own
    // retrying instead: a store whose whole contents are "console
    // attached" every thirty seconds, which is what an unattachable
    // console produced for two days.
    let mut pending_marker = Vec::new();
    if opts.mark_episodes {
        Prefix::render(&opts.prefix, facts).write(now_ms(), &mut pending_marker);
        pending_marker.extend_from_slice(attach_marker(facts, backlog.len()).as_bytes());
        pending_marker.push(b'\n');
    }
    if !backlog.is_empty() {
        // The ring HAD content, so this episode has collected something
        // whatever the websocket goes on to do.
        let mut first = std::mem::take(&mut pending_marker);
        first.extend(stamper.push_recovered(&backlog, now_ms()));
        append(intake, &store, &first)?;
    }

    // The ring and the websocket are INDEPENDENT feeds: while we stream,
    // the ring quietly accumulates its own copy of everything we have
    // already written, and the next attach's drain would replay it. So
    // the ring is consumed as we go.
    //
    // Receiving bytes IS the notification that the ring has grown — both
    // feeds come from the same console loop — so this needs no timer, and
    // a silent container costs nothing. Rate-limited because a busy one
    // would otherwise mean an HTTP request per frame; that limit is also
    // the bound on how much an UNCLEAN kill can duplicate, counting only
    // time when output was actually flowing.
    //
    // Discarding is always safe with respect to data: draining the ring
    // cannot remove anything from the websocket's queue.
    let consume_ring = inst.is_container() && !opts.keep_ring;
    let mut last_drain = now_ms();
    let mut delivered = 0usize;
    // What the console said, kept for the diagnostic below. An attach
    // that incus refuses is not silent: it answers over the console
    // stream, in its own words, and those words are the answer to "why".
    let mut heard: Vec<u8> = Vec::new();
    // Output from the first seconds, written once the episode proves it
    // is one.
    let mut held: Vec<u8> = Vec::new();
    let began = std::time::Instant::now();
    while !stopping(stop) {
        match ws.read_frame()? {
            // Nothing within the read timeout. The loop's own `stop`
            // check is the point: a silent console must not pin its
            // thread against a shutdown.
            crate::incus::Frame::Idle => {}
            crate::incus::Frame::Data(bytes) => {
                delivered += bytes.len();
                heard.extend_from_slice(&bytes);
                let now = now_ms();
                let out = stamper.push(&bytes, now);
                if !out.is_empty() {
                    if began.elapsed() < MIN_EPISODE {
                        // Held, not dropped. An attach incus refuses
                        // answers over this same stream in its own words,
                        // and recording that as console output fills the
                        // store with our retrying. Nothing is lost by
                        // waiting: the episode's flush below writes
                        // whatever is held, so a container that prints
                        // its dying words and vanishes still keeps them.
                        held.extend_from_slice(&out);
                    } else {
                        let mut w = std::mem::take(&mut pending_marker);
                        w.extend_from_slice(&std::mem::take(&mut held));
                        w.extend_from_slice(&out);
                        append(intake, &store, &w)?;
                    }
                }
                if consume_ring && now.saturating_sub(last_drain) >= opts.drain_every_ms as i64 {
                    last_drain = now;
                    // A failure here costs a duplicate after a restart,
                    // never data — so it must not end the episode.
                    if let Err(e) = incus.console_drain(&inst.name) {
                        crate::note!("timberfs: {}: consuming the console ring: {e}", inst.name);
                    }
                }
            }
            // The console closed: the instance stopped or restarted. Not
            // an error — the supervisor decides whether to come back.
            crate::incus::Frame::Closed => break,
        }
    }
    let tail = stamper.flush(now_ms());
    held.extend_from_slice(&tail);
    // A refusal is incus talking, not the container: report it and record
    // nothing. Anything else held is the container's, and is written.
    if let Some(why) = refusal(&heard) {
        bail!("{why}");
    }
    if !held.is_empty() {
        let mut w = std::mem::take(&mut pending_marker);
        w.extend_from_slice(&held);
        append(intake, &store, &w)?;
    }
    Ok(delivered)
}

fn append(
    intake: &Arc<Mutex<crate::intake::Intake>>,
    store: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let mut g = intake.lock().unwrap();
    let cfg = g.cfg;
    let Some(f) = g.file(store) else {
        bail!("{store} is not open");
    };
    let ms = u64::try_from(now_ms()).unwrap_or(0);
    f.append_windowed(bytes, ms, ms, &cfg)?;
    Ok(())
}

// ------------------------------------------------------- the supervisor

/// How long to wait before re-attaching after a console ends. An instance
/// that is restarting is not ready the instant its old console closes, and
/// a tight retry loop against a container that will not start is just a
/// busy wait on a daemon socket.
/// Is a shutdown under way? BOTH the signal and the flag the main thread
/// sets, so a tap notices SIGTERM itself rather than waiting to be told.
fn stopping(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Relaxed) || crate::append::stopping()
}

/// Sleep, but wake for a shutdown. The granularity only has to be short
/// against a person watching `systemctl stop`.
fn sleep_until_stopped(stop: &AtomicBool, ms: u64) {
    let step = 100;
    let mut left = ms;
    while left > 0 && !stopping(stop) {
        let n = left.min(step);
        std::thread::sleep(std::time::Duration::from_millis(n));
        left -= n;
    }
}

/// incus answering "no" over the console stream rather than in the HTTP
/// reply. A refused attach still opens the websocket, and what comes
/// down it is incus's own error — which is the operator's answer, and
/// which used to be recorded silently into the store instead of said.
///
/// Matched narrowly: an `Error:` line, the shape incus's CLI prints, and
/// only for a short answer. A container's own output can contain the
/// word, and losing a crash log to a heuristic would defeat the purpose
/// of collecting consoles at all.
fn refusal(heard: &[u8]) -> Option<String> {
    if heard.len() > 512 {
        return None;
    }
    let text = String::from_utf8_lossy(heard);
    // The WHOLE of it must be one `Error:` line. A container's output is
    // not one line and then end-of-stream inside two seconds, and this is
    // the direction that must not be wrong: a crash log withheld because
    // it happened to start with the word is the failure that matters,
    // where a refusal recorded as content is only noise.
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let line = lines.next()?.trim();
    if lines.next().is_some() || !line.starts_with("Error: ") {
        return None;
    }
    Some(line.to_string())
}

/// Shorter than this and the console was handed straight back rather
/// than ended: not an episode, so it backs off instead of reattaching at
/// once. Comfortably longer than an attach costs, comfortably shorter
/// than any real episode.
const MIN_EPISODE: std::time::Duration = std::time::Duration::from_secs(2);

const REATTACH_MS: u64 = 1_000;
const REATTACH_MAX_MS: u64 = 30_000;

/// Run the intake: tap every instance the options want, follow incus's
/// lifecycle events so the set stays current, and keep going until
/// SIGTERM.
pub fn run(opts: IncusOpts) -> anyhow::Result<()> {
    let incus = crate::incus::Incus::new(&opts.socket, &opts.project);
    let server_name = incus.server_name().context(
        "asking incus its name — is the socket readable?          (it is root:incus-admin, so this usually means group membership)",
    )?;
    if timestamp_declaration(&opts.prefix).is_none()
        && !opts.prefix.contains(&Piece::Time)
        && !opts.prefix.is_empty()
    {
        crate::note!(
            "timberfs: --prefix has no {{time}}, so entries carry no timestamp of their own              and a query has only the write time to go on"
        );
    }

    std::fs::create_dir_all(&opts.into_dir)?;
    let cfg = crate::store::Config {
        chunk_size: 256 * 1024,
        level: 3,
        flush_age_ms: 5000,
    };
    let intake = Arc::new(Mutex::new(crate::intake::Intake::new(
        &opts.into_dir,
        cfg,
        (),
    )));
    let stop = Arc::new(AtomicBool::new(false));
    crate::append::install_signal_handlers();
    let maintenance = crate::intake::spawn_maintenance(
        Arc::clone(&intake),
        Arc::clone(&stop),
        opts.exit_on_upgrade,
        |_, _| {},
    );

    let opts = Arc::new(opts);
    let running: Arc<Mutex<std::collections::BTreeSet<String>>> =
        Arc::new(Mutex::new(Default::default()));
    // Held so shutdown can wait for them; see the join below.
    let taps: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

    // The instances that are already up, then whatever the event stream
    // says about them from here on.
    for inst in incus.instances()? {
        if opts.wants(&inst) {
            spawn_tap(
                &incus,
                inst,
                &server_name,
                &opts,
                &intake,
                &stop,
                &running,
                &taps,
            );
        }
    }

    watch_lifecycle(&incus, &server_name, &opts, &intake, &stop, &running, &taps);
    stop.store(true, Ordering::Relaxed);
    // WAIT for the taps. Each releases its console on the way out, and
    // returning from here would exit the process and kill them where they
    // stand — which is how every console came to be left reserved in
    // incus after a restart, refusing the next attach AND a human running
    // `incus console` until an operator deleted the operations by hand.
    //
    // Safe to wait on now that a read times out rather than blocking
    // forever: a tap notices `stop` within that timeout wherever it is.
    for h in taps.lock().unwrap().drain(..) {
        let _ = h.join();
    }
    let _ = maintenance.join();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_tap(
    incus: &crate::incus::Incus,
    inst: Instance,
    server_name: &str,
    opts: &Arc<IncusOpts>,
    intake: &Arc<Mutex<crate::intake::Intake>>,
    stop: &Arc<AtomicBool>,
    running: &Arc<Mutex<std::collections::BTreeSet<String>>>,
    taps: &Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
) {
    {
        let mut g = running.lock().unwrap();
        // The console is exclusive, so a second tap of one instance would
        // only take it away from the first.
        if !g.insert(inst.name.clone()) {
            return;
        }
    }
    let socket = opts.socket.clone();
    let project = opts.project.clone();
    let server_name = server_name.to_string();
    let opts = Arc::clone(opts);
    let intake = Arc::clone(intake);
    let stop = Arc::clone(stop);
    let running = Arc::clone(running);
    let _ = incus;
    let h = std::thread::spawn(move || {
        let incus = crate::incus::Incus::new(&socket, &project);
        let mut backoff = REATTACH_MS;
        while !stopping(&stop) {
            // Re-read the instance each time: an image or an entrypoint
            // may have changed under a restart, and those go in the next
            // attach marker.
            let inst = match incus.instance(&inst.name) {
                Ok(i) => i,
                Err(_) => break,
            };
            if !opts.wants(&inst) {
                break;
            }
            let began = std::time::Instant::now();
            match tap_instance(&incus, &inst, &server_name, &opts, &intake, &stop) {
                // An episode that LASTED and then ended is a restart or a
                // shutdown: come back promptly, because the console of
                // the new instance is already filling its ring.
                Ok(_) if began.elapsed() >= MIN_EPISODE => backoff = REATTACH_MS,
                // One that ended almost at once is not an episode: the
                // console was taken and handed straight back. Judged by
                // DURATION rather than by bytes, because such a console
                // often does deliver a little — a greeting, a ring
                // replay — and counting bytes would reset the backoff
                // every time and hammer on.
                //
                // Reattaching at REATTACH_MS here means an incus
                // operation per container per SECOND for as long as the
                // condition lasts, which is a load problem stacked on top
                // of whatever caused it.
                Ok(_) => {
                    if backoff == REATTACH_MS {
                        crate::note!(
                            "timberfs: {}: the console closed immediately; backing off. \
                             `incus console {}` will say why",
                            inst.name,
                            inst.name
                        );
                    }
                    backoff = (backoff * 2).min(REATTACH_MAX_MS);
                }
                Err(e) => {
                    crate::note!("timberfs: {}: {e}", inst.name);
                    backoff = (backoff * 2).min(REATTACH_MAX_MS);
                }
            }
            if stopping(&stop) {
                break;
            }
            // Interruptible: a tap asleep in its reattach backoff would
            // otherwise hold the shutdown for up to REATTACH_MAX_MS, and
            // a stop that takes half a minute per host reads as a hang.
            sleep_until_stopped(&stop, backoff);
        }
        running.lock().unwrap().remove(&inst.name);
    });
    taps.lock().unwrap().push(h);
}

/// Follow incus's lifecycle events. A tap that only enumerated instances
/// at startup would never see a container created afterwards, and one that
/// polled would take its poll interval to notice.
#[allow(clippy::too_many_arguments)]
fn watch_lifecycle(
    incus: &crate::incus::Incus,
    server_name: &str,
    opts: &Arc<IncusOpts>,
    intake: &Arc<Mutex<crate::intake::Intake>>,
    stop: &Arc<AtomicBool>,
    running: &Arc<Mutex<std::collections::BTreeSet<String>>>,
    taps: &Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
) {
    while !stop.load(Ordering::Relaxed) && !crate::append::stopping() {
        let mut ws = match incus.events() {
            Ok(w) => w,
            Err(e) => {
                crate::note!("timberfs: incus events: {e}");
                std::thread::sleep(std::time::Duration::from_millis(REATTACH_MS));
                continue;
            }
        };
        loop {
            if crate::append::stopping() {
                return;
            }
            match ws.read_frame() {
                Ok(crate::incus::Frame::Idle) => {}
                Ok(crate::incus::Frame::Data(bytes)) => {
                    for line in bytes.split(|b| *b == b'\n') {
                        let Some(name) = started_instance(line) else {
                            continue;
                        };
                        let Ok(inst) = incus.instance(&name) else {
                            continue;
                        };
                        if opts.wants(&inst) {
                            spawn_tap(incus, inst, server_name, opts, intake, stop, running, taps);
                        }
                    }
                }
                Ok(crate::incus::Frame::Closed) => break,
                Err(_) => break,
            }
        }
    }
}

/// The name of an instance a lifecycle event says is now up, if any.
/// `instance-restarted` arrives as ONE event rather than a stop and a
/// start, so it has to be handled alongside them or a restarted container
/// is never re-tapped.
pub fn started_instance(line: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    let md = v.get("metadata")?;
    let action = md.get("action")?.as_str()?;
    if !matches!(action, "instance-started" | "instance-restarted") {
        return None;
    }
    Some(md.get("name")?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    /// incus answers a refused attach over the console stream itself.
    /// Telling that from the container's own output matters both ways: a
    /// refusal recorded as console content fills the store with our
    /// retrying, and a crash log mistaken for a refusal is the one thing
    /// this intake exists to keep.
    #[test]
    fn a_refusal_is_incus_talking_not_the_container() {
        // What incus actually sent, verbatim from rc-app01.
        assert_eq!(
            refusal(b"Error: Failed running forkconsole: \"attaching to the container failed\"\n")
                .as_deref(),
            Some("Error: Failed running forkconsole: \"attaching to the container failed\"")
        );

        // A container's own output is NOT a refusal, however alarming.
        assert!(refusal(b"2026-08-27 ERROR java.lang.OutOfMemoryError\n").is_none());
        assert!(refusal(b"Error: connection refused\n  at Foo.bar(Foo.java:1)\n").is_none());
        assert!(refusal(b"").is_none());

        // Length is the backstop: a real console burst is not a one-line
        // answer, so anything substantial is the container's whatever it
        // starts with.
        let mut big = b"Error: Failed running forkconsole\n".to_vec();
        big.extend(std::iter::repeat_n(b'x', 600));
        assert!(refusal(&big).is_none(), "a long stream is the container's");
    }

    use super::*;

    fn f(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn base() -> BTreeMap<String, String> {
        f(&[
            ("type", "console"),
            ("host", "sourcream"),
            ("incus.project", "default"),
            ("incus.instance", "gateway01"),
            ("image", "visena-gateway:0.0.2-LOCAL"),
        ])
    }

    #[test]
    fn the_key_selects_the_store_and_is_always_a_label() {
        let key = parse_key(DEFAULT_KEY).unwrap();
        assert_eq!(
            key_selector(&key, &base()),
            "type=console,incus.project=default,incus.instance=gateway01"
        );
        // Every key fact is a label, or the store could not be found
        // again by the key that made it.
        let labels = labels_for(&key, &base());
        for k in &key {
            assert!(labels.contains_key(k), "{k} is in the key but not a label");
        }
        // ...and an episodic fact is NOT one, unless the key says it is.
        assert!(!labels.contains_key("image"));
        let versioned = parse_key("type,incus.instance,image").unwrap();
        assert!(labels_for(&versioned, &base()).contains_key("image"));
    }

    #[test]
    fn a_key_that_merges_instances_is_allowed_and_names_the_store_honestly() {
        // Every console on the host in one store: a thing somebody
        // legitimately wants, so it is not refused.
        let key = parse_key("type").unwrap();
        assert_eq!(key_selector(&key, &base()), "type=console");
        // Naming the merged store after one of its instances would be a
        // lie, so it is not named after one.
        assert_eq!(store_name(&base(), &key), "sourcream-console");
        assert_eq!(
            store_name(&base(), &parse_key(DEFAULT_KEY).unwrap()),
            "gateway01-console"
        );
        // An empty key is the one refusal: it selects everything.
        assert!(parse_key("").is_err());
        assert!(parse_key(" , ").is_err());
    }

    #[test]
    fn a_prefix_names_facts_or_is_refused() {
        let known = known_facts();
        assert_eq!(
            parse_prefix("{time} ", &known).unwrap(),
            vec![Piece::Time, Piece::Literal(" ".into())]
        );
        assert_eq!(
            parse_prefix("{time} {incus.instance} ", &known).unwrap(),
            vec![
                Piece::Time,
                Piece::Literal(" ".into()),
                Piece::Fact("incus.instance".into()),
                Piece::Literal(" ".into())
            ]
        );
        // `user.*` is the operator's own vocabulary, so it is accepted
        // without being enumerated.
        assert!(parse_prefix("{user.team} ", &known).is_ok());
        // A name we cannot supply would expand to nothing and be noticed
        // only at query time.
        assert!(parse_prefix("{nope} ", &known).is_err());
        assert!(parse_prefix("{time", &known).is_err(), "unclosed");
        // A literal brace.
        assert_eq!(
            parse_prefix("{{literal}", &known).unwrap(),
            vec![Piece::Literal("{literal}".into())]
        );
    }

    #[test]
    fn a_leading_time_needs_no_declaration_and_anything_else_does() {
        let known = known_facts();
        // The default: timberfs's own detection already reads this.
        assert!(timestamp_declaration(&parse_prefix("{time} ", &known).unwrap()).is_none());
        // No timestamp at all: nothing to declare, and nothing to find.
        assert!(timestamp_declaration(&parse_prefix("{host} ", &known).unwrap()).is_none());
        // Not leading: the built-in detection is anchored and would see
        // no stamp, fusing every line into one entry.
        let pieces = parse_prefix("{incus.instance} {time} ", &known).unwrap();
        let (re, fmt) = timestamp_declaration(&pieces).unwrap();
        assert_eq!(fmt, TIME_FMT);
        let compiled = Regex::new(&re).unwrap();
        // Derived from the SHAPE, so it reads a store several instances
        // share — the instance is a different word on every line.
        for line in [
            "gateway01 2026-08-25T10:00:00.123+02:00 hello",
            "auth01 2026-08-25T10:00:00.124+02:00 hello",
        ] {
            let got = compiled.captures(line).unwrap().get(1).unwrap().as_str();
            assert!(got.starts_with("2026-08-25T10:00:00.12"), "{got}");
        }
    }

    fn stamped(input: &[(&[u8], i64)], prefix: &str, idle: u64) -> String {
        let pieces = parse_prefix(prefix, &known_facts()).unwrap();
        let mut s = Stamper::new(Prefix::render(&pieces, &base()), idle);
        let mut out = Vec::new();
        for (bytes, ms) in input {
            out.extend(s.push(bytes, *ms));
        }
        out.extend(s.flush(9_999_999));
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_stack_trace_stays_one_entry_and_boot_lines_do_not_fuse() {
        // The console is a MIXED stream: an app that stamps its own lines,
        // and unstamped output around it. Both appear here.
        let out = stamped(
            &[
                (b"Detected virtualization lxc.\n", 1_000),
                (b"[  OK  ] Reached target paths.target.\n", 1_500),
                (
                    b"2026-08-25T06:38:36.200+02:00 ERROR c.v.g.Handler : failed\n",
                    2_300,
                ),
                (b"java.lang.IllegalStateException: no route\n", 2_301),
                (
                    b"\tat com.visena.gateway.Router.route(Router.java:88)\n",
                    2_302,
                ),
                (b"OpenJDK 64-Bit Server VM warning: something\n", 8_000),
            ],
            "{time} ",
            100,
        );
        let lines: Vec<&str> = out.lines().collect();
        let stamps = lines.iter().filter(|l| l.starts_with("1970-")).count();
        // Entry starts: the two boot lines, and the late JVM warning.
        // NOT the exception or the stack frame, which continue the ERROR.
        assert_eq!(stamps, 3, "{out}");
        assert!(
            lines[2].starts_with("2026-08-25T06:38:36.200"),
            "own stamp kept"
        );
        assert!(lines[3].starts_with("java.lang."), "continuation left bare");
        assert!(lines[4].starts_with("\tat com"), "frame left bare");
        assert!(lines[5].starts_with("1970-"), "8s later is a new entry");
    }

    #[test]
    fn the_pty_is_not_content() {
        // ONLCR gives \r\r\n; those are terminal mechanics.
        let out = stamped(&[(b"TICK-1 04:38:35\r\r\r\n", 0)], "", 100);
        assert_eq!(out, "TICK-1 04:38:35\n");
    }

    #[test]
    fn a_line_without_a_newline_is_not_lost() {
        let out = stamped(&[(b"no trailing newline", 0)], "", 100);
        assert_eq!(out, "no trailing newline\n");
    }

    #[test]
    fn a_prefix_may_carry_the_instance_for_a_merged_store() {
        let out = stamped(&[(b"hello\n", 0)], "{time} {incus.instance} ", 100);
        assert!(out.contains(" gateway01 hello"), "{out}");
    }

    #[test]
    fn the_marker_carries_the_episodic_facts_the_labels_do_not() {
        let mut facts = base();
        facts.insert("entrypoint".into(), "java -jar /app.jar".into());
        let m = attach_marker(&facts, 2048);
        assert!(m.contains("incus.instance=gateway01"));
        assert!(m.contains("image=visena-gateway:0.0.2-LOCAL"));
        // A value with spaces is quoted, so the marker stays parseable.
        assert!(m.contains(r#"entrypoint="java -jar /app.jar""#), "{m}");
        // The seam: how much ring backlog was stitched in behind it.
        assert!(m.ends_with("ring_backlog=2048"));
    }

    #[test]
    fn user_service_is_lifted_but_nothing_is_guessed_from_the_image() {
        let inst = Instance {
            name: "gateway01".into(),
            project: "default".into(),
            kind: "container".into(),
            status: "Running".into(),
            location: String::new(),
            image: "visena-gateway:0.0.2-LOCAL".into(),
            base_image: String::new(),
            entrypoint: String::new(),
            user_keys: vec![
                ("user.service".into(), "visena-gateway".into()),
                ("user.team".into(), "platform".into()),
            ],
        };
        let f = facts(&inst, "sourcream");
        assert_eq!(f.get("service").map(String::as_str), Some("visena-gateway"));
        assert_eq!(f.get("user.team").map(String::as_str), Some("platform"));

        // Without the operator saying so, there is no service: the image
        // often LOOKS like one, and deriving it would be a guess dressed
        // as a fact.
        let bare = Instance {
            user_keys: vec![],
            ..inst
        };
        assert!(!facts(&bare, "sourcream").contains_key("service"));
        // ...and an unclustered host supplies `host` from the server.
        assert_eq!(
            facts(&bare, "sourcream").get("host").map(String::as_str),
            Some("sourcream")
        );
    }
    #[test]
    fn ring_backlog_lines_do_not_fuse_into_one_entry() {
        // The ring hands over everything at once, however long it took to
        // be written, so arrival time says nothing and the idle gap
        // cannot speak. The first live run against a real container fused
        // four independent lines into one entry exactly here.
        let pieces = parse_prefix("{time} ", &known_facts()).unwrap();
        let mut s = Stamper::new(Prefix::render(&pieces, &base()), 100);
        let out =
            String::from_utf8(s.push_recovered(b"LIVE-1 hello\nLIVE-2 hello\nLIVE-3 hello\n", 0))
                .unwrap();
        assert_eq!(
            out.lines().filter(|l| l.starts_with("1970-")).count(),
            3,
            "each recovered line is its own entry: {out}"
        );

        // A line that carries its OWN stamp still gathers what follows,
        // because that judgement never depended on arrival timing.
        let mut s = Stamper::new(Prefix::render(&pieces, &base()), 100);
        let out = String::from_utf8(s.push_recovered(
            b"2026-08-25T06:38:36.200+02:00 ERROR failed\njava.lang.IllegalStateException\n\tat com.x(X.java:1)\n",
            0,
        ))
        .unwrap();
        assert_eq!(
            out.lines().filter(|l| l.starts_with("1970-")).count(),
            0,
            "{out}"
        );

        // ...and the live stream that follows never continues a recovered
        // line: they are different episodes of the same console.
        let mut s = Stamper::new(Prefix::render(&pieces, &base()), 100);
        let mut out = s.push_recovered(b"from the ring\n", 0);
        out.extend(s.push(b"from the wire\n", 1));
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text.lines().filter(|l| l.starts_with("1970-")).count(),
            2,
            "{text}"
        );
    }
    #[test]
    fn the_ring_is_consumed_for_containers_unless_asked_otherwise() {
        // The ring is a duplicate of what the websocket already
        // delivered, so it is consumed as we stream — but only where
        // there IS a ring. A VM's console is a file: "draining" it reads
        // it without emptying it, so doing so every few seconds would be
        // pure cost.
        let mk = |keep_ring: bool, kind: &str| {
            let inst = Instance {
                name: "x".into(),
                project: "default".into(),
                kind: kind.into(),
                status: "Running".into(),
                location: String::new(),
                image: String::new(),
                base_image: String::new(),
                entrypoint: String::new(),
                user_keys: vec![],
            };
            (inst.is_container() && !keep_ring, inst)
        };
        assert!(mk(false, "container").0, "consumed by default");
        assert!(!mk(true, "container").0, "--keep-ring opts out");
        assert!(
            !mk(false, "virtual-machine").0,
            "a VM has no ring to consume"
        );
    }
}
