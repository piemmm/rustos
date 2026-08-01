//! Host tests for the composer's registration → assembly → publication
//! decisions.
//!
//! These prove the *judgement*: which discovered devices form which array, and
//! when an array may be brought online. The reassembly arithmetic underneath
//! (which member is authoritative, which is stale) is proven once in the
//! shared metadata layer, and the escalation arithmetic once in the shared
//! retry cadence; here only the decisions built on them are asserted.

use alloc::vec;
use alloc::vec::Vec;

use super::{Admission, ComposerAction, MemberRegistry, MemberStanding};

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::BlockGeometry;
use tairix_abi::raid_ipc::MemberOffer;
use tairix_abi::time::Time64;
use tairix_raid::{ArraySuperblock, ArrayUuid, RaidLevel, RetryCadence, SlotDisposition};

const UUID_A: ArrayUuid = [0xA1; 16];
const UUID_B: ArrayUuid = [0xB2; 16];

const GEO: BlockGeometry = BlockGeometry {
    block_size: 512,
    block_count: 4096,
};

/// The stripe unit every striped array in these tests uses.
const CHUNK: u32 = 8;

/// A superblock claiming `slot` of a `count`-member array of `level`.
fn superblock(
    level: RaidLevel,
    array: ArrayUuid,
    count: u16,
    slot: u16,
    generation: u64,
) -> ArraySuperblock {
    ArraySuperblock {
        array_uuid: array,
        raid_level: level,
        member_count: count,
        member_slot: slot,
        geometry: GEO,
        generation,
        updated_at: Time64::from_secs(1_700_000_000),
        chunk_blocks: if level.is_striped() { CHUNK } else { 0 },
    }
}

/// The offer an agent for the device on `endpoint` would present.
fn offer(endpoint: u64) -> MemberOffer {
    MemberOffer {
        endpoint,
        window: endpoint + 0x1000,
        node: 0,
    }
}

/// The membership call the composer holds open for the device on `endpoint`.
/// Deliberately unlike the endpoint id, so a test cannot pass by confusing the
/// two.
fn membership_of(endpoint: u64) -> u64 {
    0x9000 + endpoint
}

/// Register the device on `endpoint` carrying `superblock`, asserting it was
/// admitted, and hand back its member index.
fn admit(
    registry: &mut MemberRegistry,
    endpoint: u64,
    class: BlkDeviceClass,
    superblock: ArraySuperblock,
    now_ns: u64,
) -> usize {
    match registry.admit(
        membership_of(endpoint),
        offer(endpoint),
        class,
        superblock,
        now_ns,
    ) {
        Admission::Registered { index } => index,
        other => panic!("a decodable member must be registered, got {other:?}"),
    }
}

/// The settle window an array of `class` devices is given: that class's own
/// recovery grace window, read from the shared cadence rather than restated.
fn settle_ns(class: BlkDeviceClass) -> u64 {
    RetryCadence::for_class(class).base_ns()
}

/// Compose and publish `array_uuid` the way the live composer would, returning
/// the slot table it was built from.
fn publish(registry: &mut MemberRegistry, array_uuid: ArrayUuid) -> Vec<SlotDisposition> {
    let identity = registry
        .identity(array_uuid)
        .expect("a registered array resolves");
    let mut slots = vec![SlotDisposition::Missing; usize::from(identity.member_count)];
    identity
        .fill_slots(registry.candidates(), &mut slots)
        .expect("the slot table is the array's width");
    registry.note_composed(array_uuid, &slots);
    slots
}

#[test]
fn an_offered_member_is_registered_against_its_own_metadata() {
    let mut registry = MemberRegistry::new();
    let index = admit(
        &mut registry,
        7,
        BlkDeviceClass::Removable,
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 4),
        0,
    );
    assert_eq!(index, 0);
    let held = registry.members()[index];
    // Everything the composer needs to drive and end this membership: the call
    // to answer when it ends, the transport to reach the device, the class its
    // patience is sized from, and its standing.
    assert_eq!(held.membership(), membership_of(7));
    assert_eq!(held.offer(), offer(7));
    assert_eq!(held.class(), BlkDeviceClass::Removable);
    assert_eq!(held.standing(), MemberStanding::Held);
    // The reassembly view is the one copy of the device's metadata, tagged so
    // a resolved slot maps straight back to the member that fills it.
    assert_eq!(registry.candidates()[index].tag, index);
    assert_eq!(registry.candidates()[index].superblock.member_slot, 1);
}

#[test]
fn a_second_membership_for_a_device_already_held_is_refused() {
    let mut registry = MemberRegistry::new();
    let sb = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4);
    admit(&mut registry, 7, BlkDeviceClass::Removable, sb, 0);
    assert_eq!(
        registry.admit(
            membership_of(99),
            offer(7),
            BlkDeviceClass::Removable,
            sb,
            0
        ),
        Admission::Duplicate,
        "one device must not occupy a slot twice over"
    );
    assert_eq!(registry.members().len(), 1);
    assert_eq!(registry.candidates().len(), 1);
}

#[test]
fn a_complete_array_is_composed_with_no_wait_at_all() {
    // The common path — every member discovered — must cost no delay, so an
    // array comes up at boot as promptly as a plain disk.
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::Rotational;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4),
        0,
    );
    assert!(matches!(
        registry.next_action(0),
        ComposerAction::Wait {
            deadline_ns: Some(_)
        }
    ));
    admit(
        &mut registry,
        2,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 4),
        0,
    );
    assert_eq!(
        registry.next_action(0),
        ComposerAction::Assemble { array_uuid: UUID_A }
    );
}

#[test]
fn an_incomplete_array_settles_before_it_starts_degraded() {
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::SolidState;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4),
        1_000,
    );
    let ComposerAction::Wait {
        deadline_ns: Some(deadline_ns),
    } = registry.next_action(1_000)
    else {
        panic!("a copy that may merely be slow is waited for, not written off");
    };
    assert_eq!(deadline_ns, 1_000 + settle_ns(class));
    assert!(
        deadline_ns > 1_000,
        "parking on the settle deadline must be a wait, never a spin"
    );
    // One nanosecond short of the window the array still waits; at it, the
    // surviving copy is enough and the array starts degraded.
    assert!(matches!(
        registry.next_action(deadline_ns - 1),
        ComposerAction::Wait { .. }
    ));
    assert_eq!(
        registry.next_action(deadline_ns),
        ComposerAction::Assemble { array_uuid: UUID_A }
    );
}

#[test]
fn the_settle_window_is_read_from_the_members_own_hardware() {
    // A spinning disk may legitimately still be spinning up; a solid-state one
    // that has not appeared is not coming. The wait is that difference, taken
    // from the class the device itself declared.
    let mut fast = MemberRegistry::new();
    admit(
        &mut fast,
        1,
        BlkDeviceClass::SolidState,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4),
        0,
    );
    let mut slow = MemberRegistry::new();
    admit(
        &mut slow,
        1,
        BlkDeviceClass::Rotational,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4),
        0,
    );
    let (
        ComposerAction::Wait {
            deadline_ns: Some(fast_deadline),
        },
        ComposerAction::Wait {
            deadline_ns: Some(slow_deadline),
        },
    ) = (fast.next_action(0), slow.next_action(0))
    else {
        panic!("both arrays are incomplete and waiting");
    };
    assert!(slow_deadline > fast_deadline);
}

#[test]
fn the_settle_window_follows_the_most_patient_member_of_the_array() {
    // A mixed array can only be as impatient as its slowest member: writing
    // off a spinning member because a solid-state sibling arrived first would
    // rebuild a disk that was merely still spinning up.
    let mut registry = MemberRegistry::new();
    admit(
        &mut registry,
        1,
        BlkDeviceClass::SolidState,
        superblock(RaidLevel::Mirror, UUID_A, 3, 0, 4),
        0,
    );
    admit(
        &mut registry,
        2,
        BlkDeviceClass::Rotational,
        superblock(RaidLevel::Mirror, UUID_A, 3, 1, 4),
        1_000,
    );
    let ComposerAction::Wait {
        deadline_ns: Some(deadline_ns),
    } = registry.next_action(1_000)
    else {
        panic!("a copy is still missing, so the array is waiting");
    };
    assert_eq!(
        deadline_ns,
        settle_ns(BlkDeviceClass::Rotational),
        "the window widens to the slowest member's, but still runs from the \
         array's first member: a trickle of arrivals cannot postpone assembly"
    );
}

#[test]
fn an_array_its_members_cannot_serve_is_never_brought_online() {
    // One member of a three-member RAID5 leaves two chunks of every stripe
    // unreconstructable. Publishing it would hand a filesystem a device that
    // silently cannot read parts of itself.
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::SolidState;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Parity, UUID_A, 3, 0, 4),
        0,
    );
    assert_eq!(
        registry.next_action(0),
        ComposerAction::Wait { deadline_ns: None },
        "no deadline: only a further member changes the answer, so there is nothing to time"
    );
    assert_eq!(
        registry.next_action(u64::MAX),
        ComposerAction::Wait { deadline_ns: None },
        "and no amount of waiting makes an unservable array servable"
    );
    // A second member makes the array reconstructable, and it starts degraded
    // once its window elapses.
    admit(
        &mut registry,
        2,
        class,
        superblock(RaidLevel::Parity, UUID_A, 3, 1, 4),
        0,
    );
    let ComposerAction::Wait {
        deadline_ns: Some(deadline_ns),
    } = registry.next_action(0)
    else {
        panic!("two of three members can serve, after the settle window");
    };
    assert_eq!(
        registry.next_action(deadline_ns),
        ComposerAction::Assemble { array_uuid: UUID_A }
    );
}

#[test]
fn a_stripe_missing_a_member_is_never_brought_online() {
    // RAID0 holds nothing spare, so a gap is a hole in the address space
    // however long the composer waits for it.
    let mut registry = MemberRegistry::new();
    admit(
        &mut registry,
        1,
        BlkDeviceClass::SolidState,
        superblock(RaidLevel::Stripe, UUID_A, 2, 0, 4),
        0,
    );
    assert_eq!(
        registry.next_action(u64::MAX),
        ComposerAction::Wait { deadline_ns: None }
    );
}

#[test]
fn composing_an_array_marks_only_its_own_members() {
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::Virtual;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4),
        0,
    );
    admit(
        &mut registry,
        2,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 4),
        0,
    );
    let foreign = admit(
        &mut registry,
        3,
        class,
        superblock(RaidLevel::Mirror, UUID_B, 2, 0, 9),
        0,
    );
    let slots = publish(&mut registry, UUID_A);
    assert_eq!(slots.len(), 2);
    assert_eq!(registry.members()[0].standing(), MemberStanding::Composed);
    assert_eq!(registry.members()[1].standing(), MemberStanding::Composed);
    assert_eq!(
        registry.members()[foreign].standing(),
        MemberStanding::Held,
        "a device belongs to the array its own metadata names and no other"
    );
    // The published array is not composed a second time; the other array is
    // still waiting for its own missing copy.
    assert!(matches!(
        registry.next_action(0),
        ComposerAction::Wait {
            deadline_ns: Some(_)
        }
    ));
}

#[test]
fn a_slot_table_from_another_array_marks_nothing() {
    // `note_composed` is told which array it published, so a slot table
    // resolved for a different one cannot mark the wrong devices in service.
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::Virtual;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 1, 0, 4),
        0,
    );
    let elsewhere = admit(
        &mut registry,
        2,
        class,
        superblock(RaidLevel::Mirror, UUID_B, 1, 0, 4),
        0,
    );
    registry.note_composed(
        UUID_A,
        &[SlotDisposition::Present {
            tag: elsewhere,
            in_sync: true,
        }],
    );
    assert_eq!(
        registry.members()[elsewhere].standing(),
        MemberStanding::Held
    );
}

#[test]
fn a_member_that_turns_up_late_joins_the_array_already_serving() {
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::SolidState;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 8),
        0,
    );
    let deadline_ns = settle_ns(class);
    assert_eq!(
        registry.next_action(deadline_ns),
        ComposerAction::Assemble { array_uuid: UUID_A }
    );
    publish(&mut registry, UUID_A);

    // The absent copy appears, behind the generation the survivors advanced
    // to: it rejoins as the rebuild target its own metadata says it is, never
    // as a copy the array would read from.
    let late = admit(
        &mut registry,
        2,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 7),
        deadline_ns,
    );
    assert_eq!(
        registry.next_action(deadline_ns),
        ComposerAction::Join {
            array_uuid: UUID_A,
            member: late,
            slot: 1,
            in_sync: false,
        }
    );
    registry.note_joined(late);
    assert_eq!(
        registry.next_action(deadline_ns),
        ComposerAction::Wait { deadline_ns: None },
        "a member placed once is not offered for placement again"
    );
}

#[test]
fn a_stale_claimant_of_an_occupied_slot_is_held_rather_than_swapped_in() {
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::Virtual;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 9),
        0,
    );
    admit(
        &mut registry,
        2,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 9),
        0,
    );
    publish(&mut registry, UUID_A);

    // An older copy of slot 1 — a disk pulled from this array long ago and put
    // back. It is not refused (a fresher member could yet redefine the array),
    // but it never displaces the copy in service.
    let stale = admit(
        &mut registry,
        3,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 3),
        0,
    );
    assert_eq!(
        registry.next_action(0),
        ComposerAction::Wait { deadline_ns: None }
    );
    assert_eq!(registry.members()[stale].standing(), MemberStanding::Held);
}

#[test]
fn a_member_that_disagrees_about_the_array_is_held_unused_not_refused() {
    // Its own superblock claims a different width, so the authoritative
    // members will not place it. Keeping it registered costs nothing and lets
    // a later, fresher member legitimately redefine the array — refusing it
    // would let one corrupt disk evict a healthy one from consideration.
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::Virtual;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 9),
        0,
    );
    admit(
        &mut registry,
        2,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 9),
        0,
    );
    let mismatched = admit(
        &mut registry,
        3,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 5, 4, 9),
        0,
    );
    let slots = publish(&mut registry, UUID_A);
    assert_eq!(slots.len(), 2, "the freshest members fix the array's shape");
    assert_eq!(
        registry.members()[mismatched].standing(),
        MemberStanding::Held
    );
    assert_eq!(
        registry.next_action(0),
        ComposerAction::Wait { deadline_ns: None },
        "and it is never joined into an array whose shape it contradicts"
    );
}

#[test]
fn a_refused_assembly_backs_off_instead_of_being_retried_at_once() {
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::SolidState;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 1, 0, 4),
        0,
    );
    assert_eq!(
        registry.next_action(0),
        ComposerAction::Assemble { array_uuid: UUID_A }
    );
    // The devices could not be read. A tight retry loop would hammer unwell
    // hardware, so the next attempt is paced.
    registry.note_assembly_failed(UUID_A, 0);
    let ComposerAction::Wait {
        deadline_ns: Some(first),
    } = registry.next_action(0)
    else {
        panic!("a refused assembly waits before it is tried again");
    };
    assert!(first > 0);
    assert_eq!(
        registry.next_action(first),
        ComposerAction::Assemble { array_uuid: UUID_A }
    );
    // A second refusal waits longer than the first.
    registry.note_assembly_failed(UUID_A, first);
    let ComposerAction::Wait {
        deadline_ns: Some(second),
    } = registry.next_action(first)
    else {
        panic!("still refused, so still paced");
    };
    assert!(second - first > first);
}

#[test]
fn publishing_an_array_clears_the_escalation_it_was_carrying() {
    let mut registry = MemberRegistry::new();
    admit(
        &mut registry,
        1,
        BlkDeviceClass::SolidState,
        superblock(RaidLevel::Mirror, UUID_A, 1, 0, 4),
        0,
    );
    registry.note_assembly_failed(UUID_A, 0);
    let ComposerAction::Wait {
        deadline_ns: Some(retry),
    } = registry.next_action(0)
    else {
        panic!("the refused attempt is paced");
    };
    publish(&mut registry, UUID_A);
    assert_eq!(
        registry.next_action(retry),
        ComposerAction::Wait { deadline_ns: None },
        "an array in service asks for nothing further"
    );
}

#[test]
fn the_soonest_deadline_across_the_arrays_is_the_one_to_park_on() {
    let mut registry = MemberRegistry::new();
    admit(
        &mut registry,
        1,
        BlkDeviceClass::Rotational,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4),
        0,
    );
    admit(
        &mut registry,
        2,
        BlkDeviceClass::SolidState,
        superblock(RaidLevel::Mirror, UUID_B, 2, 0, 4),
        0,
    );
    assert_eq!(
        registry.next_action(0),
        ComposerAction::Wait {
            deadline_ns: Some(settle_ns(BlkDeviceClass::SolidState))
        },
        "one park serves every array, so it is armed to whichever is due first"
    );
}

#[test]
fn releasing_a_member_renumbers_the_reassembly_view_in_step() {
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::Virtual;
    for slot in 0..3u16 {
        admit(
            &mut registry,
            u64::from(slot) + 1,
            class,
            superblock(RaidLevel::Mirror, UUID_A, 3, slot, 4),
            0,
        );
    }
    let released = registry.release(1).expect("the middle member is held");
    assert_eq!(released.offer().endpoint, 2);
    assert_eq!(
        released.membership(),
        membership_of(2),
        "the caller answers the call of the member that actually left"
    );
    assert_eq!(registry.members().len(), 2);
    assert_eq!(registry.candidates().len(), 2);
    for (index, candidate) in registry.candidates().iter().enumerate() {
        assert_eq!(candidate.tag, index, "a tag always names its own member");
    }
    assert_eq!(registry.candidates()[1].superblock.member_slot, 2);
    assert_eq!(registry.release(9), None, "there is no such member");
}

#[test]
fn an_array_whose_last_member_leaves_is_forgotten() {
    let mut registry = MemberRegistry::new();
    let class = BlkDeviceClass::SolidState;
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4),
        0,
    );
    assert!(matches!(
        registry.next_action(0),
        ComposerAction::Wait {
            deadline_ns: Some(_)
        }
    ));
    registry.release(0);
    assert_eq!(
        registry.next_action(0),
        ComposerAction::Wait { deadline_ns: None },
        "no members, no arrays, nothing to wait for"
    );
    // The array is discovered afresh when a member of it returns, settle
    // window and all, rather than resuming a stale one.
    admit(
        &mut registry,
        1,
        class,
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4),
        5_000,
    );
    assert_eq!(
        registry.next_action(5_000),
        ComposerAction::Wait {
            deadline_ns: Some(5_000 + settle_ns(class))
        }
    );
}

#[test]
fn an_empty_registry_asks_for_nothing() {
    let mut registry = MemberRegistry::default();
    assert_eq!(
        registry.next_action(0),
        ComposerAction::Wait { deadline_ns: None }
    );
    assert!(registry.members().is_empty());
    assert!(registry.identity(UUID_A).is_none());
}
