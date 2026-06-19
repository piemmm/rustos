//! Unit tests for the `drivers/bus/usb/xhci` driver entry and bind table.
//!
//! The bus-agnostic xHCI protocol (the `Xhci` engine, the ring/TRB
//! vocabulary, and `UsbDevice` enumeration) is proven in `lib/usb`; these
//! tests cover the pieces that live *here* — the §8 `register` entry's
//! capability gate and the §18.3 `BIND_KEYS` match table.

use rustos_abi::{CapabilityId, DriverError, DriverHost, DriverKind, HwMatchKey};

use super::{register, BIND_KEYS, BIND_PRIORITY, XHCI_PCI_CLASS};

/// Mock driver host modelling the load-time `CAP_DRV_LOAD` grant.
struct MockHost {
    drv_load: bool,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        match cap {
            CapabilityId::DRV_LOAD => self.drv_load,
            _ => false,
        }
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

#[test]
fn register_requires_drv_load() {
    assert!(register(&MockHost { drv_load: true }).is_ok());
    assert_eq!(
        register(&MockHost { drv_load: false }),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn bind_table_class_matches_any_xhci_controller() {
    // One class-wildcard key: the VL805 (and any other xHCI host) binds
    // by class alone, whatever its vendor/device id.
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    let vl805 = HwMatchKey::pci(0x1106, 0x3483, XHCI_PCI_CLASS);
    let other = HwMatchKey::pci(0x8086, 0x1111, XHCI_PCI_CLASS);
    assert!(BIND_KEYS[0].key.matches(&vl805));
    assert!(BIND_KEYS[0].key.matches(&other));
    // A USB controller of a different prog-if (EHCI, `0x0C0320`) is not
    // an xHCI host and must not bind.
    let ehci = HwMatchKey::pci(0x1106, 0x3483, 0x0C_03_20);
    assert!(!BIND_KEYS[0].key.matches(&ehci));
}
