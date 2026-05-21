//! Watchman wire layer.
//!
//! See [`docs/refresh.md`](../../docs/refresh.md) for how this fits into
//! the auto-refresh pipeline.
//!
//! Sync facade — internally builds a current-thread tokio runtime per
//! call (~µs cost; one call per CLI invocation). Keeps tokio confined
//! to this module so the rest of the crate stays sync.

use std::path::{Path, PathBuf};

use watchman_client::Error as WatchmanError;
use watchman_client::prelude::*;

/// Result of a `since` query.
///
/// Two structural outcomes. Anything else (no daemon, daemon error)
/// surfaces via [`WatchError`].
#[derive(Debug)]
pub enum Delta {
    /// Watchman returned `is_fresh_instance` — daemon restart, journal
    /// loss, brand-new watch. Caller must full-bake.
    Fresh { new_clock: String },
    /// Steady-state delta. `hints` is the list of project-relative
    /// paths that changed (added, modified, or deleted — the patcher
    /// disambiguates by stat'ing each one). May be empty.
    Touched {
        hints: Vec<String>,
        new_clock: String,
    },
}

/// Errors from [`since`]. Both variants collapse to "full bake" at the
/// orchestrator; the split lets it print a one-line nudge for
/// `Unavailable` (suggest `brew install watchman`) and a different
/// log line for `Query` (which is exceptional, not a missing tool).
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// Watchman CLI not found, daemon socket unreachable, or discovery
    /// failed. The user-facing "you should install watchman" branch.
    #[error("watchman unavailable")]
    Unavailable,
    /// Anything else — BSER decode, query rejected, transport error.
    /// Exceptional; orchestrator logs and falls back to full bake.
    #[error("watchman query failed: {0}")]
    Query(#[from] anyhow::Error),
}

// `NameOnly` deserializes directly from the `name` string and is the
// cheapest field set Watchman supports. We don't need `exists` — the
// patcher stats each hint itself to decide delete-vs-parse, so the
// extra column is just bytes on the wire.

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

/// Query Watchman for everything that changed under `project_root`
/// since `prev_clock`. See [`Delta`] for the result shape and
/// [`WatchError`] for failure modes.
///
/// `prev_clock`: pass `None` for the first call on a project (Watchman
/// returns a `Fresh` delta with the new clock; orchestrator treats it
/// as full-bake). Pass `Some(s)` with a clock from a prior call to get
/// an incremental delta.
pub fn since(
    project_root: &Path,
    prev_clock: Option<&str>,
) -> Result<Delta, WatchError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(|e| {
            WatchError::Query(anyhow::Error::new(e).context("build tokio runtime"))
        })?;
    rt.block_on(since_inner(project_root, prev_clock))
}

async fn since_inner(
    project_root: &Path,
    prev_clock: Option<&str>,
) -> Result<Delta, WatchError> {
    let client = Connector::new().connect().await.map_err(map_connect_err)?;

    let canonical = CanonicalPath::canonicalize(project_root).map_err(|e| {
        WatchError::Query(anyhow::Error::new(e).context("canonicalize project_root"))
    })?;
    let resolved = client
        .resolve_root(canonical)
        .await
        .map_err(|e| WatchError::Query(anyhow::Error::new(e)))?;

    let expression = Expr::All(vec![
        Expr::Any(
            TOPLEVEL_DIRS
                .iter()
                .map(|d| {
                    Expr::DirName(DirNameTerm {
                        path: PathBuf::from(d),
                        depth: None,
                    })
                })
                .collect(),
        ),
        Expr::Suffix(SUFFIXES.iter().map(PathBuf::from).collect()),
    ]);

    let request = QueryRequestCommon {
        since: prev_clock
            .map(|c| Clock::Spec(ClockSpec::StringClock(c.to_owned()))),
        expression: Some(expression),
        ..Default::default()
    };

    let result = client
        .query::<NameOnly>(&resolved, request)
        .await
        .map_err(|e| WatchError::Query(anyhow::Error::new(e)))?;

    let new_clock = clock_to_string(result.clock)?;

    if result.is_fresh_instance {
        return Ok(Delta::Fresh { new_clock });
    }

    let hints: Vec<String> = result
        .files
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            // Watchman returns paths relative to the watch root. With
            // `relative_root` auto-set from ResolvedRoot, they're
            // relative to the project root — which is exactly the
            // shape our `hint` strings use.
            row.name.into_inner().into_os_string().into_string().ok()
        })
        .collect();

    Ok(Delta::Touched { hints, new_clock })
}

fn clock_to_string(clock: Clock) -> Result<String, WatchError> {
    match clock {
        Clock::Spec(ClockSpec::StringClock(s)) => Ok(s),
        // UnixTimestamp clocks aren't emitted by the server in normal
        // operation; we requested a string-clock-shaped `since`.
        Clock::Spec(ClockSpec::UnixTimestamp(t)) => Ok(t.to_string()),
        // We don't request SCM-aware queries; if the server volunteered
        // one, something's wrong upstream. Surface it as a Query error
        // so the orchestrator full-bakes; better than silently dropping
        // the scm metadata and continuing with a partial clock.
        Clock::ScmAware(_) => Err(WatchError::Query(anyhow::anyhow!(
            "watchman returned an SCM-aware clock; not requested",
        ))),
    }
}

/// Watchman returns very different errors for "binary not installed"
/// (`ConnectionDiscovery`, from the CLI subprocess used to find the
/// socket) vs. "daemon not running" (`Connect`, from the socket open).
/// We treat both as "unavailable" — the user-facing fix is the same
/// (`brew install watchman` or restart the daemon).
fn map_connect_err(e: WatchmanError) -> WatchError {
    match e {
        WatchmanError::ConnectionDiscovery { .. } | WatchmanError::Connect { .. } => {
            WatchError::Unavailable
        }
        other => WatchError::Query(anyhow::Error::new(other)),
    }
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
