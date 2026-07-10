//! Conformance tests for the A64 decoder: known encodings checked
//! byte-for-byte, the `.inst` fallback for uncovered encodings, and the
//! fail-closed truncation path.

use super::decode;
use crate::BAD_MNEMONIC;

/// One conformance row: the little-endian word at `address` must decode to
/// exactly this mnemonic, operand text, and branch target.
struct Case {
    word: u32,
    address: u64,
    mnemonic: &'static str,
    operands: &'static str,
    target: Option<u64>,
}

/// Hand-assembled A64 encodings.
const CASES: &[Case] = &[
    // --- Branches, exception generation, system ---
    Case {
        word: 0xd503_201f,
        address: 0,
        mnemonic: "nop",
        operands: "",
        target: None,
    },
    Case {
        word: 0xd503_203f,
        address: 0,
        mnemonic: "yield",
        operands: "",
        target: None,
    },
    Case {
        word: 0xd65f_03c0,
        address: 0,
        mnemonic: "ret",
        operands: "x30",
        target: None,
    },
    Case {
        word: 0xd61f_0220,
        address: 0,
        mnemonic: "br",
        operands: "x17",
        target: None,
    },
    Case {
        word: 0xd400_0001,
        address: 0,
        mnemonic: "svc",
        operands: "#0x0",
        target: None,
    },
    Case {
        word: 0x1400_0001,
        address: 0x1000,
        mnemonic: "b",
        operands: "0x1004",
        target: Some(0x1004),
    },
    Case {
        word: 0x9400_0000,
        address: 0x1000,
        mnemonic: "bl",
        operands: "0x1000",
        target: Some(0x1000),
    },
    Case {
        word: 0x5400_0041,
        address: 0x1000,
        mnemonic: "b.ne",
        operands: "0x1008",
        target: Some(0x1008),
    },
    Case {
        word: 0xb400_0040,
        address: 0x1000,
        mnemonic: "cbz",
        operands: "x0,0x1008",
        target: Some(0x1008),
    },
    // --- Data processing (immediate) ---
    Case {
        word: 0x9100_03fd,
        address: 0,
        mnemonic: "add",
        operands: "x29,sp,#0x0",
        target: None,
    },
    Case {
        word: 0xd100_83ff,
        address: 0,
        mnemonic: "sub",
        operands: "sp,sp,#0x20",
        target: None,
    },
    Case {
        word: 0x5280_0020,
        address: 0,
        mnemonic: "movz",
        operands: "w0,#0x1",
        target: None,
    },
    Case {
        word: 0xf2a0_0021,
        address: 0,
        mnemonic: "movk",
        operands: "x1,#0x1,lsl #16",
        target: None,
    },
    Case {
        word: 0x9000_0000,
        address: 0x1234,
        mnemonic: "adrp",
        operands: "x0,0x1000",
        target: Some(0x1000),
    },
    Case {
        word: 0x1000_0041,
        address: 0x1000,
        mnemonic: "adr",
        operands: "x1,0x1008",
        target: Some(0x1008),
    },
    Case {
        word: 0x9240_0c00,
        address: 0,
        mnemonic: "and",
        operands: "x0,x0,#0xf",
        target: None,
    },
    Case {
        word: 0x3200_0000,
        address: 0,
        mnemonic: "orr",
        operands: "w0,w0,#0x1",
        target: None,
    },
    // --- Loads and stores ---
    Case {
        word: 0xf940_0420,
        address: 0,
        mnemonic: "ldr",
        operands: "x0,[x1,#8]",
        target: None,
    },
    Case {
        word: 0xb900_0fe1,
        address: 0,
        mnemonic: "str",
        operands: "w1,[sp,#12]",
        target: None,
    },
    Case {
        word: 0xa9bf_7bfd,
        address: 0,
        mnemonic: "stp",
        operands: "x29,x30,[sp,#-16]!",
        target: None,
    },
    Case {
        word: 0xa8c1_7bfd,
        address: 0,
        mnemonic: "ldp",
        operands: "x29,x30,[sp],#16",
        target: None,
    },
    Case {
        word: 0xc85f_7c20,
        address: 0,
        mnemonic: "ldxr",
        operands: "x0,[x1]",
        target: None,
    },
    // --- Data processing (register) ---
    Case {
        word: 0x8b02_0020,
        address: 0,
        mnemonic: "add",
        operands: "x0,x1,x2",
        target: None,
    },
    Case {
        word: 0x4b45_0883,
        address: 0,
        mnemonic: "sub",
        operands: "w3,w4,w5,lsr #2",
        target: None,
    },
    Case {
        word: 0x9ac2_0820,
        address: 0,
        mnemonic: "udiv",
        operands: "x0,x1,x2",
        target: None,
    },
    Case {
        word: 0x9b02_0c20,
        address: 0,
        mnemonic: "madd",
        operands: "x0,x1,x2,x3",
        target: None,
    },
    Case {
        word: 0x9a82_0020,
        address: 0,
        mnemonic: "csel",
        operands: "x0,x1,x2,eq",
        target: None,
    },
];

#[test]
fn known_encodings_decode_byte_for_byte() {
    for case in CASES {
        let bytes = case.word.to_le_bytes();
        let insn = decode(&bytes, case.address).expect("non-empty input decodes");
        assert_eq!(
            insn.mnemonic, case.mnemonic,
            "mnemonic for {:#010x}",
            case.word
        );
        assert_eq!(
            insn.operands, case.operands,
            "operands for {}",
            case.mnemonic
        );
        assert_eq!(
            insn.branch_target, case.target,
            "target for {}",
            case.mnemonic
        );
        assert_eq!(insn.length, 4);
        assert_eq!(insn.bytes, bytes);
        assert_eq!(insn.address, case.address);
    }
}

#[test]
fn uncovered_encodings_render_as_inst() {
    // The all-zero word (op0 = 0000, unallocated).
    let insn = decode(&[0, 0, 0, 0], 0).expect("decodes");
    assert_eq!(insn.mnemonic, ".inst");
    assert_eq!(insn.operands, "0x00000000");
    assert_eq!(insn.length, 4);

    // A SIMD data-processing word (op0 = 0111) is summarised, never guessed.
    let insn = decode(&0x4ea1_1c20u32.to_le_bytes(), 0).expect("decodes");
    assert_eq!(insn.mnemonic, ".inst");
    assert_eq!(insn.operands, "0x4ea11c20");
}

#[test]
fn truncation_fails_closed() {
    assert!(decode(&[], 0).is_none());
    let insn = decode(&[0x1f, 0x20], 0).expect("decodes");
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 2);
}
