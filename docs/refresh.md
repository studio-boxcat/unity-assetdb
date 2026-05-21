# Refresh (Watchman-driven incremental bake)

> **Related:** [[asset-database.md]] (bake pipeline + storage schema) ·
> [[profiling.md]] (wall-clock baselines).

The bake's old mtime cache (`asset-db.cache.bin`) is gone. In its place,
an opaque [Watchman](https://facebook.github.io/watchman/) clock token
lives in the `asset-db.bin` header. Every query subcommand auto-
refreshes through `refresh::refresh` before serving its answer:

```
unity-assetdb find Foo
  │
  ├─ resolve_project_root(cwd)              ← already canonicalizes
  ├─ store::read(asset-db.bin)
  │   ├─ Err(any)  → full bake (covers NotFound, SchemaMismatch, corruption)
  │   └─ Ok(db)    → header carries `watchman_clock: Option<String>`
  │
  ├─ refresh::refresh(&mut db, project_root, out_dir, on_warn)
  │   ├─ db.watchman_clock == None         → full bake (first-run / post-v6)
  │   ├─ watch::since(prev_clock)
  │   │   ├─ Err(Unavailable)              → stderr nudge + full bake
  │   │   ├─ Err(Query(e))                 → stderr log + full bake
  │   │   ├─ Ok(Fresh)                     → full bake (journal lost / new watch)
  │   │   └─ Ok(Touched { hints, clock })
  │   │       ├─ hints.empty()             → keep clock in memory, **no bin write**
  │   │       ├─ hints.len() > 5000        → full bake (threshold beats sequential patch)
  │   │       └─ otherwise                 → patch in place → write bin
  │
  └─ run query on `db` → answer
```

Full bake is the universal fallback. Every soft failure collapses to it.

## Background / prior art

| Pattern | Why it lost |
|---|---|
| mtime-cache (the previous design) | Documented `cache_does_not_detect_asset_only_touch` blind spot. Hand edits that bumped only the asset (not its `.meta`) served stale rows. |
| `gix-status` (per-query git diff) | Lower precision — only sees git-tracked changes. ~50 ms per call, similar to Watchman, but no advantage to outweigh the install-free positioning since meow-tower already runs Watchman. |
| `notify` crate (in-process watcher) | No cross-invocation persistence. Would need a unity-assetdb daemon, which the codebase didn't need yet. |
| Content hashing every file | Correct but ~hundreds-of-ms-per-bake floor; not worth it for a 18 K-asset project. |

Watchman won on two axes: precise change detection (kernel-level
inotify / FSEvents) and a stable opaque clock token that survives our
process restarts. Reference Rust client: the official Meta-maintained
[`watchman_client`](https://docs.rs/watchman_client) crate (BSER over
Unix socket; auto-spawns the daemon).

## Use cases (ranked by frequency)

1. **`find <pattern>`** — most common; every refresh path runs this.
2. **`guid <path>` / `path <guid>`** — pre-pull resolution.
3. **`usage <guid|path>`** — impact analysis.
4. **`list [--type]`** — type-filtered inventory.
5. **`alias <name>`** — exact-name dedup.
6. **`register <path>`** — adds an entry; bumps the bin but does not
   touch the clock (the next query's refresh picks the new row up as
   part of the steady-state Watchman delta).
7. **`bake [--force]`** — manual / scripted full-walk override.

Every interactive path (1–5) goes through `refresh::refresh` before
returning. `register` does not — it owns its own flock and writes the
bin directly. `bake` is the cold-path canonical.

## Module layout

```
src/
├─ watch.rs        (~210 LOC + 2 unit tests)
│   pub fn since(project_root, prev_clock) -> Result<Delta, WatchError>
│   sync facade; wraps a current-thread tokio runtime per-call
├─ refresh.rs      (~250 LOC + 7 unit tests)
│   pub fn refresh(&mut db, project_root, out_dir, on_warn) -> Result<RefreshOutcome, RefreshError>
│   pub const PATCH_THRESHOLD: usize = 5_000
│   pub(crate) fn patch(&mut db, project_root, &[hint])
├─ bake.rs
│   bake() unchanged shape; now seeds `db.watchman_clock` via watch::since(None)
│   pub(crate) build_db_from_raw / raw_from_entry / parse_one_raw  ← reused by refresh
├─ store.rs        (schema v7)
│   AssetDb { schema_version, watchman_clock: Option<String>, script_types, entries }
│   Removed: BakeCache / CachedEntry / CACHE_FILENAME / cache_path / {read,write,encode,decode}_cache
└─ bin/unity-assetdb.rs
    open_db_or_refresh — any StoreError → full bake; else refresh
```

## Types

```rust
// watch.rs
pub enum Delta {
    Fresh   { new_clock: String },
    Touched { hints: Vec<String>, new_clock: String },
}

pub enum WatchError {
    Unavailable,             // daemon not installed / unreachable
    Query(anyhow::Error),    // BSER decode, query rejected, transport
}

pub fn since(project_root: &Path, prev_clock: Option<&str>)
    -> Result<Delta, WatchError>;
```

```rust
// refresh.rs
pub enum RefreshOutcome {
    ClockOnly,        // empty delta — clock advanced in memory only
    Patched(usize),   // N hints went through patch + bin written
    Rebaked,          // fell through to full bake + bin written
}

pub enum RefreshError {
    Bake(BakeError),
    Store(StoreError),
}

pub fn refresh(
    db: &mut AssetDb,
    project_root: &Path,
    out_dir: &Path,
    on_warn: Option<&dyn Fn(&str)>,
) -> Result<RefreshOutcome, RefreshError>;
```

```rust
// store.rs
pub struct AssetDb {
    pub schema_version: u16,            // 7
    pub watchman_clock: Option<String>, // opaque, per Watchman protocol
    pub script_types: Vec<u128>,
    pub entries: Vec<AssetEntry>,
}
```

## Patch semantics

`refresh::patch(db, project_root, hints)` (private):

1. Normalize hints to canonical (companion) form. Watchman may report a
   user edit as both `Foo.prefab` and `Foo.prefab.meta`; we want one
   re-parse per asset. Dedupe via sort + dedup over `Vec<String>`.
2. For each canonical hint:
   - If `<hint>.meta` doesn't exist on disk → queue deletion.
   - Else `bake::parse_one_raw(project_root, meta_path)`:
     - `Ok(None)` (no companion) → queue deletion.
     - `Ok(Some(raw))` → queue add/update.
3. Compute `to_drop_guids`: all GUIDs that either match a touched hint
   or appear in the new-parse set (covers rename within one delta —
   Watchman reports `(deleted old_hint, changed new_hint)` and the
   same GUID lives in the new `.meta`).
4. Convert surviving `db.entries` (skipping `to_drop_guids`) back to
   `RawEntry`s via `bake::raw_from_entry`; append the new parses.
5. Rebuild via `bake::build_db_from_raw` — same dedup + script-intern
   pipeline the full bake uses. Asset-type changes (cross-extension
   rename) and name-collision shifts settle naturally.

The patch path **always rewrites the bin**. The empty-delta path
(`RefreshOutcome::ClockOnly`) explicitly *does not* rewrite — the
2-3 MB encode + atomic rename dominates a no-op query (saves ~5 ms).
The new clock stays in-memory only; if Watchman's journal rolls past
our stored clock before the next non-empty delta, the next `since`
returns `Fresh` and we full-bake. Acceptable.

## NoWatchman + error nudges

Refresh emits exactly one line to `on_warn` (which the CLI routes to
stderr) per fallback:

```
watchman unavailable; install (brew install watchman) for incremental updates
```

```
watchman query failed: <err>; falling back to full bake
```

No sentinel file, no env var, no rate-limiting. Stateless and easy to
test. The full bake fires regardless.

## Threshold

`PATCH_THRESHOLD = 5_000` (const in `refresh.rs`). Above this, full-
bake wins because sequential `parse_one_raw` at ~100 µs / file plus
the dedup-pass cost approaches the parallel walk's ~1 s budget.
Tunable per-bench. The const is `pub` so a future regression test
can pin it.

## Path filter at query time

`watch::since` issues:

```rust
QueryRequestCommon {
    since: prev_clock.map(|c| Clock::Spec(ClockSpec::StringClock(c))),
    expression: Some(Expr::All(vec![
        Expr::Any(vec![
            Expr::DirName(DirNameTerm { path: "Assets".into(),         depth: None }),
            Expr::DirName(DirNameTerm { path: "Packages".into(),       depth: None }),
            Expr::DirName(DirNameTerm { path: "ProjectSettings".into(), depth: None }),
        ]),
        Expr::Suffix(/* meta, prefab, asset, anim, controller, mat, mask,
                        mixer, playable, spriteatlas, spriteatlasv2, unity,
                        fbx, png, jpg, … */),
    ])),
    ..Default::default()
}
```

`Library/`, `Temp/`, `obj/`, `Logs/` never surface — the filter
applies even though Watchman roots at the highest `.git` ancestor.
**No `.watchmanconfig` required on the user side.** (Users free to
drop one for FSEvents-level ignore acceleration, but it's not load-
bearing.)

## Async boundary

`watchman_client` is tokio-based; the rest of the crate is sync.
`watch::since` is a sync facade — builds a single-threaded tokio
runtime at function entry, `block_on`s `since_inner`, drops the
runtime. Per-call overhead: sub-millisecond. Tokio dep is
`default-features = false, features = ["rt", "net"]`.

## Lifecycle

| Event                           | bin       | clock     | Daemon    | Action |
|---------------------------------|-----------|-----------|-----------|--------|
| First run, fresh tree           | absent    | n/a       | any       | full bake → save (with clock if Watchman live) |
| Steady state, no edits          | present   | valid     | live      | `since` empty Touched → no write, ClockOnly |
| Edit one `.meta`                | present   | valid     | live      | Touched(1) → patch → write |
| `git checkout` ≤ 5000 files     | present   | valid     | live      | Touched(N) → patch → write |
| `git checkout` > 5000 files     | present   | valid     | live      | Touched > threshold → full bake |
| Reboot / Watchman restart       | present   | syntactic | restarted | Fresh → full bake |
| Watchman uninstalled            | present   | valid     | absent    | Unavailable → full bake + nudge |
| Schema bump (v6 bin)            | present (v6) | n/a    | any       | SchemaMismatch → full bake (no migration) |
| Bin corruption / partial write  | corrupt   | n/a       | any       | any StoreError → full bake |
| Manual force                    | n/a       | n/a       | any       | `rm <out_dir>/asset-db.bin && unity-assetdb bake` |
| `wt` slot reused                | possibly stale | possibly stale | live | Fresh or Touched on first refresh → settles |
| Project moved on disk           | present   | from another watch | live | Fresh (Watchman clocks include root identity) → full bake |
| Manual bin edit                 | corrupt   | n/a       | any       | SchemaMismatch / decode err → full bake |

## Pitfalls

- **Watchman roots at the highest `.git`/`.hg`/`.watchmanconfig`
  ancestor**, not your requested path. `ResolvedRoot` carries the
  offset; `Client::query` wires it into `relative_root` for you. Don't
  hand-compose paths.
- **Clock strings are opaque.** Per the [Watchman clockspec
  docs](https://facebook.github.io/watchman/docs/clockspec):
  > "the fundamental clock specifier string's contents should be
  > considered to be opaque to the client as the server occasionally
  > evolves the meaning of the clockspec and its format is expressly
  > not a stable API."
  Persist as `String`. Never parse.
- **`fresh_instance: true` is not an error.** Daemon restart, journal
  overflow, brand-new watch — all legitimate. Treat as full-bake
  signal; do not panic or log loudly.
- **Async-vs-sync.** Don't import `watchman_client`'s async types into
  sync code paths. The boundary is `watch::since`'s `block_on`. Same
  rule if you ever add a second Watchman touchpoint — wrap it the same way.
- **Settle window costs ~20 ms.** Watchman waits ~20 ms after the last
  event before answering `since` queries to avoid mid-burst snapshots.
  This is a fixed floor on the steady-state warm path. Tunable via
  `SyncTimeout::DisableCookie` but **leave it alone** — turning off the
  sync cookie produces correctness footguns for marginal speedup.
- **Transient parse errors during patch.** If Unity is mid-rewriting a
  `.meta` when `refresh::patch` calls `parse_one_raw`, the YAML may be
  truncated and parse fails. The error currently propagates as
  `RefreshError::Bake` and surfaces to the query subcommand. Rare in
  practice (Unity writes are atomic on most platforms); if it becomes
  a pain point, catch per-hint and full-bake on first failure.
- **Cross-platform path separators.** Watchman returns forward slashes
  on all platforms. Our `hint` field also uses forward slashes (via
  `rel_hint` in `bake.rs`). Don't `Path::join` non-Windows-normalized
  hints with a `PathBuf` on Windows — the resulting mix breaks
  `parse_one_raw`. (Not a real concern today; macOS-only project, but
  documented for future portability.)

## Non-goals

- No background daemon of our own.
- No `notify` / `gix` / mtime fallback for the no-Watchman case. Just
  full-bake.
- No SCM-aware Watchman queries (mergebase clocks, fat clocks). Over-
  engineered at our scale.
- No content hashing — Watchman is the only source of truth.
- No persistent state outside the bin header. No sidecar `.clock` file.
- No v6 decoder for migration debugging. Pre-1.0; `rm asset-db.bin`
  is the migration tool.
- No legacy `asset-db.cache.bin` cleanup. `Library/` is Unity's regen
  dir; users wipe it when they want.

## References

- [`watchman_client` crate](https://docs.rs/watchman_client) — Meta's
  official Rust client.
- [Watchman clockspec](https://facebook.github.io/watchman/docs/clockspec).
- [Watchman since command](https://facebook.github.io/watchman/docs/cmd/since).
- [`git-fsmonitor-watchman-rs`](https://crates.io/crates/git-fsmonitor-watchman-rs)
  — canonical small-CLI Rust shape this module borrows from.
- Phase 1 audit + Phase 2 reviews captured in conversation
  transcripts; the overhaul shipped 2026-05-21.
