//! RustOS instruction decoders (`lib/disasm`).
//!
//! The file manager's disassembly viewer — and next an `objdump`-class
//! command app — need to render machine code as text for the four Tier-1
//! ISAs. That decoding is identical wherever it happens, so it lives here
//! once, one module per ISA:
//!
//! * [`riscv64`] — RV64GC, including the C (compressed) extension; the
//!   16/32-bit length discipline is the core correctness property.
//! * [`aarch64`] — fixed 32-bit A64 decode of the major encoding groups;
//!   an unknown encoding renders as `.inst 0x…`, never skipped or guessed.
//! * [`wasm`] — the structured opcode stream of a code-section body, with
//!   block nesting rendered by indentation and strictly validated LEB128
//!   immediates (an overlong encoding fails closed).
//! * [`x86_64`] — the variable-length decoder: legacy prefixes, REX,
//!   ModRM/SIB, displacement/immediate sizing over the one- and two-byte
//!   opcode maps; an undecodable byte is a `(bad)` single byte, so the
//!   stream resynchronises exactly as binutils does.
//!
//! Every decoder is a **pure function of a byte slice and a start
//! address**: no state, no I/O, no allocation beyond the returned text. It
//! always makes forward progress (a returned instruction consumes at least
//! one length unit, so a walk over any input terminates), it never reads
//! past the slice, and it never executes or interprets anything — the
//! output is text in the one shared vocabulary, [`Insn`].
//!
//! The decoders parse untrusted executable-file bytes, so they are held to
//! the untrusted-input bar: fail closed on every malformed encoding with a
//! fixed validation bound where a count rides the input (never a growable
//! capacity), never panic, and each ISA has a fuzz harness under `tests/`
//! proving decode terminates panic-free on arbitrary bytes. Like
//! `lib/binfmt`, this crate links into the minimum-capability parser
//! sandbox, so it stays `no_std + alloc` with no dependencies.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod aarch64;
pub mod riscv64;
pub mod wasm;
pub mod x86_64;

use alloc::string::String;
use alloc::vec::Vec;

/// Most encoding bytes ever retained on an [`Insn`].
///
/// x86_64 caps a legal instruction at 15 bytes; riscv64 parcels reach 8,
/// A64 is always 4. Only a wasm instruction can legitimately outgrow this
/// (a `br_table` carries one LEB128 target per entry), and then
/// [`Insn::bytes`] holds the first `MAX_INSN_BYTES` while [`Insn::length`]
/// still reports the full span consumed.
pub const MAX_INSN_BYTES: usize = 15;

/// Mnemonic rendered for bytes no decoder table accounts for.
pub const BAD_MNEMONIC: &str = "(bad)";

/// One decoded instruction — the shared output vocabulary of every ISA
/// module in this crate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Insn {
    /// Address of the first encoding byte.
    pub address: u64,
    /// The encoding bytes, capped at [`MAX_INSN_BYTES`] (see [`Insn::length`]
    /// for the full span).
    pub bytes: Vec<u8>,
    /// Full number of bytes this instruction consumed. Always at least 1,
    /// so a decode walk makes forward progress on any input.
    pub length: usize,
    /// Mnemonic text (for wasm, prefixed with the nesting indentation).
    pub mnemonic: String,
    /// Operand text; empty when the instruction takes none.
    pub operands: String,
    /// Resolved absolute target of a direct branch/call, when the
    /// instruction is one and the target is encoded in the bytes.
    pub branch_target: Option<u64>,
}

impl Insn {
    /// Builds an instruction over `consumed` (the exact encoding bytes),
    /// retaining at most [`MAX_INSN_BYTES`] of them.
    pub(crate) fn new(
        address: u64,
        consumed: &[u8],
        mnemonic: String,
        operands: String,
        branch_target: Option<u64>,
    ) -> Self {
        let kept = consumed.len().min(MAX_INSN_BYTES);
        Self {
            address,
            bytes: consumed[..kept].to_vec(),
            length: consumed.len(),
            mnemonic,
            operands,
            branch_target,
        }
    }

    /// Builds the honest undecodable rendering over `consumed` bytes.
    pub(crate) fn bad(address: u64, consumed: &[u8]) -> Self {
        Self::new(
            address,
            consumed,
            String::from(BAD_MNEMONIC),
            String::new(),
            None,
        )
    }
}

/// Sign-extends the low `bits` bits of `value` (0 < `bits` ≤ 64).
pub(crate) fn sign_extend(value: u64, bits: u32) -> i64 {
    debug_assert!((1..=64).contains(&bits));
    if bits >= 64 {
        return i64::from_le_bytes(value.to_le_bytes());
    }
    // Shift the sign bit up to bit 63 in unsigned arithmetic, then let an
    // arithmetic right shift replicate it back down — no i64 overflow at
    // any width (a subtraction-based form overflows at bits = 63).
    let low = value & ((1u64 << bits) - 1);
    let up = 64 - bits;
    i64::from_le_bytes((low << up).to_le_bytes()) >> up
}

/// `base + offset` with two's-complement wrap, for branch targets.
pub(crate) fn branch_target(base: u64, offset: i64) -> u64 {
    base.wrapping_add_signed(offset)
}

#[cfg(test)]
mod tests {
    use super::sign_extend;

    #[test]
    fn sign_extend_positive_and_negative() {
        assert_eq!(sign_extend(0x7ff, 12), 2047);
        assert_eq!(sign_extend(0x800, 12), -2048);
        assert_eq!(sign_extend(0xfff, 12), -1);
        assert_eq!(sign_extend(0, 12), 0);
        assert_eq!(sign_extend(u64::MAX, 64), -1);
        assert_eq!(sign_extend(1, 1), -1);
        // Regression (fuzz find): bits = 63 overflowed the old
        // subtraction-based sign computation.
        assert_eq!(sign_extend((1u64 << 63) - 1, 63), -1);
        assert_eq!(sign_extend(1u64 << 62, 63), i64::MIN >> 1);
    }
}
