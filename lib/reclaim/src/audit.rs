//! Stable audit-log event IDs for the reclaimable-cache subsystem
//! (`plans/SMARTRAM.md` SMART9).
//!
//! Security-relevant reclaimable-cache failures emit exactly one
//! structured record through [`tairix_log`]. The numeric identifiers
//! are part of the audit contract with external log consumers and may
//! not be re-used or re-numbered. Per the range convention established
//! in `lib/log` (subsystems pick ranges of `1_000`), the reclaimable-
//! memory subsystem owns `2_000..3_000` — this crate emits 2000 and
//! 2001 for every cache it governs (kernel-side and desktop-side
//! alike), and the kernel's `ramzip` tier emits 2002 and 2003 from the
//! same range.
//!
//! # Event catalogue
//!
//! | ID   | Level | Name                     | Sink  | When |
//! |-----:|-------|--------------------------|-------|------|
//! | 2000 | Error | `RECLAIM_CACHE_REFUSED`  | audit | A cache candidate failed the [`classification gate`](crate::model::CacheCandidate::classify) at construction; the cache is poisoned from birth and its consumer serves without caching (fail closed). The `cause` field names the [`AdmissionRefusal`]. |
//! | 2001 | Error | `RECLAIM_CACHE_POISONED` | audit | A live cache detected an internal ledger or index defect (a corruption-like event), drained itself, and disabled admission (fail closed). The `cause` field names the defect. |
//!
//! Every record carries the same field shape: `cache` (the emitting
//! cache's fixed label), `owner` (the owning kernel subsystem's or
//! userland process's name, or the owner kind `volume` / `task` /
//! `session`), `owner_id` (the owning volume's mount handle, task id, or
//! seat; `0` for a named kernel subsystem or userland process), and
//! `cause`. The fields are fixed labels and numeric handles only — never
//! file names, cached plaintext, keys, or capability tokens
//! (`plans/SMARTRAM.md` section 9).
//!
//! Adding a new event requires assigning the next free identifier
//! across the whole `2_000..3_000` range — the `ramzip` tier's events
//! (2002, 2003) live in `kernel/mem::ramzip` — and updating the tables
//! in `docs/src/architecture/memory.md`.

use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};

use crate::model::{AdmissionRefusal, ReclaimOwner};

/// Audit event identifiers emitted for the reclaimable-cache
/// subsystem.
///
/// The numeric values are part of the stable contract between TAIRiX
/// and external log consumers; see the module-level table for the
/// meaning of each ID.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ReclaimAuditEvent {
    /// A cache candidate failed the classification gate at
    /// construction; the cache is poisoned from birth.
    CacheRefused,
    /// A live cache detected a ledger or index defect and disabled
    /// itself (fail closed).
    CachePoisoned,
}

impl ReclaimAuditEvent {
    /// Stable numeric identifier carried by the emitted log record.
    #[must_use]
    pub const fn id(self) -> EventId {
        EventId(match self {
            Self::CacheRefused => 2000,
            Self::CachePoisoned => 2001,
        })
    }

    /// Short, fixed name used as the `message` field of the emitted
    /// [`tairix_log::Event`]. Kept under the 120-character convention
    /// described in `lib/log` so a single record fits one terminal
    /// line.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::CacheRefused => "reclaimable cache classification refused",
            Self::CachePoisoned => "reclaimable cache poisoned",
        }
    }
}

/// The `owner` / `owner_id` field pair for a cache's declared owner.
///
/// A named kernel subsystem or userland process logs its name with id
/// `0`; a volume, task, or desktop-session owner logs its kind with the
/// numeric handle. An unclassified cache (the refusal itself may be a
/// missing owner) logs `unknown`.
const fn owner_fields(owner: Option<ReclaimOwner>) -> (&'static str, u64) {
    match owner {
        None => ("unknown", 0),
        Some(ReclaimOwner::KernelSubsystem(name) | ReclaimOwner::UserlandProcess(name)) => {
            (name, 0)
        }
        Some(ReclaimOwner::FilesystemVolume { volume }) => ("volume", volume),
        Some(ReclaimOwner::Task { task }) => ("task", task),
        Some(ReclaimOwner::DesktopSession { seat }) => ("session", seat),
    }
}

/// Emit one event with the fixed `cache` / `owner` / `owner_id` /
/// `cause` field shape.
fn emit(
    sink: &dyn Sink,
    event: ReclaimAuditEvent,
    cache: &'static str,
    owner: Option<ReclaimOwner>,
    cause: &'static str,
) {
    let (owner_name, owner_id) = owner_fields(owner);
    log(
        sink,
        &Event {
            level: Level::Error,
            id: event.id(),
            message: event.message(),
            fields: &[
                Field {
                    key: "cache",
                    value: FieldValue::Str(cache),
                },
                Field {
                    key: "owner",
                    value: FieldValue::Str(owner_name),
                },
                Field {
                    key: "owner_id",
                    value: FieldValue::UnsignedInt(owner_id),
                },
                Field {
                    key: "cause",
                    value: FieldValue::Str(cause),
                },
            ],
        },
    );
}

/// Log that the cache labelled `cache` was refused by the
/// classification gate and starts poisoned (fail closed).
pub fn log_cache_refused(
    sink: &dyn Sink,
    cache: &'static str,
    owner: Option<ReclaimOwner>,
    refusal: AdmissionRefusal,
) {
    emit(
        sink,
        ReclaimAuditEvent::CacheRefused,
        cache,
        owner,
        refusal.cause(),
    );
}

/// Log that the live cache labelled `cache` detected the internal
/// defect named by `cause` and poisoned itself (fail closed).
pub fn log_cache_poisoned(
    sink: &dyn Sink,
    cache: &'static str,
    owner: Option<ReclaimOwner>,
    cause: &'static str,
) {
    emit(sink, ReclaimAuditEvent::CachePoisoned, cache, owner, cause);
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::cell::RefCell;
    use std::string::{String, ToString};
    use std::vec::Vec;

    use tairix_log::{Event, FieldValue, Sink};

    use super::{log_cache_poisoned, log_cache_refused, ReclaimAuditEvent};
    use crate::model::{AdmissionRefusal, ReclaimOwner};

    /// One captured record: id, message, and rendered `(key, value)`
    /// fields.
    type Captured = (u32, String, Vec<(String, String)>);

    /// Sink capturing every record; tests are single-threaded so a
    /// `RefCell` suffices.
    struct CaptureSink {
        records: RefCell<Vec<Captured>>,
    }

    impl CaptureSink {
        fn new() -> Self {
            Self {
                records: RefCell::new(Vec::new()),
            }
        }
    }

    impl Sink for CaptureSink {
        fn write_event(&self, event: &Event<'_>) {
            let fields = event
                .fields
                .iter()
                .map(|f| {
                    let value = match f.value {
                        FieldValue::Str(s) => s.to_string(),
                        FieldValue::UnsignedInt(v) => v.to_string(),
                        _ => "<other>".to_string(),
                    };
                    (f.key.to_string(), value)
                })
                .collect();
            self.records
                .borrow_mut()
                .push((event.id.0, event.message.to_string(), fields));
        }
    }

    #[test]
    fn event_ids_are_in_the_kernel_mem_range_and_unique() {
        let ids = [
            ReclaimAuditEvent::CacheRefused.id().0,
            ReclaimAuditEvent::CachePoisoned.id().0,
        ];
        for id in ids {
            assert!((2000..3000).contains(&id));
        }
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn refusal_logs_the_stable_event_with_owner_and_cause() {
        let sink = CaptureSink::new();
        log_cache_refused(
            &sink,
            "clean_fs",
            Some(ReclaimOwner::FilesystemVolume { volume: 7 }),
            AdmissionRefusal::SensitiveMaterial,
        );
        let records = sink.records.borrow();
        assert_eq!(records.len(), 1);
        let (id, message, fields) = &records[0];
        assert_eq!(*id, 2000);
        assert_eq!(message, "reclaimable cache classification refused");
        assert_eq!(
            fields.as_slice(),
            [
                ("cache".to_string(), "clean_fs".to_string()),
                ("owner".to_string(), "volume".to_string()),
                ("owner_id".to_string(), "7".to_string()),
                ("cause".to_string(), "sensitive_material".to_string()),
            ]
        );
    }

    #[test]
    fn poison_logs_the_stable_event_for_every_owner_shape() {
        let sink = CaptureSink::new();
        log_cache_poisoned(
            &sink,
            "launch",
            Some(ReclaimOwner::KernelSubsystem("app_store")),
            "ledger_imbalance",
        );
        log_cache_poisoned(
            &sink,
            "transform",
            Some(ReclaimOwner::Task { task: 42 }),
            "orphan_index_slot",
        );
        log_cache_poisoned(
            &sink,
            "font.client",
            Some(ReclaimOwner::UserlandProcess("font-client")),
            "ledger_imbalance",
        );
        log_cache_poisoned(&sink, "clean_fs", None, "ledger_imbalance");
        let records = sink.records.borrow();
        assert_eq!(records.len(), 4);
        assert!(records.iter().all(|(id, ..)| *id == 2001));
        assert_eq!(
            records[0].2[1],
            ("owner".to_string(), "app_store".to_string())
        );
        assert_eq!(records[0].2[2], ("owner_id".to_string(), "0".to_string()));
        assert_eq!(records[1].2[1], ("owner".to_string(), "task".to_string()));
        assert_eq!(records[1].2[2], ("owner_id".to_string(), "42".to_string()));
        assert_eq!(
            records[2].2[1],
            ("owner".to_string(), "font-client".to_string())
        );
        assert_eq!(records[2].2[2], ("owner_id".to_string(), "0".to_string()));
        assert_eq!(
            records[3].2[1],
            ("owner".to_string(), "unknown".to_string())
        );
    }

    #[test]
    fn records_carry_only_fixed_labels_and_numeric_handles() {
        // The field shape is closed: exactly cache/owner/owner_id/cause,
        // so no path can smuggle a filename, plaintext, or token into
        // the diagnostic record.
        let sink = CaptureSink::new();
        log_cache_refused(
            &sink,
            "transform",
            Some(ReclaimOwner::FilesystemVolume { volume: 1 }),
            AdmissionRefusal::UnknownClass,
        );
        let records = sink.records.borrow();
        let keys: Vec<&str> = records[0].2.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["cache", "owner", "owner_id", "cause"]);
    }
}
