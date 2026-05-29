//! Parallel walker over a Unity project's `Assets/` tree.
//!
//! Uses [`ignore::WalkBuilder`] with `standard_filters(false)` — gitignore
//! is deliberately OFF. Unity doesn't honor `.gitignore` either, and
//! gitignored `.meta` files still carry GUIDs that prefabs can reference.
//! `is_unity_hidden` (`.foo`, `foo~`) is the only filter.
//! See [`asset_walk_builder`] and [[`asset-database.md`]](#populating).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore::WalkBuilder;
pub(crate) use unity_path_rules::is_unity_hidden;
use unity_path_rules::is_opaque_subtree;

/// Errors from walking the Unity project tree.
#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("get cwd: {0}")]
    GetCwd(#[source] std::io::Error),
    #[error("canonicalize project {}: {source}", path.display())]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("not a Unity project: {} (missing Assets/ or ProjectSettings/)", path.display())]
    NotProject { path: PathBuf },
    #[error("no Unity project root found above {}: needs `Assets/` + `ProjectSettings/`", cwd.display())]
    NoProjectRoot { cwd: PathBuf },
    #[error("Assets/ not found at {}", path.display())]
    AssetsMissing { path: PathBuf },
    #[error("walk error: {0}")]
    Walk(#[from] ignore::Error),
    #[error("read dir {}: {source}", path.display())]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Resolve a Unity project root: arg-given or climb up from CWD until we
/// find a directory containing both `Assets/` and `ProjectSettings/`.
pub fn resolve_project_root(arg: Option<&Path>) -> Result<PathBuf, WalkError> {
    if let Some(p) = arg {
        let p = p.canonicalize().map_err(|source| WalkError::Canonicalize {
            path: p.to_path_buf(),
            source,
        })?;
        ensure_project(&p)?;
        return Ok(p);
    }
    let cwd = std::env::current_dir().map_err(WalkError::GetCwd)?;
    let mut cur: &Path = &cwd;
    loop {
        if is_project(cur) {
            return Ok(cur.to_path_buf());
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return Err(WalkError::NoProjectRoot { cwd: cwd.clone() }),
        }
    }
}

fn is_project(p: &Path) -> bool {
    p.join("Assets").is_dir() && p.join("ProjectSettings").is_dir()
}

fn ensure_project(p: &Path) -> Result<(), WalkError> {
    if !is_project(p) {
        return Err(WalkError::NotProject {
            path: p.to_path_buf(),
        });
    }
    Ok(())
}

/// Visit every `.meta` file under `<project>/Assets/` and `<project>/Packages/`,
/// calling `factory()` once per worker thread to produce a `FnMut(&Path)`
/// visitor. Per-thread state lives inside the visitor and avoids the contention
/// of a single `Mutex<Vec<_>>` shared across threads.
///
/// Both top-level dirs are walked because Unity treats UPM packages as
/// first-class asset sources — prefabs/materials/`.mixer` files under
/// `Packages/` are referenced by `Assets/` content and need to round-trip
/// through asset-db like any other asset.
///
/// Unity-hidden paths are skipped — Unity's importer ignores any folder
/// or file whose name starts with `.` or ends with `~` (see
/// <https://docs.unity3d.com/Manual/SpecialFolders.html>). Including them
/// would surface fake assets (templates, scratch copies) that Unity itself
/// never sees.
///
/// # Panics
///
/// Panics if the worker-side `Mutex` guarding the first-error slot is
/// poisoned by another worker thread panic. Worker visitors aren't
/// expected to panic in practice — the bake recovers panics into
/// errors via `run_with_panic_safety`.
pub fn walk_meta_files<F, V>(project_root: &Path, factory: F) -> Result<(), WalkError>
where
    F: Fn() -> V + Sync,
    V: FnMut(&Path) + Send + 'static,
{
    let assets = project_root.join("Assets");
    if !assets.is_dir() {
        return Err(WalkError::AssetsMissing { path: assets });
    }
    let packages = project_root.join("Packages");

    let mut builder = asset_walk_builder(&assets);
    if packages.is_dir() {
        builder.add(&packages);
    }
    let walker = builder.build_parallel();

    let err: Arc<Mutex<Option<WalkError>>> = Arc::new(Mutex::new(None));

    walker.run(|| {
        let mut visit = factory();
        let err = Arc::clone(&err);
        Box::new(move |res| {
            use ignore::WalkState;
            let entry = match res {
                Ok(e) => e,
                Err(e) => {
                    *err.lock().unwrap() = Some(WalkError::Walk(e));
                    return WalkState::Quit;
                }
            };
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "meta")
                    && !meta_targets_blacklisted_ext(path)
                {
                    visit(path);
                }
            }
            WalkState::Continue
        })
    });

    // Take the captured error if any. `Arc::try_unwrap` would silently
    // drop the error on a lingering clone or poisoned lock — instead lock,
    // take, and let `unwrap()` propagate poison as a panic (a poisoned
    // walk-error mutex is a bug we want to surface).
    if let Some(e) = err.lock().unwrap().take() {
        return Err(e);
    }
    Ok(())
}

/// Visit every file/directory under `<project>/Assets/` and
/// `<project>/Packages/<pkg>/` that is missing a sibling `.meta`,
/// calling `visit(path, is_dir)` for each candidate.
///
/// Asset roots themselves (`Assets/`, `Packages/`) and direct children
/// of `Packages/` are skipped — Unity treats embedded UPM package roots
/// and `manifest.json` / `packages-lock.json` as virtual / unmanaged
/// and never authors a `.meta` for them.
///
/// Opaque subtrees — folder-based Android plugins (`.androidlib`,
/// `.androidpack`, `.aar`) and git submodule roots — are visited at
/// the root level (so the folder itself gets a synthesized `.meta` if
/// absent) but never descended into. Unity hands Android plugin
/// contents to Gradle untouched, and submodules are foreign repos
/// whose working tree we shouldn't dirty.
///
/// Parallelism: each top-level subdirectory of `Assets/` and
/// `Packages/<pkg>/` is walked in its own thread via [`std::thread::scope`].
/// `read_dir` syscalls dominate the pre-pass on warm bakes (no synthesis
/// I/O on the happy path), and the directories are independent so the
/// scheduler gets near-linear speedup on multi-core. Hits are collected
/// per-thread then visited sequentially in the calling thread — keeps
/// the visitor `FnMut` (synthesize closes over a mutable counter and
/// `Option<Error>`) and the synthesis writes serialized (each meta is a
/// rename-over-tmp; serial keeps the out-dir flock contention predictable).
pub fn walk_for_missing_meta<F>(project_root: &Path, mut visit: F) -> Result<(), WalkError>
where
    F: FnMut(&Path, bool),
{
    let assets = project_root.join("Assets");
    if !assets.is_dir() {
        return Err(WalkError::AssetsMissing { path: assets });
    }
    let hits = walk_root_parallel(&assets, ASSETS_MIN_DEPTH)?;
    for (path, is_dir) in hits {
        visit(&path, is_dir);
    }

    let packages = project_root.join("Packages");
    if packages.is_dir() {
        let hits = walk_root_parallel(&packages, PACKAGES_MIN_DEPTH)?;
        for (path, is_dir) in hits {
            visit(&path, is_dir);
        }
    }
    Ok(())
}

/// Per-subtree return shape: pairs of `(path-missing-meta, is_dir)`.
/// `walk_root_parallel` collects these across threads, then sorts before
/// returning so the call order of `visit` is deterministic across runs
/// (a precondition for byte-stable bake output even when synthesis fires).
type MissingMetaHits = Vec<(PathBuf, bool)>;

/// Walk a root directory's immediate subdirectories in parallel. Files
/// directly under `root` are walked sequentially in the calling thread
/// (a Unity project has few of these — `manifest.json`/`packages-lock.json`
/// at the `Packages/` root, the occasional stray under `Assets/`). The
/// subtree walks (the bulk of the work) run concurrently via
/// [`std::thread::scope`].
fn walk_root_parallel(root: &Path, min_depth: usize) -> Result<MissingMetaHits, WalkError> {
    use std::ffi::OsString;
    let rd = std::fs::read_dir(root).map_err(|source| WalkError::ReadDir {
        path: root.to_path_buf(),
        source,
    })?;
    // Collect root-level state in one pass (mirrors `walk_dir_for_missing_meta`).
    struct RootEntry {
        name: OsString,
        path: PathBuf,
        is_dir: bool,
    }
    let mut metas: ahash::AHashSet<OsString> = ahash::AHashSet::new();
    let mut root_entries: Vec<RootEntry> = Vec::new();
    for res in rd {
        let entry = res.map_err(|source| WalkError::ReadDir {
            path: root.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        if is_unity_hidden(&name) {
            continue;
        }
        if let Some(ext) = Path::new(&name).extension() {
            if ext == "meta" {
                if let Some(stem) = Path::new(&name).file_stem() {
                    metas.insert(stem.to_owned());
                }
                continue;
            }
            if is_blacklisted_extension(ext) {
                continue;
            }
        }
        let ft = entry.file_type().map_err(|source| WalkError::ReadDir {
            path: root.to_path_buf(),
            source,
        })?;
        root_entries.push(RootEntry {
            name,
            path: entry.path(),
            is_dir: ft.is_dir(),
        });
    }

    let root_depth = 1usize;
    let mut hits: MissingMetaHits = Vec::new();

    // Root-level visits (files or opaque-folder roots at depth 1).
    for e in &root_entries {
        if root_depth >= min_depth && !metas.contains(&e.name) {
            hits.push((e.path.clone(), e.is_dir));
        }
    }

    // Subtrees in parallel. Each thread walks its assigned subdirectory
    // sequentially and returns its own hit vec; the calling thread joins
    // and merges.
    std::thread::scope(|s| -> Result<(), WalkError> {
        let mut handles: Vec<std::thread::ScopedJoinHandle<'_, Result<MissingMetaHits, WalkError>>> =
            Vec::new();
        for e in &root_entries {
            if e.is_dir && !is_opaque_subtree(&e.name, &e.path) {
                let path = e.path.clone();
                handles.push(s.spawn(move || {
                    let mut local: MissingMetaHits = Vec::new();
                    walk_dir_collect(&path, root_depth, min_depth, &mut local)?;
                    Ok(local)
                }));
            }
        }
        for h in handles {
            let sub = h.join().expect("walk-subtree thread panicked")?;
            hits.extend(sub);
        }
        Ok(())
    })?;

    hits.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(hits)
}

/// Synthesize at every depth under `Assets/`.
const ASSETS_MIN_DEPTH: usize = 1;
/// Skip `Packages/` direct children — embedded UPM package roots and
/// `manifest.json` / `packages-lock.json` carry no `.meta`.
const PACKAGES_MIN_DEPTH: usize = 2;

/// Recursive subtree walker for the missing-meta pre-pass. Pre-order so
/// the folder itself is reported before descent. Hand-rolled (not
/// `ignore::Walk`) because we need to visit an opaque folder root then
/// refuse to descend into it — a distinction `ignore`'s `filter_entry`
/// can't express (filtering a dir suppresses both visit and recursion).
///
/// Single-threaded — called per-subtree from `walk_root_parallel` which
/// forks one thread per top-level directory.
fn walk_dir_collect(
    dir: &Path,
    depth: usize,
    min_depth: usize,
    hits: &mut MissingMetaHits,
) -> Result<(), WalkError> {
    // Two-stage read of each directory: first pass collects every entry
    // and the set of `.meta` filenames present, second pass tests
    // candidates against that set. This eliminates one stat call per
    // entry — the original `Path::exists()` check on `<name>.meta`
    // dominated warm-bake wall-clock time (~80 ms on a 20k-file project)
    // even when no synthesis was needed.
    use std::ffi::OsString;
    let rd = std::fs::read_dir(dir).map_err(|source| WalkError::ReadDir {
        path: dir.to_path_buf(),
        source,
    })?;
    struct Candidate {
        name: OsString,
        path: PathBuf,
        is_dir: bool,
    }
    let mut metas: ahash::AHashSet<OsString> = ahash::AHashSet::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    for res in rd {
        let entry = res.map_err(|source| WalkError::ReadDir {
            path: dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        if is_unity_hidden(&name) {
            continue;
        }
        // `Foo.meta` → record the bare name `Foo` so the candidate test
        // is a hash lookup, not a stat. Skip blacklisted-extension files
        // (`.md`, `.pspec`, …) — they carry no Unity import semantics,
        // and synthesizing a `.meta` would pollute the asset-db's name
        // pool.
        if let Some(ext) = Path::new(&name).extension() {
            if ext == "meta" {
                if let Some(stem) = Path::new(&name).file_stem() {
                    metas.insert(stem.to_owned());
                }
                continue;
            }
            if is_blacklisted_extension(ext) {
                continue;
            }
        }
        let ft = entry.file_type().map_err(|source| WalkError::ReadDir {
            path: dir.to_path_buf(),
            source,
        })?;
        candidates.push(Candidate {
            name,
            path: entry.path(),
            is_dir: ft.is_dir(),
        });
    }
    let entry_depth = depth + 1;
    for c in &candidates {
        if entry_depth >= min_depth && !metas.contains(&c.name) {
            hits.push((c.path.clone(), c.is_dir));
        }
    }
    for c in candidates {
        if c.is_dir && !is_opaque_subtree(&c.name, &c.path) {
            walk_dir_collect(&c.path, entry_depth, min_depth, hits)?;
        }
    }
    Ok(())
}

/// `WalkBuilder` preconfigured with Unity's hidden-file rule — the
/// reusable primitive for any tool that walks a Unity project tree.
///
/// `standard_filters(false)` because Unity doesn't honor `.gitignore`
/// either — gitignored `.meta` files still carry guids prefabs reference.
/// `is_unity_hidden` covers `.foo` / `foo~` per Unity's special-folder rule.
/// See [Walker ignore behavior](docs/asset-database.md#populating).
pub fn asset_walk_builder(root: &Path) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.standard_filters(false)
        .follow_links(false)
        .filter_entry(|e| !is_unity_hidden(e.file_name()));
    b
}

/// File extensions the asset-db refuses to index. Files with these
/// extensions exist inside `Assets/` (often as siblings of real assets)
/// but carry no Unity import semantics worth tracking — excluding them
/// keeps the name pool focused on real assets and avoids spurious
/// `.meta` synthesis for documentation and tool source files.
///
/// Current set: `md` (markdown docs), `pspec` (pspec serializer source),
/// `py` / `exe` (vendored tool helpers shipped inside UPM packages, e.g.
/// Firebase's `generate_xml_from_google_services_json`), `pdb` (debug
/// symbol files paired with managed `.dll` plugins), `asmdef` / `asmref`
/// (Unity assembly-definition assets — GUID-identified by downstream
/// consumers, vendored packages routinely ship `Editor/Assembly.asmref`
/// at identical depth-2 paths). Match is case-sensitive — Unity itself
/// is case-sensitive for asset paths on Linux build agents, and a `.MD`
/// / `.PY` file is rare enough not to warrant a normalization step here.
fn is_blacklisted_extension(ext: &std::ffi::OsStr) -> bool {
    matches!(
        ext.as_encoded_bytes(),
        b"md" | b"pspec" | b"py" | b"exe" | b"pdb" | b"asmdef" | b"asmref",
    )
}

/// `Foo.md.meta` → `true`; `Foo.prefab.meta` → `false`.
///
/// Inspects the "inner" extension of a `.meta` path — i.e. the extension
/// of the path with `.meta` stripped. Used by [`walk_meta_files`] to
/// skip `.meta` files that belong to blacklisted-extension assets before
/// they enter the parser pipeline. Borrow-only — runs once per `.meta`
/// in the project, so no per-entry allocation.
fn meta_targets_blacklisted_ext(meta_path: &Path) -> bool {
    meta_path
        .file_stem()
        .and_then(|s| Path::new(s).extension())
        .is_some_and(is_blacklisted_extension)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn rejects_non_project() {
        let tmp = std::env::temp_dir().join(format!("unity-assetdb-walk-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let result = resolve_project_root(Some(&tmp));
        assert!(result.is_err());
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Recognized non-asset extensions: markdown docs and pspec source
    /// files. These sit inside `Assets/` but are not themselves assets —
    /// the bake excludes them from indexing entirely.
    #[test]
    fn is_blacklisted_extension_known_set() {
        for ext in ["md", "pspec", "py", "exe", "pdb", "asmdef", "asmref"] {
            assert!(
                is_blacklisted_extension(OsStr::new(ext)),
                "{ext} should be blacklisted",
            );
        }
    }

    #[test]
    fn is_blacklisted_extension_rejects_real_assets() {
        for ext in ["prefab", "asset", "png", "controller", "mat", "anim", "txt", "json"] {
            assert!(
                !is_blacklisted_extension(OsStr::new(ext)),
                "{ext} should NOT be classified as blacklisted",
            );
        }
    }

    #[test]
    fn meta_targets_blacklisted_ext_inspects_inner_extension() {
        // `Foo.md.meta` → blacklisted; `Foo.prefab.meta` → asset.
        assert!(meta_targets_blacklisted_ext(Path::new("UI/Foo.md.meta")));
        assert!(meta_targets_blacklisted_ext(Path::new("UI/Foo.pspec.meta")));
        assert!(!meta_targets_blacklisted_ext(Path::new("UI/Foo.prefab.meta")));
        assert!(!meta_targets_blacklisted_ext(Path::new("UI/Foo.asset.meta")));
        // A bare `.meta` (no inner extension) is malformed but must not
        // be misclassified as blacklisted.
        assert!(!meta_targets_blacklisted_ext(Path::new("Foo.meta")));
    }
}
