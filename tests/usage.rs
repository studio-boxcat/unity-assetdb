//! End-to-end tests for `usage::find_usages` + its coherence with the
//! baked `asset-db.bin`. Builds tiny synthetic Unity projects, plants
//! YAML files referencing a known GUID, then asserts the walker finds
//! exactly the expected hits — and that `usage <project-rel-path>` still
//! resolves via the bake artifact after a re-bake.

use std::fs;
use std::path::{Path, PathBuf};

use unity_assetdb::bake::{BakeOptions, bake};
use unity_assetdb::query;
use unity_assetdb::usage::{UsageMatch, find_usages};
use unity_assetdb::walk::WalkError;

// ─── fixture helpers ─────────────────────────────────────────────────────

/// Single source of truth for the GUID we plant + scan for. Other forms
/// are derived so they can't drift.
const TARGET_HEX_STR: &str = "deadbeef00112233445566778899aabb";

fn target_hex() -> &'static [u8; 32] {
    TARGET_HEX_STR.as_bytes().try_into().unwrap()
}

fn target_guid() -> u128 {
    u128::from_str_radix(TARGET_HEX_STR, 16).unwrap()
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn unique_tmp(label: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("unity-assetdb-usage-test-{label}-{pid}-{nanos}"))
}

/// Fresh empty project rooted at `/tmp/...` with just the Unity-project
/// marker files. Each test extends this with its own YAML fixtures.
fn fresh_project(label: &str) -> PathBuf {
    let root = unique_tmp(label);
    let _ = fs::remove_dir_all(&root);
    write(
        &root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    );
    fs::create_dir_all(root.join("Assets")).unwrap();
    root
}

/// Run the canonical `BakeOptions` (silent sinks, no sanitizer) against
/// `root`. Mirrors `bake_at` in tests/bake.rs and tests/query.rs.
fn bake_at(root: &Path) -> PathBuf {
    let out_dir = root.join("Library").join("unity-assetdb");
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

/// Pretty rel-path for assertions — `find_usages` returns project-rel
/// `PathBuf`s; on a temp project under `/var/folders/...` these are
/// strictly `Assets/...` / `Packages/...` so the comparison is portable.
fn rel(m: &UsageMatch) -> String {
    m.path.to_string_lossy().replace('\\', "/")
}

// ─── tests ───────────────────────────────────────────────────────────────

#[test]
fn finds_guid_in_prefab_with_correct_line_and_text() {
    let root = fresh_project("basic");

    write(
        &root.join("Assets/UI/Card.prefab"),
        &format!(
            "%YAML 1.1\n\
             --- !u!114 &100\n\
             MonoBehaviour:\n\
               m_Script: {{fileID: 11500000, guid: {TARGET_HEX_STR}, type: 3}}\n\
               m_Name: Card\n"
        ),
    );

    let hits = find_usages(&root, target_hex()).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(rel(&hits[0]), "Assets/UI/Card.prefab");
    assert_eq!(hits[0].line, 4);
    assert!(hits[0].text.contains(&format!("guid: {TARGET_HEX_STR}")));
    assert!(hits[0].text.starts_with("m_Script:"));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn finds_refs_across_all_yaml_extensions() {
    let root = fresh_project("ext-coverage");

    let yaml_body = format!(
        "%YAML 1.1\n--- !u!114 &100\nMonoBehaviour:\n  \
         m_Script: {{fileID: 11500000, guid: {TARGET_HEX_STR}, type: 3}}\n"
    );

    for ext in ["prefab", "unity", "asset", "mat", "controller", "anim"] {
        write(&root.join(format!("Assets/X/file.{ext}")), &yaml_body);
    }
    // Unity .meta files can carry external GUID refs (e.g. SpriteAtlas userData).
    write(
        &root.join("Assets/X/file.png.meta"),
        &format!(
            "fileFormatVersion: 2\nguid: cafef00dcafef00dcafef00dcafef00d\n\
             TextureImporter:\n  externalObjects:\n    - second:\n        guid: {TARGET_HEX_STR}\n"
        ),
    );

    let hits = find_usages(&root, target_hex()).unwrap();
    let paths: Vec<_> = hits.iter().map(rel).collect();
    assert_eq!(hits.len(), 7, "got {paths:?}");
    assert_eq!(
        paths,
        vec![
            "Assets/X/file.anim",
            "Assets/X/file.asset",
            "Assets/X/file.controller",
            "Assets/X/file.mat",
            "Assets/X/file.png.meta",
            "Assets/X/file.prefab",
            "Assets/X/file.unity",
        ]
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn skips_non_yaml_extensions_even_when_content_matches() {
    let root = fresh_project("non-yaml-skipped");

    for name in ["script.cs", "notes.txt", "config.json", "tint.shader", "README"] {
        write(
            &root.join(format!("Assets/Decoys/{name}")),
            &format!("// just a comment containing {TARGET_HEX_STR}\n"),
        );
    }
    // One real YAML so the test is meaningful (would pass trivially otherwise).
    write(
        &root.join("Assets/Real/x.prefab"),
        &format!("MonoBehaviour:\n  ref: {TARGET_HEX_STR}\n"),
    );

    let hits = find_usages(&root, target_hex()).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(rel(&hits[0]), "Assets/Real/x.prefab");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn prunes_unity_hidden_directories() {
    let root = fresh_project("hidden-dirs");

    let body = format!("MonoBehaviour:\n  guid: {TARGET_HEX_STR}\n");
    write(&root.join("Assets/.scratch/sneaky.prefab"), &body);
    write(&root.join("Assets/Drafts~/draft.prefab"), &body);
    write(&root.join("Assets/Real.prefab"), &body);

    let hits = find_usages(&root, target_hex()).unwrap();
    let paths: Vec<_> = hits.iter().map(rel).collect();
    assert_eq!(paths, vec!["Assets/Real.prefab"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn scans_packages_subtree_when_present() {
    let root = fresh_project("packages-scan");

    let body = format!("MonoBehaviour:\n  guid: {TARGET_HEX_STR}\n");
    write(&root.join("Assets/A.prefab"), &body);
    write(
        &root.join("Packages/com.boxcat.libs/Runtime/Pkg.prefab"),
        &body,
    );

    let hits = find_usages(&root, target_hex()).unwrap();
    let paths: Vec<_> = hits.iter().map(rel).collect();
    assert_eq!(
        paths,
        vec!["Assets/A.prefab", "Packages/com.boxcat.libs/Runtime/Pkg.prefab"]
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn finds_multiple_hits_in_same_file_with_distinct_line_numbers() {
    let root = fresh_project("multi-hit");

    let body = format!(
        "header: 1\n\
         a: {{guid: {TARGET_HEX_STR}}}\n\
         filler: x\n\
         b: {{guid: {TARGET_HEX_STR}}}\n\
         filler: y\n\
         filler: z\n\
         c: {{guid: {TARGET_HEX_STR}}}\n"
    );
    write(&root.join("Assets/multi.prefab"), &body);

    let hits = find_usages(&root, target_hex()).unwrap();
    let lines: Vec<u32> = hits.iter().map(|h| h.line).collect();
    assert_eq!(lines, vec![2, 4, 7]);
    for h in &hits {
        assert_eq!(rel(h), "Assets/multi.prefab");
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn empty_when_nothing_references_the_guid() {
    let root = fresh_project("no-refs");
    write(
        &root.join("Assets/Unrelated.prefab"),
        "MonoBehaviour:\n  guid: 11111111111111111111111111111111\n",
    );

    let hits = find_usages(&root, target_hex()).unwrap();
    assert!(hits.is_empty(), "expected no hits, got {hits:?}");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn errors_when_assets_dir_missing() {
    let root = unique_tmp("no-assets");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();

    let err = find_usages(&root, target_hex()).unwrap_err();
    assert!(matches!(err, WalkError::AssetsMissing { .. }));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn results_sorted_by_path_then_line_for_stable_output() {
    let root = fresh_project("sort-order");

    // Plant files in an order designed to defeat naive insertion order —
    // parallel walker hits these in nondeterministic order.
    for name in ["Z.prefab", "A.prefab", "M.prefab"] {
        write(
            &root.join(format!("Assets/{name}")),
            &format!("filler\n  guid: {TARGET_HEX_STR}\nfiller\n  guid: {TARGET_HEX_STR}\n"),
        );
    }

    let hits = find_usages(&root, target_hex()).unwrap();
    let keys: Vec<_> = hits.iter().map(|h| (rel(h), h.line)).collect();
    assert_eq!(
        keys,
        vec![
            ("Assets/A.prefab".to_owned(), 2),
            ("Assets/A.prefab".to_owned(), 4),
            ("Assets/M.prefab".to_owned(), 2),
            ("Assets/M.prefab".to_owned(), 4),
            ("Assets/Z.prefab".to_owned(), 2),
            ("Assets/Z.prefab".to_owned(), 4),
        ]
    );

    fs::remove_dir_all(&root).ok();
}

// ─── coherence with the baked `asset-db.bin` ─────────────────────────────

#[test]
fn path_resolves_via_bake_then_walks_to_real_refs() {
    // End-to-end: bake a project, then run the CLI's path → guid → walker
    // pipeline (`query::guid_of_path` → `find_usages`) and verify the
    // planted ref shows up alongside the asset's own `.meta`.
    let root = fresh_project("bake-coherence");

    write(&root.join("Assets/Scripts/Card.cs"), "// stub\n");
    write(
        &root.join("Assets/Scripts/Card.cs.meta"),
        &format!("fileFormatVersion: 2\nguid: {TARGET_HEX_STR}\nMonoImporter: {{}}\n"),
    );
    write(
        &root.join("Assets/UI/Holder.prefab"),
        &format!(
            "%YAML 1.1\n\
             --- !u!114 &100\n\
             MonoBehaviour:\n\
               m_Script: {{fileID: 11500000, guid: {TARGET_HEX_STR}, type: 3}}\n\
               m_Name: Holder\n"
        ),
    );

    let out_dir = bake_at(&root);
    let db = query::open(&out_dir).unwrap();
    let entry = query::guid_of_path(&db, "Assets/Scripts/Card.cs").expect("entry baked");
    assert_eq!(entry.guid, target_guid());

    let hex_owned = query::format_guid(entry.guid);
    let hex_bytes: &[u8; 32] = hex_owned.as_bytes().try_into().unwrap();
    let hits = find_usages(&root, hex_bytes).unwrap();
    let paths: Vec<_> = hits.iter().map(rel).collect();
    // The .meta self-reference and the consumer prefab — both legitimate
    // "where does this GUID appear" hits.
    assert_eq!(
        paths,
        vec!["Assets/Scripts/Card.cs.meta", "Assets/UI/Holder.prefab"]
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn find_usages_is_independent_of_bake_artifact() {
    // `find_usages` walks live disk and never reads `asset-db.bin` — a
    // bake's presence or freshness in `Library/` must not change results.
    let root = fresh_project("rebake-stable");

    write(&root.join("Assets/A.cs"), "// a\n");
    write(
        &root.join("Assets/A.cs.meta"),
        &format!("fileFormatVersion: 2\nguid: {TARGET_HEX_STR}\nMonoImporter: {{}}\n"),
    );
    write(
        &root.join("Assets/B.prefab"),
        &format!("MonoBehaviour:\n  m_Script: {{guid: {TARGET_HEX_STR}}}\n"),
    );

    let before: Vec<_> = find_usages(&root, target_hex())
        .unwrap()
        .into_iter()
        .map(|h| (rel(&h), h.line))
        .collect();

    bake_at(&root);

    let after: Vec<_> = find_usages(&root, target_hex())
        .unwrap()
        .into_iter()
        .map(|h| (rel(&h), h.line))
        .collect();

    assert_eq!(before, after);
    assert_eq!(
        before,
        vec![("Assets/A.cs.meta".to_owned(), 2), ("Assets/B.prefab".to_owned(), 2)]
    );

    fs::remove_dir_all(&root).ok();
}
