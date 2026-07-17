//! `console.log`-backed [`tairix_log::Sink`] for the wasm32 kernel.
//!
//! Mirrors the bare-metal ports' serial sinks (e.g.
//! `tairix_arch_riscv64::serial`): one formatted line per event, in the
//! one shared diagnostic format ([`tairix_log::write_diag_line`]) the
//! `kernel/core` audit consumers and the browser harness scraper expect —
//!
//! ```text
//! [<LEVEL>] id=<id> <message> <key>=<value> ...
//! ```
//!
//! Each formatted chunk is handed straight to the host
//! [`crate::bindings::host_console_write`] as a borrowed byte slice, so
//! the sink needs no allocator and no intermediate buffer (`tairix_log` is a no-alloc hot path). The sink is a zero-sized
//! type exposed through the [`CONSOLE_SINK`] `'static` so a boot module
//! can hand the same reference to `BootInfo`'s `log_sink` / `audit_sink`
//! slots without a mutable static.

use core::fmt::Write as _;

use tairix_log::{Event, Sink};

/// [`core::fmt::Write`] adapter that forwards each formatted chunk to the
/// host `console.log` import. The host decodes the UTF-8 bytes and joins
/// the chunks of one event into a single console line.
struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        crate::bindings::host_console_write(s.as_bytes());
        Ok(())
    }
}

/// `Sink` that emits one formatted line per event to the host console.
#[derive(Debug)]
pub struct ConsoleSink;

impl ConsoleSink {
    /// Construct a sink. `const` so a module can place it in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for ConsoleSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for ConsoleSink {
    fn write_event(&self, event: &Event<'_>) {
        // The host console prints ANSI escapes literally rather than
        // rendering them, so the level tag stays uncoloured; no
        // monotonic-uptime seam is wired on this port, so the stamp is
        // omitted.
        tairix_log::write_diag_line(&mut ConsoleWriter, None, false, event);
    }
}

/// Single `'static` [`ConsoleSink`] handle a boot module installs in
/// `BootInfo`'s `log_sink` / `audit_sink` slots. Zero-sized, so no
/// `.data` footprint.
pub static CONSOLE_SINK: ConsoleSink = ConsoleSink::new();

/// Emit one line of plain text to the host console (no event framing).
///
/// Used by the panic bridge and the boot trampoline for human-readable
/// status the browser harness scrapes.
pub fn write_line(text: &str) {
    let mut w = ConsoleWriter;
    let _ = w.write_str(text);
    let _ = w.write_str("\n");
}
