//! Legacy port-I/O access seam (`abi-v1`).
//!
//! A [`PortIo`] is the host↔driver ABI seam through which a bus driver
//! that depends on architectural port I/O — today only `lib/pci`
//! reaching PCI configuration space through mechanism #1 (PCI Local Bus
//! 3.0 §3.2.2.3.2: the address word at I/O port `0xCF8` and the data word
//! at `0xCFC`) — issues 32-bit reads and writes against an I/O port.
//!
//! It lives in `lib/abi` for the same reason [`MmioMapper`](super::MmioMapper)
//! does: the bus driver has to be able to name the seam without pulling in
//! an architecture port (`kernel/arch/<target>`), which would invert the
//! dependency direction and gate the driver on a target-conditional
//! `cfg` — both forbidden by. The architecture
//! port (for x86_64, `kernel/arch/x86_64`) supplies the only real
//! implementation, encapsulating the `in`/`out` instructions and their
//! `unsafe` invariants behind this safe trait. The
//! driver never issues a port-I/O instruction itself and carries no
//! architecture gate.
//!
//! Port I/O is an x86-family mechanism; architectures without an I/O port
//! space (aarch64, riscv64, wasm32) simply never construct a [`PortIo`]
//! and reach `PCIe` through memory-mapped ECAM instead — a separate seam.

/// 32-bit port-I/O access seam.
///
/// The trait is *not* `unsafe` to implement: an implementor is
/// responsible for whatever invariants its backing transport requires,
/// and the only in-tree implementor (the x86_64 architecture port)
/// encapsulates the single `unsafe` block behind these two safe methods.
/// A driver consumes a `PortIo` by value or behind `&dyn PortIo` and only
/// ever issues the documented PCI configuration-port accesses through it.
pub trait PortIo {
    /// Read 32 bits from I/O port `port`.
    fn read32(&self, port: u16) -> u32;

    /// Write 32 bits of `value` to I/O port `port`.
    fn write32(&self, port: u16, value: u32);
}

/// 8-bit port-I/O access seam.
///
/// Distinct from [`PortIo`] (which is 32-bit and exists solely for PCI
/// mechanism #1 configuration access): legacy register files such as the
/// Intel 8042 keyboard/mouse controller (`drivers/input/ps2`, status/command
/// port `0x64` and data port `0x60`) and the 16550 UART are byte-addressed,
/// and a 32-bit read would alias three neighbouring registers. `PortIo` is a
/// frozen `abi-v1` interface, so the byte width ships as
/// this separate versioned trait rather than as an added method.
///
/// It lives in `lib/abi` for the same reason [`PortIo`] does: a driver must
/// name the seam without depending on an architecture port
/// (`kernel/arch/<target>`), which would invert the dependency direction and
/// force a target-conditional `cfg` on the driver — both forbidden by
/// . The architecture port (for x86_64,
/// `kernel/arch/x86_64`) supplies the only real implementation, encapsulating
/// the `inb`/`outb` instructions and their `unsafe` invariants behind this
/// safe trait. The driver never issues a port-I/O
/// instruction itself and carries no architecture gate.
///
/// Port I/O is an x86-family mechanism; architectures without an I/O port
/// space (aarch64, riscv64, wasm32) simply never construct a [`PortIo8`].
pub trait PortIo8 {
    /// Read 8 bits from I/O port `port`.
    fn read8(&self, port: u16) -> u8;

    /// Write 8 bits of `value` to I/O port `port`.
    fn write8(&self, port: u16, value: u8);
}

#[cfg(test)]
mod tests {
    use super::{PortIo, PortIo8};
    use core::cell::Cell;

    /// Records the last `(port, value)` written and replies to reads
    /// with `port`'s low byte, proving both seams are object-safe and
    /// usable behind `&dyn`.
    struct Recorder {
        last_port: Cell<u16>,
        last_value: Cell<u32>,
    }

    impl PortIo for Recorder {
        fn read32(&self, port: u16) -> u32 {
            self.last_port.set(port);
            u32::from(port)
        }
        fn write32(&self, port: u16, value: u32) {
            self.last_port.set(port);
            self.last_value.set(value);
        }
    }

    impl PortIo8 for Recorder {
        fn read8(&self, port: u16) -> u8 {
            self.last_port.set(port);
            port.to_le_bytes()[0]
        }
        fn write8(&self, port: u16, value: u8) {
            self.last_port.set(port);
            self.last_value.set(u32::from(value));
        }
    }

    #[test]
    fn port_io8_is_object_safe_and_byte_wide() {
        let rec = Recorder {
            last_port: Cell::new(0),
            last_value: Cell::new(0),
        };
        let io: &dyn PortIo8 = &rec;
        io.write8(0x64, 0xAE);
        assert_eq!(rec.last_port.get(), 0x64);
        assert_eq!(rec.last_value.get(), 0xAE);
        assert_eq!(io.read8(0x60), 0x60);
    }

    #[test]
    fn port_io_32_and_8_are_independent_seams() {
        let rec = Recorder {
            last_port: Cell::new(0),
            last_value: Cell::new(0),
        };
        let wide: &dyn PortIo = &rec;
        assert_eq!(wide.read32(0x00CF), 0x00CF);
        let narrow: &dyn PortIo8 = &rec;
        assert_eq!(narrow.read8(0x00CF), 0xCF);
    }
}
