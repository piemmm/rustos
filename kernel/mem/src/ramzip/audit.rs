//! Stable audit-log event IDs for the compressed anonymous-memory tier
//! (`plans/SWAPSWAPSWAP.md` sections 9 and 16).
//!
//! A sealed entry that fails authentication or decode is a
//! security-relevant event: the page's contents are unrecoverable and
//! the tier fails closed. Each such failure emits exactly one
//! structured record through [`tairix_log`]. The identifiers continue
//! the `kernel/mem` range (`2_000..3_000`) established in
//! [`tairix_reclaim::audit`]; IDs are assigned once across the whole
//! range and never re-used or re-numbered.
//!
//! # Event catalogue
//!
//! | ID   | Level | Name                    | Sink  | When |
//! |-----:|-------|-------------------------|-------|------|
//! | 2002 | Error | `RAMZIP_AUTH_FAILURE`   | audit | A compressed entry failed AEAD authentication on restore: tampered, replayed, or damaged in RAM. The entry is discarded, no plaintext is returned, and the fault escalates through the VM policy. |
//! | 2003 | Error | `RAMZIP_ENTRY_CORRUPT`  | audit | A compressed entry failed metadata validation or decompression after authenticating. The entry is discarded and no partial plaintext is returned. |
//!
//! Every record carries the same field shape: `space` (the owning
//! address-space id), `page` (the faulting page number), and `task`
//! (the owning task id). The fields are numeric handles only — never
//! page contents, key material, or nonces.

use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};

/// Audit event identifiers emitted by the compressed-memory tier.
///
/// The numeric values are part of the stable contract between TAIRiX
/// and external log consumers; see the module-level table.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum RamzipAuditEvent {
    /// A compressed entry failed authentication on restore.
    AuthenticationFailure,
    /// A compressed entry failed metadata validation or decode.
    EntryCorrupt,
}

impl RamzipAuditEvent {
    /// Stable numeric identifier carried by the emitted log record.
    #[must_use]
    pub const fn id(self) -> EventId {
        EventId(match self {
            Self::AuthenticationFailure => 2002,
            Self::EntryCorrupt => 2003,
        })
    }

    /// Short, fixed name used as the `message` field of the emitted
    /// [`tairix_log::Event`].
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::AuthenticationFailure => "ramzip entry failed authentication",
            Self::EntryCorrupt => "ramzip entry corrupt",
        }
    }
}

/// Emit the audit record for a failed restore through `sink`.
///
/// `space` and `page` locate the lost entry; `task` names the owner.
/// Only numeric handles are logged — no page contents, key bytes, or
/// nonce values ever reach the log.
pub fn log_ramzip_failure(
    sink: &dyn Sink,
    event: RamzipAuditEvent,
    space: u64,
    page: u64,
    task: u64,
) {
    log(
        sink,
        &Event {
            level: Level::Error,
            id: event.id(),
            message: event.message(),
            fields: &[
                Field {
                    key: "space",
                    value: FieldValue::UnsignedInt(space),
                },
                Field {
                    key: "page",
                    value: FieldValue::UnsignedInt(page),
                },
                Field {
                    key: "task",
                    value: FieldValue::UnsignedInt(task),
                },
            ],
        },
    );
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn event_ids_are_stable_and_distinct() {
        assert_eq!(RamzipAuditEvent::AuthenticationFailure.id(), EventId(2002));
        assert_eq!(RamzipAuditEvent::EntryCorrupt.id(), EventId(2003));
    }

    #[test]
    fn messages_are_short_and_fixed() {
        for event in [
            RamzipAuditEvent::AuthenticationFailure,
            RamzipAuditEvent::EntryCorrupt,
        ] {
            assert!(!event.message().is_empty());
            assert!(event.message().len() < 120);
        }
    }

    /// Sink capturing record ids; tests are single-threaded so a
    /// `RefCell` suffices.
    struct CaptureSink {
        ids: core::cell::RefCell<alloc::vec::Vec<u32>>,
    }

    impl Sink for CaptureSink {
        fn write_event(&self, event: &Event<'_>) {
            self.ids.borrow_mut().push(event.id.0);
        }
    }

    #[test]
    fn a_failure_emits_exactly_one_record_with_its_stable_id() {
        let sink = CaptureSink {
            ids: core::cell::RefCell::new(alloc::vec::Vec::new()),
        };
        log_ramzip_failure(&sink, RamzipAuditEvent::AuthenticationFailure, 1, 2, 3);
        log_ramzip_failure(&sink, RamzipAuditEvent::EntryCorrupt, u64::MAX, 0, 42);
        assert_eq!(*sink.ids.borrow(), alloc::vec![2002, 2003]);
    }
}
