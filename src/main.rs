//! Blitzen — the sigma-rust eval-tier runner for the SANTA conformance suite.
//!
//! Usage:
//!   blitzen <vector.json | dir>   run each entry, compare vs blessed `expected`,
//!                                 print nice/coal tally + coal detail.
//!
//! `eval::run_entry` produces actuals **blind** (it never reads `expected`); the
//! comparison here is the separate step (contract §6), using serde_json structural
//! equality — which matches the contract §5 match (objects key-order-insensitive,
//! arrays order-sensitive, numbers numeric, strings exact, null==null).

mod eval;
mod sval;

use serde_json::Value as J;
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: blitzen <vector.json | dir>");
        std::process::exit(2);
    }
    let files = collect_vector_files(Path::new(&args[1]));
    if files.is_empty() {
        eprintln!("no .json vector files found at {}", args[1]);
        std::process::exit(2);
    }

    let (mut nice, mut coal, mut total) = (0u64, 0u64, 0u64);
    let mut coal_list: Vec<(String, String, J, J)> = Vec::new();

    for f in &files {
        let text = std::fs::read_to_string(f).expect("read vector file");
        let vector: J = serde_json::from_str(&text).expect("parse vector JSON");
        let entries = match vector["entries"].as_array() {
            Some(e) => e,
            None => continue, // not an eval vector file
        };
        for entry in entries {
            total += 1;
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

            if actual == expected {
                nice += 1;
            } else {
                coal += 1;
                let short = f.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string();
                coal_list.push((short, name, actual, expected));
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
    if s.len() % 2 != 0 {
        return Err(format!("odd-length hex: {}", s));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}
