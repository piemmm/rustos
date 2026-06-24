//! The emitter: an [`Op`] (or a sequence of them) rendered to ANSI / VT / xterm
//! bytes.
//!
//! The emitter writes the canonical encoding of each operation over the same
//! control bytes ([`crate::control`]) and SGR table ([`crate::attr`]) the
//! [`crate::Parser`] reads, so the two agree by construction: parsing the
//! emitter's output reproduces the original [`Op`].
//!
//! Movement counts are clamped up to `1` (ANSI's default), so even a degenerate
//! `CursorUp(0)` emits the well-formed `CSI 1 A` and round-trips as `CursorUp(1)`.

use alloc::vec::Vec;

use crate::attr::Sgr;
use crate::control;
use crate::key::Key;
use crate::op::Op;

/// Render `op` to bytes.
#[must_use]
pub fn encode(op: &Op) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(op, &mut out);
    out
}

/// Append the byte encoding of `op` to `out`.
pub fn encode_into(op: &Op, out: &mut Vec<u8>) {
    match op {
        Op::Print(ch) => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        Op::Bell => out.push(control::BEL),
        Op::Backspace => out.push(control::BS),
        Op::Tab => out.push(control::HT),
        Op::LineFeed => out.push(control::LF),
        Op::CarriageReturn => out.push(control::CR),
        Op::CursorUp(n) => csi_count(out, *n, control::CUU),
        Op::CursorDown(n) => csi_count(out, *n, control::CUD),
        Op::CursorForward(n) => csi_count(out, *n, control::CUF),
        Op::CursorBack(n) => csi_count(out, *n, control::CUB),
        Op::CursorNextLine(n) => csi_count(out, *n, control::CNL),
        Op::CursorPrevLine(n) => csi_count(out, *n, control::CPL),
        Op::CursorColumn(col) => csi_count(out, *col, control::CHA),
        Op::CursorPosition { row, col } => {
            csi(out);
            push_decimal(out, (*row).max(1));
            out.push(control::SEPARATOR);
            push_decimal(out, (*col).max(1));
            out.push(control::CUP);
        }
        Op::EraseInDisplay(mode) => csi_value(out, mode.value(), control::ED),
        Op::EraseInLine(mode) => csi_value(out, mode.value(), control::EL),
        Op::ScrollUp(n) => csi_count(out, *n, control::SU),
        Op::ScrollDown(n) => csi_count(out, *n, control::SD),
        Op::SetScrollRegion { top, bottom } => {
            csi(out);
            push_decimal(out, (*top).max(1));
            out.push(control::SEPARATOR);
            push_decimal(out, (*bottom).max(1));
            out.push(control::DECSTBM);
        }
        Op::ResetScrollRegion => {
            csi(out);
            out.push(control::DECSTBM);
        }
        Op::EnterAltScreen => private_mode(out, control::MODE_ALT_SCREEN, true),
        Op::LeaveAltScreen => private_mode(out, control::MODE_ALT_SCREEN, false),
        Op::ShowCursor => private_mode(out, control::MODE_CURSOR_VISIBLE, true),
        Op::HideCursor => private_mode(out, control::MODE_CURSOR_VISIBLE, false),
        Op::SaveCursor => {
            out.push(control::ESC);
            out.push(control::SAVE_CURSOR);
        }
        Op::RestoreCursor => {
            out.push(control::ESC);
            out.push(control::RESTORE_CURSOR);
        }
        Op::Sgr(sgr) => encode_sgr(out, *sgr),
        Op::SetTitle(title) => encode_title(out, title),
        Op::Key(key) => encode_key(out, *key),
        Op::SetMouseMode { mode, enable } => private_mode(out, mode.mode_number(), *enable),
        Op::Mouse(report) => encode_mouse(out, report),
        Op::SetBracketedPaste(enable) => {
            private_mode(out, control::MODE_BRACKETED_PASTE, *enable);
        }
        Op::PasteStart => csi_value(out, control::PASTE_START, control::TILDE),
        Op::PasteEnd => csi_value(out, control::PASTE_END, control::TILDE),
    }
}

/// Render every `op` in `ops` to bytes, in order.
#[must_use]
pub fn encode_all(ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        encode_into(op, &mut out);
    }
    out
}

/// Write the CSI introducer (`ESC [`).
fn csi(out: &mut Vec<u8>) {
    out.push(control::ESC);
    out.push(control::CSI);
}

/// Write `CSI <count> <final>` with `count` clamped up to `1`.
fn csi_count(out: &mut Vec<u8>, count: u16, final_byte: u8) {
    csi(out);
    push_decimal(out, count.max(1));
    out.push(final_byte);
}

/// Write `CSI <value> <final>` with `value` emitted verbatim (used where `0` is
/// a meaningful parameter, e.g. erase modes).
fn csi_value(out: &mut Vec<u8>, value: u16, final_byte: u8) {
    csi(out);
    push_decimal(out, value);
    out.push(final_byte);
}

/// Write a DEC private mode set (`CSI ? <mode> h`) or reset
/// (`CSI ? <mode> l`).
fn private_mode(out: &mut Vec<u8>, mode: u16, set: bool) {
    csi(out);
    out.push(control::PRIVATE);
    push_decimal(out, mode);
    out.push(if set {
        control::SET_MODE
    } else {
        control::RESET_MODE
    });
}

/// Write `CSI <params> m` for one SGR operation.
fn encode_sgr(out: &mut Vec<u8>, sgr: Sgr) {
    let mut params = Vec::new();
    sgr.write_params(&mut params);
    csi(out);
    for (i, param) in params.iter().enumerate() {
        if i > 0 {
            out.push(control::SEPARATOR);
        }
        push_decimal(out, *param);
    }
    out.push(control::SGR);
}

/// Write the canonical encoding of a named [`Key`]: `ESC O <byte>` for the keys
/// with an `SS3` form (`F1`…`F4`), otherwise `CSI <param> ~`.
fn encode_key(out: &mut Vec<u8>, key: Key) {
    if let Some(final_byte) = key.ss3_final() {
        out.push(control::ESC);
        out.push(control::SS3);
        out.push(final_byte);
    } else if let Some(param) = key.tilde_param() {
        csi_value(out, param, control::TILDE);
    }
}

/// Write one SGR mouse report: `CSI < Cb ; Cx ; Cy M` for a press, `… m` for a
/// release. Coordinates clamp up to `1` so a degenerate `0` round-trips.
fn encode_mouse(out: &mut Vec<u8>, report: &crate::mouse::MouseReport) {
    csi(out);
    out.push(control::MOUSE_SGR);
    push_decimal(out, report.encode_button());
    out.push(control::SEPARATOR);
    push_decimal(out, report.col.max(1));
    out.push(control::SEPARATOR);
    push_decimal(out, report.row.max(1));
    out.push(if report.pressed {
        control::MOUSE_PRESS
    } else {
        control::MOUSE_RELEASE
    });
}

/// Write `OSC 0 ; <title> ST` (using the `BEL` string terminator xterm
/// accepts).
fn encode_title(out: &mut Vec<u8>, title: &str) {
    out.push(control::ESC);
    out.push(control::OSC);
    out.push(b'0');
    out.push(control::SEPARATOR);
    out.extend_from_slice(title.as_bytes());
    out.push(control::BEL);
}

/// Append the decimal ASCII digits of `value` to `out` (no `as` cast).
fn push_decimal(out: &mut Vec<u8>, value: u16) {
    // `u16::MAX` is 65535 — five digits at most.
    let mut buf = [0u8; 5];
    let mut remaining = value;
    let mut start = buf.len();
    loop {
        start -= 1;
        let digit = u8::try_from(remaining % 10).unwrap_or(0);
        buf[start] = b'0' + digit;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[start..]);
}
