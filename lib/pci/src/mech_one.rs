//! PCI configuration-access mechanism #1 (PIO bridge).
//!
//! The bridge drives the legacy configuration ports `0xCF8`/`0xCFC`
//! through the architecture-neutral [`PortIo`] seam
//! ([`tairix_abi::PortIo`]). The seam keeps
//! the `in`/`out` instructions — and the only `unsafe` they require —
//! inside the architecture port (`kernel/arch/x86_64`), so this driver
//! carries neither inline assembly nor a target-conditional `cfg` gate:
//! the unit tests drive the bridge through a recording mock and the
//! ring-0 bring-up path hands it the x86_64 backend.
//
// Same `dead_code` rationale as `config.rs`: the production reach
// path is through `dyn Bus` dispatch wired up by the driver host;
// the in-crate test module covers every helper directly.
#![allow(dead_code)]

use tairix_abi::driver::port_io::PortIo;

use crate::config::{ConfigAddress, ConfigSpace};

const PCI_CONFIG_ADDRESS_PORT: u16 = 0xCF8;
const PCI_CONFIG_DATA_PORT: u16 = 0xCFC;

/// Concrete [`ConfigSpace`] using a [`PortIo`] backend.
///
/// The bridge is parameterised on `P: PortIo` so the test suite
/// substitutes a recording mock; the production wiring uses the
/// x86_64 backend supplied by the architecture port.
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
