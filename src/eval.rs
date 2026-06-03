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
//! `ergotree_interpreter::eval::test_util::try_eval_out` (the `arbitrary` feature's
//! public arbitrary-root path), evaluating on the passed ctx.

use ergotree_ir::chain::context::arbitrary::DummyContextExtensionProvider;
use ergotree_ir::chain::context::Context;
use ergotree_ir::chain::context_extension::ContextExtension;
use ergotree_ir::ergo_tree::{ErgoTree, ErgoTreeVersion};
use ergotree_ir::mir::constant::Constant;
use ergotree_ir::mir::value::Value;
use ergotree_ir::serialization::SigmaSerializable;
use ergotree_interpreter::eval::test_util::try_eval_out;

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
    Unrepresentable,
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
            Outcome::Unrepresentable => {
                serde_json::json!({"value": null, "cost": null, "error": "unrepresentable"})
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
    ctx.pre_header.version = activated_version + 1;
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
    ctx.pre_header.version = activated_version + 1;
    ctx
}

/// Make tree bytes leniently parseable. sigma-rust's `ErgoTree::sigma_parse` rejects
/// a non-`SigmaProp` root on size-bit (v1+) trees (→ `Unparsed`/`RootTpeError`), but
/// SANTA corpus roots are arbitrary-typed. Clearing the size bit and dropping the
/// size VLQ routes parsing through the non-sized path, which has no root-type check
/// (the blesser does the equivalent "lenient deserialize for non-SigmaProp roots").
fn lenient_tree_bytes(bytes: &[u8]) -> Vec<u8> {
    const HAS_SIZE: u8 = 0x08;
    if bytes.is_empty() || bytes[0] & HAS_SIZE == 0 {
        return bytes.to_vec();
    }
    // The size is a VLQ-u32 starting at index 1; skip it (continuation = high bit set).
    let mut end = 1;
    while end < bytes.len() && bytes[end] & 0x80 != 0 {
        end += 1;
    }
    end += 1; // include the final VLQ byte (high bit clear)
    let mut out = Vec::with_capacity(bytes.len().saturating_sub(end - 1));
    out.push(bytes[0] & !HAS_SIZE);
    out.extend_from_slice(bytes.get(end..).unwrap_or_default());
    out
}

/// Evaluate one entry. Produces exactly one [`Outcome`] (totality, contract §3).
pub fn run_entry(
    tree_bytes: &[u8],
    input: Option<&serde_json::Value>,
    inputs: Option<&Vec<serde_json::Value>>,
    tree_version: u8,
    activated_version: u8,
) -> Outcome {
    // Decode the input. v2: a single SValue bound at ContextExtension var 1. A
    // representable-but-unsupported input kind ⇒ unrepresentable.
    let input_constant = match input {
        Some(j) => match sval::decode_constant(j) {
            Ok(c) => Some(c),
            Err(_) => return Outcome::Unrepresentable,
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
                            Err(_) => return Outcome::Unrepresentable,
                        }
                    }
                }
                exts.push(ext);
            }
            Some(exts)
        }
        None => None,
    };

    // Parse the outer tree. A tree the JVM blessed but sigma-rust cannot parse is a
    // real divergence surfaced as `errored` (it will mismatch a success `expected`).
    let lenient = lenient_tree_bytes(tree_bytes);
    let tree = match ErgoTree::sigma_parse_bytes(&lenient) {
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

    match try_eval_out::<Value<'static>>(root, &eval_ctx) {
        Ok(v) => {
            #[cfg(feature = "jit-cost")]
            let cost = Some(eval_ctx.jit_cost_value());
            #[cfg(not(feature = "jit-cost"))]
            let cost = None;
            match sval::encode_value(&v) {
                Ok(value) => Outcome::Success { value, cost },
                Err(_) => Outcome::Unrepresentable,
            }
        }
        Err(_) => Outcome::Errored,
    }
}
