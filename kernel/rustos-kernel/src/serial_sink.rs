//! COM1-backed [`rustos_log::Sink`] for the freestanding kernel.
//!
//! The Sink writes one record per call in the canonical format the
//! `kernel/core` audit consumers expect:
//!
//! ```text
//! [<level>] id=<id> <message> <key>=<value> <key>=<value> ...
//! ```
//!
//! The trailing newline is part of the record contract — every
//! external log scraper RustOS ships (`tools/qemu`'s serial capture,
//! the audit-on-`BootCompleted` observer used by the QEMU integration
//! test) splits on `'\n'`.
//!
//! # Lifetime
//!
//! The Sink is a zero-sized type and is exposed through the
//! [`SERIAL_SINK`] `'static` so each bin can hand the same reference
//! to `BootInfo`'s `log_sink` / `audit_sink` slots without taking a
//! mutable reference. Re-initialisation of the UART per call is
//! idempotent on the 16550 — the divisor and line-control bits are
//! sticky — so the cost is a handful of `outb`s and the overhead is
//! deliberate: it removes one shared mutable static from the bin's
//! surface (`AGENTS.md` §2 — *"No global mutable static beyond the
//! per-CPU bootstrap area"*).

use core::fmt::Write as _;

use rustos_arch_x86_64::serial::{self, Serial, COM1_BASE};
use rustos_log::{Event, Level, Sink};

/// `Sink` implementation that emits one formatted line per event to
/// the 16550 UART at [`COM1_BASE`].
///
/// Zero-sized; safe to share through a `&'static` reference because
/// every call constructs a fresh [`Serial`] handle (the UART itself is
/// the shared mutable state, not the wrapper).
#[derive(Debug)]
pub struct SerialSink;

impl SerialSink {
    /// Construct a Sink. `const` so a binary can place the handle in
    /// a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for SerialSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for SerialSink {
    fn write_event(&self, event: &Event<'_>) {
        // Re-`init` per call is idempotent on the 16550 (the divisor
        // and line-control bits are sticky). Avoids needing a `static
        // mut` for the writer.
        let mut s = Serial::init(COM1_BASE);
        // Ignore write errors: `Serial`'s `core::fmt::Write` impl is
        // infallible in practice (it busy-polls the line-status
        // register). Logging path must not panic per AGENTS.md §2.9.
        let _ = write!(
            s,
            "[{}] id={} {}",
            level_str(event.level),
            event.id.0,
            event.message
        );
        for field in event.fields {
            let _ = write!(s, " {}={}", field.key, field.value);
        }
        let _ = writeln!(s);
    }
}

const fn level_str(level: Level) -> &'static str {
    match level {
        Level::Trace => "TRACE",
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
    }
}

/// Single `'static` [`SerialSink`] handle the bins install in
/// `BootInfo`'s `log_sink` / `audit_sink` slots. Zero-sized, so no
/// `.bss` or `.data` footprint.
pub static SERIAL_SINK: SerialSink = SerialSink::new();

// Re-export `serial` so the bin crates do not need to depend on the
// arch crate by name for the few items they touch (panic-handler
// fallback, COM1 base).
pub use serial::COM1_BASE as REEXPORT_COM1_BASE;
