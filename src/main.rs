//! Blitzen — the sigma-rust runner for the SANTA conformance suite
//! (eval + wire + transaction tiers).
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
mod transaction;
mod wire;

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

/// Extract a printable message from a caught panic payload.
fn panic_note(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// Run one entry's eval under a panic net (never-panic, contract §3): an otherwise-uncaught
/// panic becomes the `panicked` outcome (coal, message in `note`) so the run continues. The
/// closure asserts unwind-safety at the call site (it only reads borrowed vector data).
fn caught_actual<F: FnOnce() -> J + std::panic::UnwindSafe>(f: F) -> J {
    match std::panic::catch_unwind(f) {
        Ok(j) => j,
        Err(p) => {
            let note = panic_note(p);
            eval::Outcome::Panicked { note: format!("panic: {note}") }.to_json()
        }
    }
}

/// The same never-panic net for transaction entries — the panic shape carries `valid`
/// (santa-transaction.actuals), not eval's `value`.
fn caught_actual_tx<F: FnOnce() -> J + std::panic::UnwindSafe>(f: F) -> J {
    match std::panic::catch_unwind(f) {
        Ok(j) => j,
        Err(p) => {
            let note = panic_note(p);
            transaction::TxOutcome::Panicked { note: format!("panic: {note}") }.to_json()
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
    // Dispatch on the schema discriminator: wire entries round-trip `bytes_hex` (the blessed
    // expected IS the entry's own bytes — round-trip to self); transaction entries validate
    // the captured tx; eval entries evaluate `tree_bytes_hex` against the blessed `expected`.
    let is_wire = vector["schema"]
        .as_str()
        .is_some_and(|s| s.starts_with("santa-wire/"));
    let is_tx = vector["schema"]
        .as_str()
        .is_some_and(|s| s.starts_with("santa-transaction/"));
    entries
        .iter()
        .map(|entry| {
            let name = entry["name"].as_str().unwrap_or("<unnamed>").to_string();
            if is_wire {
                let kind = entry["kind"].as_str().expect("wire entry missing kind");
                let bytes_hex = entry["bytes_hex"]
                    .as_str()
                    .expect("wire entry missing bytes_hex");
                let actual = caught_actual(std::panic::AssertUnwindSafe(|| {
                    wire::run_entry(kind, bytes_hex).to_json()
                }));
                let expected = serde_json::json!({"bytes_hex": bytes_hex, "error": J::Null});
                (name, actual, expected)
            } else if is_tx {
                let actual = caught_actual_tx(std::panic::AssertUnwindSafe(|| {
                    transaction::run_entry(entry).to_json()
                }));
                // The blessed expected in the actuals vocabulary: {valid, cost, error:null}.
                // `reason` is diagnostic-only (never graded) — dropped so self-compare
                // doesn't coal on differing reject strings.
                let expected = serde_json::json!({
                    "valid": entry["expected"]["valid"],
                    "cost": entry["expected"]["cost"],
                    "error": J::Null,
                });
                (name, actual, expected)
            } else {
                let tree_hex = entry["tree_bytes_hex"]
                    .as_str()
                    .expect("entry missing tree_bytes_hex");
                let tree_bytes = hex_to_bytes(tree_hex).expect("bad tree_bytes_hex");
                let input = entry.get("input").filter(|v| !v.is_null());
                let inputs = entry.get("inputs").and_then(|v| v.as_array());
                let tree_v = entry["version"]["ergoTree"].as_u64().unwrap_or(0) as u8;
                let act_v = entry["version"]["activated"].as_u64().unwrap_or(0) as u8;
                let actual = caught_actual(std::panic::AssertUnwindSafe(|| {
                    eval::run_entry(&tree_bytes, input, inputs, tree_v, act_v).to_json()
                }));
                let expected = entry["expected"].clone();
                (name, actual, expected)
            }
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

pub(crate) fn hex_to_bytes(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd-length hex: {}", s));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{caught_actual, caught_actual_tx};
    use crate::eval::Outcome;
    use serde_json::Value as J;

    #[test]
    fn caught_actual_turns_a_panic_into_panicked() {
        let j = caught_actual(std::panic::AssertUnwindSafe(|| -> J { panic!("kaboom") }));
        assert_eq!(j["error"], "panicked");
        assert_eq!(j["value"], J::Null);
        assert_eq!(j["cost"], J::Null);
        assert!(j["note"].as_str().unwrap().contains("kaboom"));
    }

    #[test]
    fn caught_actual_tx_turns_a_panic_into_a_tx_shaped_panicked() {
        // The tx net's panic shape carries `valid` (santa-transaction.actuals), not
        // eval's `value` — a value-shaped panic would fail the tx actuals schema.
        let j = caught_actual_tx(std::panic::AssertUnwindSafe(|| -> J { panic!("tx kaboom") }));
        assert_eq!(j["error"], "panicked");
        assert_eq!(j["valid"], J::Null);
        assert_eq!(j["cost"], J::Null);
        assert!(j["note"].as_str().unwrap().contains("tx kaboom"));
    }

    #[test]
    fn caught_actual_passes_through_a_normal_outcome() {
        let j = caught_actual(std::panic::AssertUnwindSafe(|| Outcome::Errored.to_json()));
        assert_eq!(j["error"], "errored");
    }
}
