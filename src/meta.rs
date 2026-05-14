//! Parser for Unity `.meta` sidecar files.
//!
//! Pulls top-level GUID, sprite-sheet sub-asset names, and the
//! TextureImporter mode fields the bake needs to recognize Single-mode
//! Sprite textures.

/// Errors from `.meta` parsing.
#[derive(Debug, thiserror::Error)]
pub enum MetaParseError {
    #[error("missing or malformed `guid:` in .meta")]
    MissingGuid,
}

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
    /// Top-level importer block name (`DefaultImporter`, `TextureImporter`,
    /// `PrefabImporter`, `NativeFormatImporter`, …) — the first un-indented
    /// `<Name>Importer:` line. None when the `.meta` carries no importer
    /// block (legacy / hand-written metas).
    pub importer: Option<String>,
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
pub fn parse(text: &str) -> Result<MetaInfo, MetaParseError> {
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
        let trimmed_left = line.trim_start();

        // Top-level importer block: un-indented `<Name>Importer:` line.
        // Caught before the `trim_start`-keyed scans below because indent
        // is the discriminator (indented `Importer:` lines under e.g. a
        // `Sprite:` sub-block would false-match without it).
        if info.importer.is_none()
            && line.len() == trimmed_left.len()
            && let Some(name) = line.strip_suffix("Importer:")
            && !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric())
        {
            info.importer = Some(format!("{name}Importer"));
        }

        // Top-level key scans — INDEPENDENT of `in_sprites` state. Each
        // matches only when the line (after left-trim) starts with the
        // key, so a `- name: textureType: …` sprite entry can't false-
        // match (it starts with `- `, not `textureType:`).
        if !have_guid
            && let Some(rest) = trimmed_left.strip_prefix("guid:")
        {
            let hex = rest.trim();
            if hex.len() == 32
                && let Ok(g) = u128::from_str_radix(hex, 16)
            {
                info.guid = g;
                have_guid = true;
            }
        } else if info.texture_type.is_none()
            && let Some(rest) = trimmed_left.strip_prefix("textureType:")
        {
            info.texture_type = rest.trim().parse().ok();
        } else if info.sprite_mode.is_none()
            && let Some(rest) = trimmed_left.strip_prefix("spriteMode:")
        {
            info.sprite_mode = rest.trim().parse().ok();
        }

        // SpriteSheet sub-list — separate state machine. Top-level keys
        // above (textureType/spriteMode) sit at SHALLOWER indent than
        // sprite-entry contents in Unity's output, but we don't rely on
        // that here: the entry parser only matches `- ` list openers and
        // `name:` / `internalID:` lines, neither of which collides with
        // the top-level scans above.
        if in_sprites {
            let trimmed = line.trim();
            // Block ends when indent drops to root (sibling key with no
            // leading whitespace). Empty lines are tolerated.
            if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
                in_sprites = false;
                flush_sprite(&mut info.sprite_sheet, &mut cur_name, &mut cur_id);
            } else if let Some(after_dash) = trimmed.strip_prefix("- ") {
                flush_sprite(&mut info.sprite_sheet, &mut cur_name, &mut cur_id);
                absorb_kv(after_dash, &mut cur_name, &mut cur_id);
            } else {
                absorb_kv(trimmed, &mut cur_name, &mut cur_id);
            }
        } else if trimmed_left == "sprites:" {
            in_sprites = true;
        }
    }

    // Flush a trailing sprite entry if the file ended inside the block.
    flush_sprite(&mut info.sprite_sheet, &mut cur_name, &mut cur_id);

    if !have_guid {
        return Err(MetaParseError::MissingGuid);
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
    fn captures_top_level_importer_name() {
        // `DefaultImporter` is the discriminator the bake's last-resort
        // fallback keys on to classify `.swf` / `.fla` / other opaque
        // blobs as `DefaultAsset` (classID 1029).
        let text = "fileFormatVersion: 2
guid: 7d602c2080b53413fa393df6b2c0af43
DefaultImporter:
  externalObjects: {}
";
        let info = parse(text).unwrap();
        assert_eq!(info.importer.as_deref(), Some("DefaultImporter"));
    }

    #[test]
    fn captures_first_importer_only_indented_lines_skipped() {
        // Indented `<X>Importer:` keys (rare but appear in nested option
        // blocks) must not clobber the top-level importer name.
        let text = "fileFormatVersion: 2
guid: 7d602c2080b53413fa393df6b2c0af43
TextureImporter:
  platformSettings:
    overrideTextureSettings:
      buildTargetImporter:
";
        let info = parse(text).unwrap();
        assert_eq!(info.importer.as_deref(), Some("TextureImporter"));
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

    /// Single-pass parser correctness: a `name:` key sitting INSIDE the
    /// `sprites:` block must NOT leak out and pollute any top-level key
    /// match (e.g. an erroneous `guid:` parse off a sub-entry's `name:`).
    /// Regression for cross-block bleed-through — the kind of bug
    /// single-pass parsers fail at when they share state inappropriately.
    #[test]
    fn sprites_block_inner_keys_dont_leak_to_top_level() {
        let text = "fileFormatVersion: 2
guid: aabbccddaabbccddaabbccddaabbccdd
TextureImporter:
  spriteSheet:
    sprites:
    - name: textureType: 99
      internalID: 12345
  textureType: 8
  spriteMode: 2
";
        let info = parse(text).unwrap();
        // sub-entry's `name:` is a sprite name, NOT the importer's
        // textureType (which is 8 below the block, not 99 inside).
        assert_eq!(info.texture_type, Some(8));
        assert_eq!(info.sprite_mode, Some(2));
        assert_eq!(info.sprite_sheet.len(), 1);
        assert_eq!(info.sprite_sheet[0].0, 12345);
    }

    /// Key order independence: pre-2022 .meta files sometimes emit
    /// `textureType` BEFORE `spriteSheet:` rather than after. The
    /// single-pass scanner must pick both up regardless of order.
    #[test]
    fn key_order_independence() {
        let text = "fileFormatVersion: 2
guid: aabbccddaabbccddaabbccddaabbccdd
TextureImporter:
  textureType: 8
  spriteMode: 1
  spriteSheet:
    sprites: []
  externalObjects: {}
";
        let info = parse(text).unwrap();
        assert_eq!(info.texture_type, Some(8));
        assert_eq!(info.sprite_mode, Some(1));
        assert!(info.sprite_sheet.is_empty());
    }

    /// CRLF line endings (Unity-on-Windows project, occasional cross-OS
    /// fixture). `str::lines` already handles both \n and \r\n; pin that
    /// our key strip-prefix matches don't accidentally include a trailing
    /// \r in the value parse.
    #[test]
    fn crlf_line_endings() {
        let text = "fileFormatVersion: 2\r\nguid: 7d602c2080b53413fa393df6b2c0af43\r\nTextureImporter:\r\n  textureType: 8\r\n  spriteMode: 1\r\n";
        let info = parse(text).unwrap();
        assert_eq!(info.guid, 0x7d602c2080b53413fa393df6b2c0af43_u128);
        assert_eq!(info.texture_type, Some(8));
        assert_eq!(info.sprite_mode, Some(1));
    }

    /// Empty file → clean error (not a panic). `process_one` catches
    /// the parse error and surfaces it as a worker error; this test
    /// pins the boundary so the `?` propagation stays valid.
    #[test]
    fn empty_file_errors_cleanly() {
        let err = parse("").unwrap_err();
        assert!(matches!(err, MetaParseError::MissingGuid));
    }
}
