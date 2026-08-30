//! The real-time-clock service protocol (`plans/TIMESYNC.md` TS-3).
//!
//! An RTC driver (`drivers/rtc/*`) owns its chip's registers and nothing
//! else; the machine clock belongs to the one process holding
//! [`CapabilityId::TIME_SET`](crate::CapabilityId::TIME_SET)
//! (`userland/system/timed`). This module is the wire contract between them:
//! a synchronous call endpoint at the well-known [`RTC_ENDPOINT`] over which
//! the clock authority reads the chip and writes a synced instant back to it.
//!
//! Splitting it this way is what makes the wall clock's provenance ladder
//! ([`WallTimeState::supersedes`](crate::WallTimeState::supersedes)) mean
//! something. `wall_time_set` takes its provenance from the caller, so a
//! driver holding the clock capability could simply assert `Trusted` and the
//! ladder would be worthless. Here the driver reports a *chip reading* and
//! `timed` tags it `Firmware` — the worst a compromised RTC driver can do is
//! lie about a time that can never overwrite a network sync.
//!
//! The framing follows [`crate::mailbox_ipc`]: borrowed buffers, no
//! allocation, and a status-framed reply so a fail-closed refusal arrives
//! in-band rather than as a truncated payload.
//!
//! # One endpoint, the first RTC discovered
//!
//! The id is a single reserved well-known value rather than a per-instance
//! slot range. Every board TAIRiX targets exposes exactly one RTC, and a
//! second one would need a *selection policy* — which chip is authoritative —
//! that no consumer has today; inventing slots without it would be surface
//! with no reader. A second driver's [`call_create`] therefore fails closed
//! with [`Errno::AlreadyExists`], which it logs and exits on, so the outcome
//! is the first RTC in hardware-tree order (deterministic) and the situation
//! is visible in the log rather than silently arbitrary.
//!
//! [`call_create`]: crate::SyscallNumber::CALL_CREATE

use crate::driver::rtc::{Rtc, RtcStatus};
use crate::le::{put_i32, put_u32, read_i32, read_u32};
use crate::time::{Duration64, Time64};
use crate::Errno;

/// Well-known kernel-owned call-endpoint id of the RTC service.
///
/// The bytes spell `"RTC\0"`. A driver binds it restricted-sender under
/// [`CapabilityId::TIME_SET`](crate::CapabilityId::TIME_SET): the only
/// principal with a reason to read or write the board's clock chip is the one
/// that sets the machine clock from it, so the kernel admits nobody else and
/// no new capability is needed for the seam.
pub const RTC_ENDPOINT: u64 = 0x5254_4300;

/// The operation a request names.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RtcOp {
    /// Read the chip: its instant, if it can vouch for one, and its status.
    Read = 1,
    /// Write an instant to the chip, clearing its oscillator-stopped flag.
    Set = 2,
}

impl RtcOp {
    /// Decode an operation from its wire word.
    ///
    /// Zero is deliberately not a defined operation, so an all-zero frame is
    /// refused rather than read as a request.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any undefined value.
    pub const fn from_u32(raw: u32) -> Result<Self, Errno> {
        match raw {
            1 => Ok(Self::Read),
            2 => Ok(Self::Set),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }
}

/// Fixed prefix of every reply frame: a status word (`0` on success, else the
/// negated [`Errno`] discriminant).
const REPLY_STATUS_LEN: usize = 4;

/// Encoded length of a request: the operation word followed by the instant a
/// [`RtcOp::Set`] carries. A [`RtcOp::Read`] leaves the instant zeroed, so
/// every frame is one fixed size and the endpoint needs one bound.
pub const REQUEST_LEN: usize = 4 + Time64::WIRE_LEN;

/// Encoded length of a successful reply: the status word, the flag word, the
/// instant, and the chip's declared precision.
pub const REPLY_LEN: usize = REPLY_STATUS_LEN + 4 + Time64::WIRE_LEN + Duration64::WIRE_LEN;

/// Reply flag: the chip vouched for the instant this frame carries. When
/// clear the instant field is meaningless and a decoder discards it.
const FLAG_TIME_PRESENT: u32 = 1 << 0;
/// Reply flag: the counter is kept running across a power cycle.
const FLAG_BATTERY_BACKED: u32 = 1 << 1;
/// Reply flag: the chip reports its oscillator stopped since the last write.
const FLAG_OSCILLATOR_STOPPED: u32 = 1 << 2;
/// Every flag bit this version defines; a frame setting any other is refused.
const FLAG_KNOWN_MASK: u32 = FLAG_TIME_PRESENT | FLAG_BATTERY_BACKED | FLAG_OSCILLATOR_STOPPED;

/// What a [`RtcOp::Read`] answered: the instant the chip could vouch for (if
/// any) and the status it reported alongside.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RtcReading {
    /// The chip's instant, or `None` when it could not vouch for one — a
    /// stopped oscillator, a set clock-integrity flag, or a register block
    /// that is not a real calendar date. Never a fabricated stand-in.
    pub time: Option<Time64>,
    /// What the chip said about itself at the same moment, so a consumer can
    /// report *why* it has no instant.
    pub status: RtcStatus,
}

/// Encode a request into `buf`, returning the bytes written.
///
/// `time` is written only for [`RtcOp::Set`]; a [`RtcOp::Read`] zeroes the
/// field so the frame is one fixed shape.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold [`REQUEST_LEN`] bytes.
pub fn encode_request(buf: &mut [u8], op: RtcOp, time: Time64) -> Result<usize, Errno> {
    if buf.len() < REQUEST_LEN {
        return Err(Errno::BufferTooSmall);
    }
    put_u32(buf, 0, op.as_u32());
    let instant = if op == RtcOp::Set {
        time
    } else {
        Time64::UNIX_EPOCH
    };
    buf[4..REQUEST_LEN].copy_from_slice(&instant.to_le_bytes());
    Ok(REQUEST_LEN)
}

/// Decode a request from `bytes`.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] if `bytes` is shorter than [`REQUEST_LEN`],
/// [`Errno::OutOfRange`] if the operation word is undefined, or
/// [`Errno::TimestampOutOfRange`] if the instant is non-canonical — all
/// before the chip is touched.
pub fn decode_request(bytes: &[u8]) -> Result<(RtcOp, Time64), Errno> {
    if bytes.len() < REQUEST_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    let op = RtcOp::from_u32(read_u32(bytes, 0))?;
    let time = Time64::from_bytes(&bytes[4..REQUEST_LEN])?;
    Ok((op, time))
}

/// Encode a successful [`RtcOp::Read`] reply into `buf`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold [`REPLY_LEN`] bytes.
pub fn encode_reading(buf: &mut [u8], reading: &RtcReading) -> Result<usize, Errno> {
    if buf.len() < REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let mut flags = 0;
    if reading.time.is_some() {
        flags |= FLAG_TIME_PRESENT;
    }
    if reading.status.battery_backed {
        flags |= FLAG_BATTERY_BACKED;
    }
    if reading.status.oscillator_stopped {
        flags |= FLAG_OSCILLATOR_STOPPED;
    }
    put_i32(buf, 0, 0);
    put_u32(buf, REPLY_STATUS_LEN, flags);
    let at = REPLY_STATUS_LEN + 4;
    let instant = reading.time.unwrap_or(Time64::UNIX_EPOCH);
    buf[at..at + Time64::WIRE_LEN].copy_from_slice(&instant.to_le_bytes());
    let at = at + Time64::WIRE_LEN;
    buf[at..at + Duration64::WIRE_LEN].copy_from_slice(&reading.status.precision.to_le_bytes());
    Ok(REPLY_LEN)
}

/// Encode a successful [`RtcOp::Set`] reply — the status word alone, since a
/// write reports nothing but whether it landed.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` is shorter than the status word.
pub fn encode_ack(buf: &mut [u8]) -> Result<usize, Errno> {
    if buf.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    put_i32(buf, 0, 0);
    Ok(REPLY_STATUS_LEN)
}

/// Encode a fail-closed error reply (status only) into `buf`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` is shorter than the status word.
pub fn encode_error_reply(buf: &mut [u8], err: Errno) -> Result<usize, Errno> {
    if buf.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    // A negative status carries `-errno`; `Errno` discriminants are positive.
    put_i32(buf, 0, -err.as_i32());
    Ok(REPLY_STATUS_LEN)
}

/// Read the status word of a reply frame, mapping a non-zero one to its
/// [`Errno`]. A status that is neither zero nor a known negated discriminant
/// is wire corruption and fails closed as [`Errno::BadMagic`].
fn reply_status(reply: &[u8]) -> Result<(), Errno> {
    if reply.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    match read_i32(reply, 0) {
        0 => Ok(()),
        negative => Err(Errno::try_from_status(negative).unwrap_or(Errno::BadMagic)),
    }
}

/// Decode a [`RtcOp::Read`] reply.
///
/// # Errors
///
/// The carried [`Errno`] for an error frame; [`Errno::BufferTooSmall`] if
/// `reply` is shorter than the status word; [`Errno::BadMagic`] if a success
/// frame is truncated or sets a flag bit this version does not define;
/// [`Errno::TimestampOutOfRange`] if a carried instant or precision is
/// non-canonical. Every one of them is a refusal, never a partial reading.
pub fn decode_reading(reply: &[u8]) -> Result<RtcReading, Errno> {
    reply_status(reply)?;
    if reply.len() < REPLY_LEN {
        return Err(Errno::BadMagic);
    }
    let flags = read_u32(reply, REPLY_STATUS_LEN);
    if flags & !FLAG_KNOWN_MASK != 0 {
        return Err(Errno::BadMagic);
    }
    let at = REPLY_STATUS_LEN + 4;
    // The instant is validated whether or not it is present: a peer that
    // cannot encode a canonical one is broken, and a broken peer is refused
    // rather than half-believed.
    let instant = Time64::from_bytes(&reply[at..at + Time64::WIRE_LEN])?;
    let at = at + Time64::WIRE_LEN;
    let precision = Duration64::from_bytes(&reply[at..at + Duration64::WIRE_LEN])?;
    Ok(RtcReading {
        time: (flags & FLAG_TIME_PRESENT != 0).then_some(instant),
        status: RtcStatus {
            precision,
            battery_backed: flags & FLAG_BATTERY_BACKED != 0,
            oscillator_stopped: flags & FLAG_OSCILLATOR_STOPPED != 0,
        },
    })
}

/// Decode a [`RtcOp::Set`] reply: `Ok(())` when the write landed.
///
/// # Errors
///
/// The carried [`Errno`] for an error frame, or [`Errno::BufferTooSmall`] if
/// `reply` is shorter than the status word.
pub fn decode_ack(reply: &[u8]) -> Result<(), Errno> {
    reply_status(reply)
}

/// Serve one request frame against `rtc`, encoding the framed reply into
/// `reply` and returning the bytes written.
///
/// The wire-level server transformation every RTC driver runs between
/// [`call_recv`](crate::SyscallNumber::CALL_RECV) and
/// [`call_reply`](crate::SyscallNumber::CALL_REPLY), so the chip logic in a
/// driver stays register access and the protocol has one implementation.
/// Every failure — a malformed request or a
/// [`DriverError`](crate::DriverError) from the chip — becomes an in-band
/// status-framed error reply, so the blocked caller is always answered and
/// fails closed.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `reply` cannot hold a status-framed reply;
/// the caller sizes it to [`REPLY_LEN`].
pub fn serve_request<R: Rtc + ?Sized>(
    rtc: &mut R,
    request: &[u8],
    reply: &mut [u8],
) -> Result<usize, Errno> {
    let (op, time) = match decode_request(request) {
        Ok(parsed) => parsed,
        Err(err) => return encode_error_reply(reply, err),
    };
    match op {
        RtcOp::Read => match (rtc.read(), rtc.status()) {
            (Ok(time), Ok(status)) => encode_reading(reply, &RtcReading { time, status }),
            (Err(err), _) | (_, Err(err)) => encode_error_reply(reply, err.as_errno()),
        },
        RtcOp::Set => match rtc.set(time) {
            Ok(()) => encode_ack(reply),
            Err(err) => encode_error_reply(reply, err.as_errno()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_ack, decode_reading, decode_request, encode_ack, encode_error_reply, encode_reading,
        encode_request, serve_request, RtcOp, RtcReading, FLAG_KNOWN_MASK, REPLY_LEN,
        REPLY_STATUS_LEN, REQUEST_LEN,
    };
    use crate::driver::rtc::{Rtc, RtcStatus};
    use crate::le::{put_i32, put_u32};
    use crate::time::{Duration64, Time64};
    use crate::{DriverError, Errno};

    fn status() -> RtcStatus {
        RtcStatus {
            precision: Duration64::from_secs(1),
            battery_backed: true,
            oscillator_stopped: false,
        }
    }

    /// An [`Rtc`] double: answers with a programmed reading, status, and set
    /// outcome, and records the last instant written.
    struct MockRtc {
        time: Result<Option<Time64>, DriverError>,
        status: Result<RtcStatus, DriverError>,
        set_result: Result<(), DriverError>,
        written: Option<Time64>,
    }

    impl MockRtc {
        fn healthy(time: Option<Time64>) -> Self {
            Self {
                time: Ok(time),
                status: Ok(status()),
                set_result: Ok(()),
                written: None,
            }
        }
    }

    impl Rtc for MockRtc {
        fn status(&mut self) -> Result<RtcStatus, DriverError> {
            self.status
        }
        fn read(&mut self) -> Result<Option<Time64>, DriverError> {
            self.time
        }
        fn set(&mut self, time: Time64) -> Result<(), DriverError> {
            self.set_result?;
            self.written = Some(time);
            Ok(())
        }
    }

    #[test]
    fn a_read_request_round_trips_and_carries_no_instant() {
        let mut buf = [0u8; REQUEST_LEN];
        let n = encode_request(&mut buf, RtcOp::Read, Time64::from_secs(99)).expect("encodes");
        assert_eq!(n, REQUEST_LEN);
        // The instant a read carries is zeroed, so the frame cannot smuggle
        // a value the driver might act on.
        assert_eq!(
            decode_request(&buf[..n]),
            Ok((RtcOp::Read, Time64::UNIX_EPOCH))
        );
    }

    #[test]
    fn a_set_request_round_trips_its_instant() {
        let time = Time64::new(1_800_000_000, 250_000_000).expect("canonical");
        let mut buf = [0u8; REQUEST_LEN];
        let n = encode_request(&mut buf, RtcOp::Set, time).expect("encodes");
        assert_eq!(decode_request(&buf[..n]), Ok((RtcOp::Set, time)));
    }

    #[test]
    fn a_malformed_request_is_refused_before_the_chip_is_touched() {
        assert_eq!(
            decode_request(&[0u8; REQUEST_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
        // An all-zero frame names no operation.
        assert_eq!(decode_request(&[0u8; REQUEST_LEN]), Err(Errno::OutOfRange));
        let mut buf = [0u8; REQUEST_LEN];
        put_u32(&mut buf, 0, 7);
        assert_eq!(decode_request(&buf), Err(Errno::OutOfRange));
        // A non-canonical nanosecond field is refused too.
        let mut buf = [0u8; REQUEST_LEN];
        put_u32(&mut buf, 0, RtcOp::Set.as_u32());
        put_u32(&mut buf, 4 + 8, 1_000_000_000);
        assert_eq!(decode_request(&buf), Err(Errno::TimestampOutOfRange));
    }

    #[test]
    fn a_reading_round_trips_with_and_without_an_instant() {
        for time in [Some(Time64::from_secs(1_800_000_000)), None] {
            let reading = RtcReading {
                time,
                status: status(),
            };
            let mut buf = [0u8; REPLY_LEN];
            let n = encode_reading(&mut buf, &reading).expect("encodes");
            assert_eq!(n, REPLY_LEN);
            assert_eq!(decode_reading(&buf[..n]), Ok(reading));
        }
    }

    #[test]
    fn an_absent_instant_never_leaks_the_field_behind_it() {
        // Even if a peer writes a plausible instant while clearing the
        // present flag, the decode yields no time.
        let mut buf = [0u8; REPLY_LEN];
        encode_reading(
            &mut buf,
            &RtcReading {
                time: Some(Time64::from_secs(1_800_000_000)),
                status: status(),
            },
        )
        .expect("encodes");
        put_u32(&mut buf, REPLY_STATUS_LEN, 0);
        assert_eq!(decode_reading(&buf).expect("decodes").time, None);
    }

    #[test]
    fn every_status_flag_survives_the_round_trip() {
        let reading = RtcReading {
            time: None,
            status: RtcStatus {
                precision: Duration64::new(0, 1).expect("canonical"),
                battery_backed: false,
                oscillator_stopped: true,
            },
        };
        let mut buf = [0u8; REPLY_LEN];
        let n = encode_reading(&mut buf, &reading).expect("encodes");
        assert_eq!(decode_reading(&buf[..n]), Ok(reading));
    }

    #[test]
    fn a_reply_fails_closed_on_corruption() {
        let reading = RtcReading {
            time: Some(Time64::from_secs(1_800_000_000)),
            status: status(),
        };
        let mut buf = [0u8; REPLY_LEN];
        encode_reading(&mut buf, &reading).expect("encodes");

        // An undefined flag bit is a peer this version does not understand.
        let mut corrupt = buf;
        put_u32(&mut corrupt, REPLY_STATUS_LEN, FLAG_KNOWN_MASK | 1 << 8);
        assert_eq!(decode_reading(&corrupt), Err(Errno::BadMagic));

        // A truncated success frame carries no reading.
        assert_eq!(decode_reading(&buf[..REPLY_LEN - 1]), Err(Errno::BadMagic));

        // A status word that is no known negated discriminant, and the most
        // negative one, both fail closed rather than aborting on negation.
        let mut corrupt = buf;
        put_i32(&mut corrupt, 0, -9_999);
        assert_eq!(decode_reading(&corrupt), Err(Errno::BadMagic));
        let mut corrupt = buf;
        put_i32(&mut corrupt, 0, i32::MIN);
        assert_eq!(decode_reading(&corrupt), Err(Errno::BadMagic));

        // A non-canonical instant in the frame is refused.
        let mut corrupt = buf;
        put_u32(&mut corrupt, REPLY_STATUS_LEN + 4 + 8, 1_000_000_000);
        assert_eq!(decode_reading(&corrupt), Err(Errno::TimestampOutOfRange));

        // So is a shorter-than-status frame.
        assert_eq!(decode_reading(&[]), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn an_error_reply_surfaces_its_errno_to_both_decoders() {
        let mut buf = [0u8; REPLY_LEN];
        let n = encode_error_reply(&mut buf, Errno::PermissionDenied).expect("encodes");
        assert_eq!(decode_reading(&buf[..n]), Err(Errno::PermissionDenied));
        assert_eq!(decode_ack(&buf[..n]), Err(Errno::PermissionDenied));
    }

    #[test]
    fn an_ack_round_trips() {
        let mut buf = [0u8; REPLY_LEN];
        let n = encode_ack(&mut buf).expect("encodes");
        assert_eq!(n, REPLY_STATUS_LEN);
        assert_eq!(decode_ack(&buf[..n]), Ok(()));
    }

    #[test]
    fn serving_a_read_answers_the_chip_reading() {
        let time = Time64::from_secs(1_800_000_000);
        let mut rtc = MockRtc::healthy(Some(time));
        let mut request = [0u8; REQUEST_LEN];
        encode_request(&mut request, RtcOp::Read, Time64::UNIX_EPOCH).expect("encodes");
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&mut rtc, &request, &mut reply).expect("serves");
        assert_eq!(
            decode_reading(&reply[..n]),
            Ok(RtcReading {
                time: Some(time),
                status: status()
            })
        );
    }

    #[test]
    fn serving_a_read_of_a_chip_that_cannot_vouch_carries_no_instant() {
        let mut rtc = MockRtc {
            time: Ok(None),
            status: Ok(RtcStatus {
                oscillator_stopped: true,
                ..status()
            }),
            set_result: Ok(()),
            written: None,
        };
        let mut request = [0u8; REQUEST_LEN];
        encode_request(&mut request, RtcOp::Read, Time64::UNIX_EPOCH).expect("encodes");
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&mut rtc, &request, &mut reply).expect("serves");
        let reading = decode_reading(&reply[..n]).expect("decodes");
        assert_eq!(reading.time, None);
        assert!(reading.status.oscillator_stopped);
    }

    #[test]
    fn serving_a_set_writes_the_instant_and_acknowledges() {
        let time = Time64::from_secs(1_800_000_042);
        let mut rtc = MockRtc::healthy(None);
        let mut request = [0u8; REQUEST_LEN];
        encode_request(&mut request, RtcOp::Set, time).expect("encodes");
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&mut rtc, &request, &mut reply).expect("serves");
        assert_eq!(decode_ack(&reply[..n]), Ok(()));
        assert_eq!(rtc.written, Some(time));
    }

    #[test]
    fn a_chip_error_is_framed_in_band_so_the_caller_is_always_answered() {
        // A read whose register access failed.
        let mut rtc = MockRtc {
            time: Err(DriverError::DeviceFault),
            status: Ok(status()),
            set_result: Ok(()),
            written: None,
        };
        let mut request = [0u8; REQUEST_LEN];
        encode_request(&mut request, RtcOp::Read, Time64::UNIX_EPOCH).expect("encodes");
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&mut rtc, &request, &mut reply).expect("serves");
        assert_eq!(decode_reading(&reply[..n]), Err(Errno::DeviceFault));

        // A read whose *status* access failed is equally a refusal, never a
        // reading with an invented status.
        let mut rtc = MockRtc {
            time: Ok(Some(Time64::from_secs(1))),
            status: Err(DriverError::DeviceFault),
            set_result: Ok(()),
            written: None,
        };
        let n = serve_request(&mut rtc, &request, &mut reply).expect("serves");
        assert_eq!(decode_reading(&reply[..n]), Err(Errno::DeviceFault));

        // A read-only chip refuses a write, and nothing is recorded.
        let mut rtc = MockRtc {
            time: Ok(None),
            status: Ok(status()),
            set_result: Err(DriverError::Unsupported),
            written: None,
        };
        encode_request(&mut request, RtcOp::Set, Time64::from_secs(7)).expect("encodes");
        let n = serve_request(&mut rtc, &request, &mut reply).expect("serves");
        assert_eq!(decode_ack(&reply[..n]), Err(Errno::NotImplemented));
        assert_eq!(rtc.written, None);
    }

    #[test]
    fn a_malformed_request_is_answered_without_touching_the_chip() {
        let mut rtc = MockRtc::healthy(Some(Time64::from_secs(1)));
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&mut rtc, &[0u8; REQUEST_LEN - 1], &mut reply).expect("serves");
        assert_eq!(decode_ack(&reply[..n]), Err(Errno::LengthOutOfRange));
        assert_eq!(rtc.written, None);
    }
}
