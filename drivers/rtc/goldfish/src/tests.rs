//! Host unit tests for the Goldfish driver, driven against a mock register
//! window standing in for the counter's MMIO block.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::ptr::NonNull;

use tairix_abi::driver::rtc::Rtc;
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwMatchKey, RegisterWindow,
    HW_COMPATIBLE_MAX,
};

use super::{
    counter_halves, register, Goldfish, BIND_KEYS, GOLDFISH_COMPATIBLE, REGISTER_BLOCK_LEN,
};

/// `TIME_LOW` word index in the backing store (byte offset 0x00).
const LOW: usize = 0;
/// `TIME_HIGH` word index (byte offset 0x04).
const HIGH: usize = 1;

/// The QEMU riscv64 `virt` fixture instant, 2027-03-05T12:00:00.123456789Z,
/// as the counter holds it: nanoseconds since the Unix epoch.
const FIXTURE_SECS: i64 = 1_804_248_000;
/// Sub-second part of the fixture instant, so both halves of the counter are
/// non-zero and neither is a byte-symmetric value.
const FIXTURE_NANOS: u32 = 123_456_789;
/// The fixture instant as a whole nanosecond count (`0x1909_F9FF_2E11_4D15`).
const FIXTURE_COUNT: u64 = 1_804_248_000_123_456_789;

/// A register block on the heap, plus the window over it. The backing must
/// outlive every window minted from it, so both live in this one value.
struct Block {
    backing: Vec<u32>,
}

impl Block {
    /// A block of `len` bytes whose counter reads zero, as an unprovisioned
    /// device does.
    fn new(len: usize) -> Self {
        Self {
            backing: vec![0u32; len.div_ceil(4)],
        }
    }

    /// Mint a window over the whole block.
    ///
    /// The returned window borrows `self.backing`, so a caller must not move
    /// or resize it while the window lives; every test here keeps the `Block`
    /// alive and reads it back only through `word`.
    fn window(&mut self) -> RegisterWindow {
        let len = self.backing.len() * 4;
        let base = NonNull::new(self.backing.as_mut_ptr().cast::<u8>()).expect("non-null heap");
        // SAFETY: `base` covers exactly `len` bytes of a `Vec<u32>`, so it is
        // 4-byte aligned, and the allocation outlives the window because the
        // `Block` owns it for the whole test. Nothing else aliases the range
        // while the window is live.
        unsafe { RegisterWindow::from_mapping(0x1010_1000, base, len) }
    }

    fn word(&self, index: usize) -> u32 {
        self.backing[index]
    }

    fn set_word(&mut self, index: usize, value: u32) {
        self.backing[index] = value;
    }

    /// Load the counter pair with `nanos`.
    fn set_counter(&mut self, nanos: u64) {
        let (low, high) = counter_halves(nanos);
        self.set_word(LOW, low);
        self.set_word(HIGH, high);
    }

    /// Read the counter pair back the way the device composes it.
    fn counter(&self) -> u64 {
        u64::from(self.word(LOW)) | (u64::from(self.word(HIGH)) << 32)
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

/// A driver bound over a full-page block whose counter holds `nanos`.
fn bound(block: &mut Block, nanos: u64) -> Goldfish {
    block.set_counter(nanos);
    Goldfish::new(block.window()).expect("binds")
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
    let expected = HwMatchKey::compatible(GOLDFISH_COMPATIBLE).expect("fits");
    assert_eq!(BIND_KEYS[0].key, expected);
    assert!(GOLDFISH_COMPATIBLE.len() <= HW_COMPATIBLE_MAX);
}

#[test]
fn the_counter_reads_as_nanoseconds_since_the_epoch() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    // The halves the fixture instant occupies, spelled out rather than
    // derived, so a change to the split cannot move both sides of the test.
    block.set_word(LOW, 0x2E11_4D15);
    block.set_word(HIGH, 0x1909_F9FF);
    let mut rtc = Goldfish::new(block.window()).expect("binds");
    assert_eq!(
        rtc.read(),
        Ok(Some(
            Time64::new(FIXTURE_SECS, FIXTURE_NANOS).expect("canonical")
        )),
        "the counter is Unix nanoseconds, split low word first"
    );
}

#[test]
fn both_halves_come_from_their_own_register() {
    // Neither half is assumed and neither offset stands in for the other: a
    // driver that read one register twice, ignored the high word, or swapped
    // the pair fails at least one of these.
    let mut block = Block::new(REGISTER_BLOCK_LEN);

    block.set_word(LOW, 0);
    block.set_word(HIGH, 0x17);
    let mut rtc = Goldfish::new(block.window()).expect("binds");
    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::new(98, 784_247_808).expect("canonical"))),
        "the high word alone is the whole count"
    );

    block.set_word(LOW, 500_000_000);
    block.set_word(HIGH, 0);
    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::new(0, 500_000_000).expect("canonical"))),
        "the low word alone is the whole count"
    );

    block.set_word(HIGH, 0x17);
    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::new(99, 284_247_808).expect("canonical"))),
        "and together they compose low | high << 32"
    );
}

#[test]
fn a_sub_second_counter_keeps_its_nanoseconds() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, 1);
    assert_eq!(rtc.read(), Ok(Some(Time64::new(0, 1).expect("canonical"))));

    block.set_counter(999_999_999);
    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::new(0, 999_999_999).expect("canonical"))),
        "the last instant below one second does not carry"
    );

    block.set_counter(1_000_000_000);
    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::from_secs(1))),
        "and one whole second carries exactly once"
    );
}

#[test]
fn an_instant_past_2038_reads_without_wrapping() {
    // 2049-03-22T09:46:40Z. A 32-bit signed seconds field would report 1901.
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, 2_500_000_000 * 1_000_000_000);
    assert_eq!(rtc.read(), Ok(Some(Time64::from_secs(2_500_000_000))));
}

#[test]
fn the_top_of_the_counter_range_decodes_exactly() {
    // 2554-07-21T23:34:33.709551615Z: the last instant an unsigned 64-bit
    // nanosecond counter holds. Neither the split nor the offset from the
    // epoch may saturate here.
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, u64::MAX);
    assert_eq!(
        rtc.read(),
        Ok(Some(
            Time64::new(18_446_744_073, 709_551_615).expect("canonical")
        ))
    );
}

#[test]
fn a_zero_counter_vouches_for_nothing() {
    // Zero is the Unix epoch, which no running machine reports, so the
    // device has nothing behind the value and must not offer it.
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, 0);
    assert_eq!(rtc.read(), Ok(None), "no fabricated instant");
    let status = rtc.status().expect("status reads");
    assert!(status.oscillator_stopped, "and it says why");

    // One nanosecond above the epoch is a count the device does stand behind.
    block.set_counter(1);
    assert_eq!(rtc.read(), Ok(Some(Time64::new(0, 1).expect("canonical"))));
    assert!(!rtc.status().expect("status reads").oscillator_stopped);
}

#[test]
fn status_reports_what_the_part_can_actually_support() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, FIXTURE_COUNT);
    let status = rtc.status().expect("status reads");
    assert_eq!(status.precision, Duration64::from_nanos(1));
    assert!(!status.oscillator_stopped);
    // The device models no backup cell and the device tree declares none, so
    // the driver understates rather than claiming persistence.
    assert!(!status.battery_backed);
}

#[test]
fn a_set_writes_both_halves_and_round_trips() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, 0);
    let time = Time64::new(FIXTURE_SECS, FIXTURE_NANOS).expect("canonical");
    assert_eq!(rtc.set(time), Ok(()));
    assert_eq!(block.word(LOW), 0x2E11_4D15, "low half in TIME_LOW");
    assert_eq!(block.word(HIGH), 0x1909_F9FF, "high half in TIME_HIGH");
    assert_eq!(block.counter(), FIXTURE_COUNT);
    // Nanosecond precision, so the sub-second part survives the round trip.
    assert_eq!(rtc.read(), Ok(Some(time)));
}

#[test]
fn a_set_reaches_each_half_on_its_own() {
    // Each write must land in its own register: a step within one high word
    // proves the low store happens, and a step that leaves the low word
    // alone proves the high store does.
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, FIXTURE_COUNT);

    let low_step = Time64::new(FIXTURE_SECS, FIXTURE_NANOS + 1_000).expect("canonical");
    assert_eq!(rtc.set(low_step), Ok(()));
    assert_eq!(block.word(LOW), 0x2E11_50FD);
    assert_eq!(block.word(HIGH), 0x1909_F9FF, "the high word did not move");
    assert_eq!(rtc.read(), Ok(Some(low_step)));

    // One whole high word on from the fixture instant: the same low word.
    let high_step = Time64::new(1_804_248_004, 418_424_085).expect("canonical");
    assert_eq!(rtc.set(high_step), Ok(()));
    assert_eq!(block.word(LOW), 0x2E11_4D15);
    assert_eq!(block.word(HIGH), 0x1909_FA00, "and now only the high one");
    assert_eq!(rtc.read(), Ok(Some(high_step)));
}

#[test]
fn setting_the_epoch_leaves_a_counter_that_vouches_for_nothing() {
    // The device cannot tell the epoch from an unprovisioned counter, so the
    // write is honoured and the read then honestly reports no time rather
    // than handing back an instant the chip does not stand behind.
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, FIXTURE_COUNT);
    assert_eq!(rtc.set(Time64::UNIX_EPOCH), Ok(()));
    assert_eq!(block.counter(), 0);
    assert_eq!(rtc.read(), Ok(None));
    assert!(rtc.status().expect("status reads").oscillator_stopped);
}

#[test]
fn an_instant_before_the_epoch_is_refused_whole() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, FIXTURE_COUNT);
    // 1969-12-31T23:59:59Z, 1901, and the far past: an unsigned counter
    // holds none of them, and must not wrap one into a future instant.
    for secs in [-1, -2_147_483_648, i64::MIN] {
        assert_eq!(
            rtc.set(Time64::from_secs(secs)),
            Err(DriverError::OutOfRange),
            "{secs} must not be wrapped or clamped into the counter"
        );
        assert_eq!(block.counter(), FIXTURE_COUNT, "and nothing is written");
    }
}

#[test]
fn an_instant_past_the_counter_range_is_refused_whole() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = bound(&mut block, FIXTURE_COUNT);
    // The exact boundary: `u64::MAX` nanoseconds is the last instant the
    // counter holds, so its seconds are accepted only with that nanosecond
    // field or below.
    let last = Time64::new(18_446_744_073, 709_551_615).expect("canonical");
    assert_eq!(rtc.set(last), Ok(()));
    assert_eq!(block.counter(), u64::MAX);

    for time in [
        Time64::new(18_446_744_073, 709_551_616).expect("canonical"),
        Time64::from_secs(18_446_744_074),
        Time64::from_secs(i64::MAX),
    ] {
        assert_eq!(
            rtc.set(time),
            Err(DriverError::OutOfRange),
            "{time:?} must not be wrapped or clamped into the counter"
        );
        assert_eq!(block.counter(), u64::MAX, "and nothing is written");
    }
}

#[test]
fn the_halves_round_trip_the_counter_they_split() {
    for nanos in [
        0,
        1,
        u64::from(u32::MAX),
        u64::from(u32::MAX) + 1,
        FIXTURE_COUNT,
        u64::MAX,
    ] {
        let (low, high) = counter_halves(nanos);
        assert_eq!(
            u64::from(low) | (u64::from(high) << 32),
            nanos,
            "{nanos} must survive the split the device takes it as"
        );
    }
    // The halves are not interchangeable: a swapped pair is a different
    // count, which is what makes the register order load-bearing.
    let (low, high) = counter_halves(FIXTURE_COUNT);
    assert_ne!(u64::from(high) | (u64::from(low) << 32), FIXTURE_COUNT);
}

#[test]
fn a_window_too_short_for_the_counter_pair_fails_closed() {
    // A mis-provisioned grant must not read whatever lies past its window.
    for len in [0, 4] {
        let mut block = Block::new(len);
        assert_eq!(
            Goldfish::new(block.window()).err(),
            Some(DriverError::DeviceFault),
            "a {len}-byte window cannot hold both counter registers"
        );
    }
    // Exactly the counter pair is enough: the driver touches nothing above
    // it, so it must not demand the whole page a real grant carries.
    let mut block = Block::new(8);
    block.set_counter(FIXTURE_COUNT);
    let mut rtc = Goldfish::new(block.window()).expect("binds");
    assert_eq!(
        rtc.read(),
        Ok(Some(
            Time64::new(FIXTURE_SECS, FIXTURE_NANOS).expect("canonical")
        ))
    );
}
