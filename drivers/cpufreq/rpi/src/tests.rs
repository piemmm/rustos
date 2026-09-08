//! Host unit tests for the Raspberry Pi CPU frequency driver, driven against
//! the protocol-faithful `lib/vcmailbox` mock firmware.
//!
//! QEMU models no `VideoCore`, so the range discovery, the applied-rate
//! reporting, and every fail-closed path are proven here; the live property
//! channel is the on-metal acceptance item (`plans/CPUFREQ.md`).

use core::cell::RefCell;

use tairix_abi::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};
use tairix_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwMatchKey, HW_COMPATIBLE_MAX,
};
use tairix_vcmailbox::mock::MockFirmware;
use tairix_vcmailbox::MailboxTransport;

use super::{register, RpiCpuFreq, BIND_KEYS, FIRMWARE_CLOCKS_COMPATIBLE, RATE_INTERVAL_HZ};

/// A channel backed by the mock firmware, adapting its `&mut self` transport
/// onto the `&self` channel the driver uses — the same `RefCell` shape the
/// production `vcmailbox` service uses for the same reason.
struct MockChannel(RefCell<MockFirmware>);

impl MockChannel {
    fn new(firmware: MockFirmware) -> Self {
        Self(RefCell::new(firmware))
    }
}

impl MailboxChannel for MockChannel {
    fn exchange(&self, message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
        self.0
            .borrow_mut()
            .exchange(message)
            .map_err(tairix_vcmailbox::MailboxError::as_driver_error)
    }
}

/// A channel whose transport always fails closed, modelling a mailbox service
/// that is absent or a doorbell that timed out.
struct DeadChannel;

impl MailboxChannel for DeadChannel {
    fn exchange(&self, _message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
        Err(DriverError::DeviceFault)
    }
}

/// A channel that returns `Ok` but leaves the message untouched, modelling
/// the firmware revisions that stamp success while never processing the tag.
struct SilentChannel;

impl MailboxChannel for SilentChannel {
    fn exchange(&self, _message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
        Ok(())
    }
}

struct MockHost {
    granted: bool,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.granted && cap == CapabilityId::DRV_LOAD
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

/// A Pi 4B's stock range: 600 MHz to 1.5 GHz, running at the minimum — the
/// state the board is actually found in, and the defect this driver closes.
fn pi4b_at_minimum() -> MockFirmware {
    let mut fw = MockFirmware::healthy();
    fw.arm_clock_min_hz = 600_000_000;
    fw.arm_clock_max_hz = 1_500_000_000;
    fw.arm_clock_hz = 600_000_000;
    fw
}

#[test]
fn register_requires_the_load_capability() {
    assert!(register(&MockHost { granted: true }).is_ok());
    assert_eq!(
        register(&MockHost { granted: false }),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn the_bind_table_names_the_device_tree_identity() {
    assert_eq!(BIND_KEYS.len(), 1);
    let expected = HwMatchKey::compatible(FIRMWARE_CLOCKS_COMPATIBLE).expect("fits");
    assert_eq!(BIND_KEYS[0].key, expected);
    assert!(FIRMWARE_CLOCKS_COMPATIBLE.len() <= HW_COMPATIBLE_MAX);
}

#[test]
fn the_range_comes_from_the_firmware_not_a_board_constant() {
    let driver = RpiCpuFreq::new(MockChannel::new(pi4b_at_minimum()));
    let limits = driver.limits().expect("the firmware reports its range");
    assert_eq!(limits.min_hz, 600_000_000);
    assert_eq!(limits.max_hz, 1_500_000_000);
    assert_eq!(limits.step_hz, RATE_INTERVAL_HZ);
}

#[test]
fn an_overclocked_board_reports_its_own_ceiling() {
    // A `config.txt` that raises `arm_freq` must be driven over the range it
    // actually has, which is why neither end is a constant here.
    let mut fw = pi4b_at_minimum();
    fw.arm_clock_max_hz = 2_000_000_000;
    let driver = RpiCpuFreq::new(MockChannel::new(fw));
    assert_eq!(driver.limits().expect("range").max_hz, 2_000_000_000);
}

#[test]
fn a_board_with_one_usable_rate_declares_a_legal_range() {
    // A range narrower than the interval worth asking at must still produce
    // limits the governor accepts, rather than a step wider than the range.
    let mut fw = pi4b_at_minimum();
    fw.arm_clock_min_hz = 1_000_000_000;
    fw.arm_clock_max_hz = 1_000_000_000;
    fw.arm_clock_hz = 1_000_000_000;
    let driver = RpiCpuFreq::new(MockChannel::new(fw));
    let limits = driver.limits().expect("a one-rate range is legal");
    assert_eq!(limits.min_hz, limits.max_hz);
}

#[test]
fn applying_a_rate_reports_what_the_firmware_actually_did() {
    // The firmware clamps and rounds, so echoing the request back would
    // report a frequency the cores are not running at.
    let channel = MockChannel::new(pi4b_at_minimum());
    let driver = RpiCpuFreq::new(channel);
    assert_eq!(driver.apply(1_500_000_000), Ok(1_500_000_000));
    assert_eq!(driver.current(), Ok(1_500_000_000));
    // Above the clock's range but inside the property interface's 32-bit rate
    // field, so the firmware — not this driver — is what clamps it.
    assert_eq!(
        driver.apply(3_000_000_000),
        Ok(1_500_000_000),
        "a target above the range is clamped by the firmware, and reported so"
    );
}

#[test]
fn the_rate_the_governor_asks_for_reaches_the_hardware() {
    // The end the reported defect turns on: the board sits at 600 MHz and
    // must actually move when the governor asks for full speed.
    let mut fw = pi4b_at_minimum();
    fw.arm_clock_grain_hz = 1;
    let driver = RpiCpuFreq::new(MockChannel::new(fw));
    assert_eq!(driver.current(), Ok(600_000_000), "as the board is found");
    assert_eq!(driver.apply(1_500_000_000), Ok(1_500_000_000));
    assert_eq!(driver.current(), Ok(1_500_000_000));
}

#[test]
fn a_target_wider_than_the_firmware_rate_field_is_refused() {
    // The property interface carries a 32-bit rate; a target that does not
    // fit is refused rather than truncated into a rate the firmware would
    // apply.
    let driver = RpiCpuFreq::new(MockChannel::new(pi4b_at_minimum()));
    assert_eq!(
        driver.apply(u64::from(u32::MAX) + 1),
        Err(DriverError::OutOfRange)
    );
}

#[test]
fn an_unreachable_firmware_fails_closed() {
    let driver = RpiCpuFreq::new(DeadChannel);
    assert_eq!(driver.limits(), Err(DriverError::DeviceFault));
    assert_eq!(driver.current(), Err(DriverError::DeviceFault));
    assert_eq!(driver.apply(1_500_000_000), Err(DriverError::DeviceFault));
}

#[test]
fn a_firmware_that_never_processes_the_tag_is_a_fault_not_a_reading() {
    // The documented Pi fault: the top-level success code is stamped while
    // the tag goes unprocessed. Reading that as a rate would report the
    // request's own zero slot as a stopped clock.
    let driver = RpiCpuFreq::new(SilentChannel);
    assert_eq!(driver.current(), Err(DriverError::BadMagic));
    assert_eq!(driver.limits(), Err(DriverError::BadMagic));
    assert_eq!(driver.apply(1_500_000_000), Err(DriverError::BadMagic));
}

#[test]
fn a_firmware_that_does_not_know_the_clock_is_a_fault_not_a_stopped_clock() {
    // Zero is how the property interface spells "no such clock". Declaring a
    // range with a zero end would ask the governor to pick a stopped rate.
    let mut fw = pi4b_at_minimum();
    fw.arm_clock_min_hz = 0;
    fw.arm_clock_max_hz = 0;
    fw.arm_clock_hz = 0;
    let driver = RpiCpuFreq::new(MockChannel::new(fw));
    assert_eq!(driver.current(), Err(DriverError::DeviceFault));
    assert_eq!(driver.limits(), Err(DriverError::DeviceFault));
}

#[test]
fn an_inverted_firmware_range_is_refused_rather_than_second_guessed() {
    let mut fw = pi4b_at_minimum();
    fw.arm_clock_min_hz = 1_500_000_000;
    fw.arm_clock_max_hz = 600_000_000;
    let driver = RpiCpuFreq::new(MockChannel::new(fw));
    assert_eq!(driver.limits(), Err(DriverError::OutOfRange));
}
