//! Eval wiring: build a deterministic `Context`, evaluate a vector's tree applied
//! to its input (bound at ContextExtension var 1) under the entry's versions, and
//! capture the raw JIT cost.
//!
//! The corpus is 82/84 context-independent (only `verify_should_respect_Context`
//! and one NEQ vector read context), so a fixed minimal context is correct for the
//! bulk; the header/box values are placeholders (unread). Matching the JVM's
//! `ErgoLikeContextTesting.dummy` for the two context-reading files is a follow-up.
//!
//! Eval entry: `ergotree_interpreter::eval::test_util::try_eval_out` (the `arbitrary`
//! feature's public arbitrary-root path) — it evaluates on the *passed* ctx, so the
//! accumulated `jit_cost_value()` is readable afterward.

use core::cell::Cell;

use ergo_chain_types::{ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Header, PreHeader, Votes};
use ergotree_ir::chain::context::arbitrary::DummyContextExtensionProvider;
use ergotree_ir::chain::context::Context;
use ergotree_ir::chain::context_extension::ContextExtension;
use ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergotree_ir::chain::ergo_box::{ErgoBox, NonMandatoryRegisters};
use ergotree_ir::chain::tx_id::TxId;
use ergotree_ir::ergo_tree::{ErgoTree, ErgoTreeVersion};
use ergotree_ir::mir::constant::Constant;
use ergotree_ir::mir::value::Value;
use ergotree_ir::serialization::SigmaSerializable;
use ergotree_interpreter::eval::test_util::try_eval_out;

use crate::sval::{self, BridgeError};

/// One entry's outcome, per the runner contract §3.
pub enum Outcome {
    Success { value: serde_json::Value, cost: u64 },
    Errored,
    NotImplemented,
    Unrepresentable,
}

impl Outcome {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Outcome::Success { value, cost } => {
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

/// The secp256k1 generator as an `EcPoint`. Any valid point works for the
/// placeholder box script / miner key (unread by context-independent ops).
fn gen_point() -> EcPoint {
    const G: [u8; 33] = [
        0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
        0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16,
        0xf8, 0x17, 0x98,
    ];
    EcPoint::sigma_parse_bytes(&G).expect("generator point")
}

/// A fixed, valid-but-placeholder self box (a generator-key P2PK box). Unread by
/// context-independent ops; values are not meant to match the JVM dummy yet.
fn dummy_box() -> ErgoBox {
    let mut tree_bytes = vec![0x00u8, 0x08, 0xcd];
    tree_bytes.extend_from_slice(
        &gen_point()
            .sigma_serialize_bytes()
            .expect("generator serialize"),
    );
    let tree = ErgoTree::sigma_parse_bytes(&tree_bytes).expect("dummy P2PK tree");
    ErgoBox::new(
        BoxValue::try_from(1_000_000u64).expect("box value"),
        tree,
        None,
        NonMandatoryRegisters::empty(),
        0,
        TxId(Digest32::zero()),
        0,
    )
    .expect("dummy box")
}

/// A fixed, valid-but-placeholder Autolykos-v2 header (zeroed digests, generator
/// miner key). Unread by context-independent ops.
fn dummy_header() -> Header {
    Header {
        version: 2,
        id: BlockId(Digest32::zero()),
        parent_id: BlockId(Digest32::zero()),
        ad_proofs_root: Digest32::zero(),
        state_root: ADDigest::zero(),
        transaction_root: Digest32::zero(),
        timestamp: 0,
        n_bits: 0,
        height: 0,
        extension_root: Digest32::zero(),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(gen_point()),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes([0, 0, 0]),
        unparsed_bytes: Box::new([]),
    }
}

/// Build a deterministic `Context<'static>` with `input` bound at extension var 1,
/// at the entry's `(tree_version, activated_version)`. Borrowed fields are leaked
/// to `'static` (mirroring the upstream `Arbitrary` impl); the runner process is
/// short-lived, so the leak is bounded and acceptable.
fn build_context(input: Option<Constant>, tree_version: u8, activated_version: u8) -> Context<'static> {
    let self_box: &'static ErgoBox = Box::leak(Box::new(dummy_box()));
    let outputs: &'static [ErgoBox] = core::slice::from_ref(self_box);
    let inputs = vec![self_box].try_into().expect("inputs bounded vec");

    let mut ext = ContextExtension::empty();
    if let Some(c) = input {
        ext.values.insert(1u8, c);
    }
    let ext: &'static ContextExtension = Box::leak(Box::new(ext));
    let provider: &'static DummyContextExtensionProvider =
        Box::leak(Box::new(DummyContextExtensionProvider(vec![ext.clone()])));

    let header = dummy_header();
    let mut pre_header = PreHeader::from(header.clone());
    pre_header.version = activated_version + 1;
    let headers: [Header; 10] = core::array::from_fn(|_| header.clone());

    Context {
        height: 0,
        self_box,
        outputs,
        data_inputs: None,
        inputs,
        pre_header,
        headers,
        extension: ext,
        tree_version: Cell::new(ErgoTreeVersion::from(tree_version)),
        extension_provider: provider,
        jit_cost: Cell::new(0),
        jit_cost_limit: None,
        constants: None,
    }
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
    tree_version: u8,
    activated_version: u8,
) -> Outcome {
    // Decode the input SValue (bound at var 1). A representable-but-unsupported
    // input kind ⇒ unrepresentable.
    let input_constant = match input {
        Some(j) => match sval::decode_constant(j) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("DECODE_ERR: {:?}", e);
                return Outcome::Unrepresentable;
            }
        },
        None => None,
    };

    // Parse the outer tree. A tree the JVM blessed but sigma-rust cannot parse is a
    // real divergence surfaced as `errored` (it will mismatch a success `expected`).
    let lenient = lenient_tree_bytes(tree_bytes);
    let tree = match ErgoTree::sigma_parse_bytes(&lenient) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("PARSE_ERR: {:?}", e);
            return Outcome::Errored;
        }
    };
    let root = match tree.root_expr() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ROOT_ERR: {:?}", e);
            return Outcome::Errored;
        }
    };
    let constants = match tree.constants() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("CONST_ERR: {:?}", e);
            return Outcome::Errored;
        }
    };

    let ctx = build_context(input_constant, tree_version, activated_version);
    let ctx_with_c = ctx.with_constants(constants);

    match try_eval_out::<Value<'static>>(root, &ctx_with_c) {
        Ok(v) => {
            let cost = ctx_with_c.jit_cost_value();
            match sval::encode_value(&v) {
                Ok(value) => Outcome::Success { value, cost },
                Err(_) => Outcome::Unrepresentable,
            }
        }
        Err(e) => {
            eprintln!("EVAL_ERR: {:?}", e);
            Outcome::Errored
        }
    }
}
