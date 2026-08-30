//! Host unit tests for the MC146818 driver, driven against a model of the
//! chip's index/data port pair and its register file.

extern crate alloc;

use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use tairix_abi::driver::rtc::{bin_to_bcd, resolve_two_digit_year, Rtc};
use tairix_abi::time::{CivilTime, Duration64, Time64};
use tairix_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwMatchKey, HW_COMPATIBLE_MAX,
};

use super::{
    register, two_digit_year, CmosPorts, Mc146818, AGREEMENT_ATTEMPTS, BIND_KEYS,
    MC146818_COMPATIBLE, PORT_RANGE_LEN, REG_DAY, REG_HOUR, REG_MINUTE, REG_MONTH, REG_SECOND,
    REG_STATUS_A, REG_STATUS_B, REG_STATUS_D, REG_YEAR, STATUS_A_UIP, STATUS_B_24_HOUR,
    STATUS_B_BINARY, STATUS_B_SET, STATUS_D_VALID_RAM, UIP_PROBES,
};

/// Registers the chip presents through the index port.
const REG_FILE_LEN: usize = 0x40;

/// A model of the chip behind its two ports: an index latch a write to port
/// offset 0 installs, and a register file the data port at offset 1
/// addresses.
///
/// Interior mutability throughout, so a test still observes and steers the
/// model while the driver holds a reference to it.
struct Cmos {
    index: Cell<u8>,
    regs: RefCell<[u8; REG_FILE_LEN]>,
    /// Remaining Status A reads that report `UIP` set.
    uip_reads: Cell<u32>,
    /// Status A reads to answer clear before `uip_reads` starts counting, so a
    /// test can place an update window *between* two block reads rather than
    /// only ahead of the first.
    uip_delay: Cell<u32>,
    /// When non-zero, the seconds register is advanced by one on each of this
    /// many reads of it — a chip ticking under the driver, which is what a
    /// torn read looks like.
    tearing_reads: Cell<u32>,
    /// Every `(index, value)` the driver wrote, in order.
    writes: RefCell<Vec<(u8, u8)>>,
    /// Status A / B / D reads and every calendar-register read, counted so a
    /// test can bound the work a refusal cost.
    reads: Cell<u32>,
    /// Reads of the seconds register, which is one per whole-block read.
    block_reads: Cell<u32>,
    /// When `Some`, the port at this offset refuses every access.
    dead_offset: Cell<Option<u16>>,
    /// When `Some`, a data-port write while the index latch names this
    /// register is refused — a chip that stops answering part-way through a
    /// multi-register write.
    refuse_reg: Cell<Option<u8>>,
}

impl Cmos {
    /// A chip whose backup cell is good and whose format is the PC default:
    /// BCD fields, 24-hour hours.
    fn new() -> Self {
        let mut regs = [0u8; REG_FILE_LEN];
        regs[usize::from(REG_STATUS_D)] = STATUS_D_VALID_RAM;
        regs[usize::from(REG_STATUS_B)] = STATUS_B_24_HOUR;
        Self {
            index: Cell::new(0),
            regs: RefCell::new(regs),
            uip_reads: Cell::new(0),
            uip_delay: Cell::new(0),
            tearing_reads: Cell::new(0),
            writes: RefCell::new(Vec::new()),
            reads: Cell::new(0),
            block_reads: Cell::new(0),
            dead_offset: Cell::new(None),
            refuse_reg: Cell::new(None),
        }
    }

    fn reg(&self, index: u8) -> u8 {
        self.regs.borrow()[usize::from(index)]
    }

    fn set_reg(&self, index: u8, value: u8) {
        self.regs.borrow_mut()[usize::from(index)] = value;
    }

    /// Program Status B's format bits, leaving its other bits alone.
    fn set_format(&self, binary: bool, twenty_four_hour: bool) {
        let mut status_b = self.reg(REG_STATUS_B) & !(STATUS_B_BINARY | STATUS_B_24_HOUR);
        if binary {
            status_b |= STATUS_B_BINARY;
        }
        if twenty_four_hour {
            status_b |= STATUS_B_24_HOUR;
        }
        self.set_reg(REG_STATUS_B, status_b);
    }

    /// Load the calendar block with raw register bytes.
    fn set_calendar(&self, fields: [u8; 6]) {
        for (index, value) in [
            REG_SECOND, REG_MINUTE, REG_HOUR, REG_DAY, REG_MONTH, REG_YEAR,
        ]
        .into_iter()
        .zip(fields)
        {
            self.set_reg(index, value);
        }
    }

    /// The calendar block as raw register bytes, in the same order.
    fn calendar(&self) -> [u8; 6] {
        [
            REG_SECOND, REG_MINUTE, REG_HOUR, REG_DAY, REG_MONTH, REG_YEAR,
        ]
        .map(|i| self.reg(i))
    }

    /// Every value written to `index`, in order.
    fn writes_to(&self, index: u8) -> Vec<u8> {
        self.writes
            .borrow()
            .iter()
            .filter(|(i, _)| *i == index)
            .map(|(_, v)| *v)
            .collect()
    }
}

impl CmosPorts for &Cmos {
    fn read(&mut self, offset: u16) -> Result<u8, DriverError> {
        if self.dead_offset.get() == Some(offset) {
            return Err(DriverError::DeviceFault);
        }
        assert_eq!(offset, 1, "a read must address the data port");
        self.reads.set(self.reads.get() + 1);
        let index = self.index.get();
        if index == REG_STATUS_A {
            let delay = self.uip_delay.get();
            if delay > 0 {
                self.uip_delay.set(delay - 1);
                return Ok(self.reg(REG_STATUS_A) & !STATUS_A_UIP);
            }
            let pending = self.uip_reads.get();
            if pending > 0 {
                self.uip_reads.set(pending - 1);
                return Ok(self.reg(REG_STATUS_A) | STATUS_A_UIP);
            }
            return Ok(self.reg(REG_STATUS_A) & !STATUS_A_UIP);
        }
        let value = self.reg(index);
        if index == REG_SECOND {
            self.block_reads.set(self.block_reads.get() + 1);
            let ticking = self.tearing_reads.get();
            if ticking > 0 {
                self.tearing_reads.set(ticking - 1);
                self.set_reg(REG_SECOND, value.wrapping_add(1));
            }
        }
        Ok(value)
    }

    fn write(&mut self, offset: u16, value: u8) -> Result<(), DriverError> {
        if self.dead_offset.get() == Some(offset) {
            return Err(DriverError::DeviceFault);
        }
        match offset {
            0 => {
                self.index.set(value);
                Ok(())
            }
            1 => {
                let index = self.index.get();
                if self.refuse_reg.get() == Some(index) {
                    return Err(DriverError::DeviceFault);
                }
                self.writes.borrow_mut().push((index, value));
                self.set_reg(index, value);
                Ok(())
            }
            other => panic!("driver touched port offset {other}"),
        }
    }
}

/// A `DriverHost` double that grants exactly the capabilities it is told to.
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

/// The fixture instant: 2027-03-05T12:34:56Z, whose every field is distinct
/// so a swapped register shows up.
const FIXTURE: CivilTime = CivilTime {
    year: 2027,
    month: 3,
    day: 5,
    hour: 12,
    minute: 34,
    second: 56,
};

/// The fixture instant as a [`Time64`].
fn fixture() -> Time64 {
    FIXTURE.to_time64().expect("canonical")
}

/// The two digits the chip stores for the fixture year.
fn fixture_yy() -> u8 {
    two_digit_year(FIXTURE.year).expect("inside the window")
}

fn bcd(value: u8) -> u8 {
    bin_to_bcd(value).expect("in range")
}

/// The first year the shared plausibility window admits, derived rather than
/// restated so a release that moves the window moves these tests with it.
fn window_base() -> i64 {
    (0u8..=99)
        .filter_map(resolve_two_digit_year)
        .min()
        .expect("the window admits every two-digit year")
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
    let expected = HwMatchKey::compatible(MC146818_COMPATIBLE).expect("fits");
    assert_eq!(BIND_KEYS[0].key, expected);
    assert!(MC146818_COMPATIBLE.len() <= HW_COMPATIBLE_MAX);
    // The index/data pair, and no more: a wider claim would ask the kernel
    // for ports the chip does not own.
    assert_eq!(PORT_RANGE_LEN, 2);
}

#[test]
fn a_bcd_block_decodes_in_twenty_four_hour_form() {
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    cmos.set_calendar([
        bcd(FIXTURE.second.try_into().expect("fits")),
        bcd(FIXTURE.minute.try_into().expect("fits")),
        bcd(FIXTURE.hour.try_into().expect("fits")),
        bcd(FIXTURE.day.try_into().expect("fits")),
        bcd(FIXTURE.month.try_into().expect("fits")),
        bcd(fixture_yy()),
    ]);
    let mut rtc = Mc146818::new(&cmos);
    assert_eq!(rtc.read(), Ok(Some(fixture())));
}

#[test]
fn a_binary_block_decodes_when_register_b_says_so() {
    // Identical fields, encoded as plain binary: the very same bytes read as
    // BCD would be a different (or invalid) time, so the DM bit is what the
    // decode turns on.
    let cmos = Cmos::new();
    cmos.set_format(true, true);
    cmos.set_calendar([56, 34, 12, 5, 3, fixture_yy()]);
    let mut rtc = Mc146818::new(&cmos);
    assert_eq!(rtc.read(), Ok(Some(fixture())));

    // The same register image read as BCD is refused: 0x56 is not a decimal
    // pair in the seconds field, so the driver must not reinterpret it.
    cmos.set_format(false, true);
    assert_eq!(rtc.read(), Ok(None), "no fabricated instant");
}

#[test]
fn twelve_hour_mode_masks_the_pm_flag_off_the_hour() {
    // Every 12-hour spelling the chip can hold maps to exactly one hour of
    // the day, and the PM flag is never part of the value.
    for binary in [false, true] {
        let cmos = Cmos::new();
        cmos.set_format(binary, false);
        let encode = |v: u8| if binary { v } else { bcd(v) };
        for hour24 in 0u8..24 {
            let twelve = match hour24 % 12 {
                0 => 12,
                other => other,
            };
            let pm = if hour24 >= 12 { 0x80 } else { 0 };
            cmos.set_calendar([
                encode(0),
                encode(0),
                encode(twelve) | pm,
                encode(1),
                encode(1),
                encode(fixture_yy()),
            ]);
            let mut rtc = Mc146818::new(&cmos);
            let expected = CivilTime {
                year: FIXTURE.year,
                month: 1,
                day: 1,
                hour: u32::from(hour24),
                minute: 0,
                second: 0,
            }
            .to_time64()
            .expect("canonical");
            assert_eq!(
                rtc.read(),
                Ok(Some(expected)),
                "12-hour {twelve}{} (binary={binary}) is hour {hour24}",
                if pm == 0 { "AM" } else { "PM" }
            );
        }
    }
}

#[test]
fn twelve_hour_midnight_and_noon_are_the_two_that_wrap() {
    // The two spellings a naive `hour + 12` gets wrong, called out on their
    // own so a regression cannot hide inside the sweep above.
    let cmos = Cmos::new();
    cmos.set_format(false, false);
    let mut rtc = Mc146818::new(&cmos);

    cmos.set_calendar([bcd(0), bcd(0), bcd(12), bcd(1), bcd(1), bcd(fixture_yy())]);
    let midnight = CivilTime {
        hour: 0,
        month: 1,
        day: 1,
        minute: 0,
        second: 0,
        ..FIXTURE
    };
    assert_eq!(
        rtc.read(),
        Ok(Some(midnight.to_time64().expect("canonical"))),
        "12 AM is hour 0"
    );

    cmos.set_reg(REG_HOUR, bcd(12) | 0x80);
    let noon = CivilTime {
        hour: 12,
        ..midnight
    };
    assert_eq!(
        rtc.read(),
        Ok(Some(noon.to_time64().expect("canonical"))),
        "12 PM is hour 12"
    );
}

#[test]
fn an_hour_the_chip_cannot_legally_hold_is_refused() {
    let cmos = Cmos::new();
    let mut rtc = Mc146818::new(&cmos);

    // 24-hour mode: 24 and above are not hours of a day.
    cmos.set_format(false, true);
    for hour in [bcd(24), bcd(99)] {
        cmos.set_calendar([bcd(0), bcd(0), hour, bcd(1), bcd(1), bcd(fixture_yy())]);
        assert_eq!(rtc.read(), Ok(None), "{hour:#04x} is not a 24-hour value");
    }

    // 12-hour mode: the field is 1..=12, so 0 and 13 are not spellings.
    cmos.set_format(false, false);
    for hour in [bcd(0), bcd(13), bcd(0) | 0x80] {
        cmos.set_calendar([bcd(0), bcd(0), hour, bcd(1), bcd(1), bcd(fixture_yy())]);
        assert_eq!(rtc.read(), Ok(None), "{hour:#04x} is not a 12-hour value");
    }
}

#[test]
fn a_register_block_that_is_not_a_calendar_date_is_refused() {
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    let mut rtc = Mc146818::new(&cmos);
    let good = [bcd(0), bcd(0), bcd(0), bcd(1), bcd(1), bcd(fixture_yy())];
    for (slot, bad, what) in [
        (0usize, bcd(60), "second 60"),
        (1, bcd(60), "minute 60"),
        (3, bcd(0), "day 0"),
        (3, bcd(32), "day 32"),
        (4, bcd(0), "month 0"),
        (4, bcd(13), "month 13"),
    ] {
        let mut fields = good;
        fields[slot] = bad;
        cmos.set_calendar(fields);
        assert_eq!(rtc.read(), Ok(None), "{what} is not a date");
    }
    // 29 February in a non-leap year is refused by the same validation.
    cmos.set_calendar([bcd(0), bcd(0), bcd(0), bcd(29), bcd(2), bcd(fixture_yy())]);
    assert_eq!(rtc.read(), Ok(None), "2027-02-29 does not exist");
}

#[test]
fn the_read_waits_out_an_update_window_then_decodes() {
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    cmos.set_calendar([bcd(56), bcd(34), bcd(12), bcd(5), bcd(3), bcd(fixture_yy())]);
    // UIP set for the first several probes, then clear: the driver must ride
    // the window out rather than read across it.
    cmos.uip_reads.set(5);
    let mut rtc = Mc146818::new(&cmos);
    assert_eq!(rtc.read(), Ok(Some(fixture())));
    assert_eq!(cmos.uip_reads.get(), 0, "every UIP probe was consumed");
}

#[test]
fn a_window_that_never_settles_fails_closed_inside_the_budget() {
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    cmos.set_calendar([bcd(56), bcd(34), bcd(12), bcd(5), bcd(3), bcd(fixture_yy())]);
    // More stuck probes than the budget allows.
    cmos.uip_reads.set(UIP_PROBES * 2);
    let mut rtc = Mc146818::new(&cmos);
    assert_eq!(
        rtc.read(),
        Ok(None),
        "a chip that never settles has no time"
    );
    // A stuck window is terminal, so the driver spends the probe budget once
    // — plus the Status D and Status B reads before the loop — and does not
    // retry the block read on top of it.
    assert!(
        cmos.reads.get() <= UIP_PROBES + 2,
        "{} reads exceeds one probe budget",
        cmos.reads.get()
    );
    assert!(
        cmos.uip_reads.get() > 0,
        "the model still had stuck probes left, so the driver gave up first"
    );
}

#[test]
fn a_torn_read_is_rejected_and_retried_until_the_pair_agrees() {
    // The chip ticks under the first read, so the first two blocks differ;
    // a driver that accepted a single read would return the torn value.
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    cmos.set_calendar([bcd(56), bcd(34), bcd(12), bcd(5), bcd(3), bcd(fixture_yy())]);
    cmos.tearing_reads.set(1);
    let mut rtc = Mc146818::new(&cmos);
    let expected = CivilTime {
        second: 57,
        ..FIXTURE
    }
    .to_time64()
    .expect("canonical");
    assert_eq!(
        rtc.read(),
        Ok(Some(expected)),
        "the settled value is the one after the tick, never a mixture"
    );

    // A chip that never stops moving never offers an agreeing pair, so the
    // read fails closed rather than handing on a straddled block — and the
    // attempt budget, not the probe budget, is what bounds the cost.
    cmos.tearing_reads.set(UIP_PROBES);
    cmos.reads.set(0);
    assert_eq!(rtc.read(), Ok(None));
    assert!(
        cmos.reads.get() <= AGREEMENT_ATTEMPTS * 7 + 2,
        "{} reads: a block read costs six times a probe, so the agreement \
         retry must not be sized like the probe budget",
        cmos.reads.get()
    );
}

#[test]
fn an_update_between_two_blocks_discards_the_earlier_one() {
    // An update window falls between the first and second block reads while
    // the registers happen to be unchanged, so the two blocks are equal. A
    // driver that compared them would accept a pair straddling an update, and
    // would need only two block reads to do it; discarding the earlier block
    // costs a third. The register image cannot distinguish the two outcomes —
    // only the number of reads can.
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    cmos.set_calendar([bcd(56), bcd(34), bcd(12), bcd(5), bcd(3), bcd(fixture_yy())]);
    // Clear for the first probe, then one probe reporting the update.
    cmos.uip_delay.set(1);
    cmos.uip_reads.set(1);
    let mut rtc = Mc146818::new(&cmos);

    assert_eq!(rtc.read(), Ok(Some(fixture())));
    assert_eq!(
        cmos.block_reads.get(),
        3,
        "the block read before the update was discarded, not paired across it"
    );
}

#[test]
fn a_flat_backup_cell_vouches_for_nothing_and_says_why() {
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    cmos.set_calendar([bcd(56), bcd(34), bcd(12), bcd(5), bcd(3), bcd(fixture_yy())]);
    // VRT clear: the registers may hold a perfectly well-formed date, and it
    // still means nothing.
    cmos.set_reg(REG_STATUS_D, 0);
    let mut rtc = Mc146818::new(&cmos);
    assert_eq!(rtc.read(), Ok(None), "no fabricated instant");
    let status = rtc.status().expect("status reads");
    assert!(status.oscillator_stopped, "and it says why");
    assert!(
        status.battery_backed,
        "the part is battery-backed by design"
    );
    assert_eq!(status.precision, Duration64::from_secs(1));

    cmos.set_reg(REG_STATUS_D, STATUS_D_VALID_RAM);
    assert_eq!(rtc.read(), Ok(Some(fixture())));
    assert!(!rtc.status().expect("status reads").oscillator_stopped);
}

#[test]
fn the_century_register_is_never_read() {
    // Register 0x32 is only a century where the ACPI FADT says so, which
    // this driver cannot see, so a hostile or unrelated byte there must not
    // move the year.
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    cmos.set_calendar([bcd(0), bcd(0), bcd(0), bcd(1), bcd(1), bcd(fixture_yy())]);
    let mut rtc = Mc146818::new(&cmos);
    let expected = rtc.read().expect("reads");
    for century in [0x00u8, 0x19, 0x20, 0x99, 0xFF] {
        cmos.set_reg(0x32, century);
        assert_eq!(
            rtc.read(),
            Ok(expected),
            "century byte {century:#04x} ignored"
        );
    }
}

#[test]
fn a_two_digit_year_resolves_inside_the_plausibility_window() {
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    let mut rtc = Mc146818::new(&cmos);
    for yy in 0u8..=99 {
        cmos.set_calendar([bcd(0), bcd(0), bcd(0), bcd(1), bcd(1), bcd(yy)]);
        let year = resolve_two_digit_year(yy).expect("in range");
        let expected = CivilTime {
            year,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        }
        .to_time64()
        .expect("canonical");
        assert_eq!(rtc.read(), Ok(Some(expected)), "{yy} resolves to {year}");
    }
    // A binary year field can hold a value two digits cannot, and that is
    // refused rather than folded into the window.
    cmos.set_format(true, true);
    for bad in [100u8, 200, 255] {
        cmos.set_calendar([0, 0, 0, 1, 1, bad]);
        assert_eq!(rtc.read(), Ok(None), "{bad} is not a two-digit year");
    }
}

#[test]
fn a_set_round_trips_in_each_format_with_set_raised_and_lowered() {
    for (binary, twenty_four_hour) in [(false, true), (true, true), (false, false), (true, false)] {
        let cmos = Cmos::new();
        cmos.set_format(binary, twenty_four_hour);
        let before = cmos.reg(REG_STATUS_B);
        let mut rtc = Mc146818::new(&cmos);
        assert_eq!(
            rtc.set(fixture()),
            Ok(()),
            "binary={binary} h24={twenty_four_hour}"
        );

        // The chip does not update while SET is high, and it is released
        // afterwards, so the counter is left running.
        assert_eq!(
            cmos.writes_to(REG_STATUS_B),
            [before | STATUS_B_SET, before],
            "SET raised across the write, then lowered"
        );
        assert_eq!(
            cmos.reg(REG_STATUS_B),
            before,
            "and the format bits were not reprogrammed"
        );
        assert_eq!(
            rtc.read(),
            Ok(Some(fixture())),
            "and the chip reads it back"
        );
    }
}

#[test]
fn a_set_writes_each_calendar_field_to_its_own_register() {
    // Written in the chip's own encoding, spelled out rather than derived, so
    // a swapped or mis-encoded register cannot pass.
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    let mut rtc = Mc146818::new(&cmos);
    assert_eq!(rtc.set(fixture()), Ok(()));
    assert_eq!(
        cmos.calendar(),
        [bcd(56), bcd(34), bcd(12), bcd(5), bcd(3), bcd(fixture_yy())]
    );

    // 12-hour mode puts the PM flag in bit 7 and 12 in the hour digits.
    cmos.set_format(false, false);
    assert_eq!(rtc.set(fixture()), Ok(()));
    assert_eq!(cmos.reg(REG_HOUR), bcd(12) | 0x80, "noon is 12 PM");
}

#[test]
fn a_year_outside_the_window_is_refused_whole() {
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    cmos.set_calendar([bcd(56), bcd(34), bcd(12), bcd(5), bcd(3), bcd(fixture_yy())]);
    let untouched = cmos.calendar();
    let mut rtc = Mc146818::new(&cmos);

    // One year either side of the hundred-year window the two digits name.
    for year in [window_base() - 1, window_base() + 100] {
        let time = CivilTime { year, ..FIXTURE }
            .to_time64()
            .expect("canonical");
        assert_eq!(
            rtc.set(time),
            Err(DriverError::OutOfRange),
            "{year} cannot be spelled in two digits inside the window"
        );
        assert_eq!(cmos.calendar(), untouched, "and nothing is written");
        assert_eq!(
            cmos.writes_to(REG_STATUS_B),
            Vec::<u8>::new(),
            "and SET was never raised, so the chip never stopped"
        );
    }
}

#[test]
fn a_pre_1970_or_post_2038_instant_is_handled_without_wrapping() {
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    let untouched = cmos.calendar();
    let mut rtc = Mc146818::new(&cmos);

    // Pre-1970 and 1901 (the 32-bit signed underflow) are outside the
    // window: refused, not wrapped into a plausible future year.
    for year in [1969, 1901, 1900, 0] {
        let time = CivilTime {
            year,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        }
        .to_time64()
        .expect("canonical");
        assert_eq!(rtc.set(time), Err(DriverError::OutOfRange), "{year}");
        assert_eq!(cmos.calendar(), untouched);
    }

    // Well past 2038 and inside the window: stored and read back exactly, so
    // no 32-bit seconds field is hiding anywhere on the path.
    let late = CivilTime {
        year: 2099,
        month: 12,
        day: 31,
        hour: 23,
        minute: 59,
        second: 59,
    }
    .to_time64()
    .expect("canonical");
    assert!(
        resolve_two_digit_year(99) == Some(2099),
        "inside the window"
    );
    assert_eq!(rtc.set(late), Ok(()));
    assert_eq!(rtc.read(), Ok(Some(late)));
}

#[test]
fn the_sub_second_part_is_dropped_as_the_declared_precision_says() {
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    let mut rtc = Mc146818::new(&cmos);
    let with_nanos = Time64::new(fixture().secs(), 999_999_999).expect("canonical");
    assert_eq!(rtc.set(with_nanos), Ok(()));
    assert_eq!(
        rtc.read(),
        Ok(Some(fixture())),
        "a one-second chip keeps whole seconds only"
    );
}

#[test]
fn a_refused_port_access_is_a_device_fault_rather_than_a_guess() {
    // A grant that cannot reach the data port: every path reports the fault
    // instead of substituting a byte.
    let cmos = Cmos::new();
    cmos.dead_offset.set(Some(1));
    let mut rtc = Mc146818::new(&cmos);
    assert_eq!(rtc.read(), Err(DriverError::DeviceFault));
    assert_eq!(rtc.status(), Err(DriverError::DeviceFault));
    assert_eq!(rtc.set(fixture()), Err(DriverError::DeviceFault));

    // And one that cannot reach the index port either.
    let cmos = Cmos::new();
    cmos.dead_offset.set(Some(0));
    let mut rtc = Mc146818::new(&cmos);
    assert_eq!(rtc.read(), Err(DriverError::DeviceFault));
    assert_eq!(rtc.set(fixture()), Err(DriverError::DeviceFault));
}

#[test]
fn a_set_releases_the_chip_even_when_a_field_write_fails() {
    // A chip left with SET high sits frozen, so the release must happen on
    // the error path too — and the error still reaches the caller.
    let cmos = Cmos::new();
    cmos.set_format(false, true);
    let before = cmos.reg(REG_STATUS_B);
    cmos.refuse_reg.set(Some(REG_MONTH));
    let mut rtc = Mc146818::new(&cmos);

    assert_eq!(
        rtc.set(fixture()),
        Err(DriverError::DeviceFault),
        "the refusal is not swallowed"
    );
    assert_eq!(
        cmos.reg(REG_STATUS_B) & STATUS_B_SET,
        0,
        "the chip was released, so its counter is running again"
    );
    assert_eq!(
        cmos.writes_to(REG_STATUS_B),
        [before | STATUS_B_SET, before]
    );
    // The write stopped at the refused register rather than carrying on.
    assert_eq!(cmos.writes_to(REG_YEAR), Vec::<u8>::new());
    assert_eq!(cmos.writes_to(REG_DAY), [bcd(5)]);
}

#[test]
fn two_digit_year_refuses_a_year_the_window_would_resolve_elsewhere() {
    // The encode-side guard on its own: only a year the shared window
    // resolves back to itself may be stored.
    for yy in 0u8..=99 {
        let year = resolve_two_digit_year(yy).expect("in range");
        assert_eq!(two_digit_year(year), Some(yy), "{year} stores as {yy}");
        assert_eq!(
            two_digit_year(year - 100),
            None,
            "a century below {year} shares its digits and must be refused"
        );
        assert_eq!(two_digit_year(year + 100), None);
    }
    for year in [1970, 1999, 2000, i64::MIN, i64::MAX] {
        assert_eq!(two_digit_year(year), None, "{year}");
    }
}
