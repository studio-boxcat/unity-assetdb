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
pub fn parse(text: &str) -> Result<MetaInfo> {
    let guid = parse_guid(text).context("missing or malformed `guid:` in .meta")?;
    let sprite_sheet = parse_sprite_sheet(text);
    let texture_type = parse_u32_field(text, "textureType:");
    let sprite_mode = parse_u32_field(text, "spriteMode:");
    Ok(MetaInfo {
        guid,
        sprite_sheet,
        texture_type,
        sprite_mode,
    })
}

/// First-match scan for a u32 scalar at any indent. `key` includes the
/// trailing `:` (e.g. `"textureType:"`) — caller's responsibility to
/// pick a key unique to TextureImporter so the whole-file scan can't
/// false-match a longer key with the same prefix.
fn parse_u32_field(text: &str, key: &str) -> Option<u32> {
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix(key) {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn parse_guid(text: &str) -> Option<u128> {
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("guid:") {
            let hex = rest.trim();
            if hex.len() == 32 {
                return u128::from_str_radix(hex, 16).ok();
            }
        }
    }
    None
}

/// Walks the `spriteSheet:` block under `TextureImporter:` and the
/// follow-on `sprites:` list, capturing each sprite's `name` + `internalID`.
/// Stays line-oriented; Unity emits a stable indented form.
///
/// List-item detection: any non-empty line whose first non-space char is
/// `-` opens a new entry, regardless of which key follows the dash. We
/// flush the previous entry on each new dash and at end-of-block.
fn parse_sprite_sheet(text: &str) -> Vec<(i64, String)> {
    let mut out: Vec<(i64, String)> = Vec::new();
    let mut in_sprites = false;
    let mut cur_name: Option<String> = None;
    let mut cur_id: Option<i64> = None;

    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed == "sprites:" {
            in_sprites = true;
            continue;
        }
        if !in_sprites {
            continue;
        }

        // Block ends when indent drops to root (sibling key with no leading
        // whitespace). Empty lines are tolerated.
        if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
            break;
        }

        // New list item. Strip the dash; the rest of the line is the
        // first key:value pair of the entry (often `serializedVersion: 2`
        // or `name: foo`).
        if let Some(after_dash) = trimmed.strip_prefix("- ") {
            flush(&mut out, &mut cur_name, &mut cur_id);
            absorb_kv(after_dash, &mut cur_name, &mut cur_id);
            continue;
        }

        absorb_kv(trimmed, &mut cur_name, &mut cur_id);
    }
    flush(&mut out, &mut cur_name, &mut cur_id);
    out
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

fn flush(out: &mut Vec<(i64, String)>, name: &mut Option<String>, id: &mut Option<i64>) {
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
