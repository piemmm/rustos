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
//! # Two consoles: the display and the UART (`plans/PI.md` P7b / P11)
//!
//! The **boot-log** path ([`ConsoleWriter`], and through it
//! [`SerialSink`]) routes by build profile (`AGENTS.md` §10 — the
//! screen is the user-facing console, the serial line is the
//! diagnostic one):
//!
//! - A **release build** renders every log line on the attached
//!   display when the framebuffer boot console is configured
//!   ([`crate::video::is_active`]) and carries it on the UART only
//!   when no video output exists.
//! - A **debug build** (`cfg(debug_assertions)`) routes the whole
//!   log/debug stream to the **UART instead** — even while a login
//!   session owns the UART — so a serial capture of a development boot
//!   carries the full diagnostic stream and the screen stays clear for
//!   the user-facing session. With no UART discovered the bounded
//!   transmit simply drops the bytes; the screen is never the debug
//!   log's sink.
//!
//! "Debug build" here is the Cargo `dev` profile, and it lines up with
//! the **image** profile because `tools/xtask` compiles the kernel in
//! the matching profile: the non-shippable `debug` SD image is built
//! from a `dev`-profile (`debug_assertions`-on) kernel, while the
//! shippable `installer` image is built `--release` (assertions off, log
//! on screen). The single release kernel cannot observe the image it is
//! planted in, so this build-profile pairing is what makes the routing
//! above match the operator's `--profile` choice (`tools/xtask`
//! `kernel_build_profile`).
//!
//! The **stream** path is different: the video console and the UART are
//! two *separate* `abi-v1` stream backings with their own session
//! contexts (`plans/PI.md` P11 — one login each). [`write_console_bytes`]
//! / [`read_console_bytes`] are the **UART console's** halves and never
//! touch the display; the video console's write half goes straight to
//! [`crate::video::write_bytes`] in the kernel binary's console device,
//! and its input comes from a keyboard source, never the UART.
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
use crate::console::{tx_wait, TxOutcome, TX_POLL_BUDGET};

/// Whether the console transmitter was declared wedged by a budget
/// expiry ([`tx_wait`]). While set, each byte costs a single readiness
/// poll (dropped if not ready) instead of a full budget, so a
/// dead/flow-blocked UART cannot crawl the boot; the first poll that
/// finds the FIFO draining clears it and transmission resumes.
static TX_WEDGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Transmit one byte through the currently-configured console UART,
/// waiting **boundedly** while the transmitter cannot yet accept it
/// ([`tx_wait`]): a transmitter that never drains is declared wedged
/// and the byte dropped rather than hanging the kernel (`AGENTS.md`
/// §2.1 — on the Pi 4 a flow-blocked, BT-attached PL011 never drains).
///
/// Freestanding-only: the MMIO access is meaningful solely on the target.
/// The host build omits it (the host tests cover the `Sink` formatting
/// through a capturing writer and the register/[`tx_wait`] helpers in
/// [`crate::console`] instead).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn putchar(byte: u8) {
    use core::sync::atomic::Ordering;

    let (base, model) = console::current();
    let status_reg = (base + model.status_offset()) as *const u32;
    let data_reg = (base + model.data_offset()) as *mut u32;
    let wedged = TX_WEDGED.load(Ordering::Relaxed);
    // SAFETY: `base` is the console UART's MMIO base — the `virt` PL011
    // default or the value discovered from the firmware device tree
    // (`console::configure_from_fdt`). The reads/writes are
    // naturally-aligned 32-bit accesses to device registers at the
    // model's documented offsets and touch no Rust-managed memory.
    let (outcome, now_wedged) = tx_wait(
        || unsafe { model.tx_ready(core::ptr::read_volatile(status_reg)) },
        wedged,
        TX_POLL_BUDGET,
    );
    if now_wedged != wedged {
        TX_WEDGED.store(now_wedged, Ordering::Relaxed);
    }
    if outcome == TxOutcome::Send {
        // SAFETY: as above — a naturally-aligned 32-bit store to the
        // model's documented data register, confirmed ready by `tx_wait`.
        unsafe {
            core::ptr::write_volatile(data_reg, u32::from(byte));
        }
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
/// caller drains what is available and returns; waiting for input is the
/// stream layer's job, not the device's (kernel-core's
/// `BlockingConsoleRead` parks an empty-handed `stream_read` caller on
/// the scheduler, `AGENTS.md` §20).
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
/// input returns `0` — a valid short read kernel-core's
/// `BlockingConsoleRead` turns into a scheduler park, re-polling when the
/// caller is next dispatched, so user space only ever sees a read with
/// bytes (`AGENTS.md` §20 — the backing owns blocking). This is the
/// device-side **backing** the stream layer attaches to fd 0
/// (`plans/PI.md` P6e-2 / `AGENTS.md` §20); it is not a program-facing
/// interface.
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

/// Write `bytes` verbatim to the configured **UART**, returning the
/// number written (always `bytes.len()`; the bounded transmit accepts
/// or drops every byte).
///
/// Unlike [`ConsoleWriter`] this performs **no** `\n` → `\r\n`
/// translation: it is the raw byte sink the `stream_write` syscall
/// (`abi-v1` number 11) emits a user program's output through, so the
/// bytes reach the device exactly as the program wrote them
/// (`plans/PI.md` P6c-2, `AGENTS.md` §10 / §16.4). It deliberately
/// never routes to the video console: the UART is its own stream
/// backing with its own login session (`plans/PI.md` P11), so a
/// program attached to the UART console writes the serial line and
/// nothing else. The downstream boot pipeline wraps this in a
/// `kernel_core::ConsoleWrite` device and installs it on `BootInfo`.
///
/// Freestanding-only transmit (the host build's [`putchar`] is inert),
/// so the host tests of the consuming `ConsoleWrite` adapter observe the
/// byte count without touching MMIO.
pub fn write_console_bytes(bytes: &[u8]) -> usize {
    for &byte in bytes {
        putchar(byte);
    }
    bytes.len()
}

/// Emit a short, ordered boot **beacon** — `tag` followed by `\r\n` — so a
/// boot that wedges *before* the consolidated boot-log line
/// (`KERNEL_BOOT_AARCH64_REACHED`, emitted only after the MMU is on) still
/// leaves a trail on the serial line whose last printed tag localises the
/// hang.
///
/// The beacon writes the **UART only** ([`putchar`] — a plain volatile
/// MMIO store guarded by a `Relaxed` atomic, with no exclusive-monitor
/// access), so it is safe to call with the MMU still off, exactly like the
/// early FDT-discovery path. It performs no allocation and deliberately
/// never touches the video console: the video renderer takes a render lock
/// whose compare-exchange is UNPREDICTABLE on the MMU-off memory the boot
/// CPU runs on (the same reason the allocator and scheduler wait for the
/// MMU, `plans/PI.md` P6c-2), so a screen mirror could itself wedge the
/// very boot a beacon exists to trace. The serial line is the single,
/// always-safe bisection channel (`AGENTS.md` §2.9 — never hang).
///
/// Freestanding-only (the whole module is gated to the bare-metal
/// target): the transmit is meaningful solely on the Pi's UART, so like
/// [`write_console_bytes`] it carries no host unit test.
pub fn beacon(tag: &str) {
    write_console_bytes(tag.as_bytes());
    write_console_bytes(b"\r\n");
}

/// [`core::fmt::Write`] adapter for the **log/debug** line path that
/// routes by build profile. A release build emits each byte through the
/// user-facing console — the video console when configured (its
/// renderer interprets `\n` itself), else the UART. A debug build
/// (`cfg(debug_assertions)`) sends the whole stream to the **UART
/// instead**, so a serial capture of a development boot carries the
/// full diagnostic stream while the screen stays clear for the session.
///
/// UART bytes get `\n` → `\r\n` translation so terminals capturing the
/// serial line render the boot log with proper line breaks; the video
/// renderer needs no such translation.
pub struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // Debug builds divert the log/debug stream to the UART (never
        // the screen); release builds render it on the display when one
        // is configured and fall back to the UART otherwise.
        if !cfg!(debug_assertions) && crate::video::is_active() {
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
