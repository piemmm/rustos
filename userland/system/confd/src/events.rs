//! Stable [`tairix_log::EventId`] constants emitted by `confd`.
//!
//! Per `lib/log` convention every subsystem owns a 1 000-wide reserved range.
//! The app-data service occupies `21000..22000` (following the desktop
//! session's `20000..21000`). Once shipped the numeric values must never be
//! re-used or re-numbered — external audit-log consumers rely on them.

use tairix_log::EventId;

use crate::StoreError;

/// Range start (inclusive) reserved for `confd` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const CONFD_RANGE_START: u32 = 21_000;
/// Range end (exclusive) reserved for `confd` event identifiers.
pub const CONFD_RANGE_END: u32 = 22_000;

/// The service bound `APPDATA_ENDPOINT` and is serving. Recorded once at
/// startup so an operator can see the store came up before any application
/// asked it for a setting.
pub const SERVICE_READY: EventId = EventId(21_001);

/// The service could not come up: the reserved endpoint could not be bound
/// (already held, or no registry) or its serve loop could not be armed. It
/// exits fail-closed rather than half-serving, and PID 1 relaunches it.
pub const SERVICE_UNAVAILABLE: EventId = EventId(21_002);

/// A caller reached the endpoint carrying no attested app identity, so it has
/// no store at all. Either a misconfiguration or a probe: a principal the
/// kernel did not admit from a signed bundle has nothing to be served.
pub const NO_APP_IDENTITY: EventId = EventId(21_003);

/// No directory under `/Users` is owned by the caller's uid, so the account
/// has no home the store could live in.
pub const NO_HOME: EventId = EventId(21_004);

/// The gated store root is absent or is not owned by the app-data service.
/// The store's parent is writable by the account, so a root the service does
/// not own is one an application planted — it is refused, never served out of.
pub const ROOT_NOT_OWNED: EventId = EventId(21_005);

/// A store's ownership pin names a different publisher: a developer other than
/// the one whose data is there is claiming the bundle identifier.
pub const PUBLISHER_MISMATCH: EventId = EventId(21_006);

/// A store's ownership pin is present but malformed, so it attests no owner
/// and nothing may be served out of the store.
pub const PIN_MALFORMED: EventId = EventId(21_007);

/// A configuration document on the volume, or a change to one, is outside the
/// format's fixed bounds. Refused whole rather than truncated.
pub const DOCUMENT_REFUSED: EventId = EventId(21_008);

/// The store volume could not be reached — the encrypted root is not yet
/// unlocked, or a read or write failed.
pub const STORE_UNAVAILABLE: EventId = EventId(21_009);

/// The event identifier recording `err`.
///
/// One mapping, so the audit stream and the caller's typed refusal can never
/// disagree about which refusal happened.
#[must_use]
pub const fn id_of(err: StoreError) -> EventId {
    match err {
        StoreError::NoAppIdentity => NO_APP_IDENTITY,
        StoreError::NoHome => NO_HOME,
        StoreError::RootNotOwned => ROOT_NOT_OWNED,
        StoreError::PublisherMismatch => PUBLISHER_MISMATCH,
        StoreError::PinMalformed => PIN_MALFORMED,
        StoreError::DocumentRefused => DOCUMENT_REFUSED,
        StoreError::Unavailable => STORE_UNAVAILABLE,
    }
}

#[cfg(test)]
mod tests {
    use super::{id_of, CONFD_RANGE_END, CONFD_RANGE_START, SERVICE_READY, SERVICE_UNAVAILABLE};
    use crate::StoreError;

    /// Every [`StoreError`] variant, so a new one cannot be added without an
    /// identifier of its own.
    const EVERY_ERROR: [StoreError; 7] = [
        StoreError::NoAppIdentity,
        StoreError::NoHome,
        StoreError::RootNotOwned,
        StoreError::PublisherMismatch,
        StoreError::PinMalformed,
        StoreError::DocumentRefused,
        StoreError::Unavailable,
    ];

    #[test]
    fn ids_are_inside_the_reserved_range() {
        let mut ids = alloc::vec![SERVICE_READY, SERVICE_UNAVAILABLE];
        ids.extend(EVERY_ERROR.iter().copied().map(id_of));
        for id in ids {
            assert!(
                id.0 >= CONFD_RANGE_START && id.0 < CONFD_RANGE_END,
                "{id:?}"
            );
        }
    }

    #[test]
    fn every_refusal_has_its_own_identifier() {
        let mut seen = alloc::vec![SERVICE_READY.0, SERVICE_UNAVAILABLE.0];
        for err in EVERY_ERROR {
            let id = id_of(err).0;
            assert!(!seen.contains(&id), "{err:?} reuses identifier {id}");
            seen.push(id);
        }
    }
}
