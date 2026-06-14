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

/// Pin the contract's canonical eval context (runner-contract.md §2) onto a template
/// clone. The corpus reads context surfaces (CONTEXT.* / preHeader.*), so these fields
/// are load-bearing: preHeader{version = activated+1 (block-version convention — script
/// activation derives from it here), parentId/votes zeroed at wire widths, timestamp 3,
/// nBits 0, height 0, minerPk = generator}, HEIGHT 0, dataInputs empty, `headers` EMPTY
/// (`ContextHeaders` expresses the pinned empty seq since the BoundedVec relaxation),
/// `lastBlockUtxoRoot` = the contract's dummy AvlTree (zero digest, all ops allowed,
/// keyLength 32, no value-length). SELF/outputs keep template values (no committed
/// vector reads them; the v4 arm rebuilds SELF's registers explicitly).
fn pin_canonical_context(ctx: &mut Context<'static>, activated_version: u8) {
    use ergo_chain_types::{ec_point, BlockId, Digest, Votes};
    use ergotree_ir::chain::context::ContextHeaders;
    use ergotree_ir::mir::avl_tree_data::{AvlTreeData, AvlTreeFlags};
    ctx.height = 0;
    ctx.data_inputs = None;
    ctx.pre_header.version = activated_version + 1;
    ctx.pre_header.parent_id = BlockId(Digest::zero());
    ctx.pre_header.timestamp = 3;
    ctx.pre_header.n_bits = 0;
    ctx.pre_header.height = 0;
    ctx.pre_header.miner_pk = Box::new(ec_point::generator());
    ctx.pre_header.votes = Votes([0u8; 3]);
    ctx.headers = ContextHeaders::from_vec(vec![]).expect("empty headers within bounds");
    ctx.last_block_utxo_root = AvlTreeData {
        // 33 zero bytes — the contract's canonical stateRoot digest. The field went
        // variable-length `Vec<u8>` in eni 3e27412b (updateDigest stores any-length
        // Coll[Byte]); the canonical value is unchanged.
        digest: vec![0u8; 33],
        tree_flags: AvlTreeFlags::new(true, true, true),
        key_length: 32,
        value_length_opt: None,
    };
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

/// Reconstruction failure for the v6-fullctx envelope, split by who failed (mirrors
/// [`decode_failure_outcome`]): `Refused` = the library refusing oracle-blessed consensus
/// material — a box/header/preHeader/minerPk it cannot parse, or an out-of-bounds
/// collection — surfaced as `errored`, the divergence it is; `Malformed` = a defect in
/// the SANTA envelope or this runner's consumption of it (missing/mistyped field, bad
/// hex), surfaced as `panicked` with the cause in `note` (the runner's own failure).
enum FullCtxError {
    Refused(String),
    Malformed(String),
}

/// Reconstruct a real `Context` from a `santa-eval/v6-fullctx` `context` envelope
/// (runner-contract §2). Unlike the v1–v5 builders — which pin the canonical *dummy*
/// context — this parses the envelope's real boxes/headers/preHeader and derives
/// `last_block_utxo_root` from `headers[0].state_root`, so the script sees true
/// blockchain state. Cloned from the impl-agnostic template (no struct literal), then
/// every load-bearing field is overwritten; the leaked boxes/extensions are `'static`
/// (short-lived process). Box ids are sigma-rust's own (re-serialized on parse), not the
/// retained input bytes — for canonical boxes the two coincide.
fn build_context_v6_fullctx(
    context: &serde_json::Value,
    tree_version: u8,
) -> Result<Context<'static>, FullCtxError> {
    use ergo_chain_types::{BlockId, Digest, EcPoint, Header, PreHeader, Votes};
    use ergotree_ir::chain::context::{ContextHeaders, TxIoVec};
    use ergotree_ir::chain::ergo_box::ErgoBox;
    use ergotree_ir::mir::avl_tree_data::{AvlTreeData, AvlTreeFlags};
    use ergotree_ir::serialization::SigmaSerializable;
    use sigma_ser::ScorexSerializable;

    let obj = context
        .as_object()
        .ok_or_else(|| FullCtxError::Malformed("context not an object".into()))?;

    // Parse a hex-array box field (`inputs` / `data_inputs` / `outputs`). A sigma-rust
    // parse refusal of oracle-blessed bytes is `Refused` (errored); a shape/hex defect
    // is `Malformed` (panicked).
    let parse_boxes = |key: &str| -> Result<Vec<ErgoBox>, FullCtxError> {
        let arr = obj
            .get(key)
            .and_then(|v| v.as_array())
            .ok_or_else(|| FullCtxError::Malformed(format!("context.{key} missing or not an array")))?;
        let mut out = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            let h = item
                .as_str()
                .ok_or_else(|| FullCtxError::Malformed(format!("context.{key}[{i}] not a string")))?;
            let bytes = crate::hex_to_bytes(h)
                .map_err(|_| FullCtxError::Malformed(format!("context.{key}[{i}] bad hex")))?;
            let b = ErgoBox::sigma_parse_bytes(&bytes)
                .map_err(|e| FullCtxError::Refused(format!("context.{key}[{i}]: {e:?}")))?;
            out.push(b);
        }
        Ok(out)
    };

    let inputs_vec = parse_boxes("inputs")?;
    let data_inputs_vec = parse_boxes("data_inputs")?;
    let outputs_vec = parse_boxes("outputs")?;

    // Headers (descending, newest first; up to 10 — fewer near genesis).
    let headers_arr = obj
        .get("headers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| FullCtxError::Malformed("context.headers missing or not an array".into()))?;
    let mut headers_vec: Vec<Header> = Vec::with_capacity(headers_arr.len());
    for (i, item) in headers_arr.iter().enumerate() {
        let h = item
            .as_str()
            .ok_or_else(|| FullCtxError::Malformed(format!("context.headers[{i}] not a string")))?;
        let bytes = crate::hex_to_bytes(h)
            .map_err(|_| FullCtxError::Malformed(format!("context.headers[{i}] bad hex")))?;
        headers_vec.push(
            Header::scorex_parse_bytes(&bytes)
                .map_err(|e| FullCtxError::Refused(format!("context.headers[{i}]: {e:?}")))?,
        );
    }

    // PreHeader: decode the bespoke LEB128 sub-encoding, parse minerPk (SEC1), map to
    // the sigma-rust type. The real block version from the envelope is used directly —
    // the v1–v5 `activated + 1` dummy pin is NOT applied on this path (contract §2).
    let ph_hex = obj
        .get("pre_header_hex")
        .and_then(|v| v.as_str())
        .ok_or_else(|| FullCtxError::Malformed("context.pre_header_hex missing".into()))?;
    let ph_bytes = crate::hex_to_bytes(ph_hex)
        .map_err(|_| FullCtxError::Malformed("context.pre_header_hex bad hex".into()))?;
    let phf = crate::preheader::decode(&ph_bytes)
        .map_err(|e| FullCtxError::Refused(format!("pre_header decode: {e}")))?;
    let miner_pk = EcPoint::scorex_parse_bytes(&phf.miner_pk)
        .map_err(|e| FullCtxError::Refused(format!("minerPk parse: {e:?}")))?;
    let pre_header = PreHeader {
        version: phf.version,
        parent_id: BlockId(Digest::<32>::from(phf.parent_id)),
        timestamp: phf.timestamp,
        n_bits: phf.n_bits,
        height: phf.height,
        miner_pk: Box::new(miner_pk),
        votes: Votes(phf.votes),
    };

    let height = obj
        .get("height")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| FullCtxError::Malformed("context.height missing or not a number".into()))?
        as u32;
    let self_index = obj
        .get("self_index")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| FullCtxError::Malformed("context.self_index missing or not a number".into()))?
        as usize;

    // last_block_utxo_root: an explicit `last_block_utxo_root_hex` digest overrides the
    // derivation; otherwise digest = headers[0].state_root (33B), flags 0x07, keyLen 32.
    let digest: Vec<u8> = if let Some(hex) = obj.get("last_block_utxo_root_hex").and_then(|v| v.as_str()) {
        crate::hex_to_bytes(hex)
            .map_err(|_| FullCtxError::Malformed("context.last_block_utxo_root_hex bad hex".into()))?
    } else {
        headers_vec
            .first()
            .map(|h| h.state_root.0.to_vec())
            .ok_or_else(|| FullCtxError::Malformed("no headers and no last_block_utxo_root_hex".into()))?
    };
    let last_block_utxo_root = AvlTreeData {
        digest,
        tree_flags: AvlTreeFlags::new(true, true, true),
        key_length: 32,
        value_length_opt: None,
    };

    // Per-input ContextExtensions (getVarFromInput by index). The SELF input's extension
    // is also the top-level `ctx.extension`; the provider serves every input by index.
    let mut input_extensions: Vec<ContextExtension> = Vec::new();
    if let Some(arr) = obj.get("input_extensions").and_then(|v| v.as_array()) {
        for (i, item) in arr.iter().enumerate() {
            let mut ext = ContextExtension::empty();
            if let Some(map) = item.as_object() {
                for (k, v) in map {
                    let id: u8 = k
                        .parse()
                        .map_err(|_| FullCtxError::Malformed(format!("input_extensions[{i}] bad key {k:?}")))?;
                    let c = sval::decode_constant(v).map_err(|e| match e {
                        sval::BridgeError::Refused(m) => {
                            FullCtxError::Refused(format!("input_extensions[{i}][{k}]: {m}"))
                        }
                        other => FullCtxError::Malformed(format!("input_extensions[{i}][{k}]: {other:?}")),
                    })?;
                    ext.values.insert(id, c);
                }
            }
            input_extensions.push(ext);
        }
    }

    // Assemble onto the impl-agnostic template shell, overwriting every read field.
    let mut ctx = CONTEXT_TEMPLATE.with(|t| t.clone());
    let inputs_leaked: &'static [ErgoBox] = Box::leak(inputs_vec.into_boxed_slice());
    let outputs_leaked: &'static [ErgoBox] = Box::leak(outputs_vec.into_boxed_slice());
    let self_box: &'static ErgoBox = inputs_leaked.get(self_index).ok_or_else(|| {
        FullCtxError::Malformed(format!(
            "self_index {self_index} out of range ({} inputs)",
            inputs_leaked.len()
        ))
    })?;
    ctx.self_box = self_box;
    ctx.outputs = outputs_leaked;
    ctx.inputs = TxIoVec::from_vec(inputs_leaked.iter().collect())
        .map_err(|e| FullCtxError::Refused(format!("inputs bounds: {e:?}")))?;
    ctx.data_inputs = if data_inputs_vec.is_empty() {
        None
    } else {
        let di_leaked: &'static [ErgoBox] = Box::leak(data_inputs_vec.into_boxed_slice());
        Some(
            TxIoVec::from_vec(di_leaked.iter().collect())
                .map_err(|e| FullCtxError::Refused(format!("data_inputs bounds: {e:?}")))?,
        )
    };
    ctx.pre_header = pre_header;
    ctx.last_block_utxo_root = last_block_utxo_root;
    ctx.headers = ContextHeaders::from_vec(headers_vec)
        .map_err(|e| FullCtxError::Refused(format!("headers bounds: {e:?}")))?;
    ctx.height = height;
    let self_ext = input_extensions
        .get(self_index)
        .cloned()
        .unwrap_or_else(ContextExtension::empty);
    ctx.extension = Box::leak(Box::new(self_ext));
    ctx.extension_provider = Box::leak(Box::new(DummyContextExtensionProvider(input_extensions)));
    ctx.tree_version.set(ErgoTreeVersion::from(tree_version));
    ctx.jit_cost_limit = None;
    ctx.reset_jit_cost();
    Ok(ctx)
}

/// Evaluate one `santa-eval/v6-fullctx` entry: reconstruct the real context from the
/// envelope, then evaluate the tree exactly as the v1–v5 paths do (lenient parse; lazy
/// constants under `jit-cost`). Totality (contract §3) — produces exactly one `Outcome`.
pub fn run_entry_fullctx(tree_bytes: &[u8], context: &serde_json::Value, tree_version: u8) -> Outcome {
    let ctx = match build_context_v6_fullctx(context, tree_version) {
        Ok(c) => c,
        Err(FullCtxError::Refused(reason)) => {
            // The library refused oracle-blessed material (a box/header/preHeader it
            // cannot parse) — a real divergence, graded `errored` (which carries no note
            // in the actuals, contract §3); the reason goes to stderr for diagnosis only.
            eprintln!("blitzen: v6-fullctx reconstruction refused: {reason}");
            return Outcome::Errored;
        }
        Err(FullCtxError::Malformed(note)) => return Outcome::Panicked { note },
    };

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
                Err(e) => Outcome::Panicked { note: format!("result encode: {e:?}") },
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
