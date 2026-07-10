//! Conformance tests for the wasm body decoder: known opcode streams
//! checked byte-for-byte, block-nesting indentation, and the fail-closed
//! paths for overlong LEB128, truncation, and unknown opcodes.

use alloc::string::String;
use alloc::vec::Vec;

use super::{decode, MAX_INDENT_LEVELS};
use crate::BAD_MNEMONIC;

/// Decodes a whole body starting at `depth`, returning
/// `(mnemonic, operands, length)` per instruction.
fn walk(body: &[u8], depth: u32) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut level = depth;
    while let Some((insn, next)) =
        decode(&body[offset..], u64::try_from(offset).unwrap_or(0), level)
    {
        offset += insn.length;
        out.push((insn.mnemonic, insn.operands, insn.length));
        level = next;
    }
    out
}

#[test]
fn flat_body_decodes_byte_for_byte() {
    // local.get 0 ; i32.const 42 ; i32.add ; end — a function body at depth 1.
    let body = [0x20, 0x00, 0x41, 0x2a, 0x6a, 0x0b];
    let rows = walk(&body, 1);
    assert_eq!(
        rows,
        [
            (String::from("  local.get"), String::from("0"), 2),
            (String::from("  i32.const"), String::from("42"), 2),
            (String::from("  i32.add"), String::new(), 1),
            (String::from("end"), String::new(), 1),
        ]
    );
}

#[test]
fn nesting_indents_blocks_and_else() {
    // block ; if (result i32) ; i32.const 1 ; else ; i32.const 0 ; end ; end
    let body = [
        0x02, 0x40, 0x04, 0x7f, 0x41, 0x01, 0x05, 0x41, 0x00, 0x0b, 0x0b, 0x0b,
    ];
    let rows = walk(&body, 1);
    let text: Vec<&str> = rows.iter().map(|(m, _, _)| m.as_str()).collect();
    assert_eq!(
        text,
        [
            "  block",
            "    if",
            "      i32.const",
            "    else",
            "      i32.const",
            "    end",
            "  end",
            "end",
        ]
    );
    assert_eq!(rows[1].1, "(result i32)");
}

#[test]
fn branch_and_call_immediates_render() {
    let rows = walk(&[0x0c, 0x01, 0x0d, 0x00, 0x10, 0x05], 0);
    assert_eq!(
        rows,
        [
            (String::from("br"), String::from("1"), 2),
            (String::from("br_if"), String::from("0"), 2),
            (String::from("call"), String::from("5"), 2),
        ]
    );
}

#[test]
fn br_table_lists_every_label() {
    let insn = decode(&[0x0e, 0x02, 0x00, 0x01, 0x02], 0, 0)
        .expect("decodes")
        .0;
    assert_eq!(insn.mnemonic, "br_table");
    assert_eq!(insn.operands, "0 1 2");
    assert_eq!(insn.length, 5);
}

#[test]
fn br_table_count_above_the_bound_fails_closed() {
    // Count 4097 exceeds MAX_BR_TABLE_TARGETS.
    let insn = decode(&[0x0e, 0x81, 0x20, 0x00], 0, 0).expect("decodes").0;
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 1);
}

#[test]
fn memarg_and_consts_render() {
    let insn = decode(&[0x28, 0x02, 0x10], 0, 0).expect("decodes").0;
    assert_eq!(insn.mnemonic, "i32.load");
    assert_eq!(insn.operands, "offset=16 align=2");
    assert_eq!(insn.length, 3);

    let insn = decode(&[0x41, 0x7f], 0, 0).expect("decodes").0;
    assert_eq!(
        (insn.mnemonic.as_str(), insn.operands.as_str()),
        ("i32.const", "-1")
    );

    let insn = decode(&[0x43, 0x00, 0x00, 0x80, 0x3f], 0, 0)
        .expect("decodes")
        .0;
    assert_eq!(
        (insn.mnemonic.as_str(), insn.operands.as_str()),
        ("f32.const", "0x3f800000")
    );
    assert_eq!(insn.length, 5);
}

#[test]
fn call_indirect_and_ref_types_render() {
    let insn = decode(&[0x11, 0x01, 0x00], 0, 0).expect("decodes").0;
    assert_eq!(insn.mnemonic, "call_indirect");
    assert_eq!(insn.operands, "(type 1) (table 0)");

    let insn = decode(&[0xd0, 0x70], 0, 0).expect("decodes").0;
    assert_eq!(
        (insn.mnemonic.as_str(), insn.operands.as_str()),
        ("ref.null", "funcref")
    );
}

#[test]
fn prefixed_opcodes_render() {
    let insn = decode(&[0xfc, 0x00], 0, 0).expect("decodes").0;
    assert_eq!(insn.mnemonic, "i32.trunc_sat_f32_s");
    assert_eq!(insn.length, 2);

    let insn = decode(&[0xfc, 0x0b, 0x00], 0, 0).expect("decodes").0;
    assert_eq!(insn.mnemonic, "memory.fill");
    assert_eq!(insn.length, 3);
}

#[test]
fn overlong_leb_fails_closed() {
    // Final-byte bits that push the value outside i32 range.
    let insn = decode(&[0x41, 0x80, 0x80, 0x80, 0x80, 0x38], 0, 0)
        .expect("decodes")
        .0;
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 1);

    // The canonical five-byte encoding of i32::MIN stays accepted.
    let insn = decode(&[0x41, 0x80, 0x80, 0x80, 0x80, 0x78], 0, 0)
        .expect("decodes")
        .0;
    assert_eq!(
        (insn.mnemonic.as_str(), insn.operands.as_str()),
        ("i32.const", "-2147483648")
    );

    // One continuation byte too many for 32 bits.
    let insn = decode(&[0x41, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00], 0, 0)
        .expect("decodes")
        .0;
    assert_eq!(insn.mnemonic, BAD_MNEMONIC);
    assert_eq!(insn.length, 1);
}

#[test]
fn truncation_and_unknown_opcodes_fail_closed() {
    let insn = decode(&[0x41], 0, 0).expect("decodes").0;
    assert_eq!((insn.mnemonic.as_str(), insn.length), (BAD_MNEMONIC, 1));

    let insn = decode(&[0xff], 0, 0).expect("decodes").0;
    assert_eq!((insn.mnemonic.as_str(), insn.length), (BAD_MNEMONIC, 1));

    // memory.grow's reserved byte must be zero.
    let insn = decode(&[0x40, 0x01], 0, 0).expect("decodes").0;
    assert_eq!((insn.mnemonic.as_str(), insn.length), (BAD_MNEMONIC, 1));

    assert!(decode(&[], 0, 0).is_none());
}

#[test]
fn indentation_clamps_at_the_bound() {
    let (insn, next) = decode(&[0x01], 0, MAX_INDENT_LEVELS + 40).expect("decodes");
    let expected_indent = usize::try_from(MAX_INDENT_LEVELS).expect("fits") * 2;
    assert_eq!(insn.mnemonic.len(), expected_indent + "nop".len());
    assert_eq!(next, MAX_INDENT_LEVELS + 40);
}
