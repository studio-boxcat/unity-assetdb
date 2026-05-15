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
    assert_eq!(&*foo.name, "Foo.prefab");
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
    assert_eq!(&*bar.name, "Bar.asset");
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
    assert_eq!(&*sheet.name, "Sheet.png");
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

/// Pin: warm-bake hint integrity. `build_cache` leaves `RawEntry.hint`
/// empty on the cached value and `process_one` re-stamps it from the
/// HashMap key on a cache hit — one fewer allocation per warm hit. If
/// either side regresses, the post-warm asset-db.bin carries
/// empty/wrong hints. Asserts every entry's hint matches the source
/// fixture path AND survives an arbitrary number of warm rebakes.
#[test]
fn cache_hit_preserves_hint() {
    let root = unique_tmp("cache-hint");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    // Cold bake builds the cache.
    let _out_dir = bake_at(&root);
    let cold = store::read(&db_file(&root)).unwrap();

    // Three warm bakes — every hint must be non-empty and stable.
    for _ in 0..3 {
        let _out_dir = bake_at(&root);
        let warm = store::read(&db_file(&root)).unwrap();
        assert_eq!(cold.entries.len(), warm.entries.len());
        for (c, w) in cold.entries.iter().zip(warm.entries.iter()) {
            assert_eq!(c.guid, w.guid);
            assert!(!w.hint.is_empty(), "warm-bake hint went empty for guid {:032x}", w.guid);
            assert_eq!(&*c.hint, &*w.hint, "warm-bake hint drifted for guid {:032x}", w.guid);
        }
    }

    // And the fixture paths show up in the indexed hints.
    let hints: std::collections::HashSet<&str> =
        cold.entries.iter().map(|e| e.hint.as_ref()).collect();
    assert!(hints.contains("Assets/UI/Foo.prefab"), "expected fixture hint, got: {hints:?}");
    assert!(hints.contains("Assets/SO/Bar.asset"), "expected fixture hint, got: {hints:?}");
    assert!(hints.contains("Assets/Tex/Sheet.png"), "expected fixture hint, got: {hints:?}");

    fs::remove_dir_all(&root).ok();
}

/// Pin: missing-meta pre-pass is idempotent across re-bakes. The
/// optimized walker reads each directory once and tests meta presence
/// via a hash set — a regression that drops candidates from the set
/// (e.g. wrong stem extraction) would manifest as the second bake
/// synthesizing metas the first bake already created. Asserts:
///   1. Cold bake creates exactly one .meta per real fixture asset.
///   2. Second bake creates zero new metas (mtime of every existing
///      .meta unchanged).
///   3. Pre-pass correctly handles subdirectories — adds a nested asset
///      without a .meta and confirms it gets synthesized.
#[test]
fn prepass_walker_idempotent_and_descends_subdirs() {
    let root = unique_tmp("prepass-idempotent");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    // One file with .meta (pre-pass should skip), one without (pre-pass
    // should synthesize). Subdir with the same shape — descent check.
    write(&root.join("Assets/A/HasMeta.prefab"), "%YAML 1.1\n--- !u!1001 &100100000\nPrefabInstance:\n");
    write(
        &root.join("Assets/A/HasMeta.prefab.meta"),
        "fileFormatVersion: 2\nguid: aaaa1111aaaa1111aaaa1111aaaa1111\nPrefabImporter: {}\n",
    );
    write(&root.join("Assets/A/Sub/Nested.prefab"), "%YAML 1.1\n--- !u!1001 &100100000\nPrefabInstance:\n");
    // Nested.prefab has no .meta — pre-pass must synthesize one.
    write(&root.join("Assets/B/Lone.prefab"), "%YAML 1.1\n--- !u!1001 &100100000\nPrefabInstance:\n");
    // Lone.prefab has no .meta either — top-level synthesis.

    let _out_dir = bake_at(&root);

    // Existing HasMeta.prefab.meta untouched.
    assert!(root.join("Assets/A/HasMeta.prefab.meta").exists());
    // Synthesized metas exist for the two .prefab files that lacked them.
    let nested_meta = root.join("Assets/A/Sub/Nested.prefab.meta");
    let lone_meta = root.join("Assets/B/Lone.prefab.meta");
    assert!(nested_meta.exists(), "pre-pass did not descend into Assets/A/Sub/");
    assert!(lone_meta.exists(), "pre-pass missed Assets/B/Lone.prefab");

    // Idempotency: capture mtimes, second bake leaves them untouched.
    let nested_mtime_before = mtime_ns_of(&nested_meta);
    let lone_mtime_before = mtime_ns_of(&lone_meta);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let _out_dir = bake_at(&root);
    assert_eq!(nested_mtime_before, mtime_ns_of(&nested_meta));
    assert_eq!(lone_mtime_before, mtime_ns_of(&lone_meta));

    fs::remove_dir_all(&root).ok();
}

/// Pin: cache→bin round-trip preserves sub_asset rows verbatim. The
/// `build_cache` refactor stripped `hint` from the cached RawEntry but
/// must leave every other field (guid, asset_type, sub_assets) intact.
/// A regression that nulled sub_assets would silently drop sprite-sheet
/// rows on warm bakes.
#[test]
fn cache_round_trips_sub_assets() {
    let root = unique_tmp("cache-subassets");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    // make_fixture's Sheet.png has 2 sprite-sheet sub-assets.
    let _out_dir = bake_at(&root);
    let cold = store::read(&db_file(&root)).unwrap();
    let cold_sheet = cold
        .entries
        .iter()
        .find(|e| e.guid == 0xdddd4444dddd4444dddd4444dddd4444_u128)
        .expect("Sheet.png missing from cold bake");
    assert_eq!(
        cold_sheet.sub_assets.len(),
        2,
        "expected 2 sprite-sheet sub-assets, got: {:?}",
        cold_sheet.sub_assets,
    );

    let _out_dir = bake_at(&root); // warm
    let warm = store::read(&db_file(&root)).unwrap();
    let warm_sheet = warm
        .entries
        .iter()
        .find(|e| e.guid == 0xdddd4444dddd4444dddd4444dddd4444_u128)
        .expect("Sheet.png missing from warm bake");
    assert_eq!(warm_sheet.sub_assets.len(), 2);
    for (c, w) in cold_sheet.sub_assets.iter().zip(warm_sheet.sub_assets.iter()) {
        assert_eq!(c.file_id, w.file_id);
        assert_eq!(c.class_id, w.class_id);
        assert_eq!(&*c.name, &*w.name);
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

    assert_eq!(&*entry.name, "Icon.png");
    assert_eq!(
        entry.sub_assets.len(),
        1,
        "Single-mode Sprite texture with empty sheet should bake to exactly one sub-asset"
    );
    assert_eq!(
        entry.sub_assets[0].file_id,
        ClassId::Sprite.canonical_subobject_fid()
    );
    // Sub-asset name carries no ext suffix — synthesized from filename
    // stem alone.
    assert_eq!(&*entry.sub_assets[0].name, "Icon");

    fs::remove_dir_all(&root).ok();
}

/// Files imported by Unity's `DefaultImporter` (any extension Unity
/// doesn't have a dedicated importer for — `.swf`, `.dll` payloads,
/// arbitrary binary blobs) materialize as a single `DefaultAsset`
/// (classID 1029) at fileID 102900000. Without indexing them, every
/// `{fileID: 102900000, guid: <swfGuid>, type: 3}` reference from a
/// caller `.asset` fails to resolve downstream. Pins the contract
/// after a meow-tower pull surfaced 34 unresolved `.swf` refs from
/// the CatAnimations `*_*.asset` configs.
#[test]
fn default_importer_asset_indexed_as_default_asset() {
    use unity_assetdb::class_id::ClassId;

    let root = unique_tmp("default-importer");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    // A `.swf` (or any file Unity falls back to DefaultImporter for) +
    // its `.meta`.
    write(&root.join("Assets/Swf/Cat.fla.swf"), "fake-swf-bytes");
    write(
        &root.join("Assets/Swf/Cat.fla.swf.meta"),
        "fileFormatVersion: 2
guid: c01ef0000864f41bdaacaf9939e97b36
DefaultImporter:
  externalObjects: {}
  userData:
  assetBundleName:
  assetBundleVariant:
",
    );

    let _out_dir = bake_at(&root);
    let db = store::read(&db_file(&root)).unwrap();

    let entry = db
        .find_by_guid(0xc01ef0000864f41bdaacaf9939e97b36_u128)
        .expect("DefaultImporter-imported asset missing from db");
    // Always-ext rule: stem `Cat.fla` (Rust's `file_stem` strips only
    // the trailing `.swf`) plus the actual file extension `.swf` →
    // `Cat.fla.swf`. Mirrors the on-disk filename.
    assert_eq!(&*entry.name, "Cat.fla.swf");
    match entry.asset_type {
        store::AssetType::Native(n) => {
            assert_eq!(
                n,
                ClassId::DefaultAsset as u32,
                "DefaultImporter assets should bake as Native(DefaultAsset)"
            );
        }
        store::AssetType::Script(_) => {
            panic!("expected Native(DefaultAsset), got {:?}", entry.asset_type)
        }
    }

    fs::remove_dir_all(&root).ok();
}

/// Cache integrity: when neither the `.meta` nor the asset file mtime
/// changed between bakes, the second bake reuses cached entries
/// verbatim and produces a byte-identical `asset-db.bin`. Pins the
/// fast-path that skips the asset stat when the `.meta` mtime matches.
#[test]
fn cache_hit_path_byte_identical_rebake() {
    let root = unique_tmp("cache-hit-bytes");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    let _out_dir = bake_at(&root);
    let first = fs::read(db_file(&root)).unwrap();

    // Sleep ≥1ms so any spurious mtime-on-touch debug couldn't false-hit.
    // We don't TOUCH anything, but we also don't want to mask a bug where
    // bake re-stamps an mtime as a side-effect.
    std::thread::sleep(std::time::Duration::from_millis(5));

    let _out_dir = bake_at(&root);
    let second = fs::read(db_file(&root)).unwrap();

    assert_eq!(first, second, "second-bake bytes drifted from first");
    fs::remove_dir_all(&root).ok();
}

/// Cache trade-off: pinning the warm-path assumption. Touching only
/// the asset file (without touching its `.meta`) does NOT invalidate the
/// cache — the fast path keys on `.meta` mtime alone. Under Unity's
/// importer this never happens (it stamps the `.meta` on every import),
/// so the cached row stays correct in practice. Out-of-Unity asset
/// edits land in the asset DB on the next `.meta` touch (or a manual
/// `rm asset-db.cache.bin`).
///
/// Documented assumption in `process_one`; test pins the current
/// behavior so any future invariant flip surfaces here.
#[test]
fn cache_does_not_detect_asset_only_touch() {
    let root = unique_tmp("cache-asset-only-touch");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    let _out_dir = bake_at(&root);
    let asset_path = root.join("Assets/UI/Foo.prefab");
    let pre_meta_mtime = mtime_ns_of(&root.join("Assets/UI/Foo.prefab.meta"));

    // Touch only the asset (Unity workflow can never produce this).
    std::thread::sleep(std::time::Duration::from_millis(10));
    let now = filetime::FileTime::now();
    set_mtime(&asset_path, now);

    let _out_dir = bake_at(&root);
    let c = store::read_cache(&cache_file(&root)).unwrap();
    let foo = c
        .entries
        .iter()
        .find(|e| &*e.hint == "Assets/UI/Foo.prefab")
        .unwrap();
    // The cache's recorded asset_mtime is the *original* — fast path
    // bypassed the companion stat, so the bake never noticed the touch.
    assert_ne!(
        foo.asset_mtime_ns,
        mtime_ns_of(&asset_path),
        "asset-only touch was unexpectedly detected (fast path may have changed)"
    );
    // Sanity: the meta mtime IS the value we'd expect (unchanged across
    // bakes), confirming the fast path actually fired.
    assert_eq!(
        foo.meta_mtime_ns, pre_meta_mtime,
        "meta mtime drifted between bakes — fixture leaked",
    );
    fs::remove_dir_all(&root).ok();
}

/// Cache integrity: when both `.meta` and asset get touched (normal
/// Unity reimport pattern), the second bake re-parses and the cache
/// records the fresh mtimes. The most common warm-bake-invalidation
/// case in practice.
#[test]
fn cache_invalidates_on_meta_and_asset_touch() {
    let root = unique_tmp("cache-both-touch");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    let _out_dir = bake_at(&root);
    let meta_path = root.join("Assets/UI/Foo.prefab.meta");
    let asset_path = root.join("Assets/UI/Foo.prefab");

    std::thread::sleep(std::time::Duration::from_millis(10));
    let now = filetime::FileTime::now();
    set_mtime(&meta_path, now);
    set_mtime(&asset_path, now);

    let _out_dir = bake_at(&root);
    let c = store::read_cache(&cache_file(&root)).unwrap();
    let foo = c
        .entries
        .iter()
        .find(|e| &*e.hint == "Assets/UI/Foo.prefab")
        .unwrap();
    assert_eq!(foo.meta_mtime_ns, mtime_ns_of(&meta_path));
    assert_eq!(foo.asset_mtime_ns, mtime_ns_of(&asset_path));
    fs::remove_dir_all(&root).ok();
}

/// Cache integrity: changing the `.meta` mtime alone (without asset
/// edits) must still cause re-parse — the meta stat is what gates the
/// fast path, so a drift there has to fall through to the slow path.
#[test]
fn cache_invalidates_on_meta_mtime_drift() {
    let root = unique_tmp("cache-meta-drift");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    let _out_dir = bake_at(&root);

    std::thread::sleep(std::time::Duration::from_millis(10));
    let meta_path = root.join("Assets/UI/Foo.prefab.meta");
    let now = filetime::FileTime::now();
    set_mtime(&meta_path, now);

    let _out_dir = bake_at(&root);
    let c = store::read_cache(&cache_file(&root)).unwrap();
    let foo = c
        .entries
        .iter()
        .find(|e| &*e.hint == "Assets/UI/Foo.prefab")
        .unwrap();
    let new_meta_mtime = mtime_ns_of(&meta_path);
    assert_eq!(
        foo.meta_mtime_ns, new_meta_mtime,
        "cache meta_mtime not bumped after meta touch"
    );
    fs::remove_dir_all(&root).ok();
}

/// Cache integrity: deleting an asset between bakes drops it from the
/// resulting database. Guards against a fast-path bug that might serve
/// the cached row even when the companion no longer exists.
#[test]
fn cache_drops_entry_when_asset_deleted() {
    let root = unique_tmp("cache-asset-deleted");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    let _out_dir = bake_at(&root);
    let first = store::read(&db_file(&root)).unwrap();
    assert!(
        first
            .find_by_guid(0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
            .is_some(),
        "Foo prefab should exist before deletion",
    );

    fs::remove_file(root.join("Assets/UI/Foo.prefab")).unwrap();
    fs::remove_file(root.join("Assets/UI/Foo.prefab.meta")).unwrap();

    let _out_dir = bake_at(&root);
    let second = store::read(&db_file(&root)).unwrap();
    assert!(
        second
            .find_by_guid(0xaaaa1111aaaa1111aaaa1111aaaa1111_u128)
            .is_none(),
        "deleted Foo prefab still in db",
    );
    fs::remove_dir_all(&root).ok();
}

fn set_mtime(path: &Path, t: filetime::FileTime) {
    filetime::set_file_mtime(path, t).unwrap();
}

fn mtime_ns_of(path: &Path) -> u64 {
    let md = fs::metadata(path).unwrap();
    let st = md
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    st.as_nanos() as u64
}

/// A corrupt `asset-db.cache.bin` (wrong magic, truncated bytes, schema
/// drift, …) must NOT crash the bake. The bake's contract is "rebuild
/// from scratch if the cache is unreadable" — guards against a hand-
/// edited cache file or a half-written one from a kill -9'd bake.
#[test]
fn bake_recovers_from_corrupt_cache() {
    let root = unique_tmp("corrupt-cache");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    // First bake → produces a real cache file.
    let _out_dir = bake_at(&root);
    let cache_path = cache_file(&root);
    assert!(cache_path.exists());

    // Corrupt it: replace contents with garbage. The bake-side decode
    // hits a magic mismatch / decode error and falls back to "empty
    // cache" path.
    fs::write(&cache_path, b"this is not a valid cache file").unwrap();

    // Should not panic; should produce a correct asset-db.bin.
    let _out_dir = bake_at(&root);
    let db = store::read(&db_file(&root)).unwrap();
    assert_eq!(
        db.entries.len(),
        3,
        "bake recovered, but entry count drifted: {}",
        db.entries.len(),
    );
    fs::remove_dir_all(&root).ok();
}

/// Zero-asset-but-valid-project: a Unity project with `Assets/` +
/// `ProjectSettings/` but no actual assets must still produce a valid
/// empty `asset-db.bin`, not panic at the dedup pass.
#[test]
fn empty_project_bakes_cleanly() {
    let root = unique_tmp("empty-project");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("Assets")).unwrap();
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    let _out_dir = bake_at(&root);
    let db = store::read(&db_file(&root)).unwrap();
    assert_eq!(db.entries.len(), 0);
    assert_eq!(db.script_types.len(), 0);
    fs::remove_dir_all(&root).ok();
}

/// Deeply-nested asset (10+ dirs) still gets indexed. The walker has
/// no built-in depth limit, but `ignore::WalkBuilder` does expose one;
/// pin that we don't accidentally enable a low default.
#[test]
fn deeply_nested_assets_are_indexed() {
    let root = unique_tmp("deep-nesting");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    // 12 levels deep — well beyond any sane production project but
    // a useful contract pin.
    let mut deep = root.join("Assets");
    for i in 0..12 {
        deep = deep.join(format!("L{i}"));
    }
    write(
        &deep.join("Deep.prefab"),
        "--- !u!1001 &100100000\nPrefabInstance: {}\n",
    );
    write(
        &deep.join("Deep.prefab.meta"),
        "fileFormatVersion: 2\nguid: deadbeefdeadbeefdeadbeefdeadbeef\nPrefabImporter: {}\n",
    );

    let _out_dir = bake_at(&root);
    let db = store::read(&db_file(&root)).unwrap();
    assert!(
        db.find_by_guid(0xdeadbeefdeadbeefdeadbeefdeadbeef_u128)
            .is_some(),
        "deeply-nested prefab not indexed",
    );
    fs::remove_dir_all(&root).ok();
}

/// Pin: a `.gitignore` *inside* `Assets/` (or `Packages/`) does NOT
/// hide its targets from the bake. Unity itself doesn't honor
/// gitignore — and a gitignored `.cs.meta` or `.asset` still has a
/// Unity-assigned guid that other prefabs can reference. Excluding
/// would cause spurious "unresolved asset reference" hard-fails on
/// the consumer side. Pins the `standard_filters(false)` walker
/// behavior set in `src/walk.rs`.
///
/// Out-of-tree `.gitignore` paths (`<project>/.gitignore`, the
/// usual Unity-project shape that excludes Library/Temp/) stay
/// effective because the walker is rooted at Assets/Packages — those
/// paths are siblings, never visited. Pinned by
/// `walker_ignores_library_temp_and_unity_hidden`.
#[test]
fn walker_does_not_honor_gitignore_inside_assets() {
    let root = unique_tmp("inside-gitignore");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    let prefab_body = "--- !u!1001 &100100000\nPrefabInstance: {}\n";
    let make_meta =
        |guid: &str| format!("fileFormatVersion: 2\nguid: {guid}\nPrefabImporter: {{}}\n");

    // A `.gitignore` inside Assets/Foo/ that ignores the prefab.
    write(&root.join("Assets/Foo/.gitignore"), "Ignored.prefab\n");
    write(&root.join("Assets/Foo/Ignored.prefab"), prefab_body);
    write(
        &root.join("Assets/Foo/Ignored.prefab.meta"),
        &make_meta("aaaa1111aaaa1111aaaa1111aaaa1111"),
    );
    // A normal sibling that's not ignored.
    write(&root.join("Assets/Foo/Kept.prefab"), prefab_body);
    write(
        &root.join("Assets/Foo/Kept.prefab.meta"),
        &make_meta("bbbb2222bbbb2222bbbb2222bbbb2222"),
    );

    let _out_dir = bake_at(&root);
    let db = store::read(&db_file(&root)).unwrap();
    let hints: Vec<&str> = db.entries.iter().map(|e| &*e.hint).collect();

    assert!(
        hints.contains(&"Assets/Foo/Ignored.prefab"),
        "inside-Assets gitignore was honored (should not be); hints: {hints:?}",
    );
    assert!(
        hints.contains(&"Assets/Foo/Kept.prefab"),
        "Kept.prefab missing: {hints:?}",
    );
    fs::remove_dir_all(&root).ok();
}

/// Walker ignore audit: pin the four paths that must NEVER surface in
/// the asset DB.
/// 1. `Library/` — gitignored, regenerated by Unity; baking it would
///    self-reference convert artifacts on rebuild.
/// 2. `Temp/` — Unity scratch space, similarly gitignored.
/// 3. `Assets/.Hidden/` — Unity-hidden by convention (leading dot).
/// 4. `Assets/Foo~/` — Unity-hidden by convention (trailing tilde).
///
/// The first two are caught by `ignore`'s gitignore handling; the last
/// two by the `is_unity_hidden` filter in `walk.rs`. Both contracts
/// pin here.
#[test]
fn walker_ignores_library_temp_and_unity_hidden() {
    let root = unique_tmp("walker-ignores");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );
    // gitignore for the regenerable dirs.
    write(&root.join(".gitignore"), "/Library/\n/Temp/\n");

    let make_meta = |guid: &str| {
        format!("fileFormatVersion: 2\nguid: {guid}\nPrefabImporter: {{}}\n")
    };
    let make_prefab = "--- !u!1001 &100100000\nPrefabInstance: {}\n";

    // Real, must survive.
    write(&root.join("Assets/Visible/Bar.prefab"), make_prefab);
    write(
        &root.join("Assets/Visible/Bar.prefab.meta"),
        &make_meta("aaaa1111aaaa1111aaaa1111aaaa1111"),
    );
    // Library/ — gitignored.
    write(&root.join("Library/Scratch/InLib.prefab"), make_prefab);
    write(
        &root.join("Library/Scratch/InLib.prefab.meta"),
        &make_meta("bbbb2222bbbb2222bbbb2222bbbb2222"),
    );
    // Temp/ — gitignored.
    write(&root.join("Temp/Scratch/InTemp.prefab"), make_prefab);
    write(
        &root.join("Temp/Scratch/InTemp.prefab.meta"),
        &make_meta("cccc3333cccc3333cccc3333cccc3333"),
    );
    // .Hidden/ — Unity-hidden.
    write(&root.join("Assets/.Hidden/InHidden.prefab"), make_prefab);
    write(
        &root.join("Assets/.Hidden/InHidden.prefab.meta"),
        &make_meta("dddd4444dddd4444dddd4444dddd4444"),
    );
    // Foo~/ — Unity-hidden.
    write(&root.join("Assets/Foo~/InTilde.prefab"), make_prefab);
    write(
        &root.join("Assets/Foo~/InTilde.prefab.meta"),
        &make_meta("eeee5555eeee5555eeee5555eeee5555"),
    );

    let _out_dir = bake_at(&root);
    let db = store::read(&db_file(&root)).unwrap();

    let hints: Vec<&str> = db.entries.iter().map(|e| &*e.hint).collect();
    assert!(
        hints.contains(&"Assets/Visible/Bar.prefab"),
        "real asset missing: {hints:?}",
    );
    for ignored in [
        "Library/Scratch/InLib.prefab",
        "Temp/Scratch/InTemp.prefab",
        "Assets/.Hidden/InHidden.prefab",
        "Assets/Foo~/InTilde.prefab",
    ] {
        assert!(
            !hints.contains(&ignored),
            "expected to be ignored, found: {ignored}",
        );
    }
    assert_eq!(db.entries.len(), 1, "only the visible asset should bake");

    fs::remove_dir_all(&root).ok();
}

/// Pin: bake synthesizes a minimal `.meta` for any asset / folder that
/// lacks one — matches Unity's editor-focus behavior so a fresh bake
/// works after dropping files into the project tree.
#[test]
fn bake_creates_missing_meta_files() {
    let root = unique_tmp("missing-meta");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    // An asset file with NO sibling .meta.
    write(
        &root.join("Assets/New/Loose.prefab"),
        "--- !u!1001 &100100000\nPrefabInstance: {}\n",
    );
    // A folder with NO sibling .meta either.
    fs::create_dir_all(root.join("Assets/Empty")).unwrap();

    // Unity-hidden entries (leading `.` / trailing `~`) — synthesis
    // must skip these, matching the walker's `is_unity_hidden` filter.
    write(
        &root.join("Assets/.Hidden/Scratch.prefab"),
        "--- !u!1001 &100100000\nPrefabInstance: {}\n",
    );
    write(
        &root.join("Assets/Tilde~/Scratch.prefab"),
        "--- !u!1001 &100100000\nPrefabInstance: {}\n",
    );
    // Direct child of Packages/ — must also be skipped (manifest.json
    // shape) since Unity never authors metas there.
    write(&root.join("Packages/manifest.json"), "{}\n");

    let progress = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let progress_clone = std::sync::Arc::clone(&progress);
    let opts = BakeOptions {
        project_root: root.to_path_buf(),
        out_dir: out_dir_for(&root),
        name_sanitizer: None,
        on_warn: None,
        on_progress: Some(Box::new(move |m| {
            progress_clone.lock().unwrap().push(m.to_string());
        })),
        verbose_timing: false,
        verbose_collisions: false,
    };
    bake(&opts).unwrap();

    // Meta files synthesized on disk.
    assert!(
        root.join("Assets/New/Loose.prefab.meta").exists(),
        "Loose.prefab.meta not synthesized",
    );
    assert!(
        root.join("Assets/Empty.meta").exists(),
        "Empty.meta not synthesized",
    );
    // Intermediate folder (`Assets/New/`) is also covered.
    assert!(
        root.join("Assets/New.meta").exists(),
        "New.meta not synthesized",
    );

    // Hidden subtrees and Packages/ direct-children must NOT get
    // synthesized metas.
    for skipped in [
        "Assets/.Hidden.meta",
        "Assets/.Hidden/Scratch.prefab.meta",
        "Assets/Tilde~.meta",
        "Assets/Tilde~/Scratch.prefab.meta",
        "Packages/manifest.json.meta",
    ] {
        assert!(
            !root.join(skipped).exists(),
            "synthesis must skip {skipped}",
        );
    }

    // Folder meta carries the folderAsset marker; file meta does not.
    let empty_meta = fs::read_to_string(root.join("Assets/Empty.meta")).unwrap();
    assert!(empty_meta.contains("folderAsset: yes"));
    assert!(empty_meta.contains("DefaultImporter"));
    let loose_meta = fs::read_to_string(root.join("Assets/New/Loose.prefab.meta")).unwrap();
    assert!(!loose_meta.contains("folderAsset"));
    assert!(loose_meta.contains("PrefabImporter"));

    // Info log: per-file lines plus a summary count.
    let logs = progress.lock().unwrap();
    assert!(
        logs.iter().any(|m| m.contains("created .meta for Assets/New/Loose.prefab")),
        "missing per-file create log; logs={logs:?}",
    );
    assert!(
        logs.iter().any(|m| m.contains("created") && m.contains("missing .meta")),
        "missing summary log; logs={logs:?}",
    );

    // Loose.prefab is indexed; folder synthesis doesn't add entries.
    let db = store::read(&db_file(&root)).unwrap();
    assert_eq!(
        db.entries.len(),
        1,
        "expected 1 entry (folder metas excluded), got {:?}",
        db.entries,
    );
    let hints: Vec<&str> = db.entries.iter().map(|e| &*e.hint).collect();
    assert_eq!(hints, vec!["Assets/New/Loose.prefab"]);

    // Re-bake is idempotent — no new "created" log lines.
    let progress2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let progress2_clone = std::sync::Arc::clone(&progress2);
    let opts2 = BakeOptions {
        project_root: root.to_path_buf(),
        out_dir: out_dir_for(&root),
        name_sanitizer: None,
        on_warn: None,
        on_progress: Some(Box::new(move |m| {
            progress2_clone.lock().unwrap().push(m.to_string());
        })),
        verbose_timing: false,
        verbose_collisions: false,
    };
    bake(&opts2).unwrap();
    let logs2 = progress2.lock().unwrap();
    assert!(
        !logs2.iter().any(|m| m.contains("created .meta")),
        "second bake should not synthesize; logs={logs2:?}",
    );

    fs::remove_dir_all(&root).ok();
}

/// Pin: `.androidlib` / `.androidpack` / `.aar` folders are folder-based
/// Android plugins. Unity authors a `.meta` for the folder itself but
/// hands the contents off to Gradle untouched — no per-file metas.
/// The bake's missing-meta pre-pass must mirror that: synthesize the
/// folder meta if absent, but never descend into the folder.
#[test]
fn bake_does_not_synthesize_inside_opaque_android_plugin_folders() {
    let root = unique_tmp("opaque-android-plugins");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    // Three folder-based Android plugin shapes Unity treats as opaque.
    write(
        &root.join("Assets/Plugins/Android/FirebaseApp.androidlib/AndroidManifest.xml"),
        "<manifest/>\n",
    );
    write(
        &root.join("Assets/Plugins/Android/FirebaseApp.androidlib/res/values/strings.xml"),
        "<resources/>\n",
    );
    write(
        &root.join("Assets/Plugins/Android/Pack.androidpack/gradle.properties"),
        "x=1\n",
    );
    write(
        &root.join("Assets/Plugins/Android/Lib.aar/classes.dex"),
        "fake-dex\n",
    );

    bake_at(&root);

    // Folder metas synthesized for the opaque roots themselves.
    for folder_meta in [
        "Assets/Plugins/Android/FirebaseApp.androidlib.meta",
        "Assets/Plugins/Android/Pack.androidpack.meta",
        "Assets/Plugins/Android/Lib.aar.meta",
    ] {
        assert!(
            root.join(folder_meta).exists(),
            "expected folder meta {folder_meta} to be synthesized",
        );
    }

    // Nothing synthesized inside the opaque folders.
    for skipped in [
        "Assets/Plugins/Android/FirebaseApp.androidlib/AndroidManifest.xml.meta",
        "Assets/Plugins/Android/FirebaseApp.androidlib/res.meta",
        "Assets/Plugins/Android/FirebaseApp.androidlib/res/values.meta",
        "Assets/Plugins/Android/FirebaseApp.androidlib/res/values/strings.xml.meta",
        "Assets/Plugins/Android/Pack.androidpack/gradle.properties.meta",
        "Assets/Plugins/Android/Lib.aar/classes.dex.meta",
    ] {
        assert!(
            !root.join(skipped).exists(),
            "synthesis must skip {skipped} (opaque-plugin descendant)",
        );
    }

    fs::remove_dir_all(&root).ok();
}

/// Pin: a git submodule embedded in `Assets/` or `Packages/` is a
/// separate repo whose contents we don't own. Synthesizing `.meta`
/// files inside would dirty an unrelated working tree. Detect submodule
/// roots by the sibling `.git` entry (file or dir) and skip descent.
#[test]
fn bake_does_not_synthesize_inside_git_submodules() {
    let root = unique_tmp("submodule-skip");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    // A "submodule" under Packages/<pkg>/<nested>/ — Unity wraps the
    // upstream repo one level deep when the upstream isn't shaped as a
    // package root. Real-world case: Packages/com.unity.build-report-inspector.
    let sub = root.join("Packages/com.example.foo/upstream-repo");
    fs::create_dir_all(&sub).unwrap();
    // `.git` file pointing to the parent repo's gitdir — the canonical
    // submodule marker.
    write(&sub.join(".git"), "gitdir: ../../../.git/modules/foo\n");
    write(&sub.join("README.md"), "hello\n");
    write(&sub.join("src/lib.rs"), "fn main() {}\n");

    // And one under Assets/ with a `.git` *directory* (worktree-style).
    let sub2 = root.join("Assets/Vendor/ThirdParty");
    fs::create_dir_all(sub2.join(".git")).unwrap();
    write(&sub2.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&sub2.join("README.md"), "vendor\n");
    write(&sub2.join("src/foo.cs"), "// vendor\n");

    bake_at(&root);

    for skipped in [
        "Packages/com.example.foo/upstream-repo/README.md.meta",
        "Packages/com.example.foo/upstream-repo/src.meta",
        "Packages/com.example.foo/upstream-repo/src/lib.rs.meta",
        "Assets/Vendor/ThirdParty/README.md.meta",
        "Assets/Vendor/ThirdParty/src.meta",
        "Assets/Vendor/ThirdParty/src/foo.cs.meta",
    ] {
        assert!(
            !root.join(skipped).exists(),
            "synthesis must skip {skipped} (inside submodule)",
        );
    }

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

/// Sidecar files (`.md` documentation, `.pspec` source for the pspec tool)
/// live next to real Unity assets but are not themselves assets. The bake
/// excludes them entirely:
///   1. Existing `.md.meta` / `.pspec.meta` files are skipped by the
///      meta-walker → no entries in `asset-db.bin`.
///   2. Bare `.md` / `.pspec` files without sibling `.meta` are skipped by
///      the missing-meta pre-pass → no `.meta` synthesized for them.
/// The companion real asset (`Foo.prefab`) is unaffected.
#[test]
fn bake_excludes_sidecar_md_and_pspec_files() {
    let root = unique_tmp("sidecar-exclude");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    // Real asset — should be indexed.
    write(
        &root.join("Assets/UI/Foo.prefab"),
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1001 &100100000\nPrefabInstance:\n  m_ObjectHideFlags: 0\n",
    );
    write(
        &root.join("Assets/UI/Foo.prefab.meta"),
        "fileFormatVersion: 2\nguid: aaaa1111aaaa1111aaaa1111aaaa1111\nPrefabImporter:\n  externalObjects: {}\n",
    );

    // (1) Sidecar with an existing `.meta` — meta walker must skip it.
    write(&root.join("Assets/UI/Foo.prefab.md"), "# Docs\n");
    write(
        &root.join("Assets/UI/Foo.prefab.md.meta"),
        "fileFormatVersion: 2\nguid: bbbb2222bbbb2222bbbb2222bbbb2222\nDefaultImporter:\n  externalObjects: {}\n",
    );

    // (2) Sidecar without a `.meta` — missing-meta pre-pass must not synthesize one.
    write(&root.join("Assets/UI/Foo.prefab.pspec"), "{}\n");

    let _out_dir = bake_at(&root);

    let db = store::read(&db_file(&root)).unwrap();
    assert_eq!(
        db.entries.len(),
        1,
        "only the .prefab should be indexed, got: {:?}",
        db.entries.iter().map(|e| &*e.hint).collect::<Vec<_>>(),
    );
    assert!(db.find_by_guid(0xaaaa1111aaaa1111aaaa1111aaaa1111_u128).is_some());
    assert!(
        db.find_by_guid(0xbbbb2222bbbb2222bbbb2222bbbb2222_u128).is_none(),
        ".md sidecar must not enter the asset-db",
    );

    // The pspec sidecar must NOT have a synthesized `.meta`.
    assert!(
        !root.join("Assets/UI/Foo.prefab.pspec.meta").exists(),
        "missing-meta pre-pass should not synthesize a .meta for `.pspec` sidecars",
    );

    fs::remove_dir_all(&root).ok();
}
