//! Bake orchestrator: walk → parse → write.
//!
//! Per-file flow:
//! 1. Read `.meta` → guid + sprite-sheet sub-assets.
//! 2. Read the asset file → top-level class ID + sub-asset rows.
//! 3. Resolve `AssetType`: native `class_id` or `Script(script_guid)`.
//! 4. Derive alias from the filename stem.
//!
//! Post-walk: alias-collision sweep (filename stems can clash; we suffix
//! with parent dir on conflict and warn).
//!
//! Note: the mtime-based `asset-db.cache.bin` was retired in schema v7
//! (see `store.rs`). Cache invalidation moves to Watchman via
//! [`crate::refresh`] + [`crate::watch`]; see `docs/refresh.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use ahash::{AHashMap, AHashSet};

use anyhow::{Context, Result};

use crate::asset;
use crate::class_id::{ClassId, class_from_ext};
use crate::guid::Guid;
use crate::meta::{self, SPRITE_MODE_SINGLE, TEXTURE_TYPE_SPRITE};
use crate::store::{self, AssetDb, AssetEntry, AssetType, StoreError, SubAsset, DB_FILENAME};
use crate::register::{generate_guid, importer_for_path, render_meta};
use crate::walk::{walk_for_missing_meta, walk_meta_files, WalkError};

/// Errors from a bake run.
///
/// `Store(StoreError)` and `Walk(WalkError)` surface the typed source
/// errors from those modules — match on them when you need to
/// distinguish (e.g. "is this a schema-mismatch that needs re-bake?").
/// `Other` carries the remaining anyhow-chained errors (dedup
/// hard-fails, duplicate-guid checks) — most consumers propagate
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

/// Caller-supplied warning sink. Bake invokes this for non-fatal events
/// (worker errors during the parallel walk, name-collision rewrites).
/// The library never writes to stderr itself.
pub type WarnSink = Box<dyn Fn(&str) + Send + Sync + 'static>;

/// Caller-supplied progress sink. Bake invokes this with the post-bake
/// summary line and (when `BakeOptions::verbose_timing` is true) with
/// per-phase timing. Separate from [`WarnSink`] so consumers can route
/// "info" output and warnings to different places.
pub type ProgressSink = Box<dyn Fn(&str) + Send + Sync + 'static>;

/// Borrowed view of a [`WarnSink`].
type WarnSinkRef<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// Borrowed view of a [`ProgressSink`].
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

/// One raw bake result, before name dedup. `script_guid` is the unmapped
/// GUID for MonoBehaviour assets — interning happens after the walk so we
/// only need one final sort.
#[derive(Clone)]
pub(crate) struct RawEntry {
    pub(crate) guid: Guid,
    pub(crate) asset_type_raw: AssetTypeRaw,
    pub(crate) hint: String,
    pub(crate) name: String,
    pub(crate) sub_assets: Vec<SubAsset>,
}

/// Hashable type discriminator: `Native(classID)` for built-in classes
/// and `Script(scriptGuid)` for MonoBehaviour-backed assets. Hashable so
/// the dedup pass can bucket by `(name, asset_type)` without depending
/// on the post-walk script-intern table.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AssetTypeRaw {
    Native(u32),
    Script(Guid),
}

/// Public, dedup-free view of a single parsed asset — what [`parse_one`]
/// returns. Script GUIDs are unmapped; caller calls
/// [`crate::store::AssetDb::intern_script`].
#[derive(Debug, Clone)]
pub struct ParsedEntry {
    pub guid: Guid,
    pub asset_type: ParsedAssetType,
    pub hint: String,
    pub sub_assets: Vec<SubAsset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedAssetType {
    Native(u32),
    Script(Guid),
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
    /// Directory where `asset-db.bin` is written. Caller composes the
    /// convention (e.g. `<project>/Library/unity-assetdb/` or a
    /// fixture-staging path).
    pub out_dir: PathBuf,
    /// Optional warning sink; see [`WarnSink`]. `None` discards warnings.
    pub on_warn: Option<WarnSink>,
    /// Optional progress sink; see [`ProgressSink`]. `None` discards the
    /// summary line.
    pub on_progress: Option<ProgressSink>,
    /// When true, [`on_progress`] also receives a per-phase timing line
    /// (prepass / walk / build / write). Env-var-driven behavior is the
    /// consumer's call.
    pub verbose_timing: bool,
    /// When true, [`on_warn`] receives a line for each name-collision
    /// rewrite during dedup. Off by default to keep steady-state warm
    /// bakes quiet.
    pub verbose_collisions: bool,
}

/// Bake entry-point. Walks `Assets/`, parses `.meta` + asset YAML,
/// captures a fresh Watchman clock (if available), writes
/// `<out_dir>/asset-db.bin`.
///
/// Always does a full walk — incremental updates go through
/// [`crate::refresh::refresh`]. There is no cache file to thread
/// through; staleness is exclusively driven by Watchman.
pub fn bake(opts: &BakeOptions) -> Result<(), BakeError> {
    bake_inner(opts).map_err(map_bake_err)
}

fn bake_inner(opts: &BakeOptions) -> Result<()> {
    let project_root = &opts.project_root;
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("create out-dir: {}", opts.out_dir.display()))?;
    let db_file = opts.out_dir.join(DB_FILENAME);

    // Advisory flock on `<out_dir>/.asset-db.lock` — shared with
    // `register` so concurrent invocations don't clobber the bin.
    // `refresh::refresh` does NOT take this lock on its patch path; a
    // lost-update race between two concurrent refreshes converges on
    // the next query (Watchman replays the delta), so the cost of an
    // unsynchronized write is benign and the simpler unlocked path
    // wins. Don't introduce a lock here without checking
    // `refresh::full_bake_into` first — it calls `bake::bake()` which
    // takes this lock with `LockWait::Forever`, so any wrapping lock
    // would deadlock the fallback path.

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
    let t_setup = t_start.elapsed();

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
    let walked = Arc::new(AtomicUsize::new(0));
    let project_root_arc: Arc<PathBuf> = Arc::new(project_root.clone());

    walk_meta_files(project_root, || {
        let raw_tx = raw_tx.clone();
        let err_tx = err_tx.clone();
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
                process_one(meta_path, &project_root)
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

    let mut raw: Vec<RawEntry> = Vec::with_capacity(2048);
    for v in raw_rx.iter() {
        raw.extend(v);
    }
    let mut db = build_db(raw, opts.on_warn.as_deref(), opts.verbose_collisions)?;
    let t_build = t_start.elapsed();

    // Seed the Watchman clock so the next `refresh` can ask for a
    // delta. We treat any failure here as "no clock available" — the
    // bake itself succeeds; the next refresh just full-bakes again.
    // The clock comes from the `Fresh` variant of a `since(None)` call
    // (Watchman always returns Fresh on a None cursor).
    db.watchman_clock = match crate::watch::since(project_root, None) {
        Ok(crate::watch::Delta::Fresh { new_clock }) => Some(new_clock),
        Ok(crate::watch::Delta::Touched { new_clock, .. }) => Some(new_clock),
        Err(crate::watch::WatchError::Unavailable) => {
            // Surface the same one-line nudge `refresh` emits so a
            // fresh `bake` against a watchman-less box doesn't quietly
            // ship a clock-less bin that the next query has to re-bake.
            if let Some(sink) = opts.on_warn.as_ref() {
                sink("watchman unavailable; install (brew install watchman) for incremental updates");
            }
            None
        }
        Err(crate::watch::WatchError::Query(e)) => {
            if let Some(sink) = opts.on_warn.as_ref() {
                sink(&format!("watchman query failed during bake clock seed: {e}"));
            }
            None
        }
    };

    store::write(&db_file, &db)
        .with_context(|| format!("write asset-db: {}", db_file.display()))?;
    let t_write = t_start.elapsed();

    if let Some(sink) = opts.on_progress.as_ref() {
        sink(&format!(
            "baked {} entries → {}",
            db.entries.len(),
            db_file.display()
        ));
        if opts.verbose_timing {
            let walked_n = walked.load(Ordering::Relaxed);
            sink(&format!(
                "  walked={walked_n} | prepass={dt_prepass:?} setup={t_setup:?} walk={:?} build={:?} write={:?} total={:?}",
                t_walk - t_setup,
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
    let body = render_meta(&guid.to_string(), kind, is_dir);
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
    Ok(parse_one_raw(project_root, meta_path)?.map(raw_to_parsed))
}

/// Like [`parse_one`] but returns the internal [`RawEntry`] shape that
/// the post-walk dedup pipeline ([`build_db_from_raw`]) consumes.
/// Pre-dedup, pre-script-intern — caller is responsible for both.
///
/// Used by [`crate::refresh::patch`] to re-parse a single touched hint
/// and splice it into an existing `AssetDb` via `build_db_from_raw`.
pub(crate) fn parse_one_raw(
    project_root: &Path,
    meta_path: &Path,
) -> Result<Option<RawEntry>, BakeError> {
    process_one(meta_path, project_root).map_err(map_bake_err)
}

/// Lift an already-baked [`AssetEntry`] back into the pre-dedup
/// [`RawEntry`] shape. Used by [`crate::refresh::patch`] to feed surviving
/// db entries through [`build_db_from_raw`] alongside freshly-parsed
/// hints — the dedup machinery then re-deduplicates the union.
///
/// `script_types` is the `AssetDb::script_types` table the entry's
/// `AssetType::Script(idx)` variant indexes into; needed to recover the
/// raw u128 script GUID for the `AssetTypeRaw::Script(g)` discriminator.
/// Convert a stored `AssetEntry` back to raw form for re-processing
/// through `build_db`. Strips collision suffixes (`^…`) from sub-asset
/// names so `build_db` sees only raw authored names and can validate +
/// re-apply dedup from scratch. Top-level names are reset from hints
/// inside `build_db`; sub-asset names have no external source, so this
/// is the only place to restore them.
pub(crate) fn raw_from_entry(entry: &AssetEntry, script_types: &[Guid]) -> RawEntry {
    let asset_type_raw = match entry.asset_type {
        AssetType::Native(n) => AssetTypeRaw::Native(n),
        AssetType::Script(idx) => AssetTypeRaw::Script(script_types[idx as usize]),
    };
    let sub_assets: Vec<SubAsset> = entry
        .sub_assets
        .iter()
        .map(|s| SubAsset {
            file_id: s.file_id,
            class_id: s.class_id,
            name: strip_collision_suffix(&s.name),
        })
        .collect();
    RawEntry {
        guid: entry.guid,
        asset_type_raw,
        hint: entry.hint.to_string(),
        name: entry.name.to_string(),
        sub_assets,
    }
}

/// Public wrapper around the post-walk dedup/build pipeline. Same
/// semantics as the full bake's post-walk phase: sort raw entries by
/// hint, dedupe names with type-aware collision suffixes, intern
/// script GUIDs, return a sorted [`AssetDb`].
///
/// Sinks default to silent — refresh's patch path doesn't surface
/// dedup warnings (steady-state should be no-op).
pub(crate) fn build_db_from_raw(raw: Vec<RawEntry>) -> Result<AssetDb, BakeError> {
    build_db(raw, None, false).map_err(map_bake_err)
}

fn raw_to_parsed(r: RawEntry) -> ParsedEntry {
    ParsedEntry {
        guid: r.guid,
        asset_type: match r.asset_type_raw {
            AssetTypeRaw::Native(n) => ParsedAssetType::Native(n),
            AssetTypeRaw::Script(g) => ParsedAssetType::Script(g),
        },
        hint: r.hint,
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
/// to describe (e.g. orphaned `.meta`, directory `.meta`). Shared by the
/// bake worker loop and the single-file [`parse_one_raw`] path.
fn process_one(meta_path: &Path, project_root: &Path) -> Result<Option<RawEntry>> {
    let companion =
        strip_meta_suffix(meta_path).ok_or_else(|| anyhow::anyhow!("not a .meta path"))?;
    let hint = rel_hint(project_root, &companion)?;
    let Ok(companion_md) = std::fs::metadata(&companion) else {
        return Ok(None);
    };
    if companion_md.is_dir() {
        return Ok(None);
    }
    parse_meta_and_asset(meta_path, &companion, &hint)
}

/// Parse `.meta` + asset YAML into a [`RawEntry`]. Split from
/// [`process_one`] for readability; the orphan/dir guards stay in
/// [`process_one`] so this body is purely the parse step.
fn parse_meta_and_asset(
    meta_path: &Path,
    companion: &Path,
    hint: &str,
) -> Result<Option<RawEntry>> {
    let meta_text = std::fs::read_to_string(meta_path)
        .with_context(|| format!("read .meta: {}", meta_path.display()))?;
    let meta_info = meta::parse(&meta_text)?;

    let ext = companion.extension().and_then(|s| s.to_str()).unwrap_or("");
    let from_ext = class_from_ext(ext);

    let mut sub_assets: Vec<SubAsset> = Vec::new();
    let mut top_class_id: Option<u32> = None;
    let mut script_guid: Option<Guid> = None;

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
        // Empty `m_Name` is fine — Unity-addressable sub-docs (Mesh /
        // Curve / generated-content bodies inside `.asset`) routinely
        // ship with no authored name. Consumers address them via
        // `$@Parent#<fid>` ("Embedded sub-asset, unnamed"); they need
        // the (parent_guid, file_id) entry in the index even without a
        // name to round-trip. The structural `is_filterable_subdoc_for_ext`
        // gate (GO/Transform/RectTransform on prefabs) still applies.
        for s in info.sub_assets {
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

/// Strip the bake-generated collision suffix (`^…`) from a sub-asset
/// name, returning the raw authored form. Called by `raw_from_entry`
/// to restore stored names before feeding them back through `build_db`.
fn strip_collision_suffix(name: &str) -> Box<str> {
    match name.find('^') {
        Some(i) => name[..i].into(),
        None => Box::from(name),
    }
}

/// Hard-fail on reserved chars in any asset / sub-asset name.
///
/// - `/` — path separator + downstream ref-grammar delimiter.
/// - `^` — collision-suffix separator (`stem^suffix`); an authored `^`
///   would be silently stripped by [`strip_collision_suffix`] on the
///   refresh round-trip, corrupting the name.
///
/// `kind` is "asset" for top-level, "sub-asset of" for embedded —
/// surfaces in the error message so the user can locate the source.
fn reject_reserved(name: &str, kind: &str, hint: &str) -> Result<()> {
    for ch in ['/', '^'] {
        if name.contains(ch) {
            anyhow::bail!(
                "{kind} `{hint}` has name `{name}` containing `{ch}`; \
                 reserved character in the asset-db naming scheme. \
                 Fix the source YAML (sub-asset names come from `m_Name`).",
            );
        }
    }
    Ok(())
}

fn build_db(
    mut raw: Vec<RawEntry>,
    on_warn: Option<WarnSinkRef<'_>>,
    verbose_collisions: bool,
) -> Result<AssetDb> {
    // Stable order: sort by hint so dedup picks the same "winner" each bake.
    raw.sort_by(|a, b| a.hint.cmp(&b.hint));

    // Reset top-level names from hints; validate all names for reserved
    // chars. Sub-asset names arrive pre-stripped by `raw_from_entry` on
    // the refresh path, so `build_db` only ever sees raw authored names.
    for r in raw.iter_mut() {
        r.name = filename_with_ext_from_hint(&r.hint);
        reject_reserved(&r.name, "asset", &r.hint)?;
        for sub in &r.sub_assets {
            reject_reserved(&sub.name, "sub-asset of", &r.hint)?;
        }
    }

    // Type-aware dedup: collisions are scoped by `(name, asset_type)`.
    // Same-name entries of distinct `asset_type` (`Foo.png` Texture2D +
    // `Foo.prefab` Prefab) get distinct alias buckets — the consuming
    // field's C# type discriminates at decode. Embedded sub-asset docs
    // of container types are excluded from the global pool entirely
    // (see [Name collisions](docs/asset-database.md#name-collisions)).

    // Pass 1: tally distinct-guid owners per `(name, asset_type)` bucket.
    let mut owners: AHashMap<(String, AssetTypeRaw), AHashSet<Guid>> =
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
    let mut taken: AHashMap<(String, AssetTypeRaw), (Guid, String)> =
        AHashMap::with_capacity(raw.len());

    for r in raw.iter_mut() {
        let top_type = r.asset_type_raw;
        if contested(&r.name, top_type) {
            let new_name = collision_suffix(top_type, &r.hint, &r.name, r.guid)?;
            if verbose_collisions && let Some(sink) = on_warn {
                sink(&format!(
                    "warning: name collision on `{}` (guid {}); renamed to `{}`",
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
            // Empty-named sub-assets bypass the global dedup pool. Real
            // Unity-addressable sub-docs (Mesh / Curve / generated-content
            // bodies inside `.asset`) routinely ship with no authored
            // name; consumers address them via `$@Parent#<fid>` ("Embedded
            // sub-asset, unnamed") — `(parent_guid, fid)` is the lookup
            // key, the empty name is not. Forcing them through dedup
            // would collide across parents (every Box_*.asset has 30+
            // empty-named Meshes) and hit the no-suffix-can-disambiguate
            // bail in `collision_suffix`. Empty-named subs still land in
            // the parent's `sub_assets` vec — that's what
            // `is_local_subasset(parent_guid, fid)` checks downstream.
            if sub.name.is_empty() {
                continue;
            }
            let sub_type = AssetTypeRaw::Native(sub.class_id);
            if contested(&sub.name, sub_type) {
                let original = sub.name.to_string();
                let new_name = collision_suffix(sub_type, &r.hint, &original, r.guid)?;
                if verbose_collisions && let Some(sink) = on_warn {
                    sink(&format!(
                        "warning: sub-asset name collision on `{}` (parent guid {}); renamed to `{}`",
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
                "duplicate top-level GUID: {} between names `{}` and `{}` — likely two .meta files share a GUID",
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
                    "duplicate sub-asset record: name={} guid={} fileID={} type={:?}",
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

/// `<stem>.<ext>` for a project-rel hint, or bare `<stem>` if no
/// extension. Canonical top-level alias shape — see
/// `docs/asset-database.md#name-collisions`. Public so `register` can
/// mint names matching the next full bake.
pub fn filename_with_ext_from_hint(hint: &str) -> String {
    let p = Path::new(hint);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    match p.extension().and_then(|s| s.to_str()) {
        Some(ext) if !ext.is_empty() => format!("{stem}.{ext}"),
        _ => stem.to_string(),
    }
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
        AssetTypeRaw::Script(g) => format!("Script:{g}"),
    }
}

/// Post-hoc dedup-pool claim. Inserts `(name, asset_type) → (guid, hint)`
/// into `taken`; tolerates same-guid re-claims (a sub-asset sharing the
/// parent's deduped alias). Bails when a distinct-guid claim collides —
/// the recorded hint of the prior claimant feeds the error so the user
/// sees both colliding paths.
fn claim(
    taken: &mut AHashMap<(String, AssetTypeRaw), (Guid, String)>,
    name: &str,
    t: AssetTypeRaw,
    guid: Guid,
    hint: &str,
) -> Result<()> {
    match taken.get(&(name.to_string(), t)) {
        Some((prev_guid, _)) if *prev_guid == guid => Ok(()),
        Some((prev_guid, prev_hint)) => anyhow::bail!(
            "asset-db: cannot disambiguate name `{name}` (asset_type {ty}) — \
             two assets share the same depth-{MIN_PARENTS} parent suffix:\n  \
             {prev_hint} (guid {prev_guid})\n  \
             {hint} (guid {guid})\n\
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
fn collision_suffix(t: AssetTypeRaw, hint: &str, stem: &str, guid: Guid) -> Result<String> {
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
/// `^` is the same separator [`parent_suffix`] uses; both code paths
/// flag the alias as bake-disambiguated rather than authored, and the
/// character is rare enough in real Unity asset paths that it's safe to
/// surface in `$Alias` refs without confusing readers (unlike `_`,
/// which is common in filenames).
///
/// 8-hex collisions across two distinct GUIDs are exceptionally rare;
/// when they do happen, [`claim`] still hard-fails — the user can
/// regenerate one of the colliding script GUIDs to resolve.
fn guid_suffix(stem: &str, guid: Guid) -> String {
    let hex = guid.to_string();
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
/// `^` is a rare char in real Unity asset paths and visually flags the
/// alias as bake-added rather than authored. Folder names with
/// pspec-ref-grammar reserved chars (`!`, `|`, `@`, `#`) pass through
/// verbatim here; pspec's `validate_asset_alias_for_ref` catches them
/// lazily at ref-compose time so editor-only assets nothing references
/// stay harmless.
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
            guid: Guid::from_u128(0),
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
        assert_eq!(filename_with_ext_from_hint("foo/Bar.prefab"), "Bar.prefab");
        assert_eq!(filename_with_ext_from_hint("foo/Bar"), "Bar");
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
        let guid = Guid::from_u128(0x9ddf5ad82f894638a9ba6a59eb87d508_u128);
        assert_eq!(guid_suffix("L", guid), "L^9ddf5ad8");

        let guid_b = Guid::from_u128(0x3751098bb0c541e296a07628e24fcb84_u128);
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

    fn raw_native(hint: &str, guid: Guid, sub_assets: Vec<SubAsset>) -> RawEntry {
        RawEntry {
            guid,
            asset_type_raw: AssetTypeRaw::Native(ClassId::Texture2D as u32),
            hint: hint.to_string(),
            // `build_db`'s first pass overwrites `name` from `hint`, so any
            // value here is fine. Empty kept the test minimal.
            name: String::new(),
            
            
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
        let png_a_guid = Guid::from_u128(0xa0_u128);
        let png_b_guid = Guid::from_u128(0xb0_u128);
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

        let db = build_db(raw, None, false).expect("build_db should succeed");

        let a_entry = db.find_by_guid(png_a_guid).unwrap();
        let b_entry = db.find_by_guid(png_b_guid).unwrap();

        // Always-ext top-level name (`Cloud1.png`) plus deterministic
        // depth-2 suffix from each entry's own hint. Each hint has 2
        // parents (`Assets/Other`, `Assets/Tower`); depth-2 takes both.
        assert_eq!(&*a_entry.name, "Cloud1.png^Assets/Other");
        assert_eq!(&*b_entry.name, "Cloud1.png^Assets/Tower");

        // Sub-asset dedup: the Sprite sub-asset's `Cloud1` lives in its own
        // type-bucket (Sprite, not Texture2D) and never grows an ext suffix
        // — sub-assets carry no file extension of their own. It stays bare.
        let png_b_sub = &b_entry.sub_assets[0];
        assert_eq!(png_b_sub.file_id, sprite_fid);
        assert_eq!(
            &*png_b_sub.name, "Cloud1",
            "Sprite sub-asset stays bare; ext suffix is a top-level-only marker",
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
        let a_guid = Guid::from_u128(0xa0_u128);
        let b_guid = Guid::from_u128(0xb0_u128);
        let c_guid = Guid::from_u128(0xc0_u128);

        // Two-claimant bake.
        let raw_two = vec![
            raw_native("Assets/X/Y/Foo.prefab", a_guid, vec![]),
            raw_native("Assets/P/Q/Foo.prefab", b_guid, vec![]),
        ];
        let db_two = build_db(raw_two, None, false).unwrap();
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
        let db_three = build_db(raw_three, None, false).unwrap();
        let a_name_three = db_three.find_by_guid(a_guid).unwrap().name.clone();
        let b_name_three = db_three.find_by_guid(b_guid).unwrap().name.clone();

        // Aliases for the original two are byte-identical across both bakes.
        assert_eq!(&*a_name_two, &*a_name_three);
        assert_eq!(&*b_name_two, &*b_name_three);
        assert_eq!(&*a_name_two, "Foo.prefab^X/Y");
        assert_eq!(&*b_name_two, "Foo.prefab^P/Q");
    }

    /// Pin: contested `.cs` MonoScripts use a GUID-prefix suffix instead of
    /// depth-2 parent dirs. Sidesteps mirror-package collisions (UniTask
    /// vs Zenject both vendoring a `Runtime/Utils/L.cs` at the same depth-2
    /// path) where the path-based rule would hard-fail. The suffix is
    /// intrinsic to the asset (first 8 hex of GUID) — independent of
    /// directory layout, stable under `git mv`.
    #[test]
    fn build_db_uses_guid_suffix_for_contested_monoscripts() {
        let a_guid = Guid::from_u128(0x9ddf5ad82f894638a9ba6a59eb87d508_u128);
        let b_guid = Guid::from_u128(0x3751098bb0c541e296a07628e24fcb84_u128);
        let raw = vec![
            RawEntry {
                guid: a_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::MonoScript as u32),
                hint: "Packages/com.boxcat.libs/UniTask/Runtime/Utils/L.cs".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
            RawEntry {
                guid: b_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::MonoScript as u32),
                hint: "Packages/com.boxcat.libs/Zenject/Runtime/Utils/L.cs".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, false).expect("build_db should succeed");
        assert_eq!(&*db.find_by_guid(a_guid).unwrap().name, "L.cs^9ddf5ad8");
        assert_eq!(&*db.find_by_guid(b_guid).unwrap().name, "L.cs^3751098b");
    }

    /// Pin: two contestants whose hints share the same depth-2 parent path
    /// produce identical aliases under the pure-suffix rule. The bake must
    /// hard-fail rather than fall back to a deeper suffix or pick a winner.
    /// Error message names BOTH hints + the asset type so the user can
    /// rename one in source.
    #[test]
    fn build_db_fails_when_two_contestants_share_depth_2_parent() {
        let a_guid = Guid::from_u128(0xa0_u128);
        let b_guid = Guid::from_u128(0xb0_u128);
        // Both hints end with `…/X/Y/Foo.png` — depth-2 suffix `X/Y` for both.
        let raw = vec![
            raw_native("Assets/X/Y/Foo.png", a_guid, vec![]),
            raw_native("Outer/X/Y/Foo.png", b_guid, vec![]),
        ];
        let err = build_db(raw, None, false)
            .expect_err("identical depth-2 suffix must hard-fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("Foo"), "msg should name the stem: {msg}");
        assert!(
            msg.contains("Assets/X/Y/Foo.png") && msg.contains("Outer/X/Y/Foo.png"),
            "msg should name both hints: {msg}",
        );
    }

    /// Pin always-ext: a Texture2D and a Prefab sharing the stem `Foo`
    /// land in distinct buckets under the `<stem>.<ext>` naming rule
    /// (`Foo.png` vs `Foo.prefab`) — neither contests, both uncontested-
    /// bare-alias-with-ext. Consumer no longer needs to discriminate by
    /// C# field type to disambiguate cross-kind same-stem cases.
    #[test]
    fn build_db_keeps_bare_alias_for_type_distinct_collisions() {
        let png_guid = Guid::from_u128(0xa0_u128);
        let prefab_guid = Guid::from_u128(0xb0_u128);
        let raw = vec![
            RawEntry {
                guid: png_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::Texture2D as u32),
                hint: "Assets/UI/Foo.png".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
            RawEntry {
                guid: prefab_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::Prefab as u32),
                hint: "Assets/UI/Foo.prefab".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, false).expect("build_db should succeed");
        // Each entry's alias is `<stem>.<ext>`; distinct exts → distinct
        // buckets → both uncontested.
        assert_eq!(&*db.find_by_guid(png_guid).unwrap().name, "Foo.png");
        assert_eq!(&*db.find_by_guid(prefab_guid).unwrap().name, "Foo.prefab");
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
        let controller_guid = Guid::from_u128(0xc0_u128);
        let other_state_guid = Guid::from_u128(0xd0_u128);
        let raw = vec![
            RawEntry {
                guid: controller_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::AnimatorController as u32),
                hint: "Assets/Anim/Player.controller".to_string(),
                name: String::new(),
                
                
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
                
                
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, false).expect("build_db should succeed");
        // Standalone gets `Idle.asset` under the always-ext rule.
        assert_eq!(
            &*db.find_by_guid(other_state_guid).unwrap().name,
            "Idle.asset",
        );
        // Embedded state stays as authored in the parent's namespace —
        // sub-assets don't carry an ext (no own file on disk).
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
        let mixer_guid = Guid::from_u128(0xe0_u128);
        let other_group_guid = Guid::from_u128(0xf0_u128);
        let raw = vec![
            RawEntry {
                guid: mixer_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::AudioMixerController as u32),
                hint: "Assets/Audio/Main.mixer".to_string(),
                name: String::new(),
                
                
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
                
                
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, false).expect("build_db should succeed");
        assert_eq!(
            &*db.find_by_guid(other_group_guid).unwrap().name,
            "Master.asset",
        );
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
        let some_script_guid = Guid::from_u128(0xd21dcc2386d650c4597f3633c75a1f98_u128);
        let pa_guid = Guid::from_u128(0xa0_u128);
        let pb_guid = Guid::from_u128(0xb0_u128);
        let raw = vec![
            RawEntry {
                guid: pa_guid,
                asset_type_raw: AssetTypeRaw::Script(some_script_guid),
                hint: "Assets/Anim/PlayableA.playable".to_string(),
                name: String::new(),
                
                
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
                
                
                sub_assets: vec![SubAsset {
                    file_id: -987_654_321,
                    class_id: ANIMATION_TRACK_CLASS_ID,
                    name: "Animation Track (2)".into(),
                }],
            },
        ];
        let db = build_db(raw, None, false).expect("build_db should succeed");
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
        let prefab_guid = Guid::from_u128(0xa0_u128);
        let other_clip_guid = Guid::from_u128(0xb0_u128);
        let raw = vec![
            RawEntry {
                guid: prefab_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::Prefab as u32),
                hint: "Assets/UI/PatternBG.prefab".to_string(),
                name: String::new(),
                
                
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
                
                
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, false).expect("build_db should succeed");
        // Standalone .anim gets `Animation.anim` — the prefab-embedded
        // `Animation` sub-asset is excluded from the global pool and
        // never contests.
        assert_eq!(
            &*db.find_by_guid(other_clip_guid).unwrap().name,
            "Animation.anim",
        );
        // Prefab-embedded sub-asset keeps its raw name (lives in parent's
        // namespace; `$Animation@PatternBG.prefab` at the consumer layer).
        let prefab_entry = db.find_by_guid(prefab_guid).unwrap();
        assert_eq!(&*prefab_entry.sub_assets[0].name, "Animation");
    }

    /// Pin: a single-owner name (one guid only, even if it appears as both
    /// a top-level alias and one of its own sub-assets) is *not*
    /// contested — it stays bare. Guards against over-renaming the common
    /// case of a Texture2D and its lone same-named Sprite sub-asset.
    #[test]
    fn build_db_keeps_bare_alias_when_name_is_uncontested() {
        let png_guid = Guid::from_u128(0xb0_u128);
        let raw = vec![raw_native(
            "Assets/Tower/Lone.png",
            png_guid,
            vec![SubAsset {
                file_id: 21300000,
                class_id: ClassId::Sprite as u32,
                name: "Lone".into(),
            }],
        )];

        let db = build_db(raw, None, false).expect("build_db should succeed");
        let entry = db.find_by_guid(png_guid).unwrap();
        // Top-level carries the always-ext suffix; the same-name Sprite
        // sub-asset stays bare (sub-assets have no file extension).
        assert_eq!(&*entry.name, "Lone.png");
        assert_eq!(&*entry.sub_assets[0].name, "Lone");
    }

    /// Pin: extensionless hints fall back to a bare-stem alias — no
    /// trailing-dot artifact. Rare in practice (Unity assets almost
    /// always have an extension) but the bake must not corrupt the
    /// alias for the degenerate case.
    #[test]
    fn build_db_keeps_bare_alias_for_extensionless_hint() {
        let guid = Guid::from_u128(0x77_u128);
        let raw = vec![RawEntry {
            guid,
            asset_type_raw: AssetTypeRaw::Native(ClassId::DefaultAsset as u32),
            hint: "Assets/Misc/README".to_string(),
            name: String::new(),
            
            
            sub_assets: vec![],
        }];
        let db = build_db(raw, None, false).expect("build_db should succeed");
        assert_eq!(&*db.find_by_guid(guid).unwrap().name, "README");
    }

    /// Pin the always-ext contract: every top-level alias carries the
    /// asset's file extension. `Foo.prefab` → `Foo.prefab`, not bare `Foo`.
    /// Disambiguates consumer-side lookups when a stem is reused across
    /// asset kinds without forcing the consumer to discriminate by C#
    /// field type.
    #[test]
    fn build_db_always_appends_ext_to_alias() {
        let prefab_guid = Guid::from_u128(0x10_u128);
        let raw = vec![RawEntry {
            guid: prefab_guid,
            asset_type_raw: AssetTypeRaw::Native(ClassId::Prefab as u32),
            hint: "Assets/UI/Foo.prefab".to_string(),
            name: String::new(),
            
            
            sub_assets: vec![],
        }];

        let db = build_db(raw, None, false).expect("build_db should succeed");
        assert_eq!(&*db.find_by_guid(prefab_guid).unwrap().name, "Foo.prefab");
    }

    /// Pin the BoxKeyObtainLongtake real-world case: a stem reused across
    /// `.unity`, `.playable`, `.cs`, `.prefab` resolves to 4 distinct
    /// aliases via the `.ext` suffix — no `^path` needed because each ext
    /// is unique within its own bucket.
    #[test]
    fn build_db_disambiguates_cross_ext_collision_via_ext_suffix() {
        let scene_guid = Guid::from_u128(0x01_u128);
        let playable_guid = Guid::from_u128(0x02_u128);
        let script_guid = Guid::from_u128(0x03_u128);
        let prefab_guid = Guid::from_u128(0x04_u128);
        let timeline_script_guid = Guid::from_u128(0xaaaa_u128);

        let raw = vec![
            RawEntry {
                guid: scene_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::SceneAsset as u32),
                hint: "Assets/Sandbox/BoxKeyObtainLongtake.unity".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
            RawEntry {
                guid: playable_guid,
                asset_type_raw: AssetTypeRaw::Script(timeline_script_guid),
                hint: "Assets/Prefabs/BoxKeyObtainLongtake.playable".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
            RawEntry {
                guid: script_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::MonoScript as u32),
                hint: "Assets/Scripts/BoxKeyObtainLongtake.cs".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
            RawEntry {
                guid: prefab_guid,
                asset_type_raw: AssetTypeRaw::Native(ClassId::Prefab as u32),
                hint: "Assets/Prefabs/BoxKeyObtainLongtake.prefab".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, false).expect("build_db should succeed");
        assert_eq!(
            &*db.find_by_guid(scene_guid).unwrap().name,
            "BoxKeyObtainLongtake.unity",
        );
        assert_eq!(
            &*db.find_by_guid(playable_guid).unwrap().name,
            "BoxKeyObtainLongtake.playable",
        );
        assert_eq!(
            &*db.find_by_guid(script_guid).unwrap().name,
            "BoxKeyObtainLongtake.cs",
        );
        assert_eq!(
            &*db.find_by_guid(prefab_guid).unwrap().name,
            "BoxKeyObtainLongtake.prefab",
        );
    }

    /// Pin the OrgelActivityTimeline shape: two Script-typed entries
    /// (`.asset` SO installer + `.playable` TimelineAsset) — both
    /// MonoBehaviour-114, distinct script GUIDs — resolve via their
    /// extensions alone, no within-ext contention.
    #[test]
    fn build_db_disambiguates_script_typed_cross_ext_via_ext_suffix() {
        let asset_guid = Guid::from_u128(0xc7_u128);
        let playable_guid = Guid::from_u128(0xed_u128);
        let installer_script = Guid::from_u128(0x1111_u128);
        let timeline_script = Guid::from_u128(0x2222_u128);
        let raw = vec![
            RawEntry {
                guid: asset_guid,
                asset_type_raw: AssetTypeRaw::Script(installer_script),
                hint: "Assets/OrgelEvent/OrgelActivityTimeline.asset".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
            RawEntry {
                guid: playable_guid,
                asset_type_raw: AssetTypeRaw::Script(timeline_script),
                hint: "Assets/OrgelEvent/OrgelActivityTimeline.playable".to_string(),
                name: String::new(),
                
                
                sub_assets: vec![],
            },
        ];
        let db = build_db(raw, None, false).expect("build_db should succeed");
        assert_eq!(
            &*db.find_by_guid(asset_guid).unwrap().name,
            "OrgelActivityTimeline.asset",
        );
        assert_eq!(
            &*db.find_by_guid(playable_guid).unwrap().name,
            "OrgelActivityTimeline.playable",
        );
    }

    /// Pin: when a top-level alias is genuinely unresolvable (contested
    /// same-ext entries with no parent segments to suffix with), the bake
    /// hard-fails rather than silently falling back to a `^<guid8>` suffix.
    /// Per the project policy: ambiguity surfaces at bake time, not encode
    /// time. (Distinct exts on the same stem fall into distinct buckets
    /// Round-trip: bake with contested sub-assets → `raw_from_entry` →
    /// `build_db` again. The first bake produces sub-asset names like
    /// `1^ConflictPopup/Screens`; `build_db` must strip the `^…` suffix
    /// before `reject_reserved` so the round-trip succeeds and dedup
    /// re-applies cleanly.
    #[test]
    fn build_db_round_trips_contested_sub_asset_names() {
        let sprite_fid: i64 = 21300000;
        let raw = vec![
            raw_native(
                "Assets/20_Contents/ConflictPopup/Screens/1.png",
                Guid::from_u128(0xa0),
                vec![SubAsset {
                    file_id: sprite_fid,
                    class_id: ClassId::Sprite as u32,
                    name: "1".into(),
                }],
            ),
            raw_native(
                "Assets/20_Contents/ShopPopup/Screens/1.png",
                Guid::from_u128(0xb0),
                vec![SubAsset {
                    file_id: sprite_fid,
                    class_id: ClassId::Sprite as u32,
                    name: "1".into(),
                }],
            ),
        ];

        // First bake — contested sub-asset "1" gets depth-2 suffix with `/`.
        let db1 = build_db(raw, None, false).expect("first bake");
        let a = db1.find_by_guid(Guid::from_u128(0xa0)).unwrap();
        assert_eq!(&*a.sub_assets[0].name, "1^ConflictPopup/Screens");

        // Round-trip: convert baked entries back to raw and re-build.
        let raw2: Vec<RawEntry> = db1
            .entries
            .iter()
            .map(|e| raw_from_entry(e, &db1.script_types))
            .collect();
        let db2 = build_db(raw2, None, false).expect("round-trip bake must not fail");

        // Same result — collision suffixes re-applied identically.
        let a2 = db2.find_by_guid(Guid::from_u128(0xa0)).unwrap();
        let b2 = db2.find_by_guid(Guid::from_u128(0xb0)).unwrap();
        assert_eq!(&*a2.sub_assets[0].name, "1^ConflictPopup/Screens");
        assert_eq!(&*b2.sub_assets[0].name, "1^ShopPopup/Screens");
    }

    /// under the always-ext rule and never reach `parent_suffix`.)
    #[test]
    fn build_db_fails_when_dedup_cannot_resolve() {
        let raw = vec![
            // Two top-level entries with the same `<stem>.<ext>` name and
            // no parent segments to walk — `parent_suffix` has nothing to
            // attach.
            raw_native("Foo.png", Guid::from_u128(0x01_u128), vec![]),
            raw_native("Foo.png", Guid::from_u128(0x02_u128), vec![]),
        ];

        let err = build_db(raw, None, false)
            .expect_err("collision with no parent dirs must hard-fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Foo") && msg.contains("disambiguate"),
            "error message should name the collision and the dedup pass: {msg}",
        );
    }
}
