//! Parser for Unity asset YAML files (`.asset`, `.prefab`, `.controller`, …).
//!
//! Produces:
//! - `top_class_id` — first `--- !u!<classID> &<fileID>` doc header.
//! - `script_guid` — `m_Script.guid` of the top doc when class is MonoBehaviour.
//! - `sub_assets` — every `--- !u!<id> &<fileID>` after the first, paired
//!   with that doc's `m_Name`.
//!
//! Stays line-oriented; faster and lighter than full YAML parsing for this
//! narrow shape. See [[json-schema.md]] for what each field means in
//! Unity's emitted YAML.

use anyhow::Result;

#[derive(Debug, Clone, Default)]
pub struct AssetInfo {
    /// First doc's class ID. None on a malformed/empty asset.
    pub top_class_id: Option<u32>,
    /// First doc's fileID (the `&NNN` after the class ID).
    pub top_file_id: Option<i64>,
    /// `m_Script.guid` for the top doc when it's MonoBehaviour-class (114).
    pub script_guid: Option<u128>,
    /// Sub-asset docs after the first. `(class_id, file_id, m_Name)`.
    /// `m_Name` is empty when the sub-doc has none — caller decides how to handle.
    pub sub_assets: Vec<SubAssetEntry>,
}

#[derive(Debug, Clone)]
pub struct SubAssetEntry {
    pub class_id: u32,
    pub file_id: i64,
    pub name: String,
}

/// What to capture from the asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    /// First doc only — top class ID + (if MonoBehaviour) m_Script.guid.
    /// Bails out as soon as it sees the second `---` doc header. Use for
    /// types that don't expose addressable sub-assets (`.prefab`,
    /// `.controller`, `.mat`, `.anim`, `.mask`, `.unity`).
    TopOnly,
    /// Full multi-doc scan: top doc + every sub-doc's `(class_id, fileID, m_Name)`.
    /// Use for types that legitimately host sub-assets (`.asset`, `.spriteatlas`).
    WithSubAssets,
}

/// Parse the YAML text. We only walk doc headers and a couple of well-known
/// keys per doc; full parsing is overkill for this shape.
pub fn parse(text: &str, mode: ParseMode) -> Result<AssetInfo> {
    let mut info = AssetInfo::default();

    // Doc structure: `--- !u!<id> &<fileID> [stripped]` opens a doc; lines
    // after the next non-doc-header line and before the following `---` are
    // its body. We collect (class, file_id, name, script_guid) per doc.
    struct DocAccum {
        class_id: u32,
        file_id: i64,
        name: Option<String>,
        script_guid: Option<u128>,
    }

    let mut doc_idx: usize = 0;
    let mut cur: Option<DocAccum> = None;

    let flush = |info: &mut AssetInfo, doc_idx: usize, d: DocAccum| {
        if doc_idx == 0 {
            info.top_class_id = Some(d.class_id);
            info.top_file_id = Some(d.file_id);
            info.script_guid = d.script_guid;
        } else {
            // class_id is propagated through to `SubAsset` in store.rs —
            // critical for prefab-embedded sub-docs whose hashed negative
            // fileID can't be reverse-derived to a class via the
            // `file_id = class * 100_000` heuristic.
            info.sub_assets.push(SubAssetEntry {
                class_id: d.class_id,
                file_id: d.file_id,
                name: d.name.unwrap_or_default(),
            });
        }
    };

    for line in text.lines() {
        if let Some((cls, fid)) = parse_doc_header(line) {
            if let Some(d) = cur.take() {
                flush(&mut info, doc_idx, d);
                doc_idx += 1;
                // TopOnly: stop the moment we've finished the first doc.
                if mode == ParseMode::TopOnly {
                    return Ok(info);
                }
            }
            cur = Some(DocAccum {
                class_id: cls,
                file_id: fid,
                name: None,
                script_guid: None,
            });
            continue;
        }

        let Some(d) = cur.as_mut() else { continue };

        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("m_Name:") {
            if d.name.is_none() {
                let s = rest.trim();
                if !s.is_empty() {
                    d.name = Some(s.to_string());
                }
            }
        } else if d.script_guid.is_none()
            && let Some(rest) = trimmed.strip_prefix("m_Script:")
        {
            // `m_Script: {fileID: …, guid: <hex32>, type: 3}` on one line
            d.script_guid = parse_inline_guid(rest);
        }
    }
    if let Some(d) = cur.take() {
        flush(&mut info, doc_idx, d);
    }
    Ok(info)
}

/// Match `--- !u!<id> &<fileID>` (with optional ` stripped` suffix).
/// Returns `(class_id, file_id)` or None.
fn parse_doc_header(line: &str) -> Option<(u32, i64)> {
    let rest = line.strip_prefix("--- !u!")?;
    let (cls_str, after) = rest.split_once(" &")?;
    let cls: u32 = cls_str.trim().parse().ok()?;
    let fid_str = after.split_whitespace().next()?;
    let fid: i64 = fid_str.parse().ok()?;
    Some((cls, fid))
}

/// Pull `guid: <hex32>` out of `{fileID: …, guid: ABC…, type: …}`.
fn parse_inline_guid(rest: &str) -> Option<u128> {
    let s = rest.trim();
    let s = s.trim_start_matches('{').trim_end_matches('}');
    for part in s.split(',') {
        let part = part.trim();
        if let Some(hex) = part.strip_prefix("guid:") {
            let hex = hex.trim();
            if hex.len() == 32 {
                return u128::from_str_radix(hex, 16).ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_top_only() {
        let text = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1001 &100100000
PrefabInstance:
  m_ObjectHideFlags: 0
";
        let info = parse(text, ParseMode::WithSubAssets).unwrap();
        assert_eq!(info.top_class_id, Some(1001));
        assert_eq!(info.top_file_id, Some(100100000));
        assert!(info.sub_assets.is_empty());
    }

    #[test]
    fn parses_monobehaviour_with_script_guid() {
        let text = "--- !u!114 &11400000
MonoBehaviour:
  m_ObjectHideFlags: 0
  m_Script: {fileID: 11500000, guid: 7d602c2080b53413fa393df6b2c0af43, type: 3}
  m_Name: TweenSeqDef
";
        let info = parse(text, ParseMode::WithSubAssets).unwrap();
        assert_eq!(info.top_class_id, Some(114));
        assert_eq!(
            info.script_guid,
            Some(0x7d602c2080b53413fa393df6b2c0af43_u128)
        );
    }

    #[test]
    fn top_only_skips_sub_docs() {
        // Same multi-doc input as `parses_sub_assets` but in TopOnly mode —
        // sub-asset list must be empty.
        let text = "--- !u!28 &2800000
Texture2D:
  m_Name: Sheet
--- !u!213 &21300000
Sprite:
  m_Name: spr_a
";
        let info = parse(text, ParseMode::TopOnly).unwrap();
        assert_eq!(info.top_class_id, Some(28));
        assert!(info.sub_assets.is_empty());
    }

    #[test]
    fn parses_sub_assets() {
        let text = "--- !u!28 &2800000
Texture2D:
  m_Name: Sheet
--- !u!213 &21300000
Sprite:
  m_Name: spr_a
--- !u!213 &21300002
Sprite:
  m_Name: spr_b
";
        let info = parse(text, ParseMode::WithSubAssets).unwrap();
        assert_eq!(info.top_class_id, Some(28));
        assert_eq!(info.sub_assets.len(), 2);
        assert_eq!(info.sub_assets[0].file_id, 21300000);
        assert_eq!(info.sub_assets[0].name, "spr_a");
        assert_eq!(info.sub_assets[1].name, "spr_b");
    }

    /// `asset::parse` is class-blind: every named sub-doc surfaces
    /// regardless of class. The extension-aware filter that drops
    /// GO-tree structural docs from prefabs lives in `bake::process_one`
    /// — pinning that here keeps the parser layer's contract clear.
    #[test]
    fn parses_keeps_all_named_subdocs_regardless_of_class() {
        let text = "--- !u!114 &11400000
MonoBehaviour:
  m_Name: TimelineAsset
--- !u!114 &-7938135556022269506
MonoBehaviour:
  m_Name: 'Animation Track (1)'
--- !u!1 &111111
GameObject:
  m_Name: '@SomeGo'
--- !u!74 &-444444
AnimationClip:
  m_Name: EmbeddedClip
";
        let info = parse(text, ParseMode::WithSubAssets).unwrap();
        // 3 named sub-docs (the class-114 top doc is the parent, not a sub).
        // The line-oriented parser preserves YAML quote literals — Unity's
        // typical output uses single-quoted strings for names with special
        // chars; the sanitize / strip-quote pass happens downstream.
        assert_eq!(info.sub_assets.len(), 3);
        assert_eq!(info.sub_assets[0].name, "'Animation Track (1)'");
        assert_eq!(info.sub_assets[1].name, "'@SomeGo'");
        assert_eq!(info.sub_assets[2].name, "EmbeddedClip");
    }
}
