//! 64-bit-native time types for the RustOS ABI (`AGENTS.md` §21).
//!
//! RustOS is 64-bit-time-native: no kernel ABI, userland ABI, IPC type, log
//! format, native filesystem, or persistent OS metadata may store absolute
//! time as 32-bit seconds. The two canonical types live here:
//!
//! - [`Time64`] — an absolute instant, signed 64-bit seconds since the Unix
//!   epoch plus a nanosecond field. It is RustOS's equivalent of Linux's
//!   `timespec64` (seconds *and* nanoseconds), not a seconds-only
//!   `time64_t`.
//! - [`Duration64`] — a span of time, signed 64-bit seconds plus a
//!   nanosecond field.
//!
//! Both store the sub-second component as a canonical nanosecond count in
//! `0..NANOS_PER_SEC`, so the derived ordering is chronological and the wire
//! encoding is unambiguous. Conversions to narrower representations (for
//! example a legacy on-disk timestamp) are **checked**: an unrepresentable
//! value fails with [`Errno::TimestampOutOfRange`] rather than silently
//! truncating, wrapping, saturating, or guessing a timezone (§21).
//!
//! Like the rest of this crate, the types are `no_std`, allocation-free, and
//! encode/decode through borrowed byte slices so the same code runs in the
//! kernel, a freestanding driver, and a WebAssembly userland binary.

use crate::le::{put_i64, put_u32, read_i64, read_u32};
use crate::Errno;

/// Nanoseconds in one second. The sub-second field of every time type is
/// kept in `0..NANOS_PER_SEC`.
pub const NANOS_PER_SEC: u32 = 1_000_000_000;

/// An absolute instant: signed seconds since the Unix epoch plus a
/// nanosecond field in `0..NANOS_PER_SEC`.
///
/// This is the RustOS canonical time type (`AGENTS.md` §21). Absolute time is
/// never stored as 32-bit seconds anywhere in the ABI; it is stored as a
/// `Time64`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct Time64 {
    secs: i64,
    nanos: u32,
}

impl Time64 {
    /// Encoded size on the wire: 8-byte seconds + 4-byte nanoseconds.
    pub const WIRE_LEN: usize = 12;

    /// The Unix epoch (`1970-01-01T00:00:00Z`).
    pub const UNIX_EPOCH: Self = Self { secs: 0, nanos: 0 };

    /// Construct an instant from whole seconds since the Unix epoch.
    #[must_use]
    pub const fn from_secs(secs: i64) -> Self {
        Self { secs, nanos: 0 }
    }

    /// Construct an instant from seconds and a nanosecond field.
    ///
    /// Returns [`Errno::TimestampOutOfRange`] if `nanos >= NANOS_PER_SEC`; the
    /// nanosecond field is never silently normalised by carrying into the
    /// seconds.
    pub fn new(secs: i64, nanos: u32) -> Result<Self, Errno> {
        if nanos >= NANOS_PER_SEC {
            return Err(Errno::TimestampOutOfRange);
        }
        Ok(Self { secs, nanos })
    }

    /// Seconds since the Unix epoch.
    #[must_use]
    pub const fn secs(&self) -> i64 {
        self.secs
    }

    /// The sub-second component, in `0..NANOS_PER_SEC`.
    #[must_use]
    pub const fn subsec_nanos(&self) -> u32 {
        self.nanos
    }

    /// Narrow the seconds to a signed 32-bit on-disk field.
    ///
    /// Returns [`Errno::TimestampOutOfRange`] if the instant falls outside the
    /// `i32`-seconds range (the classic 1901..2038 window). The check is the
    /// point of the type: a legacy filesystem driver calls this rather than
    /// casting (§21).
    pub fn secs_i32(&self) -> Result<i32, Errno> {
        i32::try_from(self.secs).map_err(|_| Errno::TimestampOutOfRange)
    }

    /// Narrow the seconds to an unsigned 32-bit on-disk field.
    ///
    /// Returns [`Errno::TimestampOutOfRange`] for instants before the Unix
    /// epoch or beyond the `u32`-seconds range (the 1970..2106 window).
    pub fn secs_u32(&self) -> Result<u32, Errno> {
        u32::try_from(self.secs).map_err(|_| Errno::TimestampOutOfRange)
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_i64(&mut out, 0, self.secs);
        put_u32(&mut out, 8, self.nanos);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if `bytes` is shorter than
    /// [`Self::WIRE_LEN`], or [`Errno::TimestampOutOfRange`] if the decoded
    /// nanosecond field is not canonical.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Self::new(read_i64(bytes, 0), read_u32(bytes, 8))
    }
}

/// A span of time: signed seconds plus a nanosecond field in
/// `0..NANOS_PER_SEC`.
///
/// The companion to [`Time64`] (`AGENTS.md` §21). A negative span is
/// represented by negative `secs` with the nanosecond field always in the
/// canonical range, so `(secs, nanos)` orders chronologically.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct Duration64 {
    secs: i64,
    nanos: u32,
}

impl Duration64 {
    /// Encoded size on the wire: 8-byte seconds + 4-byte nanoseconds.
    pub const WIRE_LEN: usize = 12;

    /// The zero duration.
    pub const ZERO: Self = Self { secs: 0, nanos: 0 };

    /// Construct a span from whole seconds.
    #[must_use]
    pub const fn from_secs(secs: i64) -> Self {
        Self { secs, nanos: 0 }
    }

    /// Construct a span from seconds and a nanosecond field.
    ///
    /// Returns [`Errno::TimestampOutOfRange`] if `nanos >= NANOS_PER_SEC`.
    pub fn new(secs: i64, nanos: u32) -> Result<Self, Errno> {
        if nanos >= NANOS_PER_SEC {
            return Err(Errno::TimestampOutOfRange);
        }
        Ok(Self { secs, nanos })
    }

    /// Construct a span from a non-negative nanosecond count.
    ///
    /// Suits monotonic clocks that report nanoseconds since boot. The split
    /// into seconds and a canonical sub-second field is exact: the quotient
    /// of a `u64` by a billion always fits an `i64`, and the remainder is
    /// always below `NANOS_PER_SEC`, so neither fallback below is reachable.
    #[must_use]
    pub fn from_nanos(total_nanos: u64) -> Self {
        let per = u64::from(NANOS_PER_SEC);
        Self {
            secs: i64::try_from(total_nanos / per).unwrap_or(i64::MAX),
            nanos: u32::try_from(total_nanos % per).unwrap_or(0),
        }
    }

    /// Whole seconds of the span.
    #[must_use]
    pub const fn secs(&self) -> i64 {
        self.secs
    }

    /// The sub-second component, in `0..NANOS_PER_SEC`.
    #[must_use]
    pub const fn subsec_nanos(&self) -> u32 {
        self.nanos
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_i64(&mut out, 0, self.secs);
        put_u32(&mut out, 8, self.nanos);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if `bytes` is shorter than
    /// [`Self::WIRE_LEN`], or [`Errno::TimestampOutOfRange`] if the decoded
    /// nanosecond field is not canonical.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Self::new(read_i64(bytes, 0), read_u32(bytes, 8))
    }
}

#[cfg(test)]
mod tests {
    use super::{Duration64, Time64, NANOS_PER_SEC};
    use crate::Errno;

    #[test]
    fn time64_round_trips_at_the_epoch() {
        let t = Time64::UNIX_EPOCH;
        assert_eq!(Time64::from_bytes(&t.to_le_bytes()), Ok(t));
        assert_eq!(t.secs(), 0);
        assert_eq!(t.subsec_nanos(), 0);
    }

    #[test]
    fn time64_round_trips_before_1970() {
        // 1901-12-13, just inside the signed 32-bit epoch floor.
        let t = Time64::new(-2_147_483_648, 123).unwrap();
        let decoded = Time64::from_bytes(&t.to_le_bytes()).unwrap();
        assert_eq!(decoded, t);
        assert_eq!(decoded.secs(), -2_147_483_648);
        assert_eq!(decoded.subsec_nanos(), 123);
    }

    #[test]
    fn time64_round_trips_after_2038() {
        // 2038-01-19 03:14:08 UTC is the first second past the i32 ceiling.
        let t = Time64::new(2_147_483_648, 999_999_999).unwrap();
        assert_eq!(Time64::from_bytes(&t.to_le_bytes()), Ok(t));
    }

    #[test]
    fn time64_round_trips_after_2106() {
        // Past the u32-seconds ceiling, where 32-bit encodings give up.
        let t = Time64::from_secs(4_294_967_296);
        assert_eq!(Time64::from_bytes(&t.to_le_bytes()), Ok(t));
    }

    #[test]
    fn time64_rejects_noncanonical_nanos() {
        assert_eq!(
            Time64::new(0, NANOS_PER_SEC),
            Err(Errno::TimestampOutOfRange)
        );
        let mut bytes = Time64::UNIX_EPOCH.to_le_bytes();
        bytes[8..12].copy_from_slice(&NANOS_PER_SEC.to_le_bytes());
        assert_eq!(Time64::from_bytes(&bytes), Err(Errno::TimestampOutOfRange));
    }

    #[test]
    fn time64_short_buffer_is_rejected() {
        assert_eq!(Time64::from_bytes(&[0u8; 11]), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn secs_i32_is_checked_around_2038() {
        assert_eq!(Time64::from_secs(2_147_483_647).secs_i32(), Ok(i32::MAX));
        assert_eq!(
            Time64::from_secs(2_147_483_648).secs_i32(),
            Err(Errno::TimestampOutOfRange)
        );
    }

    #[test]
    fn secs_u32_is_checked_at_both_ends() {
        assert_eq!(
            Time64::from_secs(-1).secs_u32(),
            Err(Errno::TimestampOutOfRange)
        );
        assert_eq!(Time64::from_secs(4_294_967_295).secs_u32(), Ok(u32::MAX));
        assert_eq!(
            Time64::from_secs(4_294_967_296).secs_u32(),
            Err(Errno::TimestampOutOfRange)
        );
    }

    #[test]
    fn duration64_from_nanos_splits_exactly() {
        let d = Duration64::from_nanos(2_500_000_001);
        assert_eq!(d.secs(), 2);
        assert_eq!(d.subsec_nanos(), 500_000_001);
        assert_eq!(Duration64::from_bytes(&d.to_le_bytes()), Ok(d));
    }

    #[test]
    fn duration64_rejects_noncanonical_nanos() {
        assert_eq!(
            Duration64::new(1, NANOS_PER_SEC),
            Err(Errno::TimestampOutOfRange)
        );
    }

    #[test]
    fn duration64_orders_chronologically() {
        assert!(Duration64::from_nanos(1) < Duration64::from_nanos(u64::from(NANOS_PER_SEC)));
        assert!(Duration64::from_secs(-1) < Duration64::ZERO);
    }
}
