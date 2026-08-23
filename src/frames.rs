//! The replication transport: a socket at each end of the frame codec.
//!
//! Blocking `TcpListener` plus a thread per connection, like the OTLP and
//! Forward intakes — no async runtime, and one connection carries one
//! stream. The stream id is in every frame so multiplexing is a transport
//! change later rather than a format change, but its price is per-stream
//! flow control (without it one stalled store head-of-line-blocks every
//! other stream on the connection), so it waits for a case that needs it.
//!
//! No TLS: a private network, or a tunnel. Same rule as the other intakes.
//!
//! The handshake is what this adds over a pipe. A sender says what it is
//! about to send; the receiver answers `accepted` with what it already
//! holds — so the sender resumes from the receiver's own position instead
//! of guessing — or `conflict`, naming the origin that holds the name. That
//! turns a collision into a sentence at setup time instead of a
//! mislabelled store found weeks later.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};

use crate::frame::{self, Frame, Framed, Hello, Mode, Run};
use crate::receive::{Numbering, Reader, ReceiveOpts, Received, Session};

/// Where per-stream flow control will attach. Declared per STREAM rather
/// than per connection because that is the shape multiplexing needs — one
/// stalled store must not head-of-line-block the others — but nothing
/// waits on it yet: a sender ships what it has and TCP's own backpressure
/// does the work while one connection carries one stream.
pub const WINDOW_BYTES: u64 = 8 << 20;

#[derive(Debug, Clone)]
pub struct IntakeOpts {
    pub listen: String,
    pub into_dir: PathBuf,
    /// Which label names the store, as in the other intakes.
    pub route: String,
    pub auto_create: bool,
    /// Receive as a replica — origin and numbering preserved together.
    pub replica: bool,
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
    let listener =
        TcpListener::bind(&opts.listen).with_context(|| format!("binding {}", opts.listen))?;
    crate::note!(
        "timberfs: frames intake listening on {} -> {} (route {}, {})",
        opts.listen,
        opts.into_dir.display(),
        opts.route,
        if opts.replica { "replica" } else { "copy" }
    );
    std::fs::create_dir_all(&opts.into_dir)
        .with_context(|| format!("creating {}", opts.into_dir.display()))?;
    for conn in listener.incoming() {
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

/// One connection: hello, the handshake, then the stream.
pub fn serve_connection(sock: TcpStream, opts: &IntakeOpts) -> anyhow::Result<Received> {
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

    let (open, stream_id) = crate::receive::read_opening(&mut r)?;
    let labels = open.labels();
    let route = labels
        .get(&opts.route)
        .and_then(|v| v.as_str())
        .unwrap_or("unrouted")
        .to_string();
    let name = crate::intake::store_name(&route);
    let dest = opts.into_dir.join(&name).join(&name);

    let ropts = ReceiveOpts {
        numbering: if opts.replica {
            Numbering::Preserve
        } else {
            Numbering::Renumber
        },
        index: opts.index,
        wal: opts.wal,
    };
    if !opts.auto_create && !crate::format::rings_path(dest.parent().unwrap(), &name).exists() {
        // Same posture as the other intakes: an undeclared stream is
        // refused rather than created, and the refusal says what to do.
        let reason = format!(
            "undeclared stream {route:?} — pre-create it, or run the intake with \
             --auto-create"
        );
        send(
            &mut w,
            stream_id,
            Frame::Conflict {
                holder_origin: [0u8; 16],
                runs: Vec::new(),
                reason: reason.clone(),
            },
        )?;
        bail!("{peer}: {reason}");
    }

    // The chunking values matter less here than in an entry intake: every
    // frame arrives already compressed and bypasses the buffer, so this
    // config only governs a flush the receive path never triggers.
    let cfg = crate::store::Config {
        chunk_size: 256 * 1024,
        level: 3,
        flush_age_ms: 5000,
    };
    let mut session = match Session::open(&dest, &open, &ropts, &cfg) {
        Ok(s) => s,
        Err(e) => {
            // The conflict is the useful half of the handshake: the sender
            // learns WHY at setup time, on a terminal, rather than finding
            // a mislabelled store later.
            let held = crate::bark::load(dest.parent().unwrap(), &name)
                .and_then(|b| {
                    b.get("origin_id")
                        .and_then(|v| v.as_str())
                        .and_then(frame::uuid_bytes)
                })
                .unwrap_or([0u8; 16]);
            send(
                &mut w,
                stream_id,
                Frame::Conflict {
                    holder_origin: held,
                    runs: Vec::new(),
                    reason: format!("{e:#}"),
                },
            )?;
            return Err(e);
        }
    };

    // Accepted, with what we already hold: the sender resumes from OUR
    // position rather than guessing, which is the whole reason the answer
    // carries coverage.
    send(
        &mut w,
        stream_id,
        Frame::Accepted {
            registration_id: registration_id(dest.parent().unwrap(), &name),
            runs: session.coverage(),
        },
    )?;

    // Ack every chunk. A byte-window cadence starved a low-volume stream
    // of acks entirely — the ack IS what advances a sender's cursor, and
    // that cursor is what `retain_unconsumed` reads, so a quiet store
    // would never release its head. A coverage frame is ~28 bytes against
    // a 25 KB chunk, so the chattiness is 0.1% and buys correctness.
    while let Some(f) = r.next_frame()? {
        let was_chunk = matches!(f.frame, Frame::Chunk { .. });
        session.apply(f.frame)?;
        if was_chunk {
            send(
                &mut w,
                stream_id,
                Frame::Coverage {
                    runs: session.coverage(),
                },
            )?;
        }
    }
    // A final ack, so a sender that streamed less than a window still
    // learns where it got to.
    send(
        &mut w,
        stream_id,
        Frame::Coverage {
            runs: session.coverage(),
        },
    )?;
    let got = session.finish()?;
    crate::note!(
        "timberfs: frames intake: {peer} -> {} ({} chunk(s), {})",
        got.store.display(),
        got.chunks,
        crate::rotate::human_bytes(got.comp_bytes)
    );
    Ok(got)
}

/// The receiver's own durable handle for this store, which it controls —
/// distinct from the origin id, which travels and is never assigned here.
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

#[derive(Debug, Clone)]
pub struct SendOpts {
    pub endpoint: String,
    /// Start here unless the receiver's answer says it already has more.
    pub first_seq: u64,
    pub sidecars: bool,
    pub timeout: Duration,
    /// Keep shipping as chunks seal, on the same connection.
    pub follow: bool,
    /// How long to wait between polls of the store in `--follow`.
    pub poll: Duration,
    /// Record the far end's acked position here, so `retain_unconsumed`
    /// knows what has left the box. Nothing else needs it: the RECEIVER's
    /// coverage is what a resume reads.
    pub cursor: Option<PathBuf>,
}

impl SendOpts {
    pub fn to(endpoint: &str) -> SendOpts {
        SendOpts {
            endpoint: endpoint.to_string(),
            first_seq: 0,
            sidecars: true,
            timeout: Duration::from_secs(30),
            follow: false,
            poll: Duration::from_secs(1),
            cursor: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub chunks: u64,
    pub comp_bytes: u64,
    /// Where the receiver stood when it accepted — the resume point, and
    /// why a sender need not remember one of its own.
    pub accepted_at: Vec<Run>,
    /// The receiver's coverage after the last ack it sent.
    pub acked: Vec<Run>,
    pub skipped_already_held: u64,
}

/// Connect, handshake, and ship `store` from wherever the receiver says it
/// left off.
pub fn cmd_send(store: &Path, opts: &SendOpts) -> anyhow::Result<Sent> {
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

    // The opening frame is the serve side's own, so a sender and a server
    // describe a stream identically. Sent in Coverage mode first: it says
    // what this store is without committing to ship anything, which is
    // what makes the handshake a question rather than an assertion.
    let mut probe = Vec::new();
    crate::serve::serve(
        store,
        &crate::serve::Request {
            stream: 0,
            mode: Mode::Coverage,
            first_seq: opts.first_seq,
            last_seq: frame::OPEN_ENDED,
            sidecars: opts.sidecars,
        },
        &mut probe,
    )?;
    // Only the stream-open half: the coverage frame after it is ours, and
    // the receiver answers with its own.
    let (_, open_len) = frame::decode(&probe)?.expect("serve wrote a whole frame");
    w.write_all(&probe[..open_len])?;
    w.flush()?;

    let accepted_at = match r.next_frame()? {
        Some(Framed {
            frame: Frame::Accepted { runs, .. },
            ..
        }) => runs,
        Some(Framed {
            frame:
                Frame::Conflict {
                    holder_origin,
                    reason,
                    ..
                },
            ..
        }) => {
            bail!(
                "{addr} refused the stream: {reason}{}",
                if holder_origin == [0u8; 16] {
                    String::new()
                } else {
                    format!(" (held by origin {})", frame::uuid_string(&holder_origin))
                }
            )
        }
        Some(other) => bail!("expected accepted or conflict, got {other:?}"),
        None => bail!("{addr} closed the connection without answering the handshake"),
    };

    // Resume from the RECEIVER's position: it is authoritative, so a
    // sender keeps no position of its own and cannot re-ship.
    let mut resume = accepted_at
        .iter()
        .map(|r| r.end + 1)
        .max()
        .unwrap_or(opts.first_seq)
        .max(opts.first_seq);
    let skipped = resume.saturating_sub(opts.first_seq);

    let mut last_sent: Option<(u64, u64)> = None;
    let mut out = Sent {
        chunks: 0,
        comp_bytes: 0,
        skipped_already_held: skipped,
        accepted_at,
        acked: Vec::new(),
    };

    // In follow mode the read side polls for acks, so its timeout IS the
    // poll interval; a one-shot send reads acks only after it has stopped
    // writing, where blocking to EOF is correct.
    if opts.follow {
        r.get_ref().set_read_timeout(Some(opts.poll)).ok();
    }

    loop {
        let mut body = Vec::new();
        let served = crate::serve::serve(
            store,
            &crate::serve::Request {
                stream: 0,
                mode: Mode::Frames,
                first_seq: resume,
                last_seq: frame::OPEN_ENDED,
                sidecars: opts.sidecars,
            },
            &mut body,
        )?;
        if served.chunks > 0 {
            // Skip serve's own stream-open: the handshake already opened
            // this stream, and a second open would be a second stream.
            let (_, skip) = frame::decode(&body)?.expect("serve wrote a whole frame");
            w.write_all(&body[skip..])?;
            w.flush()?;
            out.chunks += served.chunks;
            out.comp_bytes += served.comp_bytes;
            resume += served.chunks + served.raced_away;
            if let Some(sent) = served.last_sent {
                last_sent = Some(sent);
            }
        }
        if !opts.follow {
            break;
        }
        // Record whatever the far end has acknowledged. That is what
        // `retain_unconsumed` reads to know what has left this box — the
        // RECEIVER's position, not our own idea of progress, so nothing
        // is dropped locally until it is durably elsewhere.
        //
        // DRAIN the acks before writing: they arrive one per chunk, and
        // writing per ack made the cursor the most expensive thing in the
        // loop. One write per pass says the same thing to both of its
        // readers, neither of which needs per-chunk precision.
        let mut acked = None;
        while let Some(runs) = poll_ack(&mut r)? {
            acked = Some(runs);
        }
        match acked {
            Some(runs) => {
                out.acked = runs;
                if let Some(path) = &opts.cursor {
                    write_cursor(
                        path,
                        store,
                        &out.acked,
                        out.chunks,
                        wl_for(&out.acked, last_sent),
                    )?;
                }
            }
            None => thread::sleep(opts.poll),
        }
    }

    // Done writing: now the far end will finish and send its last ack.
    w.shutdown(std::net::Shutdown::Write).ok();
    r.get_ref().set_read_timeout(Some(opts.timeout)).ok();
    while let Some(f) = r.next_frame()? {
        if let Frame::Coverage { runs } = f.frame {
            out.acked = runs;
        }
    }
    if let Some(path) = &opts.cursor {
        write_cursor(
            path,
            store,
            &out.acked,
            out.chunks,
            wl_for(&out.acked, last_sent),
        )?;
    }
    Ok(out)
}

/// The acked chunk's write time, but only when the acknowledgement has
/// caught up with what was sent — that is the one chunk whose window this
/// sender still has in hand. Behind that, `None` leaves the recorded value
/// alone rather than overstating it with a newer chunk's time.
fn wl_for(acked: &[Run], last_sent: Option<(u64, u64)>) -> Option<u64> {
    let last_acked = acked.iter().map(|r| r.end).max()?;
    match last_sent {
        Some((seq, wl)) if seq == last_acked => Some(wl),
        _ => None,
    }
}

/// One pending ack, if the far end has sent one. The socket's read timeout
/// bounds the wait, and hitting it is not an error here — it means "nothing
/// acked yet", which is the normal state of a follow loop.
fn poll_ack<R: std::io::Read>(r: &mut Reader<R>) -> anyhow::Result<Option<Vec<Run>>> {
    match r.next_frame() {
        Ok(Some(Framed {
            frame: Frame::Coverage { runs },
            ..
        })) => Ok(Some(runs)),
        Ok(Some(_)) => Ok(None),
        Ok(None) => Ok(None),
        Err(e) => {
            // A read timeout IS "nothing yet" in a poll loop. Matched on
            // the error kind rather than its text: the same condition
            // surfaces as WouldBlock on Linux and TimedOut elsewhere, and
            // its message ("Resource temporarily unavailable") says
            // neither.
            let timed_out = e.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
            });
            if timed_out {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// Record what the FAR END holds, in the registry's cursor format, so the
/// retention interest axis can read it.
///
/// `seq` is the last chunk the receiver acknowledged, not the next one
/// wanted: the interest floor is `min(seq)` over retaining followers and
/// treats `seq >= next_seq` as a hand-edit that pins the whole store, so a
/// caught-up sender must stay one below. That over-retains by exactly one
/// chunk — the harmless direction, and interest is additive anyway.
///
/// `wl` is the acked chunk's write time when the caller knows it, and is
/// informational: `follower list` renders lag from it, so leaving it at 0
/// reads as decades behind. Deliberately a PARAMETER rather than something
/// looked up here — finding it meant parsing the whole `.rings`, once per
/// ack, which is quadratic over a run and reads 560 KB per chunk shipped on
/// a 10,000-chunk store, all for a display column.
fn write_cursor(
    path: &Path,
    store: &Path,
    acked: &[Run],
    delivered: u64,
    wl: Option<u64>,
) -> anyhow::Result<bool> {
    let Some(last) = acked.iter().map(|r| r.end).max() else {
        return Ok(false); // nothing acknowledged yet: no position to record
    };
    let (dir, name) = crate::query::resolve_backing(store)?;
    let anchor = crate::cursor::store_anchor(&dir, &name, crate::bark::load(&dir, &name).as_ref());
    let mut c = crate::cursor::Cursor::load(path)
        .unwrap_or(None)
        .unwrap_or_else(|| {
            crate::cursor::Cursor::new("frames-send", &anchor, &store.display().to_string())
        });
    if c.seq == Some(last) && c.delivered == delivered {
        return Ok(false); // unchanged: no write, so a quiet loop is quiet
    }
    c.store = anchor;
    c.path = store.display().to_string();
    c.seq = Some(last);
    c.n = 0;
    c.delivered = delivered;
    if let Some(wl) = wl {
        c.wl = wl;
    }
    c.save(path)?;
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

    /// A source store with `chunks` chunks, labelled for routing.
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

    fn opts(into: &Path, replica: bool, auto: bool) -> IntakeOpts {
        IntakeOpts {
            listen: String::new(),
            into_dir: into.to_path_buf(),
            route: "service".to_string(),
            auto_create: auto,
            replica,
            index: false,
            wal: false,
        }
    }

    /// Serve exactly one connection on an ephemeral port, in a thread.
    fn one_shot(o: IntakeOpts) -> (String, thread::JoinHandle<anyhow::Result<Received>>) {
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

    #[test]
    fn a_store_crosses_a_socket_and_arrives_byte_identical() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 5, "apache-error");
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, true, true));

        let sent = cmd_send(&src, &send_opts(&addr)).unwrap();
        let got = server.join().unwrap().unwrap();

        assert_eq!(sent.chunks, 5);
        assert_eq!(got.chunks, 5);
        assert_eq!(
            sent.accepted_at,
            vec![],
            "a fresh destination holds nothing"
        );
        assert_eq!(
            sent.acked,
            vec![Run { start: 0, end: 4 }],
            "acked to the end"
        );

        // Routed by the label, and byte-identical on arrival.
        let dst = into.join("apache-error.log").join("apache-error.log");
        let (sd, sn) = crate::query::resolve_backing(&src).unwrap();
        let (dd, dn) = crate::query::resolve_backing(&dst).unwrap();
        assert_eq!(
            std::fs::read(crate::format::trunk_path(&sd, &sn)).unwrap(),
            std::fs::read(crate::format::trunk_path(&dd, &dn)).unwrap(),
        );
    }

    #[test]
    fn the_receiver_says_where_it_left_off_and_the_sender_resumes_there() {
        // The handshake's point: the receiver's position is authoritative,
        // so a sender keeps no cursor of its own and cannot re-send.
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 3, "svc");
        let into = d.path().join("recv");

        let (addr, server) = one_shot(opts(&into, true, true));
        cmd_send(&src, &send_opts(&addr)).unwrap();
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
        for i in 3..6 {
            f.append_windowed(
                format!("2026-06-01T10:00:0{i}Z line {i} more\n").as_bytes(),
                1_000 + i as u64,
                1_000 + i as u64,
                &cfg,
            )
            .unwrap();
            f.flush_chunk(&cfg).unwrap();
        }
        drop(st);

        // The sender asks from 0 again; the receiver's answer moves it on.
        let (addr, server) = one_shot(opts(&into, true, true));
        let sent = cmd_send(&src, &send_opts(&addr)).unwrap();
        let got = server.join().unwrap().unwrap();
        assert_eq!(sent.accepted_at, vec![Run { start: 0, end: 2 }]);
        assert_eq!(sent.skipped_already_held, 3, "resumed, not re-sent");
        assert_eq!(sent.chunks, 3);
        assert_eq!(got.chunks, 3);
        assert_eq!(got.runs, vec![Run { start: 0, end: 5 }], "one run");
    }

    #[test]
    fn nothing_new_to_send_is_a_successful_no_op() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 2, "svc");
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, true, true));
        cmd_send(&src, &send_opts(&addr)).unwrap();
        server.join().unwrap().unwrap();

        let (addr, server) = one_shot(opts(&into, true, true));
        let sent = cmd_send(&src, &send_opts(&addr)).unwrap();
        let got = server.join().unwrap().unwrap();
        assert_eq!(sent.chunks, 0, "already there");
        assert_eq!(got.chunks, 0);
        assert_eq!(sent.acked, vec![Run { start: 0, end: 1 }]);
    }

    #[test]
    fn a_second_origin_is_refused_with_a_reason_the_sender_can_read() {
        // The handshake exists for this: two stores that route to one name
        // collide at SETUP, on the sender's terminal, naming the holder.
        let d = TempDir::new();
        let into = d.path().join("recv");
        let a = a_store(d.path(), "a", 2, "same-name");
        let (addr, server) = one_shot(opts(&into, true, true));
        cmd_send(&a, &send_opts(&addr)).unwrap();
        server.join().unwrap().unwrap();

        let b = a_store(d.path(), "b", 2, "same-name");
        let (addr, server) = one_shot(opts(&into, true, true));
        let err = cmd_send(&b, &send_opts(&addr)).expect_err("a different origin");
        let msg = format!("{err:#}");
        assert!(msg.contains("refused the stream"), "{msg}");
        assert!(msg.contains("one store"), "{msg}");
        assert!(msg.contains("held by origin"), "{msg}");
        let _ = server.join().unwrap();

        // ...and the first store is untouched by the refusal.
        let dst = into.join("same-name.log").join("same-name.log");
        let recs = crate::format::read_index(&crate::format::rings_path(
            dst.parent().unwrap(),
            "same-name.log",
        ))
        .unwrap();
        assert_eq!(recs.len(), 2, "the refusal wrote nothing");
    }

    #[test]
    fn an_undeclared_stream_is_refused_when_auto_create_is_off() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 1, "unknown-svc");
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, true, false));
        let err = cmd_send(&src, &send_opts(&addr)).expect_err("not pre-created");
        assert!(format!("{err:#}").contains("undeclared stream"), "{err:#}");
        let _ = server.join().unwrap();
        assert!(!into.join("unknown-svc.log").exists(), "nothing created");
    }

    #[test]
    fn a_copy_receiver_renumbers_and_claims_no_origin() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 3, "svc");
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, false, true));
        cmd_send(&src, &send_opts(&addr)).unwrap();
        server.join().unwrap().unwrap();

        let dst_dir = into.join("svc.log");
        let bark = crate::bark::load(&dst_dir, "svc.log").unwrap();
        assert!(!bark.contains_key("origin_id"), "{bark:?}");
        assert!(bark.contains_key("derived_from"), "lineage still travels");
        assert_eq!(bark.get("host").unwrap(), "apache01");
    }

    #[test]
    fn the_cursor_records_what_the_far_end_holds() {
        // The cursor is not a resume point -- the receiver's coverage is.
        // It exists so `retain_unconsumed` knows what has left this box,
        // which means it must record the far end's acknowledgement and not
        // our own idea of progress.
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 4, "svc");
        let into = d.path().join("recv");
        let cur = d.path().join("cursor.json");
        let (addr, server) = one_shot(opts(&into, true, true));
        let sent = cmd_send(
            &src,
            &SendOpts {
                cursor: Some(cur.clone()),
                timeout: Duration::from_secs(10),
                ..SendOpts::to(&addr)
            },
        )
        .unwrap();
        server.join().unwrap().unwrap();

        assert_eq!(sent.acked, vec![Run { start: 0, end: 3 }]);
        let c = crate::cursor::Cursor::load(&cur).unwrap().expect("written");
        // The LAST ACKED chunk, deliberately not the next one wanted: the
        // interest floor is min(seq) and treats seq >= next_seq as a
        // hand-edit that pins the whole store, so a caught-up sender stays
        // one below. One chunk of over-retention, the harmless direction.
        assert_eq!(c.seq, Some(3));
        assert_eq!(c.n, 0, "whole chunks only; never a partial position");
        assert_eq!(c.consumer, "frames-send");
        // `follower list` renders lag from wl, so an unset one reads as
        // decades behind rather than caught up.
        assert!(c.wl > 0, "the acked chunk's write time is recorded");
        // Anchored by the store's IDENTITY, so a moved store still matches.
        let bark = crate::bark::load(d.path(), "src.log").unwrap();
        assert_eq!(c.store, bark.get("id").unwrap().as_str().unwrap());
    }

    #[test]
    fn an_unchanged_position_is_not_rewritten() {
        // Acks arrive one per chunk, so writing on every one made the
        // cursor the most expensive thing in the loop. A write that would
        // say what the file already says is skipped.
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 3, "svc");
        let cur = d.path().join("cursor.json");
        let runs = vec![Run { start: 0, end: 2 }];
        assert!(
            write_cursor(&cur, &src, &runs, 3, Some(1_002)).unwrap(),
            "the first write happens"
        );
        assert!(
            !write_cursor(&cur, &src, &runs, 3, Some(1_002)).unwrap(),
            "the same position is not written again"
        );
        assert!(
            write_cursor(&cur, &src, &[Run { start: 0, end: 2 }], 4, Some(1_002)).unwrap(),
            "a changed delivered count is a change"
        );
    }

    #[test]
    fn the_write_time_comes_from_the_frame_not_from_the_rings() {
        // Finding it in the .rings meant parsing the whole file, once per
        // ack -- quadratic over a run. It is only supplied when the ack has
        // caught up with what was sent, which is the one chunk whose window
        // the sender still has in hand.
        assert_eq!(
            wl_for(&[Run { start: 0, end: 4 }], Some((4, 9_999))),
            Some(9_999)
        );
        // The ack is behind what was sent: no value rather than a newer
        // chunk's time, which would overstate the position.
        assert_eq!(wl_for(&[Run { start: 0, end: 2 }], Some((4, 9_999))), None);
        // Nothing sent this pass, or nothing acked at all.
        assert_eq!(wl_for(&[Run { start: 0, end: 2 }], None), None);
        assert_eq!(wl_for(&[], Some((4, 9_999))), None);
    }

    #[test]
    fn nothing_acked_writes_no_position_rather_than_a_false_one() {
        // A sender that shipped nothing must not record a position: a
        // retaining follower with no position holds everything, which is
        // the safe reading, and inventing seq 0 would release the head.
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 0, "svc");
        let into = d.path().join("recv");
        let cur = d.path().join("cursor.json");
        let (addr, server) = one_shot(opts(&into, true, true));
        cmd_send(
            &src,
            &SendOpts {
                cursor: Some(cur.clone()),
                timeout: Duration::from_secs(10),
                ..SendOpts::to(&addr)
            },
        )
        .unwrap();
        let _ = server.join().unwrap();
        assert!(!cur.exists(), "no acknowledgement, no position");
    }

    #[test]
    fn a_follower_of_type_frames_runs_frames_send() {
        // What the systemd unit executes. No `--start`: a frames sender
        // resumes from the receiver's coverage, so there is no local
        // decision about where to begin -- and no way to re-ship a store
        // by getting it wrong.
        let decl = crate::follower::Declaration {
            name: "ship".to_string(),
            store: "an-id".to_string(),
            path: "/var/log/timberfs/app/app.log".to_string(),
            kind: "frames".to_string(),
            endpoint: Some("archive:4319".to_string()),
            retaining: true,
            args: vec![],
            created: String::new(),
            extra: serde_json::Map::new(),
        };
        let argv = decl
            .argv(
                Path::new("/reg/ship/cursor.json"),
                Path::new("/var/log/timberfs/app/app.log"),
            )
            .unwrap();
        assert!(argv[0].ends_with("timberfs"), "{argv:?}");
        assert_eq!(argv[1], "frames-send");
        assert!(argv.contains(&"--follow".to_string()), "{argv:?}");
        assert!(argv.contains(&"--cursor".to_string()), "{argv:?}");
        assert!(
            argv.contains(&"archive:4319".to_string()),
            "the endpoint is passed through: {argv:?}"
        );
        assert!(
            !argv.contains(&"--start".to_string()),
            "a frames sender has no start to choose: {argv:?}"
        );
        assert_eq!(argv.last().unwrap(), "/var/log/timberfs/app/app.log");
    }

    #[test]
    fn a_client_that_is_not_speaking_this_protocol_is_refused() {
        let d = TempDir::new();
        let into = d.path().join("recv");
        let (addr, server) = one_shot(opts(&into, true, true));
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
