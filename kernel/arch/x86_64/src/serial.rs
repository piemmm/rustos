//! Minimal polled 16550 UART driver for COM1 (I/O ports `0x3F8..=0x3FF`).
//!
//! This is the *boot* console — used by panic and by the test driver to
//! stream a deterministic transcript to QEMU's stdout. It is intentionally
//! polled (no interrupts, no DMA) so it works before any IDT is installed.
//!
//! # Why not pull in `uart_16550`?
//!
//! the charter reserves `lib/` for crates with at least two consumers.
//! The x86_64 port is currently the only consumer; the rest of the kernel
//! uses `lib/log` and a `Sink` impl. When Stage 3b/3c land their own UART
//! drivers and we have ≥2 consumers, this code moves to `lib/util` per.

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::fmt;

/// COM1 base port.
pub const COM1_BASE: u16 = 0x3F8;

/// A polled writer for a 16550-compatible UART.
///
/// `Serial` holds no state of its own; the methods operate directly on
/// the device. The type exists so callers go through a `core::fmt::Write`
/// implementation and can never accidentally write a partial sequence of
/// bytes ("encapsulate behind a safe API").
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
    /// Performs the standard 16550 init sequence. Each `outb` is unsafe in
    /// the Rust sense but well-defined on every platform where this code
    /// runs (QEMU emulates a 16550 unconditionally for `-serial stdio`).
    #[must_use]
    pub fn init(base: u16) -> Self {
        // SAFETY: `outb` to a UART port is well-defined; init sequence
        // copied verbatim from the Stage-2 deliverable's reference driver
        // (Intel 16550 datasheet, table 5). No memory effects.
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
    /// [`Serial::init`] disables interrupts (`IER = 0`) as part of the
    /// standard 16550 bring-up, so the interrupt-driven receive path must
    /// reach the device through this non-reinitialising constructor — a
    /// fresh [`Serial::init`] would clear the receive-interrupt-enable bit
    /// the console armed. The line settings are sticky, so the receive path
    /// operates on the UART the boot console already configured.
    #[must_use]
    pub const fn at(base: u16) -> Self {
        Self { base }
    }

    /// Send a single byte, busy-waiting until the transmitter holding
    /// register is empty.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    pub fn write_byte(&mut self, b: u8) {
        // SAFETY: spin-poll on LSR bit 5 (THR empty), then `outb` the byte.
        // Both operations are well-defined per the 16550 datasheet.
        unsafe {
            while (inb(self.base + 5) & 0x20) == 0 {}
            outb(self.base, b);
        }
    }

    /// Drain every byte immediately available in the receive FIFO into
    /// `buf`, returning the number read (`0..=buf.len()`).
    ///
    /// Non-blocking: it reads the Receiver Buffer Register (RBR) while the
    /// Line Status Register's Data-Ready bit is set, stopping at the first
    /// empty poll or when `buf` fills. A call with an empty FIFO returns
    /// `0` — never a busy-wait for input; the caller's
    /// `BlockingConsoleRead` turns that into a scheduler park. Reads are
    /// destructive, so the caller serialises this against the receive ISR.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    pub fn read_available(&mut self, buf: &mut [u8]) -> usize {
        let mut read = 0;
        for slot in buf.iter_mut() {
            // SAFETY: LSR (base+5) and RBR (base) are the 16550's status
            // and data ports; both `inb`s are well-defined in ring 0.
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
    /// Idempotent: it reads the current IER and sets only bit 0, preserving
    /// any other enabled source. `OUT2` (the IRQ gate to the IO-APIC) was
    /// asserted by [`Serial::init`], so once this bit is set the line
    /// reaches the controller.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
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
    /// receive ISR clears this bit and leaves the surplus in the hardware
    /// FIFO; the reader re-enables it once it has drained queue space, and
    /// the 16550 re-asserts a fresh interrupt edge for the bytes still in
    /// the FIFO — lossless flow control that works with the ISA line's
    /// edge-triggered delivery.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    pub fn disable_rx_interrupt(&mut self) {
        // SAFETY: as `enable_rx_interrupt`, clearing only bit 0.
        unsafe {
            let ier = inb(self.base + 1);
            outb(self.base + 1, ier_with_rx_disabled(ier));
        }
    }
}

/// The Line Status Register's Data-Ready bit (bit 0): set while the
/// receive FIFO holds at least one unread byte (16550 datasheet, table 7).
#[must_use]
pub const fn lsr_data_ready(lsr: u8) -> bool {
    lsr & 0x01 != 0
}

/// `ier` with the Received-Data-Available enable (bit 0) set, preserving
/// every other bit.
#[must_use]
pub const fn ier_with_rx_enabled(ier: u8) -> u8 {
    ier | 0x01
}

/// `ier` with the Received-Data-Available enable (bit 0) cleared,
/// preserving every other bit.
#[must_use]
pub const fn ier_with_rx_disabled(ier: u8) -> u8 {
    ier & !0x01
}

/// Drain every immediately-available byte from COM1's receive FIFO into
/// `buf`, returning the count. The non-reinitialising receive path over the
/// boot console UART; see [`Serial::read_available`].
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

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            self.write_byte(b);
        }
        Ok(())
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[inline]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: caller has verified `port` is a legitimate I/O port and that
    // we are in ring 0 with IOPL=0 (always true in the kernel).
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
    use super::*;

    #[test]
    fn com1_base_is_canonical() {
        // Sanity tie-down: COM1's base port is fixed by every x86 PC
        // platform datasheet. Changing it would be a kernel-vendor-level
        // mistake.
        assert_eq!(COM1_BASE, 0x3F8);
    }

    #[test]
    fn lsr_data_ready_reads_bit_zero_only() {
        assert!(!lsr_data_ready(0x00));
        assert!(lsr_data_ready(0x01));
        // A UART with THR-empty (bit 5) but no RX byte is not data-ready.
        assert!(!lsr_data_ready(0x20));
        // Data-ready plus other status bits still reads ready.
        assert!(lsr_data_ready(0x61));
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
        // Other enabled sources are preserved when the RX bit is cleared.
        assert_eq!(ier_with_rx_disabled(0x03), 0x02);
        // Idempotent: already-disabled stays disabled.
        assert_eq!(ier_with_rx_disabled(0x02), 0x02);
    }

    #[test]
    fn ier_rx_enable_disable_round_trip() {
        for ier in 0u8..=0xFF {
            // Toggling RX on then off restores every non-RX bit exactly.
            let toggled = ier_with_rx_disabled(ier_with_rx_enabled(ier));
            assert_eq!(toggled, ier & !0x01);
        }
    }
}
