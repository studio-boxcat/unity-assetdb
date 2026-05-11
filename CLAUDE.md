# unity-assetdb

Walks a Unity project's `Assets/` tree, parses `.meta` and asset YAML, and writes a
compact bincode index that maps asset GUIDs (and sub-asset fileIDs) to human-readable
names. Designed for tooling that needs to reason about Unity assets by name without
loading the editor.

Originally extracted from a Unity prefab YAML ↔ JSON converter. Reusable by
any tool that wants the same GUID→name index — e.g. a Rust-side
asset-catalog baker for the Unity client.

## Crate layout

- `store` — on-disk schema (`AssetDb`, `AssetEntry`, `SubAsset`, `AssetType`).
- `class_id` — Unity classID enum (`Sprite=213`, `Prefab=1001`, …).
- `meta` — `.meta` parser (top-level GUID, sprite-sheet sub-assets, importer fields).
- `asset` — asset YAML parser (top class ID, `m_Script.guid`, sub-doc enumeration).
- `walk` — project-root resolver + parallel `Assets/` walker.
- `bake` — orchestrator: `BakeOptions { project_root, out_dir, name_sanitizer, on_warn, on_progress }`.

## CLI

```sh
unity-assetdb bake [--project <path>] [--out-dir <path>]
```

Without `--project`, walks up from the cwd until both `Assets/` and `ProjectSettings/`
are found. Without `--out-dir`, writes to `<project>/Library/unity-assetdb/`.

## Profiling

`just profile` (hyperfine cold/warm + phase breakdown). For flamegraphs,
drive `samply record` directly — see [`docs/profiling.md`](docs/profiling.md)
for the invocation, baseline numbers, and per-phase semantics.

## Status

- **API stability:** pre-1.0; signatures may shift.
- **Errors:** public API returns typed `thiserror` errors —
  `StoreError`, `MetaParseError`, `WalkError`, `BakeError`. `BakeError`
  exposes `Store(StoreError)` / `Walk(WalkError)` variants for matching
  + `Other(anyhow::Error)` for the orchestrator's chained context.
  Internal helpers in `bake.rs` still use `anyhow::Result` for ergonomic
  context chaining; the typed boundary is `pub fn bake`.
