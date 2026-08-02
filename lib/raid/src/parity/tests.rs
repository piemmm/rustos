//! Host tests for the RAID5 parity array over a fault-injecting [`Block`]
//! double.

use super::{ParityArray, ParityError, ParityMember};
use crate::mirror::{ArrayHealth, MemberState};
use crate::superblock::ArrayProgress;
use core::cell::RefCell;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, HealthSnapshot};
use tairix_abi::driver::{BufferClass, DriverError};

const BS: u32 = 512;
/// Logical blocks per member.
const MB: u64 = 8;
/// One member's byte capacity (`BS * MB`).
const CAP: usize = 512 * 8;
/// The stripe unit used across the tests, in logical blocks.
const CHUNK: u32 = 2;
/// The number of members in the standard test array.
const MEMBERS: usize = 3;

/// An in-memory block device with injectable faults. Interior mutability keeps
/// the whole device behind `&mut self` methods while a test flips faults
/// through the shared borrow [`ParityArray::member`] hands back.
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
    flush_fault: bool,
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
                flush_fault: false,
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

    fn set_flush_fault(&self, f: bool) {
        self.inner.borrow_mut().flush_fault = f;
    }

    /// The [`BufferClass`] of the most recent `write_blocks_with_class` this
    /// member observed (`None` if it was never written through that path).
    fn last_write_class(&self) -> Option<BufferClass> {
        self.inner.borrow().last_write_class
    }

    /// The first byte of member-local block `lba` in this member's store.
    fn block_byte(&self, lba: u64) -> u8 {
        self.inner.borrow().store[usize::try_from(lba).unwrap() * BS as usize]
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
        if self.inner.borrow().flush_fault {
            Err(DriverError::DeviceFault)
        } else {
            Ok(())
        }
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

/// A three-member table of fresh empty members.
fn members() -> [ParityMember<MemBlock>; MEMBERS] {
    [
        ParityMember::new(MemBlock::new()),
        ParityMember::new(MemBlock::new()),
        ParityMember::new(MemBlock::new()),
    ]
}

/// A scratch buffer of two logical blocks — the minimum a parity array needs.
fn scratch() -> [u8; 2 * 512] {
    [0u8; 2 * 512]
}

/// The array's usable logical block count for the standard test array.
const LOGICAL: u64 = MB * (MEMBERS as u64 - 1);

/// A distinct, non-zero byte per logical block, so a mis-mapped chunk or a
/// wrong reconstruction is caught. Every byte of the block is the same value,
/// so a block is fully characterised by [`MemBlock::block_byte`] and the parity
/// (XOR of uniform bytes) is itself uniform.
fn val(blk: u64) -> u8 {
    let b = u8::try_from(blk % 256).unwrap();
    b.wrapping_mul(7).wrapping_add(3)
}

/// Write logical block `blk` filled with `val(blk)`.
fn put(array: &mut ParityArray<'_, MemBlock>, blk: u64) {
    let buf = [val(blk); BS as usize];
    array.write_blocks(blk, &buf).unwrap();
}

/// Read logical block `blk` and assert it holds `val(blk)`.
fn expect(array: &mut ParityArray<'_, MemBlock>, blk: u64) {
    let mut buf = [0u8; BS as usize];
    array.read_blocks(blk, &mut buf).unwrap();
    assert_eq!(buf, [val(blk); BS as usize], "logical block {blk}");
}

/// Fill the whole array with the per-block pattern.
fn fill(array: &mut ParityArray<'_, MemBlock>) {
    for blk in 0..LOGICAL {
        put(array, blk);
    }
}

/// Assert the parity invariant across every stripe row: the XOR of every
/// member's block at each member-local LBA is zero (parity == XOR of data).
fn assert_parity_consistent(array: &ParityArray<'_, MemBlock>) {
    for l in 0..MB {
        let mut acc = 0u8;
        for i in 0..MEMBERS {
            acc ^= array.member(i).unwrap().device().unwrap().block_byte(l);
        }
        assert_eq!(acc, 0, "parity inconsistent at member-local lba {l}");
    }
}

#[test]
fn assemble_presents_capacity_of_n_minus_one_members() {
    let mut m = members();
    let mut s = scratch();
    let array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
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
fn a_full_write_and_read_round_trips_and_keeps_parity_consistent() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    for blk in 0..LOGICAL {
        expect(&mut array, blk);
    }
    assert_parity_consistent(&array);
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn a_multi_block_cross_stripe_transfer_round_trips() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    // A window that crosses several chunk and stripe boundaries.
    let mut src = [0u8; 6 * 512];
    for (i, b) in src.iter_mut().enumerate() {
        *b = u8::try_from((i / BS as usize + 1) % 251).unwrap();
    }
    array.write_blocks(1, &src).unwrap();
    let mut back = [0u8; 6 * 512];
    array.read_blocks(1, &mut back).unwrap();
    assert_eq!(src, back);
    assert_parity_consistent(&array);
}

/// The caller's [`BufferClass`] reaches the data member's write, exactly as it
/// does for the mirror and stripe, while the parity write stays
/// [`BufferClass::Sensitive`] because it carries opaque cross-stripe bytes.
/// Before the fix every RAID5 member write was forced `Sensitive`, so a
/// `NonSensitive` bulk write was needlessly zeroed on free and the class was
/// silently dropped — this asserts it is honoured.
#[test]
fn a_data_member_write_honours_the_caller_class_while_parity_stays_sensitive() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();

    // One NonSensitive single-block write: exactly the data member records the
    // caller's NonSensitive class; exactly the parity member records Sensitive;
    // the third member is untouched. This holds whatever the parity rotation
    // places where, so the assertion needs no layout arithmetic.
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
        sensitive, 1,
        "the parity member must stay Sensitive (opaque cross-stripe bytes)"
    );

    // A Sensitive caller write never leaves any member NonSensitive: a
    // Sensitive request is upheld end to end.
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
fn a_lost_member_is_reconstructed_on_read_and_the_array_degrades() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Member 0 goes offline: every read that would touch it must reconstruct
    // from the survivors, and the array drops to degraded.
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    // The first read that hits member 0 faults it; all reads still return the
    // correct data by reconstruction.
    for blk in 0..LOGICAL {
        expect(&mut array, blk);
    }
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);
}

#[test]
fn a_write_while_degraded_keeps_the_lost_data_reconstructable() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Drop member 1 for good.
    array
        .member(1)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    expect(&mut array, 0); // faults member 1 via a reconstruction path
                           // Overwrite every logical block while degraded, then read it all back:
                           // the parity recompute must keep the missing member's blocks recoverable.
    for blk in 0..LOGICAL {
        let buf = [val(blk).wrapping_add(100); BS as usize];
        array.write_blocks(blk, &buf).unwrap();
    }
    for blk in 0..LOGICAL {
        let mut buf = [0u8; BS as usize];
        array.read_blocks(blk, &mut buf).unwrap();
        assert_eq!(buf, [val(blk).wrapping_add(100); BS as usize], "blk {blk}");
    }
    assert_eq!(array.health(), ArrayHealth::Degraded);
}

#[test]
fn a_second_lost_member_fails_closed() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Two members offline: no stripe can be reconstructed.
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    array
        .member(1)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    let mut buf = [0u8; BS as usize];
    // Some block lives on member 0 or must be reconstructed using member 1;
    // either way a read fails closed rather than fabricating data.
    let mut saw_offline = false;
    for blk in 0..LOGICAL {
        if array.read_blocks(blk, &mut buf) == Err(DriverError::DeviceOffline) {
            saw_offline = true;
        }
    }
    assert!(saw_offline, "a two-member loss must fail some read closed");
    assert_eq!(array.health(), ArrayHealth::Failed);
}

#[test]
fn a_per_block_media_error_on_read_is_reconstructed_and_repaired() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // A latent bad sector at member 2, member-local block 3.
    array
        .member(2)
        .unwrap()
        .device()
        .unwrap()
        .set_medium_block(Some(3));
    // Read every block: any that maps to member 2 lba 3 is reconstructed from
    // the survivors and repaired (the double clears the bad sector on write).
    for blk in 0..LOGICAL {
        expect(&mut array, blk);
    }
    // The device was never a whole-device fault, so it stays in sync and the
    // array stays optimal; the bad sector was healed.
    assert_eq!(array.member_state(2), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_eq!(
        array
            .member(2)
            .unwrap()
            .device()
            .unwrap()
            .inner
            .borrow()
            .medium_block,
        None
    );
}

#[test]
fn a_faulted_member_is_rebuilt_with_current_data() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Member 0 faults, then returns and is re-added and rebuilt.
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    expect(&mut array, 0); // faults member 0
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    // Device comes back healthy; re-add and rebuild it.
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(None);
    array.readd_member(0).unwrap();
    assert_eq!(array.member_state(0), Some(MemberState::Resyncing));
    assert_eq!(array.health(), ArrayHealth::Recovering);
    // Rebuild a couple of blocks at a time (bounded, interruptible).
    let mut guard = 0;
    while array.needs_resync() {
        array.resync_step(2).unwrap();
        guard += 1;
        assert!(guard < 100, "resync must terminate");
    }
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // Now member 0 is a direct read source again and holds the correct data:
    // fault the *other* data-bearing members to force reads from member 0
    // where it is the data member. Simplest: verify the whole array reads back
    // correctly and parity is consistent (rebuilt member included).
    for blk in 0..LOGICAL {
        expect(&mut array, blk);
    }
    assert_parity_consistent(&array);
}

#[test]
fn a_write_during_rebuild_reaches_the_already_synced_region() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Fault, return, and begin rebuilding member 2.
    array
        .member(2)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    expect(&mut array, 4); // block 4 lives on member 2, faulting it
    assert_eq!(array.member_state(2), Some(MemberState::Faulted));
    array
        .member(2)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(None);
    array.readd_member(2).unwrap();
    // Rebuild only the first stripe row, leaving a cursor mid-array.
    array.resync_step(2).unwrap();
    assert!(array.needs_resync());
    // Overwrite the whole array while member 2 is mid-rebuild.
    for blk in 0..LOGICAL {
        let buf = [val(blk).wrapping_add(50); BS as usize];
        array.write_blocks(blk, &buf).unwrap();
    }
    // Finish the rebuild.
    let mut guard = 0;
    while array.needs_resync() {
        array.resync_step(4).unwrap();
        guard += 1;
        assert!(guard < 100);
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // Every block, including those written during the rebuild, reads back
    // correctly with member 2 fully in sync.
    for blk in 0..LOGICAL {
        let mut buf = [0u8; BS as usize];
        array.read_blocks(blk, &mut buf).unwrap();
        assert_eq!(buf, [val(blk).wrapping_add(50); BS as usize], "blk {blk}");
    }
    assert_parity_consistent(&array);
}

#[test]
fn remove_then_add_a_spare_restores_redundancy() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Fault member 1.
    array
        .member(1)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    expect(&mut array, 2); // block 2 lives on member 1 in stripe 0
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    // Pull the faulted disk: slot goes absent, real device returned.
    let _pulled = array.remove_member(1).unwrap();
    assert_eq!(array.member_state(1), Some(MemberState::Absent));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // Removing a live member is refused.
    assert!(matches!(
        array.remove_member(0),
        Err(ParityError::NotFaulted)
    ));
    // Install a fresh spare and rebuild it.
    array.add_member(1, MemBlock::new()).unwrap();
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    // Adding to an occupied slot is refused.
    assert_eq!(
        array.add_member(0, MemBlock::new()),
        Err(ParityError::SlotOccupied)
    );
    let mut guard = 0;
    while array.needs_resync() {
        array.resync_step(3).unwrap();
        guard += 1;
        assert!(guard < 100);
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);
    for blk in 0..LOGICAL {
        expect(&mut array, blk);
    }
    assert_parity_consistent(&array);
}

#[test]
fn a_write_fault_drops_the_member_and_the_write_still_lands() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Member 0 will fault its next write; the write to a block whose data or
    // parity lives on member 0 still succeeds via the surviving members, and
    // the array degrades rather than losing the write.
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_write_fault(Some(DriverError::DeviceOffline));
    for blk in 0..LOGICAL {
        let buf = [val(blk).wrapping_add(9); BS as usize];
        array.write_blocks(blk, &buf).unwrap();
    }
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // Every block still reads back the new value (member 0 reconstructed).
    for blk in 0..LOGICAL {
        let mut buf = [0u8; BS as usize];
        array.read_blocks(blk, &mut buf).unwrap();
        assert_eq!(buf, [val(blk).wrapping_add(9); BS as usize], "blk {blk}");
    }
}

#[test]
fn replace_a_faulted_member_with_a_fresh_disk_and_rebuild() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    array
        .member(1)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    expect(&mut array, 2);
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    // Replacing a live member is refused; only a faulted one.
    assert_eq!(
        array.replace_member(0, MemBlock::new()),
        Err(ParityError::NotFaulted)
    );
    array.replace_member(1, MemBlock::new()).unwrap();
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    let mut guard = 0;
    while array.needs_resync() {
        array.resync_step(8).unwrap();
        guard += 1;
        assert!(guard < 100);
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);
    for blk in 0..LOGICAL {
        expect(&mut array, blk);
    }
    assert_parity_consistent(&array);
}

#[test]
fn flush_commits_survivors_and_fails_closed_past_the_redundancy_limit() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    assert_eq!(array.flush(), Ok(()));
    // One member cannot flush: it is dropped, but the array (one loss) stays
    // serving and the flush still succeeds on the survivors.
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_flush_fault(true);
    assert_eq!(array.flush(), Ok(()));
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // A second member failing its flush pushes the array past its redundancy:
    // fail closed.
    array
        .member(1)
        .unwrap()
        .device()
        .unwrap()
        .set_flush_fault(true);
    assert_eq!(array.flush(), Err(DriverError::DeviceFault));
    assert_eq!(array.health(), ArrayHealth::Failed);
}

#[test]
fn assemble_rejects_every_ill_formed_array() {
    // Fewer than three members.
    let mut two = [
        ParityMember::new(MemBlock::new()),
        ParityMember::new(MemBlock::new()),
    ];
    let mut s = scratch();
    assert_eq!(
        ParityArray::assemble(&mut two, &mut s, CHUNK).err(),
        Some(ParityError::TooFewMembers)
    );

    // Zero stripe unit.
    let mut m = members();
    let mut s = scratch();
    assert_eq!(
        ParityArray::assemble(&mut m, &mut s, 0).err(),
        Some(ParityError::ZeroChunk)
    );

    // Scratch smaller than two blocks.
    let mut m = members();
    let mut tiny = [0u8; BS as usize];
    assert_eq!(
        ParityArray::assemble(&mut m, &mut tiny, CHUNK).err(),
        Some(ParityError::ScratchTooSmall)
    );

    // Two absent members: not enough redundancy for RAID5.
    let mut m = [
        ParityMember::new(MemBlock::new()),
        ParityMember::absent(),
        ParityMember::absent(),
    ];
    let mut s = scratch();
    assert_eq!(
        ParityArray::assemble(&mut m, &mut s, CHUNK).err(),
        Some(ParityError::InsufficientRedundancy)
    );

    // A single absent member assembles degraded (one loss is tolerable).
    let mut m = [
        ParityMember::new(MemBlock::new()),
        ParityMember::new(MemBlock::new()),
        ParityMember::absent(),
    ];
    let mut s = scratch();
    let array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    assert_eq!(array.health(), ArrayHealth::Degraded);
    assert_eq!(array.member_state(2), Some(MemberState::Absent));
}

/// A block device reporting a caller-chosen geometry, for the
/// geometry-mismatch assembly test.
struct GeoBlock(BlockGeometry);
impl Block for GeoBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(self.0)
    }
    fn read_blocks(&mut self, _lba: u64, _buf: &mut [u8]) -> Result<(), DriverError> {
        Ok(())
    }
    fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
        Ok(())
    }
    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

#[test]
fn assemble_rejects_mismatched_and_ragged_geometry() {
    // Two members disagree on size: not one array.
    let mut mixed = [
        ParityMember::new(GeoBlock(BlockGeometry {
            block_size: BS,
            block_count: MB,
        })),
        ParityMember::new(GeoBlock(BlockGeometry {
            block_size: BS,
            block_count: MB + u64::from(CHUNK),
        })),
        ParityMember::new(GeoBlock(BlockGeometry {
            block_size: BS,
            block_count: MB,
        })),
    ];
    let mut s = scratch();
    assert_eq!(
        ParityArray::assemble(&mut mixed, &mut s, CHUNK).err(),
        Some(ParityError::GeometryMismatch)
    );

    // A member whose size is not a whole number of stripe chunks.
    let mut ragged = [
        ParityMember::new(GeoBlock(BlockGeometry {
            block_size: BS,
            block_count: 7, // not a multiple of CHUNK = 2
        })),
        ParityMember::new(GeoBlock(BlockGeometry {
            block_size: BS,
            block_count: 7,
        })),
        ParityMember::new(GeoBlock(BlockGeometry {
            block_size: BS,
            block_count: 7,
        })),
    ];
    let mut s = scratch();
    assert_eq!(
        ParityArray::assemble(&mut ragged, &mut s, CHUNK).err(),
        Some(ParityError::UnalignedGeometry)
    );

    // A degenerate geometry.
    let mut zero = [
        ParityMember::new(GeoBlock(BlockGeometry {
            block_size: BS,
            block_count: 0,
        })),
        ParityMember::new(GeoBlock(BlockGeometry {
            block_size: BS,
            block_count: MB,
        })),
        ParityMember::new(GeoBlock(BlockGeometry {
            block_size: BS,
            block_count: MB,
        })),
    ];
    let mut s = scratch();
    assert_eq!(
        ParityArray::assemble(&mut zero, &mut s, CHUNK).err(),
        Some(ParityError::ZeroGeometry)
    );
}

#[test]
fn out_of_range_and_misaligned_requests_are_refused_before_any_member() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    let mut buf = [0u8; BS as usize];
    // Past the end of the usable array.
    assert_eq!(
        array.read_blocks(LOGICAL, &mut buf),
        Err(DriverError::LengthOutOfRange)
    );
    // An overflowing extent.
    assert_eq!(
        array.read_blocks(u64::MAX, &mut buf),
        Err(DriverError::LengthOutOfRange)
    );
    // A non-block-multiple / empty buffer.
    assert_eq!(
        array.read_blocks(0, &mut buf[..BS as usize - 1]),
        Err(DriverError::BufferTooSmall)
    );
    assert_eq!(
        array.read_blocks(0, &mut buf[..0]),
        Err(DriverError::BufferTooSmall)
    );
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn buffer_class_is_forwarded_and_round_trips() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    let payload = [0xABu8; 4 * 512];
    array
        .write_blocks_with_class(1, &payload, BufferClass::Sensitive)
        .unwrap();
    let mut back = [0u8; 4 * 512];
    array
        .read_blocks_with_class(1, &mut back, BufferClass::Sensitive)
        .unwrap();
    assert_eq!(payload, back);
}

#[test]
fn dropping_any_single_member_still_serves_every_block() {
    // Parity is distributed, so the array reconstructs a loss whichever member
    // (data- or parity-bearing) it is; check each in turn on a fresh array.
    for lost in 0..MEMBERS {
        let mut m = members();
        let mut s = scratch();
        let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
        fill(&mut array);
        array
            .member(lost)
            .unwrap()
            .device()
            .unwrap()
            .set_read_fault(Some(DriverError::DeviceOffline));
        for blk in 0..LOGICAL {
            expect(&mut array, blk);
        }
        assert_eq!(array.member_state(lost), Some(MemberState::Faulted));
        assert_eq!(array.health(), ArrayHealth::Degraded);
    }
}

#[test]
fn scrub_finds_and_heals_a_latent_media_error() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // A latent bad sector on member 1 at member-local block 5, which the read
    // path may never have consulted (reads go straight to the data member).
    array
        .member(1)
        .unwrap()
        .device()
        .unwrap()
        .set_medium_block(Some(5));
    array.begin_scrub();
    assert!(array.scrubbing());
    let mut guard = 0;
    while array.scrubbing() {
        array.scrub_step(2).unwrap();
        guard += 1;
        assert!(guard < 100);
    }
    // The bad sector was reconstructed from the survivors and written back.
    assert_eq!(
        array
            .member(1)
            .unwrap()
            .device()
            .unwrap()
            .inner
            .borrow()
            .medium_block,
        None
    );
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_parity_consistent(&array);
}

#[test]
fn a_clean_scrub_pass_is_ok_and_a_completed_pass_is_a_no_op() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    array.begin_scrub();
    while array.scrubbing() {
        array.scrub_step(3).unwrap();
    }
    assert!(!array.scrubbing());
    // A step past the end is a no-op success.
    assert_eq!(array.scrub_step(3), Ok(()));
    // Zero budget is refused.
    assert_eq!(array.scrub_step(0), Err(DriverError::BufferTooSmall));
}

#[test]
fn scrub_surfaces_an_unrepairable_block_but_still_advances() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Drop member 0 for good (degraded but serving).
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    expect(&mut array, 0);
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // A latent bad sector on a *surviving* member: with member 0 already gone,
    // that stripe row cannot be reconstructed, so the scrub surfaces the loss —
    // but the cursor still advances so the pass makes progress.
    array
        .member(1)
        .unwrap()
        .device()
        .unwrap()
        .set_medium_block(Some(4));
    array.begin_scrub();
    let mut saw_loss = false;
    let mut guard = 0;
    while array.scrubbing() {
        if array.scrub_step(1) == Err(DriverError::MediumError) {
            saw_loss = true;
        }
        guard += 1;
        assert!(guard < 100, "scrub must still terminate despite a loss");
    }
    assert!(saw_loss, "an unrepairable block must be surfaced");
    assert!(!array.scrubbing());
}

#[test]
fn resync_step_rejects_a_zero_budget() {
    let mut m = members();
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    expect(&mut array, 0);
    array
        .member(0)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(None);
    array.readd_member(0).unwrap();
    assert_eq!(array.resync_step(0), Err(DriverError::BufferTooSmall));
}

#[test]
fn device_health_aggregates_and_excludes_a_faulted_member() {
    // A composed parity array surfaces its members' telemetry rather than the
    // trait default (`Unavailable`).
    let m0 = MemBlock::new();
    m0.set_media_errors(1);
    let m1 = MemBlock::new();
    m1.set_media_errors(2);
    let m2 = MemBlock::new();
    m2.set_media_errors(4);
    let mut m = [
        ParityMember::new(m0),
        ParityMember::new(m1),
        ParityMember::new(m2),
    ];
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    let DeviceHealth::Available(h) = array.device_health().expect("health read") else {
        panic!("expected aggregated telemetry, not Unavailable");
    };
    assert_eq!(h.media_errors, 7);

    // Member 0 goes offline; a faulted member contributes no telemetry.
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
    assert_eq!(h.media_errors, 6);
}

#[test]
fn device_health_is_unavailable_without_member_telemetry() {
    let mut m = members();
    let mut s = scratch();
    let array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
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
        let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).expect("assembles");
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
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).expect("re-assembles");
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
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).expect("assembles");
    for cursor in [MB, MB + 1, u64::MAX] {
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: Some(cursor),
                resync_cursor: None,
            }),
            Err(ParityError::CursorOutOfRange)
        );
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: None,
                resync_cursor: Some(cursor),
            }),
            Err(ParityError::CursorOutOfRange)
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
        ParityMember::new(MemBlock::new()),
        ParityMember::new(MemBlock::new()),
        ParityMember::absent(),
    ];
    let mut s = scratch();
    let mut array = ParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();

    // The borrowed device is the member's whole disk, below the array's data
    // view: a write through it lands on that member alone, which is how a
    // caller reaches a member's reserved array-metadata blocks.
    array
        .member_device_mut(1)
        .expect("slot 1 holds a device")
        .write_blocks(2, &[0x5A; BS as usize])
        .unwrap();
    assert_eq!(
        array.member(1).unwrap().device().unwrap().block_byte(2),
        0x5A
    );
    assert_eq!(
        array.member(0).unwrap().device().unwrap().block_byte(2),
        0,
        "the write reached only the named member's device"
    );

    assert!(
        array.member_device_mut(2).is_none(),
        "an absent slot holds no device"
    );
    assert!(
        array.member_device_mut(MEMBERS).is_none(),
        "an index outside the array has no slot"
    );
}
