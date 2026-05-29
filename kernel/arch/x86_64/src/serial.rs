//! Minimal polled 16550 UART driver for COM1 (I/O ports `0x3F8..=0x3FF`).
//!
//! This is the *boot* console — used by panic and by the test driver to
//! stream a deterministic transcript to QEMU's stdout. It is intentionally
//! polled (no interrupts, no DMA) so it works before any IDT is installed.
//!
//! # Why not pull in `uart_16550`?
//!
//! `AGENTS.md` §3 reserves `lib/` for crates with at least two consumers.
//! The x86_64 port is currently the only consumer; the rest of the kernel
//! uses `lib/log` and a `Sink` impl. When Stage 3b/3c land their own UART
//! drivers and we have ≥2 consumers, this code moves to `lib/util` per
//! `AGENTS.md` §6.

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::fmt;

/// COM1 base port.
pub const COM1_BASE: u16 = 0x3F8;

/// A polled writer for a 16550-compatible UART.
///
/// `Serial` holds no state of its own; the methods operate directly on
/// the device. The type exists so callers go through a `core::fmt::Write`
/// implementation and can never accidentally write a partial sequence of
/// bytes (`AGENTS.md` §6 — "encapsulate behind a safe API").
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
}
