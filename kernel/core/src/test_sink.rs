//! In-memory [`rustos_log::Sink`] for host-side tests and feature-gated
//! integration harnesses.
//!
//! `TestSink` stores every received [`rustos_log::Event`] in order so a
//! test can assert the exact sequence of audit IDs emitted by
//! [`crate::kernel_main`] or [`crate::handle_panic`]. It is gated
//! behind `cfg(any(test, feature = "test-arch"))` so it never links
//! into a production build (`AGENTS.md` §1 — no hacks).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_log::{Event, EventId, Level, Sink};
use rustos_sync::SpinLock;

/// One captured event. The contents mirror the public fields of
/// [`Event`] but in owned form so the test can assert against them
/// after the borrowed record has gone out of scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedEvent {
    /// Severity.
    pub level: Level,
    /// Stable event id.
    pub id: EventId,
    /// Owned copy of the [`Event::message`] slice.
    pub message: String,
    /// Owned `(key, value)` copies of [`Event::fields`].
    pub fields: Vec<(String, String)>,
}

/// Capturing sink for host-side tests.
///
/// The sink uses an internal [`SpinLock`] so it can safely be installed
/// from the boot CPU and observed from a different test thread. In
/// production this would be a kernel ring buffer or a UART driver; the
/// observed *interface* is the same `Sink` trait.
#[derive(Debug, Default)]
pub struct TestSink {
    events: SpinLock<Vec<CapturedEvent>>,
}

impl TestSink {
    /// Construct an empty capturing sink.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: SpinLock::new(Vec::new()),
        }
    }

    /// Return a snapshot of every event captured so far, in arrival
    /// order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<CapturedEvent> {
        self.events.lock().clone()
    }

    /// Return only the [`EventId`]s captured so far, in arrival order.
    ///
    /// Convenience for tests that only assert init ordering, not field
    /// payloads.
    #[must_use]
    pub fn event_ids(&self) -> Vec<u32> {
        self.events.lock().iter().map(|e| e.id.0).collect()
    }

    /// Drop every captured event.
    ///
    /// Used by tests that share one sink across multiple assertions and
    /// need to ignore the events emitted during set-up (e.g. the
    /// `TaskCapabilitiesDerived` records the per-task fixture builders
    /// produce). `AGENTS.md` §7 — keep test assertions narrow.
    pub fn clear(&self) {
        self.events.lock().clear();
    }
}

impl Sink for TestSink {
    fn write_event(&self, event: &Event<'_>) {
        let fields = event
            .fields
            .iter()
            .map(|f| (f.key.to_string(), f.value.to_string()))
            .collect();
        self.events.lock().push(CapturedEvent {
            level: event.level,
            id: event.id,
            message: event.message.to_string(),
            fields,
        });
    }
}
