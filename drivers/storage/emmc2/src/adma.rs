//! 32-bit ADMA2 descriptor encoding.
//!
//! The SDHCI ADMA2 engine walks a table of 64-bit descriptors in host
//! memory (SD Host Controller Simplified Specification v3.00 §1.13). Each
//! 32-bit-address descriptor is laid out little-endian as:
//!
//! ```text
//! bits [63:32]  Address  — 32-bit data-buffer base
//! bits [31:16]  Length   — byte length (the value 0 means 65536)
//! bits  [5:4]   Act      — 0b10 = Tran (move Length bytes to/from Address)
//! bit    2      Int      — raise the DMA interrupt at this descriptor
//! bit    1      End      — last descriptor of the table
//! bit    0      Valid    — the descriptor is valid
//! ```
//!
//! The driver stages one physically-contiguous bounce buffer per transfer
//! chunk, so a single `Tran` descriptor with the `End` bit describes the
//! whole chunk. A larger transfer is chunked by the engine into successive
//! commands, so no scatter list is needed here.

/// Serialised size of one 32-bit ADMA2 descriptor, in bytes.
pub const DESC_BYTES: usize = 8;

/// Largest byte length one 32-bit ADMA2 descriptor can carry: the 16-bit
/// `Length` field, where the encoded value `0` denotes the maximum. A
/// format-fixed bound (the descriptor layout), not a scalable capacity.
pub const MAX_DESC_BYTES: usize = 1 << 16;

/// `Valid`: the descriptor is valid (attribute bit 0).
const ATTR_VALID: u16 = 1 << 0;
/// `End`: the last descriptor of the table (attribute bit 1).
const ATTR_END: u16 = 1 << 1;
/// `Act = Tran` (attribute bits `[5:4]` = `0b10`): move `Length` bytes
/// between the card and the descriptor's `Address`.
const ATTR_ACT_TRAN: u16 = 0b10 << 4;

/// Encode the single terminating `Tran` descriptor covering `len` bytes at
/// device address `addr` into `out`.
///
/// Returns the descriptor bytes (`Valid | End | Tran`). `len` must be in
/// `1..=`[`MAX_DESC_BYTES`]; the caller (the engine's chunk loop) upholds
/// this, and it is asserted in debug builds. The `End` bit terminates the
/// one-entry table, so the controller stops after this descriptor.
///
/// The `Length` field encodes `MAX_DESC_BYTES` (65536) as `0`, exactly as
/// the controller decodes it.
#[must_use]
pub fn encode_tran(addr: u32, len: usize) -> [u8; DESC_BYTES] {
    debug_assert!(
        (1..=MAX_DESC_BYTES).contains(&len),
        "chunk within one descriptor"
    );
    // 65536 wraps to the field's `0` (the spec's maximum-length encoding);
    // any shorter length is its own literal. The mask leaves a value in
    // `0..=0xFFFF`, so the `u16` conversion never truncates; the `0`
    // fallback is unreachable and only keeps the code panic-free.
    let length_field = u16::try_from(len & (MAX_DESC_BYTES - 1)).unwrap_or(0);
    let attr = ATTR_VALID | ATTR_END | ATTR_ACT_TRAN;
    let mut out = [0u8; DESC_BYTES];
    out[0..2].copy_from_slice(&attr.to_le_bytes());
    out[2..4].copy_from_slice(&length_field.to_le_bytes());
    out[4..8].copy_from_slice(&addr.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tran_descriptor_carries_valid_end_and_act() {
        let desc = encode_tran(0x1234_5678, 512);
        let attr = u16::from_le_bytes([desc[0], desc[1]]);
        assert_ne!(attr & ATTR_VALID, 0, "Valid set");
        assert_ne!(attr & ATTR_END, 0, "End set (one-entry table)");
        assert_eq!(attr & (0b11 << 4), ATTR_ACT_TRAN, "Act = Tran");
        assert_eq!(
            u16::from_le_bytes([desc[2], desc[3]]),
            512,
            "length literal"
        );
        assert_eq!(
            u32::from_le_bytes([desc[4], desc[5], desc[6], desc[7]]),
            0x1234_5678,
            "data address"
        );
    }

    #[test]
    fn max_length_encodes_as_zero() {
        // 65536 bytes is encoded as a zero Length field (the spec's
        // maximum-length convention), not as a truncated 0x10000.
        let desc = encode_tran(0, MAX_DESC_BYTES);
        assert_eq!(u16::from_le_bytes([desc[2], desc[3]]), 0);
    }
}
