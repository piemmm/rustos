//! This port's console: the 16550 UART at COM1 (I/O ports `0x3F8..=0x3FF`),
//! as the device half of the shared console-output engine.
//!
//! # What lives here and what does not
//!
//! Everything about *queueing* console output — whole-line framing, ordering,
//! shedding the least important line under pressure, counting and reporting
//! what was dropped, and the one bounded drain — is
//! [`tairix_conout`], shared by every port. What lives here is
//! only what the silicon makes specific to this architecture: the I/O-port
//! access, the line bring-up, and the status and interrupt-enable bits.
//!
//! # How output reaches the wire
//!
//! Unlike the interrupt-driven ports, this console drains **write-through**:
//! a producer's line is pushed to the device before the call returns, waiting
//! boundedly for the transmitter. That is deliberate, not a shortcut. This
//! port's UART interrupt is routed and unmasked only when the interactive
//! session starts, and the great majority of console output — the entire boot
//! log, and every panic — happens before that, with no interrupt able to drain
//! a backlog and no dispatch loop yet running to pump one. A queue that
//! deferred those bytes to an interrupt that cannot arrive would simply lose
//! the boot transcript.
//!
//! The queue is still what makes output *correct*: a line is admitted whole
//! under the console gate, so two CPUs logging at once can no longer interleave
//! their bytes mid-line, and anything that cannot be carried is counted and
//! reported rather than silently dropped.
//!
//! The wait is bounded. A transmitter that never reports itself ready — an
//! unwired or flow-blocked line — is declared wedged and its bytes dropped
//! (and counted), because an unbounded readiness spin would hang the kernel on
//! its very first log line.

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::fmt;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use tairix_conout::{tx_wait, TxOutcome, TX_POLL_BUDGET};
use tairix_conout::{ConsoleGate, ConsoleTx, DEFAULT_CAPACITY_BYTES};
use tairix_log::{Event, Sink};

use crate::irqmask::PortIrqControl;

/// COM1 base port.
pub const COM1_BASE: u16 = 0x3F8;

/// A polled writer for a 16550-compatible UART.
///
/// `Serial` holds no state of its own; the methods operate directly on the
/// device. The type exists so callers go through a `core::fmt::Write`
/// implementation and can never accidentally write a partial sequence of
/// bytes.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub struct Serial {
    base: u16,
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
impl Serial {
    /// Initialise the UART at `base` for 38400 8N1, FIFOs enabled, DTR/RTS
    /// asserted, loopback off.
    ///
    /// # Safety-invariant
    ///
    /// Performs the standard 16550 init sequence. Each `outb` is unsafe in the
    /// Rust sense but well-defined on every platform where this code runs
    /// (QEMU emulates a 16550 unconditionally for `-serial stdio`).
    #[must_use]
    pub fn init(base: u16) -> Self {
        // SAFETY: `outb` to a UART port is well-defined; the sequence is the
        // 16550 bring-up from the Intel datasheet, table 5. No memory effects.
        unsafe {
            outb(base + 1, 0x00); // disable interrupts
            outb(base + 3, 0x80); // DLAB on
            outb(base, 0x03); // divisor lo: 38400
            outb(base + 1, 0x00); // divisor hi
            outb(base + 3, 0x03); // DLAB off, 8N1
            outb(base + 2, 0xC7); // FIFO enable, clear, 14-byte threshold
            outb(base + 4, 0x0B); // DTR/RTS asserted, OUT2 (IRQ enable line)
        }
        Self { base }
    }

    /// Wrap an already-initialised UART at `base` **without** re-running the
    /// init sequence.
    ///
    /// [`Serial::init`] disables interrupts (`IER = 0`) as part of the standard
    /// 16550 bring-up, so the interrupt-driven receive path must reach the
    /// device through this non-reinitialising constructor — a fresh
    /// [`Serial::init`] would clear the receive-interrupt-enable bit the
    /// console armed. The line settings are sticky, so the receive path
    /// operates on the UART the boot console already configured.
    #[must_use]
    pub const fn at(base: u16) -> Self {
        Self { base }
    }

    /// Send a single byte, waiting **boundedly** for the transmitter-holding
    /// register to empty.
    ///
    /// A transmitter that never empties is declared wedged and the byte
    /// dropped: an unbounded wait here would hang the kernel on an unwired or
    /// flow-blocked line rather than merely losing its output.
    pub fn write_byte(&mut self, b: u8) {
        let _sent = tx_send_bounded(self.base, b);
    }

    /// Drain every byte immediately available in the receive FIFO into `buf`,
    /// returning the number read (`0..=buf.len()`).
    ///
    /// Non-blocking: it reads the Receiver Buffer Register while the Line
    /// Status Register's Data-Ready bit is set, stopping at the first empty
    /// poll or when `buf` fills. A call with an empty FIFO returns `0` — never
    /// a busy-wait for input; the caller's `BlockingConsoleRead` turns that
    /// into a scheduler park. Reads are destructive, so the caller serialises
    /// this against the receive interrupt handler.
    pub fn read_available(&mut self, buf: &mut [u8]) -> usize {
        let mut read = 0;
        for slot in buf.iter_mut() {
            // SAFETY: LSR (base+5) and RBR (base) are the 16550's status and
            // data ports; both `inb`s are well-defined in ring 0.
            unsafe {
                if !lsr_data_ready(inb(self.base + 5)) {
                    break;
                }
                *slot = inb(self.base);
            }
            read += 1;
        }
        read
    }

    /// Enable the Received-Data-Available interrupt (IER bit 0), so a byte
    /// arriving in the receive FIFO raises the UART's interrupt line.
    ///
    /// Idempotent: it reads the current IER and sets only bit 0, preserving any
    /// other enabled source. `OUT2` (the interrupt gate to the IO-APIC) was
    /// asserted by [`Serial::init`], so once this bit is set the line reaches
    /// the controller.
    pub fn enable_rx_interrupt(&mut self) {
        // SAFETY: IER is at base+1; the read-modify-write leaves the other
        // enable bits untouched. Both port ops are well-defined in ring 0.
        unsafe {
            let ier = inb(self.base + 1);
            outb(self.base + 1, ier_with_rx_enabled(ier));
        }
    }

    /// Disable the Received-Data-Available interrupt (IER bit 0).
    ///
    /// The receive-side flow-control brake: with a full software queue the
    /// receive handler clears this bit and leaves the surplus in the hardware
    /// FIFO; the reader re-enables it once it has drained queue space, and the
    /// 16550 re-asserts a fresh interrupt edge for the bytes still in the
    /// FIFO — lossless flow control that works with the ISA line's
    /// edge-triggered delivery.
    pub fn disable_rx_interrupt(&mut self) {
        // SAFETY: as `enable_rx_interrupt`, clearing only bit 0.
        unsafe {
            let ier = inb(self.base + 1);
            outb(self.base + 1, ier_with_rx_disabled(ier));
        }
    }
}

/// The Line Status Register's Data-Ready bit (bit 0): set while the receive
/// FIFO holds at least one unread byte (16550 datasheet, table 7).
#[must_use]
pub const fn lsr_data_ready(lsr: u8) -> bool {
    lsr & 0x01 != 0
}

/// The Line Status Register's Transmitter-Holding-Register-Empty bit (bit 5):
/// set while the transmitter can accept another byte (16550 datasheet,
/// table 7).
#[must_use]
pub const fn lsr_tx_ready(lsr: u8) -> bool {
    lsr & 0x20 != 0
}

/// `ier` with the Received-Data-Available enable (bit 0) set, preserving every
/// other bit.
#[must_use]
pub const fn ier_with_rx_enabled(ier: u8) -> u8 {
    ier | 0x01
}

/// `ier` with the Received-Data-Available enable (bit 0) cleared, preserving
/// every other bit.
#[must_use]
pub const fn ier_with_rx_disabled(ier: u8) -> u8 {
    ier & !0x01
}

/// This port's UART, as the shared engine's transmitter.
struct Com1Tx;

impl ConsoleTx for Com1Tx {
    /// The console's interrupt line is unmasked only once the interactive
    /// session starts, so a backlog is drained by waiting rather than by an
    /// interrupt — see the module documentation.
    const COMPLETION_INTERRUPT: bool = false;

    fn uptime_ms(&self) -> Option<u64> {
        // No monotonic-uptime seam is wired on this port yet, so a record
        // carries no stamp rather than a fabricated one.
        None
    }

    fn send_ready(&self, bytes: &[u8]) -> usize {
        let base = ready_com1();
        let mut sent = 0;
        for &byte in bytes {
            if !tx_ready_now(base) {
                break;
            }
            tx_send(base, byte);
            sent += 1;
        }
        sent
    }

    fn send_bounded(&self, bytes: &[u8]) -> usize {
        let base = ready_com1();
        let mut sent = 0;
        for &byte in bytes {
            if !tx_send_bounded(base, byte) {
                break;
            }
            sent += 1;
        }
        sent
    }

    fn send_bypass(&self, byte: u8) -> bool {
        tx_send_bounded(ready_com1(), byte)
    }

    fn set_completion_interrupt(&self, _on: bool) {
        // Never reached: this port reports no completion interrupt.
    }
}

/// The console every producer on this port writes through: the log sink, the
/// `stream_write` backing and the panic bridge alike, so all three share one
/// ordered stream on the wire.
static CONSOLE: ConsoleGate<Com1Tx, PortIrqControl, DEFAULT_CAPACITY_BYTES> =
    ConsoleGate::new(Com1Tx);

/// One-shot line bring-up guard, so `Serial::init` runs exactly once.
///
/// Re-running it per write would be a defect, not merely waste: it writes
/// `IER = 0`, disarming the receive interrupt the interactive session enabled,
/// and sets the FIFO-clear bits, discarding bytes a reader had typed ahead.
/// The line settings are sticky, so every later write reaches the device
/// through the non-reinitialising `Serial::at`, which touches only the
/// transmitter.
static COM1_READY: tairix_sync::once::Once<()> = tairix_sync::once::Once::new();

/// The initialised COM1 base port, bringing the line up on first use.
fn ready_com1() -> u16 {
    let _ = COM1_READY.call_once_infallible(|| {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        let _ = Serial::init(COM1_BASE);
    });
    COM1_BASE
}

/// Whether the transmitter was last found not to be draining
/// ([`tairix_conout::tx_wait`]). While set, a byte costs a single readiness
/// poll instead of a full budget, so a dead or flow-blocked line cannot crawl
/// the boot; the first poll that finds it draining clears it.
///
/// Freestanding-only: it tracks the state of real port I/O, and the host
/// build's transmit is inert.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static TX_WEDGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether the transmitter can accept a byte **right now**, polled exactly
/// once — no spin, no budget.
///
/// Freestanding-only; the host build reports "not ready", so a host drain is a
/// no-op.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn tx_ready_now(base: u16) -> bool {
    // SAFETY: LSR is at base+5 on a 16550; a byte-wide port read that is
    // well-defined in ring 0 and touches no memory.
    unsafe { lsr_tx_ready(inb(base + 5)) }
}

/// See the target variant above.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn tx_ready_now(_base: u16) -> bool {
    false
}

/// Write one byte to a transmitter the caller has **already confirmed ready**
/// with [`tx_ready_now`]. Correct only immediately after a true readiness
/// poll, since it performs none itself.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn tx_send(base: u16, byte: u8) {
    // SAFETY: the transmitter-holding register is the 16550's base port; a
    // byte-wide port write that is well-defined in ring 0 and touches no
    // memory. The caller confirmed readiness via `tx_ready_now`.
    unsafe {
        outb(base, byte);
    }
}

/// See the target variant above.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn tx_send(_base: u16, _byte: u8) {}

/// Transmit one byte, waiting **boundedly** for the transmitter to accept it,
/// and report whether it went out.
///
/// Freestanding-only; the host build reports the byte sent, so a host drain
/// terminates rather than looping on a device that does not exist.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn tx_send_bounded(base: u16, byte: u8) -> bool {
    use core::sync::atomic::Ordering;

    let wedged = TX_WEDGED.load(Ordering::Relaxed);
    let (outcome, now_wedged) = tx_wait(|| tx_ready_now(base), wedged, TX_POLL_BUDGET);
    if now_wedged != wedged {
        TX_WEDGED.store(now_wedged, Ordering::Relaxed);
    }
    if outcome == TxOutcome::Drop {
        return false;
    }
    tx_send(base, byte);
    true
}

/// See the target variant above.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn tx_send_bounded(_base: u16, _byte: u8) -> bool {
    true
}

/// Drain every immediately-available byte from COM1's receive FIFO into `buf`,
/// returning the count. The non-reinitialising receive path over the boot
/// console UART; see [`Serial::read_available`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn read_console_bytes(buf: &mut [u8]) -> usize {
    Serial::at(COM1_BASE).read_available(buf)
}

/// Enable COM1's Received-Data-Available interrupt; see
/// [`Serial::enable_rx_interrupt`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn enable_rx_interrupt() {
    Serial::at(COM1_BASE).enable_rx_interrupt();
}

/// Disable COM1's Received-Data-Available interrupt; see
/// [`Serial::disable_rx_interrupt`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn disable_rx_interrupt() {
    Serial::at(COM1_BASE).disable_rx_interrupt();
}

/// Queue `bytes` of a program's own output for the console, returning how many
/// were accepted.
///
/// The bytes are passed through **verbatim** — no line-ending translation —
/// because this is the raw sink a program's `stream_write` reaches: what the
/// program wrote is what the device receives.
///
/// A short count is possible and honest when the console is far behind; the
/// caller retries, exactly as it would on a pipe. It is never zero while the
/// console can still carry a byte.
#[must_use]
pub fn write_console_bytes(bytes: &[u8]) -> usize {
    CONSOLE.write_output(bytes)
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            self.write_byte(b);
        }
        Ok(())
    }
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

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[inline]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: caller has verified `port` is a legitimate I/O port and that we
    // are in ring 0 with IOPL=0 (always true in the kernel).
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: see `outb`.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use tairix_log::{Event, EventId, Level, Sink as _};

    use super::{
        ier_with_rx_disabled, ier_with_rx_enabled, lsr_data_ready, lsr_tx_ready,
        write_console_bytes, COM1_BASE, SERIAL_SINK,
    };

    #[test]
    fn com1_base_is_canonical() {
        // Sanity tie-down: COM1's base port is fixed by every x86 PC platform
        // datasheet. Changing it would be a kernel-vendor-level mistake.
        assert_eq!(COM1_BASE, 0x3F8);
    }

    #[test]
    fn lsr_data_ready_reads_bit_zero_only() {
        assert!(!lsr_data_ready(0x00));
        assert!(lsr_data_ready(0x01));
        // A UART with THR-empty (bit 5) but no receive byte is not data-ready.
        assert!(!lsr_data_ready(0x20));
        // Data-ready plus other status bits still reads ready.
        assert!(lsr_data_ready(0x61));
    }

    #[test]
    fn lsr_tx_ready_reads_bit_five_only() {
        assert!(!lsr_tx_ready(0x00));
        assert!(lsr_tx_ready(0x20));
        // A receive byte waiting does not make the transmitter ready.
        assert!(!lsr_tx_ready(0x01));
        assert!(lsr_tx_ready(0x61));
    }

    #[test]
    fn ier_rx_enable_sets_only_bit_zero() {
        assert_eq!(ier_with_rx_enabled(0x00), 0x01);
        // Other enabled sources (e.g. THR-empty, bit 1) are preserved.
        assert_eq!(ier_with_rx_enabled(0x02), 0x03);
        // Idempotent: already-enabled stays enabled.
        assert_eq!(ier_with_rx_enabled(0x01), 0x01);
    }

    #[test]
    fn ier_rx_disable_clears_only_bit_zero() {
        assert_eq!(ier_with_rx_disabled(0x01), 0x00);
        // Other enabled sources are preserved when the receive bit is cleared.
        assert_eq!(ier_with_rx_disabled(0x03), 0x02);
        // Idempotent: already-disabled stays disabled.
        assert_eq!(ier_with_rx_disabled(0x02), 0x02);
    }

    #[test]
    fn ier_rx_enable_disable_round_trip() {
        for ier in 0u8..=0xFF {
            // Toggling receive on then off restores every other bit exactly.
            let toggled = ier_with_rx_disabled(ier_with_rx_enabled(ier));
            assert_eq!(toggled, ier & !0x01);
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
    fn the_sink_routes_a_record_into_the_shared_queue() {
        SERIAL_SINK.write_event(&Event {
            level: Level::Info,
            id: EventId(4_242),
            message: "console line",
            fields: &[],
        });
    }
}
