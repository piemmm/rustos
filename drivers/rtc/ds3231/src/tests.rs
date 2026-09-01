//! Host unit tests for the DS3231 driver, driven against the shared
//! register-file part double.

use tairix_abi::driver::rtc::{bin_to_bcd, Rtc};
use tairix_abi::time::{CivilTime, Time64, RELEASE_EPOCH_SECS};
use tairix_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwMatchKey, HW_COMPATIBLE_MAX,
};
use tairix_i2c::mock::MockPart;

use super::{
    register, Ds3231, BIND_KEYS, DS1307_COMPATIBLE, DS3231_BUS_ADDRESS, DS3231_COMPATIBLE, HOUR_PM,
    HOUR_TWELVE, REG_SECONDS, REG_STATUS, STATUS_OSC_STOPPED,
};

/// The first year the wall clock's plausibility window admits — derived from
/// the release epoch rather than hard-coded, so the tests do not expire and
/// an offset from it is guaranteed to stay inside the window.
fn window_base() -> i64 {
    CivilTime::from_unix_secs(RELEASE_EPOCH_SECS).year
}

/// A 12-hour hours register as a foreign writer would have left it, spelled
/// independently of the driver's own conversion so the test checks it rather
/// than restating it.
fn twelve_hour_register(hour: u32) -> u8 {
    let pm = hour >= 12;
    let twelve = match hour % 12 {
        0 => 12,
        other => other,
    };
    bin_to_bcd(u8::try_from(twelve).expect("small")).expect("in range")
        | HOUR_TWELVE
        | if pm { HOUR_PM } else { 0 }
}

/// Seed the calendar block with `civil` in 24-hour BCD and clear the stop
/// flag, as a running part would hold it.
fn seed_running(part: &MockPart, civil: &CivilTime) {
    let bcd = |v: u32| bin_to_bcd(u8::try_from(v).expect("small")).expect("in range");
    let yy = u8::try_from(civil.year.rem_euclid(100)).expect("small");
    part.seed(
        REG_SECONDS,
        &[
            bcd(civil.second),
            bcd(civil.minute),
            bcd(civil.hour),
            1,
            bcd(civil.day),
            bcd(civil.month),
            bin_to_bcd(yy).expect("in range"),
        ],
    );
    part.seed(REG_STATUS, &[0]);
}

fn sample() -> CivilTime {
    CivilTime {
        year: window_base() + 4,
        month: 7,
        day: 4,
        hour: 13,
        minute: 45,
        second: 30,
    }
}

#[test]
fn a_running_chip_reads_back_the_instant_its_registers_hold() {
    let part = MockPart::new();
    seed_running(&part, &sample());
    let mut chip = Ds3231::new(&part);
    assert_eq!(chip.read(), Ok(sample().to_time64()));
}

#[test]
fn a_twelve_hour_register_block_decodes_to_the_same_instant() {
    // A part a foreign writer left in 12-hour mode must read back the same
    // wall time, including the two ends a hand-written conversion gets wrong.
    for hour in [0u32, 1, 11, 12, 13, 23] {
        let mut civil = sample();
        civil.hour = hour;
        let part = MockPart::new();
        seed_running(&part, &civil);
        part.seed(REG_SECONDS + 2, &[twelve_hour_register(hour)]);
        let mut chip = Ds3231::new(&part);
        assert_eq!(chip.read(), Ok(civil.to_time64()), "hour {hour}");
    }
}

#[test]
fn a_stopped_oscillator_answers_no_time_rather_than_the_registers() {
    let part = MockPart::new();
    seed_running(&part, &sample());
    part.seed(REG_STATUS, &[STATUS_OSC_STOPPED]);
    let mut chip = Ds3231::new(&part);
    assert_eq!(
        chip.read(),
        Ok(None),
        "a stopped counter vouches for nothing"
    );
    assert_eq!(
        chip.status().map(|s| s.oscillator_stopped),
        Ok(true),
        "and says why"
    );
}

#[test]
fn a_register_block_that_is_not_a_calendar_is_refused() {
    // A non-decimal nibble in any field, and a date the calendar does not
    // have, both answer "no time" rather than a plausible-looking instant.
    for (offset, byte) in [
        (0u8, 0xAAu8), // seconds
        (1, 0x6A),     // minutes
        (2, 0x2F),     // hours
        (4, 0x1F),     // date
        (5, 0x1A),     // month
        (6, 0xFF),     // year
    ] {
        let part = MockPart::new();
        seed_running(&part, &sample());
        part.seed(REG_SECONDS + offset, &[byte]);
        let mut chip = Ds3231::new(&part);
        assert_eq!(chip.read(), Ok(None), "field {offset} = {byte:#04x}");
    }

    // 31 February decodes as digits but is not a date.
    let part = MockPart::new();
    seed_running(&part, &sample());
    part.seed(REG_SECONDS + 4, &[0x31]);
    part.seed(REG_SECONDS + 5, &[0x02]);
    let mut chip = Ds3231::new(&part);
    assert_eq!(chip.read(), Ok(None));
}

#[test]
fn the_century_bit_is_masked_off_rather_than_read_as_a_century() {
    let civil = sample();
    let part = MockPart::new();
    seed_running(&part, &civil);
    let month = bin_to_bcd(u8::try_from(civil.month).expect("small")).expect("in range");
    part.seed(REG_SECONDS + 5, &[month | 0x80]);
    let mut chip = Ds3231::new(&part);
    assert_eq!(
        chip.read(),
        Ok(civil.to_time64()),
        "the carry says nothing about which century a powered part is in"
    );
}

#[test]
fn a_write_lands_in_the_registers_and_clears_the_stop_flag() {
    let part = MockPart::new();
    part.seed(REG_STATUS, &[STATUS_OSC_STOPPED]);
    let mut chip = Ds3231::new(&part);
    let civil = sample();
    chip.set(civil.to_time64().expect("valid")).expect("writes");
    assert_eq!(part.register(REG_STATUS) & STATUS_OSC_STOPPED, 0);
    // The round trip is what proves the encode and the decode agree.
    assert_eq!(chip.read(), Ok(civil.to_time64()));
}

#[test]
fn a_failed_write_leaves_the_chip_reporting_that_it_cannot_vouch() {
    let part = MockPart::new();
    part.seed(REG_STATUS, &[STATUS_OSC_STOPPED]);
    part.fail_with(DriverError::DeviceFault);
    let mut chip = Ds3231::new(&part);
    assert_eq!(
        chip.set(sample().to_time64().expect("valid")),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        part.register(REG_STATUS) & STATUS_OSC_STOPPED,
        STATUS_OSC_STOPPED,
        "the stop flag is cleared only once a real time is in the counter"
    );
}

#[test]
fn an_instant_the_two_digit_year_could_not_read_back_is_refused() {
    let part = MockPart::new();
    let mut chip = Ds3231::new(&part);
    // A year a century away shares the chip's two digits with a year inside
    // the window, so writing it would read back as a different time.
    let mut civil = sample();
    civil.year = window_base() + 100;
    assert_eq!(
        chip.set(civil.to_time64().expect("valid")),
        Err(DriverError::OutOfRange)
    );
    // Pre-1970 and far-future instants are refused for the same reason.
    assert_eq!(chip.set(Time64::from_secs(0)), Err(DriverError::OutOfRange));
    assert!(chip.set(Time64::from_secs(i64::MAX / 2)).is_err());
    // Nothing was written.
    assert_eq!(part.register(REG_SECONDS), 0);
}

#[test]
fn every_year_the_window_admits_round_trips() {
    // The window is half-open at its base and one century wide, so its first
    // and last years must both survive the two-digit round trip.
    let base = window_base();
    for offset in [0i64, 1, 50, 99] {
        let part = MockPart::new();
        let mut chip = Ds3231::new(&part);
        let mut civil = sample();
        civil.year = base + offset;
        let instant = civil.to_time64().expect("valid");
        chip.set(instant).expect("writes");
        assert_eq!(chip.read(), Ok(Some(instant)), "year {}", civil.year);
    }
}

#[test]
fn a_bus_fault_is_a_fault_not_an_absent_time() {
    let part = MockPart::new();
    part.fail_with(DriverError::NotFound);
    let mut chip = Ds3231::new(&part);
    assert_eq!(chip.read(), Err(DriverError::NotFound));
    assert_eq!(chip.status().err(), Some(DriverError::NotFound));
}

#[test]
fn the_status_reports_the_parts_own_backup_cell() {
    let part = MockPart::new();
    let mut chip = Ds3231::new(&part);
    let status = chip.status().expect("reads");
    assert!(status.battery_backed);
    assert_eq!(status.precision.secs(), 1);
}

/// A [`DriverHost`] double reporting exactly the capabilities a test grants.
struct Host {
    caps: &'static [CapabilityId],
}

impl DriverHost for Host {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.caps.contains(&cap)
    }

    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

#[test]
fn register_requires_the_load_capability() {
    assert!(register(&Host {
        caps: &[CapabilityId::DRV_LOAD]
    })
    .is_ok());
    assert_eq!(
        register(&Host { caps: &[] }).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn the_bind_table_names_both_parts_and_fits_the_abi_bound() {
    assert_eq!(BIND_KEYS.len(), 2);
    for compatible in [DS3231_COMPATIBLE, DS1307_COMPATIBLE] {
        assert!(compatible.len() <= HW_COMPATIBLE_MAX);
        let expected = HwMatchKey::compatible(compatible).expect("fits");
        assert!(
            BIND_KEYS.iter().any(|k| k.key == expected),
            "the table must name {compatible:?}"
        );
    }
    assert_eq!(DS3231_BUS_ADDRESS, 0x68);
}
