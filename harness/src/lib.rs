//! santa-harness — what a Rust SANTA runner needs besides its implementation (SANTA's runner-integration §3, §3a, §4):
//! the grading interpreter (a copy of SANTA's santa-check), selection, the oracle self-checks, the bounded-file
//! watchdog and the results it writes. It reads a SANTA checkout as data only. A copy of this crate lives in each Rust
//! runner SANTA maintains (blitzen, donner, vixen); keep the copies identical.
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

pub mod bounds;
pub mod equal;
pub mod grading;
pub mod oracle;
pub mod selection;

pub use equal::structural_equal;

/// The version slots, ordered; a runner's `version` selects its slot and every lower one.
pub const VERSIONS: [&str; 3] = ["v4", "v5", "v6"];
/// SANTA's tiers.
pub const TIERS: [&str; 7] = ["eval", "wire", "transaction", "block", "chain", "authds", "nipopow"];
/// The wire kinds (the wire vector schema's `kind` enum).
pub const WIRE_KINDS: [&str; 7] = ["Constant", "Box", "Transaction", "Header", "SigmaBoolean", "ErgoTree", "BlockTransactions"];

/// What a runner declares it implements (runner-integration §2): it decides which vector files the runner grades.
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub version: String,
    pub tiers: Vec<String>,
    /// Whether the runner claims the cost dimension (default true).
    pub cost: bool,
    /// The wire kinds it implements; None = every kind.
    pub kinds: Option<Vec<String>>,
    /// Whether it applies the rules tied to the activated script version (default true).
    pub activation: bool,
}

/// The scope keys of a runner.json (any other key, such as SANTA's `name` or `impl`, is ignored). Refused as SANTA's
/// conform refuses them: an unknown version; empty, unknown or repeated tiers; empty, unknown or repeated kinds, or
/// kinds without the wire tier; a flag that is not a boolean.
pub fn parse_scope(j: &Value) -> Result<Scope, String> {
    let version = j["version"].as_str().ok_or("runner.json: no \"version\"")?.to_string();
    if !VERSIONS.contains(&version.as_str()) {
        return Err(format!("runner.json: unknown version {version:?}; known: {VERSIONS:?}"));
    }
    let names = |k: &str| -> Result<Vec<String>, String> {
        j[k].as_array()
            .ok_or(format!("runner.json: {k:?} is not an array"))?
            .iter()
            .map(|t| t.as_str().map(String::from).ok_or(format!("runner.json: a {k} entry is not a string")))
            .collect()
    };
    let distinct = |xs: &[String], known: &[&str], what: &str| -> Result<(), String> {
        if xs.is_empty() {
            return Err(format!("runner.json: {what} is empty"));
        }
        for (i, x) in xs.iter().enumerate() {
            if !known.contains(&x.as_str()) {
                return Err(format!("runner.json: unknown {what} {x:?}"));
            }
            if xs[..i].contains(x) {
                return Err(format!("runner.json: {what} {x:?} listed twice"));
            }
        }
        Ok(())
    };
    let tiers = names("tiers")?;
    distinct(&tiers, &TIERS, "tier")?;
    let kinds = match j.get("kinds") {
        None | Some(Value::Null) => None,
        Some(_) => {
            let ks = names("kinds")?;
            distinct(&ks, &WIRE_KINDS, "wire kind")?;
            if !tiers.iter().any(|t| t == "wire") {
                return Err("runner.json: \"kinds\" declared without the wire tier".into());
            }
            Some(ks)
        }
    };
    let flag = |k: &str| match j.get(k) {
        None => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(v) => Err(format!("runner.json: {k:?} is {v}, not a boolean")),
    };
    Ok(Scope { version, tiers, cost: flag("cost")?, kinds, activation: flag("activation")? })
}

/// The runner's scope, from its runner.json.
pub fn read_scope(runner_json: &Path) -> Result<Scope, String> {
    let text = fs::read_to_string(runner_json).map_err(|e| format!("{}: {e}", runner_json.display()))?;
    let j: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", runner_json.display()))?;
    parse_scope(&j)
}

/// The SANTA commit graded against: `git rev-parse HEAD` in `santa`, when `santa` is the top of its git checkout,
/// compared as real paths (runner-integration §4.3); otherwise None.
pub fn santa_commit(santa: &Path) -> Option<String> {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(santa)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let top = git(&["rev-parse", "--show-toplevel"])?;
    if fs::canonicalize(&top).ok()? != fs::canonicalize(santa).ok()? {
        return None;
    }
    git(&["rev-parse", "HEAD"])
}
