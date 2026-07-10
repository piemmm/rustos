//! Conformance tests for the x86_64 decoder: known encodings checked
//! byte-for-byte (length above all), prefix behaviour, ModRM/SIB forms,
//! and the `(bad)` resynchronisation paths.

use alloc::vec;

use super::decode;
use crate::BAD_MNEMONIC;

/// One conformance row: the bytes at `address` must decode to exactly this
/// mnemonic, operand text, length, and branch target.
struct Case {
    bytes: &'static [u8],
    address: u64,
    mnemonic: &'static str,
    operands: &'static str,
    target: Option<u64>,
}

/// Hand-assembled x86_64 encodings.
const CASES: &[Case] = &[
    // --- Stack and simple ops ---
    Case {
        bytes: &[0x55],
        address: 0,
        mnemonic: "push",
        operands: "rbp",
        target: None,
    },
    Case {
        bytes: &[0x41, 0x57],
        address: 0,
        mnemonic: "push",
        operands: "r15",
        target: None,
    },
    Case {
        bytes: &[0x5d],
        address: 0,
        mnemonic: "pop",
        operands: "rbp",
        target: None,
    },
    Case {
        bytes: &[0xc3],
        address: 0,
        mnemonic: "ret",
        operands: "",
        target: None,
    },
    Case {
        bytes: &[0xc9],
        address: 0,
        mnemonic: "leave",
        operands: "",
        target: None,
    },
    Case {
        bytes: &[0xcc],
        address: 0,
        mnemonic: "int3",
        operands: "",
        target: None,
    },
    Case {
        bytes: &[0x90],
        address: 0,
        mnemonic: "nop",
        operands: "",
        target: None,
    },
    Case {
        bytes: &[0xf4],
        address: 0,
        mnemonic: "hlt",
        operands: "",
        target: None,
    },
    Case {
        bytes: &[0x0f, 0x05],
        address: 0,
        mnemonic: "syscall",
        operands: "",
        target: None,
    },
    Case {
        bytes: &[0x0f, 0x0b],
        address: 0,
        mnemonic: "ud2",
        operands: "",
        target: None,
    },
    // --- Prefix behaviour ---
    Case {
        bytes: &[0x66, 0x90],
        address: 0,
        mnemonic: "xchg",
        operands: "ax,ax",
        target: None,
    },
    Case {
        bytes: &[0xf3, 0xc3],
        address: 0,
        mnemonic: "rep ret",
        operands: "",
        target: None,
    },
    Case {
        bytes: &[0xf0, 0xff, 0x00],
        address: 0,
        mnemonic: "lock inc",
        operands: "DWORD PTR [rax]",
        target: None,
    },
    // --- Register-to-register arithmetic ---
    Case {
        bytes: &[0x48, 0x89, 0xe5],
        address: 0,
        mnemonic: "mov",
        operands: "rbp,rsp",
        target: None,
    },
    Case {
        bytes: &[0x31, 0xc0],
        address: 0,
        mnemonic: "xor",
        operands: "eax,eax",
        target: None,
    },
    Case {
        bytes: &[0x85, 0xc0],
        address: 0,
        mnemonic: "test",
        operands: "eax,eax",
        target: None,
    },
    Case {
        bytes: &[0x48, 0x0f, 0xaf, 0xc3],
        address: 0,
        mnemonic: "imul",
        operands: "rax,rbx",
        target: None,
    },
    // --- Immediates ---
    Case {
        bytes: &[0x48, 0x83, 0xec, 0x10],
        address: 0,
        mnemonic: "sub",
        operands: "rsp,0x10",
        target: None,
    },
    Case {
        bytes: &[0x48, 0x83, 0xc4, 0xf8],
        address: 0,
        mnemonic: "add",
        operands: "rsp,0xfffffffffffffff8",
        target: None,
    },
    Case {
        bytes: &[0xb8, 0x2a, 0x00, 0x00, 0x00],
        address: 0,
        mnemonic: "mov",
        operands: "eax,0x2a",
        target: None,
    },
    Case {
        bytes: &[0x48, 0xc7, 0xc0, 0x3c, 0x00, 0x00, 0x00],
        address: 0,
        mnemonic: "mov",
        operands: "rax,0x3c",
        target: None,
    },
    Case {
        bytes: &[0x48, 0xb8, 0xf0, 0xde, 0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12],
        address: 0,
        mnemonic: "movabs",
        operands: "rax,0x123456789abcdef0",
        target: None,
    },
    // --- Memory operands ---
    Case {
        bytes: &[0x48, 0x8b, 0x45, 0xf8],
        address: 0,
        mnemonic: "mov",
        operands: "rax,QWORD PTR [rbp-0x8]",
        target: None,
    },
    Case {
        bytes: &[0x48, 0x8b, 0x04, 0xc8],
        address: 0,
        mnemonic: "mov",
        operands: "rax,QWORD PTR [rax+rcx*8]",
        target: None,
    },
    Case {
        bytes: &[0x8b, 0x04, 0x25, 0x78, 0x56, 0x34, 0x12],
        address: 0,
        mnemonic: "mov",
        operands: "eax,DWORD PTR [0x12345678]",
        target: None,
    },
    Case {
        bytes: &[0x48, 0x8d, 0x3d, 0x00, 0x00, 0x00, 0x00],
        address: 0x2000,
        mnemonic: "lea",
        operands: "rdi,[rip+0x0]",
        target: None,
    },
    Case {
        bytes: &[0x64, 0x48, 0x8b, 0x04, 0x25, 0x28, 0x00, 0x00, 0x00],
        address: 0,
        mnemonic: "mov",
        operands: "rax,QWORD PTR fs:[0x28]",
        target: None,
    },
    Case {
        bytes: &[0xff, 0x25, 0x00, 0x00, 0x00, 0x00],
        address: 0,
        mnemonic: "jmp",
        operands: "QWORD PTR [rip+0x0]",
        target: None,
    },
    // --- Groups ---
    Case {
        bytes: &[0xff, 0xd0],
        address: 0,
        mnemonic: "call",
        operands: "rax",
        target: None,
    },
    Case {
        bytes: &[0xf7, 0xf1],
        address: 0,
        mnemonic: "div",
        operands: "ecx",
        target: None,
    },
    Case {
        bytes: &[0xc1, 0xe0, 0x04],
        address: 0,
        mnemonic: "shl",
        operands: "eax,0x4",
        target: None,
    },
    // --- Relative branches ---
    Case {
        bytes: &[0xe8, 0x00, 0x00, 0x00, 0x00],
        address: 0x1000,
        mnemonic: "call",
        operands: "0x1005",
        target: Some(0x1005),
    },
    Case {
        bytes: &[0xeb, 0xfe],
        address: 0x2000,
        mnemonic: "jmp",
        operands: "0x2000",
        target: Some(0x2000),
    },
    Case {
        bytes: &[0x74, 0x05],
        address: 0x100,
        mnemonic: "je",
        operands: "0x107",
        target: Some(0x107),
    },
    Case {
        bytes: &[0x0f, 0x84, 0x00, 0x01, 0x00, 0x00],
        address: 0,
        mnemonic: "je",
        operands: "0x106",
        target: Some(0x106),
    },
    // --- Two-byte map ---
    Case {
        bytes: &[0x0f, 0xb6, 0xc0],
        address: 0,
        mnemonic: "movzx",
        operands: "eax,al",
        target: None,
    },
    Case {
        bytes: &[0x0f, 0x94, 0xc0],
        address: 0,
        mnemonic: "sete",
        operands: "al",
        target: None,
    },
    Case {
        bytes: &[0x0f, 0xc8],
        address: 0,
        mnemonic: "bswap",
        operands: "eax",
        target: None,
    },
    Case {
        bytes: &[0x0f, 0x1f, 0x40, 0x00],
        address: 0,
        mnemonic: "nop",
        operands: "DWORD PTR [rax+0x0]",
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
        assert_eq!(
            insn.length,
            case.bytes.len(),
            "length for {}",
            case.mnemonic
        );
        assert_eq!(
            insn.branch_target, case.target,
            "target for {}",
            case.mnemonic
        );
        assert_eq!(insn.bytes, case.bytes);
        assert_eq!(insn.address, case.address);
    }
}

#[test]
fn undecodable_bytes_resynchronise_one_byte_at_a_time() {
    // 0x06 (push es) is invalid in 64-bit mode.
    let insn = decode(&[0x06, 0x90], 0).expect("decodes");
    assert_eq!((insn.mnemonic.as_str(), insn.length), (BAD_MNEMONIC, 1));

    // A dangling REX prefix with no opcode.
    let insn = decode(&[0x48], 0).expect("decodes");
    assert_eq!((insn.mnemonic.as_str(), insn.length), (BAD_MNEMONIC, 1));

    // A truncated immediate.
    let insn = decode(&[0xe8, 0x00], 0).expect("decodes");
    assert_eq!((insn.mnemonic.as_str(), insn.length), (BAD_MNEMONIC, 1));

    assert!(decode(&[], 0).is_none());
}

#[test]
fn fifteen_byte_limit_is_enforced() {
    // Fourteen prefixes plus the opcode: exactly 15 bytes, still legal.
    let mut legal = vec![0x66u8; 14];
    legal.push(0x90);
    let insn = decode(&legal, 0).expect("decodes");
    assert_eq!(insn.mnemonic, "xchg");
    assert_eq!(insn.length, 15);

    // Fifteen prefixes push the opcode past the limit: refused.
    let mut over = vec![0x66u8; 15];
    over.push(0x90);
    let insn = decode(&over, 0).expect("decodes");
    assert_eq!((insn.mnemonic.as_str(), insn.length), (BAD_MNEMONIC, 1));
}

#[test]
fn walk_stays_in_step_across_variable_lengths() {
    // push rbp ; mov rbp,rsp ; sub rsp,0x10 ; ret
    let stream = [0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x10, 0xc3];
    let mut offset = 0usize;
    let mut names = alloc::vec::Vec::new();
    while offset < stream.len() {
        let insn =
            decode(&stream[offset..], u64::try_from(offset).expect("fits")).expect("decodes");
        names.push(insn.mnemonic);
        offset += insn.length;
    }
    assert_eq!(names, ["push", "mov", "sub", "ret"]);
    assert_eq!(offset, stream.len());
}
