//! Stable audit-log event IDs and the writer used by `kernel/sec`.
//!
//! Every security-relevant decision taken by this crate emits exactly one
//! structured log record through [`rustos_log`]. The numeric identifiers
//! are part of the audit contract with external log consumers and may not be re-used or re-numbered. They live
//! in the `kernel/sec` range reserved by [`rustos_log::EventId`]
//! conventions: `1_000..2_000`.
//!
//! # Event catalogue
//!
//! | ID   | Level | Name                              | When |
//! |-----:|-------|-----------------------------------|------|
//! | 1000 | Info  | `IDENTITY_TABLE_LOADED`           | A user/group table builder produced a verified [`crate::IdentityTable`]. |
//! | 1001 | Error | `IDENTITY_TABLE_REJECTED`         | A builder was rejected (duplicate id, unknown gid, oversize set). |
//! | 1010 | Info  | `MANIFEST_VERIFIED`               | A signed `rxe` manifest passed every check. |
//! | 1011 | Error | `MANIFEST_BAD_HEADER`             | Manifest header failed structural validation (bad magic, short buffer, oversize). |
//! | 1012 | Error | `MANIFEST_ABI_MISMATCH`           | Manifest header parsed but its `abi_version` is not the kernel's. |
//! | 1013 | Error | `MANIFEST_SIGNATURE_INVALID`      | Ed25519 signature over the manifest failed verification. |
//! | 1014 | Error | `MANIFEST_UNKNOWN_CAPABILITY`     | Manifest body requested a capability ID the kernel does not know. |
//! | 1020 | Info  | `TASK_CAPABILITIES_DERIVED`       | A task's capability set was derived from a user grant and a manifest request. |
//! | 1021 | Info  | `TASK_CAPABILITIES_DELEGATED`     | A delegated subset was installed under a task. |
//! | 1022 | Error | `TASK_CAPABILITIES_DELEGATE_WIDEN`| A delegation attempt would have widened the parent set and was refused. |
//! | 1023 | Info  | `TASK_CAPABILITIES_REVOKED`       | One or more capabilities were revoked from a task. |
//! | 1030 | Info | `DMA_ALLOCATED` | A DMA buffer was allocated for a task that holds `CAP_MEM_DMA`. |
//! | 1031 | Error | `DMA_ALLOC_DENIED`                | A DMA allocation was refused because the calling task lacks `CAP_MEM_DMA`. |
//! | 1040 | Info | `MMIO_MAPPED` | A device register window was mapped for a task that holds `CAP_MMIO_MAP`. |
//! | 1041 | Error | `MMIO_MAP_DENIED`                 | An MMIO-map request was refused because the calling task lacks `CAP_MMIO_MAP`. |
//!
//! Adding a new event requires assigning the next free identifier in this
//! file and appending a row to the table in
//! `docs/src/architecture/security.md`.

use rustos_log::{log, Event, EventId, Field, Level, Sink};

/// Audit log event identifiers used by `kernel/sec`.
///
/// The associated numeric values are part of the ABI between RustOS and
/// external log consumers and may not be re-used or re-numbered. See the
/// module-level table for the meaning of each ID.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AuditEvent {
    /// A user/group table was assembled and verified.
    IdentityTableLoaded,
    /// A user/group table builder was rejected.
    IdentityTableRejected,
    /// A signed manifest verified end-to-end.
    ManifestVerified,
    /// Manifest header failed structural validation.
    ManifestBadHeader,
    /// Manifest ABI version does not match the kernel's.
    ManifestAbiMismatch,
    /// Manifest Ed25519 signature did not verify.
    ManifestSignatureInvalid,
    /// Manifest body referenced an unknown capability identifier.
    ManifestUnknownCapability,
    /// A task's effective capability set was derived.
    TaskCapabilitiesDerived,
    /// A delegated subset was installed on a task.
    TaskCapabilitiesDelegated,
    /// A delegation attempt was refused because it would have widened the
    /// parent's authority.
    TaskCapabilitiesDelegateWiden,
    /// One or more capabilities were revoked from a task.
    TaskCapabilitiesRevoked,
    /// A DMA buffer was allocated through the capability-gated
    /// per-process DMA pool.
    DmaAllocated,
    /// A DMA allocation was refused because the calling task does
    /// not hold `CapabilityId::MEM_DMA`.
    DmaAllocDenied,
    /// A device register window was mapped through the
    /// capability-gated MMIO-map facility.
    MmioMapped,
    /// An MMIO-map request was refused because the calling task does
    /// not hold `CapabilityId::MMIO_MAP`.
    MmioMapDenied,
}

impl AuditEvent {
    /// Stable numeric identifier carried by the emitted [`Event`].
    #[must_use]
    pub const fn id(self) -> EventId {
        EventId(match self {
            Self::IdentityTableLoaded => 1000,
            Self::IdentityTableRejected => 1001,
            Self::ManifestVerified => 1010,
            Self::ManifestBadHeader => 1011,
            Self::ManifestAbiMismatch => 1012,
            Self::ManifestSignatureInvalid => 1013,
            Self::ManifestUnknownCapability => 1014,
            Self::TaskCapabilitiesDerived => 1020,
            Self::TaskCapabilitiesDelegated => 1021,
            Self::TaskCapabilitiesDelegateWiden => 1022,
            Self::TaskCapabilitiesRevoked => 1023,
            Self::DmaAllocated => 1030,
            Self::DmaAllocDenied => 1031,
            Self::MmioMapped => 1040,
            Self::MmioMapDenied => 1041,
        })
    }

    /// Severity at which this event is emitted.
    ///
    /// Successful security decisions are recorded at [`Level::Info`] so
    /// that operators can review the positive trail; refused decisions are
    /// recorded at [`Level::Error`] so they surface above a routine info
    /// filter without further configuration.
    #[must_use]
    pub const fn level(self) -> Level {
        match self {
            Self::IdentityTableLoaded
            | Self::ManifestVerified
            | Self::TaskCapabilitiesDerived
            | Self::TaskCapabilitiesDelegated
            | Self::TaskCapabilitiesRevoked
            | Self::DmaAllocated
            | Self::MmioMapped => Level::Info,
            Self::IdentityTableRejected
            | Self::ManifestBadHeader
            | Self::ManifestAbiMismatch
            | Self::ManifestSignatureInvalid
            | Self::ManifestUnknownCapability
            | Self::TaskCapabilitiesDelegateWiden
            | Self::DmaAllocDenied
            | Self::MmioMapDenied => Level::Error,
        }
    }

    /// Short, stable human-readable message embedded in the [`Event`].
    ///
    /// The text is consumed by structured log readers and must not change
    /// once shipped: callers correlate by [`Self::id`] but operators read
    /// the message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::IdentityTableLoaded => "identity table loaded",
            Self::IdentityTableRejected => "identity table rejected",
            Self::ManifestVerified => "manifest verified",
            Self::ManifestBadHeader => "manifest header malformed",
            Self::ManifestAbiMismatch => "manifest abi version mismatch",
            Self::ManifestSignatureInvalid => "manifest signature invalid",
            Self::ManifestUnknownCapability => "manifest requests unknown capability",
            Self::TaskCapabilitiesDerived => "task capabilities derived",
            Self::TaskCapabilitiesDelegated => "task capabilities delegated",
            Self::TaskCapabilitiesDelegateWiden => "task delegation would widen authority",
            Self::TaskCapabilitiesRevoked => "task capabilities revoked",
            Self::DmaAllocated => "dma buffer allocated",
            Self::DmaAllocDenied => "dma allocation denied: missing CAP_MEM_DMA",
            Self::MmioMapped => "mmio register window mapped",
            Self::MmioMapDenied => "mmio map denied: missing CAP_MMIO_MAP",
        }
    }
}

/// Emit `event` to `sink` with the supplied structured fields.
///
/// Returns whatever [`rustos_log::log`] returns: `true` if the event made
/// it past the global level filter, `false` if it was dropped. Callers in
/// this crate ignore the return value because the audit trail's
/// configuration — not the call site — decides whether the record reaches
/// a backing store; the *decision* itself is recorded by virtue of the
/// call.
pub(crate) fn record<S: Sink + ?Sized>(sink: &S, event: AuditEvent, fields: &[Field<'_>]) -> bool {
    log(
        sink,
        &Event {
            level: event.level(),
            id: event.id(),
            message: event.message(),
            fields,
        },
    )
}

/// Shared test-only recording sink and helpers used by every module's
/// tests. Lives outside `#[cfg(test)] mod tests` so `clippy::
/// items_after_test_module` does not fire and so unit tests in
/// `identity.rs`, `manifest.rs`, and `captable.rs` can import it via
/// `crate::audit::RecordingSink`.
#[cfg(test)]
pub(crate) mod test_support {
    extern crate alloc;
    extern crate std;

    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use rustos_log::{set_max_level, Event, Level, Sink};
    use std::cell::RefCell;

    /// Single-threaded recording sink used by every `kernel/sec` test.
    ///
    /// `RefCell` is sufficient because Cargo tests in this crate are
    /// `#[test]` functions executed sequentially when they touch the
    /// global log threshold (we lower it to `Trace` in tests so every
    /// audit `Info` event is kept).
    pub(crate) struct RecordingSink {
        events: RefCell<Vec<(Level, u32, String)>>,
    }

    impl RecordingSink {
        pub fn new() -> Self {
            // Lower the global filter so audit `Info` events are not
            // dropped by the default `Info` threshold (which is "drop
            // strictly less severe", i.e. `Info` is kept anyway — but
            // tests that introduce a `Debug` sub-event still need the
            // lower bound).
            set_max_level(Level::Trace);
            Self {
                events: RefCell::new(Vec::new()),
            }
        }

        pub fn ids(&self) -> Vec<u32> {
            self.events.borrow().iter().map(|e| e.1).collect()
        }

        pub fn len(&self) -> usize {
            self.events.borrow().len()
        }
    }

    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events
                .borrow_mut()
                .push((event.level, event.id.0, event.message.to_string()));
        }
    }
}

#[cfg(test)]
pub(crate) use test_support::RecordingSink;

#[cfg(test)]
mod tests {
    use super::{record, AuditEvent, RecordingSink};
    use rustos_log::{EventId, Field};

    #[test]
    fn ids_are_frozen() {
        // Verifying the numeric values pins the audit contract.
        assert_eq!(AuditEvent::IdentityTableLoaded.id(), EventId(1000));
        assert_eq!(AuditEvent::IdentityTableRejected.id(), EventId(1001));
        assert_eq!(AuditEvent::ManifestVerified.id(), EventId(1010));
        assert_eq!(AuditEvent::ManifestBadHeader.id(), EventId(1011));
        assert_eq!(AuditEvent::ManifestAbiMismatch.id(), EventId(1012));
        assert_eq!(AuditEvent::ManifestSignatureInvalid.id(), EventId(1013));
        assert_eq!(AuditEvent::ManifestUnknownCapability.id(), EventId(1014));
        assert_eq!(AuditEvent::TaskCapabilitiesDerived.id(), EventId(1020));
        assert_eq!(AuditEvent::TaskCapabilitiesDelegated.id(), EventId(1021));
        assert_eq!(
            AuditEvent::TaskCapabilitiesDelegateWiden.id(),
            EventId(1022)
        );
        assert_eq!(AuditEvent::TaskCapabilitiesRevoked.id(), EventId(1023));
        assert_eq!(AuditEvent::DmaAllocated.id(), EventId(1030));
        assert_eq!(AuditEvent::DmaAllocDenied.id(), EventId(1031));
        assert_eq!(AuditEvent::MmioMapped.id(), EventId(1040));
        assert_eq!(AuditEvent::MmioMapDenied.id(), EventId(1041));
    }

    #[test]
    fn record_forwards_one_event_per_call() {
        let sink = RecordingSink::new();
        let kept = record(&sink, AuditEvent::ManifestVerified, &[]);
        assert!(kept);
        assert_eq!(sink.ids(), [AuditEvent::ManifestVerified.id().0]);
    }

    #[test]
    fn record_passes_fields_through() {
        let sink = RecordingSink::new();
        let fields = [Field {
            key: "uid",
            value: "42",
        }];
        record(&sink, AuditEvent::TaskCapabilitiesDerived, &fields);
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn ids_fall_within_kernel_sec_reserved_range() {
        // `rustos_log::EventId` reserves `1_000..2_000` for `kernel/sec`.
        for ev in [
            AuditEvent::IdentityTableLoaded,
            AuditEvent::IdentityTableRejected,
            AuditEvent::ManifestVerified,
            AuditEvent::ManifestBadHeader,
            AuditEvent::ManifestAbiMismatch,
            AuditEvent::ManifestSignatureInvalid,
            AuditEvent::ManifestUnknownCapability,
            AuditEvent::TaskCapabilitiesDerived,
            AuditEvent::TaskCapabilitiesDelegated,
            AuditEvent::TaskCapabilitiesDelegateWiden,
            AuditEvent::TaskCapabilitiesRevoked,
            AuditEvent::DmaAllocated,
            AuditEvent::DmaAllocDenied,
            AuditEvent::MmioMapped,
            AuditEvent::MmioMapDenied,
        ] {
            let id = ev.id().0;
            assert!(
                (1_000..2_000).contains(&id),
                "audit id {id} outside kernel/sec range"
            );
        }
    }
}
