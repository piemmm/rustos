//! Host tests for the reassembly → member bridge.

use super::{fill_members, AssembleError};
use crate::mirror::{MemberRole, MirrorMember};
use crate::parity::ParityMember;
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::DriverError;
use tairix_abi::raid::MemberState;
use tairix_abi::raid::SlotDisposition;

/// A trivial [`Block`] whose only observable is the `id` a test tags it with,
/// so a placement can be traced back to the device the supplier handed over.
/// The bridge never performs I/O, so the transfer methods are unreachable
/// stubs (a real device double lives with each engine's own tests).
struct TagDev {
    id: usize,
}

impl TagDev {
    const fn new(id: usize) -> Self {
        Self { id }
    }
}

impl Block for TagDev {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: 512,
            block_count: 1,
        })
    }

    fn read_blocks(&mut self, _lba: u64, _buf: &mut [u8]) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }

    fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// The id of the device backing a placed member, or `None` for an absent slot.
fn placed_id(member: &MirrorMember<TagDev>) -> Option<usize> {
    member.device().map(|d| d.id)
}

#[test]
fn current_stale_and_absent_slots_place_with_the_right_role_and_device() {
    let slots = [
        SlotDisposition::Present {
            tag: 5,
            in_sync: true,
        },
        SlotDisposition::Present {
            tag: 2,
            in_sync: false,
        },
        SlotDisposition::Missing,
    ];
    let mut members: [MirrorMember<TagDev>; 3] = [
        MirrorMember::absent(),
        MirrorMember::absent(),
        MirrorMember::absent(),
    ];

    fill_members(&slots, &mut members, |tag| Some(TagDev::new(tag)))
        .expect("well-formed slots place");

    // The in-sync copy joins Current, backed by its own device.
    assert_eq!(members[0].role(), MemberRole::Current);
    assert_eq!(placed_id(&members[0]), Some(5));

    // The copy the generation counter proved behind joins Stale — a rebuild
    // target, never a trusted read source.
    assert_eq!(members[1].role(), MemberRole::Stale);
    assert_eq!(placed_id(&members[1]), Some(2));

    // The missing slot is absent, holding no device, so the array knows its
    // true width.
    assert_eq!(members[2].state(), MemberState::Absent);
    assert_eq!(placed_id(&members[2]), None);
}

#[test]
fn the_bridge_works_uniformly_across_redundant_member_types() {
    // The same slot table populates a RAID5 parity member buffer, proving the
    // trait spans every redundant level, not just the mirror.
    let slots = [
        SlotDisposition::Present {
            tag: 0,
            in_sync: true,
        },
        SlotDisposition::Present {
            tag: 1,
            in_sync: false,
        },
        SlotDisposition::Missing,
    ];
    let mut members: [ParityMember<TagDev>; 3] = [
        ParityMember::absent(),
        ParityMember::absent(),
        ParityMember::absent(),
    ];

    fill_members(&slots, &mut members, |tag| Some(TagDev::new(tag)))
        .expect("well-formed slots place");

    assert_eq!(members[0].role(), MemberRole::Current);
    assert_eq!(members[0].device().map(|d| d.id), Some(0));
    assert_eq!(members[1].role(), MemberRole::Stale);
    assert_eq!(members[1].device().map(|d| d.id), Some(1));
    assert_eq!(members[2].state(), MemberState::Absent);
    assert!(members[2].device().is_none());
}

#[test]
fn a_width_mismatch_fails_closed_and_leaves_the_buffer_untouched() {
    let slots = [
        SlotDisposition::Present {
            tag: 0,
            in_sync: true,
        },
        SlotDisposition::Missing,
        SlotDisposition::Missing,
    ];
    // Buffer one short of the slot table.
    let mut members: [MirrorMember<TagDev>; 2] = [MirrorMember::absent(), MirrorMember::absent()];

    let err = fill_members(&slots, &mut members, |tag| Some(TagDev::new(tag)))
        .expect_err("a mis-sized buffer fails closed");
    assert_eq!(err, AssembleError::WidthMismatch);

    // Nothing was placed: both slots remain absent.
    assert!(placed_id(&members[0]).is_none());
    assert!(placed_id(&members[1]).is_none());
}

#[test]
fn a_present_slot_whose_device_is_unavailable_fails_closed() {
    let slots = [SlotDisposition::Present {
        tag: 9,
        in_sync: true,
    }];
    let mut members: [MirrorMember<TagDev>; 1] = [MirrorMember::absent()];

    // The supplier cannot resolve the tag: fail closed rather than silently
    // demote the present slot to absent (which would drop a copy).
    let err = fill_members(&slots, &mut members, |_tag| None)
        .expect_err("an unresolvable present slot fails closed");
    assert_eq!(err, AssembleError::MissingDevice { tag: 9 });
}

#[test]
fn the_device_supplier_is_consulted_once_per_present_slot_and_never_for_a_gap() {
    let slots = [
        SlotDisposition::Present {
            tag: 7,
            in_sync: true,
        },
        SlotDisposition::Missing,
        SlotDisposition::Present {
            tag: 3,
            in_sync: false,
        },
    ];
    let mut members: [MirrorMember<TagDev>; 3] = [
        MirrorMember::absent(),
        MirrorMember::absent(),
        MirrorMember::absent(),
    ];

    let mut asked = AskLog::new();
    fill_members(&slots, &mut members, |tag| {
        asked.push(tag);
        Some(TagDev::new(tag))
    })
    .expect("well-formed slots place");

    // Exactly the two present slots' tags, in slot order; the gap was skipped.
    assert_eq!(asked.as_slice(), &[7, 3]);
    assert_eq!(placed_id(&members[0]), Some(7));
    assert_eq!(placed_id(&members[1]), None);
    assert_eq!(placed_id(&members[2]), Some(3));
}

/// A tiny fixed-capacity recorder so the test needs no allocator.
struct AskLog {
    tags: [usize; 8],
    len: usize,
}

impl AskLog {
    fn push(&mut self, tag: usize) {
        self.tags[self.len] = tag;
        self.len += 1;
    }

    fn as_slice(&self) -> &[usize] {
        &self.tags[..self.len]
    }

    fn new() -> Self {
        Self {
            tags: [0; 8],
            len: 0,
        }
    }
}
