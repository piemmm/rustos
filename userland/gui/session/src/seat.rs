//! Backing the desktop's input channels with the kernel seat registry.
//!
//! [`DeviceInputSource`](crate::DeviceInputSource) and
//! [`KeyboardInputSource`](crate::KeyboardInputSource) decode fixed-width
//! input records taken from a [`PointerInputChannel`] / [`KeyInputChannel`]
//! seam. This module is the *kernel* backing for those seams:
//! [`SeatInputChannel`] drains each record from the per-seat, owner-gated
//! channel the kernel seat registry routed the desktop's input to
//! (`plans/DISPLAY.md`; `plans/PI.md` P11 — input follows the surface
//! owner).
//!
//! On a running system the records arrive through the seat-addressed
//! [`POINTER_READ`](rustos_abi::SyscallNumber::POINTER_READ) /
//! [`KEYBOARD_READ`](rustos_abi::SyscallNumber::KEYBOARD_READ) syscalls
//! (`rustos_rt::pointer_read` / `rustos_rt::keyboard_read`); those calls are
//! wrapped behind the injected [`SeatEventReader`] seam, so this
//! `userland/gui` crate holds no seat lease of its own and stays
//! host-testable. Tests back the seam with an in-memory queue.
//!
//! The security properties live kernel-side, not here: each drain is gated
//! on `CAP_INPUT_READ` **and** owner-gated against the seat's live lease, so
//! only the task that acquired the seat (the window manager session) ever
//! receives the stream — another `CAP_INPUT_READ` holder cannot siphon it.
//! The channel's own job is narrow and fail-closed: a whole record
//! ([`WIRE_LEN`](rustos_abi::input::PointerInput::WIRE_LEN) bytes) is handed
//! to the decoder, an empty drain is `None`, and a reader that produces a
//! partial record surfaces an [`Errno`] rather than being misinterpreted —
//! a truncated read can never be decoded as a spurious pointer move or key
//! press.
//!
//! The same shape serves both input kinds — a pointer record and a key
//! record are each a fixed-width drain from the caller's own seat — so
//! [`SeatInputChannel`] implements both seam traits through one shared
//! validation path; which records flow is decided by the reader it wraps
//! (a pointer reader or a keyboard reader), not by the channel type.

use rustos_abi::input::{KeyInput, PointerInput};
use rustos_abi::Errno;

use crate::device::PointerInputChannel;
use crate::keyboard::KeyInputChannel;

/// A source of fixed-width input records drained from the kernel seat
/// registry.
///
/// On a running system this wraps one seat-addressed drain syscall for the
/// seat the desktop session owns — `rustos_rt::pointer_read` for a pointer
/// channel, `rustos_rt::keyboard_read` for a keyboard channel — mapping a
/// negative return onto its [`Errno`]; tests back it with an in-memory
/// queue. It deals only in raw record bytes: validating the drained length
/// and decoding are [`SeatInputChannel`]'s and the input sources' jobs.
pub trait SeatEventReader {
    /// Drain the next pending record into `buf`, returning the number of
    /// bytes written — one whole record, or `0` when the channel is
    /// momentarily empty.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the drain itself is
    /// refused (for example the caller lost the seat lease:
    /// [`Errno::SeatNotOwner`] / [`Errno::SeatRevoked`]).
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno>;
}

/// An input channel that drains fixed-width records from the kernel seat
/// registry through an injected [`SeatEventReader`].
///
/// Construct one with [`new`](Self::new), wrapping the reader for the record
/// kind it drains, then hand it to the matching input source: a pointer
/// reader in [`DeviceInputSource`](crate::DeviceInputSource), a keyboard
/// reader in [`KeyboardInputSource`](crate::KeyboardInputSource). It
/// implements both [`PointerInputChannel`] and [`KeyInputChannel`] through
/// one shared validation path; which records flow is decided by the reader
/// it wraps, not by the channel type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeatInputChannel<R> {
    reader: R,
}

impl<R> SeatInputChannel<R> {
    /// Wrap `reader`, the seat drain the channel takes its records from.
    #[must_use]
    pub const fn new(reader: R) -> Self {
        Self { reader }
    }

    /// The underlying reader.
    #[must_use]
    pub const fn reader(&self) -> &R {
        &self.reader
    }

    /// The underlying reader, mutably.
    pub fn reader_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Consume the channel, returning the reader it wrapped.
    #[must_use]
    pub fn into_reader(self) -> R {
        self.reader
    }
}

impl<R: SeatEventReader> SeatInputChannel<R> {
    /// Drain one record and return its `N` bytes.
    ///
    /// Returns `Ok(None)` when the channel is momentarily empty, and fails
    /// closed on any inconsistency:
    ///
    /// * a refused drain → the reader's [`Errno`] (for example
    ///   [`Errno::SeatNotOwner`] after losing the lease);
    /// * a drain of any length other than exactly `N` →
    ///   [`Errno::LengthOutOfRange`] — a partial record is never handed to
    ///   the decoder.
    fn drain_record<const N: usize>(&mut self) -> Result<Option<[u8; N]>, Errno> {
        let mut record = [0u8; N];
        let read = self.reader.read(&mut record)?;
        if read == 0 {
            return Ok(None);
        }
        if read != N {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Some(record))
    }
}

impl<R: SeatEventReader> PointerInputChannel for SeatInputChannel<R> {
    fn next_record(&mut self) -> Result<Option<[u8; PointerInput::WIRE_LEN]>, Errno> {
        self.drain_record::<{ PointerInput::WIRE_LEN }>()
    }
}

impl<R: SeatEventReader> KeyInputChannel for SeatInputChannel<R> {
    fn next_record(&mut self) -> Result<Option<[u8; KeyInput::WIRE_LEN]>, Errno> {
        self.drain_record::<{ KeyInput::WIRE_LEN }>()
    }
}

#[cfg(test)]
mod tests {
    use super::{SeatEventReader, SeatInputChannel};
    use crate::device::{DeviceInputSource, PointerInputChannel};
    use crate::keyboard::KeyboardInputSource;
    use crate::shell::InputSource;
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;
    use rustos_abi::input::{
        KeyInput, KeyValue, Modifiers as AbiModifiers, NamedKeyCode, PointerButtonCode,
        PointerInput,
    };
    use rustos_abi::Errno;
    use rustos_wm::{InputEvent, Key, NamedKey, Point};

    /// An in-memory reader that yields queued records, optionally faulting
    /// once before the next drain.
    struct QueueReader {
        records: VecDeque<Vec<u8>>,
        fault: Option<Errno>,
    }

    impl QueueReader {
        fn new() -> Self {
            Self {
                records: VecDeque::new(),
                fault: None,
            }
        }

        fn push(&mut self, record: Vec<u8>) {
            self.records.push_back(record);
        }

        fn fault_with(&mut self, errno: Errno) {
            self.fault = Some(errno);
        }
    }

    impl SeatEventReader for QueueReader {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            if let Some(errno) = self.fault.take() {
                return Err(errno);
            }
            match self.records.pop_front() {
                None => Ok(0),
                Some(record) => {
                    let n = record.len().min(buf.len());
                    buf[..n].copy_from_slice(&record[..n]);
                    Ok(n)
                }
            }
        }
    }

    fn pointer_channel() -> SeatInputChannel<QueueReader> {
        SeatInputChannel::new(QueueReader::new())
    }

    #[test]
    fn drains_a_pointer_move_from_the_seat_channel() {
        let mut channel = pointer_channel();
        let record = PointerInput::Moved { x: 7, y: -3 };
        channel.reader_mut().push(record.to_le_bytes().to_vec());
        let mut source = DeviceInputSource::new(channel);
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::PointerMoved {
                to: Point::new(7, -3)
            }))
        );
        assert_eq!(source.poll(), Ok(None));
    }

    #[test]
    fn drains_a_pointer_press_from_the_seat_channel() {
        let mut channel = pointer_channel();
        let record = PointerInput::Pressed(PointerButtonCode::Secondary);
        channel.reader_mut().push(record.to_le_bytes().to_vec());
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Ok(Some(record.to_le_bytes()))
        );
    }

    #[test]
    fn drains_a_key_press_from_the_seat_channel() {
        let mut channel = SeatInputChannel::new(QueueReader::new());
        let record = KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::Enter),
            modifiers: AbiModifiers {
                ctrl: true,
                ..AbiModifiers::default()
            },
        };
        channel.reader_mut().push(record.to_le_bytes().to_vec());
        let mut source = KeyboardInputSource::new(channel);
        assert_eq!(
            source.poll(),
            Ok(Some(InputEvent::KeyPressed {
                key: Key::Named(NamedKey::Enter),
                modifiers: rustos_wm::Modifiers {
                    ctrl: true,
                    ..rustos_wm::Modifiers::default()
                },
            }))
        );
        assert_eq!(source.poll(), Ok(None));
    }

    #[test]
    fn drained_channel_yields_none() {
        let mut channel = pointer_channel();
        assert_eq!(PointerInputChannel::next_record(&mut channel), Ok(None));
    }

    #[test]
    fn reader_fault_propagates_then_recovers() {
        let mut channel = pointer_channel();
        let record = PointerInput::Moved { x: 1, y: 2 };
        channel.reader_mut().push(record.to_le_bytes().to_vec());
        channel.reader_mut().fault_with(Errno::SeatNotOwner);
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Err(Errno::SeatNotOwner)
        );
        // The one-shot fault clears and the queued record still drains.
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Ok(Some(record.to_le_bytes()))
        );
    }

    #[test]
    fn partial_record_is_refused() {
        // A reader that produces fewer bytes than a whole record fails
        // closed: the truncated bytes are never handed to the decoder.
        let mut channel = pointer_channel();
        channel.reader_mut().push(alloc::vec![1u8, 2, 3, 4]);
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn malformed_record_payload_surfaces_the_decoder_error() {
        // A whole-length record that is structurally invalid (bad pointer
        // magic) surfaces the record decoder's error through the input
        // source, not a spurious event.
        let mut channel = pointer_channel();
        channel
            .reader_mut()
            .push(alloc::vec![0u8; PointerInput::WIRE_LEN]);
        let mut source = DeviceInputSource::new(channel);
        assert_eq!(source.poll(), Err(Errno::BadMagic));
    }

    #[test]
    fn into_reader_returns_the_wrapped_reader() {
        let mut channel = pointer_channel();
        channel
            .reader_mut()
            .push(PointerInput::Moved { x: 0, y: 0 }.to_le_bytes().to_vec());
        let reader = channel.into_reader();
        assert_eq!(reader.records.len(), 1);
    }
}
