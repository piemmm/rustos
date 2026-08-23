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

#[cfg(test)]
mod tests {
    use super::{validate_key, MAX_KEY_DEPTH, MAX_KEY_LEN};
    use crate::ConfError;
    use alloc::string::String;

    #[test]
    fn every_store_name_is_a_legal_key() {
        // A bundle identifier and a bulk-store file name are validated by
        // `lib/abi`'s one store-name grammar, and both are used as a key's
        // leading segments by a registry that keys on an identifier (the
        // program library's `<id>.<field>`). The two grammars must therefore
        // agree, or a name the system admits would be a key no document could
        // hold. This pins the direction that matters: everything the store
        // grammar accepts, this one accepts.
        for name in [
            "os.tairix.terminal",
            "a",
            "a-b_c.d0",
            "com.example.editor",
            "0leading-digit",
        ] {
            assert_eq!(
                tairix_abi::appinfo::validate_bundle_id(name),
                Ok(()),
                "`{name}` must be a legal store name"
            );
            assert_eq!(
                validate_key(name),
                Ok(()),
                "`{name}` is a store name, so it must be a legal key"
            );
        }
        // And the shapes both refuse, refused by both.
        for name in ["-leading", "_leading", ".leading", "Upper", "two..dots", ""] {
            assert!(
                tairix_abi::appinfo::validate_bundle_id(name).is_err(),
                "`{name}` must not be a store name"
            );
            assert_eq!(validate_key(name), Err(ConfError::KeyInvalid), "`{name}`");
        }
    }

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
}
