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
}
