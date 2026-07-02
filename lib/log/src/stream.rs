//! The closed set of log streams.
//!
//! Every log record belongs to exactly one stream, and a stream is the unit
//! of authority, retention, and on-disk separation: `boot`/`runtime`/`debug`
//! records have different retention and access rules from `security`/`audit`
//! records, and each stream owns its own append-only segment chain under
//! `/System/Logs/<stream>/`.
//!
//! The set is **closed**: a caller may *request* a stream, but the journal
//! assigns the effective one, and there is no "other" or free-form stream.
//! The discriminants are part of the on-disk record/segment format and must
//! not be renumbered.

use rustos_abi::Errno;

/// The stream a log record belongs to.
///
/// The `u8` discriminants are stable on-disk values (they appear in every
/// [`crate::segment::SegmentHeader`]); they must not be renumbered.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Stream {
    /// Firmware hand-off, kernel bring-up, the early driver/storage/root
    /// path, and boot-service startup. Kernel and trusted bootstrap only.
    Boot = 0,
    /// Ordinary process, service, driver, and application logs. The default
    /// for a non-privileged emitter.
    Runtime = 1,
    /// High-volume diagnostic logs with short retention.
    Debug = 2,
    /// Security-relevant allow/deny decisions and policy checks. Trusted
    /// kernel or security services only.
    Security = 3,
    /// Audit-relevant state changes and privileged decisions. Trusted audit
    /// emitters only.
    Audit = 4,
    /// Journal self-events: loss, seal, rotation, and verification records.
    /// The journal service only.
    Journal = 5,
}

impl Stream {
    /// Every stream, in discriminant order.
    ///
    /// `ALL[i].as_u8() as usize == i`, so this doubles as the canonical
    /// iteration order for the per-stream state a journal keeps (one slot per
    /// stream, indexed by [`Self::as_u8`]).
    pub const ALL: [Stream; 6] = [
        Stream::Boot,
        Stream::Runtime,
        Stream::Debug,
        Stream::Security,
        Stream::Audit,
        Stream::Journal,
    ];

    /// Numeric on-disk discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a stream from its on-disk discriminant, failing closed.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any value outside the closed set — never
    /// guessing at an unknown stream.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Boot),
            1 => Ok(Self::Runtime),
            2 => Ok(Self::Debug),
            3 => Ok(Self::Security),
            4 => Ok(Self::Audit),
            5 => Ok(Self::Journal),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// The stream's identifying label, fed to [`crate::stream_genesis`] so a
    /// segment lifted onto a different stream fails verification.
    ///
    /// These bytes are part of the genesis derivation and must not change.
    #[must_use]
    pub const fn genesis_label(self) -> &'static [u8] {
        match self {
            Self::Boot => b"boot",
            Self::Runtime => b"runtime",
            Self::Debug => b"debug",
            Self::Security => b"security",
            Self::Audit => b"audit",
            Self::Journal => b"journal",
        }
    }

    /// Whether a *closed* segment of this stream must be cryptographically
    /// sealed (audit and security, per the log integrity model): those
    /// streams' tamper-evidence rests on a MAC ordinary services cannot
    /// forge, so a closed segment without a valid seal is refused.
    #[must_use]
    pub const fn requires_seal(self) -> bool {
        matches!(self, Self::Security | Self::Audit)
    }

    /// Whether writing this stream is restricted to a trusted emitter.
    ///
    /// `boot` (firmware/kernel bring-up), `security`, `audit`, and `journal`
    /// carry system authority: an ordinary process must never be able to
    /// place a record on one of them. `runtime` and `debug` are open to
    /// non-privileged emitters (debug being rate-limited by the journal).
    /// The stream-authority resolver ([`crate::resolve_stream`]) uses this to
    /// downgrade an untrusted caller's privileged request to `runtime` and
    /// flag it as a spoof attempt.
    #[must_use]
    pub const fn requires_trusted_emitter(self) -> bool {
        matches!(
            self,
            Self::Boot | Self::Security | Self::Audit | Self::Journal
        )
    }

    /// Whether this stream may be rate-limited, sampled, or dropped under
    /// pressure to protect the machine from log-driven denial of service.
    ///
    /// Only the two non-privileged, high-volume streams — `runtime` and
    /// `debug` — may be dropped. `boot`, `security`, `audit`, and `journal`
    /// carry system authority and must never be silently dropped: an
    /// audit/security record that cannot be accepted fails closed to the
    /// caller instead. This is the single definition of which streams the
    /// rate limiter gates.
    #[must_use]
    pub const fn is_rate_limitable(self) -> bool {
        matches!(self, Self::Runtime | Self::Debug)
    }
}

#[cfg(test)]
mod tests {
    use super::Stream;
    use rustos_abi::Errno;

    const ALL: [Stream; 6] = Stream::ALL;

    #[test]
    fn discriminants_round_trip() {
        for s in ALL {
            assert_eq!(Stream::from_u8(s.as_u8()), Ok(s));
        }
    }

    #[test]
    fn from_u8_fails_closed_on_unknown() {
        assert_eq!(Stream::from_u8(6), Err(Errno::OutOfRange));
        assert_eq!(Stream::from_u8(255), Err(Errno::OutOfRange));
    }

    #[test]
    fn genesis_labels_are_distinct() {
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a.genesis_label(), b.genesis_label());
            }
        }
    }

    #[test]
    fn only_audit_and_security_require_a_seal() {
        assert!(Stream::Security.requires_seal());
        assert!(Stream::Audit.requires_seal());
        assert!(!Stream::Boot.requires_seal());
        assert!(!Stream::Runtime.requires_seal());
        assert!(!Stream::Debug.requires_seal());
        assert!(!Stream::Journal.requires_seal());
    }

    #[test]
    fn only_runtime_and_debug_are_caller_writable() {
        assert!(!Stream::Runtime.requires_trusted_emitter());
        assert!(!Stream::Debug.requires_trusted_emitter());
        assert!(Stream::Boot.requires_trusted_emitter());
        assert!(Stream::Security.requires_trusted_emitter());
        assert!(Stream::Audit.requires_trusted_emitter());
        assert!(Stream::Journal.requires_trusted_emitter());
    }

    #[test]
    fn only_runtime_and_debug_are_rate_limitable() {
        assert!(Stream::Runtime.is_rate_limitable());
        assert!(Stream::Debug.is_rate_limitable());
        assert!(!Stream::Boot.is_rate_limitable());
        assert!(!Stream::Security.is_rate_limitable());
        assert!(!Stream::Audit.is_rate_limitable());
        assert!(!Stream::Journal.is_rate_limitable());
    }
}
