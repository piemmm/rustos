//! Console-backed [`rustos_log::Sink`] for the freestanding aarch64 kernel.
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
//! # Video first, UART as the fallback (`plans/PI.md` P7b)
//!
//! Console output defaults to the **attached display**: when the
//! framebuffer boot console is configured ([`crate::video::is_active`])
//! every transmitted byte is rendered on screen through
//! [`crate::video::write_bytes`], and the UART is used only when no
//! video output exists (`AGENTS.md` §10 — the screen is the user-facing
//! console, the serial line is the last resort). Console *input* stays
//! on the UART either way (the display has no receive side).
//!
//! # Board-discovered base and model (`plans/PI.md` P2)
//!
//! The UART transmit path is **board-independent**: it reads the current
//! MMIO base and register layout from [`crate::console`] on every byte.
//! The QEMU `virt` board's PrimeCell PL011 is the pre-discovery default;
//! a board whose console lives elsewhere (the Raspberry Pi — a PL011 or a
//! BCM2835 AUX mini-UART at the SoC's high peripheral window) overrides
//! both by calling [`crate::console::configure_from_fdt`] early in boot.
//! There is one console abstraction with two register backends, not two
//! consoles (`AGENTS.md` §2.2).
//!
//! The sink is a zero-sized type exposed through the [`SERIAL_SINK`]
//! `'static` so a bin can hand the same reference to `BootInfo`'s
//! `log_sink` / `audit_sink` slots without a mutable static (`AGENTS.md`
//! §2 — no global mutable state beyond the per-CPU bootstrap area). The
//! underlying shared mutable state is the UART itself plus the console
//! base/model cell in [`crate::console`], not this wrapper.

use core::fmt::Write as _;

use rustos_log::{Event, Level, Sink};

use crate::console;

/// Transmit one byte through the currently-configured console UART,
/// busy-waiting while the transmitter cannot yet accept it.
///
/// Freestanding-only: the MMIO access is meaningful solely on the target.
/// The host build omits it (the host tests cover the `Sink` formatting
/// through a capturing writer and the register helpers in
/// [`crate::console`] instead).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn putchar(byte: u8) {
    let (base, model) = console::current();
    let status_reg = (base + model.status_offset()) as *const u32;
    let data_reg = (base + model.data_offset()) as *mut u32;
    // SAFETY: `base` is the console UART's MMIO base — the `virt` PL011
    // default or the value discovered from the firmware device tree
    // (`console::configure_from_fdt`). The reads/writes are
    // naturally-aligned 32-bit accesses to device registers at the
    // model's documented offsets and touch no Rust-managed memory.
    unsafe {
        while !model.tx_ready(core::ptr::read_volatile(status_reg)) {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile(data_reg, u32::from(byte));
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn putchar(_byte: u8) {}

/// Read one byte from the currently-configured console UART **without
/// blocking**, returning `None` when the receive FIFO holds no byte.
///
/// This is the non-blocking counterpart of [`putchar`]: it polls the
/// model's receive-ready bit once and, if a byte is waiting, reads the
/// data register. It never busy-waits for input (`AGENTS.md` §2.1) — the
/// caller drains what is available and returns, so a `stream_read` with
/// no pending input is a valid short (zero-length) read rather than a
/// spin.
///
/// Freestanding-only: the host build returns `None` (no device), so the
/// consuming [`read_console_bytes`] reports a zero-length read there.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn getchar() -> Option<u8> {
    let (base, model) = console::current();
    let status_reg = (base + model.status_offset()) as *const u32;
    let data_reg = (base + model.data_offset()) as *const u32;
    // SAFETY: `base` is the console UART's MMIO base — the `virt` PL011
    // default or the value discovered from the firmware device tree
    // (`console::configure_from_fdt`). The reads are naturally-aligned
    // 32-bit accesses to device registers at the model's documented
    // offsets and touch no Rust-managed memory. The data register is read
    // only after `rx_ready` confirms a byte is present, so it never pops
    // an empty receive FIFO.
    unsafe {
        if !model.rx_ready(core::ptr::read_volatile(status_reg)) {
            return None;
        }
        // The received byte is in the low 8 bits of the data register;
        // the upper bits carry framing/parity error flags this bootstrap
        // backing does not surface.
        Some((core::ptr::read_volatile(data_reg) & 0xff) as u8)
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn getchar() -> Option<u8> {
    None
}

/// Fill `buf` with whatever console input is **immediately available**,
/// returning the number of bytes read (`0..=buf.len()`).
///
/// Non-blocking: it drains the receive FIFO into `buf` and stops at the
/// first byte that is not yet available (or when `buf` is full), so it
/// never busy-waits for input (`AGENTS.md` §2.1). A read with no pending
/// input returns `0` — a valid short read the `stream_read` handler
/// reports to the caller, which loops. This is the device-side **backing**
/// the stream layer attaches to fd 0 (`plans/PI.md` P6e-2 / `AGENTS.md`
/// §20); it is not a program-facing interface.
///
/// Freestanding-only receive (the host build's [`getchar`] yields
/// `None`), so the host tests of the consuming `ConsoleRead` adapter
/// observe a zero-length read without touching MMIO.
pub fn read_console_bytes(buf: &mut [u8]) -> usize {
    let mut read = 0;
    for slot in buf.iter_mut() {
        match getchar() {
            Some(byte) => {
                *slot = byte;
                read += 1;
            }
            None => break,
        }
    }
    read
}

/// Write `bytes` verbatim to the current console — the video console
/// when one is configured, else the configured UART — returning the
/// number written (always `bytes.len()`; both backings accept every
/// byte).
///
/// Unlike [`ConsoleWriter`] this performs **no** `\n` → `\r\n`
/// translation: it is the raw byte sink the `stream_write` syscall
/// (`abi-v1` number 11) emits a user program's output through, so the
/// bytes reach the device exactly as the program wrote them
/// (`plans/PI.md` P6c-2, `AGENTS.md` §10 / §16.4). The downstream boot
/// pipeline wraps this in a `kernel_core::ConsoleWrite` device and
/// installs it on `BootInfo`.
///
/// Freestanding-only transmit (the host build's [`putchar`] is inert),
/// so the host tests of the consuming `ConsoleWrite` adapter observe the
/// byte count without touching MMIO.
pub fn write_console_bytes(bytes: &[u8]) -> usize {
    if crate::video::is_active() {
        crate::video::write_bytes(bytes);
    } else {
        for &byte in bytes {
            putchar(byte);
        }
    }
    bytes.len()
}

/// [`core::fmt::Write`] adapter that emits each byte through the
/// current console: the video console when configured (its renderer
/// interprets `\n` itself), else the UART with `\n` → `\r\n`
/// translation so terminals capturing `-serial stdio` render the boot
/// log with proper line breaks.
pub struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if crate::video::is_active() {
            crate::video::write_bytes(s.as_bytes());
            return Ok(());
        }
        for byte in s.bytes() {
            if byte == b'\n' {
                putchar(b'\r');
            }
            putchar(byte);
        }
        Ok(())
    }
}

/// `Sink` that emits one formatted line per event through the console.
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
        let mut w = ConsoleWriter;
        // Ignore write errors: `ConsoleWriter` is infallible (the MMIO
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
