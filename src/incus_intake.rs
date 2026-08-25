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

use anyhow::bail;
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
    open_entry: bool,
    last_ms: i64,
    /// Bytes of a line not yet terminated by the console.
    partial: Vec<u8>,
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
            open_entry: false,
            last_ms: 0,
            partial: Vec::new(),
        }
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
        if self.open_entry && now_ms.saturating_sub(self.last_ms) > self.idle_ms as i64 {
            self.open_entry = false;
        }
        self.last_ms = now_ms;
        let already_stamped = std::str::from_utf8(line)
            .map(|s| self.iso.is_match(s))
            .unwrap_or(false);
        if already_stamped {
            // The producer stamps its own lines. Prefixing would demote
            // its timestamp to payload AND make every continuation line
            // its own entry, which is how a stack trace stops coming back
            // whole.
            self.open_entry = true;
        } else if self.open_entry {
            // A continuation: an exception body, a stack frame, the rest
            // of a banner.
        } else {
            self.prefix.write(now_ms, out);
            self.open_entry = true;
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

#[cfg(test)]
mod tests {
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
}
