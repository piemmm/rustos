//! Host unit tests for the Raspberry Pi RTC driver, driven against the
//! protocol-faithful `lib/vcmailbox` mock firmware.
//!
//! QEMU models no `VideoCore`, so the register selection, the health decode,
//! and every fail-closed path are proven here; the live property channel is
//! the on-metal acceptance item (`plans/TIMESYNC.md` TS-4).

use core::cell::RefCell;

use tairix_abi::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};
use tairix_abi::driver::rtc::Rtc;
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwMatchKey, HW_COMPATIBLE_MAX,
};
use tairix_vcmailbox::mock::MockFirmware;
use tairix_vcmailbox::{MailboxTransport, RtcRegister};

use super::{register, RpiRtc, BIND_KEYS, RPI_RTC_COMPATIBLE};

/// A channel backed by the mock firmware, adapting its `&mut self` transport
/// onto the `&self` channel the class seam uses — the same `RefCell` shape
/// the production `vcmailbox` service uses for the same reason.
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

/// A firmware holding `secs` on its counter and `mv` on its backup cell.
fn firmware(secs: u32, mv: u32) -> MockFirmware {
    let mut fw = MockFirmware::healthy();
    fw.rtc_secs = secs;
    fw.rtc_backup_mv = mv;
    fw
}

/// 2026-08-30T00:00:00Z — past the 32-bit *signed* boundary and inside the
/// counter's unsigned range.
const SAMPLE_SECS: u32 = 1_787_011_200;

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
    let expected = HwMatchKey::compatible(RPI_RTC_COMPATIBLE).expect("fits");
    assert_eq!(BIND_KEYS[0].key, expected);
    assert!(RPI_RTC_COMPATIBLE.len() <= HW_COMPATIBLE_MAX);
}

#[test]
fn a_programmed_counter_reads_as_seconds_since_the_epoch() {
    let mut rtc = RpiRtc::new(MockChannel::new(firmware(SAMPLE_SECS, 3000)));
    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::from_secs(i64::from(SAMPLE_SECS))))
    );
}

#[test]
fn an_unprogrammed_counter_has_no_instant_to_vouch_for() {
    // Zero is the chip's own "never set since it lost power" state. Reporting
    // it as 1970 would hand the clock authority a fabricated wall time.
    let mut rtc = RpiRtc::new(MockChannel::new(firmware(0, 0)));
    assert_eq!(rtc.read(), Ok(None));
    let status = rtc.status().expect("status reads");
    assert!(status.oscillator_stopped, "an unset counter says so");
    assert!(!status.battery_backed, "no cell fitted");
    assert_eq!(status.precision, Duration64::from_secs(1));
}

#[test]
fn the_backup_cell_voltage_is_what_battery_backing_is_reported_from() {
    // The battery is an optional accessory, so the voltage register — not the
    // board's identity — is the only honest evidence the counter persists.
    let mut fitted = RpiRtc::new(MockChannel::new(firmware(SAMPLE_SECS, 3000)));
    assert!(fitted.status().expect("status reads").battery_backed);

    let mut absent = RpiRtc::new(MockChannel::new(firmware(SAMPLE_SECS, 0)));
    let status = absent.status().expect("status reads");
    assert!(!status.battery_backed);
    assert!(
        !status.oscillator_stopped,
        "a running counter with no cell still holds this boot's time"
    );
}

#[test]
fn a_write_lands_on_the_counter_and_reads_back() {
    let channel = MockChannel::new(firmware(0, 3000));
    let mut rtc = RpiRtc::new(channel);
    let at = Time64::from_secs(i64::from(SAMPLE_SECS));
    assert_eq!(rtc.set(at), Ok(()));
    assert_eq!(rtc.read(), Ok(Some(at)));
    assert!(
        !rtc.status().expect("status reads").oscillator_stopped,
        "a written counter can vouch for its value"
    );
}

#[test]
fn a_write_discards_the_sub_second_part_the_chip_cannot_hold() {
    let channel = MockChannel::new(firmware(0, 0));
    let mut rtc = RpiRtc::new(channel);
    let at = Time64::new(i64::from(SAMPLE_SECS), 750_000_000).expect("valid instant");
    assert_eq!(rtc.set(at), Ok(()));
    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::from_secs(i64::from(SAMPLE_SECS)))),
        "the declared one-second precision is what the chip keeps"
    );
}

#[test]
fn an_instant_the_counter_cannot_hold_is_refused_whole() {
    let channel = MockChannel::new(firmware(SAMPLE_SECS, 3000));
    let mut rtc = RpiRtc::new(channel);

    // Past 2106-02-07T06:28:15Z, the unsigned 32-bit counter's last second.
    assert_eq!(
        rtc.set(Time64::from_secs(i64::from(u32::MAX) + 1)),
        Err(DriverError::OutOfRange)
    );
    // Before the epoch.
    assert_eq!(rtc.set(Time64::from_secs(-1)), Err(DriverError::OutOfRange));
    // The epoch second itself: the chip cannot tell it from "never set", so
    // writing it would make a later read answer `Ok(None)`.
    assert_eq!(rtc.set(Time64::from_secs(0)), Err(DriverError::OutOfRange));

    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::from_secs(i64::from(SAMPLE_SECS)))),
        "a refused write never touched the chip"
    );
}

#[test]
fn the_counters_last_representable_second_is_accepted() {
    let channel = MockChannel::new(firmware(0, 0));
    let mut rtc = RpiRtc::new(channel);
    let last = Time64::from_secs(i64::from(u32::MAX));
    assert_eq!(rtc.set(last), Ok(()));
    assert_eq!(rtc.read(), Ok(Some(last)));
}

#[test]
fn a_dead_mailbox_is_a_fault_not_a_fabricated_reading() {
    let mut rtc = RpiRtc::new(DeadChannel);
    assert_eq!(rtc.read(), Err(DriverError::DeviceFault));
    assert_eq!(rtc.status(), Err(DriverError::DeviceFault));
    assert_eq!(
        rtc.set(Time64::from_secs(i64::from(SAMPLE_SECS))),
        Err(DriverError::DeviceFault)
    );
}

#[test]
fn firmware_that_never_processes_the_tag_fails_closed() {
    // The documented Pi 5 fault: the exchange succeeds and the response
    // buffer is left as sent, so the counter slot still holds the request's
    // own zero. Believing it would report 1970 as a firmware-provenance wall
    // time; the per-tag response bit is what rules it out.
    let mut rtc = RpiRtc::new(SilentChannel);
    assert_eq!(rtc.read(), Err(DriverError::BadMagic));
    assert_eq!(rtc.status(), Err(DriverError::BadMagic));
    assert_eq!(
        rtc.set(Time64::from_secs(i64::from(SAMPLE_SECS))),
        Err(DriverError::BadMagic)
    );
}

#[test]
fn the_driver_reads_the_time_register_not_another() {
    // The selector is what keeps a millivolt reading from being served as a
    // wall time, so assert the driver names the counter.
    let channel = MockChannel::new(firmware(SAMPLE_SECS, 4242));
    let mut rtc = RpiRtc::new(channel);
    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::from_secs(i64::from(SAMPLE_SECS)))),
        "not the backup-cell millivolts"
    );
    assert_ne!(
        RtcRegister::Time.as_u32(),
        RtcRegister::BackupVolts.as_u32()
    );
}
