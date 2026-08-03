//! Host tests for the RAID6 double-parity array over a fault-injecting
//! [`Block`] double.

use super::{DualParityArray, DualParityError, DualParityMember, SCRATCH_BLOCKS};
use crate::superblock::ArrayProgress;
use core::cell::RefCell;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, HealthSnapshot};
use tairix_abi::driver::{BufferClass, DriverError};
use tairix_abi::raid::{ArrayHealth, MemberState};

// A small block size keeps the fault-injecting device doubles off the stack's
// large-array lint while the array's byte-wise GF math is size-agnostic.
const BS: u32 = 64;
/// Logical blocks per member.
const MB: u64 = 8;
/// One member's byte capacity (`BS * MB`).
const CAP: usize = 64 * 8;
/// The stripe unit used across the tests, in logical blocks.
const CHUNK: u32 = 2;
/// The number of members in the standard test array (2 data + P + Q).
const MEMBERS: usize = 4;
/// The array's usable logical block count (`MB * (MEMBERS - 2)`).
const LOGICAL: u64 = MB * (MEMBERS as u64 - 2);

/// An in-memory block device with injectable faults. Interior mutability keeps
/// the whole device behind `&mut self` methods while a test flips faults
/// through the shared borrow [`DualParityArray::member`] hands back.
struct MemBlock {
    inner: RefCell<Inner>,
}

struct Inner {
    store: [u8; CAP],
    present: bool,
    read_fault: Option<DriverError>,
    write_fault: Option<DriverError>,
    /// A single member-local block that returns [`DriverError::MediumError`]
    /// on read — a latent bad sector, not a dead device.
    medium_block: Option<u64>,
    /// The [`BufferClass`] of the most recent `write_blocks_with_class` this
    /// member observed, so a test can prove the caller's class is forwarded.
    last_write_class: Option<BufferClass>,
    /// The health telemetry this member reports.
    health: DeviceHealth,
}

impl MemBlock {
    fn new() -> Self {
        Self {
            inner: RefCell::new(Inner {
                store: [0u8; CAP],
                present: true,
                read_fault: None,
                write_fault: None,
                medium_block: None,
                last_write_class: None,
                health: DeviceHealth::Unavailable,
            }),
        }
    }

    /// Make this member report `media_errors` integrity faults through its
    /// health telemetry.
    fn set_media_errors(&self, media_errors: u64) {
        self.inner.borrow_mut().health = DeviceHealth::Available(HealthSnapshot {
            power_on_hours: 0,
            unsafe_shutdowns: 0,
            media_errors,
            reallocated_sectors: 0,
            pending_sectors: 0,
            uncorrectable_sectors: 0,
            crc_errors: 0,
            percentage_used: 0,
            available_spare: 100,
            temperature_kelvin: 300,
            critical_warning: false,
        });
    }

    fn set_read_fault(&self, e: Option<DriverError>) {
        self.inner.borrow_mut().read_fault = e;
    }

    fn set_write_fault(&self, e: Option<DriverError>) {
        self.inner.borrow_mut().write_fault = e;
    }

    fn set_medium_block(&self, b: Option<u64>) {
        self.inner.borrow_mut().medium_block = b;
    }

    /// The [`BufferClass`] of the most recent `write_blocks_with_class` this
    /// member observed (`None` if it was never written through that path).
    fn last_write_class(&self) -> Option<BufferClass> {
        self.inner.borrow().last_write_class
    }

    fn span(lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        let bs = BS as usize;
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(bs))
            .ok_or(DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > CAP {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok((start, end))
    }
}

impl Block for MemBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        if self.inner.borrow().present {
            Ok(BlockGeometry {
                block_size: BS,
                block_count: MB,
            })
        } else {
            Err(DriverError::DeviceOffline)
        }
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let inner = self.inner.borrow();
        if let Some(e) = inner.read_fault {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        if let Some(bad) = inner.medium_block {
            let blocks = (buf.len() / BS as usize) as u64;
            if bad >= lba && bad < lba + blocks {
                return Err(DriverError::MediumError);
            }
        }
        buf.copy_from_slice(&inner.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let mut inner = self.inner.borrow_mut();
        if let Some(e) = inner.write_fault {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        // A successful write to a bad-sector block models the device
        // reallocating the sector: the latent media error is cleared.
        if let Some(bad) = inner.medium_block {
            let blocks = (buf.len() / BS as usize) as u64;
            if bad >= lba && bad < lba + blocks {
                inner.medium_block = None;
            }
        }
        inner.store[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.inner.borrow_mut().last_write_class = Some(class);
        self.write_blocks(lba, buf)
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(self.inner.borrow().health)
    }
}

/// A four-member table of fresh empty members.
fn members() -> [DualParityMember<MemBlock>; MEMBERS] {
    [
        DualParityMember::new(MemBlock::new()),
        DualParityMember::new(MemBlock::new()),
        DualParityMember::new(MemBlock::new()),
        DualParityMember::new(MemBlock::new()),
    ]
}

/// A scratch buffer of the minimum required blocks.
fn scratch() -> [u8; SCRATCH_BLOCKS * 512] {
    [0u8; SCRATCH_BLOCKS * 512]
}

/// A distinct, non-zero byte per logical block, so a mis-mapped chunk or a
/// wrong reconstruction is caught. Every byte of the block is the same value.
fn val(blk: u64) -> u8 {
    let b = u8::try_from(blk % 256).unwrap();
    b.wrapping_mul(37).wrapping_add(11)
}

/// Write logical block `blk` filled with `val(blk)`.
fn put(array: &mut DualParityArray<'_, MemBlock>, blk: u64) {
    let buf = [val(blk); BS as usize];
    array.write_blocks(blk, &buf).unwrap();
}

/// Read logical block `blk` and assert it holds `val(blk)`.
fn expect(array: &mut DualParityArray<'_, MemBlock>, blk: u64) {
    let mut buf = [0u8; BS as usize];
    array.read_blocks(blk, &mut buf).unwrap();
    assert_eq!(buf, [val(blk); BS as usize], "logical block {blk}");
}

/// Fill the whole array with the per-block pattern.
fn fill(array: &mut DualParityArray<'_, MemBlock>) {
    for blk in 0..LOGICAL {
        put(array, blk);
    }
}

/// Read and verify every logical block.
fn expect_all(array: &mut DualParityArray<'_, MemBlock>) {
    for blk in 0..LOGICAL {
        expect(array, blk);
    }
}

/// Fault member `idx` by injecting a whole-device read error and touching the
/// array so the fault is observed, then clearing the injection so the member
/// is simply "gone" from the array's point of view.
fn fault_member(array: &mut DualParityArray<'_, MemBlock>, idx: usize) {
    array
        .member(idx)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    // A full read touches every member as a data source in some stripe.
    for blk in 0..LOGICAL {
        let mut buf = [0u8; BS as usize];
        let _ = array.read_blocks(blk, &mut buf);
    }
    assert_eq!(array.member_state(idx), Some(MemberState::Faulted));
    array
        .member(idx)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(None);
}

/// The caller's [`BufferClass`] reaches the data member's write, exactly as it
/// does for the mirror and stripe, while both syndrome (P and Q) writes stay
/// [`BufferClass::Sensitive`] because they carry opaque cross-stripe bytes.
/// Before the fix every RAID6 member write was forced `Sensitive`, so a
/// `NonSensitive` bulk write was needlessly zeroed on free and the class was
/// silently dropped — this asserts it is honoured.
#[test]
fn a_data_member_write_honours_the_caller_class_while_syndromes_stay_sensitive() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();

    // One NonSensitive single-block write touches its data member (caller's
    // class) plus the P and Q members (Sensitive); the other data member is
    // untouched. Layout-agnostic: whatever the P/Q rotation places where.
    let buf = [val(0); BS as usize];
    array
        .write_blocks_with_class(0, &buf, BufferClass::NonSensitive)
        .unwrap();
    let classes: [Option<BufferClass>; MEMBERS] = core::array::from_fn(|i| {
        array
            .member(i)
            .unwrap()
            .device()
            .unwrap()
            .last_write_class()
    });
    let nonsensitive = classes
        .iter()
        .filter(|c| **c == Some(BufferClass::NonSensitive))
        .count();
    let sensitive = classes
        .iter()
        .filter(|c| **c == Some(BufferClass::Sensitive))
        .count();
    assert_eq!(
        nonsensitive, 1,
        "the data member must honour the caller's NonSensitive class"
    );
    assert_eq!(
        sensitive, 2,
        "both P and Q writes must stay Sensitive (opaque cross-stripe bytes)"
    );

    // A Sensitive caller write never leaves any member NonSensitive.
    let buf = [val(1); BS as usize];
    array
        .write_blocks_with_class(1, &buf, BufferClass::Sensitive)
        .unwrap();
    for i in 0..MEMBERS {
        assert_ne!(
            array
                .member(i)
                .unwrap()
                .device()
                .unwrap()
                .last_write_class(),
            Some(BufferClass::NonSensitive),
            "a Sensitive write must never be downgraded to NonSensitive"
        );
    }
}

#[test]
fn assemble_presents_capacity_of_n_minus_two_members() {
    let mut m = members();
    let mut s = scratch();
    let array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    assert_eq!(array.member_count(), MEMBERS);
    assert_eq!(
        array.array_geometry(),
        BlockGeometry {
            block_size: BS,
            block_count: LOGICAL,
        }
    );
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn write_then_read_round_trips_every_block() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    expect_all(&mut array);
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn a_single_lost_member_is_reconstructed_and_the_array_is_degraded() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    fault_member(&mut array, 0);
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // Every block is still served, reconstructed from the survivors.
    expect_all(&mut array);
}

#[test]
fn any_two_lost_members_are_reconstructed_and_the_array_is_degraded() {
    // Exercise every distinct pair of lost members: the solver's case analysis
    // must recover data through P, through Q, and through the 2×2 system.
    for (a, b) in [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)] {
        let mut m = members();
        let mut s = scratch();
        let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
        fill(&mut array);
        fault_member(&mut array, a);
        fault_member(&mut array, b);
        assert_eq!(array.health(), ArrayHealth::Degraded, "pair {a},{b}");
        expect_all(&mut array);
    }
}

#[test]
fn a_third_loss_fails_the_array_closed() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    fault_member(&mut array, 0);
    fault_member(&mut array, 1);
    // A third member goes offline: no redundancy remains to reconstruct a
    // stripe, so I/O fails closed rather than fabricating data.
    array
        .member(2)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    let mut buf = [0u8; BS as usize];
    for blk in 0..LOGICAL {
        let _ = array.read_blocks(blk, &mut buf);
    }
    assert_eq!(array.health(), ArrayHealth::Failed);
    assert!(array.read_blocks(0, &mut buf).is_err());
}

#[test]
fn a_per_block_media_error_is_reconstructed_and_repaired_in_place() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Member 0 is a data source at member-local block 4 (stripe 2); inject a
    // latent bad sector there.
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_medium_block(Some(4));
    // Reads still return correct data (reconstructed) and the member stays a
    // healthy source — a media error is not a whole-device fault.
    expect_all(&mut array);
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn scrub_heals_a_latent_media_error_on_a_syndrome_the_read_path_never_touches() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Member 3 holds the P syndrome for stripe 0 at member-local block 0 — a
    // block ordinary reads never consult. A latent error there is invisible
    // until the copies that depend on it are needed.
    array
        .member(3)
        .unwrap()
        .device()
        .unwrap()
        .set_medium_block(Some(0));
    expect_all(&mut array); // still fine: the latent block is never read.

    // A scrub proactively reads and repairs it.
    array.begin_scrub();
    while array.scrubbing() {
        array.scrub_step(MB).unwrap();
    }

    // Prove the repair: lose both data members of stripe 0 (members 1 and 2);
    // reconstructing their data now depends on the repaired P (member 3) and Q
    // (member 0). Without the scrub this would fail on the bad P block.
    fault_member(&mut array, 1);
    fault_member(&mut array, 2);
    expect_all(&mut array);
}

#[test]
fn a_returning_member_is_rebuilt_with_current_data_including_degraded_writes() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Two members fault; the array runs degraded.
    fault_member(&mut array, 0);
    fault_member(&mut array, 1);
    // Overwrite the whole array while degraded (only members 2 and 3 accept
    // writes; the new data must stay reconstructable through the syndromes).
    for blk in 0..LOGICAL {
        let buf = [val(blk) ^ 0xAA; BS as usize];
        array.write_blocks(blk, &buf).unwrap();
    }
    // Re-add and rebuild both members from the survivors.
    array.readd_member(0).unwrap();
    array.readd_member(1).unwrap();
    assert_eq!(array.health(), ArrayHealth::Recovering);
    while array.needs_resync() {
        array.resync_step(MB).unwrap();
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // Lose the *other* two members: the just-rebuilt 0 and 1 must now carry the
    // current (degraded-window) data.
    fault_member(&mut array, 2);
    fault_member(&mut array, 3);
    for blk in 0..LOGICAL {
        let mut buf = [0u8; BS as usize];
        array.read_blocks(blk, &mut buf).unwrap();
        assert_eq!(buf, [val(blk) ^ 0xAA; BS as usize], "logical block {blk}");
    }
}

#[test]
fn the_disk_replacement_cycle_restores_redundancy() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    fault_member(&mut array, 2);
    // Pull the faulted disk (slot vacated to Absent) and slot in a fresh spare.
    let _pulled = array.remove_member(2).unwrap();
    assert_eq!(array.member_state(2), Some(MemberState::Absent));
    array.add_member(2, MemBlock::new()).unwrap();
    assert_eq!(array.member_state(2), Some(MemberState::Resyncing));
    while array.needs_resync() {
        array.resync_step(MB).unwrap();
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // The spare now carries current data: lose two other members and verify.
    fault_member(&mut array, 0);
    fault_member(&mut array, 3);
    expect_all(&mut array);
}

#[test]
fn a_write_error_drops_a_member_but_the_write_still_succeeds() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Member 1 refuses all writes: the next write faults it, yet the data is
    // still stored durably on the survivors (single fault, fully recoverable).
    array
        .member(1)
        .unwrap()
        .device()
        .unwrap()
        .set_write_fault(Some(DriverError::DeviceFault));
    for blk in 0..LOGICAL {
        let buf = [val(blk) ^ 0x5A; BS as usize];
        array.write_blocks(blk, &buf).unwrap();
    }
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // Clear the injection and read back: the new data survived the fault.
    array
        .member(1)
        .unwrap()
        .device()
        .unwrap()
        .set_write_fault(None);
    for blk in 0..LOGICAL {
        let mut buf = [0u8; BS as usize];
        array.read_blocks(blk, &mut buf).unwrap();
        assert_eq!(buf, [val(blk) ^ 0x5A; BS as usize], "logical block {blk}");
    }
}

#[test]
fn a_missing_member_assembles_absent_and_degraded() {
    let mut m = [
        DualParityMember::new(MemBlock::new()),
        DualParityMember::new(MemBlock::new()),
        DualParityMember::new(MemBlock::new()),
        DualParityMember::absent(),
    ];
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    assert_eq!(array.member_state(3), Some(MemberState::Absent));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // A degraded (one member short) array still serves reads and writes.
    fill(&mut array);
    expect_all(&mut array);
}

#[test]
fn flush_commits_survivors_and_fails_closed_past_two_losses() {
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    array.flush().unwrap();
    fault_member(&mut array, 0);
    fault_member(&mut array, 1);
    // Two losses: flush still commits the survivors.
    array.flush().unwrap();
    // A third loss: flush fails closed.
    array
        .member(2)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    let mut buf = [0u8; BS as usize];
    for blk in 0..LOGICAL {
        let _ = array.read_blocks(blk, &mut buf);
    }
    assert!(array.flush().is_err());
}

#[test]
fn a_six_member_array_recovers_any_two_losses_with_higher_q_coefficients() {
    // Six members means four data positions (0..=3), exercising Q coefficients
    // g⁰, g¹, g², g³ and 2×2 solves between non-adjacent data positions.
    const N: usize = 6;
    const LOG6: u64 = MB * (N as u64 - 2);
    for (a, b) in [(0, 3), (1, 4), (2, 5), (0, 5), (3, 4)] {
        let mut m = [
            DualParityMember::new(MemBlock::new()),
            DualParityMember::new(MemBlock::new()),
            DualParityMember::new(MemBlock::new()),
            DualParityMember::new(MemBlock::new()),
            DualParityMember::new(MemBlock::new()),
            DualParityMember::new(MemBlock::new()),
        ];
        let mut s = scratch();
        let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
        for blk in 0..LOG6 {
            let buf = [val(blk); BS as usize];
            array.write_blocks(blk, &buf).unwrap();
        }
        // Fault the two chosen members by injecting a whole-device read error
        // and touching the array, then clearing the injection.
        for &idx in &[a, b] {
            array
                .member(idx)
                .unwrap()
                .device()
                .unwrap()
                .set_read_fault(Some(DriverError::DeviceOffline));
        }
        let mut buf = [0u8; BS as usize];
        for blk in 0..LOG6 {
            let _ = array.read_blocks(blk, &mut buf);
        }
        for &idx in &[a, b] {
            assert_eq!(array.member_state(idx), Some(MemberState::Faulted));
            array
                .member(idx)
                .unwrap()
                .device()
                .unwrap()
                .set_read_fault(None);
        }
        assert_eq!(array.health(), ArrayHealth::Degraded, "pair {a},{b}");
        for blk in 0..LOG6 {
            array.read_blocks(blk, &mut buf).unwrap();
            assert_eq!(buf, [val(blk); BS as usize], "pair {a},{b} block {blk}");
        }
    }
}

#[test]
fn assemble_fails_closed_on_bad_shapes() {
    // Fewer than four slots cannot form a double-parity array.
    let mut three = [
        DualParityMember::new(MemBlock::new()),
        DualParityMember::new(MemBlock::new()),
        DualParityMember::new(MemBlock::new()),
    ];
    let mut s = scratch();
    assert_eq!(
        DualParityArray::assemble(&mut three, &mut s, CHUNK).map(|_| ()),
        Err(DualParityError::TooFewMembers)
    );

    // A zero stripe unit.
    let mut m = members();
    assert_eq!(
        DualParityArray::assemble(&mut m, &mut s, 0).map(|_| ()),
        Err(DualParityError::ZeroChunk)
    );

    // Too little scratch (fewer than SCRATCH_BLOCKS blocks).
    let mut m = members();
    let mut tiny = [0u8; (SCRATCH_BLOCKS - 1) * 64];
    assert_eq!(
        DualParityArray::assemble(&mut m, &mut tiny, CHUNK).map(|_| ()),
        Err(DualParityError::ScratchTooSmall)
    );

    // Three absent slots leave too little redundancy.
    let mut redundancy = [
        DualParityMember::new(MemBlock::new()),
        DualParityMember::absent(),
        DualParityMember::absent(),
        DualParityMember::absent(),
    ];
    assert_eq!(
        DualParityArray::assemble(&mut redundancy, &mut s, CHUNK).map(|_| ()),
        Err(DualParityError::InsufficientRedundancy)
    );
}

#[test]
fn device_health_aggregates_and_excludes_a_faulted_member() {
    // A composed double-parity array surfaces its members' telemetry rather
    // than the trait default (`Unavailable`); integrity faults sum across all
    // members (data and syndrome alike).
    let m0 = MemBlock::new();
    m0.set_media_errors(1);
    let m1 = MemBlock::new();
    m1.set_media_errors(2);
    let m2 = MemBlock::new();
    m2.set_media_errors(4);
    let m3 = MemBlock::new();
    m3.set_media_errors(8);
    let mut m = [
        DualParityMember::new(m0),
        DualParityMember::new(m1),
        DualParityMember::new(m2),
        DualParityMember::new(m3),
    ];
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    let DeviceHealth::Available(h) = array.device_health().expect("health read") else {
        panic!("expected aggregated telemetry, not Unavailable");
    };
    assert_eq!(h.media_errors, 15);

    // Member 0 goes offline; a faulted member contributes no telemetry, and
    // the double-parity array still serves from its survivors.
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    let mut buf = [0u8; BS as usize];
    for blk in 0..LOGICAL {
        let _ = array.read_blocks(blk, &mut buf);
    }
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    let DeviceHealth::Available(h) = array.device_health().expect("health read") else {
        panic!("expected the survivors' telemetry");
    };
    assert_eq!(h.media_errors, 14);
}

#[test]
fn device_health_is_unavailable_without_member_telemetry() {
    let mut m = members();
    let mut s = scratch();
    let array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    assert_eq!(
        array.device_health().expect("health read"),
        DeviceHealth::Unavailable
    );
}

#[test]
fn a_verification_pass_resumes_where_a_restart_left_it() {
    // A pass over a 100 TB+ array runs for hours, so it will meet a restart.
    // Losing the cursor would restart the pass every time, and an array
    // rebooted often enough would never finish verifying itself at all.
    let checkpoint = {
        let mut m = members();
        let mut s = scratch();
        let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).expect("assembles");
        array.begin_scrub();
        array.scrub_step(1).expect("one scrub chunk");
        assert!(array.scrubbing());
        let checkpoint = array.progress();
        assert_eq!(checkpoint.scrub_cursor, Some(array.scrub_cursor()));
        assert_eq!(checkpoint.resync_cursor, None);
        checkpoint
    };

    // The serving process restarts and the array is assembled afresh, which by
    // itself abandons the pass; the checkpointed position resumes it.
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).expect("re-assembles");
    assert!(!array.scrubbing(), "a fresh assembly is not mid-pass");
    assert_eq!(array.progress(), ArrayProgress::IDLE);
    array
        .restore_progress(checkpoint)
        .expect("the checkpointed position is adopted");
    assert!(array.scrubbing());
    assert_eq!(Some(array.scrub_cursor()), checkpoint.scrub_cursor);

    let mut steps = 0u32;
    while array.scrubbing() {
        array.scrub_step(1).expect("scrub chunk");
        steps += 1;
        assert!(steps <= 100, "the pass terminates");
    }
}

#[test]
fn a_restored_cursor_outside_the_array_is_refused_and_changes_nothing() {
    // A cursor past the end cannot have come from this array. Adopted as a
    // rebuild position it would mark a member fully rebuilt without its tail
    // ever being written, leaving stale data trusted as current — so it is
    // refused outright rather than clamped.
    let mut m = members();
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).expect("assembles");
    for cursor in [MB, MB + 1, u64::MAX] {
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: Some(cursor),
                resync_cursor: None,
            }),
            Err(DualParityError::CursorOutOfRange)
        );
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: None,
                resync_cursor: Some(cursor),
            }),
            Err(DualParityError::CursorOutOfRange)
        );
    }
    assert!(!array.scrubbing());
    assert_eq!(array.progress(), ArrayProgress::IDLE);

    // The last real block is accepted, so the refusal is exactly at the end
    // and not one block early.
    array
        .restore_progress(ArrayProgress {
            scrub_cursor: Some(MB - 1),
            resync_cursor: None,
        })
        .expect("the last block is a valid position");
    assert_eq!(array.scrub_cursor(), MB - 1);
}

#[test]
fn member_device_mut_reaches_the_named_members_own_device() {
    let mut m = [
        DualParityMember::new(MemBlock::new()),
        DualParityMember::new(MemBlock::new()),
        DualParityMember::new(MemBlock::new()),
        DualParityMember::absent(),
    ];
    let mut s = scratch();
    let mut array = DualParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();

    // The borrowed device is the member's whole disk, below the array's data
    // view: a write through it lands on that member alone, which is how a
    // caller reaches a member's reserved array-metadata blocks.
    array
        .member_device_mut(1)
        .expect("slot 1 holds a device")
        .write_blocks(2, &[0x5A; BS as usize])
        .unwrap();
    let mut buf = [0u8; BS as usize];
    array
        .member_device_mut(1)
        .unwrap()
        .read_blocks(2, &mut buf)
        .unwrap();
    assert_eq!(buf, [0x5A; BS as usize]);
    array
        .member_device_mut(0)
        .unwrap()
        .read_blocks(2, &mut buf)
        .unwrap();
    assert_eq!(
        buf, [0u8; BS as usize],
        "the write reached only the named member's device"
    );

    assert!(
        array.member_device_mut(3).is_none(),
        "an absent slot holds no device"
    );
    assert!(
        array.member_device_mut(MEMBERS).is_none(),
        "an index outside the array has no slot"
    );
}
