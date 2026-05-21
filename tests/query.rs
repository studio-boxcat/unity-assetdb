//! End-to-end test for the `query` module — bakes a small project then
//! exercises every `unity_assetdb::query::*` lookup.

use std::fs;
use std::path::{Path, PathBuf};

use unity_assetdb::bake::{BakeOptions, bake};
use unity_assetdb::query::{
    self, AssetTypeFilter, QueryError, asset_type_str, format_guid, parse_guid,
};

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
    std::env::temp_dir().join(format!("unity-assetdb-query-test-{label}-{pid}-{nanos}"))
}

fn out_dir_for(root: &Path) -> PathBuf {
    root.join("Library").join("unity-assetdb")
}

fn make_basic_project(root: &Path) {
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );

    // Foo.prefab — Native(Prefab)
    write(
        &root.join("Assets/UI/Foo.prefab"),
        "%YAML 1.1\n--- !u!1001 &100100000\nPrefabInstance:\n",
    );
    write(
        &root.join("Assets/UI/Foo.prefab.meta"),
        "fileFormatVersion: 2\nguid: aaaa1111aaaa1111aaaa1111aaaa1111\nPrefabImporter:\n  externalObjects: {}\n",
    );

    // Bar/Inner.png — Native(Texture2D)
    write(&root.join("Assets/Bar/Inner.png"), "fake-png-bytes");
    write(
        &root.join("Assets/Bar/Inner.png.meta"),
        "fileFormatVersion: 2\nguid: cccc3333cccc3333cccc3333cccc3333\nTextureImporter:\n",
    );

    // Bar.asset — Script(<guid>)
    write(
        &root.join("Assets/SO/Bar.asset"),
        "%YAML 1.1\n--- !u!114 &11400000\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: bbbb2222bbbb2222bbbb2222bbbb2222, type: 3}\n  m_Name: Bar\n",
    );
    write(
        &root.join("Assets/SO/Bar.asset.meta"),
        "fileFormatVersion: 2\nguid: dddd4444dddd4444dddd4444dddd4444\nNativeFormatImporter: {}\n",
    );
}

fn bake_at(root: &Path) -> PathBuf {
    let out_dir = out_dir_for(root);
    let opts = BakeOptions {
        project_root: root.to_path_buf(),
        out_dir: out_dir.clone(),
        on_warn: None,
        on_progress: None,
        verbose_timing: false,
        verbose_collisions: false,
    };
    bake(&opts).unwrap();
    out_dir
}

#[test]
fn guid_of_path_hits_existing_entry() {
    let root = unique_tmp("guid-hit");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    let entry = query::guid_of_path(&db, "Assets/UI/Foo.prefab").expect("hit");
    assert_eq!(entry.guid, 0xaaaa1111aaaa1111aaaa1111aaaa1111_u128);
}

#[test]
fn guid_of_path_normalizes_backslashes() {
    let root = unique_tmp("guid-bslash");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    assert!(query::guid_of_path(&db, "Assets\\UI\\Foo.prefab").is_some());
    assert!(query::guid_of_path(&db, "./Assets/UI/Foo.prefab").is_some());
}

#[test]
fn guid_of_path_miss() {
    let root = unique_tmp("guid-miss");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    assert!(query::guid_of_path(&db, "Assets/Nope.prefab").is_none());
}

#[test]
fn find_by_hint_substring_matches_case_insensitively() {
    let root = unique_tmp("find-by-hint");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    // Lowercase, partial path: matches Foo.prefab via 'ui/foo'.
    let hits = query::find_by_hint(&db, "ui/foo");
    assert_eq!(hits.len(), 1);
    assert_eq!(&*hits[0].hint, "Assets/UI/Foo.prefab");

    // 'Bar' substring matches both Bar/Inner.png and SO/Bar.asset.
    let mut hints: Vec<&str> = query::find_by_hint(&db, "Bar")
        .into_iter()
        .map(|e| e.hint.as_ref())
        .collect();
    hints.sort();
    assert_eq!(hints, vec!["Assets/Bar/Inner.png", "Assets/SO/Bar.asset"]);

    // Miss returns empty (never errors).
    assert!(query::find_by_hint(&db, "no-such-thing").is_empty());
}

#[test]
fn path_of_guid_hits_existing_entry() {
    let root = unique_tmp("path-hit");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    let entry =
        query::path_of_guid(&db, 0xaaaa1111aaaa1111aaaa1111aaaa1111_u128).expect("hit");
    assert_eq!(&*entry.hint, "Assets/UI/Foo.prefab");
}

#[test]
fn parse_guid_tolerates_hyphens_and_uppercase() {
    let g = parse_guid("AAAA1111-AAAA1111-AAAA1111-AAAA1111").unwrap();
    assert_eq!(g, 0xaaaa1111aaaa1111aaaa1111aaaa1111_u128);
}

#[test]
fn parse_guid_rejects_bad_hex() {
    assert!(matches!(
        parse_guid("not-32-hex"),
        Err(QueryError::BadGuidHex(_))
    ));
    assert!(matches!(
        parse_guid("gggg1111aaaa1111aaaa1111aaaa1111"),
        Err(QueryError::BadGuidHex(_))
    ));
}

#[test]
fn find_substring_case_insensitive() {
    let root = unique_tmp("find");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    let hits = query::find(&db, "foo"); // lowercase against `Foo.prefab`
    assert_eq!(hits.len(), 1);
    assert_eq!(&*hits[0].name, "Foo.prefab");

    // Substring across all entries.
    let hits2 = query::find(&db, "ar");
    let names: Vec<&str> = hits2.iter().map(|e| e.name.as_ref()).collect();
    assert!(names.contains(&"Bar.asset"));
}

#[test]
fn find_empty_when_no_match() {
    let root = unique_tmp("find-none");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    assert!(query::find(&db, "xyzzy").is_empty());
}

#[test]
fn list_no_filter_returns_all() {
    let root = unique_tmp("list-all");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    let all: Vec<_> = query::list(&db, None).collect();
    assert_eq!(all.len(), 3);
}

#[test]
fn list_filters_by_native_classid() {
    let root = unique_tmp("list-native");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    let prefabs: Vec<_> = query::list(
        &db,
        Some(AssetTypeFilter::Native(
            unity_assetdb::class_id::ClassId::Prefab as u32,
        )),
    )
    .collect();
    assert_eq!(prefabs.len(), 1);
    assert_eq!(&*prefabs[0].name, "Foo.prefab");
}

#[test]
fn list_filters_by_script_guid() {
    let root = unique_tmp("list-script");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    let script_guid = 0xbbbb2222bbbb2222bbbb2222bbbb2222_u128;
    let scripts: Vec<_> =
        query::list(&db, Some(AssetTypeFilter::Script(script_guid))).collect();
    assert_eq!(scripts.len(), 1);
    assert_eq!(&*scripts[0].name, "Bar.asset");
}

#[test]
fn list_filter_unknown_script_returns_empty() {
    let root = unique_tmp("list-script-miss");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    let scripts: Vec<_> = query::list(&db, Some(AssetTypeFilter::Script(0x1234_u128))).collect();
    assert!(scripts.is_empty());
}

#[test]
fn type_filter_parse_bogus_errors() {
    assert!(matches!(
        AssetTypeFilter::parse("NotAClass"),
        Err(QueryError::UnknownAssetType(_))
    ));
}

#[test]
fn alias_exact_match_returns_entries() {
    let root = unique_tmp("alias-hit");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    let hits = query::alias(&db, "Foo.prefab");
    assert_eq!(hits.len(), 1);
    assert_eq!(&*hits[0].name, "Foo.prefab");
}

#[test]
fn alias_miss_returns_empty() {
    let root = unique_tmp("alias-miss");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    assert!(query::alias(&db, "Nope").is_empty());
}

#[test]
fn format_and_parse_guid_round_trip() {
    let g = 0xabcd_ef01_2345_6789_abcd_ef01_2345_6789_u128;
    let s = format_guid(g);
    assert_eq!(s, "abcdef0123456789abcdef0123456789");
    assert_eq!(parse_guid(&s).unwrap(), g);
}

#[test]
fn asset_type_str_formats_native_and_script() {
    let root = unique_tmp("type-str");
    let _ = fs::remove_dir_all(&root);
    make_basic_project(&root);
    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();

    let foo = query::path_of_guid(&db, 0xaaaa1111aaaa1111aaaa1111aaaa1111_u128).unwrap();
    assert_eq!(asset_type_str(foo.asset_type, &db), "Prefab");

    let bar = query::path_of_guid(&db, 0xdddd4444dddd4444dddd4444dddd4444_u128).unwrap();
    let s = asset_type_str(bar.asset_type, &db);
    assert!(s.starts_with("Script:"), "got {s}");
    assert!(s.contains("bbbb2222bbbb2222bbbb2222bbbb2222"));
}
