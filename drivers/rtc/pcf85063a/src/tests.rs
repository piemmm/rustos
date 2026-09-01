//! Host unit tests for the Pcf85063a driver, driven against the shared
//! register-file part double.

use tairix_abi::driver::rtc::{bin_to_bcd, Rtc};
use tairix_abi::time::{CivilTime, Time64, RELEASE_EPOCH_SECS};
use tairix_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwMatchKey, HW_COMPATIBLE_MAX,
};
use tairix_i2c::mock::MockPart;

use super::{
    register, Pcf85063a, BIND_KEYS, CONTROL_1_STOP, CONTROL_1_TWELVE_HOUR, HOUR_PM,
    PCF85063A_BUS_ADDRESS, PCF85063A_COMPATIBLE, REG_CONTROL_1, REG_SECONDS,
    SECONDS_INTEGRITY_LOST,
};

/// The first year the wall clock's plausibility window admits — derived from
/// the release epoch rather than hard-coded, so the tests do not expire and
/// an offset from it is guaranteed to stay inside the window.
fn window_base() -> i64 {
    CivilTime::from_unix_secs(RELEASE_EPOCH_SECS).year
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
        | if pm { HOUR_PM } else { 0 }
}

/// Seed the calendar block with `civil` in 24-hour BCD and a clear integrity
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
            bcd(civil.day),
            0,
            bcd(civil.month),
            bin_to_bcd(yy).expect("in range"),
        ],
    );
}

#[test]
fn bring_up_puts_the_chip_in_twenty_four_hour_mode_and_running() {
    let part = MockPart::new();
    // A previous owner left it stopped and in 12-hour mode, with a bit this
    // driver does not own also set.
    part.seed(
        REG_CONTROL_1,
        &[CONTROL_1_TWELVE_HOUR | CONTROL_1_STOP | 0x80],
    );
    Pcf85063a::open(&part).expect("binds");
    let control = part.register(REG_CONTROL_1);
    assert_eq!(control & (CONTROL_1_TWELVE_HOUR | CONTROL_1_STOP), 0);
    assert_eq!(control & 0x80, 0x80, "the board's other settings survive");
}

#[test]
fn a_running_chip_reads_back_the_instant_its_registers_hold() {
    let part = MockPart::new();
    let mut chip = Pcf85063a::open(&part).expect("binds");
    seed_running(&part, &sample());
    assert_eq!(chip.read(), Ok(sample().to_time64()));
}

#[test]
fn a_twelve_hour_register_block_decodes_to_the_same_instant() {
    // The mode bit and the field can legitimately disagree in the window
    // before this driver's first write, so a part still in 12-hour mode must
    // read back the same wall time — including the two ends a hand-written
    // conversion gets wrong.
    for hour in [0u32, 1, 11, 12, 13, 23] {
        let mut civil = sample();
        civil.hour = hour;
        let part = MockPart::new();
        let mut chip = Pcf85063a::open(&part).expect("binds");
        seed_running(&part, &civil);
        part.seed(REG_SECONDS + 2, &[twelve_hour_register(hour)]);
        part.seed(REG_CONTROL_1, &[CONTROL_1_TWELVE_HOUR]);
        assert_eq!(chip.read(), Ok(civil.to_time64()), "hour {hour}");
    }
}

#[test]
fn a_lost_clock_integrity_answers_no_time_rather_than_the_registers() {
    let part = MockPart::new();
    let mut chip = Pcf85063a::open(&part).expect("binds");
    let civil = sample();
    seed_running(&part, &civil);
    let seconds = bin_to_bcd(u8::try_from(civil.second).expect("small")).expect("in range");
    part.seed(REG_SECONDS, &[seconds | SECONDS_INTEGRITY_LOST]);
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
        (0u8, 0x6Au8), // seconds
        (1, 0x6A),     // minutes
        (2, 0x2F),     // hours
        (3, 0x1F),     // days
        (5, 0x1A),     // months
        (6, 0xFF),     // years
    ] {
        let part = MockPart::new();
        let mut chip = Pcf85063a::open(&part).expect("binds");
        seed_running(&part, &sample());
        part.seed(REG_SECONDS + offset, &[byte]);
        assert_eq!(chip.read(), Ok(None), "field {offset} = {byte:#04x}");
    }

    // 31 February decodes as digits but is not a date.
    let part = MockPart::new();
    let mut chip = Pcf85063a::open(&part).expect("binds");
    seed_running(&part, &sample());
    part.seed(REG_SECONDS + 3, &[0x31]);
    part.seed(REG_SECONDS + 5, &[0x02]);
    assert_eq!(chip.read(), Ok(None));
}

#[test]
fn a_write_lands_in_the_registers_and_clears_the_integrity_flag() {
    let part = MockPart::new();
    let mut chip = Pcf85063a::open(&part).expect("binds");
    part.seed(REG_SECONDS, &[SECONDS_INTEGRITY_LOST]);
    let civil = sample();
    chip.set(civil.to_time64().expect("valid")).expect("writes");
    assert_eq!(part.register(REG_SECONDS) & SECONDS_INTEGRITY_LOST, 0);
    // The round trip is what proves the encode and the decode agree.
    assert_eq!(chip.read(), Ok(civil.to_time64()));
}

#[test]
fn a_failed_write_leaves_the_chip_reporting_that_it_cannot_vouch() {
    let part = MockPart::new();
    let mut chip = Pcf85063a::open(&part).expect("binds");
    part.seed(REG_SECONDS, &[SECONDS_INTEGRITY_LOST]);
    part.fail_with(DriverError::DeviceFault);
    assert_eq!(
        chip.set(sample().to_time64().expect("valid")),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        part.register(REG_SECONDS) & SECONDS_INTEGRITY_LOST,
        SECONDS_INTEGRITY_LOST,
        "the flag is cleared only by the write that puts a real time in"
    );
}

#[test]
fn an_instant_the_two_digit_year_could_not_read_back_is_refused() {
    let part = MockPart::new();
    let mut chip = Pcf85063a::open(&part).expect("binds");
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
    assert_eq!(part.register(REG_SECONDS), 0, "nothing was written");
}

#[test]
fn every_year_the_window_admits_round_trips() {
    // The window is half-open at its base and one century wide, so its first
    // and last years must both survive the two-digit round trip.
    let base = window_base();
    for offset in [0i64, 1, 50, 99] {
        let part = MockPart::new();
        let mut chip = Pcf85063a::open(&part).expect("binds");
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
    let mut chip = Pcf85063a::open(&part).expect("binds");
    part.fail_with(DriverError::NotFound);
    assert_eq!(chip.read(), Err(DriverError::NotFound));
    assert_eq!(chip.status().err(), Some(DriverError::NotFound));
}

#[test]
fn an_unreachable_part_refuses_bring_up_rather_than_coming_up_blind() {
    let part = MockPart::new();
    part.fail_with(DriverError::NotFound);
    assert_eq!(Pcf85063a::open(&part).err(), Some(DriverError::NotFound));
}

#[test]
fn the_part_claims_no_persistence_it_cannot_demonstrate() {
    // The chip has no backup-cell input or switch-over circuit to read, so
    // there is nothing to report and the conservative answer stands.
    let part = MockPart::new();
    let mut chip = Pcf85063a::open(&part).expect("binds");
    let status = chip.status().expect("reads");
    assert!(!status.battery_backed);
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
fn the_bind_table_names_the_part_and_fits_the_abi_bound() {
    assert_eq!(BIND_KEYS.len(), 1);
    assert!(PCF85063A_COMPATIBLE.len() <= HW_COMPATIBLE_MAX);
    let expected = HwMatchKey::compatible(PCF85063A_COMPATIBLE).expect("fits");
    assert_eq!(BIND_KEYS[0].key, expected);
    assert_eq!(PCF85063A_BUS_ADDRESS, 0x51);
}
