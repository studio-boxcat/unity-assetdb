use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use unity_assetdb::bake::{BakeOptions, bake};
use unity_assetdb::walk::resolve_project_root;

#[derive(Parser)]
#[command(name = "unity-assetdb", about = "Unity asset GUID → name index baker.")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Walk Assets/, write `<out_dir>/asset-db.bin` (mtime-cached re-bake).
    Bake {
        /// Unity project root. Defaults: walk up from CWD until both
        /// `Assets/` and `ProjectSettings/` are found.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Output directory for `asset-db.bin` and `asset-db.cache.bin`.
        /// Default: `<project>/Library/unity-assetdb/`.
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Characters to scrub from asset names (replaced with `_`).
        /// Each char in this string is treated as an individual char
        /// to scrub. Empty / unset = no rewriting.
        ///
        /// Use case: consumers whose serialization grammar reserves
        /// specific characters (e.g. pspec reserves `/|#\@^` in its
        /// ref grammar; addressables consumers may scrub `/`). The
        /// crate stays grammar-neutral by default — the caller supplies
        /// the chars.
        ///
        /// On a rewrite, a warning is emitted via stderr.
        #[arg(long, value_name = "CHARS")]
        scrub_chars: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Bake {
            project,
            out_dir,
            scrub_chars,
        } => {
            let project_root = resolve_project_root(project.as_deref())?;
            let out_dir =
                out_dir.unwrap_or_else(|| project_root.join("Library").join("unity-assetdb"));
            let verbose_timing = std::env::var("UNITY_ASSETDB_TIMING").is_ok();
            let verbose_collisions = std::env::var("UNITY_ASSETDB_VERBOSE").is_ok();
            let name_sanitizer = scrub_chars.map(|chars| {
                let scrub: Vec<char> = chars.chars().collect();
                let sanitizer: unity_assetdb::bake::NameSanitizer =
                    Box::new(move |s: &str| scrub_chars_in(s, &scrub));
                sanitizer
            });
            let opts = BakeOptions {
                project_root,
                out_dir,
                name_sanitizer,
                on_warn: Some(Box::new(|m| eprintln!("{m}"))),
                on_progress: Some(Box::new(|m| eprintln!("{m}"))),
                verbose_timing,
                verbose_collisions,
            };
            bake(&opts)?;
        }
    }
    Ok(())
}

/// Replace each `scrub` char in `name` with `_`. Returns `Some(rewritten)`
/// when at least one char was rewritten; `None` when the input was
/// already clean. Matches the contract of `BakeOptions::name_sanitizer`.
fn scrub_chars_in(name: &str, scrub: &[char]) -> Option<String> {
    let first = name.char_indices().find(|(_, c)| scrub.contains(c))?;
    let mut out = String::with_capacity(name.len());
    out.push_str(&name[..first.0]);
    out.push('_');
    for c in name[first.0 + first.1.len_utf8()..].chars() {
        out.push(if scrub.contains(&c) { '_' } else { c });
    }
    Some(out)
}
