//! This port's console: the UART device half of the shared console-output
//! engine, plus the receive path and the boot beacon.
//!
//! # What lives here and what does not
//!
//! Everything about *queueing* console output — whole-line framing, ordering,
//! shedding the least important line under pressure, counting and reporting
//! what was dropped, and the one bounded drain — is
//! [`tairix_conout`], shared by every port. What lives here is
//! only what the silicon makes specific to this architecture: which registers
//! carry a byte, how readiness is reported, and how the device's interrupt is
//! armed, masked and identified.
//!
//! The console model itself ([`crate::console`]) is discovered from the
//! firmware device tree at boot, so this file names no board: a PL011 and a
//! mini-UART differ in their register offsets and bit positions, which the
//! model supplies as data.
//!
//! # How output reaches the wire
//!
//! A producer — a log record or a program's `stream_write` — copies its bytes
//! into the shared queue and returns. It never spins at the UART's byte rate,
//! because a transmitter carries a few thousand bytes a second while the CPU
//! issues millions of instructions in the same time; a producer that waited
//! would stall whatever it was doing (the keyboard report pump, a driver
//! bring-up) for the duration.
//!
//! Two things move the queued bytes to the device, and both are non-blocking:
//!
//! 1. **The transmit interrupt.** As the FIFO drains, the device raises its
//!    interrupt and the handler tops the FIFO back up
//!    ([`service_uart_tx_irq`]), so output flows at the line's real rate with
//!    no CPU spent waiting. The trigger is lowered to fire the moment the FIFO
//!    runs dry ([`enable_tx_interrupt`]), because a device whose transmitter is
//!    held off by flow control never crosses a half-full threshold on a small
//!    drain and would leave the interrupt un-asserted forever.
//! 2. **The dispatch loop**, which tops the FIFO up on every iteration and
//!    before it parks ([`pump_tx`]). This keeps output moving even while a
//!    perpetually-runnable in-kernel task keeps the loop off its idle branch,
//!    and it arms the interrupt before the CPU parks so the interrupt is what
//!    wakes it to drain the rest.
//!
//! The single interrupt line carries both directions, so the handler reads the
//! *masked* interrupt status once and services only the direction that
//! actually fired. That is what keeps it from stealing receive bytes a
//! passphrase prompt is polling for while the receive interrupt is masked.
//!
//! # Why the beacon bypasses all of it
//!
//! [`beacon`] writes the device directly, byte by byte. It runs before the MMU
//! is on, where the queue's lock cannot be used at all (an atomic
//! compare-exchange is architecturally UNPREDICTABLE without the MMU, the same
//! reason the allocator and scheduler wait for it), and its whole purpose is to
//! localise a hang *immediately* — deferring it into a queue that may never
//! drain would defeat it. It never touches the framebuffer either, whose
//! renderer has the same pre-MMU hazard: the serial line is the one always-safe
//! bisection channel.

use tairix_conout::{ConsoleGate, ConsoleTx, DEFAULT_CAPACITY_BYTES};
use tairix_log::{Event, Sink};

use crate::irqmask::PortIrqControl;

// The console MMIO seam is reached only by the freestanding device primitives
// below; the host build has no device and uses inert stubs, so importing it
// there would be an unused import. The module is host-compiled because the
// sink's routing and the read path are host-testable.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
use tairix_conout::{tx_wait, TxOutcome, TX_POLL_BUDGET};

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
use crate::console;

/// This port's UART, as the shared engine's transmitter.
///
/// Zero-sized: every operation reads the discovered console model, so a
/// device-tree console switch needs no state here.
struct UartTx;

impl ConsoleTx for UartTx {
    /// The device raises an interrupt as its transmit FIFO drains, so a
    /// backlog is drained by that interrupt rather than by waiting.
    const COMPLETION_INTERRUPT: bool = true;

    fn uptime_ms(&self) -> Option<u64> {
        Some(crate::kernel_arch::uptime_ms())
    }

    fn send_ready(&self, bytes: &[u8]) -> usize {
        let mut sent = 0;
        for &byte in bytes {
            if !tx_ready_now() {
                break;
            }
            tx_send(byte);
            sent += 1;
        }
        sent
    }

    fn send_bounded(&self, bytes: &[u8]) -> usize {
        let mut sent = 0;
        for &byte in bytes {
            if !tx_send_bounded(byte) {
                break;
            }
            sent += 1;
        }
        sent
    }

    fn send_bypass(&self, byte: u8) -> bool {
        tx_send_bounded(byte)
    }

    fn set_completion_interrupt(&self, on: bool) {
        if on {
            enable_tx_interrupt();
        } else {
            disable_tx_interrupt();
        }
    }
}

/// The console every producer on this port writes through: the log sink, the
/// `stream_write` backing, and the panic bridge alike, so all three share one
/// ordered stream on the wire.
static CONSOLE: ConsoleGate<UartTx, PortIrqControl, DEFAULT_CAPACITY_BYTES> =
    ConsoleGate::new(UartTx);

/// Whether the transmitter was last found not to be draining
/// ([`tairix_conout::tx_wait`]). While set, a byte costs a single readiness
/// poll instead of a full budget, so a dead or flow-blocked device cannot
/// crawl the boot; the first poll that finds it draining clears it.
///
/// Freestanding-only: it tracks the state of real MMIO, and the host build's
/// transmit is inert.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
static TX_WEDGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether the transmitter can accept a byte **right now**, polled exactly
/// once — no spin, no budget.
///
/// Freestanding-only; the host build reports "not ready", so a host drain is a
/// no-op.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn tx_ready_now() -> bool {
    let (base, model) = console::current();
    let status_reg = (base + model.status_offset()) as *const u32;
    // SAFETY: `base` is the discovered console UART's MMIO base and
    // `status_offset()` the model's documented status register; a
    // naturally-aligned 32-bit volatile read of a device register that touches
    // no Rust-managed memory.
    unsafe { model.tx_ready(core::ptr::read_volatile(status_reg)) }
}

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn tx_ready_now() -> bool {
    false
}

/// Write one byte to a transmitter the caller has **already confirmed ready**
/// with [`tx_ready_now`]. Correct only immediately after a true readiness
/// poll, since it performs none itself.
///
/// Freestanding-only; inert on the host build.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn tx_send(byte: u8) {
    let (base, model) = console::current();
    let data_reg = (base + model.data_offset()) as *mut u32;
    // SAFETY: `base` is the discovered console UART's MMIO base and
    // `data_offset()` the model's documented data register; a naturally-aligned
    // 32-bit volatile store to a device register that touches no Rust-managed
    // memory. The caller confirmed readiness via `tx_ready_now`.
    unsafe {
        core::ptr::write_volatile(data_reg, u32::from(byte));
    }
}

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn tx_send(_byte: u8) {}

/// Transmit one byte, waiting **boundedly** for the transmitter to accept it,
/// and report whether it went out.
///
/// A transmitter that never becomes ready is declared wedged and the byte
/// dropped rather than hanging the kernel — a real case, not a theoretical
/// one: a UART wired to a flow-controlled peer that never asserts ready would
/// otherwise stall the machine on its first log line.
///
/// Freestanding-only; the host build reports the byte sent, so a host drain
/// terminates rather than looping on a device that does not exist.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn tx_send_bounded(byte: u8) -> bool {
    use core::sync::atomic::Ordering;

    let wedged = TX_WEDGED.load(Ordering::Relaxed);
    let (outcome, now_wedged) = tx_wait(tx_ready_now, wedged, TX_POLL_BUDGET);
    if now_wedged != wedged {
        TX_WEDGED.store(now_wedged, Ordering::Relaxed);
    }
    if outcome == TxOutcome::Drop {
        return false;
    }
    tx_send(byte);
    true
}

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn tx_send_bounded(_byte: u8) -> bool {
    true
}

/// Move queued output to the device without waiting, and arm the transmit
/// interrupt for whatever the FIFO could not take.
///
/// Called by the dispatch loop after each dispatched task and again before it
/// parks (through the `KernelArch::pump_console_tx` seam), so a backlog drains
/// at the loop's rate as well as at the interrupt's, and the interrupt is
/// always armed before the CPU parks.
pub fn pump_tx() {
    CONSOLE.pump();
}

/// Drain queued output to the device, waiting for it, and return when it is
/// empty or the transmitter has stopped draining.
///
/// For the panic bridge alone: the buffered context that led to the failure
/// must reach a serial capture *before* the CPU parks, and a CPU that is about
/// to stop dispatching can no longer starve anything by waiting.
pub fn flush_serial_blocking() {
    CONSOLE.flush();
}

/// Format one event in the shared diagnostic line shape, stamped with uptime
/// and a coloured level tag (a serial capture and the framebuffer console both
/// render ANSI colour).
///
/// Used by the direct paths only ([`ConsoleWriter`]); the queued path renders
/// inside the shared engine, in the same shape.
fn write_formatted<W: core::fmt::Write>(w: &mut W, event: &Event<'_>) {
    tairix_log::write_diag_line(w, Some(crate::kernel_arch::uptime_ms()), true, event);
}

/// Read one byte from the console **without blocking**, or `None` when the
/// receive FIFO is empty.
///
/// It never waits for input: waiting belongs to the stream layer, which parks
/// the reading task on the scheduler rather than polling a device.
///
/// Freestanding-only; the host build has no device, so a host read is empty.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn getchar() -> Option<u8> {
    let (base, model) = console::current();
    let status_reg = (base + model.status_offset()) as *const u32;
    let data_reg = (base + model.data_offset()) as *const u32;
    // SAFETY: `base` is the discovered console UART's MMIO base and the
    // offsets are the model's documented registers, so both are
    // naturally-aligned 32-bit device registers and touch no Rust-managed
    // memory. The data register is read only after `rx_ready` confirms a byte
    // is present, so it never pops an empty receive FIFO.
    unsafe {
        if !model.rx_ready(core::ptr::read_volatile(status_reg)) {
            return None;
        }
        // The received byte is in the low 8 bits; the upper bits carry
        // framing/parity flags this bootstrap backing does not surface.
        Some((core::ptr::read_volatile(data_reg) & 0xff) as u8)
    }
}

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn getchar() -> Option<u8> {
    None
}

/// Switch the console from poll-only receive to **receive-interrupt-driven**,
/// so an arriving byte raises an interrupt.
///
/// The interactive session needs this: both the root-unlock prompt and `login`
/// park off the run queue between keystrokes, and this interrupt is what
/// drains the FIFO into the input queue and wakes the parked reader.
///
/// Freestanding-only; the register sequence itself is host-tested in
/// [`crate::console`].
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn enable_rx_interrupt() {
    let (base, model) = console::current();
    for step in model.rx_interrupt_sequence() {
        apply_reg_rmw(base, step);
    }
}

/// Apply one [`console::RegRmw`] to the live console MMIO:
/// `*reg = (*reg & !step.clear) | step.set`, skipping a no-op step. The one
/// register read-modify-write every interrupt enable/disable helper shares.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn apply_reg_rmw(base: usize, step: console::RegRmw) {
    if step.is_noop() {
        return;
    }
    let reg = (base + step.offset) as *mut u32;
    // SAFETY: `base` is the discovered console UART's MMIO base and
    // `step.offset` the model's documented register offset, so `reg` is a
    // naturally-aligned 32-bit device register. The read-modify-write
    // preserves every bit outside the step's masks and touches no
    // Rust-managed memory.
    unsafe {
        let cur = core::ptr::read_volatile(reg);
        core::ptr::write_volatile(reg, (cur & !step.clear) | step.set);
    }
}

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn enable_rx_interrupt() {}

/// Clear the console's latched receive / receive-timeout interrupt.
///
/// The receive handler calls this after draining the FIFO empty: on a PL011 the
/// receive-timeout latch does *not* clear merely by emptying the FIFO, so
/// without this write the line stays asserted and the handler re-fires forever,
/// starving every other task. A model whose latch clears on a data-register
/// read needs nothing here.
///
/// Freestanding-only; the clear policy is host-tested in [`crate::console`].
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn clear_rx_interrupt() {
    let (base, model) = console::current();
    if let Some((offset, value)) = model.rx_interrupt_clear() {
        let reg = (base + offset) as *mut u32;
        // SAFETY: `base` is the discovered console UART's MMIO base and
        // `offset` the model's documented write-1-to-clear register; a
        // naturally-aligned 32-bit store of the clear mask that touches no
        // Rust-managed memory.
        unsafe {
            core::ptr::write_volatile(reg, value);
        }
    }
}

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn clear_rx_interrupt() {}

/// Arm the transmit interrupt so the device reports its FIFO draining.
///
/// It first lowers the FIFO trigger to one-eighth full, so the interrupt fires
/// the moment the FIFO runs dry. Without that, a transmitter held off by flow
/// control never crosses the reset-default half-full threshold on a small drain
/// and the interrupt never re-asserts — output then stops entirely. It touches
/// no receive bits, so arming it is safe during the passphrase window where the
/// receive interrupt is deliberately masked. Idempotent.
///
/// Freestanding-only; the register policy is host-tested in
/// [`crate::console`].
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn enable_tx_interrupt() {
    let (base, model) = console::current();
    for step in model.tx_interrupt_enable() {
        apply_reg_rmw(base, step);
    }
}

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn enable_tx_interrupt() {}

/// Mask the transmit interrupt (and clear its latch where the model needs it),
/// so an empty FIFO does not re-fire the handler forever.
///
/// Freestanding-only.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn disable_tx_interrupt() {
    let (base, model) = console::current();
    apply_reg_rmw(base, model.tx_interrupt_disable());
    if let Some((offset, value)) = model.tx_interrupt_clear() {
        let reg = (base + offset) as *mut u32;
        // SAFETY: `base` is the discovered console UART's MMIO base and
        // `offset` the model's documented write-1-to-clear register; a
        // naturally-aligned 32-bit store of the clear mask that touches no
        // Rust-managed memory.
        unsafe {
            core::ptr::write_volatile(reg, value);
        }
    }
}

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn disable_tx_interrupt() {}

/// Service a console interrupt: top the transmit FIFO up if the transmit
/// interrupt fired, and report whether a receive interrupt is also pending so
/// the caller drains the receive path.
///
/// Reading the *masked* status once is what lets one interrupt line carry both
/// directions safely: while the receive interrupt is masked — the passphrase
/// poll window — it never appears here, so those bytes are left for the poll
/// that owns them rather than being stolen. The transmit half never waits on
/// the device, and re-arms the interrupt only while output is still owed, so
/// the interrupt disarms itself as the queue empties.
///
/// Freestanding-only; the host build reports no receive interrupt.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn service_uart_tx_irq() -> bool {
    let (base, model) = console::current();
    let status_reg = (base + model.interrupt_status_offset()) as *const u32;
    // SAFETY: `base` is the discovered console UART's MMIO base and
    // `interrupt_status_offset()` the model's documented masked-status
    // register; a naturally-aligned 32-bit volatile read of a device register
    // that touches no Rust-managed memory.
    let status = unsafe { core::ptr::read_volatile(status_reg) };
    if model.tx_interrupt_fired(status) {
        CONSOLE.service_completion();
    }
    model.rx_interrupt_fired(status)
}

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
#[must_use]
pub fn service_uart_tx_irq() -> bool {
    false
}

/// Start interrupt-driven transmit for output already queued when the console
/// interrupt line is first brought up.
///
/// Records produced before the interrupt controller was live — the early boot
/// phases — would otherwise sit queued until the next producer happened along
/// to arm the interrupt.
pub fn prime_tx_irq() {
    CONSOLE.pump();
}

/// Fill `buf` with whatever console input is **immediately available**,
/// returning how many bytes were read.
///
/// Non-blocking by design: a read with nothing pending returns zero, which the
/// stream backing turns into a scheduler park, so a program only ever sees a
/// read that carries bytes. This is the device-side *backing* the stream layer
/// attaches to standard input; it is not a program-facing interface.
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

/// Queue `bytes` of a program's own output for the console, returning how many
/// were accepted.
///
/// The bytes are passed through **verbatim** — no line-ending translation —
/// because this is the raw sink a program's `stream_write` reaches: what the
/// program wrote is what the device receives. It deliberately never mirrors to
/// the framebuffer: the serial line is its own console with its own login
/// session, so a program attached to it writes that line and nothing else.
///
/// A short count is possible and honest when the console is far behind; the
/// caller retries, exactly as it would on a pipe. It is never zero while the
/// console can still carry a byte.
#[must_use]
pub fn write_console_bytes(bytes: &[u8]) -> usize {
    CONSOLE.write_output(bytes)
}

/// Emit a short, ordered boot **beacon** — `tag` followed by a line break — so
/// a boot that wedges before the consolidated boot record still leaves a trail
/// whose last printed tag localises the hang.
///
/// Direct to the device, deliberately: see the module documentation for why a
/// beacon can use neither the queue nor the framebuffer.
pub fn beacon(tag: &str) {
    for &byte in tag.as_bytes() {
        tx_send_bounded(byte);
    }
    tx_send_bounded(b'\r');
    tx_send_bounded(b'\n');
}

/// [`core::fmt::Write`] adapter for the diagnostic line path that routes by
/// build profile: a development build sends the whole stream to the serial
/// line, so a capture carries the full diagnostic stream while the screen
/// stays clear for the session; a shippable build renders on the framebuffer
/// when one is configured.
///
/// Serial bytes get line-ending translation so a captured log renders line
/// breaks; the framebuffer renderer interprets a bare newline itself.
pub struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if !cfg!(debug_assertions) && crate::video::is_active() {
            crate::video::write_bytes(s.as_bytes());
            return Ok(());
        }
        for byte in s.bytes() {
            if byte == b'\n' {
                tx_send_bounded(b'\r');
            }
            tx_send_bounded(byte);
        }
        Ok(())
    }
}

/// The log sink: one formatted line per event, on the console.
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
        // A shippable build with a live framebuffer renders straight to the
        // screen: that is the user-facing console, and a framebuffer write is
        // a memory store, not a slow transmit, so there is nothing to decouple
        // from — the queue exists for the UART's byte rate.
        if !cfg!(debug_assertions) && crate::video::is_active() {
            let mut w = ConsoleWriter;
            write_formatted(&mut w, event);
            return;
        }
        CONSOLE.write_event(event);
    }
}

/// The single `'static` sink handle the kernel binary installs as the log and
/// audit sink. Zero-sized, so it costs no memory.
pub static SERIAL_SINK: SerialSink = SerialSink::new();

#[cfg(all(test, not(all(target_arch = "aarch64", target_os = "none"))))]
mod tests {
    use core::fmt::Write as _;

    use tairix_log::{Event, EventId, Level, Sink as _};

    use super::{read_console_bytes, write_console_bytes, ConsoleWriter, CONSOLE, SERIAL_SINK};

    /// A minimal event; the line shape itself is tested where it is defined.
    fn event() -> Event<'static> {
        Event {
            level: Level::Info,
            id: EventId(4_242),
            message: "console line",
            fields: &[],
        }
    }

    #[test]
    fn program_output_is_accepted_for_the_shared_queue() {
        // The host build has no device, so this proves the wiring and the
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
    fn a_read_with_no_device_is_a_short_read_not_a_wait() {
        let mut buf = [0u8; 8];
        assert_eq!(read_console_bytes(&mut buf), 0);
    }

    #[test]
    fn the_sink_routes_a_record_into_the_shared_queue() {
        SERIAL_SINK.write_event(&event());
        // The record is queued rather than lost: the host transmitter accepts
        // nothing through the non-blocking path, so it is still owed.
        CONSOLE.pump();
    }

    #[test]
    fn the_direct_writer_accepts_a_line_without_a_device() {
        let mut writer = ConsoleWriter;
        writer
            .write_str("direct line\n")
            .expect("the direct writer is infallible");
    }
}
