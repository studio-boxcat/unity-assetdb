//! CLI binary for `unity-assetdb`.
//!
//! Subcommands:
//! - `bake` — walk the project, write the `asset-db.bin` index.
//! - `guid <path>` / `path <guid>` / `find <pattern>` / `list [--type <kind>]`
//!   / `alias <name>` — read-only queries against a baked index.
//! - `register <path> [--type <importer>]` — synthesize a `.meta` and
//!   incrementally update the bin.
//!
//! Output discipline: data → stdout, warnings / suggestions / progress →
//! stderr. Exit codes: 0 = OK / hit, 1 = miss on a point lookup, 2 = bad
//! usage or I/O / corruption error.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use unity_assetdb::bake::{BakeOptions, bake};
use unity_assetdb::query::{self, AssetTypeFilter, parse_guid};
use unity_assetdb::register::{ImporterKind, RegisterOptions, register};
use unity_assetdb::store::{AssetDb, AssetEntry};
use unity_assetdb::suggest::suggest;
use unity_assetdb::walk::resolve_project_root;

#[derive(Parser)]
#[command(
    name = "unity-assetdb",
    about = "Unity asset GUID → name index baker + queries.",
    version,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Walk Assets/, write `<out_dir>/asset-db.bin` (mtime-cached re-bake).
    Bake {
        #[command(flatten)]
        common: CommonOpts,
        /// Characters to scrub from asset names (replaced with `_`). Each
        /// char is treated as an individual scrub target. Pass the same
        /// value to `query alias` / `register` so names round-trip.
        #[arg(long, value_name = "CHARS")]
        scrub_chars: Option<String>,
    },
    /// Project-relative path → GUID hex.
    Guid {
        #[command(flatten)]
        common: CommonOpts,
        #[command(flatten)]
        out: OutputOpts,
        path: String,
    },
    /// GUID → project-relative path.
    Path {
        #[command(flatten)]
        common: CommonOpts,
        #[command(flatten)]
        out: OutputOpts,
        guid: String,
    },
    /// Case-insensitive substring match on asset names. Prints all hits;
    /// empty output (no suggestions) when nothing matches.
    Find {
        #[command(flatten)]
        common: CommonOpts,
        #[command(flatten)]
        out: OutputOpts,
        pattern: String,
    },
    /// List entries, optionally filtered by type.
    List {
        #[command(flatten)]
        common: CommonOpts,
        #[command(flatten)]
        out: OutputOpts,
        /// `Sprite` | `Prefab` | … (ClassId name) or `Script:<32hex>`.
        #[arg(long, value_name = "KIND")]
        r#type: Option<String>,
    },
    /// Exact-name lookup. Returns all entries sharing the name (across
    /// asset types). Auto-applies `--scrub-chars` to the input so callers
    /// can pass the raw filename.
    Alias {
        #[command(flatten)]
        common: CommonOpts,
        #[command(flatten)]
        out: OutputOpts,
        /// Scrub chars to apply to the input before compare (mirror the
        /// `bake --scrub-chars` value).
        #[arg(long, value_name = "CHARS")]
        scrub_chars: Option<String>,
        name: String,
    },
    /// Synthesize a minimal `.meta` for an asset Unity hasn't imported
    /// yet, incrementally update `asset-db.bin`, print the GUID.
    Register {
        #[command(flatten)]
        common: CommonOpts,
        /// Importer kind override. Defaults to the extension table.
        /// Accepts both `NativeFormat` and `NativeFormatImporter` spellings.
        #[arg(long, value_name = "IMPORTER")]
        r#type: Option<String>,
        /// Scrub chars to apply to the new entry's name (mirror
        /// `bake --scrub-chars`).
        #[arg(long, value_name = "CHARS")]
        scrub_chars: Option<String>,
        /// Block this many seconds for the out_dir flock. 0 = try once.
        #[arg(long, default_value_t = 30, value_name = "SECS")]
        lock_timeout: u64,
        path: PathBuf,
    },
}

/// Flags shared by every subcommand.
#[derive(Args)]
struct CommonOpts {
    /// Unity project root. Defaults: walk up from CWD until both
    /// `Assets/` and `ProjectSettings/` are found.
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    /// Output directory holding `asset-db.bin`. Default:
    /// `<project>/Library/unity-assetdb/`.
    #[arg(long, global = true)]
    out_dir: Option<PathBuf>,
}

#[derive(Args)]
struct OutputOpts {
    /// Emit one JSON object per line instead of TSV.
    #[arg(long, global = true)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(code) => code,
        Err(e) => {
            // Quiet shutdown when stdout was closed (e.g. piped to `head`).
            // Surfaces as `std::io::Error::BrokenPipe` somewhere in the
            // chain — printing "error: Broken pipe" then exit 2 is noise.
            if is_broken_pipe(&e) {
                return ExitCode::SUCCESS;
            }
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
}

fn run(cmd: Commands) -> anyhow::Result<ExitCode> {
    match cmd {
        Commands::Bake { common, scrub_chars } => {
            run_bake(common, scrub_chars)?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Guid { common, out, path } => run_guid(common, out, &path),
        Commands::Path { common, out, guid } => run_path(common, out, &guid),
        Commands::Find { common, out, pattern } => run_find(common, out, &pattern),
        Commands::List { common, out, r#type } => run_list(common, out, r#type.as_deref()),
        Commands::Alias {
            common,
            out,
            scrub_chars,
            name,
        } => run_alias(common, out, scrub_chars.as_deref(), &name),
        Commands::Register {
            common,
            r#type,
            scrub_chars,
            lock_timeout,
            path,
        } => run_register(common, r#type.as_deref(), scrub_chars, lock_timeout, path),
    }
}

// ─── bake ────────────────────────────────────────────────────────────────

fn run_bake(common: CommonOpts, scrub_chars: Option<String>) -> anyhow::Result<()> {
    let (project_root, out_dir) = resolve_paths(&common)?;
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
    Ok(())
}

/// Replace each `scrub` char in `name` with `_`. Returns `Some(rewritten)`
/// when at least one char was rewritten; `None` when the input was
/// already clean.
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

// ─── query: guid / path / find / list / alias ────────────────────────────

fn run_guid(common: CommonOpts, out: OutputOpts, path: &str) -> anyhow::Result<ExitCode> {
    let out_dir = resolve_out_dir(&common)?;
    let db = query::open(&out_dir)?;
    if let Some(entry) = query::guid_of_path(&db, path) {
        let stdout = std::io::stdout();
        let mut w = stdout.lock();
        if out.json {
            write_row(&mut w, entry, &db, true)?;
        } else {
            w.write_all(&u128_hex(entry.guid))?;
            w.write_all(b"\n")?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let needle = query::normalize_hint(path);
    print_miss("path", &needle, db.entries.iter().map(|e| e.hint.as_ref()));
    Ok(ExitCode::from(1))
}

fn run_path(common: CommonOpts, out: OutputOpts, guid_str: &str) -> anyhow::Result<ExitCode> {
    let out_dir = resolve_out_dir(&common)?;
    let db = query::open(&out_dir)?;
    let guid = parse_guid(guid_str)?;
    if let Some(entry) = query::path_of_guid(&db, guid) {
        let stdout = std::io::stdout();
        let mut w = stdout.lock();
        if out.json {
            write_row(&mut w, entry, &db, true)?;
        } else {
            writeln!(w, "{}", entry.hint)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    // No fuzzy on raw hex — distance over hex strings isn't useful.
    let hex = u128_hex(guid);
    eprintln!("guid not found: {}", std::str::from_utf8(&hex).unwrap());
    Ok(ExitCode::from(1))
}

fn run_find(common: CommonOpts, out: OutputOpts, pattern: &str) -> anyhow::Result<ExitCode> {
    let out_dir = resolve_out_dir(&common)?;
    let db = query::open(&out_dir)?;
    let hits = query::find(&db, pattern);
    // Bulk emit — `Stdout` is unbuffered when piped, so without
    // `BufWriter` 18 k `writeln!` calls would be 18 k write syscalls.
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::new(stdout.lock());
    for e in hits {
        write_row(&mut w, e, &db, out.json)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn run_list(
    common: CommonOpts,
    out: OutputOpts,
    type_str: Option<&str>,
) -> anyhow::Result<ExitCode> {
    let out_dir = resolve_out_dir(&common)?;
    let db = query::open(&out_dir)?;
    let filter = match type_str {
        Some(s) => Some(AssetTypeFilter::parse(s)?),
        None => None,
    };
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::new(stdout.lock());
    for e in query::list(&db, filter) {
        write_row(&mut w, e, &db, out.json)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn run_alias(
    common: CommonOpts,
    out: OutputOpts,
    scrub_chars: Option<&str>,
    name: &str,
) -> anyhow::Result<ExitCode> {
    let out_dir = resolve_out_dir(&common)?;
    let db = query::open(&out_dir)?;
    let hits = query::alias(&db, name, scrub_chars.unwrap_or(""));
    if hits.is_empty() {
        print_miss("alias", name, db.entries.iter().map(|e| e.name.as_ref()));
        return Ok(ExitCode::from(1));
    }
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::new(stdout.lock());
    for e in hits {
        write_row(&mut w, e, &db, out.json)?;
    }
    Ok(ExitCode::SUCCESS)
}

// ─── register ────────────────────────────────────────────────────────────

fn run_register(
    common: CommonOpts,
    type_str: Option<&str>,
    scrub_chars: Option<String>,
    lock_timeout_secs: u64,
    target: PathBuf,
) -> anyhow::Result<ExitCode> {
    let (project_root, out_dir) = resolve_paths(&common)?;
    let importer_override = match type_str {
        Some(s) => Some(ImporterKind::parse(s).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown importer kind `{s}` — try NativeFormat/Prefab/Texture/Audio/\
                 TrueTypeFont/Shader/TextScript/Mono/SpriteAtlas/Default"
            )
        })?),
        None => None,
    };
    let opts = RegisterOptions {
        project_root,
        out_dir,
        target,
        importer_override,
        scrub_chars,
        lock_timeout: Duration::from_secs(lock_timeout_secs),
    };
    let outcome = register(&opts)?;
    if !outcome.created_meta {
        eprintln!("note: meta already exists; printing existing guid");
    }
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    w.write_all(&u128_hex(outcome.guid))?;
    w.write_all(b"\n")?;
    Ok(ExitCode::SUCCESS)
}

// ─── output helpers ──────────────────────────────────────────────────────

fn resolve_paths(common: &CommonOpts) -> anyhow::Result<(PathBuf, PathBuf)> {
    let project_root = resolve_project_root(common.project.as_deref())?;
    let out_dir = common
        .out_dir
        .clone()
        .unwrap_or_else(|| project_root.join("Library").join("unity-assetdb"));
    Ok((project_root, out_dir))
}

/// Read-only variant of [`resolve_paths`] for query subcommands: when
/// `--out-dir` is supplied explicitly, skip the project-root walk-up so
/// queries work from any directory (e.g. running against a pre-baked
/// `asset-db.bin` in an arbitrary location).
fn resolve_out_dir(common: &CommonOpts) -> anyhow::Result<PathBuf> {
    if let Some(out_dir) = common.out_dir.clone() {
        return Ok(out_dir);
    }
    let project_root = resolve_project_root(common.project.as_deref())?;
    Ok(project_root.join("Library").join("unity-assetdb"))
}

/// Write one entry to `w` in TSV or JSON. Uses [`u128_hex`] instead of
/// `std::fmt`'s `{:032x}` for the GUID columns — measured ~17% faster
/// per row on 18 k-entry emit by skipping the formatter trait dispatch.
fn write_row(
    w: &mut impl Write,
    entry: &AssetEntry,
    db: &AssetDb,
    json: bool,
) -> std::io::Result<()> {
    let guid_hex = u128_hex(entry.guid);
    if json {
        w.write_all(b"{\"guid\":\"")?;
        w.write_all(&guid_hex)?;
        w.write_all(b"\",\"name\":\"")?;
        write_json_escaped(w, &entry.name)?;
        w.write_all(b"\",\"type\":\"")?;
        write_asset_type(w, entry.asset_type, db, |w, s| write_json_escaped(w, s))?;
        w.write_all(b"\",\"hint\":\"")?;
        write_json_escaped(w, &entry.hint)?;
        w.write_all(b"\"}\n")
    } else {
        w.write_all(&guid_hex)?;
        w.write_all(b"\t")?;
        write_tsv_escaped(w, &entry.name)?;
        w.write_all(b"\t")?;
        write_asset_type(w, entry.asset_type, db, |w, s| w.write_all(s.as_bytes()))?;
        w.write_all(b"\t")?;
        write_tsv_escaped(w, &entry.hint)?;
        w.write_all(b"\n")
    }
}

/// Format a `u128` as 32 lowercase hex bytes. Bypasses `std::fmt::write`
/// — the formatter trait dispatch is ~17% of the per-row write cost on
/// `list` emit (bench in `examples/bench_list.rs`).
pub(crate) fn u128_hex(v: u128) -> [u8; 32] {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let bytes = v.to_be_bytes();
    let mut out = [0u8; 32];
    for (i, b) in bytes.iter().enumerate() {
        out[i * 2] = LUT[(b >> 4) as usize];
        out[i * 2 + 1] = LUT[(b & 0xf) as usize];
    }
    out
}

fn write_asset_type<W: Write>(
    w: &mut W,
    t: unity_assetdb::store::AssetType,
    db: &AssetDb,
    mut escape: impl FnMut(&mut W, &str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use unity_assetdb::class_id::ClassId;
    use unity_assetdb::store::AssetType;
    match t {
        AssetType::Native(n) => match ClassId::from_raw(n) {
            Some(c) => escape(w, c.name()),
            None => write!(w, "Native:{n}"),
        },
        AssetType::Script(idx) => {
            w.write_all(b"Script:")?;
            w.write_all(&u128_hex(db.script_guid(idx)))
        }
    }
}

/// TSV cell escape — asset names can carry tabs/newlines from third-party
/// packages, raw emission would corrupt downstream pipelines.
fn write_tsv_escaped(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        let esc: Option<&[u8]> = match b {
            b'\\' => Some(b"\\\\"),
            b'\t' => Some(b"\\t"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            _ => None,
        };
        if let Some(esc) = esc {
            w.write_all(&s.as_bytes()[start..i])?;
            w.write_all(esc)?;
            start = i + 1;
        }
    }
    w.write_all(&s.as_bytes()[start..])
}

/// JSON string-content escape — surrounding quotes are caller's. Covers
/// `"`, `\`, control chars; non-ASCII passes through as UTF-8.
fn write_json_escaped(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        let esc: Option<&[u8]> = match b {
            b'"' => Some(b"\\\""),
            b'\\' => Some(b"\\\\"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            b'\t' => Some(b"\\t"),
            c if c < 0x20 => {
                w.write_all(&s.as_bytes()[start..i])?;
                write!(w, "\\u{:04x}", c)?;
                start = i + 1;
                continue;
            }
            _ => None,
        };
        if let Some(esc) = esc {
            w.write_all(&s.as_bytes()[start..i])?;
            w.write_all(esc)?;
            start = i + 1;
        }
    }
    w.write_all(&s.as_bytes()[start..])
}

/// Emit a miss diagnostic + fuzzy suggestions to stderr.
fn print_miss<'a>(kind: &str, needle: &str, pool: impl IntoIterator<Item = &'a str>) {
    eprintln!("{kind} not found: {needle}");
    let hits = suggest(needle, pool, 5);
    if !hits.is_empty() {
        eprintln!("did you mean:");
        for h in hits {
            eprintln!("  {h}");
        }
    }
}

