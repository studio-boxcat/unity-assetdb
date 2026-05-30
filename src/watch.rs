//! Watchman wire layer — thin wrapper over the shared [`unity_watch`] crate.
//!
//! See [`docs/refresh.md`](../../docs/refresh.md) for how this fits into
//! the auto-refresh pipeline. The generic `since`/`Delta`/`WatchError`
//! machinery (sync facade over `watchman_client`) lives in the standalone
//! [`unity-watch`](https://github.com/studio-boxcat/unity-watch) crate; this
//! module only pins the project-specific [`Filter`] — which dirs +
//! extensions the GUID baker cares about.

use std::path::Path;

use unity_watch::Filter;
pub use unity_watch::{Delta, WatchError};

/// File suffixes Watchman should report changes for. Drives the
/// `Suffix` filter so we never see `Library/`, `Temp/`, or build
/// artifacts even though Watchman roots at a higher ancestor.
///
/// Kept in sync (by intent, not by code) with [`crate::class_id::class_from_ext`].
/// Drift here is benign — extra suffixes cost noise, missing ones make
/// us full-bake on the next query.
const SUFFIXES: &[&str] = &[
    "meta",
    // Unity scene/serialized assets
    "prefab", "asset", "anim", "controller", "mat", "mask", "mixer",
    "playable", "spriteatlas", "spriteatlasv2", "unity",
    // Source media
    "fbx", "obj", "blend", "dae",
    "png", "jpg", "jpeg", "psd", "tga", "tif", "tiff", "exr", "hdr", "gif", "bmp",
    "wav", "ogg", "mp3", "aif", "aiff",
    // Code + shaders
    "cs", "shader", "compute", "cginc", "hlsl", "glslinc",
    // Fonts + text
    "ttf", "otf",
    "txt", "json", "xml", "yaml", "yml",
    // Misc
    "dll", "so", "dylib",
];

/// Top-level directories the Unity project owns. Limits Watchman's
/// scan to these subtrees of the resolved root.
const TOPLEVEL_DIRS: &[&str] = &["Assets", "Packages", "ProjectSettings"];

/// Query Watchman for everything (matching the GUID-baker [`Filter`])
/// that changed under `project_root` since `prev_clock`. See [`Delta`]
/// for the result shape and [`WatchError`] for failure modes.
///
/// `prev_clock`: pass `None` for the first call on a project (Watchman
/// returns a `Fresh` delta with the new clock; orchestrator treats it
/// as full-bake). Pass `Some(s)` with a clock from a prior call to get
/// an incremental delta.
pub fn since(project_root: &Path, prev_clock: Option<&str>) -> Result<Delta, WatchError> {
    unity_watch::since(
        project_root,
        prev_clock,
        &Filter::new(TOPLEVEL_DIRS, SUFFIXES),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_tmp as tmp;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    fn unique_tmp(label: &str) -> PathBuf {
        tmp("watch", label)
    }

    /// Integration test against a real Watchman daemon. Gated with
    /// `#[ignore]` so `cargo test` stays green on machines without
    /// Watchman; run with `cargo test --ignored watch::tests::` to
    /// exercise.
    ///
    /// Verifies the end-to-end happy path: first `since(None)` returns
    /// `Fresh`; second `since(Some(clock))` against the same project
    /// returns `Touched` with the changed file included.
    #[test]
    #[ignore = "requires watchman daemon"]
    fn since_returns_fresh_then_touched() {
        let root = unique_tmp("watch-since");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Assets/UI")).unwrap();
        fs::create_dir_all(root.join("ProjectSettings")).unwrap();
        fs::write(
            root.join("ProjectSettings/ProjectVersion.txt"),
            "m_EditorVersion: 2022.3.0f1\n",
        )
        .unwrap();
        fs::write(root.join("Assets/UI/Foo.prefab"), "stub").unwrap();
        fs::write(
            root.join("Assets/UI/Foo.prefab.meta"),
            "fileFormatVersion: 2\nguid: deadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();

        // First call seeds the watch — expect Fresh.
        let first = since(&root, None).expect("watchman should be running");
        let clock = match first {
            Delta::Fresh { new_clock } => new_clock,
            Delta::Touched { .. } => panic!("expected Fresh on first call"),
        };

        // Force a settle so the second `since` definitely sees the touch.
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(root.join("Assets/UI/Foo.prefab.meta"), "stub\n").unwrap();

        let second = since(&root, Some(&clock)).expect("watchman should be running");
        match second {
            Delta::Touched { hints, .. } => {
                assert!(
                    hints.iter().any(|h| h.ends_with("Foo.prefab.meta")),
                    "expected touched hint for Foo.prefab.meta, got {hints:?}",
                );
            }
            Delta::Fresh { .. } => panic!("expected Touched on second call"),
        }

        // Best-effort cleanup. `watch-del` would be tidier but we don't
        // depend on it.
        let _ = Command::new("watchman").arg("watch-del").arg(&root).status();
        fs::remove_dir_all(&root).ok();
    }

    /// `since` against a path with no Watchman daemon should not panic.
    /// We can't reliably test the daemon-absent case in CI (it might be
    /// installed), but we can at least exercise that the error path
    /// returns and not crash. Runs unconditionally because it doesn't
    /// require a daemon (any path is acceptable).
    #[test]
    fn since_does_not_panic_on_nonexistent_path() {
        let result = since(Path::new("/nonexistent/unity-assetdb-test-path"), None);
        // Either Unavailable (no daemon) or Query (daemon rejected the
        // path). Both fine — assertion is that we got an Err.
        assert!(result.is_err(), "expected Err for non-existent path");
    }
}
