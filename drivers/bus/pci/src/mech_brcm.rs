//! BCM2711 windowed configuration access mechanism.
//!
//! The BCM2711 (Raspberry Pi 4) PCIe root complex does **not** map
//! configuration space flat the way standard ECAM ([`crate::mech_ecam`])
//! does. Instead it exposes a two-register *windowed* indirection inside
//! the controller's own register block:
//!
//! * the root bus's own configuration header (`bus 0`, `devfn 0`) is read
//!   directly at the controller base plus the register byte offset; any
//!   other function on bus 0 is "no device";
//! * a downstream function (`bus >= 1`, e.g. the VL805 USB controller at
//!   `01:00.0`) is reached by writing its block index — the same
//!   `(bus << 20) | (devfn << 12)` ECAM block address
//!   [`ConfigAddress::ecam_offset`] computes — into the
//!   [`EXT_CFG_INDEX`] register, then accessing the dword through the
//!   4 KiB [`EXT_CFG_DATA`] window at the register byte offset.
//!
//! This windowing is the *only* BCM2711-specific knowledge here; the
//! enumeration, BAR sizing, and capability walk above it
//! ([`crate::enumerate`]) are the generic PCI core, so the BCM2711 PCIe
//! host-bridge bring-up driver (`drivers/bus/pcie_brcm`) does not
//! re-implement any of it (`AGENTS.md` §2.2). The bring-up driver trains
//! the link and then hands its register window here through
//! [`crate::mechanism_brcm`].
//!
//! The window is a kernel-mapped [`RegisterWindow`] obtained from the
//! MMIO-map facility after a [`CapabilityId`](rustos_abi::CapabilityId)
//! check (`AGENTS.md` §4); the driver never synthesises a pointer. An
//! access that lands outside the mapped window — or a config access to a
//! device behind the bridge before the index write fits the window —
//! resolves to the PCI "no device" sentinel (all-ones), so a walk fails
//! closed rather than reaching out of bounds (`AGENTS.md` §5.4).

// Same `dead_code` rationale as `config.rs` / `mech_ecam.rs`: the
// production reach path is through `dyn Bus` dispatch wired up by the
// driver host (via `crate::mechanism_brcm`); the in-crate test module
// covers every helper directly.
#![allow(dead_code)]

use rustos_abi::RegisterWindow;

use crate::config::{ConfigAddress, ConfigSpace};

/// Controller-relative byte offset of the external configuration-space
/// **index** register (BCM2711 `PCIE_EXT_CFG_INDEX`). Writing a
/// `(bus << 20) | (devfn << 12)` block address here selects the
/// downstream function the [`EXT_CFG_DATA`] window then reaches.
pub const EXT_CFG_INDEX: usize = 0x9000;

/// Controller-relative byte offset of the external configuration-space
/// **data** window (BCM2711 `PCIE_EXT_CFG_DATA`): a 4 KiB aperture
/// mirroring the selected function's configuration space.
pub const EXT_CFG_DATA: usize = 0x8000;

/// A [`ConfigSpace`] backed by the BCM2711 windowed indirection.
///
/// Holds the kernel-mapped [`RegisterWindow`] over the PCIe controller's
/// register block — the same window the bring-up driver trained the link
/// through. The window base addresses the controller's register 0; the
/// root-bus configuration header lives at offset 0, the windowed config
/// registers at [`EXT_CFG_INDEX`] / [`EXT_CFG_DATA`].
pub struct BrcmConfigSpace {
    window: RegisterWindow,
}

impl BrcmConfigSpace {
    /// Construct a [`BrcmConfigSpace`] over the controller register
    /// `window`.
    ///
    /// The window must cover the controller register block through at
    /// least [`EXT_CFG_INDEX`] (`0x9000`); the device-tree advertises a
    /// `0x9310`-byte window for the `brcm,bcm2711-pcie` node, which the
    /// bring-up driver maps in full.
    #[must_use]
    pub const fn new(window: RegisterWindow) -> Self {
        Self { window }
    }

    /// Resolve the controller-relative byte offset a configuration dword
    /// is accessed through, performing the index write for a downstream
    /// function.
    ///
    /// Returns `None` for the PCI "no device" cases: a malformed
    /// address, or any function other than `devfn 0` on the root bus
    /// (the root complex presents only its own header there).
    fn data_offset(&self, addr: ConfigAddress) -> Option<usize> {
        // The shared range gate (`AGENTS.md` §5.4); also guarantees the
        // register byte offset is < 256, comfortably inside the 4 KiB
        // data window.
        let block = addr.ecam_offset()?;
        let reg = (addr.register as usize) << 2;
        if addr.bus == 0 {
            // The root complex's own configuration header is reached
            // directly; only function 0 of device 0 exists.
            if addr.device != 0 || addr.function != 0 {
                return None;
            }
            return Some(reg);
        }
        // Downstream: select the function's block (the bus/devfn part of
        // the ECAM block address, register stripped), then access it
        // through the data window.
        let index = u32::try_from(block & !0xFFF).ok()?;
        self.window.write_u32(EXT_CFG_INDEX, index).ok()?;
        Some(EXT_CFG_DATA + reg)
    }
}

impl ConfigSpace for BrcmConfigSpace {
    fn read32(&self, addr: ConfigAddress) -> u32 {
        let Some(offset) = self.data_offset(addr) else {
            return 0xFFFF_FFFF;
        };
        self.window.read_u32(offset).unwrap_or(0xFFFF_FFFF)
    }

    fn write32(&self, addr: ConfigAddress, value: u32) {
        let Some(offset) = self.data_offset(addr) else {
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
    /// controller register block of `bytes` bytes. The returned `Vec`
    /// owns the backing and must outlive the window.
    fn ctrl_window(bytes: usize) -> (Vec<u32>, RegisterWindow) {
        let mut backing = vec![0u32; bytes / 4];
        let base = NonNull::new(backing.as_mut_ptr().cast::<u8>()).expect("non-null heap buffer");
        let len = backing.len() * 4;
        // SAFETY: `base` is 4-byte aligned (the `Vec<u32>` allocation
        // guarantee) and covers exactly `len` bytes; the backing `Vec`
        // is returned to the caller so it outlives the window, and no
        // other reference aliases it while the window is live.
        let window = unsafe { RegisterWindow::from_mapping(0xFD50_0000, base, len) };
        (backing, window)
    }

    #[test]
    fn root_bus_reads_header_directly() {
        let (mut backing, window) = ctrl_window(0x9400);
        // Vendor/device of the root complex at bus 0, dev 0, fn 0,
        // register 0 — read straight from controller offset 0.
        backing[0] = 0x2711_14E4;
        let cs = BrcmConfigSpace::new(window);
        assert_eq!(
            cs.read32(ConfigAddress {
                bus: 0,
                device: 0,
                function: 0,
                register: 0,
            }),
            0x2711_14E4,
        );
    }

    #[test]
    fn root_bus_other_functions_are_no_device() {
        let (_backing, window) = ctrl_window(0x9400);
        let cs = BrcmConfigSpace::new(window);
        // Any device/function other than 00.0 on the root bus is empty.
        assert_eq!(
            cs.read32(ConfigAddress {
                bus: 0,
                device: 1,
                function: 0,
                register: 0,
            }),
            0xFFFF_FFFF,
        );
        assert_eq!(
            cs.read32(ConfigAddress {
                bus: 0,
                device: 0,
                function: 1,
                register: 0,
            }),
            0xFFFF_FFFF,
        );
    }

    #[test]
    fn downstream_access_writes_index_then_reads_data_window() {
        let (mut backing, window) = ctrl_window(0x9400);
        // Plant the VL805's vendor/device in the data window at the
        // register offset the driver will read (register 0 → byte 0).
        backing[EXT_CFG_DATA / 4] = 0x3483_1106;
        let cs = BrcmConfigSpace::new(window);
        let vl805 = ConfigAddress {
            bus: 1,
            device: 0,
            function: 0,
            register: 0,
        };
        assert_eq!(cs.read32(vl805), 0x3483_1106);
        // The index register was programmed with the bus/devfn block
        // address (bus 1, devfn 0 → 0x10_0000), register stripped.
        assert_eq!(backing[EXT_CFG_INDEX / 4], 0x10_0000);
    }

    #[test]
    fn downstream_register_offset_indexes_the_data_window() {
        let (mut backing, window) = ctrl_window(0x9400);
        // Register 4 (byte 0x10, BAR0) of a downstream function.
        backing[(EXT_CFG_DATA + 0x10) / 4] = 0xDEAD_BEEF;
        let cs = BrcmConfigSpace::new(window);
        assert_eq!(
            cs.read32(ConfigAddress {
                bus: 2,
                device: 3,
                function: 1,
                register: 4,
            }),
            0xDEAD_BEEF,
        );
        // Index = (2 << 20) | (3 << 15) | (1 << 12).
        assert_eq!(
            backing[EXT_CFG_INDEX / 4],
            (2 << 20) | (3 << 15) | (1 << 12),
        );
    }

    #[test]
    fn downstream_write_round_trips_through_the_window() {
        let (_backing, window) = ctrl_window(0x9400);
        let cs = BrcmConfigSpace::new(window);
        let addr = ConfigAddress {
            bus: 1,
            device: 0,
            function: 0,
            register: 1,
        };
        cs.write32(addr, 0x0000_0146);
        assert_eq!(cs.read32(addr), 0x0000_0146);
    }

    #[test]
    fn out_of_range_address_is_no_device() {
        let (_backing, window) = ctrl_window(0x9400);
        let cs = BrcmConfigSpace::new(window);
        assert_eq!(
            cs.read32(ConfigAddress {
                bus: 0,
                device: 99,
                function: 0,
                register: 0,
            }),
            0xFFFF_FFFF,
        );
    }

    #[test]
    fn access_beyond_window_reads_no_device_sentinel() {
        // A window that does not even cover the index register: every
        // downstream access fails closed rather than reaching past the
        // mapping.
        let (_backing, window) = ctrl_window(0x100);
        let cs = BrcmConfigSpace::new(window);
        assert_eq!(
            cs.read32(ConfigAddress {
                bus: 1,
                device: 0,
                function: 0,
                register: 0,
            }),
            0xFFFF_FFFF,
        );
    }
}
