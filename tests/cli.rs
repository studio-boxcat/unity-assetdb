//! CLI contract tests. Drives the `unity-assetdb` binary as a subprocess
//! and asserts the stdout / stderr / exit-code shape every query
//! subcommand promises.
//!
//! Scope: contract behavior only — output discipline, exit codes,
//! "did you mean:" suggestions on miss, TSV-vs-JSON shape. Logic
//! correctness lives in the library unit + integration tests
//! (`tests/bake.rs` etc.). Watchman behavior is irrelevant here; each
//! test starts from a clean tempdir, so the bin's first read is
//! always missing → full bake.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;

// ─── fixture ─────────────────────────────────────────────────────────────

/// Per-test project tree. Mirrors `tests/bake.rs::make_fixture` but
/// kept here so the CLI suite stays self-contained — these tests run
/// against the binary, not the library, so reaching into bake's
/// fixture would couple the two crates' test machinery.
fn make_fixture(root: &Path) {
    fs::create_dir_all(root.join("ProjectSettings")).unwrap();
    fs::write(
        root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    )
    .unwrap();

    fs::create_dir_all(root.join("Assets/UI")).unwrap();
    fs::write(
        root.join("Assets/UI/Foo.prefab"),
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1001 &100100000\nPrefabInstance:\n  m_ObjectHideFlags: 0\n",
    )
    .unwrap();
    fs::write(
        root.join("Assets/UI/Foo.prefab.meta"),
        "fileFormatVersion: 2\nguid: aaaa1111aaaa1111aaaa1111aaaa1111\nPrefabImporter:\n",
    )
    .unwrap();

    fs::create_dir_all(root.join("Assets/SO")).unwrap();
    fs::write(
        root.join("Assets/SO/Bar.asset"),
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nMonoBehaviour:\n  m_Name: Bar\n",
    )
    .unwrap();
    fs::write(
        root.join("Assets/SO/Bar.asset.meta"),
        "fileFormatVersion: 2\nguid: bbbb2222bbbb2222bbbb2222bbbb2222\nNativeFormatImporter:\n",
    )
    .unwrap();
}

fn unique_tmp(label: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("unity-assetdb-cli-{label}-{pid}-{nanos}"))
}

/// Run the binary with `--project` + `--out-dir` appended after the
/// subcommand. clap's `global = true` on those args makes them legal
/// at any subcommand level but they must come after the subcommand
/// itself — `unity-assetdb find Foo --project <p>`, not the reverse.
fn run(root: &Path, subcommand_args: &[&str]) -> Command {
    let mut c = Command::cargo_bin("unity-assetdb").expect("binary exists");
    let out_dir = root.join("Library/unity-assetdb");
    c.args(subcommand_args)
        .arg("--project")
        .arg(root)
        .arg("--out-dir")
        .arg(out_dir);
    c
}

/// Bake a fresh bin for the fixture. Most query tests rely on this
/// having run first.
fn prime_bake(root: &Path) {
    run(root, &["bake"]).assert().success();
}

// ─── bake ────────────────────────────────────────────────────────────────

#[test]
fn bake_writes_bin_and_exits_zero() {
    let root = unique_tmp("bake");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);

    run(&root, &["bake"]).assert().success();

    assert!(
        root.join("Library/unity-assetdb/asset-db.bin").exists(),
        "asset-db.bin not written",
    );
    fs::remove_dir_all(&root).ok();
}

// ─── find ────────────────────────────────────────────────────────────────

#[test]
fn find_hit_emits_tsv_row() {
    let root = unique_tmp("find-hit-tsv");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["find", "Foo"]).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    // TSV row: <guid>\t<name>\t<type>\t<hint>\n
    let line = stdout.lines().next().expect("at least one row");
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(cols.len(), 4, "expected 4 TSV cols, got: {line:?}");
    assert_eq!(cols[0].len(), 32, "guid is 32 hex chars");
    assert_eq!(cols[1], "Foo.prefab");
    assert_eq!(cols[2], "Prefab");
    assert_eq!(cols[3], "Assets/UI/Foo.prefab");
    fs::remove_dir_all(&root).ok();
}

#[test]
fn find_hit_with_json_emits_one_object_per_line() {
    let root = unique_tmp("find-hit-json");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["find", "Foo", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    let line = stdout.lines().next().expect("at least one row");
    // Minimal structural check: looks like an object with the four
    // documented keys. We don't pull in serde_json just for this.
    assert!(line.starts_with('{') && line.ends_with('}'));
    for key in ["\"guid\":", "\"name\":", "\"type\":", "\"hint\":"] {
        assert!(line.contains(key), "missing key {key} in: {line}");
    }
    fs::remove_dir_all(&root).ok();
}

#[test]
fn find_miss_exits_one_with_did_you_mean() {
    let root = unique_tmp("find-miss");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    // "Foo.prefad" sits at edit distance 1 from "Foo.prefab" — well
    // within `suggest::suggest`'s ≤2 threshold. Triggers the
    // suggestion-list branch in `print_miss`.
    let out = run(&root, &["find", "Foo.prefad"]).assert().code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("name not found: Foo.prefad"),
        "expected miss line, got stderr: {stderr}",
    );
    assert!(
        stderr.contains("did you mean:") && stderr.contains("Foo.prefab"),
        "expected suggestion header + Foo.prefab, got stderr: {stderr}",
    );
    fs::remove_dir_all(&root).ok();
}

// ─── alias ───────────────────────────────────────────────────────────────

#[test]
fn alias_hit_emits_row() {
    let root = unique_tmp("alias-hit");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["alias", "Foo.prefab"]).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(
        stdout.lines().any(|l| l.contains("Foo.prefab")),
        "expected hit, got: {stdout}",
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn alias_miss_exits_one_with_did_you_mean() {
    let root = unique_tmp("alias-miss");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    // "Foo.prefad" sits at edit-distance 1 from "Foo.prefab" — within
    // the suggest threshold. Pin both the miss line and the suggestion
    // list header.
    let out = run(&root, &["alias", "Foo.prefad"]).assert().code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("alias not found: Foo.prefad")
            && stderr.contains("did you mean:")
            && stderr.contains("Foo.prefab"),
        "expected alias miss + suggestion + neighbor, got: {stderr}",
    );
    fs::remove_dir_all(&root).ok();
}

// ─── guid ────────────────────────────────────────────────────────────────

#[test]
fn guid_with_exact_path_emits_single_row() {
    let root = unique_tmp("guid-exact");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["guid", "Assets/UI/Foo.prefab"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "exact-hint hit should return 1 row");
    assert!(lines[0].contains("aaaa1111aaaa1111aaaa1111aaaa1111"));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn guid_substring_fallback_lists_matches() {
    let root = unique_tmp("guid-substr");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    // "UI" isn't a full hint but appears as a path segment — substring fallback.
    let out = run(&root, &["guid", "UI"]).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(
        stdout.lines().any(|l| l.contains("Assets/UI/Foo.prefab")),
        "expected UI substring hit, got: {stdout}",
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn guid_miss_exits_one_with_did_you_mean() {
    let root = unique_tmp("guid-miss");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    // "Assets/UI/Foo.prefad" sits at edit distance 1 from
    // "Assets/UI/Foo.prefab" — within the suggest threshold.
    let out = run(&root, &["guid", "Assets/UI/Foo.prefad"])
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("path not found:")
            && stderr.contains("did you mean:")
            && stderr.contains("Assets/UI/Foo.prefab"),
        "expected path miss + suggestion + neighbor, got: {stderr}",
    );
    fs::remove_dir_all(&root).ok();
}

// ─── path ────────────────────────────────────────────────────────────────

#[test]
fn path_hit_emits_hint() {
    let root = unique_tmp("path-hit");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["path", "aaaa1111aaaa1111aaaa1111aaaa1111"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert_eq!(stdout.trim(), "Assets/UI/Foo.prefab");
    fs::remove_dir_all(&root).ok();
}

#[test]
fn path_miss_exits_one() {
    let root = unique_tmp("path-miss");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["path", "deadbeefdeadbeefdeadbeefdeadbeef"])
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("guid not found:"),
        "expected guid miss line, got stderr: {stderr}",
    );
    fs::remove_dir_all(&root).ok();
}

// ─── list ────────────────────────────────────────────────────────────────

#[test]
fn list_emits_one_row_per_entry() {
    let root = unique_tmp("list-all");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["list"]).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert_eq!(stdout.lines().count(), 2, "expected 2 entries (Foo + Bar)");
    fs::remove_dir_all(&root).ok();
}

#[test]
fn list_with_type_filter_narrows_to_prefab() {
    let root = unique_tmp("list-type");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["list", "--type", "Prefab"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "only Foo.prefab matches");
    assert!(lines[0].contains("Foo.prefab"));
    fs::remove_dir_all(&root).ok();
}

// ─── usage ───────────────────────────────────────────────────────────────

#[test]
fn usage_empty_when_no_references() {
    let root = unique_tmp("usage-empty");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    // GUID that doesn't appear in any fixture file — `.meta` files
    // ARE scanned (they're YAML and carry `guid:`), so a fixture-asset
    // GUID would self-match. Pick a hex that's nowhere on disk to
    // exercise the true zero-hit path.
    let out = run(&root, &["usage", "cccccccccccccccccccccccccccccccc"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(stdout.is_empty(), "expected empty stdout, got: {stdout:?}");
    fs::remove_dir_all(&root).ok();
}

#[test]
fn usage_finds_meta_self_reference() {
    // The `.meta` file for any asset contains its own GUID; usage
    // treats `.meta` as YAML and reports that self-reference. Pin
    // this so a future restriction (e.g. excluding the asset's own
    // .meta) surfaces here rather than as a downstream surprise.
    let root = unique_tmp("usage-self");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["usage", "aaaa1111aaaa1111aaaa1111aaaa1111"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Assets/UI/Foo.prefab.meta"),
        "expected meta self-ref, got: {stdout:?}",
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn usage_with_unresolved_path_exits_one() {
    let root = unique_tmp("usage-path-miss");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    prime_bake(&root);

    let out = run(&root, &["usage", "Assets/Bogus/Nope.prefab"])
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("path not found:"),
        "expected path miss line, got stderr: {stderr}",
    );
    fs::remove_dir_all(&root).ok();
}

// ─── auto-bake fallback ──────────────────────────────────────────────────

#[test]
fn query_without_prior_bake_auto_bakes() {
    let root = unique_tmp("auto-bake");
    let _ = fs::remove_dir_all(&root);
    make_fixture(&root);
    // No prime_bake! `find` should auto-bake on its own via
    // open_db_or_refresh's "any StoreError → full bake" branch.

    run(&root, &["find", "Foo"])
        .assert()
        .success();
    assert!(
        root.join("Library/unity-assetdb/asset-db.bin").exists(),
        "auto-bake should have written the bin",
    );
    fs::remove_dir_all(&root).ok();
}
