//! What a realm sends a client.
//!
//! The realm sends answers: who is where, what the stored world looks like
//! over the generated base, what just happened, and — when the session ends —
//! why. Terrain itself is never sent: the world is a pure function of its
//! seed and the client generates the ground it walks on, so the wire carries
//! only what the seed cannot predict.

use tairix_abi::time::Time64;

use crate::bounds::{
    ACCOUNT_ID_LEN, MAX_ACCOUNT_NAME_LEN, MAX_CHAT_BYTES, MAX_CONSOLE_REPLY_BYTES,
    MAX_DAY_LENGTH_SECONDS, MAX_ENTITIES_IN_INTEREST, MAX_GAME_EVENTS, MAX_TICK_HZ,
    MAX_WORLD_EDITS, MESSAGE_HEADER_LEN, TIME64_LEN,
};
use crate::client::ChatChannel;
use crate::codec::{Reader, WireSeq, Writer};
use crate::error::{DisconnectReason, WireError};
use crate::value::{AccountId, ChunkCoord, EntityId, EntityState, GameEvent, WorldEdit};

/// A digest pinning one of the three things a client and a realm must agree
/// on exactly.
pub type Digest = [u8; 32];

/// The realm settings a client needs before it can simulate or draw anything.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RealmParameters {
    /// The authoritative step rate. Thirty is the default: twenty quantises
    /// every input to fifty milliseconds, which is enough to make a dodge
    /// feel unreliable however much the client polishes it.
    pub tick_hz: u16,
    /// How long a full day-night cycle lasts.
    pub day_length_seconds: u32,
}

impl RealmParameters {
    fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let tick_hz = r.u16()?;
        let day_length_seconds = r.u32()?;
        if tick_hz == 0 || tick_hz > MAX_TICK_HZ {
            return Err(WireError::FieldOutOfRange);
        }
        if day_length_seconds == 0 || day_length_seconds > MAX_DAY_LENGTH_SECONDS {
            return Err(WireError::FieldOutOfRange);
        }
        Ok(Self {
            tick_hz,
            day_length_seconds,
        })
    }

    fn write(self, w: &mut Writer<'_>) -> Result<(), WireError> {
        if self.tick_hz == 0 || self.tick_hz > MAX_TICK_HZ {
            return Err(WireError::FieldOutOfRange);
        }
        if self.day_length_seconds == 0 || self.day_length_seconds > MAX_DAY_LENGTH_SECONDS {
            return Err(WireError::FieldOutOfRange);
        }
        w.u16(self.tick_hz)?;
        w.u32(self.day_length_seconds)
    }
}

/// What the realm is, and what a client must match to play on it.
///
/// The generator digest is not a formality. A client generates the terrain it
/// walks on, so one whose generator differs by a single stage would draw
/// ground the realm does not simulate and diverge on collision — a defect
/// that presents as "I fell through the floor" and is near-impossible to
/// diagnose from the symptom. A mismatch on any of the three is refused at
/// connect, never negotiated down.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Welcome {
    /// The protocol the realm speaks, echoing what the handshake agreed.
    pub protocol_version: u16,
    /// The seed the whole world is a pure function of.
    pub realm_seed: u64,
    /// The realm's settings.
    pub parameters: RealmParameters,
    /// Digest of the realm's content documents.
    pub content_digest: Digest,
    /// Digest of the realm's world generator.
    pub world_generator_digest: Digest,
    /// Digest of the realm's rules.
    pub rules_digest: Digest,
}

/// Whether the realm accepted the credential.
///
/// A refusal carries nothing at all — not which of the account or the secret
/// was wrong, not whether the account exists. One indistinguishable failure
/// is what stops an attacker enumerating accounts by trying them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AuthResult {
    /// Accepted, as this account.
    Accepted(AccountId),
    /// Refused.
    Refused,
}

/// Every entity in the client's interest at a tick.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Snapshot<'a> {
    /// The authoritative tick this state is from.
    pub tick: u64,
    /// The last client intent the realm had applied at that tick. Everything
    /// the client sent after it is replayed over this state.
    pub acknowledged_intent: u64,
    /// The entities themselves.
    pub entities: WireSeq<'a, EntityState>,
}

/// What changed in the client's interest since the last tick it was told
/// about.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Delta<'a> {
    /// The authoritative tick.
    pub tick: u64,
    /// The last client intent the realm had applied.
    pub acknowledged_intent: u64,
    /// Entities that came into interest.
    pub entered: WireSeq<'a, EntityState>,
    /// Entities already in interest whose state moved.
    pub updated: WireSeq<'a, EntityState>,
    /// Entities that left interest.
    pub departed: WireSeq<'a, EntityId>,
}

/// Stored changes to the generated world, for one chunk.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WorldDelta<'a> {
    /// Which chunk.
    pub chunk: ChunkCoord,
    /// The edits, applied over the chunk the client generated itself.
    pub edits: WireSeq<'a, WorldEdit>,
}

/// Whether a console command ran.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConsoleStatus {
    /// It ran; the body is its output.
    Ok,
    /// It did not; the body says why.
    Refused,
}

impl ConsoleStatus {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Ok => 1,
            Self::Refused => 2,
        }
    }

    const fn from_u8(value: u8) -> Result<Self, WireError> {
        Ok(match value {
            1 => Self::Ok,
            2 => Self::Refused,
            _ => return Err(WireError::UnknownDiscriminant),
        })
    }
}

/// A message from a realm to a client.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ServerMessage<'a> {
    /// The realm's identity and settings, sent once the session is up.
    Welcome(Welcome),
    /// The verdict on the client's credential.
    AuthResult(AuthResult),
    /// Everything in interest at a tick.
    Snapshot(Snapshot<'a>),
    /// What changed since the last tick.
    Delta(Delta<'a>),
    /// Stored world changes for one chunk.
    WorldDelta(WorldDelta<'a>),
    /// What just happened, for sound and effects.
    Events(WireSeq<'a, GameEvent>),
    /// Something somebody said.
    ChatMessage {
        /// Which channel it arrived on.
        channel: ChatChannel,
        /// Who said it.
        sender: &'a str,
        /// What they said. Data, never a control sequence.
        body: &'a str,
    },
    /// The outcome of a console command.
    ConsoleReply {
        /// Whether it ran.
        status: ConsoleStatus,
        /// Its output, or the reason it did not run.
        body: &'a str,
    },
    /// The answer to a ping, carrying the only clock that counts.
    Pong {
        /// The token the client sent.
        token: u64,
        /// The realm's wall clock.
        server_time: Time64,
        /// The realm's current authoritative tick.
        server_tick: u64,
    },
    /// The session is ending, and this is why.
    Disconnect(DisconnectReason),
}

impl<'a> ServerMessage<'a> {
    const WELCOME: u16 = 1;
    const AUTH_RESULT: u16 = 2;
    const SNAPSHOT: u16 = 3;
    const DELTA: u16 = 4;
    const WORLD_DELTA: u16 = 5;
    const EVENTS: u16 = 6;
    const CHAT_MESSAGE: u16 = 7;
    const CONSOLE_REPLY: u16 = 8;
    const PONG: u16 = 9;
    const DISCONNECT: u16 = 10;

    /// The message's kind tag.
    #[must_use]
    pub const fn kind(&self) -> u16 {
        match self {
            Self::Welcome(_) => Self::WELCOME,
            Self::AuthResult(_) => Self::AUTH_RESULT,
            Self::Snapshot(_) => Self::SNAPSHOT,
            Self::Delta(_) => Self::DELTA,
            Self::WorldDelta(_) => Self::WORLD_DELTA,
            Self::Events(_) => Self::EVENTS,
            Self::ChatMessage { .. } => Self::CHAT_MESSAGE,
            Self::ConsoleReply { .. } => Self::CONSOLE_REPLY,
            Self::Pong { .. } => Self::PONG,
            Self::Disconnect(_) => Self::DISCONNECT,
        }
    }

    /// Decode one message from a record's plaintext.
    ///
    /// Total, and bounded before anything is read in bulk: a count is checked
    /// against its cap before the run it prefixes is touched, so a hostile
    /// count buys the decoder no work.
    ///
    /// # Errors
    ///
    /// A typed [`WireError`] for an unknown kind, a count past its bound, a
    /// short or malformed field, a non-canonical encoding, or trailing bytes.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let message = match r.u16()? {
            Self::WELCOME => Self::Welcome(Welcome {
                protocol_version: r.u16()?,
                realm_seed: r.u64()?,
                parameters: RealmParameters::read(&mut r)?,
                content_digest: r.array::<32>()?,
                world_generator_digest: r.array::<32>()?,
                rules_digest: r.array::<32>()?,
            }),
            Self::AUTH_RESULT => Self::AuthResult(match r.u8()? {
                1 => AuthResult::Accepted(AccountId(r.u64()?)),
                2 => {
                    // Fixed-width whichever way it went, so a refusal is
                    // indistinguishable from an acceptance by length alone.
                    r.padding(ACCOUNT_ID_LEN)?;
                    AuthResult::Refused
                }
                _ => return Err(WireError::UnknownDiscriminant),
            }),
            Self::SNAPSHOT => {
                let tick = r.u64()?;
                let acknowledged_intent = r.u64()?;
                let count = usize::from(r.u16()?);
                if count > MAX_ENTITIES_IN_INTEREST {
                    return Err(WireError::BoundExceeded);
                }
                Self::Snapshot(Snapshot {
                    tick,
                    acknowledged_intent,
                    entities: WireSeq::read(&mut r, count)?,
                })
            }
            Self::DELTA => {
                let tick = r.u64()?;
                let acknowledged_intent = r.u64()?;
                let entered = usize::from(r.u16()?);
                let updated = usize::from(r.u16()?);
                let departed = usize::from(r.u16()?);
                // Entered and updated entities are both *in* the interest
                // set, so their sum is what the cap bounds; checking them
                // separately would admit a frame twice the size the interest
                // model can produce.
                if entered + updated > MAX_ENTITIES_IN_INTEREST
                    || departed > MAX_ENTITIES_IN_INTEREST
                {
                    return Err(WireError::BoundExceeded);
                }
                Self::Delta(Delta {
                    tick,
                    acknowledged_intent,
                    entered: WireSeq::read(&mut r, entered)?,
                    updated: WireSeq::read(&mut r, updated)?,
                    departed: WireSeq::read(&mut r, departed)?,
                })
            }
            Self::WORLD_DELTA => {
                let chunk = ChunkCoord {
                    x: r.i32()?,
                    y: r.i32()?,
                };
                let count = usize::from(r.u16()?);
                if count > MAX_WORLD_EDITS {
                    return Err(WireError::BoundExceeded);
                }
                Self::WorldDelta(WorldDelta {
                    chunk,
                    edits: WireSeq::read(&mut r, count)?,
                })
            }
            Self::EVENTS => {
                let count = usize::from(r.u16()?);
                if count > MAX_GAME_EVENTS {
                    return Err(WireError::BoundExceeded);
                }
                Self::Events(WireSeq::read(&mut r, count)?)
            }
            Self::CHAT_MESSAGE => Self::ChatMessage {
                channel: ChatChannel::from_u8(r.u8()?)?,
                sender: r.text8(1, MAX_ACCOUNT_NAME_LEN)?,
                body: r.text16(1, MAX_CHAT_BYTES, false)?,
            },
            Self::CONSOLE_REPLY => Self::ConsoleReply {
                status: ConsoleStatus::from_u8(r.u8()?)?,
                // A console reply is the one body that legitimately wraps
                // and tabulates, so newline and tab are admitted; every
                // other control byte stays refused.
                body: r.text16(0, MAX_CONSOLE_REPLY_BYTES, true)?,
            },
            Self::PONG => Self::Pong {
                token: r.u64()?,
                server_time: Time64::from_bytes(r.take(TIME64_LEN)?)
                    .map_err(|_| WireError::FieldOutOfRange)?,
                server_tick: r.u64()?,
            },
            Self::DISCONNECT => Self::Disconnect(DisconnectReason::from_u16(r.u16()?)?),
            _ => return Err(WireError::UnknownDiscriminant),
        };
        r.finish()?;
        Ok(message)
    }

    /// Encode into `out`, returning the length written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short, or a typed refusal
    /// when a field or a run is outside its bound.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.u16(self.kind())?;
        match self {
            Self::Welcome(welcome) => {
                w.u16(welcome.protocol_version)?;
                w.u64(welcome.realm_seed)?;
                welcome.parameters.write(&mut w)?;
                w.bytes(&welcome.content_digest)?;
                w.bytes(&welcome.world_generator_digest)?;
                w.bytes(&welcome.rules_digest)?;
            }
            Self::AuthResult(result) => match result {
                AuthResult::Accepted(account) => {
                    w.u8(1)?;
                    w.u64(account.0)?;
                }
                AuthResult::Refused => {
                    w.u8(2)?;
                    w.padding(ACCOUNT_ID_LEN)?;
                }
            },
            Self::Snapshot(snapshot) => {
                let count = bounded_count(snapshot.entities.len(), MAX_ENTITIES_IN_INTEREST)?;
                w.u64(snapshot.tick)?;
                w.u64(snapshot.acknowledged_intent)?;
                w.u16(count)?;
                snapshot.entities.write(&mut w)?;
            }
            Self::Delta(delta) => {
                let live = delta.entered.len() + delta.updated.len();
                if live > MAX_ENTITIES_IN_INTEREST {
                    return Err(WireError::BoundExceeded);
                }
                let entered = bounded_count(delta.entered.len(), MAX_ENTITIES_IN_INTEREST)?;
                let updated = bounded_count(delta.updated.len(), MAX_ENTITIES_IN_INTEREST)?;
                let departed = bounded_count(delta.departed.len(), MAX_ENTITIES_IN_INTEREST)?;
                w.u64(delta.tick)?;
                w.u64(delta.acknowledged_intent)?;
                w.u16(entered)?;
                w.u16(updated)?;
                w.u16(departed)?;
                delta.entered.write(&mut w)?;
                delta.updated.write(&mut w)?;
                delta.departed.write(&mut w)?;
            }
            Self::WorldDelta(world) => {
                let count = bounded_count(world.edits.len(), MAX_WORLD_EDITS)?;
                w.i32(world.chunk.x)?;
                w.i32(world.chunk.y)?;
                w.u16(count)?;
                world.edits.write(&mut w)?;
            }
            Self::Events(events) => {
                let count = bounded_count(events.len(), MAX_GAME_EVENTS)?;
                w.u16(count)?;
                events.write(&mut w)?;
            }
            Self::ChatMessage {
                channel,
                sender,
                body,
            } => {
                w.u8(channel.as_u8())?;
                w.text8(sender, 1, MAX_ACCOUNT_NAME_LEN)?;
                w.text16(body, 1, MAX_CHAT_BYTES, false)?;
            }
            Self::ConsoleReply { status, body } => {
                w.u8(status.as_u8())?;
                w.text16(body, 0, MAX_CONSOLE_REPLY_BYTES, true)?;
            }
            Self::Pong {
                token,
                server_time,
                server_tick,
            } => {
                w.u64(*token)?;
                w.bytes(&server_time.to_le_bytes())?;
                w.u64(*server_tick)?;
            }
            Self::Disconnect(reason) => w.u16(reason.as_u16())?,
        }
        debug_assert!(w.written() >= MESSAGE_HEADER_LEN);
        Ok(w.written())
    }
}

/// Narrow a run's length to its wire prefix, refusing one past its bound.
fn bounded_count(len: usize, max: usize) -> Result<u16, WireError> {
    if len > max {
        return Err(WireError::BoundExceeded);
    }
    u16::try_from(len).map_err(|_| WireError::BoundExceeded)
}

#[cfg(test)]
mod tests;
