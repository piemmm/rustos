//! Backing the desktop's [`InputSource`] with a live device channel.
//!
//! [`DesktopShell`](crate::DesktopShell) drives the desktop by
//! [`pump`](crate::DesktopShell::pump)ing an injected [`InputSource`] — the
//! one seam through which pointer events reach the desktop. This module is the
//! *live* backing for that seam: [`DeviceInputSource`] reads framed
//! [`PointerInput`] records from a kernel input channel and decodes each into
//! the desktop's `lib/input` [`InputEvent`] vocabulary the window manager and
//! taskbar route.
//!
//! The raw bytes arrive through an injected [`PointerInputChannel`] seam — a
//! capability-checked kernel input channel on a running system, an in-memory
//! queue in tests (`AGENTS.md` §7) — so this `userland/gui` crate holds no
//! input capability of its own and the decode runs above the device, not
//! inside it (§17.4 / §19.5). Every record is validated by
//! [`PointerInput::from_bytes`] before it becomes an [`InputEvent`]; a
//! malformed record surfaces its [`Errno`] and the shell's
//! [`pump`](crate::DesktopShell::pump) stops without misinterpreting the bytes
//! (`AGENTS.md` §5.4 / §2.9).
//!
//! [`InputSource`]: crate::InputSource
//! [`InputEvent`]: rustos_wm::InputEvent

use rustos_abi::input::{PointerButtonCode, PointerInput};
use rustos_abi::Errno;
use rustos_wm::{InputEvent, Point, PointerButton};

use crate::shell::InputSource;

/// A source of framed [`PointerInput`] record bytes from the kernel.
///
/// On a running system this is a capability-checked kernel input channel that
/// hands the desktop one [`PointerInput::WIRE_LEN`]-byte record at a time;
/// tests back it with an in-memory queue (`AGENTS.md` §7). It deals only in
/// raw bytes: decoding and validating them is [`DeviceInputSource`]'s job, so
/// the channel itself need not understand the wire format.
pub trait PointerInputChannel {
    /// Take the next pending record's bytes, or `None` when the channel is
    /// momentarily drained.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the channel itself faults
    /// (for example it was closed). The bytes are not interpreted here; a
    /// short or corrupt record is the decoder's concern, not the channel's.
    fn next_record(&mut self) -> Result<Option<[u8; PointerInput::WIRE_LEN]>, Errno>;
}

/// An [`InputSource`] that decodes [`PointerInput`] records from a
/// [`PointerInputChannel`].
///
/// Wrap a channel with [`new`](Self::new), then hand the source to
/// [`DesktopShell::pump`](crate::DesktopShell::pump): each
/// [`poll`](InputSource::poll) reads one record from the channel and decodes
/// it into an [`InputEvent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceInputSource<C> {
    channel: C,
}

impl<C> DeviceInputSource<C> {
    /// Build a device input source over `channel`.
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

/// Map a decoded [`PointerButtonCode`] to the desktop's [`PointerButton`].
///
/// The two enumerations are deliberately separate — the first is the frozen
/// ABI wire code, the second the `lib/input` routing vocabulary — and this is
/// the single place the desktop crosses between them.
const fn pointer_button(code: PointerButtonCode) -> PointerButton {
    match code {
        PointerButtonCode::Primary => PointerButton::Primary,
        PointerButtonCode::Secondary => PointerButton::Secondary,
        PointerButtonCode::Middle => PointerButton::Middle,
    }
}

/// Translate a decoded ABI [`PointerInput`] into the desktop [`InputEvent`].
fn to_input_event(record: PointerInput) -> InputEvent {
    match record {
        PointerInput::Moved { x, y } => InputEvent::PointerMoved {
            to: Point::new(x, y),
        },
        PointerInput::Pressed(button) => InputEvent::PointerPressed {
            button: pointer_button(button),
        },
        PointerInput::Released(button) => InputEvent::PointerReleased {
            button: pointer_button(button),
        },
    }
}

impl<C: PointerInputChannel> InputSource for DeviceInputSource<C> {
    fn poll(&mut self) -> Result<Option<InputEvent>, Errno> {
        match self.channel.next_record()? {
            None => Ok(None),
            Some(bytes) => Ok(Some(to_input_event(PointerInput::from_bytes(&bytes)?))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceInputSource, PointerInputChannel};
    use crate::InputSource;
    use alloc::collections::VecDeque;
    use rustos_abi::input::{PointerButtonCode, PointerInput};
    use rustos_abi::Errno;
    use rustos_wm::{InputEvent, Point, PointerButton};

    /// An in-memory channel that yields queued records, optionally faulting.
    struct QueueChannel {
        records: VecDeque<[u8; PointerInput::WIRE_LEN]>,
        fault: Option<Errno>,
    }

    impl QueueChannel {
        fn new(events: &[PointerInput]) -> Self {
            Self {
                records: events.iter().map(PointerInput::to_le_bytes).collect(),
                fault: None,
            }
        }

        fn push_raw(&mut self, bytes: [u8; PointerInput::WIRE_LEN]) {
            self.records.push_back(bytes);
        }

        fn fault_with(&mut self, errno: Errno) {
            self.fault = Some(errno);
        }
    }

    impl PointerInputChannel for QueueChannel {
        fn next_record(&mut self) -> Result<Option<[u8; PointerInput::WIRE_LEN]>, Errno> {
            if let Some(errno) = self.fault.take() {
                return Err(errno);
            }
            Ok(self.records.pop_front())
        }
    }

    #[test]
    fn decodes_moved_to_absolute_point() {
        let mut source =
            DeviceInputSource::new(QueueChannel::new(&[PointerInput::Moved { x: 12, y: -5 }]));
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerMoved {
                to: Point::new(12, -5)
            }))
        );
        assert_eq!(source.poll(), Ok(None));
    }

    #[test]
    fn decodes_each_button_for_press_and_release() {
        let events = [
            PointerInput::Pressed(PointerButtonCode::Primary),
            PointerInput::Released(PointerButtonCode::Secondary),
            PointerInput::Pressed(PointerButtonCode::Middle),
        ];
        let mut source = DeviceInputSource::new(QueueChannel::new(&events));
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerPressed {
                button: PointerButton::Primary
            }))
        );
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerReleased {
                button: PointerButton::Secondary
            }))
        );
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerPressed {
                button: PointerButton::Middle
            }))
        );
        assert_eq!(source.poll(), Ok(None));
    }

    #[test]
    fn malformed_record_surfaces_bad_magic() {
        let mut channel = QueueChannel::new(&[]);
        channel.push_raw([0u8; PointerInput::WIRE_LEN]);
        let mut source = DeviceInputSource::new(channel);
        // An all-zero record has the wrong magic and must be refused, never
        // misinterpreted (`AGENTS.md` §5.4 / §2.9).
        assert_eq!(source.poll(), Err(Errno::BadMagic));
    }

    #[test]
    fn channel_fault_propagates() {
        let mut channel = QueueChannel::new(&[PointerInput::Moved { x: 1, y: 2 }]);
        channel.fault_with(Errno::NotFound);
        let mut source = DeviceInputSource::new(channel);
        assert_eq!(source.poll(), Err(Errno::NotFound));
        // After the one-shot fault clears, the queued record still decodes.
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerMoved {
                to: Point::new(1, 2)
            }))
        );
    }

    #[test]
    fn into_channel_returns_the_wrapped_channel() {
        let source =
            DeviceInputSource::new(QueueChannel::new(&[PointerInput::Moved { x: 0, y: 0 }]));
        let channel = source.into_channel();
        assert_eq!(channel.records.len(), 1);
    }
}
