//! Parser for Unity `.meta` sidecar files.
//!
//! Pulls top-level GUID, sprite-sheet sub-asset names, and the
//! TextureImporter mode fields the bake needs to recognize Single-mode
//! Sprite textures.

use anyhow::{Context, Result};

/// `TextureImporter.textureType` enum — Unity only auto-generates a
/// Sprite sub-object when the importer is in Sprite mode.
pub const TEXTURE_TYPE_SPRITE: u32 = 8;

/// `TextureImporter.spriteMode` — Single produces one implicit Sprite
/// named after the texture file; Multiple uses the `sprites:` list.
pub const SPRITE_MODE_SINGLE: u32 = 1;

/// Parsed contents of a `.meta` file.
#[derive(Debug, Clone, Default)]
pub struct MetaInfo {
    /// 32-hex GUID parsed as u128.
    pub guid: u128,
    /// Texture sprite-sheet sub-assets, if the importer is TextureImporter
    /// with sprite mode = Multiple. `(file_id, name)` pairs.
    pub sprite_sheet: Vec<(i64, String)>,
    /// `TextureImporter.textureType`. None for non-texture importers.
    pub texture_type: Option<u32>,
    /// `TextureImporter.spriteMode`. None for non-texture importers.
    pub sprite_mode: Option<u32>,
}

/// Parse a `.meta` file's text contents.
///
/// Format reference: <https://docs.unity3d.com/Manual/SpecialFolders.html>
/// Robust enough for the YAML subset Unity emits — line-oriented, `key: value`,
/// without resorting to a full YAML parser.
///
/// Single-pass: walks every line once, picking off the guid, the
/// TextureImporter mode fields, and the spriteSheet `sprites:` list in
/// the same loop. Replaces the older four-scan-per-file shape — a
/// measurable cold-path win since `str::lines` is memchr-bound and
/// every redundant pass is wasted SIMD time.
pub fn parse(text: &str) -> Result<MetaInfo> {
    let mut info = MetaInfo::default();
    let mut have_guid = false;

    // SpriteSheet `sprites:` list cursor. `in_sprites` is true between the
    // `sprites:` key line and the indent drop that ends the block. Per-item
    // `cur_name` / `cur_id` accumulate the current entry; flushed on each
    // new `-` and at block exit.
    let mut in_sprites = false;
    let mut cur_name: Option<String> = None;
    let mut cur_id: Option<i64> = None;

    for line in text.lines() {
        // sprites-block handling has to come first — the block lives at a
        // deeper indent than the key scans below, and an early `continue`
        // here keeps the simple key scans from accidentally matching e.g.
        // a sub-`name:` inside a sprite entry as a top-level field.
        if in_sprites {
            let trimmed = line.trim();
            // Block ends when indent drops to root (sibling key with no
            // leading whitespace). Empty lines are tolerated.
            if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
                in_sprites = false;
                flush_sprite(&mut info.sprite_sheet, &mut cur_name, &mut cur_id);
                // Fall through — this line may itself be a key we want
                // (textureType / spriteMode appear after spriteSheet in
                // some importer versions).
            } else if let Some(after_dash) = trimmed.strip_prefix("- ") {
                // New list item — flush the previous and start a new one
                // with whatever k:v rides on the dash line.
                flush_sprite(&mut info.sprite_sheet, &mut cur_name, &mut cur_id);
                absorb_kv(after_dash, &mut cur_name, &mut cur_id);
                continue;
            } else {
                absorb_kv(trimmed, &mut cur_name, &mut cur_id);
                continue;
            }
        }

        let trimmed = line.trim_start();

        if !have_guid
            && let Some(rest) = trimmed.strip_prefix("guid:")
        {
            let hex = rest.trim();
            if hex.len() == 32
                && let Ok(g) = u128::from_str_radix(hex, 16)
            {
                info.guid = g;
                have_guid = true;
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("textureType:") {
            info.texture_type = rest.trim().parse().ok();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("spriteMode:") {
            info.sprite_mode = rest.trim().parse().ok();
            continue;
        }
        if trimmed == "sprites:" {
            in_sprites = true;
            continue;
        }
    }

    // Flush a trailing sprite entry if the file ended inside the block.
    flush_sprite(&mut info.sprite_sheet, &mut cur_name, &mut cur_id);

    if !have_guid {
        return Err(anyhow::anyhow!("missing or malformed `guid:` in .meta"))
            .context("parse meta");
    }
    Ok(info)
}

fn absorb_kv(line: &str, cur_name: &mut Option<String>, cur_id: &mut Option<i64>) {
    if let Some(rest) = line.strip_prefix("name:") {
        let s = rest.trim();
        if cur_name.is_none() && !s.is_empty() {
            *cur_name = Some(s.to_string());
        }
    } else if let Some(rest) = line.strip_prefix("internalID:") {
        *cur_id = rest.trim().parse().ok();
    }
}

fn flush_sprite(out: &mut Vec<(i64, String)>, name: &mut Option<String>, id: &mut Option<i64>) {
    if let (Some(n), Some(i)) = (name.take(), id.take())
        && !n.is_empty()
    {
        out.push((i, n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_guid() {
        let text = "fileFormatVersion: 2\nguid: 7d602c2080b53413fa393df6b2c0af43\n";
        let info = parse(text).unwrap();
        assert_eq!(info.guid, 0x7d602c2080b53413fa393df6b2c0af43_u128);
        assert!(info.sprite_sheet.is_empty());
    }

    #[test]
    fn rejects_short_guid() {
        let text = "guid: deadbeef\n";
        assert!(parse(text).is_err());
    }

    #[test]
    fn parses_sprite_sheet() {
        let text = "fileFormatVersion: 2
guid: 7d602c2080b53413fa393df6b2c0af43
TextureImporter:
  spriteSheet:
    sprites:
    - serializedVersion: 2
      name: spr_a
      internalID: 11111
      rect:
        serializedVersion: 2
    - serializedVersion: 2
      name: spr_b
      internalID: 22222
  spritePackingTag:
";
        let info = parse(text).unwrap();
        assert_eq!(
            info.sprite_sheet,
            vec![(11111, "spr_a".to_string()), (22222, "spr_b".to_string()),]
        );
    }

    #[test]
    fn parses_texture_type_and_sprite_mode() {
        let text = "fileFormatVersion: 2
guid: 7d602c2080b53413fa393df6b2c0af43
TextureImporter:
  textureType: 8
  spriteMode: 1
  spriteSheet:
    sprites: []
";
        let info = parse(text).unwrap();
        assert_eq!(info.texture_type, Some(8));
        assert_eq!(info.sprite_mode, Some(1));
    }

    #[test]
    fn missing_texture_fields_are_none() {
        let text = "fileFormatVersion: 2\nguid: 7d602c2080b53413fa393df6b2c0af43\n";
        let info = parse(text).unwrap();
        assert_eq!(info.texture_type, None);
        assert_eq!(info.sprite_mode, None);
    }
}
