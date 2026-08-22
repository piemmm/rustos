//! The key grammar.

use tairix_abi::appdata_ipc::APPDATA_KEY_MAX;

use crate::ConfError;

/// Maximum length, in bytes, of a whole key.
///
/// A fixed validation bound on untrusted input: a key is a short dotted
/// identifier, so this bounds the work a hostile line can demand while
/// leaving ample room for a deeply-namespaced setting.
///
/// The number itself is the app-data channel's own key field width: a key
/// crosses the wire as one field of a fixed-shape record, so the bound is
/// part of the `abi-v1` contract and lives with it. Importing it rather than
/// restating it is what makes it impossible for the format and the wire to
/// disagree about how long a key may be.
pub const MAX_KEY_LEN: usize = APPDATA_KEY_MAX;

/// Maximum number of dot-separated segments a key may have.
///
/// Structure comes from dotted keys rather than nesting syntax, so depth is
/// the one dimension a hostile document could grow without limit; bounding it
/// keeps a key's shape as cheap to reason about as its length.
pub const MAX_KEY_DEPTH: usize = 8;

/// Validate a configuration key.
///
/// The grammar is dot-separated segments of ASCII lowercase letters, digits,
/// `-` and `_`, each segment non-empty and starting with a letter or digit.
/// It is deliberately narrow: a key is compared byte-for-byte, so admitting
/// case variants or Unicode look-alikes would let two spellings of "the same"
/// setting disagree, and admitting whitespace or `=` would let a key swallow
/// its own separator.
///
/// # Errors
///
/// [`ConfError::KeyInvalid`] if the key is empty, longer than
/// [`MAX_KEY_LEN`], deeper than [`MAX_KEY_DEPTH`], has an empty segment, or
/// holds a character outside the grammar.
pub fn validate_key(key: &str) -> Result<(), ConfError> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(ConfError::KeyInvalid);
    }
    let mut depth = 0;
    for segment in key.split('.') {
        depth += 1;
        if depth > MAX_KEY_DEPTH {
            return Err(ConfError::KeyInvalid);
        }
        let mut bytes = segment.bytes();
        let Some(first) = bytes.next() else {
            return Err(ConfError::KeyInvalid);
        };
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(ConfError::KeyInvalid);
        }
        if !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
            return Err(ConfError::KeyInvalid);
        }
    }
    Ok(())
}

/// Validate a key **prefix** — the filter a bounded key listing selects on.
///
/// A prefix is matched byte-for-byte against a key, so it is legal exactly
/// when it could begin one: the empty string (matching every key), or a run
/// of the key alphabet no longer and no deeper than a whole key. It is
/// deliberately *not* [`validate_key`]: `recent.` is the natural way to ask
/// for a key family and is not itself a key, while a segment-complete rule
/// would refuse it.
///
/// Because the alphabet is the key alphabet, a prefix cannot spell a path
/// separator, a dot-dot, a control character, or an upper-case variant — so
/// no listing filter can name anything outside the key space.
///
/// # Errors
///
/// [`ConfError::KeyInvalid`] if the prefix is longer than [`MAX_KEY_LEN`],
/// deeper than [`MAX_KEY_DEPTH`], or holds a character outside the grammar.
pub fn validate_key_prefix(prefix: &str) -> Result<(), ConfError> {
    if prefix.len() > MAX_KEY_LEN {
        return Err(ConfError::KeyInvalid);
    }
    if prefix.split('.').count() > MAX_KEY_DEPTH {
        return Err(ConfError::KeyInvalid);
    }
    if !prefix.bytes().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' || b == b'.'
    }) {
        return Err(ConfError::KeyInvalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_key, validate_key_prefix, MAX_KEY_DEPTH, MAX_KEY_LEN};
    use crate::ConfError;
    use alloc::string::String;

    #[test]
    fn accepts_the_shapes_a_settings_document_uses() {
        for key in [
            "scheme",
            "font.size",
            "effects.blur",
            "recent.0",
            "a",
            "a-b_c.d0",
        ] {
            assert_eq!(validate_key(key), Ok(()), "`{key}` must be a legal key");
        }
    }

    #[test]
    fn refuses_every_shape_that_would_make_two_keys_ambiguous() {
        for key in [
            "",
            "Scheme",
            "font size",
            "font=size",
            ".font",
            "font.",
            "font..size",
            "-leading",
            "_leading",
            "font.Size",
            "café",
            "font.size#",
            "font.size\n",
        ] {
            assert_eq!(
                validate_key(key),
                Err(ConfError::KeyInvalid),
                "`{key}` must never be a legal key"
            );
        }
    }

    #[test]
    fn refuses_an_over_long_or_too_deep_key() {
        let long: String = core::iter::repeat_n('a', MAX_KEY_LEN + 1).collect();
        assert_eq!(validate_key(&long), Err(ConfError::KeyInvalid));
        let at_bound: String = core::iter::repeat_n('a', MAX_KEY_LEN).collect();
        assert_eq!(validate_key(&at_bound), Ok(()));

        let deep = ["a"; MAX_KEY_DEPTH + 1].join(".");
        assert_eq!(validate_key(&deep), Err(ConfError::KeyInvalid));
        let at_depth = ["a"; MAX_KEY_DEPTH].join(".");
        assert_eq!(validate_key(&at_depth), Ok(()));
    }

    #[test]
    fn a_prefix_may_stop_anywhere_a_key_could_continue() {
        for prefix in ["", "recent.", "font", "effects.bl", "a-b_c.d0", "0"] {
            assert_eq!(
                validate_key_prefix(prefix),
                Ok(()),
                "`{prefix}` must be a legal prefix"
            );
        }
        // Every legal key is a legal prefix of itself.
        for key in ["scheme", "font.size", "recent.0"] {
            assert_eq!(validate_key(key), Ok(()));
            assert_eq!(validate_key_prefix(key), Ok(()));
        }
    }

    #[test]
    fn a_prefix_cannot_leave_the_key_alphabet() {
        for prefix in [
            "Recent.",
            "font size",
            "font=",
            "../",
            "/etc",
            "font\n",
            "café",
            "#",
        ] {
            assert_eq!(
                validate_key_prefix(prefix),
                Err(ConfError::KeyInvalid),
                "`{prefix}` must never be a legal prefix"
            );
        }
        let long: String = core::iter::repeat_n('a', MAX_KEY_LEN + 1).collect();
        assert_eq!(validate_key_prefix(&long), Err(ConfError::KeyInvalid));
        let deep = ["a"; MAX_KEY_DEPTH + 1].join(".");
        assert_eq!(validate_key_prefix(&deep), Err(ConfError::KeyInvalid));
    }
}
