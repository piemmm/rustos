//! SBI-console-backed [`rustos_log::Sink`] for the freestanding riscv64
//! kernel.
//!
//! Mirrors the x86_64 COM1 sink (`rustos_kernel::x86_64::serial_sink`): one
//! formatted line per event, in the canonical format the `kernel/core`
//! audit consumers and the QEMU serial scraper expect —
//!
//! ```text
//! [<level>] id=<id> <message> <key>=<value> ...
//! ```
//!
//! The sink is a zero-sized type exposed through the [`SERIAL_SINK`]
//! `'static` so a bin can hand the same reference to `BootInfo`'s
//! `log_sink` / `audit_sink` slots without a mutable static
//! (no global mutable state beyond the per-CPU
//! bootstrap area). The underlying shared mutable state is the UART
//! behind the SBI console, not this wrapper.

use core::fmt::Write as _;

use rustos_log::{Event, Level, Sink};

use crate::sbi;

/// [`core::fmt::Write`] adapter that emits each byte through the SBI
/// console, translating `\n` into `\r\n` so terminals capturing
/// `-serial stdio` render the boot log with proper line breaks.
pub struct SbiWriter;

impl core::fmt::Write for SbiWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                sbi::console_putchar(b'\r');
            }
            sbi::console_putchar(byte);
        }
        Ok(())
    }
}

/// Write `bytes` verbatim to the SBI console, returning the number of
/// bytes accepted (always `bytes.len()`).
///
/// Unlike [`SbiWriter`] — which translates `\n` into `\r\n` so the boot
/// *log* renders with proper line breaks — this performs **no**
/// translation: each byte reaches the device exactly as the caller
/// supplied it. It is the verbatim backing the bin crate's system
/// console device (`stream_write`, fd 1) forwards through, so a program's
/// output is byte-for-byte what it wrote.
///
/// The legacy `console_putchar` SBI call reports no status and the
/// busy-wait transmit accepts every byte, so the write is total and
/// never short, and the path cannot panic.
#[must_use]
pub fn write_console_bytes(bytes: &[u8]) -> usize {
    for &byte in bytes {
        sbi::console_putchar(byte);
    }
    bytes.len()
}

/// `Sink` that emits one formatted line per event through the SBI
/// console.
#[derive(Debug)]
pub struct SerialSink;

impl SerialSink {
    /// Construct a sink. `const` so a binary can place it in a `static`.
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
        let mut w = SbiWriter;
        // Ignore write errors: `SbiWriter` is infallible (the SBI
        // console call returns no status). The logging path must not
        // panic.
        let _ = write!(
            w,
            "[{}] id={} {}",
            level_str(event.level),
            event.id.0,
            event.message
        );
        for field in event.fields {
            let _ = write!(w, " {}={}", field.key, field.value);
        }
        let _ = writeln!(w);
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

/// Single `'static` [`SerialSink`] handle the bin installs in
/// `BootInfo`'s `log_sink` / `audit_sink` slots. Zero-sized, so no
/// `.bss` or `.data` footprint.
pub static SERIAL_SINK: SerialSink = SerialSink::new();
