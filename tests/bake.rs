//! End-to-end test: build a tiny synthetic Unity project tree, run bake,
//! verify the binary store lookups.

use std::fs;
use std::path::{Path, PathBuf};

use unity_assetdb::bake::{BakeOptions, bake};
use unity_assetdb::store;

/// Drives the `BakeOptions` API with the canonical
/// `<root>/Library/unity-assetdb/` out dir, no sanitizer, silent sinks.
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

fn out_dir_for(root: &Path) -> PathBuf {
    root.join("Library").join("unity-assetdb")
}

fn db_file(root: &Path) -> PathBuf {
    store::db_path(&out_dir_for(root))
}

fn cache_file(root: &Path) -> PathBuf {
    store::cache_path(&out_dir_for(root))
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn make_fixture(root: &Path) {
    // Mark this dir as a Unity project root.
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    // A prefab + its meta.
    let prefab_dir = root.join("Assets/UI");
    write(
        &prefab_dir.join("Foo.prefab"),
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1001 &100100000\nPrefabInstance:\n  m_ObjectHideFlags: 0\n",
    );
    write(
        &prefab_dir.join("Foo.prefab.meta"),
        "fileFormatVersion: 2\nguid: aaaa1111aaaa1111aaaa1111aaaa1111\nPrefabImporter:\n  externalObjects: {}\n",
    );

    // A ScriptableObject .asset → AssetType::Script
    write(
        &root.join("Assets/SO/Bar.asset"),
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nMonoBehaviour:\n  m_ObjectHideFlags: 0\n  m_Script: {fileID: 11500000, guid: bbbb2222bbbb2222bbbb2222bbbb2222, type: 3}\n  m_Name: Bar\n",
    );
    write(
        &root.join("Assets/SO/Bar.asset.meta"),
        "fileFormatVersion: 2\nguid: cccc3333cccc3333cccc3333cccc3333\nNativeFormatImporter: {}\n",
    );

    // A texture with sprite-sheet sub-assets in its .meta.
    write(&root.join("Assets/Tex/Sheet.png"), "fake-png-bytes");
    write(
        &root.join("Assets/Tex/Sheet.png.meta"),
        "fileFormatVersion: 2
guid: dddd4444dddd4444dddd4444dddd4444
TextureImporter:
  spriteSheet:
    sprites:
    - serializedVersion: 2
      name: spr_a
      internalID: 11111
    - serializedVersion: 2
      name: spr_b
      internalID: 22222
",
    );
}

fn unique_tmp(label: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("unity-assetdb-bake-test-{label}-{pid}-{nanos}"))
}

#[test]
fn bake_then_load_roundtrip() {
    let root = unique_tmp("roundtrip");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    let _out_dir = bake_at(&root);

    let bin = db_file(&root);
    assert!(
        bin.exists(),
        "asset-db.bin not created at {}",
        bin.display()
    );

    let db = store::read(&bin).unwrap();
    assert_eq!(
        db.entries.len(),
        3,
        "expected 3 entries, got {:?}",
        db.entries
    );

    // Foo.prefab
    let foo = db
        .find_by_guid(0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
        .expect("Foo.prefab missing");
    assert_eq!(&*foo.name, "Foo");
    match foo.asset_type {
        store::AssetType::Native(n) => {
            assert_eq!(n, unity_assetdb::class_id::ClassId::Prefab as u32);
        }
        store::AssetType::Script(_) => {
            panic!("expected Native(Prefab) for Foo, got {:?}", foo.asset_type)
        }
    }

    // Bar.asset → Script(...) referencing the script guid.
    let bar = db
        .find_by_guid(0xcccc3333cccc3333cccc3333cccc3333_u128)
        .expect("Bar.asset missing");
    assert_eq!(&*bar.name, "Bar");
    match bar.asset_type {
        store::AssetType::Script(idx) => {
            assert_eq!(db.script_guid(idx), 0xbbbb2222bbbb2222bbbb2222bbbb2222_u128);
        }
        store::AssetType::Native(_) => {
            panic!("expected Script for Bar, got {:?}", bar.asset_type)
        }
    }

    // Sheet.png → Native(Texture2D), 2 sub-assets.
    let sheet = db
        .find_by_guid(0xdddd4444dddd4444dddd4444dddd4444_u128)
        .expect("Sheet.png missing");
    assert_eq!(&*sheet.name, "Sheet");
    match sheet.asset_type {
        store::AssetType::Native(n) => {
            assert_eq!(n, unity_assetdb::class_id::ClassId::Texture2D as u32);
        }
        store::AssetType::Script(_) => panic!("expected Native(Texture2D) for Sheet"),
    }
    assert_eq!(sheet.sub_assets.len(), 2);
    assert_eq!(sheet.sub_assets[0].file_id, 11111);
    assert_eq!(&*sheet.sub_assets[0].name, "spr_a");
    assert_eq!(sheet.sub_assets[1].file_id, 22222);
    assert_eq!(&*sheet.sub_assets[1].name, "spr_b");

    // Entries are guid-sorted on disk.
    let guids: Vec<u128> = db.entries.iter().map(|e| e.guid).collect();
    let mut sorted = guids.clone();
    sorted.sort_unstable();
    assert_eq!(guids, sorted);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn cache_reuse_preserves_names() {
    let root = unique_tmp("cache");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    // First bake.
    let _out_dir = bake_at(&root);
    let first = store::read(&db_file(&root)).unwrap();

    // Second bake without touching anything → identical contents.
    let _out_dir = bake_at(&root);
    let second = store::read(&db_file(&root)).unwrap();

    assert_eq!(first.entries.len(), second.entries.len());
    for (a, b) in first.entries.iter().zip(second.entries.iter()) {
        assert_eq!(a.guid, b.guid);
        assert_eq!(a.name, b.name);
        assert_eq!(a.sub_assets.len(), b.sub_assets.len());
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn duplicate_top_level_guid_hard_fails() {
    // Two .meta sharing a GUID (hand-edited copy-paste) — neither under a
    // Unity-hidden directory, so the walker filter doesn't catch them.
    let root = unique_tmp("dup-guid");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );
    write(
        &root.join("Assets/A.prefab"),
        "--- !u!1001 &100100000\nPrefabInstance: {}\n",
    );
    write(
        &root.join("Assets/A.prefab.meta"),
        "fileFormatVersion: 2\nguid: 1111111111111111111111111111aaaa\nPrefabImporter: {}\n",
    );
    write(
        &root.join("Assets/B.prefab"),
        "--- !u!1001 &100100000\nPrefabInstance: {}\n",
    );
    write(
        &root.join("Assets/B.prefab.meta"),
        "fileFormatVersion: 2\nguid: 1111111111111111111111111111aaaa\nPrefabImporter: {}\n",
    );

    let opts = BakeOptions {
        project_root: root.to_path_buf(),
        out_dir: out_dir_for(&root),
        name_sanitizer: None,
        on_warn: None,
        on_progress: None,
        verbose_timing: false,
        verbose_collisions: false,
    };
    let err = bake(&opts).expect_err("expected hard-fail on duplicate GUID");
    let msg = format!("{err}");
    assert!(
        msg.contains("duplicate top-level GUID"),
        "unexpected error: {msg}"
    );

    fs::remove_dir_all(&root).ok();
}

/// Integration smoke for the implicit-Sprite synthesis path:
/// Single-mode Sprite texture with an empty `sprites:` list bakes to a
/// single sub-asset row carrying Sprite's canonical fileID and the
/// texture's stem. Pure-function predicate branches (Multiple-mode,
/// non-Sprite textureType, non-empty sheet) are exercised cheaper as
/// unit tests at `bake::tests::synthesize_implicit_sprite_*`.
#[test]
fn implicit_sprite_subasset_synthesis() {
    use unity_assetdb::class_id::ClassId;

    let root = unique_tmp("synth-single");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );
    write(&root.join("Assets/Tex/Icon.png"), "fake-png-bytes");
    write(
        &root.join("Assets/Tex/Icon.png.meta"),
        "fileFormatVersion: 2
guid: eeee5555eeee5555eeee5555eeee5555
TextureImporter:
  textureType: 8
  spriteMode: 1
  spriteSheet:
    sprites: []
",
    );

    let _out_dir = bake_at(&root);
    let db = store::read(&db_file(&root)).unwrap();
    let entry = db
        .find_by_guid(0xeeee5555eeee5555eeee5555eeee5555_u128)
        .expect("Icon.png missing from db");

    assert_eq!(&*entry.name, "Icon");
    assert_eq!(
        entry.sub_assets.len(),
        1,
        "Single-mode Sprite texture with empty sheet should bake to exactly one sub-asset"
    );
    assert_eq!(
        entry.sub_assets[0].file_id,
        ClassId::Sprite.canonical_subobject_fid()
    );
    assert_eq!(&*entry.sub_assets[0].name, "Icon");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn cache_file_lives_alongside_bin() {
    let root = unique_tmp("cache-file");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    let _out_dir = bake_at(&root);
    let cache = cache_file(&root);
    assert!(
        cache.exists(),
        "cache file not created at {}",
        cache.display()
    );

    // Cache stores hints + mtimes; convert artifact does not.
    let c = store::read_cache(&cache).unwrap();
    assert_eq!(c.entries.len(), 3);
    // Hints are project-root-relative so Assets/ and Packages/ share one scheme.
    let foo = c
        .entries
        .iter()
        .find(|e| &*e.hint == "Assets/UI/Foo.prefab")
        .unwrap();
    assert!(foo.meta_mtime_ns > 0);
    assert!(foo.asset_mtime_ns > 0);

    fs::remove_dir_all(&root).ok();
}
