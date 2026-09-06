//! The file intake: a NAMED SET of a system's log files, tailed into a
//! store each, by one process.
//!
//! The producer keeps writing its own files and timberfs reads them, so
//! nothing in its write path depends on this — the same contract
//! `timberfs-follow@` has, one declaration wider. A system rarely has one
//! log: Exim writes three, Apache two, and stating the retention policy
//! for «exim» once is the point.
//!
//! No registry, unlike a follower's. A follower's positions are durable
//! state something else reads (`retain_unconsumed` takes its floor from
//! them); a tail's checkpoint is the store it writes, so there is no
//! position to keep and nothing to keep it in. That same statelessness is
//! what makes one process safe here: a restart re-syncs against each
//! store's own lines and can neither lose nor duplicate.
//!
//! See docs/plans/file-intake.md.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

/// Where a set's declaration lives, under `/etc/timberfs`.
pub const CONF_DIR: &str = "file.d";

const STORE_DIR: &str = "STORE_DIR";
const DECLARE: &str = "DECLARE";
const SOURCE: &str = "SOURCE";
const POLL: &str = "POLL";
const FLUSH_AGE: &str = "FLUSH_AGE";
const ROTATED: &str = "ROTATED";

/// Every key this build reads, for the message an unknown one gets.
const KEYS: &[&str] = &[STORE_DIR, DECLARE, SOURCE, POLL, FLUSH_AGE, ROTATED];

/// ⚠ NOT an `EXTRA_OPTS` string of flags, which is what the systemd units
/// carry. A unit can only hand a CLI a string, so its knobs must be
/// spelled as flags; a file timberfs parses itself has no such excuse, and
/// structure inside a string is structure re-parsed under rules neither
/// side owns. Typed keys are validated at startup with a line number, and
/// a misspelled one is refused rather than passed through to be ignored.
const DEFAULT_POLL_MS: u64 = 1_000;
const DEFAULT_FLUSH_AGE_MS: u64 = 60_000;

/// One source in a set, and what it becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The section name, which IS the store's name and its handle.
    pub store: String,
    pub source: PathBuf,
    pub store_dir: PathBuf,
    /// `KEY=VALUE` declarations handed to `set`, in the order written.
    pub declare: Vec<String>,
    /// How often to look for new data and for a replaced file.
    pub poll_ms: u64,
    /// How long followed data may sit unflushed — a visibility knob, not a
    /// durability one: the source file is the durable copy.
    pub flush_age_ms: u64,
    /// Where rotation moves the source, for data written while this was
    /// NOT running. Empty means the derived defaults.
    pub rotated: Vec<PathBuf>,
}

impl Entry {
    /// `<store_dir>/<store>/<store>.log` — the layout every timberfs
    /// intake writes, where the path spells what the store IS and never
    /// how the data got in.
    pub fn dest(&self) -> PathBuf {
        self.store_dir
            .join(&self.store)
            .join(format!("{}.log", self.store))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSet {
    pub name: String,
    pub entries: Vec<Entry>,
}

/// What the preamble declared, and so what a section inherits.
#[derive(Default)]
struct Defaults {
    dir: Option<PathBuf>,
    declare: Option<Vec<String>>,
    poll_ms: Option<u64>,
    flush_age_ms: Option<u64>,
    rotated: Option<Vec<PathBuf>>,
}

/// A section being read. Each field is `None` until the section states it,
/// which is what lets a section fall back to the preamble per KEY.
struct Open {
    store: String,
    at: usize,
    source: Option<PathBuf>,
    dir: Option<PathBuf>,
    declare: Option<Vec<String>>,
    poll_ms: Option<u64>,
    flush_age_ms: Option<u64>,
    rotated: Option<Vec<PathBuf>>,
}

impl Open {
    /// Resolve against the preamble and become an entry. A section that
    /// states a key REPLACES the default wholly rather than merging into
    /// it: `DECLARE` is a space-separated string, and a partial merge of
    /// one is a rule nobody can predict from reading the file.
    fn close(self, d: &Defaults) -> anyhow::Result<Entry> {
        let Some(source) = self.source else {
            bail!(
                "[{}] (line {}) declares no {SOURCE}, and there is no sane default",
                self.store,
                self.at
            );
        };
        Ok(Entry {
            store: self.store,
            source,
            store_dir: self
                .dir
                .or_else(|| d.dir.clone())
                .unwrap_or_else(|| PathBuf::from("/var/log/timberfs")),
            declare: self
                .declare
                .or_else(|| d.declare.clone())
                .unwrap_or_default(),
            poll_ms: self.poll_ms.or(d.poll_ms).unwrap_or(DEFAULT_POLL_MS),
            flush_age_ms: self
                .flush_age_ms
                .or(d.flush_age_ms)
                .unwrap_or(DEFAULT_FLUSH_AGE_MS),
            rotated: self
                .rotated
                .or_else(|| d.rotated.clone())
                .unwrap_or_default(),
        })
    }
}

/// A section name becomes a directory, a file name and a handle an
/// operator types, so it is VALIDATED rather than sanitized. The network
/// intakes sanitize because a sender chose the name and cannot be told;
/// here it was typed into a file by whoever runs the machine, so a name
/// that cannot be a store is a typo to report — silently rewriting it
/// would leave them querying a handle that does not exist.
fn check_store_name(name: &str, line: usize) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("line {line}: a section needs a name — it becomes the store's name");
    }
    if name.starts_with('.') {
        bail!("line {line}: [{name}] cannot name a store: a leading dot hides the directory");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| c.is_whitespace() || *c == '/' || *c == '\0')
    {
        bail!("line {line}: [{name}] cannot name a store: {bad:?} is not allowed in a handle");
    }
    Ok(())
}

/// Parse one set's declaration.
///
/// ⚠ A line this build cannot use is FATAL here, where `limits.conf` skips
/// it. The postures differ because the consequences do: a ceiling that was
/// skipped still answers the query, while a source that was skipped is a
/// log nobody is collecting and nothing later says so. An intake has a
/// startup to fail at, and this is what it is for.
pub fn parse(name: &str, text: &str) -> anyhow::Result<FileSet> {
    let mut defaults = Defaults::default();
    let mut open: Option<Open> = None;
    let mut entries: Vec<Entry> = Vec::new();
    // Both keyed for the message a collision has to produce: which OTHER
    // section, and where.
    let mut named: BTreeMap<String, usize> = BTreeMap::new();
    let mut followed: BTreeMap<PathBuf, String> = BTreeMap::new();

    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }

        if let Some(head) = s.strip_prefix('[') {
            let Some(store) = head.strip_suffix(']') else {
                bail!("line {line}: a section header ends with ']' — got {s:?}");
            };
            let store = store.trim().to_string();
            check_store_name(&store, line)?;
            if let Some(first) = named.insert(store.clone(), line) {
                bail!(
                    "line {line}: [{store}] was already declared on line {first} — \
                     a store is named once"
                );
            }
            if let Some(prev) = open.take() {
                push(prev, &defaults, &mut entries, &mut followed)?;
            }
            open = Some(Open {
                store,
                at: line,
                source: None,
                dir: None,
                declare: None,
                poll_ms: None,
                flush_age_ms: None,
                rotated: None,
            });
            continue;
        }

        let Some((key, value)) = s.split_once('=') else {
            bail!("line {line}: expected KEY=VALUE or [section] — got {s:?}");
        };
        let key = key.trim();
        let value = value.trim();
        if !KEYS.contains(&key) {
            bail!(
                "line {line}: unknown key {key:?} — this build reads {}",
                KEYS.join(", ")
            );
        }
        // Parsed HERE, so a bad duration or a relative path is a startup
        // failure naming its line rather than a tail that dies later.
        let words = || -> Vec<String> { value.split_whitespace().map(str::to_string).collect() };
        let ms = |k: &str| -> anyhow::Result<u64> {
            crate::append::parse_duration_ms(value)
                .with_context(|| format!("line {line}: {k}={value}"))
        };
        let paths = || -> anyhow::Result<Vec<PathBuf>> {
            value
                .split_whitespace()
                .map(|w| {
                    let p = PathBuf::from(w);
                    if p.is_absolute() {
                        Ok(p)
                    } else {
                        bail!("line {line}: {ROTATED} {w} must be an absolute path")
                    }
                })
                .collect()
        };

        let Some(cur) = open.as_mut() else {
            // The preamble: everything but a source, which belongs to the
            // store that follows it.
            match key {
                SOURCE => bail!(
                    "line {line}: {SOURCE} belongs to a section — it is what ONE store \
                     follows, and a set has no single one"
                ),
                STORE_DIR => defaults.dir = Some(PathBuf::from(value)),
                DECLARE => defaults.declare = Some(words()),
                POLL => defaults.poll_ms = Some(ms(POLL)?),
                FLUSH_AGE => defaults.flush_age_ms = Some(ms(FLUSH_AGE)?),
                _ => defaults.rotated = Some(paths()?),
            }
            continue;
        };
        match key {
            SOURCE => {
                let p = PathBuf::from(value);
                if !p.is_absolute() {
                    bail!(
                        "line {line}: {SOURCE}={value} must be an absolute path — an intake \
                         has no working directory for a relative one to mean anything"
                    );
                }
                cur.source = Some(p);
            }
            STORE_DIR => cur.dir = Some(PathBuf::from(value)),
            DECLARE => cur.declare = Some(words()),
            POLL => cur.poll_ms = Some(ms(POLL)?),
            FLUSH_AGE => cur.flush_age_ms = Some(ms(FLUSH_AGE)?),
            _ => cur.rotated = Some(paths()?),
        }
    }
    if let Some(last) = open {
        push(last, &defaults, &mut entries, &mut followed)?;
    }

    if entries.is_empty() {
        bail!("no [section] declares a source, so this set would follow nothing");
    }
    Ok(FileSet {
        name: name.to_string(),
        entries,
    })
}

/// Resolve a finished section and file it, refusing a source two sections
/// share: one file into two stores doubles the reading and the bytes, and
/// is a copied section somebody forgot to edit.
fn push(
    open: Open,
    defaults: &Defaults,
    entries: &mut Vec<Entry>,
    followed: &mut BTreeMap<PathBuf, String>,
) -> anyhow::Result<()> {
    let entry = open.close(defaults)?;
    if let Some(other) = followed.insert(entry.source.clone(), entry.store.clone()) {
        bail!(
            "[{}] and [{other}] both follow {} — one file into two stores doubles \
             the work and the data",
            entry.store,
            entry.source.display()
        );
    }
    entries.push(entry);
    Ok(())
}

/// Read `<etc>/file.d/<name>.conf`.
pub fn load(name: &str, etc: &Path) -> anyhow::Result<FileSet> {
    let path = etc.join(CONF_DIR).join(format!("{name}.conf"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading the file-intake set {name} from {}", path.display()))?;
    parse(name, &text).with_context(|| format!("in {}", path.display()))
}

/// How the set is run, as opposed to what it declares.
#[derive(Debug, Clone)]
pub struct RunOpts {
    /// Exit 85 when this binary is replaced on disk, so a supervised run
    /// re-execs into the new one.
    pub exit_on_upgrade: bool,
    /// Declare and converge, then exit without tailing anything.
    pub check_only: bool,
}

/// Bring one entry's store to what the set declares, before anything
/// tails it: `create --if-not-exists` then `set`, which is exactly what
/// `timberfs-follow@`'s two `ExecStartPre=` lines do, in process.
fn converge(entry: &Entry, host: &str) -> anyhow::Result<()> {
    let dest = entry.dest();
    crate::bark::cmd_create(&dest, false, false, None, None, false, &[], true)
        .with_context(|| format!("[{}] creating {}", entry.store, dest.display()))?;
    let mut sets = vec![format!("host={host}")];
    sets.extend(entry.declare.iter().cloned());
    crate::bark::declare(&dest, &sets, &[])
        .with_context(|| format!("[{}] declaring {}", entry.store, sets.join(" ")))?;
    Ok(())
}

/// Run a set: a tail per entry, in one process.
///
/// ⚠ Every store is DECLARED first, serially, before any tail starts. A
/// typo in the fifth section then fails the unit at startup rather than
/// after four tails are already writing — which is the difference between
/// a config error and a partial deployment nobody notices.
pub fn cmd_file_intake(set: &FileSet, opts: &RunOpts) -> anyhow::Result<()> {
    let host = hostname();
    for entry in &set.entries {
        converge(entry, &host)?;
    }
    if opts.check_only {
        for entry in &set.entries {
            println!(
                "{}\t{}\t{}",
                entry.store,
                entry.source.display(),
                entry.dest().display()
            );
        }
        return Ok(());
    }

    crate::append::install_signal_handlers();
    crate::note!(
        "timberfs: file intake {} following {} source(s)",
        set.name,
        set.entries.len()
    );

    // ONE binary watch for the set. A per-store watch would flush its own
    // store and `exit`, which is right for a single-store writer and wrong
    // here: the other tails would go with it holding unflushed buffers.
    // Tripping the stop flag instead winds every one of them down through
    // the path SIGTERM already uses.
    let watch = opts
        .exit_on_upgrade
        .then(crate::store::BinaryWatch::current)
        .flatten();
    let upgraded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(watch) = watch {
        let upgraded = std::sync::Arc::clone(&upgraded);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            if crate::append::stopping() {
                return;
            }
            if watch.changed() {
                crate::note!("timberfs: file intake: binary replaced; winding down to re-exec");
                upgraded.store(true, std::sync::atomic::Ordering::Relaxed);
                crate::append::request_stop();
                return;
            }
        });
    }

    let (tx, rx) = std::sync::mpsc::channel::<(String, anyhow::Result<()>)>();
    for entry in &set.entries {
        let store = entry.store.clone();
        let entry = entry.clone();
        let tx = tx.clone();
        std::thread::Builder::new()
            .name(store.clone())
            .spawn(move || {
                // A panicking thread dies alone and SILENTLY: that store
                // would stop while the unit stayed green and every other
                // store kept filling. Caught so it is reported by name —
                // the default hook has already printed it to the journal.
                let store = entry.store.clone();
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tail(&entry)));
                let outcome = ran.unwrap_or_else(|_| {
                    Err(anyhow::anyhow!("the tail panicked; see the log above"))
                });
                tx.send((store, outcome)).ok();
            })
            .with_context(|| format!("[{store}] starting its tail"))?;
    }
    drop(tx);

    // A tail returns only when it is asked to stop or it fails. Either way
    // the set is a unit of supervision: the first one to end that was not
    // asked to decides the exit, and systemd restarts all of them.
    let mut failure: Option<(String, anyhow::Error)> = None;
    let mut ended = 0usize;
    while let Ok((store, outcome)) = rx.recv() {
        ended += 1;
        match outcome {
            Err(e) if failure.is_none() => {
                crate::note!("timberfs: file intake: [{store}] ended: {e:#}");
                failure = Some((store, e));
                crate::append::request_stop();
            }
            Err(_) => {}
            Ok(()) if !crate::append::stopping() && failure.is_none() => {
                failure = Some((
                    store.clone(),
                    anyhow::anyhow!("[{store}] stopped following on its own"),
                ));
                crate::append::request_stop();
            }
            Ok(()) => {}
        }
    }
    debug_assert_eq!(ended, set.entries.len());

    if upgraded.load(std::sync::atomic::Ordering::Relaxed) {
        std::process::exit(crate::store::EXIT_BINARY_UPGRADED);
    }
    match failure {
        Some((store, e)) => Err(e).with_context(|| {
            format!(
                "file intake {}: [{store}] ended, so the whole set was stopped",
                set.name
            )
        }),
        None => Ok(()),
    }
}

fn tail(entry: &Entry) -> anyhow::Result<()> {
    crate::follow::cmd_follow(
        &entry.source,
        &entry.dest(),
        crate::store::Config {
            chunk_size: 256 * 1024,
            level: 3,
            flush_age_ms: entry.flush_age_ms,
        },
        crate::import::ImportOpts {
            // The store's own declaration decides these: `converge` has
            // already written what the set asked for, and `cmd_follow`
            // merges the manifest's format in.
            time: crate::bark::TimeFormat::default(),
            quick: false,
            index: false,
            wal: false,
        },
        None,
        None,
        crate::follow::FollowOpts {
            poll_ms: entry.poll_ms,
            rotated: entry.rotated.clone(),
            // The set owns the watch; a per-store one would exit the
            // process while the other tails still held unflushed buffers.
            exit_on_upgrade: false,
            wait_for_writer: 0.0,
        },
    )
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> anyhow::Result<FileSet> {
        parse("exim", text)
    }

    /// The shape the whole change exists for: one system's logs, one file,
    /// with the policy stated once and overridden where it differs.
    #[test]
    fn a_section_is_a_store_and_inherits_the_sets_defaults() {
        let set = one("STORE_DIR=/srv/logs\n\
             DECLARE=index=true retain=90d\n\
             POLL=2s\n\
             \n\
             [exim-main]\n\
             SOURCE=/var/log/exim4/mainlog\n\
             \n\
             [exim-panic]\n\
             SOURCE=/var/log/exim4/paniclog\n\
             DECLARE=retain=365d\n\
             FLUSH_AGE=5s\n")
        .unwrap();
        assert_eq!(set.entries.len(), 2);

        let main = &set.entries[0];
        assert_eq!(main.store, "exim-main");
        assert_eq!(
            main.dest(),
            PathBuf::from("/srv/logs/exim-main/exim-main.log")
        );
        assert_eq!(main.declare, ["index=true", "retain=90d"]);
        assert_eq!(main.poll_ms, 2_000);
        assert_eq!(main.flush_age_ms, DEFAULT_FLUSH_AGE_MS);

        // A section that states a key REPLACES the default wholly — the
        // 90d is gone rather than merged under the 365d.
        let panic = &set.entries[1];
        assert_eq!(panic.declare, ["retain=365d"]);
        assert_eq!(panic.flush_age_ms, 5_000);
        assert_eq!(panic.poll_ms, 2_000, "what it did not state, it inherits");
        assert_eq!(panic.store_dir, PathBuf::from("/srv/logs"));
    }

    /// Every refusal here is a log nobody would be collecting, so each is
    /// a startup failure naming its line rather than a skipped line.
    #[test]
    fn a_declaration_that_would_collect_nothing_is_refused() {
        let cases = [
            ("STORE_DIR=/srv\n", "would follow nothing"),
            ("[a]\n", "declares no SOURCE"),
            (
                "SOURCE=/var/log/x\n[a]\nSOURCE=/var/log/y\n",
                "belongs to a section",
            ),
            ("[a]\nSOURCE=relative/path\n", "must be an absolute path"),
            ("[a]\nSOURCE=/x\nRETAIN=90d\n", "unknown key"),
            (
                "[a]\nSOURCE=/x\n[a]\nSOURCE=/y\n",
                "already declared on line",
            ),
            ("[a]\nSOURCE=/x\n[b]\nSOURCE=/x\n", "both follow /x"),
            ("[a b]\nSOURCE=/x\n", "not allowed in a handle"),
            ("[.hidden]\nSOURCE=/x\n", "a leading dot"),
            ("[a\nSOURCE=/x\n", "ends with ']'"),
            ("nonsense\n", "expected KEY=VALUE"),
            ("[a]\nSOURCE=/x\nPOLL=never\n", "POLL=never"),
        ];
        for (text, want) in cases {
            let err = one(text).expect_err(text);
            let msg = format!("{err:#}");
            assert!(msg.contains(want), "{text:?} gave {msg:?}, wanted {want:?}");
        }
    }

    /// The set-wide default is the point of the preamble, and the built-in
    /// one has to hold for a file that states nothing but its sources.
    #[test]
    fn the_smallest_useful_declaration_is_a_source() {
        let set = one("[app]\nSOURCE=/var/log/app.log\n").unwrap();
        let e = &set.entries[0];
        assert_eq!(e.store_dir, PathBuf::from("/var/log/timberfs"));
        assert_eq!(e.dest(), PathBuf::from("/var/log/timberfs/app/app.log"));
        assert!(e.declare.is_empty());
        assert!(e.rotated.is_empty());
        assert_eq!(
            (e.poll_ms, e.flush_age_ms),
            (DEFAULT_POLL_MS, DEFAULT_FLUSH_AGE_MS)
        );
    }

    /// An «everything on this host» set is a longer list and nothing else
    /// — no mode, no flag, and each store still its own policy.
    #[test]
    fn a_host_wide_set_is_just_a_longer_list() {
        let set = parse(
            "host",
            "[exim-main]\nSOURCE=/var/log/exim4/mainlog\nDECLARE=retain=90d\n\
             [apache-access]\nSOURCE=/var/log/apache2/access.log\nDECLARE=retain=30d\n\
             [syslog]\nSOURCE=/var/log/syslog\nSTORE_DIR=/srv/other\n",
        )
        .unwrap();
        assert_eq!(set.entries.len(), 3);
        assert_eq!(set.entries[2].store_dir, PathBuf::from("/srv/other"));
        // Sources need not share a directory: a set is a named LIST.
        let dirs: Vec<_> = set
            .entries
            .iter()
            .filter_map(|e| e.source.parent())
            .collect();
        assert_eq!(dirs.len(), 3);
        assert!(dirs.iter().collect::<std::collections::BTreeSet<_>>().len() == 3);
    }

    #[test]
    fn comments_and_blank_lines_are_not_declarations() {
        let set = one("# a set\n\n  # indented\n[a]\n\nSOURCE=/x\n").unwrap();
        assert_eq!(set.entries.len(), 1);
        assert_eq!(set.entries[0].source, PathBuf::from("/x"));
    }
}
