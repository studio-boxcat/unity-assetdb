//! On-disk schemas for the bake pipeline.
//!
//! Two files, written side-by-side under the consumer-chosen out-dir
//! (commonly `<project>/Library/<consumer>/`):
//!
//! - `asset-db.bin` — convert artifact. Lean: per-entry guid, asset type,
//!   name, sub-assets. Sorted by guid for O(log n) binary-search lookup;
//!   no path/mtime baggage.
//! - `asset-db.cache.bin` — bake-only cache, gitignored alongside.
//!   Maps `hint → (mtimes, resolved bake state)` so unchanged assets skip
//!   re-parsing on subsequent bakes. Downstream consumers never read this.
//!
//! Script (MonoBehaviour / ScriptableObject) types are interned in
//! `script_types` and referenced by index — keeps per-entry payload small
//! (8 bytes for `AssetType`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bincode::{Decode, Encode};

use crate::class_id::ClassId;

/// Bumped whenever the on-disk schema changes incompatibly.
/// A version mismatch is a hard fail — the user re-bakes.
///
/// History:
/// - v4: every name in `entries[].name` and `entries[].sub_assets[].name`
///   resolves to a unique guid (name namespace unified across top-level
///   and sub-asset rows). Pre-v4 bakes could carry colliding sub-asset
///   names; readers no longer accept them.
/// - v5: two changes shipped together.
///   1. File magic renamed `PSPECADB` → `UADBIN__` and `PSPECABC` →
///      `UADCACHE` to drop the historical "pspec" prefix.
///   2. `SubAsset` carries `class_id` so non-canonical sub-asset fileIDs
///      (prefab-embedded `AnimationClip` with hashed negative fids) keep
///      their real Unity class instead of a `file_id / 100_000` heuristic
///      collapsing them to `ScriptableObject`. Top-level entries also
///      share their alias bucket with same-named entries of a different
///      `asset_type` — type-aware reverse lookup discriminates at query
///      time. See [Name collisions](docs/asset-database.md#name-collisions).
///   Pre-v5 bakes are unreadable; re-bake required after upgrading.
pub const SCHEMA_VERSION: u16 = 5;

/// File magic — first 8 bytes. `b"UADBIN__"`.
pub const MAGIC: [u8; 8] = *b"UADBIN__";

/// File magic for the bake-only cache file.
pub const CACHE_MAGIC: [u8; 8] = *b"UADCACHE";

/// Type of a Unity asset.
///
/// `Native(classId)` for built-in types (Sprite, Prefab, Texture2D, …).
/// `Script(idx)` for MonoBehaviour-backed assets — `idx` indexes into
/// [`AssetDb::script_types`], whose entries are u128 script GUIDs that
/// match the `guid` field on entries in `types.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub enum AssetType {
    Native(u32),
    Script(u32),
}

impl AssetType {
    pub fn native(class_id: ClassId) -> Self {
        Self::Native(class_id as u32)
    }
}

/// One sub-object inside an asset that has its own fileID.
///
/// Sprite-atlas entries, multi-clip animations, sprite-sheet sub-sprites,
/// prefab-embedded `AnimationClip` docs. Per-entry list is sorted by
/// `file_id` for binary-search lookups.
///
/// `class_id` is the Unity native classID of the sub-doc (`74` for
/// `AnimationClip`, `213` for `Sprite`, etc.). Stored explicitly because
/// prefab-embedded sub-asset fileIDs are hashed (negative or non-multiple-
/// of-100000) and a `file_id / 100_000` heuristic collapses them to
/// `ScriptableObject` — the asset DB needs the real class for the
/// strict-typed-field elision rule consumers apply downstream.
///
/// `name` is `Box<str>` rather than `String` — strings here are immutable
/// once decoded; dropping the capacity field saves 8 bytes per entry.
#[derive(Debug, Clone, Encode, Decode)]
pub struct SubAsset {
    pub file_id: i64,
    pub class_id: u32,
    pub name: Box<str>,
}

/// One top-level Unity asset, as stored in the convert artifact.
///
/// `name` is the asset's filename stem (with optional collision suffix).
/// At convert time it's prefixed with `$` to form a JSON ref (`$Foo`),
/// but the prefix is purely a JSON encoding convention — never stored.
/// String fields use `Box<str>` (immutable; saves the 8-byte capacity
/// field a `String` carries for growability).
#[derive(Debug, Clone, Encode, Decode)]
pub struct AssetEntry {
    pub guid: u128,
    pub asset_type: AssetType,
    pub name: Box<str>,
    pub sub_assets: Vec<SubAsset>,
    /// Project-root-relative path (`Assets/Foo.prefab`,
    /// `Packages/com.boxcat.libs/Bar.mixer`). Convert-side uses this so
    /// `SourcePrefabResolver` can locate base prefabs by guid without
    /// re-walking the project tree.
    pub hint: Box<str>,
}

/// Whole-database envelope. `entries` is sorted by `guid` so convert-time
/// lookups are `binary_search_by_key`.
#[derive(Debug, Clone, Default, Encode, Decode)]
pub struct AssetDb {
    pub schema_version: u16,
    /// Interned script GUIDs (u128). `AssetType::Script(idx)` indexes here.
    /// Sorted ascending; deduplicated.
    pub script_types: Vec<u128>,
    /// Sorted by `guid` ascending.
    pub entries: Vec<AssetEntry>,
}

impl AssetDb {
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ..Default::default()
        }
    }

    /// O(log n) lookup by GUID. None if absent.
    pub fn find_by_guid(&self, guid: u128) -> Option<&AssetEntry> {
        let idx = self.entries.binary_search_by_key(&guid, |e| e.guid).ok()?;
        Some(&self.entries[idx])
    }

    /// Resolve `AssetType::Script(idx)` to its underlying script GUID.
    /// Panics on out-of-range idx — that's a corrupt-file error, fail loud.
    pub fn script_guid(&self, idx: u32) -> u128 {
        self.script_types[idx as usize]
    }

    /// Bake-side intern: returns the index of `guid`, inserting if new.
    pub fn intern_script(&mut self, guid: u128) -> u32 {
        match self.script_types.binary_search(&guid) {
            Ok(idx) => idx as u32,
            Err(idx) => {
                self.script_types.insert(idx, guid);
                idx as u32
            }
        }
    }

    /// Sort `entries` by guid and each `sub_assets` by `file_id`.
    /// Call after bulk-loading.
    pub fn sort(&mut self) {
        self.entries.sort_by_key(|e| e.guid);
        for e in &mut self.entries {
            e.sub_assets.sort_by_key(|s| s.file_id);
        }
    }
}

// ─── Bake-only cache ─────────────────────────────────────────────────────

/// `AssetType` variant for the cache. Stores the script GUID directly so
/// the cache doesn't depend on the in-memory `script_types` table — each
/// bake interns scripts fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum CachedAssetType {
    Native(u32),
    Script(u128),
}

/// One cached parse result, keyed by `hint`. Lets a re-bake skip the
/// .meta + asset reads when both mtimes match.
#[derive(Debug, Clone, Encode, Decode)]
pub struct CachedEntry {
    pub hint: Box<str>,
    pub meta_mtime_ns: u64,
    pub asset_mtime_ns: u64,
    pub guid: u128,
    pub asset_type: CachedAssetType,
    pub sub_assets: Vec<SubAsset>,
}

/// Bake-only cache file envelope. `entries` order is hint-sorted so re-writes
/// are deterministic, but lookups go through a HashMap built at load.
#[derive(Debug, Clone, Default, Encode, Decode)]
pub struct BakeCache {
    pub schema_version: u16,
    pub entries: Vec<CachedEntry>,
}

impl BakeCache {
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ..Default::default()
        }
    }
}

// ─── Path helpers ────────────────────────────────────────────────────────

/// Convert artifact filename.
pub const DB_FILENAME: &str = "asset-db.bin";

/// Bake-only mtime cache filename. Sibling to [`DB_FILENAME`].
pub const CACHE_FILENAME: &str = "asset-db.cache.bin";

/// `<dir>/asset-db.bin`. Caller composes the directory convention
/// (e.g. `<project>/Library/unity-assetdb/`).
pub fn db_path(dir: &Path) -> PathBuf {
    dir.join(DB_FILENAME)
}

/// `<dir>/asset-db.cache.bin`. Sibling to [`db_path`].
pub fn cache_path(dir: &Path) -> PathBuf {
    dir.join(CACHE_FILENAME)
}

// ─── IO ──────────────────────────────────────────────────────────────────

/// Read the convert artifact.
pub fn read(path: &Path) -> Result<AssetDb> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read asset-db: {}", path.display()))?;
    decode(&bytes)
}

pub fn decode(bytes: &[u8]) -> Result<AssetDb> {
    let body = check_magic(bytes, MAGIC, "asset-db")?;
    let cfg = bincode::config::standard();
    let (db, _): (AssetDb, _) = bincode::decode_from_slice(body, cfg).context("bincode decode")?;
    if db.schema_version != SCHEMA_VERSION {
        anyhow::bail!(
            "asset-db schema {} expected {}, re-bake required",
            db.schema_version,
            SCHEMA_VERSION
        );
    }
    Ok(db)
}

/// Write the convert artifact, creating parent dirs as needed.
pub fn write(path: &Path, db: &AssetDb) -> Result<()> {
    write_bytes(path, &encode(db)?)
}

pub fn encode(db: &AssetDb) -> Result<Vec<u8>> {
    encode_with_magic(db, MAGIC)
}

/// Read the bake-only cache. Returns `BakeCache::new()` (empty, current
/// schema) if the file is missing or unreadable — first bake or stale
/// cache, parse everything from scratch.
pub fn read_cache(path: &Path) -> Result<BakeCache> {
    let bytes = std::fs::read(path).with_context(|| format!("read cache: {}", path.display()))?;
    decode_cache(&bytes)
}

pub fn decode_cache(bytes: &[u8]) -> Result<BakeCache> {
    let body = check_magic(bytes, CACHE_MAGIC, "asset-db.cache")?;
    let cfg = bincode::config::standard();
    let (cache, _): (BakeCache, _) =
        bincode::decode_from_slice(body, cfg).context("bincode decode cache")?;
    if cache.schema_version != SCHEMA_VERSION {
        anyhow::bail!(
            "asset-db cache schema {} expected {}",
            cache.schema_version,
            SCHEMA_VERSION
        );
    }
    Ok(cache)
}

pub fn write_cache(path: &Path, cache: &BakeCache) -> Result<()> {
    write_bytes(path, &encode_cache(cache)?)
}

pub fn encode_cache(cache: &BakeCache) -> Result<Vec<u8>> {
    encode_with_magic(cache, CACHE_MAGIC)
}

fn encode_with_magic<T: Encode>(value: &T, magic: [u8; 8]) -> Result<Vec<u8>> {
    let cfg = bincode::config::standard();
    let body = bincode::encode_to_vec(value, cfg).context("bincode encode")?;
    let mut out = Vec::with_capacity(magic.len() + body.len());
    out.extend_from_slice(&magic);
    out.extend_from_slice(&body);
    Ok(out)
}

fn check_magic<'a>(bytes: &'a [u8], magic: [u8; 8], label: &str) -> Result<&'a [u8]> {
    if bytes.len() < magic.len() {
        anyhow::bail!("{label} too short ({} bytes)", bytes.len());
    }
    let (head, body) = bytes.split_at(magic.len());
    if head != magic {
        anyhow::bail!("{label} magic mismatch");
    }
    Ok(body)
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir: {}", parent.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("write: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class_id::ClassId;

    #[test]
    fn roundtrip_empty() {
        let db = AssetDb::new();
        let bytes = encode(&db).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert!(back.entries.is_empty());
        assert!(back.script_types.is_empty());
    }

    #[test]
    fn roundtrip_with_entries() {
        let mut db = AssetDb::new();
        let script_guid = 0x1234_5678_9abc_def0_1122_3344_5566_7788_u128;
        let idx = db.intern_script(script_guid);
        db.entries.push(AssetEntry {
            guid: 0xaabb_ccdd_u128,
            asset_type: AssetType::native(ClassId::Prefab),
            name: "Foo".into(),
            sub_assets: vec![],
            hint: "Assets/UI/Foo.prefab".into(),
        });
        db.entries.push(AssetEntry {
            guid: 0x1111_2222_u128,
            asset_type: AssetType::Script(idx),
            name: "Bar".into(),
            sub_assets: vec![SubAsset {
                file_id: 21300000,
                class_id: ClassId::Sprite as u32,
                name: "Bar_sub".into(),
            }],
            hint: "Assets/Tween/Bar.asset".into(),
        });
        db.sort();

        let bytes = encode(&db).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.script_types, vec![script_guid]);
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries[0].guid, 0x1111_2222_u128);
        assert_eq!(&*back.find_by_guid(0xaabb_ccdd_u128).unwrap().name, "Foo");
        assert!(back.find_by_guid(0xdead_beef_u128).is_none());
    }

    #[test]
    fn intern_dedups() {
        let mut db = AssetDb::new();
        let g = 42u128;
        let a = db.intern_script(g);
        let b = db.intern_script(g);
        assert_eq!(a, b);
        assert_eq!(db.script_types.len(), 1);
    }

    #[test]
    fn magic_mismatch_errors() {
        let bad = b"NOTAPDB!extra".to_vec();
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn cache_roundtrip() {
        let mut c = BakeCache::new();
        c.entries.push(CachedEntry {
            hint: "UI/Foo.prefab".into(),
            meta_mtime_ns: 1,
            asset_mtime_ns: 2,
            guid: 0xaa_u128,
            asset_type: CachedAssetType::Native(1001),
            sub_assets: vec![],
        });
        c.entries.push(CachedEntry {
            hint: "Tween/Bar.asset".into(),
            meta_mtime_ns: 3,
            asset_mtime_ns: 4,
            guid: 0xbb_u128,
            asset_type: CachedAssetType::Script(0xcc_u128),
            sub_assets: vec![],
        });

        let bytes = encode_cache(&c).unwrap();
        let back = decode_cache(&bytes).unwrap();
        assert_eq!(back.entries.len(), 2);
        assert_eq!(
            back.entries[1].asset_type,
            CachedAssetType::Script(0xcc_u128)
        );
    }

    #[test]
    fn cache_magic_distinct_from_db() {
        // Cache file must not be mistaken for db (and vice versa).
        let c = BakeCache::new();
        let cache_bytes = encode_cache(&c).unwrap();
        assert!(decode(&cache_bytes).is_err());

        let db = AssetDb::new();
        let db_bytes = encode(&db).unwrap();
        assert!(decode_cache(&db_bytes).is_err());
    }
}
