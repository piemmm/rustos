//! Wire-level header for an IPC message.
//!
//! Every message exchanged through a kernel IPC port begins with an
//! [`IpcMessageHeader`]. The struct is `#[repr(C)]` with explicitly sized
//! fields encoded little-endian on the wire; layout and field order are part
//! of the frozen `abi-v1` contract.

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
        let magic = u32_le(bytes, 0);
        if magic != IPC_MESSAGE_HEADER_MAGIC {
            return Err(Errno::BadMagic);
        }
        let version = u16_le(bytes, 4);
        if u32::from(version) != crate::ABI_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        let flags = u16_le(bytes, 6);
        let endpoint = u64_le(bytes, 8);
        let sender = u64_le(bytes, 16);
        let payload_len = u32_le(bytes, 24);
        if payload_len > IPC_MESSAGE_MAX_PAYLOAD_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let reserved = u32_le(bytes, 28);
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

#[inline]
fn u16_le(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

#[inline]
fn u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[inline]
fn u64_le(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::{IpcMessageHeader, IPC_MESSAGE_HEADER_MAGIC, IPC_MESSAGE_MAX_PAYLOAD_LEN};
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
}
