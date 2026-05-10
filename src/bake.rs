//! Bake orchestrator: walk → parse → cache → write.
//!
//! Per-file flow:
//! 1. Stat `.meta` and the companion asset file. If both mtimes match the
//!    cached values → reuse cached entry, skip parse.
//! 2. Else read `.meta` → guid + sprite-sheet sub-assets.
//! 3. Read the asset file → top-level class ID + sub-asset rows.
//! 4. Resolve `AssetType`: native `class_id` or `Script(script_guid)`.
//! 5. Derive alias from the filename stem.
//!
//! Post-walk: alias-collision sweep (filename stems can clash; we suffix
//! with parent dir on conflict and warn).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use ahash::{AHashMap, AHashSet};

use anyhow::{Context, Result};

use crate::asset;
use crate::class_id::{ClassId, class_from_ext};
use crate::meta::{self, SPRITE_MODE_SINGLE, TEXTURE_TYPE_SPRITE};
use crate::store::{
    self, AssetDb, AssetEntry, AssetType, BakeCache, CachedAssetType, CachedEntry, SubAsset,
    CACHE_FILENAME, DB_FILENAME,
};
use crate::walk::walk_meta_files;

/// Caller-supplied name sanitizer. Returns `Some(rewritten)` when the
/// input contains characters the consumer wants to scrub from asset
/// names; `None` to keep the input as-is. Bake calls this once per
/// top-level filename stem and once per sub-asset YAML `m_Name`.
///
/// Bound is `Send + Sync + 'static` because [`BakeOptions`] flows into
/// `ignore::WalkParallel` worker closures.
///
/// Default behavior (no sanitizer) leaves all names verbatim.
pub type NameSanitizer = Box<dyn Fn(&str) -> Option<String> + Send + Sync + 'static>;

/// Caller-supplied warning sink. Bake invokes this for non-fatal events
/// (worker errors during the parallel walk, name-collision rewrites,
/// sanitizer rewrites). The library never writes to stderr itself.
pub type WarnSink = Box<dyn Fn(&str) + Send + Sync + 'static>;

/// Caller-supplied progress sink. Bake invokes this with the post-bake
/// summary line and (when `BakeOptions::verbose_timing` is true) with
/// per-phase timing. Separate from [`WarnSink`] so consumers can route
/// "info" output and warnings to different places.
pub type ProgressSink = Box<dyn Fn(&str) + Send + Sync + 'static>;

/// Convert `SystemTime` → ns-since-UNIX. Saturates to 0 on pre-epoch
/// (which would only happen if the user's clock is bogus).
fn mtime_ns(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// One raw bake result, before name dedup. `script_guid` is the unmapped
/// GUID for MonoBehaviour assets — interning happens after the walk so we
/// only need one final sort.
#[derive(Clone)]
struct RawEntry {
    guid: u128,
    asset_type_raw: AssetTypeRaw,
    hint: String,
    name: String,
    meta_mtime_ns: u64,
    asset_mtime_ns: u64,
    sub_assets: Vec<SubAsset>,
}

#[derive(Clone, Copy)]
enum AssetTypeRaw {
    Native(u32),
    Script(u128),
}

/// Per-worker-thread accumulator. Sends its collected `entries` + `errors`
/// to the main thread via Drop — `ignore::WalkBuilder::run` drops each
/// thread's visitor closure (and thus its captured `ThreadLocal`) on
/// thread exit, so the main thread sees all batches once `walker.run`
/// returns.
struct ThreadLocal {
    entries: Vec<RawEntry>,
    errors: Vec<String>,
    raw_tx: mpsc::Sender<Vec<RawEntry>>,
    err_tx: mpsc::Sender<Vec<String>>,
}

impl Drop for ThreadLocal {
    fn drop(&mut self) {
        let entries = std::mem::take(&mut self.entries);
        let errors = std::mem::take(&mut self.errors);
        // Channel-closed errors are unreachable here — main thread holds
        // the receivers until after `walker.run` returns.
        let _ = self.raw_tx.send(entries);
        let _ = self.err_tx.send(errors);
    }
}

/// Cache key: hint (Assets-relative, forward-slashed). ahash beats siphash
/// by ~2x for our small-string keys.
type CacheMap = AHashMap<String, RawEntry>;

/// Run a `Result<Option<T>>`-producing closure under `catch_unwind` and
/// flatten the four-way outcome (success-with-value / success-skip /
/// inner-err / panic) into `Result<Option<T>, String>`. The closure
/// is wrapped in `AssertUnwindSafe` because parallel-walk visitors
/// capture Arc state by ref, and the bake worker treats process_one
/// as panic-safe on its inputs.
///
/// `label` prefixes both inner errors and panic reports with the
/// asset path; `task_name` names the operation in the panic line
/// (e.g. `"process_one"`) so the message reads
/// `"<path>: panic in <task_name>: <payload>"`.
///
/// Pulled out of the inline closure inside `bake_action`'s parallel
/// walk so panic-payload extraction (string / String / non-string)
/// can be unit-tested without spinning up a project tree.
fn run_with_panic_safety<T, F>(label: &str, task_name: &str, f: F) -> Result<Option<T>, String>
where
    F: FnOnce() -> Result<Option<T>>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(opt)) => Ok(opt),
        Ok(Err(e)) => Err(format!("{label}: {e}")),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            Err(format!("{label}: panic in {task_name}: {msg}"))
        }
    }
}

/// Build the in-memory cache from a previously-saved `BakeCache`. Each
/// `CachedEntry` becomes a `RawEntry` keyed by its hint. Cache hits during
/// the walk drop straight into the post-walk pipeline.
///
/// `String::from(Box<str>)` is O(1) — Rust hands the heap allocation
/// directly from the box to the new String, no copy. The map key is then
/// cloned once for the parallel field on `RawEntry` (one alloc per entry).
fn build_cache(cache: BakeCache) -> CacheMap {
    let mut out = AHashMap::with_capacity(cache.entries.len());
    for e in cache.entries {
        let asset_type_raw = match e.asset_type {
            CachedAssetType::Native(n) => AssetTypeRaw::Native(n),
            CachedAssetType::Script(g) => AssetTypeRaw::Script(g),
        };
        let hint = String::from(e.hint);
        let raw = RawEntry {
            guid: e.guid,
            asset_type_raw,
            hint: hint.clone(),
            name: String::new(), // re-derived in build_db
            meta_mtime_ns: e.meta_mtime_ns,
            asset_mtime_ns: e.asset_mtime_ns,
            sub_assets: e.sub_assets,
        };
        out.insert(hint, raw);
    }
    out
}

/// Build the on-disk cache from the post-walk raw entries. Sorted by hint
/// so the file is byte-stable across re-bakes when nothing changed.
fn build_bake_cache(raw: &[RawEntry]) -> BakeCache {
    let mut entries: Vec<CachedEntry> = raw
        .iter()
        .map(|r| CachedEntry {
            hint: r.hint.clone().into_boxed_str(),
            meta_mtime_ns: r.meta_mtime_ns,
            asset_mtime_ns: r.asset_mtime_ns,
            guid: r.guid,
            asset_type: match r.asset_type_raw {
                AssetTypeRaw::Native(n) => CachedAssetType::Native(n),
                AssetTypeRaw::Script(g) => CachedAssetType::Script(g),
            },
            sub_assets: r.sub_assets.clone(),
        })
        .collect();
    entries.sort_by(|a, b| a.hint.cmp(&b.hint));
    BakeCache {
        schema_version: store::SCHEMA_VERSION,
        entries,
    }
}

/// Caller-supplied bake configuration.
///
/// Built by the consumer's CLI / library entry point and handed to
/// [`bake`]. The library never reads env vars, never resolves the
/// project root for you, and never writes to stderr — every side
/// channel routes through one of the optional sinks below.
pub struct BakeOptions {
    /// Project root containing `Assets/` + `ProjectSettings/`. Caller
    /// resolves this (typically via [`crate::walk::resolve_project_root`])
    /// before constructing options.
    pub project_root: PathBuf,
    /// Directory where `asset-db.bin` and `asset-db.cache.bin` are written.
    /// Caller composes the convention (e.g. `<project>/Library/unity-assetdb/`
    /// or a fixture-staging path).
    pub out_dir: PathBuf,
    /// Optional name sanitizer; see [`NameSanitizer`].
    pub name_sanitizer: Option<NameSanitizer>,
    /// Optional warning sink; see [`WarnSink`]. `None` discards warnings.
    pub on_warn: Option<WarnSink>,
    /// Optional progress sink; see [`ProgressSink`]. `None` discards the
    /// summary line.
    pub on_progress: Option<ProgressSink>,
    /// When true, [`on_progress`] also receives a per-phase timing line
    /// (cache / walk / build / write). Env-var-driven behavior is the
    /// consumer's call.
    pub verbose_timing: bool,
    /// When true, [`on_warn`] receives a line for each name-collision
    /// rewrite during dedup. Off by default to keep steady-state warm
    /// bakes quiet.
    pub verbose_collisions: bool,
}

impl BakeOptions {
    /// Minimal options: project root + out dir. All sinks `None`,
    /// no sanitizer, verbose flags off.
    pub fn new(project_root: PathBuf, out_dir: PathBuf) -> Self {
        Self {
            project_root,
            out_dir,
            name_sanitizer: None,
            on_warn: None,
            on_progress: None,
            verbose_timing: false,
            verbose_collisions: false,
        }
    }
}

/// Bake entry-point. Walks `Assets/`, parses `.meta` + asset YAML,
/// writes `<out_dir>/asset-db.bin` and `<out_dir>/asset-db.cache.bin`.
pub fn bake(opts: &BakeOptions) -> Result<()> {
    let project_root = &opts.project_root;
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("create out-dir: {}", opts.out_dir.display()))?;
    let db_file = opts.out_dir.join(DB_FILENAME);
    let cache_file = opts.out_dir.join(CACHE_FILENAME);
    let t_start = Instant::now();

    // Load bake-only cache. Missing/corrupt → empty (first bake or stale).
    let cache: CacheMap = match store::read_cache(&cache_file) {
        Ok(c) => build_cache(c),
        Err(_) => AHashMap::new(),
    };
    let cache_size = cache.len();
    let t_cache = t_start.elapsed();

    // Per-thread accumulators: each worker drops its `Vec<RawEntry>` and
    // `Vec<String>` (errors) into channels at thread exit via `Drop`. Avoids
    // the Mutex<Vec> contention 16k pushes on 8 cores produced — measured
    // ~3-4 ms warm savings on meow-tower.
    //
    // `ignore::WalkParallel::run` requires `'static + Send` visitors, so
    // shared state goes through `Arc`. Each worker clones the Arc once at
    // factory time — the clone cost is negligible vs the per-entry work.
    let (raw_tx, raw_rx) = mpsc::channel::<Vec<RawEntry>>();
    let (err_tx, err_rx) = mpsc::channel::<Vec<String>>();
    let cache_arc = Arc::new(cache);
    let cache_hits = Arc::new(AtomicUsize::new(0));
    let walked = Arc::new(AtomicUsize::new(0));
    let project_root_arc: Arc<PathBuf> = Arc::new(project_root.clone());

    walk_meta_files(project_root, || {
        let raw_tx = raw_tx.clone();
        let err_tx = err_tx.clone();
        let cache = Arc::clone(&cache_arc);
        let cache_hits = Arc::clone(&cache_hits);
        let walked = Arc::clone(&walked);
        let project_root = Arc::clone(&project_root_arc);
        let mut local = ThreadLocal {
            entries: Vec::with_capacity(2048),
            errors: Vec::new(),
            raw_tx,
            err_tx,
        };
        move |meta_path: &Path| {
            walked.fetch_add(1, Ordering::Relaxed);
            // Catch panics so a single malformed .meta or unforeseen
            // bug doesn't silently terminate the worker thread (which
            // would lose its ThreadLocal accumulator). `ignore::WalkParallel`
            // doesn't propagate visitor panics; without this, a panic in
            // `process_one` produces a partial DB with no surfaced error.
            // Helper does the catch_unwind + payload-downcast — see
            // `run_with_panic_safety`.
            let label = meta_path.display().to_string();
            match run_with_panic_safety(&label, "process_one", || {
                process_one(meta_path, &project_root, &cache, &cache_hits)
            }) {
                Ok(Some(r)) => local.entries.push(r),
                Ok(None) => {}
                Err(msg) => local.errors.push(msg),
            }
        }
    })?;
    drop(raw_tx);
    drop(err_tx);
    let t_walk = t_start.elapsed();

    let mut errors: Vec<String> = Vec::new();
    for v in err_rx.iter() {
        errors.extend(v);
    }
    if let Some(sink) = opts.on_warn.as_ref() {
        for e in &errors {
            sink(&format!("warning: {e}"));
        }
    }

    let mut raw: Vec<RawEntry> = Vec::with_capacity(cache_size + 256);
    for v in raw_rx.iter() {
        raw.extend(v);
    }
    // Build cache from `raw` (consumes nothing) before `build_db` consumes
    // it. Sequence the writes so the cache is only persisted after the
    // convert artifact lands — a half-baked cache without a matching db
    // would let a later run skip parsing for entries that aren't in the
    // db yet.
    let bake_cache = build_bake_cache(&raw);
    let db = build_db(
        raw,
        opts.name_sanitizer.as_deref(),
        opts.on_warn.as_deref(),
        opts.verbose_collisions,
    )?;
    let t_build = t_start.elapsed();

    // No-op skip: every entry came from cache AND nothing was dropped from
    // cache (count stable). Skips ~2-3 ms of bincode encode + file write
    // on the steady-state warm path. Still skips only when both files are
    // present — first run or after a manual delete writes anyway.
    let hit_n = cache_hits.load(Ordering::Relaxed);
    let no_op =
        hit_n == cache_size && hit_n == db.entries.len() && db_file.exists() && cache_file.exists();

    if !no_op {
        store::write(&db_file, &db)
            .with_context(|| format!("write asset-db: {}", db_file.display()))?;
        store::write_cache(&cache_file, &bake_cache)
            .with_context(|| format!("write cache: {}", cache_file.display()))?;
    }
    let t_write = t_start.elapsed();

    if let Some(sink) = opts.on_progress.as_ref() {
        sink(&format!(
            "baked {} entries → {}",
            db.entries.len(),
            db_file.display()
        ));
        if opts.verbose_timing {
            let walked_n = walked.load(Ordering::Relaxed);
            let parsed_n = db.entries.len() - hit_n;
            let write_phase = if no_op { "skipped" } else { "wrote" };
            sink(&format!(
                "  walked={walked_n} hit={hit_n} parsed={parsed_n} | cache={:?} walk={:?} build={:?} write={:?} ({write_phase}) total={:?}",
                t_cache,
                t_walk - t_cache,
                t_build - t_walk,
                t_write - t_build,
                t_write,
            ));
        }
    }
    Ok(())
}

/// Per-`.meta` work. Returns `Ok(None)` when the meta has no companion file
/// to describe (e.g. orphaned `.meta`, directory `.meta`).
fn process_one(
    meta_path: &Path,
    project_root: &Path,
    cache: &CacheMap,
    cache_hits: &AtomicUsize,
) -> Result<Option<RawEntry>> {
    let companion =
        strip_meta_suffix(meta_path).ok_or_else(|| anyhow::anyhow!("not a .meta path"))?;

    // Skip directory `.meta` files — directories don't get asset-db rows.
    let Ok(companion_md) = std::fs::metadata(&companion) else {
        return Ok(None);
    };
    if companion_md.is_dir() {
        return Ok(None);
    }

    let meta_md =
        std::fs::metadata(meta_path).with_context(|| format!("stat: {}", meta_path.display()))?;

    let meta_mtime_ns = mtime_ns(meta_md.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    let asset_mtime_ns = mtime_ns(companion_md.modified().unwrap_or(SystemTime::UNIX_EPOCH));

    let hint = rel_hint(project_root, &companion)?;

    // Cache hit?
    if let Some(cached) = cache.get(&hint)
        && cached.meta_mtime_ns == meta_mtime_ns
        && cached.asset_mtime_ns == asset_mtime_ns
    {
        cache_hits.fetch_add(1, Ordering::Relaxed);
        return Ok(Some(cached.clone()));
    }

    // Cache miss → parse.
    let meta_text = std::fs::read_to_string(meta_path)
        .with_context(|| format!("read .meta: {}", meta_path.display()))?;
    let meta_info = meta::parse(&meta_text)?;

    let ext = companion.extension().and_then(|s| s.to_str()).unwrap_or("");
    let from_ext = class_from_ext(ext);

    let mut sub_assets: Vec<SubAsset> = Vec::new();
    let mut top_class_id: Option<u32> = None;
    let mut script_guid: Option<u128> = None;

    // YAML peek strategy:
    //  - TopOnly: types whose extra docs are internal scene-graph (.prefab
    //    GameObjects, .controller AnimatorStates, …) — read just enough to
    //    capture top class ID + m_Script.guid, then bail.
    //  - WithSubAssets: types where extra docs ARE addressable from outside
    //    (.asset multi-doc, .spriteatlas packed sprites). Read fully.
    //  - None: extension already says everything (`.png`, `.fbx`, scripts).
    let parse_mode: Option<asset::ParseMode> = match ext {
        "asset" | "spriteatlas" | "spriteatlasv2" => Some(asset::ParseMode::WithSubAssets),
        // `.playable` files are MonoBehaviour-backed (TimelineAsset and friends);
        // the YAML peek captures the script guid so they land as `Script(...)`
        // entries, the same path as `.asset` ScriptableObjects.
        // `.mixer` is `AudioMixerController` (native classID 244, Editor-only).
        "prefab" | "controller" | "anim" | "mat" | "mask" | "unity" | "playable" | "mixer" => {
            Some(asset::ParseMode::TopOnly)
        }
        _ => None,
    };

    if let Some(mode) = parse_mode {
        let asset_text = read_asset_for_mode(&companion, mode)?;
        let info = asset::parse(&asset_text, mode)?;
        top_class_id = info.top_class_id;
        script_guid = info.script_guid;
        for s in info.sub_assets {
            if !s.name.is_empty() {
                sub_assets.push(SubAsset {
                    file_id: s.file_id,
                    name: s.name.into_boxed_str(),
                });
            }
        }
    }

    // Precedence: script_guid (MonoBehaviour-backed) > from_ext > top_class_id.
    // `.prefab` and `.unity` deliberately let from_ext win — their YAML's first
    // doc is a *contained* object (GameObject = classID 1), not the asset's
    // class (Prefab = 1001). Falling back to top_class_id only for extensions
    // without a stable class mapping (e.g. `.asset`, where the YAML peek is
    // the only signal).
    let asset_type_raw = if let Some(g) = script_guid {
        AssetTypeRaw::Script(g)
    } else if let Some(cls) = from_ext {
        AssetTypeRaw::Native(cls as u32)
    } else if let Some(cls) = top_class_id.and_then(ClassId::from_raw) {
        AssetTypeRaw::Native(cls as u32)
    } else if let Some(cls) = top_class_id {
        // Unknown raw class ID — store anyway; lookup will treat as Native.
        AssetTypeRaw::Native(cls)
    } else {
        return Ok(None);
    };

    let name = filename_stem(&companion);

    // Implicit Sprite sub-asset for Single-mode textures. Compute first
    // (borrows `meta_info` whole); the for-loop below moves
    // `meta_info.sprite_sheet`, so the predicate must run before that.
    let implicit_sprite = synthesize_implicit_sprite(&meta_info, &name);

    // Texture sprite-sheet sub-assets (from .meta).
    for (fid, name) in meta_info.sprite_sheet {
        sub_assets.push(SubAsset {
            file_id: fid,
            name: name.into_boxed_str(),
        });
    }

    if let Some(sub) = implicit_sprite {
        sub_assets.push(sub);
    }

    Ok(Some(RawEntry {
        guid: meta_info.guid,
        asset_type_raw,
        hint,
        name,
        meta_mtime_ns,
        asset_mtime_ns,
        sub_assets,
    }))
}

/// Synthesize the implicit Sprite sub-asset Unity auto-generates for
/// Single-mode Sprite textures. Unity creates one Sprite (fileID
/// `21300000` = `ClassId::Sprite × 100_000`) named after the texture
/// file but never writes it to the `.meta` — the `sprites:` list stays
/// empty. Without synthesizing it here, `AssetMap::elidable_subasset_fid`
/// (`mapping/asset_map.rs`) can't fire and `_sprite: $TexName` fields
/// keep the redundant `#f21300000` suffix on pull.
///
/// Returns `None` when:
///   - the `.meta`'s `spriteSheet.sprites:` list is non-empty (explicit
///     entries own the sub-asset list — atlases, multi-sprite sheets);
///   - `textureType` isn't 8 (Sprite); or
///   - `spriteMode` isn't 1 (Single).
///
/// Branches pinned by `bake_asset_db::bake::tests::synthesize_implicit_sprite_*`.
fn synthesize_implicit_sprite(meta: &meta::MetaInfo, stem: &str) -> Option<SubAsset> {
    if meta.sprite_sheet.is_empty()
        && meta.texture_type == Some(TEXTURE_TYPE_SPRITE)
        && meta.sprite_mode == Some(SPRITE_MODE_SINGLE)
    {
        Some(SubAsset {
            file_id: ClassId::Sprite.canonical_subobject_fid(),
            name: stem.to_string().into_boxed_str(),
        })
    } else {
        None
    }
}

fn warn_sanitized(on_warn: Option<&(dyn Fn(&str) + Send + Sync)>, kind: &str, hint: &str, old: &str, new: &str) {
    if let Some(sink) = on_warn {
        sink(&format!(
            "warning: {kind} {hint} name `{old}` contains ref-reserved char; renamed to `{new}`",
        ));
    }
}

fn build_db(
    mut raw: Vec<RawEntry>,
    sanitizer: Option<&(dyn Fn(&str) -> Option<String> + Send + Sync)>,
    on_warn: Option<&(dyn Fn(&str) + Send + Sync)>,
    verbose_collisions: bool,
) -> Result<AssetDb> {
    // Stable order: sort by hint so dedup picks the same "winner" each bake.
    raw.sort_by(|a, b| a.hint.cmp(&b.hint));

    // Reset every entry's name to its raw filename stem before dedup
    // (cached entries arrive with their previously-suffixed name; if we
    // dedup against that, collisions compound across bakes), then sanitize
    // ref-reserved chars in both top-level and sub-asset names — covers the
    // three name sources (filename stem, YAML m_Name sub-assets, `.meta`
    // sprite-sheet entries) in one pass before dedup uses `r.name` as key.
    for r in raw.iter_mut() {
        r.name = filename_stem_from_hint(&r.hint);
        if let Some(san) = sanitizer
            && let Some(clean) = san(&r.name)
        {
            warn_sanitized(on_warn, "asset", &r.hint, &r.name, &clean);
            r.name = clean;
        }
        if let Some(san) = sanitizer {
            for sub in r.sub_assets.iter_mut() {
                if let Some(clean) = san(&sub.name) {
                    warn_sanitized(on_warn, "sub-asset of", &r.hint, &sub.name, &clean);
                    sub.name = clean.into_boxed_str();
                }
            }
        }
    }

    let verbose = verbose_collisions;

    // Pass 1: tally every name's distinct-guid owners across both
    // top-level and sub-asset claims. A name owned by ≥2 distinct guids
    // is "contested" — every claimant must rename, no one keeps the bare
    // alias. Single-owner names (including the case where a top-level
    // and its own sub-asset share a stem) stay bare since reverse-lookup
    // resolves uniquely.
    let mut owners: AHashMap<String, AHashSet<u128>> = AHashMap::with_capacity(raw.len());
    for r in &raw {
        owners.entry(r.name.clone()).or_default().insert(r.guid);
        for sub in &r.sub_assets {
            owners
                .entry(sub.name.to_string())
                .or_default()
                .insert(r.guid);
        }
    }
    let contested = |name: &str| owners.get(name).is_some_and(|s| s.len() > 1);

    // Pass 2: walk entries in hint-sorted order, renaming every contested
    // claim. `taken` tracks names already claimed in this pass so the
    // disambiguator never picks a candidate that collides with an earlier
    // (different-guid) entry; same-guid sharing remains allowed (a
    // contested sub-asset within a renamed parent ends up with the
    // parent's renamed alias when their hints share enough path).
    let mut taken: AHashMap<String, u128> = AHashMap::with_capacity(raw.len());
    for r in raw.iter_mut() {
        if contested(&r.name) {
            let new_name = disambiguate(&r.name, &r.hint, r.guid, &taken)?;
            if verbose && let Some(sink) = on_warn {
                sink(&format!(
                    "warning: name collision on `{}` (guid {:032x}); renamed to `{}`",
                    r.name, r.guid, new_name,
                ));
            }
            r.name = new_name;
        }
        match taken.get(&r.name) {
            Some(&prev) if prev != r.guid => anyhow::bail!(
                "asset-db: name `{}` claimed by both guid {:032x} and {prev:032x} \
                 after dedup — `disambiguate` produced a non-unique alias",
                r.name,
                r.guid,
            ),
            _ => {
                taken.insert(r.name.clone(), r.guid);
            }
        }

        for sub in r.sub_assets.iter_mut() {
            if contested(&sub.name) {
                let original = sub.name.to_string();
                let new_name = disambiguate(&original, &r.hint, r.guid, &taken)?;
                if verbose && let Some(sink) = on_warn {
                    sink(&format!(
                        "warning: sub-asset name collision on `{}` (parent guid {:032x}); renamed to `{}`",
                        original, r.guid, new_name,
                    ));
                }
                sub.name = new_name.into_boxed_str();
            }
            // Same-guid sharing is allowed — a sub-asset's deduped name
            // will often equal the parent's deduped alias (same hint
            // feeds disambiguate), and that's the desired outcome.
            if !taken.contains_key(&*sub.name) {
                taken.insert(sub.name.to_string(), r.guid);
            }
        }
    }

    // Intern script types and finalize entries.
    let mut db = AssetDb::new();
    let entries: Vec<AssetEntry> = raw
        .into_iter()
        .map(|r| {
            let asset_type = match r.asset_type_raw {
                AssetTypeRaw::Native(n) => AssetType::Native(n),
                AssetTypeRaw::Script(g) => AssetType::Script(db.intern_script(g)),
            };
            AssetEntry {
                guid: r.guid,
                asset_type,
                name: r.name.into_boxed_str(),
                sub_assets: r.sub_assets,
                hint: r.hint.into_boxed_str(),
            }
        })
        .collect();
    db.entries = entries;
    db.sort();
    check_no_full_duplicates(&db)?;
    Ok(db)
}

/// Hard-fail on two corruption cases:
///
/// 1. **Two top-level entries share a GUID.** Hand-edited or copy-pasted
///    `.meta` whose GUID wasn't rewritten. The name-dedup loop only
///    renames when guids *differ*, so same-guid pairs flow through with
///    distinct names and `db.sort()` doesn't merge them. Catches the
///    duplicate-`.meta` case the Unity-hidden walker filter also guards
///    against — belt and braces.
///
/// 2. **Within-entry sub-asset rows share `(name, fileID)`.** Two YAML
///    sub-docs in the same asset declared identical names + fileIDs —
///    asset-side corruption, parser bug, or atlas content collision.
fn check_no_full_duplicates(db: &AssetDb) -> Result<()> {
    // Top-level: guid uniqueness. `db.entries` is already guid-sorted, so
    // a single pass over consecutive pairs catches every dup.
    for w in db.entries.windows(2) {
        if w[0].guid == w[1].guid {
            anyhow::bail!(
                "duplicate top-level GUID: {:032x} between names `{}` and `{}` — likely two .meta files share a GUID",
                w[0].guid,
                w[0].name,
                w[1].name,
            );
        }
    }

    // Sub-assets: (guid, fileID, name) uniqueness within each entry.
    let mut seen: AHashSet<(i64, &str)> = AHashSet::new();
    for e in &db.entries {
        seen.clear();
        for s in &e.sub_assets {
            if !seen.insert((s.file_id, &*s.name)) {
                anyhow::bail!(
                    "duplicate sub-asset record: name={} guid={:032x} fileID={} type={:?}",
                    s.name,
                    e.guid,
                    s.file_id,
                    e.asset_type,
                );
            }
        }
    }
    Ok(())
}

/// Read just enough of the asset to satisfy `mode`.
///
/// `TopOnly` reads the first 4 KiB and truncates at the last newline — that
/// covers a YAML preamble (`%YAML 1.1\n%TAG …\n`), the first
/// `--- !u!<id> &<fid>` header, and a `m_Script` line for .asset
/// MonoBehaviours (≤ ~200 bytes). `WithSubAssets` reads the full file.
///
/// Trimming at the last newline guards against UTF-8 boundary cuts inside a
/// multi-byte character — every YAML line is complete UTF-8.
fn read_asset_for_mode(path: &Path, mode: asset::ParseMode) -> Result<String> {
    use std::io::Read;
    match mode {
        asset::ParseMode::WithSubAssets => {
            std::fs::read_to_string(path).with_context(|| format!("read asset: {}", path.display()))
        }
        asset::ParseMode::TopOnly => {
            const HEAD_BYTES: u64 = 4096;
            let f = std::fs::File::open(path)
                .with_context(|| format!("open asset: {}", path.display()))?;
            let mut buf = Vec::with_capacity(HEAD_BYTES as usize);
            f.take(HEAD_BYTES)
                .read_to_end(&mut buf)
                .with_context(|| format!("read asset: {}", path.display()))?;
            // Drop trailing partial line so .lines() yields only complete
            // (and thus complete-UTF-8) lines. If the head has no newline at
            // all (pathological — single-line YAML > 4 KiB), keep the buffer
            // and let `from_utf8` decide.
            if let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') {
                buf.truncate(last_nl + 1);
            }
            String::from_utf8(buf)
                .with_context(|| format!("non-utf8 asset head: {}", path.display()))
        }
    }
}

fn strip_meta_suffix(p: &Path) -> Option<PathBuf> {
    let s = p.to_str()?;
    s.strip_suffix(".meta").map(PathBuf::from)
}

fn rel_hint(project_root: &Path, companion: &Path) -> Result<String> {
    // Strip the project root, not just `Assets/`. The walker now visits both
    // `<project>/Assets/` and `<project>/Packages/`, so hints look like
    // `Assets/Foo.prefab` or `Packages/com.boxcat.libs/Bar.mixer`.
    let rel = companion
        .strip_prefix(project_root)
        .with_context(|| format!("strip prefix: {}", companion.display()))?;
    let s = rel.to_string_lossy().replace('\\', "/");
    Ok(s)
}

fn filename_stem(p: &Path) -> String {
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

fn filename_stem_from_hint(hint: &str) -> String {
    Path::new(hint)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Pick a unique alias for `stem` given `hint` and an existing `taken` map.
/// Strategy: try `stem^dir` for successively-deeper parent dirs. A candidate
/// is considered "free" iff it's absent from `taken` *or* already mapped to
/// `owner_guid` (the latter covers the same-guid sub-asset case where the
/// parent's deduped top-level alias is a valid name to share).
///
/// Hard-fails when no parent segment yields a free candidate — ambiguity
/// surfaces at bake time rather than getting papered over with a guid suffix.
/// See [[asset-database.md#name-collisions]] for the `^` separator rationale.
fn disambiguate(
    stem: &str,
    hint: &str,
    owner_guid: u128,
    taken: &AHashMap<String, u128>,
) -> Result<String> {
    let parts: Vec<&str> = Path::new(hint)
        .parent()
        .map(|p| p.iter().filter_map(|c| c.to_str()).collect::<Vec<_>>())
        .unwrap_or_default();

    // Walk parent segments from nearest to root, picking the shortest
    // suffix that doesn't collide with a different-guid owner.
    let mut suffix = String::new();
    for seg in parts.iter().rev() {
        if !suffix.is_empty() {
            suffix.insert(0, '/');
        }
        suffix.insert_str(0, seg);
        let candidate = format!("{stem}^{suffix}");
        match taken.get(&candidate) {
            None => return Ok(candidate),
            Some(&prev) if prev == owner_guid => return Ok(candidate),
            Some(_) => continue,
        }
    }
    anyhow::bail!(
        "asset-db: cannot disambiguate name `{stem}` for guid {owner_guid:032x} \
         (hint `{hint}`) — every parent-segment suffix is already taken by \
         another asset. Rename one of the colliding assets in source.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_with_panic_safety_passes_through_ok_some() {
        let r: Result<Option<i32>, String> = run_with_panic_safety("path", "task", || Ok(Some(42)));
        assert_eq!(r, Ok(Some(42)));
    }

    #[test]
    fn run_with_panic_safety_passes_through_ok_none() {
        let r: Result<Option<i32>, String> = run_with_panic_safety("path", "task", || Ok(None));
        assert_eq!(r, Ok(None));
    }

    #[test]
    fn run_with_panic_safety_formats_inner_error_with_label() {
        let r: Result<Option<i32>, String> = run_with_panic_safety("foo.meta", "task", || {
            Err(anyhow::anyhow!("malformed yaml"))
        });
        assert_eq!(r, Err("foo.meta: malformed yaml".to_string()));
    }

    #[test]
    fn run_with_panic_safety_catches_str_panic() {
        let r: Result<Option<i32>, String> =
            run_with_panic_safety("foo.meta", "process_one", || {
                std::panic::panic_any("boom (&str payload)")
            });
        assert_eq!(
            r,
            Err("foo.meta: panic in process_one: boom (&str payload)".to_string())
        );
    }

    #[test]
    fn run_with_panic_safety_catches_string_panic() {
        let r: Result<Option<i32>, String> =
            run_with_panic_safety("foo.meta", "process_one", || {
                // String payloads come from `panic!("{x}")` via the format!
                // path — the runtime hands a String, not a &str.
                panic!("formatted {}", "msg")
            });
        assert_eq!(
            r,
            Err("foo.meta: panic in process_one: formatted msg".to_string())
        );
    }

    #[test]
    fn run_with_panic_safety_handles_non_string_panic_payload() {
        // `panic_any(42_i32)` produces a panic whose payload isn't &str
        // or String. The helper falls back to a sentinel message rather
        // than dropping the error silently.
        let r: Result<Option<i32>, String> =
            run_with_panic_safety("foo.meta", "process_one", || std::panic::panic_any(42_i32));
        assert_eq!(
            r,
            Err("foo.meta: panic in process_one: <non-string panic payload>".to_string())
        );
    }

    fn meta_for(
        texture_type: Option<u32>,
        sprite_mode: Option<u32>,
        sprites: Vec<(i64, String)>,
    ) -> meta::MetaInfo {
        meta::MetaInfo {
            guid: 0,
            sprite_sheet: sprites,
            texture_type,
            sprite_mode,
        }
    }

    #[test]
    fn synthesize_implicit_sprite_fires_on_single_mode_sprite_with_empty_sheet() {
        let m = meta_for(Some(TEXTURE_TYPE_SPRITE), Some(SPRITE_MODE_SINGLE), vec![]);
        let sub = synthesize_implicit_sprite(&m, "Icon").expect("synthesis should fire");
        assert_eq!(sub.file_id, ClassId::Sprite.canonical_subobject_fid());
        assert_eq!(&*sub.name, "Icon");
    }

    #[test]
    fn synthesize_implicit_sprite_skips_when_sheet_non_empty() {
        // Explicit sprites own the sub-asset list — atlas-shaped meta
        // doesn't get a phantom main-Sprite layered on top.
        let m = meta_for(
            Some(TEXTURE_TYPE_SPRITE),
            Some(SPRITE_MODE_SINGLE),
            vec![(12345, "explicit_a".into())],
        );
        assert!(synthesize_implicit_sprite(&m, "Icon").is_none());
    }

    #[test]
    fn synthesize_implicit_sprite_skips_on_multiple_mode() {
        // spriteMode: 2 (Multiple = atlas) means "the sprites: list is
        // canonical, even if currently empty". No synthesis.
        let m = meta_for(Some(TEXTURE_TYPE_SPRITE), Some(2), vec![]);
        assert!(synthesize_implicit_sprite(&m, "Icon").is_none());
    }

    #[test]
    fn synthesize_implicit_sprite_skips_on_non_sprite_texture() {
        // textureType: 0 (Default) — texture isn't a Sprite at all.
        let m = meta_for(Some(0), Some(SPRITE_MODE_SINGLE), vec![]);
        assert!(synthesize_implicit_sprite(&m, "Icon").is_none());
    }

    #[test]
    fn synthesize_implicit_sprite_skips_when_predicates_absent() {
        // Both texture_type and sprite_mode None — `.meta` from a
        // non-texture asset (or a stale .meta missing the fields).
        let m = meta_for(None, None, vec![]);
        assert!(synthesize_implicit_sprite(&m, "Icon").is_none());
    }

    #[test]
    fn stem_basic() {
        assert_eq!(filename_stem(Path::new("foo/Bar.prefab")), "Bar");
        assert_eq!(filename_stem_from_hint("foo/Bar.prefab"), "Bar");
    }

    #[test]
    fn disambiguate_walks_parents() {
        let mut taken = AHashMap::new();
        taken.insert("Foo".to_string(), 1u128);
        // Nearest parent suffix wins on first try.
        let alias = disambiguate("Foo", "pkg/Editor/Foo.cs", 2, &taken).unwrap();
        assert_eq!(alias, "Foo^Editor");

        // First-level parent already taken (by a different guid) → falls
        // back to deeper path.
        taken.insert("Foo^Editor".to_string(), 3);
        let alias = disambiguate("Foo", "pkg/Editor/Foo.cs", 2, &taken).unwrap();
        assert_eq!(alias, "Foo^pkg/Editor");
    }

    #[test]
    fn disambiguate_returns_existing_when_same_owner() {
        // When the candidate suffix is already mapped to `owner_guid`, the
        // sub-asset can safely share that alias — its lookup path resolves
        // back to the same guid, so no real ambiguity exists.
        let mut taken = AHashMap::new();
        taken.insert("Cloud1".to_string(), 0xa0_u128);
        taken.insert("Cloud1^Tower".to_string(), 0xb0_u128);
        let alias = disambiguate("Cloud1", "Assets/Tower/Cloud1.png", 0xb0_u128, &taken).unwrap();
        assert_eq!(alias, "Cloud1^Tower");
    }

    #[test]
    fn disambiguate_hard_fails_when_no_parent_segments() {
        let mut taken = AHashMap::new();
        taken.insert("Foo".to_string(), 1u128);
        // Hint has no directories — nothing to suffix with. Must error
        // rather than silently fall back to a guid suffix.
        let err = disambiguate("Foo", "Foo.cs", 2u128, &taken).expect_err("must hard-fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("disambiguate"), "msg: {msg}");
        assert!(msg.contains("Foo"), "msg: {msg}");
    }

    fn raw_native(hint: &str, guid: u128, sub_assets: Vec<SubAsset>) -> RawEntry {
        RawEntry {
            guid,
            asset_type_raw: AssetTypeRaw::Native(ClassId::Texture2D as u32),
            hint: hint.to_string(),
            // `build_db`'s first pass overwrites `name` from `hint`, so any
            // value here is fine. Empty kept the test minimal.
            name: String::new(),
            meta_mtime_ns: 0,
            asset_mtime_ns: 0,
            sub_assets,
        }
    }

    /// Pin: when a name is claimed by ≥2 distinct guids (whether at the
    /// top level or inside sub-assets), every claimant must rename — no
    /// "first wins" carve-out. The deduped form is consistent across
    /// claimants: each entry resolves through `disambiguate` against its
    /// own hint.
    ///
    /// Mirrors the real-fixture `Cloud1` case: a `Cloud1.asset` and a
    /// `Cloud1.png` Texture2D (whose Sprite sub-asset is also named
    /// `Cloud1`) all rename. The png's sub-asset shares the parent's
    /// renamed alias since they have the same guid + hint.
    #[test]
    fn build_db_renames_every_claimant_when_name_is_contested() {
        let asset_guid = 0xa0_u128;
        let png_guid = 0xb0_u128;
        let sprite_fid: i64 = 21300000;

        let raw = vec![
            raw_native("Assets/Other/Cloud1.asset", asset_guid, vec![]),
            raw_native(
                "Assets/Tower/Cloud1.png",
                png_guid,
                vec![SubAsset {
                    file_id: sprite_fid,
                    name: "Cloud1".into(),
                }],
            ),
        ];

        let db = build_db(raw, None, None, false).expect("build_db should succeed");

        let asset_entry = db.find_by_guid(asset_guid).unwrap();
        let png_entry = db.find_by_guid(png_guid).unwrap();

        // Neither entry keeps the bare alias — both renamed.
        assert_ne!(&*asset_entry.name, "Cloud1");
        assert_ne!(&*png_entry.name, "Cloud1");
        assert!(
            asset_entry.name.starts_with("Cloud1^"),
            "asset top-level not deduped: {}",
            asset_entry.name,
        );
        assert!(
            png_entry.name.starts_with("Cloud1^"),
            "png top-level not deduped: {}",
            png_entry.name,
        );
        // Distinct hints → distinct deduped suffixes.
        assert_ne!(&*asset_entry.name, &*png_entry.name);

        // Sub-asset dedup: png's Sprite shares the parent's renamed alias
        // (same guid + same hint feeds `disambiguate` to the same suffix).
        let sub = &png_entry.sub_assets[0];
        assert_eq!(sub.file_id, sprite_fid);
        assert_eq!(
            &*sub.name, &*png_entry.name,
            "sub-asset name must match parent's deduped top-level alias",
        );
    }

    /// Pin: a single-owner name (one guid only, even if it appears as both
    /// a top-level alias and one of its own sub-assets) is *not*
    /// contested — it stays bare. Guards against over-renaming the common
    /// case of a Texture2D and its lone same-named Sprite sub-asset.
    #[test]
    fn build_db_keeps_bare_alias_when_name_is_uncontested() {
        let png_guid = 0xb0_u128;
        let raw = vec![raw_native(
            "Assets/Tower/Lone.png",
            png_guid,
            vec![SubAsset {
                file_id: 21300000,
                name: "Lone".into(),
            }],
        )];

        let db = build_db(raw, None, None, false).expect("build_db should succeed");
        let entry = db.find_by_guid(png_guid).unwrap();
        assert_eq!(&*entry.name, "Lone");
        assert_eq!(&*entry.sub_assets[0].name, "Lone");
    }

    /// Pin: when a top-level alias is genuinely unresolvable (no parent
    /// segments left to walk and the bare stem is already taken), the
    /// bake hard-fails rather than silently falling back to a `^<guid8>`
    /// suffix. Per the project policy: ambiguity surfaces at bake time,
    /// not encode time.
    #[test]
    fn build_db_fails_when_dedup_cannot_resolve() {
        let raw = vec![
            // Two top-level entries with the same bare stem and no parent
            // segments to walk — `disambiguate` has nothing to suffix with.
            raw_native("Foo.asset", 0x01_u128, vec![]),
            raw_native("Foo.prefab", 0x02_u128, vec![]),
        ];

        let err = build_db(raw, None, None, false).expect_err("collision with no parent dirs must hard-fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Foo") && msg.contains("disambiguate"),
            "error message should name the collision and the dedup pass: {msg}",
        );
    }
}
