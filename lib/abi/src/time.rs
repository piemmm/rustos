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

/// Nanoseconds in one millisecond.
///
/// Here beside [`NANOS_PER_SEC`] so a millisecond-valued setting or budget has
/// one conversion to the nanoseconds every kernel deadline is expressed in,
/// rather than a private copy per subsystem.
pub const NANOS_PER_MILLI: u64 = 1_000_000;

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

/// The epoch of this TAIRiX release, in whole seconds since the Unix epoch
/// (`2026-01-01T00:00:00Z`).
///
/// No running TAIRiX system can legitimately believe the current time is
/// *before* the release it was built from, so this is the floor of the
/// plausibility window a time source's reading is checked against
/// (`plans/TIMESYNC.md`): a reading below it means the clock is wildly
/// wrong, not merely stale. It is a fixed validation bound, never a
/// capacity — widening it to admit an implausible reading would defeat the
/// check — and it is bumped at each release like any other version
/// constant.
///
/// Deliberately coarser than the exact build timestamp: a compile-time
/// build stamp would make the value vary per build and cost the
/// reproducible-build guarantee that a pinned toolchain and locked
/// dependency tree produce a bit-identical image.
pub const RELEASE_EPOCH_SECS: i64 = 1_767_225_600;

/// Width of the plausibility window above [`RELEASE_EPOCH_SECS`], in whole
/// seconds (100 Julian years).
///
/// A reading at or beyond `RELEASE_EPOCH_SECS + PLAUSIBLE_FUTURE_SECS` is
/// nonsense from a clock that has lost its mind (a stopped oscillator
/// reading as all-ones, a hostile time source) rather than a machine that
/// has genuinely been running for a century. Like the floor, a fixed
/// validation bound.
pub const PLAUSIBLE_FUTURE_SECS: i64 = 3_155_760_000;

/// Most network time servers a machine may be configured with.
///
/// One definition, shared by the configuration store that validates the list
/// and the NTP client engine whose state holds it, so a configured server can
/// never be silently past the engine's reach. A fixed validation bound on
/// configuration input, not a capacity that should scale with the machine: a
/// client needs a handful of servers to be robust, and a longer list would
/// only spread queries thinner while enlarging the state a hostile server set
/// can occupy.
pub const MAX_TIME_SERVERS: usize = 8;

/// Whether `time` falls inside the plausibility window
/// `RELEASE_EPOCH_SECS ..= RELEASE_EPOCH_SECS + PLAUSIBLE_FUTURE_SECS`.
///
/// The one definition of the window test, so a time source, the kernel, and
/// any tool that reports on the clock cannot drift apart on what
/// "implausible" means. It is a *plausibility* judgement, not an
/// authorisation one: a principal holding `CAP_TIME_SET` may still set a
/// deliberately odd time, and this never becomes an ambient veto on that.
#[must_use]
pub const fn is_plausible_wall_time(time: Time64) -> bool {
    let secs = time.secs();
    secs >= RELEASE_EPOCH_SECS && secs <= RELEASE_EPOCH_SECS + PLAUSIBLE_FUTURE_SECS
}

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

/// The monotonic clock behind a seam, so an engine that *measures* elapsed
/// time is host-testable against a clock a test advances by hand.
///
/// Only differences between two readings are meaningful — the epoch is
/// unspecified — and a reading that fails to advance yields a zero-length
/// span rather than a negative one. The production implementation is the
/// unprivileged `clock_get` syscall, coarsened to
/// [`COARSE_CLOCK_GRANULARITY_NS`] for a caller without `CAP_TIME_HIRES`.
pub trait MonotonicClock {
    /// A monotonically non-decreasing nanosecond reading.
    fn now_ns(&self) -> u64;
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

    /// The span from `earlier` to `self`, saturating the seconds at the `i64`
    /// bounds rather than wrapping.
    ///
    /// The difference of two instants, completing the pair with
    /// [`saturating_add`](Self::saturating_add) and
    /// [`saturating_sub`](Self::saturating_sub) (which offset one instant *by*
    /// a span). Its first user asks "how long ago did this recorded event
    /// happen?" of a persisted [`Time64`] stamp.
    ///
    /// A `self` that precedes `earlier` yields a negative span rather than
    /// silently clamping to zero, so a caller can tell "no time has passed"
    /// from "the ordering is not what I assumed" (a stepped clock, a record
    /// from the future) and decide for itself.
    ///
    /// [`saturating_add`]: Self::saturating_add
    #[must_use]
    pub fn saturating_duration_since(self, earlier: Self) -> Duration64 {
        let mut secs = self.secs.saturating_sub(earlier.secs);
        let nanos = if self.nanos < earlier.nanos {
            // Borrow one whole second to cover the nanosecond underflow; both
            // fields are `< NANOS_PER_SEC`, so one borrow always suffices and
            // the result stays canonical.
            secs = secs.saturating_sub(1);
            self.nanos + NANOS_PER_SEC - earlier.nanos
        } else {
            self.nanos - earlier.nanos
        };
        Duration64 { secs, nanos }
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

    /// How much a reading in this state may be trusted, for the ordering
    /// [`supersedes`](Self::supersedes) enforces.
    ///
    /// [`Trusted`](Self::Trusted) and [`Adjusted`](Self::Adjusted) share a
    /// rank deliberately: both describe a principal that examined the clock
    /// and decided it was wrong — a validated network sample, or a human
    /// stepping it by hand. Neither outranks the other, so a later network
    /// sync still corrects a manual step and a later manual step still
    /// corrects a sync, while *neither* can be undone by a local counter.
    const fn trust_rank(self) -> u8 {
        match self {
            Self::Unset => 0,
            Self::Firmware => 1,
            Self::Trusted | Self::Adjusted => 2,
        }
    }

    /// `true` when a write in this state may replace a clock currently in
    /// `current` — the wall clock's **provenance ladder**.
    ///
    /// A local counter ([`Firmware`](Self::Firmware): an RTC, firmware
    /// hand-off) establishes an [`Unset`](Self::Unset) clock and may replace
    /// another such counter's reading, but it can never overwrite a validated
    /// network sample or a deliberate correction. Rolling a machine's clock
    /// backwards is how an expired certificate is revived and how audit
    /// reasoning is reordered, so the kernel refuses the write rather than
    /// trusting every clock source to be polite.
    #[must_use]
    pub const fn supersedes(self, current: Self) -> bool {
        self.trust_rank() >= current.trust_rank()
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

    /// The span as a whole nanosecond count, saturating at [`u64::MAX`] and
    /// flooring a negative span at zero.
    ///
    /// The inverse of [`from_nanos`](Self::from_nanos), for the callers that
    /// hand a span to an interface counting plain nanoseconds (the kernel's
    /// monotonic timeouts and deadlines). A negative span has no meaning as an
    /// unsigned count, so it reports zero rather than wrapping to an enormous
    /// positive one — the direction that cannot turn a backwards clock into an
    /// effectively infinite wait.
    #[must_use]
    pub fn saturating_total_nanos(&self) -> u64 {
        let Ok(secs) = u64::try_from(self.secs) else {
            return 0;
        };
        secs.saturating_mul(u64::from(NANOS_PER_SEC))
            .saturating_add(u64::from(self.nanos))
    }
}

/// Seconds in one UTC day.
pub const SECS_PER_DAY: i64 = 86_400;

/// Days from the Unix epoch (1970-01-01) to the given proleptic-Gregorian
/// civil date. `month` is `1..=12`, `day` is `1..=31`; the caller validates
/// the ranges. Negative for dates before the epoch.
///
/// Howard Hinnant's `days_from_civil` algorithm, exact for every date in the
/// `i64` range.
#[must_use]
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = i64::from(month);
    let d = i64::from(day);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic-Gregorian civil date `(year, month, day)` for a count of days
/// from the Unix epoch — the inverse of [`days_from_civil`].
#[must_use]
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    // `mp` is `0..=11`, so `m` is `1..=12` and `d` is `1..=31`: both fit `u32`.
    (year, narrow_small(m), narrow_small(d))
}

/// Narrow a known-small, non-negative `i64` calendar component to `u32`.
///
/// Applied only to a month, day, or time-of-day component the caller has just
/// derived in range, so the zero fallback is unreachable; it exists so the
/// conversion is total rather than a panicking cast.
fn narrow_small(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

/// Days in `month` of `year`, honouring the proleptic-Gregorian leap rule.
///
/// Returns `0` for a month outside `1..=12`, so a caller validating a
/// hardware register block's field rejects it rather than admitting a date
/// the calendar cannot hold.
#[must_use]
pub const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// A broken-down UTC civil time: the calendar fields of an absolute instant.
///
/// The one decomposition of a count of seconds since the Unix epoch into
/// `(year, month, day, hour, minute, second)`, so no consumer — a listing's
/// date column, the desktop clock, or an RTC chip's BCD register block
/// ([`crate::driver::rtc`]) — re-derives the day/time arithmetic. All fields
/// are UTC; TAIRiX has no timezone offset to apply here.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CivilTime {
    /// Proleptic-Gregorian year (may be negative for dates before year 1).
    pub year: i64,
    /// Month of the year, `1..=12`.
    pub month: u32,
    /// Day of the month, `1..=31`.
    pub day: u32,
    /// Hour of the day, `0..=23`.
    pub hour: u32,
    /// Minute of the hour, `0..=59`.
    pub minute: u32,
    /// Second of the minute, `0..=59` (no leap seconds).
    pub second: u32,
}

impl CivilTime {
    /// Decompose `secs` seconds since the Unix epoch into UTC calendar
    /// fields. Negative `secs` (instants before 1970) are handled exactly
    /// through Euclidean division, so the time-of-day is always in range.
    #[must_use]
    pub fn from_unix_secs(secs: i64) -> Self {
        let days = secs.div_euclid(SECS_PER_DAY);
        let tod = secs.rem_euclid(SECS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: narrow_small(tod / 3_600),
            minute: narrow_small((tod % 3_600) / 60),
            second: narrow_small(tod % 60),
        }
    }

    /// Decompose the whole-seconds part of a [`Time64`] instant. The
    /// sub-second field is not part of the calendar breakdown; a consumer
    /// that renders nanoseconds reads [`Time64::subsec_nanos`] itself.
    #[must_use]
    pub fn from_time64(time: Time64) -> Self {
        Self::from_unix_secs(time.secs())
    }

    /// `true` when every field is inside its calendar range, including the
    /// month's own day count under the year's leap rule.
    ///
    /// Hour `24` and second `60` are rejected: TAIRiX models no leap second
    /// and no end-of-day alias, so a source offering either is offering a
    /// value the calendar cannot represent.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.month >= 1
            && self.month <= 12
            && self.day >= 1
            && self.day <= days_in_month(self.year, self.month)
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
    }

    /// The whole-second Unix instant these fields name, or `None` when they
    /// are not a valid civil time ([`Self::is_valid`]).
    ///
    /// Fails closed rather than normalising an out-of-range field, so a
    /// corrupt register block never yields a plausible-looking instant.
    #[must_use]
    pub fn to_unix_secs(&self) -> Option<i64> {
        if !self.is_valid() {
            return None;
        }
        let days = days_from_civil(self.year, self.month, self.day);
        Some(
            days * SECS_PER_DAY
                + i64::from(self.hour) * 3_600
                + i64::from(self.minute) * 60
                + i64::from(self.second),
        )
    }

    /// The whole-second [`Time64`] instant these fields name, or `None` when
    /// they are not a valid civil time ([`Self::is_valid`]).
    #[must_use]
    pub fn to_time64(&self) -> Option<Time64> {
        self.to_unix_secs().map(Time64::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        civil_from_days, coarsen_clock_ns, days_from_civil, days_in_month, is_plausible_wall_time,
        CivilTime, Duration64, Time64, COARSE_CLOCK_GRANULARITY_NS, NANOS_PER_SEC,
        PLAUSIBLE_FUTURE_SECS, RELEASE_EPOCH_SECS,
    };
    use crate::Errno;

    #[test]
    fn plausibility_window_admits_the_release_epoch_and_its_ceiling() {
        assert!(is_plausible_wall_time(Time64::from_secs(
            RELEASE_EPOCH_SECS
        )));
        assert!(is_plausible_wall_time(Time64::from_secs(
            RELEASE_EPOCH_SECS + PLAUSIBLE_FUTURE_SECS
        )));
        // Well inside: a decade past the release.
        assert!(is_plausible_wall_time(Time64::from_secs(
            RELEASE_EPOCH_SECS + 315_576_000
        )));
    }

    #[test]
    fn plausibility_window_refuses_readings_outside_it() {
        // The Unix epoch itself: the placeholder an unset clock reports.
        assert!(!is_plausible_wall_time(Time64::UNIX_EPOCH));
        // One second before the release cannot be the current time.
        assert!(!is_plausible_wall_time(Time64::from_secs(
            RELEASE_EPOCH_SECS - 1
        )));
        // Pre-1970 and the far future are both nonsense from a clock.
        assert!(!is_plausible_wall_time(Time64::from_secs(-1)));
        assert!(!is_plausible_wall_time(Time64::from_secs(
            RELEASE_EPOCH_SECS + PLAUSIBLE_FUTURE_SECS + 1
        )));
        assert!(!is_plausible_wall_time(Time64::from_secs(i64::MAX)));
        assert!(!is_plausible_wall_time(Time64::from_secs(i64::MIN)));
    }

    #[test]
    fn plausibility_window_ignores_the_subsecond_field() {
        // The floor is a whole-second bound, so nanoseconds never decide it.
        let at_floor = Time64::new(RELEASE_EPOCH_SECS, NANOS_PER_SEC - 1).expect("canonical");
        assert!(is_plausible_wall_time(at_floor));
        let below = Time64::new(RELEASE_EPOCH_SECS - 1, NANOS_PER_SEC - 1).expect("canonical");
        assert!(!is_plausible_wall_time(below));
    }

    #[test]
    fn plausible_future_is_a_century_and_the_ceiling_cannot_overflow() {
        // 100 Julian years, so the window is documented in the same unit the
        // constant claims.
        assert_eq!(PLAUSIBLE_FUTURE_SECS, 100 * 365 * 86_400 + 25 * 86_400);
        assert!(RELEASE_EPOCH_SECS
            .checked_add(PLAUSIBLE_FUTURE_SECS)
            .is_some());
    }

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

    #[test]
    fn duration_since_measures_the_span_between_two_instants() {
        let earlier = Time64::new(1_700_000_000, 900_000_000).unwrap();
        let later = Time64::new(1_700_000_003, 100_000_000).unwrap();
        let span = later.saturating_duration_since(earlier);
        // Two whole seconds plus 200 ms: the nanosecond field borrows one
        // second and stays canonical.
        assert_eq!(span.secs(), 2);
        assert_eq!(span.subsec_nanos(), 200_000_000);
        assert_eq!(span.saturating_total_nanos(), 2_200_000_000);
        // It is the exact inverse of offsetting the earlier instant forward.
        assert_eq!(earlier.saturating_add(span), later);
    }

    #[test]
    fn duration_since_is_zero_for_the_same_instant() {
        let t = Time64::new(-1, 5).unwrap();
        assert_eq!(t.saturating_duration_since(t), Duration64::ZERO);
    }

    #[test]
    fn duration_since_reports_a_negative_span_when_ordering_reverses() {
        // A "later" instant that actually precedes `earlier` (a stepped clock,
        // or a record claiming the future) must be visible as negative, not
        // silently clamped, so the caller can reject it.
        let earlier = Time64::from_secs(2_000_000_000);
        let later = Time64::from_secs(1_000_000_000);
        let span = later.saturating_duration_since(earlier);
        assert!(span < Duration64::ZERO);
        // As an unsigned nanosecond count a negative span floors at zero
        // rather than wrapping to an effectively infinite wait.
        assert_eq!(span.saturating_total_nanos(), 0);
    }

    #[test]
    fn duration_since_spans_the_epoch_and_the_2038_boundary() {
        // 1901-12-13 to 2038-01-19: a span no 32-bit second count could hold.
        let earlier = Time64::new(-2_147_483_648, 1).unwrap();
        let later = Time64::new(2_147_483_648, 0).unwrap();
        let span = later.saturating_duration_since(earlier);
        assert_eq!(span.secs(), 4_294_967_295);
        assert_eq!(span.subsec_nanos(), 999_999_999);
        assert_eq!(earlier.saturating_add(span), later);
    }

    #[test]
    fn duration_since_saturates_instead_of_wrapping() {
        let span = Time64::from_secs(i64::MAX).saturating_duration_since(Time64::from_secs(-1));
        assert_eq!(span.secs(), i64::MAX);
    }

    #[test]
    fn a_local_counter_never_overwrites_a_validated_reading() {
        use crate::WallTimeState::{Adjusted, Firmware, Trusted, Unset};
        // Firmware establishes an unset clock and may replace another
        // counter's reading...
        assert!(Firmware.supersedes(Unset));
        assert!(Firmware.supersedes(Firmware));
        // ...but never a network sync or a deliberate correction.
        assert!(!Firmware.supersedes(Trusted));
        assert!(!Firmware.supersedes(Adjusted));
    }

    #[test]
    fn a_deliberate_source_may_replace_any_reading() {
        use crate::WallTimeState::{Adjusted, Firmware, Trusted, Unset};
        for source in [Trusted, Adjusted] {
            for current in [Unset, Firmware, Trusted, Adjusted] {
                assert!(
                    source.supersedes(current),
                    "{source:?} must be able to replace {current:?}"
                );
            }
        }
    }

    #[test]
    fn the_ladder_is_reflexive_so_a_source_can_refresh_itself() {
        use crate::WallTimeState::{Adjusted, Firmware, Trusted, Unset};
        for state in [Unset, Firmware, Trusted, Adjusted] {
            assert!(state.supersedes(state), "{state:?} must refresh itself");
        }
    }

    #[test]
    fn civil_round_trips_across_the_epoch_and_leap_days() {
        for secs in [
            0,               // 1970-01-01T00:00:00Z
            -1,              // 1969-12-31T23:59:59Z
            -2_208_988_800,  // 1900-01-01T00:00:00Z (not a leap year)
            951_782_400,     // 2000-02-29T00:00:00Z (a leap year)
            1_709_214_367,   // 2024-02-29T13:46:07Z
            2_147_483_648,   // one second past the 32-bit boundary
            4_102_444_800,   // 2100-01-01T00:00:00Z (not a leap year)
            253_402_300_799, // 9999-12-31T23:59:59Z
        ] {
            let civil = CivilTime::from_unix_secs(secs);
            assert!(civil.is_valid(), "{secs} decomposed to an invalid date");
            assert_eq!(civil.to_unix_secs(), Some(secs), "round trip of {secs}");
        }
    }

    #[test]
    fn known_instants_decompose_field_for_field() {
        let epoch = CivilTime::from_unix_secs(0);
        assert_eq!((epoch.year, epoch.month, epoch.day), (1970, 1, 1));
        assert_eq!((epoch.hour, epoch.minute, epoch.second), (0, 0, 0));

        // A leap day past 2038, so the decomposition is exercised well
        // outside the 32-bit range.
        let leap = CivilTime::from_unix_secs(1_709_214_367);
        assert_eq!((leap.year, leap.month, leap.day), (2024, 2, 29));
        assert_eq!((leap.hour, leap.minute, leap.second), (13, 46, 7));

        // One second before the epoch keeps its time-of-day in range rather
        // than wrapping negative.
        let pre = CivilTime::from_unix_secs(-1);
        assert_eq!((pre.year, pre.month, pre.day), (1969, 12, 31));
        assert_eq!((pre.hour, pre.minute, pre.second), (23, 59, 59));
    }

    #[test]
    fn from_time64_ignores_the_sub_second_field() {
        let time = Time64::new(1_709_214_367, 500_000_000).expect("valid nanos");
        assert_eq!(
            CivilTime::from_time64(time),
            CivilTime::from_unix_secs(1_709_214_367)
        );
        assert_eq!(
            CivilTime::from_time64(time).to_time64(),
            Some(Time64::from_secs(1_709_214_367))
        );
    }

    #[test]
    fn an_out_of_range_field_fails_closed_instead_of_normalising() {
        let base = CivilTime::from_unix_secs(0);
        for bad in [
            CivilTime { month: 0, ..base },
            CivilTime { month: 13, ..base },
            CivilTime { day: 0, ..base },
            CivilTime { day: 32, ..base },
            // 2023 is not a leap year, so 29 February does not exist.
            CivilTime {
                year: 2023,
                month: 2,
                day: 29,
                ..base
            },
            CivilTime { hour: 24, ..base },
            CivilTime { minute: 60, ..base },
            // No leap second: 60 is not a representable second.
            CivilTime { second: 60, ..base },
        ] {
            assert!(!bad.is_valid(), "{bad:?} must not validate");
            assert_eq!(bad.to_unix_secs(), None, "{bad:?} must not convert");
            assert_eq!(bad.to_time64(), None, "{bad:?} must not convert");
        }
        // The leap day the rule *does* admit still converts.
        let leap = CivilTime {
            year: 2024,
            month: 2,
            day: 29,
            ..base
        };
        assert!(leap.is_valid());
        assert!(leap.to_time64().is_some());
    }

    #[test]
    fn days_in_month_follows_the_gregorian_leap_rule() {
        assert_eq!(days_in_month(2024, 2), 29, "divisible by 4");
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(1900, 2), 28, "century, not divisible by 400");
        assert_eq!(days_in_month(2000, 2), 29, "divisible by 400");
        assert_eq!(days_in_month(2024, 4), 30);
        assert_eq!(days_in_month(2024, 12), 31);
        // An out-of-range month yields zero, so every day fails validation.
        assert_eq!(days_in_month(2024, 0), 0);
        assert_eq!(days_in_month(2024, 13), 0);
    }

    #[test]
    fn epoch_day_anchors_match_the_calendar() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }

    #[test]
    fn total_nanos_round_trips_from_nanos_and_saturates() {
        for ns in [0u64, 1, 999_999_999, 1_000_000_000, 2_500_000_001] {
            assert_eq!(Duration64::from_nanos(ns).saturating_total_nanos(), ns);
        }
        // A span far beyond what a nanosecond count can express saturates at
        // the ceiling rather than wrapping to a small value.
        assert_eq!(
            Duration64::from_secs(i64::MAX).saturating_total_nanos(),
            u64::MAX
        );
    }
}
