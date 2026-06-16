//! Eval wiring: build a `Context`, evaluate a vector's tree applied to its input
//! (bound at ContextExtension var 1) under the entry's versions, and — when built
//! with the `jit-cost` feature — capture the raw JIT cost.
//!
//! **Impl-agnostic by construction.** The `Context` is cloned from a once-generated
//! `arbitrary` value, so its *field set* is whatever sigma-rust we're built against —
//! no struct literal pins the runner to one fork. Only the fields eval depends on
//! (input binding + versions) are overwritten; the corpus is 82/84 context-independent,
//! so the placeholder boxes/headers are unread for the bulk.
//!
//! The cost path (`jit_cost_value`) and lazy-constants path (`with_constants`) exist
//! only in sigma-rust builds carrying that work (e.g. the eni branch), so they are
//! gated behind the `jit-cost` feature. Without it the runner still evaluates values
//! but reports no cost — so an impl with no JIT-cost model (upstream develop) builds
//! and runs, it just lands in the coal column on cost. Eval entry:
//! `ergotree_interpreter::eval::test_util::try_eval_with_deserialize` — substitutes
//! DeserializeContext nodes against the entry's context (the eager whole-tree pass), then
//! evals on the passed ctx via `try_eval_out` (so JIT-cost capture is unchanged; a
//! substitution failure surfaces as `errored`). The `arbitrary` feature's public path.

use ergotree_ir::chain::context::arbitrary::DummyContextExtensionProvider;
use ergotree_ir::chain::context::Context;
use ergotree_ir::chain::context_extension::ContextExtension;
use ergotree_ir::chain::ergo_box::NonMandatoryRegisterId;
use ergotree_ir::chain::ergo_box::NonMandatoryRegisters;
use ergotree_ir::ergo_tree::{ErgoTree, ErgoTreeVersion};
use ergotree_ir::mir::constant::Constant;
use ergotree_ir::mir::value::Value;
use ergotree_interpreter::eval::test_util::try_eval_with_deserialize;

use crate::sval;

/// One entry's outcome, per the runner contract §3.
pub enum Outcome {
    /// Evaluated to a value. `cost` is `Some` only when built with `jit-cost`;
    /// `None` means the impl has no JIT-cost model (cost not measured).
    Success {
        value: serde_json::Value,
        cost: Option<u64>,
    },
    Errored,
    /// Contract outcome (§3) for an op/method/type the runner doesn't implement.
    /// Not yet emitted — sigma-rust eval reports unimplemented ops as generic
    /// errors not yet distinguished from a genuine `errored`. TODO: map them.
    #[allow(dead_code)]
    NotImplemented,
    /// A failure that isn't a clean eval `errored`: an otherwise-uncaught panic caught by main's
    /// net, OR a SANTA-bridge failure (input decode / result encode) that `run_entry` records
    /// directly. Never-panic, contract §3: always coal, message in `note`. The runner reports the
    /// real failure rather than pre-classifying it into a softer "excuse" outcome.
    Panicked { note: String },
}

impl Outcome {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Outcome::Success { value, cost } => {
                // TODO: once SANTA splits eval/costing, an unmeasured cost is honestly
                // `null` ("cost not emitted"). Until the schema allows that on a
                // success, emit 0 — a number that simply mismatches the blessed cost.
                let cost = cost.map_or_else(|| serde_json::json!(0), |c| serde_json::json!(c));
                serde_json::json!({"value": value, "cost": cost, "error": serde_json::Value::Null})
            }
            Outcome::Errored => {
                serde_json::json!({"value": null, "cost": null, "error": "errored"})
            }
            Outcome::NotImplemented => {
                serde_json::json!({"value": null, "cost": null, "error": "not-implemented"})
            }
            Outcome::Panicked { note } => {
                serde_json::json!({"value": null, "cost": null, "error": "panicked", "note": note})
            }
        }
    }
}

thread_local! {
    /// A once-generated `arbitrary` Context used purely as a struct shell: its field
    /// set adapts to whatever sigma-rust we're built against, so no field literal pins
    /// the runner to one impl. Fixed seed (deterministic); every field eval reads is
    /// overwritten per entry, placeholder fields stay fixed.
    static CONTEXT_TEMPLATE: Context<'static> = arbitrary_context();
}

fn arbitrary_context() -> Context<'static> {
    use proptest::prelude::any;
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;
    any::<Context<'static>>()
        .new_tree(&mut TestRunner::deterministic())
        .expect("Context arbitrary strategy")
        .current()
}

/// Pin the contract's canonical eval context (runner-contract.md Â§2) onto a template
/// clone. The corpus reads context surfaces (CONTEXT.* / preHeader.*), so these fields
/// are load-bearing: preHeader{version = activated+1 (block-version convention â script
/// activation derives from it here), parentId/votes zeroed at wire widths, timestamp 3,
/// nBits 0, height 0, minerPk = generator}, HEIGHT 0, dataInputs empty. `headers` keeps
/// the template's value: sigma-rust's `[Header; 10]` cannot express the pinned EMPTY
/// headers â CONTEXT.headers is a structural divergence by model, not a wiring gap.
/// SELF/outputs keep template values (no committed vector reads them; the v4 arm
/// rebuilds SELF's registers explicitly).
fn pin_canonical_context(ctx: &mut Context<'static>, activated_version: u8) {
    use ergo_chain_types::{ec_point, BlockId, Digest, Votes};
    ctx.height = 0;
    ctx.data_inputs = None;
    ctx.pre_header.version = activated_version + 1;
    ctx.pre_header.parent_id = BlockId(Digest::zero());
    ctx.pre_header.timestamp = 3;
    ctx.pre_header.n_bits = 0;
    ctx.pre_header.height = 0;
    *ctx.pre_header.miner_pk = ec_point::generator();
    ctx.pre_header.votes = Votes([0u8; 3]);
}

/// Build a `Context<'static>` with `input` bound at ContextExtension var 1, at the
/// entry's `(tree_version, activated_version)`. Cloned from the arbitrary template
/// (impl-agnostic field set); the leaked extension is `'static` (short-lived process).
fn build_context(input: Option<Constant>, tree_version: u8, activated_version: u8) -> Context<'static> {
    let mut ctx = CONTEXT_TEMPLATE.with(|t| t.clone());

    let mut ext = ContextExtension::empty();
    if let Some(c) = input {
        ext.values.insert(1u8, c);
    }
    let ext: &'static ContextExtension = Box::leak(Box::new(ext));
    ctx.extension = ext;
    ctx.extension_provider = Box::leak(Box::new(DummyContextExtensionProvider(vec![ext.clone()])));

    ctx.tree_version.set(ErgoTreeVersion::from(tree_version));
    pin_canonical_context(&mut ctx, activated_version);
    ctx
}

/// Build a `Context<'static>` carrying per-input ContextExtensions (santa-eval/v3,
/// getVarFromInput) — one extension per spending-tx input, read by index. The top-level
/// extension is empty (getVar is not used here).
fn build_context_v3(
    input_extensions: Vec<ContextExtension>,
    tree_version: u8,
    activated_version: u8,
) -> Context<'static> {
    let mut ctx = CONTEXT_TEMPLATE.with(|t| t.clone());
    let empty: &'static ContextExtension = Box::leak(Box::new(ContextExtension::empty()));
    ctx.extension = empty;
    ctx.extension_provider = Box::leak(Box::new(DummyContextExtensionProvider(input_extensions)));
    ctx.tree_version.set(ErgoTreeVersion::from(tree_version));
    pin_canonical_context(&mut ctx, activated_version);
    ctx
}

/// Build a `Context<'static>` for santa-eval/v4: the SELF box carries custom non-mandatory
/// registers (R4-R9 from `self_registers`), and `input` is bound at ContextExtension var 1.
///
/// `self_registers` maps "4"-"9" string keys to decoded `Constant`s (same as
/// `sval::decode_constant` output). The registers must be densely packed from R4 — a gap
/// (e.g. R4 + R6, missing R5) is rejected by `NonMandatoryRegisters::new` and bubbles up as
/// `Panicked`. The SELF box from the template is cloned and its registers replaced via
/// `with_additional_registers` (gated behind `arbitrary`, always enabled for blitzen).
fn build_context_v4(
    self_registers: Vec<(NonMandatoryRegisterId, Constant)>,
    input: Constant,
    tree_version: u8,
    activated_version: u8,
) -> Result<Context<'static>, String> {
    let mut ctx = CONTEXT_TEMPLATE.with(|t| t.clone());

    // Build NonMandatoryRegisters (densely packed from R4 upwards).
    let regs = NonMandatoryRegisters::new(self_registers)
        .map_err(|e| format!("selfRegisters: {}", e))?;

    // Replace the SELF box registers: clone the template's self_box, apply registers.
    let new_self: ergotree_ir::chain::ergo_box::ErgoBox =
        ctx.self_box.clone().with_additional_registers(regs);
    let new_self: &'static ergotree_ir::chain::ergo_box::ErgoBox =
        Box::leak(Box::new(new_self));
    ctx.self_box = new_self;

    // Bind var 1 to `input` in the context extension.
    let mut ext = ContextExtension::empty();
    ext.values.insert(1u8, input);
    let ext: &'static ContextExtension = Box::leak(Box::new(ext));
    ctx.extension = ext;
    ctx.extension_provider = Box::leak(Box::new(DummyContextExtensionProvider(vec![ext.clone()])));

    ctx.tree_version.set(ErgoTreeVersion::from(tree_version));
    pin_canonical_context(&mut ctx, activated_version);
    Ok(ctx)
}

/// Build a `Context<'static>` for santa-eval/v5: the SELF box's top-level
/// ContextExtension is the entry's `extension` map verbatim (keys 0..255). A key
/// `>= 0x80` is left in place so sigma-rust's own context-construction guard
/// decides (the JVM crashes on it) — the runner does not pre-filter the key domain.
/// Mirrors the other builders' field set; the leaked extension is `'static`.
fn build_context_v5(
    extension: ContextExtension,
    tree_version: u8,
    activated_version: u8,
) -> Context<'static> {
    let mut ctx = CONTEXT_TEMPLATE.with(|t| t.clone());
    let ext: &'static ContextExtension = Box::leak(Box::new(extension));
    ctx.extension = ext;
    ctx.extension_provider = Box::leak(Box::new(DummyContextExtensionProvider(vec![ext.clone()])));
    ctx.tree_version.set(ErgoTreeVersion::from(tree_version));
    pin_canonical_context(&mut ctx, activated_version);
    ctx
}

// Tree parsing is `ErgoTree::sigma_parse_bytes_lenient`: it accepts the arbitrary-typed
// (non-`SigmaProp`) roots the SANTA corpus carries while parsing the REAL header — so
// size-bit semantics (Rule-1012) are preserved, unlike the retired byte-munging
// `lenient_tree_bytes` (which cleared the size bit and with it the rule's trigger).
// Mirrors the blesser's `deserializeErgoTree(checkType=false)`. Provenance differs by
// branch: eni carries the helper natively (`arbitrary`-gated, 642041be); develop gets
// it from patches/sigma-rust-lenient-parse.patch applied at build time (see santa-run)
// until the helper + Rule-1012 PR merges upstream.

/// Map an input-decode failure to its contract outcome: the library REFUSING the
/// bytes (`BridgeError::Refused` — its parse/`try_from` verdict on oracle-blessed
/// material) is `errored`, a real divergence on an accept vector; any other bridge
/// failure (malformed SANTA JSON, unsupported kind) is the runner's own ⇒
/// `panicked` with the cause in `note`.
fn decode_failure_outcome(e: sval::BridgeError, site: &str) -> Outcome {
    match e {
        sval::BridgeError::Refused(_) => Outcome::Errored,
        other => Outcome::Panicked { note: format!("{}: {:?}", site, other) },
    }
}

/// Evaluate one entry. Produces exactly one [`Outcome`] (totality, contract §3).
pub fn run_entry(
    tree_bytes: &[u8],
    input: Option<&serde_json::Value>,
    inputs: Option<&Vec<serde_json::Value>>,
    self_registers: Option<&serde_json::Map<String, serde_json::Value>>,
    extension: Option<&serde_json::Map<String, serde_json::Value>>,
    tree_version: u8,
    activated_version: u8,
) -> Outcome {
    // v5 (Context.extension_key_domain): the SELF box carries a TOP-LEVEL
    // ContextExtension {key 0..255 -> SValue} (distinct from v3's per-input
    // `inputs[].extension`). Build it verbatim — including any key >= 0x80 — and let
    // sigma-rust decide: ContextExtension keys are a signed `Byte` JVM-side, so a key
    // >= 0x80 crashes JVM context construction, and sigma-rust's matching guard
    // surfaces as `errored` here (where the impl lacks the guard, the divergence
    // surfaces as an accept). The runner does NOT pre-judge the key domain — that
    // would mask an impl that wrongly accepts.
    if let Some(ext_map) = extension {
        let mut ext = ContextExtension::empty();
        for (k, v) in ext_map {
            let id: u8 = match k.parse() {
                Ok(id) => id,
                Err(_) => return Outcome::Errored,
            };
            match sval::decode_constant(v) {
                Ok(c) => {
                    ext.values.insert(id, c);
                }
                Err(e) => return decode_failure_outcome(e, "v5 extension decode"),
            }
        }

        let tree = match ErgoTree::sigma_parse_bytes_lenient(tree_bytes) {
            Ok(t) => t,
            Err(_) => return Outcome::Errored,
        };
        #[cfg(feature = "jit-cost")]
        let root = match tree.root_expr() {
            Ok(r) => r,
            Err(_) => return Outcome::Errored,
        };
        #[cfg(not(feature = "jit-cost"))]
        let root_owned = match tree.proposition() {
            Ok(r) => r,
            Err(_) => return Outcome::Errored,
        };
        #[cfg(not(feature = "jit-cost"))]
        let root = &root_owned;

        let ctx = build_context_v5(ext, tree_version, activated_version);

        #[cfg(feature = "jit-cost")]
        let constants = match tree.constants() {
            Ok(c) => c,
            Err(_) => return Outcome::Errored,
        };
        #[cfg(feature = "jit-cost")]
        let eval_ctx = ctx.with_constants(constants);
        #[cfg(not(feature = "jit-cost"))]
        let eval_ctx = ctx;

        return match try_eval_with_deserialize::<Value<'static>>(root, &eval_ctx) {
            Ok(v) => {
                #[cfg(feature = "jit-cost")]
                let cost = Some(eval_ctx.jit_cost_value());
                #[cfg(not(feature = "jit-cost"))]
                let cost = None;
                match sval::encode_value(&v) {
                    Ok(value) => Outcome::Success { value, cost },
                    Err(e) => Outcome::Panicked { note: format!("result encode: {:?}", e) },
                }
            }
            Err(_) => Outcome::Errored,
        };
    }
    // v4 (Box.getReg dynamic-index): SELF box has custom non-mandatory registers + var 1 = index.
    // `self_registers` present ⇒ v4 entry — build SELF with registers and bind var 1.
    // Must be checked before the v2 `input_constant` path (v4 also has `input`).
    if let Some(reg_map) = self_registers {
        // Decode var 1 input (the register-index selector).
        let input_json = match input {
            Some(j) => j,
            None => return Outcome::Panicked { note: "v4 entry missing input".into() },
        };
        let var1 = match sval::decode_constant(input_json) {
            Ok(c) => c,
            Err(e) => return decode_failure_outcome(e, "v4 input decode"),
        };
        // Decode selfRegisters: "4"-"9" → (NonMandatoryRegisterId, Constant), sorted by id.
        let mut pairs: Vec<(NonMandatoryRegisterId, Constant)> = Vec::with_capacity(reg_map.len());
        for (k, v) in reg_map {
            let id: u8 = match k.parse() {
                Ok(n) => n,
                Err(_) => return Outcome::Panicked { note: format!("v4 selfRegisters: bad key {:?}", k) },
            };
            let reg_id = match id {
                4 => NonMandatoryRegisterId::R4,
                5 => NonMandatoryRegisterId::R5,
                6 => NonMandatoryRegisterId::R6,
                7 => NonMandatoryRegisterId::R7,
                8 => NonMandatoryRegisterId::R8,
                9 => NonMandatoryRegisterId::R9,
                _ => return Outcome::Panicked { note: format!("v4 selfRegisters: key {} out of R4-R9 range", id) },
            };
            match sval::decode_constant(v) {
                Ok(c) => pairs.push((reg_id, c)),
                Err(e) => return decode_failure_outcome(e, &format!("v4 selfRegisters[{}] decode", k)),
            }
        }
        // Sort by register id (R4 < R5 < … < R9) to ensure dense packing order.
        pairs.sort_by_key(|(rid, _)| *rid as u8);

        // Parse the tree before building context (lenient: build-patched entry, see above).
        let tree = match ErgoTree::sigma_parse_bytes_lenient(tree_bytes) {
            Ok(t) => t,
            Err(_) => return Outcome::Errored,
        };
        #[cfg(feature = "jit-cost")]
        let root = match tree.root_expr() {
            Ok(r) => r,
            Err(_) => return Outcome::Errored,
        };
        #[cfg(not(feature = "jit-cost"))]
        let root_owned = match tree.proposition() {
            Ok(r) => r,
            Err(_) => return Outcome::Errored,
        };
        #[cfg(not(feature = "jit-cost"))]
        let root = &root_owned;

        let ctx = match build_context_v4(pairs, var1, tree_version, activated_version) {
            Ok(c) => c,
            Err(e) => return Outcome::Panicked { note: e },
        };

        #[cfg(feature = "jit-cost")]
        let constants = match tree.constants() {
            Ok(c) => c,
            Err(_) => return Outcome::Errored,
        };
        #[cfg(feature = "jit-cost")]
        let eval_ctx = ctx.with_constants(constants);
        #[cfg(not(feature = "jit-cost"))]
        let eval_ctx = ctx;

        return match try_eval_with_deserialize::<Value<'static>>(root, &eval_ctx) {
            Ok(v) => {
                #[cfg(feature = "jit-cost")]
                let cost = Some(eval_ctx.jit_cost_value());
                #[cfg(not(feature = "jit-cost"))]
                let cost = None;
                match sval::encode_value(&v) {
                    Ok(value) => Outcome::Success { value, cost },
                    Err(e) => Outcome::Panicked { note: format!("result encode: {:?}", e) },
                }
            }
            Err(_) => Outcome::Errored,
        };
    }

    // Decode the input. v2: a single SValue bound at ContextExtension var 1. The library
    // REFUSING the input bytes (try_from/parse — its verdict on oracle-blessed material)
    // is a real divergence ⇒ `errored`; a SANTA-side bridge failure (malformed JSON,
    // unsupported kind) is the runner's own ⇒ `panicked` with the cause in `note`.
    let input_constant = match input {
        Some(j) => match sval::decode_constant(j) {
            Ok(c) => Some(c),
            Err(e) => return decode_failure_outcome(e, "input decode"),
        },
        None => None,
    };
    // v3 (getVarFromInput): per-input ContextExtensions — one {varId -> Constant} map per
    // spending-tx input, read by index. Present ⇒ a v3 entry (build_context_v3 below).
    let input_extensions: Option<Vec<ContextExtension>> = match inputs {
        Some(arr) => {
            let mut exts = Vec::with_capacity(arr.len());
            for inp in arr {
                let mut ext = ContextExtension::empty();
                if let Some(map) = inp.get("extension").and_then(|e| e.as_object()) {
                    for (k, v) in map {
                        let id: u8 = match k.parse() {
                            Ok(id) => id,
                            Err(_) => return Outcome::Errored,
                        };
                        match sval::decode_constant(v) {
                            Ok(c) => {
                                ext.values.insert(id, c);
                            }
                            Err(e) => return decode_failure_outcome(e, "input decode (v3)"),
                        }
                    }
                }
                exts.push(ext);
            }
            Some(exts)
        }
        None => None,
    };

    // Parse the outer tree (lenient: build-patched entry, see above). A tree the JVM
    // blessed but sigma-rust cannot parse is a real divergence surfaced as `errored`
    // (it will mismatch a success `expected`).
    let tree = match ErgoTree::sigma_parse_bytes_lenient(tree_bytes) {
        Ok(t) => t,
        Err(_) => return Outcome::Errored,
    };
    // Root accessor is impl-specific (gate it like the constants block below): eni's
    // `root_expr()` (jit-cost) borrows the root with ConstPlaceholders retained for lazy
    // `with_constants` resolution; upstream develop has no `root_expr()` — its
    // `proposition()` returns an owned `Expr` with constants already inlined.
    #[cfg(feature = "jit-cost")]
    let root = match tree.root_expr() {
        Ok(r) => r,
        Err(_) => return Outcome::Errored,
    };
    #[cfg(not(feature = "jit-cost"))]
    let root_owned = match tree.proposition() {
        Ok(r) => r,
        Err(_) => return Outcome::Errored,
    };
    #[cfg(not(feature = "jit-cost"))]
    let root = &root_owned;

    let ctx = match input_extensions {
        Some(exts) => build_context_v3(exts, tree_version, activated_version),
        None => build_context(input_constant, tree_version, activated_version),
    };

    // Constant binding differs by impl: with `jit-cost` (eni) the tree keeps
    // ConstPlaceholders resolved lazily from the context; without it (upstream)
    // constants are already inlined in `root_expr`.
    #[cfg(feature = "jit-cost")]
    let constants = match tree.constants() {
        Ok(c) => c,
        Err(_) => return Outcome::Errored,
    };
    #[cfg(feature = "jit-cost")]
    let eval_ctx = ctx.with_constants(constants);
    #[cfg(not(feature = "jit-cost"))]
    let eval_ctx = ctx;

    match try_eval_with_deserialize::<Value<'static>>(root, &eval_ctx) {
        Ok(v) => {
            #[cfg(feature = "jit-cost")]
            let cost = Some(eval_ctx.jit_cost_value());
            #[cfg(not(feature = "jit-cost"))]
            let cost = None;
            match sval::encode_value(&v) {
                Ok(value) => Outcome::Success { value, cost },
                Err(e) => Outcome::Panicked { note: format!("result encode: {:?}", e) },
            }
        }
        Err(_) => Outcome::Errored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decode-failure contract split: a library refusal is `errored` (the
    /// impl's verdict on oracle-blessed input — a gradeable divergence); any
    /// other bridge failure stays `panicked` with the site in the note.
    #[test]
    fn decode_failure_outcome_split() {
        assert!(matches!(
            decode_failure_outcome(sval::BridgeError::Refused("x".into()), "input decode"),
            Outcome::Errored
        ));
        match decode_failure_outcome(sval::BridgeError::Decode("x".into()), "input decode") {
            Outcome::Panicked { note } => assert!(note.starts_with("input decode:"), "note: {}", note),
            other => panic!("expected Panicked, got {:?}", outcome_name(&other)),
        }
    }

    fn outcome_name(o: &Outcome) -> &'static str {
        match o {
            Outcome::Success { .. } => "Success",
            Outcome::Errored => "Errored",
            Outcome::NotImplemented => "NotImplemented",
            Outcome::Panicked { .. } => "Panicked",
        }
    }
}
