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

/// A conservative ceiling on a single stream's in-flight bytes. Per STREAM
/// from the start, not per connection: the window is needed either way so a
/// sender does not stall on each chunk's ack, and scoping it here is what
/// leaves multiplexing as bookkeeping rather than a redesign.
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

    let mut unacked = 0u64;
    while let Some(f) = r.next_frame()? {
        let size = match &f.frame {
            Frame::Chunk { comp_len, .. } => *comp_len,
            _ => 0,
        };
        session.apply(f.frame)?;
        unacked += size;
        if unacked >= WINDOW_BYTES / 2 {
            send(
                &mut w,
                stream_id,
                Frame::Coverage {
                    runs: session.coverage(),
                },
            )?;
            unacked = 0;
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

    // Resume from the RECEIVER's position: it is authoritative, so a sender
    // keeps no cursor of its own for this.
    let resume = accepted_at
        .iter()
        .map(|r| r.end + 1)
        .max()
        .unwrap_or(opts.first_seq)
        .max(opts.first_seq);

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
    // Skip serve's own stream-open: the handshake already opened this
    // stream, and a second open would be a second stream.
    let (_, skip) = frame::decode(&body)?.expect("serve wrote a whole frame");
    w.write_all(&body[skip..])?;
    w.flush()?;
    w.shutdown(std::net::Shutdown::Write).ok();

    let mut acked = Vec::new();
    while let Some(f) = r.next_frame()? {
        if let Frame::Coverage { runs } = f.frame {
            acked = runs;
        }
    }
    Ok(Sent {
        chunks: served.chunks,
        comp_bytes: served.comp_bytes,
        skipped_already_held: resume.saturating_sub(opts.first_seq),
        accepted_at,
        acked,
    })
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
            endpoint: addr.to_string(),
            first_seq: 0,
            sidecars: true,
            timeout: Duration::from_secs(10),
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
