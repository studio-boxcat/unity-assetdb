//! Filesystem predicates that mirror Unity's importer rules.
//!
//! These are the *universal* Unity-side rules — anything Unity's importer
//! ignores at the filesystem level. They are intentionally scoped to that:
//! tool-specific exclusions (e.g. an asset-db dropping `.md` / `.asmdef`
//! from its name pool, or a meta-pairing checker allowing `.tmp`) belong
//! to the calling tool, not here.
//!
//! See <https://docs.unity3d.com/Manual/SpecialFolders.html> and
//! <https://docs.unity3d.com/Manual/android-library-project-import.html>.

use std::ffi::OsStr;
use std::path::Path;

/// `true` if Unity's importer would hide this entry — name starts with
/// `.` or ends with `~`. Byte-level so non-UTF-8 names (rare on Unix)
/// don't slip past a `to_str()` check.
pub fn is_unity_hidden(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes.first() == Some(&b'.') || bytes.last() == Some(&b'~')
}

/// `true` if the folder is a folder-based Android plugin: `.androidlib`
/// (Gradle module), `.androidpack` (Play Asset Delivery), or `.aar`
/// (folder form of an Android archive). The folder itself is a Unity
/// asset (with its own `.meta`), but its *contents* are handed to
/// Gradle untouched and never carry `.meta` files.
pub fn is_opaque_plugin_dir(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e == "androidlib" || e == "androidpack" || e == "aar")
}

/// `true` if `dir` looks like a git submodule or nested independent
/// repo — `<dir>/.git` exists (as file or directory). Callers that walk
/// a Unity project usually want to avoid descending into these: they're
/// foreign working trees and writes there would dirty another repo.
pub fn is_submodule_root(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Combined "don't descend" predicate: an opaque plugin folder *or* a
/// submodule root. The folder itself may still be visited; only its
/// contents should be skipped.
pub fn is_opaque_subtree(name: &OsStr, path: &Path) -> bool {
    is_opaque_plugin_dir(name) || is_submodule_root(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_dot_prefix() {
        assert!(is_unity_hidden(OsStr::new(".git")));
        assert!(is_unity_hidden(OsStr::new(".DS_Store")));
    }

    #[test]
    fn hidden_tilde_suffix() {
        assert!(is_unity_hidden(OsStr::new("Backup~")));
        assert!(is_unity_hidden(OsStr::new("scratch.cs~")));
    }

    #[test]
    fn visible_names() {
        assert!(!is_unity_hidden(OsStr::new("Player.cs")));
        assert!(!is_unity_hidden(OsStr::new("My~Folder")));
        assert!(!is_unity_hidden(OsStr::new("Foo.bar")));
    }

    #[test]
    fn opaque_plugin_dirs() {
        assert!(is_opaque_plugin_dir(OsStr::new("AdMob.androidlib")));
        assert!(is_opaque_plugin_dir(OsStr::new("Levels.androidpack")));
        assert!(is_opaque_plugin_dir(OsStr::new("foo.aar")));
    }

    #[test]
    fn not_opaque_plugin_dirs() {
        assert!(!is_opaque_plugin_dir(OsStr::new("Plugins")));
        assert!(!is_opaque_plugin_dir(OsStr::new("foo.androidlib.txt")));
    }

    // ─── I/O predicates ────────────────────────────────────────────────
    //
    // Submodule detection touches the filesystem. We exercise it with
    // tempdirs constructed by hand (no `tempfile` dep — this crate has
    // no dev-deps and keeping that property is cheap).

    use std::fs;
    use std::path::PathBuf;

    fn unique_tmp(label: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("unity-path-rules-{label}-{pid}-{nanos}"))
    }

    /// Git directly clones into `<dir>/.git/` — the canonical submodule
    /// shape pre-modules-config. Most user projects look like this.
    #[test]
    fn submodule_root_detects_dot_git_directory() {
        let root = unique_tmp("submodule-dir");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".git")).unwrap();
        assert!(is_submodule_root(&root));
        fs::remove_dir_all(&root).ok();
    }

    /// `git submodule add` writes `<dir>/.git` as a *file* (a "gitlink"
    /// pointing at the parent repo's modules dir). Predicate must
    /// accept both forms, hence the `.exists()` choice over an
    /// `is_dir()` check.
    #[test]
    fn submodule_root_detects_dot_git_file_gitlink() {
        let root = unique_tmp("submodule-file");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(".git"),
            "gitdir: ../.git/modules/some-submodule\n",
        )
        .unwrap();
        assert!(is_submodule_root(&root));
        fs::remove_dir_all(&root).ok();
    }

    /// A regular directory with no `.git` sibling is not a submodule
    /// root. Pin the negative so a regression that reads e.g.
    /// `<dir>/foo` doesn't silently flip the predicate.
    #[test]
    fn submodule_root_rejects_plain_dir() {
        let root = unique_tmp("submodule-none");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("README"), "stub").unwrap();
        assert!(!is_submodule_root(&root));
        fs::remove_dir_all(&root).ok();
    }

    /// Non-existent path: `.git` lookup short-circuits to false via
    /// `Path::exists`. Pin so we don't accidentally bubble I/O errors
    /// out of a predicate that's supposed to be infallible.
    #[test]
    fn submodule_root_rejects_nonexistent_path() {
        let missing = unique_tmp("submodule-missing");
        assert!(!missing.exists(), "tempdir setup invariant");
        assert!(!is_submodule_root(&missing));
    }

    /// `is_opaque_subtree` is the OR of `is_opaque_plugin_dir(name)` +
    /// `is_submodule_root(path)`. Cover the four corners.
    #[test]
    fn opaque_subtree_plugin_only() {
        let root = unique_tmp("opaque-plugin");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // Name says "plugin", no .git — true via the plugin branch.
        assert!(is_opaque_subtree(OsStr::new("foo.androidlib"), &root));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn opaque_subtree_submodule_only() {
        let root = unique_tmp("opaque-submodule");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".git")).unwrap();
        // Name is a plain folder, but `.git` makes the path a
        // submodule root — true via the submodule branch.
        assert!(is_opaque_subtree(OsStr::new("Plugins"), &root));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn opaque_subtree_both_predicates() {
        let root = unique_tmp("opaque-both");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".git")).unwrap();
        // A `.aar`-named folder that's also a git submodule (pathological
        // but legal). Predicate is OR — true.
        assert!(is_opaque_subtree(OsStr::new("vendor.aar"), &root));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn opaque_subtree_neither_predicate() {
        let root = unique_tmp("opaque-neither");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // Plain folder, no `.git` — both branches false.
        assert!(!is_opaque_subtree(OsStr::new("Scripts"), &root));
        fs::remove_dir_all(&root).ok();
    }
}
