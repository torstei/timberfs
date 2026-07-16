//! Forests: directories timberfs searches for stores by a short handle, so
//! `timberfs query nginx` finds /var/log/timberfs/nginx/nginx.log without the
//! caller spelling out the full path. Full paths keep working unchanged — a
//! forest is consulted only for a bare token that is not already a store on
//! disk, so path-based usage carries zero added overhead.
//!
//! Config lives in /etc/timberfs/forests.d/*.conf, one forest per file,
//! KEY=VALUE (the same idiom as the /etc/timberfs/<instance>.conf mount
//! configs). P1 reads one key, `DIR=<absolute path>`; blank lines, `#`
//! comments and unknown keys are ignored (forward-compat). Files are read in
//! sorted filename order, which is also the search order. The env var
//! TIMBERFS_FORESTS (colon-separated absolute dirs) replaces the config
//! wholesale — a test/one-off override that keeps this resolver a pure
//! function with no clap plumbing.

use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::format;
use crate::query::is_bundle;

/// Where forest configs live. Also named in the "nothing configured" error so
/// the user knows where to look.
const FORESTS_DIR: &str = "/etc/timberfs/forests.d";
/// Override env var: colon-separated absolute dirs, replacing the config.
const FORESTS_ENV: &str = "TIMBERFS_FORESTS";

/// A configured forest: a directory searched for stores, plus the name it was
/// configured under (the config filename minus `.conf`; the directory path
/// for an env-provided forest). The name is only for diagnostics and
/// ambiguity messages — qualified handles (`default:nginx`) come later.
pub(crate) struct Forest {
    pub(crate) name: String,
    pub(crate) dir: PathBuf,
}

/// The handle a store's `.rings` file is reachable by: the file name minus
/// `.rings`, minus a single trailing `.log`. Layout-independent — a flat
/// `nginx.rings` and a nested `nginx/nginx.log.rings` both yield `nginx`.
/// Returns None when the name is not a `.rings` file at all.
///
/// ```text
/// nginx.log.rings      -> nginx
/// nginx.rings          -> nginx
/// metrics.jsonl.rings  -> metrics.jsonl   (only .log is stripped)
/// nginx.log.log.rings  -> nginx.log       (a single strip)
/// ```
fn handle_of(rings_file_name: &str) -> Option<&str> {
    let stem = rings_file_name.strip_suffix(&format!(".{}", format::RINGS_EXT))?;
    Some(stem.strip_suffix(".log").unwrap_or(stem))
}

/// Resolve a user-supplied source argument to a store path. A full path,
/// relative path or `.timber` bundle is returned unchanged; only a bare token
/// that names no existing store is looked up as a handle across the forests.
pub fn resolve_source(arg: &Path) -> anyhow::Result<PathBuf> {
    // 1. An existing store (or a `.timber` bundle, existing or not) wins with
    //    no forest scan, so every full-path/relative/bundle invocation
    //    behaves exactly as it did before forests existed.
    if is_bundle(arg) || is_existing_store(arg) {
        return Ok(arg.to_path_buf());
    }
    // 2. Anything with a path separator is a path, never a handle: hand it
    //    back so the normal "no index file" error fires, as it did before.
    let Some(handle) = bare_token(arg) else {
        return Ok(arg.to_path_buf());
    };
    // 3. A bare token that is not an on-disk store: look it up as a handle.
    lookup_handle(handle)
}

/// True when `arg` already names a store: it exists, or its `<arg>.trunk` /
/// `<arg>.rings` backing file does (the logical-name form resolve_backing
/// accepts).
fn is_existing_store(arg: &Path) -> bool {
    arg.exists()
        || append_ext(arg, format::TRUNK_EXT).exists()
        || append_ext(arg, format::RINGS_EXT).exists()
}

/// A bare handle token: the whole argument, iff it contains no path
/// separator and is valid UTF-8. Anything with a `/` (relative or absolute)
/// or a non-UTF-8 name is None and treated as a literal path.
fn bare_token(arg: &Path) -> Option<&str> {
    let s = arg.to_str()?;
    if s.contains('/') {
        return None;
    }
    Some(s)
}

/// Append `.ext` to the whole path (not `Path::with_extension`, which would
/// replace an existing one): `app.log` + `rings` -> `app.log.rings`.
fn append_ext(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Find `handle` across the configured forests, erroring on zero or several
/// matches with a message that points the user at a full path.
fn lookup_handle(handle: &str) -> anyhow::Result<PathBuf> {
    let forests = load_forests();
    // (forest name, store path) for every scanned store whose handle matches.
    let mut matches: Vec<(&str, PathBuf)> = Vec::new();
    for forest in &forests {
        for (h, store) in scan_forest(&forest.dir) {
            if h == handle {
                matches.push((forest.name.as_str(), store));
            }
        }
    }
    match matches.len() {
        1 => Ok(matches.pop().unwrap().1),
        0 => {
            if forests.is_empty() {
                bail!("no forests configured (see {FORESTS_DIR}/); pass a full path");
            }
            let searched = forests
                .iter()
                .map(|f| f.dir.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("no store `{handle}` in any forest (searched: {searched}); pass a full path");
        }
        _ => {
            let candidates = matches
                .iter()
                .map(|(name, store)| format!("  {name}: {}", store.display()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "handle `{handle}` is ambiguous — it matches several stores:\n{candidates}\n\
                 pass a full path to pick one"
            );
        }
    }
}

/// The forests for `timberfs list`: the given directories as ad-hoc forests
/// (named by their own path, same as an env-provided forest) when any are
/// given, otherwise every configured forest.
pub(crate) fn forests_for_list(dirs: &[PathBuf]) -> Vec<Forest> {
    if dirs.is_empty() {
        load_forests()
    } else {
        dirs.iter()
            .map(|dir| Forest {
                name: dir.display().to_string(),
                dir: dir.clone(),
            })
            .collect()
    }
}

/// The configured forests, in search order. TIMBERFS_FORESTS, when set,
/// replaces the config entirely; otherwise read /etc/timberfs/forests.d/*.conf
/// in sorted filename order.
fn load_forests() -> Vec<Forest> {
    if let Some(env) = std::env::var_os(FORESTS_ENV) {
        return std::env::split_paths(&env)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| Forest {
                name: dir.display().to_string(),
                dir,
            })
            .collect();
    }
    let Ok(entries) = std::fs::read_dir(FORESTS_DIR) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("conf"))
        .collect();
    files.sort();
    files.iter().filter_map(|p| parse_forest_file(p)).collect()
}

/// Read one forest config file. Returns None when it declares no usable `DIR`.
fn parse_forest_file(path: &Path) -> Option<Forest> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut dir: Option<PathBuf> = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "DIR" {
            dir = Some(PathBuf::from(value.trim()));
        }
    }
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("forest")
        .to_string();
    dir.map(|dir| Forest { name, dir })
}

/// Every store discovered in a forest, as (handle, logical-name path). Scans
/// the forest root and its immediate subdirectories for `*.rings` — flat
/// stores at the root, nested stores one level down. A missing or unreadable
/// forest yields nothing (skipped silently).
pub(crate) fn scan_forest(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(sub) = std::fs::read_dir(&path) {
                for sub_entry in sub.flatten() {
                    push_if_rings(&sub_entry.path(), &mut out);
                }
            }
        } else {
            push_if_rings(&path, &mut out);
        }
    }
    out
}

/// If `path` is a `.rings` file, push (handle, logical-name path) — the
/// logical-name path is the `.rings` file with only its `.rings` suffix
/// stripped, which resolve_backing then splits back into (dir, name).
fn push_if_rings(path: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let Some(handle) = handle_of(name) else {
        return;
    };
    let logical = name
        .strip_suffix(&format!(".{}", format::RINGS_EXT))
        .expect("handle_of matched, so the .rings suffix is present");
    out.push((handle.to_string(), path.with_file_name(logical)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    // TIMBERFS_FORESTS is process-global; serialize the tests that set it so
    // cargo's parallel test threads don't race on the env var. The lock is
    // held only across the resolve() call, not the assertions, so a failing
    // assertion never poisons it for the next test.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique scratch directory that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("timberfs-forest-{}-{n}", std::process::id()));
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

    /// Create an empty `.rings` file (plus its dir), the marker scan_forest
    /// keys on. Its `.trunk` is irrelevant to resolution, so we skip it.
    fn touch_rings(dir: &Path, rings_name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(rings_name), b"").unwrap();
    }

    /// Resolve `arg` with TIMBERFS_FORESTS pointed at `dirs`. The env var is
    /// set and cleared under ENV_LOCK, around the resolve call only.
    fn resolve_with_forests(dirs: &[&Path], arg: &str) -> anyhow::Result<PathBuf> {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let joined = std::env::join_paths(dirs.iter().map(|d| d.as_os_str())).unwrap();
        std::env::set_var(FORESTS_ENV, &joined);
        let result = resolve_source(Path::new(arg));
        std::env::remove_var(FORESTS_ENV);
        result
    }

    #[test]
    fn handle_of_strips_rings_then_a_single_log() {
        assert_eq!(handle_of("nginx.log.rings"), Some("nginx"));
        assert_eq!(handle_of("nginx.rings"), Some("nginx"));
        assert_eq!(handle_of("metrics.jsonl.rings"), Some("metrics.jsonl"));
        assert_eq!(handle_of("nginx.log.log.rings"), Some("nginx.log"));
        // Not a .rings file at all.
        assert_eq!(handle_of("nginx.trunk"), None);
    }

    #[test]
    fn resolves_a_nested_store_by_handle() {
        let forest = TempDir::new();
        touch_rings(&forest.path().join("nginx"), "nginx.log.rings");
        let resolved = resolve_with_forests(&[forest.path()], "nginx").unwrap();
        assert_eq!(resolved, forest.path().join("nginx").join("nginx.log"));
    }

    #[test]
    fn resolves_a_flat_store_by_handle() {
        let forest = TempDir::new();
        touch_rings(forest.path(), "app.log.rings");
        let resolved = resolve_with_forests(&[forest.path()], "app").unwrap();
        assert_eq!(resolved, forest.path().join("app.log"));
    }

    #[test]
    fn existing_store_path_passes_through_unchanged() {
        // The logical name has no file of its own, but <arg>.rings exists, so
        // step 1 must return the argument verbatim without any forest scan.
        let dir = TempDir::new();
        touch_rings(dir.path(), "real.log.rings");
        let arg = dir.path().join("real.log");
        // No forests set: if this scanned, it would hit the real /etc — but
        // step 1 short-circuits before that.
        let resolved = resolve_source(&arg).unwrap();
        assert_eq!(resolved, arg);
    }

    #[test]
    fn slashed_nonexistent_path_passes_through_unchanged() {
        let arg = Path::new("some/nonexistent/store.log");
        let resolved = resolve_source(arg).unwrap();
        assert_eq!(resolved, arg);
    }

    #[test]
    fn a_slashed_name_never_becomes_a_handle() {
        // Even with a matching `nginx` store in the forest, `./nginx` has a
        // separator, so it stays a literal path (and misses, as it should).
        let forest = TempDir::new();
        touch_rings(&forest.path().join("nginx"), "nginx.log.rings");
        let resolved = resolve_with_forests(&[forest.path()], "./nginx").unwrap();
        assert_eq!(resolved, Path::new("./nginx"));
    }

    #[test]
    fn unknown_handle_is_an_error() {
        let forest = TempDir::new();
        touch_rings(&forest.path().join("nginx"), "nginx.log.rings");
        let err = resolve_with_forests(&[forest.path()], "absent")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no store `absent`"), "got: {err}");
    }

    #[test]
    fn ambiguous_handle_is_an_error() {
        // Same handle in two forests: the resolver must refuse rather than
        // guess, and name both candidates.
        let a = TempDir::new();
        let b = TempDir::new();
        touch_rings(a.path(), "dup.log.rings");
        touch_rings(b.path(), "dup.log.rings");
        let err = resolve_with_forests(&[a.path(), b.path()], "dup")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "got: {err}");
        assert!(err.contains("dup.log"), "got: {err}");
    }

    #[test]
    fn no_forests_configured_is_a_distinct_error() {
        let err = resolve_with_forests(&[], "nginx").unwrap_err().to_string();
        assert!(err.contains("no forests configured"), "got: {err}");
    }
}
