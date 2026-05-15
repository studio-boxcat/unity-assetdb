//! Register a new asset by synthesizing a `.meta` file outside Unity.
//!
//! The flow:
//! 1. Acquire an advisory flock on `<out_dir>/.asset-db.lock` so a
//!    concurrent `bake` can't clobber the db while we're mid-update.
//! 2. If `<path>.meta` exists → parse, idempotent return (with optional
//!    incremental db-insert if absent from the bin).
//! 3. Otherwise synthesize a minimal `.meta` with a fresh 128-bit GUID
//!    and the importer kind chosen from the file's extension (or the
//!    `--type` override). The body carries only the four `externalObjects`
//!    / `userData` / `assetBundleName` / `assetBundleVariant` fields —
//!    Unity rewrites the rest on next editor focus.
//! 4. Re-parse the asset via [`crate::bake::parse_one`] to compute the
//!    same `AssetEntry` shape a `bake` would produce, intern any script
//!    GUID, insert sorted into the loaded `AssetDb`, atomic-write
//!    `asset-db.bin` and `asset-db.cache.bin`.
//!
//! "Minimal meta" assumption: Unity, on next focus, re-imports the asset
//! and overwrites the importer block with project defaults, **preserving
//! the `guid:` field**. The synthesized GUID becomes the stable identity
//! callers can immediately pipe into pspec refs / catalog bakes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bake::{self, BakeError, ParsedAssetType, ParsedEntry};
use crate::query;
use crate::store::{
    self, AssetDb, AssetEntry, AssetType, BakeCache, CachedAssetType, CachedEntry, LockWait,
    StoreError,
};

/// Unity importer kind. Each variant maps to the YAML block key (e.g.
/// `NativeFormat` → `NativeFormatImporter:`). Extension → kind mapping
/// lives in [`importer_for_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImporterKind {
    NativeFormat,
    Prefab,
    Texture,
    Audio,
    TrueTypeFont,
    Shader,
    TextScript,
    Mono,
    SpriteAtlas,
    /// Fallback for extensions without a specialized importer (`.unity`,
    /// `.tps`, …) and for folders (combined with `folderAsset: yes`).
    Default,
}

impl ImporterKind {
    /// YAML key Unity writes for this importer block.
    pub fn yaml_key(self) -> &'static str {
        match self {
            Self::NativeFormat => "NativeFormatImporter",
            Self::Prefab => "PrefabImporter",
            Self::Texture => "TextureImporter",
            Self::Audio => "AudioImporter",
            Self::TrueTypeFont => "TrueTypeFontImporter",
            Self::Shader => "ShaderImporter",
            Self::TextScript => "TextScriptImporter",
            Self::Mono => "MonoImporter",
            Self::SpriteAtlas => "SpriteAtlasImporter",
            Self::Default => "DefaultImporter",
        }
    }

    /// Parse a `--type` argument. Accepts both the `ImporterKind` variant
    /// name (`NativeFormat`, `Texture`) and the YAML key Unity emits
    /// (`NativeFormatImporter`, `TextureImporter`) for ergonomics —
    /// callers often paste from a sibling `.meta`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "NativeFormat" | "NativeFormatImporter" => Self::NativeFormat,
            "Prefab" | "PrefabImporter" => Self::Prefab,
            "Texture" | "TextureImporter" => Self::Texture,
            "Audio" | "AudioImporter" => Self::Audio,
            "TrueTypeFont" | "TrueTypeFontImporter" => Self::TrueTypeFont,
            "Shader" | "ShaderImporter" => Self::Shader,
            "TextScript" | "TextScriptImporter" => Self::TextScript,
            "Mono" | "MonoImporter" => Self::Mono,
            "SpriteAtlas" | "SpriteAtlasImporter" => Self::SpriteAtlas,
            "Default" | "DefaultImporter" => Self::Default,
            _ => return None,
        })
    }
}

/// Pick the default importer kind for `path`. Folders → `Default` with
/// the `folderAsset: yes` flag (caller handles the flag). Files use the
/// extension table; unknown extensions fall back to `Default`.
pub fn importer_for_path(path: &Path, is_dir: bool) -> ImporterKind {
    if is_dir {
        return ImporterKind::Default;
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "asset" | "controller" | "anim" | "playable" | "mat" | "preset" | "mixer"
        | "overridecontroller" | "physicmaterial" | "physicsmaterial2d" => ImporterKind::NativeFormat,
        "prefab" => ImporterKind::Prefab,
        "png" | "jpg" | "jpeg" | "tga" | "psd" | "gif" | "bmp" | "tif" | "tiff" | "exr"
        | "hdr" | "iff" | "pict" => ImporterKind::Texture,
        "wav" | "mp3" | "ogg" | "aif" | "aiff" | "flac" => ImporterKind::Audio,
        "ttf" | "otf" | "ttc" => ImporterKind::TrueTypeFont,
        "shader" | "compute" => ImporterKind::Shader,
        "txt" | "html" | "htm" | "xml" | "bytes" | "json" | "csv" | "yaml" => {
            ImporterKind::TextScript
        }
        "cs" => ImporterKind::Mono,
        "spriteatlas" | "spriteatlasv2" => ImporterKind::SpriteAtlas,
        // Everything else (.unity, .tps, .pdf, .swf, unknown) → Default.
        _ => ImporterKind::Default,
    }
}

/// Render the minimal `.meta` content for `kind` with `guid_hex` as the
/// GUID field. `is_folder` inserts the `folderAsset: yes` marker between
/// the guid line and the importer block — required for Unity to treat
/// the asset as a folder rather than a binary blob.
pub fn render_meta(guid_hex: &str, kind: ImporterKind, is_folder: bool) -> String {
    let folder_line = if is_folder { "folderAsset: yes\n" } else { "" };
    // Trailing space on the empty-string fields is REQUIRED — bare
    // `userData:` parses as YAML null and Unity's importer rejects it.
    // Samples from meow-tower (`.asset`, `.prefab`) confirm the
    // `userData: \n` (one trailing space) shape.
    format!(
        "fileFormatVersion: 2\n\
         guid: {guid_hex}\n\
         {folder_line}{key}:\n  \
         externalObjects: {{}}\n  \
         userData: \n  \
         assetBundleName: \n  \
         assetBundleVariant: \n",
        key = kind.yaml_key(),
    )
}

/// Generate a fresh 128-bit GUID via the OS RNG. 32-char lowercase hex.
/// 128-bit random is collision-safe at any realistic project scale —
/// no in-db collision check.
pub fn generate_guid() -> Result<u128, RegisterError> {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).map_err(RegisterError::Rand)?;
    Ok(u128::from_be_bytes(buf))
}

/// Errors from a `register` run.
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Bake(#[from] BakeError),
    #[error("{op} {}: {source}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("path not under project root {}: {}", root.display(), path.display())]
    NotInProject { root: PathBuf, path: PathBuf },
    #[error(
        "existing .meta declares importer `{existing}`, but --type requested `{requested}` — \
         pass no --type to accept the existing kind, or delete the .meta to re-author"
    )]
    ImporterMismatch {
        existing: String,
        requested: &'static str,
    },
    #[error("could not generate GUID: {0}")]
    Rand(#[source] getrandom::Error),
    #[error(
        "could not acquire lock on {} after {timeout:?} — another bake or register is running",
        path.display()
    )]
    LockHeld { path: PathBuf, timeout: Duration },
}

/// Caller-supplied register configuration.
pub struct RegisterOptions {
    /// Project root containing `Assets/` + `ProjectSettings/`.
    pub project_root: PathBuf,
    /// Out-dir holding `asset-db.bin` (the file `register` mutates).
    pub out_dir: PathBuf,
    /// Path to the asset Unity should treat as registered. Absolute or
    /// project-relative; `register` canonicalizes and rejects paths
    /// outside `project_root`.
    pub target: PathBuf,
    /// Override the ext-derived importer kind. Errors if it differs from
    /// the existing `.meta`'s importer block (when a meta is already on disk).
    pub importer_override: Option<ImporterKind>,
    /// If `Some(non-empty)`, apply this scrub set to the new entry's
    /// `name` before insert — mirrors `bake --scrub-chars`. Callers
    /// running bake with a scrub set MUST pass the same value here, else
    /// the registered entry won't match consumers' lookup conventions.
    pub scrub_chars: Option<String>,
    /// Block this long acquiring `<out_dir>/.asset-db.lock`. `Duration::ZERO`
    /// = try-lock only (immediate `LockHeld` on contention).
    pub lock_timeout: Duration,
}

/// Outcome of [`register`].
#[derive(Debug, Clone, Copy)]
pub struct RegisterOutcome {
    /// The asset's GUID after the operation (newly minted or pre-existing).
    pub guid: u128,
    /// `true` when this call synthesized the `.meta` (idempotent re-runs
    /// return `false`).
    pub created_meta: bool,
    /// `true` when the `AssetDb` row was inserted or updated.
    pub db_updated: bool,
}

/// Register `opts.target` against `opts.out_dir`'s `AssetDb`. See module
/// docs for the full flow.
pub fn register(opts: &RegisterOptions) -> Result<RegisterOutcome, RegisterError> {
    // Canonicalize project_root once — `parse_one` does string-prefix
    // matching of `companion.strip_prefix(project_root)`, which only
    // works if both sides resolve symlinks consistently. macOS /tmp →
    // /private/tmp would break it otherwise.
    let project_root = opts
        .project_root
        .canonicalize()
        .map_err(|source| RegisterError::Io {
            op: "canonicalize project_root",
            path: opts.project_root.clone(),
            source,
        })?;
    let target_abs = canonicalize_under_project(&opts.target, &project_root)?;
    let meta_path = bake::with_meta_suffix(&target_abs);

    std::fs::create_dir_all(&opts.out_dir).map_err(|source| RegisterError::Io {
        op: "create out-dir",
        path: opts.out_dir.clone(),
        source,
    })?;
    let wait = if opts.lock_timeout.is_zero() {
        LockWait::TryOnce
    } else {
        LockWait::Until(opts.lock_timeout)
    };
    let _lock = store::acquire_lock(&opts.out_dir, wait).map_err(|e| match e {
        StoreError::Io { source, .. } if source.kind() == std::io::ErrorKind::WouldBlock => {
            RegisterError::LockHeld {
                path: store::lock_path(&opts.out_dir),
                timeout: opts.lock_timeout,
            }
        }
        other => RegisterError::Store(other),
    })?;

    let (guid, created_meta) = synthesize_or_read_meta(&target_abs, &meta_path, opts)?;

    let db_updated = update_db_for(
        &project_root,
        &opts.out_dir,
        &meta_path,
        opts.scrub_chars.as_deref(),
    )?;

    Ok(RegisterOutcome {
        guid,
        created_meta,
        db_updated,
    })
}

/// Try `create_new` (atomic exclusive create). On success, synthesize
/// the meta body and write — caller "created" it. On `AlreadyExists`,
/// fall back to parsing the existing meta. Sidesteps the TOCTOU window
/// between `exists()` and `write()`.
///
/// Write order: meta → bin → cache. A failure between meta-create and
/// db-write leaves a meta on disk that isn't yet indexed; a re-run of
/// register sees the existing meta and inserts the missing row.
fn synthesize_or_read_meta(
    target_abs: &Path,
    meta_path: &Path,
    opts: &RegisterOptions,
) -> Result<(u128, bool), RegisterError> {
    let is_dir = std::fs::metadata(target_abs)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    let kind = opts
        .importer_override
        .unwrap_or_else(|| importer_for_path(target_abs, is_dir));
    let guid = generate_guid()?;
    let hex = query::format_guid(guid);
    let body = render_meta(&hex, kind, is_dir);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(meta_path)
    {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(body.as_bytes()).map_err(|source| RegisterError::Io {
                op: "write meta",
                path: meta_path.to_path_buf(),
                source,
            })?;
            Ok((guid, true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = parse_existing_meta(meta_path, opts.importer_override)?;
            Ok((existing, false))
        }
        Err(source) => Err(RegisterError::Io {
            op: "create meta",
            path: meta_path.to_path_buf(),
            source,
        }),
    }
}

/// Read an existing `.meta` and return its GUID, optionally validating
/// the importer kind against `--type`.
fn parse_existing_meta(
    meta_path: &Path,
    importer_override: Option<ImporterKind>,
) -> Result<u128, RegisterError> {
    let text = std::fs::read_to_string(meta_path).map_err(|source| RegisterError::Io {
        op: "read meta",
        path: meta_path.to_path_buf(),
        source,
    })?;
    let info = crate::meta::parse(&text).map_err(|e| RegisterError::Io {
        op: "parse meta",
        path: meta_path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
    })?;
    if let Some(requested) = importer_override {
        let existing = detect_importer_key(&text).unwrap_or("<unknown>");
        if existing != requested.yaml_key() {
            return Err(RegisterError::ImporterMismatch {
                existing: existing.to_owned(),
                requested: requested.yaml_key(),
            });
        }
    }
    Ok(info.guid)
}

/// Find the importer YAML key in an existing meta. Looks for the first
/// line ending in `Importer:` at column 0.
fn detect_importer_key(text: &str) -> Option<&str> {
    for line in text.lines() {
        if !line.starts_with(' ')
            && !line.starts_with('\t')
            && let Some(rest) = line.strip_suffix(':')
            && rest.ends_with("Importer")
            && !rest.contains(' ')
        {
            return Some(rest);
        }
    }
    None
}

/// Load `asset-db.bin` (empty if absent), re-parse via `bake::parse_one`,
/// insert sorted, write both bin and cache atomically. Returns whether
/// the db was actually modified.
fn update_db_for(
    project_root: &Path,
    out_dir: &Path,
    meta_path: &Path,
    scrub_chars: Option<&str>,
) -> Result<bool, RegisterError> {
    let db_path = store::db_path(out_dir);
    let cache_path = store::cache_path(out_dir);

    let mut db = read_or_empty_db(&db_path)?;
    let mut cache = read_or_empty_cache(&cache_path)?;

    let Some(mut parsed) = bake::parse_one(project_root, meta_path)? else {
        return Ok(false);
    };

    // `AssetDb::sort()` keys sub-assets by `file_id`; mirror it so the
    // unchanged-row check doesn't false-diverge against a baked row.
    parsed.sub_assets.sort_by_key(|s| s.file_id);

    let ParsedEntry {
        guid,
        asset_type: parsed_at,
        hint,
        meta_mtime_ns,
        asset_mtime_ns,
        sub_assets,
    } = parsed;
    // Mint the always-ext alias the next full bake would derive — keeps
    // register-inserted rows from churning on the following `bake`.
    let name = bake::filename_with_ext_from_hint(&hint);

    let cached_asset_type = match parsed_at {
        ParsedAssetType::Native(n) => CachedAssetType::Native(n),
        ParsedAssetType::Script(g) => CachedAssetType::Script(g),
    };
    let asset_type = match parsed_at {
        ParsedAssetType::Native(n) => AssetType::Native(n),
        ParsedAssetType::Script(g) => AssetType::Script(db.intern_script(g)),
    };
    let name = match scrub_chars {
        Some(chars) if !chars.is_empty() => query::scrub_in(&name, chars),
        _ => name,
    };

    if let Some(existing) = db.find_by_guid(guid)
        && existing.asset_type == asset_type
        && &*existing.name == name.as_str()
        && &*existing.hint == hint.as_str()
        && sub_assets_match(&existing.sub_assets, &sub_assets)
    {
        return Ok(false);
    }

    db.insert_sorted(AssetEntry {
        guid,
        asset_type,
        name: name.into_boxed_str(),
        sub_assets: sub_assets.clone(),
        hint: hint.clone().into_boxed_str(),
    });
    cache.insert_sorted(CachedEntry {
        hint: hint.into_boxed_str(),
        meta_mtime_ns,
        asset_mtime_ns,
        guid,
        asset_type: cached_asset_type,
        sub_assets,
    });

    store::write(&db_path, &db)?;
    store::write_cache(&cache_path, &cache)?;
    Ok(true)
}

fn read_or_empty_db(path: &Path) -> Result<AssetDb, RegisterError> {
    match store::read(path) {
        Ok(d) => Ok(d),
        Err(StoreError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(AssetDb::new())
        }
        Err(e) => Err(e.into()),
    }
}

fn read_or_empty_cache(path: &Path) -> Result<BakeCache, RegisterError> {
    match store::read_cache(path) {
        Ok(c) => Ok(c),
        Err(StoreError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(BakeCache::new())
        }
        // Corrupt cache: re-build from scratch. The cache is gitignored
        // and the next bake will refill it; throwing here would block
        // register on an issue the user can't easily fix.
        Err(_) => Ok(BakeCache::new()),
    }
}

fn sub_assets_match(a: &[crate::store::SubAsset], b: &[crate::store::SubAsset]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.file_id == y.file_id && x.class_id == y.class_id && x.name == y.name)
}

/// Resolve `target` to an absolute path under the (already-canonicalized)
/// `project_root`. The target's parent must exist; the target itself need
/// not (register is allowed to author a meta for an asset whose payload
/// hasn't been written yet — `parse_one` returns None in that case).
///
/// Rejects targets whose canonical parent is outside the project root.
fn canonicalize_under_project(target: &Path, project_root: &Path) -> Result<PathBuf, RegisterError> {
    let abs = if target.is_absolute() {
        target.to_path_buf()
    } else {
        project_root.join(target)
    };
    let parent = abs.parent().unwrap_or(Path::new("."));
    let parent_canon = parent.canonicalize().map_err(|source| RegisterError::Io {
        op: "canonicalize parent",
        path: parent.to_path_buf(),
        source,
    })?;
    if !parent_canon.starts_with(project_root) {
        return Err(RegisterError::NotInProject {
            root: project_root.to_path_buf(),
            path: abs,
        });
    }
    let file_name = abs.file_name().ok_or_else(|| RegisterError::Io {
        op: "extract file name",
        path: abs.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"),
    })?;
    Ok(parent_canon.join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importer_for_path_table() {
        let p = |s: &str| importer_for_path(Path::new(s), false);
        assert_eq!(p("Foo.asset"), ImporterKind::NativeFormat);
        assert_eq!(p("Foo.controller"), ImporterKind::NativeFormat);
        assert_eq!(p("Foo.prefab"), ImporterKind::Prefab);
        assert_eq!(p("Foo.png"), ImporterKind::Texture);
        assert_eq!(p("Foo.PNG"), ImporterKind::Texture); // case-insensitive
        assert_eq!(p("Foo.wav"), ImporterKind::Audio);
        assert_eq!(p("Foo.ttf"), ImporterKind::TrueTypeFont);
        assert_eq!(p("Foo.shader"), ImporterKind::Shader);
        assert_eq!(p("Foo.bytes"), ImporterKind::TextScript);
        assert_eq!(p("Foo.cs"), ImporterKind::Mono);
        assert_eq!(p("Foo.spriteatlasv2"), ImporterKind::SpriteAtlas);
        assert_eq!(p("Foo.unity"), ImporterKind::Default);
        assert_eq!(p("Foo.tps"), ImporterKind::Default);
        assert_eq!(p("Foo.xyz"), ImporterKind::Default);
        assert_eq!(
            importer_for_path(Path::new("SomeDir"), true),
            ImporterKind::Default,
        );
    }

    #[test]
    fn parse_importer_kind_accepts_both_spellings() {
        assert_eq!(
            ImporterKind::parse("NativeFormat"),
            Some(ImporterKind::NativeFormat),
        );
        assert_eq!(
            ImporterKind::parse("NativeFormatImporter"),
            Some(ImporterKind::NativeFormat),
        );
        assert_eq!(ImporterKind::parse("Bogus"), None);
    }

    #[test]
    fn render_meta_shapes() {
        let m = render_meta(
            "aabbccddaabbccddaabbccddaabbccdd",
            ImporterKind::NativeFormat,
            false,
        );
        assert!(m.starts_with("fileFormatVersion: 2\n"));
        assert!(m.contains("guid: aabbccddaabbccddaabbccddaabbccdd\n"));
        assert!(m.contains("NativeFormatImporter:\n"));
        assert!(m.contains("  userData: \n")); // trailing space matters
        assert!(!m.contains("folderAsset:"));

        let folder = render_meta(
            "aabbccddaabbccddaabbccddaabbccdd",
            ImporterKind::Default,
            true,
        );
        assert!(folder.contains("folderAsset: yes\n"));
        assert!(folder.contains("DefaultImporter:\n"));
    }

    #[test]
    fn detect_importer_finds_block() {
        let text = "fileFormatVersion: 2\nguid: a\nNativeFormatImporter:\n  externalObjects: {}\n";
        assert_eq!(detect_importer_key(text), Some("NativeFormatImporter"));
        let text2 = "fileFormatVersion: 2\nguid: a\nfolderAsset: yes\nDefaultImporter:\n";
        assert_eq!(detect_importer_key(text2), Some("DefaultImporter"));
    }

    #[test]
    fn generate_guid_is_random() {
        let a = generate_guid().unwrap();
        let b = generate_guid().unwrap();
        assert_ne!(a, b);
        assert_ne!(a, 0);
    }
}
