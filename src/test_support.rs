//! Shared `#[cfg(test)]` helpers for internal unit tests
//! (`src/refresh.rs::tests`, `src/watch.rs::tests`). Integration
//! tests under `tests/` are a separate crate and re-roll their own
//! versions — sharing across the crate boundary would force these
//! helpers into the public API.

use std::path::{Path, PathBuf};

/// Per-test temp dir name. Collision-resistant under parallel
/// `cargo test` via PID + nanos; caller is responsible for cleanup.
pub(crate) fn unique_tmp(prefix: &str, label: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("unity-assetdb-{prefix}-{label}-{pid}-{nanos}"))
}

/// `mkdir -p` parent + write body.
pub(crate) fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}
