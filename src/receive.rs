//! Consuming a replication stream into a store: the other half of
//! `serve`. See `docs/plans/native-replication.md`.
//!
//! Frames carry compressed bytes that go to the trunk verbatim and a
//! record to the rings — nothing is decompressed, which is the whole point
//! of the wire.
//!
//! A destination is a REPLICA and nothing else: the sender's chunk
//! numbering, its origin and its identity are preserved together, because
//! each is the same claim — this is that store, in another place. A
//! destination that numbered its own chunks would share no address with
//! its source and could not answer where a sender got to, so it is not a
//! mode here; `export`/`import` is where an independent tape is made. See
//! docs/plans/frames-selection.md.
//!
//! What this layer does decide is what to do with SIDECARS it is offered.
//! It does not decide WHICH store the stream lands in: that is the
//! receiving end's lookup, which belongs to whoever calls this, not to
//! the code that writes bytes.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use crate::frame::{self, Frame, Framed, Run};

#[derive(Debug, Clone, Copy, Default)]
pub struct ReceiveOpts {
    /// The destination's own policy, not the sender's. Settings never
    /// travel; only labels and identity do.
    pub index: bool,
    pub wal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub store: PathBuf,
    pub created: bool,
    pub chunks: u64,
    pub comp_bytes: u64,
    /// What the destination now holds — the ack, and a coverage answer in
    /// its own right.
    pub runs: Vec<Run>,
    /// Sidecars offered and refused, because this build did not recognise
    /// the kind or its parameters. Counted rather than silently dropped:
    /// it means the destination is rebuilding something it was handed.
    pub sidecars_declined: u64,
    pub sidecars_adopted: u64,
    /// Frame kinds this build does not know, skipped by their length.
    pub frames_skipped: u64,
}

/// A buffered frame reader. `decode` works on a slice and says "not yet",
/// so this only has to keep reading until it stops saying that.
pub struct Reader<R> {
    src: R,
    buf: Vec<u8>,
    at: usize,
    eof: bool,
}

impl<R: Read> Reader<R> {
    pub fn new(src: R) -> Self {
        Reader {
            src,
            buf: Vec::new(),
            at: 0,
            eof: false,
        }
    }

    /// The underlying source, for a caller that needs to reconfigure it —
    /// a socket's read timeout, say, which a poll loop and a drain-to-EOF
    /// want set differently.
    pub fn get_ref(&self) -> &R {
        &self.src
    }

    fn fill(&mut self) -> anyhow::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        // Compact rather than grow without bound: a long stream would
        // otherwise keep every frame it has already handed out.
        if self.at > 0 && self.at == self.buf.len() {
            self.buf.clear();
            self.at = 0;
        } else if self.at > 1 << 20 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        let mut chunk = [0u8; 64 << 10];
        let n = self.src.read(&mut chunk).context("reading the stream")?;
        if n == 0 {
            self.eof = true;
            return Ok(false);
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(true)
    }

    pub fn next_frame(&mut self) -> anyhow::Result<Option<Framed>> {
        loop {
            match frame::decode(&self.buf[self.at..])? {
                Some((f, used)) => {
                    self.at += used;
                    return Ok(Some(f));
                }
                None => {
                    if !self.fill()? {
                        // A partial frame at EOF is a truncated stream, not
                        // an end: say so rather than presenting short data
                        // as complete.
                        if self.at < self.buf.len() {
                            bail!(
                                "stream ended mid-frame ({} trailing bytes) — truncated",
                                self.buf.len() - self.at
                            );
                        }
                        return Ok(None);
                    }
                }
            }
        }
    }

    pub fn read_hello(&mut self) -> anyhow::Result<()> {
        loop {
            match frame::decode_hello(&self.buf[self.at..])? {
                Some((_, used)) => {
                    self.at += used;
                    return Ok(());
                }
                None => {
                    if !self.fill()? {
                        bail!("stream ended before its hello — not a replication stream");
                    }
                }
            }
        }
    }
}

/// Consume a stream into `dest`, creating it if absent.
/// What the far end's `stream-open` said. Split out so a transport can
/// answer the handshake between the open and the chunks — a pipe cannot,
/// which is why `receive` exists as well.
#[derive(Debug, Clone)]
pub struct Opening {
    pub origin_id: [u8; 16],
    pub sender_id: [u8; 16],
    pub provenance: Vec<u8>,
    pub sidecars: Vec<crate::frame::Sidecar>,
}

impl Opening {
    /// What the stream says the store IS: its labels, and its name. Empty
    /// when it declared none.
    pub fn labels(&self) -> serde_json::Map<String, serde_json::Value> {
        if self.provenance.is_empty() {
            return serde_json::Map::new();
        }
        serde_json::from_slice(&self.provenance).unwrap_or_default()
    }

    /// The store's identity, as text. `None` for a pair that carries none
    /// on either side — which `identity` calls "not a store", and which
    /// cannot be replicated because there is nothing to key the
    /// destination on.
    pub fn store_id(&self) -> Option<String> {
        (self.origin_id != [0u8; 16]).then(|| frame::uuid_string(&self.origin_id))
    }
}

/// A destination being written by one stream. Holds the writer locks for
/// its whole life, so a second stream for the same store is refused by the
/// lock rather than by racing it.
pub struct Session {
    dir: PathBuf,
    name: String,
    st: crate::store::Store,
    cfg: crate::store::Config,
    adopt_pages: bool,
    out: Received,
    _dir_lock: std::fs::File,
    _file_lock: std::fs::File,
}

impl Session {
    /// Validate the opening against `dest` and take the locks. Refuses
    /// before creating anything, so a rejected stream leaves no trace.
    pub fn open(
        dest: &Path,
        open: &Opening,
        opts: &ReceiveOpts,
        cfg: &crate::store::Config,
    ) -> anyhow::Result<Session> {
        let (dir, name) = crate::query::resolve_backing(dest)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating backing directory {}", dir.display()))?;
        let existed = crate::format::rings_path(&dir, &name).exists();

        // One destination store, one origin. Checked here because this is
        // where an origin is claimed; without it a reinstall or a renamed
        // host silently appends a second tape to the first, and the
        // manifest then names only one of them.
        //
        // On `origin_id` alone, not on `id`: a store an operator
        // pre-created, or one an older build minted an identity for, is a
        // legitimate destination wearing a local name, and the origin is
        // the member that says whose tape it holds.
        if let Some(bark) = crate::bark::load(&dir, &name) {
            if let Some(held) = bark
                .get("origin_id")
                .and_then(|v| v.as_str())
                .and_then(frame::uuid_bytes)
            {
                if held != open.origin_id {
                    bail!(
                        "{}: holds origin {} and the stream carries {} — one store, one \
                         tape. Receive into a different store",
                        dest.display(),
                        frame::uuid_string(&held),
                        frame::uuid_string(&open.origin_id)
                    );
                }
            }
        }

        let dir_lock = match crate::store::lock_backing_shared(&dir)? {
            Some(l) => l,
            None => bail!(
                "backing directory {} is served by a timberfs mount; unmount first",
                dir.display()
            ),
        };
        let file_lock = match crate::store::lock_file_exclusive(&dir, &name)? {
            Some(f) => f,
            None => bail!("{name} already has a writer"),
        };

        // The manifest is written BEFORE the pair, so the identity is in
        // the rings header from the first byte: a pair whose two sides
        // disagree is refused by every writer, this one included.
        if !existed {
            seed_manifest(&dir, &name, open, opts)?;
        }
        let mut st = crate::store::Store {
            dir: dir.clone(),
            cfg: *cfg,
            files: std::collections::BTreeMap::new(),
        };
        st.create(&name)?;
        if existed {
            // A store the operator pre-created keeps its manifest — the
            // OTLP intake makes the same promise, and overwriting a
            // declaration is not a receiver's business. But the ORIGIN is
            // not provenance, it is the address: without it the numbering
            // is preserved and nothing records whose it is, so
            // `(origin_id, seq)` does not apply and the one-store-one-tape
            // guard above has nothing to compare against.
            //
            // Only into a store with no chunks, per the rule already
            // established for adopting a numbering: a store that has
            // written something has bytes an origin claim would
            // misattribute, and only a never-written one is eligible.
            let empty = st.files.get(&name).is_some_and(|f| f.chunks.is_empty());
            if empty {
                claim_origin(&dir, &name, open.origin_id, open.sender_id)?;
            }
        }

        // The sender's grain parameters, if offered: pages are adopted only
        // when the tokenizer matches, since a filter read under different
        // constants gives false negatives.
        let mut adopt_pages = false;
        let mut declined = 0u64;
        for s in &open.sidecars {
            if s.kind == crate::frame::Sidecar::tag(crate::serve::GRAIN_TAG)
                && crate::grain::header_matches(&s.bytes)
            {
                adopt_pages = crate::bark::index_declared(&dir, &name);
            } else {
                declined += 1;
            }
        }

        Ok(Session {
            out: Received {
                store: dest.to_path_buf(),
                created: !existed,
                chunks: 0,
                comp_bytes: 0,
                runs: Vec::new(),
                sidecars_declined: declined,
                sidecars_adopted: 0,
                frames_skipped: 0,
            },
            dir,
            name,
            st,
            cfg: *cfg,
            adopt_pages,
            _dir_lock: dir_lock,
            _file_lock: file_lock,
        })
    }

    /// Where this session is writing. The transport needs it to answer
    /// the handshake with a handle of its own.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// What this destination holds now — the ack, and a coverage answer.
    pub fn coverage(&self) -> Vec<Run> {
        let file = self.st.files.get(&self.name).expect("created in open");
        crate::serve::runs_of(file.chunks.iter().map(|c| c.seq))
    }

    /// Apply one frame. Returns false for a frame that ends the stream
    /// (there is none yet) so a transport loop reads uniformly.
    pub fn apply(&mut self, f: Frame) -> anyhow::Result<()> {
        match f {
            Frame::Chunk {
                seq,
                uncomp_len,
                comp_len,
                comp,
                first_write_ms,
                last_write_ms,
                sidecars,
            } => {
                let Some(bytes) = comp else {
                    bail!(
                        "chunk {seq} arrived without its payload — an index-mode \
                         stream describes a store, it cannot build one"
                    );
                };
                if bytes.len() as u64 != comp_len {
                    bail!(
                        "chunk {seq} declared {comp_len} bytes and carried {}",
                        bytes.len()
                    );
                }
                let cfg = self.cfg;
                let file = self.st.files.get_mut(&self.name).expect("created in open");
                file.append_wire_frame(
                    &bytes,
                    uncomp_len,
                    first_write_ms,
                    last_write_ms,
                    Some(seq),
                    &cfg,
                )
                .with_context(|| format!("appending chunk {seq} to {}", self.name))?;
                self.out.chunks += 1;
                self.out.comp_bytes += comp_len;
                for s in &sidecars {
                    if self.adopt_pages
                        && s.kind == crate::frame::Sidecar::tag(crate::serve::GRAIN_TAG)
                    {
                        crate::grain::append_page(&self.dir, &self.name, &s.bytes)?;
                        self.out.sidecars_adopted += 1;
                    } else {
                        self.out.sidecars_declined += 1;
                    }
                }
            }
            // Coverage from the far end is information, not instruction.
            Frame::Coverage { .. } => {}
            Frame::Unknown { .. } => self.out.frames_skipped += 1,
            other => bail!("unexpected {other:?} in a replication stream"),
        }
        Ok(())
    }

    pub fn finish(mut self) -> anyhow::Result<Received> {
        self.out.runs = self.coverage();
        let (dir, name) = (self.dir.clone(), self.name.clone());
        let adopted = self.out.sidecars_adopted;
        let chunks = self.out.chunks;
        let out = std::mem::replace(
            &mut self.out,
            Received {
                store: PathBuf::new(),
                created: false,
                chunks: 0,
                comp_bytes: 0,
                runs: Vec::new(),
                sidecars_declined: 0,
                sidecars_adopted: 0,
                frames_skipped: 0,
            },
        );
        drop(self);
        // A grain declared but not adopted is rebuilt rather than left
        // short, so the destination's index is whole either way.
        if crate::bark::index_declared(&dir, &name) && adopted == 0 && chunks > 0 {
            crate::grain::extend_grain(&dir, &name)?;
        }
        Ok(out)
    }
}

/// Consume a whole stream from `src` into `dest` — the pipe case, with no
/// handshake. A transport drives a `Session` directly instead.
pub fn receive(
    dest: &Path,
    src: impl Read,
    opts: &ReceiveOpts,
    cfg: &crate::store::Config,
) -> anyhow::Result<Received> {
    let mut r = Reader::new(src);
    r.read_hello()?;
    let (open, _) = read_opening(&mut r)?;
    let mut session = Session::open(dest, &open, opts, cfg)?;
    while let Some(f) = r.next_frame()? {
        session.apply(f.frame)?;
    }
    session.finish()
}

/// Read the stream's opening frame — the pipe case, where the first frame
/// after the hello is the only stream there is.
pub fn read_opening<R: Read>(r: &mut Reader<R>) -> anyhow::Result<(Opening, u32)> {
    let Some(first) = r.next_frame()? else {
        bail!("stream carried a hello and nothing else");
    };
    let stream = first.stream;
    Ok((opening_of(first.frame)?, stream))
}

/// What a `stream-open` said, as the thing a session is opened from. A
/// transport that multiplexes meets one at any point in the connection,
/// not only first.
pub fn opening_of(f: Frame) -> anyhow::Result<Opening> {
    match f {
        Frame::StreamOpen {
            origin_id,
            sender_id,
            provenance,
            sidecars,
            ..
        } => Ok(Opening {
            origin_id,
            sender_id,
            provenance,
            sidecars,
        }),
        other => bail!("a stream must open with stream-open, not {other:?}"),
    }
}

/// Record the origin on a store that already had a manifest but no origin,
/// leaving everything the operator declared alone. Absent keys only: this
/// adds an address, it does not restate provenance.
fn claim_origin(
    dir: &Path,
    name: &str,
    origin_id: [u8; 16],
    sender_id: [u8; 16],
) -> anyhow::Result<()> {
    if origin_id == [0u8; 16] {
        return Ok(());
    }
    let mut map = crate::bark::load(dir, name).unwrap_or_default();
    if map.contains_key("origin_id") {
        return Ok(());
    }
    map.insert(
        "origin_id".to_string(),
        serde_json::Value::String(frame::uuid_string(&origin_id)),
    );
    map.entry("derived_op")
        .or_insert_with(|| serde_json::Value::String("receive".to_string()));
    if sender_id != [0u8; 16] {
        map.entry("derived_from")
            .or_insert_with(|| serde_json::Value::String(frame::uuid_string(&sender_id)));
    }
    crate::bark::save(dir, name, &map)?;
    Ok(())
}

/// The destination's manifest: the store's identity and labels, which
/// travel, and nothing of the pair's, which does not.
///
/// No `derived_from`: a replica derives from nothing, it IS the store. The
/// `created` stamp is minted here rather than inherited — it answers since
/// when THIS host has held the store, which is what `--follow-from
/// discovery` compares a follower's declaration against.
fn seed_manifest(dir: &Path, name: &str, open: &Opening, opts: &ReceiveOpts) -> anyhow::Result<()> {
    let mut map: serde_json::Map<String, serde_json::Value> = if open.provenance.is_empty() {
        serde_json::Map::new()
    } else {
        serde_json::from_slice(&open.provenance).context("reading the stream's labels")?
    };
    map.insert(
        "derived_op".to_string(),
        serde_json::Value::String("receive".to_string()),
    );
    if let Some(id) = open.store_id() {
        // The identity travels, so the destination adopts it rather than
        // minting one — a second identity would make one tape answer to
        // two names, and every position keyed on it is keyed on the wrong
        // one. `origin_id` says the same thing for a reader that knows
        // only the older spelling.
        map.insert("id".to_string(), serde_json::Value::String(id.clone()));
        map.insert("origin_id".to_string(), serde_json::Value::String(id));
    }
    crate::bark::save(dir, name, &map)?;
    if opts.index {
        crate::bark::declare_index(dir, name)?;
    }
    if opts.wal {
        crate::bark::declare_wal(dir, name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Mode;
    use crate::serve::{self, Request};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("timberfs-recv-test-{}-{n}", std::process::id()));
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

    fn cfg() -> crate::store::Config {
        crate::store::Config {
            chunk_size: 1 << 20,
            level: 1,
            flush_age_ms: u64::MAX,
        }
    }

    /// A source store with `chunks` one-line chunks and the given labels.
    fn a_store(dir: &Path, name: &str, chunks: usize, index: bool) -> PathBuf {
        let path = dir.join(format!("{name}.log"));
        crate::bark::cmd_create(&path, index, false, None, None, false, &[], false).unwrap();
        crate::bark::cmd_set(&path, &["host=apache01".into(), "service=err".into()], &[]).unwrap();
        let logical = format!("{name}.log");
        let mut st = crate::store::Store {
            dir: dir.to_path_buf(),
            cfg: cfg(),
            files: std::collections::BTreeMap::new(),
        };
        st.create(&logical).unwrap();
        let f = st.files.get_mut(&logical).unwrap();
        for i in 0..chunks {
            f.append_windowed(
                format!("2026-06-01T10:00:0{i}Z line {i} padding padding\n").as_bytes(),
                1_000 + i as u64,
                1_000 + i as u64,
                &cfg(),
            )
            .unwrap();
            f.flush_chunk(&cfg()).unwrap();
        }
        drop(st);
        if index {
            let (d, n) = crate::query::resolve_backing(&path).unwrap();
            crate::grain::extend_grain(&d, &n).unwrap();
        }
        path
    }

    fn wire(src: &Path, mode: Mode) -> Vec<u8> {
        let mut buf = frame::encode_hello(frame::Hello {
            version: frame::VERSION,
            incompat: 0,
        })
        .to_vec();
        serve::serve(src, &Request::everything(mode), &mut buf).unwrap();
        buf
    }

    #[test]
    fn a_replica_is_byte_identical_and_keeps_the_numbering() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 5, false);
        let bytes = wire(&src, Mode::Frames);
        let dst = d.path().join("dst.log");
        let opts = ReceiveOpts::default();
        let got = receive(&dst, &bytes[..], &opts, &cfg()).unwrap();

        assert!(got.created);
        assert_eq!(got.chunks, 5);
        assert_eq!(got.runs, vec![Run { start: 0, end: 4 }]);

        // The trunk is the same bytes, so nothing was recompressed...
        let (sd, sn) = crate::query::resolve_backing(&src).unwrap();
        let (dd, dn) = crate::query::resolve_backing(&dst).unwrap();
        assert_eq!(
            std::fs::read(crate::format::trunk_path(&sd, &sn)).unwrap(),
            std::fs::read(crate::format::trunk_path(&dd, &dn)).unwrap(),
        );
        // ...and every chunk answers to the same number on both ends,
        // which is what makes (origin_id, seq) an address.
        let s = crate::format::read_index(&crate::format::rings_path(&sd, &sn)).unwrap();
        let t = crate::format::read_index(&crate::format::rings_path(&dd, &dn)).unwrap();
        assert_eq!(
            s.iter().map(|c| c.seq).collect::<Vec<_>>(),
            t.iter().map(|c| c.seq).collect::<Vec<_>>()
        );
        for (a, b) in s.iter().zip(&t) {
            assert_eq!(
                (a.uncomp_len, a.first_write_ms, a.last_write_ms),
                (b.uncomp_len, b.first_write_ms, b.last_write_ms)
            );
        }
    }

    /// The reversal: a position, a chunk address and a tape offset are
    /// each keyed by identity, so a replica minting a second one would
    /// make a single tape answer to two names.
    #[test]
    fn the_identity_travels_and_the_replica_is_the_same_store() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 2, false);
        let bytes = wire(&src, Mode::Frames);
        let dst = d.path().join("dst.log");
        receive(
            &dst,
            &bytes[..],
            &ReceiveOpts {
                index: true,
                ..Default::default()
            },
            &cfg(),
        )
        .unwrap();

        let sb = crate::bark::load(d.path(), "src.log").unwrap();
        let db = crate::bark::load(d.path(), "dst.log").unwrap();
        let sid = sb.get("id").unwrap().as_str().unwrap();
        // The identity and the labels travelled; so did the name, which
        // nothing at this end could reconstruct.
        assert_eq!(db.get("host").unwrap(), "apache01");
        assert_eq!(db.get("id").unwrap().as_str().unwrap(), sid);
        assert_eq!(db.get("origin_id").unwrap(), sid);
        // The EFFECTIVE name: the source declares none, so its path is
        // all the name it has, and the replica's path is a uuid.
        assert_eq!(db.get("name").unwrap(), "src");
        // A replica derives from nothing: it IS the store.
        assert!(!db.contains_key("derived_from"), "{db:?}");
        assert_eq!(db.get("derived_op").unwrap(), "receive");
        // The receiver's own policy, which the sender never sent.
        assert_eq!(db.get("index").unwrap(), true);
        // And the pair says so too, so a lost manifest keeps the identity.
        let (dd, dn) = crate::query::resolve_backing(&dst).unwrap();
        assert_eq!(
            crate::bark::carried_identity(&dd, &dn).as_deref(),
            Some(sid)
        );
    }

    /// A replica IS the store, so the pair's own facts are the only ones
    /// minted here — and `created` is one of them, answering since when
    /// THIS host has held it rather than when the identity was made.
    #[test]
    fn the_pairs_own_facts_are_not_inherited() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 2, false);
        let mut sb = crate::bark::load(d.path(), "src.log").unwrap();
        sb.insert("created".into(), serde_json::json!("2019-01-01T00:00:00Z"));
        crate::bark::save(d.path(), "src.log", &sb).unwrap();

        let dst = d.path().join("dst.log");
        receive(
            &dst,
            &wire(&src, Mode::Frames)[..],
            &ReceiveOpts::default(),
            &cfg(),
        )
        .unwrap();
        let db = crate::bark::load(d.path(), "dst.log").unwrap();
        assert_ne!(
            db.get("created").unwrap(),
            "2019-01-01T00:00:00Z",
            "created is the pair's, not the identity's"
        );
        // Settings are the destination's own, so nothing declared here.
        assert!(!db.contains_key("index"), "{db:?}");
        assert!(!db.contains_key("wal"), "{db:?}");
    }

    #[test]
    fn a_pre_created_store_still_gets_the_origin_recorded() {
        // `--replica` preserved the numbering but recorded no origin when
        // the operator had pre-created the destination, so the address did
        // not apply and the one-store-one-origin guard had nothing to
        // compare. The manifest the operator wrote is otherwise untouched.
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 3, false);
        let dst = d.path().join("declared.log");
        crate::bark::cmd_create(&dst, false, true, None, None, false, &[], false).unwrap();
        let before = crate::bark::load(d.path(), "declared.log").unwrap();

        let opts = ReceiveOpts::default();
        receive(&dst, &wire(&src, Mode::Frames)[..], &opts, &cfg()).unwrap();

        let sb = crate::bark::load(d.path(), "src.log").unwrap();
        let db = crate::bark::load(d.path(), "declared.log").unwrap();
        assert_eq!(db.get("origin_id").unwrap(), sb.get("id").unwrap());
        // The operator's own declarations survive, id included.
        assert_eq!(db.get("id"), before.get("id"));
        assert_eq!(db.get("wal"), before.get("wal"));
        // Labels are NOT added: a pre-created manifest is the operator's,
        // which is the promise the OTLP intake makes too.
        assert!(!db.contains_key("service"), "{db:?}");

        // And the guard now engages, which was the point.
        let other = a_store(d.path(), "other", 1, false);
        let err = receive(&dst, &wire(&other, Mode::Frames)[..], &opts, &cfg())
            .expect_err("a second origin must be refused");
        assert!(format!("{err:#}").contains("one store"), "{err:#}");
    }

    #[test]
    fn a_store_that_already_holds_chunks_is_not_claimed() {
        // Only a never-written store may adopt an origin: one that has
        // written something has bytes an origin claim would misattribute.
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 2, false);
        let dst = a_store(d.path(), "used", 2, false);
        let opts = ReceiveOpts::default();
        // Its numbering is at 2, and the stream starts at 0, so the frames
        // are refused anyway -- but the manifest must not have been
        // claimed on the way to that refusal.
        let _ = receive(&dst, &wire(&src, Mode::Frames)[..], &opts, &cfg());
        let db = crate::bark::load(d.path(), "used.log").unwrap();
        assert!(!db.contains_key("origin_id"), "{db:?}");
    }

    #[test]
    fn one_store_one_origin() {
        // A second origin into a store that already claims one is refused,
        // which is the reinstall and the renamed-host case both.
        let d = TempDir::new();
        let a = a_store(d.path(), "a", 2, false);
        let dst = d.path().join("dst.log");
        let opts = ReceiveOpts::default();
        receive(&dst, &wire(&a, Mode::Frames)[..], &opts, &cfg()).unwrap();

        let b = a_store(d.path(), "b", 2, false);
        let err = receive(&dst, &wire(&b, Mode::Frames)[..], &opts, &cfg())
            .expect_err("a different origin must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("one store"), "{msg}");
    }

    #[test]
    fn a_preserved_number_must_continue_the_numbering_exactly() {
        // No numbering base exists, so a stream that starts mid-tape
        // cannot be received as a replica -- and says why rather than
        // leaving a hole under a preserved number.
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 6, false);
        let mut bytes = frame::encode_hello(frame::Hello {
            version: frame::VERSION,
            incompat: 0,
        })
        .to_vec();
        let req = Request {
            stream: 0,
            mode: Mode::Frames,
            first_seq: 3,
            last_seq: frame::OPEN_ENDED,
            sidecars: false,
            max_chunks: None,
        };
        serve::serve(&src, &req, &mut bytes).unwrap();
        let dst = d.path().join("mid.log");
        let err = receive(&dst, &bytes[..], &ReceiveOpts::default(), &cfg())
            .expect_err("a fresh store cannot start at chunk 3");
        assert!(
            format!("{err:#}").contains("continue it exactly"),
            "{err:#}"
        );
    }

    #[test]
    fn resuming_appends_where_the_destination_left_off() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 3, false);
        let dst = d.path().join("dst.log");
        let opts = ReceiveOpts::default();
        receive(&dst, &wire(&src, Mode::Frames)[..], &opts, &cfg()).unwrap();

        // The source grows; ship only what the destination lacks.
        let logical = "src.log".to_string();
        let mut st = crate::store::Store {
            dir: d.path().to_path_buf(),
            cfg: cfg(),
            files: std::collections::BTreeMap::new(),
        };
        st.create(&logical).unwrap();
        let f = st.files.get_mut(&logical).unwrap();
        for i in 3..5 {
            f.append_windowed(
                format!("2026-06-01T10:00:0{i}Z line {i} more\n").as_bytes(),
                1_000 + i as u64,
                1_000 + i as u64,
                &cfg(),
            )
            .unwrap();
            f.flush_chunk(&cfg()).unwrap();
        }
        drop(st);

        let mut bytes = frame::encode_hello(frame::Hello {
            version: frame::VERSION,
            incompat: 0,
        })
        .to_vec();
        let req = Request {
            stream: 0,
            mode: Mode::Frames,
            first_seq: 3,
            last_seq: frame::OPEN_ENDED,
            sidecars: false,
            max_chunks: None,
        };
        serve::serve(&src, &req, &mut bytes).unwrap();
        let got = receive(&dst, &bytes[..], &opts, &cfg()).unwrap();
        assert!(!got.created);
        assert_eq!(got.chunks, 2);
        assert_eq!(got.runs, vec![Run { start: 0, end: 4 }], "one run, resumed");
    }

    #[test]
    fn grain_pages_are_adopted_instead_of_re_tokenized() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 4, true);
        let bytes = wire(&src, Mode::Frames);
        let dst = d.path().join("dst.log");
        let got = receive(
            &dst,
            &bytes[..],
            &ReceiveOpts {
                index: true,
                ..Default::default()
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(got.sidecars_adopted, 4, "one page per chunk");
        assert_eq!(got.sidecars_declined, 0);
        // The adopted index answers the same questions as the source's.
        let (sd, sn) = crate::query::resolve_backing(&src).unwrap();
        let (dd, dn) = crate::query::resolve_backing(&dst).unwrap();
        assert_eq!(
            std::fs::read(crate::format::grain_path(&sd, &sn)).unwrap(),
            std::fs::read(crate::format::grain_path(&dd, &dn)).unwrap(),
        );
    }

    #[test]
    fn an_index_mode_stream_cannot_build_a_store() {
        // It describes what a peer holds; there are no bytes in it.
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 2, false);
        let bytes = wire(&src, Mode::Index);
        let dst = d.path().join("dst.log");
        let err = receive(&dst, &bytes[..], &ReceiveOpts::default(), &cfg())
            .expect_err("index mode carries no payload");
        assert!(
            format!("{err:#}").contains("without its payload"),
            "{err:#}"
        );
    }

    #[test]
    fn a_truncated_stream_is_refused_rather_than_accepted_short() {
        let d = TempDir::new();
        let src = a_store(d.path(), "src", 4, false);
        let bytes = wire(&src, Mode::Frames);
        let dst = d.path().join("dst.log");
        let cut = &bytes[..bytes.len() - 10];
        let err = receive(&dst, cut, &ReceiveOpts::default(), &cfg())
            .expect_err("a partial frame at EOF is not an end");
        assert!(format!("{err:#}").contains("truncated"), "{err:#}");
    }

    #[test]
    fn the_replica_answers_queries_identically_to_its_origin() {
        // The end the whole wire exists for: a store shipped verbatim is
        // the same log on the far side, greppable by the same index.
        let d = TempDir::new();
        let src = d.path().join("big.log");
        crate::bark::cmd_create(&src, true, false, None, None, false, &[], false).unwrap();
        let mut st = crate::store::Store {
            dir: d.path().to_path_buf(),
            cfg: cfg(),
            files: std::collections::BTreeMap::new(),
        };
        st.create("big.log").unwrap();
        let scfg = st.cfg;
        let f = st.files.get_mut("big.log").unwrap();
        // Ten chunks: flushed in batches, since one append plus one flush
        // is one chunk however large it is.
        for batch in 0..10u64 {
            let mut lines = String::new();
            for i in batch * 2_000..(batch + 1) * 2_000 {
                lines.push_str(&format!(
                    "2026-06-01T10:00:00Z req-{i:06} status={} payload\n",
                    if i % 997 == 0 { 500 } else { 200 }
                ));
            }
            f.append_windowed(lines.as_bytes(), 1_000 + batch, 1_000 + batch, &scfg)
                .unwrap();
            f.flush_chunk(&scfg).unwrap();
        }
        drop(st);
        let (sd, sn) = crate::query::resolve_backing(&src).unwrap();
        crate::grain::extend_grain(&sd, &sn).unwrap();

        let bytes = wire(&src, Mode::Frames);
        let dst = d.path().join("replica.log");
        let got = receive(
            &dst,
            &bytes[..],
            &ReceiveOpts {
                index: true,
                ..Default::default()
            },
            &cfg(),
        )
        .unwrap();
        assert!(got.chunks > 1, "several chunks, {got:?}");

        // Same bytes, same numbering, same index, same answers.
        let (dd, dn) = crate::query::resolve_backing(&dst).unwrap();
        for ext in ["trunk", "grain"] {
            let a = d.path().join(format!("{sn}.{ext}"));
            let b = d.path().join(format!("{dn}.{ext}"));
            assert_eq!(
                std::fs::read(&a).unwrap(),
                std::fs::read(&b).unwrap(),
                "{ext}"
            );
        }
        let _ = (sd, dd);
        // And it reads back as the same log: the trunk is a concatenation
        // of independent frames, so decompressing the whole file is what a
        // reader without timberfs would do.
        let text = |p: &Path| {
            let (d2, n2) = crate::query::resolve_backing(p).unwrap();
            let bytes = std::fs::read(crate::format::trunk_path(&d2, &n2)).unwrap();
            String::from_utf8(zstd::stream::decode_all(&bytes[..]).unwrap()).unwrap()
        };
        let a = text(&src);
        assert_eq!(a, text(&dst), "the replica reads back identically");
        assert_eq!(a.lines().count(), 20_000);
    }

    #[test]
    fn a_stream_without_a_hello_is_not_a_replication_stream() {
        let d = TempDir::new();
        let dst = d.path().join("dst.log");
        let err = receive(
            &dst,
            &b"garbage bytes___"[..],
            &ReceiveOpts::default(),
            &cfg(),
        )
        .expect_err("no magic");
        assert!(
            format!("{err:#}").contains("not a timberfs replication stream"),
            "{err:#}"
        );
    }
}
