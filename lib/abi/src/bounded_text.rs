//! A generic bounded, validated UTF-8 text field for fixed-width IPC wire
//! frames.
//!
//! [`BoundedText`] is the one definition every fixed-width request that
//! needs a short, validated display-text field builds on — the
//! notification channel's title and body
//! ([`crate::notify_ipc::NotifyTitle`], [`crate::notify_ipc::NotifyBody`])
//! and the Switchboard tray-summary channel's top-task name
//! ([`crate::switchboard_ipc::TrayTaskName`]). Sharing the one validator
//! keeps every consumer's construction and decode rules identical instead of
//! copy-pasting the same bounds/control-character check per channel.
//!
//! The text crosses a trust boundary into desktop chrome (a notification
//! title, a hover readout) and carries no authority — like a window title,
//! it is a name, not a credential. It is validated at construction *and*
//! again at decode, so a value that reached an IPC request is always
//! well-formed; a malformed field is refused, never sanitised.

use crate::Errno;

/// The character-acceptability rule shared by every bounded text field in
/// the ABI: a character is acceptable unless it is a control character —
/// with `\n` privileged when the caller's own grammar is legitimately
/// multi-line.
///
/// [`BoundedText`] calls this with `allow_newline = false`, so it forbids
/// every control character exactly as before. The one exception in the
/// crate is [`crate::pinboard_ipc::PinboardDocument`], a rendered
/// multi-line settings document that cannot be a `BoundedText` (its `\n`
/// separators would otherwise be refused) but must reject every other
/// control character identically. Sharing this one function keeps the two
/// validators' character rule from drifting into two independent copies of
/// "is this character acceptable".
#[must_use]
pub(crate) fn is_forbidden_character(c: char, allow_newline: bool) -> bool {
    c.is_control() && !(allow_newline && c == '\n')
}

/// A validated text field: at least `MIN` and at most `MAX` bytes of
/// well-formed UTF-8 with no control characters.
///
/// One generic definition serves every bounded display-text field in the
/// ABI, which validate identically and differ only in their bounds — the
/// size is the type parameter, so there is no second copy of the validator
/// to drift.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct BoundedText<const MIN: usize, const MAX: usize> {
    bytes: [u8; MAX],
    len: u8,
}

impl<const MIN: usize, const MAX: usize> BoundedText<MIN, MAX> {
    /// Compile-time soundness of the chosen bounds: the range is
    /// non-degenerate and the length always fits the one-byte wire prefix.
    /// Forced to evaluate by the `let () = Self::INVARIANTS;` in every
    /// constructor, so an unsound instantiation fails the build.
    const INVARIANTS: () = {
        assert!(MIN <= MAX, "BoundedText requires MIN <= MAX");
        assert!(
            MAX <= u8::MAX as usize,
            "BoundedText MAX must fit its one-byte length prefix"
        );
    };

    /// Build a text field from `text`, validating length and content.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — shorter than `MIN` or longer than
    ///   `MAX` bytes when UTF-8 encoded.
    /// * [`Errno::OutOfRange`] — contains a control character.
    pub fn new(text: &str) -> Result<Self, Errno> {
        let () = Self::INVARIANTS;
        let len = u8::try_from(text.len()).map_err(|_| Errno::LengthOutOfRange)?;
        if text.len() < MIN || text.len() > MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if text.chars().any(|c| is_forbidden_character(c, false)) {
            return Err(Errno::OutOfRange);
        }
        let mut bytes = [0u8; MAX];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Ok(Self { bytes, len })
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The buffer was validated as UTF-8 at construction/decode; an
        // impossible failure yields the empty string, never a panic.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    /// Whether the field is empty (only possible when `MIN == 0`).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Decode a text field from its fixed-width wire image: one length
    /// byte's worth of validated text, with the tail required zero.
    pub(crate) fn from_wire(len: u8, bytes: &[u8; MAX]) -> Result<Self, Errno> {
        let () = Self::INVARIANTS;
        let len_usize = usize::from(len);
        if len_usize < MIN || len_usize > MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if bytes[len_usize..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let text = core::str::from_utf8(&bytes[..len_usize]).map_err(|_| Errno::OutOfRange)?;
        if text.chars().any(|c| is_forbidden_character(c, false)) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { bytes: *bytes, len })
    }

    /// The wire length-prefix byte for this text.
    ///
    /// A crate-internal encoding detail of the fixed-width frame a consumer
    /// embeds this field in; callers read the text through
    /// [`Self::as_str`].
    pub(crate) const fn len_byte(&self) -> u8 {
        self.len
    }

    /// The fixed-width wire buffer backing this text.
    ///
    /// A crate-internal encoding detail; callers read the text through
    /// [`Self::as_str`].
    pub(crate) const fn raw_bytes(&self) -> &[u8; MAX] {
        &self.bytes
    }
}

impl<const MAX: usize> BoundedText<0, MAX> {
    /// The empty field, available only where the bounds admit one.
    ///
    /// A `const` so a fixed-width frame can initialise an array of records
    /// before decoding fills it, without a fallible constructor in a path
    /// that cannot fail.
    pub(crate) const EMPTY: Self = Self {
        bytes: [0; MAX],
        len: 0,
    };
}

impl<const MIN: usize, const MAX: usize> core::fmt::Debug for BoundedText<MIN, MAX> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("BoundedText").field(&self.as_str()).finish()
    }
}

/// A validated text field whose bound exceeds what one length byte can
/// carry: at least `MIN` and at most `MAX` bytes of well-formed UTF-8, with
/// `'\n'` permitted only where `NEWLINE` says the caller's own grammar is
/// legitimately multi-line.
///
/// [`BoundedText`]'s sibling, and the same validator in every respect but
/// the width of the wire length prefix — two bytes rather than one, which is
/// the only reason the two cannot be one type. The character rule is shared
/// through the crate-private `is_forbidden_character`, so there is no second
/// copy of "is this character acceptable" to drift.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct WideText<const MIN: usize, const MAX: usize, const NEWLINE: bool> {
    bytes: [u8; MAX],
    len: u16,
}

impl<const MIN: usize, const MAX: usize, const NEWLINE: bool> WideText<MIN, MAX, NEWLINE> {
    /// Compile-time soundness of the chosen bounds: the range is
    /// non-degenerate and the length always fits the two-byte wire prefix.
    const INVARIANTS: () = {
        assert!(MIN <= MAX, "WideText requires MIN <= MAX");
        assert!(
            MAX <= u16::MAX as usize,
            "WideText MAX must fit its two-byte length prefix"
        );
    };

    /// Build a field from `text`, validating length and content.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — shorter than `MIN` or longer than
    ///   `MAX` bytes when UTF-8 encoded.
    /// * [`Errno::OutOfRange`] — contains a forbidden control character.
    pub fn new(text: &str) -> Result<Self, Errno> {
        let () = Self::INVARIANTS;
        let len = u16::try_from(text.len()).map_err(|_| Errno::LengthOutOfRange)?;
        if text.len() < MIN || text.len() > MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if text.chars().any(|c| is_forbidden_character(c, NEWLINE)) {
            return Err(Errno::OutOfRange);
        }
        let mut bytes = [0u8; MAX];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Ok(Self { bytes, len })
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The buffer was validated as UTF-8 at construction/decode; an
        // impossible failure yields the empty string, never a panic.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    /// Whether the field is empty (only possible when `MIN == 0`).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Decode a field from its fixed-width wire image: `len` bytes of
    /// validated text, with the tail required zero.
    pub(crate) fn from_wire(len: u16, bytes: &[u8; MAX]) -> Result<Self, Errno> {
        let () = Self::INVARIANTS;
        let len_usize = usize::from(len);
        if len_usize < MIN || len_usize > MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if bytes[len_usize..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let text = core::str::from_utf8(&bytes[..len_usize]).map_err(|_| Errno::OutOfRange)?;
        if text.chars().any(|c| is_forbidden_character(c, NEWLINE)) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { bytes: *bytes, len })
    }

    /// The wire length-prefix value for this text.
    pub(crate) const fn len_u16(&self) -> u16 {
        self.len
    }

    /// The fixed-width wire buffer backing this text.
    pub(crate) const fn raw_bytes(&self) -> &[u8; MAX] {
        &self.bytes
    }
}

impl<const MIN: usize, const MAX: usize, const NEWLINE: bool> core::fmt::Debug
    for WideText<MIN, MAX, NEWLINE>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("WideText").field(&self.as_str()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundedText, WideText};
    use crate::Errno;

    type Short = BoundedText<1, 8>;
    type Optional = BoundedText<0, 8>;
    type Wide = WideText<1, 300, false>;
    type WideLines = WideText<1, 300, true>;

    #[test]
    fn round_trips_within_bounds() {
        let text = Short::new("hi").expect("within bounds");
        assert_eq!(text.as_str(), "hi");
        assert!(!text.is_empty());
    }

    #[test]
    fn empty_is_valid_only_when_min_is_zero() {
        assert_eq!(Short::new(""), Err(Errno::LengthOutOfRange));
        assert!(Optional::new("").expect("MIN 0 permits empty").is_empty());
    }

    #[test]
    fn rejects_over_long_and_control_characters() {
        assert_eq!(Short::new(&"x".repeat(9)), Err(Errno::LengthOutOfRange));
        assert_eq!(Short::new("a\nb"), Err(Errno::OutOfRange));
    }

    #[test]
    fn wire_round_trip_and_dirty_tail() {
        let text = Short::new("hi").expect("within bounds");
        let decoded =
            Short::from_wire(text.len_byte(), text.raw_bytes()).expect("well-formed wire image");
        assert_eq!(decoded, text);

        let mut dirty = *text.raw_bytes();
        dirty[text.len_byte() as usize] = 0xAA;
        assert_eq!(
            Short::from_wire(text.len_byte(), &dirty),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn a_wide_field_validates_exactly_as_a_narrow_one_past_the_byte_bound() {
        // The only difference between the two is the prefix width, so the
        // wide one has to refuse and accept the same things a byte-prefixed
        // field does — including past the length a byte could state.
        let long = "x".repeat(300);
        let text = Wide::new(&long).expect("the widest field");
        assert_eq!(text.as_str(), long);
        assert_eq!(text.len_u16(), 300);
        assert_eq!(Wide::new(""), Err(Errno::LengthOutOfRange));
        assert_eq!(Wide::new(&"x".repeat(301)), Err(Errno::LengthOutOfRange));
        assert_eq!(
            Wide::new(
                "a
b"
            ),
            Err(Errno::OutOfRange)
        );
        assert_eq!(Wide::new("a	b"), Err(Errno::OutOfRange));

        let decoded =
            Wide::from_wire(text.len_u16(), text.raw_bytes()).expect("well-formed wire image");
        assert_eq!(decoded, text);
        let mut dirty = *Wide::new("hi").expect("short").raw_bytes();
        dirty[2] = 0xAA;
        assert_eq!(Wide::from_wire(2, &dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn a_multi_line_wide_field_privileges_only_the_newline() {
        let text = WideLines::new(
            "a
b",
        )
        .expect("newlines are the grammar's own");
        assert_eq!(
            text.as_str(),
            "a
b"
        );
        assert_eq!(WideLines::new("a\rb"), Err(Errno::OutOfRange));
        assert_eq!(WideLines::new("a	b"), Err(Errno::OutOfRange));
    }
}
