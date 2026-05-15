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
    self, AssetDb, AssetEntry, AssetType, BakeCache, CachedAssetType, CachedEntry, StoreError,
    SubAsset, CACHE_FILENAME, DB_FILENAME,
};
use crate::query::format_guid;
use crate::register::{generate_guid, importer_for_path, render_meta};
use crate::walk::{walk_for_missing_meta, walk_meta_files, WalkError};

/// Errors from a bake run.
///
/// `Store(StoreError)` and `Walk(WalkError)` surface the typed source
/// errors from those modules — match on them when you need to
/// distinguish (e.g. "is this a schema-mismatch that needs re-bake?").
/// `Other` carries the remaining anyhow-chained errors (cache I/O,
/// dedup hard-fails, duplicate-guid checks) — most consumers propagate
/// these untouched.
#[derive(Debug, thiserror::Error)]
pub enum BakeError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Walk(#[from] WalkError),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

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

/// Borrowed view of a [`NameSanitizer`] for internal helpers. Kept as a
/// named type so per-call signatures don't trip clippy's `type_complexity`.
type NameSanitizerRef<'a> = &'a (dyn Fn(&str) -> Option<String> + Send + Sync);

/// Borrowed view of a [`WarnSink`]. See [`NameSanitizerRef`].
type WarnSinkRef<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// Borrowed view of a [`ProgressSink`]. See [`NameSanitizerRef`].
type ProgressSinkRef<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// File extensions whose asset has embedded sub-asset docs that should
/// NOT join the global dedup pool — they live in the parent's namespace
/// and consumers resolve them via parent-scoped addressing (`$Sub@Parent`).
///
/// Extension-keyed rather than `AssetType`-keyed because the top doc of a
/// `.playable` file is whichever sub-doc Unity sorts first by hashed fileID
/// (often an `AnimationTrack`, not the `TimelineAsset` itself), so the
/// resulting `AssetTypeRaw::Script(...)` carries an unstable script guid.
/// The extension is the only stable container discriminator.
const EMBEDDED_CONTAINER_EXTS: &[&str] = &["prefab", "controller", "anim", "mixer", "playable"];

fn is_embedded_container(hint: &str) -> bool {
    Path::new(hint)
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| EMBEDDED_CONTAINER_EXTS.contains(&ext))
}

/// True when `class_id` is a structural sub-doc that should be filtered
/// out at parse time for the given container extension.
///
/// `.prefab`: GO / Transform / RectTransform / MonoBehaviour are all
/// part of the GameObject tree — never addressable as sub-assets.
/// `.controller` / `.anim` / `.mixer` / `.playable`: MonoBehaviour-114
/// docs ARE addressable sub-assets (Timeline tracks, AudioMixerGroup,
/// etc.) — only filter the GO-tree triplet, which doesn't appear in
/// these files anyway (the predicate is a no-op there but stays valid
/// for future-proofing).
fn is_filterable_subdoc_for_ext(class_id: u32, ext: &str) -> bool {
    let cls = ClassId::from_raw(class_id);
    let is_go_tree = matches!(
        cls,
        Some(ClassId::GameObject | ClassId::Transform | ClassId::RectTransform)
    );
    let is_component = matches!(cls, Some(ClassId::MonoBehaviour));
    is_go_tree || (is_component && ext == "prefab")
}

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

/// Hashable type discriminator: `Native(classID)` for built-in classes
/// and `Script(scriptGuid)` for MonoBehaviour-backed assets. Hashable so
/// the dedup pass can bucket by `(name, asset_type)` without depending
/// on the post-walk script-intern table.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum AssetTypeRaw {
    Native(u32),
    Script(u128),
}

/// Public, dedup-free view of a single parsed asset — what [`parse_one`]
/// returns. Script GUIDs are unmapped; caller calls
/// [`crate::store::AssetDb::intern_script`].
#[derive(Debug, Clone)]
pub struct ParsedEntry {
    pub guid: u128,
    pub asset_type: ParsedAssetType,
    pub hint: String,
    /// Raw filename stem — no dedup, no sanitizer applied.
    pub name: String,
    pub meta_mtime_ns: u64,
    pub asset_mtime_ns: u64,
    pub sub_assets: Vec<SubAsset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedAssetType {
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
        // The HashMap key duplicates the value's hint. Storing the same
        // string twice burns 18k extra allocations on a warm bake; instead,
        // leave hint empty in the cached value and stamp it from the key
        // when `process_one` clones the entry on a cache hit. RawEntry's
        // hint field is the consumer of record.
        let hint = String::from(e.hint);
        let raw = RawEntry {
            guid: e.guid,
            asset_type_raw,
            hint: String::new(),
            name: String::new(),
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

/// Bake entry-point. Walks `Assets/`, parses `.meta` + asset YAML,
/// writes `<out_dir>/asset-db.bin` and `<out_dir>/asset-db.cache.bin`.
pub fn bake(opts: &BakeOptions) -> Result<(), BakeError> {
    bake_inner(opts).map_err(map_bake_err)
}

fn bake_inner(opts: &BakeOptions) -> Result<()> {
    let project_root = &opts.project_root;
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("create out-dir: {}", opts.out_dir.display()))?;
    let db_file = opts.out_dir.join(DB_FILENAME);
    let cache_file = opts.out_dir.join(CACHE_FILENAME);

    // Advisory flock on `<out_dir>/.asset-db.lock` — shared with
    // `register` so concurrent invocations don't clobber bin/cache.
    let _lock = store::acquire_lock(&opts.out_dir, store::LockWait::Forever)
        .with_context(|| format!("lock: {}", opts.out_dir.display()))?;

    let t_start = Instant::now();

    // Pre-pass: synthesize a minimal `.meta` for every asset / folder
    // under `Assets/` (and inside `Packages/<pkg>/`) that lacks one.
    // Mirrors what Unity does on editor focus, so a fresh `bake` works
    // against a project tree where someone dropped files in without
    // opening the editor. See `crate::register` for the synthesized
    // body — Unity rewrites the importer block on next focus while
    // preserving the GUID.
    let t_prepass = Instant::now();
    synthesize_missing_metas(project_root, opts.on_progress.as_deref())
        .context("synthesize missing .meta files")?;
    let dt_prepass = t_prepass.elapsed();

    // Load bake-only cache. Missing/corrupt → empty (first bake or stale).
    let t_cache_io = Instant::now();
    let cache_bytes = std::fs::read(&cache_file).ok();
    let dt_cache_io = t_cache_io.elapsed();
    let t_cache_decode = Instant::now();
    let cache_decoded = cache_bytes.as_deref().and_then(|b| store::decode_cache(b).ok());
    let dt_cache_decode = t_cache_decode.elapsed();
    let t_cache_map = Instant::now();
    let cache: CacheMap = cache_decoded.map(build_cache).unwrap_or_default();
    let dt_cache_map = t_cache_map.elapsed();
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
                "  prepass={dt_prepass:?} cache_io={dt_cache_io:?} cache_decode={dt_cache_decode:?} cache_map={dt_cache_map:?}",
            ));
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

/// Scan `Assets/` and `Packages/<pkg>/` for files/folders missing a
/// sibling `.meta` and synthesize one for each — same minimal body
/// `register` writes. Emits one info line per created file plus a
/// summary line (when any were created) through `on_progress`.
///
/// Called from `bake_inner` before the main walk so freshly-authored
/// metas join the same parse pass as pre-existing ones. The `bake`
/// flock is held across the call, so we share the same concurrency
/// guarantee as `register`.
fn synthesize_missing_metas(
    project_root: &Path,
    on_progress: Option<ProgressSinkRef<'_>>,
) -> Result<()> {
    // Visitor can't return Result — stash the first failure and bubble
    // it up so a partial bake never silently misses rows.
    let mut created = 0usize;
    let mut err: Option<anyhow::Error> = None;
    walk_for_missing_meta(project_root, |path, is_dir| {
        if err.is_some() {
            return;
        }
        match write_minimal_meta(path, is_dir) {
            Ok(true) => {
                created += 1;
                if let Some(sink) = on_progress {
                    let rel = path
                        .strip_prefix(project_root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    sink(&format!("created .meta for {rel}"));
                }
            }
            Ok(false) => {} // raced with another writer — fine.
            Err(e) => err = Some(e),
        }
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    if created > 0 && let Some(sink) = on_progress {
        sink(&format!("created {created} missing .meta file(s)"));
    }
    Ok(())
}

/// Write a minimal `.meta` next to `path` with a fresh GUID. Returns
/// `Ok(true)` when the file was created, `Ok(false)` when another
/// writer beat us to it (`AlreadyExists`).
fn write_minimal_meta(path: &Path, is_dir: bool) -> Result<bool> {
    use std::io::Write;
    let kind = importer_for_path(path, is_dir);
    let guid = generate_guid().map_err(|e| anyhow::anyhow!("generate guid: {e}"))?;
    let body = render_meta(&format_guid(guid), kind, is_dir);
    let meta_path = with_meta_suffix(path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&meta_path)
    {
        Ok(mut f) => {
            f.write_all(body.as_bytes())
                .with_context(|| format!("write meta: {}", meta_path.display()))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("create meta: {}", meta_path.display()))),
    }
}

/// Single-asset parse, callable outside the parallel walk.
///
/// Stats `meta_path` + its companion file, parses both, returns the same
/// shape `bake` would produce for this asset (sans dedup and sans
/// post-walk `script_types` interning).
///
/// Returns `Ok(None)` when the meta has no companion to describe
/// (orphaned `.meta`, directory `.meta`).
///
/// Used by `register` to compute the entry to insert into an existing
/// `AssetDb` after synthesizing a new `.meta`. See [`crate::register`].
pub fn parse_one(
    project_root: &Path,
    meta_path: &Path,
) -> Result<Option<ParsedEntry>, BakeError> {
    parse_one_inner(project_root, meta_path).map_err(map_bake_err)
}

fn parse_one_inner(project_root: &Path, meta_path: &Path) -> Result<Option<ParsedEntry>> {
    let companion =
        strip_meta_suffix(meta_path).ok_or_else(|| anyhow::anyhow!("not a .meta path"))?;
    let hint = rel_hint(project_root, &companion)?;
    let meta_md =
        std::fs::metadata(meta_path).with_context(|| format!("stat: {}", meta_path.display()))?;
    let meta_mtime_ns = mtime_ns(meta_md.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    let Ok(companion_md) = std::fs::metadata(&companion) else {
        return Ok(None);
    };
    if companion_md.is_dir() {
        return Ok(None);
    }
    let asset_mtime_ns = mtime_ns(companion_md.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    let raw = process_one_uncached(meta_path, &companion, &hint, meta_mtime_ns, asset_mtime_ns)?;
    Ok(raw.map(raw_to_parsed))
}

fn raw_to_parsed(r: RawEntry) -> ParsedEntry {
    ParsedEntry {
        guid: r.guid,
        asset_type: match r.asset_type_raw {
            AssetTypeRaw::Native(n) => ParsedAssetType::Native(n),
            AssetTypeRaw::Script(g) => ParsedAssetType::Script(g),
        },
        hint: r.hint,
        name: r.name,
        meta_mtime_ns: r.meta_mtime_ns,
        asset_mtime_ns: r.asset_mtime_ns,
        sub_assets: r.sub_assets,
    }
}

/// Surface `?`-propagated `StoreError` / `WalkError` as typed variants so
/// consumers can match on them. Anything else falls through to `Other`.
fn map_bake_err(e: anyhow::Error) -> BakeError {
    match e.downcast::<StoreError>() {
        Ok(s) => BakeError::Store(s),
        Err(e) => match e.downcast::<WalkError>() {
            Ok(w) => BakeError::Walk(w),
            Err(e) => BakeError::Other(e),
        },
    }
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

    let hint = rel_hint(project_root, &companion)?;

    // Cache-hit fast path: stat `.meta` only. If the mtime matches the
    // cache, trust the cached row outright — no companion stat. Saves
    // ~1 stat × N entries on the warm bake, the bake's dominant cost
    // (warm walk against meow-tower dropped from 47 ms → ~26 ms).
    //
    // **Cache assumption**: Unity's importer touches the `.meta` mtime
    // whenever it re-imports the asset, so a `.meta` mtime drift is the
    // canonical "this asset changed" signal. Hand-editing the asset YAML
    // *without* touching the .meta will serve a stale cached row until
    // the next .meta touch (or a manual `rm asset-db.cache.bin`).
    // Documented + pinned by `tests/bake.rs::cache_does_not_detect_asset_only_touch`.
    let meta_md =
        std::fs::metadata(meta_path).with_context(|| format!("stat: {}", meta_path.display()))?;
    let meta_mtime_ns = mtime_ns(meta_md.modified().unwrap_or(SystemTime::UNIX_EPOCH));

    if let Some(cached) = cache.get(&hint)
        && cached.meta_mtime_ns == meta_mtime_ns
    {
        cache_hits.fetch_add(1, Ordering::Relaxed);
        // `cached.hint` is intentionally empty (see `build_cache`) — the
        // hit's hint is the one we computed above, identical bytes either
        // way. One allocation per hit instead of two.
        let mut out = cached.clone();
        out.hint = hint;
        return Ok(Some(out));
    }

    // Cache miss. Now stat the companion — handles directory-`.meta`
    // exclusion too. Slow path beyond here re-parses both files.
    let Ok(companion_md) = std::fs::metadata(&companion) else {
        return Ok(None);
    };
    if companion_md.is_dir() {
        return Ok(None);
    }
    let asset_mtime_ns = mtime_ns(companion_md.modified().unwrap_or(SystemTime::UNIX_EPOCH));

    process_one_uncached(meta_path, &companion, &hint, meta_mtime_ns, asset_mtime_ns)
}

/// Slow path: parse `.meta` + asset YAML, build a `RawEntry`. Shared
/// between the "no cache row at all" and "cache row but companion mtime
/// drifted" cases — both end up doing the same parse work.
fn process_one_uncached(
    meta_path: &Path,
    companion: &Path,
    hint: &str,
    meta_mtime_ns: u64,
    asset_mtime_ns: u64,
) -> Result<Option<RawEntry>> {
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
    //  - WithSubAssets: types where extra docs ARE addressable from outside.
    //    `.asset`/`.spriteatlas`/`.spriteatlasv2` host explicit sub-assets;
    //    `.prefab`/`.controller`/`.anim`/`.mixer`/`.playable` can host
    //    embedded sub-asset docs (legacy `AnimationClip` inline in a
    //    prefab; AnimatorState in a controller; AudioMixerGroup in a
    //    mixer; Timeline tracks in a playable) that other prefabs
    //    address as `{fileID, guid: <parent.guid>, type: 3}`. Without
    //    capturing them the embedded ref encodes as `&#f<fid>` and
    //    cross-prefab refs degrade to the parent alias + `#f<fid>` suffix.
    //    Embeds are excluded from the global dedup pool — see
    //    `is_embedded` in `build_db`.
    //  - TopOnly: types whose extra docs are internal scene-graph that
    //    isn't addressable from outside (`.unity`, `.mat`, `.mask`).
    //  - None: extension already says everything (`.png`, `.fbx`, scripts).
    let parse_mode: Option<asset::ParseMode> = match ext {
        "asset" | "spriteatlas" | "spriteatlasv2" | "prefab" | "controller" | "anim"
        | "mixer" | "playable" => Some(asset::ParseMode::WithSubAssets),
        "mat" | "mask" | "unity" => Some(asset::ParseMode::TopOnly),
        _ => None,
    };

    if let Some(mode) = parse_mode {
        let asset_text = read_asset_for_mode(companion, mode)?;
        let info = asset::parse(&asset_text, mode)?;
        top_class_id = info.top_class_id;
        script_guid = info.script_guid;
        for s in info.sub_assets {
            if s.name.is_empty() {
                continue;
            }
            if is_filterable_subdoc_for_ext(s.class_id, ext) {
                continue;
            }
            sub_assets.push(SubAsset {
                file_id: s.file_id,
                class_id: s.class_id,
                name: s.name.into_boxed_str(),
            });
        }
    }

    // Precedence: script_guid (MonoBehaviour-backed) > from_ext > top_class_id
    // > `DefaultImporter` fallback (see `ClassId::DefaultAsset`).
    // `.prefab` and `.unity` deliberately let from_ext win — their YAML's
    // first doc is a *contained* object (GameObject = classID 1), not the
    // asset's class (Prefab = 1001). Falling back to top_class_id only for
    // extensions without a stable class mapping (e.g. `.asset`, where the
    // YAML peek is the only signal).
    let asset_type_raw = if let Some(g) = script_guid {
        AssetTypeRaw::Script(g)
    } else if let Some(cls) = from_ext {
        AssetTypeRaw::Native(cls as u32)
    } else if let Some(cls) = top_class_id.and_then(ClassId::from_raw) {
        AssetTypeRaw::Native(cls as u32)
    } else if let Some(cls) = top_class_id {
        // Unknown raw class ID — store anyway; lookup will treat as Native.
        AssetTypeRaw::Native(cls)
    } else if meta_info.importer.as_deref() == Some("DefaultImporter") {
        AssetTypeRaw::Native(ClassId::DefaultAsset as u32)
    } else {
        return Ok(None);
    };

    let name = filename_stem(companion);

    // Implicit Sprite sub-asset for Single-mode textures. Compute first
    // (borrows `meta_info` whole); the for-loop below moves
    // `meta_info.sprite_sheet`, so the predicate must run before that.
    let implicit_sprite = synthesize_implicit_sprite(&meta_info, &name);

    // Texture sprite-sheet sub-assets (from .meta). Always class Sprite —
    // .meta `sprites:` entries are by definition Sprite sub-assets of the
    // texture (Unity's Sprite-mode importer creates them at fileID-as-hash).
    for (fid, name) in meta_info.sprite_sheet {
        sub_assets.push(SubAsset {
            file_id: fid,
            class_id: ClassId::Sprite as u32,
            name: name.into_boxed_str(),
        });
    }

    if let Some(sub) = implicit_sprite {
        sub_assets.push(sub);
    }

    Ok(Some(RawEntry {
        guid: meta_info.guid,
        asset_type_raw,
        hint: hint.to_string(),
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
            class_id: ClassId::Sprite as u32,
            name: stem.to_string().into_boxed_str(),
        })
    } else {
        None
    }
}

fn warn_sanitized(on_warn: Option<WarnSinkRef<'_>>, kind: &str, hint: &str, old: &str, new: &str) {
    if let Some(sink) = on_warn {
        sink(&format!(
            "warning: {kind} {hint} name `{old}` contains ref-reserved char; renamed to `{new}`",
        ));
    }
}

fn build_db(
    mut raw: Vec<RawEntry>,
    sanitizer: Option<NameSanitizerRef<'_>>,
    on_warn: Option<WarnSinkRef<'_>>,
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

    // Type-aware dedup: collisions are scoped by `(name, asset_type)`.
    // Same-name entries of distinct `asset_type` (`Foo.png` Texture2D +
    // `Foo.prefab` Prefab) get distinct alias buckets — the consuming
    // field's C# type discriminates at decode. Embedded sub-asset docs
    // of container types are excluded from the global pool entirely
    // (see [Name collisions](docs/asset-database.md#name-collisions)).

    // Pass 1: tally distinct-guid owners per `(name, asset_type)` bucket.
    let mut owners: AHashMap<(String, AssetTypeRaw), AHashSet<u128>> =
        AHashMap::with_capacity(raw.len());
    for r in &raw {
        let key = (r.name.clone(), r.asset_type_raw);
        owners.entry(key).or_default().insert(r.guid);
        if is_embedded_container(&r.hint) {
            continue;
        }
        for sub in &r.sub_assets {
            let key = (
                sub.name.to_string(),
                AssetTypeRaw::Native(sub.class_id),
            );
            owners.entry(key).or_default().insert(r.guid);
        }
    }
    let contested = |name: &str, t: AssetTypeRaw| {
        owners
            .get(&(name.to_string(), t))
            .is_some_and(|s| s.len() > 1)
    };

    // Pass 2: walk entries in hint-sorted order, renaming every contested
    // claim via `parent_suffix` (depth-2, pure function of own hint). The
    // post-hoc `taken` map tracks `(name, asset_type) → (guid, hint)` and
    // hard-fails when two distinct-guid claimants compute the same suffix
    // (e.g. hints sharing the same last 2 parent segments); the recorded
    // hint feeds the error so the user sees both colliding paths.
    // Same-guid sharing remains allowed.
    let mut taken: AHashMap<(String, AssetTypeRaw), (u128, String)> =
        AHashMap::with_capacity(raw.len());

    for r in raw.iter_mut() {
        let top_type = r.asset_type_raw;
        if contested(&r.name, top_type) {
            let new_name = collision_suffix(top_type, &r.hint, &r.name, r.guid)?;
            if verbose_collisions && let Some(sink) = on_warn {
                sink(&format!(
                    "warning: name collision on `{}` (guid {:032x}); renamed to `{}`",
                    r.name, r.guid, new_name,
                ));
            }
            r.name = new_name;
        }
        claim(&mut taken, &r.name, top_type, r.guid, &r.hint)?;

        if is_embedded_container(&r.hint) {
            // Prefab-embedded sub-assets bypass the global dedup pool;
            // sanitization already happened above. Names stay as authored
            // and resolve via `$Sub@Parent` at the codec layer.
            continue;
        }
        for sub in r.sub_assets.iter_mut() {
            let sub_type = AssetTypeRaw::Native(sub.class_id);
            if contested(&sub.name, sub_type) {
                let original = sub.name.to_string();
                let new_name = collision_suffix(sub_type, &r.hint, &original, r.guid)?;
                if verbose_collisions && let Some(sink) = on_warn {
                    sink(&format!(
                        "warning: sub-asset name collision on `{}` (parent guid {:032x}); renamed to `{}`",
                        original, r.guid, new_name,
                    ));
                }
                sub.name = new_name.into_boxed_str();
            }
            claim(&mut taken, &sub.name, sub_type, r.guid, &r.hint)?;
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

/// `Foo.png.meta` → `Foo.png`. None when `p` isn't a `.meta` path.
pub fn strip_meta_suffix(p: &Path) -> Option<PathBuf> {
    let s = p.to_str()?;
    s.strip_suffix(".meta").map(PathBuf::from)
}

/// `Foo.png` → `Foo.png.meta`. Inverse of [`strip_meta_suffix`].
pub fn with_meta_suffix(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".meta");
    PathBuf::from(s)
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

/// Depth of the parent-suffix the dedup pass uses when an alias is
/// contested. Structural rule, not a tuning knob — see
/// [Name collisions](docs/asset-database.md#name-collisions).
const MIN_PARENTS: usize = 2;

/// Render an `AssetTypeRaw` as a human-readable string for diagnostics.
/// Mirrors [`crate::query::asset_type_str`] but operates on the
/// pre-intern raw form (carries the script GUID directly) — used in
/// bake-side error messages where the [`crate::store::AssetDb`] hasn't
/// been finalized yet.
fn asset_type_raw_str(t: AssetTypeRaw) -> String {
    match t {
        AssetTypeRaw::Native(n) => match ClassId::from_raw(n) {
            Some(c) => c.name().to_string(),
            None => format!("Native:{n}"),
        },
        AssetTypeRaw::Script(g) => format!("Script:{}", format_guid(g)),
    }
}

/// Post-hoc dedup-pool claim. Inserts `(name, asset_type) → (guid, hint)`
/// into `taken`; tolerates same-guid re-claims (a sub-asset sharing the
/// parent's deduped alias). Bails when a distinct-guid claim collides —
/// the recorded hint of the prior claimant feeds the error so the user
/// sees both colliding paths.
fn claim(
    taken: &mut AHashMap<(String, AssetTypeRaw), (u128, String)>,
    name: &str,
    t: AssetTypeRaw,
    guid: u128,
    hint: &str,
) -> Result<()> {
    match taken.get(&(name.to_string(), t)) {
        Some((prev_guid, _)) if *prev_guid == guid => Ok(()),
        Some((prev_guid, prev_hint)) => anyhow::bail!(
            "asset-db: cannot disambiguate name `{name}` (asset_type {ty}) — \
             two assets share the same depth-{MIN_PARENTS} parent suffix:\n  \
             {prev_hint} (guid {prev_guid:032x})\n  \
             {hint} (guid {guid:032x})\n\
             Rename one in source.",
            ty = asset_type_raw_str(t),
        ),
        None => {
            taken.insert((name.to_string(), t), (guid, hint.to_string()));
            Ok(())
        }
    }
}

/// Pick the collision-disambiguation suffix for a contested entry.
/// `.cs` MonoScript filenames are conventional Unity classnames whose
/// downstream lookups go through GUIDs regardless, so mirror-package
/// vendoring (UniTask vs Zenject both shipping a `Runtime/Utils/L.cs`)
/// routinely produces depth-2 path collisions — the GUID-prefix suffix
/// sidesteps the problem entirely. Every other asset type gets the
/// path-based depth-2 suffix where the surrounding directories are
/// meaningful.
fn collision_suffix(t: AssetTypeRaw, hint: &str, stem: &str, guid: u128) -> Result<String> {
    if matches!(t, AssetTypeRaw::Native(c) if c == ClassId::MonoScript as u32) {
        return Ok(guid_suffix(stem, guid));
    }
    parent_suffix(hint, stem, MIN_PARENTS)
}

/// Length of the GUID-prefix suffix used by [`guid_suffix`]. 8 hex chars
/// = 32 bits = ~0.01% birthday-collision odds at N=1000 — comfortable
/// headroom for typical projects.
const GUID_SUFFIX_LEN: usize = 8;

/// Suffix `stem` with the first 8 hex chars of `guid`: `L^9ddf5ad8`.
/// Used for contested structural assets (see [`uses_guid_suffix`])
/// where path-based suffixing fails on mirror-package vendoring. Alias
/// is intrinsic to the asset: survives `git mv` and is independent of
/// sibling churn.
///
/// 8-hex collisions across two distinct GUIDs are exceptionally rare;
/// when they do happen, [`claim`] still hard-fails — the user can
/// regenerate one of the colliding script GUIDs to resolve.
fn guid_suffix(stem: &str, guid: u128) -> String {
    let hex = format_guid(guid);
    format!("{stem}^{}", &hex[..GUID_SUFFIX_LEN])
}

/// Compute a contested entry's alias as `stem^<last min_parents parents>`.
///
/// Pure function of `hint`: no `taken` map, no `owner_guid`, no order
/// dependence. The shape of the suffix doesn't change when sibling claimants
/// are added or removed elsewhere in the project — the stability win over
/// the prior "shortest-suffix wins among contestants" rule.
///
/// `min_parents` is a soft floor: take the **last** N parent segments
/// joined with `/`. If `hint` has fewer than N parent segments, take all
/// available (so a root-near asset still gets a suffix; only a totally
/// parentless hint errors).
///
/// Hard-fails when `hint` has zero parent segments — ambiguity surfaces
/// at bake time rather than getting papered over with a guid suffix. The
/// caller (`build_db`) is responsible for detecting cross-contestant
/// suffix collisions (two hints whose last `min_parents` segments are
/// identical) post-hoc; this helper just emits the candidate.
///
/// See [Name collisions](docs/asset-database.md#name-collisions) for the
/// `^` separator rationale.
fn parent_suffix(hint: &str, stem: &str, min_parents: usize) -> Result<String> {
    let parts: Vec<&str> = Path::new(hint)
        .parent()
        .map(|p| p.iter().filter_map(|c| c.to_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    if parts.is_empty() {
        anyhow::bail!(
            "asset-db: cannot disambiguate name `{stem}` — hint `{hint}` has no \
             parent segments. Rename one of the colliding assets in source.",
        );
    }
    let take = min_parents.min(parts.len()).max(1);
    let suffix = parts[parts.len() - take..].join("/");
    Ok(format!("{stem}^{suffix}"))
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
            importer: None,
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

    /// `is_filterable_subdoc_for_ext` is the single point where parse-
    /// time sub-asset filtering decides what's a structural prefab tree
    /// doc vs. a real sub-asset. Pin the contract per extension.
    #[test]
    fn is_filterable_subdoc_for_ext_branches_correctly() {
        // .prefab: GO + Transform + RectTransform + MonoBehaviour-as-component.
        for cls in [1, 4, 224, 114] {
            assert!(
                is_filterable_subdoc_for_ext(cls, "prefab"),
                "class {cls} should be filtered for .prefab",
            );
        }
        // .playable: Timeline tracks live as MB-114 — must NOT filter.
        // GO/Transform never appear in .playable but the predicate stays
        // valid (no-op).
        assert!(!is_filterable_subdoc_for_ext(114, "playable"));
        assert!(is_filterable_subdoc_for_ext(1, "playable"));
        // .controller: AnimatorState (1102), BlendTree (206) — never
        // filtered.
        assert!(!is_filterable_subdoc_for_ext(1102, "controller"));
        assert!(!is_filterable_subdoc_for_ext(114, "controller"));
        // .mixer: AudioMixerGroup (273) — never filtered.
        assert!(!is_filterable_subdoc_for_ext(273, "mixer"));
        assert!(!is_filterable_subdoc_for_ext(114, "mixer"));
        // .asset / .spriteatlas: MB-114 are real ScriptableObject sub-
        // assets. Real classes (Sprite=213) are never filtered either.
        assert!(!is_filterable_subdoc_for_ext(114, "asset"));
        assert!(!is_filterable_subdoc_for_ext(213, "spriteatlas"));
    }

    #[test]
    fn stem_basic() {
        assert_eq!(filename_stem(Path::new("foo/Bar.prefab")), "Bar");
        assert_eq!(filename_stem_from_hint("foo/Bar.prefab"), "Bar");
    }

    #[test]
    fn parent_suffix_takes_last_two_parents() {
        // Standard case: hint has ≥ 2 parent segments; take the deepest 2,
        // joined with `/`.
        let alias = parent_suffix("Assets/UI/Prefabs/Button.prefab", "Button", 2).unwrap();
        assert_eq!(alias, "Button^UI/Prefabs");

        // Deeper path → still last 2 (independent of total depth).
        let alias = parent_suffix(
            "Assets/20_Contents/SettingsPopup/Prefabs/Button.prefab",
            "Button",
            2,
        )
        .unwrap();
        assert_eq!(alias, "Button^SettingsPopup/Prefabs");
    }

    #[test]
    fn parent_suffix_pads_when_fewer_parents_than_requested() {
        // `Assets/Foo.prefab` has exactly one parent segment (`Assets`).
        // The rule is "take last min_parents OR all available, whichever
        // is smaller" — never errors when at least one parent exists.
        let alias = parent_suffix("Assets/Foo.prefab", "Foo", 2).unwrap();
        assert_eq!(alias, "Foo^Assets");
    }

    #[test]
    fn parent_suffix_is_pure_function_of_hint() {
        // No `taken`, no `owner_guid`, no order dependence — same hint
        // always yields the same suffix. The whole point of the rewrite.
        let a = parent_suffix("Assets/A/B/Foo.prefab", "Foo", 2).unwrap();
        let b = parent_suffix("Assets/A/B/Foo.prefab", "Foo", 2).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "Foo^A/B");
    }

    #[test]
    fn guid_suffix_uses_first_8_hex_of_guid() {
        // `.cs` MonoScripts collide structurally (mirror-package vendoring:
        // UniTask/Runtime/Utils/L.cs vs Zenject/Runtime/Utils/L.cs). The
        // GUID-suffix rule sidesteps the path-based depth-2 ambiguity
        // entirely: alias is intrinsic to the asset (survives `git mv`).
        let guid = 0x9ddf5ad82f894638a9ba6a59eb87d508_u128;
        assert_eq!(guid_suffix("L", guid), "L^9ddf5ad8");

        let guid_b = 0x3751098bb0c541e296a07628e24fcb84_u128;
        assert_eq!(guid_suffix("L", guid_b), "L^3751098b");
    }

    #[test]
    fn parent_suffix_hard_fails_when_no_parent_segments() {
        // Hint is a bare filename — nothing to suffix with. Must error
        // rather than silently fall back to a guid suffix.
        let err =
            parent_suffix("Foo.cs", "Foo", 2).expect_err("must hard-fail with no parents");
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

    /// Pin: when a name is claimed by ≥2 distinct guids of the same
    /// `asset_type`, every claimant must rename — no "first wins" carve-out.
    /// Each entry's alias is `stem^<last 2 parents of hint>`, derived
    /// purely from its own hint (independent of iteration order or which
    /// siblings exist).
    ///
    /// Two same-type Texture2D `Cloud1.png` files in different folders
    /// share the bare alias `Cloud1` until type-aware dedup forces both to
    /// suffix.
    #[test]
    fn build_db_renames_every_claimant_when_name_is_contested() {
        let png_a_guid = 0xa0_u128;
        let png_b_guid = 0xb0_u128;
        let sprite_fid: i64 = 21300000;

        let raw = vec![
            raw_native("Assets/Other/Cloud1.png", png_a_guid, vec![]),
            raw_native(
                "Assets/Tower/Cloud1.png",
                png_b_guid,
                vec![SubAsset {
                    file_id: sprite_fid,
                    class_id: ClassId::Sprite as u32,
                    name: "Cloud1".into(),
                }],
            ),
        ];

        let db = build_db(raw, None, None, false).expect("build_db should succeed");

        let a_entry = db.find_by_guid(png_a_guid).unwrap();
        let b_entry = db.find_by_guid(png_b_guid).unwrap();

        // Deterministic depth-2 suffix from each entry's own hint. Each
        // hint has 2 parents (`Assets/Other`, `Assets/Tower`); depth-2
        // takes both.
        assert_eq!(&*a_entry.name, "Cloud1^Assets/Other");
        assert_eq!(&*b_entry.name, "Cloud1^Assets/Tower");

        // Sub-asset dedup: the Sprite sub-asset's `Cloud1` lives in its own
        // type-bucket (Sprite, not Texture2D), so it isn't contested by the
        // Texture2D collision above. It stays bare. The png_b entry is the
        // only Sprite-bucket owner.
        let png_b_sub = &b_entry.sub_assets[0];
        assert_eq!(png_b_sub.file_id, sprite_fid);
        assert_eq!(
            &*png_b_sub.name, "Cloud1",
            "Sprite sub-asset should stay bare under type-aware dedup",
        );
    }

    /// Pin the stability win: a contested entry's alias is a pure function
    /// of its own hint. Adding an unrelated third claimant to the same
    /// `(name, asset_type)` bucket does NOT shift the first two's aliases —
    /// each one is computed independently from its own depth-2 suffix.
    ///
    /// Pre-rewrite the order-dependent shortest-suffix rule made this fail:
    /// the new claimant could displace whichever sibling currently held
    /// the shorter form.
    #[test]
    fn build_db_contested_alias_is_independent_of_other_siblings() {
        let a_guid = 0xa0_u128;
        let b_guid = 0xb0_u128;
        let c_guid = 0xc0_u128;

        // Two-claimant bake.
        let raw_two = vec![
            raw_native("Assets/X/Y/Foo.prefab", a_guid, vec![]),
            raw_native("Assets/P/Q/Foo.prefab", b_guid, vec![]),
        ];
        let db_two = build_db(raw_two, None, None, false).unwrap();
        let a_name_two = db_two.find_by_guid(a_guid).unwrap().name.clone();
        let b_name_two = db_two.find_by_guid(b_guid).unwrap().name.clone();

        // Three-claimant bake — add an unrelated `Foo.prefab` deeper in
        // the tree. Under the old order-dependent rule it could rotate
        // which of {a, b} kept the shorter suffix.
        let raw_three = vec![
            raw_native("Assets/X/Y/Foo.prefab", a_guid, vec![]),
            raw_native("Assets/P/Q/Foo.prefab", b_guid, vec![]),
            raw_native("Assets/M/N/Foo.prefab", c_guid, vec![]),
        ];
        let db_three = build_db(raw_three, None, None, false).unwrap();
        let a_name_three = db_three.find_by_guid(a_guid).unwrap().name.clone();
        let b_name_three = db_three.find_by_guid(b_guid).unwrap().name.clone();

        // Aliases for the original two are byte-identical across both bakes.
        assert_eq!(&*a_name_two, &*a_name_three);
        assert_eq!(&*b_name_two, &*b_name_three);
        assert_eq!(&*a_name_two, "Foo^X/Y");
        assert_eq!(&*b_name_two, "Foo^P/Q");
    }

    /// Pin: contested `.cs` MonoScripts use a GUID-prefix suffix instead of
    /// depth-2 parent dirs. Sidesteps mirror-package collisions (UniTask
    /// vs Zenject both vendoring a `Runtime/Utils/L.cs` at the same depth-2
    /// path) where the path-based rule would hard-fail. The suffix is
    /// intrinsic to the asset (first 8 hex of GUID) — independent of
    /// directory layout, stable under `git mv`.
    #[test]
    fn build_db_uses_guid_suffix_for_contested_monoscripts() {
        let a_guid = 0x9ddf5ad82f894638a9ba6a59eb87d508_u128;
        let b_guid = 0x3751098bb0c541e296a07628e24fcb84_u128;
        let raw = vec![
            RawEntry {
                guid: a_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::MonoScript as u32),
                hint: "Packages/com.boxcat.libs/UniTask/Runtime/Utils/L.cs".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![],
            },
            RawEntry {
                guid: b_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::MonoScript as u32),
                hint: "Packages/com.boxcat.libs/Zenject/Runtime/Utils/L.cs".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, None, false).expect("build_db should succeed");
        assert_eq!(&*db.find_by_guid(a_guid).unwrap().name, "L^9ddf5ad8");
        assert_eq!(&*db.find_by_guid(b_guid).unwrap().name, "L^3751098b");
    }

    /// Pin: two contestants whose hints share the same depth-2 parent path
    /// produce identical aliases under the pure-suffix rule. The bake must
    /// hard-fail rather than fall back to a deeper suffix or pick a winner.
    /// Error message names BOTH hints + the asset type so the user can
    /// rename one in source.
    #[test]
    fn build_db_fails_when_two_contestants_share_depth_2_parent() {
        let a_guid = 0xa0_u128;
        let b_guid = 0xb0_u128;
        // Both hints end with `…/X/Y/Foo.png` — depth-2 suffix `X/Y` for both.
        let raw = vec![
            raw_native("Assets/X/Y/Foo.png", a_guid, vec![]),
            raw_native("Outer/X/Y/Foo.png", b_guid, vec![]),
        ];
        let err = build_db(raw, None, None, false)
            .expect_err("identical depth-2 suffix must hard-fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("Foo"), "msg should name the stem: {msg}");
        assert!(
            msg.contains("Assets/X/Y/Foo.png") && msg.contains("Outer/X/Y/Foo.png"),
            "msg should name both hints: {msg}",
        );
    }

    /// Pin type-aware dedup: a Texture2D and a Prefab sharing the stem
    /// `Foo` both keep the bare alias. Reverse lookup discriminates by
    /// the field's declared C# type at the consumer layer.
    #[test]
    fn build_db_keeps_bare_alias_for_type_distinct_collisions() {
        let png_guid = 0xa0_u128;
        let prefab_guid = 0xb0_u128;
        let raw = vec![
            RawEntry {
                guid: png_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::Texture2D as u32),
                hint: "Assets/UI/Foo.png".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![],
            },
            RawEntry {
                guid: prefab_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::Prefab as u32),
                hint: "Assets/UI/Foo.prefab".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, None, false).expect("build_db should succeed");
        // Both keep bare `Foo` because they live in distinct type buckets.
        assert_eq!(&*db.find_by_guid(png_guid).unwrap().name, "Foo");
        assert_eq!(&*db.find_by_guid(prefab_guid).unwrap().name, "Foo");
    }

    /// Pin: AnimatorController-embedded sub-assets are excluded from the
    /// global dedup pool, mirroring the prefab-embedded rule. Without the
    /// exclusion, an embedded AnimatorState named `Idle` would contest a
    /// hypothetical standalone `.asset` of the same name AND same Unity
    /// classID (AnimatorState exists as both an embedded sub of
    /// `.controller` and a top-level `.asset` in Unity), forcing both to
    /// rename via parent-dir suffix. The exclusion keeps the embedded
    /// state in its parent's namespace where it's addressed via
    /// `$Idle@Player` at the consumer layer.
    #[test]
    fn build_db_skips_controller_embedded_subassets_in_global_pool() {
        const ANIMATOR_STATE_CLASS_ID: u32 = 1102;
        let controller_guid = 0xc0_u128;
        let other_state_guid = 0xd0_u128;
        let raw = vec![
            RawEntry {
                guid: controller_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::AnimatorController as u32),
                hint: "Assets/Anim/Player.controller".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![SubAsset {
                    file_id: -123_456_789_012,
                    class_id: ANIMATOR_STATE_CLASS_ID,
                    name: "Idle".into(),
                }],
            },
            // Standalone .asset whose top class IS AnimatorState — same
            // (name, class_id) bucket as the embedded one. With
            // exclusion, only this one claims the global `Idle` alias.
            RawEntry {
                guid: other_state_guid,
                asset_type_raw: AssetTypeRaw::Native(ANIMATOR_STATE_CLASS_ID),
                hint: "Assets/Other/Idle.asset".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, None, false).expect("build_db should succeed");
        // Standalone keeps bare `Idle`.
        assert_eq!(&*db.find_by_guid(other_state_guid).unwrap().name, "Idle");
        // Embedded state stays as authored in the parent's namespace.
        let ctrl_entry = db.find_by_guid(controller_guid).unwrap();
        assert_eq!(&*ctrl_entry.sub_assets[0].name, "Idle");
    }

    /// Same shape as the controller test, for AudioMixerController:
    /// AudioMixerGroup sub-asset class collides with itself between an
    /// embedded `Main.mixer` group and a hypothetical standalone
    /// `.asset` of the same class. Exclusion keeps the embed in the
    /// parent's namespace.
    #[test]
    fn build_db_skips_mixer_embedded_subassets_in_global_pool() {
        const AUDIO_MIXER_GROUP_CLASS_ID: u32 = 273;
        let mixer_guid = 0xe0_u128;
        let other_group_guid = 0xf0_u128;
        let raw = vec![
            RawEntry {
                guid: mixer_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::AudioMixerController as u32),
                hint: "Assets/Audio/Main.mixer".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![SubAsset {
                    file_id: 9_001,
                    class_id: AUDIO_MIXER_GROUP_CLASS_ID,
                    name: "Master".into(),
                }],
            },
            RawEntry {
                guid: other_group_guid,
                asset_type_raw: AssetTypeRaw::Native(AUDIO_MIXER_GROUP_CLASS_ID),
                hint: "Assets/Other/Master.asset".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, None, false).expect("build_db should succeed");
        assert_eq!(&*db.find_by_guid(other_group_guid).unwrap().name, "Master");
        let mixer_entry = db.find_by_guid(mixer_guid).unwrap();
        assert_eq!(&*mixer_entry.sub_assets[0].name, "Master");
    }

    /// Pin: `.playable` files are treated as embedded containers — their
    /// Timeline track sub-assets bypass the global dedup pool. Many
    /// `.playable` files in a project share Unity-default track names like
    /// `Animation Track (2)`; without the exclusion they contest in the
    /// global pool and `disambiguate` hard-fails when the shared
    /// parent-dir suffixes are exhausted. Exclusion is keyed on the
    /// `.playable` extension (`is_embedded_container`) because the
    /// top-doc script guid of a playable is whichever sub-doc Unity
    /// sorts first by hashed fileID — unstable as a discriminator.
    #[test]
    fn build_db_skips_playable_embedded_tracks_in_global_pool() {
        // Track class id + script guid placeholders — bake stores both
        // but doesn't validate them against any registry. Extension is
        // the discriminator that triggers the exclusion.
        const ANIMATION_TRACK_CLASS_ID: u32 = 5004;
        let some_script_guid = 0xd21dcc2386d650c4597f3633c75a1f98_u128;
        let pa_guid = 0xa0_u128;
        let pb_guid = 0xb0_u128;
        let raw = vec![
            RawEntry {
                guid: pa_guid,
                asset_type_raw: AssetTypeRaw::Script(some_script_guid),
                hint: "Assets/Anim/PlayableA.playable".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![SubAsset {
                    file_id: -123_456_789,
                    class_id: ANIMATION_TRACK_CLASS_ID,
                    name: "Animation Track (2)".into(),
                }],
            },
            RawEntry {
                guid: pb_guid,
                asset_type_raw: AssetTypeRaw::Script(some_script_guid),
                hint: "Assets/Anim/PlayableB.playable".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![SubAsset {
                    file_id: -987_654_321,
                    class_id: ANIMATION_TRACK_CLASS_ID,
                    name: "Animation Track (2)".into(),
                }],
            },
        ];
        let db = build_db(raw, None, None, false).expect("build_db should succeed");
        // Both playables keep their embedded track names as authored —
        // sub-assets live in the parent's namespace, not the global pool.
        assert_eq!(
            &*db.find_by_guid(pa_guid).unwrap().sub_assets[0].name,
            "Animation Track (2)"
        );
        assert_eq!(
            &*db.find_by_guid(pb_guid).unwrap().sub_assets[0].name,
            "Animation Track (2)"
        );
    }

    /// Pin: prefab-embedded sub-assets are excluded from the global dedup
    /// pool. Their names stay as authored even when another asset in the
    /// project shares the name. They resolve via `$Sub@Parent` at the
    /// consumer layer, not the global alias bucket.
    #[test]
    fn build_db_skips_prefab_embedded_subassets_in_global_pool() {
        let prefab_guid = 0xa0_u128;
        let other_clip_guid = 0xb0_u128;
        let raw = vec![
            RawEntry {
                guid: prefab_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::Prefab as u32),
                hint: "Assets/UI/PatternBG.prefab".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![SubAsset {
                    file_id: -4_468_419_427_481_386_445,
                    class_id: ClassId::AnimationClip as u32,
                    name: "Animation".into(),
                }],
            },
            RawEntry {
                guid: other_clip_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::AnimationClip as u32),
                hint: "Assets/Other/Animation.anim".to_string(),
                name: String::new(),
                meta_mtime_ns: 0,
                asset_mtime_ns: 0,
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, None, false).expect("build_db should succeed");
        // Standalone .anim keeps bare `Animation` — the prefab-embedded
        // `Animation` doesn't claim the global alias.
        assert_eq!(
            &*db.find_by_guid(other_clip_guid).unwrap().name,
            "Animation"
        );
        // Prefab-embedded sub-asset keeps its raw name (lives in parent's
        // namespace; `$Animation@PatternBG` at the consumer layer).
        let prefab_entry = db.find_by_guid(prefab_guid).unwrap();
        assert_eq!(&*prefab_entry.sub_assets[0].name, "Animation");
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
                class_id: ClassId::Sprite as u32,
                name: "Lone".into(),
            }],
        )];

        let db = build_db(raw, None, None, false).expect("build_db should succeed");
        let entry = db.find_by_guid(png_guid).unwrap();
        assert_eq!(&*entry.name, "Lone");
        assert_eq!(&*entry.sub_assets[0].name, "Lone");
    }

    /// Pin: when a top-level alias is genuinely unresolvable (contested
    /// entries with no parent segments to suffix with), the bake hard-fails
    /// rather than silently falling back to a `^<guid8>` suffix. Per the
    /// project policy: ambiguity surfaces at bake time, not encode time.
    #[test]
    fn build_db_fails_when_dedup_cannot_resolve() {
        let raw = vec![
            // Two top-level entries with the same bare stem and no parent
            // segments to walk — `parent_suffix` has nothing to attach.
            raw_native("Foo.asset", 0x01_u128, vec![]),
            raw_native("Foo.prefab", 0x02_u128, vec![]),
        ];

        let err = build_db(raw, None, None, false)
            .expect_err("collision with no parent dirs must hard-fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Foo") && msg.contains("disambiguate"),
            "error message should name the collision and the dedup pass: {msg}",
        );
    }
}
