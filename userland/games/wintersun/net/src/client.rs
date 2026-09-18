//! What a client sends a realm.
//!
//! A client sends **intents**, never state. "I am holding north-west", "cast
//! spell seven at this point" — where it *is*, what it hit, what it owns are
//! the realm's answers, computed from intents it validated. There is no
//! message here that asserts a position, a hit, or a balance, and there is no
//! trust level that would add one.

use crate::bounds::{
    MAX_ACCOUNT_NAME_LEN, MAX_CHAT_BYTES, MAX_CONSOLE_COMMAND_BYTES, MAX_PASSWORD_LEN,
    MESSAGE_HEADER_LEN,
};
use crate::codec::{Reader, Writer};
use crate::error::WireError;
use crate::value::{ActionId, Aim, CharacterId, Direction, SlotIndex, SpellId, TickPhase};
use crate::value::{EntityId, TickInstant};

/// How a client proves who it is.
///
/// Three forms, and the realm treats every failure of any of them
/// identically: a wrong password, an unknown account, and an unusable key all
/// end the session with the same reason, so an attacker cannot learn which
/// accounts exist by trying them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Credential<'a> {
    /// A player on the realm's own machine. There is no secret: the gateway
    /// reads the kernel's attestation of the connecting task and needs
    /// nothing from the client at all.
    LocalAttested,
    /// A password, checked against the account's stored derivation.
    ///
    /// The bytes borrow the opened record, which is wiped when that record is
    /// dropped, so a password does not outlive the frame that carried it.
    Password(&'a [u8]),
    /// A pinned account key, signing this session's transcript.
    ///
    /// The signature covers the transcript, so it is worthless on any other
    /// session: a captured `Authenticate` cannot be replayed against a realm
    /// the attacker also connects to.
    PublicKey {
        /// The account's Ed25519 key.
        key: [u8; 32],
        /// Its signature over this session's authentication payload.
        signature: [u8; 64],
    },
}

impl Credential<'_> {
    const LOCAL: u8 = 1;
    const PASSWORD: u8 = 2;
    const PUBLIC_KEY: u8 = 3;

    const fn tag(&self) -> u8 {
        match self {
            Self::LocalAttested => Self::LOCAL,
            Self::Password(_) => Self::PASSWORD,
            Self::PublicKey { .. } => Self::PUBLIC_KEY,
        }
    }
}

/// Which audience a chat line is addressed to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ChatChannel {
    /// Everyone within earshot in the world.
    Say,
    /// The sender's party.
    Party,
    /// The sender's guild.
    Guild,
    /// One named account.
    Whisper,
    /// The whole realm.
    Realm,
}

impl ChatChannel {
    /// The stable wire code.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Say => 1,
            Self::Party => 2,
            Self::Guild => 3,
            Self::Whisper => 4,
            Self::Realm => 5,
        }
    }

    /// Decode a channel code.
    ///
    /// # Errors
    ///
    /// [`WireError::UnknownDiscriminant`] outside the closed set.
    pub const fn from_u8(value: u8) -> Result<Self, WireError> {
        Ok(match value {
            1 => Self::Say,
            2 => Self::Party,
            3 => Self::Guild,
            4 => Self::Whisper,
            5 => Self::Realm,
            _ => return Err(WireError::UnknownDiscriminant),
        })
    }

    /// Every channel, in wire-code order.
    pub const ALL: &'static [Self] = &[
        Self::Say,
        Self::Party,
        Self::Guild,
        Self::Whisper,
        Self::Realm,
    ];
}

/// What a client asks be done with an inventory slot.
///
/// The slots and what may occupy them are the realm's rules; this is the
/// verb the client asks for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ItemOp {
    /// Consume or activate what is in the slot.
    Use,
    /// Equip it.
    Equip,
    /// Unequip it.
    Unequip,
    /// Drop it into the world.
    Drop,
    /// Move it to another slot.
    MoveTo,
}

impl ItemOp {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Use => 1,
            Self::Equip => 2,
            Self::Unequip => 3,
            Self::Drop => 4,
            Self::MoveTo => 5,
        }
    }

    const fn from_u8(value: u8) -> Result<Self, WireError> {
        Ok(match value {
            1 => Self::Use,
            2 => Self::Equip,
            3 => Self::Unequip,
            4 => Self::Drop,
            5 => Self::MoveTo,
            _ => return Err(WireError::UnknownDiscriminant),
        })
    }

    /// Every operation, in wire-code order.
    pub const ALL: &'static [Self] = &[
        Self::Use,
        Self::Equip,
        Self::Unequip,
        Self::Drop,
        Self::MoveTo,
    ];
}

/// What the player is asking to do.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IntentKind {
    /// Hold a direction. The realm decides how far that gets you.
    Move(Direction),
    /// Perform an action from the realm's action table.
    Action {
        /// Which action.
        action: ActionId,
        /// Where it is aimed and when the client saw that.
        aim: Aim,
    },
    /// Cast a spell.
    Cast {
        /// Which spell.
        spell: SpellId,
        /// Where it is aimed and when the client saw that.
        aim: Aim,
    },
    /// Interact with an entity: a door, a container, a vendor.
    Interact {
        /// Which entity.
        entity: EntityId,
    },
    /// Do something with an inventory slot.
    Item {
        /// Which verb.
        op: ItemOp,
        /// Which slot.
        slot: SlotIndex,
        /// The destination slot, present only for [`ItemOp::MoveTo`].
        target_slot: Option<SlotIndex>,
    },
}

/// One intent, stamped so the realm can order it and the client can replay it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Intent {
    /// The client's own monotonic counter. The realm echoes the last one it
    /// applied, and the client replays everything after it over each
    /// authoritative state — which is what makes its own movement feel
    /// immediate without the realm ever believing it.
    pub sequence: u64,
    /// The tick the client believes it is acting in, and where inside that
    /// tick the input was sampled.
    ///
    /// The realm places the action *within* the tick rather than snapping it
    /// to the boundary, recovering most of the granularity a fixed step
    /// costs. It is an input to validate, never authority: the realm's clock
    /// decides what a tick is.
    pub sampled: TickInstant,
    /// What the player is asking to do.
    pub kind: IntentKind,
}

impl Intent {
    fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let sequence = r.u64()?;
        let tick = r.u64()?;
        let phase = TickPhase(r.u16()?);
        let kind = match r.u8()? {
            1 => IntentKind::Move(Direction::new(r.i16()?, r.i16()?)?),
            2 => IntentKind::Action {
                action: ActionId(r.u16()?),
                aim: Aim::read(r)?,
            },
            3 => IntentKind::Cast {
                spell: SpellId(r.u16()?),
                aim: Aim::read(r)?,
            },
            4 => IntentKind::Interact {
                entity: EntityId(r.u64()?),
            },
            5 => {
                let op = ItemOp::from_u8(r.u8()?)?;
                let slot = SlotIndex(r.u16()?);
                let has_target = r.flag()?;
                let target = r.u16()?;
                if has_target != matches!(op, ItemOp::MoveTo) {
                    return Err(WireError::FieldOutOfRange);
                }
                if !has_target && target != 0 {
                    return Err(WireError::NonCanonicalPadding);
                }
                IntentKind::Item {
                    op,
                    slot,
                    target_slot: has_target.then_some(SlotIndex(target)),
                }
            }
            _ => return Err(WireError::UnknownDiscriminant),
        };
        Ok(Self {
            sequence,
            sampled: TickInstant { tick, phase },
            kind,
        })
    }

    fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        w.u64(self.sequence)?;
        w.u64(self.sampled.tick)?;
        w.u16(self.sampled.phase.0)?;
        match &self.kind {
            IntentKind::Move(direction) => {
                w.u8(1)?;
                w.i16(direction.x())?;
                w.i16(direction.y())
            }
            IntentKind::Action { action, aim } => {
                w.u8(2)?;
                w.u16(action.0)?;
                aim.write(w)
            }
            IntentKind::Cast { spell, aim } => {
                w.u8(3)?;
                w.u16(spell.0)?;
                aim.write(w)
            }
            IntentKind::Interact { entity } => {
                w.u8(4)?;
                w.u64(entity.0)
            }
            IntentKind::Item {
                op,
                slot,
                target_slot,
            } => {
                if target_slot.is_some() != matches!(op, ItemOp::MoveTo) {
                    return Err(WireError::FieldOutOfRange);
                }
                w.u8(5)?;
                w.u8(op.as_u8())?;
                w.u16(slot.0)?;
                w.flag(target_slot.is_some())?;
                w.u16(target_slot.map_or(0, |s| s.0))
            }
        }
    }
}

/// A message from a client to a realm.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ClientMessage<'a> {
    /// Prove who you are. Sent first, inside the encrypted session.
    Authenticate {
        /// The realm account being claimed.
        account: &'a str,
        /// How it is being proved.
        credential: Credential<'a>,
    },
    /// Enter the world as one of the account's characters.
    SelectCharacter {
        /// Which character.
        character: CharacterId,
    },
    /// Ask to do something.
    Intent(Intent),
    /// Say something.
    Chat {
        /// To whom.
        channel: ChatChannel,
        /// The named account, for a whisper and nothing else.
        target: Option<&'a str>,
        /// What was said. Never interpreted, never a command path.
        body: &'a str,
    },
    /// Run a realm console command. Authority for it is the realm's to
    /// decide, against the account's role; a client-side role check would be
    /// decoration.
    ConsoleCommand {
        /// The command line.
        body: &'a str,
    },
    /// Measure the round trip.
    Ping {
        /// Echoed back in the reply.
        token: u64,
    },
}

impl<'a> ClientMessage<'a> {
    const AUTHENTICATE: u16 = 1;
    const SELECT_CHARACTER: u16 = 2;
    const INTENT: u16 = 3;
    const CHAT: u16 = 4;
    const CONSOLE_COMMAND: u16 = 5;
    const PING: u16 = 6;

    /// The message's kind tag.
    #[must_use]
    pub const fn kind(&self) -> u16 {
        match self {
            Self::Authenticate { .. } => Self::AUTHENTICATE,
            Self::SelectCharacter { .. } => Self::SELECT_CHARACTER,
            Self::Intent(_) => Self::INTENT,
            Self::Chat { .. } => Self::CHAT,
            Self::ConsoleCommand { .. } => Self::CONSOLE_COMMAND,
            Self::Ping { .. } => Self::PING,
        }
    }

    /// Decode one message from a record's plaintext.
    ///
    /// Total: any byte string yields this message or a typed refusal. Every
    /// field is bounds-checked here, before anything the realm calls a rule
    /// can see it.
    ///
    /// # Errors
    ///
    /// A typed [`WireError`] for an unknown kind, a short or over-long field,
    /// a malformed text field, a non-canonical encoding, or trailing bytes.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let message = match r.u16()? {
            Self::AUTHENTICATE => {
                let tag = r.u8()?;
                let account = r.text8(1, MAX_ACCOUNT_NAME_LEN)?;
                let credential = match tag {
                    Credential::LOCAL => Credential::LocalAttested,
                    Credential::PASSWORD => {
                        let len = usize::from(r.u8()?);
                        if len == 0 || len > MAX_PASSWORD_LEN {
                            return Err(WireError::BoundExceeded);
                        }
                        Credential::Password(r.take(len)?)
                    }
                    Credential::PUBLIC_KEY => Credential::PublicKey {
                        key: r.array::<32>()?,
                        signature: r.array::<64>()?,
                    },
                    _ => return Err(WireError::UnknownDiscriminant),
                };
                Self::Authenticate {
                    account,
                    credential,
                }
            }
            Self::SELECT_CHARACTER => Self::SelectCharacter {
                character: CharacterId(r.u64()?),
            },
            Self::INTENT => Self::Intent(Intent::read(&mut r)?),
            Self::CHAT => {
                let channel = ChatChannel::from_u8(r.u8()?)?;
                let target = r.optional_text8(MAX_ACCOUNT_NAME_LEN)?;
                // A whisper names exactly one account; every other channel
                // names none. Either mismatch is a frame no encoder produces.
                if target.is_some() != matches!(channel, ChatChannel::Whisper) {
                    return Err(WireError::FieldOutOfRange);
                }
                Self::Chat {
                    channel,
                    target,
                    body: r.text16(1, MAX_CHAT_BYTES, false)?,
                }
            }
            Self::CONSOLE_COMMAND => Self::ConsoleCommand {
                body: r.text16(1, MAX_CONSOLE_COMMAND_BYTES, false)?,
            },
            Self::PING => Self::Ping { token: r.u64()? },
            _ => return Err(WireError::UnknownDiscriminant),
        };
        r.finish()?;
        Ok(message)
    }

    /// Encode into `out`, returning the length written.
    ///
    /// The encoder applies the same bounds as the decoder, so it can never
    /// emit a frame its own decoder would refuse.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short, or a typed refusal
    /// when a field is outside its bound.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.u16(self.kind())?;
        match self {
            Self::Authenticate {
                account,
                credential,
            } => {
                w.u8(credential.tag())?;
                w.text8(account, 1, MAX_ACCOUNT_NAME_LEN)?;
                match credential {
                    Credential::LocalAttested => {}
                    Credential::Password(password) => {
                        if password.is_empty() || password.len() > MAX_PASSWORD_LEN {
                            return Err(WireError::BoundExceeded);
                        }
                        let Ok(len) = u8::try_from(password.len()) else {
                            return Err(WireError::BoundExceeded);
                        };
                        w.u8(len)?;
                        w.bytes(password)?;
                    }
                    Credential::PublicKey { key, signature } => {
                        w.bytes(key)?;
                        w.bytes(signature)?;
                    }
                }
            }
            Self::SelectCharacter { character } => w.u64(character.0)?,
            Self::Intent(intent) => intent.write(&mut w)?,
            Self::Chat {
                channel,
                target,
                body,
            } => {
                if target.is_some() != matches!(channel, ChatChannel::Whisper) {
                    return Err(WireError::FieldOutOfRange);
                }
                w.u8(channel.as_u8())?;
                match target {
                    Some(name) => w.text8(name, 1, MAX_ACCOUNT_NAME_LEN)?,
                    None => w.u8(0)?,
                }
                w.text16(body, 1, MAX_CHAT_BYTES, false)?;
            }
            Self::ConsoleCommand { body } => {
                w.text16(body, 1, MAX_CONSOLE_COMMAND_BYTES, false)?;
            }
            Self::Ping { token } => w.u64(*token)?,
        }
        debug_assert!(w.written() >= MESSAGE_HEADER_LEN);
        Ok(w.written())
    }
}

#[cfg(test)]
mod tests;
