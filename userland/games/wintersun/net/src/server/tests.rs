use tairix_abi::time::Time64;

use super::{
    AuthResult, ConsoleStatus, Delta, RealmParameters, ServerMessage, Snapshot, Welcome, WorldDelta,
};
use crate::bounds::{
    MAX_CHAT_BYTES, MAX_CONSOLE_REPLY_BYTES, MAX_ENTITIES_IN_INTEREST, MAX_GAME_EVENTS,
    MAX_PLAINTEXT_LEN, MAX_TICK_HZ, MAX_WORLD_EDITS,
};
use crate::client::ChatChannel;
use crate::codec::WireSeq;
use crate::error::{DisconnectReason, WireError};
use crate::value::{
    AccountId, ChunkCoord, EntityId, EntityKind, EntityState, Facing, GameEvent, PlayEvent,
    SpellId, WorldChange, WorldEdit, WorldPoint, WorldVector,
};

fn entity(id: u64) -> EntityState {
    EntityState {
        id: EntityId(id),
        kind: EntityKind(3),
        at: WorldPoint {
            x: -1_000,
            y: 2_000,
        },
        motion: WorldVector { x: 4, y: -5 },
        facing: Facing(0x1234),
    }
}

fn edit(cell: u16) -> WorldEdit {
    WorldEdit {
        cell_x: cell,
        cell_y: cell,
        change: WorldChange::Height(-7),
    }
}

fn event(tick: u64) -> GameEvent {
    GameEvent {
        tick,
        at: WorldPoint { x: 1, y: 2 },
        event: PlayEvent::Cast {
            caster: EntityId(9),
            spell: SpellId(4),
        },
    }
}

const PARAMETERS: RealmParameters = RealmParameters {
    tick_hz: 30,
    day_length_seconds: 1_800,
};

fn round_trip(message: &ServerMessage<'_>) -> usize {
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    let n = message.encode(&mut out).expect("encodes");
    assert_eq!(&ServerMessage::decode(&out[..n]).expect("decodes"), message);
    n
}

#[test]
fn every_message_round_trips_exactly() {
    let entities = [entity(1), entity(2), entity(3)];
    let departed = [EntityId(4), EntityId(5)];
    let edits = [edit(0), edit(1)];
    let events = [event(1), event(2)];

    for message in [
        ServerMessage::Welcome(Welcome {
            protocol_version: crate::bounds::PROTOCOL_VERSION,
            realm_seed: 0x0BAD_C0DE_DEAD_BEEF,
            parameters: PARAMETERS,
            content_digest: [1u8; 32],
            world_generator_digest: [2u8; 32],
            rules_digest: [3u8; 32],
        }),
        ServerMessage::AuthResult(AuthResult::Accepted(AccountId(42))),
        ServerMessage::AuthResult(AuthResult::Refused),
        ServerMessage::Snapshot(Snapshot {
            tick: 90_113,
            acknowledged_intent: 7,
            entities: WireSeq::from_items(&entities),
        }),
        ServerMessage::Snapshot(Snapshot {
            tick: 0,
            acknowledged_intent: 0,
            entities: WireSeq::empty(),
        }),
        ServerMessage::Delta(Delta {
            tick: 90_114,
            acknowledged_intent: 8,
            entered: WireSeq::from_items(&entities[..1]),
            updated: WireSeq::from_items(&entities[1..]),
            departed: WireSeq::from_items(&departed),
        }),
        ServerMessage::WorldDelta(WorldDelta {
            chunk: ChunkCoord { x: -3, y: 4 },
            edits: WireSeq::from_items(&edits),
        }),
        ServerMessage::Events(WireSeq::from_items(&events)),
        ServerMessage::ChatMessage {
            channel: ChatChannel::Guild,
            sender: "someone",
            body: "in the guild",
        },
        ServerMessage::ConsoleReply {
            status: ConsoleStatus::Ok,
            body: "zone\t3\nplayers\t17",
        },
        ServerMessage::ConsoleReply {
            status: ConsoleStatus::Refused,
            body: "",
        },
        ServerMessage::Pong {
            token: 99,
            server_time: Time64::from_secs(2_147_483_648),
            server_tick: 90_115,
        },
        ServerMessage::Disconnect(DisconnectReason::ShuttingDown),
    ] {
        round_trip(&message);
    }
}

#[test]
fn every_disconnect_reason_round_trips_through_a_message() {
    for reason in DisconnectReason::ALL {
        round_trip(&ServerMessage::Disconnect(*reason));
    }
}

#[test]
fn an_unknown_disconnect_code_is_refused() {
    let mut bytes = [0u8; 4];
    bytes[..2].copy_from_slice(&10u16.to_le_bytes());
    bytes[2..].copy_from_slice(&999u16.to_le_bytes());
    assert_eq!(
        ServerMessage::decode(&bytes),
        Err(WireError::UnknownDiscriminant)
    );
}

#[test]
fn an_unknown_message_kind_is_refused() {
    for kind in [0u16, 11, 250, u16::MAX] {
        assert_eq!(
            ServerMessage::decode(&kind.to_le_bytes()),
            Err(WireError::UnknownDiscriminant)
        );
    }
}

#[test]
fn a_snapshot_at_the_interest_cap_is_admitted_and_one_past_it_is_not() {
    let mut entities = [entity(0); MAX_ENTITIES_IN_INTEREST + 1];
    for (i, e) in entities.iter_mut().enumerate() {
        let Ok(id) = u64::try_from(i) else {
            unreachable!("a small index")
        };
        *e = entity(id);
    }
    let at_cap = ServerMessage::Snapshot(Snapshot {
        tick: 1,
        acknowledged_intent: 0,
        entities: WireSeq::from_items(&entities[..MAX_ENTITIES_IN_INTEREST]),
    });
    round_trip(&at_cap);

    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ServerMessage::Snapshot(Snapshot {
            tick: 1,
            acknowledged_intent: 0,
            entities: WireSeq::from_items(&entities),
        })
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn a_forged_snapshot_count_past_the_cap_is_refused_before_any_entity_is_read() {
    // Only the header and the count are present: a decoder that trusted the
    // count would read past the end, and one that bounded it first refuses
    // here with nothing else to go on.
    let mut bytes = [0u8; 20];
    bytes[..2].copy_from_slice(&3u16.to_le_bytes());
    let Ok(count) = u16::try_from(MAX_ENTITIES_IN_INTEREST + 1) else {
        unreachable!("the cap is far below 65535")
    };
    bytes[18..20].copy_from_slice(&count.to_le_bytes());
    assert_eq!(ServerMessage::decode(&bytes), Err(WireError::BoundExceeded));

    // A wildly larger count is refused the same way, not by overflowing.
    bytes[18..20].copy_from_slice(&u16::MAX.to_le_bytes());
    assert_eq!(ServerMessage::decode(&bytes), Err(WireError::BoundExceeded));
}

#[test]
fn a_delta_is_bounded_by_the_sum_of_what_is_in_interest() {
    let entities = [entity(0); MAX_ENTITIES_IN_INTEREST];
    let half = MAX_ENTITIES_IN_INTEREST / 2;
    round_trip(&ServerMessage::Delta(Delta {
        tick: 1,
        acknowledged_intent: 0,
        entered: WireSeq::from_items(&entities[..half]),
        updated: WireSeq::from_items(&entities[half..]),
        departed: WireSeq::empty(),
    }));

    // Each run is within the cap on its own, but together they claim more
    // entities in interest than the cap allows.
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ServerMessage::Delta(Delta {
            tick: 1,
            acknowledged_intent: 0,
            entered: WireSeq::from_items(&entities),
            updated: WireSeq::from_items(&entities[..1]),
            departed: WireSeq::empty(),
        })
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );

    // And the decoder refuses the same claim from a forged frame.
    let mut bytes = [0u8; 24];
    bytes[..2].copy_from_slice(&4u16.to_le_bytes());
    let Ok(cap) = u16::try_from(MAX_ENTITIES_IN_INTEREST) else {
        unreachable!("the cap is far below 65535")
    };
    bytes[18..20].copy_from_slice(&cap.to_le_bytes());
    bytes[20..22].copy_from_slice(&1u16.to_le_bytes());
    bytes[22..24].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(ServerMessage::decode(&bytes), Err(WireError::BoundExceeded));
}

#[test]
fn a_departed_run_past_the_cap_is_refused() {
    let departed = [EntityId(0); MAX_ENTITIES_IN_INTEREST + 1];
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ServerMessage::Delta(Delta {
            tick: 1,
            acknowledged_intent: 0,
            entered: WireSeq::empty(),
            updated: WireSeq::empty(),
            departed: WireSeq::from_items(&departed),
        })
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn world_edits_and_events_are_bounded_at_their_caps() {
    let edits = [edit(0); MAX_WORLD_EDITS + 1];
    round_trip(&ServerMessage::WorldDelta(WorldDelta {
        chunk: ChunkCoord { x: 0, y: 0 },
        edits: WireSeq::from_items(&edits[..MAX_WORLD_EDITS]),
    }));
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ServerMessage::WorldDelta(WorldDelta {
            chunk: ChunkCoord { x: 0, y: 0 },
            edits: WireSeq::from_items(&edits),
        })
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );

    let events = [event(0); MAX_GAME_EVENTS + 1];
    round_trip(&ServerMessage::Events(WireSeq::from_items(
        &events[..MAX_GAME_EVENTS],
    )));
    assert_eq!(
        ServerMessage::Events(WireSeq::from_items(&events)).encode(&mut out),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn realm_parameters_are_refused_outside_their_range() {
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    for parameters in [
        RealmParameters {
            tick_hz: 0,
            day_length_seconds: 60,
        },
        RealmParameters {
            tick_hz: MAX_TICK_HZ + 1,
            day_length_seconds: 60,
        },
        RealmParameters {
            tick_hz: 30,
            day_length_seconds: 0,
        },
        RealmParameters {
            tick_hz: 30,
            day_length_seconds: u32::MAX,
        },
    ] {
        assert_eq!(
            ServerMessage::Welcome(Welcome {
                protocol_version: 1,
                realm_seed: 0,
                parameters,
                content_digest: [0u8; 32],
                world_generator_digest: [0u8; 32],
                rules_digest: [0u8; 32],
            })
            .encode(&mut out),
            Err(WireError::FieldOutOfRange)
        );
    }

    // A forged Welcome carrying them is refused at decode too.
    let good = ServerMessage::Welcome(Welcome {
        protocol_version: 1,
        realm_seed: 0,
        parameters: PARAMETERS,
        content_digest: [0u8; 32],
        world_generator_digest: [0u8; 32],
        rules_digest: [0u8; 32],
    });
    let n = good.encode(&mut out).expect("encodes");
    out[12..14].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(
        ServerMessage::decode(&out[..n]),
        Err(WireError::FieldOutOfRange)
    );
}

#[test]
fn a_refused_auth_result_carries_nothing_and_is_the_same_size_as_an_acceptance() {
    let mut refused = [0u8; 64];
    let mut accepted = [0u8; 64];
    let a = ServerMessage::AuthResult(AuthResult::Refused)
        .encode(&mut refused)
        .expect("encodes");
    let b = ServerMessage::AuthResult(AuthResult::Accepted(AccountId(7)))
        .encode(&mut accepted)
        .expect("encodes");
    assert_eq!(
        a, b,
        "a refusal must not be distinguishable by length alone"
    );
    // Its payload is padding, and a set bit there would be a covert channel.
    refused[3] = 1;
    assert_eq!(
        ServerMessage::decode(&refused[..a]),
        Err(WireError::NonCanonicalPadding)
    );
}

#[test]
fn an_unknown_auth_outcome_or_console_status_is_refused() {
    let mut bytes = [0u8; 16];
    bytes[..2].copy_from_slice(&2u16.to_le_bytes());
    bytes[2] = 9;
    assert_eq!(
        ServerMessage::decode(&bytes[..11]),
        Err(WireError::UnknownDiscriminant)
    );

    let mut reply = [0u8; 16];
    reply[..2].copy_from_slice(&8u16.to_le_bytes());
    reply[2] = 3;
    reply[3..5].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(
        ServerMessage::decode(&reply[..5]),
        Err(WireError::UnknownDiscriminant)
    );
}

#[test]
fn a_console_reply_wraps_and_tabulates_but_carries_no_escape() {
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ServerMessage::ConsoleReply {
            status: ConsoleStatus::Ok,
            body: "clear\u{1b}[2J",
        }
        .encode(&mut out),
        Err(WireError::BadText)
    );
    round_trip(&ServerMessage::ConsoleReply {
        status: ConsoleStatus::Ok,
        body: "a\tb\nc",
    });
}

#[test]
fn a_chat_message_body_is_bounded_and_control_free() {
    let mut text = [0u8; MAX_CHAT_BYTES + 1];
    text.fill(b'x');
    let at_bound = core::str::from_utf8(&text[..MAX_CHAT_BYTES]).expect("ascii");
    round_trip(&ServerMessage::ChatMessage {
        channel: ChatChannel::Realm,
        sender: "realm",
        body: at_bound,
    });

    let past_bound = core::str::from_utf8(&text).expect("ascii");
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ServerMessage::ChatMessage {
            channel: ChatChannel::Realm,
            sender: "realm",
            body: past_bound,
        }
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn a_console_reply_at_its_bound_is_admitted_and_one_past_it_is_not() {
    let mut text = [0u8; MAX_CONSOLE_REPLY_BYTES + 1];
    text.fill(b'y');
    round_trip(&ServerMessage::ConsoleReply {
        status: ConsoleStatus::Ok,
        body: core::str::from_utf8(&text[..MAX_CONSOLE_REPLY_BYTES]).expect("ascii"),
    });
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ServerMessage::ConsoleReply {
            status: ConsoleStatus::Ok,
            body: core::str::from_utf8(&text).expect("ascii"),
        }
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn every_truncation_of_a_populated_message_is_refused_not_a_panic() {
    let entities = [entity(1), entity(2)];
    let edits = [edit(0)];
    let events = [event(1)];
    for message in [
        ServerMessage::Snapshot(Snapshot {
            tick: 1,
            acknowledged_intent: 2,
            entities: WireSeq::from_items(&entities),
        }),
        ServerMessage::Delta(Delta {
            tick: 1,
            acknowledged_intent: 2,
            entered: WireSeq::from_items(&entities[..1]),
            updated: WireSeq::from_items(&entities[1..]),
            departed: WireSeq::from_items(&[EntityId(3)]),
        }),
        ServerMessage::WorldDelta(WorldDelta {
            chunk: ChunkCoord { x: 1, y: 2 },
            edits: WireSeq::from_items(&edits),
        }),
        ServerMessage::Events(WireSeq::from_items(&events)),
        ServerMessage::Pong {
            token: 1,
            server_time: Time64::from_secs(-1),
            server_tick: 2,
        },
    ] {
        let mut out = [0u8; MAX_PLAINTEXT_LEN];
        let n = message.encode(&mut out).expect("encodes");
        for len in 0..n {
            assert!(
                ServerMessage::decode(&out[..len]).is_err(),
                "a truncated {message:?} must be refused"
            );
        }
    }
}

#[test]
fn time_before_the_epoch_and_past_2038_round_trips() {
    for secs in [-2_208_988_800i64, -1, 0, 2_147_483_647, 4_102_444_800] {
        round_trip(&ServerMessage::Pong {
            token: 1,
            server_time: Time64::from_secs(secs),
            server_tick: 2,
        });
    }
}

#[test]
fn a_malformed_time_in_a_pong_is_refused() {
    let mut out = [0u8; 64];
    let n = ServerMessage::Pong {
        token: 1,
        server_time: Time64::from_secs(0),
        server_tick: 2,
    }
    .encode(&mut out)
    .expect("encodes");
    // The nanosecond field follows the token and the seconds; a value at or
    // past one second is not a normalised instant and is never carried.
    out[18..22].copy_from_slice(&1_000_000_000u32.to_le_bytes());
    assert_eq!(
        ServerMessage::decode(&out[..n]),
        Err(WireError::FieldOutOfRange)
    );
}
