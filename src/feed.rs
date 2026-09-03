//! Feeding a consumer: the loop that owns the read, the selection and
//! the positions, and hands records to a program that says how far to
//! move them.
//!
//! The consumer is a CHILD, and they live and die together: if it exits,
//! this exits non-zero and the unit is restarted. One lifecycle in one
//! place, and a fresh stream every time — which is also what lets the
//! announced-labels map live in memory, since the consumer's own copy of
//! it has exactly the same life (see consumer.rs and
//! docs/plans/consumer-protocol.md).
//!
//! ⚠ Reports are read on their OWN THREAD. Writing records into a full
//! pipe blocks, and the consumer writing reports into a full pipe blocks
//! too — so draining reports only between writes would deadlock the pair
//! at the first busy moment.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::{Map, Value};

use crate::consumer::{Diet, Report};
use crate::records::Frames;
use crate::ship::{Shipper, Store};

/// How long to wait for a consumer's hello before giving up on it. A
/// consumer reached over ssh may take a moment; one that never speaks
/// must not wedge a follower in silence.
pub const HELLO_WAIT: Duration = Duration::from_secs(30);

/// How long a consumer may say NOTHING mid-stream, with something in
/// flight, before a one-shot gives up on it.
///
/// ⚠ NOT the same knob as `HELLO_WAIT`, though the numbers agree here.
/// The two pull in opposite directions under test: a consumer that never
/// speaks must not stall the suite, so the hello wait wants to be SHORT,
/// while a shell consumer forking five processes an entry must not be
/// given up on, so this wants to be LONG. One field could not be both,
/// which is the argument that they are two questions.
pub const MAX_SILENCE: Duration = Duration::from_secs(30);

/// How long a consumer gets to exit after its input closes, before it is
/// killed. It has already been told the stream ended; this is only the
/// difference between a clean report and a signal.
const GRACE: Duration = Duration::from_secs(5);

pub struct Opts {
    pub selector: crate::select::Selector,
    pub dirs: Vec<PathBuf>,
    pub positions: Option<PathBuf>,
    pub batch_entries: u64,
    pub poll: Duration,
    /// Keep going when a poll finds nothing. Without it the loop drains
    /// what is there and exits — a one-shot, durable because the
    /// positions are.
    pub follow: bool,
    /// The consumer and its arguments. A LIST, so no quoting round trip
    /// happens on the way to it.
    pub argv: Vec<String>,
    /// How long to wait for the hello.
    pub hello_wait: Duration,
    /// How long the consumer may say nothing mid-stream with something
    /// in flight, before a one-shot stops waiting (see `MAX_SILENCE`).
    pub max_silence: Duration,
    /// Where a store this follower has never read is picked up.
    pub follow_from: crate::ship::FollowFrom,
    /// When the interest began, if something knows better than the
    /// positions file — see `Shipper::since_declared`.
    pub since: Option<String>,
}

/// What a position file says wrote it: the consumer's own program, so an
/// operator reading one learns what was feeding it. The verb's name
/// would be the same string for every feed on the host, which answers
/// nothing.
fn consumer_name(argv0: &str) -> String {
    Path::new(argv0)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| argv0.to_string())
}

/// How the loop waits, which is all `feed` needs of `Opts`.
struct Pacing {
    poll: Duration,
    follow: bool,
    /// See `MAX_SILENCE`.
    max_silence: Duration,
}

/// One store as this loop tracks it across polls.
struct Tracked {
    path: String,
    /// The labels last ANNOUNCED to the consumer. In memory only: the
    /// consumer's copy dies with the stream, so persisting ours could
    /// only make the two disagree.
    announced: Option<Map<String, Value>>,
    /// Entries forwarded and not yet acknowledged. A watermark names
    /// bytes; what the position file records beside it — the chunk for
    /// the retention floor, the write time for a person reading it —
    /// was stated by the answer when those bytes went out, and this is
    /// where it is kept until the consumer says it took them. Bounded by
    /// the batch size.
    pending: Vec<Pending>,
}

struct Pending {
    /// Just past this entry, which is what a watermark for it looks like.
    end: u64,
    chunk: Option<u64>,
    wl: u64,
}

pub fn run(opts: Opts) -> anyhow::Result<()> {
    let Opts {
        selector,
        dirs,
        positions,
        batch_entries,
        poll,
        follow,
        argv,
        hello_wait,
        max_silence,
        follow_from,
        since,
    } = opts;
    let (argv0, args) = argv
        .split_first()
        .context("no consumer to feed: give the command after `--`")?;
    // ⚠ This verb, unlike every other, must NOT die of SIGPIPE. `main`
    // restores the default so `query | head` ends quietly like any Unix
    // tool; here the pipe is a consumer that may have crashed, and a
    // follower that vanishes on signal 13 tells systemd nothing and an
    // operator less. Ignored, so the write returns EPIPE and the child's
    // own exit status can be reported as the cause.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let mut child = Command::new(argv0)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning the consumer {argv0}"))?;
    let mut sink = child.stdin.take().expect("piped stdin");
    let reports = spawn_reports(&mut child);

    let mut shipper = Shipper::open(&consumer_name(argv0), selector, dirs, positions.as_deref())?
        .with_batch_entries(batch_entries)
        .with_follow_from(follow_from);
    if let Some(when) = since.as_deref() {
        shipper = shipper.since_declared(when);
    }

    // Nothing is read from a store until the consumer has said it
    // implements this protocol: a position must not move on the word of
    // something that never said so.
    let holds = match await_hello(&reports, hello_wait) {
        Ok(h) => h,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };

    let mut tracked: HashMap<String, Tracked> = HashMap::new();
    let mut result = feed(
        &mut shipper,
        &mut sink,
        &reports,
        Pacing {
            poll,
            follow,
            max_silence,
        },
        holds,
        &mut tracked,
    );
    // Closing its stdin is how a consumer learns the stream ended, so
    // that comes first and a kill only after it has had a chance to
    // leave on its own — killing at once would report every clean
    // one-shot as a signal death.
    drop(sink);
    // Everything the consumer will ever say, before deciding anything:
    // it has the whole stream and closed input tells it so, and a
    // watermark arriving a millisecond after an arbitrary sleep is a
    // batch re-sent next run for no reason.
    if result.is_ok() {
        result = drain(&mut shipper, &reports, &mut tracked, Drain::ToEnd).and_then(|moved| {
            if moved {
                shipper.persist()?;
            }
            Ok(())
        });
    }
    let status = wait_out(&mut child, GRACE);
    // The child's own failure is the CAUSE and a broken pipe here is its
    // symptom, so it is reported first — otherwise a consumer that
    // exited 3 is announced as "could not write", which sends the reader
    // looking in the wrong place.
    match (status, result) {
        (Some(s), r) if !s.success() => match r {
            Err(e) => bail!("the consumer exited {s} ({e:#})"),
            Ok(()) => bail!("the consumer exited {s}"),
        },
        (None, _) => bail!(
            "the consumer did not exit within {}s of its input closing, and was killed",
            GRACE.as_secs()
        ),
        (Some(_), r) => r,
    }
}

/// Wait for the consumer to leave, killing it if it will not. `None`
/// means it had to be killed.
fn wait_out(child: &mut Child, grace: Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Err(_) => return None,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Read reports off-thread, so a full pipe in one direction cannot stop
/// the other being drained.
fn spawn_reports(child: &mut Child) -> mpsc::Receiver<anyhow::Result<Report>> {
    let out = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut r = crate::consumer::Reader::new(BufReader::new(out));
        loop {
            match r.next_report() {
                Ok(Some(rep)) => {
                    if tx.send(Ok(rep)).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            }
        }
    });
    rx
}

fn await_hello(
    reports: &mpsc::Receiver<anyhow::Result<Report>>,
    wait: Duration,
) -> anyhow::Result<Vec<(String, u64)>> {
    match reports.recv_timeout(wait) {
        Ok(Ok(Report::Hello { reads, holds })) => match reads {
            Diet::Records => Ok(holds),
            // In the grammar so a consumer can ask, refused so it is
            // told rather than fed the wrong thing: the chunks path has
            // no cursor parameter and emits no positions yet (see
            // docs/plans/consumer-protocol.md).
            Diet::Chunks => bail!(
                "the consumer reads chunks, which this timberfs does not serve yet — it has \
                 no per-store position on that path"
            ),
        },
        Ok(Ok(_)) => bail!("the consumer reported before saying hello"),
        Ok(Err(e)) => Err(e),
        Err(RecvTimeoutError::Timeout) => bail!(
            "the consumer said no hello in {}s. Every consumer declares itself — see \
             timberfs-records(5) and the consumer protocol",
            wait.as_secs()
        ),
        Err(RecvTimeoutError::Disconnected) => {
            bail!("the consumer closed its output without saying hello")
        }
    }
}

fn feed(
    shipper: &mut Shipper,
    sink: &mut impl Write,
    reports: &mpsc::Receiver<anyhow::Result<Report>>,
    pacing: Pacing,
    holds: Vec<(String, u64)>,
    tracked: &mut HashMap<String, Tracked>,
) -> anyhow::Result<()> {
    // A claimed watermark is honoured only where nothing is recorded:
    // a number is not a proof, and skipping what was never delivered is
    // the silent failure. Where there IS no position it costs nothing we
    // know and saves re-shipping a store the destination already holds.
    for (id, offset) in holds {
        if shipper.seed(&id, "", offset) {
            crate::note!(
                "timberfs: {id}: the consumer already holds it through offset {offset}; \
                 starting there"
            );
        } else {
            crate::note!(
                "timberfs: {id}: the consumer claims offset {offset}, and a recorded position \
                 outranks a claim — starting where we left off"
            );
        }
    }
    shipper.persist()?;

    // One stream for the consumer's whole life: stream-start once,
    // `source` as stores join or change, entries indefinitely, and NO
    // stream-end — whose absence is this format's "still live" marker.
    write!(
        sink,
        "\x1estream-start\x1fv=1\x1fserver_version={}\x1ffollow=1\x1forder=arrival",
        crate::querydoc::server_version()
    )?;
    sink.write_all(b"\0")?;

    let mut said: Option<usize> = None;
    // When the consumer last went quiet with something in flight; reset
    // by every report. `None` means it is not quiet.
    let mut quiet_since: Option<std::time::Instant> = None;
    // Did the last read hold a parked store out? If it did, finding
    // nothing says nothing about the selection being drained. Set by
    // every read before it happens, so the initializer is never read —
    // declared here because the loop's tail needs it.
    let mut skipped;
    loop {
        let (buf, stores, matched) = {
            // A store with anything UNACKNOWLEDGED is parked: it has
            // nothing new to offer, and asking would cost the stores
            // beside it. Its recorded position only moves when the
            // consumer acknowledges, so a read starting there hands back
            // the entries already in flight — duplicates, filling the
            // shared entry cap, on every poll, for as long as the
            // trouble lasts. Measured: a consumer taking entries and
            // acknowledging none held this at 99% of a core without the
            // park and 0% with it, and the test for it does not fail
            // without it, it never finishes.
            //
            // ⚠ So the depth is one outstanding batch per store, and
            // pipelining deeper is not a matter of raising a number: it
            // needs a SENT offset per store, kept beside the
            // acknowledged one and read from instead of it. Otherwise
            // "further ahead" means "the same entries again", which is
            // what this stops.
            let parked = |id: &str| tracked.get(id).is_some_and(|t| !t.pending.is_empty());
            // Whether ANY store was held out of this read, because that
            // decides what its emptiness means (see below).
            skipped = tracked.values().any(|t| !t.pending.is_empty());
            shipper.poll_excluding(&parked)?
        };
        if said != Some(matched) {
            crate::note!("timberfs: following {matched} store(s)");
            said = Some(matched);
        }
        announce(sink, &stores, tracked)?;
        let (sent, more, stopped_in) = splice(sink, &buf, tracked)?;
        shipper.stopped_in(stopped_in);
        sink.flush()?;

        if drain(shipper, reports, tracked, Drain::Now)? {
            shipper.persist()?;
        }
        // Work was done, so go again rather than waiting on a clock.
        if sent > 0 || more {
            continue;
        }
        // Nothing to send. Either the stores are quiet, or every store
        // that has anything left is parked waiting to be acknowledged.
        if !tracked.values().any(|t| !t.pending.is_empty()) {
            // ⚠ And only where the read was not narrowed. A store parked
            // AT POLL TIME is excluded from the read, and its report can
            // arrive in the drain a few lines above — so "nothing to
            // send and nothing in flight" is reached with the store
            // unparked and still holding entries nobody asked it for.
            // That is a race between the exclusion and the drain, and it
            // ended a one-shot at 6 of 8 entries on a CI runner while
            // passing here: the same defect as the timeout below, one
            // path over.
            if skipped {
                continue;
            }
            if !pacing.follow {
                // The caller closes the consumer's input and drains it
                // to the end, which is exact where a sleep is a guess.
                return Ok(());
            }
            thread::sleep(pacing.poll);
            continue;
        }
        // Something is in flight, so what to wait for is a REPORT and
        // not the clock: a consumer that catches up is served at once
        // rather than at the next tick, and a poll interval never
        // becomes the ceiling on one store's throughput.
        match reports.recv_timeout(pacing.poll) {
            Ok(rep) => {
                quiet_since = None;
                take(shipper, tracked, rep?)?;
                drain(shipper, reports, tracked, Drain::Now)?;
                shipper.persist()?;
            }
            // ⚠ A poll interval of silence is NOT the end of a one-shot,
            // which is what it used to be taken for. A store with
            // anything unacknowledged is parked, so its remaining
            // entries become available only when its report arrives —
            // returning here abandoned them, and a consumer merely
            // SLOWER than the poll interval then drained a fraction of
            // the selection and exited 0. Measured before the fix: 4 of
            // 20 entries, with a consumer taking 100 ms an entry against
            // a 20 ms poll.
            //
            // So silence is only an end once it has lasted long enough
            // to mean the consumer is not coming back, and that is the
            // same question `max_silence` answers at the hello.
            Err(RecvTimeoutError::Timeout) => {
                let since = *quiet_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= pacing.max_silence {
                    if !pacing.follow {
                        let stalled: Vec<&str> = tracked
                            .iter()
                            .filter(|(_, t)| !t.pending.is_empty())
                            .map(|(id, _)| id.as_str())
                            .collect();
                        // Not an error: the positions are honest about
                        // what was taken, so a re-run resumes exactly
                        // here. But saying nothing would report a
                        // part-drained selection as a finished one.
                        crate::note!(
                            "timberfs: the consumer said nothing for {}s with {} store(s) \
                             unacknowledged ({}) — stopping there; their positions did not \
                             move, so a re-run sends those entries again",
                            pacing.max_silence.as_secs().max(1),
                            stalled.len(),
                            stalled.join(", ")
                        );
                        return Ok(());
                    }
                    quiet_since = None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => bail!("the consumer closed its output"),
        }
    }
}

/// How far to drain a consumer's reports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drain {
    /// Whatever has arrived. Its end is the loop carrying on, so the
    /// consumer's output closing is a failure.
    Now,
    /// Until its output closes, which is how a consumer says it has
    /// finished speaking.
    ToEnd,
}

/// A `source` record for every store the consumer has not been told
/// about, or whose labels have changed since it was.
///
/// ⚠ It is also a FLUSH BOUNDARY for that store: a consumer batching
/// entries must ship what it holds before adopting new labels, or
/// entries that arrived under the old ones go out attributed to the new.
fn announce(
    sink: &mut impl Write,
    stores: &[Store],
    tracked: &mut HashMap<String, Tracked>,
) -> anyhow::Result<()> {
    for (id, path, labels) in stores {
        let path_text = path.display().to_string();
        let entry = tracked.entry(id.clone()).or_insert_with(|| Tracked {
            path: path_text.clone(),
            announced: None,
            pending: Vec::new(),
        });
        if entry.announced.as_ref() == Some(labels) {
            continue;
        }
        entry.announced = Some(labels.clone());
        entry.path = path_text.clone();
        write!(sink, "\x1esource\x1fpath={path_text}\x1fid={id}\x1flabels=")?;
        sink.write_all(Value::Object(labels.clone()).to_string().as_bytes())?;
        sink.write_all(b"\0")?;
    }
    Ok(())
}

/// Forward the entries of one answer and drop everything else: its
/// brackets are this loop's business, and its `position` records are the
/// bookkeeping the consumer's own watermarks replace. Two authorities
/// for one number is a bug waiting to be written.
///
/// Returns how many entries went out, whether a bound stopped the read
/// (so the caller polls again rather than waiting), and which store it
/// stopped in (so the next read starts after that one).
fn splice(
    sink: &mut impl Write,
    buf: &[u8],
    tracked: &mut HashMap<String, Tracked>,
) -> anyhow::Result<(u64, bool, Option<String>)> {
    let mut sent = 0u64;
    let mut more = false;
    let mut last_delivering: Option<String> = None;
    for frame in Frames::new(buf) {
        let frame = frame?;
        match frame.kind {
            b"entry" => {
                let Some(id) = frame.field("id") else {
                    bail!("a followed answer left an entry unattributed");
                };
                let (Some(offset), Some(len)) = (frame.number("offset"), frame.number("len"))
                else {
                    bail!("an entry record states no offset and length to acknowledge");
                };
                if let Some(t) = tracked.get_mut(id) {
                    t.pending.push(Pending {
                        end: offset + len,
                        chunk: frame.number("chunk"),
                        wl: frame.number("wl").unwrap_or(0),
                    });
                }
                last_delivering = Some(id.to_string());
                sink.write_all(frame.bytes)?;
                sent += 1;
            }
            b"stream-end" => {
                more = frame.field("status") == Some("limited");
            }
            _ => {}
        }
    }
    // Only a capped read has a place to resume from: an exhausted one
    // read every store to its end, so the order next time does not
    // matter.
    Ok((sent, more, if more { last_delivering } else { None }))
}

/// One report, acted on. Extracted so the drain and the loop's own wait
/// cannot come to treat a watermark differently.
fn take(
    shipper: &mut Shipper,
    tracked: &mut HashMap<String, Tracked>,
    rep: Report,
) -> anyhow::Result<bool> {
    match rep {
        Report::Progress { id, offset } => {
            let Some(t) = tracked.get_mut(&id) else {
                crate::note!(
                    "timberfs: the consumer acknowledged store {id}, which it was never sent \
                     anything from — ignoring"
                );
                return Ok(false);
            };
            // What the position file records beside the offset — the
            // chunk for the retention floor, the write time for a person
            // — as the answer stated it when those entries went out.
            let mut chunk = None;
            let mut wl = 0u64;
            let mut delivered = 0u64;
            t.pending.retain(|p| {
                if p.end <= offset {
                    chunk = p.chunk.max(chunk);
                    wl = wl.max(p.wl);
                    delivered += 1;
                    false
                } else {
                    true
                }
            });
            let path = t.path.clone();
            shipper.acknowledge(&id, &path, offset, chunk, wl, delivered);
            Ok(true)
        }
        Report::Note { id, offset, text } => {
            // Recorded rather than only logged: `follower status` is
            // another process, and a stalled follower's reason has to
            // outlive the line it was printed on.
            crate::note!(
                "timberfs: consumer note{}{}: {text}",
                id.as_deref().map(|i| format!(" [{i}]")).unwrap_or_default(),
                offset.map(|o| format!(" @{o}")).unwrap_or_default()
            );
            Ok(shipper.take_note(id.as_deref(), offset, &text))
        }
        Report::Hello { .. } => bail!("the consumer said hello twice"),
    }
}

/// Take what the consumer has said, and move what it says to.
fn drain(
    shipper: &mut Shipper,
    reports: &mpsc::Receiver<anyhow::Result<Report>>,
    tracked: &mut HashMap<String, Tracked>,
    how: Drain,
) -> anyhow::Result<bool> {
    let mut moved = false;
    loop {
        let got = match how {
            Drain::Now => reports.try_recv(),
            Drain::ToEnd => reports.recv().map_err(|_| mpsc::TryRecvError::Disconnected),
        };
        match got {
            Ok(Ok(rep)) => moved |= take(shipper, tracked, rep)?,
            Ok(Err(e)) => return Err(e),
            Err(mpsc::TryRecvError::Empty) => return Ok(moved),
            Err(mpsc::TryRecvError::Disconnected) => match how {
                Drain::ToEnd => return Ok(moved),
                Drain::Now => bail!("the consumer closed its output"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Config, Store};
    use std::path::Path;

    /// A forest of stores, each line its own sealed entry.
    fn forest(tag: &str, stores: &[(&str, usize)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("timberfs-feed-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cfg = Config {
            chunk_size: 1 << 20,
            level: 3,
            flush_age_ms: 60_000,
        };
        for (name, lines) in stores {
            let dir = root.join(name);
            let log = format!("{name}.log");
            let mut st = Store::open(&dir, cfg).unwrap();
            st.create(&log).unwrap();
            crate::bark::ensure_identified(&dir, &log).unwrap();
            let mut bark = crate::bark::load(&dir, &log).unwrap_or_default();
            bark.insert("service".into(), Value::String("apache".into()));
            crate::bark::save(&dir, &log, &bark).unwrap();
            let f = st.files.get_mut(&log).unwrap();
            for i in 0..*lines {
                let line = format!("2026-09-01T10:00:{:02}Z INFO {name} line {i}\n", i % 60);
                f.append_stamped(line.as_bytes(), 1_000_000 + i as u64, &cfg)
                    .unwrap();
                f.flush_chunk(&cfg).unwrap();
            }
            // The index mirrors the manifest's identity at open.
            drop(st);
            Store::open(&dir, cfg).unwrap();
        }
        root
    }

    /// A consumer written in `sh`, which is the claim this protocol
    /// exists to make good on: three printfs and a read loop.
    ///
    /// `ack` false makes it say hello and nothing else — a consumer that
    /// never reports, whose positions must therefore never move.
    fn shell_consumer(root: &Path, out: &Path, ack: bool) -> Vec<String> {
        let ack = if ack {
            r#"printf '\036progress\037id=%s\037offset=%s\000' "$id" "$((off + len))""#
        } else {
            ":"
        };
        let script = format!(
            r#"printf '\036hello\037v=1\037reads=records\000'
while IFS= read -r -d '' hdr; do
  kind=${{hdr%%$'\037'*}}; kind=${{kind#$'\036'}}
  [ "$kind" = entry ] || continue
  f() {{ printf '%s' "$hdr" | tr '\037' '\n' | sed -n "s/^$1=//p"; }}
  len=$(f len); off=$(f offset); id=$(f id)
  payload=$(head -c "$len"; head -c 1 >/dev/null)
  printf '%s\n' "$payload" >> {out}
  {ack}
done"#,
            out = out.display(),
        );
        let _ = root;
        vec!["bash".into(), "-c".into(), script]
    }

    fn opts(root: &Path, positions: Option<&Path>, argv: Vec<String>) -> Opts {
        Opts {
            selector: crate::select::Selector::parse("[service=apache]").unwrap(),
            dirs: vec![root.to_path_buf()],
            positions: positions.map(Path::to_path_buf),
            batch_entries: 512,
            poll: Duration::from_millis(20),
            follow: false,
            argv,
            // A consumer that never speaks must not stall the suite;
            // 30s is right in production and wrong here.
            hello_wait: Duration::from_millis(500),
            // ⚠ Wider than the hello wait, which is short: these tests
            // feed SHELL consumers that fork several processes an entry,
            // and a ceiling tight enough to be a race is a test that
            // sometimes fails. It only has to exceed the gap BETWEEN
            // reports — the clock resets on each — not a whole run, so
            // two seconds is ~40x the gap rather than 200x the suite.
            // A test whose consumer stalls on PURPOSE sets its own,
            // since there is nothing there to wait for.
            max_silence: Duration::from_secs(2),
            // The tests are about the loop, so they take the store as
            // it stands rather than the default's date comparison.
            follow_from: crate::ship::FollowFrom::Begin,
            since: None,
        }
    }

    /// `opts` for a consumer that will never report: there is nothing to
    /// wait for, so the silence ceiling is paid in full by every such
    /// test and should be short.
    fn mute_opts(root: &Path, positions: Option<&Path>, argv: Vec<String>) -> Opts {
        Opts {
            max_silence: Duration::from_millis(200),
            ..opts(root, positions, argv)
        }
    }

    fn lines(p: &Path) -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The whole contract, against a consumer implemented in shell: it
    /// receives every entry of every matched store, its watermarks move
    /// the positions, and a second run sends nothing.
    #[test]
    fn a_shell_consumer_receives_the_selection_and_its_acks_move_the_positions() {
        let _forking = crate::store::fork_guard();
        let root = forest("shell", &[("web01", 2), ("web02", 3)]);
        let out = root.join("got.txt");
        let pos = root.join("positions.json");

        run(opts(&root, Some(&pos), shell_consumer(&root, &out, true))).unwrap();
        let mut got = lines(&out);
        got.sort();
        assert_eq!(got.len(), 5, "every entry of both stores: {got:?}");
        assert!(got.iter().filter(|l| l.contains("web01")).count() == 2);
        assert!(got.iter().filter(|l| l.contains("web02")).count() == 3);

        let held = crate::cursor::Positions::load(&pos).unwrap().unwrap();
        assert_eq!(held.at.len(), 2);
        for (id, at) in &held.at {
            assert!(at.offset > 0, "{id} did not advance");
            // Each line is its own sealed chunk here, so the retention
            // floor is the last one the consumer acknowledged.
            assert_eq!(
                at.chunk,
                Some(at.delivered - 1),
                "{id}: the floor is not the chunk it got to"
            );
        }

        // Nothing twice.
        run(opts(&root, Some(&pos), shell_consumer(&root, &out, true))).unwrap();
        assert_eq!(lines(&out).len(), 5, "a second run re-sent entries");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A position file is read by a person as well as by timberfs, so
    /// every field in it has to mean something. Two did not: `consumer`
    /// held the verb's name, identical for every feed on the host, and
    /// `wl` was always zero because this path never carried the write
    /// time an entry record states.
    #[test]
    fn a_position_file_says_what_wrote_it_and_when_it_got_there() {
        let _forking = crate::store::fork_guard();
        let root = forest("fields", &[("web01", 2)]);
        let out = root.join("got.txt");
        let pos = root.join("positions.json");
        run(opts(&root, Some(&pos), shell_consumer(&root, &out, true))).unwrap();

        let held = crate::cursor::Positions::load(&pos).unwrap().unwrap();
        assert_eq!(
            held.consumer, "bash",
            "the consumer's program, not the verb's name"
        );
        let at = held.at.values().next().expect("one store");
        assert!(at.wl > 0, "the write time an entry stated was thrown away");
        assert!(at.delivered > 0 && at.offset > 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ONE STORE IN TROUBLE MUST NOT COST THE OTHERS. A consumer that
    /// acknowledges S1 and never acknowledges S2 — its data refused, its
    /// destination wedged, whatever — must leave S1 shipping at full
    /// speed while S2 parks.
    ///
    /// ⚠ And the loop must not SPIN. Without a per-store window S2 is
    /// re-read from the same place on every poll, its entries fill the
    /// shared cap so the read always reports `limited`, and the loop
    /// never rests: measured before the window, one store in trouble
    /// pushed the same rejected entries at a full core indefinitely.
    #[test]
    fn a_store_that_is_never_acknowledged_parks_and_the_others_run() {
        let _forking = crate::store::fork_guard();
        let root = forest("stall", &[("aaa", 6), ("zzz", 6)]);
        let out = root.join("got.txt");
        let pos = root.join("positions.json");
        // Acknowledges everything except the store named zzz.
        let script = format!(
            r#"printf '\036hello\037v=1\037reads=records\000'
while IFS= read -r -d '' hdr; do
  kind=${{hdr%%$'\037'*}}; kind=${{kind#$'\036'}}
  [ "$kind" = entry ] || continue
  f() {{ printf '%s' "$hdr" | tr '\037' '
' | sed -n "s/^$1=//p"; }}
  len=$(f len); off=$(f offset); id=$(f id); src=$(f src)
  payload=$(head -c "$len"; head -c 1 >/dev/null)
  printf '%s
' "$payload" >> {out}
  case "$src" in *zzz*) ;; *) printf '\036progress\037id=%s\037offset=%s\000' "$id" "$((off + len))";; esac
done"#,
            out = out.display()
        );
        let mut o = opts(&root, Some(&pos), vec!["bash".into(), "-c".into(), script]);
        // A small cap, so a stalled store would crowd the other out.
        o.batch_entries = 4;
        run(o).unwrap();

        let got = lines(&out);
        assert_eq!(
            got.iter().filter(|l| l.contains("aaa")).count(),
            6,
            "the healthy store did not finish: {got:?}"
        );
        // The stalled one was tried, once, up to its window — and not
        // over and over.
        let tried = got.iter().filter(|l| l.contains("zzz")).count();
        assert!(tried > 0 && tried <= 8, "zzz was re-sent {tried} times");

        let held = crate::cursor::Positions::load(&pos).unwrap().unwrap();
        assert_eq!(
            held.at.len(),
            1,
            "only the acknowledged store has a position"
        );
        assert_eq!(held.at.values().next().unwrap().delivered, 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⚠ A consumer merely SLOWER than the poll interval used to end a
    /// one-shot: a store with anything unacknowledged is parked, so its
    /// remaining entries become available only when its report arrives,
    /// and a poll-interval timeout was taken for "nothing more is
    /// coming". Measured against the release binary before the fix: 20
    /// entries, a 20 ms poll and a consumer taking ~100 ms an entry
    /// delivered 4 and exited 0 — a fifth of the selection reported as a
    /// finished drain.
    ///
    /// ⚠ The slowness is MADE here, not hoped for. An earlier version of
    /// this test had no `sleep` and relied on a fork-heavy shell being
    /// incidentally slower than the poll; it passed here and failed on a
    /// CI runner at 6 of 8, for a reason that could not be reproduced.
    /// A test that does not control the condition it is named for cannot
    /// say which of the two it caught.
    ///
    /// And it asserts on what was ACKNOWLEDGED, not on what the consumer
    /// managed to write: the positions are the thing under test, and the
    /// consumer's own file lags them by however long its last write
    /// takes.
    #[test]
    fn a_consumer_slower_than_the_poll_interval_still_drains_the_selection() {
        let _forking = crate::store::fork_guard();
        let root = forest("slowcon", &[("aaa", 8)]);
        let pos = root.join("positions.json");
        // 50 ms an entry against a 1 ms poll, so the store is parked at
        // essentially every poll — and a 60 s ceiling, so silence can
        // never be what ends the run.
        let script = r#"printf '\036hello\037v=1\037reads=records\000'
while IFS= read -r -d '' hdr; do
  kind=${hdr%%$'\037'*}; kind=${kind#$'\036'}
  [ "$kind" = entry ] || continue
  f() { printf '%s' "$hdr" | tr '\037' '
' | sed -n "s/^$1=//p"; }
  len=$(f len); off=$(f offset); id=$(f id)
  head -c "$len" >/dev/null; head -c 1 >/dev/null
  sleep 0.05
  printf '\036progress\037id=%s\037offset=%s\000' "$id" "$((off + len))"
done"#;
        let mut o = opts(
            &root,
            Some(&pos),
            vec!["bash".into(), "-c".into(), script.into()],
        );
        o.batch_entries = 2;
        o.poll = Duration::from_millis(1);
        o.max_silence = Duration::from_secs(60);
        run(o).unwrap();
        let held = crate::cursor::Positions::load(&pos).unwrap().unwrap();
        let delivered: u64 = held.at.values().map(|a| a.delivered).sum();
        assert_eq!(
            delivered, 8,
            "the drain stopped early on a consumer that was only slow: {:?}",
            held.at
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// And silence that LASTS still ends a one-shot — otherwise a wedged
    /// consumer hangs it forever, which is what waiting on a report
    /// unconditionally would have cost. It acknowledges nothing, so a
    /// re-run sends those entries again.
    #[test]
    fn a_one_shot_gives_up_on_lasting_silence_rather_than_hanging() {
        let _forking = crate::store::fork_guard();
        let root = forest("wedged", &[("aaa", 4)]);
        let pos = root.join("positions.json");
        // Says hello, takes everything, and never reports again.
        let script = r#"printf '\036hello\037v=1\037reads=records\000'
while IFS= read -r -d '' hdr; do :; done"#;
        let mut o = opts(
            &root,
            Some(&pos),
            vec!["bash".into(), "-c".into(), script.into()],
        );
        o.max_silence = Duration::from_millis(300);
        let began = std::time::Instant::now();
        run(o).unwrap();
        // Generous on purpose: the failure this pins is HANGING, so the
        // bound only has to separate bounded from unbounded — never fast
        // from slow, which a loaded machine decides.
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "a wedged consumer was waited on indefinitely"
        );
        let held = crate::cursor::Positions::load(&pos).unwrap().unwrap();
        assert!(
            held.at.is_empty(),
            "a position moved for entries nothing acknowledged: {:?}",
            held.at
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A consumer that says hello and never reports gets the entries and
    /// moves nothing — silence is "has not got there", which is what
    /// makes mandatory conformance worth having.
    #[test]
    fn a_consumer_that_never_reports_never_advances() {
        let _forking = crate::store::fork_guard();
        let root = forest("mute", &[("web01", 2)]);
        let out = root.join("got.txt");
        let pos = root.join("positions.json");

        run(mute_opts(
            &root,
            Some(&pos),
            shell_consumer(&root, &out, false),
        ))
        .unwrap();
        assert_eq!(lines(&out).len(), 2, "it was still fed");
        let held = crate::cursor::Positions::load(&pos).unwrap().unwrap();
        assert!(
            held.at.is_empty(),
            "nothing was acknowledged, so nothing moved"
        );

        // So the same entries come again, which is the point.
        run(mute_opts(
            &root,
            Some(&pos),
            shell_consumer(&root, &out, false),
        ))
        .unwrap();
        assert_eq!(lines(&out).len(), 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// No hello, no run: a position must not move on the word of
    /// something that never said it speaks this protocol.
    #[test]
    fn a_consumer_that_says_no_hello_is_refused() {
        let _forking = crate::store::fork_guard();
        let root = forest("nohello", &[("web01", 1)]);
        let err = run(opts(
            &root,
            None,
            vec!["bash".into(), "-c".into(), "cat >/dev/null".into()],
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("hello"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A note is recorded and not merely logged, because `follower
    /// status` is another process — and it is recorded even though
    /// nothing advanced, which is exactly when it is needed.
    #[test]
    fn a_note_is_recorded_even_though_nothing_moved() {
        let _forking = crate::store::fork_guard();
        let root = forest("note", &[("web01", 1)]);
        let pos = root.join("positions.json");
        let script = r#"printf '\036hello\037v=1\037reads=records\000'
printf '\036note\037text="collector unreachable"\000'
cat >/dev/null"#;
        run(mute_opts(
            &root,
            Some(&pos),
            vec!["bash".into(), "-c".into(), script.into()],
        ))
        .unwrap();
        let held = crate::cursor::Positions::load(&pos).unwrap().unwrap();
        assert!(held.at.is_empty(), "nothing was acknowledged");
        let note = held.note.expect("the note outlived the process");
        assert_eq!(note.text, "collector unreachable");
        assert!(!note.when.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A consumer that dies is reported as the cause, with its exit
    /// status — not as the broken pipe that is its symptom, and not as a
    /// silent SIGPIPE death of this process.
    #[test]
    fn a_consumer_that_dies_is_reported_with_its_status() {
        let _forking = crate::store::fork_guard();
        let root = forest("dies", &[("web01", 50)]);
        let script = r#"printf '\036hello\037v=1\037reads=records\000'
exit 3"#;
        let err = run(opts(
            &root,
            None,
            vec!["bash".into(), "-c".into(), script.into()],
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("exited"), "{err}");
        assert!(err.contains('3'), "the consumer's own status: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A claimed watermark is honoured where nothing is recorded and
    /// ignored where something is: a number is not a proof, so it may
    /// save a re-ship but must never skip what we know was not sent.
    #[test]
    fn a_claim_seeds_an_unread_store_and_yields_to_a_recorded_position() {
        let _forking = crate::store::fork_guard();
        let root = forest("claim", &[("web01", 3)]);
        let out = root.join("got.txt");
        let pos = root.join("positions.json");
        let id = crate::bark::identity_of(&root.join("web01"), "web01.log").unwrap();

        // Claiming everything on a store with no position skips it.
        let script = format!(
            r#"printf '\036hello\037v=1\037reads=records\037holds={{"{id}":100000}}\000'
cat >/dev/null"#
        );
        run(opts(
            &root,
            Some(&pos),
            vec!["bash".into(), "-c".into(), script],
        ))
        .unwrap();
        assert_eq!(
            lines(&out).len(),
            0,
            "the claim was honoured, so nothing was sent"
        );
        assert_eq!(
            crate::cursor::Positions::load(&pos).unwrap().unwrap().at[&id].offset,
            100000
        );

        // And now that a position IS recorded, a claim behind it is ignored.
        let script = format!(
            r#"printf '\036hello\037v=1\037reads=records\037holds={{"{id}":0}}\000'
cat >/dev/null"#
        );
        run(opts(
            &root,
            Some(&pos),
            vec!["bash".into(), "-c".into(), script],
        ))
        .unwrap();
        assert_eq!(
            crate::cursor::Positions::load(&pos).unwrap().unwrap().at[&id].offset,
            100000,
            "a claim does not rewind a recorded position"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
