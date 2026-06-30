//! Structured, level-filtered logging for RustOS.
//!
//! Design goals:
//!
//! * **Structured.** Every emitted record carries a stable [`EventId`]; the
//!   message body is a borrowed `&str` plus a slice of borrowed key/value
//!   pairs ([`Field`]) so the hot path never touches an allocator.
//! * **Level-filtered.** A single atomic [`Level`] gates whether a record is
//!   handed to the sink. Below the filter level, [`log`] returns in O(1)
//!   with no formatting work performed.
//! * **No-alloc.** No `format!`, no `String`, no `Box`. Sinks are expected
//!   to write into static buffers, ring buffers, or device registers.
//! * **Stable event IDs.** Event identifiers are assigned by their callers
//!   and treated as part of the ABI between RustOS and external log
//!   consumers; they may not be re-used or re-numbered.
//!
//! Sink installation is intentionally not a global registration table.
//! Stage 2 will wire `kernel/sec` to a single in-kernel sink and userland
//! processes to one of the IPC-backed sinks defined by Stage 4; both
//! present this crate's [`Sink`] trait to their callers, no more.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod chain;
pub mod field;

pub use chain::{
    verify_chain, verify_fresh_chain, ChainError, ChainedEntry, LogChain, GENESIS_ANCHOR,
};
pub use field::{
    reserved_prefix, Decimal, FieldList, FieldName, FieldValue, IpAddr, MacAddr, ScalarType,
    ToFieldValue, Uuid, RESERVED_PREFIXES,
};

use core::sync::atomic::{AtomicU8, Ordering};

/// Severity of a logged event.
///
/// Numeric ordering matters: a sink that filters with `level >= MIN`
/// receives the more severe records. The numeric values themselves are
/// `abi-v1` and must not be renumbered.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Level {
    /// Fine-grained tracing; off by default.
    Trace = 0,
    /// Diagnostic information.
    Debug = 1,
    /// Routine operational events.
    Info = 2,
    /// Recoverable anomalies.
    Warn = 3,
    /// Errors that affect correctness or security.
    Error = 4,
}

impl Level {
    /// Numeric representation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`]; returns `None` for unknown values.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Trace),
            1 => Some(Self::Debug),
            2 => Some(Self::Info),
            3 => Some(Self::Warn),
            4 => Some(Self::Error),
            _ => None,
        }
    }
}

/// Stable numeric identifier for a logged event.
///
/// The identifier is part of the contract with external log consumers:
/// once published, it must never change meaning or be re-used. New events
/// take the next free identifier in their owning subsystem's reserved
/// range (subsystems pick ranges of `1_000`, e.g. `1_000..2_000` for
/// `kernel/sec`, `2_000..3_000` for `kernel/mem`, …).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct EventId(pub u32);

/// Structured key/value field carried by an [`Event`].
///
/// Values are `&str` so callers can pass debug-formatted slices written
/// into stack buffers — no allocation is performed by this crate.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Field<'a> {
    /// Field name.
    pub key: &'a str,
    /// Field value rendered as text.
    pub value: &'a str,
}

/// Structured log record.
///
/// All references are borrowed for the lifetime of the call; sinks must
/// either consume the record synchronously or copy what they need into
/// their own storage (a ring buffer, a serial register, etc.).
#[derive(Copy, Clone, Debug)]
pub struct Event<'a> {
    /// Severity.
    pub level: Level,
    /// Stable identifier.
    pub id: EventId,
    /// Short, human-readable description. Held to <= 120 characters by
    /// convention so a single record fits one terminal line.
    pub message: &'a str,
    /// Optional structured fields. Empty slice when absent.
    pub fields: &'a [Field<'a>],
}

/// Receiver of filtered log events.
///
/// Sinks must be cheap to call from a kernel context and must not panic.
/// Implementations typically copy the event into a ring buffer to be
/// consumed by an async drainer.
pub trait Sink {
    /// Handle one filtered event.
    fn write_event(&self, event: &Event<'_>);
}

/// Global maximum severity filter, exclusive of records below this level.
///
/// Initialised to [`Level::Info`]; change with [`set_max_level`] (e.g. from
/// boot-time configuration parsed by `kernel/core`). Stored as the numeric
/// `u8` form of [`Level`] so that the atomic stays lock-free on every
/// Tier-1 target.
static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Return the current global level threshold.
#[must_use]
pub fn max_level() -> Level {
    Level::from_u8(MAX_LEVEL.load(Ordering::Relaxed)).unwrap_or(Level::Info)
}

/// Set the global level threshold.
///
/// Records strictly less severe than `level` are dropped before reaching
/// any sink.
pub fn set_max_level(level: Level) {
    MAX_LEVEL.store(level.as_u8(), Ordering::Relaxed);
}

/// Submit `event` to `sink` if it meets the current threshold.
///
/// Returns `true` if the event was delivered, `false` if it was filtered
/// out. Calling this with a level below [`max_level`] is the fast path:
/// the only work performed is one relaxed atomic load and one comparison.
pub fn log<S: Sink + ?Sized>(sink: &S, event: &Event<'_>) -> bool {
    if event.level.as_u8() < MAX_LEVEL.load(Ordering::Relaxed) {
        return false;
    }
    sink.write_event(event);
    true
}

#[cfg(test)]
mod tests {
    use super::{log, max_level, set_max_level, Event, EventId, Field, Level, Sink};
    use core::cell::RefCell;

    /// Sink that stores received events in a thread-local-equivalent
    /// `RefCell` — sufficient because tests are single-threaded.
    struct CountingSink {
        events: RefCell<Vec<(Level, u32, String)>>,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
            }
        }
    }

    impl Sink for CountingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events
                .borrow_mut()
                .push((event.level, event.id.0, event.message.to_string()));
        }
    }

    // Tests share the global `MAX_LEVEL`. To stay correct under
    // `cargo test`'s default multi-threaded execution we serialise every
    // test that touches the threshold with a `Mutex`. This is a test-only
    // concern: the global is `Relaxed`-atomic on the production path.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_level<R>(level: Level, body: impl FnOnce() -> R) -> R {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = max_level();
        set_max_level(level);
        let result = body();
        set_max_level(previous);
        drop(guard);
        result
    }

    #[test]
    fn level_numeric_round_trip() {
        for raw in 0..=4u8 {
            let level = Level::from_u8(raw).expect("known level");
            assert_eq!(level.as_u8(), raw);
        }
        assert!(Level::from_u8(5).is_none());
    }

    #[test]
    fn level_ordering_is_severity_ascending() {
        assert!(Level::Trace < Level::Debug);
        assert!(Level::Debug < Level::Info);
        assert!(Level::Info < Level::Warn);
        assert!(Level::Warn < Level::Error);
    }

    #[test]
    fn below_threshold_events_are_dropped() {
        with_level(Level::Warn, || {
            let sink = CountingSink::new();
            let dropped = Event {
                level: Level::Info,
                id: EventId(10),
                message: "ignored",
                fields: &[],
            };
            let kept = Event {
                level: Level::Error,
                id: EventId(11),
                message: "kept",
                fields: &[],
            };
            assert!(!log(&sink, &dropped));
            assert!(log(&sink, &kept));
            let recorded = sink.events.borrow();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].1, 11);
        });
    }

    #[test]
    fn fields_are_forwarded() {
        with_level(Level::Trace, || {
            let sink = CountingSink::new();
            let event = Event {
                level: Level::Info,
                id: EventId(7),
                message: "with fields",
                fields: &[Field {
                    key: "k",
                    value: "v",
                }],
            };
            assert!(log(&sink, &event));
            assert_eq!(sink.events.borrow().len(), 1);
        });
    }

    extern crate alloc;
    extern crate std;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
}
