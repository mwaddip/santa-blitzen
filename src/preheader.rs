//! Decoder for the walker full-context envelope's `pre_header_hex` sub-encoding —
//! the ergots ↔ SANTA shared wire contract (see prompts/walker-jvm-oracle-santa.md
//! and the runner contract §2 `santa-eval/v6-fullctx`).
//!
//! `PreHeader` is a derived view of the spending block's header with **no consensus
//! standalone serializer**, so ergots defined this sub-encoding by hand and every
//! conformer must mirror it byte-for-byte. It mirrors jvm-blesser's `PreHeaderCodec`.
//! Field order + widths:
//!
//!   version    1 raw byte
//!   parentId   32 raw bytes
//!   timestamp  VLQ u64 (unsigned LEB128 — timestamps exceed 2^53)
//!   nBits      VLQ u32 (unsigned LEB128)
//!   height     VLQ u32 (unsigned LEB128)
//!   minerPk    33 raw bytes (SEC1 compressed)
//!   votes      3 raw bytes
//!
//! The VLQ is plain unsigned LEB128 (NOT sigma-ser's ZigZag `put_u64`). Decode-only:
//! the runner consumes envelopes, it never emits them. Truncation is an `Err` (the
//! caller surfaces a clean `errored`), never a panic.

/// Raw field view of a PreHeader — the wire shape, independent of sigma-rust types.
/// The eval context builder (`eval::build_context_v6_fullctx`) maps this into a
/// sigma-rust `PreHeader`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreHeaderFields {
    pub version: u8,
    pub parent_id: [u8; 32],
    pub timestamp: u64,
    pub n_bits: u32,
    pub height: u32,
    pub miner_pk: [u8; 33],
    pub votes: [u8; 3],
}

/// Read one unsigned-LEB128 (VLQ) value starting at `offset`; returns (value, next offset).
fn read_vlq_u(bytes: &[u8], offset: usize) -> Result<(u64, usize), String> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    let mut pos = offset;
    loop {
        if pos >= bytes.len() {
            return Err(format!("VLQ truncated at offset {pos}"));
        }
        if shift >= 64 {
            return Err("VLQ overflows 64 bits".to_string());
        }
        let b = bytes[pos];
        pos += 1;
        result |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok((result, pos));
        }
        shift += 7;
    }
}

/// Decode `pre_header_hex` bytes into raw PreHeader fields.
pub fn decode(bytes: &[u8]) -> Result<PreHeaderFields, String> {
    // version(1) + parentId(32)
    if bytes.len() < 33 {
        return Err(format!("pre_header truncated before parentId: {} bytes", bytes.len()));
    }
    let version = bytes[0];
    let mut parent_id = [0u8; 32];
    parent_id.copy_from_slice(&bytes[1..33]);
    // timestamp / nBits / height — unsigned LEB128
    let (timestamp, pos) = read_vlq_u(bytes, 33)?;
    let (n_bits, pos) = read_vlq_u(bytes, pos)?;
    let (height, mut pos) = read_vlq_u(bytes, pos)?;
    // minerPk(33) + votes(3)
    if bytes.len() < pos + 36 {
        return Err(format!(
            "pre_header truncated before minerPk+votes: have {}, need {}",
            bytes.len(),
            pos + 36
        ));
    }
    let mut miner_pk = [0u8; 33];
    miner_pk.copy_from_slice(&bytes[pos..pos + 33]);
    pos += 33;
    let mut votes = [0u8; 3];
    votes.copy_from_slice(&bytes[pos..pos + 3]);
    Ok(PreHeaderFields {
        version,
        parent_id,
        timestamp,
        n_bits: u32::try_from(n_bits).map_err(|_| format!("nBits out of u32 range: {n_bits}"))?,
        height: u32::try_from(height).map_err(|_| format!("height out of u32 range: {height}"))?,
        miner_pk,
        votes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Byte-golden against the captured h111927 envelope's `pre_header_hex`. Pins the
    /// full layout: raw `version`, the 32-byte `parentId`, the three VLQ fields, the
    /// 33-byte SEC1 `minerPk`, and the 3-byte `votes`. `height` cross-checks the
    /// envelope's own `"height": 111927`, and `minerPk`/`votes` landing at the right
    /// offset proves `timestamp`+`nBits` consumed exactly the right number of VLQ bytes.
    #[test]
    fn decodes_h111927_preheader() {
        let bytes = unhex(
            "048b09b76eb6d538ccf84b3de97499d5a9ad5f9136bb450ac11e6771ed170085c3eae3cdf0bb33cacb8f28b7ea0602339b416a576ed390679cd7372902f04a442177ef067ee223036d5864b98a26cb000000",
        );
        let f = decode(&bytes).expect("decode h111927 pre_header");
        assert_eq!(f.version, 4, "block version");
        assert_eq!(f.height, 111927, "preHeader height == captured block height");
        assert_eq!(f.votes, [0u8, 0, 0], "votes");
        assert_eq!(
            hex(&f.parent_id),
            "8b09b76eb6d538ccf84b3de97499d5a9ad5f9136bb450ac11e6771ed170085c3",
            "parentId (32B)",
        );
        assert_eq!(
            hex(&f.miner_pk),
            "02339b416a576ed390679cd7372902f04a442177ef067ee223036d5864b98a26cb",
            "minerPk (33B SEC1)",
        );
        // No independent golden pins the exact timestamp/nBits; the eval integration
        // test validates the full reconstruction end-to-end. Here: smoke that the VLQ
        // fields decoded to populated values.
        assert!(f.timestamp > 0, "timestamp populated");
        assert!(f.n_bits > 0, "nBits populated");
    }
}
