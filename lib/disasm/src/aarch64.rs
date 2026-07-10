//! A64 (AArch64) instruction decoder.
//!
//! Every A64 instruction is exactly 32 bits, so the length discipline is
//! trivial; the honest-rendering discipline is the core property here: an
//! encoding the tables do not cover is rendered as `.inst 0x…` — never
//! skipped, never guessed — so a reader always sees the word that was
//! there. The major encoding groups are decoded with full operands: PC-rel
//! addressing, add/sub and logical immediates, move-wide, bitfield and
//! extract, all branch forms, exception generation and hints, loads and
//! stores (register, pair, literal), and the data-processing register
//! families. SIMD/FP data processing is summarised as `.inst` with full
//! operand decode staged; FP register loads/stores decode through the
//! normal load/store groups.

use alloc::format;
use alloc::string::String;

use crate::{branch_target, sign_extend, Insn};

/// 64-bit general register name (`sp_or_zero` picks the x31 meaning).
fn xreg(field: u32, sp: bool) -> String {
    let field = field & 31;
    if field == 31 {
        String::from(if sp { "sp" } else { "xzr" })
    } else {
        format!("x{field}")
    }
}

/// 32-bit general register name (`sp_or_zero` picks the w31 meaning).
fn wreg(field: u32, sp: bool) -> String {
    let field = field & 31;
    if field == 31 {
        String::from(if sp { "wsp" } else { "wzr" })
    } else {
        format!("w{field}")
    }
}

/// Register name in the width selected by `sf` (bit 31).
fn reg(sf: u32, field: u32, sp: bool) -> String {
    if sf == 1 {
        xreg(field, sp)
    } else {
        wreg(field, sp)
    }
}

/// Bits `[hi:lo]` of `word` (inclusive bounds, hi < 32).
fn bits(word: u32, hi: u32, lo: u32) -> u32 {
    (word >> lo) & ((1 << (hi - lo + 1)) - 1)
}

/// Condition-code name for a 4-bit field.
fn cond(field: u32) -> &'static str {
    [
        "eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "al",
        "nv",
    ][usize::try_from(field & 15).unwrap_or(0)]
}

/// The honest rendering for an encoding the tables do not cover.
fn unknown(word: u32, address: u64, consumed: &[u8]) -> Insn {
    Insn::new(
        address,
        consumed,
        String::from(".inst"),
        format!("{word:#010x}"),
        None,
    )
}

/// Decodes one instruction at `address` from the front of `code`.
///
/// Returns `None` only for an empty slice. Fewer than four remaining bytes
/// render as `(bad)` over what is present; a full word always decodes to
/// either a named instruction or the `.inst 0x…` fallback, so a walk makes
/// forward progress on any input.
#[must_use]
pub fn decode(code: &[u8], address: u64) -> Option<Insn> {
    if code.is_empty() {
        return None;
    }
    if code.len() < 4 {
        return Some(Insn::bad(address, code));
    }
    let consumed = &code[..4];
    let word = u32::from_le_bytes([code[0], code[1], code[2], code[3]]);
    Some(decode_word(word, address, consumed))
}

/// Dispatches on the top-level op0 group field (bits 28:25).
fn decode_word(word: u32, address: u64, consumed: &[u8]) -> Insn {
    match bits(word, 28, 25) {
        0b1000 | 0b1001 => data_processing_immediate(word, address, consumed),
        0b1010 | 0b1011 => branches_and_system(word, address, consumed),
        0b0100 | 0b0110 | 0b1100 | 0b1110 => loads_and_stores(word, address, consumed),
        0b0101 | 0b1101 => data_processing_register(word, address, consumed),
        _ => unknown(word, address, consumed),
    }
}

/// Builds an [`Insn`] with a static mnemonic.
fn ins(address: u64, consumed: &[u8], mnemonic: &str, operands: String) -> Insn {
    Insn::new(address, consumed, String::from(mnemonic), operands, None)
}

/// A PC-relative branch rendered with its absolute target.
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

/// Branches, exception generation, and system instructions.
fn branches_and_system(word: u32, address: u64, consumed: &[u8]) -> Insn {
    // B / BL: imm26 word offset.
    if bits(word, 30, 26) == 0b00101 {
        let name = if bits(word, 31, 31) == 1 { "bl" } else { "b" };
        let offset = sign_extend(u64::from(bits(word, 25, 0)), 26) * 4;
        return jump(address, consumed, name, "", offset);
    }
    // CBZ / CBNZ.
    if bits(word, 30, 25) == 0b01_1010 {
        let name = if bits(word, 24, 24) == 1 {
            "cbnz"
        } else {
            "cbz"
        };
        let offset = sign_extend(u64::from(bits(word, 23, 5)), 19) * 4;
        let prefix = reg(bits(word, 31, 31), bits(word, 4, 0), false);
        return jump(address, consumed, name, &prefix, offset);
    }
    // TBZ / TBNZ: bit number = b5:b40.
    if bits(word, 30, 25) == 0b01_1011 {
        let name = if bits(word, 24, 24) == 1 {
            "tbnz"
        } else {
            "tbz"
        };
        let bit = (bits(word, 31, 31) << 5) | bits(word, 23, 19);
        let offset = sign_extend(u64::from(bits(word, 18, 5)), 14) * 4;
        let rt = reg(bits(word, 31, 31), bits(word, 4, 0), false);
        return jump(address, consumed, name, &format!("{rt},#{bit}"), offset);
    }
    // B.cond.
    if bits(word, 31, 24) == 0b0101_0100 && bits(word, 4, 4) == 0 {
        let name = format!("b.{}", cond(bits(word, 3, 0)));
        let offset = sign_extend(u64::from(bits(word, 23, 5)), 19) * 4;
        let target = branch_target(address, offset);
        return Insn::new(
            address,
            consumed,
            name,
            format!("{target:#x}"),
            Some(target),
        );
    }
    // BR / BLR / RET.
    if bits(word, 31, 10) == 0b11_0101_1000_0111_1100_0000 && bits(word, 4, 0) == 0 {
        return ins(address, consumed, "br", xreg(bits(word, 9, 5), false));
    }
    if bits(word, 31, 10) == 0b11_0101_1000_1111_1100_0000 && bits(word, 4, 0) == 0 {
        return ins(address, consumed, "blr", xreg(bits(word, 9, 5), false));
    }
    if bits(word, 31, 10) == 0b11_0101_1001_0111_1100_0000 && bits(word, 4, 0) == 0 {
        return ins(address, consumed, "ret", xreg(bits(word, 9, 5), false));
    }
    // SVC / HVC / SMC / BRK / HLT.
    if bits(word, 31, 24) == 0b1101_0100 {
        let imm = bits(word, 20, 5);
        let name = match (bits(word, 23, 21), bits(word, 4, 0)) {
            (0b000, 0b00001) => "svc",
            (0b000, 0b00010) => "hvc",
            (0b000, 0b00011) => "smc",
            (0b001, 0b00000) => "brk",
            (0b010, 0b00000) => "hlt",
            _ => return unknown(word, address, consumed),
        };
        return ins(address, consumed, name, format!("#{imm:#x}"));
    }
    // HINT space (CRn=2): nop and friends, keyed by CRm:op2.
    if bits(word, 31, 12) == 0b1101_0101_0000_0011_0010 && bits(word, 4, 0) == 0b11111 {
        let number = bits(word, 11, 5);
        let name = match number {
            0 => "nop",
            1 => "yield",
            2 => "wfe",
            3 => "wfi",
            4 => "sev",
            5 => "sevl",
            _ => return ins(address, consumed, "hint", format!("#{number}")),
        };
        return ins(address, consumed, name, String::new());
    }
    unknown(word, address, consumed)
}

/// Decodes an A64 bitmask ("logical") immediate, per the architecture's
/// `DecodeBitMasks` pseudocode (Arm ARM, aslp `DecodeBitMasks`).
#[allow(clippy::similar_names)] // immr/imms are the architecture's own field names.
fn logical_imm(sf: u32, n: u32, immr: u32, imms: u32) -> Option<u64> {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = combined.ilog2();
    let size = 1u32 << len;
    if sf == 0 && size > 32 {
        return None;
    }
    let levels = size - 1;
    let s = imms & levels;
    let r = immr & levels;
    if s == levels {
        return None;
    }
    let element_ones = u64::from(s) + 1;
    let mut element = if element_ones >= 64 {
        u64::MAX
    } else {
        (1u64 << element_ones) - 1
    };
    if r != 0 {
        let size64 = u64::from(size);
        let rot = u64::from(r);
        element = ((element >> rot) | (element << (size64 - rot)))
            & if size >= 64 {
                u64::MAX
            } else {
                (1u64 << size64) - 1
            };
    }
    let mut value = 0u64;
    let mut filled = 0u32;
    while filled < 64 {
        value |= element << filled;
        filled += size;
    }
    if sf == 0 {
        value &= 0xffff_ffff;
    }
    Some(value)
}

/// The data-processing (immediate) group.
#[allow(clippy::too_many_lines)] // One exhaustive group match; splitting it would scatter the map.
#[allow(clippy::similar_names)] // immr/imms are the architecture's own field names.
fn data_processing_immediate(word: u32, address: u64, consumed: &[u8]) -> Insn {
    let sf = bits(word, 31, 31);
    let rd = bits(word, 4, 0);
    let rn = bits(word, 9, 5);
    match bits(word, 25, 23) {
        // PC-relative addressing.
        0b000 | 0b001 => {
            let imm_raw = u64::from((bits(word, 23, 5) << 2) | bits(word, 30, 29));
            let xd = xreg(rd, false);
            if bits(word, 31, 31) == 1 {
                let offset = sign_extend(imm_raw, 21) << 12;
                let target = branch_target(address & !0xfff, offset);
                Insn::new(
                    address,
                    consumed,
                    String::from("adrp"),
                    format!("{xd},{target:#x}"),
                    Some(target),
                )
            } else {
                let target = branch_target(address, sign_extend(imm_raw, 21));
                Insn::new(
                    address,
                    consumed,
                    String::from("adr"),
                    format!("{xd},{target:#x}"),
                    Some(target),
                )
            }
        }
        // Add/subtract immediate.
        0b010 => {
            let set_flags = bits(word, 29, 29) == 1;
            let name = match (bits(word, 30, 30), set_flags) {
                (0, false) => "add",
                (0, true) => "adds",
                (1, false) => "sub",
                (1, true) => "subs",
                _ => return unknown(word, address, consumed),
            };
            let imm = bits(word, 21, 10);
            let shift = if bits(word, 22, 22) == 1 {
                ",lsl #12"
            } else {
                ""
            };
            let dst = reg(sf, rd, !set_flags);
            let src = reg(sf, rn, true);
            ins(
                address,
                consumed,
                name,
                format!("{dst},{src},#{imm:#x}{shift}"),
            )
        }
        // Logical immediate.
        0b100 => {
            let name = match bits(word, 30, 29) {
                0b00 => "and",
                0b01 => "orr",
                0b10 => "eor",
                _ => "ands",
            };
            let n = bits(word, 22, 22);
            if sf == 0 && n == 1 {
                return unknown(word, address, consumed);
            }
            let Some(value) = logical_imm(sf, n, bits(word, 21, 16), bits(word, 15, 10)) else {
                return unknown(word, address, consumed);
            };
            let dst = reg(sf, rd, name != "ands");
            let src = reg(sf, rn, false);
            ins(address, consumed, name, format!("{dst},{src},#{value:#x}"))
        }
        // Move wide immediate.
        0b101 => {
            let name = match bits(word, 30, 29) {
                0b00 => "movn",
                0b10 => "movz",
                0b11 => "movk",
                _ => return unknown(word, address, consumed),
            };
            let hw = bits(word, 22, 21);
            if sf == 0 && hw > 1 {
                return unknown(word, address, consumed);
            }
            let imm = bits(word, 20, 5);
            let dst = reg(sf, rd, false);
            let shift = if hw == 0 {
                String::new()
            } else {
                format!(",lsl #{}", hw * 16)
            };
            ins(address, consumed, name, format!("{dst},#{imm:#x}{shift}"))
        }
        // Bitfield.
        0b110 => {
            let name = match bits(word, 30, 29) {
                0b00 => "sbfm",
                0b01 => "bfm",
                0b10 => "ubfm",
                _ => return unknown(word, address, consumed),
            };
            let n = bits(word, 22, 22);
            if n != sf {
                return unknown(word, address, consumed);
            }
            let immr = bits(word, 21, 16);
            let imms = bits(word, 15, 10);
            let dst = reg(sf, rd, false);
            let src = reg(sf, rn, false);
            ins(
                address,
                consumed,
                name,
                format!("{dst},{src},#{immr},#{imms}"),
            )
        }
        // Extract.
        0b111 => {
            if bits(word, 30, 29) != 0 || bits(word, 21, 21) != 0 || bits(word, 22, 22) != sf {
                return unknown(word, address, consumed);
            }
            let imms = bits(word, 15, 10);
            if sf == 0 && imms > 31 {
                return unknown(word, address, consumed);
            }
            let dst = reg(sf, rd, false);
            let src1 = reg(sf, rn, false);
            let src2 = reg(sf, bits(word, 20, 16), false);
            ins(
                address,
                consumed,
                "extr",
                format!("{dst},{src1},{src2},#{imms}"),
            )
        }
        _ => unknown(word, address, consumed),
    }
}

/// SIMD/FP register name for a load/store element size in bytes.
fn vreg(size_log2: u32, field: u32) -> String {
    let prefix = match size_log2 {
        0 => 'b',
        1 => 'h',
        2 => 's',
        3 => 'd',
        _ => 'q',
    };
    format!("{prefix}{}", field & 31)
}

/// Renders an addressing form: offset (`[rn,#i]`), pre (`[rn,#i]!`),
/// post (`[rn],#i`), or plain (`[rn]`) when the offset is zero.
fn address_form(rn: u32, offset: i64, mode: AddressMode) -> String {
    let base = xreg(rn, true);
    match mode {
        AddressMode::Offset if offset == 0 => format!("[{base}]"),
        AddressMode::Offset => format!("[{base},#{offset}]"),
        AddressMode::Pre => format!("[{base},#{offset}]!"),
        AddressMode::Post => format!("[{base}],#{offset}"),
    }
}

/// The three A64 immediate addressing modes.
#[derive(Copy, Clone)]
enum AddressMode {
    Offset,
    Pre,
    Post,
}

/// The loads-and-stores group.
#[allow(clippy::too_many_lines)] // One exhaustive form match; splitting it would scatter the map.
fn loads_and_stores(word: u32, address: u64, consumed: &[u8]) -> Insn {
    let size = bits(word, 31, 30);
    let v = bits(word, 26, 26);
    let opc = bits(word, 23, 22);
    let rt = bits(word, 4, 0);
    let rn = bits(word, 9, 5);

    // Load register (literal): opc x V 011 000 imm19.
    if bits(word, 29, 24) == 0b01_1000 {
        let offset = sign_extend(u64::from(bits(word, 23, 5)), 19) * 4;
        let dst = match (v, size) {
            (0, 0b00) => wreg(rt, false),
            (0, 0b01) => xreg(rt, false),
            (0, 0b10) => return jump(address, consumed, "ldrsw", &xreg(rt, false), offset),
            (1, 0b00) => vreg(2, rt),
            (1, 0b01) => vreg(3, rt),
            (1, 0b10) => vreg(4, rt),
            _ => return unknown(word, address, consumed),
        };
        return jump(address, consumed, "ldr", &dst, offset);
    }

    // Exclusive and acquire/release: size 001000 …
    if bits(word, 29, 24) == 0b00_1000 && v == 0 {
        let suffix = match size {
            0b00 => "b",
            0b01 => "h",
            _ => "",
        };
        let t = if size == 0b11 {
            xreg(rt, false)
        } else {
            wreg(rt, false)
        };
        let addr = format!("[{}]", xreg(rn, true));
        let o2 = bits(word, 23, 23);
        let load = bits(word, 22, 22) == 1;
        let o1 = bits(word, 21, 21);
        let o0 = bits(word, 15, 15);
        if o1 != 0 || bits(word, 14, 10) != 0b11111 {
            return unknown(word, address, consumed);
        }
        let ws = wreg(bits(word, 20, 16), false);
        let rs_clear = bits(word, 20, 16) == 0b11111;
        let (name, operands) = match (o2, load, o0) {
            (0, false, 0) => (format!("stxr{suffix}"), format!("{ws},{t},{addr}")),
            (0, false, 1) => (format!("stlxr{suffix}"), format!("{ws},{t},{addr}")),
            (0, true, 0) if rs_clear => (format!("ldxr{suffix}"), format!("{t},{addr}")),
            (0, true, 1) if rs_clear => (format!("ldaxr{suffix}"), format!("{t},{addr}")),
            (1, false, 1) if rs_clear => (format!("stlr{suffix}"), format!("{t},{addr}")),
            (1, true, 1) if rs_clear => (format!("ldar{suffix}"), format!("{t},{addr}")),
            _ => return unknown(word, address, consumed),
        };
        return Insn::new(address, consumed, name, operands, None);
    }

    // Register pair: opc V 101 x mode.
    if bits(word, 29, 27) == 0b101 {
        let mode_bits = bits(word, 24, 23);
        let load = bits(word, 22, 22) == 1;
        let (dst1, dst2, scale) = match (v, size) {
            (0, 0b00) => (wreg(rt, false), wreg(bits(word, 14, 10), false), 2u32),
            (0, 0b01) if load => (xreg(rt, false), xreg(bits(word, 14, 10), false), 2),
            (0, 0b10) => (xreg(rt, false), xreg(bits(word, 14, 10), false), 3),
            (1, 0b00) => (vreg(2, rt), vreg(2, bits(word, 14, 10)), 2),
            (1, 0b01) => (vreg(3, rt), vreg(3, bits(word, 14, 10)), 3),
            (1, 0b10) => (vreg(4, rt), vreg(4, bits(word, 14, 10)), 4),
            _ => return unknown(word, address, consumed),
        };
        let base = if v == 0 && size == 0b01 && load {
            "ldpsw"
        } else {
            match (mode_bits, load) {
                (0b00, false) => "stnp",
                (0b00, true) => "ldnp",
                (_, false) => "stp",
                (_, true) => "ldp",
            }
        };
        let mode = match mode_bits {
            0b01 => AddressMode::Post,
            0b10 | 0b00 => AddressMode::Offset,
            0b11 => AddressMode::Pre,
            _ => return unknown(word, address, consumed),
        };
        let offset = sign_extend(u64::from(bits(word, 21, 15)), 7) << scale;
        let addr = address_form(rn, offset, mode);
        return Insn::new(
            address,
            consumed,
            String::from(base),
            format!("{dst1},{dst2},{addr}"),
            None,
        );
    }

    // Single register forms: size V 111 …
    if bits(word, 29, 27) != 0b111 {
        return unknown(word, address, consumed);
    }
    let Some((name, dst, scale)) = single_register_name(size, v, opc, rt) else {
        return unknown(word, address, consumed);
    };

    if bits(word, 24, 24) == 1 {
        // Unsigned scaled 12-bit offset.
        let offset = i64::from(bits(word, 21, 10)) << scale;
        let addr = address_form(rn, offset, AddressMode::Offset);
        return Insn::new(address, consumed, name, format!("{dst},{addr}"), None);
    }
    let imm9 = sign_extend(u64::from(bits(word, 20, 12)), 9);
    match (bits(word, 21, 21), bits(word, 11, 10)) {
        (0, 0b00) => {
            // Unscaled: the name gains a `u` after the base op (stur/ldur).
            let unscaled = name.replacen("str", "stur", 1).replacen("ldr", "ldur", 1);
            let addr = address_form(rn, imm9, AddressMode::Offset);
            Insn::new(address, consumed, unscaled, format!("{dst},{addr}"), None)
        }
        (0, 0b01) => {
            let addr = address_form(rn, imm9, AddressMode::Post);
            Insn::new(address, consumed, name, format!("{dst},{addr}"), None)
        }
        (0, 0b11) => {
            let addr = address_form(rn, imm9, AddressMode::Pre);
            Insn::new(address, consumed, name, format!("{dst},{addr}"), None)
        }
        (1, 0b10) => {
            // Register offset.
            let option = bits(word, 15, 13);
            let rm = bits(word, 20, 16);
            let index = match option {
                0b011 | 0b111 => xreg(rm, false),
                0b010 | 0b110 => wreg(rm, false),
                _ => return unknown(word, address, consumed),
            };
            let extend = match option {
                0b010 => ",uxtw",
                0b011 => "",
                0b110 => ",sxtw",
                _ => ",sxtx",
            };
            let amount = if bits(word, 12, 12) == 1 && scale > 0 {
                format!(" #{scale}")
            } else {
                String::new()
            };
            let addr = format!("[{},{index}{extend}{amount}]", xreg(rn, true));
            Insn::new(address, consumed, name, format!("{dst},{addr}"), None)
        }
        _ => unknown(word, address, consumed),
    }
}

/// Name, destination register, and offset scale for the single-register
/// load/store forms, when the encoding is allocated.
fn single_register_name(size: u32, v: u32, opc: u32, rt: u32) -> Option<(String, String, u32)> {
    if v == 1 {
        let scale = if size == 0b00 && (opc & 0b10) != 0 {
            4
        } else {
            size
        };
        let name = if opc & 1 == 1 { "ldr" } else { "str" };
        if opc > 0b01 && size != 0b00 {
            return None;
        }
        return Some((String::from(name), vreg(scale, rt), scale));
    }
    let (name, wide) = match (size, opc) {
        (0b00, 0b00) => ("strb", false),
        (0b00, 0b01) => ("ldrb", false),
        (0b00, 0b10) => ("ldrsb", true),
        (0b00, 0b11) => ("ldrsb", false),
        (0b01, 0b00) => ("strh", false),
        (0b01, 0b01) => ("ldrh", false),
        (0b01, 0b10) => ("ldrsh", true),
        (0b01, 0b11) => ("ldrsh", false),
        (0b10, 0b00) => ("str", false),
        (0b10, 0b01) => ("ldr", false),
        (0b10, 0b10) => ("ldrsw", true),
        (0b11, 0b00) => ("str", true),
        (0b11, 0b01) => ("ldr", true),
        _ => return None,
    };
    let dst = if wide {
        xreg(rt, false)
    } else {
        wreg(rt, false)
    };
    Some((String::from(name), dst, size))
}

/// Shift-type name for a 2-bit shift field.
fn shift_name(field: u32) -> &'static str {
    ["lsl", "lsr", "asr", "ror"][usize::try_from(field & 3).unwrap_or(0)]
}

/// The data-processing (register) group.
#[allow(clippy::too_many_lines)] // One exhaustive family match; splitting it would scatter the map.
fn data_processing_register(word: u32, address: u64, consumed: &[u8]) -> Insn {
    let sf = bits(word, 31, 31);
    let rd = bits(word, 4, 0);
    let rn = bits(word, 9, 5);
    let rm = bits(word, 20, 16);

    // Logical (shifted register).
    if bits(word, 28, 24) == 0b0_1010 {
        let name = match (bits(word, 30, 29), bits(word, 21, 21)) {
            (0b00, 0) => "and",
            (0b00, 1) => "bic",
            (0b01, 0) => "orr",
            (0b01, 1) => "orn",
            (0b10, 0) => "eor",
            (0b10, 1) => "eon",
            (0b11, 0) => "ands",
            _ => "bics",
        };
        let amount = bits(word, 15, 10);
        if sf == 0 && amount > 31 {
            return unknown(word, address, consumed);
        }
        let shift = if amount == 0 && bits(word, 23, 22) == 0 {
            String::new()
        } else {
            format!(",{} #{amount}", shift_name(bits(word, 23, 22)))
        };
        let text = format!(
            "{},{},{}{shift}",
            reg(sf, rd, false),
            reg(sf, rn, false),
            reg(sf, rm, false)
        );
        return ins(address, consumed, name, text);
    }

    // Add/subtract (shifted or extended register).
    if bits(word, 28, 24) == 0b0_1011 {
        let set_flags = bits(word, 29, 29) == 1;
        let name = match (bits(word, 30, 30), set_flags) {
            (0, false) => "add",
            (0, true) => "adds",
            (1, false) => "sub",
            _ => "subs",
        };
        if bits(word, 21, 21) == 1 {
            // Extended register.
            if bits(word, 23, 22) != 0 {
                return unknown(word, address, consumed);
            }
            let option = bits(word, 15, 13);
            let amount = bits(word, 12, 10);
            if amount > 4 {
                return unknown(word, address, consumed);
            }
            let ext = [
                "uxtb", "uxth", "uxtw", "uxtx", "sxtb", "sxth", "sxtw", "sxtx",
            ][usize::try_from(option).unwrap_or(0)];
            let m = if option & 0b011 == 0b011 {
                reg(sf, rm, false)
            } else {
                wreg(rm, false)
            };
            let suffix = if amount == 0 {
                format!(",{ext}")
            } else {
                format!(",{ext} #{amount}")
            };
            let text = format!(
                "{},{},{m}{suffix}",
                reg(sf, rd, !set_flags),
                reg(sf, rn, true)
            );
            return ins(address, consumed, name, text);
        }
        // Shifted register (shift type 0b11 is reserved).
        if bits(word, 23, 22) == 0b11 {
            return unknown(word, address, consumed);
        }
        let amount = bits(word, 15, 10);
        if sf == 0 && amount > 31 {
            return unknown(word, address, consumed);
        }
        let shift = if amount == 0 && bits(word, 23, 22) == 0 {
            String::new()
        } else {
            format!(",{} #{amount}", shift_name(bits(word, 23, 22)))
        };
        let text = format!(
            "{},{},{}{shift}",
            reg(sf, rd, false),
            reg(sf, rn, false),
            reg(sf, rm, false)
        );
        return ins(address, consumed, name, text);
    }

    // ADC/SBC.
    if bits(word, 28, 21) == 0b1101_0000 && bits(word, 15, 10) == 0 {
        let name = match (bits(word, 30, 30), bits(word, 29, 29)) {
            (0, 0) => "adc",
            (0, 1) => "adcs",
            (1, 0) => "sbc",
            _ => "sbcs",
        };
        let text = format!(
            "{},{},{}",
            reg(sf, rd, false),
            reg(sf, rn, false),
            reg(sf, rm, false)
        );
        return ins(address, consumed, name, text);
    }

    // Conditional compare (register or immediate).
    if bits(word, 28, 21) == 0b1101_0010
        && bits(word, 29, 29) == 1
        && bits(word, 10, 10) == 0
        && bits(word, 4, 4) == 0
    {
        let name = if bits(word, 30, 30) == 1 {
            "ccmp"
        } else {
            "ccmn"
        };
        let nzcv = bits(word, 3, 0);
        let second = if bits(word, 11, 11) == 1 {
            format!("#{rm:#x}")
        } else {
            reg(sf, rm, false)
        };
        let text = format!(
            "{},{second},#{nzcv:#x},{}",
            reg(sf, rn, false),
            cond(bits(word, 15, 12))
        );
        return ins(address, consumed, name, text);
    }

    // Conditional select.
    if bits(word, 28, 21) == 0b1101_0100 && bits(word, 29, 29) == 0 {
        let name = match (bits(word, 30, 30), bits(word, 11, 10)) {
            (0, 0b00) => "csel",
            (0, 0b01) => "csinc",
            (1, 0b00) => "csinv",
            (1, 0b01) => "csneg",
            _ => return unknown(word, address, consumed),
        };
        let text = format!(
            "{},{},{},{}",
            reg(sf, rd, false),
            reg(sf, rn, false),
            reg(sf, rm, false),
            cond(bits(word, 15, 12))
        );
        return ins(address, consumed, name, text);
    }

    // Data-processing 1-source and 2-source.
    if bits(word, 28, 21) == 0b1101_0110 && bits(word, 29, 29) == 0 {
        if bits(word, 30, 30) == 1 {
            if rm != 0 {
                return unknown(word, address, consumed);
            }
            let name = match (bits(word, 15, 10), sf) {
                (0b00_0000, _) => "rbit",
                (0b00_0001, _) => "rev16",
                (0b00_0010, 0) | (0b00_0011, 1) => "rev",
                (0b00_0010, _) => "rev32",
                (0b00_0100, _) => "clz",
                (0b00_0101, _) => "cls",
                _ => return unknown(word, address, consumed),
            };
            let text = format!("{},{}", reg(sf, rd, false), reg(sf, rn, false));
            return ins(address, consumed, name, text);
        }
        let name = match bits(word, 15, 10) {
            0b00_0010 => "udiv",
            0b00_0011 => "sdiv",
            0b00_1000 => "lslv",
            0b00_1001 => "lsrv",
            0b00_1010 => "asrv",
            0b00_1011 => "rorv",
            _ => return unknown(word, address, consumed),
        };
        let text = format!(
            "{},{},{}",
            reg(sf, rd, false),
            reg(sf, rn, false),
            reg(sf, rm, false)
        );
        return ins(address, consumed, name, text);
    }

    // Data-processing 3-source.
    if bits(word, 28, 24) == 0b1_1011 && bits(word, 30, 29) == 0 {
        let ra = bits(word, 14, 10);
        let o0 = bits(word, 15, 15);
        let (name, long) = match (bits(word, 23, 21), o0) {
            (0b000, 0) => ("madd", false),
            (0b000, 1) => ("msub", false),
            (0b001, 0) => ("smaddl", true),
            (0b001, 1) => ("smsubl", true),
            (0b010, 0) => ("smulh", false),
            (0b101, 0) => ("umaddl", true),
            (0b101, 1) => ("umsubl", true),
            (0b110, 0) => ("umulh", false),
            _ => return unknown(word, address, consumed),
        };
        if (long || name.ends_with("ulh")) && sf == 0 {
            return unknown(word, address, consumed);
        }
        if name.ends_with("ulh") {
            if ra != 0b11111 {
                return unknown(word, address, consumed);
            }
            let text = format!(
                "{},{},{}",
                xreg(rd, false),
                xreg(rn, false),
                xreg(rm, false)
            );
            return ins(address, consumed, name, text);
        }
        let (n, m) = if long {
            (wreg(rn, false), wreg(rm, false))
        } else {
            (reg(sf, rn, false), reg(sf, rm, false))
        };
        let text = format!("{},{n},{m},{}", reg(sf, rd, false), reg(sf, ra, false));
        return ins(address, consumed, name, text);
    }

    unknown(word, address, consumed)
}

#[cfg(test)]
#[path = "aarch64_tests.rs"]
mod tests;
