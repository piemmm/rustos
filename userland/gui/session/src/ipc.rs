//! Backing the desktop's input channels with a kernel IPC endpoint.
//!
//! [`DeviceInputSource`](crate::DeviceInputSource) and
//! [`KeyboardInputSource`](crate::KeyboardInputSource) decode framed input
//! records taken from a [`PointerInputChannel`] / [`KeyInputChannel`] seam,
//! but until now the only backing for those seams was an in-memory test
//! queue. This module is the *kernel* backing: [`IpcInputChannel`] delivers
//! each record as the payload of an `abi-v1` IPC message received from a
//! bound kernel endpoint.
//!
//! On a running system the raw messages arrive through the
//! [`SyscallNumber::IPC_RECV`](rustos_abi::SyscallNumber::IPC_RECV) syscall;
//! that syscall is wrapped behind the injected [`MessagePort`] seam so this
//! `userland/gui` crate holds no endpoint capability of its own and the
//! framing runs above the kernel boundary, not inside it (`AGENTS.md` §17.4 /
//! §19.5). Tests back the seam with an in-memory queue (`AGENTS.md` §7).
//!
//! Each received message is validated before its payload becomes a record:
//! the [`IpcMessageHeader`] must decode (magic, ABI version, reserved field,
//! bounded payload length), the message must be destined for the endpoint the
//! channel is bound to, and the payload must be exactly the record's
//! [`WIRE_LEN`](rustos_abi::input::PointerInput::WIRE_LEN). A message that
//! fails any check surfaces its [`Errno`] rather than being misinterpreted, so
//! a truncated, misrouted, or corrupt frame can never be decoded as a spurious
//! pointer move or key press (`AGENTS.md` §5.4 / §2.9).
//!
//! The same framing serves both input kinds — a pointer record and a key
//! record are each a fixed-length payload behind one IPC header — so
//! [`IpcInputChannel`] implements both seam traits through one shared
//! validation path rather than two (`AGENTS.md` §2.2); a given channel is
//! bound to one endpoint and wrapped in the matching input source.

use alloc::vec::Vec;

use rustos_abi::input::{KeyInput, PointerInput};
use rustos_abi::ipc::IpcMessageHeader;
use rustos_abi::Errno;

use crate::device::PointerInputChannel;
use crate::keyboard::KeyInputChannel;

/// A source of raw `abi-v1` IPC message bytes from a bound kernel endpoint.
///
/// On a running system this wraps the
/// [`SyscallNumber::IPC_RECV`](rustos_abi::SyscallNumber::IPC_RECV) syscall for
/// the endpoint the desktop bound its input channel to; tests back it with an
/// in-memory queue (`AGENTS.md` §7). It deals only in raw bytes — validating
/// and framing them is [`IpcInputChannel`]'s job — so the port itself need not
/// understand the input wire formats.
pub trait MessagePort {
    /// Copy the next pending message (header followed by payload) into `buf`
    /// and return its length in bytes, or `None` when the endpoint is
    /// momentarily drained.
    ///
    /// The implementation writes at most `buf.len()` bytes; a message longer
    /// than `buf` is the caller's sizing error, reported here as
    /// [`Errno::MessageTooLarge`].
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the endpoint itself faults
    /// (for example it was closed). The bytes are not interpreted here; a
    /// short, misrouted, or corrupt message is [`IpcInputChannel`]'s concern.
    fn recv(&mut self, buf: &mut [u8]) -> Result<Option<usize>, Errno>;
}

/// An input channel that frames fixed-length records out of IPC messages
/// received from a bound kernel endpoint.
///
/// Construct one with [`new`](Self::new), binding it to the `endpoint` the
/// kernel routes the device's input messages to, then wrap it in the matching
/// input source: a pointer endpoint in
/// [`DeviceInputSource`](crate::DeviceInputSource), a keyboard endpoint in
/// [`KeyboardInputSource`](crate::KeyboardInputSource). It implements both
/// [`PointerInputChannel`] and [`KeyInputChannel`] through one shared
/// validation path (`AGENTS.md` §2.2); which records flow is decided by the
/// endpoint it is bound to, not by the channel type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IpcInputChannel<P> {
    port: P,
    endpoint: u64,
    buf: Vec<u8>,
}

impl<P> IpcInputChannel<P> {
    /// Bind a channel to `endpoint`, receiving its messages through `port`.
    #[must_use]
    pub const fn new(port: P, endpoint: u64) -> Self {
        Self {
            port,
            endpoint,
            buf: Vec::new(),
        }
    }

    /// The endpoint this channel is bound to.
    #[must_use]
    pub const fn endpoint(&self) -> u64 {
        self.endpoint
    }

    /// The underlying message port.
    #[must_use]
    pub const fn port(&self) -> &P {
        &self.port
    }

    /// The underlying message port, mutably.
    pub fn port_mut(&mut self) -> &mut P {
        &mut self.port
    }

    /// Consume the channel, returning the port it wrapped.
    #[must_use]
    pub fn into_port(self) -> P {
        self.port
    }
}

impl<P: MessagePort> IpcInputChannel<P> {
    /// Receive one message and return its `N`-byte payload as a record.
    ///
    /// Returns `Ok(None)` when the endpoint is drained, and fails closed
    /// (`AGENTS.md` §5.4 / §2.9) on any inconsistency:
    ///
    /// * a frame too short to hold the header and an `N`-byte payload →
    ///   [`Errno::BufferTooSmall`];
    /// * a header that does not decode → its [`IpcMessageHeader::from_bytes`]
    ///   error;
    /// * a payload shorter than `N` → [`Errno::BufferTooSmall`], longer than
    ///   `N` → [`Errno::MessageTooLarge`];
    /// * a message destined for another endpoint → [`Errno::NotFound`].
    fn recv_record<const N: usize>(&mut self) -> Result<Option<[u8; N]>, Errno> {
        let frame_len = IpcMessageHeader::WIRE_LEN + N;
        self.buf.clear();
        self.buf.resize(frame_len, 0);
        let Some(received) = self.port.recv(&mut self.buf)? else {
            return Ok(None);
        };
        if received < frame_len {
            return Err(Errno::BufferTooSmall);
        }
        let header = IpcMessageHeader::from_bytes(&self.buf)?;
        if header.endpoint != self.endpoint {
            return Err(Errno::NotFound);
        }
        let payload_len = header.payload_len as usize;
        if payload_len < N {
            return Err(Errno::BufferTooSmall);
        }
        if payload_len > N {
            return Err(Errno::MessageTooLarge);
        }
        let mut record = [0u8; N];
        record.copy_from_slice(&self.buf[IpcMessageHeader::WIRE_LEN..frame_len]);
        Ok(Some(record))
    }
}

impl<P: MessagePort> PointerInputChannel for IpcInputChannel<P> {
    fn next_record(&mut self) -> Result<Option<[u8; PointerInput::WIRE_LEN]>, Errno> {
        self.recv_record::<{ PointerInput::WIRE_LEN }>()
    }
}

impl<P: MessagePort> KeyInputChannel for IpcInputChannel<P> {
    fn next_record(&mut self) -> Result<Option<[u8; KeyInput::WIRE_LEN]>, Errno> {
        self.recv_record::<{ KeyInput::WIRE_LEN }>()
    }
}

#[cfg(test)]
mod tests {
    use super::{IpcInputChannel, MessagePort};
    use crate::device::{DeviceInputSource, PointerInputChannel};
    use crate::keyboard::{KeyInputChannel, KeyboardInputSource};
    use crate::shell::InputSource;
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;
    use rustos_abi::input::{KeyInput, KeyValue, Modifiers as AbiModifiers, PointerButtonCode};
    use rustos_abi::input::{NamedKeyCode, PointerInput};
    use rustos_abi::ipc::{IpcMessageHeader, IPC_MESSAGE_HEADER_MAGIC};
    use rustos_abi::{Errno, ABI_VERSION_CURRENT_U16};
    use rustos_wm::{InputEvent, Key, NamedKey, Point};

    const POINTER_ENDPOINT: u64 = 0x0011_2233_4455_6677;
    const KEY_ENDPOINT: u64 = 0x8899_AABB_CCDD_EEFF;

    /// Frame a payload into a full `abi-v1` IPC message for `endpoint`.
    fn frame(endpoint: u64, payload: &[u8]) -> Vec<u8> {
        let header = IpcMessageHeader {
            magic: IPC_MESSAGE_HEADER_MAGIC,
            version: ABI_VERSION_CURRENT_U16,
            flags: 0,
            endpoint,
            sender: 0,
            #[allow(clippy::cast_possible_truncation)]
            payload_len: payload.len() as u32,
            reserved: 0,
        };
        let mut bytes = header.to_le_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    /// An in-memory port that yields queued message frames, optionally
    /// faulting once before the next frame.
    struct QueuePort {
        frames: VecDeque<Vec<u8>>,
        fault: Option<Errno>,
    }

    impl QueuePort {
        fn new() -> Self {
            Self {
                frames: VecDeque::new(),
                fault: None,
            }
        }

        fn push(&mut self, frame: Vec<u8>) {
            self.frames.push_back(frame);
        }

        fn fault_with(&mut self, errno: Errno) {
            self.fault = Some(errno);
        }
    }

    impl MessagePort for QueuePort {
        fn recv(&mut self, buf: &mut [u8]) -> Result<Option<usize>, Errno> {
            if let Some(errno) = self.fault.take() {
                return Err(errno);
            }
            match self.frames.pop_front() {
                None => Ok(None),
                Some(frame) => {
                    if frame.len() > buf.len() {
                        return Err(Errno::MessageTooLarge);
                    }
                    buf[..frame.len()].copy_from_slice(&frame);
                    Ok(Some(frame.len()))
                }
            }
        }
    }

    fn pointer_channel() -> IpcInputChannel<QueuePort> {
        IpcInputChannel::new(QueuePort::new(), POINTER_ENDPOINT)
    }

    #[test]
    fn frames_a_pointer_move_from_an_ipc_message() {
        let mut channel = pointer_channel();
        let record = PointerInput::Moved { x: 7, y: -3 };
        channel
            .port_mut()
            .push(frame(POINTER_ENDPOINT, &record.to_le_bytes()));
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
    fn frames_a_pointer_press_from_an_ipc_message() {
        let mut channel = pointer_channel();
        let record = PointerInput::Pressed(PointerButtonCode::Secondary);
        channel
            .port_mut()
            .push(frame(POINTER_ENDPOINT, &record.to_le_bytes()));
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Ok(Some(record.to_le_bytes()))
        );
    }

    #[test]
    fn frames_a_key_press_from_an_ipc_message() {
        let mut channel = IpcInputChannel::new(QueuePort::new(), KEY_ENDPOINT);
        let record = KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::Enter),
            modifiers: AbiModifiers {
                ctrl: true,
                ..AbiModifiers::default()
            },
        };
        channel
            .port_mut()
            .push(frame(KEY_ENDPOINT, &record.to_le_bytes()));
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
    fn drained_endpoint_yields_none() {
        let mut channel = pointer_channel();
        assert_eq!(PointerInputChannel::next_record(&mut channel), Ok(None));
    }

    #[test]
    fn port_fault_propagates_then_recovers() {
        let mut channel = pointer_channel();
        let record = PointerInput::Moved { x: 1, y: 2 };
        channel
            .port_mut()
            .push(frame(POINTER_ENDPOINT, &record.to_le_bytes()));
        channel.port_mut().fault_with(Errno::PermissionDenied);
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Err(Errno::PermissionDenied)
        );
        // The one-shot fault clears and the queued frame still decodes.
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Ok(Some(record.to_le_bytes()))
        );
    }

    #[test]
    fn corrupt_header_is_refused() {
        let mut channel = pointer_channel();
        let mut bytes = frame(
            POINTER_ENDPOINT,
            &PointerInput::Moved { x: 0, y: 0 }.to_le_bytes(),
        );
        bytes[0] ^= 0xFF; // break the IPC magic
        channel.port_mut().push(bytes);
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn message_for_another_endpoint_is_refused() {
        let mut channel = pointer_channel();
        let record = PointerInput::Moved { x: 4, y: 5 };
        // Framed for a different endpoint than the channel is bound to.
        channel
            .port_mut()
            .push(frame(POINTER_ENDPOINT ^ 0x1, &record.to_le_bytes()));
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Err(Errno::NotFound)
        );
    }

    #[test]
    fn oversize_payload_is_refused() {
        let mut channel = pointer_channel();
        let mut payload = PointerInput::Moved { x: 0, y: 0 }.to_le_bytes().to_vec();
        payload.push(0); // one byte too long for a pointer record
        channel.port_mut().push(frame(POINTER_ENDPOINT, &payload));
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Err(Errno::MessageTooLarge)
        );
    }

    #[test]
    fn truncated_frame_is_refused() {
        let mut channel = pointer_channel();
        let mut bytes = frame(
            POINTER_ENDPOINT,
            &PointerInput::Moved { x: 0, y: 0 }.to_le_bytes(),
        );
        bytes.truncate(IpcMessageHeader::WIRE_LEN + PointerInput::WIRE_LEN - 1);
        channel.port_mut().push(bytes);
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn into_port_returns_the_wrapped_port() {
        let mut channel = pointer_channel();
        channel.port_mut().push(frame(
            POINTER_ENDPOINT,
            &PointerInput::Moved { x: 0, y: 0 }.to_le_bytes(),
        ));
        assert_eq!(channel.endpoint(), POINTER_ENDPOINT);
        let port = channel.into_port();
        assert_eq!(port.frames.len(), 1);
    }

    #[test]
    fn short_payload_is_refused() {
        // A frame whose advertised payload is shorter than a pointer record:
        // the header says 4 bytes but the channel needs WIRE_LEN, and because
        // the full frame is then shorter than header+WIRE_LEN it is rejected
        // as truncated before the payload-length check is reached.
        let mut channel = pointer_channel();
        channel
            .port_mut()
            .push(frame(POINTER_ENDPOINT, &[1, 2, 3, 4]));
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Err(Errno::BufferTooSmall)
        );

        // Now a frame that *is* long enough overall but advertises a short
        // payload, so the payload-length check fails closed.
        let mut padded = frame(POINTER_ENDPOINT, &[1, 2, 3, 4]);
        padded.resize(IpcMessageHeader::WIRE_LEN + PointerInput::WIRE_LEN, 0);
        let mut channel = pointer_channel();
        channel.port_mut().push(padded);
        assert_eq!(
            PointerInputChannel::next_record(&mut channel),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn unbound_button_press_payload_is_refused() {
        // A correctly framed message whose payload is a structurally invalid
        // pointer record (bad pointer magic) surfaces the record decoder's
        // error, not a spurious event.
        let mut channel = pointer_channel();
        channel
            .port_mut()
            .push(frame(POINTER_ENDPOINT, &[0u8; PointerInput::WIRE_LEN]));
        let mut source = DeviceInputSource::new(channel);
        assert_eq!(source.poll(), Err(Errno::BadMagic));
    }

    #[test]
    fn key_channel_rejects_pointer_endpoint_message() {
        // Binding a channel to the key endpoint but feeding it a pointer
        // endpoint's frame is refused — the framing is shared but the binding
        // is not (`AGENTS.md` §5.4).
        let mut channel = IpcInputChannel::new(QueuePort::new(), KEY_ENDPOINT);
        let record = KeyInput::Released {
            key: KeyValue::Char('q'),
            modifiers: AbiModifiers::default(),
        };
        channel
            .port_mut()
            .push(frame(POINTER_ENDPOINT, &record.to_le_bytes()));
        assert_eq!(
            KeyInputChannel::next_record(&mut channel),
            Err(Errno::NotFound)
        );
    }
}
