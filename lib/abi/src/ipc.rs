//! Wire-level header for an IPC message.
//!
//! Every message exchanged through a kernel IPC port begins with an
//! [`IpcMessageHeader`]. The struct is `#[repr(C)]` with explicitly sized
//! fields encoded little-endian on the wire; layout and field order are part
//! of the frozen `abi-v1` contract.

use crate::le::{read_u16, read_u32, read_u64};
use crate::Errno;

/// Magic number identifying an `abi-v1` IPC message (`"IPC1"` little-endian).
pub const IPC_MESSAGE_HEADER_MAGIC: u32 = u32::from_le_bytes(*b"IPC1");

/// Maximum payload length (bytes) advertised by an [`IpcMessageHeader`].
///
/// Bounded so that an attacker cannot trick a receiver into expecting a
/// payload it cannot represent or allocate; far larger than any sensible
/// IPC message.
pub const IPC_MESSAGE_MAX_PAYLOAD_LEN: u32 = 1 << 20;

/// Header carried in front of every IPC message.
///
/// Total wire size is exactly [`IpcMessageHeader::WIRE_LEN`] bytes. The
/// header is encoded little-endian regardless of host architecture: all
/// Tier-1 targets are little-endian, and the explicit encoding lets a future
/// big-endian port participate without breaking the ABI.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct IpcMessageHeader {
    /// Must equal [`IPC_MESSAGE_HEADER_MAGIC`].
    pub magic: u32,
    /// ABI version of the message format.
    pub version: u16,
    /// Implementation-defined flag bits; reserved bits must be zero.
    pub flags: u16,
    /// Destination endpoint identifier in the receiver's address space.
    pub endpoint: u64,
    /// Sender task identifier; filled in by the kernel, ignored on send.
    pub sender: u64,
    /// Length of the payload that follows the header, in bytes.
    pub payload_len: u32,
    /// Reserved; must be zero in `abi-v1`.
    pub reserved: u32,
}

impl IpcMessageHeader {
    /// Encoded size of an [`IpcMessageHeader`] on the wire.
    pub const WIRE_LEN: usize = 32;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub const fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        let magic = self.magic.to_le_bytes();
        let version = self.version.to_le_bytes();
        let flags = self.flags.to_le_bytes();
        let endpoint = self.endpoint.to_le_bytes();
        let sender = self.sender.to_le_bytes();
        let payload_len = self.payload_len.to_le_bytes();
        let reserved = self.reserved.to_le_bytes();
        out[0] = magic[0];
        out[1] = magic[1];
        out[2] = magic[2];
        out[3] = magic[3];
        out[4] = version[0];
        out[5] = version[1];
        out[6] = flags[0];
        out[7] = flags[1];
        out[8] = endpoint[0];
        out[9] = endpoint[1];
        out[10] = endpoint[2];
        out[11] = endpoint[3];
        out[12] = endpoint[4];
        out[13] = endpoint[5];
        out[14] = endpoint[6];
        out[15] = endpoint[7];
        out[16] = sender[0];
        out[17] = sender[1];
        out[18] = sender[2];
        out[19] = sender[3];
        out[20] = sender[4];
        out[21] = sender[5];
        out[22] = sender[6];
        out[23] = sender[7];
        out[24] = payload_len[0];
        out[25] = payload_len[1];
        out[26] = payload_len[2];
        out[27] = payload_len[3];
        out[28] = reserved[0];
        out[29] = reserved[1];
        out[30] = reserved[2];
        out[31] = reserved[3];
        out
    }

    /// Decode `bytes` into an [`IpcMessageHeader`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the magic word does not match.
    /// * [`Errno::AbiVersionUnsupported`] if `version` is not [`crate::ABI_VERSION_CURRENT`].
    /// * [`Errno::LengthOutOfRange`] if `payload_len` exceeds
    ///   [`IPC_MESSAGE_MAX_PAYLOAD_LEN`].
    /// * [`Errno::BadMagic`] if the reserved field is non-zero (a deliberate
    ///   choice: reserved-must-be-zero violations are wire corruption, not a
    ///   length error).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let magic = read_u32(bytes, 0);
        if magic != IPC_MESSAGE_HEADER_MAGIC {
            return Err(Errno::BadMagic);
        }
        let version = read_u16(bytes, 4);
        if u32::from(version) != crate::ABI_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        let flags = read_u16(bytes, 6);
        let endpoint = read_u64(bytes, 8);
        let sender = read_u64(bytes, 16);
        let payload_len = read_u32(bytes, 24);
        if payload_len > IPC_MESSAGE_MAX_PAYLOAD_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let reserved = read_u32(bytes, 28);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            magic,
            version,
            flags,
            endpoint,
            sender,
            payload_len,
            reserved,
        })
    }
}

/// Maximum length, in bytes, of a [`PortName`].
///
/// Chosen so the encoded wire form ([`PortName::WIRE_LEN`]) is a tidy 32
/// bytes — one length byte followed by the name — matching the
/// [`IpcMessageHeader`] footprint.
pub const PORT_NAME_MAX_LEN: usize = 31;

/// A validated, well-known name that resolves a kernel IPC port without
/// hard-coding its numeric endpoint.
///
/// A numeric `endpoint` (see [`IpcMessageHeader::endpoint`]) is an opaque
/// handle a process must already hold; a `PortName` lets a process reach a
/// *well-known* endpoint — the desktop's pointer-input port, the keyboard
/// port, a long-running system service — by a stable name its publisher
/// chose, so a binder need not embed a kernel-assigned number. The kernel
/// port registry maps a `PortName` to the live port's endpoint; nothing
/// about the name grants authority, the per-send capability check is
/// unchanged (`AGENTS.md` §5.2).
///
/// A name is a non-empty, at most [`PORT_NAME_MAX_LEN`]-byte ASCII string
/// drawn from a deliberately small alphabet: it begins with a lowercase
/// letter and continues with lowercase letters, digits, `'.'`, or `'_'`,
/// with no trailing `'.'` and no `".."`. Constraining the alphabet keeps
/// names canonical (one spelling per endpoint), printable in a log line,
/// and free of separators a path or routing layer might re-interpret.
/// [`PortName::from_ascii`] is the only constructor and rejects anything
/// outside that grammar, so an ill-formed name is unrepresentable
/// (`AGENTS.md` §2.9 / §5.4).
///
/// The internal buffer is NUL-padded beyond the name's length, so two
/// values are equal exactly when their names are, and the derived ordering
/// gives the registry a stable map key.
#[repr(C)]
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PortName {
    bytes: [u8; PORT_NAME_MAX_LEN],
    len: u8,
}

impl PortName {
    /// Encoded size of a [`PortName`] on the wire: a length byte followed
    /// by [`PORT_NAME_MAX_LEN`] name bytes (NUL-padded past the length).
    pub const WIRE_LEN: usize = PORT_NAME_MAX_LEN + 1;

    /// Build a [`PortName`] from raw ASCII bytes, validating the grammar.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `name` is empty or longer than
    ///   [`PORT_NAME_MAX_LEN`].
    /// * [`Errno::OutOfRange`] if any byte falls outside the allowed
    ///   alphabet, the first byte is not a lowercase letter, the name ends
    ///   with `'.'`, or it contains `".."`.
    pub const fn from_ascii(name: &[u8]) -> Result<Self, Errno> {
        if name.is_empty() || name.len() > PORT_NAME_MAX_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        if !name[0].is_ascii_lowercase() {
            return Err(Errno::OutOfRange);
        }
        let mut i = 0;
        while i < name.len() {
            let b = name[i];
            let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_';
            if !ok {
                return Err(Errno::OutOfRange);
            }
            if b == b'.' && (i + 1 == name.len() || name[i + 1] == b'.') {
                return Err(Errno::OutOfRange);
            }
            i += 1;
        }

        let mut bytes = [0u8; PORT_NAME_MAX_LEN];
        let mut j = 0;
        while j < name.len() {
            bytes[j] = name[j];
            j += 1;
        }
        Ok(Self {
            bytes,
            #[allow(clippy::cast_possible_truncation)]
            len: name.len() as u8,
        })
    }

    /// The name's length in bytes (always `1..=PORT_NAME_MAX_LEN`).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Always `false`: a [`PortName`] is never empty by construction.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// The name's bytes, without the NUL padding.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len()]
    }

    /// The name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The constructor admits only ASCII bytes, which are valid UTF-8;
        // the empty fallback is unreachable.
        core::str::from_utf8(self.as_bytes()).unwrap_or_default()
    }

    /// Encode `self` into its little-endian wire representation: a length
    /// byte followed by the name, NUL-padded to [`PORT_NAME_MAX_LEN`].
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0] = self.len;
        out[1..].copy_from_slice(&self.bytes);
        out
    }

    /// Decode `bytes` into a [`PortName`].
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::LengthOutOfRange`] if the length byte is zero or exceeds
    ///   [`PORT_NAME_MAX_LEN`].
    /// * [`Errno::OutOfRange`] if the name violates the grammar (see
    ///   [`PortName::from_ascii`]).
    /// * [`Errno::BadMagic`] if any padding byte past the length is
    ///   non-zero (wire corruption, mirroring the reserved-field check on
    ///   [`IpcMessageHeader`]).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let len = bytes[0] as usize;
        if len == 0 || len > PORT_NAME_MAX_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let name = &bytes[1..=len];
        let parsed = Self::from_ascii(name)?;
        for &pad in &bytes[1 + len..Self::WIRE_LEN] {
            if pad != 0 {
                return Err(Errno::BadMagic);
            }
        }
        Ok(parsed)
    }
}

impl core::fmt::Debug for PortName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("PortName").field(&self.as_str()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IpcMessageHeader, PortName, IPC_MESSAGE_HEADER_MAGIC, IPC_MESSAGE_MAX_PAYLOAD_LEN,
        PORT_NAME_MAX_LEN,
    };
    use crate::{Errno, ABI_VERSION_CURRENT};

    fn sample() -> IpcMessageHeader {
        IpcMessageHeader {
            magic: IPC_MESSAGE_HEADER_MAGIC,
            #[allow(clippy::cast_possible_truncation)]
            version: ABI_VERSION_CURRENT as u16,
            flags: 0,
            endpoint: 0x0123_4567_89AB_CDEF,
            sender: 0,
            payload_len: 16,
            reserved: 0,
        }
    }

    #[test]
    fn wire_size_is_thirty_two() {
        assert_eq!(IpcMessageHeader::WIRE_LEN, 32);
        assert_eq!(core::mem::size_of::<IpcMessageHeader>(), 32);
    }

    #[test]
    fn round_trip_encodes_and_decodes() {
        let h = sample();
        let bytes = h.to_le_bytes();
        let decoded = IpcMessageHeader::from_bytes(&bytes).expect("valid header");
        assert_eq!(decoded, h);
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(
            IpcMessageHeader::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample().to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(IpcMessageHeader::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_bad_version() {
        let mut header = sample();
        header.version = 99;
        let bytes = header.to_le_bytes();
        assert_eq!(
            IpcMessageHeader::from_bytes(&bytes),
            Err(Errno::AbiVersionUnsupported)
        );
    }

    #[test]
    fn rejects_oversize_payload() {
        let mut header = sample();
        header.payload_len = IPC_MESSAGE_MAX_PAYLOAD_LEN + 1;
        let bytes = header.to_le_bytes();
        assert_eq!(
            IpcMessageHeader::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn rejects_nonzero_reserved() {
        let mut header = sample();
        header.reserved = 1;
        let bytes = header.to_le_bytes();
        assert_eq!(IpcMessageHeader::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn port_name_wire_size_is_thirty_two() {
        assert_eq!(PortName::WIRE_LEN, 32);
    }

    #[test]
    fn port_name_accepts_a_dotted_name() {
        let name = PortName::from_ascii(b"input.pointer").expect("valid name");
        assert_eq!(name.as_str(), "input.pointer");
        assert_eq!(name.as_bytes(), b"input.pointer");
        assert_eq!(name.len(), 13);
        assert!(!name.is_empty());
    }

    #[test]
    fn port_name_accepts_underscores_and_digits() {
        let name = PortName::from_ascii(b"svc_2.endpoint_0").expect("valid name");
        assert_eq!(name.as_str(), "svc_2.endpoint_0");
    }

    #[test]
    fn port_name_round_trips_through_the_wire() {
        let name = PortName::from_ascii(b"input.keyboard").expect("valid name");
        let decoded = PortName::from_bytes(&name.to_le_bytes()).expect("round-trip");
        assert_eq!(decoded, name);
        assert_eq!(decoded.as_str(), "input.keyboard");
    }

    #[test]
    fn port_name_rejects_empty() {
        assert_eq!(PortName::from_ascii(b""), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn port_name_rejects_overlong() {
        let too_long = [b'a'; PORT_NAME_MAX_LEN + 1];
        assert_eq!(
            PortName::from_ascii(&too_long),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn port_name_accepts_the_maximum_length() {
        let max = [b'a'; PORT_NAME_MAX_LEN];
        let name = PortName::from_ascii(&max).expect("max length is valid");
        assert_eq!(name.len(), PORT_NAME_MAX_LEN);
        assert_eq!(PortName::from_bytes(&name.to_le_bytes()), Ok(name));
    }

    #[test]
    fn port_name_rejects_a_non_letter_first_byte() {
        assert_eq!(PortName::from_ascii(b"1abc"), Err(Errno::OutOfRange));
        assert_eq!(PortName::from_ascii(b".abc"), Err(Errno::OutOfRange));
        assert_eq!(PortName::from_ascii(b"_abc"), Err(Errno::OutOfRange));
    }

    #[test]
    fn port_name_rejects_disallowed_bytes() {
        assert_eq!(PortName::from_ascii(b"Input"), Err(Errno::OutOfRange));
        assert_eq!(PortName::from_ascii(b"a/b"), Err(Errno::OutOfRange));
        assert_eq!(PortName::from_ascii(b"a b"), Err(Errno::OutOfRange));
    }

    #[test]
    fn port_name_rejects_trailing_and_double_dots() {
        assert_eq!(PortName::from_ascii(b"a."), Err(Errno::OutOfRange));
        assert_eq!(PortName::from_ascii(b"a..b"), Err(Errno::OutOfRange));
    }

    #[test]
    fn port_name_from_bytes_rejects_short_buffer() {
        assert_eq!(
            PortName::from_bytes(&[0u8; PortName::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn port_name_from_bytes_rejects_bad_length_byte() {
        let mut wire = [0u8; PortName::WIRE_LEN];
        assert_eq!(PortName::from_bytes(&wire), Err(Errno::LengthOutOfRange));
        wire[0] = u8::try_from(PORT_NAME_MAX_LEN + 1).expect("fits in a byte");
        assert_eq!(PortName::from_bytes(&wire), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn port_name_from_bytes_rejects_nonzero_padding() {
        let name = PortName::from_ascii(b"abc").expect("valid name");
        let mut wire = name.to_le_bytes();
        wire[PortName::WIRE_LEN - 1] = 1;
        assert_eq!(PortName::from_bytes(&wire), Err(Errno::BadMagic));
    }

    #[test]
    fn port_name_from_bytes_rejects_bad_grammar_in_payload() {
        let mut wire = [0u8; PortName::WIRE_LEN];
        wire[0] = 3;
        wire[1] = b'a';
        wire[2] = b'/';
        wire[3] = b'b';
        assert_eq!(PortName::from_bytes(&wire), Err(Errno::OutOfRange));
    }

    #[test]
    fn port_name_equality_ignores_padding() {
        let a = PortName::from_ascii(b"a").expect("valid");
        let b = PortName::from_ascii(b"a").expect("valid");
        assert_eq!(a, b);
        let c = PortName::from_ascii(b"ab").expect("valid");
        assert_ne!(a, c);
    }

    #[test]
    fn port_name_debug_shows_the_name() {
        extern crate alloc;
        let name = PortName::from_ascii(b"input.pointer").expect("valid");
        assert_eq!(alloc::format!("{name:?}"), "PortName(\"input.pointer\")");
    }
}
