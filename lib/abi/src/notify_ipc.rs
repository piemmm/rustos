//! The desktop-notification IPC protocol (`plans/NEW-TASKBAR.md` T8): the
//! reserved rendezvous the desktop session binds and the fixed-width,
//! fail-closed requests a **producer service** posts a transient
//! notification through.
//!
//! A producer (a system service — battery, network, an application's
//! background agent) raises a short notification the desktop shows in its
//! notification area, and clears it when it no longer applies. The session
//! keys every notification to the kernel-attested identity of the producer
//! that posted it ([`crate::origin`] / `call_peer_origin`), never to
//! anything claimed on the wire, so one producer can neither replace nor
//! clear another's notification. Within its own identity a producer names a
//! notification by a [`NotifyRequest::Raise::key`] slot, so it can update or
//! clear exactly the one it raised.
//!
//! The title and body are producer-supplied **display text** validated at
//! construction and again at decode (bounded UTF-8, no control characters,
//! never sanitised): they cross a trust boundary into the desktop's
//! notification chrome and carry no authority — like a window title, the
//! text is a name, not a credential. The one authority-bearing fact, *which*
//! producer a notification belongs to, is the session's kernel-attested
//! caller identity, never a field here.
//!
//! Requests are the fixed-width [`NotifyRequest`]; both operations answer
//! with the shared status frame ([`crate::reply::encode_status_reply`] /
//! [`crate::reply::decode_status_reply`]) — success, or a typed refusal.
//! Every decode fails closed: an unknown magic, version, operation, or
//! severity, an over-long or empty title, a title/body that is not UTF-8 or
//! holds a control character, or a dirty reserved tail refuses rather than
//! guessing.

use crate::bounded_text::BoundedText;
use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::Errno;

/// Reserved well-known call-endpoint id of the desktop session's
/// notification service (`"NO"` ASCII hex-spelled prefix, mirroring
/// [`crate::window_ipc::WINDOW_ENDPOINT`]'s convention). Like the window
/// and Switchboard tray-summary rendezvous it is **seat-scoped**
/// ([`crate::ipc::is_reserved_endpoint`],
/// [`crate::ipc::is_seat_scoped_endpoint`]): the kernel authorises its
/// bind either by `CAP_IPC_BIND_PRIVILEGED` or by
/// the caller's kernel-attested **live seat lease** — the desktop session
/// that owns the seat serves the notifications shown on it, and nothing
/// else may. A squatter claiming the rendezvous first could suppress a
/// service's notifications or feed the desktop fabricated ones, so an
/// unentitled bind fails closed.
pub const NOTIFY_ENDPOINT: u64 = 0x4E4F_1001;

/// Magic number identifying a notification-channel request (`"NOT1"`
/// little-endian).
pub const NOTIFY_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"NOT1");

/// The `notify-v1` protocol version.
pub const NOTIFY_VERSION_V1: u16 = 1;

/// Maximum request, in bytes, the [`NOTIFY_ENDPOINT`] accepts: exactly one
/// fixed-width [`NotifyRequest`].
pub const NOTIFY_MAX_REQUEST: usize = NotifyRequest::WIRE_LEN;

/// Maximum encoded length, in bytes, of a notification title.
///
/// A validation bound, not a capacity ([`crate::rlimit`] governs
/// capacities): a title is a one-line heading, so a longer value can never
/// name a real notification and nothing legitimate is refused by the bound.
pub const NOTIFY_TITLE_MAX: usize = 64;

/// Maximum encoded length, in bytes, of a notification body.
///
/// A validation bound, not a capacity: a notification body is a short
/// sentence or two, deliberately terse (the notification area is not a log
/// viewer). A producer with more to say posts to the system log
/// ([`crate::log_ingress`]), not the desktop.
pub const NOTIFY_BODY_MAX: usize = 192;

/// A notification title: one non-empty line, at most [`NOTIFY_TITLE_MAX`]
/// bytes.
///
/// Built on the shared [`BoundedText`] validator (`crate::bounded_text`), so
/// its construction and decode rules are identical to every other bounded
/// display-text field in the ABI; the text is validated at construction *and*
/// again at decode, so a value that reached a [`NotifyRequest`] is always
/// well-formed.
pub type NotifyTitle = BoundedText<1, NOTIFY_TITLE_MAX>;

/// A notification body: at most [`NOTIFY_BODY_MAX`] bytes, and permitted to
/// be empty (a title-only notification).
pub type NotifyBody = BoundedText<0, NOTIFY_BODY_MAX>;

/// How prominently the desktop should present a notification.
///
/// A closed set: the desktop maps each level to a shared-control state and
/// its display prominence (a more severe notification sorts ahead of a
/// calmer one). An unknown wire byte is refused rather than guessed, so a
/// producer can never smuggle an out-of-band level past the decoder.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NotifySeverity {
    /// Neutral information (a background job finished, a device appeared).
    Info,
    /// A positive outcome worth a calm acknowledgement.
    Success,
    /// A condition the user should notice but that is not yet failing.
    Warning,
    /// A failing or urgent condition demanding attention.
    Critical,
}

/// Wire discriminant of [`NotifySeverity::Info`].
const SEVERITY_INFO: u8 = 1;
/// Wire discriminant of [`NotifySeverity::Success`].
const SEVERITY_SUCCESS: u8 = 2;
/// Wire discriminant of [`NotifySeverity::Warning`].
const SEVERITY_WARNING: u8 = 3;
/// Wire discriminant of [`NotifySeverity::Critical`].
const SEVERITY_CRITICAL: u8 = 4;

impl NotifySeverity {
    /// The wire discriminant of this severity (a non-zero byte; zero is
    /// reserved so a zeroed frame never decodes as a valid severity).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Info => SEVERITY_INFO,
            Self::Success => SEVERITY_SUCCESS,
            Self::Warning => SEVERITY_WARNING,
            Self::Critical => SEVERITY_CRITICAL,
        }
    }

    /// Decode a severity from its wire discriminant.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any byte outside the closed set (fail
    /// closed on a corrupt or hostile frame).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            SEVERITY_INFO => Ok(Self::Info),
            SEVERITY_SUCCESS => Ok(Self::Success),
            SEVERITY_WARNING => Ok(Self::Warning),
            SEVERITY_CRITICAL => Ok(Self::Critical),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One notification-channel operation (`plans/NEW-TASKBAR.md` T8).
///
/// Every request acts on the caller's **own** notifications: the session
/// derives ownership from the kernel-attested identity of the in-flight
/// caller, never from a claimed id, so the [`Self::Raise::key`] here is a
/// per-producer name, not a credential.
// `Raise` carries the title and body inline: a fixed-frame wire request is
// `Copy` and allocation-free, so the enum's size is its largest variant's by
// design, and `Clear` (a bare key) is the small one. Boxing to equalise the
// variants would force an allocation into an ABI decode type and drop `Copy`
// — the wrong trade for a transient per-call request that is encoded and
// dropped, never stored in bulk, so the size difference is deliberate.
#[allow(clippy::large_enum_variant)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NotifyRequest {
    /// Raise the caller's notification identified by `key`, or replace it
    /// in place if one with that key is already showing. The session lists
    /// it in the notification area at the prominence `severity` selects.
    Raise {
        /// The producer-chosen slot naming this notification within the
        /// caller's own identity; a later `Raise` with the same key updates
        /// it and a [`Self::Clear`] with the same key removes it.
        key: u32,
        /// How prominently to present the notification.
        severity: NotifySeverity,
        /// The notification's one-line heading (non-empty).
        title: NotifyTitle,
        /// The notification's short body (may be empty for a title-only
        /// notification).
        body: NotifyBody,
    },
    /// Clear the caller's notification identified by `key`. Clearing a key
    /// the caller is not currently showing is a success, not an error
    /// (idempotent teardown): a producer may clear unconditionally without
    /// tracking whether the notification is still up.
    Clear {
        /// The slot of the notification to remove.
        key: u32,
    },
}

/// Wire operation discriminant of [`NotifyRequest::Raise`].
const OP_RAISE: u16 = 1;
/// Wire operation discriminant of [`NotifyRequest::Clear`].
const OP_CLEAR: u16 = 2;

/// Byte offset of the operation block's `key` field (shared by both ops).
const KEY_OFFSET: usize = 8;
/// Byte offset of a `Raise`'s severity discriminant.
const SEVERITY_OFFSET: usize = 12;
/// Byte offset of a `Raise`'s title length prefix.
const TITLE_LEN_OFFSET: usize = 13;
/// Byte offset of a `Raise`'s body length prefix.
const BODY_LEN_OFFSET: usize = 14;
/// Byte offset of a `Raise`'s title text.
const TITLE_OFFSET: usize = 15;
/// Byte offset of a `Raise`'s body text.
const BODY_OFFSET: usize = TITLE_OFFSET + NOTIFY_TITLE_MAX;

impl NotifyRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), and a
    /// fixed operation block whose unused tail must be zero. A `Raise` is
    /// the widest — a 4-byte key, a severity byte, two length bytes, and
    /// the full-width title and body buffers.
    pub const WIRE_LEN: usize = BODY_OFFSET + NOTIFY_BODY_MAX;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, NOTIFY_REQUEST_MAGIC);
        put_u16(&mut out, 4, NOTIFY_VERSION_V1);
        match *self {
            Self::Raise {
                key,
                severity,
                title,
                body,
            } => {
                put_u16(&mut out, 6, OP_RAISE);
                put_u32(&mut out, KEY_OFFSET, key);
                out[SEVERITY_OFFSET] = severity.as_u8();
                out[TITLE_LEN_OFFSET] = title.len_byte();
                out[BODY_LEN_OFFSET] = body.len_byte();
                out[TITLE_OFFSET..TITLE_OFFSET + NOTIFY_TITLE_MAX]
                    .copy_from_slice(title.raw_bytes());
                out[BODY_OFFSET..BODY_OFFSET + NOTIFY_BODY_MAX].copy_from_slice(body.raw_bytes());
            }
            Self::Clear { key } => {
                put_u16(&mut out, 6, OP_CLEAR);
                put_u32(&mut out, KEY_OFFSET, key);
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole request.
    /// * [`Errno::BadMagic`] — wrong magic, a dirty reserved tail, or a
    ///   dirty title/body tail.
    /// * [`Errno::AbiVersionUnsupported`] — not `notify-v1`.
    /// * [`Errno::OutOfRange`] — an operation or severity outside the
    ///   closed set, or a title/body that is not UTF-8 or holds a control
    ///   character.
    /// * [`Errno::LengthOutOfRange`] — a title length outside
    ///   `1..=NOTIFY_TITLE_MAX` or a body length above [`NOTIFY_BODY_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != NOTIFY_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != NOTIFY_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = read_u16(bytes, 6);
        let key = read_u32(bytes, KEY_OFFSET);
        match op {
            OP_RAISE => {
                let severity = NotifySeverity::from_u8(bytes[SEVERITY_OFFSET])?;
                let mut title_bytes = [0u8; NOTIFY_TITLE_MAX];
                title_bytes.copy_from_slice(&bytes[TITLE_OFFSET..TITLE_OFFSET + NOTIFY_TITLE_MAX]);
                let title = NotifyTitle::from_wire(bytes[TITLE_LEN_OFFSET], &title_bytes)?;
                let mut body_bytes = [0u8; NOTIFY_BODY_MAX];
                body_bytes.copy_from_slice(&bytes[BODY_OFFSET..BODY_OFFSET + NOTIFY_BODY_MAX]);
                let body = NotifyBody::from_wire(bytes[BODY_LEN_OFFSET], &body_bytes)?;
                Ok(Self::Raise {
                    key,
                    severity,
                    title,
                    body,
                })
            }
            OP_CLEAR => {
                reserved_zero(bytes, SEVERITY_OFFSET)?;
                Ok(Self::Clear { key })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Refuse a request whose reserved tail (from `from` to the end of the
/// fixed frame) carries any non-zero byte — wire corruption or a smuggled
/// field, never silently ignored.
fn reserved_zero(bytes: &[u8], from: usize) -> Result<(), Errno> {
    if bytes[from..NotifyRequest::WIRE_LEN].iter().any(|&b| b != 0) {
        return Err(Errno::BadMagic);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        NotifyBody, NotifyRequest, NotifySeverity, NotifyTitle, NOTIFY_BODY_MAX, NOTIFY_TITLE_MAX,
    };
    use crate::Errno;

    fn raise() -> NotifyRequest {
        NotifyRequest::Raise {
            key: 0x0102_0304,
            severity: NotifySeverity::Warning,
            title: NotifyTitle::new("Battery low").expect("a valid title"),
            body: NotifyBody::new("12% remaining — connect a charger.").expect("a valid body"),
        }
    }

    #[test]
    fn raise_round_trips_every_severity() {
        for severity in [
            NotifySeverity::Info,
            NotifySeverity::Success,
            NotifySeverity::Warning,
            NotifySeverity::Critical,
        ] {
            let request = NotifyRequest::Raise {
                key: 7,
                severity,
                title: NotifyTitle::new("Sync complete").expect("a valid title"),
                body: NotifyBody::new("").expect("an empty body is valid"),
            };
            let decoded =
                NotifyRequest::from_bytes(&request.to_le_bytes()).expect("a well-formed frame");
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn clear_round_trips() {
        let request = NotifyRequest::Clear { key: 42 };
        let decoded =
            NotifyRequest::from_bytes(&request.to_le_bytes()).expect("a well-formed frame");
        assert_eq!(decoded, request);
    }

    #[test]
    fn full_width_title_and_body_round_trip() {
        let title = NotifyTitle::new(&"t".repeat(NOTIFY_TITLE_MAX)).expect("max-length title");
        let body = NotifyBody::new(&"b".repeat(NOTIFY_BODY_MAX)).expect("max-length body");
        let request = NotifyRequest::Raise {
            key: 1,
            severity: NotifySeverity::Info,
            title,
            body,
        };
        let decoded =
            NotifyRequest::from_bytes(&request.to_le_bytes()).expect("a well-formed frame");
        assert_eq!(decoded, request);
    }

    #[test]
    fn severity_discriminants_round_trip_and_fail_closed() {
        for severity in [
            NotifySeverity::Info,
            NotifySeverity::Success,
            NotifySeverity::Warning,
            NotifySeverity::Critical,
        ] {
            assert_eq!(NotifySeverity::from_u8(severity.as_u8()), Ok(severity));
        }
        assert_eq!(NotifySeverity::from_u8(0), Err(Errno::OutOfRange));
        assert_eq!(NotifySeverity::from_u8(5), Err(Errno::OutOfRange));
    }

    #[test]
    fn text_bounds_are_enforced() {
        // A title must be present; a body may be empty.
        assert_eq!(NotifyTitle::new(""), Err(Errno::LengthOutOfRange));
        assert!(NotifyBody::new("").is_ok());
        // Over-long by one byte is refused on both.
        assert_eq!(
            NotifyTitle::new(&"x".repeat(NOTIFY_TITLE_MAX + 1)),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            NotifyBody::new(&"x".repeat(NOTIFY_BODY_MAX + 1)),
            Err(Errno::LengthOutOfRange)
        );
        // Control characters are refused, never sanitised.
        assert_eq!(NotifyTitle::new("line\nbreak"), Err(Errno::OutOfRange));
        assert_eq!(NotifyBody::new("bell\u{7}"), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_short_buffer() {
        let frame = raise().to_le_bytes();
        assert_eq!(
            NotifyRequest::from_bytes(&frame[..frame.len() - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn rejects_bad_magic_version_and_op() {
        let mut frame = raise().to_le_bytes();
        frame[0] ^= 0xFF;
        assert_eq!(NotifyRequest::from_bytes(&frame), Err(Errno::BadMagic));

        let mut frame = raise().to_le_bytes();
        frame[4] = 0xFF;
        assert_eq!(
            NotifyRequest::from_bytes(&frame),
            Err(Errno::AbiVersionUnsupported)
        );

        let mut frame = raise().to_le_bytes();
        // Operation 9 is outside the closed set.
        frame[6] = 9;
        frame[7] = 0;
        assert_eq!(NotifyRequest::from_bytes(&frame), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_unknown_severity() {
        let mut frame = raise().to_le_bytes();
        frame[super::SEVERITY_OFFSET] = 0;
        assert_eq!(NotifyRequest::from_bytes(&frame), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_dirty_title_and_body_tails() {
        // A byte past the declared title length must be zero.
        let mut frame = raise().to_le_bytes();
        let title_len = usize::from(frame[super::TITLE_LEN_OFFSET]);
        frame[super::TITLE_OFFSET + title_len] = 0xAA;
        assert_eq!(NotifyRequest::from_bytes(&frame), Err(Errno::BadMagic));

        // A byte past the declared body length must be zero.
        let mut frame = raise().to_le_bytes();
        let body_len = usize::from(frame[super::BODY_LEN_OFFSET]);
        frame[super::BODY_OFFSET + body_len] = 0xBB;
        assert_eq!(NotifyRequest::from_bytes(&frame), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_over_long_title_length_prefix() {
        let mut frame = raise().to_le_bytes();
        // A length prefix beyond the buffer width can never be valid.
        frame[super::TITLE_LEN_OFFSET] =
            u8::try_from(NOTIFY_TITLE_MAX + 1).expect("64 + 1 fits a u8");
        assert_eq!(
            NotifyRequest::from_bytes(&frame),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn rejects_empty_title_length_prefix() {
        let mut frame = raise().to_le_bytes();
        // Zero the declared title length and its bytes; a title is required.
        frame[super::TITLE_LEN_OFFSET] = 0;
        for byte in &mut frame[super::TITLE_OFFSET..super::TITLE_OFFSET + NOTIFY_TITLE_MAX] {
            *byte = 0;
        }
        assert_eq!(
            NotifyRequest::from_bytes(&frame),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn clear_rejects_dirty_reserved_tail() {
        let mut frame = NotifyRequest::Clear { key: 3 }.to_le_bytes();
        frame[super::SEVERITY_OFFSET] = 1;
        assert_eq!(NotifyRequest::from_bytes(&frame), Err(Errno::BadMagic));
    }
}
