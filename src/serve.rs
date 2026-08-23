//! Reading a store out onto the replication wire: `coverage` (what I
//! hold), `index` (per-chunk metadata, no bytes) or `frames` (the bytes
//! too). See `docs/plans/native-replication.md`.
//!
//! Read-only and lock-free, like `query` and `info` — and it reuses
//! `query`'s seqlock guard rather than reimplementing it, because a
//! concurrent retention collapse shifts the offsets under a reader and
//! getting that wrong yields garbage bytes rather than an error.
//!
//! `stream-open` is emitted by whoever SENDS the data, so this stream is
//! self-describing: which origin, which labels, what range, what
//! granularity, then the content. That is the same shape a pushing sender
//! writes, which is why one codec serves both directions.

use std::io::Write;
use std::path::Path;

use anyhow::Context;

use crate::frame::{self, Frame, Framed, Mode, Run, Sidecar};

/// The `.grain` tag, at both levels: the store's parameter header rides
/// `stream-open`, each chunk's filter page rides its chunk. A receiver
/// whose tokenizer parameters differ does not recognise the kind and
/// rebuilds, which is the behaviour we want and not a check to remember.
pub const GRAIN_TAG: &str = "GRAIN001";

#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub stream: u32,
    pub mode: Mode,
    pub first_seq: u64,
    /// `frame::OPEN_ENDED` for "as far as this store goes".
    pub last_seq: u64,
    /// Ship `.grain` pages beside the chunks. Worth it: without them a
    /// receiver that maintains a token index has to decompress every
    /// chunk to re-tokenize what the sender already computed.
    pub sidecars: bool,
}

impl Request {
    pub fn everything(mode: Mode) -> Request {
        Request {
            stream: 0,
            mode,
            first_seq: 0,
            last_seq: frame::OPEN_ENDED,
            sidecars: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Served {
    pub frames: u64,
    pub chunks: u64,
    /// Compressed bytes of chunk payload actually written — zero in
    /// `Coverage` and `Index` mode.
    pub comp_bytes: u64,
    /// Chunks whose bytes a concurrent head-drop retained away between
    /// reading the index and reading the frame. Reported rather than
    /// hidden: the stream is short by this many, which the receiver sees
    /// as a gap and the operator should see as a number.
    pub raced_away: u64,
    /// The highest chunk actually sent, and its write window's end. Handed
    /// back so a caller needing that write time does not re-read the rings
    /// to find what it just had in hand.
    pub last_sent: Option<(u64, u64)>,
}

/// Group a store's chunk numbers into runs, both ends inclusive. Today a
/// store is one run — retention drops only prefixes — but the grouping is
/// written for gaps because a fragment set is the same answer with more of
/// them, and a run list that only ever worked for the contiguous case
/// would have to be rewritten to say so.
pub fn runs_of(seqs: impl IntoIterator<Item = u64>) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for s in seqs {
        match out.last_mut() {
            Some(last) if s == last.end + 1 => last.end = s,
            Some(last) if s <= last.end => {} // duplicate or out of order
            _ => out.push(Run { start: s, end: s }),
        }
    }
    out
}

/// Write `input`'s answer to `req` as frames.
pub fn serve(input: &Path, req: &Request, out: &mut impl Write) -> anyhow::Result<Served> {
    let mut handle = crate::query::open_source(input)?;
    let guard = crate::query::seq_guard(input);
    let mut stats = Served::default();

    let bark = handle.bark.clone().unwrap_or_default();
    let mut open_sidecars = Vec::new();
    if req.sidecars {
        if let Some(header) = grain_header(input) {
            open_sidecars.push(Sidecar {
                kind: Sidecar::tag(GRAIN_TAG),
                bytes: header,
            });
        }
    }

    // Which chunks the request selects. Done before stream-open so its
    // declared range is what is actually coming, not what was asked for.
    let selected: Vec<crate::format::ChunkRecord> = handle
        .records
        .iter()
        .filter(|c| {
            c.seq >= req.first_seq && (req.last_seq == frame::OPEN_ENDED || c.seq <= req.last_seq)
        })
        .copied()
        .collect();
    let positions: Vec<usize> = handle
        .records
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.seq >= req.first_seq && (req.last_seq == frame::OPEN_ENDED || c.seq <= req.last_seq)
        })
        .map(|(i, _)| i)
        .collect();

    let mut buf = Vec::new();
    frame::encode(
        &Framed {
            stream: req.stream,
            frame: Frame::StreamOpen {
                origin_id: origin_of(&bark),
                sender_id: id_of(&bark),
                first_seq: selected.first().map(|c| c.seq).unwrap_or(req.first_seq),
                last_seq: selected.last().map(|c| c.seq).unwrap_or(frame::OPEN_ENDED),
                mode: req.mode,
                provenance: serde_json::to_vec(&crate::bark::provenance(&bark))
                    .context("serializing the store's labels")?,
                sidecars: open_sidecars,
            },
        },
        &mut buf,
    );
    out.write_all(&buf)?;
    stats.frames += 1;

    if req.mode == Mode::Coverage {
        let mut buf = Vec::new();
        frame::encode(
            &Framed {
                stream: req.stream,
                frame: Frame::Coverage {
                    runs: runs_of(selected.iter().map(|c| c.seq)),
                },
            },
            &mut buf,
        );
        out.write_all(&buf)?;
        stats.frames += 1;
        return Ok(stats);
    }

    let grain = if req.sidecars {
        grain_path(input).and_then(|p| crate::grain::load(&p).ok())
    } else {
        None
    };

    for (c, pos) in selected.into_iter().zip(positions) {
        let mut sidecars = Vec::new();
        if let Some(page) = grain.as_ref().and_then(|g| g.page(pos)) {
            sidecars.push(Sidecar {
                kind: Sidecar::tag(GRAIN_TAG),
                bytes: page.to_vec(),
            });
        }
        let comp = if req.mode == Mode::Frames {
            match crate::query::read_chunk_raw(input, &guard, &mut handle, c)? {
                Some(bytes) => Some(bytes),
                None => {
                    // Retained away between the index read and the frame
                    // read. Legitimate — the same as if the request had
                    // arrived a moment later — so skip it and count it.
                    stats.raced_away += 1;
                    continue;
                }
            }
        } else {
            None
        };
        stats.comp_bytes += comp.as_ref().map(|b| b.len() as u64).unwrap_or(0);
        let mut buf = Vec::new();
        frame::encode(
            &Framed {
                stream: req.stream,
                frame: Frame::Chunk {
                    seq: c.seq,
                    uncomp_len: c.uncomp_len,
                    comp_len: c.comp_len,
                    comp,
                    first_write_ms: c.first_write_ms,
                    last_write_ms: c.last_write_ms,
                    sidecars,
                },
            },
            &mut buf,
        );
        out.write_all(&buf)?;
        stats.frames += 1;
        stats.chunks += 1;
        stats.last_sent = Some((c.seq, c.last_write_ms));
    }
    Ok(stats)
}

/// The travelling half of the address: a recorded `origin_id` if this
/// store received one, else its own `id` — a store nobody handed a lineage
/// to IS the origin. All-zero when it declares no identity at all, which a
/// plain `append` store does not.
fn origin_of(bark: &serde_json::Map<String, serde_json::Value>) -> [u8; 16] {
    bark.get("origin_id")
        .and_then(|v| v.as_str())
        .and_then(frame::uuid_bytes)
        .unwrap_or_else(|| id_of(bark))
}

fn id_of(bark: &serde_json::Map<String, serde_json::Value>) -> [u8; 16] {
    bark.get("id")
        .and_then(|v| v.as_str())
        .and_then(frame::uuid_bytes)
        .unwrap_or([0u8; 16])
}

/// A backing pair's `.grain`, if it has one. A bundle carries no grain —
/// bundles do not ship one yet — so `None`, and the receiver rebuilds.
fn grain_path(input: &Path) -> Option<std::path::PathBuf> {
    let (dir, name) = crate::query::resolve_backing(input).ok()?;
    let p = crate::format::grain_path(&dir, &name);
    p.exists().then_some(p)
}

/// The `.grain`'s 16-byte parameter header, which is store-level and so
/// rides `stream-open` rather than every chunk.
fn grain_header(input: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(grain_path(input)?).ok()?;
    (bytes.len() >= 16).then(|| bytes[..16].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::decode;

    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("timberfs-serve-test-{}-{n}", std::process::id()));
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

    /// Decode a whole served stream back into frames.
    fn frames(bytes: &[u8]) -> Vec<Framed> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let (f, used) = decode(&bytes[at..]).unwrap().expect("a whole frame");
            out.push(f);
            at += used;
        }
        out
    }

    /// A store with `chunks` one-line chunks, each flushed explicitly.
    fn a_store(dir: &Path, name: &str, chunks: usize) -> std::path::PathBuf {
        let path = dir.join(format!("{name}.log"));
        crate::bark::cmd_create(&path, true, false, None, None, false, &[], false).unwrap();
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
                format!("2026-06-01T10:00:0{i}Z line {i} padding padding\n").as_bytes(),
                1_000 + i as u64,
                1_000 + i as u64,
                &cfg,
            )
            .unwrap();
            f.flush_chunk(&cfg).unwrap();
        }
        path
    }

    #[test]
    fn coverage_mode_answers_with_runs_and_no_bytes() {
        let d = TempDir::new();
        let p = a_store(d.path(), "cov", 4);
        let mut buf = Vec::new();
        let stats = serve(&p, &Request::everything(Mode::Coverage), &mut buf).unwrap();
        assert_eq!(stats.chunks, 0);
        assert_eq!(stats.comp_bytes, 0);
        let fs = frames(&buf);
        assert_eq!(fs.len(), 2, "stream-open then coverage");
        match &fs[1].frame {
            Frame::Coverage { runs } => assert_eq!(runs, &[Run { start: 0, end: 3 }]),
            other => panic!("{other:?}"),
        }
        // A discovery answer stays small whatever the store holds.
        assert!(buf.len() < 256, "coverage was {} bytes", buf.len());
    }

    #[test]
    fn index_mode_carries_the_sizes_without_the_payload() {
        let d = TempDir::new();
        let p = a_store(d.path(), "idx", 3);
        let mut buf = Vec::new();
        let stats = serve(&p, &Request::everything(Mode::Index), &mut buf).unwrap();
        assert_eq!(stats.chunks, 3);
        assert_eq!(stats.comp_bytes, 0, "no payload in index mode");
        for f in frames(&buf).iter().skip(1) {
            match &f.frame {
                Frame::Chunk { comp, comp_len, .. } => {
                    assert!(comp.is_none());
                    assert!(*comp_len > 0, "the TRUE size still travels");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn frames_mode_ships_the_compressed_bytes_verbatim() {
        let d = TempDir::new();
        let p = a_store(d.path(), "frm", 3);
        let (dir, name) = crate::query::resolve_backing(&p).unwrap();
        let trunk = std::fs::read(crate::format::trunk_path(&dir, &name)).unwrap();

        let mut buf = Vec::new();
        let stats = serve(&p, &Request::everything(Mode::Frames), &mut buf).unwrap();
        assert_eq!(stats.chunks, 3);
        assert_eq!(stats.raced_away, 0);

        // Verbatim means verbatim: each frame's bytes are the trunk's own,
        // and they still decompress on their own.
        let mut at = 0u64;
        for f in frames(&buf).iter().skip(1) {
            match &f.frame {
                Frame::Chunk { comp, comp_len, .. } => {
                    let bytes = comp.as_ref().expect("payload present");
                    assert_eq!(bytes.len() as u64, *comp_len);
                    assert_eq!(
                        &bytes[..],
                        &trunk[at as usize..(at + comp_len) as usize],
                        "byte-identical to the trunk"
                    );
                    zstd::stream::decode_all(&bytes[..]).expect("a standalone frame");
                    at += comp_len;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(stats.comp_bytes, at, "every chunk accounted for");
    }

    #[test]
    fn the_stream_says_which_origin_and_what_labels() {
        let d = TempDir::new();
        let p = a_store(d.path(), "who", 1);
        crate::bark::cmd_set(&p, &["host=apache01".into(), "service=err".into()], &[]).unwrap();
        let mut buf = Vec::new();
        serve(&p, &Request::everything(Mode::Index), &mut buf).unwrap();
        let bark = crate::bark::load(d.path(), "who.log").unwrap();
        let own = frame::uuid_bytes(bark.get("id").unwrap().as_str().unwrap()).unwrap();
        match &frames(&buf)[0].frame {
            Frame::StreamOpen {
                origin_id,
                sender_id,
                provenance,
                ..
            } => {
                // Nobody handed this store a lineage, so it IS the origin.
                assert_eq!(*origin_id, own);
                assert_eq!(*sender_id, own);
                let labels: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_slice(provenance).unwrap();
                assert_eq!(labels.get("host").unwrap(), "apache01");
                // Settings are not labels and must not travel.
                assert!(!labels.contains_key("index"), "{labels:?}");
                assert!(!labels.contains_key("id"), "{labels:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_range_selects_and_the_declared_span_is_what_follows() {
        let d = TempDir::new();
        let p = a_store(d.path(), "rng", 6);
        let mut buf = Vec::new();
        let req = Request {
            stream: 2,
            mode: Mode::Index,
            first_seq: 2,
            last_seq: 4,
            sidecars: false,
        };
        let stats = serve(&p, &req, &mut buf).unwrap();
        assert_eq!(stats.chunks, 3);
        let fs = frames(&buf);
        assert!(fs.iter().all(|f| f.stream == 2));
        match &fs[0].frame {
            // The declared span is what is COMING, not what was asked for.
            Frame::StreamOpen {
                first_seq,
                last_seq,
                ..
            } => {
                assert_eq!((*first_seq, *last_seq), (2, 4));
            }
            other => panic!("{other:?}"),
        }
        let seqs: Vec<u64> = fs
            .iter()
            .skip(1)
            .map(|f| match &f.frame {
                Frame::Chunk { seq, .. } => *seq,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(seqs, vec![2, 3, 4]);
    }

    #[test]
    fn grain_pages_ride_along_so_the_receiver_need_not_re_tokenize() {
        let d = TempDir::new();
        let p = a_store(d.path(), "grn", 3);
        let (dir, name) = crate::query::resolve_backing(&p).unwrap();
        crate::grain::extend_grain(&dir, &name).unwrap();

        let mut buf = Vec::new();
        serve(&p, &Request::everything(Mode::Index), &mut buf).unwrap();
        let fs = frames(&buf);
        // The store-level parameter header rides stream-open; the per-chunk
        // pages ride their chunks. Same tag at both levels, so a receiver
        // with different tokenizer parameters recognises neither.
        match &fs[0].frame {
            Frame::StreamOpen { sidecars, .. } => {
                assert_eq!(sidecars.len(), 1);
                assert_eq!(sidecars[0].kind, Sidecar::tag(GRAIN_TAG));
                assert_eq!(sidecars[0].bytes.len(), 16);
                assert_eq!(&sidecars[0].bytes[..8], crate::grain::GRAIN_MAGIC);
            }
            other => panic!("{other:?}"),
        }
        for f in fs.iter().skip(1) {
            match &f.frame {
                Frame::Chunk { sidecars, .. } => {
                    assert_eq!(sidecars.len(), 1, "a page per chunk");
                    assert!(!sidecars[0].bytes.is_empty());
                }
                other => panic!("{other:?}"),
            }
        }
        // ...and not when they are not asked for.
        let mut bare = Vec::new();
        let req = Request {
            sidecars: false,
            ..Request::everything(Mode::Index)
        };
        serve(&p, &req, &mut bare).unwrap();
        assert!(bare.len() < buf.len());
    }

    #[test]
    fn a_served_frames_stream_reconstructs_the_trunk_exactly() {
        // The whole premise: concatenating what was served gives back the
        // origin's trunk byte for byte, so a receiver appends and is done.
        let d = TempDir::new();
        let p = a_store(d.path(), "exact", 12);
        let (dir, name) = crate::query::resolve_backing(&p).unwrap();
        let trunk = std::fs::read(crate::format::trunk_path(&dir, &name)).unwrap();

        let mut buf = Vec::new();
        serve(&p, &Request::everything(Mode::Frames), &mut buf).unwrap();
        let rebuilt: Vec<u8> = frames(&buf)
            .iter()
            .skip(1)
            .flat_map(|f| match &f.frame {
                Frame::Chunk { comp, .. } => comp.clone().unwrap(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(rebuilt, trunk, "the trunk round-trips through the wire");
        // And the reconstruction is a valid log on its own terms.
        let text = zstd::stream::decode_all(&rebuilt[..]).unwrap();
        assert_eq!(String::from_utf8(text).unwrap().lines().count(), 12);
    }

    #[test]
    fn runs_group_gaps_because_a_fragment_set_is_the_same_answer() {
        assert_eq!(runs_of([]), vec![]);
        assert_eq!(runs_of([0, 1, 2]), vec![Run { start: 0, end: 2 }]);
        assert_eq!(
            runs_of([0, 1, 5, 6, 9]),
            vec![
                Run { start: 0, end: 1 },
                Run { start: 5, end: 6 },
                Run { start: 9, end: 9 }
            ]
        );
        // A single chunk far along the tape is a run of one.
        assert_eq!(
            runs_of([424_242]),
            vec![Run {
                start: 424_242,
                end: 424_242
            }]
        );
    }

    #[test]
    fn an_empty_store_serves_an_empty_answer_that_is_still_an_answer() {
        let d = TempDir::new();
        let p = a_store(d.path(), "empty", 0);
        let mut buf = Vec::new();
        let stats = serve(&p, &Request::everything(Mode::Coverage), &mut buf).unwrap();
        assert_eq!(stats.chunks, 0);
        match &frames(&buf)[1].frame {
            Frame::Coverage { runs } => assert!(runs.is_empty(), "no runs, not no answer"),
            other => panic!("{other:?}"),
        }
    }
}
