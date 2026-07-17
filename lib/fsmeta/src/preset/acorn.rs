//! Acorn / RISC OS (ADFS, `FileCore`) preset conversions.
//!
//! A RISC OS object is typed by the top of its 32-bit load address: when the
//! high 12 bits are `&FFF`, bits `19..8` hold a 12-bit filetype (e.g. `&FFF` =
//! Text) and the remaining 40 bits (`load & 0xFF` as the high byte, then the
//! whole exec word) hold a centisecond timestamp since 1900. An untyped object
//! instead uses load/exec as literal load and execution addresses.
//!
//! The registry stores the decoded `acorn.filetype` *and* preserves the raw
//! load/exec words (`acorn.loadaddr` / `acorn.execaddr`), so a copy back to
//! ADFS reproduces the native fields byte-for-byte.

use tairix_abi::time::Time64;

use crate::MetadataError;

/// Seconds between the RISC OS epoch (1900-01-01) and the Unix epoch
/// (1970-01-01). RISC OS counts centiseconds from 1900.
const RISC_OS_EPOCH_TO_UNIX_SECS: i64 = 2_208_988_800;

/// Centiseconds in one second.
const CENTIS_PER_SEC: u64 = 100;

/// Nanoseconds in one centisecond.
const NANOS_PER_CENTI: u32 = 10_000_000;

/// The RISC OS timestamp is a 40-bit (5-byte) centisecond count.
const CENTIS_LIMIT: u64 = 1 << 40;

/// High 12 bits of a load address that mark a filetyped object.
const TYPED_MARKER: u32 = 0xFFF;

/// A decoded RISC OS load/exec pair.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LoadExec {
    /// A filetyped object: a 12-bit filetype and a 40-bit centisecond stamp.
    Typed {
        /// The 12-bit filetype (`0..=0xFFF`).
        filetype: u16,
        /// The 40-bit centisecond timestamp since 1900.
        centiseconds: u64,
    },
    /// An untyped object with literal load and execution addresses.
    Untyped {
        /// The literal 32-bit load address.
        load: u32,
        /// The literal 32-bit execution address.
        exec: u32,
    },
}

/// Decode a raw load/exec pair into a filetyped or untyped object.
#[must_use]
pub fn decode_load_exec(load: u32, exec: u32) -> LoadExec {
    if load >> 20 == TYPED_MARKER {
        let filetype = u16::try_from((load >> 8) & 0xFFF).unwrap_or(0);
        let centiseconds = (u64::from(load & 0xFF) << 32) | u64::from(exec);
        LoadExec::Typed {
            filetype,
            centiseconds,
        }
    } else {
        LoadExec::Untyped { load, exec }
    }
}

/// Encode a 12-bit filetype and a 40-bit centisecond stamp back into the raw
/// load/exec words.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if `filetype` exceeds 12 bits or
/// `centiseconds` exceeds 40 bits.
pub fn encode_typed(filetype: u16, centiseconds: u64) -> Result<(u32, u32), MetadataError> {
    if filetype > 0xFFF || centiseconds >= CENTIS_LIMIT {
        return Err(MetadataError::NotRepresentable);
    }
    let high_byte = u32::try_from(centiseconds >> 32).unwrap_or(0) & 0xFF;
    let load = (TYPED_MARKER << 20) | (u32::from(filetype) << 8) | high_byte;
    let exec = u32::try_from(centiseconds & 0xFFFF_FFFF).unwrap_or(0);
    Ok((load, exec))
}

/// Encode a 12-bit filetype as three lowercase hex digits (e.g. `&FFF` →
/// `b"fff"`), the canonical `acorn.filetype` value.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if `filetype` exceeds 12 bits.
pub fn filetype_to_value(filetype: u16) -> Result<[u8; 3], MetadataError> {
    if filetype > 0xFFF {
        return Err(MetadataError::NotRepresentable);
    }
    Ok([
        hex_digit(u8::try_from((filetype >> 8) & 0xF).unwrap_or(0)),
        hex_digit(u8::try_from((filetype >> 4) & 0xF).unwrap_or(0)),
        hex_digit(u8::try_from(filetype & 0xF).unwrap_or(0)),
    ])
}

/// Parse an `acorn.filetype` value (three hex digits, upper or lower case)
/// back into a 12-bit filetype.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if the value is not exactly three hex
/// digits.
pub fn filetype_from_value(value: &[u8]) -> Result<u16, MetadataError> {
    if value.len() != 3 {
        return Err(MetadataError::NotRepresentable);
    }
    let mut out: u16 = 0;
    for &byte in value {
        let nibble = hex_value(byte).ok_or(MetadataError::NotRepresentable)?;
        out = (out << 4) | u16::from(nibble);
    }
    Ok(out)
}

/// Encode a 32-bit load or exec address as eight lowercase hex digits, the
/// canonical `acorn.loadaddr` / `acorn.execaddr` value.
#[must_use]
pub fn addr_to_value(addr: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 28 - 4 * u32::try_from(i).unwrap_or(0);
        *slot = hex_digit(u8::try_from((addr >> shift) & 0xF).unwrap_or(0));
    }
    out
}

/// Parse an `acorn.loadaddr` / `acorn.execaddr` value (eight hex digits, upper
/// or lower case) back into a 32-bit address.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if the value is not exactly eight hex
/// digits.
pub fn addr_from_value(value: &[u8]) -> Result<u32, MetadataError> {
    if value.len() != 8 {
        return Err(MetadataError::NotRepresentable);
    }
    let mut out: u32 = 0;
    for &byte in value {
        let nibble = hex_value(byte).ok_or(MetadataError::NotRepresentable)?;
        out = (out << 4) | u32::from(nibble);
    }
    Ok(out)
}

/// Convert a RISC OS 40-bit centisecond timestamp to a [`Time64`].
///
/// # Errors
///
/// [`MetadataError::TimestampOutOfRange`] if `centiseconds` exceeds 40 bits or
/// the resulting instant is not representable.
pub fn centiseconds_to_time64(centiseconds: u64) -> Result<Time64, MetadataError> {
    if centiseconds >= CENTIS_LIMIT {
        return Err(MetadataError::TimestampOutOfRange);
    }
    let secs_since_1900 = i64::try_from(centiseconds / CENTIS_PER_SEC)
        .map_err(|_| MetadataError::TimestampOutOfRange)?;
    let unix_secs = secs_since_1900 - RISC_OS_EPOCH_TO_UNIX_SECS;
    let rem_centis = u32::try_from(centiseconds % CENTIS_PER_SEC).unwrap_or(0);
    Time64::new(unix_secs, rem_centis * NANOS_PER_CENTI)
        .map_err(|_| MetadataError::TimestampOutOfRange)
}

/// Convert a [`Time64`] to a RISC OS 40-bit centisecond timestamp, checked.
///
/// # Errors
///
/// [`MetadataError::TimestampOutOfRange`] if the instant is before 1900, is
/// beyond the 40-bit centisecond range, or carries sub-centisecond precision
/// (which RISC OS cannot represent and this never silently drops).
pub fn time64_to_centiseconds(time: Time64) -> Result<u64, MetadataError> {
    if time.subsec_nanos() % NANOS_PER_CENTI != 0 {
        return Err(MetadataError::TimestampOutOfRange);
    }
    let since_1900 = time
        .secs()
        .checked_add(RISC_OS_EPOCH_TO_UNIX_SECS)
        .ok_or(MetadataError::TimestampOutOfRange)?;
    if since_1900 < 0 {
        return Err(MetadataError::TimestampOutOfRange);
    }
    let sub_centis = u64::from(time.subsec_nanos() / NANOS_PER_CENTI);
    let centiseconds = u64::try_from(since_1900)
        .map_err(|_| MetadataError::TimestampOutOfRange)?
        .checked_mul(CENTIS_PER_SEC)
        .and_then(|c| c.checked_add(sub_centis))
        .ok_or(MetadataError::TimestampOutOfRange)?;
    if centiseconds >= CENTIS_LIMIT {
        return Err(MetadataError::TimestampOutOfRange);
    }
    Ok(centiseconds)
}

/// `FileCore` attribute bits in canonical `acorn.attr` bit order:
/// owner read (`R`), owner write (`W`), locked (`L`), directory (`D`),
/// execute-only (`E`), public read (`r`), public write (`w`), public
/// execute (`e`, 8-bit ADFS only), and private (`P`, 8-bit ADFS only).
pub const ATTR_BITS: u16 = 0x01FF;

/// Owner-part attribute letters and their bit positions.
const ATTR_OWNER: [(u8, u16); 6] = [
    (b'R', 1 << 0),
    (b'W', 1 << 1),
    (b'L', 1 << 2),
    (b'D', 1 << 3),
    (b'E', 1 << 4),
    (b'P', 1 << 8),
];

/// Public-part attribute letters and their bit positions.
const ATTR_PUBLIC: [(u8, u16); 3] = [(b'r', 1 << 5), (b'w', 1 << 6), (b'e', 1 << 7)];

/// Longest canonical `acorn.attr` value (`RWLDEP/rwe`).
pub const ATTR_VALUE_MAX: usize = 10;

/// Encode `FileCore` attribute bits as the canonical `acorn.attr` value:
/// the owner letters in `RWLDEP` order, a `/`, then the public letters
/// in `rwe` order (a locked, publicly readable directory encodes as
/// `b"RLD/r"`).
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if `attr` carries bits outside
/// [`ATTR_BITS`].
pub fn attr_to_value(attr: u16) -> Result<([u8; ATTR_VALUE_MAX], usize), MetadataError> {
    if attr & !ATTR_BITS != 0 {
        return Err(MetadataError::NotRepresentable);
    }
    let mut out = [0u8; ATTR_VALUE_MAX];
    let mut len = 0;
    for (letter, bit) in ATTR_OWNER {
        if attr & bit != 0 {
            out[len] = letter;
            len += 1;
        }
    }
    out[len] = b'/';
    len += 1;
    for (letter, bit) in ATTR_PUBLIC {
        if attr & bit != 0 {
            out[len] = letter;
            len += 1;
        }
    }
    Ok((out, len))
}

/// Parse an `acorn.attr` value back into `FileCore` attribute bits. The
/// letters may appear in any order around the single `/`; duplicate or
/// unknown letters are rejected.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] on any malformed value.
pub fn attr_from_value(value: &[u8]) -> Result<u16, MetadataError> {
    let slash = value
        .iter()
        .position(|&b| b == b'/')
        .ok_or(MetadataError::NotRepresentable)?;
    let (owner, public) = (&value[..slash], &value[slash + 1..]);
    if public.contains(&b'/') {
        return Err(MetadataError::NotRepresentable);
    }
    let mut attr = 0u16;
    for &letter in owner {
        let (_, bit) = ATTR_OWNER
            .iter()
            .find(|(l, _)| *l == letter)
            .ok_or(MetadataError::NotRepresentable)?;
        if attr & bit != 0 {
            return Err(MetadataError::NotRepresentable);
        }
        attr |= bit;
    }
    for &letter in public {
        let (_, bit) = ATTR_PUBLIC
            .iter()
            .find(|(l, _)| *l == letter)
            .ok_or(MetadataError::NotRepresentable)?;
        if attr & bit != 0 {
            return Err(MetadataError::NotRepresentable);
        }
        attr |= bit;
    }
    Ok(attr)
}

/// Encode a 40-bit centisecond datestamp as the canonical
/// `acorn.datestamp` value: ten lowercase hex digits, so the raw stamp
/// round-trips exactly.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if `centiseconds` exceeds 40 bits.
pub fn datestamp_to_value(centiseconds: u64) -> Result<[u8; 10], MetadataError> {
    if centiseconds >= CENTIS_LIMIT {
        return Err(MetadataError::NotRepresentable);
    }
    let mut out = [0u8; 10];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 36 - 4 * u32::try_from(i).unwrap_or(0);
        *slot = hex_digit(u8::try_from((centiseconds >> shift) & 0xF).unwrap_or(0));
    }
    Ok(out)
}

/// Parse an `acorn.datestamp` value (ten hex digits, upper or lower
/// case) back into a 40-bit centisecond stamp.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if the value is not exactly ten
/// hex digits.
pub fn datestamp_from_value(value: &[u8]) -> Result<u64, MetadataError> {
    if value.len() != 10 {
        return Err(MetadataError::NotRepresentable);
    }
    let mut out: u64 = 0;
    for &byte in value {
        let nibble = hex_value(byte).ok_or(MetadataError::NotRepresentable)?;
        out = (out << 4) | u64::from(nibble);
    }
    Ok(out)
}

/// The lowercase-hex character for a nibble `0..=15`.
fn hex_digit(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        _ => b'a' + (nibble - 10),
    }
}

/// The nibble value of a hex character (upper or lower case), or `None`.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
