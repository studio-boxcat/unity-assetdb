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
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Bake { project, out_dir } => {
            let project_root = resolve_project_root(project.as_deref())?;
            let out_dir = out_dir.unwrap_or_else(|| {
                project_root.join("Library").join("unity-assetdb")
            });
            let verbose_timing = std::env::var("UNITY_ASSETDB_TIMING").is_ok();
            let verbose_collisions = std::env::var("UNITY_ASSETDB_VERBOSE").is_ok();
            let opts = BakeOptions {
                project_root,
                out_dir,
                name_sanitizer: None,
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
