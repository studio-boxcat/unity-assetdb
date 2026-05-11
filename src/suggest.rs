//! Tiny fuzzy-match helper for "did you mean" suggestions on miss.
//!
//! Two-stage: case-insensitive substring matches first (cheap), then
//! Levenshtein-distance ≤ 2 candidates as fill. Cap defaults to 5 — past
//! that, suggestions stop being useful and start being noise.
//!
//! Hand-rolled; no fuzzy-match crate. Levenshtein uses a two-row rolling
//! DP and short-circuits when the row minimum exceeds the threshold.

/// Suggest up to `max` candidates for `needle` from `haystack`. Substring
/// matches (case-insensitive) come first in input order; Levenshtein-≤2
/// neighbors fill the remainder, sorted by edit distance ascending.
pub fn suggest<'a, I>(needle: &str, haystack: I, max: usize) -> Vec<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let needle_lc = needle.to_lowercase();
    let needle_bytes_lc: Vec<u8> = needle_lc.bytes().collect();
    let mut out: Vec<&'a str> = Vec::new();
    let mut dist_pool: Vec<(u8, &'a str)> = Vec::new();

    for s in haystack {
        if out.len() >= max {
            break;
        }
        // ASCII fast path on substring — skips `to_lowercase()` allocation
        // when the haystack item is ASCII (true for ~all asset names).
        if needle.is_ascii() && s.is_ascii() {
            if ascii_icase_contains(s.as_bytes(), &needle_bytes_lc) {
                out.push(s);
                continue;
            }
            // Length prune before edit-distance — strings whose length
            // gap exceeds the threshold can't match.
            if s.len().abs_diff(needle.len()) > 2 {
                continue;
            }
            if let Some(d) = ascii_edit_distance_at_most(&needle_bytes_lc, s.as_bytes(), 2) {
                dist_pool.push((d, s));
            }
            continue;
        }
        // Unicode fallback.
        let s_lc = s.to_lowercase();
        if s_lc.contains(&needle_lc) {
            out.push(s);
            continue;
        }
        if let Some(d) = edit_distance_at_most(&needle_lc, &s_lc, 2) {
            dist_pool.push((d, s));
        }
    }

    dist_pool.sort_by_key(|(d, s)| (*d, s.len()));
    for (_, s) in dist_pool {
        if out.len() >= max {
            break;
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn ascii_icase_contains(hay: &[u8], needle_lc: &[u8]) -> bool {
    if needle_lc.is_empty() {
        return true;
    }
    if hay.len() < needle_lc.len() {
        return false;
    }
    hay.windows(needle_lc.len())
        .any(|w| w.iter().zip(needle_lc).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

/// ASCII-bytes variant of [`edit_distance_at_most`]. Avoids the per-call
/// `Vec<char>` allocation; `needle_lc` is pre-lowercased; `b` is
/// lowercased on the fly via `to_ascii_lowercase`.
fn ascii_edit_distance_at_most(needle_lc: &[u8], b: &[u8], max_distance: u8) -> Option<u8> {
    let n = needle_lc.len();
    let m = b.len();
    if n.abs_diff(m) > max_distance as usize {
        return None;
    }
    if n == 0 {
        return (m <= max_distance as usize).then_some(m as u8);
    }
    if m == 0 {
        return (n <= max_distance as usize).then_some(n as u8);
    }
    let k = max_distance as u16;
    let mut prev: Vec<u16> = (0..=m as u16).collect();
    let mut cur: Vec<u16> = vec![0; m + 1];
    for i in 1..=n {
        cur[0] = i as u16;
        let mut row_min = cur[0];
        let ai = needle_lc[i - 1];
        for j in 1..=m {
            let cost = u16::from(ai != b[j - 1].to_ascii_lowercase());
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if cur[j] < row_min {
                row_min = cur[j];
            }
        }
        if row_min > k {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[m];
    (d <= k).then_some(d as u8)
}

/// Levenshtein distance with a small-`k` cutoff. Returns `Some(d)` when
/// `d <= max_distance`, else `None`. Two-row rolling DP; early-exit when
/// the row minimum exceeds `max_distance`.
///
/// Inputs are compared as `char` sequences (not bytes) — Unicode-safe for
/// the asset names we suggest over.
fn edit_distance_at_most(a: &str, b: &str, max_distance: u8) -> Option<u8> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();

    // Trivial length-gap pruning: |n-m| > k → distance > k.
    if n.abs_diff(m) > max_distance as usize {
        return None;
    }
    if n == 0 {
        return (m <= max_distance as usize).then_some(m as u8);
    }
    if m == 0 {
        return (n <= max_distance as usize).then_some(n as u8);
    }

    let k = max_distance as usize;
    let mut prev: Vec<u16> = (0..=m as u16).collect();
    let mut cur: Vec<u16> = vec![0; m + 1];
    for i in 1..=n {
        cur[0] = i as u16;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = u16::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if cur[j] < row_min {
                row_min = cur[j];
            }
        }
        if row_min > k as u16 {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[m];
    (d <= k as u16).then_some(d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_matches_come_first() {
        let pool = ["Foo", "FooBar", "Bar", "FoBar"];
        let out = suggest("foo", pool.iter().copied(), 5);
        assert_eq!(out[0], "Foo");
        assert_eq!(out[1], "FooBar");
    }

    #[test]
    fn edit_distance_fills_when_no_substring() {
        let pool = ["abcd", "xyz", "abce"];
        // "abcf" → "abcd" and "abce" are both distance 1.
        let out = suggest("abcf", pool.iter().copied(), 5);
        assert!(out.contains(&"abcd"));
        assert!(out.contains(&"abce"));
        assert!(!out.contains(&"xyz"));
    }

    #[test]
    fn respects_cap() {
        let pool = ["aa", "ab", "ac", "ad", "ae", "af", "ag"];
        let out = suggest("a", pool.iter().copied(), 3);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn empty_result_when_far() {
        let pool = ["lorem", "ipsum"];
        let out = suggest("zzzzz", pool.iter().copied(), 5);
        assert!(out.is_empty());
    }

    #[test]
    fn edit_distance_threshold() {
        assert_eq!(edit_distance_at_most("kitten", "sitting", 3), Some(3));
        assert_eq!(edit_distance_at_most("kitten", "sitting", 2), None);
        assert_eq!(edit_distance_at_most("foo", "foo", 0), Some(0));
        assert_eq!(edit_distance_at_most("", "abc", 2), None);
        assert_eq!(edit_distance_at_most("", "abc", 3), Some(3));
    }

    #[test]
    fn unicode_safe() {
        // 3 chars not 9 bytes — distance is char-based.
        assert_eq!(edit_distance_at_most("あいう", "あいえ", 1), Some(1));
    }
}
