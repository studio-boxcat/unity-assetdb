//! Unity engine-builtin asset GUIDs.
//!
//! Unity emits a small family of "builtin" asset GUIDs whose hex shape is
//! all-zeros except for a single bucket-discriminator character at offset
//! 16. These identify assets baked into the engine binary — default
//! resources (`'d'`), engine builtins (`'e'`), builtin extras (`'f'`) — and
//! never appear in a user's `Assets/` tree. A baked `asset-db.bin` won't
//! contain them; downstream tools (e.g. a YAML ↔ JSON converter) recognize
//! the shape and route through their own builtin lookup table instead of
//! hard-failing on a missing asset.
//!
//! The shape is `0000000000000000<bucket>000000000000000` — 16 zeros, one
//! hex bucket char, 15 zeros (total 32). This module owns the
//! domain-knowledge predicates; ref-grammar surface (the `$<builtin:NNN[:b]>`
//! JSON form pspec emits) stays in the consumer.

/// `true` iff `guid` matches the all-zero-with-one-bucket-hex builtin
/// shape: 32 chars, 16 leading zeros, any char at offset 16, 15 trailing
/// zeros. The bucket char is NOT validated as hex — callers that need
/// strict bucket validation pair this with [`bucket_of`].
pub fn is_unity_builtin_guid(guid: &str) -> bool {
    guid.len() == 32
        && guid[..16].chars().all(|c| c == '0')
        && guid[17..].chars().all(|c| c == '0')
}

/// Extract the bucket discriminator (single char at offset 16) from a
/// builtin guid. Returns `None` when `guid` is not a builtin shape — the
/// `is_unity_builtin_guid` invariant fails. Callers that have already
/// validated the shape can `unwrap`; downstream emission paths prefer
/// the option flavor.
pub fn bucket_of(guid: &str) -> Option<char> {
    if !is_unity_builtin_guid(guid) {
        return None;
    }
    guid.as_bytes().get(16).map(|&b| b as char)
}

/// Reconstruct a builtin guid from a bucket discriminator. The
/// returned string always has the shape `is_unity_builtin_guid`
/// recognizes; the bucket char is written verbatim — caller validates
/// hex-ness if needed (the ref-grammar parser rejects non-hex on the
/// way in).
pub fn guid_from_bucket(bucket: char) -> String {
    let mut s = String::with_capacity(32);
    s.push_str("0000000000000000");
    s.push(bucket);
    s.push_str("000000000000000");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_canonical_buckets() {
        assert!(is_unity_builtin_guid("0000000000000000e000000000000000"));
        assert!(is_unity_builtin_guid("0000000000000000d000000000000000"));
        assert!(is_unity_builtin_guid("0000000000000000f000000000000000"));
    }

    #[test]
    fn rejects_non_builtin_shapes() {
        // Real-asset guid.
        assert!(!is_unity_builtin_guid("aabbccddaabbccddaabbccddaabbccdd"));
        // Wrong length.
        assert!(!is_unity_builtin_guid("0000000000000000e00000000000000"));
        // Non-zero outside the bucket slot.
        assert!(!is_unity_builtin_guid("000000000000000ae000000000000000"));
        assert!(!is_unity_builtin_guid("0000000000000000e000000000000001"));
    }

    #[test]
    fn bucket_of_round_trips() {
        for bucket in ['d', 'e', 'f'] {
            let g = guid_from_bucket(bucket);
            assert!(is_unity_builtin_guid(&g));
            assert_eq!(bucket_of(&g), Some(bucket));
        }
    }

    #[test]
    fn bucket_of_misses_non_builtin() {
        assert_eq!(bucket_of("aabbccddaabbccddaabbccddaabbccdd"), None);
    }
}
