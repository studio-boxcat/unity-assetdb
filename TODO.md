# TODO

## Shipped

- **~~Watchman-driven incremental bake (replaces `BakeCache`).~~** ✅
  Shipped 2026-05-21. `BakeCache` / `cache.bin` / mtime-tracking stack
  removed; opaque Watchman clock token now lives in the `AssetDb`
  header. Auto-refresh wired into every query subcommand via
  `refresh::refresh`. Meow-tower steady-state ≈ 120 ms / patched
  ≈ 150 ms / cold full bake ≈ 1 s. See [[docs/refresh.md]] for the
  design; numbers + microbench in [[docs/profiling.md]].
- **~~CLI integration tests.~~** ✅ `tests/cli.rs` covers the stdout /
  stderr / exit-code contract for every subcommand (`bake`, `find`,
  `alias`, `guid`, `path`, `list`, `usage`) using `assert_cmd`. 17
  tests pin TSV + `--json` output shape, "did you mean:" suggestions
  on miss, and the auto-bake-on-missing-bin path.
- **~~Clippy `doc_lazy_continuation`.~~** ✅ Fixed in `src/store.rs` +
  `tests/bake.rs`. `cargo clippy --all-targets -- -D warnings` is
  clean.
- **~~`unity-path-rules` I/O predicate tests.~~** ✅ Tempdir-based
  tests for `is_submodule_root` (`.git` directory vs gitlink-file vs
  absent) + `is_opaque_subtree` (four-corner OR coverage). No new
  dev-deps; uses the same `unique_tmp` pattern as the rest of the
  workspace.
- **~~`unity-path-rules` publish dep gap.~~** ✅ Resolved via
  `justfile`'s new `publish` + `publish-dry-run` recipes. The
  ergonomic answer keeps the `version = "0.1"` field (correct for
  crates.io publish) and adds an ordered two-step recipe that
  publishes the sub-crate first, then the root. Manual prereq:
  `cargo login`. Run `just publish-dry-run` for safe validation,
  `just publish` to release.

## Deferred (intentional — wait for a use case)

- **Daemon mode.** A long-lived `unity-assetdb` daemon would collapse
  the steady-state cost from ~120 ms to <10 ms by avoiding bin reload
  + Watchman handshake per invocation. ~hundreds of LOC of IPC +
  lifecycle. Only justified if a real sub-10 ms use case materializes
  (IDE plugin, tight build-loop).
- **Localized dedup in `refresh::patch`.** Patch currently re-runs
  `build_db_from_raw` over the whole entry set on every touched-hint
  batch (~10 ms on 18 K entries). Localized dedup keyed on the
  affected `(name, asset_type)` buckets would knock that to sub-ms
  for single-file edits. ~150 LOC. Wait for a measured pain point —
  current patch latency is dominated by bin rewrite, not dedup.
- **Watchman-gated end-to-end `tests/refresh.rs`.** Today's coverage
  exercises `refresh::patch` unit-style (bypassing Watchman) plus a
  single `#[ignore]`'d `src/watch.rs::tests::since_returns_fresh_then_touched`.
  A full refresh → patch → bin round-trip integration test against a
  live daemon would close the loop, but the current split is honest
  about what's gated and what isn't.
