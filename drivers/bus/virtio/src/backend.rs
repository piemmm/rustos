//! PCI and MMIO backend seams for the virtio [`crate::Transport`]
//! trait.
//!
//! Stage 4.D wires these seams to the kernel-side BAR / MMIO-region
//! capability that the existing `drivers/bus/pci` and
//! `drivers/bus/mmio` crates discover (`PLAN.md` Stage 4 status).
//! Until that capability is wired, this module ships **only the
//! safe trait surfaces** — [`PortIo`] and [`MmioOps`] — plus the
//! tiny [`PciBackend`] / [`MmioBackend`] adapter shells. The
//! concrete `Transport` impls live behind the same `#[cfg]` gates as
//! the existing bus-driver `unsafe asm!` blocks and are landed in
//! the Stage-4.D wiring PR; this crate documents the contract here
//! so the virtio-blk and virtio-net driver crates can already be
//! reviewed against a stable seam (`AGENTS.md` §2.4 / §8).
//!
//! Keeping the seams in *this* crate, not in a `lib/`, follows
//! `AGENTS.md` §2.2 / §6: the seams currently have a single caller
//! (the virtio transport) and so the two-caller rule does not
//! justify a `lib/` crate yet.

/// Safe abstraction over the architecture's port I/O space.
///
/// Mirrors `drivers/bus/pci`'s `PortIo` trait so that this crate can
/// host its own legacy-PCI register accessors without depending on
/// `rustos-drv-bus-pci`'s internals (`AGENTS.md` §2.4 — no implicit
/// inter-crate API surface).
pub trait PortIo {
    /// Read a 32-bit value from `port`.
    fn read_u32(&self, port: u16) -> u32;
    /// Write a 32-bit value to `port`.
    fn write_u32(&self, port: u16, value: u32);
}

/// Safe abstraction over the architecture's volatile MMIO accesses.
///
/// Mirrors `drivers/bus/mmio`'s `MmioRead` and extends it with a
/// matching write side; the virtio MMIO transport (`virtio 1.1
/// §4.2`) requires both directions.
pub trait MmioOps {
    /// Volatile-read a 32-bit value `offset` bytes into the device's
    /// MMIO window.
    fn read_u32(&self, offset: usize) -> u32;
    /// Volatile-write a 32-bit value `offset` bytes into the
    /// device's MMIO window.
    fn write_u32(&self, offset: usize, value: u32);
}

/// PCI-bus backend adapter.
///
/// Carries a reference to the [`PortIo`] seam, along with the
/// PCI device's bus / device / function triple. The concrete
/// `Transport` impl lives in the Stage-4.D follow-up PR; this
/// adapter shell is exported so that the bus-driver hand-off API
/// can be sketched against a stable type name today.
pub struct PciBackend<'a, P: PortIo + ?Sized> {
    /// Borrowed `PortIo` seam.
    pub port_io: &'a P,
    /// PCI device address.
    pub bus: u8,
    /// PCI device slot.
    pub device: u8,
    /// PCI function.
    pub function: u8,
}

/// MMIO-bus backend adapter.
///
/// Carries a reference to the [`MmioOps`] seam and the device's
/// MMIO window length, planted by `drivers/bus/mmio` when it
/// hand-offs the device.
pub struct MmioBackend<'a, M: MmioOps + ?Sized> {
    /// Borrowed `MmioOps` seam.
    pub mmio: &'a M,
    /// Length of the device's MMIO window, in bytes.
    pub window_len: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    /// Deterministic in-memory fake [`PortIo`] used by the in-crate
    /// unit tests (the same pattern `drivers/bus/pci` uses for its
    /// mock).
    struct FakePortIo {
        last_port: Cell<u16>,
        last_value: Cell<u32>,
    }

    impl PortIo for FakePortIo {
        fn read_u32(&self, port: u16) -> u32 {
            self.last_port.set(port);
            0xDEAD_BEEF
        }
        fn write_u32(&self, port: u16, value: u32) {
            self.last_port.set(port);
            self.last_value.set(value);
        }
    }

    struct FakeMmio {
        last_offset: Cell<usize>,
        last_value: Cell<u32>,
    }

    impl MmioOps for FakeMmio {
        fn read_u32(&self, offset: usize) -> u32 {
            self.last_offset.set(offset);
            0xCAFE_BABE
        }
        fn write_u32(&self, offset: usize, value: u32) {
            self.last_offset.set(offset);
            self.last_value.set(value);
        }
    }

    #[test]
    fn fake_port_io_round_trip() {
        let fake = FakePortIo {
            last_port: Cell::new(0),
            last_value: Cell::new(0),
        };
        assert_eq!(fake.read_u32(0xCF8), 0xDEAD_BEEF);
        assert_eq!(fake.last_port.get(), 0xCF8);
        fake.write_u32(0xCFC, 0x1234_5678);
        assert_eq!(fake.last_port.get(), 0xCFC);
        assert_eq!(fake.last_value.get(), 0x1234_5678);
    }

    #[test]
    fn fake_mmio_round_trip() {
        let fake = FakeMmio {
            last_offset: Cell::new(0),
            last_value: Cell::new(0),
        };
        assert_eq!(fake.read_u32(0x70), 0xCAFE_BABE);
        assert_eq!(fake.last_offset.get(), 0x70);
        fake.write_u32(0x14, 0xAA);
        assert_eq!(fake.last_offset.get(), 0x14);
        assert_eq!(fake.last_value.get(), 0xAA);
    }

    #[test]
    fn pci_backend_shell_carries_address() {
        let fake = FakePortIo {
            last_port: Cell::new(0),
            last_value: Cell::new(0),
        };
        let b = PciBackend {
            port_io: &fake,
            bus: 0,
            device: 5,
            function: 1,
        };
        assert_eq!(b.bus, 0);
        assert_eq!(b.device, 5);
        assert_eq!(b.function, 1);
        assert_eq!(b.port_io.read_u32(0), 0xDEAD_BEEF);
    }

    #[test]
    fn mmio_backend_shell_carries_window() {
        let fake = FakeMmio {
            last_offset: Cell::new(0),
            last_value: Cell::new(0),
        };
        let b = MmioBackend {
            mmio: &fake,
            window_len: 0x200,
        };
        assert_eq!(b.window_len, 0x200);
        assert_eq!(b.mmio.read_u32(0), 0xCAFE_BABE);
    }
}
