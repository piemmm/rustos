//! Backing the desktop's [`InputSource`] with a live keyboard channel.
//!
//! [`DesktopShell`](crate::DesktopShell) drives the desktop by
//! [`pump`](crate::DesktopShell::pump)ing an injected [`InputSource`]. The
//! pointer's live backing is [`DeviceInputSource`](crate::DeviceInputSource);
//! this module is the keyboard's: [`KeyboardInputSource`] reads framed
//! [`KeyInput`] records from a kernel keyboard channel and decodes each into
//! the desktop's `lib/input` [`InputEvent`] vocabulary the window manager
//! delivers to the focused window.
//!
//! The raw bytes arrive through an injected [`KeyInputChannel`] seam — a
//! capability-checked kernel input channel on a running system, an in-memory
//! queue in tests — so this `userland/gui` crate holds no
//! input capability of its own and the decode runs above the device, not
//! inside it. Every record is validated by
//! [`KeyInput::from_bytes`] before it becomes an [`InputEvent`]; a malformed
//! record surfaces its [`Errno`] and the shell's
//! [`pump`](crate::DesktopShell::pump) stops without misinterpreting the bytes.
//!
//! [`InputSource`]: crate::InputSource
//! [`InputEvent`]: rustos_wm::InputEvent
//! [`DeviceInputSource`]: crate::DeviceInputSource

use rustos_abi::input::{KeyInput, KeyValue, Modifiers as AbiModifiers, NamedKeyCode};
use rustos_abi::Errno;
use rustos_wm::{InputEvent, Key, Modifiers, NamedKey};

use crate::shell::InputSource;

/// A source of framed [`KeyInput`] record bytes from the kernel.
///
/// On a running system this is a capability-checked kernel keyboard channel
/// that hands the desktop one [`KeyInput::WIRE_LEN`]-byte record at a time;
/// tests back it with an in-memory queue. It deals only in
/// raw bytes: decoding and validating them is [`KeyboardInputSource`]'s job,
/// so the channel itself need not understand the wire format.
pub trait KeyInputChannel {
    /// Take the next pending record's bytes, or `None` when the channel is
    /// momentarily drained.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the channel itself faults
    /// (for example it was closed). The bytes are not interpreted here; a
    /// short or corrupt record is the decoder's concern, not the channel's.
    fn next_record(&mut self) -> Result<Option<[u8; KeyInput::WIRE_LEN]>, Errno>;
}

/// An [`InputSource`] that decodes [`KeyInput`] records from a
/// [`KeyInputChannel`].
///
/// Wrap a channel with [`new`](Self::new), then hand the source to
/// [`DesktopShell::pump`](crate::DesktopShell::pump): each
/// [`poll`](InputSource::poll) reads one record from the channel and decodes
/// it into an [`InputEvent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardInputSource<C> {
    channel: C,
}

impl<C> KeyboardInputSource<C> {
    /// Build a keyboard input source over `channel`.
    pub const fn new(channel: C) -> Self {
        Self { channel }
    }

    /// The underlying channel.
    pub const fn channel(&self) -> &C {
        &self.channel
    }

    /// The underlying channel, mutably.
    pub fn channel_mut(&mut self) -> &mut C {
        &mut self.channel
    }

    /// Consume the source, returning the channel it wrapped.
    pub fn into_channel(self) -> C {
        self.channel
    }
}

/// Map the ABI wire [`AbiModifiers`] to the desktop's [`Modifiers`].
///
/// The two structs are deliberately separate — the first is the wire ABI, the
/// second the `lib/input` routing vocabulary — and this is the single place
/// the desktop crosses between them (the same split as `PointerButtonCode` vs
/// `PointerButton`).
const fn modifiers(abi: AbiModifiers) -> Modifiers {
    Modifiers {
        shift: abi.shift,
        ctrl: abi.ctrl,
        alt: abi.alt,
        meta: abi.meta,
    }
}

/// Map an ABI wire [`NamedKeyCode`] to the desktop's [`NamedKey`].
///
/// The wire ABI gives every function key its own discriminant; the routing
/// vocabulary folds them into one [`NamedKey::Function`] carrying the number,
/// so callers match on the family rather than twelve variants.
const fn named_key(code: NamedKeyCode) -> NamedKey {
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

/// Map a decoded ABI [`KeyValue`] to the desktop's [`Key`].
const fn key(value: KeyValue) -> Key {
    match value {
        KeyValue::Char(c) => Key::Char(c),
        KeyValue::Named(named) => Key::Named(named_key(named)),
    }
}

/// Translate a decoded ABI [`KeyInput`] into the desktop [`InputEvent`].
fn to_input_event(record: KeyInput) -> InputEvent {
    let key = key(record.key());
    let modifiers = modifiers(record.modifiers());
    match record {
        KeyInput::Pressed { .. } => InputEvent::KeyPressed { key, modifiers },
        KeyInput::Released { .. } => InputEvent::KeyReleased { key, modifiers },
    }
}

impl<C: KeyInputChannel> InputSource for KeyboardInputSource<C> {
    fn poll(&mut self) -> Result<Option<InputEvent>, Errno> {
        match self.channel.next_record()? {
            None => Ok(None),
            Some(bytes) => Ok(Some(to_input_event(KeyInput::from_bytes(&bytes)?))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyInputChannel, KeyboardInputSource};
    use crate::InputSource;
    use alloc::collections::VecDeque;
    use rustos_abi::input::{KeyInput, KeyValue, Modifiers as AbiModifiers, NamedKeyCode};
    use rustos_abi::Errno;
    use rustos_wm::{InputEvent, Key, Modifiers, NamedKey};

    /// An in-memory channel that yields queued records, optionally faulting.
    struct QueueChannel {
        records: VecDeque<[u8; KeyInput::WIRE_LEN]>,
        fault: Option<Errno>,
    }

    impl QueueChannel {
        fn new(events: &[KeyInput]) -> Self {
            Self {
                records: events.iter().map(KeyInput::to_le_bytes).collect(),
                fault: None,
            }
        }

        fn push_raw(&mut self, bytes: [u8; KeyInput::WIRE_LEN]) {
            self.records.push_back(bytes);
        }

        fn fault_with(&mut self, errno: Errno) {
            self.fault = Some(errno);
        }
    }

    impl KeyInputChannel for QueueChannel {
        fn next_record(&mut self) -> Result<Option<[u8; KeyInput::WIRE_LEN]>, Errno> {
            if let Some(errno) = self.fault.take() {
                return Err(errno);
            }
            Ok(self.records.pop_front())
        }
    }

    #[test]
    fn decodes_char_press_with_modifiers() {
        let mut source = KeyboardInputSource::new(QueueChannel::new(&[KeyInput::Pressed {
            key: KeyValue::Char('z'),
            modifiers: AbiModifiers {
                ctrl: true,
                ..AbiModifiers::default()
            },
        }]));
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::KeyPressed {
                key: Key::Char('z'),
                modifiers: Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                },
            }))
        );
        assert_eq!(source.poll(), Ok(None));
    }

    #[test]
    fn decodes_named_release_and_folds_function_keys() {
        let events = [
            KeyInput::Released {
                key: KeyValue::Named(NamedKeyCode::Escape),
                modifiers: AbiModifiers::default(),
            },
            KeyInput::Pressed {
                key: KeyValue::Named(NamedKeyCode::F5),
                modifiers: AbiModifiers::default(),
            },
        ];
        let mut source = KeyboardInputSource::new(QueueChannel::new(&events));
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::KeyReleased {
                key: Key::Named(NamedKey::Escape),
                modifiers: Modifiers::default(),
            }))
        );
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::KeyPressed {
                key: Key::Named(NamedKey::Function { number: 5 }),
                modifiers: Modifiers::default(),
            }))
        );
        assert_eq!(source.poll(), Ok(None));
    }

    #[test]
    fn malformed_record_surfaces_bad_magic() {
        let mut channel = QueueChannel::new(&[]);
        channel.push_raw([0u8; KeyInput::WIRE_LEN]);
        let mut source = KeyboardInputSource::new(channel);
        // An all-zero record has the wrong magic and must be refused, never
        // misinterpreted.
        assert_eq!(source.poll(), Err(Errno::BadMagic));
    }

    #[test]
    fn channel_fault_propagates() {
        let mut channel = QueueChannel::new(&[KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: AbiModifiers::default(),
        }]);
        channel.fault_with(Errno::NotFound);
        let mut source = KeyboardInputSource::new(channel);
        assert_eq!(source.poll(), Err(Errno::NotFound));
        // After the one-shot fault clears, the queued record still decodes.
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::KeyPressed {
                key: Key::Char('a'),
                modifiers: Modifiers::default(),
            }))
        );
    }

    #[test]
    fn into_channel_returns_the_wrapped_channel() {
        let source = KeyboardInputSource::new(QueueChannel::new(&[KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: AbiModifiers::default(),
        }]));
        let channel = source.into_channel();
        assert_eq!(channel.records.len(), 1);
    }
}
