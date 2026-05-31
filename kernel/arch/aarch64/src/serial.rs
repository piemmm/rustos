//! PL011-UART-backed [`rustos_log::Sink`] for the freestanding aarch64
//! kernel.
//!
//! Mirrors the riscv64 SBI-console sink (`kernel/arch/riscv64::serial`)
//! and the x86_64 COM1 sink: one formatted line per event, in the
//! canonical format the `kernel/core` audit consumers and the QEMU
//! serial scraper expect —
//!
//! ```text
//! [<level>] id=<id> <message> <key>=<value> ...
//! ```
//!
//! The QEMU `virt` board exposes a PrimeCell PL011 UART (ARM DDI 0183)
//! at [`PL011_BASE`], which QEMU wires to `-serial stdio`. The sink is a
//! zero-sized type exposed through the [`SERIAL_SINK`] `'static` so a bin
//! can hand the same reference to `BootInfo`'s `log_sink` / `audit_sink`
//! slots without a mutable static (`AGENTS.md` §2 — no global mutable
//! state beyond the per-CPU bootstrap area). The underlying shared
//! mutable state is the UART itself, not this wrapper.

use core::fmt::Write as _;

use rustos_log::{Event, Level, Sink};

/// MMIO base address of the QEMU `virt` board's PL011 UART. Fixed by the
/// `virt` memory map; the host runner wires it to `-serial stdio`.
pub const PL011_BASE: usize = 0x0900_0000;

/// Byte offset of the data register (`UARTDR`): a write transmits the
/// low byte (ARM PrimeCell PL011 TRM §3.3.1).
const UARTDR: usize = 0x00;

/// Byte offset of the flag register (`UARTFR`).
const UARTFR: usize = 0x18;

/// `UARTFR.TXFF` — transmit FIFO full (bit 5). Polled before each store
/// so a write never drops a byte into a full FIFO.
const UARTFR_TXFF: u32 = 1 << 5;

/// Transmit one byte through the PL011, busy-waiting while the transmit
/// FIFO is full.
///
/// Freestanding-only: the MMIO access is meaningful solely on the
/// `virt` board. The host build omits it (the host tests cover the
/// `Sink` formatting through a capturing writer instead).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn putchar(byte: u8) {
    // SAFETY: `PL011_BASE` is the fixed MMIO address of the `virt`
    // board's PL011 UART. The reads/writes are naturally-aligned 32-bit
    // accesses to device registers and touch no Rust-managed memory.
    unsafe {
        let fr = (PL011_BASE + UARTFR) as *const u32;
        while (core::ptr::read_volatile(fr) & UARTFR_TXFF) != 0 {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile((PL011_BASE + UARTDR) as *mut u32, u32::from(byte));
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn putchar(_byte: u8) {}

/// [`core::fmt::Write`] adapter that emits each byte through the PL011,
/// translating `\n` into `\r\n` so terminals capturing `-serial stdio`
/// render the boot log with proper line breaks.
pub struct Pl011Writer;

impl core::fmt::Write for Pl011Writer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                putchar(b'\r');
            }
            putchar(byte);
        }
        Ok(())
    }
}

/// `Sink` that emits one formatted line per event through the PL011.
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
        let mut w = Pl011Writer;
        // Ignore write errors: `Pl011Writer` is infallible (the MMIO
        // store returns no status). The logging path must not panic
        // (`AGENTS.md` §2.9).
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
