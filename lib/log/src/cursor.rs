//! Advancing, bounds-checked little-endian byte cursors for the record layer.
//!
//! The logical-record body ([`crate::record`]) and its segment-local string
//! compression ([`crate::dict`]) both walk untrusted bytes with a running
//! position, and both must fail closed on the first out-of-range length rather
//! than index blindly. That plumbing lives here once so the two encoders and
//! decoders cannot drift apart.
//!
//! This is deliberately distinct from `rustos_abi`'s offset-indexed
//! [`le`](rustos_abi) helpers: those take a caller-checked fixed offset and
//! never advance or bounds-check, which suits fixed-`WIRE_LEN` ABI structs.
//! The record layer instead streams variable-length fields, so it needs the
//! advancing, self-checking form below.

use rustos_abi::Errno;

/// Append `src` at `*pos`, advancing it. Fails closed if `out` is too small.
pub(crate) fn put_bytes(out: &mut [u8], pos: &mut usize, src: &[u8]) -> Result<(), Errno> {
    let end = pos.checked_add(src.len()).ok_or(Errno::BufferTooSmall)?;
    if end > out.len() {
        return Err(Errno::BufferTooSmall);
    }
    out[*pos..end].copy_from_slice(src);
    *pos = end;
    Ok(())
}

/// Append one byte.
pub(crate) fn put_u8(out: &mut [u8], pos: &mut usize, v: u8) -> Result<(), Errno> {
    put_bytes(out, pos, &[v])
}

/// Append a little-endian `u16`.
pub(crate) fn put_u16(out: &mut [u8], pos: &mut usize, v: u16) -> Result<(), Errno> {
    put_bytes(out, pos, &v.to_le_bytes())
}

/// Append a little-endian `u64`.
pub(crate) fn put_u64(out: &mut [u8], pos: &mut usize, v: u64) -> Result<(), Errno> {
    put_bytes(out, pos, &v.to_le_bytes())
}

/// Borrow the next `n` bytes at `*pos`, advancing it. Fails closed if fewer
/// than `n` bytes remain.
pub(crate) fn take<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], Errno> {
    let end = pos.checked_add(n).ok_or(Errno::LengthOutOfRange)?;
    let slice = bytes.get(*pos..end).ok_or(Errno::LengthOutOfRange)?;
    *pos = end;
    Ok(slice)
}

/// Read one byte.
pub(crate) fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8, Errno> {
    Ok(take(bytes, pos, 1)?[0])
}

/// Read a little-endian `u16`.
pub(crate) fn read_u16(bytes: &[u8], pos: &mut usize) -> Result<u16, Errno> {
    let s = take(bytes, pos, 2)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

/// Read a little-endian `u64`.
pub(crate) fn read_u64(bytes: &[u8], pos: &mut usize) -> Result<u64, Errno> {
    let s = take(bytes, pos, 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(u64::from_le_bytes(a))
}
