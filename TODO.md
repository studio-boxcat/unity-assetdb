# TODO

_Shipped work lives in git history; this file tracks only deferred items._

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
