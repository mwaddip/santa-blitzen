//! Transaction-tier validation: decode one captured entry's tx + input boxes from
//! node-API JSON (sigma-rust serde), build the minimal state context the JVM blesser
//! pinned (TxValidate.scala), and run ergo-lib's stateful validation —
//! `TransactionContext::validate`, the sigma-rust port of the node's `validateStateful`
//! (per-input script verify + ERG/token preservation + dust + height checks). The tx
//! analog of [`crate::eval::run_entry`] / [`crate::wire::run_entry`].
//!
//! Outcome mapping (santa-transaction.actuals): a *verdict* is `valid: true` (with the
//! accumulated cost when the impl yields one) or `valid: false` + the impl's reason —
//! every `TxValidationError` is a clean reject, mirroring the JVM oracle's
//! `validateStateful → Failure ⇒ invalid` mapping (TxValidate.scala). A decode/setup
//! failure (sigma-rust can't even read node JSON the JVM accepted) is `errored` — no
//! verdict was reached; that too is a real divergence on an accept vector.
//!
//! Cost is impl-conditional like eval's: with `jit-cost` (the eni branch) `validate`
//! returns the accumulated block-cost total (tx init + per-input reduction + crypto
//! verification — `Result<u64, _>`); upstream develop's returns `Result<(), _>` — no
//! tx-cost model ⇒ `cost: null` (value-only grading; runner.json declares `cost`
//! per branch). The no-feature arm matches `Ok(_)` so this one file compiles against
//! either signature.

use ergo_chain_types::{ADDigest, AutolykosSolution, BlockId, Digest32, Header, PreHeader, Votes};
use ergo_lib::chain::ergo_state_context::{ErgoStateContext, Headers};
use ergo_lib::chain::parameters::{Parameter, Parameters};
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::wallet::tx_context::TransactionContext;
use ergotree_ir::chain::ergo_box::ErgoBox;

/// One transaction entry's outcome — the tx analog of [`crate::eval::Outcome`]:
/// value+cost become verdict (`valid`) + cost.
pub enum TxOutcome {
    /// Validation reached a verdict. `cost` is `Some` only on accept under `jit-cost`
    /// (eni); `reason` is `Some` only on reject (the impl's error string, diagnostic).
    Verdict {
        valid: bool,
        cost: Option<u64>,
        reason: Option<String>,
    },
    /// No verdict: the runner couldn't decode the entry or assemble the tx context.
    Errored { reason: String },
    /// A would-be panic, caught by main's never-panic net (contract §3).
    Panicked { note: String },
}

impl TxOutcome {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            TxOutcome::Verdict { valid, cost, reason } => {
                let mut j = serde_json::json!({
                    "valid": valid,
                    "cost": cost,
                    "error": serde_json::Value::Null,
                });
                if let Some(r) = reason {
                    j["reason"] = serde_json::json!(r);
                }
                j
            }
            TxOutcome::Errored { reason } => serde_json::json!({
                "valid": null, "cost": null, "error": "errored", "reason": reason
            }),
            TxOutcome::Panicked { note } => serde_json::json!({
                "valid": null, "cost": null, "error": "panicked", "note": note
            }),
        }
    }
}

/// The minimal `ErgoStateContext`, mirroring the JVM blesser's (TxValidate.scala):
/// a synthetic pre-header at `height` with `version = activated + 1` (block version),
/// zero parent/timestamp/nBits/votes, the group generator as miner key; launch-default
/// parameters with `BlockVersion` pinned to match (the JVM updated the same key).
///
/// The JVM oracle blessed with EMPTY last-headers; sigma-rust's `Headers` is a fixed
/// `[Header; 10]`, so we pin 10 synthetic headers built from the same zeroed fields.
/// They are unread by the captured corpus — a script touching `CONTEXT.headers` could
/// not have blessed valid against the oracle's empty seq in the first place.
fn state_context(height: u32, activated: u8) -> ErgoStateContext {
    let block_version = activated + 1;
    let miner_pk = Box::new(ergo_chain_types::ec_point::generator());
    let pre_header = PreHeader {
        version: block_version,
        parent_id: BlockId(Digest32::zero()),
        timestamp: 0,
        n_bits: 0,
        height,
        miner_pk: miner_pk.clone(),
        votes: Votes([0u8; 3]),
    };
    let header = Header {
        version: block_version,
        id: BlockId(Digest32::zero()),
        parent_id: BlockId(Digest32::zero()),
        ad_proofs_root: Digest32::zero(),
        state_root: ADDigest::zero(),
        transaction_root: Digest32::zero(),
        timestamp: 0,
        n_bits: 0,
        height,
        extension_root: Digest32::zero(),
        autolykos_solution: AutolykosSolution {
            miner_pk,
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes([0u8; 3]),
        unparsed_bytes: Box::new([]),
    };
    let headers: Headers = std::array::from_fn(|_| header.clone());
    let mut parameters = Parameters::default();
    parameters
        .parameters_table
        .insert(Parameter::BlockVersion, block_version as i32);
    ErgoStateContext::new(pre_header, headers, parameters)
}

fn decode_boxes(v: &serde_json::Value) -> Result<Vec<ErgoBox>, serde_json::Error> {
    serde_json::from_value(v.clone())
}

/// Validate one transaction entry. Produces exactly one [`TxOutcome`] (totality,
/// contract §3). `entry` is the whole vector entry: `tx` + `inputBoxes` +
/// `dataInputBoxes` decode via sigma-rust's node-JSON serde (the `Transaction` decode
/// recomputes and checks the tx id); `context.height` + `version.activated` pin the
/// state context.
pub fn run_entry(entry: &serde_json::Value) -> TxOutcome {
    let tx: Transaction = match serde_json::from_value(entry["tx"].clone()) {
        Ok(t) => t,
        Err(e) => return TxOutcome::Errored { reason: format!("tx decode: {e}") },
    };
    let input_boxes = match decode_boxes(&entry["inputBoxes"]) {
        Ok(b) => b,
        Err(e) => return TxOutcome::Errored { reason: format!("inputBoxes decode: {e}") },
    };
    let data_input_boxes = match decode_boxes(&entry["dataInputBoxes"]) {
        Ok(b) => b,
        Err(e) => return TxOutcome::Errored { reason: format!("dataInputBoxes decode: {e}") },
    };
    // Strict, not lenient-defaulted like eval's version read: a missing height or
    // activated version silently flips tx verdicts, so absence is a loud no-verdict.
    let height = match entry["context"]["height"].as_u64() {
        Some(h) => h as u32,
        None => return TxOutcome::Errored { reason: "entry missing context.height".into() },
    };
    let activated = match entry["version"]["activated"].as_u64() {
        Some(a) => a as u8,
        None => return TxOutcome::Errored { reason: "entry missing version.activated".into() },
    };

    // Assemble boxes_to_spend/data_boxes keyed by the tx's input ids. A failure here
    // (box missing, bounds) means the runner can't even pose the validation question.
    let tx_context = match TransactionContext::new(tx, input_boxes, data_input_boxes) {
        Ok(c) => c,
        Err(e) => return TxOutcome::Errored { reason: format!("tx context: {e}") },
    };

    let state_context = state_context(height, activated);
    match tx_context.validate(&state_context) {
        Ok(_total) => {
            // eni's validate returns the accumulated block-cost total; upstream
            // develop's returns () — `Ok(_total)` binds either, the feature picks.
            #[cfg(feature = "jit-cost")]
            let cost = Some(_total);
            #[cfg(not(feature = "jit-cost"))]
            let cost = None;
            TxOutcome::Verdict { valid: true, cost, reason: None }
        }
        Err(e) => TxOutcome::Verdict {
            valid: false,
            cost: None,
            reason: Some(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::run_entry;
    use serde_json::Value as J;

    /// The keystone captured entry, inlined verbatim from
    /// `vectors/transaction/v6/captured/bigint-downcast-2666.json` (entries[0]) so the
    /// test is hermetic in the standalone runner repo (no SANTA vector tree needed).
    /// A real testnet tx whose input script needs the v3-gated BigInt→Long Downcast —
    /// JVM-blessed {valid: true, cost: 14846}.
    const KEYSTONE_BIGINT_DOWNCAST_2666: &str = r#"{
  "name": "bigint-downcast-2666",
  "source": "testnet:testnet-bigint-downcast-v3@2666",
  "tx": {
    "dataInputs": [],
    "id": "fcb58864185ff84be56fb405b3919939eeb706e2577c3f48fb730628e23c87da",
    "inputs": [
      {
        "boxId": "90a2f39533729813e4635ec9a735ec803b4645bc28f63363b13fac9332f32fe9",
        "spendingProof": {
          "extension": {
            "0": "1a012056002fb6194f8a7f147b54c98c98bd72205f46c35fed0f3c4111071ecb0763b7",
            "1": "0e720356c9f063a7fdddd566da55d7d8dee934b6a0b291014d1285c8f93ef935ed85500256002fb6194f8a7f147b54c98c98bd72205f46c35fed0f3c4111071ecb0763b7ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00000009000000007735940000000400",
            "2": "0e7602000000000000000000000000000000000000000000000000000000000000000056002fb6194f8a7f147b54c98c98bd72205f46c35fed0f3c4111071ecb0763b70000000002ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00000009000000007735940000000400"
          },
          "proofBytes": ""
        }
      },
      {
        "boxId": "b98a06c14edc67f3fb11e7a8903fcdf83bdcb37e52f8173c872bd2327bfb895a",
        "spendingProof": {
          "extension": {},
          "proofBytes": "ceb615c07ec2e6927f88953ace2cbd7ae90f1d17235674833c2a431dc43b17d4aa010e15404604de4f0793e7fa73d716b3b084e2d0f70eb4"
        }
      }
    ],
    "outputs": [
      {
        "additionalRegisters": {},
        "assets": [],
        "boxId": "9e14e1c81f99daff20f153bc3c9a70a926b3dcc822d29c69b5f2cb8268c1fabb",
        "creationHeight": 2664,
        "ergoTree": "0008cd02856c5e4eb4b915005dccb8e0f42bed1777cae48565be13a77cbf31e3c93f4515",
        "index": 0,
        "transactionId": "fcb58864185ff84be56fb405b3919939eeb706e2577c3f48fb730628e23c87da",
        "value": 67500000000
      },
      {
        "additionalRegisters": {},
        "assets": [],
        "boxId": "eef4af49b5e90145da835aeb5f436c8debce71a3e96d2244f1de96c30aa23150",
        "creationHeight": 2664,
        "ergoTree": "1005040004000e36100204a00b08cd0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798ea02d192a39a8cc7a701730073011001020402d19683030193a38cc7b2a57300000193c2b2a57301007473027303830108cdeeac93b1a57304",
        "index": 1,
        "transactionId": "fcb58864185ff84be56fb405b3919939eeb706e2577c3f48fb730628e23c87da",
        "value": 1000000
      },
      {
        "additionalRegisters": {},
        "assets": [],
        "boxId": "d6c874bdf207a649fc19512db3b76bca71469ae98e35ced6604336290e06ccc8",
        "creationHeight": 2664,
        "ergoTree": "0008cd02856c5e4eb4b915005dccb8e0f42bed1777cae48565be13a77cbf31e3c93f4515",
        "index": 2,
        "transactionId": "fcb58864185ff84be56fb405b3919939eeb706e2577c3f48fb730628e23c87da",
        "value": 19999998000000
      }
    ]
  },
  "inputBoxes": [
    {
      "boxId": "90a2f39533729813e4635ec9a735ec803b4645bc28f63363b13fac9332f32fe9",
      "value": 67500000000,
      "ergoTree": "1b8d030b02000400041005000580897a040004020402040004000400d809d6018301027300d602e5e3001a83010e7201d603e4c6a70464d604dc0c1d720201addc640b7203027202e5e3010e7201d9010432e47204d605e4c6a70705d606e4c6a70606d607ad7204d901073c0e0e86028c7207017d9d9c7e7205067e7cb48c7207027301730206720605d608b072077303d90108414d0e9a8c7208018c8c72080202d609c1a795ed8f720872099199720972087304d801d60ab2a5730500d19683080193c2720ac2a793c1720a9972097208937206e4c6720a0606937205e4c6720a070593e4c6a70504e4c6720a050493db6401e4dc640e7203027202e5e3020e7201db6401e4c6720a0464afdc0c1db4a573069ab172047307017207d9010b3c634d0ed802d60d8c720b01d60e8c720b02ed93cbc2720d8c720e0193c1720d8c720e0293c5a7c5b2a4730800d19683020193c5a7c5b2a4730900afdc0c1db4a5730ab17204017207d9010a3c634d0ed802d60c8c720a01d60d8c720a02ed93cbc2720c8c720d0192c1720c8c720d02",
      "assets": [],
      "additionalRegisters": {
        "R5": "0402",
        "R7": "05808c82f5f603",
        "R4": "6478dde4eb8cef46cf37b528266da8c31a32105b9a93fc35276f266ae6c1cb5e9501072000",
        "R6": "060477359400"
      },
      "creationHeight": 2569,
      "transactionId": "70c3473a32853507053bf22063dd8129e951e1ba5b91f543fa626da774bab72e",
      "index": 0
    },
    {
      "additionalRegisters": {},
      "assets": [],
      "boxId": "b98a06c14edc67f3fb11e7a8903fcdf83bdcb37e52f8173c872bd2327bfb895a",
      "creationHeight": 2569,
      "ergoTree": "0008cd02856c5e4eb4b915005dccb8e0f42bed1777cae48565be13a77cbf31e3c93f4515",
      "index": 2,
      "transactionId": "70c3473a32853507053bf22063dd8129e951e1ba5b91f543fa626da774bab72e",
      "value": 19999999000000
    }
  ],
  "dataInputBoxes": [],
  "context": {
    "height": 2666
  },
  "version": {
    "activated": 3,
    "ergoTree": 3
  },
  "expected": {
    "valid": true,
    "cost": 14846,
    "reason": null
  }
}"#;

    /// Shape test only: a verdict must be reached (`error: null`, `valid` a bool).
    /// Whether `valid` is true (eni: carries the v6 fixes, matching the JVM bless) or
    /// false (upstream develop: pre-fix `reduce_to_crypto` rejects the v3-gated
    /// Downcast) is per-branch — graded by conform against `expected`, not asserted
    /// here. The verdict is logged for eyeballing.
    #[test]
    fn keystone_reaches_a_verdict() {
        let entry: J = serde_json::from_str(KEYSTONE_BIGINT_DOWNCAST_2666).expect("fixture JSON");
        let j = run_entry(&entry).to_json();
        println!("keystone bigint-downcast-2666 verdict: {j}");
        assert_eq!(j["error"], J::Null, "expected a verdict, got: {j}");
        assert!(j["valid"].is_boolean(), "valid must be a bool, got: {j}");
        if j["valid"] == J::Bool(false) {
            assert!(j["reason"].is_string(), "a reject must carry a reason: {j}");
        }
    }

    /// A malformed entry (tx not decodable) is `errored` — the runner failed to reach
    /// a verdict; never a panic, never a silent skip (contract totality).
    #[test]
    fn undecodable_tx_is_errored() {
        let entry: J = serde_json::json!({
            "name": "broken",
            "tx": {},
            "inputBoxes": [],
            "dataInputBoxes": [],
            "context": {"height": 1},
            "version": {"activated": 3, "ergoTree": 3}
        });
        let j = run_entry(&entry).to_json();
        assert_eq!(j["error"], "errored");
        assert_eq!(j["valid"], J::Null);
        assert_eq!(j["cost"], J::Null);
        assert!(j["reason"].is_string(), "errored must carry a reason: {j}");
    }
}
