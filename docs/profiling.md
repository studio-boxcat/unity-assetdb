# Profiling

> **Related:** [`asset-database.md`](asset-database.md) (the artifact + bake
> pipeline being profiled).

Two recipes in the project [justfile]:

- **`just profile`** — wall-clock cold/warm + per-phase timings via
  [hyperfine] + the built-in `UNITY_ASSETDB_TIMING=1` phase counter.
- **`just profile-flamegraph`** — sampling profile via [samply]; output is a
  Firefox Profiler JSON.

Both default to `MEOW_CLIENT=/Users/jameskim/Develop/meow-tower`. Override
via env: `MEOW_CLIENT=/path/to/other/project just profile`.

[justfile]: ../justfile
[hyperfine]: https://github.com/sharkdp/hyperfine
[samply]: https://github.com/mstange/samply

## Quick start

```sh
brew install hyperfine                # one-time
cargo install samply                  # one-time

just profile                          # cold/warm + phase breakdown
just profile-flamegraph               # cold flamegraph (default)
just profile-flamegraph cold=0        # warm flamegraph
samply load /tmp/unity-assetdb-profile/profile.json   # open viewer
```

## What each phase covers

The `UNITY_ASSETDB_TIMING=1` line breaks down the bake into four phases.
Source: `BakeOptions::verbose_timing` in `src/bake.rs`.

| Phase | Covers |
|-------|--------|
| `cache` | `store::read_cache` — decode `asset-db.cache.bin` into the in-memory `CacheMap` (HashMap). |
| `walk` | Parallel `ignore::WalkBuilder` traversal of `Assets/` + per-`.meta` `process_one` (mtime check, `.meta` parse, asset YAML peek, optional class-based sub-doc filter). Workers send results via `mpsc` channels. |
| `build` | `build_db` — sub-asset dedup pass (type-aware bucketing, parent-dir suffix walk for collisions) + script-guid interning + final sort. |
| `write` | `store::write` + `store::write_cache` — bincode encode + file write. Shows `(skipped)` on the no-op path (every entry was a cache hit AND nothing dropped from the cache). |

## Baseline numbers — meow-tower (18,171 entries, 20,562 `.meta` files)

Captured 2026-05-11 against
[meow-tower](https://github.com/studio-boxcat/meow-tower)'s `Assets/`
tree on an M-series mac, post-optimization (`1b05485` single-pass
parser + cache-hit stat trim, `da74104` walker `standard_filters` off).
Numbers are 8-run hyperfine means with `--warmup 3`.

| Scenario | Total | Notes |
|----------|------:|-------|
| **Cold, OS cache cold** | ~800 ms | First bake after a fresh checkout; every `.meta`/`.asset` read hits disk. Dominated by `walk` (~770 ms). |
| **Warm (full hit)** | 64 ms ± 5 ms | Every entry from `asset-db.cache.bin`; `write` skips the no-op path. |

### Per-phase breakdown

```
warm:  walked=20562 hit=18171 parsed=0
       cache=5.1ms walk=42.8ms build=17.7ms write=(skipped) total=65.7ms

cold:  walked=20562 hit=0 parsed=18171
       cache=0.0ms walk=768ms build=24.7ms write=4.9ms total=798ms
       (OS cache cold; warm-OS rerun after this writes ~370ms)
```

### Optimization history

| Change | Warm walk | Total warm |
|--------|----------:|----------:|
| Baseline (pre-optimization) | 47 ms | 80 ms |
| `1b05485` single-pass parser + cache-hit stat trim | 46 ms | 76 ms |
| `da74104` `standard_filters(false)` (skip gitignore parse) | **43 ms** | **64 ms** |

`da74104` also flipped a semantic: `.gitignore` files inside
`Assets/`/`Packages/` are no longer honored (Unity itself doesn't
honor them; gitignored `.meta` files still carry guids that prefabs
can reference). 8 previously-excluded entries now bake in meow-tower
— Zenject codegen, SmartLibrary `.asset` files, dev scratch `.cs`.

### Headline observations

- **`walk` dominates both paths.** Warm: 43 ms of 66 ms (65%) just to stat
  20,562 `.meta` files + check 18,171 cache keys. Cold: ~770 ms of 800 ms.
- **`build` is small and stable.** ~18 ms for the dedup pass over 18k
  entries; doesn't vary with cold/warm because it operates on in-memory
  `RawEntry`s post-walk.
- **The no-op-skip write path saves ~7 ms** on the warm path — visible in
  the gap between `write=(skipped)` and the `wrote` cold value.
- **Cold-walk is parser-bound, not IO-bound, on a warm OS cache.** 1.31 s
  drops to ~340 ms with the OS cache hot — IO is ~1 s of the cold-cold
  number; the remaining 340 ms is `.meta` + asset YAML parse + the
  type-aware dedup setup, scattered across the worker threads.

## Flamegraph reading

`just profile-flamegraph` writes `/tmp/unity-assetdb-profile/profile.json`.
Open with `samply load <path>` to launch Firefox Profiler with the data
loaded. Expected hotspots on the cold path:

- `meta::parse_sprite_sheet` — line-oriented scan of every texture's
  `.meta`. Plenty of `str::lines` + `trim` overhead per file but constant
  per-file; only dominant when the project is sprite-heavy.
- `asset::parse` — line-oriented YAML peek for the WithSubAssets
  extensions (`.prefab`/`.controller`/`.anim`/`.mixer`/`.playable`/
  `.asset`/`.spriteatlas*`). Hot for projects with many prefabs.
- `bincode::decode_from_slice` on the cache path — only fires when a
  cache file is present. Inert on the cold-cold path.

`build_db`'s hashmap churn appears as `ahash::AHasher` callers but it's
~17 ms total, so it'll be visually thin in the flamegraph.

## Comparing to pspec's wrapper

`pspec bake-asset-db` adds:
- A `sanitize_asset_name` callback that fires once per top-level filename
  + once per sub-asset name.
- A warn sink that writes to stderr on rewrite or worker error.
- The `Library/pspec/` out-dir convention (`pspec_db_dir` in pspec's
  `lib.rs`) rather than the crate default `Library/unity-assetdb/`.

None of these meaningfully change the cost profile — the sanitizer is
a `RESERVED_NAME_CHARS` scan over ≤256 chars per name, the warn sink
fires on a handful of names in practice. The crate-vs-pspec wall-clock
delta is within noise (`hyperfine` runs on both bins).

## When to re-profile

- After any change to `walk::walk_meta_files` or the per-worker
  accumulator (`ThreadLocal` in `bake.rs`) — the walk dominates both
  paths.
- After bumping `SCHEMA_VERSION` — invalidates `asset-db.cache.bin`, so
  the next bake is necessarily cold.
- After widening the WithSubAssets extension list — adds per-file YAML
  parsing work that wasn't there before.

Capture the per-phase line + diff against this doc's baseline.
