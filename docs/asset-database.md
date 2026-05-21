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

One on-disk file, by default under `<project>/Library/<consumer>/`
(gitignored — `Library/` is Unity's regenerable cache directory). The
consumer picks the subdir; this crate doesn't bake the path. The CLI
defaults to `<project>/Library/unity-assetdb/`.

| File | Role | Read by |
|------|------|---------|
| `asset-db.bin` | **Convert artifact** — `(guid, asset_type, name, sub_assets, hint)` per entry. Sorted by GUID for O(log n) lookups. Header also carries an opaque Watchman clock token (see [[refresh.md]]). | downstream consumer + bake + refresh |

A second file (`asset-db.cache.bin`) used to live alongside as a
mtime-keyed parse cache. It was removed in schema v7 — see
[[refresh.md]] for the Watchman-driven replacement that obviated it.

### Binary schema

Defined in `src/store.rs`. Bincode-2, magic-prefixed.

**`asset-db.bin`** — one envelope per project:

| Field | Type | Notes |
|-------|------|-------|
| `schema_version` | `u16` | Bumped on incompatible schema changes; mismatch → re-bake required. Currently **7** (Watchman cutover). |
| `watchman_clock` | `Option<String>` | Opaque clock token from the last [`refresh`](refresh.md) or full bake. `None` means "no Watchman state yet" — next refresh treats it as `Fresh` and full-bakes to seed. |
| `script_types` | `Vec<u128>` | Interned script GUIDs (sorted, dedup'd). Indexed by `AssetType::Script`. |
| `entries` | `Vec<AssetEntry>` | **Sorted by GUID** for O(log n) binary-search lookup. |

Each `AssetEntry`:

| Field | Type | Notes |
|-------|------|-------|
| `guid` | `u128` | 32-hex Unity GUID. |
| `asset_type` | `AssetType` | Tagged enum — `Native(class_id)` or `Script(script_idx)`. See [Asset typing](#asset-typing). |
| `name` | `Box<str>` | `<stem>.<ext>` derived from `hint` on every bake, with optional collision suffix appended. See [Name collisions](#name-collisions). |
| `sub_assets` | `Vec<SubAsset { file_id: i64, class_id: u32, name: Box<str> }>` | Every addressable doc inside this asset, regardless of whether it's the file's "top" (first YAML doc) or "main" per `.meta::mainObjectFileID`. Consumers gate "is this row the main asset?" by comparing `file_id` against the parent entry's canonical-fid (`asset_type_to_file_id`) or, for files where `.meta::mainObjectFileID` is non-canonical, the meta-driven value. Includes sprite-sheet entries, sub-clips, the implicit Sprite sub-object Unity auto-generates for Single-mode Sprite textures (fileID `21300000` = `ClassId::Sprite × 100_000`, name = bare filename stem; synthesized at bake since `.meta` omits it), and empty-named sub-docs (Mesh/Curve bodies in `.asset`, embedded clips that don't author `m_Name`) — empty-named rows stay in the parent's vec but bypass the global alias-bucket dedup since the empty name can't disambiguate. `class_id` stored explicitly so prefab-embedded sub-asset rows (whose hashed fileIDs would otherwise collapse via the `file_id / 100_000` heuristic) retain their real Unity class. Sorted by `file_id`. Synthesis predicate pinned by `bake::tests::synthesize_implicit_sprite_*` (4 branch tests); end-to-end smoke at `tests/bake.rs::implicit_sprite_subasset_synthesis`. |
| `hint` | `Box<str>` | Project-root-relative path (`Assets/Foo.prefab`, `Packages/com.boxcat.libs/Bar.mixer`). Lets downstream consumers locate assets by guid without re-walking the project tree. |

`Box<str>` instead of `String` saves 8 bytes per string (no growable-capacity field) once decoded.


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
are found. `--out-dir` redirects `asset-db.bin` away from the default
— used for fixture-regen recipes that read from an upstream Unity
project but must not write back into it.

**Bake vs refresh.** Every query subcommand auto-refreshes via Watchman
([[refresh.md]]) before serving its answer — explicit `bake` is for
scripts, CI, or forcing a re-walk after a schema bump. Most
interactive users never type `bake` directly.

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
are skipped by both the meta walker (their existing `.meta` is not
indexed) and the missing-meta pre-pass (no `.meta` synthesized).
Companion real assets are still indexed; only the sibling blacklisted
file is dropped. Current set:

| Ext | Why |
|-----|-----|
| `.md` | Markdown docs co-located with assets. |
| `.pspec` | pspec serializer source files. |
| `.py`, `.exe` | Vendored tool helpers inside UPM packages (e.g. Firebase's `generate_xml_from_google_services_json`). |
| `.pdb` | Debug symbol sidecars paired with managed `.dll` plugins. |
| `.asmdef`, `.asmref` | Unity assembly-definition assets — GUID-identified by consumers; vendored packages routinely ship `Editor/Assembly.asmref` at identical depth-2 paths. |

Predicate lives in `walk::is_blacklisted_extension`; pinned by
`walk::tests::is_blacklisted_extension_*` and
`tests/bake.rs::bake_excludes_sidecar_md_and_pspec_files`.

Two classes of folders are visited at the root but never descended into,
so their contents stay free of synthesized metas. The predicates live in
the [`unity-path-rules`](../crates/unity-path-rules) sub-crate so other
Unity tools can honor the same rules:

- **Folder-based Android plugins** — names ending in `.androidlib`,
  `.androidpack`, or `.aar` (`is_opaque_plugin_dir`). Unity hands the
  contents to Gradle untouched and never authors per-file metas inside.
  See [Unity manual: Android library project import](https://docs.unity3d.com/Manual/android-library-project-import.html).
  Pinned by `tests/bake.rs::bake_does_not_synthesize_inside_opaque_android_plugin_folders`.
- **Git submodule roots** — any directory with a sibling `.git` file or
  directory (`is_submodule_root`). The subtree is owned by another repo;
  synthesizing metas there would dirty an unrelated working tree. Pinned
  by `tests/bake.rs::bake_does_not_synthesize_inside_git_submodules`.

The bake always re-walks the full tree. There's no longer an in-bake
fast path — incremental updates go through [`refresh`](refresh.md)
(Watchman-driven). On meow-tower, cold full bake ≈ 1 s; subsequent
queries hit the refresh path at ≈ 120 ms steady-state. See
[`profiling.md`](profiling.md) for the breakdown.

Set the consumer's verbose-timing flag (`UNITY_ASSETDB_TIMING=1` for
the CLI) for a per-phase line (`prepass / walk / build / write`).

### Library use

```rust
use unity_assetdb::bake::{BakeOptions, bake};
use unity_assetdb::walk::resolve_project_root;

let project_root = resolve_project_root(None)?;
let opts = BakeOptions {
    project_root: project_root.clone(),
    out_dir: project_root.join("Library").join("my-tool"),
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

**Always-ext aliases.** Every top-level entry's `name` is
`<stem>.<ext>` — the bake derives it directly from the hint on every
run. `Foo.prefab` aliases as `Foo.prefab`, not bare `Foo`. Cross-kind
same-stem collisions
(`BoxKeyObtainLongtake.unity` + `.playable` + `.cs` + `.prefab`,
or `OrgelActivityTimeline.asset` + `.playable`) resolve at the bake
layer via the ext suffix alone, so consumers can drop their
C#-field-type discriminators for the type-distinct cases that used to
need them. Files with no extension (rare — e.g. extensionless binary
blobs) keep a bare-stem alias.

The bake's dedup pass operates on a name pool keyed by
`(name, asset_type)`. Since `name` always carries the extension,
distinct-ext same-stem entries automatically fall into distinct
buckets and never contest. Within a single bucket (same `<stem>.<ext>`,
same `asset_type`, distinct guids) the within-ext disambiguation rules
below apply.

**Sub-asset namespacing:** sprite-sheet style sub-assets (Sprite
sub-objects on a `.spriteatlas` or texture) join the global pool. Their
names stay bare — sub-assets have no file extension of their own — and
consumers address them as `$<sub>@<parent_alias>`, which now naturally
carries the parent's ext (`$Foo@Bar.prefab`). Prefab-embedded sub-assets
(legacy `AnimationClip` doc inline in a `.prefab`, AnimatorState in a
`.controller`, AudioMixerGroup in a `.mixer`, Timeline tracks in a
`.playable`) are EXCLUDED from the global pool entirely; they live in
their parent's namespace and resolve through the same `$…@…` scheme.

**Empty-name bypass:** sub-asset docs whose YAML carries no `m_Name`
(Mesh / Curve / generated-content bodies inside `.asset`, anonymous
embedded clips, etc.) remain in the parent's `sub_assets` vec but
bypass the global alias-bucket dedup pool — every Box_*.asset hosts
30+ empty-named Mesh sub-docs and forcing them through the dedup-claim
step would collide across parents without any name suffix able to
disambiguate. Consumers address these via the `(parent_guid, file_id)`
pair (e.g. pspec's `$@Parent#<fid>` "Embedded sub-asset, unnamed" form
— `#<fid>` is mandatory on that shape).

**First-doc inclusion:** under `WithSubAssets` parse mode every YAML
doc lands in `sub_assets` — including the first. The earlier "top doc
excluded" rule incorrectly conflated YAML order with main-asset
identity; `.asset` files commonly emit a sub-MB first and pin
`mainObjectFileID: 11400000` (the synthetic canonical-fid MB) as the
main. Consumers gate "is this the main?" via the meta-driven
`mainObjectFileID`, not by YAML position.

**No-winner rule:** when ≥ 2 distinct guids claim the same
`(name, asset_type)` pair (same stem + same ext + same type), **every**
claimant gets renamed via the depth-2 path suffix below.

**Depth-2 suffix rule (default):** each contested entry's alias is
`<stem>.<ext>^<last-2-parent-dirs-of-hint>`, joined with `/`. The
suffix is a pure function of the entry's own hint — no `taken`-map
consultation, no order-dependence:

| Hint | Alias |
|------|-------|
| `Assets/10_UIElements/04_Prefabs/Button.prefab` | `Button.prefab^10_UIElements/04_Prefabs` |
| `Assets/20_Contents/SettingsPopup/Prefabs/Button.prefab` | `Button.prefab^SettingsPopup/Prefabs` |
| `Assets/20_Contents/WaitForUpdatesPopup/Prefabs/Button.prefab` | `Button.prefab^WaitForUpdatesPopup/Prefabs` |

Hints with fewer than 2 parent segments take whatever's available
(`Assets/Foo.prefab` → `Foo.prefab^Assets`). Hints with zero parent
segments hard-fail — no suffix possible.

**GUID-suffix rule for `.cs` MonoScripts:** contested `Native(MonoScript)`
entries use `<stem>.cs^<first-8-hex-of-guid>` instead of the path-based
rule (`L.cs^9ddf5ad8`, `L.cs^3751098b`, …). MonoScript filenames are
conventional Unity classnames whose downstream lookups go through GUIDs
regardless, and mirror-package vendoring (UniTask vs. Zenject both
shipping a `Runtime/Utils/L.cs`) makes the path-based depth-2 alias
structurally ambiguous. The GUID suffix is intrinsic to the asset —
survives `git mv` and is independent of sibling churn. 8 hex chars =
~0.01% birthday-collision odds at N=1000; on the rare collision the
bake still hard-fails and the user can regenerate one of the GUIDs.

**Hard-fail on shared depth-2 parent.** Two contestants whose hints share
the same last 2 parent dirs (e.g. `Assets/X/Y/Foo.prefab` and
`Pkg/X/Y/Foo.prefab`) compute identical aliases. The bake aborts with both
hints + the asset type in the error; the user renames one in source. No
silent fallback to deeper suffix — that's the order-dependent drift the
depth-2 rule exists to kill.

**What stays stable / what still drifts.** A contested entry's alias is
independent of which siblings exist: adding or removing an unrelated
co-colliding asset never perturbs the others. The remaining drift is the
ext-bare ↔ ext-with-`^path` promote/demote at the contest boundary
within a single ext bucket — a unique `Foo.prefab` flips to
`Foo.prefab^P/Q` the moment a second same-ext `Foo.prefab` lands
anywhere, and pops back if the sibling is later removed. Cross-ext
same-stem siblings (`Foo.prefab` + `Foo.cs`) DON'T cause this flip
because they sit in distinct buckets. GUIDs remain the only truly stable
identifier.

The `^` separator is rare in Unity asset paths and (unlike parens) doesn't
collide with naturally-paren-named assets like `QuestWidget (Side).prefab`.

Pinned by `bake::tests::build_db_always_appends_ext_to_alias`,
`build_db_disambiguates_cross_ext_collision_via_ext_suffix` (BoxKey
case), `build_db_disambiguates_script_typed_cross_ext_via_ext_suffix`
(Orgel timeline case), `parent_suffix_*` (pure-helper semantics),
`guid_suffix_uses_first_8_hex_of_guid` +
`build_db_uses_guid_suffix_for_contested_monoscripts` (MonoScript
carve-out), `build_db_renames_every_claimant_when_name_is_contested`
(no-winner rule), `build_db_contested_alias_is_independent_of_other_siblings`
(stability), and `build_db_fails_when_two_contestants_share_depth_2_parent`
(hard-fail).

The bake also hard-fails if any `(name, guid, fileID, asset_type)` tuple
appears twice in the final database — a defensive invariant that surfaces
hand-edited corruption or duplicate-GUID copy-paste. Unity's "hidden" path
conventions (folders/files starting with `.` or ending with `~`) are
excluded from the walk so that template/scratch copies don't trip the check.

### Reserved-character policy

The bake leaves YAML `m_Name` values verbatim with one universal
exception: **`/` is rejected at bake time** for any top-level or
sub-asset name. The character is reserved as the Unix filesystem path
separator *and* as a structural delimiter in every consumer-side
reference grammar we've encountered; silently rewriting it would mask
malformed source. The error surfaces through `BakeError` and names the
offending hint so the user can fix the YAML.

Other consumer-specific reserved chars (`#`, `@`, `|`, etc.) are not
this crate's concern — consumers validate lazily at ref-compose time
(e.g. `pspec`'s `compose_asset_shortcut`).

[`ignore`]: https://docs.rs/ignore

---

## When to regenerate

- New asset / move / rename / GUID change → next `unity-assetdb` query auto-refreshes via Watchman; explicit `bake` only needed for schema bumps or forced re-walks. See [[refresh.md]].
- Sub-asset added (new sprite in a sheet, new clip in a model) → same.
- Schema bump → forced re-bake (loader hard-fails on `schema_version` mismatch).
