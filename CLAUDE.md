# unity-assetdb

> **Related:** [`docs/asset-database.md`](docs/asset-database.md) (storage
> schema + bake pipeline) · [`docs/refresh.md`](docs/refresh.md) (Watchman-
> driven incremental refresh) · [`docs/profiling.md`](docs/profiling.md)
> (wall-clock baselines + phase breakdown).

Walks a Unity project's `Assets/` tree, parses `.meta` and asset YAML, and writes a
compact bincode index that maps asset GUIDs (and sub-asset fileIDs) to human-readable
names. Designed for tooling that needs to reason about Unity assets by name without
loading the editor.

Originally extracted from a Unity prefab YAML ↔ JSON converter. Reusable by
any tool that wants the same GUID→name index — e.g. a Rust-side
asset-catalog baker for the Unity client.

## Workspace layout

The repo is a Cargo workspace. The root crate is `unity-assetdb` (the bake +
query pipeline below). One sub-crate lives under `crates/`:

- [`unity-path-rules`](crates/unity-path-rules) — universal Unity filesystem
  predicates (`is_unity_hidden`, `is_opaque_plugin_dir`, `is_submodule_root`,
  `is_opaque_subtree`). Extracted so other tools — e.g.
  [`unity-meta-cop`](https://github.com/studio-boxcat/unity-meta-cop) — can
  honor the same ignore rules without depending on the bake pipeline. Tool-
  specific exclusions (e.g. `is_blacklisted_extension` for this crate's name
  pool) stay private to `unity-assetdb`.

## Modules

The single root crate `unity-assetdb` (everything under `src/`) splits into:

- `store` — on-disk schema (`AssetDb` with `watchman_clock` header,
  `AssetEntry`, `SubAsset`, `AssetType`).
- `class_id` — Unity classID enum (`Sprite=213`, `Prefab=1001`, …).
- `meta` — `.meta` parser (top-level GUID, sprite-sheet sub-assets, importer fields).
- `asset` — asset YAML parser (top class ID, `m_Script.guid`, sub-doc enumeration).
- `walk` — project-root resolver + parallel `Assets/` walker.
- `bake` — full-bake orchestrator (`BakeOptions`, `bake`, `parse_one`).
- `watch` — Watchman wire layer (`since`, `Delta`, `WatchError`). Sync
  facade over `watchman_client`'s tokio API.
- `refresh` — auto-refresh: `watch::since` → patch in place / clock-only
  update / full bake on fresh_instance / no-op nudge if Watchman is
  absent. Every CLI query subcommand goes through this.
- `query` — read-only lookups against a baked `asset-db.bin` (`guid_of_path`,
  `path_of_guid`, `find`, `find_by_hint`, `list`, `alias`).
- `register` — synthesize a minimal `.meta` outside Unity, incremental
  db insert. Advisory-flocked against concurrent bakes.
- `suggest` — fuzzy "did you mean" helper used by the query CLI on miss.
- `usage` — scan project YAML for files referencing a given GUID
  (`find_usages`, `UsageMatch`). Native substitute for `rg <hex>` that
  knows which extensions are Unity YAML.
- `builtin` — Unity engine-builtin GUID predicates (shape + bucket
  discrimination). These all-zeros-except-one-hex GUIDs identify
  assets baked into the engine binary — never in `Assets/`.

## CLI

```sh
just install                                   # cargo install --path . → ~/.cargo/bin/unity-assetdb

# Bake the index. Auto-synthesizes missing `.meta` files (see asset-database.md).
unity-assetdb bake [--project <path>] [--out-dir <path>]

# Queries (TSV by default, --json opt-in). Name/path lookups
# (guid/path/find/alias, or `usage <path>` with an unresolved path)
# exit 1 on miss and print fuzzy "did you mean" suggestions to stderr.
# `list` and `usage`'s file scan never miss (empty output = empty result).
unity-assetdb guid  <path|pattern>             # exact hint → 1 row; else substring on hints
unity-assetdb path  <guid>                     # → project-rel hint
unity-assetdb find  <pattern>                  # case-insensitive substring on names; suggests on miss
unity-assetdb list  [--type <kind>]            # all entries, optional ClassId or Script:<32hex>
unity-assetdb alias <name>                     # exact-match on `<stem>.<ext>` — see [[asset-database.md#name-collisions]].
unity-assetdb usage <guid|path>                # path\tline\ttext for every YAML file referencing the GUID

# Register a new asset without booting Unity. Synthesizes a minimal .meta
# with a fresh 128-bit GUID; Unity refills the importer block on next focus
# while preserving the GUID. Idempotent — re-running on an asset that
# already has a .meta prints the existing GUID.
unity-assetdb register <path> [--type <importer>] [--lock-timeout <secs>]
```

Without `--project`, walks up from the cwd until both `Assets/` and `ProjectSettings/`
are found. Without `--out-dir`, writes to `<project>/Library/unity-assetdb/`.

**Auto-refresh.** Every query subcommand transparently refreshes the bin
via [Watchman](docs/refresh.md) before serving its answer — see
[[refresh.md]] for the decision tree.

Names containing `/` or `^` in source YAML **hard-fail** at bake time.
See [[asset-database.md#reserved-character-policy]].

Output discipline: data → stdout, warnings / suggestions / progress → stderr.
TSV cells escape `\t`/`\n`/`\\`. JSON output is one object per line.

`bake` and `register` share an advisory flock on
`<out_dir>/.asset-db.lock` so concurrent writers don't clobber the bin.
`refresh::refresh` skips the flock on the patch path — concurrent
refreshes converge on the next query via Watchman replay. See
[[refresh.md]] and the `bake_inner` deadlock breadcrumb.

## Profiling

`just profile` (hyperfine cold/warm + phase breakdown) — see
[`docs/profiling.md`](docs/profiling.md) for the recipes, baseline
numbers, and per-phase semantics.

## Status

- **API stability:** pre-1.0; signatures may shift.
- **Errors:** the public boundary (`bake` / `parse_one` / `register` /
  queries) returns typed per-module `thiserror` errors; `BakeError` and
  `RegisterError` expose a `Store(_)` variant for matching plus
  `Other(anyhow::Error)` for chained context. Internal `bake.rs` helpers
  stay on `anyhow::Result`.
- **Minimal-meta assumption (`register` + `bake`'s missing-meta pre-pass):**
  synthesizes only the importer block's `externalObjects` / `userData` /
  `assetBundleName` / `assetBundleVariant` fields. Unity re-imports and
  rewrites it with project defaults on next editor focus **while preserving
  the GUID** — edit in Unity afterward if you need a specific importer
  config (atlas packables, texture platform overrides).
