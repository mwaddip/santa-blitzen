//! Blitzen — the sigma-rust eval-tier runner for the SANTA conformance suite.
//!
//! Two modes — `eval::run_entry` produces actuals **blind** (never reads `expected`)
//! in both:
//!   blitzen <vector.json | dir>           self-compare: run each entry, compare vs
//!                                          the blessed `expected`, print nice/coal.
//!   blitzen emit <vectors-dir> <out-dir>  emit actuals for the SANTA orchestrator:
//!                                          write <out-dir>/<same-filename> = an object
//!                                          mapping entry name → { value, cost, error }.
//!                                          No comparison — the orchestrator owns the
//!                                          single §5 comparator across every runner.
//!
//! Self-compare's comparison is the separate step (contract §6), using serde_json
//! structural equality — which matches the §5 match (objects key-order-insensitive,
//! arrays order-sensitive, numbers numeric, strings exact, null==null).

mod eval;
mod sval;

use serde_json::Value as J;
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("emit") => {
            if args.len() < 4 {
                eprintln!("usage: blitzen emit <vectors-dir> <out-dir>");
                std::process::exit(2);
            }
            emit(Path::new(&args[2]), Path::new(&args[3]));
        }
        Some(target) => self_compare(Path::new(target)),
        None => {
            eprintln!(
                "usage:\n  \
                 blitzen <vector.json | dir>           self-compare vs blessed expected\n  \
                 blitzen emit <vectors-dir> <out-dir>  emit actuals for the SANTA orchestrator"
            );
            std::process::exit(2);
        }
    }
}

/// Evaluate every entry of one vector file (blind), pairing each entry's `name`
/// with its actual JSON and the vector's blessed `expected` JSON. Returns empty for
/// a file carrying no `entries` (not an eval vector). A malformed vector aborts the
/// run via `expect` — a loud error, never a silent skip (contract §3 totality).
fn run_vector_file(path: &Path) -> Vec<(String, J, J)> {
    let text = std::fs::read_to_string(path).expect("read vector file");
    let vector: J = serde_json::from_str(&text).expect("parse vector JSON");
    let entries = match vector["entries"].as_array() {
        Some(e) => e,
        None => return Vec::new(),
    };
    entries
        .iter()
        .map(|entry| {
            let name = entry["name"].as_str().unwrap_or("<unnamed>").to_string();
            let tree_hex = entry["tree_bytes_hex"]
                .as_str()
                .expect("entry missing tree_bytes_hex");
            let tree_bytes = hex_to_bytes(tree_hex).expect("bad tree_bytes_hex");
            let input = entry.get("input").filter(|v| !v.is_null());
            let tree_v = entry["version"]["ergoTree"].as_u64().unwrap_or(0) as u8;
            let act_v = entry["version"]["activated"].as_u64().unwrap_or(0) as u8;
            let actual = eval::run_entry(&tree_bytes, input, tree_v, act_v).to_json();
            let expected = entry["expected"].clone();
            (name, actual, expected)
        })
        .collect()
}

/// Self-compare mode: evaluate the corpus and tally nice/coal vs the blessed `expected`.
fn self_compare(path: &Path) {
    let files = collect_vector_files(path);
    if files.is_empty() {
        eprintln!("no .json vector files found at {}", path.display());
        std::process::exit(2);
    }

    let (mut nice, mut coal, mut total) = (0u64, 0u64, 0u64);
    let mut coal_list: Vec<(String, String, J, J)> = Vec::new();

    for f in &files {
        let short = short_name(f);
        for (name, actual, expected) in run_vector_file(f) {
            total += 1;
            if actual == expected {
                nice += 1;
            } else {
                coal += 1;
                coal_list.push((short.clone(), name, actual, expected));
            }
        }
    }

    println!(
        "blitzen: {} nice / {} coal / {} entries across {} files",
        nice,
        coal,
        total,
        files.len()
    );
    for (file, name, actual, expected) in coal_list.iter().take(60) {
        println!("\nCOAL  {}  ::  {}", file, name);
        println!("  actual:   {}", actual);
        println!("  expected: {}", expected);
    }
    if coal_list.len() > 60 {
        println!("\n... and {} more coal entries", coal_list.len() - 60);
    }
}

/// Emit mode: write one actuals file per vector (`<out-dir>/<same-filename>`), each an
/// object mapping entry `name` → its `{ value, cost, error }` (santa-eval.actuals schema).
/// Blind — never reads `expected`; the orchestrator owns the §5 comparison. Exit 0 once
/// every actuals file is written; a malformed vector aborts loudly (non-zero) via `expect`.
fn emit(vectors_dir: &Path, out_dir: &Path) {
    let files = collect_vector_files(vectors_dir);
    if files.is_empty() {
        eprintln!("no .json vector files found at {}", vectors_dir.display());
        std::process::exit(2);
    }
    std::fs::create_dir_all(out_dir).expect("create out dir");

    let mut written = 0u64;
    for f in &files {
        let entries = run_vector_file(f);
        if entries.is_empty() {
            continue; // not an eval vector → no actuals file
        }
        let mut actuals = serde_json::Map::new();
        for (name, actual, _expected) in entries {
            actuals.insert(name, actual);
        }
        let filename = f.file_name().expect("vector filename");
        let out_path = out_dir.join(filename);
        let text = serde_json::to_string_pretty(&J::Object(actuals)).expect("serialize actuals");
        std::fs::write(&out_path, text).expect("write actuals file");
        written += 1;
    }
    eprintln!(
        "blitzen emit: wrote {} actuals files to {}",
        written,
        out_dir.display()
    );
}

fn short_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}

fn collect_vector_files(path: &Path) -> Vec<PathBuf> {
    if path.is_dir() {
        let mut out: Vec<PathBuf> = std::fs::read_dir(path)
            .expect("read dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        out.sort();
        out
    } else {
        vec![path.to_path_buf()]
    }
}

fn hex_to_bytes(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd-length hex: {}", s));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}
