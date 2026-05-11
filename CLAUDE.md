# unity-assetdb

> **Related:** [`docs/asset-database.md`](docs/asset-database.md) (storage
> schema + bake pipeline) · [`docs/profiling.md`](docs/profiling.md) (wall-
> clock baselines + samply recipe).

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
- `bake` — orchestrator (`BakeOptions`, `bake`, `parse_one`).
- `query` — read-only lookups against a baked `asset-db.bin` (`guid_of_path`,
  `path_of_guid`, `find`, `list`, `alias`).
- `register` — synthesize a minimal `.meta` outside Unity, incremental
  db insert. Advisory-flocked against concurrent bakes.
- `suggest` — fuzzy "did you mean" helper used by the query CLI on miss.

## CLI

```sh
just install                                   # cargo install --path . → ~/.cargo/bin/unity-assetdb

# Bake the index.
unity-assetdb bake [--project <path>] [--out-dir <path>] [--scrub-chars <chars>]

# Queries (TSV by default, --json opt-in). Exit 1 on point-lookup miss
# (guid/path/alias), exit 0 otherwise. find/list never miss (empty = empty).
unity-assetdb guid  <project-rel-path>         # → 32-hex GUID
unity-assetdb path  <guid>                     # → project-rel hint
unity-assetdb find  <pattern>                  # case-insensitive substring on names
unity-assetdb list  [--type <kind>]            # all entries, optional ClassId or Script:<32hex>
unity-assetdb alias <name> [--scrub-chars <c>] # exact-match (auto-scrubs input)

# Register a new asset without booting Unity. Synthesizes a minimal .meta
# with a fresh 128-bit GUID; Unity refills the importer block on next focus
# while preserving the GUID. Idempotent — re-running on an asset that
# already has a .meta prints the existing GUID.
unity-assetdb register <path> [--type <importer>] [--scrub-chars <c>] [--lock-timeout <secs>]
```

Without `--project`, walks up from the cwd until both `Assets/` and `ProjectSettings/`
are found. Without `--out-dir`, writes to `<project>/Library/unity-assetdb/`.

Output discipline: data → stdout, warnings / suggestions / progress → stderr.
TSV cells escape `\t`/`\n`/`\\`. JSON output is one object per line.

`bake` and `register` share an advisory flock on `<out_dir>/.asset-db.lock`
to keep the bin coherent under concurrent invocations.

## Profiling

`just profile` (hyperfine cold/warm + phase breakdown). For flamegraphs,
drive `samply record` directly — see [`docs/profiling.md`](docs/profiling.md)
for the invocation, baseline numbers, and per-phase semantics.

## Status

- **API stability:** pre-1.0; signatures may shift.
- **Errors:** public API returns typed `thiserror` errors —
  `StoreError`, `MetaParseError`, `WalkError`, `BakeError`, `QueryError`,
  `RegisterError`. `BakeError` and `RegisterError` expose
  `Store(StoreError)` variants for matching + `Other(anyhow::Error)` for
  chained context. Internal helpers in `bake.rs` still use `anyhow::Result`
  for ergonomic context chaining; the typed boundary is `pub fn bake` /
  `pub fn parse_one` / `pub fn register`.
- **`register` minimal-meta assumption:** synthesizes the `<Importer>:`
  block with only `externalObjects`/`userData`/`assetBundleName`/
  `assetBundleVariant` fields. Unity, on next editor focus, re-imports
  the asset and rewrites the importer block with project defaults **while
  preserving the GUID**. If a specific importer config is needed (atlas
  packables, texture platform overrides), edit in Unity after focus.
