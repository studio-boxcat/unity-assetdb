# Profiling

> **Related:** [`asset-database.md`](asset-database.md) (artifact + bake
> pipeline) · [`refresh.md`](refresh.md) (Watchman-driven incremental
> path being profiled below).

Three recipes in the project [justfile]:

- **`just profile`** — wall-clock cold / steady-state / single-touch
  refresh via [hyperfine] + the built-in `UNITY_ASSETDB_TIMING=1`
  phase counter.
- **`just bench`** — isolated phase microbenches: `examples/bench_bake.rs`
  for the bake + refresh path, `examples/bench_list.rs` for the
  query/emit hot path. Useful when phase noise drowns out a wall-clock
  signal.
- **`just compare BEFORE AFTER`** — release-binaries from two refs (in
  throwaway worktrees) and runs cold + warm hyperfine head-to-head.
  Strips the manual checkout-rebuild dance.

Defaults to `MEOW_CLIENT=/Users/jameskim/Develop/meow-tower`. Override
via env: `MEOW_CLIENT=/path/to/other/project just profile`.

[justfile]: ../justfile
[hyperfine]: https://github.com/sharkdp/hyperfine

## Quick start

```sh
brew install hyperfine watchman       # one-time

just profile                          # cold / steady-state / touch+refresh
```

## What each phase covers

### `bake` phases — `UNITY_ASSETDB_TIMING=1`

Full-bake breakdown emitted from `bake_inner` in `src/bake.rs`. Four
phases now that the mtime cache is gone:

| Phase | Covers |
|-------|--------|
| `prepass` | `synthesize_missing_metas` — walk `Assets/` + `Packages/<pkg>/` once to find files lacking `.meta`, write minimal metas for new ones. |
| `walk` | Parallel `ignore::WalkBuilder` traversal of `Assets/` + per-`.meta` `process_one` (`.meta` parse, asset YAML peek, sub-asset extraction). |
| `build` | `build_db` — sub-asset dedup pass (type-aware bucketing, depth-2 parent-dir suffix or GUID-suffix for MonoScripts) + script-guid interning + final sort. |
| `write` | `store::write` — bincode encode + atomic rename of `asset-db.bin`. Includes the `watch::since(None)` call that seeds the new clock token. |

### `refresh` phases

The auto-refresh path orchestrated by `src/refresh.rs`. No internal
phase counter (yet); profile via `examples/bench_bake.rs::"refresh
(Watchman empty-delta)"`.

Steady-state cost ≈ `store::read` + `watch::since` + (`store::write` if
the delta is non-empty). Patch cost ≈ steady-state +
`bake::parse_one_raw` × N + `bake::build_db_from_raw`.

## Baseline numbers — meow-tower (18,170 entries, 20,264 `.meta` files)

Captured 2026-05-21 on an M-series mac (12 logical cores), release
binary, Watchman 2026.05.18 installed. Hyperfine means with
`--warmup 2` unless noted.

| Scenario | Total | Notes |
|----------|------:|-------|
| **Cold** (no bin) | 0.7-1.3 s | First call. Full bake + initial Watchman crawl run in parallel. |
| **Steady state** (empty delta) | 120 ms ± 2 | Bin load (5 ms) + `watch::since` (≈ 90 ms incl. settle) + query exec (≈ 5 ms) + process startup (≈ 20 ms). |
| **Patch** (1-file touch) | 150 ms ± 5 | Steady state + parse-one + `build_db_from_raw` + bin rewrite. Cost is dominated by the rewrite, not the parse. |
| **Fresh instance** (Watchman restart) | ≈ 0.9 s | Same shape as cold: refresh sees `Fresh` and falls back to a full bake. |

### Per-phase microbench (`examples/bench_bake.rs`, 20 iters)

```
store::decode (asset-db.bin in-memory)      5.4 ms / iter
walk_for_missing_meta (pre-pass only)      57.4 ms / iter
full bake (all phases)                     856.5 ms / iter
refresh (Watchman empty-delta)             115.7 ms / iter
```

Note `full bake` is ~860 ms even though wall-clock cold is ~1.3 s —
the extra ~400 ms in wall-clock includes process startup *and* the
first Watchman crawl, which the microbench omits (Watchman is already
warmed by the priming bake).

### Where the warm cost goes

Steady state = `store::decode` (5 ms) + `watch::since` round-trip
(≈ 90 ms) + everything else (≈ 20 ms). Watchman dominates.

**The Watchman cost is mostly the 20 ms settle window** (configurable,
not tuned) plus inter-process IPC. Drop the settle and you'd shave
~20 ms at the cost of correctness during burst writes.

### Query subcommands

Same hardware, post-overhaul, 25-run hyperfine means with `--warmup 3`,
release binary, stdout to `/dev/null`. Against a primed bin.

| Subcommand | Mean | σ | Notes |
|---|---:|---:|---|
| `find` (auto-refresh, empty delta) | 120 ms | 2 | Watchman RPC + bin load + scan |
| `bake` (forced full) | ~1 s | varies | Use for scripts / CI / schema bumps |

Pre-overhaul (mtime cache, no Watchman) baseline was ~76 ms warm.
The +44 ms steady-state regression is the cost of precise change
detection (Watchman RPC) — incremental updates no longer have any
false-positive risk (the asset-only-touch hole the mtime cache
documented in `cache_does_not_detect_asset_only_touch` is gone).

### Where future wins could come from

- **Skip `store::read` on the trivially-empty-delta path.** Refresh
  currently reads the full bin (5 ms) even when the only thing it'll
  do is bump the clock. Returning the delta from `watch::since` *first*
  and lazy-reading the bin only when patching saves ~5 ms / steady-
  state query. ~30 LOC of helper restructuring; defer until a
  measured pain point.
- **mmap the bin** — `bincode::decode_from_slice` over a memory-
  mapped file removes the 5 ms decode. ~150 LOC + a fixed-layout
  schema; significant work for a fixed-cost saving the user can't
  perceive at ~120 ms wall.
- **Daemon-mode unity-assetdb** — collapses process startup + bin load
  to ~0; ~hundreds of LOC of IPC + lifecycle. See `TODO.md`.

## Where the cost goes

The cold path is ~85% syscall I/O (`read`, `__open`, `stat`) — the
full-bake walk + parse + dedup pipeline. The `refresh` warm path shifts
the cost to Watchman IPC (tokio runtime startup + a single async block
on the BSER socket); no useful Rust-level hotspots there.

The parser/build hotspots on the bake path:

- `meta::parse` — line-oriented scan of every `.meta`. Hot when the
  project is sprite-heavy.
- `asset::parse` — line-oriented YAML peek for the WithSubAssets
  extensions (`.prefab`/`.controller`/`.anim`/`.mixer`/`.playable`/
  `.asset`/`.spriteatlas*`). Hot for prefab-heavy projects.
- `build_db`'s hashmap churn (`ahash::AHasher`) — ~17 ms on warm bake.
- `bincode::decode_from_slice` — bin load on every query path.

## When to re-profile

- After any change to `walk::walk_meta_files`, `bake::process_one`,
  or the per-worker accumulator (`ThreadLocal` in `bake.rs`) — these
  dominate the cold/full-bake path.
- After any change to `refresh::patch` or the `build_db_from_raw`
  conversion — these dominate the patch path.
- After bumping `SCHEMA_VERSION` — invalidates `asset-db.bin`, so the
  next call is necessarily cold.
- After widening the WithSubAssets extension list — adds per-file YAML
  parsing work that wasn't there before.

Capture the per-phase line + `examples/bench_bake.rs` microbench +
diff against this doc's baseline.
