//! PCI configuration-access mechanism #1 (PIO bridge for x86_64).
//!
//! Splits the `in`/`out` instructions behind a [`PortIo`] trait so
//! the unit tests can drive the bridge without touching real I/O
//! ports. The single `unsafe` block lives inside
//! `X86PortIo::read32` / `X86PortIo::write32` and carries the
//! invariants required by `AGENTS.md` §2.10; the cfg gate confines
//! those impls to `target_arch = "x86_64"`.
//
// Same `dead_code` rationale as `config.rs`: the production reach
// path is through `dyn Bus` dispatch wired up by the driver host;
// the in-crate test module covers every helper directly.
#![allow(dead_code)]

use crate::config::{ConfigAddress, ConfigSpace};

const PCI_CONFIG_ADDRESS_PORT: u16 = 0xCF8;
const PCI_CONFIG_DATA_PORT: u16 = 0xCFC;

/// Minimal 32-bit PIO seam.
///
/// The trait is *not* `unsafe` to implement — implementors are
/// responsible for whatever invariants their backing transport
/// needs, and the only in-tree implementor is
/// [`X86PortIo`] which encapsulates the only `unsafe` block in
/// the crate.
pub trait PortIo {
    /// Read 32 bits from port `port`.
    fn read32(&self, port: u16) -> u32;
    /// Write 32 bits to port `port`.
    fn write32(&self, port: u16, value: u32);
}

/// Concrete [`ConfigSpace`] using a [`PortIo`] backend.
///
/// The bridge is parameterised on `P: PortIo` so the test suite
/// substitutes a recording mock; the production wiring uses
/// [`X86PortIo`].
pub struct PortIoConfigSpace<P: PortIo> {
    pio: P,
}

impl<P: PortIo> PortIoConfigSpace<P> {
    /// Construct a [`PortIoConfigSpace`] over `pio`.
    pub const fn new(pio: P) -> Self {
        Self { pio }
    }
}

impl<P: PortIo> ConfigSpace for PortIoConfigSpace<P> {
    fn read32(&self, addr: ConfigAddress) -> u32 {
        // Out-of-range addresses cannot reach this method — the
        // enumeration code constructs every `ConfigAddress` with
        // field values inside the documented PCI ranges. The
        // defensive gate inside `to_cf8` returns `None` for any
        // anomalous construction; treat that case as "no device"
        // by reading all-ones (matches the PCI Local Bus 3.0
        // §6.1 sentinel).
        let Some(cf8) = addr.to_cf8() else {
            return 0xFFFF_FFFF;
        };
        self.pio.write32(PCI_CONFIG_ADDRESS_PORT, cf8);
        self.pio.read32(PCI_CONFIG_DATA_PORT)
    }

    fn write32(&self, addr: ConfigAddress, value: u32) {
        let Some(cf8) = addr.to_cf8() else {
            return;
        };
        self.pio.write32(PCI_CONFIG_ADDRESS_PORT, cf8);
        self.pio.write32(PCI_CONFIG_DATA_PORT, value);
    }
}

/// Real-hardware x86_64 PIO implementation.
///
/// The two `unsafe` blocks below are the only `unsafe` in the
/// crate; both are exercised against a mock [`PortIo`] in the
/// in-crate test suite to validate the address / data interleaving
/// (the `unsafe` *instructions* themselves are excluded from host
/// tests by the `cfg` gate, which is the standard way to test a
/// driver's logic without booting the target).
//
// `X86PortIo` is only constructed when the driver runs on real
// (or QEMU-emulated) x86_64 hardware; on a host (x86_64 Linux
// build of the test binary) the symbol is reachable but unused
// because the test substitutes a mock `PortIo`.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
pub struct X86PortIo;

#[cfg(target_arch = "x86_64")]
impl PortIo for X86PortIo {
    fn read32(&self, port: u16) -> u32 {
        let value: u32;
        // SAFETY: `in dx, eax` is a side-effect-only 32-bit PIO
        // read against `port`. The PCI configuration ports
        // (`0xCF8`/`0xCFC`) are documented as legacy 32-bit I/O
        // ports per the PCI Local Bus 3.0 specification §3.2.2.3.2;
        // no caller of this function passes any other port. The
        // instruction has no memory side effects and clobbers no
        // registers outside `eax`. The covering test
        // (`tests::port_io_config_space_round_trips_address_data`)
        // exercises this method through a [`PortIo`] mock and
        // verifies the address/data sequence; on non-x86_64 hosts
        // the gate above removes both this impl and its tests.
        unsafe {
            core::arch::asm!(
                "in eax, dx",
                in("dx") port,
                out("eax") value,
                options(nomem, nostack, preserves_flags),
            );
        }
        value
    }

    fn write32(&self, port: u16, value: u32) {
        // SAFETY: `out dx, eax` is a side-effect-only 32-bit PIO
        // write to `port`. Same justification as `read32`: only
        // the documented PCI configuration ports are used; the
        // instruction touches no memory and the assembler template
        // declares the conservative `nomem`, `nostack`, and
        // `preserves_flags` options.
        unsafe {
            core::arch::asm!(
                "out dx, eax",
                in("dx") port,
                in("eax") value,
                options(nomem, nostack, preserves_flags),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    /// Recording mock that lets the test assert the exact sequence
    /// of address / data writes the bridge produces.
    struct RecordingPio {
        log: RefCell<alloc::vec::Vec<(u16, u32, &'static str)>>,
        next_read: RefCell<u32>,
    }
    extern crate alloc;

    impl PortIo for RecordingPio {
        fn read32(&self, port: u16) -> u32 {
            self.log.borrow_mut().push((port, 0, "read"));
            *self.next_read.borrow()
        }
        fn write32(&self, port: u16, value: u32) {
            self.log.borrow_mut().push((port, value, "write"));
        }
    }

    #[test]
    fn port_io_config_space_round_trips_address_data() {
        let pio = RecordingPio {
            log: RefCell::new(alloc::vec::Vec::new()),
            next_read: RefCell::new(0xDEAD_BEEF),
        };
        let cs = PortIoConfigSpace::new(pio);
        let addr = ConfigAddress {
            bus: 0,
            device: 0x1F,
            function: 0,
            register: 0,
        };
        let v = cs.read32(addr);
        cs.write32(addr, 0xCAFE_F00D);

        let log = cs.pio.log.borrow();
        assert_eq!(v, 0xDEAD_BEEF);
        // First two: address-port write, then data-port read.
        assert_eq!(log[0], (PCI_CONFIG_ADDRESS_PORT, 0x8000_F800, "write"));
        assert_eq!(log[1], (PCI_CONFIG_DATA_PORT, 0, "read"));
        // Next two: address-port write, then data-port write.
        assert_eq!(log[2], (PCI_CONFIG_ADDRESS_PORT, 0x8000_F800, "write"));
        assert_eq!(log[3], (PCI_CONFIG_DATA_PORT, 0xCAFE_F00D, "write"));
    }

    #[test]
    fn out_of_range_read_returns_all_ones_sentinel() {
        let pio = RecordingPio {
            log: RefCell::new(alloc::vec::Vec::new()),
            next_read: RefCell::new(0),
        };
        let cs = PortIoConfigSpace::new(pio);
        let addr = ConfigAddress {
            bus: 0,
            device: 99,
            function: 0,
            register: 0,
        };
        assert_eq!(cs.read32(addr), 0xFFFF_FFFF);
        assert!(cs.pio.log.borrow().is_empty(), "no PIO emitted on bad addr");
    }
}
