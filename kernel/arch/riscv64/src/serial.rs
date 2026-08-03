//! This port's console: the firmware debug console, as the device half of the
//! shared console-output engine.
//!
//! # What lives here and what does not
//!
//! Everything about *queueing* console output — whole-line framing, ordering,
//! shedding the least important line under pressure, counting and reporting
//! what was dropped, and the one drain — is
//! [`tairix_conout`], shared by every port. What lives here is
//! only what this architecture makes specific: output goes out through a
//! firmware call ([`crate::sbi`]) rather than a device register, so this port
//! never names a UART at all and works on any board whose firmware implements
//! the interface.
//!
//! # How output reaches the wire
//!
//! This console drains **write-through**: a producer's line reaches the
//! firmware before the call returns. The firmware interface offers neither a
//! readiness status nor a completion interrupt — a call simply returns once the
//! byte is accepted — so there is nothing to defer a backlog to, and nothing
//! that could report a transmitter as busy. That makes the port's transmit wait
//! *firmware-side* and outside this kernel's control: unlike the ports that
//! poll a status register, a console wedged below the firmware boundary cannot
//! be detected or bounded here.
//!
//! The queue is what makes output *correct*: a line is admitted whole under the
//! console gate, so two harts logging at once can no longer interleave their
//! bytes mid-line, and anything that cannot be carried is counted and reported
//! rather than silently dropped.

use tairix_conout::{ConsoleGate, ConsoleTx, DEFAULT_CAPACITY_BYTES};
use tairix_log::{Event, Sink};

use crate::irqmask::PortIrqControl;
use crate::sbi;

/// This port's firmware console, as the shared engine's transmitter.
struct SbiTx;

impl ConsoleTx for SbiTx {
    /// The firmware console raises no completion interrupt, so a backlog is
    /// drained by the producer rather than by an interrupt.
    const COMPLETION_INTERRUPT: bool = false;

    fn uptime_ms(&self) -> Option<u64> {
        // No monotonic-uptime seam is wired on this port yet, so a record
        // carries no stamp rather than a fabricated one.
        None
    }

    fn send_ready(&self, _bytes: &[u8]) -> usize {
        // The firmware interface cannot report whether the console will accept
        // a byte without waiting, so nothing can honestly be sent on a path
        // that must not wait. Reporting zero here is what keeps the
        // never-waiting callers — an interrupt handler, a hot path — from
        // blocking inside firmware; the producer's own drain carries the bytes.
        0
    }

    fn send_bounded(&self, bytes: &[u8]) -> usize {
        for &byte in bytes {
            sbi::console_putchar(byte);
        }
        bytes.len()
    }

    fn send_bypass(&self, byte: u8) -> bool {
        sbi::console_putchar(byte);
        true
    }

    fn set_completion_interrupt(&self, _on: bool) {
        // Never reached: this port reports no completion interrupt.
    }
}

/// The console every producer on this port writes through: the log sink and the
/// `stream_write` backing alike, so both share one ordered stream on the wire.
static CONSOLE: ConsoleGate<SbiTx, PortIrqControl, DEFAULT_CAPACITY_BYTES> =
    ConsoleGate::new(SbiTx);

/// [`core::fmt::Write`] adapter that emits each byte straight through the
/// firmware console, translating a bare newline so a captured line renders as a
/// line break.
///
/// The direct path, for the panic banner: a panicking hart is about to halt, so
/// its record must reach the wire now rather than through a queue whose holder
/// may be the very hart that died.
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

/// Queue `bytes` of a program's own output for the console, returning how many
/// were accepted.
///
/// The bytes are passed through **verbatim** — no line-ending translation —
/// because this is the raw sink a program's `stream_write` reaches: what the
/// program wrote is what the console receives.
///
/// A short count is possible and honest when the console is far behind; the
/// caller retries, exactly as it would on a pipe. It is never zero while the
/// console can still carry a byte.
#[must_use]
pub fn write_console_bytes(bytes: &[u8]) -> usize {
    CONSOLE.write_output(bytes)
}

/// The log sink: one formatted line per event, on the console.
///
/// Zero-sized, so a binary can hand the same `'static` reference to both the
/// log and audit slots.
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
        CONSOLE.write_event(event);
    }
}

/// The single `'static` sink handle the kernel binary installs as the log and
/// audit sink.
pub static SERIAL_SINK: SerialSink = SerialSink::new();

#[cfg(all(test, not(all(target_arch = "riscv64", target_os = "none"))))]
mod tests {
    use core::fmt::Write as _;

    use tairix_log::{Event, EventId, Level, Sink as _};

    use super::{write_console_bytes, SbiWriter, SERIAL_SINK};

    #[test]
    fn program_output_is_accepted_for_the_shared_queue() {
        // The host build has no firmware, so this proves the wiring and the
        // queue's acceptance, not the transmission.
        assert_eq!(
            write_console_bytes(b"program bytes"),
            b"program bytes".len()
        );
    }

    #[test]
    fn an_empty_write_is_accepted_as_nothing() {
        assert_eq!(write_console_bytes(b""), 0);
    }

    #[test]
    fn the_sink_routes_a_record_into_the_shared_queue() {
        SERIAL_SINK.write_event(&Event {
            level: Level::Info,
            id: EventId(4_242),
            message: "console line",
            fields: &[],
        });
    }

    #[test]
    fn the_direct_writer_accepts_a_line_without_firmware() {
        SbiWriter
            .write_str("direct line\n")
            .expect("the direct writer is infallible");
    }
}
