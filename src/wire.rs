//! Wire-tier round-trip: parse a `kind`'s canonical bytes with sigma-rust's serializer and
//! reserialize for byte-identity comparison downstream. The wire analog of [`crate::eval::run_entry`]
//! — a single round-trip outcome, no value/cost (docs/specs/wire-tier.md). A parse/serialize failure
//! is `errored` (sigma-rust rejected bytes the JVM blessed — a real divergence); a `kind` with no
//! sigma serializer wired here is `not-implemented`.

use ergo_lib::chain::transaction::Transaction;
use ergotree_ir::chain::ergo_box::ErgoBox;
use ergotree_ir::mir::constant::Constant;
use ergotree_ir::serialization::SigmaSerializable;
use ergotree_ir::sigma_protocol::sigma_boolean::SigmaBoolean;

/// One wire entry's outcome — the round-trip analog of [`crate::eval::Outcome`]: `bytes_hex`
/// replaces value+cost (the wire tier has no cost dimension).
pub enum WireOutcome {
    RoundTrip { bytes_hex: String },
    Errored,
    NotImplemented,
    Panicked { note: String },
}

impl WireOutcome {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            WireOutcome::RoundTrip { bytes_hex } => {
                serde_json::json!({"bytes_hex": bytes_hex, "error": serde_json::Value::Null})
            }
            WireOutcome::Errored => serde_json::json!({"bytes_hex": null, "error": "errored"}),
            WireOutcome::NotImplemented => {
                serde_json::json!({"bytes_hex": null, "error": "not-implemented"})
            }
            WireOutcome::Panicked { note } => {
                serde_json::json!({"bytes_hex": null, "error": "panicked", "note": note})
            }
        }
    }
}

/// Round-trip `bytes` through `T`'s sigma serializer: parse then reserialize. A parse or serialize
/// failure is `errored` — sigma-rust couldn't reproduce bytes the JVM blessed (a real divergence).
fn roundtrip<T: SigmaSerializable>(bytes: &[u8]) -> WireOutcome {
    match T::sigma_parse_bytes(bytes) {
        Ok(obj) => match obj.sigma_serialize_bytes() {
            Ok(out) => WireOutcome::RoundTrip { bytes_hex: bytes_to_hex(&out) },
            Err(_) => WireOutcome::Errored,
        },
        Err(_) => WireOutcome::Errored,
    }
}

/// Round-trip one wire entry. `kind` selects the serializer. Header isn't wired here — it parses
/// via ScorexSerializable, not SigmaSerializable → not-implemented.
pub fn run_entry(kind: &str, bytes_hex: &str) -> WireOutcome {
    let bytes = match crate::hex_to_bytes(bytes_hex) {
        Ok(b) => b,
        Err(e) => return WireOutcome::Panicked { note: format!("bad bytes_hex: {e}") },
    };
    match kind {
        "Box" => roundtrip::<ErgoBox>(&bytes),
        "Constant" => roundtrip::<Constant>(&bytes),
        "SigmaBoolean" => roundtrip::<SigmaBoolean>(&bytes),
        "Transaction" => roundtrip::<Transaction>(&bytes),
        _ => WireOutcome::NotImplemented,
    }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::run_entry;
    use serde_json::Value as J;

    #[test]
    fn box_round_trips_to_its_own_bytes() {
        // sbox_minimal from vectors/wire/v5/authored/Box.json
        let hex = "c0843d09020101000000000000000000000000000000000000000000000000000000000000000000000000";
        let j = run_entry("Box", hex).to_json();
        assert_eq!(j["error"], J::Null);
        assert_eq!(j["bytes_hex"], hex);
    }

    #[test]
    fn sigma_boolean_round_trips_to_its_own_bytes() {
        let hex = "d3"; // TrivialProp(true) from vectors/wire/v5/authored/SigmaBoolean.json
        let j = run_entry("SigmaBoolean", hex).to_json();
        assert_eq!(j["error"], J::Null);
        assert_eq!(j["bytes_hex"], hex);
    }

    #[test]
    fn constant_round_trips_to_its_own_bytes() {
        // bool_0 from vectors/wire/v5/vendored/Constant.json (Fleet)
        let hex = "0101";
        let j = run_entry("Constant", hex).to_json();
        assert_eq!(j["error"], J::Null);
        assert_eq!(j["bytes_hex"], hex);
    }

    #[test]
    fn transaction_round_trips_to_its_own_bytes() {
        // tx_466f1aef from vectors/wire/v5/vendored/Transaction.json (Fleet signed tx)
        let hex = "010c7a0f145994fa15b02eca8189b454eeff9eec5ca33fa135b4474d4ee8ed35c70000000220fa2bf23962cdf51b07722d6237c0c7b8a44f78856c0f7ec308dc1ef1a92a51d9a2cc8a09abfaed87afacfbb7daee79a6b26f10c6613fc13d3f3953e5521d1a0280cc9497e9d1870f101004020e36100204a00b08cd0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798ea02d192a39a8cc7a7017300730110010204020404040004c0fd4f05808c82f5f6030580b8c9e5ae040580f882ad16040204c0944004c0f407040004000580f882ad16d19683030191a38cc7a7019683020193c2b2a57300007473017302830108cdeeac93a38cc7b2a573030001978302019683040193b1a5730493c2a7c2b2a573050093958fa3730673079973089c73097e9a730a9d99a3730b730c0599c1a7c1b2a5730d00938cc7b2a5730e0001a390c1a7730fb3825c0200010180b0abe9c1c7fa1300809ccdca64100204a00b08cd0274e729bb6615cbda94d9d176a2f1525068f12b330e38bbbf387232797dfd891fea02d192a39a8cc7a70173007301b3825c010180f085da2c00";
        let j = run_entry("Transaction", hex).to_json();
        assert_eq!(j["error"], J::Null);
        assert_eq!(j["bytes_hex"], hex);
    }

    #[test]
    fn unwired_kind_is_not_implemented() {
        // Header still parses via ScorexSerializable, not SigmaSerializable -> not wired here.
        let j = run_entry("Header", "00").to_json();
        assert_eq!(j["error"], "not-implemented");
        assert_eq!(j["bytes_hex"], J::Null);
    }
}
