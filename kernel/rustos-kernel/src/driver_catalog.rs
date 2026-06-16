//! In-kernel production driver-candidate catalogue (PLAN Stage 4.HW item 5;
//! `plans/PI.md` P10 5c).
//!
//! This is the data-driven replacement for hand-sequenced board bring-up.
//! Each driver of the Pi 4 USB chain publishes its hardware bind table as a
//! canonical `pub const BIND_KEYS` (`AGENTS.md` §18.3); the catalogue pairs
//! each table with the driver's logical `/System/Drivers/` image path and
//! lets the shared [`rustos_devmatch`] resolver decide which driver binds a
//! discovered hardware-tree node — exactly as the user-space `devmgr`
//! autoloader does, against the same match policy (`AGENTS.md` §2.2).
//!
//! The catalogue is **authored from the driver crates' `BIND_KEYS`**, never
//! re-typed: adding or retiring a chain driver is one entry here, and the
//! match data stays the driver's own (`AGENTS.md` §2.2 / §18.5).
//!
//! # Why in-kernel, for now
//!
//! The steady state is a user-space `devmgr` autoloading user-space driver
//! processes over a capability-gated driver-host-IPC surface (PLAN Stage
//! 4.HW increment 1 / `plans/PI.md` P10 5d). Until that surface lands the
//! Pi 4 USB chain is brought up in-kernel, but the **bring-up decision** is
//! still made by matching the discovered node against this catalogue — so
//! the kernel no longer *hunts* for a keyboard; it brings the bus up
//! because a driver's bind table matched a discovered device, and leaves an
//! unmatched node unbound and logged (`AGENTS.md` §18.4). The load
//! *mechanism* (routing through the signed-manifest `Host::load` gate with
//! `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`) is the next sub-increment (5c-ii).

use rustos_abi::HwMatchKey;
use rustos_devmatch::{resolve, DriverCandidate, MatchResolution};

/// Logical image path of the BCM2711 PCIe root-complex driver
/// (`drivers/bus/pcie_brcm`).
pub const PCIE_BRCM_PATH: &str = "/System/Drivers/pcie_brcm";

/// Logical image path of the xHCI USB host-controller bus driver
/// (`drivers/bus/usb`).
pub const BUS_USB_PATH: &str = "/System/Drivers/bus_usb";

/// Logical image path of the HID boot-protocol input driver
/// (`drivers/input/usb_hid`).
pub const USB_HID_PATH: &str = "/System/Drivers/usb_hid";

/// The production driver-candidate catalogue for the Pi 4 USB chain.
///
/// One entry per chain driver, pairing its logical image path with the
/// driver crate's own canonical `BIND_KEYS` (`AGENTS.md` §18.3). The bind
/// tables are borrowed `'static` from the driver crates, so the catalogue
/// is the single source of truth `devmgr`-style matching resolves a
/// discovered node against — never a re-typed copy (`AGENTS.md` §2.2).
#[must_use]
pub fn chain_candidates() -> [DriverCandidate<'static>; 3] {
    [
        DriverCandidate {
            path: PCIE_BRCM_PATH,
            bind_keys: rustos_drv_bus_pcie_brcm::BIND_KEYS,
        },
        DriverCandidate {
            path: BUS_USB_PATH,
            bind_keys: rustos_drv_bus_usb::BIND_KEYS,
        },
        DriverCandidate {
            path: USB_HID_PATH,
            bind_keys: rustos_drv_input_usb_hid::BIND_KEYS,
        },
    ]
}

/// Resolve a discovered node's `match_keys` against the chain catalogue.
///
/// Returns the deterministic [`MatchResolution`] (`AGENTS.md` §18.3): the
/// strictly highest-priority matching driver wins, an unbroken tie across
/// distinct drivers is a packaging defect, and a node matching nothing is
/// [`MatchResolution::Unmatched`] — left unbound by the caller (§18.4).
#[must_use]
pub fn resolve_chain(node_keys: &[HwMatchKey]) -> MatchResolution {
    resolve(node_keys, &chain_candidates())
}

/// Build the discovered PCIe root-complex node's `compatible` match key
/// from the discovery-contract identity string
/// (`kernel/arch/aarch64::platform::PCIE_COMPATIBLE`).
///
/// This reconstructs the *discovered* node's identity — the `compatible`
/// string the device tree advertised and the platform discovery verified
/// before yielding the bring-up windows — not the driver's bind key, so the
/// match against [`chain_candidates`] is a genuine node↔driver resolution
/// (`AGENTS.md` §18.5). Returns [`None`] fail-closed if the string does not
/// fit a [`HwMatchKey`] (`AGENTS.md` §2.9); the caller then leaves the node
/// unbound rather than guessing.
#[must_use]
pub fn bridge_compatible_key(compatible: &[u8]) -> Option<HwMatchKey> {
    HwMatchKey::compatible(compatible).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The BCM2711 PCIe root complex advertises this `compatible` string
    /// (mirrors `kernel/arch/aarch64::platform::PCIE_COMPATIBLE`).
    const PCIE_COMPATIBLE: &[u8] = b"brcm,bcm2711-pcie";

    /// The 24-bit xHCI PCI class (`base 0x0C`, sub `0x03`, prog-if `0x30`).
    const XHCI_CLASS: u32 = 0x0C_03_30;

    /// The HID boot-keyboard / boot-mouse 24-bit USB interface classes.
    const HID_KEYBOARD_CLASS: u32 = 0x03_01_01;
    const HID_MOUSE_CLASS: u32 = 0x03_01_02;

    fn winner_path(resolution: MatchResolution) -> Option<&'static str> {
        match resolution {
            MatchResolution::Winner { candidate, .. } => Some(chain_candidates()[candidate].path),
            _ => None,
        }
    }

    #[test]
    fn discovered_pcie_bridge_binds_the_root_complex_driver() {
        let key = bridge_compatible_key(PCIE_COMPATIBLE).expect("the contract string fits");
        assert_eq!(
            winner_path(resolve_chain(&[key])),
            Some(PCIE_BRCM_PATH),
            "the discovered brcm,bcm2711-pcie node must bind pcie_brcm"
        );
    }

    #[test]
    fn enumerated_vl805_function_binds_the_usb_bus_driver() {
        // The VL805 xHCI function node the PCI bus emits
        // (`PciBus::describe_function`): a concrete vendor/device with the
        // full 24-bit xHCI class. The bus driver's class-wildcard bind key
        // claims it without hard-coding the device id (`AGENTS.md` §18.5).
        let vl805 = [HwMatchKey::pci(0x1106, 0x3483, XHCI_CLASS)];
        assert_eq!(winner_path(resolve_chain(&vl805)), Some(BUS_USB_PATH));
    }

    #[test]
    fn enumerated_hid_interface_binds_the_input_driver() {
        // The HID child node the USB host emits (`UsbDevice::describe_device`)
        // keyed by its interface class. Both boot classes bind usb_hid.
        let keyboard = [HwMatchKey::usb(0x3434, 0x0E21, HID_KEYBOARD_CLASS)];
        let mouse = [HwMatchKey::usb(0x046D, 0xC52B, HID_MOUSE_CLASS)];
        assert_eq!(winner_path(resolve_chain(&keyboard)), Some(USB_HID_PATH));
        assert_eq!(winner_path(resolve_chain(&mouse)), Some(USB_HID_PATH));
    }

    #[test]
    fn an_unrelated_device_is_left_unmatched() {
        // An AHCI storage controller matches no chain driver: the caller
        // leaves it unbound and logged (`AGENTS.md` §18.4), never a guess.
        let ahci = [HwMatchKey::pci(0x8086, 0x2922, 0x01_06_01)];
        assert_eq!(resolve_chain(&ahci), MatchResolution::Unmatched);
        // A different-protocol HID device (a non-boot interface) also fails
        // the boot-class wildcard and is unmatched.
        let non_boot_hid = [HwMatchKey::usb(0x3434, 0x0E21, 0x03_00_00)];
        assert_eq!(resolve_chain(&non_boot_hid), MatchResolution::Unmatched);
    }

    #[test]
    fn the_catalogue_is_authored_from_the_driver_crates_bind_keys() {
        // The catalogue must borrow each driver's own published table, so
        // the match data can never drift from the signed-manifest source
        // (`AGENTS.md` §2.2 / §18.3).
        let candidates = chain_candidates();
        assert_eq!(candidates[0].bind_keys, rustos_drv_bus_pcie_brcm::BIND_KEYS);
        assert_eq!(candidates[1].bind_keys, rustos_drv_bus_usb::BIND_KEYS);
        assert_eq!(candidates[2].bind_keys, rustos_drv_input_usb_hid::BIND_KEYS);
    }

    #[test]
    fn a_too_long_compatible_string_fails_closed() {
        // 256 bytes overruns HW_COMPATIBLE_MAX: no key, the caller leaves
        // the node unbound rather than fabricating one (`AGENTS.md` §2.9).
        let oversized = [b'a'; 256];
        assert!(bridge_compatible_key(&oversized).is_none());
    }
}
