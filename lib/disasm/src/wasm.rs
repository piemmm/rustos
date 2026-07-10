//! Wasm code-section body decoder.
//!
//! Decodes the structured opcode stream of one function body (the bytes
//! `lib/binfmt`'s `code_bodies` walk frames), rendering block nesting by
//! indentation. Every immediate is read as strict LEB128 — bounded to the
//! ceiling of the value width and refused when padding bits are set — so
//! the classic overlong-LEB attack fails closed as a `(bad)` byte instead
//! of desynchronising the stream.
//!
//! Because nesting is a property of the surrounding stream, [`decode`]
//! takes the current block depth and returns the depth the *next*
//! instruction is at; it stays a pure function of its inputs. `address` is
//! the instruction's offset within the module or body — wasm has no load
//! addresses, so branch labels are rendered as relative label indices and
//! [`crate::Insn::branch_target`] stays `None`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::{sign_extend, Insn};

/// Most `br_table` targets accepted before the instruction is refused.
///
/// A validation bound on untrusted input, not a capacity: a hostile count
/// must not make the decoder consume unbounded bytes as one "instruction".
pub const MAX_BR_TABLE_TARGETS: u32 = 4096;

/// Two spaces of indentation per open block.
const INDENT: &str = "  ";

/// A strictly-read unsigned LEB128 value: `(value, bytes consumed)`.
fn uleb(code: &[u8], bits: u32) -> Option<(u64, usize)> {
    let max_bytes = usize::try_from(bits.div_ceil(7)).unwrap_or(10);
    let mut value: u64 = 0;
    for (index, &byte) in code.iter().enumerate().take(max_bytes) {
        let shift = u32::try_from(index * 7).unwrap_or(63);
        let payload = u64::from(byte & 0x7f);
        if shift >= bits {
            return None;
        }
        // Refuse set bits beyond the declared width (overlong padding).
        if bits - shift < 7 && payload >> (bits - shift) != 0 {
            return None;
        }
        value |= payload << shift;
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

/// A strictly-read signed LEB128 value: `(value, bytes consumed)`.
fn sleb(code: &[u8], bits: u32) -> Option<(i64, usize)> {
    let max_bytes = usize::try_from(bits.div_ceil(7)).unwrap_or(10);
    let mut value: u64 = 0;
    for (index, &byte) in code.iter().enumerate().take(max_bytes) {
        let shift = u32::try_from(index * 7).unwrap_or(63);
        let payload = u64::from(byte & 0x7f);
        if shift < 64 {
            value |= payload << shift;
        }
        if byte & 0x80 == 0 {
            let used = shift + 7;
            let extended = sign_extend(value, used.min(64));
            // Refuse a value that does not fit the declared width.
            if bits < 64 {
                let min = -(1i64 << (bits - 1));
                let max = (1i64 << (bits - 1)) - 1;
                if extended < min || extended > max {
                    return None;
                }
            }
            return Some((extended, index + 1));
        }
        if shift >= bits {
            return None;
        }
    }
    None
}

/// Renders a block type immediate: `("suffix text", bytes consumed)`.
fn block_type(code: &[u8]) -> Option<(String, usize)> {
    let first = *code.first()?;
    if first == 0x40 {
        return Some((String::new(), 1));
    }
    if let Some(name) = value_type(first) {
        return Some((format!("(result {name})"), 1));
    }
    // Otherwise a type index encoded as a signed 33-bit LEB.
    let (index, used) = sleb(code, 33)?;
    if index < 0 {
        return None;
    }
    Some((format!("(type {index})"), used))
}

/// Value-type byte name, when `byte` is one.
fn value_type(byte: u8) -> Option<&'static str> {
    match byte {
        0x7f => Some("i32"),
        0x7e => Some("i64"),
        0x7d => Some("f32"),
        0x7c => Some("f64"),
        0x7b => Some("v128"),
        0x70 => Some("funcref"),
        0x6f => Some("externref"),
        _ => None,
    }
}

/// Mnemonic for an opcode that takes no immediate, when `opcode` is one.
#[allow(clippy::too_many_lines)] // The wasm numeric-opcode name table is one flat map.
fn plain_op(opcode: u8) -> Option<&'static str> {
    Some(match opcode {
        0x00 => "unreachable",
        0x01 => "nop",
        0x0f => "return",
        0x1a => "drop",
        0x1b => "select",
        0xd1 => "ref.is_null",
        0x45 => "i32.eqz",
        0x46 => "i32.eq",
        0x47 => "i32.ne",
        0x48 => "i32.lt_s",
        0x49 => "i32.lt_u",
        0x4a => "i32.gt_s",
        0x4b => "i32.gt_u",
        0x4c => "i32.le_s",
        0x4d => "i32.le_u",
        0x4e => "i32.ge_s",
        0x4f => "i32.ge_u",
        0x50 => "i64.eqz",
        0x51 => "i64.eq",
        0x52 => "i64.ne",
        0x53 => "i64.lt_s",
        0x54 => "i64.lt_u",
        0x55 => "i64.gt_s",
        0x56 => "i64.gt_u",
        0x57 => "i64.le_s",
        0x58 => "i64.le_u",
        0x59 => "i64.ge_s",
        0x5a => "i64.ge_u",
        0x5b => "f32.eq",
        0x5c => "f32.ne",
        0x5d => "f32.lt",
        0x5e => "f32.gt",
        0x5f => "f32.le",
        0x60 => "f32.ge",
        0x61 => "f64.eq",
        0x62 => "f64.ne",
        0x63 => "f64.lt",
        0x64 => "f64.gt",
        0x65 => "f64.le",
        0x66 => "f64.ge",
        0x67 => "i32.clz",
        0x68 => "i32.ctz",
        0x69 => "i32.popcnt",
        0x6a => "i32.add",
        0x6b => "i32.sub",
        0x6c => "i32.mul",
        0x6d => "i32.div_s",
        0x6e => "i32.div_u",
        0x6f => "i32.rem_s",
        0x70 => "i32.rem_u",
        0x71 => "i32.and",
        0x72 => "i32.or",
        0x73 => "i32.xor",
        0x74 => "i32.shl",
        0x75 => "i32.shr_s",
        0x76 => "i32.shr_u",
        0x77 => "i32.rotl",
        0x78 => "i32.rotr",
        0x79 => "i64.clz",
        0x7a => "i64.ctz",
        0x7b => "i64.popcnt",
        0x7c => "i64.add",
        0x7d => "i64.sub",
        0x7e => "i64.mul",
        0x7f => "i64.div_s",
        0x80 => "i64.div_u",
        0x81 => "i64.rem_s",
        0x82 => "i64.rem_u",
        0x83 => "i64.and",
        0x84 => "i64.or",
        0x85 => "i64.xor",
        0x86 => "i64.shl",
        0x87 => "i64.shr_s",
        0x88 => "i64.shr_u",
        0x89 => "i64.rotl",
        0x8a => "i64.rotr",
        0x8b => "f32.abs",
        0x8c => "f32.neg",
        0x8d => "f32.ceil",
        0x8e => "f32.floor",
        0x8f => "f32.trunc",
        0x90 => "f32.nearest",
        0x91 => "f32.sqrt",
        0x92 => "f32.add",
        0x93 => "f32.sub",
        0x94 => "f32.mul",
        0x95 => "f32.div",
        0x96 => "f32.min",
        0x97 => "f32.max",
        0x98 => "f32.copysign",
        0x99 => "f64.abs",
        0x9a => "f64.neg",
        0x9b => "f64.ceil",
        0x9c => "f64.floor",
        0x9d => "f64.trunc",
        0x9e => "f64.nearest",
        0x9f => "f64.sqrt",
        0xa0 => "f64.add",
        0xa1 => "f64.sub",
        0xa2 => "f64.mul",
        0xa3 => "f64.div",
        0xa4 => "f64.min",
        0xa5 => "f64.max",
        0xa6 => "f64.copysign",
        0xa7 => "i32.wrap_i64",
        0xa8 => "i32.trunc_f32_s",
        0xa9 => "i32.trunc_f32_u",
        0xaa => "i32.trunc_f64_s",
        0xab => "i32.trunc_f64_u",
        0xac => "i64.extend_i32_s",
        0xad => "i64.extend_i32_u",
        0xae => "i64.trunc_f32_s",
        0xaf => "i64.trunc_f32_u",
        0xb0 => "i64.trunc_f64_s",
        0xb1 => "i64.trunc_f64_u",
        0xb2 => "f32.convert_i32_s",
        0xb3 => "f32.convert_i32_u",
        0xb4 => "f32.convert_i64_s",
        0xb5 => "f32.convert_i64_u",
        0xb6 => "f32.demote_f64",
        0xb7 => "f64.convert_i32_s",
        0xb8 => "f64.convert_i32_u",
        0xb9 => "f64.convert_i64_s",
        0xba => "f64.convert_i64_u",
        0xbb => "f64.promote_f32",
        0xbc => "i32.reinterpret_f32",
        0xbd => "i64.reinterpret_f64",
        0xbe => "f32.reinterpret_i32",
        0xbf => "f64.reinterpret_i64",
        0xc0 => "i32.extend8_s",
        0xc1 => "i32.extend16_s",
        0xc2 => "i64.extend8_s",
        0xc3 => "i64.extend16_s",
        0xc4 => "i64.extend32_s",
        _ => return None,
    })
}

/// Mnemonic for a memory load/store opcode, when `opcode` is one.
fn memory_op(opcode: u8) -> Option<&'static str> {
    Some(match opcode {
        0x28 => "i32.load",
        0x29 => "i64.load",
        0x2a => "f32.load",
        0x2b => "f64.load",
        0x2c => "i32.load8_s",
        0x2d => "i32.load8_u",
        0x2e => "i32.load16_s",
        0x2f => "i32.load16_u",
        0x30 => "i64.load8_s",
        0x31 => "i64.load8_u",
        0x32 => "i64.load16_s",
        0x33 => "i64.load16_u",
        0x34 => "i64.load32_s",
        0x35 => "i64.load32_u",
        0x36 => "i32.store",
        0x37 => "i64.store",
        0x38 => "f32.store",
        0x39 => "f64.store",
        0x3a => "i32.store8",
        0x3b => "i32.store16",
        0x3c => "i64.store8",
        0x3d => "i64.store16",
        0x3e => "i64.store32",
        _ => return None,
    })
}

/// Mnemonic for an opcode whose sole immediate is one 32-bit index.
fn index_op(opcode: u8) -> Option<&'static str> {
    Some(match opcode {
        0x0c => "br",
        0x0d => "br_if",
        0x10 => "call",
        0x20 => "local.get",
        0x21 => "local.set",
        0x22 => "local.tee",
        0x23 => "global.get",
        0x24 => "global.set",
        0x25 => "table.get",
        0x26 => "table.set",
        0xd2 => "ref.func",
        _ => return None,
    })
}

/// Deepest nesting level the indentation renders; deeper levels clamp.
///
/// A validation bound on hostile input: a stream of a million `block`
/// opcodes must not make each rendered line a megabyte of spaces.
pub const MAX_INDENT_LEVELS: u32 = 32;

/// Decoded pieces of one instruction, before indentation is applied:
/// `(bytes consumed, mnemonic, operands, render level, next depth)`.
type Pieces = (usize, &'static str, String, u32, u32);

/// Decodes one instruction at `address` (its offset within the module or
/// body) from the front of `code`, currently `depth` blocks deep.
///
/// Returns `None` only for an empty slice; otherwise the instruction and
/// the depth the *next* instruction sits at. An unknown opcode or a
/// truncated/overlong immediate renders the opcode byte alone as `(bad)`,
/// so the walk always makes forward progress and resynchronises on the
/// next byte.
#[must_use]
pub fn decode(code: &[u8], address: u64, depth: u32) -> Option<(Insn, u32)> {
    let opcode = *code.first()?;
    let Some((length, name, operands, level, next)) = pieces(code, opcode, depth) else {
        return Some((Insn::bad(address, &code[..1]), depth));
    };
    if code.len() < length {
        return Some((Insn::bad(address, &code[..1]), depth));
    }
    let indent = INDENT.repeat(usize::try_from(level.min(MAX_INDENT_LEVELS)).unwrap_or(0));
    let mnemonic = format!("{indent}{name}");
    Some((
        Insn::new(address, &code[..length], mnemonic, operands, None),
        next,
    ))
}

/// Decodes the opcode and immediates of the instruction opening `code`.
#[allow(clippy::too_many_lines)] // One exhaustive opcode match; splitting it would scatter the map.
fn pieces(code: &[u8], opcode: u8, depth: u32) -> Option<Pieces> {
    if let Some(name) = plain_op(opcode) {
        return Some((1, name, String::new(), depth, depth));
    }
    if let Some(name) = index_op(opcode) {
        let (index, used) = uleb(&code[1..], 32)?;
        return Some((1 + used, name, format!("{index}"), depth, depth));
    }
    if let Some(name) = memory_op(opcode) {
        let (align, a_used) = uleb(&code[1..], 32)?;
        let (offset, o_used) = uleb(&code[1 + a_used..], 32)?;
        let text = format!("offset={offset} align={align}");
        return Some((1 + a_used + o_used, name, text, depth, depth));
    }
    match opcode {
        0x02..=0x04 => {
            let name = match opcode {
                0x02 => "block",
                0x03 => "loop",
                _ => "if",
            };
            let (suffix, used) = block_type(&code[1..])?;
            let text = suffix.trim_start().into();
            Some((1 + used, name, text, depth, depth.saturating_add(1)))
        }
        0x05 => Some((1, "else", String::new(), depth.saturating_sub(1), depth)),
        0x0b => {
            let level = depth.saturating_sub(1);
            Some((1, "end", String::new(), level, level))
        }
        0x0e => {
            let (count, mut used) = uleb(&code[1..], 32)?;
            if count > u64::from(MAX_BR_TABLE_TARGETS) {
                return None;
            }
            let mut labels: Vec<String> = Vec::new();
            for _ in 0..=count {
                let (label, l_used) = uleb(&code[1 + used..], 32)?;
                labels.push(label.to_string());
                used += l_used;
            }
            let text = labels.join(" ");
            Some((1 + used, "br_table", text, depth, depth))
        }
        0x11 => {
            let (type_index, t_used) = uleb(&code[1..], 32)?;
            let (table, b_used) = uleb(&code[1 + t_used..], 32)?;
            let text = format!("(type {type_index}) (table {table})");
            Some((1 + t_used + b_used, "call_indirect", text, depth, depth))
        }
        0x1c => {
            let (count, mut used) = uleb(&code[1..], 32)?;
            // The spec fixes the vector at one entry today; a small ceiling
            // keeps a hostile count from swallowing the stream.
            if count == 0 || count > 16 {
                return None;
            }
            let mut text = String::new();
            for _ in 0..count {
                let name = value_type(*code.get(1 + used)?)?;
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(name);
                used += 1;
            }
            Some((1 + used, "select", text, depth, depth))
        }
        0x3f | 0x40 => {
            let name = if opcode == 0x3f {
                "memory.size"
            } else {
                "memory.grow"
            };
            // The reserved memory-index byte must be zero today.
            if *code.get(1)? != 0 {
                return None;
            }
            Some((2, name, String::new(), depth, depth))
        }
        0x41 => {
            let (value, used) = sleb(&code[1..], 32)?;
            Some((1 + used, "i32.const", format!("{value}"), depth, depth))
        }
        0x42 => {
            let (value, used) = sleb(&code[1..], 64)?;
            Some((1 + used, "i64.const", format!("{value}"), depth, depth))
        }
        0x43 => {
            let raw = code.get(1..5)?;
            let bits = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            Some((5, "f32.const", format!("{bits:#010x}"), depth, depth))
        }
        0x44 => {
            let raw = code.get(1..9)?;
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(raw);
            let bits = u64::from_le_bytes(bytes);
            Some((9, "f64.const", format!("{bits:#018x}"), depth, depth))
        }
        0xd0 => {
            let name = value_type(*code.get(1)?)?;
            Some((2, "ref.null", String::from(name), depth, depth))
        }
        0xfc => {
            let (sub, used) = uleb(&code[1..], 32)?;
            let (length, name, text) = prefixed(&code[1 + used..], sub)?;
            Some((1 + used + length, name, text, depth, depth))
        }
        _ => None,
    }
}

/// Decodes the immediates of a `0xFC`-prefixed instruction: the bytes
/// after the sub-opcode become `(immediate length, mnemonic, operands)`.
fn prefixed(rest: &[u8], sub: u64) -> Option<(usize, &'static str, String)> {
    let sat = |name| Some((0, name, String::new()));
    match sub {
        0 => sat("i32.trunc_sat_f32_s"),
        1 => sat("i32.trunc_sat_f32_u"),
        2 => sat("i32.trunc_sat_f64_s"),
        3 => sat("i32.trunc_sat_f64_u"),
        4 => sat("i64.trunc_sat_f32_s"),
        5 => sat("i64.trunc_sat_f32_u"),
        6 => sat("i64.trunc_sat_f64_s"),
        7 => sat("i64.trunc_sat_f64_u"),
        8 => {
            let (data, used) = uleb(rest, 32)?;
            // The trailing reserved memory-index byte must be zero.
            if *rest.get(used)? != 0 {
                return None;
            }
            Some((used + 1, "memory.init", format!("{data}")))
        }
        9 => {
            let (data, used) = uleb(rest, 32)?;
            Some((used, "data.drop", format!("{data}")))
        }
        10 => {
            if *rest.first()? != 0 || *rest.get(1)? != 0 {
                return None;
            }
            Some((2, "memory.copy", String::new()))
        }
        11 => {
            if *rest.first()? != 0 {
                return None;
            }
            Some((1, "memory.fill", String::new()))
        }
        12 | 14 => {
            let name = if sub == 12 {
                "table.init"
            } else {
                "table.copy"
            };
            let (first, f_used) = uleb(rest, 32)?;
            let (second, s_used) = uleb(&rest[f_used..], 32)?;
            Some((f_used + s_used, name, format!("{first} {second}")))
        }
        13 | 15 | 16 | 17 => {
            let name = match sub {
                13 => "elem.drop",
                15 => "table.grow",
                16 => "table.size",
                _ => "table.fill",
            };
            let (index, used) = uleb(rest, 32)?;
            Some((used, name, format!("{index}")))
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "wasm_tests.rs"]
mod tests;
