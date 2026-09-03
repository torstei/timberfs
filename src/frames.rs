//! The replication transport: a socket at each end of the frame codec.
//!
//! Blocking `TcpListener` plus a thread per connection, like the OTLP and
//! Forward intakes — no async runtime. One connection carries a
//! SELECTION: a stream per store the sender's predicate matches, opened
//! as the store appears and closed when it leaves. The stream id has
//! always been in every frame; what kept one store per connection was
//! per-stream flow control, and that objection assumed the streams want
//! to be independent — a sender of a selection is one destination and so
//! one queue by design. See docs/plans/frames-selection.md.
//!
//! No TLS: a private network, or a tunnel. Same rule as the other intakes.
//!
//! The handshake is what this adds over a pipe. A sender says what a
//! store is; the receiver answers `accepted` with what it already holds —
//! so the sender resumes from the receiver's own position instead of
//! guessing — or `conflict`, naming the tape that holds the identity.
//! Both are per STREAM, so one refused store leaves the other forty-nine
//! shipping.
//!
//! The destination is found by the store's IDENTITY, which travels: a
//! replica is not a derivative, it is the store in another place. Nothing
//! here routes by a label, and nothing derives a path from anything a
//! sender wrote.

use std::collections::HashMap;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};

use crate::frame::{self, Frame, Framed, Hello, Mode, Run};
use crate::receive::{Opening, Reader, ReceiveOpts, Received, Session};
use crate::select::Selector;

/// Where per-stream flow control will attach. Declared per STREAM because
/// that is the shape the PULL direction needs — a receiver asking for
/// what it lacks cannot decline what arrives — but nothing waits on it
/// yet: a sender ships what it has and TCP's own backpressure does the
/// work.
pub const WINDOW_BYTES: u64 = 8 << 20;

/// How many chunks one store may hand to the wire before its sender reads
/// the acks they produced.
///
/// ⚠ Not pacing: a bound is REQUIRED for correctness. The receiver acks
/// every chunk, so a sender that writes a whole backlog without reading
/// fills the receiver's write buffer, the receiver then blocks writing and
/// stops reading, and both ends wait on each other forever. It also bounds
/// the memory a pass holds, `serve` rendering its whole range into one
/// buffer. 256 acks is far inside any socket buffer, and a store with more
/// than that waiting simply takes another turn.
const CHUNKS_PER_TURN: u64 = 256;

/// How long a mid-pass drain waits for acks that have not arrived. Short
/// on purpose: it is emptying a buffer, not pacing the loop — `--poll` is
/// what paces it — and an ack still in flight is read on the next turn.
const DRAIN_WAIT: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub struct IntakeOpts {
    pub listen: String,
    /// Exit cleanly when this binary is replaced on disk, so a supervised
    /// run re-execs into the new one. The same contract as the other
    /// intakes: exit code 85, paired with `RestartForceExitStatus` in the
    /// unit.
    pub exit_on_upgrade: bool,
    pub into_dir: PathBuf,
    pub auto_create: bool,
    pub index: bool,
    pub wal: bool,
}

fn hello_bytes() -> Vec<u8> {
    frame::encode_hello(Hello {
        version: frame::VERSION,
        incompat: 0,
    })
    .to_vec()
}

fn send(out: &mut impl Write, stream: u32, f: Frame) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    frame::encode(&Framed { stream, frame: f }, &mut buf);
    out.write_all(&buf)?;
    out.flush()?;
    Ok(())
}

// ------------------------------------------------------------------ server

/// Listen and receive, one thread per connection.
pub fn cmd_intake(opts: &IntakeOpts) -> anyhow::Result<()> {
    // Socket activation when systemd handed us the listener, as the other
    // intakes do: the unit then owns the address and the port is bound
    // before the first sender can miss it.
    let listener = match crate::intake::socket_activated_listener() {
        Some(l) => l,
        None => TcpListener::bind(&opts.listen)
            .with_context(|| format!("binding frames-intake listener on {}", opts.listen))?,
    };
    let watch = if opts.exit_on_upgrade {
        crate::store::BinaryWatch::current()
    } else {
        None
    };
    crate::note!(
        "timberfs: frames intake listening on {} -> {} (by identity{})",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| opts.listen.clone()),
        opts.into_dir.display(),
        if opts.auto_create {
            ", creating unseen stores"
        } else {
            ""
        }
    );
    std::fs::create_dir_all(&opts.into_dir)
        .with_context(|| format!("creating {}", opts.into_dir.display()))?;
    for conn in listener.incoming() {
        // Checked between connections rather than mid-stream: a receive in
        // flight finishes, and the sender reconnects to the new binary.
        if watch.as_ref().is_some_and(|w| w.changed()) {
            crate::note!("timberfs: frames intake: binary replaced; exiting to re-exec");
            std::process::exit(crate::store::EXIT_BINARY_UPGRADED);
        }
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("timberfs: frames intake: accept failed: {e}");
                continue;
            }
        };
        let opts = opts.clone();
        thread::spawn(move || {
            if let Err(e) = serve_connection(stream, &opts) {
                eprintln!("timberfs: frames intake: {e:#}");
            }
        });
    }
    Ok(())
}

/// One connection: hello, then a stream per store — each opened by its own
/// handshake, fed chunks, and finished by `stream-close` or by the
/// connection ending.
///
/// A malformed frame ends the CONNECTION, since framing is the length
/// field and there is nothing to resynchronise to; a refused store ends
/// only its own stream.
pub fn serve_connection(sock: TcpStream, opts: &IntakeOpts) -> anyhow::Result<Vec<Received>> {
    let peer = sock
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".to_string());
    sock.set_nodelay(true).ok();
    let mut w = sock.try_clone().context("cloning the socket for writing")?;
    let mut r = Reader::new(sock);

    r.read_hello()?;
    w.write_all(&hello_bytes())?;
    w.flush()?;

    let mut open: HashMap<u32, Session> = HashMap::new();
    let mut done: Vec<Received> = Vec::new();
    while let Some(f) = r.next_frame()? {
        let id = f.stream;
        match f.frame {
            Frame::StreamOpen { .. } => {
                let opening = crate::receive::opening_of(f.frame)?;
                // A second open on a live stream id is the sender
                // reusing an id it has not closed, which would append
                // one store's chunks to another's.
                if open.contains_key(&id) {
                    bail!("{peer}: stream {id} was opened twice without being closed");
                }
                match open_stream(opts, &opening) {
                    Ok(session) => {
                        send(
                            &mut w,
                            id,
                            Frame::Accepted {
                                registration_id: registration_id(session.dir(), session.name()),
                                runs: session.coverage(),
                            },
                        )?;
                        open.insert(id, session);
                    }
                    Err(refusal) => {
                        crate::note!("timberfs: frames intake: {peer}: {}", refusal.reason);
                        send(
                            &mut w,
                            id,
                            Frame::Conflict {
                                holder_origin: refusal.holder,
                                runs: Vec::new(),
                                reason: refusal.reason,
                            },
                        )?;
                    }
                }
            }
            Frame::StreamClose => {
                if let Some(session) = open.remove(&id) {
                    done.push(finish(session, &peer)?);
                }
            }
            other => {
                let Some(session) = open.get_mut(&id) else {
                    // A refused stream's sender may still have frames in
                    // flight, and an unknown kind on an unknown stream is
                    // exactly what the length field exists to let us
                    // skip. Neither is a reason to drop the connection.
                    continue;
                };
                let was_chunk = matches!(other, Frame::Chunk { .. });
                session.apply(other)?;
                if was_chunk {
                    // Ack every chunk. A byte-window cadence starved a
                    // low-volume stream of acks entirely — the ack IS
                    // what advances a sender's position, and that
                    // position is what the retention interest axis
                    // reads, so a quiet store would never release its
                    // head. A coverage frame is ~28 bytes against a
                    // 25 KB chunk.
                    send(
                        &mut w,
                        id,
                        Frame::Coverage {
                            runs: session.coverage(),
                        },
                    )?;
                }
            }
        }
    }
    // A final ack per stream, so a sender that streamed less than a
    // window still learns where it got to.
    for (id, session) in open {
        send(
            &mut w,
            id,
            Frame::Coverage {
                runs: session.coverage(),
            },
        )?;
        done.push(finish(session, &peer)?);
    }
    Ok(done)
}

fn finish(session: Session, peer: &str) -> anyhow::Result<Received> {
    let got = session.finish()?;
    crate::note!(
        "timberfs: frames intake: {peer} -> {} ({} chunk(s), {})",
        got.store.display(),
        got.chunks,
        crate::rotate::human_bytes(got.comp_bytes)
    );
    Ok(got)
}

/// Why one stream was refused, and by whom — the useful half of the
/// handshake: the sender learns at setup time, on a terminal, rather than
/// finding a mislabelled store weeks later.
struct Refusal {
    reason: String,
    holder: [u8; 16],
}

impl Refusal {
    fn new(reason: String) -> Refusal {
        Refusal {
            reason,
            holder: [0u8; 16],
        }
    }
}

/// The destination for one stream, opened.
fn open_stream(opts: &IntakeOpts, opening: &Opening) -> Result<Session, Refusal> {
    let Some(id) = opening.store_id() else {
        return Err(Refusal::new(
            "the stream carries no store identity, and a replica is keyed by one — give the \
             source an identity with `timberfs identity <store> --mint`"
                .to_string(),
        ));
    };
    let dest = match find_destination(&opts.into_dir, &id) {
        Ok(Some(dest)) => dest,
        Ok(None) if opts.auto_create => opts.into_dir.join(&id).join(&id),
        Ok(None) => {
            // Same posture as the other intakes: an undeclared stream is
            // refused rather than created, and the refusal says what to
            // do. A destination cannot be pre-created by NAME here — the
            // key is the identity — so declaring one means writing it.
            return Err(Refusal::new(format!(
                "store {id} has not been received here before — run the intake with \
                 --auto-create, or declare a destination for it with \
                 `timberfs create <store> --set origin_id={id}`"
            )));
        }
        Err(reason) => return Err(Refusal::new(reason)),
    };

    // The chunking values matter less here than in an entry intake: every
    // frame arrives already compressed and bypasses the buffer, so this
    // config only governs a flush the receive path never triggers.
    let cfg = crate::store::Config {
        chunk_size: 256 * 1024,
        level: 3,
        flush_age_ms: 5000,
    };
    let ropts = ReceiveOpts {
        index: opts.index,
        wal: opts.wal,
    };
    Session::open(&dest, opening, &ropts, &cfg).map_err(|e| {
        let (dir, name) = crate::query::resolve_backing(&dest).unwrap_or_default();
        Refusal {
            reason: format!("{e:#}"),
            holder: crate::bark::load(&dir, &name)
                .and_then(|b| {
                    b.get("origin_id")
                        .and_then(|v| v.as_str())
                        .and_then(frame::uuid_bytes)
                })
                .unwrap_or([0u8; 16]),
        }
    })
}

/// Which store here holds the tape `id` names, if any.
///
/// `origin_id` first, because that is the member that SAYS so: a store
/// this receiver has already received declares it, an operator
/// pre-declaring a destination writes it, and a store received by a build
/// that minted its own identity wrote it too — so an upgrade keeps
/// receiving into the store it has rather than starting a second copy of
/// everything it holds.
///
/// `Err` when more than one store answers: nothing downstream can pick a
/// winner, and appending to the wrong one is the failure this whole key
/// exists to prevent.
fn find_destination(into_dir: &Path, id: &str) -> Result<Option<PathBuf>, String> {
    let dirs = [into_dir.to_path_buf()];
    for key in ["origin_id", "id"] {
        let Ok(sel) = Selector::parse(&format!("{key}={id}")) else {
            continue;
        };
        let mut found = crate::select::resolve(&dirs, &sel);
        match found.len() {
            0 => continue,
            1 => {
                let m = found.pop().expect("one match");
                return Ok(Some(m.dir.join(&m.name)));
            }
            n => {
                let names = found
                    .iter()
                    .map(|m| format!("  {}", m.dir.join(&m.name).display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(format!(
                    "{n} stores under {} declare {key}={id}, so there is no one store to \
                     receive into:\n{names}",
                    into_dir.display()
                ));
            }
        }
    }
    Ok(None)
}

/// The receiver's own durable handle for this store, which it controls.
/// For a store whose identity travelled it IS that identity; for one an
/// older build minted, it is what that build minted.
fn registration_id(dir: &Path, name: &str) -> [u8; 16] {
    crate::bark::load(dir, name)
        .and_then(|b| {
            b.get("id")
                .and_then(|v| v.as_str())
                .and_then(frame::uuid_bytes)
        })
        .unwrap_or([0u8; 16])
}

// ------------------------------------------------------------------ client

/// Which stores a send is about.
#[derive(Debug, Clone)]
pub enum Sources {
    /// One store, by path. Not resolved through a selector, so a store
    /// outside every forest still ships.
    One(PathBuf),
    /// Every store matching a predicate, re-resolved each pass so one
    /// that appears joins the connection and one that stops matching
    /// leaves it.
    Select {
        select: String,
        /// Directories to sweep BESIDE the configured forests — a place
        /// to LOOK, never an address, exactly as a follower's `look_in`.
        look_in: Vec<PathBuf>,
    },
}

impl Sources {
    /// What was asked for, for a message about what it found. A selection
    /// that matched nothing has to say which predicate and where it
    /// looked, or "no store to ship" names neither the mistake nor the
    /// place to make it.
    pub fn describe(&self) -> String {
        match self {
            Sources::One(p) => p.display().to_string(),
            Sources::Select { select, look_in } => {
                let mut dirs: Vec<String> = crate::forest::forest_dirs()
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect();
                dirs.extend(look_in.iter().map(|d| d.display().to_string()));
                if dirs.is_empty() {
                    format!("{select} (no forest is configured and no --look-in was given)")
                } else {
                    format!("{select} in {}", dirs.join(", "))
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SendOpts {
    pub endpoint: String,
    pub sidecars: bool,
    pub timeout: Duration,
    /// Keep shipping as chunks seal, on the same connection.
    pub follow: bool,
    /// How long to wait between polls of the selection in `--follow`.
    pub poll: Duration,
    /// Record the far end's acked position per store here, so a store
    /// declaring `cursors=<dir>` can REPORT what has left this box
    /// (`info`, `list`). Nothing else needs it: the RECEIVER's coverage
    /// is what a resume reads.
    pub positions: Option<PathBuf>,
}

impl SendOpts {
    pub fn to(endpoint: &str) -> SendOpts {
        SendOpts {
            endpoint: endpoint.to_string(),
            sidecars: true,
            timeout: Duration::from_secs(30),
            follow: false,
            poll: Duration::from_secs(1),
            positions: None,
        }
    }
}

/// What one store's stream did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSent {
    /// The store's identity — what the destination is keyed by.
    pub store: String,
    pub path: PathBuf,
    pub chunks: u64,
    pub comp_bytes: u64,
    /// Where the receiver stood when it accepted — the resume point, and
    /// why a sender need not remember one of its own.
    pub accepted_at: Vec<Run>,
    /// The receiver's coverage after the last ack it sent.
    pub acked: Vec<Run>,
    pub skipped_already_held: u64,
}

/// One store the far end would not take, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sent {
    pub streams: Vec<StreamSent>,
    pub refused: Vec<Refused>,
    /// Stores the selection matched that carry no identity, so nothing
    /// could key a destination on them. Named rather than skipped.
    pub unidentified: Vec<PathBuf>,
}

impl Sent {
    pub fn chunks(&self) -> u64 {
        self.streams.iter().map(|s| s.chunks).sum()
    }

    pub fn comp_bytes(&self) -> u64 {
        self.streams.iter().map(|s| s.comp_bytes).sum()
    }

    pub fn skipped_already_held(&self) -> u64 {
        self.streams.iter().map(|s| s.skipped_already_held).sum()
    }
}

/// One store's stream, while the connection is up.
struct Stream {
    wire: u32,
    store: String,
    path: PathBuf,
    /// The next chunk number to serve — advanced by what `serve` examined,
    /// not by how many chunks it sent: a chunk retention took away between
    /// the index read and the frame read is never coming, and resuming
    /// below it would ask for it on every pass forever.
    resume: u64,
    acked: Vec<Run>,
    accepted_at: Vec<Run>,
    /// The highest chunk handed to the wire, and where it sits. Recorded
    /// only into the position file, and only once the acknowledgement has
    /// caught up with it.
    last_sent: Option<crate::serve::LastSent>,
    chunks: u64,
    comp_bytes: u64,
    skipped: u64,
    /// Chunks delivered since the position file was last written —
    /// `Positions::advance` adds this, so it is a delta and not a total.
    undeclared: u64,
}

/// Say once, per store, that a pair carrying no identity cannot be
/// replicated — the destination is keyed by one. Named rather than
/// silently skipped, and a reader has no business minting an identity
/// into someone else's manifest.
fn note_unidentified(out: &mut Sent, found: Vec<PathBuf>) {
    for path in found {
        if out.unidentified.contains(&path) {
            continue;
        }
        crate::note!(
            "timberfs: {} carries no identity, so it cannot be replicated \
             (`timberfs identity {} --mint`)",
            path.display(),
            path.display()
        );
        out.unidentified.push(path);
    }
}

/// One store to ship: its identity, and where it is.
struct Source {
    id: String,
    path: PathBuf,
}

/// The stores this send is about, resolved now.
fn resolve_sources(src: &Sources) -> anyhow::Result<(Vec<Source>, Vec<PathBuf>)> {
    let mut out = Vec::new();
    let mut unidentified = Vec::new();
    match src {
        Sources::One(path) => {
            let path = crate::forest::resolve_source(path)?;
            let (dir, name) = crate::query::resolve_backing(&path)?;
            match crate::bark::identity_of(&dir, &name) {
                Some(id) => out.push(Source { id, path }),
                None => unidentified.push(path),
            }
        }
        Sources::Select { select, look_in } => {
            let sel = Selector::parse(select)?;
            let mut dirs = crate::forest::forest_dirs();
            for d in look_in {
                if !dirs.contains(d) {
                    dirs.push(d.clone());
                }
            }
            for m in crate::select::resolve(&dirs, &sel) {
                match m.id {
                    Some(id) => out.push(Source {
                        id,
                        path: m.dir.join(&m.name),
                    }),
                    None => unidentified.push(m.dir.join(&m.name)),
                }
            }
        }
    }
    Ok((out, unidentified))
}

/// Connect, then ship the selection: a stream per store, opened as the
/// store appears, resumed from wherever the receiver says it left off.
pub fn cmd_send(src: &Sources, opts: &SendOpts) -> anyhow::Result<Sent> {
    let mut out = Sent::default();
    // Resolved before connecting, so a one-shot that matches nothing says
    // so instead of failing on an endpoint it had no reason to reach. A
    // follow run connects anyway: its stores appear later, which is the
    // whole reason the selection is re-resolved each pass.
    let first = resolve_sources(src)?;
    if !opts.follow && first.0.is_empty() {
        note_unidentified(&mut out, first.1);
        return Ok(out);
    }
    let addr = opts.endpoint.clone();
    let sock = TcpStream::connect(&addr).with_context(|| format!("connecting to {addr}"))?;
    sock.set_nodelay(true).ok();
    sock.set_read_timeout(Some(opts.timeout)).ok();
    sock.set_write_timeout(Some(opts.timeout)).ok();
    let mut w = sock.try_clone().context("cloning the socket for writing")?;
    let mut r = Reader::new(sock);

    w.write_all(&hello_bytes())?;
    w.flush()?;
    r.read_hello()?;

    let mut live: Vec<Stream> = Vec::new();
    let mut next_wire = 0u32;
    // A store refused once is not asked again on this connection: the
    // answer will not change while the far end holds what it holds, and
    // re-opening every poll would be a refusal per second in the log.
    let mut refused: Vec<String> = Vec::new();

    loop {
        let (sources, unidentified) = resolve_sources(src)?;
        note_unidentified(&mut out, unidentified);

        // A store that has left the selection: end its stream, so the far
        // end can release the writer locks and the open store it holds.
        let held: Vec<String> = sources.iter().map(|s| s.id.clone()).collect();
        let mut closing = Vec::new();
        live.retain(|s| {
            if held.contains(&s.store) {
                true
            } else {
                closing.push(s.wire);
                false
            }
        });
        for wire in closing {
            send(&mut w, wire, Frame::StreamClose)?;
        }

        for s in &sources {
            if live.iter().any(|l| l.store == s.id) || refused.contains(&s.id) {
                continue;
            }
            match open_send_stream(&mut w, &mut r, next_wire, s, opts, &mut live)? {
                Some(reason) => {
                    refused.push(s.id.clone());
                    out.refused.push(Refused {
                        path: s.path.clone(),
                        reason,
                    });
                }
                None => next_wire += 1,
            }
        }

        // A turn each, and the acks read between them: one store's
        // backlog must not fill the far end's write buffer while nothing
        // is emptying it here.
        let mut shipped = 0u64;
        for i in 0..live.len() {
            shipped += ship_turn(&mut w, &mut live[i], opts)?;
            drain_acks(&mut r, &mut live, DRAIN_WAIT)?;
        }
        // Record whatever the far end has acknowledged. That is what a
        // retention interest axis reads to know what has left this box —
        // the RECEIVER's position, not our own idea of progress. Once per
        // pass rather than once per ack: writing per ack made the position
        // the most expensive thing in the loop, and neither of its readers
        // needs per-chunk precision.
        save_positions(opts.positions.as_deref(), &mut live)?;

        if !opts.follow {
            // A turn is bounded, so a backlog takes several — but a pass
            // that moved nothing has nothing left to move.
            if shipped == 0 {
                break;
            }
            continue;
        }
        if shipped == 0 {
            thread::sleep(opts.poll);
        }
    }

    // Done writing: now the far end will finish and send its last acks.
    w.shutdown(std::net::Shutdown::Write).ok();
    r.get_ref().set_read_timeout(Some(opts.timeout)).ok();
    while let Some(f) = r.next_frame()? {
        note_ack(&mut live, &f);
    }
    save_positions(opts.positions.as_deref(), &mut live)?;
    for s in live {
        out.streams.push(StreamSent {
            store: s.store,
            path: s.path,
            chunks: s.chunks,
            comp_bytes: s.comp_bytes,
            accepted_at: s.accepted_at,
            acked: s.acked,
            skipped_already_held: s.skipped,
        });
    }
    Ok(out)
}

/// Open one store's stream and wait for its answer. `Ok(Some(reason))` is
/// a refusal of that store alone; the connection and every other stream
/// carry on.
fn open_send_stream(
    w: &mut impl Write,
    r: &mut Reader<TcpStream>,
    wire: u32,
    src: &Source,
    opts: &SendOpts,
    live: &mut Vec<Stream>,
) -> anyhow::Result<Option<String>> {
    // The opening frame is the serve side's own, so a sender and a server
    // describe a stream identically. Sent in Coverage mode: it says what
    // this store is without committing to ship anything, which is what
    // makes the handshake a question rather than an assertion.
    let mut probe = Vec::new();
    crate::serve::serve(
        &src.path,
        &crate::serve::Request {
            stream: wire,
            mode: Mode::Coverage,
            first_seq: 0,
            last_seq: frame::OPEN_ENDED,
            sidecars: opts.sidecars,
            max_chunks: None,
        },
        &mut probe,
    )?;
    // Only the stream-open half: the coverage frame after it is ours, and
    // the receiver answers with its own.
    let (_, open_len) = frame::decode(&probe)?.expect("serve wrote a whole frame");
    w.write_all(&probe[..open_len])?;
    w.flush()?;

    // An answer is worth the full timeout even in follow mode, where the
    // read side is otherwise tuned to the poll interval.
    let restore = opts.follow.then_some(opts.poll);
    r.get_ref().set_read_timeout(Some(opts.timeout)).ok();
    let answer = loop {
        let Some(f) = r.next_frame()? else {
            bail!(
                "{} closed the connection without answering the handshake for {}",
                opts.endpoint,
                src.path.display()
            );
        };
        match f.frame {
            Frame::Accepted { runs, .. } if f.stream == wire => break Ok(runs),
            Frame::Conflict {
                holder_origin,
                reason,
                ..
            } if f.stream == wire => {
                break Err(format!(
                    "{reason}{}",
                    if holder_origin == [0u8; 16] {
                        String::new()
                    } else {
                        format!(" (held by origin {})", frame::uuid_string(&holder_origin))
                    }
                ))
            }
            // An ack for a stream already shipping, arriving between our
            // open and its answer.
            _ => {
                note_ack(live, &f);
            }
        }
    };
    if let Some(poll) = restore {
        r.get_ref().set_read_timeout(Some(poll)).ok();
    }
    let accepted_at = match answer {
        Ok(runs) => runs,
        Err(reason) => return Ok(Some(reason)),
    };

    // Resume from the RECEIVER's position: it is authoritative, so a
    // sender keeps no position of its own and cannot re-ship.
    let resume = accepted_at.iter().map(|r| r.end + 1).max().unwrap_or(0);
    live.push(Stream {
        wire,
        store: src.id.clone(),
        path: src.path.clone(),
        resume,
        acked: Vec::new(),
        accepted_at,
        last_sent: None,
        chunks: 0,
        comp_bytes: 0,
        skipped: resume,
        undeclared: 0,
    });
    Ok(None)
}

/// Serve one store's turn: from where its stream stands, up to
/// `CHUNKS_PER_TURN`. Returns how many chunks went on the wire.
fn ship_turn(w: &mut impl Write, s: &mut Stream, opts: &SendOpts) -> anyhow::Result<u64> {
    let mut body = Vec::new();
    let served = crate::serve::serve(
        &s.path,
        &crate::serve::Request {
            stream: s.wire,
            mode: Mode::Frames,
            first_seq: s.resume,
            last_seq: frame::OPEN_ENDED,
            sidecars: opts.sidecars,
            max_chunks: Some(CHUNKS_PER_TURN),
        },
        &mut body,
    )?;
    if served.chunks == 0 {
        // Even with nothing to send, a chunk retention took away is
        // examined, and the position must move past it.
        if let Some(seq) = served.last_examined {
            s.resume = s.resume.max(seq + 1);
        }
        return Ok(0);
    }
    // Skip serve's own stream-open: the handshake already opened this
    // stream, and a second open would be a second stream.
    let (_, skip) = frame::decode(&body)?.expect("serve wrote a whole frame");
    w.write_all(&body[skip..])?;
    w.flush()?;
    s.chunks += served.chunks;
    s.comp_bytes += served.comp_bytes;
    s.undeclared += served.chunks;
    if let Some(seq) = served.last_examined {
        s.resume = s.resume.max(seq + 1);
    }
    if let Some(sent) = served.last_sent {
        s.last_sent = Some(sent);
    }
    Ok(served.chunks)
}

/// Take every ack the far end has sent, without blocking longer than one
/// poll. True when at least one arrived.
fn drain_acks(
    r: &mut Reader<TcpStream>,
    live: &mut [Stream],
    poll: Duration,
) -> anyhow::Result<bool> {
    r.get_ref().set_read_timeout(Some(poll)).ok();
    let mut any = false;
    loop {
        match r.next_frame() {
            Ok(Some(f)) => {
                any |= note_ack(live, &f);
            }
            Ok(None) => return Ok(any),
            Err(e) => {
                // A read timeout IS "nothing yet" in a poll loop. Matched
                // on the error kind rather than its text: the same
                // condition surfaces as WouldBlock on Linux and TimedOut
                // elsewhere, and its message ("Resource temporarily
                // unavailable") says neither.
                let timed_out = e.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    )
                });
                if timed_out {
                    return Ok(any);
                }
                return Err(e);
            }
        }
    }
}

/// File one coverage frame against the stream it belongs to.
fn note_ack(live: &mut [Stream], f: &Framed) -> bool {
    let Frame::Coverage { runs } = &f.frame else {
        return false;
    };
    let Some(s) = live.iter_mut().find(|s| s.wire == f.stream) else {
        return false;
    };
    s.acked = runs.clone();
    true
}

/// Record what the FAR END holds, per store, in the registry's positions
/// format — so the reporting columns and, once frames join the consumer
/// protocol, the retention interest axis can read it.
///
/// ⚠ A CACHE, not a cursor. Nothing reads it to decide where to start: a
/// resume comes from the receiver's own coverage, so losing this file
/// costs nothing but conservative retention until the first ack.
///
/// The offset and the chunk move TOGETHER and only when the
/// acknowledgement has caught up with what was sent — a chunk boundary is
/// an offset, so for this sender the resume point and the retention floor
/// are one fact, and taking them from two different chunks is the only way
/// to make them disagree. Behind that, the previous entry stands rather
/// than being overstated by a newer chunk's position.
///
/// `chunk` is the last chunk the receiver acknowledged, not the next one
/// wanted: the interest floor treats a position at or past `next_seq` as a
/// hand-edit that pins the whole store, so a caught-up sender stays one
/// below. That over-retains by exactly one chunk — the harmless direction,
/// and interest is additive anyway.
fn save_positions(path: Option<&Path>, live: &mut [Stream]) -> anyhow::Result<bool> {
    let Some(path) = path else {
        return Ok(false);
    };
    let mut held = crate::cursor::Positions::load(path)
        .unwrap_or(None)
        .unwrap_or_else(|| crate::cursor::Positions::new("frames-send"));
    let mut changed = false;
    for s in live.iter_mut() {
        let (Some(last), Some(sent)) = (s.acked.iter().map(|r| r.end).max(), s.last_sent) else {
            continue; // nothing acknowledged yet: no position to record
        };
        if sent.seq != last {
            continue; // the ack is behind what was sent; leave it alone
        }
        if s.undeclared == 0 && held.at.get(&s.store).is_some_and(|a| a.chunk == Some(last)) {
            continue; // unchanged: no write, so a quiet loop is quiet
        }
        held.advance(
            &s.store,
            &s.path.display().to_string(),
            sent.offset,
            Some(last),
            sent.last_write_ms,
            s.undeclared,
        );
        s.undeclared = 0;
        changed = true;
    }
    if !changed {
        return Ok(false);
    }
    held.save(path)?;
    Ok(true)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("timberfs-frames-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A source store with `chunks` chunks, labelled.
    fn a_store(dir: &Path, name: &str, chunks: usize, service: &str) -> PathBuf {
        let path = dir.join(format!("{name}.log"));
        crate::bark::cmd_create(&path, false, false, None, None, false, &[], false).unwrap();
        crate::bark::cmd_set(
            &path,
            &["host=apache01".into(), format!("service={service}")],
            &[],
        )
        .unwrap();
        let cfg = crate::store::Config {
            chunk_size: 1 << 20,
            level: 1,
            flush_age_ms: u64::MAX,
        };
        let logical = format!("{name}.log");
        let mut st = crate::store::Store {
            dir: dir.to_path_buf(),
            cfg,
            files: std::collections::BTreeMap::new(),
        };
        st.create(&logical).unwrap();
        let f = st.files.get_mut(&logical).unwrap();
        for i in 0..chunks {
            f.append_windowed(
                format!("2026-06-01T10:00:0{i}Z line {i} padding\n").as_bytes(),
                1_000 + i as u64,
                1_000 + i as u64,
                &cfg,
            )
            .unwrap();
            f.flush_chunk(&cfg).unwrap();
        }
        path
    }

    fn id_of(store: &Path) -> String {
        let (dir, name) = crate::query::resolve_backing(store).unwrap();
        crate::bark::identity_of(&dir, &name).unwrap()
    }

    /// Where a stream for `store` lands under `into`: its identity, twice.
    fn landed(into: &Path, store: &Path) -> PathBuf {
        let id = id_of(store);
        into.join(&id).join(&id)
    }

    fn opts(into: &Path, auto: bool) -> IntakeOpts {
        IntakeOpts {
            listen: String::new(),
            into_dir: into.to_path_buf(),
            auto_create: auto,
            index: false,
            wal: false,
            exit_on_upgrade: false,
        }
    }

    /// Serve exactly one connection on an ephemeral port, in a thread.
    fn one_shot(o: IntakeOpts) -> (String, thread::JoinHandle<anyhow::Result<Vec<Received>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let h = thread::spawn(move || {
            let (sock, _) = l.accept()?;
            serve_connection(sock, &o)
        });
        (addr, h)
    }

    fn send_opts(addr: &str) -> SendOpts {
        SendOpts {
            timeout: Duration::from_secs(10),
            ..SendOpts::to(addr)
        }
    }

    fn one(store: &Path) -> Sources {
        Sources::One(store.to_path_buf())
    }

    #[test]
    fn a_store_crosses_a_socket_and_arrives_byte_identical() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 5, "apache-error");
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, true));

        let sent = cmd_send(&one(&src), &send_opts(&addr)).unwrap();
        let got = server.join().unwrap().unwrap();

        assert_eq!(sent.chunks(), 5);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].chunks, 5);
        assert_eq!(
            sent.streams[0].accepted_at,
            vec![],
            "a fresh destination holds nothing"
        );
        assert_eq!(
            sent.streams[0].acked,
            vec![Run { start: 0, end: 4 }],
            "acked to the end"
        );

        // Keyed by identity, and byte-identical on arrival.
        let dst = landed(&into, &src);
        let (sd, sn) = crate::query::resolve_backing(&src).unwrap();
        let (dd, dn) = crate::query::resolve_backing(&dst).unwrap();
        assert_eq!(
            std::fs::read(crate::format::trunk_path(&sd, &sn)).unwrap(),
            std::fs::read(crate::format::trunk_path(&dd, &dn)).unwrap(),
        );
        // The name travelled, so the replica is not just a uuid — and it
        // is the EFFECTIVE name, which a store that never declared one
        // has only in its path.
        let db = crate::bark::load(&dd, &dn).unwrap();
        assert_eq!(db.get("service").unwrap(), "apache-error");
        assert_eq!(db.get("id").unwrap().as_str().unwrap(), id_of(&src));
        assert_eq!(db.get("name").unwrap(), "src");
    }

    /// The whole point of the mux: one socket, one process, N stores —
    /// and each one its own handshake, so a store that is refused takes
    /// nothing else down with it.
    #[test]
    fn one_connection_carries_a_selection() {
        let d = TempDir::new();
        let forest = d.path().join("node");
        std::fs::create_dir_all(&forest).unwrap();
        let a = a_store(&forest, "apache-error", 3, "apache-error");
        let b = a_store(&forest, "apache-access", 2, "apache-access");
        let other = a_store(&forest, "postgres", 4, "postgres");
        let into = d.path().join("archive");
        let (addr, server) = one_shot(opts(&into, true));

        let sent = cmd_send(
            &Sources::Select {
                select: "[service=~apache-.*]".to_string(),
                look_in: vec![forest.clone()],
            },
            &send_opts(&addr),
        )
        .unwrap();
        let got = server.join().unwrap().unwrap();

        assert_eq!(sent.streams.len(), 2, "{sent:?}");
        assert_eq!(sent.chunks(), 5);
        assert_eq!(got.len(), 2);
        for s in [&a, &b] {
            let dst = landed(&into, s);
            let (sd, sn) = crate::query::resolve_backing(s).unwrap();
            let (dd, dn) = crate::query::resolve_backing(&dst).unwrap();
            assert_eq!(
                std::fs::read(crate::format::trunk_path(&sd, &sn)).unwrap(),
                std::fs::read(crate::format::trunk_path(&dd, &dn)).unwrap(),
                "{}",
                s.display()
            );
        }
        assert!(
            !landed(&into, &other).parent().unwrap().exists(),
            "the predicate did not select it"
        );
    }

    /// Two hosts whose stores share a name, service and hostname — the
    /// collision routing by label could not be made to survive, and which
    /// keying on identity does not have.
    #[test]
    fn two_hosts_with_the_same_labels_do_not_merge() {
        let d = TempDir::new();
        let one_host = d.path().join("apache01");
        let two_host = d.path().join("apache02");
        std::fs::create_dir_all(&one_host).unwrap();
        std::fs::create_dir_all(&two_host).unwrap();
        let a = a_store(&one_host, "apache-error", 3, "apache-error");
        let b = a_store(&two_host, "apache-error", 2, "apache-error");
        let into = d.path().join("archive");

        for src in [&a, &b] {
            let (addr, server) = one_shot(opts(&into, true));
            cmd_send(&one(src), &send_opts(&addr)).unwrap();
            server.join().unwrap().unwrap();
        }

        assert_ne!(id_of(&a), id_of(&b));
        for (src, chunks) in [(&a, 3usize), (&b, 2usize)] {
            let dst = landed(&into, src);
            let (dd, dn) = crate::query::resolve_backing(&dst).unwrap();
            let recs = crate::format::read_index(&crate::format::rings_path(&dd, &dn)).unwrap();
            assert_eq!(recs.len(), chunks, "{}", dst.display());
        }
    }

    #[test]
    fn the_receiver_says_where_it_left_off_and_the_sender_resumes_there() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 3, "apache-error");
        let into = d.path().join("recv");

        let (addr, server) = one_shot(opts(&into, true));
        cmd_send(&one(&src), &send_opts(&addr)).unwrap();
        server.join().unwrap().unwrap();

        // The source grows.
        let cfg = crate::store::Config {
            chunk_size: 1 << 20,
            level: 1,
            flush_age_ms: u64::MAX,
        };
        let mut st = crate::store::Store {
            dir: d.path().to_path_buf(),
            cfg,
            files: std::collections::BTreeMap::new(),
        };
        st.create("src.log").unwrap();
        let f = st.files.get_mut("src.log").unwrap();
        for i in 3..5u64 {
            f.append_windowed(
                format!("2026-06-01T10:00:0{i}Z line {i} more\n").as_bytes(),
                1_000 + i,
                1_000 + i,
                &cfg,
            )
            .unwrap();
            f.flush_chunk(&cfg).unwrap();
        }
        drop(st);

        let (addr, server) = one_shot(opts(&into, true));
        let sent = cmd_send(&one(&src), &send_opts(&addr)).unwrap();
        let got = server.join().unwrap().unwrap();
        assert_eq!(
            sent.streams[0].accepted_at,
            vec![Run { start: 0, end: 2 }],
            "the receiver's own coverage is the resume point"
        );
        assert_eq!(sent.chunks(), 2, "only what it lacked");
        assert_eq!(sent.streams[0].skipped_already_held, 3);
        assert_eq!(got[0].runs, vec![Run { start: 0, end: 4 }]);
    }

    #[test]
    fn nothing_new_to_send_is_a_successful_no_op() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 2, "apache-error");
        let into = d.path().join("recv");
        for pass in 0..2 {
            let (addr, server) = one_shot(opts(&into, true));
            let sent = cmd_send(&one(&src), &send_opts(&addr)).unwrap();
            server.join().unwrap().unwrap();
            assert_eq!(sent.chunks(), if pass == 0 { 2 } else { 0 });
        }
    }

    /// A store whose head has been dropped starts mid-tape, so a pass that
    /// advanced by CHUNKS SENT rather than by what it examined would
    /// re-serve the whole store on the next pass, forever.
    #[test]
    fn a_pass_resumes_past_what_it_examined_not_past_what_it_sent() {
        let d = TempDir::new();
        let src = d.path().join("src.log");
        crate::bark::cmd_create(&src, false, false, Some("1h"), None, false, &[], false).unwrap();
        let cfg = crate::store::Config {
            chunk_size: 1 << 20,
            level: 1,
            flush_age_ms: u64::MAX,
        };
        let mut st = crate::store::Store {
            dir: d.path().to_path_buf(),
            cfg,
            files: std::collections::BTreeMap::new(),
        };
        st.create("src.log").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let f = st.files.get_mut("src.log").unwrap();
        for i in 0..6u64 {
            // Three chunks from 1970 and three from now, so an age policy
            // drops exactly the head.
            let ms = if i < 3 { 1_000 + i } else { now };
            f.append_windowed(
                format!("2026-06-01T10:00:0{i}Z line {i} padding\n").as_bytes(),
                ms,
                ms,
                &cfg,
            )
            .unwrap();
            f.flush_chunk(&cfg).unwrap();
        }
        drop(st);
        crate::rotate::cmd_trim(&src, false).unwrap();
        let (sd, sn) = crate::query::resolve_backing(&src).unwrap();
        let recs = crate::format::read_index(&crate::format::rings_path(&sd, &sn)).unwrap();
        assert_eq!(recs.first().unwrap().seq, 3, "the tape now starts at 3");

        let mut body = Vec::new();
        let served = crate::serve::serve(
            &src,
            &crate::serve::Request {
                stream: 0,
                mode: Mode::Frames,
                first_seq: 0,
                last_seq: frame::OPEN_ENDED,
                sidecars: false,
                max_chunks: None,
            },
            &mut body,
        )
        .unwrap();
        assert_eq!(served.chunks, 3);
        assert_eq!(
            served.last_examined,
            Some(5),
            "the next pass starts at 6, not at 3"
        );
    }

    /// A turn is bounded so the acks it provokes cannot fill the far end's
    /// write buffer while nothing here is emptying it — but a backlog
    /// larger than one turn must still arrive whole, in one send.
    #[test]
    fn a_backlog_larger_than_one_turn_still_ships_completely() {
        let d = TempDir::new();
        let n = (CHUNKS_PER_TURN + 7) as usize;
        let src = a_store(d.path(), "src", n, "apache-error");
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, true));
        let sent = cmd_send(&one(&src), &send_opts(&addr)).unwrap();
        let got = server.join().unwrap().unwrap();
        assert_eq!(sent.chunks(), n as u64);
        assert_eq!(got[0].chunks, n as u64);
        assert_eq!(
            got[0].runs,
            vec![Run {
                start: 0,
                end: n as u64 - 1
            }]
        );
    }

    /// A stream whose store is not in the selection any more is closed, so
    /// the far end releases its writer locks instead of holding one per
    /// store the connection has ever seen.
    #[test]
    fn a_store_leaving_the_selection_closes_its_stream() {
        let d = TempDir::new();
        let into = d.path().join("recv");
        let src = a_store(d.path(), "src", 2, "apache-error");
        let (addr, server) = one_shot(opts(&into, true));

        // Drive the wire by hand: open, ship, close, then a second stream
        // on a fresh id — which is what a pass does when the selection
        // changes underneath it.
        let sock = TcpStream::connect(&addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut w = sock.try_clone().unwrap();
        let mut r = Reader::new(sock);
        w.write_all(&hello_bytes()).unwrap();
        w.flush().unwrap();
        r.read_hello().unwrap();
        let mut body = Vec::new();
        crate::serve::serve(
            &src,
            &crate::serve::Request::everything(Mode::Frames),
            &mut body,
        )
        .unwrap();
        w.write_all(&body).unwrap();
        w.flush().unwrap();
        send(&mut w, 0, Frame::StreamClose).unwrap();
        w.shutdown(std::net::Shutdown::Write).ok();
        while r.next_frame().unwrap().is_some() {}
        let got = server.join().unwrap().unwrap();
        assert_eq!(got.len(), 1, "the close finished the session");
        assert_eq!(got[0].chunks, 2);
    }

    #[test]
    fn a_second_origin_is_refused_with_a_reason_the_sender_can_read() {
        // Two stores, one destination — which identity keying makes take
        // a deliberate act to arrange: the second store's directory is
        // handed the first's origin.
        let d = TempDir::new();
        let a = a_store(d.path(), "a", 2, "apache-error");
        let b = a_store(d.path(), "b", 2, "apache-access");
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, true));
        cmd_send(&one(&a), &send_opts(&addr)).unwrap();
        server.join().unwrap().unwrap();

        // Point b's would-be destination at a's tape.
        let hijack = into.join(id_of(&b));
        std::fs::create_dir_all(&hijack).unwrap();
        let mut map = serde_json::Map::new();
        map.insert("id".into(), serde_json::json!(id_of(&b)));
        map.insert("origin_id".into(), serde_json::json!(id_of(&a)));
        crate::bark::save(&hijack, &id_of(&b), &map).unwrap();

        let (addr, server) = one_shot(opts(&into, true));
        let sent = cmd_send(&one(&b), &send_opts(&addr)).unwrap();
        server.join().unwrap().unwrap();
        assert!(sent.streams.is_empty(), "{sent:?}");
        assert_eq!(sent.refused.len(), 1);
        assert!(
            sent.refused[0].reason.contains("one store"),
            "{:?}",
            sent.refused
        );
    }

    /// One store's conflict must not stop the others: the handshake is per
    /// stream, so the connection carries on.
    #[test]
    fn a_refused_store_leaves_the_rest_shipping() {
        let d = TempDir::new();
        let forest = d.path().join("node");
        std::fs::create_dir_all(&forest).unwrap();
        let a = a_store(&forest, "apache-error", 3, "apache-error");
        let b = a_store(&forest, "apache-access", 2, "apache-access");
        let into = d.path().join("archive");
        // a's destination declares somebody else's origin.
        let hijack = into.join(id_of(&a));
        std::fs::create_dir_all(&hijack).unwrap();
        let mut map = serde_json::Map::new();
        map.insert("id".into(), serde_json::json!(id_of(&a)));
        map.insert("origin_id".into(), serde_json::json!(id_of(&b)));
        crate::bark::save(&hijack, &id_of(&a), &map).unwrap();

        let (addr, server) = one_shot(opts(&into, true));
        let sent = cmd_send(
            &Sources::Select {
                select: "[service=~apache-.*]".to_string(),
                look_in: vec![forest.clone()],
            },
            &send_opts(&addr),
        )
        .unwrap();
        server.join().unwrap().unwrap();
        assert_eq!(sent.refused.len(), 1, "{sent:?}");
        assert_eq!(sent.streams.len(), 1, "{sent:?}");
        assert_eq!(sent.chunks(), 2, "the other store shipped");
    }

    #[test]
    fn an_unreceived_store_is_refused_when_auto_create_is_off() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 2, "apache-error");
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, false));
        let sent = cmd_send(&one(&src), &send_opts(&addr)).unwrap();
        let _ = server.join().unwrap();
        assert_eq!(sent.refused.len(), 1, "{sent:?}");
        let reason = &sent.refused[0].reason;
        assert!(reason.contains("--auto-create"), "{reason}");
        assert!(
            reason.contains("origin_id="),
            "it names the other fix: {reason}"
        );
    }

    /// An archive that received before the identity travelled keeps its own
    /// minted id and its directory, and keeps receiving — or an upgrade
    /// would start a second copy of every store it holds.
    #[test]
    fn a_store_received_by_an_older_build_is_still_the_destination() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 4, "apache-error");
        let into = d.path().join("recv");

        // What the older build wrote: a routed directory, its own id, and
        // the origin recorded beside it.
        let legacy = into.join("apache-error.log");
        std::fs::create_dir_all(&legacy).unwrap();
        crate::bark::cmd_create(
            &legacy.join("apache-error.log"),
            false,
            false,
            None,
            None,
            false,
            &[
                format!("origin_id={}", id_of(&src)),
                "service=apache-error".to_string(),
            ],
            false,
        )
        .unwrap();
        let minted = crate::bark::load(&legacy, "apache-error.log")
            .unwrap()
            .get("id")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(minted, id_of(&src));

        let (addr, server) = one_shot(opts(&into, true));
        let sent = cmd_send(&one(&src), &send_opts(&addr)).unwrap();
        let got = server.join().unwrap().unwrap();
        assert_eq!(sent.chunks(), 4);
        assert_eq!(got[0].store, legacy.join("apache-error.log"));
        assert!(
            !into.join(id_of(&src)).exists(),
            "no second copy was started"
        );
        assert_eq!(
            crate::bark::load(&legacy, "apache-error.log")
                .unwrap()
                .get("id")
                .unwrap(),
            minted.as_str(),
            "it keeps the identity it was given"
        );
    }

    /// A destination declared before any data arrives, which is how a
    /// receiver's retention policy is settled up front now that a store
    /// cannot be pre-created by NAME.
    #[test]
    fn a_pre_declared_destination_is_found_by_the_origin_it_names() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 3, "apache-error");
        let into = d.path().join("recv");
        let declared = into.join("slot");
        std::fs::create_dir_all(&declared).unwrap();
        crate::bark::cmd_create(
            &declared.join("slot.log"),
            false,
            false,
            None,
            Some("5G"),
            false,
            &[format!("origin_id={}", id_of(&src))],
            false,
        )
        .unwrap();

        let (addr, server) = one_shot(opts(&into, false));
        let sent = cmd_send(&one(&src), &send_opts(&addr)).unwrap();
        let got = server.join().unwrap().unwrap();
        assert!(sent.refused.is_empty(), "{sent:?}");
        assert_eq!(sent.chunks(), 3);
        assert_eq!(got[0].store, declared.join("slot.log"));
        // The operator's declaration is untouched, which is the promise
        // every intake makes about a store somebody else made.
        let db = crate::bark::load(&declared, "slot.log").unwrap();
        assert_eq!(db.get("retain_size").unwrap(), "5G");
        assert!(!db.contains_key("service"), "{db:?}");
    }

    #[test]
    fn the_positions_record_what_the_far_end_holds() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 3, "apache-error");
        let into = d.path().join("recv");
        let pos = d.path().join("positions.json");
        let (addr, server) = one_shot(opts(&into, true));
        cmd_send(
            &one(&src),
            &SendOpts {
                positions: Some(pos.clone()),
                ..send_opts(&addr)
            },
        )
        .unwrap();
        server.join().unwrap().unwrap();

        let held = crate::cursor::Positions::load(&pos).unwrap().unwrap();
        assert_eq!(held.consumer, "frames-send");
        let at = held.at.get(&id_of(&src)).expect("keyed by identity");
        // The last chunk the receiver ACKNOWLEDGED, not the next one
        // wanted: the interest floor reads a position at or past next_seq
        // as a hand-edit pinning the whole store.
        assert_eq!(at.chunk, Some(2));
        assert_eq!(at.delivered, 3);
        assert!(at.wl > 0, "the write time comes from the frame: {at:?}");
        // A chunk boundary is an offset, and it is the same one at both
        // ends of a replica.
        let (sd, sn) = crate::query::resolve_backing(&src).unwrap();
        let recs = crate::format::read_index(&crate::format::rings_path(&sd, &sn)).unwrap();
        assert_eq!(at.offset, recs[2].uncomp_start);
    }

    #[test]
    fn an_unchanged_position_is_not_rewritten() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 2, "apache-error");
        let into = d.path().join("recv");
        let pos = d.path().join("positions.json");
        for _ in 0..2 {
            let (addr, server) = one_shot(opts(&into, true));
            cmd_send(
                &one(&src),
                &SendOpts {
                    positions: Some(pos.clone()),
                    ..send_opts(&addr)
                },
            )
            .unwrap();
            server.join().unwrap().unwrap();
        }
        let held = crate::cursor::Positions::load(&pos).unwrap().unwrap();
        assert_eq!(
            held.at.get(&id_of(&src)).unwrap().delivered,
            2,
            "a second send that shipped nothing must not count again"
        );
    }

    #[test]
    fn nothing_acked_writes_no_position_rather_than_a_false_one() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 0, "apache-error");
        let into = d.path().join("recv");
        let pos = d.path().join("positions.json");
        let (addr, server) = one_shot(opts(&into, true));
        cmd_send(
            &one(&src),
            &SendOpts {
                positions: Some(pos.clone()),
                ..send_opts(&addr)
            },
        )
        .unwrap();
        let _ = server.join().unwrap();
        assert!(!pos.exists(), "no acknowledgement, no position");
    }

    /// A store with no identity on either side cannot be replicated: the
    /// destination is keyed by one. Named rather than silently skipped.
    #[test]
    fn a_store_with_no_identity_is_named_not_shipped() {
        let d = TempDir::new();
        let path = d.path().join("plain.log");
        let cfg = crate::store::Config {
            chunk_size: 1 << 20,
            level: 1,
            flush_age_ms: u64::MAX,
        };
        let mut st = crate::store::Store {
            dir: d.path().to_path_buf(),
            cfg,
            files: std::collections::BTreeMap::new(),
        };
        st.create("plain.log").unwrap();
        let f = st.files.get_mut("plain.log").unwrap();
        f.append_windowed(b"2026-06-01T10:00:00Z line\n", 1, 1, &cfg)
            .unwrap();
        f.flush_chunk(&cfg).unwrap();
        drop(st);

        // No listener, and none is needed: a one-shot with nothing to
        // ship says so rather than failing on an endpoint it had no
        // reason to reach.
        let sent = cmd_send(&one(&path), &send_opts("127.0.0.1:1")).unwrap();
        assert_eq!(sent.unidentified, vec![path]);
        assert!(sent.streams.is_empty());
    }

    /// `frames` was a follower TYPE. Types are gone — a follower is fed
    /// a program — and `frames-send` is not one of those programs yet:
    /// it reads a store rather than a stream, and resumes from the
    /// RECEIVER's coverage rather than from a position timberfs holds.
    ///
    /// ⚠ Its way in is the consumer protocol's own hello, which may
    /// carry the coverage a destination already holds — see
    /// docs/plans/consumer-protocol.md. Until then a declaration naming
    /// it is refused where it is READ, with the command that fixes it.
    #[test]
    fn a_frames_type_is_refused_with_the_fix_named() {
        let reg = std::env::temp_dir().join(format!(
            "timberfs-framesdecl-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(crate::follower::follower_dir(&reg, "ship")).unwrap();
        std::fs::write(
            crate::follower::decl_path(&reg, "ship"),
            br#"{"store":"an-id","type":"frames","endpoint":"archive:4319"}"#,
        )
        .unwrap();
        let err = crate::follower::Declaration::load(&reg, "ship")
            .unwrap_err()
            .to_string();
        assert!(err.contains("frames"), "{err}");
        assert!(
            err.contains("follower update"),
            "it should name the fix, not just refuse: {err}"
        );
        let _ = std::fs::remove_dir_all(&reg);
    }

    #[test]
    fn a_client_that_is_not_speaking_this_protocol_is_refused() {
        let d = TempDir::new();
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, true));
        let mut sock = TcpStream::connect(&addr).unwrap();
        sock.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        sock.flush().unwrap();
        drop(sock);
        let err = server
            .join()
            .unwrap()
            .expect_err("not a replication stream");
        assert!(
            format!("{err:#}").contains("not a timberfs replication stream"),
            "{err:#}"
        );
    }
}
