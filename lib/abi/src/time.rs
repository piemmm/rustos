//! 64-bit-native time types for the TAIRiX ABI.
//!
//! TAIRiX is 64-bit-time-native: no kernel ABI, userland ABI, IPC type, log
//! format, native filesystem, or persistent OS metadata may store absolute
//! time as 32-bit seconds. The two canonical types live here:
//!
//! - [`Time64`] — an absolute instant, signed 64-bit seconds since the Unix
//!   epoch plus a nanosecond field. It is TAIRiX's equivalent of Linux's
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
//! truncating, wrapping, saturating, or guessing a timezone.
//!
//! Like the rest of this crate, the types are `no_std`, allocation-free, and
//! encode/decode through borrowed byte slices so the same code runs in the
//! kernel, a freestanding driver, and a WebAssembly userland binary.

use crate::le::{put_i64, put_u32, read_i64, read_u32};
use crate::Errno;

/// Nanoseconds in one second. The sub-second field of every time type is
/// kept in `0..NANOS_PER_SEC`.
pub const NANOS_PER_SEC: u32 = 1_000_000_000;

/// Resolution, in nanoseconds, that the monotonic clock is coarsened to
/// for callers that do not hold
/// [`CapabilityId::TIME_HIRES`](crate::CapabilityId::TIME_HIRES).
///
/// `clock_get` (`abi-v1` syscall 7) is unprivileged, so every task —
/// including the parser sandboxes and untrusted `userland/apps` —
/// can read it. A sub-microsecond timer is a primitive for the
/// cache- and execution-timing side channels hardens
/// against, so the default value handed to an untrusted caller is
/// floored to this granularity (one microsecond). A principal that is
/// explicitly trusted with precise timing holds `CAP_TIME_HIRES` and
/// reads the clock at full nanosecond resolution. The value is data:
/// tightening or relaxing it changes only this constant, not the
/// `clock_get` ABI signature (security by default).
pub const COARSE_CLOCK_GRANULARITY_NS: u64 = 1_000;

/// Floor `ns` to [`COARSE_CLOCK_GRANULARITY_NS`].
///
/// Returns the largest multiple of [`COARSE_CLOCK_GRANULARITY_NS`] that
/// is `<= ns`, so the coarsened reading never exceeds the true reading
/// and the sequence stays monotonically non-decreasing. This is the one
/// place the coarsening arithmetic lives, so the kernel `clock_get`
/// handler and any future fast-path reader agree.
#[must_use]
pub const fn coarsen_clock_ns(ns: u64) -> u64 {
    ns - (ns % COARSE_CLOCK_GRANULARITY_NS)
}

/// An absolute instant: signed seconds since the Unix epoch plus a
/// nanosecond field in `0..NANOS_PER_SEC`.
///
/// This is the TAIRiX canonical time type. Absolute time is
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
    /// casting.
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

    /// Add a [`Duration64`] span, saturating the seconds at the `i64`
    /// bounds rather than wrapping.
    ///
    /// The nanosecond fields of both operands are canonical (`0..NANOS_PER_SEC`),
    /// so their sum is below `2 * NANOS_PER_SEC` and at most one whole second
    /// is carried into the seconds. This is the arithmetic the kernel wall
    /// clock uses to project a stored instant forward by an elapsed monotonic
    /// span; saturating (never wrapping) keeps a runaway span deterministic
    /// instead of silently rolling the year over.
    #[must_use]
    pub fn saturating_add(self, span: Duration64) -> Self {
        let mut secs = self.secs.saturating_add(span.secs());
        // Both nanos fields are `< NANOS_PER_SEC`, so the sum is
        // `< 2 * NANOS_PER_SEC` and fits a `u32` (which tops out above
        // `4 * NANOS_PER_SEC`); at most one second carries.
        let mut nanos = self.nanos + span.subsec_nanos();
        if nanos >= NANOS_PER_SEC {
            nanos -= NANOS_PER_SEC;
            secs = secs.saturating_add(1);
        }
        Self { secs, nanos }
    }

    /// Subtract a [`Duration64`] span, saturating the seconds at the `i64`
    /// bounds rather than wrapping.
    ///
    /// The exact complement of [`saturating_add`](Self::saturating_add): both
    /// nanosecond fields are canonical (`0..NANOS_PER_SEC`), so at most one
    /// second is borrowed. Its first user is projecting a wall-clock reading
    /// *back* to the boot instant (`wall_now - since_boot`) for the System
    /// Information uptime feed; keeping the arithmetic here means the forward
    /// and backward projections share one tested definition.
    #[must_use]
    pub fn saturating_sub(self, span: Duration64) -> Self {
        let mut secs = self.secs.saturating_sub(span.secs());
        let mut nanos = self.nanos;
        if nanos < span.subsec_nanos() {
            // Borrow one whole second to cover the nanosecond underflow; both
            // fields are `< NANOS_PER_SEC`, so one borrow always suffices.
            nanos = nanos + NANOS_PER_SEC - span.subsec_nanos();
            secs = secs.saturating_sub(1);
        } else {
            nanos -= span.subsec_nanos();
        }
        Self { secs, nanos }
    }
}

/// The kernel's honest assessment of how trustworthy the wall-clock reading
/// it returns is.
///
/// Ordering on disk and in the log stays on the monotonic clock and sequence
/// numbers; this state is *provenance* metadata so a consumer can tell a
/// firmware-seeded guess from a network-synchronised time. The kernel attests
/// it from its own clock state, never from caller-supplied bytes.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Default)]
pub enum WallTimeState {
    /// No wall time has ever been set this boot; the reading is the Unix
    /// epoch placeholder and carries no real-world meaning.
    #[default]
    Unset = 0,
    /// Seeded once from firmware / an RTC at boot — plausibly close but not
    /// independently verified.
    Firmware = 1,
    /// Set by a trusted time source (e.g. an authenticated network time
    /// service).
    Trusted = 2,
    /// A previously-set wall time was corrected after the fact (a step
    /// adjustment); the offset is no longer the original source's.
    Adjusted = 3,
}

impl WallTimeState {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a [`WallTimeState`] from its wire discriminant.
    ///
    /// Returns [`Errno::OutOfRange`] for any value that is not a defined
    /// variant — never inventing a state (fail closed).
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Unset),
            1 => Ok(Self::Firmware),
            2 => Ok(Self::Trusted),
            3 => Ok(Self::Adjusted),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// `true` if a real wall time has been established (any state other than
    /// [`Unset`](Self::Unset)).
    #[must_use]
    pub const fn is_set(self) -> bool {
        !matches!(self, Self::Unset)
    }
}

/// A wall-clock reading: the absolute [`Time64`] instant plus the
/// [`WallTimeState`] describing how trustworthy it is.
///
/// This is the value the `wall_time_get` syscall returns and the carrier the
/// log layer stamps records with. The seconds-and-nanoseconds instant is
/// 64-bit-native; the state byte is the provenance tag.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct WallClockReading {
    time: Time64,
    state: WallTimeState,
}

impl WallClockReading {
    /// Encoded size on the wire: a [`Time64`] (12 bytes) plus the
    /// one-byte [`WallTimeState`] discriminant.
    pub const WIRE_LEN: usize = Time64::WIRE_LEN + 1;

    /// Construct a reading from an instant and its provenance state.
    #[must_use]
    pub const fn new(time: Time64, state: WallTimeState) -> Self {
        Self { time, state }
    }

    /// The reading with no wall time established: the Unix epoch tagged
    /// [`WallTimeState::Unset`].
    pub const UNSET: Self = Self::new(Time64::UNIX_EPOCH, WallTimeState::Unset);

    /// The absolute instant.
    #[must_use]
    pub const fn time(&self) -> Time64 {
        self.time
    }

    /// The provenance state.
    #[must_use]
    pub const fn state(&self) -> WallTimeState {
        self.state
    }

    /// Encode `self` little-endian into a fixed-size buffer.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[..Time64::WIRE_LEN].copy_from_slice(&self.time.to_le_bytes());
        out[Time64::WIRE_LEN] = self.state.as_u8();
        out
    }

    /// Decode a reading from `bytes`.
    ///
    /// Fails closed: [`Errno::BufferTooSmall`] if `bytes` is shorter than
    /// [`Self::WIRE_LEN`], [`Errno::TimestampOutOfRange`] if the instant's
    /// nanosecond field is non-canonical, or [`Errno::OutOfRange`] if the
    /// state byte is not a defined variant.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let time = Time64::from_bytes(&bytes[..Time64::WIRE_LEN])?;
        let state = WallTimeState::from_u8(bytes[Time64::WIRE_LEN])?;
        Ok(Self { time, state })
    }
}

/// A span of time: signed seconds plus a nanosecond field in
/// `0..NANOS_PER_SEC`.
///
/// The companion to [`Time64`]. A negative span is
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
    use super::{coarsen_clock_ns, Duration64, Time64, COARSE_CLOCK_GRANULARITY_NS, NANOS_PER_SEC};
    use crate::Errno;

    #[test]
    fn coarsen_floors_to_granularity_and_never_exceeds_input() {
        let g = COARSE_CLOCK_GRANULARITY_NS;
        assert_eq!(coarsen_clock_ns(0), 0);
        assert_eq!(coarsen_clock_ns(g - 1), 0);
        assert_eq!(coarsen_clock_ns(g), g);
        assert_eq!(coarsen_clock_ns(g + 1), g);
        assert_eq!(coarsen_clock_ns(2 * g - 1), g);
        for ns in [0u64, 1, 999, 1_000, 1_001, 123_456, u64::MAX] {
            let c = coarsen_clock_ns(ns);
            assert!(c <= ns, "coarsened {c} must not exceed raw {ns}");
            assert_eq!(c % g, 0, "coarsened {c} must be a multiple of {g}");
            assert!(ns - c < g, "coarsening must drop strictly less than {g}");
        }
    }

    #[test]
    fn coarsen_is_monotonic_non_decreasing() {
        let mut prev = 0;
        for ns in 0u64..5_000 {
            let c = coarsen_clock_ns(ns);
            assert!(c >= prev, "coarsened sequence must not decrease");
            prev = c;
        }
    }

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
    fn saturating_add_carries_a_whole_second() {
        let base = Time64::new(100, 800_000_000).unwrap();
        let sum = base.saturating_add(super::Duration64::new(1, 300_000_000).unwrap());
        assert_eq!(sum.secs(), 102);
        assert_eq!(sum.subsec_nanos(), 100_000_000);
    }

    #[test]
    fn saturating_add_no_carry() {
        let base = Time64::new(-5, 100).unwrap();
        let sum = base.saturating_add(super::Duration64::new(2, 200).unwrap());
        assert_eq!(sum.secs(), -3);
        assert_eq!(sum.subsec_nanos(), 300);
    }

    #[test]
    fn saturating_add_clamps_at_i64_max() {
        let base = Time64::from_secs(i64::MAX);
        let sum = base.saturating_add(super::Duration64::from_secs(1_000));
        assert_eq!(sum.secs(), i64::MAX);
    }

    #[test]
    fn saturating_sub_borrows_a_whole_second() {
        let base = Time64::new(102, 100_000_000).unwrap();
        let diff = base.saturating_sub(super::Duration64::new(1, 300_000_000).unwrap());
        assert_eq!(diff.secs(), 100);
        assert_eq!(diff.subsec_nanos(), 800_000_000);
    }

    #[test]
    fn saturating_sub_no_borrow() {
        let base = Time64::new(-3, 300).unwrap();
        let diff = base.saturating_sub(super::Duration64::new(2, 200).unwrap());
        assert_eq!(diff.secs(), -5);
        assert_eq!(diff.subsec_nanos(), 100);
    }

    #[test]
    fn saturating_sub_is_the_inverse_of_add() {
        // wall_now - since_boot then + since_boot recovers wall_now (the
        // uptime feed's boot-instant projection is round-trip stable).
        let wall = Time64::new(1_700_000_000, 250_000_000).unwrap();
        let span = super::Duration64::new(42, 900_000_000).unwrap();
        assert_eq!(wall.saturating_sub(span).saturating_add(span), wall);
    }

    #[test]
    fn saturating_sub_clamps_at_i64_min() {
        let base = Time64::from_secs(i64::MIN);
        let diff = base.saturating_sub(super::Duration64::from_secs(1_000));
        assert_eq!(diff.secs(), i64::MIN);
    }

    #[test]
    fn wall_time_state_round_trips_and_rejects_unknown() {
        use super::WallTimeState;
        for s in [
            WallTimeState::Unset,
            WallTimeState::Firmware,
            WallTimeState::Trusted,
            WallTimeState::Adjusted,
        ] {
            assert_eq!(WallTimeState::from_u8(s.as_u8()), Ok(s));
        }
        assert_eq!(WallTimeState::default(), WallTimeState::Unset);
        assert!(!WallTimeState::Unset.is_set());
        assert!(WallTimeState::Firmware.is_set());
        assert!(WallTimeState::Trusted.is_set());
        assert!(WallTimeState::Adjusted.is_set());
        assert_eq!(WallTimeState::from_u8(4), Err(Errno::OutOfRange));
        assert_eq!(WallTimeState::from_u8(0xff), Err(Errno::OutOfRange));
    }

    #[test]
    fn wall_clock_reading_round_trips() {
        use super::{WallClockReading, WallTimeState};
        let r = WallClockReading::new(
            Time64::new(1_700_000_000, 123_456_789).unwrap(),
            WallTimeState::Trusted,
        );
        let bytes = r.to_le_bytes();
        assert_eq!(bytes.len(), WallClockReading::WIRE_LEN);
        let decoded = WallClockReading::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, r);
        assert_eq!(decoded.time().secs(), 1_700_000_000);
        assert_eq!(decoded.state(), WallTimeState::Trusted);
    }

    #[test]
    fn wall_clock_reading_unset_is_epoch_and_unset() {
        use super::{WallClockReading, WallTimeState};
        assert_eq!(WallClockReading::UNSET.time(), Time64::UNIX_EPOCH);
        assert_eq!(WallClockReading::UNSET.state(), WallTimeState::Unset);
        assert_eq!(WallClockReading::default(), WallClockReading::UNSET);
    }

    #[test]
    fn wall_clock_reading_fails_closed() {
        use super::WallClockReading;
        assert_eq!(
            WallClockReading::from_bytes(&[0u8; WallClockReading::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // Bad state byte at the trailing position.
        let mut bytes = [0u8; WallClockReading::WIRE_LEN];
        bytes[Time64::WIRE_LEN] = 9;
        assert_eq!(WallClockReading::from_bytes(&bytes), Err(Errno::OutOfRange));
        // Non-canonical nanos in the instant.
        let mut bytes = [0u8; WallClockReading::WIRE_LEN];
        bytes[8..12].copy_from_slice(&NANOS_PER_SEC.to_le_bytes());
        assert_eq!(
            WallClockReading::from_bytes(&bytes),
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
