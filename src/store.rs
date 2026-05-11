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
/// - v6: `.prefab`/`.controller`/`.anim`/`.mixer`/`.playable` sub-asset
///   rows now exclude the GameObject-tree structural classes (GameObject,
///   Transform, RectTransform, MonoBehaviour-as-component scoped to
///   `.prefab` only). Pre-v6 caches carry leaked `'@<name>'` sub-asset
///   rows for child GOs that would re-emerge on warm bakes; the bump
///   invalidates them.
pub const SCHEMA_VERSION: u16 = 6;

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
/// .meta + asset reads when the meta mtime matches.
///
/// `asset_mtime_ns` is recorded but no longer participates in the warm
/// fast-path invalidation check — `process_one` keys solely on
/// `meta_mtime_ns`. The field is retained for forensic value (a future
/// re-bake or external tooling can compare against the live asset
/// mtime) and as a schema slot for richer invalidation logic should it
/// land. The implication: under hand-edits that touch the asset
/// without touching the .meta, this field can become stale on the
/// next re-bake (still serves the cached row). See
/// `tests/bake.rs::cache_does_not_detect_asset_only_touch`.
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

// ─── Errors ──────────────────────────────────────────────────────────────

/// Errors from reading/writing the on-disk asset-db / cache binaries.
///
/// Distinguishes filesystem errors (path-tagged), envelope-level
/// validation (magic + schema), and the underlying `bincode` codec
/// errors. Consumers can match on `SchemaMismatch` to detect "needs
/// re-bake" without string-parsing.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{op} {}: {source}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{label} too short ({len} bytes)")]
    MagicTooShort { label: &'static str, len: usize },
    #[error("{label} magic mismatch")]
    MagicMismatch { label: &'static str },
    #[error("{label} schema {found} expected {expected}, re-bake required")]
    SchemaMismatch {
        label: &'static str,
        found: u16,
        expected: u16,
    },
    #[error("bincode decode: {0}")]
    BincodeDecode(#[from] bincode::error::DecodeError),
    #[error("bincode encode: {0}")]
    BincodeEncode(#[from] bincode::error::EncodeError),
}

// ─── IO ──────────────────────────────────────────────────────────────────

/// Read the convert artifact.
pub fn read(path: &Path) -> Result<AssetDb, StoreError> {
    let bytes = std::fs::read(path).map_err(|source| StoreError::Io {
        op: "read asset-db",
        path: path.to_path_buf(),
        source,
    })?;
    decode(&bytes)
}

pub fn decode(bytes: &[u8]) -> Result<AssetDb, StoreError> {
    let body = check_magic(bytes, MAGIC, "asset-db")?;
    let cfg = bincode::config::standard();
    let (db, _): (AssetDb, _) = bincode::decode_from_slice(body, cfg)?;
    if db.schema_version != SCHEMA_VERSION {
        return Err(StoreError::SchemaMismatch {
            label: "asset-db",
            found: db.schema_version,
            expected: SCHEMA_VERSION,
        });
    }
    Ok(db)
}

/// Write the convert artifact, creating parent dirs as needed.
pub fn write(path: &Path, db: &AssetDb) -> Result<(), StoreError> {
    write_bytes(path, &encode(db)?)
}

pub fn encode(db: &AssetDb) -> Result<Vec<u8>, StoreError> {
    encode_with_magic(db, MAGIC)
}

/// Read the bake-only cache. Caller decides what to do on error —
/// the bake treats any error here as "no cache, parse from scratch".
pub fn read_cache(path: &Path) -> Result<BakeCache, StoreError> {
    let bytes = std::fs::read(path).map_err(|source| StoreError::Io {
        op: "read cache",
        path: path.to_path_buf(),
        source,
    })?;
    decode_cache(&bytes)
}

pub fn decode_cache(bytes: &[u8]) -> Result<BakeCache, StoreError> {
    let body = check_magic(bytes, CACHE_MAGIC, "asset-db.cache")?;
    let cfg = bincode::config::standard();
    let (cache, _): (BakeCache, _) = bincode::decode_from_slice(body, cfg)?;
    if cache.schema_version != SCHEMA_VERSION {
        return Err(StoreError::SchemaMismatch {
            label: "asset-db.cache",
            found: cache.schema_version,
            expected: SCHEMA_VERSION,
        });
    }
    Ok(cache)
}

pub fn write_cache(path: &Path, cache: &BakeCache) -> Result<(), StoreError> {
    write_bytes(path, &encode_cache(cache)?)
}

pub fn encode_cache(cache: &BakeCache) -> Result<Vec<u8>, StoreError> {
    encode_with_magic(cache, CACHE_MAGIC)
}

fn encode_with_magic<T: Encode>(value: &T, magic: [u8; 8]) -> Result<Vec<u8>, StoreError> {
    let cfg = bincode::config::standard();
    let body = bincode::encode_to_vec(value, cfg)?;
    let mut out = Vec::with_capacity(magic.len() + body.len());
    out.extend_from_slice(&magic);
    out.extend_from_slice(&body);
    Ok(out)
}

fn check_magic<'a>(
    bytes: &'a [u8],
    magic: [u8; 8],
    label: &'static str,
) -> Result<&'a [u8], StoreError> {
    if bytes.len() < magic.len() {
        return Err(StoreError::MagicTooShort {
            label,
            len: bytes.len(),
        });
    }
    let (head, body) = bytes.split_at(magic.len());
    if head != magic {
        return Err(StoreError::MagicMismatch { label });
    }
    Ok(body)
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
            op: "create dir",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, bytes).map_err(|source| StoreError::Io {
        op: "write",
        path: path.to_path_buf(),
        source,
    })?;
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

    /// Schema version is bumped on every breaking layout change. A
    /// downgrade-shaped payload (correct magic but lower version)
    /// must hard-fail decoding so the bake re-parses from scratch
    /// rather than serving structurally wrong sub-asset rows.
    #[test]
    fn schema_version_downgrade_hard_fails() {
        let mut db = AssetDb::new();
        db.schema_version = SCHEMA_VERSION.saturating_sub(1);
        let bytes = encode(&db).unwrap();
        let err = decode(&bytes).unwrap_err().to_string();
        assert!(
            err.contains("schema") && err.contains("re-bake"),
            "expected schema-version error, got: {err}",
        );
    }

    /// Schema version upgrades (an envelope from a *newer* unity-assetdb
    /// build) also hard-fail — consumer can't safely interpret
    /// fields it doesn't know about. Same error path as downgrades.
    #[test]
    fn schema_version_upgrade_hard_fails() {
        let mut db = AssetDb::new();
        db.schema_version = SCHEMA_VERSION + 1;
        let bytes = encode(&db).unwrap();
        let err = decode(&bytes).unwrap_err().to_string();
        assert!(err.contains("schema"), "expected schema-version error, got: {err}");
    }

    /// Cache schema mismatch is detected the same way — a v5 cache
    /// against a v6 reader must trip the error path so the bake
    /// rebuilds the cache from scratch instead of serving stale rows.
    #[test]
    fn cache_schema_version_mismatch_hard_fails() {
        let mut c = BakeCache::new();
        c.schema_version = SCHEMA_VERSION.saturating_sub(1);
        let bytes = encode_cache(&c).unwrap();
        let err = decode_cache(&bytes).unwrap_err().to_string();
        assert!(err.contains("schema"), "expected schema mismatch, got: {err}");
    }

    /// `SubAsset.class_id` is a v5+ field. Pin that a round-trip
    /// through the encoder preserves it — the read path uses this
    /// value as the discriminator for sub-asset namespacing rules,
    /// so silent truncation would be a hard-to-spot correctness bug.
    #[test]
    fn subasset_class_id_round_trips() {
        let mut db = AssetDb::new();
        db.entries.push(AssetEntry {
            guid: 0xa0_u128,
            asset_type: AssetType::native(ClassId::AnimatorController),
            name: "Foo".into(),
            sub_assets: vec![
                SubAsset {
                    file_id: 9100000,
                    class_id: ClassId::AnimatorController as u32,
                    name: "Foo_self".into(),
                },
                SubAsset {
                    file_id: -123456789,
                    class_id: 1102, // AnimatorState
                    name: "Idle".into(),
                },
            ],
            hint: "Assets/Foo.controller".into(),
        });
        db.sort();

        let bytes = encode(&db).unwrap();
        let back = decode(&bytes).unwrap();
        let entry = back.find_by_guid(0xa0_u128).unwrap();
        assert_eq!(entry.sub_assets.len(), 2);
        let self_sub = entry
            .sub_assets
            .iter()
            .find(|s| &*s.name == "Foo_self")
            .unwrap();
        assert_eq!(self_sub.class_id, ClassId::AnimatorController as u32);
        let idle = entry.sub_assets.iter().find(|s| &*s.name == "Idle").unwrap();
        assert_eq!(idle.class_id, 1102);
    }
}
