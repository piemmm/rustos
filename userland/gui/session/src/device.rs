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
//! The seat channel is deliberately **screen-independent**: a driver injects
//! relative displacements ([`PointerInput::MovedBy`]) and resolved button
//! edges, because only the seat owner — this desktop session, which owns the
//! compositor — knows the screen's pixel extent. [`DeviceInputSource`] is
//! where that policy lives: it holds the absolute pointer position, starts it
//! at the screen's centre, accumulates each displacement with saturating
//! arithmetic, and clamps the result to the screen rectangle, so the pointer
//! can never leave the screen no matter what a (compromised) injector sends.
//! Construction refuses an empty screen outright (fail closed).
//!
//! The raw bytes arrive through an injected [`PointerInputChannel`] seam — a
//! capability-checked kernel input channel on a running system, an in-memory
//! queue in tests — so this `userland/gui` crate holds no
//! input capability of its own and the decode runs above the device, not
//! inside it. Every record is validated by
//! [`PointerInput::from_bytes`] before it becomes an [`InputEvent`]; a
//! malformed record surfaces its [`Errno`] and the shell's
//! [`pump`](crate::DesktopShell::pump) stops without misinterpreting the bytes.
//!
//! [`InputSource`]: crate::InputSource
//! [`InputEvent`]: tairix_wm::InputEvent

use tairix_abi::input::{PointerButtonCode, PointerInput};
use tairix_abi::Errno;
use tairix_wm::{InputEvent, Point, PointerButton, Rect};

use crate::shell::InputSource;

/// A source of framed [`PointerInput`] record bytes from the kernel.
///
/// On a running system this is a capability-checked kernel input channel that
/// hands the desktop one [`PointerInput::WIRE_LEN`]-byte record at a time;
/// tests back it with an in-memory queue. It deals only in
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
/// [`PointerInputChannel`] and resolves them against the screen.
///
/// Wrap a channel with [`new`](Self::new), handing it the compositor's
/// screen rectangle, then give the source to
/// [`DesktopShell::pump`](crate::DesktopShell::pump): each
/// [`poll`](InputSource::poll) reads one record from the channel, decodes
/// it, and — for motion — advances the held pointer position, clamped to
/// the screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceInputSource<C> {
    channel: C,
    /// The screen rectangle every accumulated position is clamped into.
    screen: Rect,
    /// The current absolute pointer position; motion records advance it.
    pointer: Point,
}

impl<C> DeviceInputSource<C> {
    /// Build a device input source over `channel`, resolving motion against
    /// `screen` (the compositor's pixel rectangle). The pointer starts at
    /// the screen's centre.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] when `screen` is empty: a screenless
    /// source could never establish a valid pointer position, so it is
    /// refused at construction rather than misbehaving later (fail closed).
    pub fn new(channel: C, screen: Rect) -> Result<Self, Errno> {
        if screen.is_empty() {
            return Err(Errno::OutOfRange);
        }
        let centre = Point::new(
            screen.left().saturating_add_unsigned(screen.width / 2),
            screen.top().saturating_add_unsigned(screen.height / 2),
        );
        Ok(Self {
            channel,
            screen,
            pointer: centre,
        })
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

    /// The current absolute pointer position.
    #[must_use]
    pub const fn pointer(&self) -> Point {
        self.pointer
    }

    /// Advance the pointer by one displacement, saturating and clamping so
    /// the result always lies on the screen — a hostile or faulty injector
    /// can pin the pointer to an edge, never move it off-screen or wrap it.
    fn displace(&mut self, dx: i32, dy: i32) -> Point {
        let max_x = self.screen.right() - 1;
        let max_y = self.screen.bottom() - 1;
        self.pointer = Point::new(
            self.pointer
                .x
                .saturating_add(dx)
                .clamp(self.screen.left(), max_x),
            self.pointer
                .y
                .saturating_add(dy)
                .clamp(self.screen.top(), max_y),
        );
        self.pointer
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

impl<C: PointerInputChannel> InputSource for DeviceInputSource<C> {
    fn poll(&mut self) -> Result<Option<InputEvent>, Errno> {
        match self.channel.next_record()? {
            None => Ok(None),
            Some(bytes) => Ok(Some(match PointerInput::from_bytes(&bytes)? {
                PointerInput::MovedBy { dx, dy } => InputEvent::PointerMoved {
                    to: self.displace(dx, dy),
                },
                PointerInput::Pressed(button) => InputEvent::PointerPressed {
                    button: pointer_button(button),
                },
                PointerInput::Released(button) => InputEvent::PointerReleased {
                    button: pointer_button(button),
                },
                // A scroll is a delta at the current pointer position, not a
                // move: the pointer stays put and the router routes the ticks
                // to the viewport under it.
                PointerInput::Scrolled { dx, dy } => InputEvent::PointerScrolled { dx, dy },
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceInputSource, PointerInputChannel};
    use crate::InputSource;
    use alloc::collections::VecDeque;
    use tairix_abi::input::{PointerButtonCode, PointerInput};
    use tairix_abi::Errno;
    use tairix_wm::{InputEvent, Point, PointerButton, Rect};

    /// The screen the tests resolve motion against: 640×480 at the origin,
    /// so the pointer starts at its centre (320, 240).
    const SCREEN: Rect = Rect::new(0, 0, 640, 480);

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

    fn source(events: &[PointerInput]) -> DeviceInputSource<QueueChannel> {
        DeviceInputSource::new(QueueChannel::new(events), SCREEN).expect("non-empty screen")
    }

    #[test]
    fn empty_screen_is_refused_at_construction() {
        assert_eq!(
            DeviceInputSource::new(QueueChannel::new(&[]), Rect::EMPTY).map(|_| ()),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn pointer_starts_at_the_screen_centre() {
        let source = source(&[]);
        assert_eq!(source.pointer(), Point::new(320, 240));
    }

    #[test]
    fn displacements_accumulate_into_an_absolute_position() {
        let mut source = source(&[
            PointerInput::MovedBy { dx: 12, dy: -5 },
            PointerInput::MovedBy { dx: -2, dy: 0 },
        ]);
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerMoved {
                to: Point::new(332, 235)
            }))
        );
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerMoved {
                to: Point::new(330, 235)
            }))
        );
        assert_eq!(source.poll(), Ok(None));
    }

    #[test]
    fn motion_is_clamped_to_the_screen() {
        // A displacement past every edge — including i32 extremes, which
        // must saturate rather than wrap — pins the pointer to the edge.
        let mut source = source(&[
            PointerInput::MovedBy {
                dx: i32::MIN,
                dy: i32::MIN,
            },
            PointerInput::MovedBy {
                dx: i32::MAX,
                dy: i32::MAX,
            },
        ]);
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerMoved {
                to: Point::new(0, 0)
            }))
        );
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerMoved {
                to: Point::new(639, 479)
            }))
        );
    }

    #[test]
    fn decodes_each_button_for_press_and_release() {
        let events = [
            PointerInput::Pressed(PointerButtonCode::Primary),
            PointerInput::Released(PointerButtonCode::Secondary),
            PointerInput::Pressed(PointerButtonCode::Middle),
        ];
        let mut source = source(&events);
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
    fn buttons_do_not_move_the_pointer() {
        let mut source = source(&[PointerInput::Pressed(PointerButtonCode::Primary)]);
        let before = source.pointer();
        assert!(matches!(
            source.poll(),
            Ok(Some(InputEvent::PointerPressed { .. }))
        ));
        assert_eq!(source.pointer(), before);
    }

    #[test]
    fn scroll_maps_to_ticks_and_does_not_move_the_pointer() {
        let mut source = source(&[PointerInput::Scrolled { dx: -1, dy: 4 }]);
        let before = source.pointer();
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerScrolled { dx: -1, dy: 4 }))
        );
        assert_eq!(source.pointer(), before);
    }

    #[test]
    fn malformed_record_surfaces_bad_magic() {
        let mut channel = QueueChannel::new(&[]);
        channel.push_raw([0u8; PointerInput::WIRE_LEN]);
        let mut source = DeviceInputSource::new(channel, SCREEN).expect("non-empty screen");
        // An all-zero record has the wrong magic and must be refused, never
        // misinterpreted.
        assert_eq!(source.poll(), Err(Errno::BadMagic));
    }

    #[test]
    fn channel_fault_propagates() {
        let mut channel = QueueChannel::new(&[PointerInput::MovedBy { dx: 1, dy: 2 }]);
        channel.fault_with(Errno::NotFound);
        let mut source = DeviceInputSource::new(channel, SCREEN).expect("non-empty screen");
        assert_eq!(source.poll(), Err(Errno::NotFound));
        // After the one-shot fault clears, the queued record still decodes.
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerMoved {
                to: Point::new(321, 242)
            }))
        );
    }

    #[test]
    fn into_channel_returns_the_wrapped_channel() {
        let source = source(&[PointerInput::MovedBy { dx: 0, dy: 0 }]);
        let channel = source.into_channel();
        assert_eq!(channel.records.len(), 1);
    }
}
