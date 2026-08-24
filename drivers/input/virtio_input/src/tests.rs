//! virtio-input driver-shell unit tests: the `register` gate and the
//! bind table. The open/poll/decode device-logic tests live with
//! the logic in `lib/virtio_input`.

use super::*;
use tairix_abi::{CapabilityId, DriverError};

#[test]
fn register_requires_drv_load() {
    struct H {
        grant: bool,
    }
    impl DriverHost for H {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            cap == CapabilityId::DRV_LOAD && self.grant
        }
        fn kind(&self) -> tairix_abi::driver::DriverKind {
            tairix_abi::driver::DriverKind::UserSpace
        }
    }
    assert_eq!(
        register(&H { grant: false }),
        Err(DriverError::PermissionDenied)
    );
    assert!(register(&H { grant: true }).is_ok());
}

#[test]
fn bind_table_matches_a_virtio_input_node() {
    // One entry at the declared exact-match priority, matching a
    // discovered virtio node whose probed device id is `virtio-input`.
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    let input = HwMatchKey::virtio(VIRTIO_INPUT_DEVICE_ID);
    assert!(BIND_KEYS[0].key.matches(&input));

    // A different virtio device (e.g. virtio-blk, device id 2) and a
    // non-virtio node both fail the match — the caller leaves them
    // unbound rather than guessing.
    let blk = HwMatchKey::virtio(2);
    assert!(!BIND_KEYS[0].key.matches(&blk));
    let pci_input = HwMatchKey::pci(0x1234, 0x5678, 0x09_00_00);
    assert!(!BIND_KEYS[0].key.matches(&pci_input));
}
