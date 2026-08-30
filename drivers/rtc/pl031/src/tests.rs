//! Host unit tests for the PL031 driver, driven against a mock register
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

use super::{register, Pl031, BIND_KEYS, PL031_COMPATIBLE, REGISTER_BLOCK_LEN};

/// `RTCDR` word index in the backing store (byte offset 0x000).
const DR: usize = 0;
/// `RTCLR` word index (byte offset 0x008).
const LR: usize = 2;
/// `RTCCR` word index (byte offset 0x00C).
const CR: usize = 3;

/// A register block on the heap, plus the window over it. The backing must
/// outlive every window minted from it, so both live in this one value.
struct Block {
    backing: Vec<u32>,
}

impl Block {
    /// A block of `len` bytes whose control register starts clear, as a real
    /// PL031 does out of reset.
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
        unsafe { RegisterWindow::from_mapping(0x0901_0000, base, len) }
    }

    fn word(&self, index: usize) -> u32 {
        self.backing[index]
    }

    fn set_word(&mut self, index: usize, value: u32) {
        self.backing[index] = value;
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
    let expected = HwMatchKey::compatible(PL031_COMPATIBLE).expect("fits");
    assert_eq!(BIND_KEYS[0].key, expected);
    assert!(PL031_COMPATIBLE.len() <= HW_COMPATIBLE_MAX);
}

#[test]
fn bring_up_starts_a_counter_the_reset_left_stopped() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    assert_eq!(block.word(CR), 0, "a reset PL031 is not counting");
    let _rtc = Pl031::new(block.window()).expect("binds");
    assert_eq!(block.word(CR) & 1, 1, "bring-up sets the start bit");
}

#[test]
fn bring_up_leaves_an_already_running_counter_alone() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    // A platform that starts the counter for us (QEMU's `virt` board) also
    // sets reserved bits we must not clear.
    block.set_word(CR, 0x8000_0001);
    let _rtc = Pl031::new(block.window()).expect("binds");
    assert_eq!(block.word(CR), 0x8000_0001, "no needless write");
}

#[test]
fn a_running_counter_reads_as_seconds_since_the_epoch() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    block.set_word(CR, 1);
    // 2026-08-30T00:00:00Z, comfortably past the 2038 32-bit *signed*
    // boundary's sibling cases and inside the counter's unsigned range.
    block.set_word(DR, 1_787_011_200);
    let mut rtc = Pl031::new(block.window()).expect("binds");
    assert_eq!(
        rtc.read(),
        Ok(Some(Time64::from_secs(1_787_011_200))),
        "the counter is Unix seconds"
    );
}

#[test]
fn the_top_of_the_unsigned_counter_range_is_not_read_as_negative() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    block.set_word(CR, 1);
    // 2106-02-07T06:28:15Z: the last instant a 32-bit unsigned seconds
    // counter holds. A signed widening would report 1969 instead.
    block.set_word(DR, u32::MAX);
    let mut rtc = Pl031::new(block.window()).expect("binds");
    assert_eq!(rtc.read(), Ok(Some(Time64::from_secs(4_294_967_295))));
}

#[test]
fn a_counter_that_will_not_start_vouches_for_nothing() {
    // A part whose start bit refuses to latch is not counting, so it has no
    // time to report — and must not offer the register's contents anyway.
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    block.set_word(DR, 1_787_011_200);
    let mut rtc = Pl031::new(block.window()).expect("binds");
    // Undo the bring-up write to model a chip that ignores it.
    block.set_word(CR, 0);
    assert_eq!(rtc.read(), Ok(None), "no fabricated instant");
    let status = rtc.status().expect("status reads");
    assert!(status.oscillator_stopped, "and it says why");
}

#[test]
fn status_reports_what_the_part_can_actually_support() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    block.set_word(CR, 1);
    let mut rtc = Pl031::new(block.window()).expect("binds");
    let status = rtc.status().expect("status reads");
    assert_eq!(status.precision, Duration64::from_secs(1));
    assert!(!status.oscillator_stopped);
    // The part has no backup-cell indicator and the device tree declares
    // none, so the driver understates rather than claiming persistence.
    assert!(!status.battery_backed);
}

#[test]
fn a_set_loads_the_counter_and_starts_it() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = Pl031::new(block.window()).expect("binds");
    block.set_word(CR, 0);
    // A sub-second component is dropped: the declared precision says so.
    let time = Time64::new(1_787_011_200, 750_000_000).expect("canonical");
    assert_eq!(rtc.set(time), Ok(()));
    assert_eq!(block.word(LR), 1_787_011_200);
    assert_eq!(block.word(CR) & 1, 1, "a loaded counter is left running");
}

#[test]
fn an_instant_the_counter_cannot_hold_is_refused_whole() {
    let mut block = Block::new(REGISTER_BLOCK_LEN);
    let mut rtc = Pl031::new(block.window()).expect("binds");
    for secs in [-1, 1 << 32, i64::MAX, i64::MIN] {
        assert_eq!(
            rtc.set(Time64::from_secs(secs)),
            Err(DriverError::OutOfRange),
            "{secs} must not be wrapped or clamped into the counter"
        );
        assert_eq!(block.word(LR), 0, "and nothing is written");
    }
}

#[test]
fn a_window_too_short_for_the_registers_fails_closed() {
    // A mis-provisioned grant must not read whatever lies at offset zero.
    let mut block = Block::new(8);
    assert_eq!(
        Pl031::new(block.window()).err(),
        Some(DriverError::DeviceFault)
    );
}
