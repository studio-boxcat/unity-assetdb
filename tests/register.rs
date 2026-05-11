//! End-to-end test for `register` — bakes a small project, then exercises
//! both the new-meta and idempotent paths.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use unity_assetdb::bake::{BakeOptions, bake};
use unity_assetdb::query;
use unity_assetdb::register::{ImporterKind, RegisterError, RegisterOptions, register};
use unity_assetdb::store;

fn write(path: &Path, body: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn unique_tmp(label: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("unity-assetdb-register-test-{label}-{pid}-{nanos}"))
}

fn out_dir_for(root: &Path) -> PathBuf {
    root.join("Library").join("unity-assetdb")
}

/// Minimal Unity project skeleton — just ProjectSettings + Assets/.
fn make_empty_project(root: &Path) {
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );
    fs::create_dir_all(root.join("Assets")).unwrap();
}

/// Initial bake to materialize a (possibly empty) `asset-db.bin` so
/// `register` has a file to mutate.
fn bake_at(root: &Path) -> PathBuf {
    let out_dir = out_dir_for(root);
    let opts = BakeOptions {
        project_root: root.to_path_buf(),
        out_dir: out_dir.clone(),
        name_sanitizer: None,
        on_warn: None,
        on_progress: None,
        verbose_timing: false,
        verbose_collisions: false,
    };
    bake(&opts).unwrap();
    out_dir
}

fn register_opts(root: &Path, out_dir: &Path, target: PathBuf) -> RegisterOptions {
    RegisterOptions {
        project_root: root.to_path_buf(),
        out_dir: out_dir.to_path_buf(),
        target,
        importer_override: None,
        scrub_chars: None,
        lock_timeout: Duration::from_secs(5),
    }
}

#[test]
fn register_new_asset_synthesizes_meta_and_inserts_db() {
    let root = unique_tmp("new");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);
    let out_dir = bake_at(&root);

    // Author the asset file (no meta yet).
    let target = root.join("Assets/Tween/MyTween.asset");
    write(
        &target,
        "%YAML 1.1\n--- !u!114 &11400000\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: ffff7777ffff7777ffff7777ffff7777, type: 3}\n  m_Name: MyTween\n",
    );

    let outcome = register(&register_opts(&root, &out_dir, target.clone())).unwrap();
    assert!(outcome.created_meta);
    assert!(outcome.db_updated);
    assert_ne!(outcome.guid, 0);

    // Meta on disk holds the printed GUID.
    let meta_text = fs::read_to_string(target.with_extension("asset.meta")).unwrap();
    let expected = format!("guid: {}", query::format_guid(outcome.guid));
    assert!(meta_text.contains(&expected), "meta text: {meta_text}");
    assert!(meta_text.contains("NativeFormatImporter:"));

    // DB has the entry.
    let db = store::read(&store::db_path(&out_dir)).unwrap();
    let entry = db.find_by_guid(outcome.guid).expect("entry in db");
    assert_eq!(&*entry.name, "MyTween");
    assert_eq!(&*entry.hint, "Assets/Tween/MyTween.asset");
    // Script type → interned.
    match entry.asset_type {
        store::AssetType::Script(idx) => assert_eq!(
            db.script_guid(idx),
            0xffff7777ffff7777ffff7777ffff7777_u128,
        ),
        t => panic!("expected Script, got {t:?}"),
    }
}

#[test]
fn register_is_idempotent_when_meta_exists() {
    let root = unique_tmp("idempotent");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);
    let out_dir = bake_at(&root);

    let target = root.join("Assets/Foo.prefab");
    write(&target, "%YAML 1.1\n--- !u!1001 &100100000\nPrefabInstance:\n");

    let first = register(&register_opts(&root, &out_dir, target.clone())).unwrap();
    assert!(first.created_meta);

    let second = register(&register_opts(&root, &out_dir, target.clone())).unwrap();
    assert!(!second.created_meta, "second call must not re-author meta");
    assert_eq!(second.guid, first.guid);
    // Second call sees an unchanged db row → no rewrite.
    assert!(!second.db_updated);
}

#[test]
fn register_folder_emits_folder_asset_marker() {
    let root = unique_tmp("folder");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);
    let out_dir = bake_at(&root);

    let target = root.join("Assets/Empty");
    fs::create_dir_all(&target).unwrap();

    let outcome = register(&register_opts(&root, &out_dir, target.clone())).unwrap();
    assert!(outcome.created_meta);
    assert_ne!(outcome.guid, 0);

    let meta_text = fs::read_to_string({
        let mut s = target.into_os_string();
        s.push(".meta");
        PathBuf::from(s)
    })
    .unwrap();
    assert!(meta_text.contains("folderAsset: yes"));
    assert!(meta_text.contains("DefaultImporter:"));
}

#[test]
fn register_type_mismatch_errors() {
    let root = unique_tmp("mismatch");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);
    let out_dir = bake_at(&root);

    let target = root.join("Assets/Foo.asset");
    write(&target, "%YAML 1.1\n--- !u!114 &11400000\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: ffff7777ffff7777ffff7777ffff7777, type: 3}\n  m_Name: Foo\n");

    // First register → minimal NativeFormatImporter meta.
    register(&register_opts(&root, &out_dir, target.clone())).unwrap();

    // Re-register with a conflicting --type.
    let mut opts = register_opts(&root, &out_dir, target);
    opts.importer_override = Some(ImporterKind::Texture);
    let err = register(&opts).unwrap_err();
    assert!(matches!(err, RegisterError::ImporterMismatch { .. }), "got {err:?}");
}

#[test]
fn register_unknown_extension_falls_back_to_default() {
    let root = unique_tmp("unknown-ext");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);
    let out_dir = bake_at(&root);

    let target = root.join("Assets/Data.xyz");
    write(&target, "any-bytes\n");

    let outcome = register(&register_opts(&root, &out_dir, target.clone())).unwrap();
    assert!(outcome.created_meta);

    let meta_text = fs::read_to_string(target.with_extension("xyz.meta")).unwrap();
    assert!(meta_text.contains("DefaultImporter:"));
}

#[test]
fn register_applies_scrub_chars_to_entry_name() {
    let root = unique_tmp("scrub");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);
    let out_dir = bake_at(&root);

    let target = root.join("Assets/sample^v1.asset");
    write(&target, "%YAML 1.1\n--- !u!114 &11400000\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: ffff7777ffff7777ffff7777ffff7777, type: 3}\n  m_Name: x\n");

    let mut opts = register_opts(&root, &out_dir, target);
    opts.scrub_chars = Some("^".to_owned());
    let outcome = register(&opts).unwrap();

    let db = store::read(&store::db_path(&out_dir)).unwrap();
    let entry = db.find_by_guid(outcome.guid).unwrap();
    assert_eq!(&*entry.name, "sample_v1");
}

#[test]
fn register_with_explicit_type_override() {
    let root = unique_tmp("type-override");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);
    let out_dir = bake_at(&root);

    // .xyz would normally be DefaultImporter; override to TextScript.
    let target = root.join("Assets/note.xyz");
    write(&target, "hello\n");

    let mut opts = register_opts(&root, &out_dir, target.clone());
    opts.importer_override = Some(ImporterKind::TextScript);
    register(&opts).unwrap();

    let meta_text = fs::read_to_string(target.with_extension("xyz.meta")).unwrap();
    assert!(meta_text.contains("TextScriptImporter:"));
    assert!(!meta_text.contains("DefaultImporter:"));
}

#[test]
fn register_rejects_paths_outside_project() {
    let root = unique_tmp("outside");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);
    let out_dir = bake_at(&root);

    let outside = std::env::temp_dir().join(format!("not-in-{}", std::process::id()));
    fs::create_dir_all(&outside).unwrap();
    let target = outside.join("Foo.asset");
    fs::write(&target, "x").unwrap();

    let err = register(&register_opts(&root, &out_dir, target)).unwrap_err();
    assert!(matches!(err, RegisterError::NotInProject { .. }), "got {err:?}");
}

/// Regression for the intern_script index-shift hazard: when register
/// inserts a new script GUID that sorts BEFORE existing entries' script
/// GUIDs, those existing entries' `Script(idx)` references must be
/// remapped or they'll silently point at the wrong script.
#[test]
fn register_remaps_existing_script_indices_on_new_intern() {
    let root = unique_tmp("intern-remap");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);

    // First asset has a script GUID with a HIGH sort key.
    let high_target = root.join("Assets/HighScript.asset");
    write(
        &high_target,
        "%YAML 1.1\n--- !u!114 &11400000\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: ffffffffffffffffffffffffffffffff, type: 3}\n  m_Name: H\n",
    );
    let out_dir = bake_at(&root);
    let outcome1 = register(&register_opts(&root, &out_dir, high_target.clone())).unwrap();

    // Sanity: db has one Script entry referring to the high guid.
    let db = store::read(&store::db_path(&out_dir)).unwrap();
    let high_entry = db.find_by_guid(outcome1.guid).unwrap();
    let high_idx = match high_entry.asset_type {
        store::AssetType::Script(i) => i,
        _ => panic!(),
    };
    assert_eq!(
        db.script_guid(high_idx),
        0xffffffffffffffffffffffffffffffff_u128,
    );

    // Second asset has a script GUID with a LOWER sort key — its sorted
    // insert into `script_types` will push the high guid to idx+1, and
    // the first entry's `Script(idx)` must be remapped.
    let low_target = root.join("Assets/LowScript.asset");
    write(
        &low_target,
        "%YAML 1.1\n--- !u!114 &11400000\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: 00000000000000000000000000000001, type: 3}\n  m_Name: L\n",
    );
    register(&register_opts(&root, &out_dir, low_target)).unwrap();

    // Re-load and assert: both entries' script_types lookups still
    // resolve to their original script GUIDs.
    let db = store::read(&store::db_path(&out_dir)).unwrap();
    let high_after = db.find_by_guid(outcome1.guid).unwrap();
    match high_after.asset_type {
        store::AssetType::Script(idx) => assert_eq!(
            db.script_guid(idx),
            0xffffffffffffffffffffffffffffffff_u128,
            "high-script entry's idx was not remapped after low-script intern",
        ),
        t => panic!("expected Script, got {t:?}"),
    }
}

#[test]
fn register_meta_lookup_idempotent_inserts_missing_db_row() {
    // Asset already has a meta but db row is missing — register parses
    // existing meta, doesn't author a new one, but still inserts the row.
    let root = unique_tmp("missing-row");
    let _ = fs::remove_dir_all(&root);
    make_empty_project(&root);
    let out_dir = bake_at(&root);

    let target = root.join("Assets/Pre.prefab");
    write(&target, "%YAML 1.1\n--- !u!1001 &100100000\nPrefabInstance:\n");
    write(
        &target.with_extension("prefab.meta"),
        "fileFormatVersion: 2\nguid: cafebabecafebabecafebabecafebabe\nPrefabImporter:\n  externalObjects: {}\n",
    );

    let outcome = register(&register_opts(&root, &out_dir, target)).unwrap();
    assert!(!outcome.created_meta, "meta already existed");
    assert!(outcome.db_updated, "missing db row should be inserted");
    assert_eq!(outcome.guid, 0xcafebabecafebabecafebabecafebabe_u128);

    let db = store::read(&store::db_path(&out_dir)).unwrap();
    assert!(db.find_by_guid(outcome.guid).is_some());
}
