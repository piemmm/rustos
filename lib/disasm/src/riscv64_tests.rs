//! Conformance tests for the RV64GC decoder: known encodings checked
//! byte-for-byte, the 16/32-bit length discipline, and the fail-closed
//! renderings for truncated, reserved, and illegal parcels.

use alloc::vec::Vec;

use super::decode;
use crate::BAD_MNEMONIC;

/// One conformance row: encoding bytes at `address` must decode to exactly
/// this mnemonic, operand text, length, and branch target.
struct Case {
    bytes: &'static [u8],
    address: u64,
    mnemonic: &'static str,
    operands: &'static str,
    length: usize,
    target: Option<u64>,
}

/// Hand-assembled RV64GC encodings (little-endian byte order).
const CASES: &[Case] = &[
    // --- RV64I ---
    Case {
        bytes: &0x0000_0513u32.to_le_bytes(),
        address: 0,
        mnemonic: "addi",
        operands: "a0,zero,0",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0xfe01_0113u32.to_le_bytes(),
        address: 0,
        mnemonic: "addi",
        operands: "sp,sp,-32",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x0000_8067u32.to_le_bytes(),
        address: 0,
        mnemonic: "jalr",
        operands: "zero,0(ra)",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x0001_17b7u32.to_le_bytes(),
        address: 0,
        mnemonic: "lui",
        operands: "a5,0x11",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x0000_0297u32.to_le_bytes(),
        address: 0,
        mnemonic: "auipc",
        operands: "t0,0x0",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x0007_b703u32.to_le_bytes(),
        address: 0,
        mnemonic: "ld",
        operands: "a4,0(a5)",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x00f7_3023u32.to_le_bytes(),
        address: 0,
        mnemonic: "sd",
        operands: "a5,0(a4)",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x00f7_0463u32.to_le_bytes(),
        address: 0x1000,
        mnemonic: "beq",
        operands: "a4,a5,0x1008",
        length: 4,
        target: Some(0x1008),
    },
    Case {
        bytes: &0x0080_00efu32.to_le_bytes(),
        address: 0x2000,
        mnemonic: "jal",
        operands: "ra,0x2008",
        length: 4,
        target: Some(0x2008),
    },
    Case {
        bytes: &0x4037_d793u32.to_le_bytes(),
        address: 0,
        mnemonic: "srai",
        operands: "a5,a5,3",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x0000_0073u32.to_le_bytes(),
        address: 0,
        mnemonic: "ecall",
        operands: "",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x3020_0073u32.to_le_bytes(),
        address: 0,
        mnemonic: "mret",
        operands: "",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x0ff0_000fu32.to_le_bytes(),
        address: 0,
        mnemonic: "fence",
        operands: "iorw,iorw",
        length: 4,
        target: None,
    },
    // --- M ---
    Case {
        bytes: &0x02f7_07b3u32.to_le_bytes(),
        address: 0,
        mnemonic: "mul",
        operands: "a5,a4,a5",
        length: 4,
        target: None,
    },
    // --- A ---
    Case {
        bytes: &0x00b6_252fu32.to_le_bytes(),
        address: 0,
        mnemonic: "amoadd.w",
        operands: "a0,a1,(a2)",
        length: 4,
        target: None,
    },
    Case {
        bytes: &0x1605_a52fu32.to_le_bytes(),
        address: 0,
        mnemonic: "lr.w.aqrl",
        operands: "a0,(a1)",
        length: 4,
        target: None,
    },
    // --- Zicsr ---
    Case {
        bytes: &0x3000_2573u32.to_le_bytes(),
        address: 0,
        mnemonic: "csrrs",
        operands: "a0,0x300,zero",
        length: 4,
        target: None,
    },
    // --- F/D ---
    Case {
        bytes: &0x02c5_8553u32.to_le_bytes(),
        address: 0,
        mnemonic: "fadd.d",
        operands: "fa0,fa1,fa2",
        length: 4,
        target: None,
    },
    // --- C extension ---
    Case {
        bytes: &0x4501u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.li",
        operands: "a0,0",
        length: 2,
        target: None,
    },
    Case {
        bytes: &0x850au16.to_le_bytes(),
        address: 0,
        mnemonic: "c.mv",
        operands: "a0,sp",
        length: 2,
        target: None,
    },
    Case {
        bytes: &0x9002u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.ebreak",
        operands: "",
        length: 2,
        target: None,
    },
    Case {
        bytes: &0x8082u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.jr",
        operands: "ra",
        length: 2,
        target: None,
    },
    Case {
        bytes: &0x0001u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.nop",
        operands: "",
        length: 2,
        target: None,
    },
    Case {
        bytes: &0x1141u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.addi",
        operands: "sp,-16",
        length: 2,
        target: None,
    },
    Case {
        bytes: &0xe406u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.sdsp",
        operands: "ra,8(sp)",
        length: 2,
        target: None,
    },
    Case {
        bytes: &0x60a2u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.ldsp",
        operands: "ra,8(sp)",
        length: 2,
        target: None,
    },
    Case {
        bytes: &0xa001u16.to_le_bytes(),
        address: 0x4000,
        mnemonic: "c.j",
        operands: "0x4000",
        length: 2,
        target: Some(0x4000),
    },
    Case {
        bytes: &0xc801u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.beqz",
        operands: "s0,0x10",
        length: 2,
        target: Some(0x10),
    },
    Case {
        bytes: &0x0040u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.addi4spn",
        operands: "s0,sp,4",
        length: 2,
        target: None,
    },
    Case {
        bytes: &0x4398u16.to_le_bytes(),
        address: 0,
        mnemonic: "c.lw",
        operands: "a4,0(a5)",
        length: 2,
        target: None,
    },
];

#[test]
fn known_encodings_decode_byte_for_byte() {
    for case in CASES {
        let insn = decode(case.bytes, case.address).expect("non-empty input decodes");
        assert_eq!(
            insn.mnemonic, case.mnemonic,
            "mnemonic for {:02x?}",
            case.bytes
        );
        assert_eq!(
            insn.operands, case.operands,
            "operands for {}",
            case.mnemonic
        );
        assert_eq!(insn.length, case.length, "length for {}", case.mnemonic);
        assert_eq!(
            insn.branch_target, case.target,
            "target for {}",
            case.mnemonic
        );
        assert_eq!(insn.address, case.address);
        assert_eq!(
            insn.bytes, case.bytes,
            "retained bytes for {}",
            case.mnemonic
        );
    }
}

#[test]
fn empty_input_yields_none() {
    assert!(decode(&[], 0).is_none());
}

#[test]
fn all_zero_parcel_is_the_illegal_instruction() {
    let insn = decode(&[0, 0], 0).expect("decodes");
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 2);
}

#[test]
fn truncated_full_word_consumes_what_is_present() {
    // 0x13 declares a 32-bit parcel; only two bytes exist.
    let insn = decode(&[0x13, 0x05], 0).expect("decodes");
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 2);

    let insn = decode(&[0x13], 0).expect("decodes");
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 1);
}

#[test]
fn reserved_length_parcels_consume_their_declared_length() {
    // Low six bits 0b011111 declare a 48-bit parcel; RV64GC defines none.
    let insn = decode(&[0x1f, 0, 0, 0, 0, 0, 0x13, 0x05], 0).expect("decodes");
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 6);

    // Low seven bits 0b0111111 declare a 64-bit parcel.
    let insn = decode(&[0x3f, 0, 0, 0, 0, 0, 0, 0], 0).expect("decodes");
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 8);

    // The ≥80-bit reserved space consumes one 16-bit parcel.
    let insn = decode(&[0x7f, 0, 0, 0], 0).expect("decodes");
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 2);
}

#[test]
fn mixed_width_walk_stays_in_step() {
    // c.li a0,0 ; addi sp,sp,-32 ; c.ebreak — 2 + 4 + 2 bytes.
    let mut stream = Vec::new();
    stream.extend_from_slice(&0x4501u16.to_le_bytes());
    stream.extend_from_slice(&0xfe01_0113u32.to_le_bytes());
    stream.extend_from_slice(&0x9002u16.to_le_bytes());

    let first = decode(&stream, 0x100).expect("first");
    assert_eq!((first.mnemonic.as_str(), first.length), ("c.li", 2));
    let second = decode(&stream[2..], 0x102).expect("second");
    assert_eq!((second.mnemonic.as_str(), second.length), ("addi", 4));
    let third = decode(&stream[6..], 0x106).expect("third");
    assert_eq!((third.mnemonic.as_str(), third.length), ("c.ebreak", 2));
}

#[test]
fn unallocated_encodings_fail_closed() {
    // BRANCH funct3=2 is unallocated.
    let insn = decode(&0x0000_2063u32.to_le_bytes(), 0).expect("decodes");
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 4);

    // Reserved compressed encoding: quadrant 0, funct3=4.
    let insn = decode(&0x8000u16.to_le_bytes(), 0).expect("decodes");
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 2);
}
