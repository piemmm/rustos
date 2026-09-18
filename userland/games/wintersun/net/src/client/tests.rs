use super::{ChatChannel, ClientMessage, Credential, Intent, IntentKind, ItemOp};
use crate::bounds::{MAX_ACCOUNT_NAME_LEN, MAX_CHAT_BYTES, MAX_PASSWORD_LEN, MAX_PLAINTEXT_LEN};
use crate::error::WireError;
use crate::value::{
    ActionId, Aim, CharacterId, Direction, EntityId, SlotIndex, SpellId, TickInstant, TickPhase,
    WorldPoint,
};

/// A run of `len` `a`s, built without an allocator so the unit tests stay
/// inside the crate's `no_std`, allocation-free surface.
fn filler(buffer: &mut [u8], len: usize) -> &str {
    buffer[..len].fill(b'a');
    core::str::from_utf8(&buffer[..len]).expect("ascii")
}

fn aim() -> Aim {
    Aim {
        target: Some(EntityId(412)),
        at: WorldPoint { x: -900, y: 900 },
        viewed: TickInstant {
            tick: 90_113,
            phase: TickPhase(0x2000),
        },
    }
}

fn intent(kind: IntentKind) -> ClientMessage<'static> {
    ClientMessage::Intent(Intent {
        sequence: 7,
        sampled: TickInstant {
            tick: 90_115,
            phase: TickPhase(0x4000),
        },
        kind,
    })
}

/// Every message the vocabulary defines, one of each shape.
fn every_message() -> [ClientMessage<'static>; 14] {
    [
        ClientMessage::Authenticate {
            account: "player",
            credential: Credential::LocalAttested,
        },
        ClientMessage::Authenticate {
            account: "player",
            credential: Credential::Password(b"correct horse"),
        },
        ClientMessage::Authenticate {
            account: "player",
            credential: Credential::PublicKey {
                key: [3u8; 32],
                signature: [4u8; 64],
            },
        },
        ClientMessage::SelectCharacter {
            character: CharacterId(0xDEAD_BEEF),
        },
        intent(IntentKind::Move(
            Direction::new(-23_170, -23_170).expect("unit diagonal"),
        )),
        intent(IntentKind::Move(Direction::still())),
        intent(IntentKind::Action {
            action: ActionId(3),
            aim: aim(),
        }),
        intent(IntentKind::Cast {
            spell: SpellId(7),
            aim: Aim {
                target: None,
                ..aim()
            },
        }),
        intent(IntentKind::Interact {
            entity: EntityId(55),
        }),
        intent(IntentKind::Item {
            op: ItemOp::Use,
            slot: SlotIndex(2),
            target_slot: None,
        }),
        intent(IntentKind::Item {
            op: ItemOp::MoveTo,
            slot: SlotIndex(2),
            target_slot: Some(SlotIndex(9)),
        }),
        ClientMessage::Chat {
            channel: ChatChannel::Say,
            target: None,
            body: "hello, realm",
        },
        ClientMessage::Chat {
            channel: ChatChannel::Whisper,
            target: Some("other"),
            body: "just you",
        },
        ClientMessage::Ping {
            token: 0x0102_0304_0506_0708,
        },
    ]
}

fn round_trip(message: &ClientMessage<'_>) -> usize {
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    let n = message.encode(&mut out).expect("encodes");
    assert_eq!(&ClientMessage::decode(&out[..n]).expect("decodes"), message);
    n
}

#[test]
fn every_message_round_trips_exactly() {
    for message in every_message() {
        let n = round_trip(&message);
        assert!(n <= MAX_PLAINTEXT_LEN);
    }
    // The console command is its own case because its body is bounded
    // separately from chat's.
    round_trip(&ClientMessage::ConsoleCommand {
        body: "realm stats --zone 3",
    });
}

#[test]
fn every_message_kind_is_distinct() {
    let mut seen = [false; 16];
    for message in every_message() {
        let kind = usize::from(message.kind());
        assert!(kind < seen.len());
        seen[kind] = true;
    }
    assert_eq!(seen.iter().filter(|s| **s).count(), 5);
}

#[test]
fn an_unknown_message_kind_is_refused() {
    for kind in [0u16, 7, 99, u16::MAX] {
        let bytes = kind.to_le_bytes();
        assert_eq!(
            ClientMessage::decode(&bytes),
            Err(WireError::UnknownDiscriminant)
        );
    }
}

#[test]
fn trailing_bytes_after_a_message_are_refused() {
    let message = ClientMessage::Ping { token: 1 };
    let mut out = [0u8; 64];
    let n = message.encode(&mut out).expect("encodes");
    assert!(ClientMessage::decode(&out[..n]).is_ok());
    assert_eq!(
        ClientMessage::decode(&out[..=n]),
        Err(WireError::TrailingBytes)
    );
}

#[test]
fn every_truncation_of_every_message_is_refused_not_a_panic() {
    for message in every_message() {
        let mut out = [0u8; MAX_PLAINTEXT_LEN];
        let n = message.encode(&mut out).expect("encodes");
        for len in 0..n {
            assert!(
                ClientMessage::decode(&out[..len]).is_err(),
                "a truncated {message:?} must be refused"
            );
        }
    }
}

#[test]
fn an_account_name_at_its_bound_is_admitted_and_one_past_it_is_not() {
    let mut text = [0u8; MAX_ACCOUNT_NAME_LEN + 1];
    let at_bound = filler(&mut text, MAX_ACCOUNT_NAME_LEN);
    round_trip(&ClientMessage::Authenticate {
        account: at_bound,
        credential: Credential::LocalAttested,
    });

    let mut text = [0u8; MAX_ACCOUNT_NAME_LEN + 1];
    let past_bound = filler(&mut text, MAX_ACCOUNT_NAME_LEN + 1);
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ClientMessage::Authenticate {
            account: past_bound,
            credential: Credential::LocalAttested,
        }
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );

    // And the decoder refuses it too, so a hand-built frame gains nothing.
    let mut forged = [0u8; MAX_PLAINTEXT_LEN];
    forged[..2].copy_from_slice(&1u16.to_le_bytes());
    forged[2] = 1;
    let Ok(len) = u8::try_from(past_bound.len()) else {
        unreachable!("the bound is far below 255")
    };
    forged[3] = len;
    forged[4..4 + past_bound.len()].copy_from_slice(past_bound.as_bytes());
    assert_eq!(
        ClientMessage::decode(&forged[..4 + past_bound.len()]),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn an_empty_account_name_is_refused() {
    let mut out = [0u8; 64];
    assert_eq!(
        ClientMessage::Authenticate {
            account: "",
            credential: Credential::LocalAttested,
        }
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );
    assert_eq!(
        ClientMessage::decode(&[1, 0, 1, 0]),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn a_password_at_its_bound_is_admitted_and_one_past_it_is_not() {
    let at_bound = [0x61u8; MAX_PASSWORD_LEN];
    round_trip(&ClientMessage::Authenticate {
        account: "player",
        credential: Credential::Password(&at_bound),
    });

    let past_bound = [0x61u8; MAX_PASSWORD_LEN + 1];
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ClientMessage::Authenticate {
            account: "player",
            credential: Credential::Password(&past_bound),
        }
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn an_empty_password_is_refused() {
    let mut out = [0u8; 64];
    assert_eq!(
        ClientMessage::Authenticate {
            account: "player",
            credential: Credential::Password(b""),
        }
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn an_unknown_credential_kind_is_refused() {
    let mut forged = [0u8; 16];
    forged[..2].copy_from_slice(&1u16.to_le_bytes());
    forged[2] = 9;
    forged[3] = 1;
    forged[4] = b'a';
    assert_eq!(
        ClientMessage::decode(&forged[..5]),
        Err(WireError::UnknownDiscriminant)
    );
}

#[test]
fn a_chat_body_at_its_bound_is_admitted_and_one_past_it_is_not() {
    let mut text = [0u8; MAX_CHAT_BYTES + 1];
    let at_bound = filler(&mut text, MAX_CHAT_BYTES);
    round_trip(&ClientMessage::Chat {
        channel: ChatChannel::Realm,
        target: None,
        body: at_bound,
    });

    let mut text = [0u8; MAX_CHAT_BYTES + 1];
    let past_bound = filler(&mut text, MAX_CHAT_BYTES + 1);
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ClientMessage::Chat {
            channel: ChatChannel::Realm,
            target: None,
            body: past_bound,
        }
        .encode(&mut out),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn chat_carrying_a_control_sequence_is_refused() {
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ClientMessage::Chat {
            channel: ChatChannel::Say,
            target: None,
            body: "clear\u{1b}[2J",
        }
        .encode(&mut out),
        Err(WireError::BadText)
    );

    // And a hand-built frame carrying one is refused at decode, which is the
    // check that actually protects a console.
    let body = b"clear\x1b[2J";
    let mut forged = [0u8; 64];
    forged[..2].copy_from_slice(&4u16.to_le_bytes());
    forged[2] = ChatChannel::Say.as_u8();
    forged[3] = 0;
    let Ok(len) = u16::try_from(body.len()) else {
        unreachable!("a short literal")
    };
    forged[4..6].copy_from_slice(&len.to_le_bytes());
    forged[6..6 + body.len()].copy_from_slice(body);
    assert_eq!(
        ClientMessage::decode(&forged[..6 + body.len()]),
        Err(WireError::BadText)
    );
}

#[test]
fn a_whisper_needs_a_target_and_no_other_channel_may_have_one() {
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        ClientMessage::Chat {
            channel: ChatChannel::Whisper,
            target: None,
            body: "to nobody",
        }
        .encode(&mut out),
        Err(WireError::FieldOutOfRange)
    );
    assert_eq!(
        ClientMessage::Chat {
            channel: ChatChannel::Say,
            target: Some("someone"),
            body: "to everyone",
        }
        .encode(&mut out),
        Err(WireError::FieldOutOfRange)
    );

    // Forge each mismatch directly: the decoder must refuse them too.
    for (channel, target) in [(ChatChannel::Whisper, ""), (ChatChannel::Say, "other")] {
        let mut forged = [0u8; 64];
        forged[..2].copy_from_slice(&4u16.to_le_bytes());
        forged[2] = channel.as_u8();
        let Ok(tlen) = u8::try_from(target.len()) else {
            unreachable!("a short literal")
        };
        forged[3] = tlen;
        let mut at = 4;
        forged[at..at + target.len()].copy_from_slice(target.as_bytes());
        at += target.len();
        forged[at..at + 2].copy_from_slice(&2u16.to_le_bytes());
        at += 2;
        forged[at..at + 2].copy_from_slice(b"hi");
        at += 2;
        assert_eq!(
            ClientMessage::decode(&forged[..at]),
            Err(WireError::FieldOutOfRange)
        );
    }
}

#[test]
fn an_unknown_chat_channel_is_refused() {
    for code in [0u8, 6, 200, u8::MAX] {
        assert_eq!(
            ChatChannel::from_u8(code),
            Err(WireError::UnknownDiscriminant)
        );
    }
    for channel in ChatChannel::ALL {
        assert_eq!(ChatChannel::from_u8(channel.as_u8()), Ok(*channel));
    }
}

#[test]
fn a_move_target_slot_is_required_and_forbidden_everywhere_else() {
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    assert_eq!(
        intent(IntentKind::Item {
            op: ItemOp::MoveTo,
            slot: SlotIndex(1),
            target_slot: None,
        })
        .encode(&mut out),
        Err(WireError::FieldOutOfRange)
    );
    for op in [ItemOp::Use, ItemOp::Equip, ItemOp::Unequip, ItemOp::Drop] {
        assert_eq!(
            intent(IntentKind::Item {
                op,
                slot: SlotIndex(1),
                target_slot: Some(SlotIndex(2)),
            })
            .encode(&mut out),
            Err(WireError::FieldOutOfRange)
        );
    }
}

#[test]
fn a_forged_move_intent_past_a_unit_vector_is_refused() {
    // Build a legal move, then overwrite its direction with a saturated one.
    let message = intent(IntentKind::Move(Direction::new(1, 1).expect("short")));
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    let n = message.encode(&mut out).expect("encodes");
    let at = n - 4;
    out[at..at + 2].copy_from_slice(&32_767i16.to_le_bytes());
    out[at + 2..at + 4].copy_from_slice(&32_767i16.to_le_bytes());
    assert_eq!(
        ClientMessage::decode(&out[..n]),
        Err(WireError::FieldOutOfRange)
    );
}

#[test]
fn an_unknown_intent_or_item_op_is_refused() {
    let message = intent(IntentKind::Interact {
        entity: EntityId(1),
    });
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    let n = message.encode(&mut out).expect("encodes");
    // Kind byte sits after the message tag, the sequence and the instant.
    out[2 + 8 + 8 + 2] = 99;
    assert_eq!(
        ClientMessage::decode(&out[..n]),
        Err(WireError::UnknownDiscriminant)
    );

    for op in ItemOp::ALL {
        assert_eq!(ItemOp::from_u8(op.as_u8()), Ok(*op));
    }
    for code in [0u8, 6, u8::MAX] {
        assert_eq!(ItemOp::from_u8(code), Err(WireError::UnknownDiscriminant));
    }
}

#[test]
fn encoding_into_a_short_buffer_is_refused_not_a_panic() {
    for message in every_message() {
        let mut out = [0u8; MAX_PLAINTEXT_LEN];
        let n = message.encode(&mut out).expect("encodes");
        for len in 0..n {
            let mut small = [0u8; MAX_PLAINTEXT_LEN];
            assert!(
                message.encode(&mut small[..len]).is_err(),
                "a short buffer must refuse {message:?}"
            );
        }
    }
}
