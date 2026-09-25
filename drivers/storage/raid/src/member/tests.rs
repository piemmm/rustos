//! Host tests for lending a member's data window to one client at a time.

use super::{MemberDevice, MemberWindow};
use crate::testkit::{MemberDisk, DEVICE_BLOCKS};

use tairix_abi::driver::block::Block;
use tairix_abi::driver::DriverError;

#[test]
fn a_window_is_lent_to_one_client_at_a_time() {
    let window = MemberWindow::new();
    let lease = window.lend().expect("a free window is lent");
    assert!(window.is_lent());
    assert!(
        window.lend().is_none(),
        "a second client over the same window would be a second exclusive view of its bytes"
    );
    drop(lease);
    assert!(!window.is_lent(), "dropping the loan returns the window");
    assert!(window.lend().is_some());
}

#[test]
fn dropping_a_member_device_returns_its_window() {
    let window = MemberWindow::new();
    let lease = window.lend().expect("a free window is lent");
    let device = MemberDevice::new(MemberDisk::new(DEVICE_BLOCKS), lease);
    assert!(window.is_lent());
    drop(device);
    assert!(!window.is_lent());
}

#[test]
fn a_departed_device_refuses_every_transfer_without_reaching_the_device() {
    let window = MemberWindow::new();
    let disk = MemberDisk::new(DEVICE_BLOCKS);
    let lease = window.lend().expect("a free window is lent");
    let mut device = MemberDevice::new(disk.clone(), lease);
    let block = [0xA5u8; 512];
    device
        .write_blocks(3, &block)
        .expect("a live member writes through");

    window.depart();
    assert!(device.departed());
    assert_eq!(
        device.write_blocks(4, &block),
        Err(DriverError::DeviceOffline)
    );
    assert!(disk.block_is_blank(4), "the write never reached the device");
    let mut read = [0u8; 512];
    assert_eq!(
        device.read_blocks(3, &mut read),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(read, [0u8; 512], "nor did the read");
    assert_eq!(device.flush(), Err(DriverError::DeviceOffline));
    assert!(
        device.geometry().is_ok(),
        "what the device is stays known; only reaching it is refused"
    );
}
