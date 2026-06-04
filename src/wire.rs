//! Wire-tier round-trip: parse a `kind`'s canonical bytes with sigma-rust's serializer and
//! reserialize for byte-identity comparison downstream. The wire analog of [`crate::eval::run_entry`]
//! — a single round-trip outcome, no value/cost (docs/specs/wire-tier.md). A parse/serialize failure
//! is `errored` (sigma-rust rejected bytes the JVM blessed — a real divergence); a `kind` with no
//! sigma serializer wired here is `not-implemented`.

use ergotree_ir::chain::ergo_box::ErgoBox;
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

/// Round-trip one wire entry. `kind` selects the serializer. Transaction/Header/Constant aren't
/// wired yet (Header parses via ScorexSerializable, not SigmaSerializable) → not-implemented.
pub fn run_entry(kind: &str, bytes_hex: &str) -> WireOutcome {
    let bytes = match crate::hex_to_bytes(bytes_hex) {
        Ok(b) => b,
        Err(e) => return WireOutcome::Panicked { note: format!("bad bytes_hex: {e}") },
    };
    match kind {
        "Box" => roundtrip::<ErgoBox>(&bytes),
        "SigmaBoolean" => roundtrip::<SigmaBoolean>(&bytes),
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
    fn unwired_kind_is_not_implemented() {
        let j = run_entry("Transaction", "00").to_json();
        assert_eq!(j["error"], "not-implemented");
        assert_eq!(j["bytes_hex"], J::Null);
    }
}
