//! RV64GC instruction decoder (I, M, A, F, D, Zicsr, Zifencei, C).
//!
//! The 16/32-bit length discipline is the core correctness property: the
//! parcel length is read from the low opcode bits exactly as the RISC-V
//! unprivileged ISA specifies (§1.5, expanded-length encoding), so a
//! compressed instruction never swallows its successor and a walk stays in
//! step with the real instruction stream. Reserved longer parcels (48/64
//! bit) are consumed at their declared length and rendered as `(bad)` —
//! RV64GC defines no such instruction, but mis-lengthing them would
//! desynchronise everything after.
//!
//! Mnemonics are canonical, never pseudo-instruction aliases (`addi
//! x0,x0,0` is rendered `addi zero,zero,0`, not `nop`): an alias table is a
//! second spelling of the same decode and a reader inspecting bytes wants
//! the encoding named, not paraphrased. Registers use ABI names; CSRs are
//! rendered as hex numbers.

use alloc::format;
use alloc::string::String;

use crate::{branch_target, sign_extend, Insn};

/// ABI names of the integer registers `x0..x31`.
const XREG: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// ABI names of the floating-point registers `f0..f31`.
const FREG: [&str; 32] = [
    "ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7", "fs0", "fs1", "fa0", "fa1", "fa2",
    "fa3", "fa4", "fa5", "fa6", "fa7", "fs2", "fs3", "fs4", "fs5", "fs6", "fs7", "fs8", "fs9",
    "fs10", "fs11", "ft8", "ft9", "ft10", "ft11",
];

/// Bits `[hi:lo]` of `word` as a `u32` (inclusive bounds, hi < 32).
fn bits(word: u32, hi: u32, lo: u32) -> u32 {
    (word >> lo) & ((1 << (hi - lo + 1)) - 1)
}

/// Integer register name for a 5-bit field.
fn x(field: u32) -> &'static str {
    XREG[usize::try_from(field & 31).unwrap_or(0)]
}

/// FP register name for a 5-bit field.
fn f(field: u32) -> &'static str {
    FREG[usize::try_from(field & 31).unwrap_or(0)]
}

/// Parcel length in bytes declared by the low bits of the first parcel.
fn declared_length(first: u16) -> usize {
    if first & 0b11 != 0b11 {
        2
    } else if first & 0b1_1100 != 0b1_1100 {
        4
    } else if first & 0b11_1111 == 0b01_1111 {
        6
    } else if first & 0b111_1111 == 0b011_1111 {
        8
    } else {
        // ≥ 80-bit reserved space: no RV64GC instruction lives here; consume
        // one parcel so the walk resynchronises at the next 16-bit boundary.
        2
    }
}

/// Decodes one instruction at `address` from the front of `code`.
///
/// Returns `None` only for an empty slice. Any other input yields an
/// instruction consuming at least one byte: a truncated or reserved parcel
/// renders as `(bad)` over the bytes that are actually present, so a walk
/// always makes forward progress and never reads past the slice.
#[must_use]
pub fn decode(code: &[u8], address: u64) -> Option<Insn> {
    if code.is_empty() {
        return None;
    }
    if code.len() == 1 {
        return Some(Insn::bad(address, code));
    }
    let first = u16::from_le_bytes([code[0], code[1]]);
    let length = declared_length(first);
    if code.len() < length {
        return Some(Insn::bad(address, code));
    }
    let consumed = &code[..length];
    let insn = match length {
        2 => decode_compressed(first, address, consumed),
        4 => {
            let word = u32::from_le_bytes([code[0], code[1], code[2], code[3]]);
            decode_full(word, address, consumed)
        }
        _ => Insn::bad(address, consumed),
    };
    Some(insn)
}

/// Builds an [`Insn`] with a static mnemonic.
fn ins(address: u64, consumed: &[u8], mnemonic: &str, operands: String) -> Insn {
    Insn::new(address, consumed, String::from(mnemonic), operands, None)
}

/// I-type immediate (bits 31:20, sign-extended).
fn imm_i(word: u32) -> i64 {
    sign_extend(u64::from(bits(word, 31, 20)), 12)
}

/// S-type immediate.
fn imm_s(word: u32) -> i64 {
    sign_extend(u64::from((bits(word, 31, 25) << 5) | bits(word, 11, 7)), 12)
}

/// B-type immediate (byte offset).
fn imm_b(word: u32) -> i64 {
    let raw = (bits(word, 31, 31) << 12)
        | (bits(word, 7, 7) << 11)
        | (bits(word, 30, 25) << 5)
        | (bits(word, 11, 8) << 1);
    sign_extend(u64::from(raw), 13)
}

/// J-type immediate (byte offset).
fn imm_j(word: u32) -> i64 {
    let raw = (bits(word, 31, 31) << 20)
        | (bits(word, 19, 12) << 12)
        | (bits(word, 20, 20) << 11)
        | (bits(word, 30, 21) << 1);
    sign_extend(u64::from(raw), 21)
}

/// `fence` operand letters for a 4-bit `iorw` mask.
fn fence_set(mask: u32) -> String {
    let mut out = String::new();
    for (bit, letter) in [(3, 'i'), (2, 'o'), (1, 'r'), (0, 'w')] {
        if mask & (1 << bit) != 0 {
            out.push(letter);
        }
    }
    out
}

/// A taken PC-relative branch/jump rendered with its absolute target.
fn jump(address: u64, consumed: &[u8], mnemonic: &str, prefix: &str, offset: i64) -> Insn {
    let target = branch_target(address, offset);
    let operands = if prefix.is_empty() {
        format!("{target:#x}")
    } else {
        format!("{prefix},{target:#x}")
    };
    Insn::new(
        address,
        consumed,
        String::from(mnemonic),
        operands,
        Some(target),
    )
}

/// Decodes a full 32-bit instruction word.
#[allow(clippy::too_many_lines)] // One exhaustive opcode match; splitting it would scatter the map.
fn decode_full(word: u32, address: u64, consumed: &[u8]) -> Insn {
    let opcode = bits(word, 6, 0);
    let rd = bits(word, 11, 7);
    let f3 = bits(word, 14, 12);
    let rs1 = bits(word, 19, 15);
    let rs2 = bits(word, 24, 20);
    let f7 = bits(word, 31, 25);
    let bad = || Insn::bad(address, consumed);

    match opcode {
        // LOAD
        0x03 => {
            let name = match f3 {
                0 => "lb",
                1 => "lh",
                2 => "lw",
                3 => "ld",
                4 => "lbu",
                5 => "lhu",
                6 => "lwu",
                _ => return bad(),
            };
            ins(
                address,
                consumed,
                name,
                format!("{},{}({})", x(rd), imm_i(word), x(rs1)),
            )
        }
        // LOAD-FP
        0x07 => {
            let name = match f3 {
                2 => "flw",
                3 => "fld",
                _ => return bad(),
            };
            ins(
                address,
                consumed,
                name,
                format!("{},{}({})", f(rd), imm_i(word), x(rs1)),
            )
        }
        // MISC-MEM
        0x0f => match f3 {
            0 => {
                let pred = fence_set(bits(word, 27, 24));
                let succ = fence_set(bits(word, 23, 20));
                ins(address, consumed, "fence", format!("{pred},{succ}"))
            }
            1 => ins(address, consumed, "fence.i", String::new()),
            _ => bad(),
        },
        // OP-IMM
        0x13 => match f3 {
            0 | 2 | 3 | 4 | 6 | 7 => {
                let name = match f3 {
                    0 => "addi",
                    2 => "slti",
                    3 => "sltiu",
                    4 => "xori",
                    6 => "ori",
                    _ => "andi",
                };
                ins(
                    address,
                    consumed,
                    name,
                    format!("{},{},{}", x(rd), x(rs1), imm_i(word)),
                )
            }
            1 | 5 => {
                let name = match (f3, bits(word, 31, 26)) {
                    (1, 0x00) => "slli",
                    (5, 0x00) => "srli",
                    (5, 0x10) => "srai",
                    _ => return bad(),
                };
                let shamt = bits(word, 25, 20);
                ins(
                    address,
                    consumed,
                    name,
                    format!("{},{},{shamt}", x(rd), x(rs1)),
                )
            }
            _ => bad(),
        },
        // OP-IMM-32
        0x1b => match (f3, f7) {
            (0, _) => ins(
                address,
                consumed,
                "addiw",
                format!("{},{},{}", x(rd), x(rs1), imm_i(word)),
            ),
            (1 | 5, 0x00) | (5, 0x20) => {
                let name = match (f3, f7) {
                    (1, _) => "slliw",
                    (_, 0x00) => "srliw",
                    _ => "sraiw",
                };
                ins(
                    address,
                    consumed,
                    name,
                    format!("{},{},{rs2}", x(rd), x(rs1)),
                )
            }
            _ => bad(),
        },
        // AUIPC / LUI
        0x17 => ins(
            address,
            consumed,
            "auipc",
            format!("{},{:#x}", x(rd), bits(word, 31, 12)),
        ),
        0x37 => ins(
            address,
            consumed,
            "lui",
            format!("{},{:#x}", x(rd), bits(word, 31, 12)),
        ),
        // STORE
        0x23 => {
            let name = match f3 {
                0 => "sb",
                1 => "sh",
                2 => "sw",
                3 => "sd",
                _ => return bad(),
            };
            ins(
                address,
                consumed,
                name,
                format!("{},{}({})", x(rs2), imm_s(word), x(rs1)),
            )
        }
        // STORE-FP
        0x27 => {
            let name = match f3 {
                2 => "fsw",
                3 => "fsd",
                _ => return bad(),
            };
            ins(
                address,
                consumed,
                name,
                format!("{},{}({})", f(rs2), imm_s(word), x(rs1)),
            )
        }
        // AMO
        0x2f => {
            let width = match f3 {
                2 => "w",
                3 => "d",
                _ => return bad(),
            };
            let name = match bits(word, 31, 27) {
                0b00010 if rs2 == 0 => "lr",
                0b00011 => "sc",
                0b00001 => "amoswap",
                0b00000 => "amoadd",
                0b00100 => "amoxor",
                0b01100 => "amoand",
                0b01000 => "amoor",
                0b10000 => "amomin",
                0b10100 => "amomax",
                0b11000 => "amominu",
                0b11100 => "amomaxu",
                _ => return bad(),
            };
            let order = match (bits(word, 26, 26), bits(word, 25, 25)) {
                (1, 1) => ".aqrl",
                (1, 0) => ".aq",
                (0, 1) => ".rl",
                _ => "",
            };
            let mnemonic = format!("{name}.{width}{order}");
            let operands = if name == "lr" {
                format!("{},({})", x(rd), x(rs1))
            } else {
                format!("{},{},({})", x(rd), x(rs2), x(rs1))
            };
            Insn::new(address, consumed, mnemonic, operands, None)
        }
        // OP
        0x33 => {
            let name = match (f7, f3) {
                (0x00, 0) => "add",
                (0x00, 1) => "sll",
                (0x00, 2) => "slt",
                (0x00, 3) => "sltu",
                (0x00, 4) => "xor",
                (0x00, 5) => "srl",
                (0x00, 6) => "or",
                (0x00, 7) => "and",
                (0x20, 0) => "sub",
                (0x20, 5) => "sra",
                (0x01, 0) => "mul",
                (0x01, 1) => "mulh",
                (0x01, 2) => "mulhsu",
                (0x01, 3) => "mulhu",
                (0x01, 4) => "div",
                (0x01, 5) => "divu",
                (0x01, 6) => "rem",
                (0x01, 7) => "remu",
                _ => return bad(),
            };
            ins(
                address,
                consumed,
                name,
                format!("{},{},{}", x(rd), x(rs1), x(rs2)),
            )
        }
        // OP-32
        0x3b => {
            let name = match (f7, f3) {
                (0x00, 0) => "addw",
                (0x00, 1) => "sllw",
                (0x00, 5) => "srlw",
                (0x20, 0) => "subw",
                (0x20, 5) => "sraw",
                (0x01, 0) => "mulw",
                (0x01, 4) => "divw",
                (0x01, 5) => "divuw",
                (0x01, 6) => "remw",
                (0x01, 7) => "remuw",
                _ => return bad(),
            };
            ins(
                address,
                consumed,
                name,
                format!("{},{},{}", x(rd), x(rs1), x(rs2)),
            )
        }
        // BRANCH
        0x63 => {
            let name = match f3 {
                0 => "beq",
                1 => "bne",
                4 => "blt",
                5 => "bge",
                6 => "bltu",
                7 => "bgeu",
                _ => return bad(),
            };
            let prefix = format!("{},{}", x(rs1), x(rs2));
            jump(address, consumed, name, &prefix, imm_b(word))
        }
        // JALR / JAL
        0x67 if f3 == 0 => ins(
            address,
            consumed,
            "jalr",
            format!("{},{}({})", x(rd), imm_i(word), x(rs1)),
        ),
        0x6f => jump(address, consumed, "jal", x(rd), imm_j(word)),
        // SYSTEM
        0x73 => match f3 {
            0 if rd == 0 && rs1 == 0 => {
                let name = match bits(word, 31, 20) {
                    0x000 => "ecall",
                    0x001 => "ebreak",
                    0x102 => "sret",
                    0x302 => "mret",
                    0x105 => "wfi",
                    _ => return bad(),
                };
                ins(address, consumed, name, String::new())
            }
            1..=3 => {
                let name = match f3 {
                    1 => "csrrw",
                    2 => "csrrs",
                    _ => "csrrc",
                };
                let csr = bits(word, 31, 20);
                ins(
                    address,
                    consumed,
                    name,
                    format!("{},{csr:#x},{}", x(rd), x(rs1)),
                )
            }
            5..=7 => {
                let name = match f3 {
                    5 => "csrrwi",
                    6 => "csrrsi",
                    _ => "csrrci",
                };
                let csr = bits(word, 31, 20);
                ins(address, consumed, name, format!("{},{csr:#x},{rs1}", x(rd)))
            }
            _ => bad(),
        },
        // Fused multiply-add family
        0x43 | 0x47 | 0x4b | 0x4f => {
            let base = match opcode {
                0x43 => "fmadd",
                0x47 => "fmsub",
                0x4b => "fnmsub",
                _ => "fnmadd",
            };
            let fmt = match bits(word, 26, 25) {
                0 => "s",
                1 => "d",
                _ => return bad(),
            };
            let rs3 = bits(word, 31, 27);
            Insn::new(
                address,
                consumed,
                format!("{base}.{fmt}"),
                format!("{},{},{},{}", f(rd), f(rs1), f(rs2), f(rs3)),
                None,
            )
        }
        // OP-FP
        0x53 => decode_fp(word, address, consumed),
        _ => bad(),
    }
}

/// Decodes the OP-FP (0x53) group: F and D extension register ops.
#[allow(clippy::too_many_lines)] // One exhaustive funct5 match; splitting it would scatter the map.
fn decode_fp(word: u32, address: u64, consumed: &[u8]) -> Insn {
    let rd = bits(word, 11, 7);
    let f3 = bits(word, 14, 12);
    let rs1 = bits(word, 19, 15);
    let rs2 = bits(word, 24, 20);
    let funct5 = bits(word, 31, 27);
    let bad = || Insn::bad(address, consumed);
    let fmt = match bits(word, 26, 25) {
        0 => "s",
        1 => "d",
        _ => return bad(),
    };
    let named = |name: &str, operands: String| {
        Insn::new(address, consumed, format!("{name}.{fmt}"), operands, None)
    };

    match funct5 {
        0x00..=0x03 => {
            let name = match funct5 {
                0x00 => "fadd",
                0x01 => "fsub",
                0x02 => "fmul",
                _ => "fdiv",
            };
            named(name, format!("{},{},{}", f(rd), f(rs1), f(rs2)))
        }
        0x0b if rs2 == 0 => named("fsqrt", format!("{},{}", f(rd), f(rs1))),
        0x04 => {
            let name = match f3 {
                0 => "fsgnj",
                1 => "fsgnjn",
                2 => "fsgnjx",
                _ => return bad(),
            };
            named(name, format!("{},{},{}", f(rd), f(rs1), f(rs2)))
        }
        0x05 => {
            let name = match f3 {
                0 => "fmin",
                1 => "fmax",
                _ => return bad(),
            };
            named(name, format!("{},{},{}", f(rd), f(rs1), f(rs2)))
        }
        0x08 => {
            let name = match (fmt, rs2) {
                ("s", 1) => "fcvt.s.d",
                ("d", 0) => "fcvt.d.s",
                _ => return bad(),
            };
            ins(address, consumed, name, format!("{},{}", f(rd), f(rs1)))
        }
        0x14 => {
            let name = match f3 {
                0 => "fle",
                1 => "flt",
                2 => "feq",
                _ => return bad(),
            };
            named(name, format!("{},{},{}", x(rd), f(rs1), f(rs2)))
        }
        0x18 => {
            let to = match rs2 {
                0 => "w",
                1 => "wu",
                2 => "l",
                3 => "lu",
                _ => return bad(),
            };
            let mnemonic = format!("fcvt.{to}.{fmt}");
            Insn::new(
                address,
                consumed,
                mnemonic,
                format!("{},{}", x(rd), f(rs1)),
                None,
            )
        }
        0x1a => {
            let from = match rs2 {
                0 => "w",
                1 => "wu",
                2 => "l",
                3 => "lu",
                _ => return bad(),
            };
            let mnemonic = format!("fcvt.{fmt}.{from}");
            Insn::new(
                address,
                consumed,
                mnemonic,
                format!("{},{}", f(rd), x(rs1)),
                None,
            )
        }
        0x1c if rs2 == 0 && f3 == 0 => {
            let name = if fmt == "s" { "fmv.x.w" } else { "fmv.x.d" };
            ins(address, consumed, name, format!("{},{}", x(rd), f(rs1)))
        }
        0x1c if rs2 == 0 && f3 == 1 => named("fclass", format!("{},{}", x(rd), f(rs1))),
        0x1e if rs2 == 0 && f3 == 0 => {
            let name = if fmt == "s" { "fmv.w.x" } else { "fmv.d.x" };
            ins(address, consumed, name, format!("{},{}", f(rd), x(rs1)))
        }
        _ => bad(),
    }
}

/// Compressed register name (3-bit field selects `x8..x15`).
fn xc(field: u32) -> &'static str {
    x((field & 7) + 8)
}

/// Compressed FP register name (3-bit field selects `f8..f15`).
fn fc(field: u32) -> &'static str {
    f((field & 7) + 8)
}

/// Decodes one 16-bit compressed instruction (RV64 C extension).
#[allow(clippy::too_many_lines)] // One exhaustive quadrant match; splitting it would scatter the map.
fn decode_compressed(first: u16, address: u64, consumed: &[u8]) -> Insn {
    let w = u32::from(first);
    let f3 = bits(w, 15, 13);
    let bad = || Insn::bad(address, consumed);

    match w & 3 {
        // Quadrant 0
        0 => match f3 {
            0 => {
                // c.addi4spn: nzuimm[5:4|9:6|2|3] = inst[12:11|10:7|6|5].
                let imm = (bits(w, 12, 11) << 4)
                    | (bits(w, 10, 7) << 6)
                    | (bits(w, 6, 6) << 2)
                    | (bits(w, 5, 5) << 3);
                if imm == 0 {
                    // Covers the all-zero parcel, the defined illegal instruction.
                    return bad();
                }
                ins(
                    address,
                    consumed,
                    "c.addi4spn",
                    format!("{},sp,{imm}", xc(bits(w, 4, 2))),
                )
            }
            1 | 3 | 5 | 7 => {
                // c.fld / c.ld / c.fsd / c.sd: uimm[5:3|7:6] = inst[12:10|6:5].
                let off = (bits(w, 12, 10) << 3) | (bits(w, 6, 5) << 6);
                let (name, reg) = match f3 {
                    1 => ("c.fld", fc(bits(w, 4, 2))),
                    3 => ("c.ld", xc(bits(w, 4, 2))),
                    5 => ("c.fsd", fc(bits(w, 4, 2))),
                    _ => ("c.sd", xc(bits(w, 4, 2))),
                };
                ins(
                    address,
                    consumed,
                    name,
                    format!("{reg},{off}({})", xc(bits(w, 9, 7))),
                )
            }
            2 | 6 => {
                // c.lw / c.sw: uimm[5:3|2|6] = inst[12:10|6|5].
                let off = (bits(w, 12, 10) << 3) | (bits(w, 6, 6) << 2) | (bits(w, 5, 5) << 6);
                let name = if f3 == 2 { "c.lw" } else { "c.sw" };
                ins(
                    address,
                    consumed,
                    name,
                    format!("{},{off}({})", xc(bits(w, 4, 2)), xc(bits(w, 9, 7))),
                )
            }
            _ => bad(),
        },
        // Quadrant 1
        1 => {
            let rd = bits(w, 11, 7);
            let imm6 = sign_extend(u64::from((bits(w, 12, 12) << 5) | bits(w, 6, 2)), 6);
            match f3 {
                0 => {
                    if rd == 0 && imm6 == 0 {
                        return ins(address, consumed, "c.nop", String::new());
                    }
                    ins(address, consumed, "c.addi", format!("{},{imm6}", x(rd)))
                }
                1 => {
                    if rd == 0 {
                        return bad();
                    }
                    ins(address, consumed, "c.addiw", format!("{},{imm6}", x(rd)))
                }
                2 => ins(address, consumed, "c.li", format!("{},{imm6}", x(rd))),
                3 => {
                    if rd == 2 {
                        // c.addi16sp: nzimm[9|4|6|8:7|5] = inst[12|6|5|4:3|2].
                        let raw = (bits(w, 12, 12) << 9)
                            | (bits(w, 6, 6) << 4)
                            | (bits(w, 5, 5) << 6)
                            | (bits(w, 4, 3) << 7)
                            | (bits(w, 2, 2) << 5);
                        let imm = sign_extend(u64::from(raw), 10);
                        if imm == 0 {
                            return bad();
                        }
                        ins(address, consumed, "c.addi16sp", format!("sp,{imm}"))
                    } else {
                        if imm6 == 0 {
                            return bad();
                        }
                        // Rendered like lui: the 20-bit field the six bits sign-extend into.
                        let field = u64::from_le_bytes(imm6.to_le_bytes()) & 0xf_ffff;
                        ins(address, consumed, "c.lui", format!("{},{field:#x}", x(rd)))
                    }
                }
                4 => {
                    let rdp = xc(bits(w, 9, 7));
                    match bits(w, 11, 10) {
                        0 | 1 => {
                            let name = if bits(w, 11, 10) == 0 {
                                "c.srli"
                            } else {
                                "c.srai"
                            };
                            let shamt = (bits(w, 12, 12) << 5) | bits(w, 6, 2);
                            ins(address, consumed, name, format!("{rdp},{shamt}"))
                        }
                        2 => ins(address, consumed, "c.andi", format!("{rdp},{imm6}")),
                        _ => {
                            let name = match (bits(w, 12, 12), bits(w, 6, 5)) {
                                (0, 0) => "c.sub",
                                (0, 1) => "c.xor",
                                (0, 2) => "c.or",
                                (0, 3) => "c.and",
                                (1, 0) => "c.subw",
                                (1, 1) => "c.addw",
                                _ => return bad(),
                            };
                            ins(
                                address,
                                consumed,
                                name,
                                format!("{rdp},{}", xc(bits(w, 4, 2))),
                            )
                        }
                    }
                }
                5 => {
                    // c.j: imm[11|4|9:8|10|6|7|3:1|5] = inst[12|11|10:9|8|7|6|5:3|2].
                    let raw = (bits(w, 12, 12) << 11)
                        | (bits(w, 11, 11) << 4)
                        | (bits(w, 10, 9) << 8)
                        | (bits(w, 8, 8) << 10)
                        | (bits(w, 7, 7) << 6)
                        | (bits(w, 6, 6) << 7)
                        | (bits(w, 5, 3) << 1)
                        | (bits(w, 2, 2) << 5);
                    jump(
                        address,
                        consumed,
                        "c.j",
                        "",
                        sign_extend(u64::from(raw), 12),
                    )
                }
                _ => {
                    // c.beqz / c.bnez: imm[8|4:3|7:6|2:1|5] = inst[12|11:10|6:5|4:3|2].
                    let raw = (bits(w, 12, 12) << 8)
                        | (bits(w, 11, 10) << 3)
                        | (bits(w, 6, 5) << 6)
                        | (bits(w, 4, 3) << 1)
                        | (bits(w, 2, 2) << 5);
                    let name = if f3 == 6 { "c.beqz" } else { "c.bnez" };
                    jump(
                        address,
                        consumed,
                        name,
                        xc(bits(w, 9, 7)),
                        sign_extend(u64::from(raw), 9),
                    )
                }
            }
        }
        // Quadrant 2
        2 => {
            let rd = bits(w, 11, 7);
            let rs2 = bits(w, 6, 2);
            match f3 {
                0 => {
                    let shamt = (bits(w, 12, 12) << 5) | bits(w, 6, 2);
                    ins(address, consumed, "c.slli", format!("{},{shamt}", x(rd)))
                }
                1 | 3 => {
                    // c.fldsp / c.ldsp: uimm[5|4:3|8:6] = inst[12|6:5|4:2].
                    let off = (bits(w, 12, 12) << 5) | (bits(w, 6, 5) << 3) | (bits(w, 4, 2) << 6);
                    if f3 == 1 {
                        ins(address, consumed, "c.fldsp", format!("{},{off}(sp)", f(rd)))
                    } else {
                        if rd == 0 {
                            return bad();
                        }
                        ins(address, consumed, "c.ldsp", format!("{},{off}(sp)", x(rd)))
                    }
                }
                2 => {
                    // c.lwsp: uimm[5|4:2|7:6] = inst[12|6:4|3:2].
                    if rd == 0 {
                        return bad();
                    }
                    let off = (bits(w, 12, 12) << 5) | (bits(w, 6, 4) << 2) | (bits(w, 3, 2) << 6);
                    ins(address, consumed, "c.lwsp", format!("{},{off}(sp)", x(rd)))
                }
                4 => match (bits(w, 12, 12), rd, rs2) {
                    (0, 0, 0) => bad(),
                    (0, _, 0) => ins(address, consumed, "c.jr", String::from(x(rd))),
                    (0, _, _) => ins(address, consumed, "c.mv", format!("{},{}", x(rd), x(rs2))),
                    (_, 0, 0) => ins(address, consumed, "c.ebreak", String::new()),
                    (_, _, 0) => ins(address, consumed, "c.jalr", String::from(x(rd))),
                    _ => ins(address, consumed, "c.add", format!("{},{}", x(rd), x(rs2))),
                },
                5 | 7 => {
                    // c.fsdsp / c.sdsp: uimm[5:3|8:6] = inst[12:10|9:7].
                    let off = (bits(w, 12, 10) << 3) | (bits(w, 9, 7) << 6);
                    if f3 == 5 {
                        ins(
                            address,
                            consumed,
                            "c.fsdsp",
                            format!("{},{off}(sp)", f(rs2)),
                        )
                    } else {
                        ins(address, consumed, "c.sdsp", format!("{},{off}(sp)", x(rs2)))
                    }
                }
                _ => {
                    // c.swsp: uimm[5:2|7:6] = inst[12:9|8:7].
                    let off = (bits(w, 12, 9) << 2) | (bits(w, 8, 7) << 6);
                    ins(address, consumed, "c.swsp", format!("{},{off}(sp)", x(rs2)))
                }
            }
        }
        _ => bad(),
    }
}

#[cfg(test)]
#[path = "riscv64_tests.rs"]
mod tests;
