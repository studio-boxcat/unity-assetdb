# Profiling

> **Related:** [`asset-database.md`](asset-database.md) (the artifact + bake
> pipeline being profiled).

One recipe in the project [justfile]:

- **`just profile`** — wall-clock cold/warm + per-phase timings via
  [hyperfine] + the built-in `UNITY_ASSETDB_TIMING=1` phase counter.

Defaults to `MEOW_CLIENT=/Users/jameskim/Develop/meow-tower`. Override
via env: `MEOW_CLIENT=/path/to/other/project just profile`.

For a flamegraph, drive [samply] directly (release symbols come from
the line-tables-only debug info pinned in `Cargo.toml`):

```sh
cargo build --release
rm -f /tmp/unity-assetdb-profile/asset-db{,.cache}.bin   # cold only
samply record --unstable-presymbolicate --save-only \
  --output /tmp/unity-assetdb-profile/cold.json \
  target/release/unity-assetdb bake --project "$MEOW_CLIENT" --out-dir /tmp/unity-assetdb-profile
samply load /tmp/unity-assetdb-profile/cold.json   # opens Firefox Profiler
```

`--unstable-presymbolicate` emits a `.syms.json` sidecar so the trace
opens with full symbols even without the binary alongside.

[justfile]: ../justfile
[hyperfine]: https://github.com/sharkdp/hyperfine
[samply]: https://github.com/mstange/samply

## Quick start

```sh
brew install hyperfine                # one-time
cargo install samply                  # one-time (only if flamegraphing)

just profile                          # cold/warm + phase breakdown
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

## Baseline numbers — meow-tower (18,169 entries, 20,560 `.meta` files)

Captured 2026-05-11 against
[meow-tower](https://github.com/studio-boxcat/meow-tower)'s `Assets/`
tree on an M-series mac (12 logical cores), post-optimization
(`1b05485` single-pass parser + cache-hit stat trim, `da74104`
walker `standard_filters` off). Numbers are 5-run hyperfine means
with `--warmup 2`.

| Scenario | Total | Notes |
|----------|------:|-------|
| **Cold, OS cache cold** | ~800 ms | First bake after a fresh checkout; every `.meta`/`.asset` read hits disk. Dominated by `walk` (~770 ms). |
| **Warm (full hit)** | 64 ms ± 5 ms | Every entry from `asset-db.cache.bin`; `write` skips the no-op path. |

### Per-phase breakdown

```
warm:  walked=20560 hit=18169 parsed=0
       cache=3.5ms walk=34.6ms build=17.5ms write=(skipped) total=55.6ms

cold:  walked=20560 hit=0 parsed=18169
       cache=0.0ms walk=745ms build=18.4ms write=5.9ms total=769ms
       (OS cache cold; warm-OS rerun after this writes ~400ms)
```

### Query + register subcommands

Captured 2026-05-12, same hardware. Hyperfine 15-run means, `--warmup 3`,
release binary, stdout to `/dev/null`. Against the warm bake above
(18,169-entry `asset-db.bin`).

| Subcommand | Mean | σ | What it does |
|---|---:|---:|---|
| `path <guid>` | 5.2 ms | 0.2 | bin load + binary-search by guid |
| `alias <name>` (hit) | 5.8 ms | 0.5 | bin load + linear scan on name |
| `guid <path>` (hit) | 5.3 ms | 0.3 | bin load + linear scan on hint |
| `find <pattern>` | 6.5 ms | 0.4 | bin load + ASCII case-insensitive substring scan |
| `list --type Sprite` | 7.8 ms | 0.7 | bin load + filter + emit 1,041 rows (BufWriter) |
| `list` (full emit) | 7.9 ms | 0.4 | bin load + emit all 18,169 rows (BufWriter + u128_hex) |
| `register` (idempotent) | 7–12 ms | (variable) | bin load + parse existing meta + diff |
| `register` (fresh asset) | 10.9 ms | 1.1 | bin load + synthesize meta + insert row + atomic write |
| `guid <pattern>` (substring, hit) | 6.6 ms | 0.3 | bin load + exact scan (miss) + ASCII substring scan on `hint` |
| `guid <pattern>` (substring, miss) | 7.5 ms | 0.3 | +suggest pool scan over hints |
| `usage <guid>` (heavy, 121 hits) | 510 ms | 45 | parallel walk over 30,955 YAML files (58 MB total) + memmem |
| `usage <guid>` (light, 3 hits) | 558 ms | 39 | same walk; hit count is in the noise |

Last four rows recaptured 2026-05-13 against an 18,197-entry bake of the
same project (+28 entries since the original 2026-05-12 capture).

`usage`'s cost is dominated by the walk, not the per-file scan. For
context, `rg <hex>` over the same file set (`--no-ignore -t unity` with
the same extension list) measures 494 ms ± 47 ms on this hardware —
i.e. `usage` is at the I/O-bound floor.

### Where the time actually goes

Microbenched via `examples/bench_list.rs` (run with
`cargo build --release --example bench_list && target/release/examples/bench_list`):

| Phase | Cost / iter | Notes |
|---|---:|---|
| `fs::read` (2.1 MB) | 0.07 ms | OS page cache after warmup |
| `store::decode` (bincode in-memory) | 1.79 ms | **Floor every query pays.** |
| iter all entries (no IO) | 0.01 ms | guid-sorted Vec; trivial |
| `write_row` × 18 k → sink | 2.04 ms | `std::fmt` for `{:032x}` + write_tsv_escaped |
| Same but with `u128_hex` | 1.70 ms | −17% per row; LUT bypass of `fmt::Write` dispatch |
| Raw write_all bytes × 18 k | 0.20 ms | floor for the emit loop (no formatting) |

Total CPU for a full `list`: 1.79 ms (decode) + 1.70 ms (formatting) +
~0.2 ms (byte writes) ≈ 3.7 ms. Real-world hyperfine reports 7.9 ms,
so ~4 ms is process startup (libstd, clap, binary load) — fixed cost.

### Optimization history (query path)

| Change | `list` full | Notes |
|--------|------------:|-------|
| Baseline (pre-`BufWriter`) | 16.8 ms | Each `writeln!` was its own syscall when piped |
| `BufWriter::new(stdout.lock())` | ~10 ms | Batch into 8 KB chunks; ~200 syscalls instead of 18 k |
| `u128_hex` LUT instead of `{:032x}` | 7.9 ms | Skip `std::fmt::write` trait dispatch for the GUID columns |

### Probed and rejected

- **bincode `with_fixed_int_encoding()`**: ~5% decode speedup (1.79 → 1.67 ms)
  at +24% file size (2.20 → 2.72 MB). Schema bump for marginal gain. Skipped.
- **Memory-map + offset index** to skip full decode for point lookups: ~1.8 ms
  potential saving on `path`/`guid`/`alias`, requires a new on-disk format
  with a sorted (guid, offset) table appended to the bin. ~150 LOC of
  schema work for a saving the user can't perceive at this scale.

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

- **`walk` dominates both paths** — ~62% of warm wall (34.6 of 55.6 ms)
  and ~97% of cold wall (745 of 769 ms). The Deep profile below
  unpacks where the walk time actually goes.
- **`build` is small and stable** (~17 ms over 18 k entries). Doesn't
  vary with cold/warm because it operates on in-memory `RawEntry`s
  post-walk.
- **The no-op write-skip path saves ~6 ms** on the warm path — visible
  in the gap between `write=(skipped)` and the cold `wrote` value.

## Flamegraph reading

Open the samply JSON with `samply load <path>` to launch Firefox
Profiler.

### Deep profile (2026-05-11)

Self-time breakdown, symbolicated via samply `--unstable-presymbolicate`
against the release binary with line tables:

**Cold path (~400 ms, 12 workers, ~5 k samples):**

| Bucket | Self time | What it is |
|--------|----------:|------------|
| `read` syscall | 61.6% | Reading `.meta` + asset YAML from disk |
| `__open` | 15.1% | File opens |
| `stat` | 6.2% | File metadata |
| `__semwait_signal` | 7.9% | Thread idle/coordination |
| `__getdirentries64` | 1.7% | Directory enumeration |
| `core::slice::memchr` + `str::trim_start_matches` | ~2.3% | YAML line scanning |
| Everything else (parse logic, hashing, alloc) | ~5% | — |

**~85% pure syscall I/O.** Cold has essentially no Rust-level
optimization headroom without an mmap or io_uring redesign.

**Warm path (~60 ms, 12 workers, ~220 samples — percentages are
±2 pp at this sample count):**

- `ignore::walk::Worker::run` is ~74% inclusive but our visitor
  (`process_one`) is only ~29% inclusive — i.e. **~45% of warm
  CPU is `ignore`-internal machinery** (DirEntry materialization,
  work-stealing dispatch). With `standard_filters(false)` already
  set, that machinery is the floor for `ignore` — no further
  tunables.
- Self-time: ~19% `stat` (per-`.meta` cache check) + ~14% `__open` +
  ~10% `__getdirentries64` + ~17% `__semwait_signal` ≈ 60% syscalls
  + thread wait. ~9% allocator churn (libsystem_malloc), ~3% path
  manipulation.

### What was tried and didn't help

| Attempt | Outcome |
|---------|---------|
| Lazy panic-label allocation in the worker closure (don't format `Path::display()` per file unless erroring) | Perf-neutral — within hyperfine σ. Not worth the code churn. |
| Skip `String::replace('\\','/')` in `rel_hint` on Unix | Perf-neutral. Dropped. |
| Hand-rolled `std::fs::read_dir` + `std::thread::scope` walker | **+11 ms warm**. Single `Mutex<Vec<PathBuf>>` work stack can't match `ignore`'s crossbeam-deque work-stealing scheduler under 12 workers. Reverted. |
| Tune `ignore::WalkBuilder` further | No knobs left — `standard_filters(false)`, `follow_links(false)`, `filter_entry` for hidden are already set. The remaining cost is per-entry `DirEntry` construction + dispatch, which isn't configurable. |

### Where future wins could come from

The structural floor only breaks with a redesign. The lone candidate
is a hand-rolled lock-free work-stealing walker — matching `ignore`'s
scheduler quality but trimmed to our needs (no `DirEntry`
materialization for non-`.meta` files). ~200+ LOC of concurrency
code chasing 5–10 ms of warm; doesn't pencil out for a tool that
already finishes in 60 ms.

### Considered and rejected

- **Dir-mtime as a cache shortcut** — the obvious idea of skipping
  per-file `stat` when a directory's mtime is stable doesn't work:
  Unix dir mtime only changes when entries are added/removed/renamed,
  not when a contained file's contents (or mtime) are edited. Unity
  touches the `.meta` mtime on reimport without touching the parent
  dir, so dir-mtime stability is consistent with a stale cache.
  The per-file mtime check is the only correct signal.

### Rust-CPU symbols to recognize

These are the parser/build hotspots that surface once syscall I/O
recedes (warm-OS cold runs, or warm-bake parse-miss runs). On a
cold-cold trace they're buried under `read`/`open` and hard to see.

- `meta::parse` — line-oriented scan of every `.meta`. `str::lines` +
  `trim` overhead per file; constant per-file, only dominant when the
  project is sprite-heavy.
- `asset::parse` — line-oriented YAML peek for the WithSubAssets
  extensions (`.prefab`/`.controller`/`.anim`/`.mixer`/`.playable`/
  `.asset`/`.spriteatlas*`). Hot for prefab-heavy projects.
- `bincode::decode_from_slice` on the cache path — only fires when a
  cache file is present. Inert on the cold-cold path.

`build_db`'s hashmap churn appears as `ahash::AHasher` callers but
it's ~17 ms total — thin in the flamegraph.

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
