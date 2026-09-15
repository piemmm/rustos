//! System notices: the one state-edge broadcast a process converges on
//! (`plans/NOTICE.md`).
//!
//! A notice is a **state edge**, not an occurrence. Each [`NoticeTopic`] names
//! one machine-wide value; a subscriber learns only that the value moved and
//! then reads the current one, so there is no queue, no history, nothing to
//! overflow, and nothing to drop. That is what makes it the right shape for
//! facts a process must *agree* with rather than witness: the desktop's
//! appearance and density, the mount table's composition, the memory-pressure
//! band.
//!
//! The mechanism is three parts:
//!
//! * [`WaitSourceKind::SystemNotice`](crate::WaitSourceKind::SystemNotice) —
//!   the wait-set member whose `id` is the topic, ready when the topic's
//!   generation differs from the one the member last observed.
//! * [`SyscallNumber::NOTICE_READ`](crate::SyscallNumber::NOTICE_READ) — the
//!   unprivileged, non-blocking read of a topic's current payload, so a woken
//!   subscriber has the value without an IPC round trip.
//! * [`SyscallNumber::NOTICE_PUBLISH`](crate::SyscallNumber::NOTICE_PUBLISH) —
//!   the publish, authorised per topic and fail-closed.
//!
//! # Payload shapes are exact, not merely bounded
//!
//! Every topic carries a payload of one fixed length ([`NoticeTopic::payload_len`]),
//! so a publish of any other length is refused rather than stored and handed
//! to a subscriber that would then have to defend against it. [`Notice`] is
//! the one encode/decode for all of them: the publisher writes through it and
//! the subscriber reads through it, so neither can spell a payload the other
//! would not accept.

use crate::desktop::DesktopInfo;
use crate::Errno;

/// Largest payload any topic carries — the buffer a subscriber sizes and the
/// kernel retains per topic.
///
/// A containment bound, not a capacity: it is what stops a topic's payload
/// growing into a channel, so it stays fixed. A topic needing more than this
/// is carrying a document, not a state edge, and belongs on an IPC endpoint.
pub const NOTICE_PAYLOAD_MAX: usize = 16;

/// One machine-wide value a process may converge on.
///
/// The set is closed and deliberately small: each topic widens what every
/// subscriber, reviewer, and audit must reason about, and a topic exists only
/// where a *state* has to be agreed rather than an event delivered.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum NoticeTopic {
    /// The desktop the foreground session composites: its screen extent, UI
    /// density, and appearance, as a [`DesktopInfo`]. Published by the holder
    /// of a seat's live display lease — the one principal that owns what is on
    /// screen — and read by every windowed application.
    Desktop = 0,
    /// The mount table's composition changed: a volume was attached,
    /// re-backed, or removed. Payload-less — the generation *is* the news, and
    /// what is mounted is then read through the System Information API under
    /// the caller's own authority. Published by the kernel alone.
    Mounts = 1,
    /// The machine's memory-pressure band, as its depth. Published by the
    /// kernel alone, from the gauge whatever was spending memory refreshed.
    ///
    /// The band is a five-level, hysteresis-damped, machine-wide indicator and
    /// carries no per-process, per-user, or byte-level figure, so reading it
    /// needs no capability — exactly as reading the load average does.
    MemoryPressure = 2,
}

impl NoticeTopic {
    /// The wire value for this topic.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Recover a topic from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` is not a known topic (fail closed on a
    /// malformed argument).
    pub const fn from_u32(value: u32) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Desktop),
            1 => Ok(Self::Mounts),
            2 => Ok(Self::MemoryPressure),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Recover a topic from the `id` a wait-set member or syscall argument
    /// carries.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a value outside the topic set, including one
    /// whose high bits are set — a wide argument is refused, never truncated
    /// into a topic it is not.
    pub const fn from_u64(value: u64) -> Result<Self, Errno> {
        if value > u32::MAX as u64 {
            return Err(Errno::OutOfRange);
        }
        #[allow(clippy::cast_possible_truncation)] // Bounded above.
        Self::from_u32(value as u32)
    }

    /// The exact payload length this topic carries; never above
    /// [`NOTICE_PAYLOAD_MAX`].
    #[must_use]
    pub const fn payload_len(self) -> usize {
        match self {
            Self::Desktop => DesktopInfo::WIRE_LEN,
            Self::Mounts => 0,
            Self::MemoryPressure => 1,
        }
    }

    /// Every topic, for a caller that must cover them all.
    pub const ALL: [Self; 3] = [Self::Desktop, Self::Mounts, Self::MemoryPressure];
}

/// A topic's payload, decoded.
///
/// One definition for both directions, so a publisher cannot write a shape a
/// subscriber would refuse.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Notice {
    /// [`NoticeTopic::Desktop`]: the desktop as the session composites it.
    Desktop(DesktopInfo),
    /// [`NoticeTopic::Mounts`]: the mount table moved; read it to learn how.
    Mounts,
    /// [`NoticeTopic::MemoryPressure`]: the band's depth.
    MemoryPressure {
        /// The band depth, as `tairix_reclaim::PressureBand::depth` spells it.
        /// Validated by that type, not here — this layer carries the scalar.
        band: u8,
    },
}

impl Notice {
    /// The topic this payload belongs to.
    #[must_use]
    pub const fn topic(&self) -> NoticeTopic {
        match self {
            Self::Desktop(_) => NoticeTopic::Desktop,
            Self::Mounts => NoticeTopic::Mounts,
            Self::MemoryPressure { .. } => NoticeTopic::MemoryPressure,
        }
    }

    /// Decode the payload held for `topic`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `bytes` is not exactly the topic's
    /// [`payload_len`](NoticeTopic::payload_len), and whatever the payload's
    /// own decode refuses for a malformed body. A payload is never partially
    /// adopted.
    pub fn decode(topic: NoticeTopic, bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() != topic.payload_len() {
            return Err(Errno::LengthOutOfRange);
        }
        match topic {
            NoticeTopic::Desktop => DesktopInfo::from_bytes(bytes).map(Self::Desktop),
            NoticeTopic::Mounts => Ok(Self::Mounts),
            NoticeTopic::MemoryPressure => Ok(Self::MemoryPressure { band: bytes[0] }),
        }
    }

    /// Write this payload to `out`, answering how many bytes it took.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `out` is shorter than the topic's
    /// [`payload_len`](NoticeTopic::payload_len).
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let len = self.topic().payload_len();
        let Some(slot) = out.get_mut(..len) else {
            return Err(Errno::LengthOutOfRange);
        };
        match self {
            Self::Desktop(info) => slot.copy_from_slice(&info.to_le_bytes()),
            Self::Mounts => {}
            Self::MemoryPressure { band } => slot[0] = *band,
        }
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::{Notice, NoticeTopic, NOTICE_PAYLOAD_MAX};
    use crate::desktop::{Appearance, DesktopInfo};
    use crate::Errno;

    fn desktop() -> DesktopInfo {
        match DesktopInfo::new(1920, 1080, 150, Appearance::Light) {
            Ok(info) => info,
            Err(_) => unreachable!("a valid desktop"),
        }
    }

    #[test]
    fn topic_wire_values_are_frozen_and_unknown_is_refused() {
        assert_eq!(NoticeTopic::Desktop.as_u32(), 0);
        assert_eq!(NoticeTopic::Mounts.as_u32(), 1);
        assert_eq!(NoticeTopic::MemoryPressure.as_u32(), 2);
        for topic in NoticeTopic::ALL {
            assert_eq!(NoticeTopic::from_u32(topic.as_u32()), Ok(topic));
            assert_eq!(NoticeTopic::from_u64(u64::from(topic.as_u32())), Ok(topic));
        }
        assert_eq!(NoticeTopic::from_u32(3), Err(Errno::OutOfRange));
        assert_eq!(NoticeTopic::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    /// A wide `id` must not be truncated into a topic it is not: the low
    /// bits of `1 << 32` are `Desktop`'s wire value.
    #[test]
    fn a_wide_id_is_refused_not_truncated() {
        assert_eq!(NoticeTopic::from_u64(1 << 32), Err(Errno::OutOfRange));
        assert_eq!(NoticeTopic::from_u64(u64::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn every_payload_fits_the_ceiling() {
        for topic in NoticeTopic::ALL {
            assert!(topic.payload_len() <= NOTICE_PAYLOAD_MAX, "{topic:?}");
        }
    }

    #[test]
    fn each_topic_round_trips_through_its_exact_length() {
        for notice in [
            Notice::Desktop(desktop()),
            Notice::Mounts,
            Notice::MemoryPressure { band: 3 },
        ] {
            let mut buf = [0xAAu8; NOTICE_PAYLOAD_MAX];
            let topic = notice.topic();
            let len = notice.encode(&mut buf).expect("encodes");
            assert_eq!(len, topic.payload_len());
            assert_eq!(Notice::decode(topic, &buf[..len]), Ok(notice));
            // Nothing beyond the payload is touched.
            assert!(buf[len..].iter().all(|&b| b == 0xAA), "{topic:?}");
        }
    }

    #[test]
    fn a_payload_of_the_wrong_length_is_refused() {
        let good = Notice::Desktop(desktop());
        let mut buf = [0u8; NOTICE_PAYLOAD_MAX];
        let len = good.encode(&mut buf).expect("encodes");
        assert_eq!(
            Notice::decode(NoticeTopic::Desktop, &buf[..len - 1]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            Notice::decode(NoticeTopic::Desktop, &buf),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            Notice::decode(NoticeTopic::Mounts, &buf[..1]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            Notice::decode(NoticeTopic::MemoryPressure, &[]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn encoding_into_a_short_buffer_is_refused() {
        let mut buf = [0u8; DesktopInfo::WIRE_LEN - 1];
        assert_eq!(
            Notice::Desktop(desktop()).encode(&mut buf),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(Notice::Mounts.encode(&mut []), Ok(0));
    }

    /// A malformed desktop body is refused rather than adopted: the payload
    /// crosses a trust boundary like any other.
    #[test]
    fn a_malformed_desktop_payload_is_refused() {
        let zeroed = [0u8; DesktopInfo::WIRE_LEN];
        assert!(Notice::decode(NoticeTopic::Desktop, &zeroed).is_err());
    }
}
