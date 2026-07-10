//! The FUSE layer: presents the decompressed, logical view of the backing
//! store as a flat directory of append-only files.
//!
//! Semantics:
//!   - writes must be at EOF (append-only); anything else gets EPERM
//!   - truncate to 0 is allowed and starts the file over (copytruncate
//!     rotation); truncate to any other size gets EPERM
//!   - rename/unlink work (normal mv-based log rotation)
//!   - close() flushes the buffer into a chunk; fsync() also syncs the
//!     backing files, so fsync through the mount means durable on disk
//!   - reads are served with FOPEN_DIRECT_IO so tail -f style polling
//!     always sees freshly appended data instead of stale page cache
//!
//! The write-time index is exposed per file through xattrs
//! (user.timberfs.first_write, user.timberfs.last_write, user.timberfs.chunks,
//! user.timberfs.compressed_size); the full index and time-range extraction
//! are available via `timberfs index` / `timberfs query` against the backing
//! files, which works whether or not the filesystem is mounted.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::consts::FOPEN_DIRECT_IO;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
};

use crate::query::fmt_ms_rfc3339;
use crate::store::{now_ms, Store};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: u64 = 1;

const XATTR_NAMES: &[&str] = &[
    "user.timberfs.chunks",
    "user.timberfs.compressed_size",
    "user.timberfs.first_write",
    "user.timberfs.last_write",
];

fn ms_to_systemtime(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

fn io_errno(e: &std::io::Error) -> i32 {
    if let Some(errno) = e.raw_os_error() {
        return errno;
    }
    match e.kind() {
        std::io::ErrorKind::NotFound => libc::ENOENT,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => libc::EINVAL,
        std::io::ErrorKind::PermissionDenied => libc::EPERM,
        _ => libc::EIO,
    }
}

/// A rotation request arriving through the setxattr control channel:
/// "cutoff=<unix_ms>;target=<name>" or "cutoff=<unix_ms>;delete".
struct RotateReq {
    cutoff_ms: u64,
    target: Option<String>,
}

fn parse_rotate_request(s: &str) -> Option<RotateReq> {
    let mut cutoff = None;
    let mut target = None;
    let mut delete = false;
    for part in s.split(';') {
        if let Some(v) = part.strip_prefix("cutoff=") {
            cutoff = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("target=") {
            if v.is_empty() || v.contains('/') {
                return None;
            }
            target = Some(v.to_string());
        } else if part == "delete" {
            delete = true;
        } else if !part.is_empty() {
            return None;
        }
    }
    let cutoff_ms = cutoff?;
    if delete == target.is_some() {
        return None;
    }
    Some(RotateReq { cutoff_ms, target })
}

pub struct TimberFs {
    store: Arc<Mutex<Store>>,
    /// Filled in after the FUSE session is created; used to invalidate the
    /// kernel's cached attributes when a rotation shrinks a file behind
    /// the kernel's back (else O_APPEND writers compute stale offsets).
    notifier: Arc<Mutex<Option<fuser::Notifier>>>,
    ino_to_name: HashMap<u64, String>,
    name_to_ino: HashMap<String, u64>,
    next_ino: u64,
    uid: u32,
    gid: u32,
}

impl TimberFs {
    fn new(store: Arc<Mutex<Store>>, notifier: Arc<Mutex<Option<fuser::Notifier>>>) -> TimberFs {
        let names: Vec<String> = store.lock().unwrap().files.keys().cloned().collect();
        let mut fs = TimberFs {
            store,
            notifier,
            ino_to_name: HashMap::new(),
            name_to_ino: HashMap::new(),
            next_ino: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        };
        for name in names {
            fs.assign_ino(&name);
        }
        fs
    }

    fn assign_ino(&mut self, name: &str) -> u64 {
        if let Some(ino) = self.name_to_ino.get(name) {
            return *ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.ino_to_name.insert(ino, name.to_string());
        self.name_to_ino.insert(name.to_string(), ino);
        ino
    }

    fn name_of(&self, ino: u64) -> Option<String> {
        self.ino_to_name.get(&ino).cloned()
    }

    fn root_attr(&self) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino: ROOT_INO,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn file_attr(&self, ino: u64, name: &str) -> Option<FileAttr> {
        let store = self.store.lock().unwrap();
        let f = store.files.get(name)?;
        let mtime = ms_to_systemtime(f.last_write_ms().unwrap_or_else(now_ms));
        Some(FileAttr {
            ino,
            size: f.size(),
            // Report compressed usage so du(1) shows the real disk footprint
            // while ls -l shows the logical size.
            blocks: f.comp_size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }

    fn flush_file(&mut self, ino: u64, sync: bool, reply: ReplyEmpty) {
        let name = match self.name_of(ino) {
            Some(n) => n,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let mut store = self.store.lock().unwrap();
        let cfg = store.cfg;
        let f = match store.files.get_mut(&name) {
            Some(f) => f,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let res = if sync {
            f.sync(&cfg)
        } else {
            f.flush_chunk(&cfg)
        };
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(io_errno(&e)),
        }
    }
}

impl Filesystem for TimberFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        if parent != ROOT_INO {
            reply.error(libc::ENOENT);
            return;
        }
        let name = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        match self.name_to_ino.get(name).copied() {
            Some(ino) => match self.file_attr(ino, name) {
                Some(attr) => reply.entry(&TTL, &attr, 0),
                None => reply.error(libc::ENOENT),
            },
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == ROOT_INO {
            reply.attr(&TTL, &self.root_attr());
            return;
        }
        let name = match self.name_of(ino) {
            Some(n) => n,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        match self.file_attr(ino, &name) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(libc::ENOENT),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if ino == ROOT_INO {
            reply.attr(&TTL, &self.root_attr());
            return;
        }
        let name = match self.name_of(ino) {
            Some(n) => n,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        if let Some(sz) = size {
            let mut store = self.store.lock().unwrap();
            let dir = store.dir.clone();
            let f = match store.files.get_mut(&name) {
                Some(f) => f,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };
            if sz == 0 && f.size() > 0 {
                if let Err(e) = f.reset(&dir, &name) {
                    reply.error(io_errno(&e));
                    return;
                }
            } else if sz != f.size() {
                // Append-only: no truncating to arbitrary sizes.
                reply.error(libc::EPERM);
                return;
            }
        }
        match self.file_attr(ino, &name) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if ino != ROOT_INO {
            reply.error(libc::ENOTDIR);
            return;
        }
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ROOT_INO, FileType::Directory, ".".to_string()),
            (ROOT_INO, FileType::Directory, "..".to_string()),
        ];
        let mut names: Vec<(String, u64)> = self
            .name_to_ino
            .iter()
            .map(|(n, i)| (n.clone(), *i))
            .collect();
        names.sort();
        for (name, ino) in names {
            entries.push((ino, FileType::RegularFile, name));
        }
        for (idx, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*ino, (idx + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        if self.ino_to_name.contains_key(&ino) {
            reply.opened(0, FOPEN_DIRECT_IO);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if parent != ROOT_INO {
            reply.error(libc::EPERM);
            return;
        }
        let name = match name.to_str() {
            Some(n) => n.to_string(),
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        {
            let mut store = self.store.lock().unwrap();
            if let Err(e) = store.create(&name) {
                reply.error(io_errno(&e));
                return;
            }
        }
        let ino = self.assign_ino(&name);
        match self.file_attr(ino, &name) {
            Some(attr) => reply.created(&TTL, &attr, 0, 0, FOPEN_DIRECT_IO),
            None => reply.error(libc::EIO),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let name = match self.name_of(ino) {
            Some(n) => n,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let mut store = self.store.lock().unwrap();
        let f = match store.files.get_mut(&name) {
            Some(f) => f,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        match f.read(offset as u64, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        write_flags: u32,
        flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let name = match self.name_of(ino) {
            Some(n) => n,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let mut store = self.store.lock().unwrap();
        let cfg = store.cfg;
        let f = match store.files.get_mut(&name) {
            Some(f) => f,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        // For O_APPEND handles the kernel-computed offset is advisory (and
        // can be stale right after a rotation shrank the file) — POSIX says
        // the write lands at the current EOF, so just append. Everyone else
        // must write exactly at EOF: the filesystem is append-only.
        let append_handle = flags & libc::O_APPEND != 0;
        if !append_handle && (offset < 0 || offset as u64 != f.size()) {
            eprintln!(
                "timberfs: {name}: rejecting non-EOF write: offset={offset} size={} flags={flags:#x} write_flags={write_flags:#x}",
                f.size()
            );
            reply.error(libc::EPERM);
            return;
        }
        match f.append(data, &cfg) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        self.flush_file(ino, false, reply);
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.flush_file(ino, true, reply);
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.flush_file(ino, false, reply);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if parent != ROOT_INO {
            reply.error(libc::ENOENT);
            return;
        }
        let name = match name.to_str() {
            Some(n) => n.to_string(),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let res = self.store.lock().unwrap().remove(&name);
        match res {
            Ok(()) => {
                if let Some(ino) = self.name_to_ino.remove(&name) {
                    self.ino_to_name.remove(&ino);
                }
                reply.ok();
            }
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        if parent != ROOT_INO || newparent != ROOT_INO {
            reply.error(libc::EPERM);
            return;
        }
        let (old, new) = match (name.to_str(), newname.to_str()) {
            (Some(o), Some(n)) => (o.to_string(), n.to_string()),
            _ => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let res = self.store.lock().unwrap().rename(&old, &new);
        match res {
            Ok(()) => {
                if let Some(target_ino) = self.name_to_ino.remove(&new) {
                    self.ino_to_name.remove(&target_ino);
                }
                if let Some(ino) = self.name_to_ino.remove(&old) {
                    self.ino_to_name.insert(ino, new.clone());
                    self.name_to_ino.insert(new, ino);
                }
                reply.ok();
            }
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    /// setxattr doubles as the control channel: writing
    /// user.timberfs.rotate = "cutoff=<ms>;target=<name>" (or ";delete")
    /// performs an online rotation atomically under the store lock. This is
    /// what `timberfs rotate` uses when the backing directory is mounted.
    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let fname = match self.name_of(ino) {
            Some(n) => n,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        if name.to_str() != Some("user.timberfs.rotate") {
            reply.error(libc::ENOTSUP);
            return;
        }
        let req = match std::str::from_utf8(value)
            .ok()
            .and_then(parse_rotate_request)
        {
            Some(r) => r,
            None => {
                eprintln!("timberfs: {fname}: malformed rotate request");
                reply.error(libc::EINVAL);
                return;
            }
        };
        let res =
            self.store
                .lock()
                .unwrap()
                .rotate_head(&fname, req.target.as_deref(), req.cutoff_ms);
        match res {
            Ok(stats) => {
                if let Some(t) = &req.target {
                    self.assign_ino(t);
                }
                eprintln!(
                    "timberfs: {fname}: rotated {} chunk(s) ({} compressed bytes) {}",
                    stats.chunks_moved,
                    stats.comp_bytes,
                    match &req.target {
                        Some(t) => format!("into {t}"),
                        None => "deleted".to_string(),
                    }
                );
                reply.ok();
                if stats.chunks_moved > 0 {
                    // The file just shrank behind the kernel's back; drop
                    // its cached attributes so O_APPEND writers holding the
                    // file open recompute their offset from the new size.
                    if let Some(n) = self.notifier.lock().unwrap().as_ref() {
                        if let Err(e) = n.inval_inode(ino, 0, 0) {
                            eprintln!("timberfs: {fname}: inode invalidation failed: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("timberfs: {fname}: rotate failed: {e}");
                reply.error(io_errno(&e));
            }
        }
    }

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let fname = match self.name_of(ino) {
            Some(n) => n,
            None => {
                reply.error(libc::ENODATA);
                return;
            }
        };
        let key = match name.to_str() {
            Some(k) => k,
            None => {
                reply.error(libc::ENODATA);
                return;
            }
        };
        let store = self.store.lock().unwrap();
        let f = match store.files.get(&fname) {
            Some(f) => f,
            None => {
                reply.error(libc::ENODATA);
                return;
            }
        };
        let value: Option<String> = match key {
            "user.timberfs.chunks" => Some(f.chunks.len().to_string()),
            "user.timberfs.compressed_size" => Some(f.comp_size.to_string()),
            "user.timberfs.first_write" => f.first_write_ms().map(fmt_ms_rfc3339),
            "user.timberfs.last_write" => f.last_write_ms().map(fmt_ms_rfc3339),
            _ => None,
        };
        match value {
            Some(v) => {
                let bytes = v.into_bytes();
                if size == 0 {
                    reply.size(bytes.len() as u32);
                } else if size >= bytes.len() as u32 {
                    reply.data(&bytes);
                } else {
                    reply.error(libc::ERANGE);
                }
            }
            None => reply.error(libc::ENODATA),
        }
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: ReplyXattr) {
        if !self.ino_to_name.contains_key(&ino) {
            if size == 0 {
                reply.size(0);
            } else {
                reply.data(&[]);
            }
            return;
        }
        let mut buf = Vec::new();
        for key in XATTR_NAMES {
            buf.extend_from_slice(key.as_bytes());
            buf.push(0);
        }
        if size == 0 {
            reply.size(buf.len() as u32);
        } else if size >= buf.len() as u32 {
            reply.data(&buf);
        } else {
            reply.error(libc::ERANGE);
        }
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let files = self.name_to_ino.len() as u64;
        reply.statfs(1 << 30, 1 << 29, 1 << 29, files, 1 << 20, 4096, 255, 4096);
    }
}

pub fn mount(store: Store, mountpoint: &Path, allow_other: bool) -> anyhow::Result<()> {
    let mountpoint = std::fs::canonicalize(mountpoint)?;
    let mountpoint = mountpoint.as_path();
    // Claim the backing dir exclusively (a mount owns in-memory state for
    // every file in it) and advertise our mountpoint so `timberfs rotate`
    // can route requests through the mount.
    let lock = match crate::store::lock_backing_exclusive(&store.dir)? {
        Some(f) => f,
        None => {
            let appenders = crate::store::active_file_locks(&store.dir);
            if appenders.is_empty() {
                anyhow::bail!(
                    "backing directory {} is already in use by another timberfs process{}",
                    store.dir.display(),
                    crate::store::read_lock_raw(&store.dir)
                        .map(|s| format!(
                            " ({})",
                            s.split_whitespace().collect::<Vec<_>>().join(", ")
                        ))
                        .unwrap_or_default()
                );
            } else {
                anyhow::bail!(
                    "backing directory {} has active appender(s) on: {} — stop them before mounting",
                    store.dir.display(),
                    appenders.join(", ")
                );
            }
        }
    };
    crate::store::write_lock_info(
        &lock,
        &format!(
            "mountpoint={}\npid={}\n",
            mountpoint.display(),
            std::process::id()
        ),
    )?;
    let _lock = lock; // hold the flock until we return

    let store = Arc::new(Mutex::new(store));

    // Background flusher: bounds both data loss on a crash and the
    // write-time granularity of the index for slow writers.
    {
        let store = Arc::clone(&store);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(1000));
            store.lock().unwrap().flush_aged();
        });
    }

    let mut options = vec![
        MountOption::FSName("timberfs".to_string()),
        MountOption::AutoUnmount,
    ];
    if allow_other {
        options.push(MountOption::AllowOther);
    }
    let result = run_session(&store, mountpoint, &options);
    let result = match result {
        Err(e) if !allow_other => {
            // auto_unmount can require allow_other rights (user_allow_other
            // in /etc/fuse.conf) on some setups; degrade gracefully.
            eprintln!(
                "timberfs: mount with auto_unmount failed ({e}); retrying without it \
                 (unmount manually with: fusermount3 -u {})",
                mountpoint.display()
            );
            let options = vec![MountOption::FSName("timberfs".to_string())];
            run_session(&store, mountpoint, &options)
        }
        r => r,
    };
    store.lock().unwrap().flush_all();
    result?;
    Ok(())
}

/// mount2() minus the convenience: we need the session in hand to give the
/// filesystem a Notifier for kernel cache invalidation after rotations.
fn run_session(
    store: &Arc<Mutex<Store>>,
    mountpoint: &Path,
    options: &[MountOption],
) -> std::io::Result<()> {
    let notifier_slot = Arc::new(Mutex::new(None));
    let fs = TimberFs::new(Arc::clone(store), Arc::clone(&notifier_slot));
    let mut session = fuser::Session::new(fs, mountpoint, options)?;
    *notifier_slot.lock().unwrap() = Some(session.notifier());
    session.run()
}
