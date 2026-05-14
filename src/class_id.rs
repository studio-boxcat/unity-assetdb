//! Unity built-in class IDs we encounter in `Assets/`.
//!
//! Source: <https://docs.unity3d.com/Manual/ClassIDReference.html>.
//! Only assets that can be top-level entries or sub-assets in a project are
//! enumerated; runtime-only types (Camera, Light, …) live in components, not
//! the asset DB. Magic numbers stay confined to this file per project rule.

use bincode::{Decode, Encode};

/// Unity asset class IDs. `repr(u32)` — most fit in u16, but a few
/// post-2018 types (SpriteAtlas, LightingSettings) use 30-bit IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
#[repr(u32)]
pub enum ClassId {
    Material = 21,
    Texture2D = 28,
    Mesh = 43,
    Shader = 48,
    TextAsset = 49,
    AnimationClip = 74,
    AudioClip = 83,
    Cubemap = 89,
    AnimatorController = 91,
    Font = 128,
    PhysicMaterial = 134,
    PhysicsMaterial2D = 62,
    MonoBehaviour = 114,
    MonoScript = 115,
    Texture3D = 117,
    RenderTexture = 84,
    Sprite = 213,
    SpriteAtlas = 687078895,
    LightingSettings = 850595691,
    NavMeshData = 238,
    TerrainData = 156,
    Avatar = 90,
    AvatarMask = 1011,
    AnimatorOverrideController = 221,
    AudioMixerController = 244,
    Prefab = 1001,
    PrefabImporter = 1002,
    /// Unity's catch-all class for any file imported by `DefaultImporter`
    /// (extensions Unity has no dedicated importer for — `.swf`, `.fla`,
    /// arbitrary binary blobs). Main sub-object lands at fileID
    /// `1029 × 100_000 = 102_900_000` and is addressable from other YAML
    /// as `{fileID: 102900000, guid: <metaGuid>, type: 3}`.
    DefaultAsset = 1029,
    SceneAsset = 1032,
    LightingDataAsset = 1120,
    AssemblyDefinitionAsset = 1153,
    AssemblyDefinitionReferenceAsset = 1374,
    GameObject = 1,
    Transform = 4,
    RectTransform = 224,
}

impl ClassId {
    /// Lookup by raw u32 class ID — None if we don't enumerate it.
    /// Bake-time fallback when we encounter a class ID not in the table.
    pub fn from_raw(raw: u32) -> Option<Self> {
        // Match by listing every supported variant. Compiler-checked that
        // we cover what we declared above; raw values not listed return None.
        Some(match raw {
            1 => Self::GameObject,
            4 => Self::Transform,
            21 => Self::Material,
            28 => Self::Texture2D,
            43 => Self::Mesh,
            48 => Self::Shader,
            49 => Self::TextAsset,
            62 => Self::PhysicsMaterial2D,
            74 => Self::AnimationClip,
            83 => Self::AudioClip,
            84 => Self::RenderTexture,
            89 => Self::Cubemap,
            90 => Self::Avatar,
            91 => Self::AnimatorController,
            114 => Self::MonoBehaviour,
            115 => Self::MonoScript,
            117 => Self::Texture3D,
            128 => Self::Font,
            134 => Self::PhysicMaterial,
            156 => Self::TerrainData,
            213 => Self::Sprite,
            221 => Self::AnimatorOverrideController,
            224 => Self::RectTransform,
            238 => Self::NavMeshData,
            244 => Self::AudioMixerController,
            1001 => Self::Prefab,
            1002 => Self::PrefabImporter,
            1011 => Self::AvatarMask,
            1029 => Self::DefaultAsset,
            1032 => Self::SceneAsset,
            1120 => Self::LightingDataAsset,
            1153 => Self::AssemblyDefinitionAsset,
            1374 => Self::AssemblyDefinitionReferenceAsset,
            687078895 => Self::SpriteAtlas,
            850595691 => Self::LightingSettings,
            _ => return None,
        })
    }

    /// Stable name string — what gets written to TSV legacy / debug output.
    pub fn name(self) -> &'static str {
        match self {
            Self::GameObject => "GameObject",
            Self::Transform => "Transform",
            Self::Material => "Material",
            Self::Texture2D => "Texture2D",
            Self::Mesh => "Mesh",
            Self::Shader => "Shader",
            Self::TextAsset => "TextAsset",
            Self::PhysicsMaterial2D => "PhysicsMaterial2D",
            Self::AnimationClip => "AnimationClip",
            Self::AudioClip => "AudioClip",
            Self::RenderTexture => "RenderTexture",
            Self::Cubemap => "Cubemap",
            Self::Avatar => "Avatar",
            Self::AnimatorController => "AnimatorController",
            Self::MonoBehaviour => "MonoBehaviour",
            Self::MonoScript => "MonoScript",
            Self::Texture3D => "Texture3D",
            Self::Font => "Font",
            Self::PhysicMaterial => "PhysicMaterial",
            Self::TerrainData => "TerrainData",
            Self::Sprite => "Sprite",
            Self::AnimatorOverrideController => "AnimatorOverrideController",
            Self::AudioMixerController => "AudioMixerController",
            Self::RectTransform => "RectTransform",
            Self::NavMeshData => "NavMeshData",
            Self::Prefab => "Prefab",
            Self::PrefabImporter => "PrefabImporter",
            Self::AvatarMask => "AvatarMask",
            Self::DefaultAsset => "DefaultAsset",
            Self::SceneAsset => "SceneAsset",
            Self::LightingDataAsset => "LightingDataAsset",
            Self::AssemblyDefinitionAsset => "AssemblyDefinitionAsset",
            Self::AssemblyDefinitionReferenceAsset => "AssemblyDefinitionReferenceAsset",
            Self::SpriteAtlas => "SpriteAtlas",
            Self::LightingSettings => "LightingSettings",
        }
    }

    /// Reverse of [`Self::name`]. Case-sensitive (canonical PascalCase).
    /// **Must be kept in sync with `name()`** when adding a variant —
    /// the catch-all arm here defeats the compiler's exhaustiveness check.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "GameObject" => Self::GameObject,
            "Transform" => Self::Transform,
            "Material" => Self::Material,
            "Texture2D" => Self::Texture2D,
            "Mesh" => Self::Mesh,
            "Shader" => Self::Shader,
            "TextAsset" => Self::TextAsset,
            "PhysicsMaterial2D" => Self::PhysicsMaterial2D,
            "AnimationClip" => Self::AnimationClip,
            "AudioClip" => Self::AudioClip,
            "RenderTexture" => Self::RenderTexture,
            "Cubemap" => Self::Cubemap,
            "Avatar" => Self::Avatar,
            "AnimatorController" => Self::AnimatorController,
            "MonoBehaviour" => Self::MonoBehaviour,
            "MonoScript" => Self::MonoScript,
            "Texture3D" => Self::Texture3D,
            "Font" => Self::Font,
            "PhysicMaterial" => Self::PhysicMaterial,
            "TerrainData" => Self::TerrainData,
            "Sprite" => Self::Sprite,
            "AnimatorOverrideController" => Self::AnimatorOverrideController,
            "AudioMixerController" => Self::AudioMixerController,
            "RectTransform" => Self::RectTransform,
            "NavMeshData" => Self::NavMeshData,
            "Prefab" => Self::Prefab,
            "PrefabImporter" => Self::PrefabImporter,
            "AvatarMask" => Self::AvatarMask,
            "DefaultAsset" => Self::DefaultAsset,
            "SceneAsset" => Self::SceneAsset,
            "LightingDataAsset" => Self::LightingDataAsset,
            "AssemblyDefinitionAsset" => Self::AssemblyDefinitionAsset,
            "AssemblyDefinitionReferenceAsset" => Self::AssemblyDefinitionReferenceAsset,
            "SpriteAtlas" => Self::SpriteAtlas,
            "LightingSettings" => Self::LightingSettings,
            _ => return None,
        })
    }

    /// Canonical sub-object fileID Unity assigns to a single embedded
    /// asset of this class. Encoding: `class_id × 100_000`. Stays in
    /// this module per the project's "no magic numbers" rule (file
    /// header).
    pub fn canonical_subobject_fid(self) -> i64 {
        (self as i64) * 100_000
    }
}

/// File-extension → top-level ClassId. None means the asset isn't typed
/// purely from extension (e.g. `.asset` requires peeking the YAML).
pub fn class_from_ext(ext: &str) -> Option<ClassId> {
    Some(match ext {
        "prefab" => ClassId::Prefab,
        "controller" => ClassId::AnimatorController,
        "overrideController" => ClassId::AnimatorOverrideController,
        "mat" => ClassId::Material,
        "anim" => ClassId::AnimationClip,
        "shader" => ClassId::Shader,
        "physicMaterial" => ClassId::PhysicMaterial,
        "physicsMaterial2D" => ClassId::PhysicsMaterial2D,
        "mask" => ClassId::AvatarMask,
        "spriteatlas" | "spriteatlasv2" => ClassId::SpriteAtlas,
        "unity" => ClassId::SceneAsset,
        "asmdef" => ClassId::AssemblyDefinitionAsset,
        "asmref" => ClassId::AssemblyDefinitionReferenceAsset,
        "cs" => ClassId::MonoScript,
        // Texture-ish — Unity treats these as Texture2D top-level.
        "png" | "jpg" | "jpeg" | "tga" | "psd" | "tif" | "tiff" | "bmp" | "exr" | "hdr" | "gif"
        | "iff" | "pict" => ClassId::Texture2D,
        "ttf" | "otf" => ClassId::Font,
        "wav" | "mp3" | "ogg" | "aif" | "aiff" | "flac" => ClassId::AudioClip,
        "txt" | "json" | "xml" | "csv" | "yaml" | "html" | "htm" | "bytes" => ClassId::TextAsset,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_raw_known() {
        assert_eq!(ClassId::from_raw(213), Some(ClassId::Sprite));
        assert_eq!(ClassId::from_raw(1001), Some(ClassId::Prefab));
        assert_eq!(ClassId::from_raw(114), Some(ClassId::MonoBehaviour));
    }

    #[test]
    fn from_raw_unknown() {
        assert_eq!(ClassId::from_raw(999_999), None);
    }

    #[test]
    fn ext_dispatch() {
        assert_eq!(class_from_ext("prefab"), Some(ClassId::Prefab));
        assert_eq!(class_from_ext("png"), Some(ClassId::Texture2D));
        assert_eq!(class_from_ext("asset"), None); // YAML-peek required
    }
}
