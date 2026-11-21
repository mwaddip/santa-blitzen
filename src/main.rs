//! Blitzen: sigma-rust's runner for SANTA, the Ergo consensus conformance suite
//! (<https://github.com/mwaddip/santa>). It runs SANTA's test vectors through sigma-rust, grades
//! sigma-rust's answers against the reference implementation's (the harness, `harness/`), and
//! writes its results; SANTA re-grades them as a canary. The vector and actuals formats are SANTA's runner contract
//! (`docs/contract/runner-contract.md` for the eval tier, and its `-wire`, `-transaction` and
//! `-nipopow` companions); "runner contract §N" below refers to it. An entry's `expected` is the
//! reference's answer, recorded ("blessed") when the vector was made.
//!
//! The modes, all producing the actuals blind (never reading `expected`):
//!   blitzen run <runner.json> <santa-dir> <out-dir>  the self-contained runner (SANTA's
//!                                          runner-integration §3): select, run, bound, grade,
//!                                          write results.json and actuals/ (the harness).
//!   blitzen one <santa-dir> <relpath> <out-file>     a bounded file's child: run that one file.
//!   blitzen <vector.json | dir>           self-compare: run each entry, compare with its
//!                                          `expected`, print nice/coal.
//!
//! Self-compare uses serde_json's structural equality, which is the contract's match (§5):
//! objects key-order-insensitive, arrays order-sensitive, numbers numeric, strings exact.

mod binding;
mod eval;
mod nipopow;
mod sval;
mod transaction;
mod wire;

use serde_json::Value as J;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The harness's runner function: one vector file → its actuals by entry name.
fn run_file(path: &Path, _doc: &J) -> BTreeMap<String, J> {
    run_vector_file(path).into_iter().map(|(name, actual, _expected)| (name, actual)).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let done = |r: Result<(), String>| {
        if let Err(e) = r {
            eprintln!("blitzen: {e}");
            std::process::exit(1);
        }
    };
    match args.get(1).map(String::as_str) {
        Some("run") if args.len() == 5 => {
            done(santa_harness::main_run(Path::new(&args[2]), Path::new(&args[3]), Path::new(&args[4]), &run_file))
        }
        Some("one") if args.len() == 5 => {
            done(santa_harness::main_one(Path::new(&args[2]), &args[3], Path::new(&args[4]), &run_file))
        }
        Some(target) if target != "run" && target != "one" => self_compare(Path::new(target)),
        _ => {
            eprintln!(
                "usage:\n  \
                 blitzen run <runner.json> <santa-dir> <out-dir>  the self-contained runner\n  \
                 blitzen one <santa-dir> <relpath> <out-file>     a bounded file's child\n  \
                 blitzen <vector.json | dir>                      self-compare vs blessed expected"
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

/// Run one entry under a panic net (runner contract §3: never panic): a panic becomes the tier's
/// `panicked` actual, built by `panicked` from the panic's message, and the run goes on. The
/// closures only read the vector, so asserting unwind-safety is sound.
fn caught(f: impl FnOnce() -> J, panicked: fn(String) -> J) -> J {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(j) => j,
        Err(p) => panicked(format!("panic: {}", panic_note(p))),
    }
}

/// The eval-shaped `panicked` actual (also the wire and NiPoPoW tiers').
fn eval_panicked(note: String) -> J {
    eval::Outcome::Panicked { note }.to_json()
}

/// The transaction-shaped `panicked` actual: it carries `valid` where eval's carries `value`.
fn tx_panicked(note: String) -> J {
    transaction::TxOutcome::Panicked { note }.to_json()
}

/// The tier a vector file's `schema` names: only the schemas this runner was written for, by exact name and
/// version.
enum Tier<'v> {
    /// `santa-eval/v6-fullctx`: its context carries the 9 headers a script sees, which this
    /// `Context` (exactly 10) cannot hold, so its entries are not-implemented.
    FullContextEval,
    /// `santa-nipopow/*`: the file's chain, which each entry asks interlinks or a proof over.
    NiPoPoW(&'v [J]),
    Wire,
    Transaction,
    Eval,
    /// Any other schema (a newer version of one above, or a tier this runner does not mount): every entry is
    /// not-implemented, never read as a known schema (runner-integration §3).
    Unknown,
}

fn tier_of(vector: &J) -> Tier<'_> {
    match vector["schema"].as_str().unwrap_or("") {
        "santa-eval/v1" | "santa-eval/v2" | "santa-eval/v3" | "santa-eval/v4" | "santa-eval/v5" => {
            Tier::Eval
        }
        "santa-eval/v6-fullctx" => Tier::FullContextEval,
        "santa-nipopow/v1" => Tier::NiPoPoW(
            vector["chain"]
                .as_array()
                .expect("nipopow vector missing chain"),
        ),
        "santa-wire/v1" => Tier::Wire,
        "santa-transaction/v1" => Tier::Transaction,
        _ => Tier::Unknown,
    }
}

/// Evaluate every entry of one vector file (blind), pairing each entry's `name`
/// with its actual JSON and the vector's blessed `expected` JSON. Returns empty for
/// a file carrying no `entries`. A malformed vector aborts the run via `expect`: a loud
/// error, never a silent skip (runner contract §3: every entry gets an outcome).
fn run_vector_file(path: &Path) -> Vec<(String, J, J)> {
    let text = std::fs::read_to_string(path).expect("read vector file");
    let vector: J = serde_json::from_str(&text).expect("parse vector JSON");
    let entries = match vector["entries"].as_array() {
        Some(e) => e,
        None => return Vec::new(),
    };
    let tier = tier_of(&vector);
    entries
        .iter()
        .map(|entry| {
            let name = entry["name"].as_str().unwrap_or("<unnamed>").to_string();
            let (actual, expected) = match tier {
                Tier::FullContextEval => (
                    eval::Outcome::NotImplemented.to_json(),
                    entry["expected"].clone(),
                ),
                // the tier-neutral form: the runner cannot know the actual shape of a schema it was not written for
                Tier::Unknown => (
                    serde_json::json!({"error": "not-implemented"}),
                    entry["expected"].clone(),
                ),
                Tier::NiPoPoW(chain) => (
                    caught(|| nipopow_entry(entry, chain), eval_panicked),
                    entry["expected"].clone(),
                ),
                Tier::Wire => wire_entry(entry),
                Tier::Transaction => transaction_entry(entry),
                Tier::Eval => (eval_entry(entry), entry["expected"].clone()),
            };
            (name, actual, expected)
        })
        .collect()
}

fn nipopow_entry(entry: &J, chain: &[J]) -> J {
    match entry["kind"].as_str().unwrap_or("") {
        "nipopow_interlinks" => nipopow::run_interlinks(chain),
        "nipopow_prove" => {
            let m = entry["payload"]["m"].as_u64().expect("missing m") as u32;
            let k = entry["payload"]["k"].as_u64().expect("missing k") as u32;
            nipopow::run_prove(chain, m, k, entry["payload"]["headerId"].as_str())
        }
        _ => serde_json::json!({"error": "not-implemented"}),
    }
}

/// A wire entry's actual, and its expected: its own bytes back (a round-trip to itself).
fn wire_entry(entry: &J) -> (J, J) {
    let kind = entry["kind"].as_str().expect("wire entry missing kind");
    let bytes_hex = entry["bytes_hex"]
        .as_str()
        .expect("wire entry missing bytes_hex");
    let actual = caught(|| wire::run_entry(kind, bytes_hex).to_json(), eval_panicked);
    (
        actual,
        serde_json::json!({"bytes_hex": bytes_hex, "error": J::Null}),
    )
}

/// A transaction entry's actual, and its expected in the actuals vocabulary. `reason` is
/// diagnostic only, never graded, so it is left out of the expected.
fn transaction_entry(entry: &J) -> (J, J) {
    let actual = caught(|| transaction::run_entry(entry).to_json(), tx_panicked);
    let expected = serde_json::json!({
        "valid": entry["expected"]["valid"],
        "cost": entry["expected"]["cost"],
        "error": J::Null,
    });
    (actual, expected)
}

fn eval_entry(entry: &J) -> J {
    let tree_hex = entry["tree_bytes_hex"]
        .as_str()
        .expect("entry missing tree_bytes_hex");
    let tree_bytes = hex_to_bytes(tree_hex).expect("bad tree_bytes_hex");
    let input = entry.get("input").filter(|v| !v.is_null());
    let inputs = entry.get("inputs").and_then(|v| v.as_array());
    let self_registers = entry.get("selfRegisters").and_then(|v| v.as_object()); // v4
    let extension = entry.get("extension").and_then(|v| v.as_object()); // v5
    let tree_v = entry["version"]["ergoTree"].as_u64().unwrap_or(0) as u8;
    let act_v = entry["version"]["activated"].as_u64().unwrap_or(0) as u8;
    caught(
        || {
            eval::run_entry(
                &tree_bytes,
                input,
                inputs,
                self_registers,
                extension,
                tree_v,
                act_v,
            )
            .to_json()
        },
        eval_panicked,
    )
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
    use super::{caught, eval_panicked, hex_to_bytes, tx_panicked};
    use crate::eval::Outcome;
    use serde_json::{json, Value as J};

    #[test]
    fn caught_turns_a_panic_into_panicked() {
        let j = caught(|| -> J { panic!("kaboom") }, eval_panicked);
        assert_eq!(j["error"], "panicked");
        assert_eq!(j["value"], J::Null);
        assert_eq!(j["cost"], J::Null);
        assert!(j["note"].as_str().unwrap().contains("kaboom"));
    }

    #[test]
    fn caught_with_the_tx_shape_carries_valid() {
        // The tx panic shape carries `valid` (santa-transaction.actuals), not eval's
        // `value`: a value-shaped panic would fail the tx actuals schema.
        let j = caught(|| -> J { panic!("tx kaboom") }, tx_panicked);
        assert_eq!(j["error"], "panicked");
        assert_eq!(j["valid"], J::Null);
        assert_eq!(j["cost"], J::Null);
        assert!(j["note"].as_str().unwrap().contains("tx kaboom"));
    }

    #[test]
    fn caught_passes_through_a_normal_outcome() {
        let j = caught(|| Outcome::Errored.to_json(), eval_panicked);
        assert_eq!(j["error"], "errored");
    }

    /// santa-eval/v4 keystone: `accept-r4-long#0` from Box.getReg_dynamic_index.json.
    /// Script: `{ SELF.getReg[Long](getVar[Int](1).get) }`
    /// SELF R4 = Long 7, var 1 (input) = Int 4.
    ///
    /// The v4 arm is exercised (selfRegisters + var 1 decode, SELF box replacement).
    /// sigma-rust currently cannot parse the dynamic getReg MethodCall tree (MethodId
    /// 19 on SBox / type id 99) — the actual outcome is `errored` today — but that is
    /// a conformance finding, not a harness property. When sigma-rust implements 99:19
    /// the actual will flip to Some(Long 7) (`error: null`) — still a valid verdict.
    ///
    /// This test pins the ENVELOPE only:
    ///   - outcome is well-formed: `error` is null or "errored" (a real verdict, never "panicked")
    ///   - if errored: no `note` field (panicked carries a note; errored must not)
    // do not pin the impl's verdict here — vectors do that; this test pins the envelope.
    #[test]
    fn v4_accept_r4_long_keystone() {
        // tree_bytes_hex from the committed vector entry.
        let tree_bytes = hex_to_bytes("1b0b00dc6313a701e4e3010405").expect("bad hex");
        // selfRegisters: R4 = Long(7)
        let self_registers_json = json!({"4": {"kind": "Long", "value": "7"}});
        let self_registers = self_registers_json.as_object().unwrap();
        // input (var 1): Int(4) — the dynamic register index
        let input_json = json!({"kind": "Int", "value": 4});
        // ergoTree v3, activated v3
        let actual = crate::eval::run_entry(
            &tree_bytes,
            Some(&input_json),
            None,
            Some(self_registers),
            None,
            3,
            3,
        )
        .to_json();
        // Harness-shape: a real verdict — either a value (error null) or errored — never panicked.
        let error = &actual["error"];
        assert!(
            error.is_null() || error == "errored",
            "v4 envelope must yield a verdict (null or errored), got: {}",
            actual
        );
        // If errored, no `note` (panicked carries a note; errored must not).
        if error == "errored" {
            assert!(
                actual.get("note").is_none(),
                "errored must not carry a note; got: {}",
                actual
            );
        }
    }

    /// A schema the runner was not written for (a newer version of one it knows, or a tier it does not mount) is
    /// answered `not-implemented` for every entry: read as a known schema, a new envelope would show as false
    /// divergences (runner-integration §3).
    #[test]
    fn an_unknown_schema_is_not_implemented() {
        use crate::run_vector_file;
        // a tree_bytes_hex and kind/bytes_hex a known arm would happily read
        for schema in ["santa-eval/v99", "santa-wire/v2", "santa-block/v1"] {
            let vector = json!({"schema": schema, "entries": [{
                "name": "e#0", "tree_bytes_hex": "000402", "kind": "Constant", "bytes_hex": "0101",
                "version": {"ergoTree": 0, "activated": 2}, "expected": {"error": null}
            }]});
            let path = std::env::temp_dir().join(format!(
                "blitzen-unknown-schema-{}.json",
                std::process::id()
            ));
            std::fs::write(&path, vector.to_string()).expect("write test vector");
            let results = run_vector_file(&path);
            assert_eq!(results.len(), 1, "{schema}");
            // the tier-neutral form, admitted by every tier's actuals schema: the runner cannot know the actual
            // shape of a schema it was not written for
            assert_eq!(
                results[0].1,
                json!({"error": "not-implemented"}),
                "{schema}"
            );
        }
    }

    /// santa-nipopow/v1 (`tests/fixtures/nipopow-chain-32.json`: SANTA's 32-header chain (from the reference node), its interlinks entry and
    /// one prove entry). This pins the arm's ENVELOPE, not develop's verdicts (the vectors do that): the interlinks
    /// entry answers one interlink vector per chain header, the prove entry a hex `proofHex` (or an honest
    /// errored / panicked), and neither is not-implemented.
    #[test]
    fn nipopow_entries_answer_interlinks_and_a_proof() {
        use crate::run_vector_file;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/nipopow-chain-32.json");
        let results = run_vector_file(&path);
        assert_eq!(results.len(), 2);
        let (_, interlinks, _) = &results[0];
        assert_eq!(interlinks["error"], J::Null, "{interlinks}");
        assert_eq!(
            interlinks["interlinks"].as_array().map(Vec::len),
            Some(32),
            "{interlinks}"
        );
        let (_, prove, _) = &results[1];
        assert_ne!(prove["error"], "not-implemented", "{prove}");
        if prove["error"].is_null() {
            let hex = prove["proofHex"].as_str().expect("proofHex");
            assert!(
                !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()),
                "{prove}"
            );
        }
    }

    /// santa-eval/v6-fullctx: every entry in a v6-fullctx vector must produce
    /// `error: "not-implemented"` — the adapter arm is not yet wired, so the
    /// dispatch must short-circuit BEFORE any eval attempt.
    #[test]
    fn v6_fullctx_returns_not_implemented() {
        use crate::run_vector_file;
        // Minimal synthetic v6-fullctx vector: one entry with a `context` field (no
        // top-level input/inputs/selfRegisters/extension — that's the v6-fullctx shape).
        let vector_json = r#"{
            "schema": "santa-eval/v6-fullctx",
            "entries": [
                {
                    "name": "test-not-implemented#0",
                    "tree_bytes_hex": "0008cd",
                    "context": {},
                    "version": {"ergoTree": 0, "activated": 2},
                    "expected": {"value": null, "cost": null, "error": "not-implemented"}
                }
            ]
        }"#;
        let path =
            std::env::temp_dir().join(format!("blitzen-v6-fullctx-{}.json", std::process::id()));
        std::fs::write(&path, vector_json).expect("write test vector");
        let results = run_vector_file(&path);
        assert_eq!(results.len(), 1, "expected one result");
        let (_name, actual, _expected) = &results[0];
        assert_eq!(
            actual["error"], "not-implemented",
            "v6-fullctx must return not-implemented, got: {}",
            actual
        );
        assert_eq!(actual["value"], J::Null, "value must be null");
        assert_eq!(actual["cost"], J::Null, "cost must be null");
    }
}
