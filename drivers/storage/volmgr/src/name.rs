//! The deterministic catalog-naming policy (`plans/DEVICES.md` D3c).
//!
//! A recognised volume is published under `/Storage/<name>`. The name is
//! derived from the volume's own facts, so re-inserting the same volume
//! re-derives the same name and a user's scripts keep working:
//!
//! 1. the volume's recorded **label**, sanitised through the alias
//!    character rules (`plans/ALIAS.md` §5.2): lowercased ASCII letters,
//!    digits, `-`, and `_`, everything else dropped, leading separators
//!    stripped — an empty result falls through;
//! 2. else the **filesystem-type fallback** `<fstype><n>` (`fat1`,
//!    `ext1`, `rustfs1`), where `n` is the volume's 1-based ordinal among
//!    its type's volumes on the probed device;
//! 3. a name the kernel reports already in use gets the volume-identity
//!    **fingerprint** appended (`plans/ALIAS.md` §3.8, rendered by
//!    `rustos_fsprobe::fingerprint`), lengthened on each further
//!    collision — distinct identities have distinct full fingerprints, so
//!    the sequence terminates deterministically, never by coin-flip.
//!
//! Every produced candidate satisfies the structural volume-name grammar
//! (`rustos_abi::volume::validate_volume_name`) by construction; the
//! kernel still re-validates it (never a trusted caller).

use rustos_abi::volume::{VolumeFsType, VOLUME_NAME_MAX};
use rustos_fsprobe::{fingerprint, FINGERPRINT_CHARS};

/// A catalog name candidate: a short ASCII string in the volume-name
/// grammar, held inline (the policy is allocation-free).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VolumeName {
    bytes: [u8; VOLUME_NAME_MAX],
    len: u8,
}

impl VolumeName {
    /// The name's bytes (ASCII, non-empty).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Append `byte`, silently ignored once full (callers bound their
    /// input; truncation at the grammar bound is the documented policy).
    fn push(&mut self, byte: u8) {
        if usize::from(self.len) < VOLUME_NAME_MAX {
            self.bytes[usize::from(self.len)] = byte;
            self.len += 1;
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Sanitise a volume label into a base name under the alias character
/// rules: ASCII letters lowercased, digits kept, `-`/`_` kept, everything
/// else dropped; leading separators stripped so the name starts with a
/// letter or digit; bounded by the name grammar. `None` when nothing
/// renderable remains (the fallback name is used instead).
#[must_use]
pub fn sanitise_label(label: &[u8]) -> Option<VolumeName> {
    let mut name = VolumeName {
        bytes: [0; VOLUME_NAME_MAX],
        len: 0,
    };
    for &byte in label {
        let mapped = match byte {
            b'A'..=b'Z' => Some(byte + (b'a' - b'A')),
            b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => Some(byte),
            _ => None,
        };
        let Some(mapped) = mapped else { continue };
        // A leading separator would violate the alias rules; drop it
        // until a letter or digit anchors the name.
        if name.is_empty() && (mapped == b'-' || mapped == b'_') {
            continue;
        }
        name.push(mapped);
    }
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// The `<fstype><n>` fallback name for a volume with no usable label:
/// `fat1`, `ext1`, `rustfs1`, … `ordinal` is 1-based among the device's
/// volumes of that type, so the derivation is stable per device layout.
#[must_use]
pub fn fallback_name(fstype: VolumeFsType, ordinal: u32) -> VolumeName {
    let prefix: &[u8] = match fstype {
        VolumeFsType::RustFs => b"rustfs",
        VolumeFsType::Ext4 => b"ext",
        VolumeFsType::Fat32 => b"fat",
    };
    let mut name = VolumeName {
        bytes: [0; VOLUME_NAME_MAX],
        len: 0,
    };
    for &byte in prefix {
        name.push(byte);
    }
    // Render the ordinal in decimal, most-significant digit first.
    let mut digits = [0u8; 10];
    let mut value = ordinal.max(1);
    let mut count = 0;
    while value > 0 {
        #[allow(clippy::cast_possible_truncation)] // A decimal digit.
        {
            digits[count] = b'0' + (value % 10) as u8;
        }
        value /= 10;
        count += 1;
    }
    for i in (0..count).rev() {
        name.push(digits[i]);
    }
    name
}

/// How many collision retries the candidate sequence offers beyond the
/// base name. The final candidate carries the full fingerprint, which is
/// unique per identity, so a longer sequence would add nothing.
pub const CANDIDATE_ATTEMPTS: usize = 4;

/// Fingerprint characters appended per retry step (4, 8, then the full
/// [`FINGERPRINT_CHARS`]).
const FINGERPRINT_STEP: usize = 4;

/// The `attempt`-th catalog-name candidate for a volume: the base name
/// (attempt 0), then the base with `-<fingerprint prefix>` appended,
/// lengthening per attempt up to the full fingerprint. The base is
/// truncated as needed so the suffix always fits the name grammar.
/// `None` once the sequence is exhausted (`attempt >=`
/// [`CANDIDATE_ATTEMPTS`]).
#[must_use]
pub fn candidate(base: &VolumeName, identity: &[u8; 16], attempt: usize) -> Option<VolumeName> {
    if attempt == 0 {
        return Some(*base);
    }
    if attempt >= CANDIDATE_ATTEMPTS {
        return None;
    }
    let fp = fingerprint(identity);
    let take = if attempt == CANDIDATE_ATTEMPTS - 1 {
        FINGERPRINT_CHARS
    } else {
        attempt * FINGERPRINT_STEP
    };
    // Keep as much of the base as leaves room for `-` + the suffix.
    let keep = VOLUME_NAME_MAX
        .saturating_sub(take + 1)
        .min(base.as_bytes().len());
    let mut name = VolumeName {
        bytes: [0; VOLUME_NAME_MAX],
        len: 0,
    };
    for &byte in &base.as_bytes()[..keep] {
        name.push(byte);
    }
    name.push(b'-');
    for &byte in &fp[..take] {
        name.push(byte);
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use rustos_abi::volume::validate_volume_name;

    use super::*;

    #[test]
    fn labels_are_lowercased_and_stripped_to_the_alias_rules() {
        let name = sanitise_label(b"My Backup Disk!").expect("renderable");
        assert_eq!(name.as_bytes(), b"mybackupdisk");
        let name = sanitise_label(b"__Data-2024__").expect("renderable");
        assert_eq!(
            name.as_bytes(),
            b"data-2024__",
            "leading separators are stripped, interior and trailing kept"
        );
        assert!(validate_volume_name(name.as_bytes()).is_ok());
    }

    #[test]
    fn an_unrenderable_label_falls_through() {
        assert_eq!(sanitise_label(b""), None);
        assert_eq!(sanitise_label(b"!!! ***"), None);
        assert_eq!(sanitise_label(b"----"), None);
        assert_eq!(sanitise_label("διακοπές".as_bytes()), None);
    }

    #[test]
    fn an_overlong_label_is_bounded_by_the_name_grammar() {
        let long = [b'a'; 100];
        let name = sanitise_label(&long).expect("renderable");
        assert_eq!(name.as_bytes().len(), VOLUME_NAME_MAX);
        assert!(validate_volume_name(name.as_bytes()).is_ok());
    }

    #[test]
    fn fallback_names_are_fstype_and_ordinal() {
        assert_eq!(fallback_name(VolumeFsType::Fat32, 1).as_bytes(), b"fat1");
        assert_eq!(fallback_name(VolumeFsType::Ext4, 2).as_bytes(), b"ext2");
        assert_eq!(
            fallback_name(VolumeFsType::RustFs, 12).as_bytes(),
            b"rustfs12"
        );
        // A zero ordinal is a caller bug; it is clamped, never rendered
        // as an empty suffix.
        assert_eq!(fallback_name(VolumeFsType::Fat32, 0).as_bytes(), b"fat1");
    }

    #[test]
    fn candidates_lengthen_the_fingerprint_deterministically() {
        let base = sanitise_label(b"backup").expect("renderable");
        let identity = [7u8; 16];
        let c0 = candidate(&base, &identity, 0).expect("base");
        assert_eq!(c0.as_bytes(), b"backup");
        let c1 = candidate(&base, &identity, 1).expect("short suffix");
        assert_eq!(&c1.as_bytes()[..7], b"backup-");
        assert_eq!(c1.as_bytes().len(), 7 + 4);
        let c2 = candidate(&base, &identity, 2).expect("longer suffix");
        assert_eq!(c2.as_bytes().len(), 7 + 8);
        let c3 = candidate(&base, &identity, 3).expect("full fingerprint");
        assert_eq!(
            c3.as_bytes().len(),
            (7 + FINGERPRINT_CHARS).min(VOLUME_NAME_MAX)
        );
        assert_eq!(candidate(&base, &identity, 4), None, "sequence exhausts");

        // Deterministic: the same volume derives the same candidates.
        assert_eq!(candidate(&base, &identity, 1), Some(c1));
        // Distinct identities diverge.
        let other = candidate(&base, &[8u8; 16], 1).expect("other");
        assert_ne!(c1, other);
        for c in [c1, c2, c3] {
            assert!(validate_volume_name(c.as_bytes()).is_ok());
        }
    }

    #[test]
    fn a_long_base_is_truncated_so_the_suffix_fits() {
        let base = sanitise_label(&[b'x'; VOLUME_NAME_MAX]).expect("renderable");
        let identity = [3u8; 16];
        for attempt in 1..CANDIDATE_ATTEMPTS {
            let c = candidate(&base, &identity, attempt).expect("candidate");
            assert!(c.as_bytes().len() <= VOLUME_NAME_MAX);
            assert!(validate_volume_name(c.as_bytes()).is_ok());
            assert!(c.as_bytes().contains(&b'-'), "suffix survives truncation");
        }
        // The full-fingerprint candidate still ends with the whole
        // fingerprint (uniqueness is preserved even for maximal bases).
        let last = candidate(&base, &identity, CANDIDATE_ATTEMPTS - 1).expect("last");
        let fp = fingerprint(&identity);
        assert!(last.as_bytes().ends_with(&fp));
    }
}
