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
        if text.chars().any(char::is_control) {
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
        if text.chars().any(char::is_control) {
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

impl<const MIN: usize, const MAX: usize> core::fmt::Debug for BoundedText<MIN, MAX> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("BoundedText").field(&self.as_str()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::BoundedText;
    use crate::Errno;

    type Short = BoundedText<1, 8>;
    type Optional = BoundedText<0, 8>;

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
}
