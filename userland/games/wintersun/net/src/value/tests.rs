use super::{
    Aim, Direction, EntityId, EntityKind, EntityState, Facing, GameEvent, ItemId, NodeState,
    PlayEvent, ResourceNodeId, SpellId, StructureId, TickInstant, TickPhase, WorldChange,
    WorldEdit, WorldPoint, WorldVector,
};
use crate::bounds::{AIM_LEN, GAME_EVENT_LEN, WORLD_EDIT_LEN};
use crate::codec::{Reader, WireItem, Writer};
use crate::error::WireError;

fn round_trip<T: WireItem + PartialEq + core::fmt::Debug>(item: T) {
    let mut out = [0u8; 64];
    let mut w = Writer::new(&mut out);
    item.write(&mut w).expect("fits");
    assert_eq!(
        w.written(),
        T::WIRE_LEN,
        "every variant must occupy the same fixed width"
    );
    let n = w.written();
    let mut r = Reader::new(&out[..n]);
    assert_eq!(T::read(&mut r).expect("decodes"), item);
    assert_eq!(r.finish(), Ok(()));
}

const POINT: WorldPoint = WorldPoint {
    x: -1_234_567,
    y: 89_012,
};

#[test]
fn a_direction_admits_a_unit_vector_and_refuses_a_longer_one() {
    assert_eq!(Direction::new(0, 0), Ok(Direction::still()));
    let diagonal = Direction::new(-23_170, 23_170).expect("a rounded unit diagonal");
    assert_eq!(diagonal.x(), -23_170);
    assert_eq!(diagonal.y(), 23_170);
    assert!(Direction::new(32_767, 0).is_ok());
    assert_eq!(
        Direction::new(32_767, 32_767),
        Err(WireError::FieldOutOfRange),
        "a component-wise saturated vector is root-two long"
    );
    assert_eq!(
        Direction::new(i16::MIN, 0),
        Err(WireError::FieldOutOfRange),
        "the one component with no positive counterpart"
    );
    assert_eq!(Direction::new(0, i16::MIN), Err(WireError::FieldOutOfRange));
}

#[test]
fn a_heading_points_where_the_axes_say() {
    // Zero is east and the turn advances toward south, which is the sense
    // `WorldPoint`'s axes have.
    assert_eq!(Facing::towards(1, 0), Some(Facing(0)));
    assert_eq!(Facing::towards(0, 1), Some(Facing(0x4000)));
    assert_eq!(Facing::towards(-1, 0), Some(Facing(0x8000)));
    assert_eq!(Facing::towards(0, -1), Some(Facing(0xC000)));
    assert_eq!(Facing::towards(1, 1), Some(Facing(0x2000)));
}

#[test]
fn a_vector_pointing_nowhere_has_no_heading() {
    assert_eq!(Facing::towards(0, 0), None);
}

#[test]
fn a_heading_is_the_inverse_of_its_own_unit_vector() {
    for raw in (0..=u16::MAX).step_by(97) {
        let facing = Facing(raw);
        let (x, y) = facing.unit_vector();
        // Back through a scaled integer pair, which is what a caller
        // actually holds: a held direction or a movement delta.
        let scale = 1_000_000.0;
        let back = Facing::towards(
            tairix_util::mathf::round_i32(x * scale),
            tairix_util::mathf::round_i32(y * scale),
        )
        .expect("a unit vector points somewhere");
        let error = i32::from(back.0) - i32::from(raw);
        let wrapped = error
            .rem_euclid(1 << 16)
            .min((1 << 16) - error.rem_euclid(1 << 16));
        assert!(wrapped <= 1, "heading {raw} came back as {} ", back.0);
    }
}

#[test]
fn a_heading_is_scale_invariant() {
    let near = Facing::towards(3, 4).expect("a heading");
    let far = Facing::towards(3_000_000, 4_000_000).expect("a heading");
    assert_eq!(near, far, "only the direction matters, never the length");
}

#[test]
fn an_entity_state_round_trips() {
    round_trip(EntityState {
        id: EntityId(0x0102_0304_0506_0708),
        kind: EntityKind(42),
        at: POINT,
        motion: WorldVector { x: -7, y: 9 },
        facing: Facing(0xBEEF),
    });
}

#[test]
fn an_entity_id_round_trips() {
    round_trip(EntityId(u64::MAX));
    round_trip(EntityId(0));
}

#[test]
fn every_world_edit_variant_round_trips_at_one_width() {
    for change in [
        WorldChange::Height(-4_096),
        WorldChange::Material {
            material: 9,
            weight: 200,
        },
        WorldChange::Structure(Some(StructureId(777))),
        WorldChange::Structure(None),
        WorldChange::ResourceNode {
            node: ResourceNodeId(5),
            state: NodeState::Available,
        },
        WorldChange::ResourceNode {
            node: ResourceNodeId(6),
            state: NodeState::Depleted,
        },
        WorldChange::ResourceNode {
            node: ResourceNodeId(7),
            state: NodeState::Respawning,
        },
    ] {
        round_trip(WorldEdit {
            cell_x: 11,
            cell_y: 22,
            change,
        });
    }
}

#[test]
fn an_unknown_world_change_kind_is_refused() {
    let mut bytes = [0u8; WORLD_EDIT_LEN];
    bytes[4] = 9;
    assert_eq!(
        WorldEdit::read(&mut Reader::new(&bytes)),
        Err(WireError::UnknownDiscriminant)
    );
    bytes[4] = 0;
    assert_eq!(
        WorldEdit::read(&mut Reader::new(&bytes)),
        Err(WireError::UnknownDiscriminant)
    );
}

#[test]
fn an_unknown_node_state_is_refused() {
    let mut out = [0u8; WORLD_EDIT_LEN];
    let mut w = Writer::new(&mut out);
    WorldEdit {
        cell_x: 0,
        cell_y: 0,
        change: WorldChange::ResourceNode {
            node: ResourceNodeId(1),
            state: NodeState::Available,
        },
    }
    .write(&mut w)
    .expect("fits");
    // Corrupt the state byte, which sits after the kind and the node id.
    out[9] = 0x55;
    assert_eq!(
        WorldEdit::read(&mut Reader::new(&out)),
        Err(WireError::UnknownDiscriminant)
    );
}

#[test]
fn a_world_edit_with_dirty_padding_is_refused() {
    let mut out = [0u8; WORLD_EDIT_LEN];
    let mut w = Writer::new(&mut out);
    WorldEdit {
        cell_x: 1,
        cell_y: 2,
        change: WorldChange::Height(3),
    }
    .write(&mut w)
    .expect("fits");
    assert!(WorldEdit::read(&mut Reader::new(&out)).is_ok());
    // The height variant uses two of five payload bytes; the rest is padding
    // and a set bit there would be a covert channel.
    out[WORLD_EDIT_LEN - 1] = 1;
    assert_eq!(
        WorldEdit::read(&mut Reader::new(&out)),
        Err(WireError::NonCanonicalPadding)
    );
}

#[test]
fn an_absent_structure_must_leave_its_id_zero() {
    let mut out = [0u8; WORLD_EDIT_LEN];
    let mut w = Writer::new(&mut out);
    WorldEdit {
        cell_x: 0,
        cell_y: 0,
        change: WorldChange::Structure(None),
    }
    .write(&mut w)
    .expect("fits");
    out[6] = 1;
    assert_eq!(
        WorldEdit::read(&mut Reader::new(&out)),
        Err(WireError::NonCanonicalPadding)
    );
}

#[test]
fn every_play_event_variant_round_trips_at_one_width() {
    for event in [
        PlayEvent::Damage {
            target: EntityId(3),
            source: Some(EntityId(4)),
            amount: 1_234,
        },
        PlayEvent::Damage {
            target: EntityId(3),
            source: None,
            amount: 0,
        },
        PlayEvent::Cast {
            caster: EntityId(5),
            spell: SpellId(7),
        },
        PlayEvent::Pickup {
            actor: EntityId(6),
            item: ItemId(8),
        },
        PlayEvent::Death {
            entity: EntityId(9),
        },
    ] {
        round_trip(GameEvent {
            tick: 90_113,
            at: POINT,
            event,
        });
    }
}

#[test]
fn an_unknown_play_event_kind_is_refused() {
    let mut bytes = [0u8; GAME_EVENT_LEN];
    bytes[8] = 200;
    assert_eq!(
        GameEvent::read(&mut Reader::new(&bytes)),
        Err(WireError::UnknownDiscriminant)
    );
}

#[test]
fn a_play_event_with_dirty_padding_is_refused() {
    let mut out = [0u8; GAME_EVENT_LEN];
    let mut w = Writer::new(&mut out);
    GameEvent {
        tick: 1,
        at: POINT,
        event: PlayEvent::Death {
            entity: EntityId(9),
        },
    }
    .write(&mut w)
    .expect("fits");
    out[GAME_EVENT_LEN - 1] = 0x80;
    assert_eq!(
        GameEvent::read(&mut Reader::new(&out)),
        Err(WireError::NonCanonicalPadding)
    );
}

#[test]
fn an_absent_damage_source_must_leave_its_id_zero() {
    let mut out = [0u8; GAME_EVENT_LEN];
    let mut w = Writer::new(&mut out);
    GameEvent {
        tick: 1,
        at: POINT,
        event: PlayEvent::Damage {
            target: EntityId(3),
            source: None,
            amount: 5,
        },
    }
    .write(&mut w)
    .expect("fits");
    // tick(8) kind(1) point(8) target(8) flag(1) then the source id.
    out[26] = 1;
    assert_eq!(
        GameEvent::read(&mut Reader::new(&out)),
        Err(WireError::NonCanonicalPadding)
    );
}

#[test]
fn an_aim_round_trips_with_and_without_a_target() {
    for target in [Some(EntityId(77)), None] {
        let aim = Aim {
            target,
            at: POINT,
            viewed: TickInstant {
                tick: 4_242,
                phase: TickPhase(0x8000),
            },
        };
        let mut out = [0u8; AIM_LEN];
        let mut w = Writer::new(&mut out);
        aim.write(&mut w).expect("fits");
        assert_eq!(w.written(), AIM_LEN);
        let mut r = Reader::new(&out);
        assert_eq!(Aim::read(&mut r), Ok(aim));
        assert_eq!(r.finish(), Ok(()));
    }
}

#[test]
fn an_absent_aim_target_must_leave_its_id_zero() {
    let mut out = [0u8; AIM_LEN];
    let mut w = Writer::new(&mut out);
    Aim {
        target: None,
        at: POINT,
        viewed: TickInstant::default(),
    }
    .write(&mut w)
    .expect("fits");
    out[1] = 1;
    assert_eq!(
        Aim::read(&mut Reader::new(&out)),
        Err(WireError::NonCanonicalPadding)
    );
}

#[test]
fn a_truncated_item_is_refused_not_a_panic() {
    for len in 0..WORLD_EDIT_LEN {
        let bytes = [0u8; WORLD_EDIT_LEN];
        let refused = WorldEdit::read(&mut Reader::new(&bytes[..len]));
        assert!(refused.is_err(), "a short world edit must be refused");
    }
    for len in 0..GAME_EVENT_LEN {
        let bytes = [0u8; GAME_EVENT_LEN];
        assert!(GameEvent::read(&mut Reader::new(&bytes[..len])).is_err());
    }
}
