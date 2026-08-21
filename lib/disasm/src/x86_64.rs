//! x86_64 instruction decoder.
//!
//! The variable-length decoder: legacy prefixes, REX, opcode (one-byte map
//! and the `0F` two-byte map), ModRM/SIB, displacement, and immediate
//! sizing. Length discipline is the core property — an instruction's
//! length must be exact or every following instruction decodes as
//! garbage — so every opcode the tables name carries its precise operand
//! sizing, a legal instruction is capped at 15 bytes, and an undecodable
//! byte renders as a `(bad)` single byte so the stream resynchronises
//! exactly as binutils does. Three-byte maps (`0F 38` / `0F 3A`) and
//! VEX/EVEX are rendered as `(bad)` for now; extending the maps is staged
//! in `plans/APPS.md`.
//!
//! Rendering is Intel syntax as binutils prints it (`mov rax,QWORD PTR
//! [rbp-0x8]`), with two documented simplifications: RIP-relative operands
//! are shown as written (`[rip+0x10]`) without the resolved comment, and a
//! segment override prefixes the bracket (`fs:[rax]`).

use alloc::format;
use alloc::string::String;

use crate::{branch_target, sign_extend, Insn};

/// A legal instruction never exceeds 15 bytes; longer prefix runs fault.
const MAX_LENGTH: usize = 15;

/// 64-bit register names, indexed by the 4-bit REX-extended number.
const REG64: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];

/// 32-bit register names.
const REG32: [&str; 16] = [
    "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d",
    "r13d", "r14d", "r15d",
];

/// 16-bit register names.
const REG16: [&str; 16] = [
    "ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w",
    "r14w", "r15w",
];

/// 8-bit register names when a REX prefix is present.
const REG8_REX: [&str; 16] = [
    "al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b",
    "r13b", "r14b", "r15b",
];

/// 8-bit register names without a REX prefix (the high-byte legacy set).
const REG8: [&str; 8] = ["al", "cl", "dl", "bl", "ah", "ch", "dh", "bh"];

/// Operand width of one register or memory operand.
#[derive(Copy, Clone, Eq, PartialEq)]
enum Width {
    Byte,
    Word,
    Dword,
    Qword,
}

impl Width {
    /// The Intel-syntax memory-operand size keyword.
    fn ptr(self) -> &'static str {
        match self {
            Self::Byte => "BYTE PTR ",
            Self::Word => "WORD PTR ",
            Self::Dword => "DWORD PTR ",
            Self::Qword => "QWORD PTR ",
        }
    }
}

/// Register name for `number` at `width` (`rex` selects the 8-bit set).
fn reg_name(width: Width, number: u32, rex: bool) -> &'static str {
    let index = usize::try_from(number & 15).unwrap_or(0);
    match width {
        Width::Byte => {
            if rex {
                REG8_REX[index]
            } else {
                REG8[index & 7]
            }
        }
        Width::Word => REG16[index],
        Width::Dword => REG32[index],
        Width::Qword => REG64[index],
    }
}

/// The legacy and REX prefixes gathered before the opcode.
#[derive(Default)]
struct Prefixes {
    /// `66`: operand size becomes 16-bit.
    operand16: bool,
    /// `67`: address size becomes 32-bit.
    address32: bool,
    /// `F0`.
    lock: bool,
    /// `F2` / `F3`.
    rep: Option<&'static str>,
    /// `26/2E/36/3E/64/65` segment override.
    segment: Option<&'static str>,
    /// The REX byte, when present.
    rex: Option<u8>,
}

impl Prefixes {
    /// True when REX.W selects 64-bit operands.
    fn rex_w(&self) -> bool {
        self.rex.is_some_and(|byte| byte & 0b1000 != 0)
    }

    /// REX.R extension for the `ModRM` `reg` field.
    fn rex_r(&self) -> u32 {
        u32::from(self.rex.is_some_and(|byte| byte & 0b0100 != 0))
    }

    /// REX.X extension for the SIB `index` field.
    fn rex_x(&self) -> u32 {
        u32::from(self.rex.is_some_and(|byte| byte & 0b0010 != 0))
    }

    /// REX.B extension for `rm`, SIB `base`, and short register numbers.
    fn rex_b(&self) -> u32 {
        u32::from(self.rex.is_some_and(|byte| byte & 0b0001 != 0))
    }

    /// The operand width selected by `66`/REX.W for a non-byte operand.
    fn operand_width(&self) -> Width {
        if self.rex_w() {
            Width::Qword
        } else if self.operand16 {
            Width::Word
        } else {
            Width::Dword
        }
    }
}

/// A bounds-checked byte cursor over the instruction bytes.
struct Reader<'a> {
    code: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(code: &'a [u8]) -> Self {
        Self { code, pos: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let byte = *self.code.get(self.pos)?;
        self.pos += 1;
        Some(byte)
    }

    fn u16(&mut self) -> Option<u16> {
        let raw = self.code.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(u16::from_le_bytes([raw[0], raw[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let raw = self.code.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn u64(&mut self) -> Option<u64> {
        let raw = self.code.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(raw);
        Some(u64::from_le_bytes(bytes))
    }

    /// A sign-extended 8-bit immediate/displacement.
    fn i8(&mut self) -> Option<i64> {
        self.u8().map(|byte| sign_extend(u64::from(byte), 8))
    }

    /// A sign-extended 32-bit immediate/displacement.
    fn i32(&mut self) -> Option<i64> {
        self.u32().map(|word| sign_extend(u64::from(word), 32))
    }
}

/// A decoded `ModRM` `r/m` operand: a register number or memory text.
enum Rm {
    Reg(u32),
    /// The bracketed effective-address text, without the size keyword.
    Mem(String),
}

/// Signed displacement rendered in binutils style (`+0x8` / `-0x8`).
fn disp_text(disp: i64) -> String {
    if disp < 0 {
        format!("-{:#x}", disp.unsigned_abs())
    } else {
        format!("+{disp:#x}")
    }
}

/// Decodes `ModRM` (and any SIB/displacement): `(reg field, r/m operand)`.
fn modrm(reader: &mut Reader<'_>, prefixes: &Prefixes) -> Option<(u32, Rm)> {
    let byte = reader.u8()?;
    let mode = u32::from(byte >> 6);
    let reg = u32::from((byte >> 3) & 7) | (prefixes.rex_r() << 3);
    let rm_low = u32::from(byte & 7);

    if mode == 3 {
        return Some((reg, Rm::Reg(rm_low | (prefixes.rex_b() << 3))));
    }

    // Address-size 67 selects the 32-bit register set for bases/indexes.
    let base_width = if prefixes.address32 {
        Width::Dword
    } else {
        Width::Qword
    };
    let segment = prefixes.segment.map_or("", |name| name);

    // RIP-relative: mod=00, rm=101.
    if mode == 0 && rm_low == 5 {
        let disp = reader.i32()?;
        return Some((reg, Rm::Mem(format!("{segment}[rip{}]", disp_text(disp)))));
    }

    let (base, index) = if rm_low == 4 {
        // SIB byte follows.
        let sib = reader.u8()?;
        let scale = 1u32 << (sib >> 6);
        let index_number = u32::from((sib >> 3) & 7) | (prefixes.rex_x() << 3);
        let base_number = u32::from(sib & 7) | (prefixes.rex_b() << 3);
        // Index 100 (rsp) means "no index".
        let index = if index_number == 4 {
            String::new()
        } else {
            format!("{}*{scale}", reg_name(base_width, index_number, true))
        };
        // Base 101 with mod=00 means "disp32 only".
        if mode == 0 && (base_number & 7) == 5 {
            let disp = reader.i32()?;
            let inner = if index.is_empty() {
                format!("{disp:#x}")
            } else {
                format!("{index}{}", disp_text(disp))
            };
            return Some((reg, Rm::Mem(format!("{segment}[{inner}]"))));
        }
        (String::from(reg_name(base_width, base_number, true)), index)
    } else {
        (
            String::from(reg_name(base_width, rm_low | (prefixes.rex_b() << 3), true)),
            String::new(),
        )
    };

    let disp = match mode {
        1 => reader.i8()?,
        2 => reader.i32()?,
        _ => 0,
    };
    let mut inner = base;
    if !index.is_empty() {
        inner = format!("{inner}+{index}");
    }
    if disp != 0 || mode != 0 {
        inner = format!("{inner}{}", disp_text(disp));
    }
    Some((reg, Rm::Mem(format!("{segment}[{inner}]"))))
}

/// Renders an `r/m` operand at `width`.
fn rm_text(rm: &Rm, width: Width, prefixes: &Prefixes) -> String {
    match rm {
        Rm::Reg(number) => String::from(reg_name(width, *number, prefixes.rex.is_some())),
        Rm::Mem(inner) => format!("{}{inner}", width.ptr()),
    }
}

/// Decodes one instruction at `address` from the front of `code`.
///
/// Returns `None` only for an empty slice. Any other input yields an
/// instruction consuming at least one byte; an undecodable or truncated
/// encoding, or one exceeding the 15-byte limit, renders as `(bad)` over a
/// single byte so the walk resynchronises like binutils.
#[must_use]
pub fn decode(code: &[u8], address: u64) -> Option<Insn> {
    if code.is_empty() {
        return None;
    }
    let window = &code[..code.len().min(MAX_LENGTH)];
    let mut reader = Reader::new(window);
    let mut prefixes = Prefixes::default();
    loop {
        let Some(byte) = reader.u8() else {
            return Some(Insn::bad(address, &code[..1]));
        };
        match byte {
            0x66 => prefixes.operand16 = true,
            0x67 => prefixes.address32 = true,
            0xf0 => prefixes.lock = true,
            0xf2 => prefixes.rep = Some("repne "),
            0xf3 => prefixes.rep = Some("rep "),
            0x26 => prefixes.segment = Some("es:"),
            0x2e => prefixes.segment = Some("cs:"),
            0x36 => prefixes.segment = Some("ss:"),
            0x3e => prefixes.segment = Some("ds:"),
            0x64 => prefixes.segment = Some("fs:"),
            0x65 => prefixes.segment = Some("gs:"),
            0x40..=0x4f => {
                // REX must immediately precede the opcode.
                prefixes.rex = Some(byte);
                let insn = opcode(&mut reader, &prefixes, address, window);
                return Some(insn.unwrap_or_else(|| Insn::bad(address, &code[..1])));
            }
            _ => {
                reader.pos -= 1;
                let insn = opcode(&mut reader, &prefixes, address, window);
                return Some(insn.unwrap_or_else(|| Insn::bad(address, &code[..1])));
            }
        }
    }
}

/// Builds the final [`Insn`] once the reader holds the full length.
fn finish(
    reader: &Reader<'_>,
    prefixes: &Prefixes,
    address: u64,
    window: &[u8],
    mnemonic: &str,
    operands: String,
    target: Option<u64>,
) -> Insn {
    let lock = if prefixes.lock { "lock " } else { "" };
    let rep = prefixes.rep.unwrap_or("");
    let name = format!("{lock}{rep}{mnemonic}");
    Insn::new(address, &window[..reader.pos], name, operands, target)
}

/// Condition-code suffixes, indexed by the low opcode nibble.
const CC: [&str; 16] = [
    "o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g",
];

/// An immediate, masked and rendered at its operand width (binutils
/// Intel style: unsigned hexadecimal at the destination width).
fn imm_hex(value: i64, width: Width) -> String {
    let raw = u64::from_le_bytes(value.to_le_bytes());
    match width {
        Width::Byte => format!("{:#x}", raw & 0xff),
        Width::Word => format!("{:#x}", raw & 0xffff),
        Width::Dword => format!("{:#x}", raw & 0xffff_ffff),
        Width::Qword => format!("{raw:#x}"),
    }
}

/// The eight arithmetic-block mnemonics (opcodes `00`–`3D`).
const ARITH: [&str; 8] = ["add", "or", "adc", "sbb", "and", "sub", "xor", "cmp"];

/// Group-1 (`80/81/83`) mnemonics share the arithmetic table; group-2
/// (`C0/C1/D0–D3`) shifts and group-3 (`F6/F7`) unary ops have their own.
const SHIFT: [&str; 8] = ["rol", "ror", "rcl", "rcr", "shl", "shr", "shl", "sar"];
const UNARY: [&str; 8] = ["test", "test", "not", "neg", "mul", "imul", "div", "idiv"];

/// Decodes the opcode and operands after the prefixes.
#[allow(clippy::too_many_lines)] // One exhaustive opcode map; splitting it would scatter the table.
fn opcode(
    reader: &mut Reader<'_>,
    prefixes: &Prefixes,
    address: u64,
    window: &[u8],
) -> Option<Insn> {
    let op = reader.u8()?;
    let wide = prefixes.operand_width();
    let rexed = prefixes.rex.is_some();

    // A relative branch: the target is relative to the next instruction.
    let relative = |reader: &mut Reader<'_>, name: &str, byte_rel: bool| -> Option<Insn> {
        let rel = if byte_rel {
            reader.i8()?
        } else {
            reader.i32()?
        };
        let end = u64::try_from(reader.pos).ok()?;
        let target = branch_target(address.wrapping_add(end), rel);
        Some(finish(
            reader,
            prefixes,
            address,
            window,
            name,
            format!("{target:#x}"),
            Some(target),
        ))
    };

    match op {
        0x0f => two_byte(reader, prefixes, address, window),
        // The arithmetic block: add/or/adc/sbb/and/sub/xor/cmp × 6 forms.
        0x00..=0x3d if (op & 7) <= 5 => {
            let name = ARITH[usize::from(op >> 3) & 7];
            let operands = match op & 7 {
                0 | 1 => {
                    let width = if op & 1 == 0 { Width::Byte } else { wide };
                    let (reg, rm) = modrm(reader, prefixes)?;
                    format!(
                        "{},{}",
                        rm_text(&rm, width, prefixes),
                        reg_name(width, reg, rexed)
                    )
                }
                2 | 3 => {
                    let width = if op & 1 == 0 { Width::Byte } else { wide };
                    let (reg, rm) = modrm(reader, prefixes)?;
                    format!(
                        "{},{}",
                        reg_name(width, reg, rexed),
                        rm_text(&rm, width, prefixes)
                    )
                }
                4 => format!("al,{}", imm_hex(reader.i8()?, Width::Byte)),
                _ => {
                    let imm = if prefixes.operand16 {
                        i64::from(reader.u16()?)
                    } else {
                        reader.i32()?
                    };
                    format!("{},{}", reg_name(wide, 0, rexed), imm_hex(imm, wide))
                }
            };
            Some(finish(
                reader, prefixes, address, window, name, operands, None,
            ))
        }
        // push/pop r64 (66 selects the 16-bit form).
        0x50..=0x5f => {
            let name = if op < 0x58 { "push" } else { "pop" };
            let number = u32::from(op & 7) | (prefixes.rex_b() << 3);
            let width = if prefixes.operand16 {
                Width::Word
            } else {
                Width::Qword
            };
            let text = String::from(reg_name(width, number, true));
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        0x63 => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let text = format!(
                "{},{}",
                reg_name(wide, reg, rexed),
                rm_text(&rm, Width::Dword, prefixes)
            );
            Some(finish(
                reader, prefixes, address, window, "movsxd", text, None,
            ))
        }
        0x68 => {
            let imm = reader.i32()?;
            let text = imm_hex(imm, Width::Qword);
            Some(finish(
                reader, prefixes, address, window, "push", text, None,
            ))
        }
        0x69 | 0x6b => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let imm = if op == 0x6b {
                reader.i8()?
            } else {
                reader.i32()?
            };
            let text = format!(
                "{},{},{}",
                reg_name(wide, reg, rexed),
                rm_text(&rm, wide, prefixes),
                imm_hex(imm, wide)
            );
            Some(finish(
                reader, prefixes, address, window, "imul", text, None,
            ))
        }
        0x6a => {
            let imm = reader.i8()?;
            let text = imm_hex(imm, Width::Qword);
            Some(finish(
                reader, prefixes, address, window, "push", text, None,
            ))
        }
        // Jcc rel8.
        0x70..=0x7f => {
            let name = format!("j{}", CC[usize::from(op & 15)]);
            relative(reader, &name, true)
        }
        // Group 1: arithmetic with immediate.
        0x80 | 0x81 | 0x83 => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let name = ARITH[usize::try_from(reg & 7).unwrap_or(0)];
            let width = if op == 0x80 { Width::Byte } else { wide };
            let imm = match op {
                0x81 if prefixes.operand16 => i64::from(reader.u16()?),
                0x81 => reader.i32()?,
                _ => reader.i8()?,
            };
            let text = format!("{},{}", rm_text(&rm, width, prefixes), imm_hex(imm, width));
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        // test / xchg r/m,r.
        0x84..=0x87 => {
            let name = if op <= 0x85 { "test" } else { "xchg" };
            let width = if op & 1 == 0 { Width::Byte } else { wide };
            let (reg, rm) = modrm(reader, prefixes)?;
            let text = format!(
                "{},{}",
                rm_text(&rm, width, prefixes),
                reg_name(width, reg, rexed)
            );
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        // mov.
        0x88..=0x8b => {
            let width = if op & 1 == 0 { Width::Byte } else { wide };
            let (reg, rm) = modrm(reader, prefixes)?;
            let text = if op < 0x8a {
                format!(
                    "{},{}",
                    rm_text(&rm, width, prefixes),
                    reg_name(width, reg, rexed)
                )
            } else {
                format!(
                    "{},{}",
                    reg_name(width, reg, rexed),
                    rm_text(&rm, width, prefixes)
                )
            };
            Some(finish(reader, prefixes, address, window, "mov", text, None))
        }
        0x8d => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let Rm::Mem(inner) = rm else {
                return None;
            };
            let text = format!("{},{inner}", reg_name(wide, reg, rexed));
            Some(finish(reader, prefixes, address, window, "lea", text, None))
        }
        0x8f => {
            let (reg, rm) = modrm(reader, prefixes)?;
            if reg & 7 != 0 {
                return None;
            }
            let text = rm_text(&rm, Width::Qword, prefixes);
            Some(finish(reader, prefixes, address, window, "pop", text, None))
        }
        0x90 => {
            if prefixes.rex_b() == 1 {
                let text = format!("{},{}", reg_name(wide, 8, true), reg_name(wide, 0, true));
                Some(finish(
                    reader, prefixes, address, window, "xchg", text, None,
                ))
            } else if prefixes.operand16 {
                Some(finish(
                    reader,
                    prefixes,
                    address,
                    window,
                    "xchg",
                    String::from("ax,ax"),
                    None,
                ))
            } else {
                Some(finish(
                    reader,
                    prefixes,
                    address,
                    window,
                    "nop",
                    String::new(),
                    None,
                ))
            }
        }
        // xchg rAX,r.
        0x91..=0x97 => {
            let number = u32::from(op & 7) | (prefixes.rex_b() << 3);
            let text = format!(
                "{},{}",
                reg_name(wide, 0, rexed),
                reg_name(wide, number, true)
            );
            Some(finish(
                reader, prefixes, address, window, "xchg", text, None,
            ))
        }
        0x98 => {
            let name = if prefixes.rex_w() {
                "cdqe"
            } else if prefixes.operand16 {
                "cbw"
            } else {
                "cwde"
            };
            Some(finish(
                reader,
                prefixes,
                address,
                window,
                name,
                String::new(),
                None,
            ))
        }
        0x99 => {
            let name = if prefixes.rex_w() {
                "cqo"
            } else if prefixes.operand16 {
                "cwd"
            } else {
                "cdq"
            };
            Some(finish(
                reader,
                prefixes,
                address,
                window,
                name,
                String::new(),
                None,
            ))
        }
        0x9c => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "pushf",
            String::new(),
            None,
        )),
        0x9d => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "popf",
            String::new(),
            None,
        )),
        0xa8 => {
            let text = format!("al,{}", imm_hex(reader.i8()?, Width::Byte));
            Some(finish(
                reader, prefixes, address, window, "test", text, None,
            ))
        }
        0xa9 => {
            let imm = if prefixes.operand16 {
                i64::from(reader.u16()?)
            } else {
                reader.i32()?
            };
            let text = format!("{},{}", reg_name(wide, 0, rexed), imm_hex(imm, wide));
            Some(finish(
                reader, prefixes, address, window, "test", text, None,
            ))
        }
        // mov r8,imm8.
        0xb0..=0xb7 => {
            let number = u32::from(op & 7) | (prefixes.rex_b() << 3);
            let imm = reader.i8()?;
            let text = format!(
                "{},{}",
                reg_name(Width::Byte, number, rexed),
                imm_hex(imm, Width::Byte)
            );
            Some(finish(reader, prefixes, address, window, "mov", text, None))
        }
        // mov r,imm (REX.W selects the 64-bit movabs form).
        0xb8..=0xbf => {
            let number = u32::from(op & 7) | (prefixes.rex_b() << 3);
            if prefixes.rex_w() {
                let imm = reader.u64()?;
                let text = format!("{},{imm:#x}", reg_name(Width::Qword, number, true));
                return Some(finish(
                    reader, prefixes, address, window, "movabs", text, None,
                ));
            }
            let (imm, width) = if prefixes.operand16 {
                (i64::from(reader.u16()?), Width::Word)
            } else {
                (i64::from(reader.u32()?), Width::Dword)
            };
            let text = format!("{},{}", reg_name(width, number, rexed), imm_hex(imm, width));
            Some(finish(reader, prefixes, address, window, "mov", text, None))
        }
        // Group 2 shifts.
        0xc0 | 0xc1 | 0xd0 | 0xd1 | 0xd2 | 0xd3 => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let name = SHIFT[usize::try_from(reg & 7).unwrap_or(0)];
            let width = if op & 1 == 0 { Width::Byte } else { wide };
            let count = match op {
                0xc0 | 0xc1 => imm_hex(reader.i8()?, Width::Byte),
                0xd0 | 0xd1 => String::from("1"),
                _ => String::from("cl"),
            };
            let text = format!("{},{count}", rm_text(&rm, width, prefixes));
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        0xc2 => {
            let imm = reader.u16()?;
            Some(finish(
                reader,
                prefixes,
                address,
                window,
                "ret",
                format!("{imm:#x}"),
                None,
            ))
        }
        0xc3 => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "ret",
            String::new(),
            None,
        )),
        // Group 11: mov r/m,imm.
        0xc6 | 0xc7 => {
            let (reg, rm) = modrm(reader, prefixes)?;
            if reg & 7 != 0 {
                return None;
            }
            let width = if op == 0xc6 { Width::Byte } else { wide };
            let imm = match width {
                Width::Byte => reader.i8()?,
                Width::Word => i64::from(reader.u16()?),
                _ => reader.i32()?,
            };
            let text = format!("{},{}", rm_text(&rm, width, prefixes), imm_hex(imm, width));
            Some(finish(reader, prefixes, address, window, "mov", text, None))
        }
        0xc9 => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "leave",
            String::new(),
            None,
        )),
        0xcc => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "int3",
            String::new(),
            None,
        )),
        0xcd => {
            let imm = reader.u8()?;
            Some(finish(
                reader,
                prefixes,
                address,
                window,
                "int",
                format!("{imm:#x}"),
                None,
            ))
        }
        0xe8 => relative(reader, "call", false),
        0xe9 => relative(reader, "jmp", false),
        0xeb => relative(reader, "jmp", true),
        0xf4 => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "hlt",
            String::new(),
            None,
        )),
        // Group 3: unary ops and test.
        0xf6 | 0xf7 => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let name = UNARY[usize::try_from(reg & 7).unwrap_or(0)];
            let width = if op == 0xf6 { Width::Byte } else { wide };
            let text = if reg & 7 <= 1 {
                let imm = match width {
                    Width::Byte => reader.i8()?,
                    Width::Word => i64::from(reader.u16()?),
                    _ => reader.i32()?,
                };
                format!("{},{}", rm_text(&rm, width, prefixes), imm_hex(imm, width))
            } else {
                rm_text(&rm, width, prefixes)
            };
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        // Group 4: inc/dec r/m8.
        0xfe => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let name = match reg & 7 {
                0 => "inc",
                1 => "dec",
                _ => return None,
            };
            let text = rm_text(&rm, Width::Byte, prefixes);
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        // Group 5.
        0xff => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let (name, width) = match reg & 7 {
                0 => ("inc", wide),
                1 => ("dec", wide),
                2 => ("call", Width::Qword),
                4 => ("jmp", Width::Qword),
                6 => ("push", Width::Qword),
                _ => return None,
            };
            let text = rm_text(&rm, width, prefixes);
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        _ => None,
    }
}

/// The `0F`-prefixed two-byte opcode map.
///
/// `0F 38` / `0F 3A` three-byte opcodes and the SSE/AVX encodings are not
/// yet in the tables and return `None` (rendered `(bad)`); extending the
/// maps is staged in `plans/APPS.md`.
#[allow(clippy::too_many_lines)] // One exhaustive opcode map; splitting it would scatter the table.
fn two_byte(
    reader: &mut Reader<'_>,
    prefixes: &Prefixes,
    address: u64,
    window: &[u8],
) -> Option<Insn> {
    let op = reader.u8()?;
    let wide = prefixes.operand_width();
    let rexed = prefixes.rex.is_some();

    match op {
        0x05 => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "syscall",
            String::new(),
            None,
        )),
        0x0b => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "ud2",
            String::new(),
            None,
        )),
        // The multi-byte nop (`0F 1F /0`).
        0x1f => {
            let (reg, rm) = modrm(reader, prefixes)?;
            if reg & 7 != 0 {
                return None;
            }
            let text = rm_text(&rm, wide, prefixes);
            Some(finish(reader, prefixes, address, window, "nop", text, None))
        }
        0x31 => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "rdtsc",
            String::new(),
            None,
        )),
        // cmovcc.
        0x40..=0x4f => {
            let name = format!("cmov{}", CC[usize::from(op & 15)]);
            let (reg, rm) = modrm(reader, prefixes)?;
            let text = format!(
                "{},{}",
                reg_name(wide, reg, rexed),
                rm_text(&rm, wide, prefixes)
            );
            Some(finish(reader, prefixes, address, window, &name, text, None))
        }
        // Jcc rel32.
        0x80..=0x8f => {
            let rel = reader.i32()?;
            let end = u64::try_from(reader.pos).ok()?;
            let target = branch_target(address.wrapping_add(end), rel);
            let name = format!("j{}", CC[usize::from(op & 15)]);
            Some(finish(
                reader,
                prefixes,
                address,
                window,
                &name,
                format!("{target:#x}"),
                Some(target),
            ))
        }
        // setcc r/m8.
        0x90..=0x9f => {
            let name = format!("set{}", CC[usize::from(op & 15)]);
            let (_, rm) = modrm(reader, prefixes)?;
            let text = rm_text(&rm, Width::Byte, prefixes);
            Some(finish(reader, prefixes, address, window, &name, text, None))
        }
        0xa2 => Some(finish(
            reader,
            prefixes,
            address,
            window,
            "cpuid",
            String::new(),
            None,
        )),
        // bt/bts/btr/btc r/m,r.
        0xa3 | 0xab | 0xb3 | 0xbb => {
            let name = match op {
                0xa3 => "bt",
                0xab => "bts",
                0xb3 => "btr",
                _ => "btc",
            };
            let (reg, rm) = modrm(reader, prefixes)?;
            let text = format!(
                "{},{}",
                rm_text(&rm, wide, prefixes),
                reg_name(wide, reg, rexed)
            );
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        0xaf => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let text = format!(
                "{},{}",
                reg_name(wide, reg, rexed),
                rm_text(&rm, wide, prefixes)
            );
            Some(finish(
                reader, prefixes, address, window, "imul", text, None,
            ))
        }
        // cmpxchg.
        0xb0 | 0xb1 => {
            let width = if op == 0xb0 { Width::Byte } else { wide };
            let (reg, rm) = modrm(reader, prefixes)?;
            let text = format!(
                "{},{}",
                rm_text(&rm, width, prefixes),
                reg_name(width, reg, rexed)
            );
            Some(finish(
                reader, prefixes, address, window, "cmpxchg", text, None,
            ))
        }
        // movzx / movsx from a byte or word source.
        0xb6 | 0xb7 | 0xbe | 0xbf => {
            let name = if op < 0xbe { "movzx" } else { "movsx" };
            let source = if op & 1 == 0 {
                Width::Byte
            } else {
                Width::Word
            };
            let (reg, rm) = modrm(reader, prefixes)?;
            let text = format!(
                "{},{}",
                reg_name(wide, reg, rexed),
                rm_text(&rm, source, prefixes)
            );
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        // Group 8: bt/bts/btr/btc r/m,imm8.
        0xba => {
            let (reg, rm) = modrm(reader, prefixes)?;
            let name = match reg & 7 {
                4 => "bt",
                5 => "bts",
                6 => "btr",
                7 => "btc",
                _ => return None,
            };
            let imm = reader.i8()?;
            let text = format!(
                "{},{}",
                rm_text(&rm, wide, prefixes),
                imm_hex(imm, Width::Byte)
            );
            Some(finish(reader, prefixes, address, window, name, text, None))
        }
        // xadd.
        0xc0 | 0xc1 => {
            let width = if op == 0xc0 { Width::Byte } else { wide };
            let (reg, rm) = modrm(reader, prefixes)?;
            let text = format!(
                "{},{}",
                rm_text(&rm, width, prefixes),
                reg_name(width, reg, rexed)
            );
            Some(finish(
                reader, prefixes, address, window, "xadd", text, None,
            ))
        }
        // bswap r.
        0xc8..=0xcf => {
            let number = u32::from(op & 7) | (prefixes.rex_b() << 3);
            let width = if prefixes.rex_w() {
                Width::Qword
            } else {
                Width::Dword
            };
            let text = String::from(reg_name(width, number, true));
            Some(finish(
                reader, prefixes, address, window, "bswap", text, None,
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "x86_64_tests.rs"]
mod tests;
