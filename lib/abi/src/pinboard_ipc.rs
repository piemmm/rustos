//! The pinboard-apply IPC protocol (`plans/PINBOARD.md` §6): the reserved
//! rendezvous the desktop session binds to accept a new pinboard settings
//! document from the wallpaper chooser (and, later, any other tool the user
//! runs to change their desktop backdrop).
//!
//! The chooser and the backdrop's own context menu both **ask**; the
//! session **decides, applies, and persists** (`plans/PINBOARD.md` §6). A
//! caller renders its intended settings into the `key value` document
//! `lib/wallpaper` owns and posts it here; the session is the one engine
//! that parses that grammar, applies the result, and rewrites the user's
//! own settings file. This module never parses the document's grammar
//! itself — it validates only that the bytes are a well-formed, bounded,
//! multi-line transportable UTF-8 text, exactly as `lib/wallpaper`'s own
//! parser re-validates on arrival.
//!
//! # Security posture
//!
//! The document is configuration data and carries **no authority**: it
//! names a wallpaper path and a handful of closed-set option values, never
//! a credential. The session serving [`PINBOARD_ENDPOINT`] accepts a
//! request only from a caller whose kernel-attested [`crate::origin`] uid
//! matches its own — anything else is refused and logged — and it then
//! reads any path the document names **under its own identity**. A caller
//! can therefore never use this channel to reach a file it could not read
//! itself: the worst a hostile or buggy caller can do is ask the session to
//! adopt a document the session's own read access already bounds.
//!
//! There is one request today, [`PinboardRequest::Apply`]; the reply is the
//! shared status frame ([`crate::reply::encode_status_reply`] /
//! [`crate::reply::decode_status_reply`]). Every decode fails closed: an
//! unknown magic, version, or operation, a document length that does not
//! match its declared length, an over-long or empty document, non-UTF-8, a
//! forbidden control character, or a dirty reserved tail all refuse rather
//! than guessing.

use crate::bounded_text::is_forbidden_character;
use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::Errno;

/// Reserved well-known call-endpoint id of the desktop session's pinboard
/// service (`"PB"` ASCII hex-spelled prefix, mirroring
/// [`crate::notify_ipc::NOTIFY_ENDPOINT`]'s convention). Like the window,
/// notification, and Switchboard tray-summary rendezvous it is
/// **seat-scoped** ([`crate::ipc::is_reserved_endpoint`],
/// [`crate::ipc::is_seat_scoped_endpoint`]): the kernel authorises its bind
/// either by `CAP_IPC_BIND_PRIVILEGED` or by the caller's kernel-attested
/// **live seat lease** — the desktop session that owns the seat serves the
/// pinboard shown on it, and nothing else may. A squatter claiming the
/// rendezvous first could adopt a fabricated document into another user's
/// session, so an unentitled bind fails closed.
pub const PINBOARD_ENDPOINT: u64 = 0x5042_1001;

/// Magic number identifying a pinboard-channel request (`"PIN1"`
/// little-endian).
pub const PINBOARD_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"PIN1");

/// The `pinboard-v1` protocol version.
pub const PINBOARD_VERSION_V1: u16 = 1;

/// Maximum request, in bytes, the [`PINBOARD_ENDPOINT`] accepts: exactly
/// one fixed-width [`PinboardRequest`].
pub const PINBOARD_MAX_REQUEST: usize = PinboardRequest::WIRE_LEN;

/// Maximum encoded length, in bytes, of a rendered pinboard settings
/// document.
///
/// A validation bound, not a capacity ([`crate::rlimit`] governs
/// capacities): the document holds five short `key = value` lines —
/// `wallpaper`, `fit`, `backdrop`, `icons`, and `sort`
/// (`plans/PINBOARD.md` §2). Four of those lines are a key name plus one
/// closed-set word or a bare `rrggbb` colour, a handful of bytes each; the
/// fifth, `wallpaper`, carries a path, and in practice that path never
/// approaches the filesystem's own path bound — a shipped master under
/// `/System/Graphics/Wallpapers/` or a user's own file a few path segments
/// deep is well under a hundred bytes. 512 bytes leaves generous headroom
/// for every real document while keeping the wire frame small; a document
/// that genuinely needs more is refused rather than silently truncated.
pub const PINBOARD_DOCUMENT_MAX: usize = 512;

/// A rendered pinboard settings document: at least one and at most
/// [`PINBOARD_DOCUMENT_MAX`] bytes of well-formed UTF-8.
///
/// Unlike [`crate::bounded_text::BoundedText`] — the shared bounded
/// display-text validator every other short ABI text field builds on — a
/// settings document is legitimately **multi-line**: the `key = value`
/// grammar `lib/appconf` owns, over which `lib/wallpaper` defines the
/// pinboard registry (`plans/PINBOARD.md` §2), puts one setting per line. `BoundedText` forbids every control character including
/// `'\n'`, so it cannot represent this field; `PinboardDocument` is a
/// sibling validator with the identical rule *except* that `'\n'` is
/// privileged. No other control character is permitted — no `'\r'`, no
/// `'\t'`, no NUL, nothing else outside printable UTF-8 — so the wire form
/// stays a flat, unambiguous byte stream the receiving parser can split on
/// `'\n'` alone. The two validators share the one character rule through
/// `crate::bounded_text::is_forbidden_character` (crate-private, so not
/// linkable here) rather than each carrying its own copy of "is this
/// character acceptable"; only the length-prefix width (one byte versus
/// two, since [`PINBOARD_DOCUMENT_MAX`] exceeds `u8::MAX`) and the newline
/// exception keep the two from being the same type.
///
/// The document is validated at construction *and* again at decode, so a
/// value that reached a [`PinboardRequest`] is always well-formed. It is
/// never sanitised: a malformed document is refused, not silently repaired.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct PinboardDocument {
    bytes: [u8; PINBOARD_DOCUMENT_MAX],
    len: u16,
}

impl PinboardDocument {
    /// Build a document from `text`, validating length and content.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — empty, or longer than
    ///   [`PINBOARD_DOCUMENT_MAX`] bytes when UTF-8 encoded.
    /// * [`Errno::OutOfRange`] — contains a control character other than
    ///   `'\n'`.
    pub fn new(text: &str) -> Result<Self, Errno> {
        if text.is_empty() || text.len() > PINBOARD_DOCUMENT_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if text.chars().any(|c| is_forbidden_character(c, true)) {
            return Err(Errno::OutOfRange);
        }
        let mut bytes = [0u8; PINBOARD_DOCUMENT_MAX];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Ok(Self {
            bytes,
            // `text.len() <= PINBOARD_DOCUMENT_MAX` (512), checked above.
            #[allow(clippy::cast_possible_truncation)]
            len: text.len() as u16,
        })
    }

    /// The document text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The buffer was validated as UTF-8 at construction/decode; an
        // impossible failure yields the empty string, never a panic.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    /// Decode a document from its fixed-width wire image: `len` bytes of
    /// validated text, with the tail required zero.
    fn from_wire(len: u16, bytes: &[u8; PINBOARD_DOCUMENT_MAX]) -> Result<Self, Errno> {
        let len_usize = usize::from(len);
        if len_usize == 0 || len_usize > PINBOARD_DOCUMENT_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if bytes[len_usize..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let text = core::str::from_utf8(&bytes[..len_usize]).map_err(|_| Errno::OutOfRange)?;
        if text.chars().any(|c| is_forbidden_character(c, true)) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { bytes: *bytes, len })
    }

    /// The wire length-prefix value for this document.
    ///
    /// A crate-internal encoding detail of the fixed-width frame this type
    /// is embedded in; callers read the text through [`Self::as_str`].
    const fn len_u16(&self) -> u16 {
        self.len
    }

    /// The fixed-width wire buffer backing this document.
    ///
    /// A crate-internal encoding detail; callers read the text through
    /// [`Self::as_str`].
    const fn raw_bytes(&self) -> &[u8; PINBOARD_DOCUMENT_MAX] {
        &self.bytes
    }
}

impl core::fmt::Debug for PinboardDocument {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("PinboardDocument")
            .field(&self.as_str())
            .finish()
    }
}

/// One pinboard-channel operation (`plans/PINBOARD.md` §6).
///
/// The only operation today: a caller asks the session to adopt a new
/// settings document. The session alone decides whether to honour the
/// request (see the module's security posture above), applies it, and
/// persists it — the caller never writes the settings file itself.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PinboardRequest {
    /// Ask the session to adopt `document` as the caller's pinboard
    /// settings.
    Apply {
        /// The rendered settings document (`plans/PINBOARD.md` §2);
        /// parsed and validated by the one engine `lib/wallpaper` owns,
        /// never by this module.
        document: PinboardDocument,
    },
}

/// Wire operation discriminant of [`PinboardRequest::Apply`].
const OP_APPLY: u16 = 1;

/// Byte offset of the document length prefix.
const DOCUMENT_LEN_OFFSET: usize = 8;
/// Byte offset of a reserved pair following the length prefix; must be
/// zero.
const RESERVED_OFFSET: usize = 10;
/// Byte offset of the document text.
const DOCUMENT_OFFSET: usize = 12;

impl PinboardRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), a
    /// document length prefix (2), a reserved pair (2), and the full-width
    /// document buffer.
    pub const WIRE_LEN: usize = DOCUMENT_OFFSET + PINBOARD_DOCUMENT_MAX;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, PINBOARD_REQUEST_MAGIC);
        put_u16(&mut out, 4, PINBOARD_VERSION_V1);
        match *self {
            Self::Apply { document } => {
                put_u16(&mut out, 6, OP_APPLY);
                put_u16(&mut out, DOCUMENT_LEN_OFFSET, document.len_u16());
                put_u16(&mut out, RESERVED_OFFSET, 0);
                out[DOCUMENT_OFFSET..DOCUMENT_OFFSET + PINBOARD_DOCUMENT_MAX]
                    .copy_from_slice(document.raw_bytes());
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole request.
    /// * [`Errno::BadMagic`] — wrong magic, a non-zero reserved pair, or a
    ///   dirty document tail.
    /// * [`Errno::AbiVersionUnsupported`] — not `pinboard-v1`.
    /// * [`Errno::OutOfRange`] — an operation outside the closed set, or a
    ///   document that is not UTF-8 or holds a control character other
    ///   than `'\n'`.
    /// * [`Errno::LengthOutOfRange`] — a document length outside
    ///   `1..=PINBOARD_DOCUMENT_MAX`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != PINBOARD_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != PINBOARD_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = read_u16(bytes, 6);
        match op {
            OP_APPLY => {
                if read_u16(bytes, RESERVED_OFFSET) != 0 {
                    return Err(Errno::BadMagic);
                }
                let mut document_bytes = [0u8; PINBOARD_DOCUMENT_MAX];
                document_bytes.copy_from_slice(
                    &bytes[DOCUMENT_OFFSET..DOCUMENT_OFFSET + PINBOARD_DOCUMENT_MAX],
                );
                let document = PinboardDocument::from_wire(
                    read_u16(bytes, DOCUMENT_LEN_OFFSET),
                    &document_bytes,
                )?;
                Ok(Self::Apply { document })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PinboardDocument, PinboardRequest, PINBOARD_DOCUMENT_MAX};
    use crate::Errno;

    fn sample_document() -> &'static str {
        "wallpaper /System/Graphics/Wallpapers/TAIRiX/tairix-dark.jpg\n\
         fit fill\n\
         backdrop theme\n\
         icons leading\n\
         sort name\n"
    }

    fn apply() -> PinboardRequest {
        PinboardRequest::Apply {
            document: PinboardDocument::new(sample_document()).expect("a valid document"),
        }
    }

    #[test]
    fn apply_round_trips_a_representative_document() {
        let request = apply();
        let decoded =
            PinboardRequest::from_bytes(&request.to_le_bytes()).expect("a well-formed frame");
        assert_eq!(decoded, request);
        match decoded {
            PinboardRequest::Apply { document } => {
                assert_eq!(document.as_str(), sample_document());
            }
        }
    }

    #[test]
    fn a_document_with_embedded_newlines_survives_intact() {
        let text = "wallpaper none\nfit centre\nbackdrop #112233\nicons trailing\nsort date\n";
        let document = PinboardDocument::new(text).expect("newlines are permitted");
        assert_eq!(document.as_str(), text);
        let request = PinboardRequest::Apply { document };
        let decoded =
            PinboardRequest::from_bytes(&request.to_le_bytes()).expect("a well-formed frame");
        assert_eq!(decoded, request);
    }

    #[test]
    fn full_width_document_round_trips() {
        // A single line filled with 'a' up to the maximum, no newline.
        let text = "a".repeat(PINBOARD_DOCUMENT_MAX);
        let document = PinboardDocument::new(&text).expect("max-length document");
        let request = PinboardRequest::Apply { document };
        let decoded =
            PinboardRequest::from_bytes(&request.to_le_bytes()).expect("a well-formed frame");
        assert_eq!(decoded, request);
    }

    #[test]
    fn document_bounds_are_enforced() {
        assert_eq!(PinboardDocument::new(""), Err(Errno::LengthOutOfRange));
        assert_eq!(
            PinboardDocument::new(&"x".repeat(PINBOARD_DOCUMENT_MAX + 1)),
            Err(Errno::LengthOutOfRange)
        );
        // A newline is permitted, but every other control character is
        // refused, never sanitised.
        assert!(PinboardDocument::new("wallpaper none\nfit fill\n").is_ok());
        assert_eq!(PinboardDocument::new("fit\tfill"), Err(Errno::OutOfRange));
        assert_eq!(
            PinboardDocument::new("fit fill\r\n"),
            Err(Errno::OutOfRange)
        );
        assert_eq!(PinboardDocument::new("bell\u{7}"), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_short_buffer() {
        let frame = apply().to_le_bytes();
        assert_eq!(
            PinboardRequest::from_bytes(&frame[..frame.len() - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn rejects_bad_magic_version_and_op() {
        let mut frame = apply().to_le_bytes();
        frame[0] ^= 0xFF;
        assert_eq!(PinboardRequest::from_bytes(&frame), Err(Errno::BadMagic));

        let mut frame = apply().to_le_bytes();
        frame[4] = 0xFF;
        assert_eq!(
            PinboardRequest::from_bytes(&frame),
            Err(Errno::AbiVersionUnsupported)
        );

        let mut frame = apply().to_le_bytes();
        // Operation 9 is outside the closed set.
        frame[6] = 9;
        frame[7] = 0;
        assert_eq!(PinboardRequest::from_bytes(&frame), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_dirty_reserved_pair() {
        let mut frame = apply().to_le_bytes();
        frame[super::RESERVED_OFFSET] = 1;
        assert_eq!(PinboardRequest::from_bytes(&frame), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_dirty_document_tail() {
        // A byte past the declared document length must be zero.
        let mut frame = apply().to_le_bytes();
        let doc_len = usize::from(super::read_u16(&frame, super::DOCUMENT_LEN_OFFSET));
        frame[super::DOCUMENT_OFFSET + doc_len] = 0xAA;
        assert_eq!(PinboardRequest::from_bytes(&frame), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_over_long_document_length_prefix() {
        let mut frame = apply().to_le_bytes();
        // A length prefix beyond the buffer width can never be valid.
        super::put_u16(
            &mut frame,
            super::DOCUMENT_LEN_OFFSET,
            u16::try_from(PINBOARD_DOCUMENT_MAX + 1).expect("512 + 1 fits a u16"),
        );
        assert_eq!(
            PinboardRequest::from_bytes(&frame),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn rejects_empty_document_length_prefix() {
        let mut frame = apply().to_le_bytes();
        // Zero the declared document length and its bytes; a document is
        // required.
        super::put_u16(&mut frame, super::DOCUMENT_LEN_OFFSET, 0);
        for byte in
            &mut frame[super::DOCUMENT_OFFSET..super::DOCUMENT_OFFSET + PINBOARD_DOCUMENT_MAX]
        {
            *byte = 0;
        }
        assert_eq!(
            PinboardRequest::from_bytes(&frame),
            Err(Errno::LengthOutOfRange)
        );
    }
}
