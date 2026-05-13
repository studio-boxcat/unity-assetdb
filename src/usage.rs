//! Find files in a Unity project that reference a given asset GUID.
//!
//! Walks `<project>/Assets/` and `<project>/Packages/` with
//! [`ignore::WalkBuilder`] (same machinery as [`crate::walk`]), scans
//! Unity YAML asset files for the 32-hex GUID byte sequence, and reports
//! `(path, line, text)` matches. Native substitute for
//! `rg <hex> Assets Packages` that already knows which extensions are
//! Unity-YAML and skips everything else.
//!
//! References in Unity YAML show up as `{fileID: …, guid: <32 hex>,
//! type: …}` (incl. sub-asset fileIDs against the same GUID), so a plain
//! byte-string search over the hex is sufficient — 128 random bits make
//! collisions effectively impossible.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore::WalkBuilder;
use memchr::memmem;

use crate::walk::{WalkError, is_unity_hidden};

/// Unity YAML asset extensions. Anything outside this set is skipped to
/// avoid grepping textures/audio/.dll binaries. If users hit a gap, the
/// escape hatch is `rg <hex>` directly.
const YAML_EXTS: &[&str] = &[
    "prefab",
    "unity",
    "asset",
    "mat",
    "controller",
    "anim",
    "preset",
    "playable",
    "mask",
    "overrideController",
    "mixer",
    "physicsMaterial2D",
    "physicMaterial",
    "spriteatlas",
    "spriteatlasv2",
    "lighting",
    "meta",
    "terrainlayer",
    "fontsettings",
    "renderTexture",
    "cubemap",
    "flare",
    "guiskin",
    "shadervariants",
    "shadergraph",
    "shadersubgraph",
    "vfx",
];

#[derive(Debug, Clone)]
pub struct UsageMatch {
    pub path: PathBuf,
    /// 1-indexed.
    pub line: u32,
    /// Matching line, trimmed of trailing `\r` and surrounding whitespace.
    pub text: String,
}

/// Scan `<project>/{Assets,Packages}` for files containing `guid_hex`
/// (lowercase 32-char ASCII). Results sorted by `(path, line)`.
pub fn find_usages(project_root: &Path, guid_hex: &[u8; 32]) -> Result<Vec<UsageMatch>, WalkError> {
    let assets = project_root.join("Assets");
    if !assets.is_dir() {
        return Err(WalkError::AssetsMissing { path: assets });
    }
    let packages = project_root.join("Packages");

    let mut builder = WalkBuilder::new(&assets);
    if packages.is_dir() {
        builder.add(&packages);
    }
    let walker = builder
        .standard_filters(false)
        .follow_links(false)
        .filter_entry(|e| !is_unity_hidden(e.file_name()))
        .build_parallel();

    let results: Arc<Mutex<Vec<UsageMatch>>> = Arc::new(Mutex::new(Vec::new()));
    let err: Arc<Mutex<Option<WalkError>>> = Arc::new(Mutex::new(None));
    let root = project_root.to_path_buf();
    // Built once, shared across workers — Finder's SIMD preamble is
    // ~µs but doing it per-file would compound across thousands of files.
    let finder = Arc::new(memmem::Finder::new(guid_hex));

    walker.run(|| {
        let results = Arc::clone(&results);
        let err = Arc::clone(&err);
        let finder = Arc::clone(&finder);
        let root = root.clone();
        Box::new(move |res| {
            use ignore::WalkState;
            let entry = match res {
                Ok(e) => e,
                Err(e) => {
                    *err.lock().unwrap() = Some(WalkError::Walk(e));
                    return WalkState::Quit;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            if !has_yaml_ext(path) {
                return WalkState::Continue;
            }
            // A single unreadable asset shouldn't poison the whole scan;
            // log so the caller knows the result set is incomplete.
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("usage: read {}: {e}", path.display());
                    return WalkState::Continue;
                }
            };
            let rel = path.strip_prefix(&root).unwrap_or(path).to_path_buf();
            let hits = scan_file(&bytes, &finder, rel);
            if !hits.is_empty() {
                results.lock().unwrap().extend(hits);
            }
            WalkState::Continue
        })
    });

    if let Some(e) = err.lock().unwrap().take() {
        return Err(e);
    }
    // Lock-and-take rather than `Arc::try_unwrap` — silent drop if a
    // worker clone outlived `run()`. See the same comment in `walk.rs`.
    let mut out = std::mem::take(&mut *results.lock().unwrap());
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(out)
}

fn has_yaml_ext(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    YAML_EXTS.iter().any(|x| x.eq_ignore_ascii_case(ext))
}

fn scan_file(bytes: &[u8], finder: &memmem::Finder, rel: PathBuf) -> Vec<UsageMatch> {
    let mut hits = Vec::new();
    let mut line: u32 = 1;
    let mut prev = 0usize;
    for pos in finder.find_iter(bytes) {
        // Incremental line-count from the previous match — monotone in
        // `pos`, so total newline scans across all hits = O(file size).
        line += memchr::memchr_iter(b'\n', &bytes[prev..pos]).count() as u32;
        prev = pos;
        let line_start = bytes[..pos]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let line_end = memchr::memchr(b'\n', &bytes[pos..])
            .map(|i| pos + i)
            .unwrap_or(bytes.len());
        let text = String::from_utf8_lossy(&bytes[line_start..line_end])
            .trim_end_matches('\r')
            .trim()
            .to_string();
        hits.push(UsageMatch {
            path: rel.clone(),
            line,
            text,
        });
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_file_picks_lines_with_guid() {
        let needle = b"571ad98c7c0d4a559a0cf213d8da355f";
        let finder = memmem::Finder::new(needle);
        let body = b"\
m_Script: {fileID: 11500000, guid: 571ad98c7c0d4a559a0cf213d8da355f, type: 3}\n\
m_Name: Foo\n\
m_OtherRef: {fileID: 0, guid: 571ad98c7c0d4a559a0cf213d8da355f, type: 3}\r\n\
unrelated: line\n";
        let hits = scan_file(body, &finder, PathBuf::from("Assets/x.prefab"));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[1].line, 3);
        assert!(hits[1].text.ends_with("type: 3}"));
    }

    #[test]
    fn yaml_ext_filter() {
        assert!(has_yaml_ext(Path::new("a/b.prefab")));
        assert!(has_yaml_ext(Path::new("a/b.PREFAB")));
        assert!(has_yaml_ext(Path::new("a/b.meta")));
        assert!(!has_yaml_ext(Path::new("a/b.png")));
        assert!(!has_yaml_ext(Path::new("a/b")));
    }
}
