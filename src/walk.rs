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

    let mut builder = WalkBuilder::new(&assets);
    if packages.is_dir() {
        builder.add(&packages);
    }
    // standard_filters(false): gitignore parsing in a Unity project is a
    // net loss — Library/ + Temp/ + build artifacts live outside Assets/
    // and Packages/. Inside-Assets `.gitignore` files exist (Zenject
    // codegen, scratch dirs, SmartLibrary `.asset` exclusions) but Unity
    // doesn't honor them either — gitignored .meta files still carry
    // guids that prefabs can reference, so the asset DB must include
    // them. See [Walker ignore behavior](docs/asset-database.md#populating).
    // `is_unity_hidden` covers `.foo` and `foo~` per Unity's special-folder rule.
    let walker = builder
        .standard_filters(false)
        .follow_links(false)
        .filter_entry(|e| !is_unity_hidden(e.file_name()))
        .build_parallel();

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
                if path.extension().is_some_and(|e| e == "meta") {
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

/// Unity-hidden file-name predicate: `.foo` or `foo~` per Unity's
/// special-folder rule. Byte-level — non-UTF-8 filenames (rare but
/// possible on Unix) would silently slip through a `to_str()` check.
pub(crate) fn is_unity_hidden(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes.first() == Some(&b'.') || bytes.last() == Some(&b'~')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_project() {
        let tmp = std::env::temp_dir().join(format!("unity-assetdb-walk-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let result = resolve_project_root(Some(&tmp));
        assert!(result.is_err());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
