//! Little-endian scalar read/write helpers shared across the ABI surface.
//!
//! All RustOS wire formats are little-endian (every Tier-1 target is
//! little-endian, and the explicit encoding lets a future big-endian port
//! participate without breaking the ABI). The same per-field index
//! arithmetic was previously open-coded in more than one module; it lives
//! here once so that the encoders and decoders cannot drift apart
//! (`AGENTS.md` §2.2 — no duplication).
//!
//! The helpers are `pub(crate)`: they are an implementation detail of the
//! `abi` crate, not part of the frozen public surface. Callers are
//! responsible for bounds-checking the slice length before invoking a read
//! (every ABI decoder does so up front against the structure's `WIRE_LEN`),
//! so these functions index directly and never allocate.

#[inline]
pub(crate) fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

#[inline]
pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[inline]
pub(crate) fn read_u64(bytes: &[u8], offset: usize) -> u64 {
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

#[inline]
pub(crate) fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    let v = value.to_le_bytes();
    bytes[offset] = v[0];
    bytes[offset + 1] = v[1];
}

#[inline]
pub(crate) fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    let v = value.to_le_bytes();
    bytes[offset] = v[0];
    bytes[offset + 1] = v[1];
    bytes[offset + 2] = v[2];
    bytes[offset + 3] = v[3];
}

#[inline]
pub(crate) fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    let v = value.to_le_bytes();
    bytes[offset] = v[0];
    bytes[offset + 1] = v[1];
    bytes[offset + 2] = v[2];
    bytes[offset + 3] = v[3];
    bytes[offset + 4] = v[4];
    bytes[offset + 5] = v[5];
    bytes[offset + 6] = v[6];
    bytes[offset + 7] = v[7];
}

#[cfg(test)]
mod tests {
    use super::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};

    #[test]
    fn u16_round_trips_at_offset() {
        let mut buf = [0u8; 4];
        put_u16(&mut buf, 1, 0xBEEF);
        assert_eq!(buf, [0x00, 0xEF, 0xBE, 0x00]);
        assert_eq!(read_u16(&buf, 1), 0xBEEF);
    }

    #[test]
    fn u32_round_trips_at_offset() {
        let mut buf = [0u8; 8];
        put_u32(&mut buf, 2, 0xDEAD_BEEF);
        assert_eq!(read_u32(&buf, 2), 0xDEAD_BEEF);
        assert_eq!(&buf[2..6], &[0xEF, 0xBE, 0xAD, 0xDE]);
    }

    #[test]
    fn u64_round_trips_at_offset() {
        let mut buf = [0u8; 16];
        put_u64(&mut buf, 4, 0x0123_4567_89AB_CDEF);
        assert_eq!(read_u64(&buf, 4), 0x0123_4567_89AB_CDEF);
        assert_eq!(
            &buf[4..12],
            &[0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]
        );
    }
}
