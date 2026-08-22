//! Stable [`tairix_log::EventId`] constants emitted by `appmgr`.
//!
//! Per `lib/log` convention every subsystem owns a
//! 1 000-wide reserved range. The application loader occupies
//! `11000..12000` (adjacent to `login`'s `10000..11000`). Once shipped the
//! numeric values must never be re-used or re-numbered — external audit-log
//! consumers rely on them.

use tairix_log::EventId;

/// Range start (inclusive) reserved for `appmgr` event identifiers.
pub const APPMGR_RANGE_START: u32 = 11_000;
/// Range end (exclusive) reserved for `appmgr` event identifiers.
pub const APPMGR_RANGE_END: u32 = 12_000;

/// A bundle was accepted: its layout validated, its manifest verified, and
/// its capability ceiling computed.
///
/// The record carries two timing fields so a slow first launch is
/// diagnosable: `load`, the time spent reading the bundle off the store (the
/// "getting it from disk" cost), and `verify`, the remaining time spent
/// checking it (layout, manifest, interface hash, signature, content hash,
/// and the entry-point image).
pub const APP_LOADED: EventId = EventId(11_001);
/// A bundle was refused because its top-level layout deviates from the fixed
/// set.
pub const APP_LAYOUT_REJECTED: EventId = EventId(11_002);
/// A bundle was refused because its `AppInfo` manifest could not be decoded
/// or targets an unsupported ABI version.
pub const APP_MANIFEST_INVALID: EventId = EventId(11_003);
/// A bundle was refused because its declared syscall-table hash does not
/// match the kernel's.
pub const APP_INTERFACE_MISMATCH: EventId = EventId(11_004);
/// A bundle was refused because its manifest signature did not verify.
pub const APP_SIGNATURE_INVALID: EventId = EventId(11_005);
/// A bundle was refused because its contents do not match the content hash
/// the signature covers.
pub const APP_CONTENT_MISMATCH: EventId = EventId(11_006);
/// A bundle could not be read from the store (an I/O failure).
pub const APP_STORE_ERROR: EventId = EventId(11_007);
/// A shared-library reference resolved within the policy.
pub const LIBRARY_RESOLVED: EventId = EventId(11_008);
/// A shared-library reference was refused because it points outside the
/// bundle's `Libraries/` and `/System/Libraries/`.
pub const LIBRARY_REFUSED: EventId = EventId(11_009);
/// A bundle was refused because its entry-point `Run` binary is not a valid
/// `rxe` load image, or its CFI tag does not match the kernel's syscall
/// interface hash.
pub const APP_RUN_IMAGE_INVALID: EventId = EventId(11_010);
/// A bundle was refused because its manifest's publisher fields are
/// malformed, or its publisher delegation certificate did not verify — the
/// bundle cannot be attributed to a developer, so it gets no identity and no
/// per-app store.
pub const APP_PUBLISHER_INVALID: EventId = EventId(11_011);

#[cfg(test)]
mod tests {
    use super::{
        APPMGR_RANGE_END, APPMGR_RANGE_START, APP_CONTENT_MISMATCH, APP_INTERFACE_MISMATCH,
        APP_LAYOUT_REJECTED, APP_LOADED, APP_MANIFEST_INVALID, APP_PUBLISHER_INVALID,
        APP_RUN_IMAGE_INVALID, APP_SIGNATURE_INVALID, APP_STORE_ERROR, LIBRARY_REFUSED,
        LIBRARY_RESOLVED,
    };

    const ALL: [u32; 11] = [
        APP_LOADED.0,
        APP_LAYOUT_REJECTED.0,
        APP_MANIFEST_INVALID.0,
        APP_INTERFACE_MISMATCH.0,
        APP_SIGNATURE_INVALID.0,
        APP_CONTENT_MISMATCH.0,
        APP_STORE_ERROR.0,
        LIBRARY_RESOLVED.0,
        LIBRARY_REFUSED.0,
        APP_RUN_IMAGE_INVALID.0,
        APP_PUBLISHER_INVALID.0,
    ];

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in ALL {
            assert!((APPMGR_RANGE_START..APPMGR_RANGE_END).contains(&id));
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = ALL;
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate appmgr EventId");
        }
    }
}
