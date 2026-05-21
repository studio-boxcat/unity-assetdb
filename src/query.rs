//! Read-only queries against a baked `asset-db.bin`.
//!
//! Library operations (the first five are 1:1 with the CLI
//! `unity-assetdb guid|path|find|list|alias`; [`find_by_hint`] is a helper
//! the `guid` CLI calls on substring fallback):
//! - [`guid_of_path`] — project-relative path → top-level entry.
//! - [`path_of_guid`] — GUID → top-level entry.
//! - [`find`] — case-insensitive substring match on `name`. Returns all hits.
//! - [`find_by_hint`] — case-insensitive substring match on `hint`
//!   (project-rel path). Powers the `guid` CLI fallback.
//! - [`list`] — full or type-filtered iterator.
//! - [`alias`] — exact-match name lookup.
//!
//! Misses on every CLI name/path lookup (`guid`/`path`/`find`/`alias`,
//! plus `usage <path>`) feed [`crate::suggest`].

use crate::class_id::ClassId;
use crate::store::{self, AssetDb, AssetEntry, AssetType, StoreError};
use std::path::Path;

/// Errors from the query layer.
///
/// `Store` surfaces typed read errors from [`crate::store`]; `BadGuidHex`
/// and `UnknownAssetType` are user-input errors; `NotFound` carries
/// partial-match suggestions for the CLI to print on stderr.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("invalid GUID `{0}` — expected 32 hex chars (hyphens tolerated)")]
    BadGuidHex(String),
    #[error("unknown asset type `{0}` — expected a ClassId name (e.g. `Sprite`, `Prefab`) or `Script:<32hex>`")]
    UnknownAssetType(String),
    #[error("{kind} not found: {query}")]
    NotFound {
        kind: &'static str,
        query: String,
        suggestions: Vec<String>,
    },
}

/// Load a baked `AssetDb` from `<out_dir>/asset-db.bin`.
pub fn open(out_dir: &Path) -> Result<AssetDb, QueryError> {
    Ok(store::read(&store::db_path(out_dir))?)
}

/// Path → top-level entry. Caller-supplied path is normalized — backslashes
/// become forward slashes (Windows compat with stored hints) and a leading
/// `./` is stripped.
pub fn guid_of_path<'a>(db: &'a AssetDb, rel_path: &str) -> Option<&'a AssetEntry> {
    let needle = normalize_hint(rel_path);
    db.find_by_hint(&needle)
}

/// Normalize a hint argument: `\\` → `/`, strip leading `./`.
pub fn normalize_hint(s: &str) -> String {
    let s = s.replace('\\', "/");
    s.strip_prefix("./").map(str::to_owned).unwrap_or(s)
}

/// GUID → top-level entry.
pub fn path_of_guid(db: &AssetDb, guid: u128) -> Option<&AssetEntry> {
    db.find_by_guid(guid)
}

/// Case-insensitive substring match on each entry's `name`. Returns all
/// hits in guid-sorted (input) order. ASCII fast path — asset names are
/// effectively always ASCII; if the needle has non-ASCII chars we fall
/// back to the Unicode `to_lowercase` path.
pub fn find<'a>(db: &'a AssetDb, pattern: &str) -> Vec<&'a AssetEntry> {
    find_by(db, pattern, |e| &e.name)
}

/// Case-insensitive substring match on each entry's `hint` (project-rel
/// path). Used by the `guid <pattern>` fallback when an exact path miss
/// turns the arg into a substring query.
pub fn find_by_hint<'a>(db: &'a AssetDb, pattern: &str) -> Vec<&'a AssetEntry> {
    let normalized = normalize_hint(pattern);
    find_by(db, &normalized, |e| &e.hint)
}

fn find_by<'a, F>(db: &'a AssetDb, pattern: &str, field: F) -> Vec<&'a AssetEntry>
where
    F: Fn(&'a AssetEntry) -> &'a str,
{
    if pattern.is_ascii() {
        let needle = pattern.as_bytes();
        db.entries
            .iter()
            .filter(|e| ascii_icase_contains(field(e).as_bytes(), needle))
            .collect()
    } else {
        let needle = pattern.to_lowercase();
        db.entries
            .iter()
            .filter(|e| field(e).to_lowercase().contains(&needle))
            .collect()
    }
}

fn ascii_icase_contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

/// Filter passed to [`list`].
#[derive(Debug, Clone, Copy)]
pub enum AssetTypeFilter {
    /// Native class ID (`Sprite`, `Prefab`, …).
    Native(u32),
    /// Script-backed type, keyed by the script's GUID. Resolves to
    /// `AssetType::Script(idx)` once mapped against `db.script_types`.
    Script(u128),
}

impl AssetTypeFilter {
    /// Parse `"Sprite"`, `"Prefab"`, …, or `"Script:<32hex>"`. Other ClassId
    /// names are looked up by [`ClassId::from_name`] (added below).
    pub fn parse(s: &str) -> Result<Self, QueryError> {
        if let Some(hex) = s.strip_prefix("Script:") {
            let g = parse_guid(hex).map_err(|_| QueryError::BadGuidHex(hex.to_owned()))?;
            return Ok(AssetTypeFilter::Script(g));
        }
        ClassId::from_name(s)
            .map(|c| AssetTypeFilter::Native(c as u32))
            .ok_or_else(|| QueryError::UnknownAssetType(s.to_owned()))
    }
}

/// Iterate entries, optionally filtered by type.
///
/// `Script(<hex>)` returns nothing when the script GUID is absent from
/// `db.script_types` — that's the "no asset is backed by this script"
/// case, not an error.
pub fn list<'a>(
    db: &'a AssetDb,
    filter: Option<AssetTypeFilter>,
) -> Box<dyn Iterator<Item = &'a AssetEntry> + 'a> {
    let Some(filter) = filter else {
        return Box::new(db.entries.iter());
    };
    match filter {
        AssetTypeFilter::Native(n) => Box::new(
            db.entries
                .iter()
                .filter(move |e| matches!(e.asset_type, AssetType::Native(x) if x == n)),
        ),
        AssetTypeFilter::Script(g) => {
            let Ok(idx) = db.script_types.binary_search(&g) else {
                return Box::new(std::iter::empty());
            };
            let idx_u32 = idx as u32;
            Box::new(
                db.entries
                    .iter()
                    .filter(move |e| matches!(e.asset_type, AssetType::Script(x) if x == idx_u32)),
            )
        }
    }
}

/// Exact-match name lookup. Returns ALL entries with this name (the
/// asset DB allows two entries with the same name when their
/// `asset_type` differs — see `store.rs` v5 history).
pub fn alias<'a>(db: &'a AssetDb, name: &str) -> Vec<&'a AssetEntry> {
    db.entries.iter().filter(|e| &*e.name == name).collect()
}

/// Parse a 32-hex GUID. Hyphens and uppercase tolerated. Fast path for
/// the no-hyphen common case skips the strip-allocation.
pub fn parse_guid(s: &str) -> Result<u128, QueryError> {
    let bad = || QueryError::BadGuidHex(s.to_owned());
    if s.len() == 32 && !s.contains('-') {
        return u128::from_str_radix(s, 16).map_err(|_| bad());
    }
    let cleaned: String = s.chars().filter(|c| *c != '-').collect();
    if cleaned.len() != 32 {
        return Err(bad());
    }
    u128::from_str_radix(&cleaned, 16).map_err(|_| bad())
}

/// Format a `u128` GUID as 32-char lowercase hex.
pub fn format_guid(g: u128) -> String {
    format!("{g:032x}")
}

/// Render `AssetType` as a human-readable string for output rows.
/// `Native(<known>)` → ClassId name; `Native(<unknown>)` → `Native:<id>`;
/// `Script(idx)` → `Script:<32-hex script guid>`.
pub fn asset_type_str(t: AssetType, db: &AssetDb) -> String {
    match t {
        AssetType::Native(n) => match ClassId::from_raw(n) {
            Some(c) => c.name().to_owned(),
            None => format!("Native:{n}"),
        },
        AssetType::Script(idx) => format!("Script:{}", format_guid(db.script_guid(idx))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_guid_accepts_lowercase_uppercase_hyphens() {
        let g = parse_guid("aabbccdd-aabbccdd-aabbccdd-aabbccdd").unwrap();
        assert_eq!(g, 0xaabbccddaabbccddaabbccddaabbccdd_u128);
        let g2 = parse_guid("AABBCCDDAABBCCDDAABBCCDDAABBCCDD").unwrap();
        assert_eq!(g, g2);
    }

    #[test]
    fn parse_guid_rejects_short() {
        assert!(parse_guid("deadbeef").is_err());
    }

    #[test]
    fn normalize_hint_strips_dot_slash_and_backslash() {
        assert_eq!(normalize_hint("Assets\\Foo.prefab"), "Assets/Foo.prefab");
        assert_eq!(normalize_hint("./Assets/Foo.prefab"), "Assets/Foo.prefab");
    }

    #[test]
    fn type_filter_parses_native_and_script() {
        assert!(matches!(
            AssetTypeFilter::parse("Sprite").unwrap(),
            AssetTypeFilter::Native(213)
        ));
        match AssetTypeFilter::parse("Script:aabbccddaabbccddaabbccddaabbccdd").unwrap() {
            AssetTypeFilter::Script(g) => assert_eq!(g, 0xaabbccddaabbccddaabbccddaabbccdd_u128),
            _ => panic!(),
        }
    }

    #[test]
    fn type_filter_unknown_kind() {
        assert!(matches!(
            AssetTypeFilter::parse("Bogus"),
            Err(QueryError::UnknownAssetType(_))
        ));
    }
}
