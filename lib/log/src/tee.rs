//! A fan-out [`Sink`] that delivers each event to several sinks.
//!
//! A subsystem sometimes needs one logical audit/log channel to reach more
//! than one destination — the classic case is retaining the boot audit trail
//! in memory *and* writing it to the serial console at the same time, so a
//! later reader (the pre-boot Supervisor's `log` command) can tail history
//! the console has already scrolled away. [`TeeSink`] is that fan-out: it
//! holds a fixed set of destination sinks and hands each filtered
//! [`Event`] to every one of them, in order.
//!
//! # Why here, and why one definition
//!
//! Fanning a log stream out to N sinks is a property of the *log* abstraction,
//! not of any one subsystem, so it lives beside [`Sink`] and is
//! defined exactly once rather than re-derived by every caller that needs two
//! destinations. It is allocation-free and `const`-constructible, so a
//! composite audit channel can live in a `static` with no initialiser code.
//!
//! # Filtering happens once, upstream
//!
//! [`log`](crate::log) applies the global level filter *before* it calls
//! [`Sink::write_event`], so by the time an event reaches a `TeeSink` it has
//! already passed the threshold; the tee re-applies no filter and every
//! destination receives exactly the same stream. A destination that copies
//! the event into its own storage (a ring) and one that renders it to a
//! device (a UART) therefore stay consistent.
//!
//! # Destinations must not block or panic
//!
//! The tee calls each destination synchronously and in order, so a
//! destination is held to the same [`Sink`] contract as any
//! other: cheap, non-blocking, never panicking. A slow destination delays the
//! ones after it, so order destinations fast-first where retention matters
//! more than immediate display.

use crate::{Event, Sink};

/// A [`Sink`] that delivers every event to each of `N` destination sinks.
///
/// The destinations are borrowed for the lifetime `'a`; in a `static` audit
/// channel that lifetime is `'static`. Each destination is `Sync` so the
/// composite is itself `Sync` and can back a shared `&'static (dyn Sink +
/// Sync)`.
///
/// ```
/// use core::sync::atomic::{AtomicUsize, Ordering};
/// use tairix_log::{Event, EventId, Level, Sink, TeeSink};
///
/// struct Counter(AtomicUsize);
/// impl Sink for Counter {
///     fn write_event(&self, _event: &Event<'_>) {
///         self.0.fetch_add(1, Ordering::Relaxed);
///     }
/// }
///
/// let a = Counter(AtomicUsize::new(0));
/// let b = Counter(AtomicUsize::new(0));
/// let tee = TeeSink::new([&a as &(dyn Sink + Sync), &b]);
/// tee.write_event(&Event {
///     level: Level::Info,
///     id: EventId(1),
///     message: "hello",
///     fields: &[],
/// });
/// assert_eq!(a.0.load(Ordering::Relaxed), 1);
/// assert_eq!(b.0.load(Ordering::Relaxed), 1);
/// ```
pub struct TeeSink<'a, const N: usize> {
    destinations: [&'a (dyn Sink + Sync); N],
}

impl<'a, const N: usize> TeeSink<'a, N> {
    /// Compose a fan-out over `destinations`.
    ///
    /// `const` so a composite audit channel can be built in a `static`
    /// initialiser with no runtime setup.
    #[must_use]
    pub const fn new(destinations: [&'a (dyn Sink + Sync); N]) -> Self {
        Self { destinations }
    }
}

impl<const N: usize> Sink for TeeSink<'_, N> {
    fn write_event(&self, event: &Event<'_>) {
        for destination in self.destinations {
            destination.write_event(event);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::TeeSink;
    use crate::{Event, EventId, Level, Sink};
    use core::cell::RefCell;
    use std::string::{String, ToString};
    use std::vec;
    use std::vec::Vec;

    /// A `Sync` recording sink: a `Mutex`-guarded log of the messages it saw,
    /// so a `&Recorder` satisfies the tee's `dyn Sink + Sync` element bound.
    struct Recorder {
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn messages(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl Sink for Recorder {
        fn write_event(&self, event: &Event<'_>) {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.message.to_string());
        }
    }

    fn event(message: &str) -> Event<'_> {
        Event {
            level: Level::Info,
            id: EventId(7),
            message,
            fields: &[],
        }
    }

    #[test]
    fn every_destination_receives_every_event_in_order() {
        let first = Recorder::new();
        let second = Recorder::new();
        let tee = TeeSink::new([&first as &(dyn Sink + Sync), &second]);

        tee.write_event(&event("alpha"));
        tee.write_event(&event("beta"));

        let expected = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(first.messages(), expected);
        assert_eq!(second.messages(), expected);
    }

    #[test]
    fn a_single_destination_tee_is_a_pass_through() {
        let only = Recorder::new();
        let tee = TeeSink::new([&only as &(dyn Sink + Sync)]);
        tee.write_event(&event("solo"));
        assert_eq!(only.messages(), vec!["solo".to_string()]);
    }

    #[test]
    fn an_empty_tee_drops_the_event_without_panicking() {
        // A zero-destination fan-out is a degenerate but valid configuration:
        // the event simply goes nowhere. It must never panic.
        let tee: TeeSink<'_, 0> = TeeSink::new([]);
        tee.write_event(&event("void"));
    }

    /// A `RefCell`-based sink is *not* `Sync`, so it cannot be a `TeeSink`
    /// destination — this test only documents that the recording helper the
    /// other tests use is deliberately `Mutex`-backed for that reason.
    #[test]
    fn recording_helper_is_sync() {
        fn assert_sync<T: Sync>() {}
        assert_sync::<Recorder>();
        // A `RefCell<Vec<_>>` sink would fail `assert_sync`, which is why the
        // helper above uses a `Mutex`.
        let _not_sync = RefCell::new(Vec::<String>::new());
    }
}
