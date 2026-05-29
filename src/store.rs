//! On-disk schema for the convert artifact.
//!
//! One file under the consumer-chosen out-dir (commonly
//! `<project>/Library/<consumer>/`):
//!
//! - `asset-db.bin` — convert artifact. Lean: per-entry guid, asset type,
//!   name, sub-assets. Sorted by guid for O(log n) binary-search lookup.
//!   Header carries an opaque Watchman clock token (see [[refresh.md]])
//!   that drives incremental updates without a sidecar cache file.
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
///      Pre-v5 bakes are unreadable; re-bake required after upgrading.
/// - v6: `.prefab`/`.controller`/`.anim`/`.mixer`/`.playable` sub-asset
///   rows now exclude the GameObject-tree structural classes (GameObject,
///   Transform, RectTransform, MonoBehaviour-as-component scoped to
///   `.prefab` only). Pre-v6 caches carry leaked `'@<name>'` sub-asset
///   rows for child GOs that would re-emerge on warm bakes; the bump
///   invalidates them.
/// - v7: mtime-based `asset-db.cache.bin` deleted. Cache invalidation
///   moves to Watchman; an opaque clock token now lives in the `AssetDb`
///   header (`watchman_clock: Option<String>`). The `CACHE_MAGIC`
///   constant, `BakeCache`/`CachedEntry`/`CachedAssetType` types, and
///   `{read,write,encode,decode}_cache` helpers are all gone. See
///   [[refresh.md]] for the auto-refresh pipeline.
pub const SCHEMA_VERSION: u16 = 7;

/// File magic — first 8 bytes. `b"UADBIN__"`.
pub const MAGIC: [u8; 8] = *b"UADBIN__";

/// Type of a Unity asset.
///
/// `Native(classId)` for built-in types (Sprite, Prefab, Texture2D, …).
/// `Script(idx)` for MonoBehaviour-backed assets — `idx` indexes into
/// [`AssetDb::script_types`], whose entries are u128 script GUIDs that
/// match the `guid` field of the corresponding `.cs.meta`.
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
    /// `Packages/com.boxcat.libs/Bar.mixer`). Lets downstream consumers
    /// locate assets by guid without re-walking the project tree.
    pub hint: Box<str>,
}

/// Whole-database envelope. `entries` is sorted by `guid` so convert-time
/// lookups are `binary_search_by_key`.
#[derive(Debug, Clone, Default, Encode, Decode)]
pub struct AssetDb {
    pub schema_version: u16,
    /// Opaque Watchman clock token from the bake/refresh that produced
    /// this `entries` snapshot. `None` means "never refreshed by
    /// Watchman" (first-ever full bake on this machine, Watchman absent,
    /// or a non-Watchman code path wrote the bin). The next call to
    /// [`crate::refresh::refresh`] treats `None` exactly like a
    /// `Fresh` reply from Watchman: full-bake to seed a clock.
    ///
    /// Kept opaque per the Watchman protocol — we never parse the
    /// inside. See [docs/refresh.md](../../docs/refresh.md).
    pub watchman_clock: Option<String>,
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

    /// O(n) scan by hint (project-relative path). Used by `register` and
    /// the `guid <path>` CLI lookup. Linear because `entries` is sorted by
    /// guid, not hint — a secondary index would cost ~hundreds of KB of
    /// transient state per query for an op that runs once per CLI
    /// invocation.
    pub fn find_by_hint(&self, hint: &str) -> Option<&AssetEntry> {
        self.entries.iter().find(|e| &*e.hint == hint)
    }

    /// Binary-search insert by guid; overwrites on collision. Caller
    /// must ensure `AssetType::Script(idx)` references a valid
    /// `script_types` slot (see [`Self::intern_script`]).
    pub fn insert_sorted(&mut self, entry: AssetEntry) {
        match self.entries.binary_search_by_key(&entry.guid, |e| e.guid) {
            Ok(idx) => self.entries[idx] = entry,
            Err(idx) => self.entries.insert(idx, entry),
        }
    }

    /// Resolve `AssetType::Script(idx)` to its underlying script GUID.
    /// Panics on out-of-range idx — that's a corrupt-file error, fail loud.
    pub fn script_guid(&self, idx: u32) -> u128 {
        self.script_types[idx as usize]
    }

    /// Intern `guid` and return its index. On sorted insertion, remaps
    /// existing entries' `AssetType::Script(i ≥ idx)` so prior indices
    /// stay valid. Bake's hot path has `entries.is_empty()`, so the
    /// remap is a no-op there; register relies on it to keep an
    /// incremental update coherent.
    pub fn intern_script(&mut self, guid: u128) -> u32 {
        match self.script_types.binary_search(&guid) {
            Ok(idx) => idx as u32,
            Err(idx) => {
                for entry in &mut self.entries {
                    if let AssetType::Script(i) = &mut entry.asset_type
                        && *i as usize >= idx
                    {
                        *i += 1;
                    }
                }
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

// ─── Path helpers ────────────────────────────────────────────────────────

/// Convert artifact filename.
pub const DB_FILENAME: &str = "asset-db.bin";

/// Advisory-lock filename. `bake` and `register` flock this to serialize
/// db rewrites — registration of a new asset must not race a parallel
/// walk that's about to overwrite `asset-db.bin`.
pub const LOCK_FILENAME: &str = ".asset-db.lock";

/// `<dir>/asset-db.bin`. Caller composes the directory convention
/// (e.g. `<project>/Library/unity-assetdb/`).
pub fn db_path(dir: &Path) -> PathBuf {
    dir.join(DB_FILENAME)
}

/// `<dir>/.asset-db.lock`. Sibling to [`db_path`].
pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join(LOCK_FILENAME)
}

/// How [`acquire_lock`] waits when the lock is contended.
pub enum LockWait {
    /// Try once; return `WouldBlock` immediately on contention.
    TryOnce,
    /// Block until acquired (native blocking flock).
    Forever,
    /// Poll with 50 ms ticks until acquired or `Duration` elapses.
    Until(std::time::Duration),
}

/// Acquire an exclusive advisory flock on `<dir>/.asset-db.lock`. Returns
/// a guard that releases on drop. Shared by bake and register.
///
/// Backed by [`std::fs::File::lock`]/[`std::fs::File::try_lock`] (stable
/// since Rust 1.89): `flock(2)` on Unix, `LockFileEx` on Windows.
pub fn acquire_lock(dir: &Path, wait: LockWait) -> Result<LockGuard, StoreError> {
    let path = lock_path(dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| StoreError::Io {
            op: "open lock",
            path: path.clone(),
            source,
        })?;
    let io_err = |op, source| StoreError::Io {
        op,
        path: path.clone(),
        source,
    };
    match wait {
        LockWait::TryOnce => {
            // `register.rs` matches `source.kind() == WouldBlock` to detect
            // contention, so the `WouldBlock` discriminant must survive the
            // `TryLockError → io::Error` adapter here.
            file.try_lock().map_err(|e| io_err("try-lock", try_lock_to_io(e)))?;
        }
        LockWait::Forever => {
            file.lock().map_err(|source| io_err("lock", source))?;
        }
        LockWait::Until(timeout) => {
            let start = std::time::Instant::now();
            let tick = std::time::Duration::from_millis(50);
            loop {
                if file.try_lock().is_ok() {
                    break;
                }
                if start.elapsed() >= timeout {
                    return Err(io_err(
                        "lock timeout",
                        std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            format!("could not acquire lock after {timeout:?}"),
                        ),
                    ));
                }
                std::thread::sleep(tick);
            }
        }
    }
    Ok(LockGuard { file })
}

fn try_lock_to_io(e: std::fs::TryLockError) -> std::io::Error {
    match e {
        std::fs::TryLockError::WouldBlock => std::io::ErrorKind::WouldBlock.into(),
        std::fs::TryLockError::Error(e) => e,
    }
}

/// RAII guard for the advisory flock acquired by [`acquire_lock`]. POSIX
/// flock releases on FD close; the explicit unlock here is belt-and-braces.
pub struct LockGuard {
    file: std::fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────

/// Errors from reading/writing `asset-db.bin`.
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

/// Atomic write: stage payload in a sibling tempfile, `fsync`, then
/// `rename` over the destination. Concurrent readers always see either
/// the old or the new bytes; a process crash mid-write leaves the prior
/// file intact (and [`tempfile::NamedTempFile`] cleans up its random-named
/// stage file on drop).
///
/// Mitigates the "register clobbers a mid-flight bake" race that the
/// advisory flock on the out_dir is the primary defense against.
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    use std::io::Write;

    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
        op: "create dir",
        path: parent.to_path_buf(),
        source,
    })?;

    // Stage in the destination directory so `persist` is a same-fs `rename(2)`.
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|source| StoreError::Io {
        op: "create tmp",
        path: parent.to_path_buf(),
        source,
    })?;
    tmp.write_all(bytes).map_err(|source| StoreError::Io {
        op: "write tmp",
        path: tmp.path().to_path_buf(),
        source,
    })?;
    tmp.as_file().sync_all().map_err(|source| StoreError::Io {
        op: "fsync tmp",
        path: tmp.path().to_path_buf(),
        source,
    })?;
    tmp.persist(path).map_err(|e| StoreError::Io {
        op: "rename",
        path: path.to_path_buf(),
        source: e.error,
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

    /// Round-trip of the new `watchman_clock` header field. The clock
    /// is opaque per the Watchman protocol — we only verify equality.
    /// Specifically pins that an `Option<String>` round-trips with both
    /// `Some` and `None`, since bincode's representation differs.
    #[test]
    fn watchman_clock_round_trips() {
        let mut db = AssetDb::new();
        db.watchman_clock = Some("c:1789432100:42:1:7".to_owned());
        let bytes = encode(&db).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.watchman_clock.as_deref(), Some("c:1789432100:42:1:7"));

        let none_db = AssetDb::new();
        let bytes = encode(&none_db).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.watchman_clock, None);
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

    /// `write_bytes` is atomic via stage-and-rename; on success no
    /// intermediate file may remain in the destination directory.
    /// Pins this contract so the next implementation swap can't
    /// regress to a stage file that leaks (the previous
    /// `.tmp.<pid>` scheme would orphan on process crash; the
    /// modern `tempfile`-backed version cleans up on drop).
    #[test]
    fn write_bytes_leaves_no_temp_sibling_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset-db.bin");
        write(&path, &AssetDb::new()).unwrap();

        let siblings: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            siblings,
            vec![std::ffi::OsString::from("asset-db.bin")],
            "post-write dir should contain only the final bin; got {siblings:?}",
        );
    }

    /// `acquire_lock(TryOnce)` errs while another guard holds the
    /// flock, and succeeds once that guard drops. Pins the
    /// contention contract independently of the underlying
    /// implementation (currently `std::fs::File::try_lock`, formerly
    /// `fs2::FileExt::try_lock_exclusive`).
    #[test]
    fn acquire_lock_try_once_blocks_then_releases() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_lock(dir.path(), LockWait::TryOnce).expect("first acquire");
        match acquire_lock(dir.path(), LockWait::TryOnce) {
            Ok(_) => panic!("second acquire must fail while first holds"),
            Err(StoreError::Io { source, .. }) => {
                assert_eq!(source.kind(), std::io::ErrorKind::WouldBlock);
            }
            Err(e) => panic!("expected Io error on contention, got {e:?}"),
        }
        drop(first);
        let _second = acquire_lock(dir.path(), LockWait::TryOnce)
            .expect("acquire after first dropped");
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
