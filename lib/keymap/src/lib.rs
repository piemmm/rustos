//! TAIRiX shared terminal key map (`lib/keymap`).
//!
//! A console-input *producer* — a directly attached keyboard driver such as
//! `drivers/input/usb_kbd` or `drivers/input/ps2` — decodes its device's
//! scancodes into the [`Key`] vocabulary (`lib/input`) and must then turn each
//! key press into the byte sequence a terminal sends down its input stream: a
//! printable character as itself, `Ctrl-C` as the `0x03` control code, the up
//! arrow as `ESC [ A`, and so on. That translation is the **terminal key map**,
//! and it is identical for every keyboard regardless of how the device
//! delivered the key — so it lives here, once, rather
//! than being re-derived in each driver.
//!
//! [`encode_key`] is that one definition: given the [`Key`] a layout produced
//! and the [`Modifiers`] held with it, it writes the console (tty) bytes into a
//! caller-supplied buffer and returns their length. The keyboard driver feeds
//! those bytes to the kernel through the `console_input` syscall
//! (`plans/PI.md` P11), which delivers them to the video console's read half.
//!
//! # One escape vocabulary
//!
//! The escape sequences the named keys send are *not* defined here: they are
//! the canonical ANSI / VT / xterm vocabulary `lib/vt` already owns. This crate
//! reuses [`tairix_vt::control`]'s control-byte constants and
//! [`tairix_vt::key::Key`]'s `SS3` / `CSI … ~` tables, so there is no second
//! escape-sequence definition in the tree. Only those
//! `const` tables are touched, so the crate is allocation-free.
//!
//! # Allocation-free and fail-closed
//!
//! [`encode_key`] never allocates and never panics: it
//! writes into the caller's buffer and returns [`KeymapError::BufferTooSmall`]
//! if the buffer cannot hold the sequence. Pass a buffer of at least
//! [`MAX_KEY_BYTES`] and that error cannot occur. A key with no console
//! encoding (for example a malformed function-key number) produces nothing
//! (`Ok(0)`) rather than guessing.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_input::{Key, Modifiers, NamedKey};
use tairix_vt::control;
use tairix_vt::key::Key as VtKey;

/// The longest console sequence any single key press encodes to, in bytes.
///
/// The two longest forms are a meta-prefixed four-byte character
/// (`ESC` + a 4-byte UTF-8 scalar) and a two-digit `CSI … ~` editing/function
/// key (`ESC [ 2 4 ~`), both five bytes. A buffer of this length passed to
/// [`encode_key`] can therefore never overflow.
pub const MAX_KEY_BYTES: usize = 5;

/// Why [`encode_key`] could not write a key's console bytes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum KeymapError {
    /// The destination buffer was too small to hold the sequence. Pass a
    /// buffer of at least [`MAX_KEY_BYTES`] bytes to rule this out.
    BufferTooSmall,
}

/// Append-only writer over a fixed caller buffer, failing closed on overflow.
struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Writer<'a> {
    const fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    /// Append one byte, or fail closed if the buffer is full.
    fn push(&mut self, byte: u8) -> Result<(), KeymapError> {
        let slot = self
            .buf
            .get_mut(self.len)
            .ok_or(KeymapError::BufferTooSmall)?;
        *slot = byte;
        self.len += 1;
        Ok(())
    }

    /// Append the UTF-8 encoding of `ch`.
    fn push_char(&mut self, ch: char) -> Result<(), KeymapError> {
        let mut utf8 = [0u8; 4];
        for &byte in ch.encode_utf8(&mut utf8).as_bytes() {
            self.push(byte)?;
        }
        Ok(())
    }

    /// Append the decimal ASCII digits of `value` (no `as` cast).
    fn push_decimal(&mut self, value: u16) -> Result<(), KeymapError> {
        // `u16::MAX` is 65535 — five digits at most.
        let mut digits = [0u8; 5];
        let mut remaining = value;
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + u8::try_from(remaining % 10).unwrap_or(0);
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        for &digit in &digits[start..] {
            self.push(digit)?;
        }
        Ok(())
    }

    /// Append a `CSI <final>` sequence (`ESC [ <final>`).
    fn push_csi_final(&mut self, final_byte: u8) -> Result<(), KeymapError> {
        self.push(control::ESC)?;
        self.push(control::CSI)?;
        self.push(final_byte)
    }

    /// Append a `CSI <param> ~` editing-key sequence.
    fn push_csi_tilde(&mut self, param: u16) -> Result<(), KeymapError> {
        self.push(control::ESC)?;
        self.push(control::CSI)?;
        self.push_decimal(param)?;
        self.push(control::TILDE)
    }

    /// Append an `SS3 <final>` function-key sequence (`ESC O <final>`).
    fn push_ss3(&mut self, final_byte: u8) -> Result<(), KeymapError> {
        self.push(control::ESC)?;
        self.push(control::SS3)?;
        self.push(final_byte)
    }
}

/// Encode one key press into the console (tty) bytes a terminal sends.
///
/// `key` is the [`Key`] a keyboard layout produced (its character already
/// reflects the active shift / caps-lock state) and `modifiers` are the
/// [`Modifiers`] still held — only [`Modifiers::ctrl`] and [`Modifiers::alt`]
/// change the bytes here, since shift and caps were already folded into the
/// produced character.
///
/// The bytes are written into `out` and the number written is returned. A key
/// with no terminal encoding writes nothing and returns `Ok(0)` (fail closed — never guess). This is an encoder of already-typed input,
/// not a parser of untrusted bytes.
///
/// # Mapping
///
/// * A printable [`Key::Char`] is sent as its UTF-8 bytes. With
///   [`Modifiers::ctrl`] held it becomes the matching C0 control code
///   (`Ctrl-A`..`Ctrl-Z` → `0x01`..`0x1A`, `Ctrl-@`..`Ctrl-_` → `0x00`..`0x1F`,
///   `Ctrl-?` → `0x7F`); a character with no control form is sent unchanged.
/// * With [`Modifiers::alt`] held, a printable character is prefixed with `ESC`
///   (the xterm "meta sends escape" convention).
/// * [`NamedKey::Enter`] → `CR` (`0x0D`), [`NamedKey::Tab`] → `HT` (`0x09`),
///   [`NamedKey::Backspace`] → `DEL` (`0x7F`), [`NamedKey::Escape`] → `ESC`.
/// * The arrow keys send `ESC [ A`..`ESC [ D` (normal cursor mode).
/// * The editing / navigation keys and `F1`..`F12` send the canonical
///   [`tairix_vt`] `SS3` / `CSI … ~` sequences.
///
/// # Errors
///
/// Returns [`KeymapError::BufferTooSmall`] if `out` cannot hold the sequence;
/// a buffer of at least [`MAX_KEY_BYTES`] bytes can never trigger this.
///
/// # Capabilities
///
/// None.
pub fn encode_key(key: Key, modifiers: Modifiers, out: &mut [u8]) -> Result<usize, KeymapError> {
    let mut writer = Writer::new(out);
    match key {
        Key::Char(ch) => {
            if modifiers.alt {
                writer.push(control::ESC)?;
            }
            if modifiers.ctrl {
                match control_byte(ch) {
                    Some(code) => writer.push(code)?,
                    None => writer.push_char(ch)?,
                }
            } else {
                writer.push_char(ch)?;
            }
        }
        Key::Named(named) => encode_named(named, &mut writer)?,
    }
    Ok(writer.len)
}

/// The C0 control code a `Ctrl`-held character produces, if any.
///
/// Mirrors a terminal's control-key arithmetic: a letter or one of the
/// `@ [ \ ] ^ _` symbols masks down to `byte & 0x1F`, space maps to `NUL`, and
/// `?` maps to `DEL` (`0x7F`). Any other character has no control form.
fn control_byte(ch: char) -> Option<u8> {
    let byte = u8::try_from(u32::from(ch)).ok()?;
    match byte {
        b'a'..=b'z' => Some(byte - b'a' + 1),
        b'@'..=b'_' => Some(byte & 0x1f),
        b' ' => Some(0x00),
        b'?' => Some(0x7f),
        _ => None,
    }
}

/// Encode a named (non-character) key into `writer`.
fn encode_named(named: NamedKey, writer: &mut Writer<'_>) -> Result<(), KeymapError> {
    match named {
        NamedKey::Enter => writer.push(control::CR),
        NamedKey::Tab => writer.push(control::HT),
        NamedKey::Backspace => writer.push(DEL),
        NamedKey::Escape => writer.push(control::ESC),
        NamedKey::Up => writer.push_csi_final(control::CUU),
        NamedKey::Down => writer.push_csi_final(control::CUD),
        NamedKey::Right => writer.push_csi_final(control::CUF),
        NamedKey::Left => writer.push_csi_final(control::CUB),
        other => match named_to_vt(other) {
            Some(vt) => {
                if let Some(final_byte) = vt.ss3_final() {
                    writer.push_ss3(final_byte)
                } else if let Some(param) = vt.tilde_param() {
                    writer.push_csi_tilde(param)
                } else {
                    Ok(())
                }
            }
            None => Ok(()),
        },
    }
}

/// The `DEL` (`0x7F`) byte a `Backspace` key sends, as Linux consoles expect.
const DEL: u8 = 0x7f;

/// Map an editing / navigation / function [`NamedKey`] to its canonical
/// [`tairix_vt`] key, or `None` for a key handled directly (the arrows and the
/// control keys) or one with no terminal encoding (a malformed function
/// number).
fn named_to_vt(named: NamedKey) -> Option<VtKey> {
    Some(match named {
        NamedKey::Insert => VtKey::Insert,
        NamedKey::Delete => VtKey::Delete,
        NamedKey::Home => VtKey::Home,
        NamedKey::End => VtKey::End,
        NamedKey::PageUp => VtKey::PageUp,
        NamedKey::PageDown => VtKey::PageDown,
        NamedKey::Function { number } => match number {
            1 => VtKey::F1,
            2 => VtKey::F2,
            3 => VtKey::F3,
            4 => VtKey::F4,
            5 => VtKey::F5,
            6 => VtKey::F6,
            7 => VtKey::F7,
            8 => VtKey::F8,
            9 => VtKey::F9,
            10 => VtKey::F10,
            11 => VtKey::F11,
            12 => VtKey::F12,
            _ => return None,
        },
        NamedKey::Enter
        | NamedKey::Escape
        | NamedKey::Backspace
        | NamedKey::Tab
        | NamedKey::Left
        | NamedKey::Right
        | NamedKey::Up
        | NamedKey::Down => return None,
    })
}

/// The wire keyboard-event vocabulary the abi<->tty bridge crosses to.
///
/// Kept behind module aliases so the wire [`AbiModifiers`] never collides with
/// the `lib/input` [`Modifiers`] this crate's encoder already uses.
use tairix_abi::input::{KeyInput, KeyValue, Modifiers as AbiModifiers, NamedKeyCode};

/// Map a wire [`NamedKeyCode`] to the `lib/input` [`NamedKey`].
///
/// The two enumerate the same closed set of non-character keys; this is the
/// one place the tree crosses between them.
const fn named_key_from_abi(code: NamedKeyCode) -> NamedKey {
    match code {
        NamedKeyCode::Enter => NamedKey::Enter,
        NamedKeyCode::Escape => NamedKey::Escape,
        NamedKeyCode::Backspace => NamedKey::Backspace,
        NamedKeyCode::Tab => NamedKey::Tab,
        NamedKeyCode::Delete => NamedKey::Delete,
        NamedKeyCode::Insert => NamedKey::Insert,
        NamedKeyCode::Home => NamedKey::Home,
        NamedKeyCode::End => NamedKey::End,
        NamedKeyCode::PageUp => NamedKey::PageUp,
        NamedKeyCode::PageDown => NamedKey::PageDown,
        NamedKeyCode::Left => NamedKey::Left,
        NamedKeyCode::Right => NamedKey::Right,
        NamedKeyCode::Up => NamedKey::Up,
        NamedKeyCode::Down => NamedKey::Down,
        NamedKeyCode::F1 => NamedKey::Function { number: 1 },
        NamedKeyCode::F2 => NamedKey::Function { number: 2 },
        NamedKeyCode::F3 => NamedKey::Function { number: 3 },
        NamedKeyCode::F4 => NamedKey::Function { number: 4 },
        NamedKeyCode::F5 => NamedKey::Function { number: 5 },
        NamedKeyCode::F6 => NamedKey::Function { number: 6 },
        NamedKeyCode::F7 => NamedKey::Function { number: 7 },
        NamedKeyCode::F8 => NamedKey::Function { number: 8 },
        NamedKeyCode::F9 => NamedKey::Function { number: 9 },
        NamedKeyCode::F10 => NamedKey::Function { number: 10 },
        NamedKeyCode::F11 => NamedKey::Function { number: 11 },
        NamedKeyCode::F12 => NamedKey::Function { number: 12 },
    }
}

/// Map a `lib/input` [`NamedKey`] to its wire [`NamedKeyCode`], or `None` for a
/// function number outside the defined `F1..=F12` range (fail closed, never
/// guess).
const fn named_key_to_abi(named: NamedKey) -> Option<NamedKeyCode> {
    Some(match named {
        NamedKey::Enter => NamedKeyCode::Enter,
        NamedKey::Escape => NamedKeyCode::Escape,
        NamedKey::Backspace => NamedKeyCode::Backspace,
        NamedKey::Tab => NamedKeyCode::Tab,
        NamedKey::Delete => NamedKeyCode::Delete,
        NamedKey::Insert => NamedKeyCode::Insert,
        NamedKey::Home => NamedKeyCode::Home,
        NamedKey::End => NamedKeyCode::End,
        NamedKey::PageUp => NamedKeyCode::PageUp,
        NamedKey::PageDown => NamedKeyCode::PageDown,
        NamedKey::Left => NamedKeyCode::Left,
        NamedKey::Right => NamedKeyCode::Right,
        NamedKey::Up => NamedKeyCode::Up,
        NamedKey::Down => NamedKeyCode::Down,
        NamedKey::Function { number } => match number {
            1 => NamedKeyCode::F1,
            2 => NamedKeyCode::F2,
            3 => NamedKeyCode::F3,
            4 => NamedKeyCode::F4,
            5 => NamedKeyCode::F5,
            6 => NamedKeyCode::F6,
            7 => NamedKeyCode::F7,
            8 => NamedKeyCode::F8,
            9 => NamedKeyCode::F9,
            10 => NamedKeyCode::F10,
            11 => NamedKeyCode::F11,
            12 => NamedKeyCode::F12,
            _ => return None,
        },
    })
}

/// Map the wire [`AbiModifiers`] to the `lib/input` [`Modifiers`].
///
/// The one definition of that field mapping, so the kernel arbiter, the
/// desktop seat, and an app's own key handling cannot disagree about which
/// modifier a record names.
#[must_use]
pub const fn modifiers_from_abi(m: AbiModifiers) -> Modifiers {
    Modifiers {
        shift: m.shift,
        ctrl: m.ctrl,
        alt: m.alt,
        meta: m.meta,
    }
}

/// Map the `lib/input` [`Modifiers`] to the wire [`AbiModifiers`], the inverse
/// of [`modifiers_from_abi`].
///
/// The desktop holds the seat's modifier state in the routing vocabulary and
/// stamps it onto the wire events it delivers, so it needs this direction as
/// much as a driver does.
#[must_use]
pub const fn modifiers_to_abi(m: Modifiers) -> AbiModifiers {
    AbiModifiers {
        shift: m.shift,
        ctrl: m.ctrl,
        alt: m.alt,
        meta: m.meta,
    }
}

/// Map a wire [`KeyValue`] to the `lib/input` [`Key`].
const fn key_from_abi(value: KeyValue) -> Key {
    match value {
        KeyValue::Char(c) => Key::Char(c),
        KeyValue::Named(code) => Key::Named(named_key_from_abi(code)),
    }
}

/// Encode one decoded [`KeyInput`] record into the console (tty) bytes a
/// terminal sends, for the kernel input-focus arbiter's *text* sink
/// (`plans/PI.md` P11).
///
/// This is [`encode_key`] reached from the wire vocabulary: it maps the
/// record's [`KeyValue`] and [`AbiModifiers`] to the `lib/input` [`Key`] /
/// [`Modifiers`] and encodes them, so the arbiter never owns a second copy of
/// the layout-to-tty mapping. Only a key **press** produces
/// bytes — a terminal sends nothing on key release — so a
/// [`KeyInput::Released`] — and a [`KeyInput::ModifiersChanged`], which names
/// no key at all — writes nothing and returns `Ok(0)`.
///
/// # Errors
///
/// Returns [`KeymapError::BufferTooSmall`] if `out` cannot hold the sequence;
/// a buffer of at least [`MAX_KEY_BYTES`] bytes can never trigger this.
///
/// # Capabilities
///
/// None.
pub fn encode_key_input(record: &KeyInput, out: &mut [u8]) -> Result<usize, KeymapError> {
    match *record {
        KeyInput::Pressed { key, modifiers } => {
            encode_key(key_from_abi(key), modifiers_from_abi(modifiers), out)
        }
        KeyInput::Released { .. } | KeyInput::ModifiersChanged { .. } => Ok(0),
    }
}

/// Build a wire [`KeyInput`] record from a decoded [`Key`] edge, for a keyboard
/// driver's *device* side (`plans/PI.md` P11): the driver resolves its
/// device's scancodes into a [`Key`] + held [`Modifiers`] and emits the key
/// edge, leaving the encoding and routing to the kernel arbiter.
///
/// `pressed` selects [`KeyInput::Pressed`] versus [`KeyInput::Released`].
/// Returns `None` for a [`Key`] with no wire representation (a function number
/// outside `F1..=F12`) — fail closed, never guess.
///
/// # Capabilities
///
/// None.
#[must_use]
pub fn key_input(key: Key, modifiers: Modifiers, pressed: bool) -> Option<KeyInput> {
    let value = match key {
        Key::Char(c) => KeyValue::Char(c),
        Key::Named(named) => KeyValue::Named(named_key_to_abi(named)?),
    };
    let modifiers = modifiers_to_abi(modifiers);
    Some(if pressed {
        KeyInput::Pressed {
            key: value,
            modifiers,
        }
    } else {
        KeyInput::Released {
            key: value,
            modifiers,
        }
    })
}

/// Build a wire [`KeyInput::ModifiersChanged`] record from the `lib/input`
/// [`Modifiers`] a keyboard driver now holds — the modifier-edge counterpart
/// of [`key_input`], so the one `lib/input`→wire modifier mapping serves both.
///
/// # Capabilities
///
/// None.
#[must_use]
pub const fn modifier_change(modifiers: Modifiers) -> KeyInput {
    KeyInput::ModifiersChanged {
        modifiers: modifiers_to_abi(modifiers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `key` held with `modifiers` into a [`MAX_KEY_BYTES`] buffer,
    /// asserting it does not overflow, and pass the written bytes to `check`.
    fn with_encoding(key: Key, modifiers: Modifiers, check: impl FnOnce(&[u8])) {
        let mut buf = [0u8; MAX_KEY_BYTES];
        let n = encode_key(key, modifiers, &mut buf).expect("buffer fits MAX_KEY_BYTES");
        check(&buf[..n]);
    }

    /// As [`with_encoding`] for a named key held with no modifiers.
    fn with_named(named: NamedKey, check: impl FnOnce(&[u8])) {
        with_encoding(Key::Named(named), Modifiers::default(), check);
    }

    #[test]
    fn plain_character_is_itself() {
        with_encoding(Key::Char('a'), Modifiers::default(), |bytes| {
            assert_eq!(bytes, b"a");
        });
    }

    #[test]
    fn shifted_character_is_already_resolved_by_the_layout() {
        // The layout produces the uppercase scalar; shift does not change the
        // bytes here.
        let mods = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        with_encoding(Key::Char('A'), mods, |bytes| assert_eq!(bytes, b"A"));
    }

    #[test]
    fn ctrl_letters_map_to_c0_controls() {
        let mods = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        for (ch, code) in [('a', 0x01u8), ('c', 0x03), ('z', 0x1a)] {
            with_encoding(Key::Char(ch), mods, |bytes| assert_eq!(bytes, &[code]));
        }
    }

    #[test]
    fn ctrl_symbols_map_to_c0_controls() {
        // Ctrl-@ → NUL, Ctrl-[ → ESC, Ctrl-? → DEL, Ctrl-Space → NUL.
        let mods = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        for (ch, code) in [('@', 0x00u8), ('[', 0x1b), ('?', 0x7f), (' ', 0x00)] {
            with_encoding(Key::Char(ch), mods, |bytes| {
                assert_eq!(bytes, &[code], "ctrl-{ch}");
            });
        }
    }

    #[test]
    fn ctrl_without_a_control_form_passes_the_character_through() {
        let mods = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        with_encoding(Key::Char('é'), mods, |bytes| {
            assert_eq!(bytes, "é".as_bytes());
        });
    }

    #[test]
    fn alt_prefixes_escape() {
        let mods = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        with_encoding(Key::Char('x'), mods, |bytes| {
            assert_eq!(bytes, &[control::ESC, b'x']);
        });
    }

    #[test]
    fn control_keys_map_to_their_bytes() {
        with_named(NamedKey::Enter, |b| assert_eq!(b, &[control::CR]));
        with_named(NamedKey::Tab, |b| assert_eq!(b, &[control::HT]));
        with_named(NamedKey::Backspace, |b| assert_eq!(b, &[DEL]));
        with_named(NamedKey::Escape, |b| assert_eq!(b, &[control::ESC]));
    }

    #[test]
    fn arrows_send_normal_cursor_sequences() {
        with_named(NamedKey::Up, |b| assert_eq!(b, b"\x1b[A"));
        with_named(NamedKey::Down, |b| assert_eq!(b, b"\x1b[B"));
        with_named(NamedKey::Right, |b| assert_eq!(b, b"\x1b[C"));
        with_named(NamedKey::Left, |b| assert_eq!(b, b"\x1b[D"));
    }

    #[test]
    fn editing_keys_send_tilde_sequences() {
        with_named(NamedKey::Home, |b| assert_eq!(b, b"\x1b[1~"));
        with_named(NamedKey::Insert, |b| assert_eq!(b, b"\x1b[2~"));
        with_named(NamedKey::Delete, |b| assert_eq!(b, b"\x1b[3~"));
        with_named(NamedKey::End, |b| assert_eq!(b, b"\x1b[4~"));
        with_named(NamedKey::PageUp, |b| assert_eq!(b, b"\x1b[5~"));
        with_named(NamedKey::PageDown, |b| assert_eq!(b, b"\x1b[6~"));
    }

    #[test]
    fn function_keys_use_ss3_then_tilde_forms() {
        with_named(NamedKey::Function { number: 1 }, |b| {
            assert_eq!(b, b"\x1bOP");
        });
        with_named(NamedKey::Function { number: 4 }, |b| {
            assert_eq!(b, b"\x1bOS");
        });
        with_named(NamedKey::Function { number: 5 }, |b| {
            assert_eq!(b, b"\x1b[15~");
        });
        with_named(NamedKey::Function { number: 12 }, |b| {
            assert_eq!(b, b"\x1b[24~");
        });
    }

    #[test]
    fn malformed_function_number_encodes_nothing() {
        let mut buf = [0u8; MAX_KEY_BYTES];
        assert_eq!(
            encode_key(
                Key::Named(NamedKey::Function { number: 99 }),
                Modifiers::default(),
                &mut buf
            ),
            Ok(0)
        );
    }

    #[test]
    fn too_small_buffer_fails_closed() {
        let mut buf = [0u8; 1];
        assert_eq!(
            encode_key(Key::Named(NamedKey::Up), Modifiers::default(), &mut buf),
            Err(KeymapError::BufferTooSmall)
        );
    }

    #[test]
    fn every_named_key_fits_max_key_bytes() {
        let named = [
            NamedKey::Enter,
            NamedKey::Escape,
            NamedKey::Backspace,
            NamedKey::Tab,
            NamedKey::Delete,
            NamedKey::Insert,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
            NamedKey::Left,
            NamedKey::Right,
            NamedKey::Up,
            NamedKey::Down,
        ];
        for key in named {
            let mut buf = [0u8; MAX_KEY_BYTES];
            assert!(encode_key(Key::Named(key), Modifiers::default(), &mut buf).is_ok());
        }
        for number in 1..=12 {
            let mut buf = [0u8; MAX_KEY_BYTES];
            assert!(encode_key(
                Key::Named(NamedKey::Function { number }),
                Modifiers::default(),
                &mut buf
            )
            .is_ok());
        }
    }

    #[test]
    fn encode_key_input_press_matches_encode_key() {
        // A pressed record encodes exactly as the equivalent `encode_key`.
        let record = key_input(
            Key::Char('c'),
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
            true,
        )
        .expect("char has a wire form");
        let mut a = [0u8; MAX_KEY_BYTES];
        let na = encode_key_input(&record, &mut a).expect("fits");
        assert_eq!(&a[..na], &[0x03]); // Ctrl-C
    }

    #[test]
    fn encode_key_input_release_writes_nothing() {
        let record =
            key_input(Key::Char('a'), Modifiers::default(), false).expect("char has a wire form");
        let mut buf = [0u8; MAX_KEY_BYTES];
        assert_eq!(encode_key_input(&record, &mut buf), Ok(0));
    }

    #[test]
    fn key_input_round_trips_named_and_char_through_the_bridge() {
        // A named key built from the `lib/input` vocabulary decodes back to
        // the same key, proving the two halves of the bridge agree.
        for key in [
            Key::Char('Q'),
            Key::Named(NamedKey::Enter),
            Key::Named(NamedKey::Left),
            Key::Named(NamedKey::Function { number: 7 }),
        ] {
            let record = key_input(key, Modifiers::default(), true).expect("wire form");
            assert_eq!(key_from_abi(record.key().expect("a key edge")), key);
        }
    }

    #[test]
    fn key_input_rejects_an_out_of_range_function_key() {
        // No wire `NamedKeyCode` exists past F12, so the edge has no record
        // (fail closed) rather than a guessed encoding.
        assert_eq!(
            key_input(
                Key::Named(NamedKey::Function { number: 13 }),
                Modifiers::default(),
                true,
            ),
            None
        );
    }
}
