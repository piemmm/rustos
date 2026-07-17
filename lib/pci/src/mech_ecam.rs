//! `PCIe` enhanced configuration access mechanism (ECAM / MMCONFIG).
//!
//! Where mechanism #1 ([`crate::mech_one`]) reaches configuration
//! space through the x86 legacy I/O ports, ECAM maps it flat into
//! MMIO: a contiguous physical region in which each
//! `(bus, device, function)` owns a 4 KiB block (PCI Express Base 3.0
//! §7.2.2). Reading or writing a configuration dword is then a single
//! naturally-aligned access at the computed offset
//! ([`ConfigAddress::ecam_offset`]).
//!
//! This is the path the Raspberry Pi 4 (BCM2711) root complex — and
//! every other `PCIe` host bridge that has no I/O-port space — uses to
//! reach its devices (the VL805 USB host controller, for the Pi).
//! The driver never synthesises a pointer: the configuration region
//! is reached through a kernel-mapped [`RegisterWindow`] obtained
//! from the MMIO-map facility after a
//! [`CapabilityId::MMIO_MAP`](tairix_abi::CapabilityId::MMIO_MAP)
//! check, exactly like a device's BAR window. The
//! caller passes that window to [`crate::mechanism_ecam`]; the
//! window's bounds checking turns any access beyond the mapped region
//! into the PCI "no device" sentinel, so a walk past the mapped buses
//! fails closed rather than reading out of bounds.
//
// Same `dead_code` rationale as `config.rs` / `mech_one.rs`: the
// production reach path is through `dyn Bus` dispatch wired up by the
// driver host (via `crate::mechanism_ecam`); the in-crate test module
// covers every helper directly.
#![allow(dead_code)]

use tairix_abi::RegisterWindow;

use crate::config::{ConfigAddress, ConfigSpace};

/// A [`ConfigSpace`] backed by a memory-mapped ECAM region.
///
/// Holds the kernel-mapped [`RegisterWindow`] over the host bridge's
/// configuration region. The window's base is the physical base of
/// `(bus 0, device 0, function 0, register 0)`; every access is the
/// flat [`ConfigAddress::ecam_offset`] within it.
pub struct EcamConfigSpace {
    window: RegisterWindow,
}

impl EcamConfigSpace {
    /// Construct an [`EcamConfigSpace`] over the configuration-region
    /// `window`.
    ///
    /// The window must cover the configuration region the caller
    /// intends to enumerate; accesses beyond its length are reported
    /// as "no device" (all-ones) rather than reaching past the
    /// mapping.
    #[must_use]
    pub const fn new(window: RegisterWindow) -> Self {
        Self { window }
    }
}

impl ConfigSpace for EcamConfigSpace {
    fn read32(&self, addr: ConfigAddress) -> u32 {
        // Out-of-range fields (a malformed `ConfigAddress`) and any
        // offset beyond the mapped region both resolve to the PCI
        // Local Bus 3.0 §6.1 "no function present" sentinel, so the
        // enumeration walk treats them as an empty slot and fails
        // closed.
        let Some(offset) = addr.ecam_offset() else {
            return 0xFFFF_FFFF;
        };
        self.window.read_u32(offset).unwrap_or(0xFFFF_FFFF)
    }

    fn write32(&self, addr: ConfigAddress, value: u32) {
        // A malformed address or an offset beyond the mapped region is
        // dropped: there is no register there to write.
        let Some(offset) = addr.ecam_offset() else {
            return;
        };
        let _ = self.window.write_u32(offset, value);
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::ptr::NonNull;

    /// Build a [`RegisterWindow`] over a freshly-allocated, zeroed
    /// ECAM region of `words` dwords. The returned `Vec` owns the
    /// backing and must outlive the window.
    fn ecam_region(words: usize) -> (Vec<u32>, RegisterWindow) {
        let mut backing = vec![0u32; words];
        let base = NonNull::new(backing.as_mut_ptr().cast::<u8>()).expect("non-null heap buffer");
        let len = backing.len() * 4;
        // SAFETY: `base` is 4-byte aligned (the `Vec<u32>` allocation
        // guarantee) and covers exactly `len` bytes; the backing `Vec`
        // is returned to the caller so it outlives the window, and no
        // other reference aliases it while the window is live.
        let window = unsafe { RegisterWindow::from_mapping(0xF800_0000, base, len) };
        (backing, window)
    }

    #[test]
    fn read_resolves_ecam_offset() {
        // One full bus block (1 MiB) is plenty for bus 0.
        let (mut backing, window) = ecam_region(0x10_0000 / 4);
        // Plant a vendor/device dword at 00:1f.3 register 0.
        let addr = ConfigAddress {
            bus: 0,
            device: 0x1F,
            function: 3,
            register: 0,
        };
        let off = addr.ecam_offset().expect("in range");
        backing[off / 4] = 0x2930_8086;
        let cs = EcamConfigSpace::new(window);
        assert_eq!(cs.read32(addr), 0x2930_8086);
    }

    #[test]
    fn write_then_read_round_trips() {
        let (_backing, window) = ecam_region(0x10_0000 / 4);
        let cs = EcamConfigSpace::new(window);
        let addr = ConfigAddress {
            bus: 0,
            device: 5,
            function: 0,
            register: 4,
        };
        cs.write32(addr, 0xCAFE_F00D);
        assert_eq!(cs.read32(addr), 0xCAFE_F00D);
    }

    #[test]
    fn access_beyond_window_reads_no_device_sentinel() {
        // A region covering only bus 0; bus 1 lies past its end.
        let (_backing, window) = ecam_region(0x10_0000 / 4);
        let cs = EcamConfigSpace::new(window);
        let bus1 = ConfigAddress {
            bus: 1,
            device: 0,
            function: 0,
            register: 0,
        };
        assert_eq!(cs.read32(bus1), 0xFFFF_FFFF);
        // The out-of-bounds write is dropped (no panic, no growth).
        cs.write32(bus1, 0x1234_5678);
        assert_eq!(cs.read32(bus1), 0xFFFF_FFFF);
    }

    #[test]
    fn out_of_range_address_reads_no_device_sentinel() {
        let (_backing, window) = ecam_region(0x10_0000 / 4);
        let cs = EcamConfigSpace::new(window);
        let bad = ConfigAddress {
            bus: 0,
            device: 99,
            function: 0,
            register: 0,
        };
        assert_eq!(cs.read32(bad), 0xFFFF_FFFF);
    }
}
