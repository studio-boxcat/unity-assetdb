# TODO

- **CLI integration tests.** No `tests/` coverage for the binary's stdout / stderr / exit-code contracts — only library APIs are tested. A small `assert_cmd`-based suite covering `guid` / `path` / `find` / `alias` / `usage` miss paths (exit 1, `did you mean:` on stderr) and hit paths (TSV / `--json` shapes) would lock the UX. Noted while normalising `find`'s miss UX (CLAUDE.md item 11).
- **Clippy: `doc_lazy_continuation` at `src/store.rs:41`.** Pre-existing — `cargo clippy --all-targets -- -D warnings` fails on it. One-line indent fix (see clippy suggestion). Surfaced while running clippy across the workspace after the `unity-path-rules` extract.
- **`unity-path-rules` publish dep gap.** Root `Cargo.toml` declares `unity-path-rules = { version = "0.1", path = "crates/unity-path-rules" }`. `cargo publish` for `unity-assetdb` will fail until `unity-path-rules` itself is published to crates.io. Either publish the sub-crate first, or strip the `version` field when going to crates.io.
- **`unity-path-rules` tests for I/O predicates.** `is_submodule_root` and `is_opaque_subtree` have no tests (they touch the filesystem). A small tempdir-based test for each would lock semantics — currently only the pure predicates (`is_unity_hidden`, `is_opaque_plugin_dir`) have coverage.
- **~~Watchman-driven incremental bake (replaces `BakeCache`).~~** ✅
  Shipped 2026-05-21. `BakeCache` / `cache.bin` / mtime-tracking stack
  removed; opaque Watchman clock token now lives in the `AssetDb`
  header. Auto-refresh wired into every query subcommand via
  `refresh::refresh`. Meow-tower steady-state ≈ 120 ms / patched
  ≈ 150 ms / cold full bake ≈ 1 s. See [[refresh.md]] for the design;
  numbers + microbench in [[profiling.md]].
- **Daemon mode (deferred).** A long-lived `unity-assetdb` daemon would
  collapse the steady-state cost from ~120 ms to <10 ms by avoiding
  bin reload + Watchman handshake per invocation. ~hundreds of LOC of
  IPC + lifecycle. Only justified if a real sub-10 ms use case
  materializes (IDE plugin, tight build-loop). No-go for now.
- **Localized dedup in `refresh::patch` (deferred).** Patch currently
  re-runs `build_db_from_raw` over the whole entry set on every
  touched-hint batch (~10 ms on 18 K entries). Localized dedup keyed
  on the affected `(name, asset_type)` buckets would knock that to
  sub-ms for single-file edits. ~150 LOC. Wait for a measured pain
  point — current patch latency is dominated by bin rewrite, not
  dedup.
