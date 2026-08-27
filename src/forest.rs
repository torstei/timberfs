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

use anyhow::{bail, Context};

use crate::format;
use crate::query::is_bundle;

/// Where forest configs live by default. Also named in the "nothing
/// configured" error so the user knows where to look.
const FORESTS_DIR: &str = "/etc/timberfs/forests.d";
/// Override for that location: TIMBERFS_FORESTS names the directories to
/// SEARCH, this one names where their declarations live. Both readers and
/// `forest create` honour it, so a non-root install writes and reads the
/// same place.
const FORESTS_DIR_ENV: &str = "TIMBERFS_FORESTS_DIR";
/// Override env var: colon-separated absolute dirs, replacing the config.
const FORESTS_ENV: &str = "TIMBERFS_FORESTS";

/// Where declarations live for this process.
fn forests_dir() -> PathBuf {
    std::env::var_os(FORESTS_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(FORESTS_DIR))
}

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

/// The handle of a store known by its LOGICAL name (`nginx.log`), as
/// opposed to its `.rings` file. The same single-`.log` strip `handle_of`
/// applies, exposed so `info` and `list` cannot disagree about what a
/// store is called.
pub fn handle_of_logical(name: &str) -> &str {
    name.strip_suffix(".log").unwrap_or(name)
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
        // No directory by that name. It may be the name a store DECLARES
        // — the only name it has once its path is opaque — or an id,
        // which is what `list` prints beside it.
        0 => lookup_declared_name(handle, &forests),
        _ => {
            let candidates = matches
                .iter()
                .map(|(name, store)| format!("  {name}: {}", store.display()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "handle `{handle}` is ambiguous — it matches several stores:\n{candidates}\n\
                 pass a full path or an id to pick one"
            );
        }
    }
}

/// Resolve by the name a store declares in its manifest. Declared names
/// are NOT unique — two hosts' `gateway01-console` in one archive is the
/// case this exists for — so several matches are reported rather than
/// picked between, exactly as an ambiguous directory handle is.
fn lookup_declared_name(token: &str, forests: &[Forest]) -> anyhow::Result<PathBuf> {
    let mut hits: Vec<(String, PathBuf, String)> = Vec::new();
    for forest in forests {
        for (handle, store) in scan_forest(&forest.dir) {
            let Ok((dir, name)) = crate::query::resolve_backing(&store) else {
                continue;
            };
            let declared = crate::bark::load(&dir, &name)
                .and_then(|b| b.get("name").and_then(|v| v.as_str()).map(str::to_string));
            if declared.as_deref() == Some(token) {
                let id = store_id(&store).unwrap_or_else(|| "no identity".to_string());
                hits.push((forest.name.clone(), store, id));
                let _ = handle;
            }
        }
    }
    match hits.len() {
        1 => Ok(hits.pop().unwrap().1),
        0 => lookup_id_prefix(token, forests),
        _ => {
            let candidates = hits
                .iter()
                .map(|(forest, store, id)| format!("  {id}  {} ({forest})", store.display()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "several stores are called `{token}` — a declared name is not unique:\n\
                 {candidates}\npick one by its id"
            )
        }
    }
}

/// Resolve a store by its `id`, in full or by a leading prefix — the form
/// `list` prints. Tried only AFTER the handle lookup misses, so a store
/// whose id happens to start like someone's handle can never shadow it.
fn lookup_id_prefix(token: &str, forests: &[Forest]) -> anyhow::Result<PathBuf> {
    // Four hex characters, git's minimum: short enough to type, long
    // enough that a mistyped handle — which almost always carries a
    // non-hex letter — is reported as a missing store instead of
    // resolving to an unrelated one.
    let id_shaped = token.len() >= 4 && token.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if !id_shaped {
        return Err(no_such_store(token, forests));
    }
    let want = token.to_ascii_lowercase();
    let mut hits: Vec<(String, String, PathBuf, String)> = Vec::new();
    for forest in forests {
        for (handle, store) in scan_forest(&forest.dir) {
            let Some(id) = store_id(&store) else { continue };
            if id.starts_with(&want) {
                hits.push((forest.name.clone(), handle, store, id));
            }
        }
    }
    match hits.len() {
        1 => Ok(hits.pop().unwrap().2),
        0 => Err(no_such_store(token, forests)),
        // A prefix that names more than one store names none of them: the
        // whole point of an id is that it is unambiguous, so widen it
        // rather than pick.
        _ => {
            let candidates = hits
                .iter()
                .map(|(forest, handle, _, id)| format!("  {id}  {handle} ({forest})"))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "id `{token}` is ambiguous — it is a prefix of several stores:\n{candidates}\n\
                 use more of the id"
            );
        }
    }
}

/// A store's declared `id`, or None where it has no manifest — which is a
/// real state (a store appended to before manifests existed), not an error.
fn store_id(store: &Path) -> Option<String> {
    let (dir, name) = crate::query::resolve_backing(store).ok()?;
    let bark = crate::bark::load(&dir, &name)?;
    bark.get("id")?.as_str().map(str::to_string)
}

fn no_such_store(token: &str, forests: &[Forest]) -> anyhow::Error {
    if forests.is_empty() {
        return anyhow::anyhow!(
            "no forests configured (see {}/); pass a full path",
            forests_dir().display()
        );
    }
    let searched = forests
        .iter()
        .map(|f| f.dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::anyhow!(
        "no store `{token}` in any forest (searched: {searched}); \
         pass a handle, an id, or a full path"
    )
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
    let Ok(entries) = std::fs::read_dir(forests_dir()) else {
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

/// Every store in the configured forests that the cursor anchor `anchor`
/// identifies — the reverse of `store_anchor`, for a follower that
/// records its store by IDENTITY and needs a path to hand a shipper.
///
/// Identity is authoritative and a path is a hint, so this is the road
/// taken when the hint no longer holds (the store moved, or the path was
/// reused by a different store). Several matches are possible only from a
/// copied `.bark`, which is a mistake worth naming rather than picking a
/// winner from — so all of them come back and the caller refuses.
pub fn stores_by_anchor(anchor: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for forest in load_forests() {
        for (_, store) in scan_forest(&forest.dir) {
            let Ok((dir, name)) = crate::query::resolve_backing(&store) else {
                continue;
            };
            let bark = crate::bark::load(&dir, &name);
            if crate::cursor::store_anchor(&dir, &name, bark.as_ref()) == anchor {
                out.push(store);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

// ------------------------------------------------- declaring a forest

/// A forest is the ONE thing a path names. Everything else a timberfs
/// command takes is a store, and a store is found by what it declares —
/// its labels, its name, its id — because a path is neither unique nor
/// stable and says nothing about what the store holds.
///
/// So this is where a path is spelled out, once, by an operator who is
/// deciding where data lives. `create` writes the config file
/// `load_forests` reads, so the file stops being the interface.
pub fn cmd_create(dir: &Path, name: Option<&str>, dry_run: bool) -> anyhow::Result<()> {
    // Absolute, because the config is read by daemons whose working
    // directory is not the operator's — a relative DIR would name a
    // different place depending on who read it.
    let dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(dir)
    };
    let name = match name {
        Some(n) => n.to_string(),
        None => default_name(&dir)?,
    };
    check_forest_name(&name)?;

    let conf = forests_dir().join(format!("{name}.conf"));
    // Read the configured set BEFORE writing, so the overlap checks see
    // the world this forest is joining.
    let existing = configured_forests();

    // Already declared, pointing at this same directory: a no-op, so
    // provisioning that runs on every boot is safe.
    if let Some(f) = existing.iter().find(|f| f.name == name) {
        if same_dir(&f.dir, &dir) {
            crate::note!(
                "timberfs: forest `{name}` already names {} ({})",
                dir.display(),
                conf.display()
            );
            return ensure_dir(&dir, dry_run);
        }
        bail!(
            "forest `{name}` already names {} — re-pointing a forest would strand every \
             store under it, so remove it first (`timberfs forest remove {name}`) if that \
             is really what you want",
            f.dir.display()
        );
    }

    // One directory, two names, is an ambiguity nothing downstream can
    // resolve: every store in it would be reported twice, once per
    // forest, and `list` could not tell them apart.
    if let Some(f) = existing.iter().find(|f| same_dir(&f.dir, &dir)) {
        bail!(
            "{} is already the forest `{}` — a directory is one forest, or every store in \
             it is found twice",
            dir.display(),
            f.name
        );
    }

    // Nesting is the same ambiguity, one level down: a forest is scanned
    // at its root AND one directory deep, so a forest inside a forest
    // makes the inner stores members of both.
    for f in &existing {
        if let Some(reason) = nests_with(&f.dir, &dir) {
            bail!(
                "{} {reason} the forest `{}` ({}) — a forest is scanned one directory deep, \
                 so the stores between them would belong to both",
                dir.display(),
                f.name,
                f.dir.display()
            );
        }
    }

    if dry_run {
        println!("would declare forest `{name}` = {}", dir.display());
        println!("would write {}", conf.display());
        return ensure_dir(&dir, true);
    }

    ensure_dir(&dir, false)?;
    let confdir = forests_dir();
    std::fs::create_dir_all(&confdir)
        .with_context(|| format!("creating {} (does this need root?)", confdir.display()))?;
    let body = format!(
        "# timberfs forest `{name}` — the directory stores under it live in.\n\
         # Declared by `timberfs forest create`; `timberfs forest list` shows it.\n\
         DIR={}\n",
        dir.display()
    );
    write_atomic(&conf, body.as_bytes())
        .with_context(|| format!("writing {} (does this need root?)", conf.display()))?;
    crate::note!(
        "timberfs: forest `{name}` = {} ({})",
        dir.display(),
        conf.display()
    );
    warn_if_env_overrides(&name);
    Ok(())
}

/// Un-declare a forest. Data is NEVER touched: the directory and every
/// store in it stay exactly as they are, and the path is printed so an
/// operator who did mean to delete the data knows where it is.
pub fn cmd_remove(name: &str, dry_run: bool) -> anyhow::Result<()> {
    let conf = forests_dir().join(format!("{name}.conf"));
    let Some(forest) = configured_forests().into_iter().find(|f| f.name == name) else {
        bail!(
            "no forest named `{name}` — `timberfs forest list` shows the ones that are \
             declared"
        );
    };
    let stores = scan_forest(&forest.dir).len();
    if dry_run {
        println!(
            "would un-declare forest `{name}` = {}",
            forest.dir.display()
        );
        println!("would remove {}", conf.display());
        return Ok(());
    }
    std::fs::remove_file(&conf)
        .with_context(|| format!("removing {} (does this need root?)", conf.display()))?;
    crate::note!("timberfs: forest `{name}` un-declared ({})", conf.display());
    crate::note!(
        "timberfs: {} and its {stores} store(s) are untouched — remove the data yourself \
         if that is what you meant",
        forest.dir.display()
    );
    Ok(())
}

/// What is declared, and whether it is usable. A forest whose directory
/// is missing or read-only is the failure that otherwise shows up as
/// "store not found" somewhere far away, so it is reported HERE.
pub fn cmd_list(json: bool, names_only: bool) -> anyhow::Result<()> {
    let forests = load_forests();
    let from_env = std::env::var_os(FORESTS_ENV).is_some();
    if names_only {
        for f in &forests {
            println!("{}", f.name);
        }
        return Ok(());
    }
    if json {
        let rows: Vec<serde_json::Value> = forests
            .iter()
            .map(|f| {
                let (exists, writable) = dir_state(&f.dir);
                serde_json::json!({
                    "name": f.name,
                    "dir": f.dir.display().to_string(),
                    "exists": exists,
                    "writable": writable,
                    "stores": scan_forest(&f.dir).len(),
                    "declared_in": if from_env {
                        FORESTS_ENV.to_string()
                    } else {
                        forests_dir().join(format!("{}.conf", f.name)).display().to_string()
                    },
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if forests.is_empty() {
        crate::note!(
            "timberfs: no forests declared — `timberfs forest create /var/log/timberfs` \
             declares one (config lives in {})",
            forests_dir().display()
        );
        return Ok(());
    }
    if from_env {
        eprintln!(
            "timberfs: {FORESTS_ENV} is set, so it REPLACES {} — what follows is \
             the environment's, and any declared forest is being ignored",
            forests_dir().display()
        );
    }
    let width = forests
        .iter()
        .map(|f| f.name.len())
        .max()
        .unwrap_or(4)
        .max(6);
    println!("{:<width$}  {:>6}  {:<9}  DIR", "FOREST", "STORES", "STATE");
    for f in &forests {
        let (exists, writable) = dir_state(&f.dir);
        let state = match (exists, writable) {
            (false, _) => "MISSING",
            (true, false) => "READONLY",
            (true, true) => "ok",
        };
        println!(
            "{:<width$}  {:>6}  {:<9}  {}",
            f.name,
            scan_forest(&f.dir).len(),
            state,
            f.dir.display()
        );
    }
    Ok(())
}

/// The directory a declared forest names. Unknown names are an error
/// that LISTS what is declared: a forest name is a small closed set an
/// operator chose, so the answer to a typo is the set itself.
pub fn dir_of(name: &str) -> anyhow::Result<PathBuf> {
    let forests = load_forests();
    if let Some(f) = forests.iter().find(|f| f.name == name) {
        return Ok(f.dir.clone());
    }
    bail!("{}", no_such_forest(name, &forests));
}

/// Where an intake writes: a declared forest by NAME, or a raw directory.
///
/// A forest is the answer, and `--into-dir` is kept working because it is
/// the only way to write into a directory that is NOT a forest — a
/// confined instance given a root of its own, say. The two are exclusive:
/// a command that took both would have to pick, and picking silently is
/// how a service ends up writing somewhere nobody expects.
pub fn into_dir(forest: Option<&str>, dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match (forest, dir) {
        (Some(name), None) => dir_of(name),
        (None, Some(dir)) => {
            eprintln!(
                "timberfs: warning: --into-dir {} is deprecated — name a forest with \
                 `--forest NAME` instead ({}). A directory that is not a forest still \
                 needs this flag; declare it with `timberfs forest create` if it should \
                 be one",
                dir.display(),
                declared_names(&load_forests())
            );
            Ok(dir)
        }
        (Some(_), Some(_)) => bail!(
            "--forest and --into-dir both name where to write, so give one: \
             --forest for a declared forest, --into-dir for a directory that is not one"
        ),
        (None, None) => {
            let forests = load_forests();
            bail!(
                "no destination — give `--forest NAME` ({}), or `--into-dir DIR` for a \
                 directory that is not a forest",
                declared_names(&forests)
            )
        }
    }
}

fn no_such_forest(name: &str, forests: &[Forest]) -> String {
    if forests.is_empty() {
        return format!(
            "no forest named `{name}`, and none is declared — \
             `timberfs forest create DIR` declares one"
        );
    }
    format!(
        "no forest named `{name}` — declared: {}",
        declared_names(forests)
    )
}

fn declared_names(forests: &[Forest]) -> String {
    if forests.is_empty() {
        "none declared".to_string()
    } else {
        forests
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The configured forests as `create`/`remove` see them: the config
/// files, NEVER the environment override. A forest is declared on disk,
/// and an env var that hides it for one process must not make `create`
/// think the name is free.
fn configured_forests() -> Vec<Forest> {
    let Ok(entries) = std::fs::read_dir(forests_dir()) else {
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

/// Warn when the environment is hiding what we just wrote — otherwise
/// `create` reports success and `list` does not show it, which reads as
/// the write having failed.
fn warn_if_env_overrides(name: &str) {
    if std::env::var_os(FORESTS_ENV).is_some() {
        eprintln!(
            "timberfs: warning: {FORESTS_ENV} is set in this environment, and it REPLACES \
             {} — `{name}` is declared on disk but will be invisible to any process with \
             that variable set",
            forests_dir().display()
        );
    }
}

fn default_name(dir: &Path) -> anyhow::Result<String> {
    dir.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!("cannot name a forest after {} — pass --name", dir.display())
        })
}

/// A forest name becomes a config FILE NAME and appears in diagnostics,
/// so it is constrained the same way a follower name is.
fn check_forest_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("a forest needs a name");
    }
    if name.len() > 64 {
        bail!("forest name {name:?} is too long (64 characters at most)");
    }
    if name == "." || name == ".." {
        bail!("{name:?} is not a name");
    }
    if name.starts_with('-') {
        bail!("forest name {name:?} may not start with `-` (it would read as a flag)");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
    {
        bail!(
            "forest name {name:?} contains {bad:?} — names are [A-Za-z0-9_.-], because the \
             name is the config file's name and appears in every ambiguity message"
        );
    }
    Ok(())
}

/// Compare two forest directories for identity. Lexical equality is not
/// enough — `/srv/logs` and `/srv/logs/` and a symlink to either are one
/// directory — so compare what the filesystem resolves them to when it
/// can, and fall back to the text when it cannot.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => normalize(a) == normalize(b),
    }
}

fn normalize(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().trim_end_matches('/').to_string())
}

/// Is one of these directories inside the other? Returns the relationship
/// as a phrase for the error message, or None when they are disjoint.
fn nests_with(existing: &Path, new: &Path) -> Option<&'static str> {
    let e = existing
        .canonicalize()
        .unwrap_or_else(|_| normalize(existing));
    let n = new.canonicalize().unwrap_or_else(|_| normalize(new));
    if n.starts_with(&e) {
        Some("is inside")
    } else if e.starts_with(&n) {
        Some("contains")
    } else {
        None
    }
}

fn ensure_dir(dir: &Path, dry_run: bool) -> anyhow::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if dry_run {
        println!("would create the directory {}", dir.display());
        return Ok(());
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating {} (does this need root?)", dir.display()))?;
    crate::note!("timberfs: created {}", dir.display());
    Ok(())
}

/// Does the directory exist, and can we write into it? Writability is
/// probed rather than deduced from the mode bits, which say nothing about
/// a read-only mount or the caller's capabilities.
fn dir_state(dir: &Path) -> (bool, bool) {
    if !dir.is_dir() {
        return (false, false);
    }
    let probe = dir.join(".timberfs-write-probe");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            (true, true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&probe);
            (true, true)
        }
        Err(_) => (true, false),
    }
}

/// tmp + rename, so a reader never sees a half-written declaration: every
/// timberfs process re-reads this config on its own schedule, and one that
/// caught a truncated `DIR=` would search the wrong place.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forest_name_becomes_a_filename_so_it_is_constrained() {
        assert!(check_forest_name("default").is_ok());
        assert!(check_forest_name("archive-2026.q3").is_ok());
        assert!(check_forest_name("").is_err());
        assert!(
            check_forest_name("..").is_err(),
            "would escape the config dir"
        );
        assert!(check_forest_name("a/b").is_err(), "would be a path");
        assert!(check_forest_name("-x").is_err(), "would read as a flag");
        assert!(check_forest_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn one_directory_is_one_forest_however_it_is_spelled() {
        // A trailing slash is not a different directory, and declaring the
        // same one twice would report every store in it twice.
        assert!(same_dir(Path::new("/srv/logs"), Path::new("/srv/logs/")));
        assert!(same_dir(Path::new("/srv/logs/"), Path::new("/srv/logs")));
        assert!(!same_dir(Path::new("/srv/logs"), Path::new("/srv/logs2")));
    }

    #[test]
    fn nesting_is_refused_because_a_forest_is_scanned_one_level_deep() {
        // /srv/logs finds /srv/logs/archive/x.rings as a nested store, and
        // so does a forest rooted at /srv/logs/archive — same store, two
        // forests, and an ambiguous handle with no way to pick.
        assert_eq!(
            nests_with(Path::new("/srv/logs"), Path::new("/srv/logs/archive")),
            Some("is inside")
        );
        assert_eq!(
            nests_with(Path::new("/srv/logs/archive"), Path::new("/srv/logs")),
            Some("contains")
        );
        assert_eq!(
            nests_with(Path::new("/srv/logs"), Path::new("/srv/other")),
            None
        );
        // A shared PREFIX is not nesting: `starts_with` on components, not
        // on the string, or /srv/logs2 would read as inside /srv/logs.
        assert_eq!(
            nests_with(Path::new("/srv/logs"), Path::new("/srv/logs2")),
            None
        );
    }
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
