//! Legacy port-I/O access seam (`abi-v1`).
//!
//! A [`PortIo`] is the host↔driver ABI seam through which a bus driver
//! that depends on architectural port I/O — today only `drivers/bus/pci`
//! reaching PCI configuration space through mechanism #1 (PCI Local Bus
//! 3.0 §3.2.2.3.2: the address word at I/O port `0xCF8` and the data word
//! at `0xCFC`) — issues 32-bit reads and writes against an I/O port.
//!
//! It lives in `lib/abi` for the same reason [`MmioMapper`](super::MmioMapper)
//! does: the bus driver has to be able to name the seam without pulling in
//! an architecture port (`kernel/arch/<target>`), which would invert the
//! dependency direction and gate the driver on a target-conditional
//! `cfg` — both forbidden by `AGENTS.md` §17.2 / §17.4. The architecture
//! port (for x86_64, `kernel/arch/x86_64`) supplies the only real
//! implementation, encapsulating the `in`/`out` instructions and their
//! `unsafe` invariants behind this safe trait (`AGENTS.md` §2.10). The
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
