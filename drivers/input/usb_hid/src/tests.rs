//! Unit tests for the USB-HID driver's §8 `register` entry and §18.3 bind
//! table. The boot-report decode/console logic is tested in `lib/hid`.

use super::*;
use rustos_abi::driver::DriverKind;

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
fn bind_table_class_matches_any_hid_boot_device() {
    // Two class-wildcard keys: an HID boot keyboard and an HID boot
    // mouse, each binding any vendor/product at that class.
    assert_eq!(BIND_KEYS.len(), 2);
    assert!(BIND_KEYS.iter().all(|k| k.priority == BIND_PRIORITY));
    let kbd = HwMatchKey::usb(0x046D, 0xC52B, HID_BOOT_KEYBOARD_CLASS);
    let mouse = HwMatchKey::usb(0x045E, 0x0040, HID_BOOT_MOUSE_CLASS);
    assert!(BIND_KEYS.iter().any(|k| k.key.matches(&kbd)));
    assert!(BIND_KEYS.iter().any(|k| k.key.matches(&mouse)));
    // A non-boot HID interface (report protocol, sub-class 0x00) does
    // not match the boot-protocol bind table.
    let report_kbd = HwMatchKey::usb(0x046D, 0xC52B, 0x03_00_01);
    assert!(!BIND_KEYS.iter().any(|k| k.key.matches(&report_kbd)));
}
