//! Auto-refresh orchestrator — wires [`crate::watch::since`] into the
//! bake/patch decision and rewrites the bin.
//!
//! See [`docs/refresh.md`](../../docs/refresh.md). Patch reuses
//! [`crate::bake::build_db_from_raw`] over (surviving entries +
//! re-parsed delta) rather than re-implementing incremental dedup;
//! costs ~20 ms warm for any sub-[`PATCH_THRESHOLD`] patch.

use std::path::Path;

use crate::bake::{self, BakeError, BakeOptions, RawEntry, raw_from_entry};
use crate::store::{self, AssetDb, StoreError};
use crate::watch::{self, Delta, WatchError};

/// Above this many touched hints, full-bake instead of patching.
/// Sequential parse runs ~100 µs/file; the parallel walk in
/// [`bake::bake`] wins above ~5 000.
pub const PATCH_THRESHOLD: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Watchman delta was empty; only the clock advanced.
    ClockOnly,
    /// Patched in place. `usize` is hints fed to [`patch`] (may include
    /// touches that re-parsed to identical entries).
    Patched(usize),
    /// Full re-bake. Fires when there's no prior clock, Watchman is
    /// unavailable / errors / returns a `Fresh` instance, or the delta
    /// exceeds [`PATCH_THRESHOLD`].
    Rebaked,
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("{0}")]
    Bake(#[from] BakeError),
    #[error("{0}")]
    Store(#[from] StoreError),
}

/// Refresh `db` against the live project tree, writing the result to
/// `<out_dir>/asset-db.bin`. The caller owns the in-memory `AssetDb`;
/// it's mutated in place when we patch, or replaced wholesale when we
/// rebake.
///
/// Output discipline matches the bake library: progress / warning sinks
/// are caller-supplied. Stderr is never written by this module.
pub fn refresh(
    db: &mut AssetDb,
    project_root: &Path,
    out_dir: &Path,
    on_warn: Option<&dyn Fn(&str)>,
) -> Result<RefreshOutcome, RefreshError> {
    // No clock to query against → seed via a full bake. This is the
    // first-run / post-schema-bump path.
    let Some(prev_clock) = db.watchman_clock.clone() else {
        full_bake_into(db, project_root, out_dir)?;
        return Ok(RefreshOutcome::Rebaked);
    };

    let delta = match watch::since(project_root, Some(&prev_clock)) {
        Ok(d) => d,
        Err(WatchError::Unavailable) => {
            if let Some(sink) = on_warn {
                sink(
                    "watchman unavailable; install (brew install watchman) for incremental updates",
                );
            }
            full_bake_into(db, project_root, out_dir)?;
            return Ok(RefreshOutcome::Rebaked);
        }
        Err(WatchError::Query(e)) => {
            if let Some(sink) = on_warn {
                sink(&format!("watchman query failed: {e}; falling back to full bake"));
            }
            full_bake_into(db, project_root, out_dir)?;
            return Ok(RefreshOutcome::Rebaked);
        }
    };

    match delta {
        Delta::Fresh { .. } => {
            full_bake_into(db, project_root, out_dir)?;
            Ok(RefreshOutcome::Rebaked)
        }
        Delta::Touched { hints, new_clock } => {
            if hints.is_empty() {
                // Skip the bin rewrite on no-op deltas — the in-memory
                // clock bump is enough until the next mutation. If the
                // journal rolls past, the next query gets `Fresh` and
                // full-bakes anyway.
                db.watchman_clock = Some(new_clock);
                return Ok(RefreshOutcome::ClockOnly);
            }
            if hints.len() > PATCH_THRESHOLD {
                full_bake_into(db, project_root, out_dir)?;
                return Ok(RefreshOutcome::Rebaked);
            }
            let count = hints.len();
            patch(db, project_root, &hints)?;
            db.watchman_clock = Some(new_clock);
            store::write(&store::db_path(out_dir), db)?;
            Ok(RefreshOutcome::Patched(count))
        }
    }
}

/// Full-bake then re-read into `db`. Bake sinks are silenced; refresh
/// has already surfaced a one-line reason via its own warn sink.
fn full_bake_into(
    db: &mut AssetDb,
    project_root: &Path,
    out_dir: &Path,
) -> Result<(), RefreshError> {
    let opts = BakeOptions {
        project_root: project_root.to_path_buf(),
        out_dir: out_dir.to_path_buf(),
        name_sanitizer: None,
        on_warn: None,
        on_progress: None,
        verbose_timing: false,
        verbose_collisions: false,
    };
    bake::bake(&opts)?;
    *db = store::read(&store::db_path(out_dir))?;
    Ok(())
}

/// Incremental patch. Re-parse touched hints, splice into `db`, re-run
/// dedup via [`bake::build_db_from_raw`]. Caller updates the clock and
/// writes the bin. `schema_version` + `watchman_clock` are preserved.
///
/// Hints may carry the `.meta` suffix; we normalize to the companion.
pub(crate) fn patch(
    db: &mut AssetDb,
    project_root: &Path,
    hints: &[String],
) -> Result<(), BakeError> {
    let mut canonical: Vec<String> = hints
        .iter()
        .map(|h| {
            h.strip_suffix(".meta")
                .map(str::to_owned)
                .unwrap_or_else(|| h.clone())
        })
        .collect();
    canonical.sort();
    canonical.dedup();

    // Re-parse each hint. Missing meta on disk → treat as delete.
    let mut parsed: Vec<(String, Option<RawEntry>)> = Vec::with_capacity(canonical.len());
    for hint in &canonical {
        let meta_path = project_root.join(format!("{hint}.meta"));
        if !meta_path.exists() {
            parsed.push((hint.clone(), None));
            continue;
        }
        let raw = bake::parse_one_raw(project_root, &meta_path)?;
        parsed.push((hint.clone(), raw));
    }

    // Two-set drop: touched_hints catches the common case + symmetric
    // renames; new_guids catches asymmetric renames where Watchman lost
    // the old-hint deletion (without it two rows could share a GUID).
    let touched_hints: ahash::AHashSet<&str> =
        parsed.iter().map(|(h, _)| h.as_str()).collect();
    let new_guids: ahash::AHashSet<u128> = parsed
        .iter()
        .filter_map(|(_, r)| r.as_ref().map(|raw| raw.guid))
        .collect();
    let mut to_drop_guids: ahash::AHashSet<u128> = ahash::AHashSet::new();
    for entry in &db.entries {
        if touched_hints.contains(entry.hint.as_ref())
            || new_guids.contains(&entry.guid)
        {
            to_drop_guids.insert(entry.guid);
        }
    }

    let mut raw: Vec<RawEntry> = Vec::with_capacity(db.entries.len() + parsed.len());
    for entry in &db.entries {
        if to_drop_guids.contains(&entry.guid) {
            continue;
        }
        raw.push(raw_from_entry(entry, &db.script_types));
    }
    for (_, opt) in parsed {
        if let Some(r) = opt {
            raw.push(r);
        }
    }

    let new_db = bake::build_db_from_raw(raw)?;
    db.entries = new_db.entries;
    db.script_types = new_db.script_types;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{unique_tmp as tmp, write};
    use std::fs;
    use std::path::PathBuf;

    fn unique_tmp(label: &str) -> PathBuf {
        tmp("refresh", label)
    }

    fn fixture(root: &Path) {
        fs::create_dir_all(root.join("ProjectSettings")).unwrap();
        write(
            &root.join("ProjectSettings/ProjectVersion.txt"),
            "m_EditorVersion: 2022.3.0f1\n",
        );
        write(
            &root.join("Assets/UI/Foo.prefab"),
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1001 &100100000\nPrefabInstance:\n  m_ObjectHideFlags: 0\n",
        );
        write(
            &root.join("Assets/UI/Foo.prefab.meta"),
            "fileFormatVersion: 2\nguid: aaaa1111aaaa1111aaaa1111aaaa1111\nPrefabImporter:\n",
        );
    }

    fn bake_fresh(root: &Path) -> AssetDb {
        let out_dir = root.join("Library/unity-assetdb");
        bake::bake(&BakeOptions {
            project_root: root.to_path_buf(),
            out_dir: out_dir.clone(),
            name_sanitizer: None,
            on_warn: None,
            on_progress: None,
            verbose_timing: false,
            verbose_collisions: false,
        })
        .unwrap();
        store::read(&store::db_path(&out_dir)).unwrap()
    }

    /// New asset → patch adds it to the db.
    #[test]
    fn patch_add() {
        let root = unique_tmp("patch-add");
        let _ = fs::remove_dir_all(&root);
        fixture(&root);
        let mut db = bake_fresh(&root);
        let initial_len = db.entries.len();

        // Add a new asset.
        write(
            &root.join("Assets/SO/Bar.asset"),
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nMonoBehaviour:\n  m_Name: Bar\n",
        );
        write(
            &root.join("Assets/SO/Bar.asset.meta"),
            "fileFormatVersion: 2\nguid: bbbb2222bbbb2222bbbb2222bbbb2222\nNativeFormatImporter:\n",
        );

        patch(&mut db, &root, &["Assets/SO/Bar.asset".to_owned()]).unwrap();

        assert_eq!(db.entries.len(), initial_len + 1);
        assert!(
            db.find_by_guid(0xbbbb2222bbbb2222bbbb2222bbbb2222_u128)
                .is_some(),
            "new Bar.asset not patched in",
        );
        fs::remove_dir_all(&root).ok();
    }

    /// Modify an existing asset (rewrite the meta — same GUID, different
    /// importer) → patch refreshes the entry.
    #[test]
    fn patch_modify() {
        let root = unique_tmp("patch-modify");
        let _ = fs::remove_dir_all(&root);
        fixture(&root);
        let mut db = bake_fresh(&root);

        // Existing Foo.prefab has importer PrefabImporter. We'll
        // synthesize a small modification: append a no-op line. The
        // GUID stays, but the parse re-runs.
        let original_guid = db
            .find_by_guid(0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
            .expect("Foo must exist")
            .guid;
        fs::write(
            root.join("Assets/UI/Foo.prefab.meta"),
            "fileFormatVersion: 2\nguid: aaaa1111aaaa1111aaaa1111aaaa1111\nPrefabImporter:\n  userData:\n",
        )
        .unwrap();

        patch(&mut db, &root, &["Assets/UI/Foo.prefab".to_owned()]).unwrap();

        let entry = db.find_by_guid(original_guid).expect("still present");
        assert_eq!(&*entry.hint, "Assets/UI/Foo.prefab");
        fs::remove_dir_all(&root).ok();
    }

    /// Delete an asset (remove both files) → patch drops the entry.
    #[test]
    fn patch_delete() {
        let root = unique_tmp("patch-delete");
        let _ = fs::remove_dir_all(&root);
        fixture(&root);
        let mut db = bake_fresh(&root);
        assert!(
            db.find_by_guid(0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
                .is_some(),
            "Foo must exist before delete",
        );

        fs::remove_file(root.join("Assets/UI/Foo.prefab")).unwrap();
        fs::remove_file(root.join("Assets/UI/Foo.prefab.meta")).unwrap();

        patch(&mut db, &root, &["Assets/UI/Foo.prefab".to_owned()]).unwrap();

        assert!(
            db.find_by_guid(0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
                .is_none(),
            "Foo entry should be dropped after delete",
        );
        fs::remove_dir_all(&root).ok();
    }

    /// Rename within the same extension: same GUID at a new hint.
    /// Watchman reports old deletion + new creation as two hints in
    /// one call; patch should leave one entry at the new path.
    #[test]
    fn patch_rename_same_ext() {
        let root = unique_tmp("patch-rename-same");
        let _ = fs::remove_dir_all(&root);
        fixture(&root);
        let mut db = bake_fresh(&root);

        // Physically move the asset to a new path, GUID intact.
        fs::create_dir_all(root.join("Assets/Moved")).unwrap();
        fs::rename(
            root.join("Assets/UI/Foo.prefab"),
            root.join("Assets/Moved/Foo.prefab"),
        )
        .unwrap();
        fs::rename(
            root.join("Assets/UI/Foo.prefab.meta"),
            root.join("Assets/Moved/Foo.prefab.meta"),
        )
        .unwrap();

        patch(
            &mut db,
            &root,
            &[
                "Assets/UI/Foo.prefab".to_owned(),
                "Assets/Moved/Foo.prefab".to_owned(),
            ],
        )
        .unwrap();

        let entry = db
            .find_by_guid(0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
            .expect("rename preserves GUID");
        assert_eq!(&*entry.hint, "Assets/Moved/Foo.prefab");
        // Pin "no duplicates": only ONE entry carries this GUID.
        let count = db
            .entries
            .iter()
            .filter(|e| e.guid == 0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
            .count();
        assert_eq!(count, 1);
        fs::remove_dir_all(&root).ok();
    }

    /// Watchman reports a hint that no longer exists on disk by the
    /// time we stat it (rapid edit-then-delete). Patch must treat that
    /// as a deletion, not a panic / error.
    #[test]
    fn patch_handles_vanished_hint() {
        let root = unique_tmp("patch-vanished");
        let _ = fs::remove_dir_all(&root);
        fixture(&root);
        let mut db = bake_fresh(&root);

        // Feed a hint whose meta file we never created. patch should
        // attempt to delete-if-exists; since it's not in the db, it's
        // a clean no-op.
        patch(
            &mut db,
            &root,
            &["Assets/UI/Ghost.prefab".to_owned()],
        )
        .unwrap();

        // db unchanged.
        assert!(
            db.find_by_guid(0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
                .is_some(),
        );
        fs::remove_dir_all(&root).ok();
    }

    /// Empty hints list is a true no-op. Caller may still bump the
    /// clock externally; `patch` itself should not touch the db.
    #[test]
    fn patch_empty_hints_noop() {
        let root = unique_tmp("patch-empty");
        let _ = fs::remove_dir_all(&root);
        fixture(&root);
        let mut db = bake_fresh(&root);
        let before = db.entries.clone();

        patch(&mut db, &root, &[]).unwrap();

        assert_eq!(db.entries.len(), before.len());
        for (a, b) in db.entries.iter().zip(before.iter()) {
            assert_eq!(a.guid, b.guid);
            assert_eq!(a.hint, b.hint);
        }
        fs::remove_dir_all(&root).ok();
    }

    /// Hint with `.meta` suffix normalizes to the companion. Watchman
    /// reports both forms; patch should not double-count.
    #[test]
    fn patch_normalizes_meta_suffix() {
        let root = unique_tmp("patch-normalize");
        let _ = fs::remove_dir_all(&root);
        fixture(&root);
        let mut db = bake_fresh(&root);

        // Both forms of the same edit.
        patch(
            &mut db,
            &root,
            &[
                "Assets/UI/Foo.prefab".to_owned(),
                "Assets/UI/Foo.prefab.meta".to_owned(),
            ],
        )
        .unwrap();

        // Exactly one entry survives.
        let count = db
            .entries
            .iter()
            .filter(|e| e.guid == 0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
            .count();
        assert_eq!(count, 1);
        fs::remove_dir_all(&root).ok();
    }
}
