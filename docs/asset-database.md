# Asset Database

> **Related:** [`profiling.md`](profiling.md) (wall-clock + per-phase
> baselines + samply hotspots for the bake pipeline below).

The asset DB maps Unity asset GUIDs (and sub-asset fileIDs) to human-readable
names. The binary stores the bare name; how downstream consumers render it
(e.g. as a `$Alias` ref, a Unity Addressables key, a catalog entry) is up to
them. The terms are interchangeable — "alias" is the rendering convention,
"name" is the storage field.

Without this index, a sprite reference would be opaque:
`{fileID: 1112529545, guid: 8f57b4a070f7b43bbb3925467e6752ab}`. With it, the
same reference resolves to a stable, readable name (`TX_FlagBig_Main`).

---

## Storage

Two on-disk files, by default under `<project>/Library/<consumer>/`
(gitignored — `Library/` is Unity's regenerable cache directory). The
consumer picks the subdir; this crate doesn't bake the path. The CLI
defaults to `<project>/Library/unity-assetdb/`.

| File | Role | Read by |
|------|------|---------|
| `asset-db.bin` | **Convert artifact** — lean: `(guid, asset_type, name, sub_assets, hint)` per entry. Sorted by GUID for O(log n) lookups. | downstream consumer + bake |
| `asset-db.cache.bin` | **Bake-only cache** — `(hint, mtimes, resolved bake state)` per entry. | bake |

### Binary schema

Defined in `src/store.rs`. Bincode-2, magic-prefixed.

**`asset-db.bin`** — one envelope per project:

| Field | Type | Notes |
|-------|------|-------|
| `schema_version` | `u16` | Bumped on incompatible schema changes; mismatch → re-bake required. |
| `script_types` | `Vec<u128>` | Interned script GUIDs (sorted, dedup'd). Indexed by `AssetType::Script`. |
| `entries` | `Vec<AssetEntry>` | **Sorted by GUID** for O(log n) binary-search lookup. |

Each `AssetEntry`:

| Field | Type | Notes |
|-------|------|-------|
| `guid` | `u128` | 32-hex Unity GUID. |
| `asset_type` | `AssetType` | Tagged enum — `Native(class_id)` or `Script(script_idx)`. See [Asset typing](#asset-typing). |
| `name` | `Box<str>` | Filename stem (with optional collision suffix). See [Name collisions](#name-collisions). |
| `sub_assets` | `Vec<SubAsset { file_id: i64, class_id: u32, name: Box<str> }>` | Sub-asset rows (sprite-sheet entries, sub-clips, plus the implicit Sprite sub-object Unity auto-generates for Single-mode Sprite textures — fileID `21300000` = `ClassId::Sprite × 100_000`, name = filename stem; synthesized at bake since `.meta` omits it). `class_id` stored explicitly so prefab-embedded sub-asset rows (whose hashed fileIDs would otherwise collapse via the `file_id / 100_000` heuristic) retain their real Unity class. Sorted by `file_id`. Synthesis predicate pinned by `bake::tests::synthesize_implicit_sprite_*` (4 branch tests); end-to-end smoke at `tests/bake.rs::implicit_sprite_subasset_synthesis`. |
| `hint` | `Box<str>` | Project-root-relative path (`Assets/Foo.prefab`, `Packages/com.boxcat.libs/Bar.mixer`). Lets downstream consumers locate assets by guid without re-walking the project tree. |

`Box<str>` instead of `String` saves 8 bytes per string (no growable-capacity field) once decoded.

**`asset-db.cache.bin`** — bake-only side file. Same magic-prefixed bincode envelope. Each entry: `(hint, meta_mtime_ns, asset_mtime_ns, guid, asset_type, sub_assets)`. Hint here is the cache lookup key; everything else lets a re-bake reconstruct the entry without re-parsing the .meta + asset. `name` is **not** cached — it's re-derived from the hint's filename stem and re-applied to the [Name collisions](#name-collisions) rule on every bake, so the bare ↔ suffix promote/demote stays current as siblings come and go.

### Asset typing

`AssetType` distinguishes built-in Unity classes from MonoBehaviour-backed assets:

- **`Native(class_id)`** — Unity built-in (Sprite=213, Prefab=1001, Texture2D=28, …). The full table lives in `src/class_id.rs` (sourced from Unity's [Class ID Reference](https://docs.unity3d.com/Manual/ClassIDReference.html)).
- **`Script(idx)`** — MonoBehaviour / ScriptableObject. `idx` indexes `AssetDb::script_types`, whose entries are u128 script GUIDs that match the `guid` field of the corresponding `.cs.meta`. Lets downstream consumers do direct u128 equality when checking "is this asset of script type X?" without re-resolving GUIDs at every lookup.

---

## Lookup rules

A name in pull output resolves by GUID + fileID:

1. **Exact match** on `{guid}:{fileID}` — sub-asset hit (sprite-sheet entry, multi-clip animation). Backed by `AssetEntry::sub_assets`.
2. **GUID-only match** — each `type` has a canonical fileID (e.g. `Prefab` → `100100000`, `Sprite` → `21300000`). See `ClassId::canonical_subobject_fid`. Most assets hit this path.
3. **Texture → Sprite fallback** — Unity often references a sprite via its backing Texture2D's fileID (`21300000`). If the direct match misses, the lookup retries against fileID `2800000` (Texture2D's native fileID). This is consumer-side policy — the crate exposes the data; the consumer decides whether to fall back.

---

## Populating

`unity-assetdb bake [--project <path>] [--out-dir <path>]` walks
`<project>/Assets/` and `<project>/Packages/` in parallel via the
[`ignore`] crate and writes the binary. Without `--project` the
command climbs from CWD until both `Assets/` and `ProjectSettings/`
are found. `--out-dir` redirects both `asset-db.bin` and the sibling
`asset-db.cache.bin` away from the default — used for fixture-regen
recipes that read from an upstream Unity project but must not write
back into it.

**Walker ignore behavior** is intentionally narrower than `ignore`'s
default `standard_filters`:

- Unity-hidden segments (leading `.`, trailing `~`) are filtered — this
  matches Unity's own special-folder rules.
- `.gitignore` files anywhere in the project tree are NOT honored.
  Unity itself doesn't, and a gitignored `.meta` still carries a
  guid that other prefabs can reference. Excluding such files would
  cause spurious "unresolved asset reference" hard-fails on the
  consumer side.
- `Library/`, `Temp/`, build artifacts (`.csproj`, `.sln`) sit
  outside the walker's roots (`Assets/`, `Packages/`), so they're
  never visited regardless of any ignore rules.

**Missing-meta pre-pass** — before the parallel walk, bake scans the same
two roots for files / folders lacking a sibling `.meta` and synthesizes
one for each (delegating to `register::render_meta` / `register::generate_guid`).
This mirrors Unity's editor-focus behavior, so a fresh bake works after
dropping files into the project tree without opening the editor. Each
synthesized path emits an info line through `on_progress`; a summary
count follows when any were created. Direct children of `Packages/`
(`manifest.json`, `packages-lock.json`, package-root dirs) are skipped —
Unity never authors metas for them. Pinned by
`tests/bake.rs::bake_creates_missing_meta_files`.

**Blacklisted-extension exclusion** — files with non-asset extensions
(`.md` docs, `.pspec` source) are skipped by both the meta walker (their
existing `.meta` is not indexed) and the missing-meta pre-pass (no
`.meta` synthesized). The companion `Foo.prefab` is still indexed; only
the sibling `Foo.prefab.md` and `Foo.prefab.pspec` are excluded.
Predicate lives in `walk::is_blacklisted_extension`; pinned by
`walk::tests::is_blacklisted_extension_*` and
`tests/bake.rs::bake_excludes_sidecar_md_and_pspec_files`.

Two classes of folders are visited at the root but never descended into,
so their contents stay free of synthesized metas:

- **Folder-based Android plugins** — names ending in `.androidlib`,
  `.androidpack`, or `.aar`. Unity hands the contents to Gradle untouched
  and never authors per-file metas inside. See [Unity manual: Android
  library project import](https://docs.unity3d.com/Manual/android-library-project-import.html).
  Pinned by `tests/bake.rs::bake_does_not_synthesize_inside_opaque_android_plugin_folders`.
- **Git submodule roots** — any directory with a sibling `.git` file or
  directory. The subtree is owned by another repo; synthesizing metas
  there would dirty an unrelated working tree. Pinned by
  `tests/bake.rs::bake_does_not_synthesize_inside_git_submodules`.

The bake is mtime-cached via `asset-db.cache.bin`: re-runs only re-parse
files whose `.meta` or asset mtime has changed. On a 16k-entry project
(meow-tower), cold ≈ 370 ms, warm ≈ 60 ms.

**Idempotent re-bakes** — when every entry is a cache hit and the count is
stable — skip both file writes entirely, holding mtimes stable across no-op
runs. Set the consumer's verbose-timing flag (`UNITY_ASSETDB_TIMING=1` for
the CLI) for a per-phase line (`cache / walk / build / write`). The `write`
field shows `(skipped)` when the no-op path triggers.

### Library use

```rust
use unity_assetdb::bake::{BakeOptions, bake};
use unity_assetdb::walk::resolve_project_root;

let project_root = resolve_project_root(None)?;
let opts = BakeOptions {
    project_root: project_root.clone(),
    out_dir: project_root.join("Library").join("my-tool"),
    name_sanitizer: None,        // Or Some(Box::new(|s| ...)) to scrub chars
    on_warn: Some(Box::new(|m| eprintln!("{m}"))),
    on_progress: Some(Box::new(|m| eprintln!("{m}"))),
    verbose_timing: false,
    verbose_collisions: false,
};
bake(&opts)?;
```

The library never writes to stderr — every warning / progress line routes
through the optional callbacks. Pass `None` to discard.

### Name collisions

Filename stems aren't unique across a project (e.g. multiple
`Dependencies.asmdef`). The bake's dedup pass operates on a name pool
keyed by `(name, asset_type)` — same-name claims of distinct `asset_type`
(`Foo.png` Texture2D vs. `Foo.prefab` Prefab) DON'T contest because
consumers can discriminate at the lookup layer using the field's declared
C# type.

**Sub-asset namespacing:** sprite-sheet style sub-assets (Sprite
sub-objects on a `.spriteatlas` or texture) join the global pool — they're
addressable as bare names. Prefab-embedded sub-assets (legacy
`AnimationClip` doc inline in a `.prefab`, AnimatorState in a
`.controller`, AudioMixerGroup in a `.mixer`, Timeline tracks in a
`.playable`) are EXCLUDED from the global pool; they live in their parent
prefab's namespace and consumers resolve them through a parent-scoped
addressing scheme.

**No-winner rule:** when ≥ 2 distinct guids claim the same `(name,
asset_type)` pair, **every** claimant gets renamed. Nobody keeps the bare
alias. Single-owner names within a `(name, asset_type)` bucket stay bare.

**Depth-2 suffix rule:** each contested entry's alias is
`stem^<last-2-parent-dirs-of-hint>`, joined with `/`. The suffix is a pure
function of the entry's own hint — no `taken`-map consultation, no
order-dependence:

| Hint | Alias |
|------|-------|
| `Assets/10_UIElements/04_Prefabs/Button.prefab` | `Button^10_UIElements/04_Prefabs` |
| `Assets/20_Contents/SettingsPopup/Prefabs/Button.prefab` | `Button^SettingsPopup/Prefabs` |
| `Assets/20_Contents/WaitForUpdatesPopup/Prefabs/Button.prefab` | `Button^WaitForUpdatesPopup/Prefabs` |

Hints with fewer than 2 parent segments take whatever's available
(`Assets/Foo.prefab` → `Foo^Assets`). Hints with zero parent segments
hard-fail — no suffix possible.

**Hard-fail on shared depth-2 parent.** Two contestants whose hints share
the same last 2 parent dirs (e.g. `Assets/X/Y/Foo.prefab` and
`Pkg/X/Y/Foo.prefab`) compute identical aliases. The bake aborts with both
hints + the asset type in the error; the user renames one in source. No
silent fallback to deeper suffix — that's the order-dependent drift the
depth-2 rule exists to kill.

**What stays stable / what still drifts.** A contested entry's alias is
independent of which siblings exist: adding or removing an unrelated
co-colliding asset never perturbs the others. The only remaining drift is
the bare ↔ suffix promote/demote at the contest boundary: a previously
unique `Foo` flips to `Foo^P/Q` the moment a second `Foo` lands anywhere,
and the surviving entry pops back to bare if the sibling is later removed.
Persisting an alias of a contested asset across bakes is safe; persisting
bare aliases that *could* become contested is not. GUIDs remain the only
truly stable identifier.

The `^` separator is rare in Unity asset paths and (unlike parens) doesn't
collide with naturally-paren-named assets like `QuestWidget (Side).prefab`.

Pinned by `bake::tests::parent_suffix_*` (pure-helper semantics),
`bake::tests::build_db_renames_every_claimant_when_name_is_contested`
(no-winner rule), `build_db_contested_alias_is_independent_of_other_siblings`
(stability), and `build_db_fails_when_two_contestants_share_depth_2_parent`
(hard-fail).

The bake also hard-fails if any `(name, guid, fileID, asset_type)` tuple
appears twice in the final database — a defensive invariant that surfaces
hand-edited corruption or duplicate-GUID copy-paste. Unity's "hidden" path
conventions (folders/files starting with `.` or ending with `~`) are
excluded from the walk so that template/scratch copies don't trip the check.

### Optional name sanitization

The default bake leaves YAML `m_Name` values verbatim. Consumers whose
serialization grammar reserves certain characters (e.g. `/`, `|`, `#`, `\`)
can pass a `name_sanitizer` callback in `BakeOptions` that returns
`Some(rewritten)` for any name that would collide with their grammar. The
bake re-runs the sanitizer once per top-level filename stem and once per
sub-asset name, before dedup; warnings flow through `on_warn`.

[`ignore`]: https://docs.rs/ignore

---

## When to regenerate

- New asset / move / rename / GUID change → re-run `unity-assetdb bake`. Mtime-cached, fast.
- Sub-asset added (new sprite in a sheet, new clip in a model) → same.
- Schema bump → forced re-bake (loader hard-fails on `schema_version` mismatch).
