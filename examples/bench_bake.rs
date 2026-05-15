//! Microbench driver for the bake pipeline. Isolates each public phase so
//! `samply`'s coarse symbolication doesn't matter and so the
//! `UNITY_ASSETDB_TIMING=1` phase breakdown can be cross-checked against
//! a stable per-iteration mean.
//!
//! Reads `MEOW_CLIENT` (default `~/Develop/meow-tower`) for the project to
//! bake. Run via `just bench` or:
//! `cargo build --release --example bench_bake && target/release/examples/bench_bake`.

use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use unity_assetdb::bake::{bake, BakeOptions};
use unity_assetdb::store;
use unity_assetdb::walk::walk_for_missing_meta;

fn measure<F: FnMut()>(label: &str, mut f: F, iters: u32) {
    for _ in 0..3 {
        f();
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        f();
    }
    let elapsed = t0.elapsed();
    let per = elapsed.as_secs_f64() * 1000.0 / iters as f64;
    println!("  {label:38}  {per:7.3} ms / iter  ({iters} iters)");
}

fn main() {
    let project = std::env::var("MEOW_CLIENT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/Users/jameskim/Develop/meow-tower"));
    let out_dir = PathBuf::from("/tmp/unity-assetdb-bench");
    std::fs::create_dir_all(&out_dir).unwrap();

    // Prime: one warm bake to populate the cache so subsequent measures
    // hit the cache-hit path.
    let opts = || BakeOptions {
        project_root: project.clone(),
        out_dir: out_dir.clone(),
        name_sanitizer: None,
        on_warn: None,
        on_progress: None,
        verbose_timing: false,
        verbose_collisions: false,
    };
    bake(&opts()).expect("priming bake should succeed");

    let cache_path = store::cache_path(&out_dir);
    let db_path = store::db_path(&out_dir);
    let cache_bytes = std::fs::read(&cache_path).unwrap();
    let db_bytes = std::fs::read(&db_path).unwrap();
    let cache = store::decode_cache(&cache_bytes).unwrap();
    let db = store::decode(&db_bytes).unwrap();
    println!(
        "primed: project={} cache={} entries, db={} entries",
        project.display(),
        cache.entries.len(),
        db.entries.len(),
    );

    println!("\n--- bake pipeline (each phase in isolation) ---");

    measure(
        "fs::read (cache.bin)",
        || {
            let b = std::fs::read(&cache_path).unwrap();
            black_box(b);
        },
        50,
    );

    measure(
        "store::decode_cache (in-memory bytes)",
        || {
            let c = store::decode_cache(&cache_bytes).unwrap();
            black_box(c);
        },
        50,
    );

    measure(
        "store::read_cache (read + decode)",
        || {
            let c = store::read_cache(&cache_path).unwrap();
            black_box(c);
        },
        50,
    );

    measure(
        "store::decode (asset-db.bin in-memory)",
        || {
            let d = store::decode(&db_bytes).unwrap();
            black_box(d);
        },
        50,
    );

    measure(
        "walk_for_missing_meta (pre-pass only)",
        || {
            let mut hits: u32 = 0;
            walk_for_missing_meta(&project, |_p, _is_dir| {
                hits += 1;
            })
            .unwrap();
            black_box(hits);
        },
        20,
    );

    measure(
        "full bake (warm, all phases)",
        || {
            bake(&opts()).unwrap();
        },
        20,
    );

    println!("\nout-dir: {}", out_dir.display());
}
