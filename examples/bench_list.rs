//! Microbench driver — isolates each cost in the `list` pipeline so
//! `samply`'s coarse symbolication doesn't matter. Run 500 iterations of
//! each phase; print mean ms.

use std::hint::black_box;
use std::io::Write;
use std::time::Instant;

use unity_assetdb::class_id::ClassId;
use unity_assetdb::store::{self, AssetDb, AssetType};

fn measure<F: FnMut()>(label: &str, mut f: F, iters: u32) {
    // 3 warmup runs
    for _ in 0..3 {
        f();
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        f();
    }
    let elapsed = t0.elapsed();
    let per = elapsed.as_secs_f64() * 1000.0 / iters as f64;
    println!("  {label:32}  {per:7.3} ms / iter  ({iters} iters, total {:?})", elapsed);
}

fn main() {
    let bin_path =
        store::db_path(std::path::Path::new("/tmp/unity-assetdb-profile"));
    let raw = std::fs::read(&bin_path).unwrap();
    let db = store::read(&bin_path).unwrap();
    println!("loaded {} entries from {}", db.entries.len(), bin_path.display());

    println!("\n--- isolated phase costs ---");

    measure(
        "fs::read (2.1 MB)",
        || {
            let _ = black_box(std::fs::read(&bin_path).unwrap());
        },
        200,
    );

    measure(
        "store::decode (in-memory bytes)",
        || {
            let d = store::decode(&raw).unwrap();
            black_box(d);
        },
        200,
    );

    measure(
        "store::read (fs::read + decode)",
        || {
            let d = store::read(&bin_path).unwrap();
            black_box(d);
        },
        200,
    );

    measure(
        "iter all entries (no IO)",
        || {
            let mut acc: u64 = 0;
            for e in db.entries.iter() {
                acc = acc.wrapping_add(e.guid as u64);
                acc = acc.wrapping_add(e.name.len() as u64);
            }
            black_box(acc);
        },
        200,
    );

    measure(
        "list → sink (full write_row)",
        || {
            let mut w = std::io::BufWriter::new(std::io::sink());
            for e in db.entries.iter() {
                write_row(&mut w, e, &db).unwrap();
            }
            w.flush().unwrap();
        },
        200,
    );

    measure(
        "list → sink (just write_all bytes)",
        || {
            let mut w = std::io::BufWriter::new(std::io::sink());
            for e in db.entries.iter() {
                // Skip formatting — just stream the raw name + hint.
                w.write_all(b"X").unwrap();
                w.write_all(e.name.as_bytes()).unwrap();
                w.write_all(b"\t").unwrap();
                w.write_all(e.hint.as_bytes()).unwrap();
                w.write_all(b"\n").unwrap();
            }
            w.flush().unwrap();
        },
        200,
    );

    measure(
        "list → sink (no BufWriter)",
        || {
            let mut w = std::io::sink();
            for e in db.entries.iter() {
                write_row(&mut w, e, &db).unwrap();
            }
        },
        200,
    );

    measure(
        "list → sink (fast hex write_row)",
        || {
            let mut w = std::io::BufWriter::new(std::io::sink());
            for e in db.entries.iter() {
                write_row_fast(&mut w, e, &db).unwrap();
            }
            w.flush().unwrap();
        },
        200,
    );
}

/// Specialized hex encoder — bypasses `std::fmt::write` for the 32-char
/// GUID column. Writes directly to a stack buffer then `write_all`.
fn u128_hex_bytes(v: u128) -> [u8; 32] {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let bytes = v.to_be_bytes();
    let mut out = [0u8; 32];
    for (i, b) in bytes.iter().enumerate() {
        out[i * 2] = LUT[(b >> 4) as usize];
        out[i * 2 + 1] = LUT[(b & 0xf) as usize];
    }
    out
}

fn write_row_fast<W: Write>(w: &mut W, e: &unity_assetdb::store::AssetEntry, db: &AssetDb) -> std::io::Result<()> {
    let g = u128_hex_bytes(e.guid);
    w.write_all(&g)?;
    w.write_all(b"\t")?;
    write_tsv_escaped(w, &e.name)?;
    w.write_all(b"\t")?;
    match e.asset_type {
        AssetType::Native(n) => match ClassId::from_raw(n) {
            Some(c) => w.write_all(c.name().as_bytes())?,
            None => write!(w, "Native:{n}")?,
        },
        AssetType::Script(idx) => {
            w.write_all(b"Script:")?;
            let g = u128_hex_bytes(db.script_guid(idx));
            w.write_all(&g)?;
        }
    }
    w.write_all(b"\t")?;
    write_tsv_escaped(w, &e.hint)?;
    w.write_all(b"\n")
}

fn write_row<W: Write>(w: &mut W, e: &unity_assetdb::store::AssetEntry, db: &AssetDb) -> std::io::Result<()> {
    write!(w, "{:032x}\t", e.guid)?;
    write_tsv_escaped(w, &e.name)?;
    w.write_all(b"\t")?;
    match e.asset_type {
        AssetType::Native(n) => match ClassId::from_raw(n) {
            Some(c) => w.write_all(c.name().as_bytes())?,
            None => write!(w, "Native:{n}")?,
        },
        AssetType::Script(idx) => write!(w, "Script:{:032x}", db.script_guid(idx))?,
    }
    w.write_all(b"\t")?;
    write_tsv_escaped(w, &e.hint)?;
    writeln!(w)
}

fn write_tsv_escaped<W: Write>(w: &mut W, s: &str) -> std::io::Result<()> {
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

