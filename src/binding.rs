//! Context bindings (runner contract §4). A binding is an SValue — decoded by
//! [`sval::decode_constant`] — or `{"kind":"Value","bytes_hex":…}`: the bytes a node itself reads
//! for that position, read here by sigma-rust's OWN reader for it and never rebuilt from an SValue.
//! The reader refusing them (a parse error, rule 1019's `check_v6_type`, bytes left over) is
//! [`BridgeError::Refused`], which every caller maps to `errored`: a node would reject the
//! transaction.

use ergotree_ir::chain::context_extension::ContextExtension;
use ergotree_ir::chain::ergo_box::{NonMandatoryRegisterId, NonMandatoryRegisters, RegisterValue};
use ergotree_ir::chain::evaluated_value::EvaluatedValue;
use ergotree_ir::ergo_tree::ErgoTreeVersion;
use ergotree_ir::serialization::sigma_byte_reader::{self, SigmaByteRead};
use ergotree_ir::serialization::{SigmaParsingError, SigmaSerializable};
use serde_json::Value as J;

use crate::sval::{self, BridgeError};

/// A `Value` binding's bytes, or `None` for an SValue.
fn value_bytes(j: &J) -> Option<Result<Vec<u8>, BridgeError>> {
    if j.get("kind").and_then(|k| k.as_str()) != Some("Value") {
        return None;
    }
    Some(match j.get("bytes_hex").and_then(|h| h.as_str()) {
        Some(h) => crate::hex_to_bytes(h)
            .map_err(|_| BridgeError::Decode(format!("Value bytes_hex: bad hex {h:?}"))),
        None => Err(BridgeError::Decode("Value binding without bytes_hex".into())),
    })
}

/// Read a frame of `len` bytes whole with `parse`, under the entry's `(activated, activated)` —
/// the versions a node parses a block's transactions under from block version 4 (ergo
/// `BlockTransactions.scala:195`, `Header.scala:154` at v6.0.7). A reader error, or bytes left
/// after the value, is the reader refusing the binding.
fn read_whole<R: SigmaByteRead, T>(
    mut r: R,
    len: usize,
    activated: u8,
    parse: impl FnOnce(&mut R) -> Result<T, SigmaParsingError>,
) -> Result<T, BridgeError> {
    let v = ErgoTreeVersion::from(activated);
    let out = r
        .with_versions(v, v, parse)
        .map_err(|e| BridgeError::Refused(format!("the reader refused the Value: {e}")))?;
    let pos = r
        .position()
        .map_err(|e| BridgeError::Decode(format!("reader position: {e:?}")))? as usize;
    if pos != len {
        return Err(BridgeError::Refused(format!(
            "{} byte(s) left after the value",
            len - pos
        )));
    }
    Ok(out)
}

/// A binding at an extension position: var 1 (v2/v4 `input`), a v3 / v5 / full-context extension
/// value. A `Value` is read by sigma-rust's `ContextExtension` reader, framed as the one-variable
/// extension `01 01 <bytes>` — the count, then a dummy id: the binding's key is the vector's and
/// the caller applies it, so a v5 key >= 0x80 still reaches the context the way the key-domain
/// vectors need. The result is what the reader holds: an `EvaluatedValue` (a constant, or an
/// evaluated expression such as a `Tuple` of constants).
pub fn extension_binding(j: &J, activated: u8) -> Result<EvaluatedValue, BridgeError> {
    let Some(bytes) = value_bytes(j) else {
        return sval::decode_constant(j).map(EvaluatedValue::from);
    };
    let frame = [&[0x01u8, 0x01][..], &bytes?].concat();
    let ext = read_whole(sigma_byte_reader::from_bytes(&frame[..]), frame.len(), activated, |r| {
        ContextExtension::sigma_parse(r)
    })?;
    ext.values
        .get(&1u8)
        .cloned()
        .ok_or_else(|| BridgeError::Refused("the extension reader returned no variable".into()))
}

/// A binding at a register position (`selfRegisters`). A `Value` is read by sigma-rust's
/// `NonMandatoryRegisters` reader, framed as the one-register map `01 <bytes>`; the result is the
/// reader's own `RegisterValue`.
pub fn register_binding(j: &J, activated: u8) -> Result<RegisterValue, BridgeError> {
    let Some(bytes) = value_bytes(j) else {
        return sval::decode_constant(j).map(RegisterValue::Parsed);
    };
    let frame = [&[0x01u8][..], &bytes?].concat();
    let regs = read_whole(sigma_byte_reader::from_bytes(&frame[..]), frame.len(), activated, |r| {
        NonMandatoryRegisters::sigma_parse(r)
    })?;
    regs.get(NonMandatoryRegisterId::R4)
        .cloned()
        .ok_or_else(|| BridgeError::Refused("the register reader returned no R4".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ergotree_ir::mir::constant::Constant;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn value_json(bytes_hex: &str) -> J {
        serde_json::json!({ "kind": "Value", "bytes_hex": bytes_hex })
    }
    fn const_hex(c: &Constant) -> String {
        hex(&c.sigma_serialize_bytes().unwrap())
    }

    #[test]
    fn an_extension_value_equals_the_json_form_of_the_same_value() {
        let json = serde_json::json!({ "kind": "Long", "value": "7" });
        let from_json = extension_binding(&json, 3).unwrap();
        let from_bytes = extension_binding(&value_json(&const_hex(&Constant::from(7i64))), 3).unwrap();
        assert_eq!(from_bytes, from_json);
    }

    #[test]
    fn a_register_value_equals_the_json_form_of_the_same_value() {
        let json = serde_json::json!({ "kind": "Long", "value": "7" });
        let from_json = register_binding(&json, 3).unwrap();
        let from_bytes = register_binding(&value_json(&const_hex(&Constant::from(7i64))), 3).unwrap();
        assert_eq!(from_bytes, from_json);
    }

    #[test]
    fn bytes_left_after_the_value_are_refused_at_both_positions() {
        let trailing = format!("{}00", const_hex(&Constant::from(7i64)));
        for got in [
            extension_binding(&value_json(&trailing), 3).map(|_| ()),
            register_binding(&value_json(&trailing), 3).map(|_| ()),
        ] {
            match got {
                Err(BridgeError::Refused(m)) => assert!(m.contains("left after the value"), "{m}"),
                other => panic!("expected the leftover-bytes refusal, got {other:?}"),
            }
        }
    }

    // Rule 1019: an Option-typed value (`Option[Int]` Some(1): type 0x24|0x04 = 0x28, then
    // 01 = Some, then Int 1 zig-zag = 02) is refused by both of a node's readers at every version.
    #[test]
    fn rule_1019_refuses_an_option_at_both_positions() {
        // The bytes ARE an Option[Int] constant — a plain constant read at v3 takes them — so the
        // refusals below are rule 1019's, not a parse error's.
        let c = sigma_byte_reader::from_bytes(&[0x28u8, 0x01, 0x02][..])
            .with_tree_version(ErgoTreeVersion::V3, |r| Constant::sigma_parse(r))
            .expect("an Option[Int] constant");
        assert_eq!(
            c.tpe,
            ergotree_ir::types::stype::SType::SOption(Box::new(ergotree_ir::types::stype::SType::SInt).into())
        );
        let some_int = value_json("280102");
        assert!(matches!(extension_binding(&some_int, 3), Err(BridgeError::Refused(_))));
        assert!(matches!(register_binding(&some_int, 3), Err(BridgeError::Refused(_))));
    }

    // A Tuple EXPRESSION (opcode 0x86, two items: Int 1, Int 2) — what the JVM's getValue hands
    // back for an extension value, and what the fork's reader holds too.
    #[test]
    fn an_extension_value_may_be_a_tuple_expression() {
        let tuple_expr = value_json("860204020404");
        let v = extension_binding(&tuple_expr, 3).unwrap();
        assert!(
            matches!(v, EvaluatedValue::Expr(ergotree_ir::chain::evaluated_value::EvaluatedExpr::Tuple(_))),
            "got {v:?}"
        );
    }
}
