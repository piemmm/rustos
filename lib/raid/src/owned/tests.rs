//! Host tests for the owning composed RAID device ([`OwnedRaidArray`]) over a
//! fault-injecting [`Block`] double.

use super::OwnedRaidArray;
use crate::array::{RaidArray, RaidError};
use crate::dualparity::{DualParityError, DualParityMember, SCRATCH_BLOCKS};
use crate::mirror::{MirrorArray, MirrorError, MirrorMember};
use crate::parity::{ParityError, ParityMember};
use crate::raid10::{Raid10Array, Raid10Error};
use crate::stripe::{StripeError, StripeMember};
use crate::triple::{
    TripleParityError, TripleParityMember, SCRATCH_BLOCKS as TRIPLE_SCRATCH_BLOCKS,
};
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::DriverError;
use tairix_abi::raid::{ArrayHealth, MemberState, RaidLevel};

const BS: u32 = 64;
/// Logical blocks per member.
const MB: u64 = 8;
/// One member's byte capacity (`BS * MB`).
const CAP: usize = 64 * 8;
/// The stripe unit used across the tests, in logical blocks.
const CHUNK: u32 = 2;

/// The state a [`FaultBlock`] shares with the [`FaultHandle`]s a test keeps
/// after the device itself has been moved into an [`OwnedRaidArray`], since
/// the wrapper exposes no `member(index)` accessor to reach back in.
struct Inner {
    store: [u8; CAP],
    present: bool,
    read_fault: Option<DriverError>,
    write_fault: Option<DriverError>,
}

impl Inner {
    fn new() -> Self {
        Self {
            store: [0u8; CAP],
            present: true,
            read_fault: None,
            write_fault: None,
        }
    }
}

/// An in-memory block device with injectable, post-move faults: the array
/// owns the [`FaultBlock`] itself, while a test keeps a cloned [`FaultHandle`]
/// onto the same shared state to flip a fault (and clear it again) on a
/// member the array now owns exclusively.
struct FaultBlock {
    inner: Rc<RefCell<Inner>>,
}

/// A cloned handle onto a [`FaultBlock`]'s shared state.
#[derive(Clone)]
struct FaultHandle(Rc<RefCell<Inner>>);

impl FaultBlock {
    fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(Inner::new())),
        }
    }

    fn handle(&self) -> FaultHandle {
        FaultHandle(Rc::clone(&self.inner))
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

impl FaultHandle {
    fn set_write_fault(&self, e: Option<DriverError>) {
        self.0.borrow_mut().write_fault = e;
    }

    /// The first byte of device-local block `lba` in the shared store.
    fn block_byte(&self, lba: u64) -> u8 {
        self.0.borrow().store[usize::try_from(lba).unwrap() * BS as usize]
    }
}

impl Block for FaultBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        if self.inner.borrow().present {
            Ok(BlockGeometry {
                block_size: BS,
                block_count: MB,
            })
        } else {
            Err(DriverError::DeviceFault)
        }
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        if let Some(e) = self.inner.borrow().read_fault {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        buf.copy_from_slice(&self.inner.borrow().store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        if let Some(e) = self.inner.borrow().write_fault {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        self.inner.borrow_mut().store[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// One block of `0xAB` bytes, the payload every round-trip test writes.
fn one_block() -> [u8; BS as usize] {
    [0xAB; BS as usize]
}

// -- Round-trip I/O, every level ------------------------------------------

#[test]
fn mirror_round_trips_io_identically_to_the_borrowed_dispatch() {
    let mut borrowed_members = [
        MirrorMember::new(FaultBlock::new()),
        MirrorMember::new(FaultBlock::new()),
    ];
    let mut borrowed =
        RaidArray::Mirror(MirrorArray::assemble(&mut borrowed_members).expect("assembles"));

    let mut owned = OwnedRaidArray::assemble_mirror(vec![
        MirrorMember::new(FaultBlock::new()),
        MirrorMember::new(FaultBlock::new()),
    ])
    .expect("assembles");

    assert_eq!(owned.level(), borrowed.level());
    assert_eq!(owned.member_count(), borrowed.member_count());
    assert_eq!(owned.array_geometry(), borrowed.array_geometry());

    let payload = one_block();
    borrowed.write_blocks(1, &payload).expect("borrowed write");
    owned.write_blocks(1, &payload).expect("owned write");

    let mut back_borrowed = [0u8; BS as usize];
    let mut back_owned = [0u8; BS as usize];
    borrowed
        .read_blocks(1, &mut back_borrowed)
        .expect("borrowed read");
    owned.read_blocks(1, &mut back_owned).expect("owned read");
    assert_eq!(back_borrowed, payload);
    assert_eq!(back_owned, payload);
    assert_eq!(owned.health(), borrowed.health());
}

#[test]
fn stripe_round_trips_io_and_reports_geometry() {
    let mut owned = OwnedRaidArray::assemble_stripe(
        vec![
            StripeMember::new(FaultBlock::new()),
            StripeMember::new(FaultBlock::new()),
        ],
        CHUNK,
    )
    .expect("assembles");

    assert_eq!(owned.level(), RaidLevel::Stripe);
    assert_eq!(owned.member_count(), 2);
    assert_eq!(owned.array_geometry().block_count, MB * 2);
    assert_eq!(owned.health(), ArrayHealth::Optimal);

    let payload = one_block();
    owned
        .write_blocks(3, &payload)
        .expect("write through stripe");
    let mut back = [0u8; BS as usize];
    owned
        .read_blocks(3, &mut back)
        .expect("read through stripe");
    assert_eq!(back, payload);
}

#[test]
fn parity_round_trips_io_and_dispatches_maintenance() {
    let scratch = vec![0u8; 2 * BS as usize];
    let mut owned = OwnedRaidArray::assemble_parity(
        vec![
            ParityMember::new(FaultBlock::new()),
            ParityMember::new(FaultBlock::new()),
            ParityMember::new(FaultBlock::new()),
        ],
        scratch,
        CHUNK,
    )
    .expect("assembles");

    assert_eq!(owned.level(), RaidLevel::Parity);
    assert_eq!(owned.array_geometry().block_count, MB * 2);

    let payload = one_block();
    owned
        .write_blocks(3, &payload)
        .expect("write through parity");
    let mut back = [0u8; BS as usize];
    owned
        .read_blocks(3, &mut back)
        .expect("read through parity");
    assert_eq!(back, payload);

    owned.begin_scrub().expect("scrub begins");
    let mut budget = [0u8; BS as usize];
    while owned.scrubbing() {
        owned.scrub_step(&mut budget).expect("scrub step");
    }
}

#[test]
fn dual_parity_round_trips_io() {
    let scratch = vec![0u8; SCRATCH_BLOCKS * BS as usize];
    let mut owned = OwnedRaidArray::assemble_dual_parity(
        vec![
            DualParityMember::new(FaultBlock::new()),
            DualParityMember::new(FaultBlock::new()),
            DualParityMember::new(FaultBlock::new()),
            DualParityMember::new(FaultBlock::new()),
        ],
        scratch,
        CHUNK,
    )
    .expect("assembles");

    assert_eq!(owned.level(), RaidLevel::DualParity);
    assert_eq!(owned.array_geometry().block_count, MB * 2);

    let payload = one_block();
    owned
        .write_blocks(3, &payload)
        .expect("write through dual parity");
    let mut back = [0u8; BS as usize];
    owned
        .read_blocks(3, &mut back)
        .expect("read through dual parity");
    assert_eq!(back, payload);
}

#[test]
fn triple_parity_round_trips_io() {
    let scratch = vec![0u8; TRIPLE_SCRATCH_BLOCKS * BS as usize];
    let mut owned = OwnedRaidArray::assemble_triple_parity(
        vec![
            TripleParityMember::new(FaultBlock::new()),
            TripleParityMember::new(FaultBlock::new()),
            TripleParityMember::new(FaultBlock::new()),
            TripleParityMember::new(FaultBlock::new()),
            TripleParityMember::new(FaultBlock::new()),
        ],
        scratch,
        CHUNK,
    )
    .expect("assembles");

    assert_eq!(owned.level(), RaidLevel::TripleParity);
    assert_eq!(owned.array_geometry().block_count, MB * 2);

    let payload = one_block();
    owned
        .write_blocks(3, &payload)
        .expect("write through triple parity");
    let mut back = [0u8; BS as usize];
    owned
        .read_blocks(3, &mut back)
        .expect("read through triple parity");
    assert_eq!(back, payload);
}

#[test]
fn raid10_round_trips_io_identically_to_the_borrowed_dispatch() {
    let mut borrowed_members = [
        MirrorMember::new(FaultBlock::new()),
        MirrorMember::new(FaultBlock::new()),
        MirrorMember::new(FaultBlock::new()),
        MirrorMember::new(FaultBlock::new()),
    ];
    let mut borrowed =
        RaidArray::Raid10(Raid10Array::assemble(&mut borrowed_members, CHUNK).expect("assembles"));

    let mut owned = OwnedRaidArray::assemble_raid10(
        vec![
            MirrorMember::new(FaultBlock::new()),
            MirrorMember::new(FaultBlock::new()),
            MirrorMember::new(FaultBlock::new()),
            MirrorMember::new(FaultBlock::new()),
        ],
        CHUNK,
    )
    .expect("assembles");

    assert_eq!(owned.level(), borrowed.level());
    assert_eq!(owned.member_count(), borrowed.member_count());
    assert_eq!(owned.array_geometry(), borrowed.array_geometry());

    let payload = one_block();
    borrowed.write_blocks(3, &payload).expect("borrowed write");
    owned.write_blocks(3, &payload).expect("owned write");
    let mut back_borrowed = [0u8; BS as usize];
    let mut back_owned = [0u8; BS as usize];
    borrowed
        .read_blocks(3, &mut back_borrowed)
        .expect("borrowed read");
    owned.read_blocks(3, &mut back_owned).expect("owned read");
    assert_eq!(back_borrowed, payload);
    assert_eq!(back_owned, payload);
}

// -- The stickiness property this type exists for -------------------------

#[test]
fn a_faulted_member_stays_faulted_across_calls_even_once_probeable_again() {
    let a = FaultBlock::new();
    let b = FaultBlock::new();
    let b_handle = b.handle();

    let mut owned =
        OwnedRaidArray::assemble_mirror(vec![MirrorMember::new(a), MirrorMember::new(b)])
            .expect("assembles");
    assert_eq!(owned.member_state(1), Some(MemberState::InSync));

    // A whole-device write fault on `b` drops it from the array; `a` still
    // accepts the write, so the array-level operation still succeeds.
    b_handle.set_write_fault(Some(DriverError::DeviceOffline));
    owned
        .write_blocks(0, &one_block())
        .expect("write survives on the other copy");
    assert_eq!(owned.member_state(1), Some(MemberState::Faulted));
    assert_eq!(owned.health(), ArrayHealth::Degraded);

    // Clear the fault: `b` would now probe perfectly cleanly again. If the
    // wrapper ever re-`assemble`d instead of building a `from_prepared` view,
    // this next call would silently re-admit `b` as `InSync`.
    b_handle.set_write_fault(None);
    owned
        .write_blocks(0, &one_block())
        .expect("write still succeeds on the surviving copy");
    assert_eq!(
        owned.member_state(1),
        Some(MemberState::Faulted),
        "a dropped member is never silently re-admitted by a later operation"
    );

    // A read observes the same thing: `b` is still probeable, but the array
    // never asks it, because it never re-derives membership from a probe.
    let mut back = [0u8; BS as usize];
    owned.read_blocks(0, &mut back).expect("read from `a`");
    assert_eq!(owned.member_state(1), Some(MemberState::Faulted));
}

// -- Maintenance cursors persist across separate calls ---------------------

#[test]
fn scrub_cursor_persists_across_separate_calls() {
    let mut owned = OwnedRaidArray::assemble_mirror(vec![
        MirrorMember::new(FaultBlock::new()),
        MirrorMember::new(FaultBlock::new()),
    ])
    .expect("assembles");

    owned.begin_scrub().expect("scrub begins");
    assert_eq!(owned.scrub_cursor(), 0);

    let mut scratch = [0u8; BS as usize];
    owned.scrub_step(&mut scratch).expect("first chunk");
    let after_first = owned.scrub_cursor();
    assert_eq!(after_first, 1, "one block advanced by the one-block budget");

    // A second, separate call resumes from the cursor the first call left
    // behind rather than restarting the pass.
    owned.scrub_step(&mut scratch).expect("second chunk");
    assert_eq!(owned.scrub_cursor(), after_first + 1);
}

#[test]
fn rebuild_cursor_persists_across_separate_calls() {
    let a = FaultBlock::new();
    let b = FaultBlock::new();
    let b_handle = b.handle();

    let mut owned =
        OwnedRaidArray::assemble_mirror(vec![MirrorMember::new(a), MirrorMember::new(b)])
            .expect("assembles");

    b_handle.set_write_fault(Some(DriverError::DeviceOffline));
    owned
        .write_blocks(0, &one_block())
        .expect("drops `b` from the array");
    assert_eq!(owned.member_state(1), Some(MemberState::Faulted));

    // `b` is healthy again; explicitly re-add it to begin a bounded rebuild.
    b_handle.set_write_fault(None);
    owned.readd_member(1).expect("re-add begins a rebuild");
    assert_eq!(owned.member_state(1), Some(MemberState::Resyncing));
    assert!(owned.needs_resync());

    let mut scratch = [0u8; BS as usize];
    owned
        .resync_step(&mut scratch)
        .expect("first rebuild chunk");
    assert!(
        owned.needs_resync(),
        "one block of an eight-block member has not finished the rebuild"
    );

    // The member's own rebuild cursor lives in the owned member itself, so a
    // second, separate call continues the rebuild rather than restarting it:
    // it completes in the remaining `MB - 1` steps, not `MB` more.
    for _ in 0..(MB - 1) {
        owned
            .resync_step(&mut scratch)
            .expect("further rebuild chunk");
    }
    assert!(!owned.needs_resync(), "the rebuild has completed");
    assert_eq!(owned.member_state(1), Some(MemberState::InSync));
}

// -- Member reconfiguration -------------------------------------------------

#[test]
fn remove_member_returns_the_device_and_vacates_the_slot() {
    let a = FaultBlock::new();
    let b = FaultBlock::new();
    let b_handle = b.handle();

    let mut owned =
        OwnedRaidArray::assemble_mirror(vec![MirrorMember::new(a), MirrorMember::new(b)])
            .expect("assembles");

    b_handle.set_write_fault(Some(DriverError::DeviceOffline));
    owned
        .write_blocks(0, &one_block())
        .expect("drops `b` from the array");
    assert_eq!(owned.member_state(1), Some(MemberState::Faulted));

    let removed = owned
        .remove_member(1)
        .expect("a faulted member can be pulled");
    // The real device came back, still holding the array's data.
    assert!(removed.geometry().is_ok());
    assert_eq!(owned.member_state(1), Some(MemberState::Absent));
    assert_eq!(owned.health(), ArrayHealth::Degraded);
}

#[test]
fn add_member_and_replace_member_behave_as_through_raid_array() {
    let a = FaultBlock::new();
    let b = FaultBlock::new();
    let b_handle = b.handle();

    let mut owned =
        OwnedRaidArray::assemble_mirror(vec![MirrorMember::new(a), MirrorMember::new(b)])
            .expect("assembles");

    b_handle.set_write_fault(Some(DriverError::DeviceOffline));
    owned
        .write_blocks(0, &one_block())
        .expect("drops `b` from the array");
    owned.remove_member(1).expect("pull the faulted device");
    assert_eq!(owned.member_state(1), Some(MemberState::Absent));

    // A spare installed into the absent slot begins rebuilding.
    owned
        .add_member(1, FaultBlock::new())
        .expect("a spare is admitted into the absent slot");
    assert_eq!(owned.member_state(1), Some(MemberState::Resyncing));

    // Fault it again, then hot-swap it in one step.
    owned
        .replace_member(1, FaultBlock::new())
        .expect_err("replace only targets a faulted slot, not a resyncing one");
}

// -- A stripe has no redundancy to operate on -------------------------------

#[test]
fn stripe_fails_every_redundancy_operation_closed() {
    let mut owned = OwnedRaidArray::assemble_stripe(
        vec![
            StripeMember::new(FaultBlock::new()),
            StripeMember::new(FaultBlock::new()),
        ],
        CHUNK,
    )
    .expect("assembles");

    let mut scratch = [0u8; BS as usize];
    assert_eq!(owned.begin_scrub().err(), Some(RaidError::NotRedundant));
    assert_eq!(
        owned.scrub_step(&mut scratch).err(),
        Some(RaidError::NotRedundant)
    );
    assert_eq!(
        owned.resync_step(&mut scratch).err(),
        Some(RaidError::NotRedundant)
    );
    assert_eq!(owned.readd_member(0).err(), Some(RaidError::NotRedundant));
    assert!(matches!(
        owned.remove_member(0),
        Err(RaidError::NotRedundant)
    ));
    assert_eq!(
        owned.add_member(0, FaultBlock::new()).err(),
        Some(RaidError::NotRedundant)
    );
    assert_eq!(
        owned.replace_member(0, FaultBlock::new()).err(),
        Some(RaidError::NotRedundant)
    );
}

// -- Fail-closed construction ------------------------------------------------

#[test]
fn fail_closed_construction_refuses_exactly_as_assemble_does() {
    let empty: Vec<MirrorMember<FaultBlock>> = Vec::new();
    assert_eq!(
        OwnedRaidArray::assemble_mirror(empty).err(),
        Some(MirrorError::NoMembers)
    );

    let empty: Vec<StripeMember<FaultBlock>> = Vec::new();
    assert_eq!(
        OwnedRaidArray::assemble_stripe(empty, CHUNK).err(),
        Some(StripeError::NoMembers)
    );

    let two = vec![
        ParityMember::new(FaultBlock::new()),
        ParityMember::new(FaultBlock::new()),
    ];
    assert_eq!(
        OwnedRaidArray::assemble_parity(two, vec![0u8; 2 * BS as usize], CHUNK).err(),
        Some(ParityError::TooFewMembers)
    );

    let three = vec![
        DualParityMember::new(FaultBlock::new()),
        DualParityMember::new(FaultBlock::new()),
        DualParityMember::new(FaultBlock::new()),
    ];
    assert_eq!(
        OwnedRaidArray::assemble_dual_parity(three, vec![0u8; SCRATCH_BLOCKS * BS as usize], CHUNK)
            .err(),
        Some(DualParityError::TooFewMembers)
    );

    let four = vec![
        TripleParityMember::new(FaultBlock::new()),
        TripleParityMember::new(FaultBlock::new()),
        TripleParityMember::new(FaultBlock::new()),
        TripleParityMember::new(FaultBlock::new()),
    ];
    assert_eq!(
        OwnedRaidArray::assemble_triple_parity(
            four,
            vec![0u8; TRIPLE_SCRATCH_BLOCKS * BS as usize],
            CHUNK
        )
        .err(),
        Some(TripleParityError::TooFewMembers)
    );

    // Even but short of the two-pair minimum, so this exercises
    // `TooFewMembers` rather than the odd-count check.
    let two = vec![
        MirrorMember::new(FaultBlock::new()),
        MirrorMember::new(FaultBlock::new()),
    ];
    assert_eq!(
        OwnedRaidArray::assemble_raid10(two, CHUNK).err(),
        Some(Raid10Error::TooFewMembers)
    );
}

// -- The transient-view seam -------------------------------------------------

#[test]
fn with_array_reaches_a_member_device_on_an_owning_array() {
    let first = FaultBlock::new();
    let second = FaultBlock::new();
    let first_handle = first.handle();
    let second_handle = second.handle();
    let mut owned =
        OwnedRaidArray::assemble_mirror(vec![MirrorMember::new(first), MirrorMember::new(second)])
            .expect("assembles");

    // The seam hands the transient view to the caller, so an operation the
    // wrappers do not cover — here reaching a member's own device, where its
    // reserved array-metadata blocks live below the array's data — needs no
    // new method on the owning wrapper.
    owned.with_array(|array| {
        array
            .member_device_mut(1)
            .expect("slot 1 holds a device")
            .write_blocks(2, &one_block())
            .expect("the member's own write");
    });

    assert_eq!(second_handle.block_byte(2), 0xAB);
    assert_eq!(
        first_handle.block_byte(2),
        0,
        "the write reached only the named member's device"
    );
}
