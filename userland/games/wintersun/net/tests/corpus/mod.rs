//! The committed regression corpus: every frame shape the decoders accept,
//! plus the boundary cases that ring their accept/reject edge.
//!
//! The charter requires a crashing input to join a regression corpus
//! alongside a unit test, so the same bytes are replayed on every run. No
//! crash has been found in these decoders yet, so the corpus is seeded
//! instead with one frame per message kind and per variant — which is what a
//! mutating fuzzer needs as a starting point anyway, and what makes the
//! "re-encodes to the same bytes" invariant meaningful rather than vacuous.
//!
//! The frames are built from the crate's own **public encoders**, so the
//! corpus can never disagree with the source of truth: a change to a wire
//! layout moves the corpus with it, and a change that breaks the round trip
//! fails here rather than silently re-baselining.
//!
//! New crashing inputs are appended as raw byte literals with a named
//! verdict test in `regression_corpus.rs`.
//!
//! The seeding and the two input shapes the mutating harnesses draw live here
//! too, so the three of them share one definition rather than a copy each.

// Compiled into each test binary in this directory, and each uses a subset,
// so per-binary `dead_code` reports are false here.
#![allow(dead_code)]

use tairix_abi::time::Time64;
use tairix_fuzzseed::Prng;
use tairix_wintersun_net::bounds::{
    MAX_CHAT_BYTES, MAX_CONSOLE_COMMAND_BYTES, MAX_CONSOLE_REPLY_BYTES, MAX_ENTITIES_IN_INTEREST,
    MAX_GAME_EVENTS, MAX_PASSWORD_LEN, MAX_PLAINTEXT_LEN, MAX_TICK_HZ, MAX_WORLD_EDITS,
};
use tairix_wintersun_net::client::{
    ChatChannel, ClientMessage, Credential, Intent, IntentKind, ItemOp,
};
use tairix_wintersun_net::codec::WireSeq;
use tairix_wintersun_net::server::{
    AuthResult, ConsoleStatus, Delta, RealmParameters, ServerMessage, Snapshot, Welcome, WorldDelta,
};
use tairix_wintersun_net::value::{
    AccountId, ActionId, Aim, CharacterId, ChunkCoord, Direction, EntityId, EntityKind,
    EntityState, Facing, GameEvent, ItemId, NodeState, PlayEvent, ResourceNodeId, SlotIndex,
    SpellId, StructureId, TickInstant, TickPhase, WorldChange, WorldEdit, WorldPoint, WorldVector,
};

/// The shared generator, seeded and logged per run by `tairix_fuzzseed` so a
/// reported crash replays from the value in its log.
pub fn seeded(name: &str) -> Prng {
    Prng::new(tairix_fuzzseed::start(name, tairix_fuzzseed::FUZZ_SEED_ENV))
}

/// Thirty-two fresh bytes: a key, a nonce, or an ephemeral scalar.
pub fn bytes32(rng: &mut Prng) -> [u8; 32] {
    let mut out = [0u8; 32];
    rng.fill(&mut out);
    out
}

/// `len` fresh bytes.
pub fn blob(rng: &mut Prng, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    rng.fill(&mut out);
    out
}

fn encode_client(message: &ClientMessage<'_>) -> Vec<u8> {
    let mut out = vec![0u8; MAX_PLAINTEXT_LEN];
    let n = message.encode(&mut out).expect("a corpus frame encodes");
    out.truncate(n);
    out
}

fn encode_server(message: &ServerMessage<'_>) -> Vec<u8> {
    let mut out = vec![0u8; MAX_PLAINTEXT_LEN];
    let n = message.encode(&mut out).expect("a corpus frame encodes");
    out.truncate(n);
    out
}

fn aim(target: Option<EntityId>) -> Aim {
    Aim {
        target,
        at: WorldPoint {
            x: -1_048_576,
            y: 1_048_575,
        },
        viewed: TickInstant {
            tick: 90_113,
            phase: TickPhase(0x2000),
        },
    }
}

fn intent(kind: IntentKind) -> ClientMessage<'static> {
    ClientMessage::Intent(Intent {
        sequence: u64::MAX,
        sampled: TickInstant {
            tick: 90_115,
            phase: TickPhase(0xFFFF),
        },
        kind,
    })
}

fn entity(id: u64) -> EntityState {
    EntityState {
        id: EntityId(id),
        kind: EntityKind(0xFFFF),
        at: WorldPoint {
            x: i32::MIN,
            y: i32::MAX,
        },
        motion: WorldVector {
            x: i16::MIN,
            y: i16::MAX,
        },
        facing: Facing(0xFFFF),
    }
}

/// Authentication frames: every credential form, and the longest account
/// name and password each admits.
fn authentication_frames() -> Vec<Vec<u8>> {
    let longest_password = [0xFEu8; MAX_PASSWORD_LEN];
    [
        ClientMessage::Authenticate {
            account: "a",
            credential: Credential::LocalAttested,
        },
        ClientMessage::Authenticate {
            account: "an-account-of-the-longest-kind-x",
            credential: Credential::Password(&longest_password),
        },
        ClientMessage::Authenticate {
            account: "account",
            credential: Credential::Password(b"\x00"),
        },
        ClientMessage::Authenticate {
            account: "account",
            credential: Credential::PublicKey {
                key: [0xAA; 32],
                signature: [0x55; 64],
            },
        },
        ClientMessage::SelectCharacter {
            character: CharacterId(u64::MAX),
        },
        ClientMessage::SelectCharacter {
            character: CharacterId(0),
        },
    ]
    .iter()
    .map(encode_client)
    .collect()
}

/// Intent frames: every kind, every item verb, and the extremes of the
/// direction, aim and slot fields.
fn intent_frames() -> Vec<Vec<u8>> {
    [
        intent(IntentKind::Move(Direction::still())),
        intent(IntentKind::Move(
            Direction::new(32_767, 0).expect("a unit vector"),
        )),
        intent(IntentKind::Move(
            Direction::new(-23_170, -23_170).expect("a rounded unit diagonal"),
        )),
        intent(IntentKind::Action {
            action: ActionId(u16::MAX),
            aim: aim(Some(EntityId(u64::MAX))),
        }),
        intent(IntentKind::Action {
            action: ActionId(0),
            aim: aim(None),
        }),
        intent(IntentKind::Cast {
            spell: SpellId(7),
            aim: aim(Some(EntityId(1))),
        }),
        intent(IntentKind::Cast {
            spell: SpellId(0),
            aim: aim(None),
        }),
        intent(IntentKind::Interact {
            entity: EntityId(0),
        }),
        intent(IntentKind::Item {
            op: ItemOp::Use,
            slot: SlotIndex(0),
            target_slot: None,
        }),
        intent(IntentKind::Item {
            op: ItemOp::Equip,
            slot: SlotIndex(u16::MAX),
            target_slot: None,
        }),
        intent(IntentKind::Item {
            op: ItemOp::Unequip,
            slot: SlotIndex(3),
            target_slot: None,
        }),
        intent(IntentKind::Item {
            op: ItemOp::Drop,
            slot: SlotIndex(3),
            target_slot: None,
        }),
        intent(IntentKind::Item {
            op: ItemOp::MoveTo,
            slot: SlotIndex(3),
            target_slot: Some(SlotIndex(u16::MAX)),
        }),
    ]
    .iter()
    .map(encode_client)
    .collect()
}

/// Talking frames: every chat channel, the longest chat body and console
/// command, and both ends of the ping token.
fn talking_frames() -> Vec<Vec<u8>> {
    let longest_chat = "c".repeat(MAX_CHAT_BYTES);
    let longest_console = "k".repeat(MAX_CONSOLE_COMMAND_BYTES);
    [
        ClientMessage::Chat {
            channel: ChatChannel::Say,
            target: None,
            body: "x",
        },
        ClientMessage::Chat {
            channel: ChatChannel::Party,
            target: None,
            body: &longest_chat,
        },
        ClientMessage::Chat {
            channel: ChatChannel::Guild,
            target: None,
            body: "a guild line",
        },
        ClientMessage::Chat {
            channel: ChatChannel::Whisper,
            target: Some("someone-else"),
            body: "just between us",
        },
        ClientMessage::Chat {
            channel: ChatChannel::Realm,
            target: None,
            body: "shouting",
        },
        ClientMessage::ConsoleCommand {
            body: &longest_console,
        },
        ClientMessage::ConsoleCommand { body: "?" },
        ClientMessage::Ping { token: 0 },
        ClientMessage::Ping { token: u64::MAX },
    ]
    .iter()
    .map(encode_client)
    .collect()
}

/// One frame per client message kind and per variant, plus the field
/// boundaries: the longest admissible account, password, chat body, and
/// console command, and the extremes of every numeric field.
pub fn client_frames() -> Vec<Vec<u8>> {
    let mut frames = authentication_frames();
    frames.extend(intent_frames());
    frames.extend(talking_frames());
    frames
}

/// The interest-set runs a delta and a snapshot carry, filled to the cap.
fn entities_at_the_cap() -> Vec<EntityState> {
    (0..MAX_ENTITIES_IN_INTEREST)
        .map(|i| entity(u64::try_from(i).unwrap_or(u64::MAX)))
        .collect()
}

/// Every world-edit variant, filled to the cap.
fn edits_at_the_cap() -> Vec<WorldEdit> {
    (0..MAX_WORLD_EDITS)
        .map(|i| {
            let index = u16::try_from(i).unwrap_or(u16::MAX);
            WorldEdit {
                cell_x: index,
                cell_y: u16::MAX - index,
                change: match i % 6 {
                    0 => WorldChange::Height(i16::MIN),
                    1 => WorldChange::Height(i16::MAX),
                    2 => WorldChange::Material {
                        material: index,
                        weight: u8::MAX,
                    },
                    3 => WorldChange::Structure(Some(StructureId(u32::MAX))),
                    4 => WorldChange::Structure(None),
                    _ => WorldChange::ResourceNode {
                        node: ResourceNodeId(u32::from(index)),
                        state: match i % 3 {
                            0 => NodeState::Available,
                            1 => NodeState::Depleted,
                            _ => NodeState::Respawning,
                        },
                    },
                },
            }
        })
        .collect()
}

/// Every play-event variant, filled to the cap.
fn events_at_the_cap() -> Vec<GameEvent> {
    (0..MAX_GAME_EVENTS)
        .map(|i| GameEvent {
            tick: u64::try_from(i).unwrap_or(u64::MAX),
            at: WorldPoint {
                x: i32::MIN,
                y: i32::MAX,
            },
            event: match i % 5 {
                0 => PlayEvent::Damage {
                    target: EntityId(1),
                    source: Some(EntityId(2)),
                    amount: u32::MAX,
                },
                1 => PlayEvent::Damage {
                    target: EntityId(1),
                    source: None,
                    amount: 0,
                },
                2 => PlayEvent::Cast {
                    caster: EntityId(3),
                    spell: SpellId(u16::MAX),
                },
                3 => PlayEvent::Pickup {
                    actor: EntityId(4),
                    item: ItemId(u32::MAX),
                },
                _ => PlayEvent::Death {
                    entity: EntityId(5),
                },
            },
        })
        .collect()
}

/// The frames that bring a session up: the realm's identity and settings at
/// both ends of every parameter range, and both authentication verdicts.
fn admission_frames() -> Vec<Vec<u8>> {
    [
        ServerMessage::Welcome(Welcome {
            protocol_version: tairix_wintersun_net::PROTOCOL_VERSION,
            realm_seed: u64::MAX,
            parameters: RealmParameters {
                tick_hz: MAX_TICK_HZ,
                day_length_seconds: 1,
            },
            content_digest: [0x11; 32],
            world_generator_digest: [0x22; 32],
            rules_digest: [0x33; 32],
        }),
        ServerMessage::Welcome(Welcome {
            protocol_version: tairix_wintersun_net::PROTOCOL_VERSION,
            realm_seed: 0,
            parameters: RealmParameters {
                tick_hz: 1,
                day_length_seconds: 1_800,
            },
            content_digest: [0; 32],
            world_generator_digest: [0; 32],
            rules_digest: [0; 32],
        }),
        ServerMessage::AuthResult(AuthResult::Accepted(AccountId(u64::MAX))),
        ServerMessage::AuthResult(AuthResult::Refused),
    ]
    .iter()
    .map(encode_server)
    .collect()
}

/// The frames that carry the world: snapshots, deltas, world deltas and
/// events, each empty and each at its cap.
fn world_frames() -> Vec<Vec<u8>> {
    let entities = entities_at_the_cap();
    let departed: Vec<EntityId> = (0..MAX_ENTITIES_IN_INTEREST)
        .map(|i| EntityId(u64::try_from(i).unwrap_or(u64::MAX)))
        .collect();
    let edits = edits_at_the_cap();
    let events = events_at_the_cap();
    let half = MAX_ENTITIES_IN_INTEREST / 2;
    [
        ServerMessage::Snapshot(Snapshot {
            tick: 0,
            acknowledged_intent: 0,
            entities: WireSeq::empty(),
        }),
        ServerMessage::Snapshot(Snapshot {
            tick: u64::MAX,
            acknowledged_intent: u64::MAX,
            entities: WireSeq::from_items(&entities),
        }),
        ServerMessage::Delta(Delta {
            tick: 1,
            acknowledged_intent: 2,
            entered: WireSeq::empty(),
            updated: WireSeq::empty(),
            departed: WireSeq::empty(),
        }),
        ServerMessage::Delta(Delta {
            tick: u64::MAX,
            acknowledged_intent: 3,
            entered: WireSeq::from_items(&entities[..half]),
            updated: WireSeq::from_items(&entities[half..]),
            departed: WireSeq::from_items(&departed),
        }),
        ServerMessage::WorldDelta(WorldDelta {
            chunk: ChunkCoord {
                x: i32::MIN,
                y: i32::MAX,
            },
            edits: WireSeq::from_items(&edits),
        }),
        ServerMessage::WorldDelta(WorldDelta {
            chunk: ChunkCoord { x: 0, y: 0 },
            edits: WireSeq::empty(),
        }),
        ServerMessage::Events(WireSeq::from_items(&events)),
        ServerMessage::Events(WireSeq::empty()),
    ]
    .iter()
    .map(encode_server)
    .collect()
}

/// The conversational frames, plus every reason a session can end with —
/// so no disconnect code is left unexercised.
fn reply_frames() -> Vec<Vec<u8>> {
    let longest_reply = "r".repeat(MAX_CONSOLE_REPLY_BYTES);
    let mut frames: Vec<Vec<u8>> = [
        ServerMessage::ChatMessage {
            channel: ChatChannel::Whisper,
            sender: "s",
            body: "m",
        },
        ServerMessage::ConsoleReply {
            status: ConsoleStatus::Ok,
            body: &longest_reply,
        },
        ServerMessage::ConsoleReply {
            status: ConsoleStatus::Ok,
            body: "zone\t3\nplayers\t17\n",
        },
        ServerMessage::ConsoleReply {
            status: ConsoleStatus::Refused,
            body: "",
        },
        ServerMessage::Pong {
            token: u64::MAX,
            server_time: Time64::from_secs(i64::MIN),
            server_tick: 0,
        },
        ServerMessage::Pong {
            token: 0,
            // Before the epoch, and past every 32-bit boundary.
            server_time: Time64::new(-2_208_988_800, 999_999_999).expect("a valid instant"),
            server_tick: u64::MAX,
        },
    ]
    .iter()
    .map(encode_server)
    .collect();
    for reason in tairix_wintersun_net::DisconnectReason::ALL {
        frames.push(encode_server(&ServerMessage::Disconnect(*reason)));
    }
    frames
}

/// One frame per server message kind and per variant, plus the field
/// boundaries: a snapshot and a delta at the interest cap, a world delta and
/// an event batch at theirs, and every disconnect reason.
pub fn server_frames() -> Vec<Vec<u8>> {
    let mut frames = admission_frames();
    frames.extend(world_frames());
    frames.extend(reply_frames());
    frames
}
