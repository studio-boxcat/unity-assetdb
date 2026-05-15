//! Parallel walker over a Unity project's `Assets/` tree.
//!
//! Uses [`ignore::WalkBuilder`] with gitignore on (default) — the same
//! mechanism `rg` / `fd` use, so anything the user already gitignores
//! (Library/, Temp/, build artifacts) is skipped automatically.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore::WalkBuilder;

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
/// Sequential (single-threaded) — the candidate set is small relative
/// to the parallel `walk_meta_files` work, and synthesis I/O serializes
/// on the same out-dir lock anyway.
pub fn walk_for_missing_meta<F>(project_root: &Path, mut visit: F) -> Result<(), WalkError>
where
    F: FnMut(&Path, bool),
{
    let assets = project_root.join("Assets");
    if !assets.is_dir() {
        return Err(WalkError::AssetsMissing { path: assets });
    }
    walk_dir_for_missing_meta(&assets, 0, ASSETS_MIN_DEPTH, &mut visit)?;

    let packages = project_root.join("Packages");
    if packages.is_dir() {
        walk_dir_for_missing_meta(&packages, 0, PACKAGES_MIN_DEPTH, &mut visit)?;
    }
    Ok(())
}

/// Synthesize at every depth under `Assets/`.
const ASSETS_MIN_DEPTH: usize = 1;
/// Skip `Packages/` direct children — embedded UPM package roots and
/// `manifest.json` / `packages-lock.json` carry no `.meta`.
const PACKAGES_MIN_DEPTH: usize = 2;

/// Manual recursive walker for the missing-meta pre-pass. Pre-order so
/// the folder itself is reported before descent. Hand-rolled (not
/// `ignore::Walk`) because we need to visit an opaque folder root then
/// refuse to descend into it — a distinction `ignore`'s `filter_entry`
/// can't express (filtering a dir suppresses both visit and recursion).
fn walk_dir_for_missing_meta<F>(
    dir: &Path,
    depth: usize,
    min_depth: usize,
    visit: &mut F,
) -> Result<(), WalkError>
where
    F: FnMut(&Path, bool),
{
    let rd = std::fs::read_dir(dir).map_err(|source| WalkError::ReadDir {
        path: dir.to_path_buf(),
        source,
    })?;
    for res in rd {
        let entry = res.map_err(|source| WalkError::ReadDir {
            path: dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        if is_unity_hidden(&name) {
            continue;
        }
        // Skip any `.meta` entry (file or — pathologically — a dir
        // named `Foo.meta`); synthesizing `Foo.meta.meta` is never right.
        // Also skip blacklisted-extension files (`.md`, `.pspec`) — they
        // carry no Unity import semantics, and a synthesized `.meta`
        // would pollute the asset-db's name pool.
        if let Some(ext) = Path::new(&name).extension() {
            if ext == "meta" || is_blacklisted_extension(ext) {
                continue;
            }
        }
        let ft = entry.file_type().map_err(|source| WalkError::ReadDir {
            path: dir.to_path_buf(),
            source,
        })?;
        let is_dir = ft.is_dir();
        let path = entry.path();
        let entry_depth = depth + 1;
        if entry_depth >= min_depth {
            let mut meta_os = path.as_os_str().to_owned();
            meta_os.push(".meta");
            if !Path::new(&meta_os).exists() {
                visit(&path, is_dir);
            }
        }
        if is_dir && !is_opaque_subtree(&name, &path) {
            walk_dir_for_missing_meta(&path, entry_depth, min_depth, visit)?;
        }
    }
    Ok(())
}

/// A directory whose *contents* should never receive synthesized
/// `.meta` files: folder-based Android plugins (Unity hands them to
/// Gradle as-is) and git submodule roots (foreign working trees).
fn is_opaque_subtree(name: &std::ffi::OsStr, path: &Path) -> bool {
    is_opaque_plugin_dir(name) || is_submodule_root(path)
}

/// Folder-based Android plugin names per Unity's manifest:
/// `.androidlib` (Gradle module), `.androidpack` (Play Asset Delivery),
/// `.aar` (folder form of an Android archive).
/// <https://docs.unity3d.com/Manual/android-library-project-import.html>
fn is_opaque_plugin_dir(name: &std::ffi::OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e == "androidlib" || e == "androidpack" || e == "aar")
}

/// Git submodule (or nested independent repo) root: `<dir>/.git` exists
/// as either a file (submodule pointer) or directory (worktree-style
/// embedded repo). Either case means the subtree is owned by another
/// repo and our synthesized `.meta` files would show up as dirty.
fn is_submodule_root(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Shared `WalkBuilder` config for every asset walk in this crate.
///
/// `standard_filters(false)`: gitignore parsing in a Unity project is a
/// net loss — `Library/` + `Temp/` + build artifacts live outside
/// `Assets/` and `Packages/`. Inside-Assets `.gitignore` files exist
/// (Zenject codegen, scratch dirs, SmartLibrary `.asset` exclusions)
/// but Unity doesn't honor them either — gitignored `.meta` files
/// still carry guids that prefabs can reference, so the asset DB must
/// include them. See [Walker ignore behavior](docs/asset-database.md#populating).
/// `is_unity_hidden` covers `.foo` and `foo~` per Unity's special-folder rule.
fn asset_walk_builder(root: &Path) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.standard_filters(false)
        .follow_links(false)
        .filter_entry(|e| !is_unity_hidden(e.file_name()));
    b
}

/// Unity-hidden file-name predicate: `.foo` or `foo~` per Unity's
/// special-folder rule. Byte-level — non-UTF-8 filenames (rare but
/// possible on Unix) would silently slip through a `to_str()` check.
pub(crate) fn is_unity_hidden(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes.first() == Some(&b'.') || bytes.last() == Some(&b'~')
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
