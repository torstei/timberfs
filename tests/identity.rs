//! However a store comes to exist, its index carries an identity and its
//! manifest either says nothing or says the same one. A store whose two
//! sides name different stores is refused by the next writer to open it.
//!
//! The paths are driven through the binary because that is how a store is
//! really made, and every one is written to again afterwards: the
//! disagreement only shows on the second open.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Dir(PathBuf);

impl Dir {
    fn new() -> Dir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("timberfs-identity-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    fn store(&self) -> PathBuf {
        self.0.join("s.log")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_timberfs"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn ok(args: &[&str], stdin: &[u8]) {
    let out = run(args, stdin);
    assert!(
        out.status.success(),
        "timberfs {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn records(text: &str) -> Vec<u8> {
    let mut s = b"\x1estream-start\x1fv=1\x00".to_vec();
    s.extend_from_slice(format!("\x1eentry\x1flen={}\x00{text}\x00", text.len()).as_bytes());
    s.extend_from_slice(b"\x1estream-end\x00");
    s
}

/// (manifest id, index id), read from the files.
fn ids(store: &Path) -> (Option<String>, Option<String>) {
    let (dir, name) = timberfs::query::resolve_backing(store).unwrap();
    let manifest = timberfs::bark::load(&dir, &name)
        .and_then(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string));
    (manifest, timberfs::bark::carried_identity(&dir, &name))
}

fn assert_one_store(store: &Path, how: &str) {
    let (manifest, index) = ids(store);
    let index = index.unwrap_or_else(|| panic!("{how}: the index carries no identity"));
    if let Some(manifest) = manifest {
        assert_eq!(
            manifest, index,
            "{how}: manifest and index name different stores"
        );
    }
}

/// The store as `how` left it, then written to by each kind of writer.
fn check(how: &str, make: impl Fn(&Path)) {
    let d = Dir::new();
    let store = d.store();
    let s = store.to_str().unwrap();
    make(&store);
    assert_one_store(&store, how);

    ok(
        &["append", "--records", "--quiet", "--into", s],
        &records("again"),
    );
    assert_one_store(&store, &format!("{how}, then a records append"));
    ok(&["append", "--quiet", "--into", s], b"and again\n");
    assert_one_store(&store, &format!("{how}, then a plain append"));
    ok(&["set", s, "host=h"], b"");
    assert_one_store(&store, &format!("{how}, then a declaration"));
}

#[test]
fn a_bare_create() {
    check("create", |p| {
        ok(&["create", "--quiet", p.to_str().unwrap()], b"")
    });
}

#[test]
fn a_create_that_declares_something() {
    check("create --set", |p| {
        ok(
            &["create", "--quiet", "--set", "host=h", p.to_str().unwrap()],
            b"",
        )
    });
    check("create --index", |p| {
        ok(&["create", "--quiet", "--index", p.to_str().unwrap()], b"")
    });
    check("create --retain", |p| {
        ok(
            &["create", "--quiet", "--retain", "30d", p.to_str().unwrap()],
            b"",
        )
    });
}

#[test]
fn a_plain_append_that_creates_the_store() {
    check("append", |p| {
        ok(
            &["append", "--quiet", "--into", p.to_str().unwrap()],
            b"hello\n",
        )
    });
}

#[test]
fn a_records_append_that_creates_the_store() {
    check("append --records", |p| {
        ok(
            &[
                "append",
                "--records",
                "--quiet",
                "--into",
                p.to_str().unwrap(),
            ],
            &records("hello"),
        )
    });
}

#[test]
fn an_import_that_creates_the_store() {
    check("import", |p| {
        let src = p.with_file_name("src.txt");
        std::fs::write(
            &src,
            "2026-10-03T10:00:00Z hello\n2026-10-03T10:00:01Z world\n",
        )
        .unwrap();
        ok(
            &[
                "import",
                "--quiet",
                src.to_str().unwrap(),
                "--into",
                p.to_str().unwrap(),
            ],
            b"",
        )
    });
}

#[test]
fn a_bare_create_and_then_a_writer_that_writes_the_manifest() {
    // The sink writes a manifest when it finishes, and a bare create had
    // none: the case where the two sides came to name different stores.
    check("create, then a records append", |p| {
        let s = p.to_str().unwrap();
        ok(&["create", "--quiet", s], b"");
        ok(
            &["append", "--records", "--quiet", "--into", s],
            &records("first"),
        );
    });
    check("create, then a plain append", |p| {
        let s = p.to_str().unwrap();
        ok(&["create", "--quiet", s], b"");
        ok(&["append", "--quiet", "--into", s], b"first\n");
    });
}

#[test]
fn a_store_recovered_into_its_manifest() {
    check("append, then create --if-not-exists", |p| {
        let s = p.to_str().unwrap();
        ok(&["append", "--quiet", "--into", s], b"first\n");
        ok(&["create", "--quiet", "--if-not-exists", s], b"");
        let (manifest, index) = ids(p);
        assert_eq!(
            manifest, index,
            "recovery writes the index's id into the manifest"
        );
    });
}
