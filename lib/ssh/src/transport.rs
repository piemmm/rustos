//! The transport layer's state machine (RFC 4253 §§4.2, 7, 11), with strict
//! key exchange.
//!
//! A [`Transport`] owns one connection's byte streams and packet framing and
//! decides what may be sent and received in each phase. It knows *that* a key
//! exchange is under way and what that forbids, never *how* one is carried out:
//! the layer above builds `SSH_MSG_KEXINIT`, runs the method, derives the keys,
//! and hands them in with [`Transport::install_keys`].
//!
//! # Strict key exchange
//!
//! OpenSSH's `kex-strict-*-v00@openssh.com` closes CVE-2023-48795, in which an
//! attacker inserts a message during the unencrypted exchange and deletes one
//! after it, leaving both sequence numbers in step while silently removing the
//! first encrypted message. Once the layer above settles strictness with
//! [`Transport::set_strict`]:
//!
//! * the peer's `SSH_MSG_KEXINIT` must have been the very first packet it sent;
//! * until the peer's first `SSH_MSG_NEWKEYS`, anything but the exchange's own
//!   messages — `SSH_MSG_IGNORE` and `SSH_MSG_DEBUG` included — ends the
//!   connection; and
//! * each direction's sequence number restarts at zero after every
//!   `SSH_MSG_NEWKEYS`, so a deleted packet can never be papered over.
//!
//! The packet after the peer's first `SSH_MSG_KEXINIT` is not even opened
//! until strictness is settled, so no message can slip past the rule while it
//! is undecided.
//!
//! # No clock, no randomness
//!
//! Encrypted packets carry random padding, which comes from a reserve the host
//! fills from the kernel CSPRNG ([`Transport::supply_padding`]) when
//! [`Transport::padding_wanted`] asks. A message that finds the reserve short
//! waits and goes out when padding arrives — a slow top-up delays a packet,
//! never ends a connection. Ordinary traffic may not touch a floor kept back
//! for the key exchange's own messages, which queue apart and go first, so an
//! exchange never stalls behind the traffic it is rekeying. Rekeying by
//! elapsed time is the host's timer calling
//! [`Transport::rekey_interval_elapsed`]; rekeying by volume the transport
//! counts itself.

use core::ops::Range;

use tairix_collections::{ByteQueue, QueueError};
use tairix_inline::RingBuf;

use crate::algorithm::Keys;
use crate::ident::{self, Ident, IdentError, Line, MAX_BANNER_LINES};
use crate::msg::{self, Block, DisconnectReason};
use crate::packet::{Framing, Opened, Opener, PacketError, Sealer, MAX_FRAMED_LEN, MAX_PADDING};
use crate::wire::Reader;
use crate::Role;

/// Bytes of random padding the transport holds at most.
pub const PADDING_RESERVE: usize = 2048;

/// Below this many reserve bytes [`Transport::padding_wanted`] asks for more:
/// enough headroom that the reply arrives before the reserve runs dry.
const PADDING_LOW_WATER: usize = PADDING_RESERVE / 2;

/// Reserve bytes ordinary messages may not use, kept for a key exchange's own
/// packets and a final `SSH_MSG_DISCONNECT`.
const PADDING_FLOOR: usize = 8 * MAX_PADDING;

/// The most bytes sealed but not yet taken, plus payloads held, may come to
/// before [`Transport::send`] refuses more. A containment bound, not flow
/// control: the channel windows above keep a working connection far below it,
/// and it always admits one packet of the largest size into an empty backlog.
pub const MAX_BACKLOG: usize = 2 * MAX_FRAMED_LEN;

/// The longest description this engine's own `SSH_MSG_DISCONNECT` carries.
const MAX_DISCONNECT_TEXT: usize = 128;

/// Room beyond [`MAX_BACKLOG`] the outbound queue keeps for a final
/// `SSH_MSG_DISCONNECT`, which is never refused for backlog.
const DISCONNECT_ROOM: usize = 256;

/// Bytes of a held record's length prefix.
const HELD_HEADER: usize = 4;

/// Why a connection ended at this end.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TransportError {
    /// The peer's identification was malformed, over-long, or of a version
    /// this engine does not speak.
    Ident(IdentError),
    /// A packet failed its framing, integrity, or padding checks.
    Packet(PacketError),
    /// The peer broke strict key exchange: a packet ahead of its first
    /// `SSH_MSG_KEXINIT`, or any message outside the exchange before its first
    /// `SSH_MSG_NEWKEYS`.
    StrictKex,
    /// The peer sent a message its phase does not allow; the number is its
    /// message number.
    Unexpected(u8),
    /// A transport message's body did not parse; the number is its message
    /// number.
    Malformed(u8),
    /// The layer above drove the key exchange out of order.
    OutOfPhase,
    /// The allocator refused buffer space.
    OutOfMemory,
    /// The connection is already closed.
    Closed,
}

/// Why a message was not accepted for sending.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SendError {
    /// The connection is closed.
    Closed,
    /// The backlog is at [`MAX_BACKLOG`]; take output and try again.
    Backlogged,
    /// The payload does not fit one packet.
    TooLarge,
    /// The message may not be sent now: a first message other than
    /// `SSH_MSG_KEXINIT`, a second `SSH_MSG_KEXINIT` in one exchange, a
    /// method message outside one, `SSH_MSG_NEWKEYS` without keys installed,
    /// or the unassigned number zero.
    OutOfPhase,
    /// The message is one the transport sends itself:
    /// `SSH_MSG_DISCONNECT` goes through [`Transport::disconnect`] and
    /// `SSH_MSG_NEWKEYS` through [`Transport::send_newkeys`].
    Reserved,
    /// The allocator refused buffer space. Nothing was queued.
    OutOfMemory,
    /// Sealing failed and the connection was closed.
    Failed(TransportError),
}

/// What the peer sent, as [`Transport::next_received`] hands it up.
///
/// Every byte string here is the peer's and untrusted; text is sanitised
/// before anything displays it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Received<'a> {
    /// A line a server sent ahead of its identification (a client only).
    Banner(&'a [u8]),
    /// A message for the layer above, `SSH_MSG_KEXINIT` included.
    Message {
        /// Its sequence number, for an `SSH_MSG_UNIMPLEMENTED` reply.
        sequence: u32,
        /// The payload, message number first.
        payload: &'a [u8],
    },
    /// The peer's `SSH_MSG_NEWKEYS`: it now sends under the keys installed
    /// for it.
    NewKeys,
    /// `SSH_MSG_DEBUG`.
    Debug {
        /// Whether the peer asks for the message to be shown.
        always_display: bool,
        /// The message.
        message: &'a [u8],
        /// Its language tag.
        language: &'a [u8],
    },
    /// `SSH_MSG_UNIMPLEMENTED`: the peer did not understand one of ours.
    Unimplemented {
        /// The sequence number of the message it did not understand.
        sequence: u32,
    },
    /// `SSH_MSG_DISCONNECT`. The transport is now closed.
    Disconnected {
        /// The peer's reason code.
        reason: DisconnectReason,
        /// Its description.
        description: &'a [u8],
        /// The description's language tag.
        language: &'a [u8],
    },
}

/// A received message, with its text located by range so the transport can
/// update its state before lending the bytes out.
enum Parsed {
    Message {
        sequence: u32,
    },
    NewKeys,
    Debug {
        always_display: bool,
        message: Range<usize>,
        language: Range<usize>,
    },
    Unimplemented {
        sequence: u32,
    },
    Disconnected {
        reason: DisconnectReason,
        description: Range<usize>,
        language: Range<usize>,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Strictness {
    Undecided,
    Strict,
    Lax,
}

/// Where this end stands in its key exchanges.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Ours {
    /// Nothing sent: the first message must be `SSH_MSG_KEXINIT`.
    Fresh,
    /// Between our `SSH_MSG_KEXINIT` and our `SSH_MSG_NEWKEYS`.
    Exchanging,
    /// Our `SSH_MSG_NEWKEYS` is sent as far as the layer above is concerned
    /// but waits for padding; the keys switch when it is sealed.
    Finishing,
    /// No exchange of ours under way.
    Idle,
}

/// Where the peer stands in its key exchanges.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Theirs {
    /// Its first `SSH_MSG_KEXINIT` has not arrived.
    AwaitingFirst,
    /// Between its first `SSH_MSG_KEXINIT`, which arrived under `kexinit`,
    /// and its first `SSH_MSG_NEWKEYS`.
    FirstExchange { kexinit: u32 },
    /// No exchange of its under way.
    Idle,
    /// Between a later `SSH_MSG_KEXINIT` and its `SSH_MSG_NEWKEYS`.
    Rekeying,
}

impl Theirs {
    /// Before the peer's first `SSH_MSG_NEWKEYS`.
    const fn initial(self) -> bool {
        matches!(self, Self::AwaitingFirst | Self::FirstExchange { .. })
    }

    /// Between one of the peer's `SSH_MSG_KEXINIT` and its `SSH_MSG_NEWKEYS`.
    const fn exchanging(self) -> bool {
        matches!(self, Self::FirstExchange { .. } | Self::Rekeying)
    }
}

/// Both sides' key-exchange state, and the keys waiting for each
/// `SSH_MSG_NEWKEYS`.
struct Kex {
    ours: Ours,
    theirs: Theirs,
    strict: Strictness,
    pending_out: Option<Keys>,
    pending_in: Option<Keys>,
}

impl Kex {
    /// The peer's first `SSH_MSG_KEXINIT` has arrived and strictness is not
    /// yet settled: nothing more may be opened until it is.
    const fn awaiting_strictness(&self) -> bool {
        matches!(self.theirs, Theirs::FirstExchange { .. })
            && matches!(self.strict, Strictness::Undecided)
    }
}

/// One connection's transport layer.
pub struct Transport {
    role: Role,
    ours: Ident,
    peer: Option<Ident>,
    banner_lines: usize,
    /// How far the peer's current line has been examined.
    line_scanned: usize,
    inbound: ByteQueue,
    /// Bytes of the item [`Self::next_received`] last lent out, dropped on
    /// the next call.
    release: usize,
    opener: Opener,
    outbound: ByteQueue,
    sealer: Sealer,
    /// Ordinary payloads waiting on the key-exchange gate or on padding, each
    /// behind a four-byte length, in order.
    held: ByteQueue,
    /// The key exchange's own payloads waiting on padding, which go before
    /// anything in [`Self::held`].
    held_exchange: ByteQueue,
    reserve: RingBuf<u8, PADDING_RESERVE>,
    kex: Kex,
    rekey_limit: Option<u64>,
    rekey_asked: bool,
    closed: bool,
}

/// Whether the transport seals `msg` ahead of anything held, drawing on the
/// padding floor: the key exchange's own messages.
const fn is_priority(msg: u8) -> bool {
    msg == msg::KEXINIT || matches!(Block::of(msg), Block::KeyExchangeMethod)
}

/// What the peer may send before its first `SSH_MSG_NEWKEYS`: the generic
/// transport messages the exchange tolerates and the exchange itself.
const fn permitted_before_keys(msg: u8) -> bool {
    matches!(
        msg,
        msg::DISCONNECT
            | msg::IGNORE
            | msg::UNIMPLEMENTED
            | msg::DEBUG
            | msg::KEXINIT
            | msg::NEWKEYS
    ) || matches!(Block::of(msg), Block::KeyExchangeMethod)
}

/// The largest prefix of `text` of at most `max` bytes that ends on a
/// character boundary.
fn clip(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Read a `string`, reporting where its bytes sit in `whole`.
fn string_range(reader: &mut Reader<'_>, whole: usize) -> Option<Range<usize>> {
    let start = whole - reader.remaining() + 4;
    let text = reader.string().ok()?;
    Some(start..start + text.len())
}

/// Classify a received payload and locate its fields, or `None` when a
/// transport message's body does not parse.
fn parse(sequence: u32, payload: &[u8]) -> Option<Parsed> {
    let whole = payload.len();
    let mut reader = Reader::new(payload);
    let message = reader.byte().ok()?;
    let parsed = match message {
        msg::DISCONNECT => {
            let reason = DisconnectReason::from_code(reader.uint32().ok()?);
            let description = string_range(&mut reader, whole)?;
            let language = string_range(&mut reader, whole)?;
            Parsed::Disconnected {
                reason,
                description,
                language,
            }
        }
        msg::UNIMPLEMENTED => Parsed::Unimplemented {
            sequence: reader.uint32().ok()?,
        },
        msg::DEBUG => {
            let always_display = reader.boolean().ok()?;
            let message = string_range(&mut reader, whole)?;
            let language = string_range(&mut reader, whole)?;
            Parsed::Debug {
                always_display,
                message,
                language,
            }
        }
        msg::NEWKEYS => Parsed::NewKeys,
        _ => return Some(Parsed::Message { sequence }),
    };
    reader.finish().ok()?;
    Some(parsed)
}

/// What a fatal error tells the peer, if the peer can still be told.
const fn farewell(err: TransportError) -> Option<(DisconnectReason, &'static str)> {
    match err {
        TransportError::Packet(PacketError::Integrity) => {
            Some((DisconnectReason::MAC_ERROR, "corrupted MAC on input"))
        }
        TransportError::Packet(_) => Some((DisconnectReason::PROTOCOL_ERROR, "packet corrupt")),
        TransportError::StrictKex => Some((
            DisconnectReason::PROTOCOL_ERROR,
            "strict key exchange violation",
        )),
        TransportError::Unexpected(_) => {
            Some((DisconnectReason::PROTOCOL_ERROR, "unexpected message"))
        }
        TransportError::Malformed(_) => {
            Some((DisconnectReason::PROTOCOL_ERROR, "malformed message"))
        }
        TransportError::OutOfPhase => Some((
            DisconnectReason::KEY_EXCHANGE_FAILED,
            "key exchange out of sequence",
        )),
        TransportError::OutOfMemory => Some((DisconnectReason::BY_APPLICATION, "out of memory")),
        // A peer that has not identified itself is not speaking SSH.
        TransportError::Ident(_) | TransportError::Closed => None,
    }
}

impl Transport {
    /// A transport for the `role` end of a connection, identifying itself as
    /// `ours`. The identification line is queued for sending at once, as RFC
    /// 4253 §4.2 has both sides do.
    ///
    /// # Errors
    ///
    /// [`TransportError::OutOfMemory`] when the line cannot be queued.
    pub fn new(role: Role, ours: Ident) -> Result<Self, TransportError> {
        let mut outbound = ByteQueue::new(MAX_BACKLOG + DISCONNECT_ROOM);
        let line = ours.as_bytes();
        let slot = outbound
            .append_slot(line.len() + 2)
            .map_err(|_| TransportError::OutOfMemory)?;
        slot[..line.len()].copy_from_slice(line);
        slot[line.len()..].copy_from_slice(b"\r\n");
        Ok(Self {
            role,
            ours,
            peer: None,
            banner_lines: 0,
            line_scanned: 0,
            inbound: ByteQueue::new(MAX_FRAMED_LEN),
            release: 0,
            opener: Opener::new(),
            outbound,
            sealer: Sealer::new(),
            held: ByteQueue::new(MAX_BACKLOG),
            held_exchange: ByteQueue::new(MAX_BACKLOG),
            reserve: RingBuf::new(),
            kex: Kex {
                ours: Ours::Fresh,
                theirs: Theirs::AwaitingFirst,
                strict: Strictness::Undecided,
                pending_out: None,
                pending_in: None,
            },
            rekey_limit: None,
            rekey_asked: false,
            closed: false,
        })
    }

    /// Which end of the connection this is.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// This end's identification — `V_C` or `V_S` of the exchange hash.
    #[must_use]
    pub const fn our_ident(&self) -> &Ident {
        &self.ours
    }

    /// The peer's identification, once it has arrived.
    #[must_use]
    pub const fn peer_ident(&self) -> Option<&Ident> {
        self.peer.as_ref()
    }

    /// Whether the connection has ended. What [`Self::pending_output`] still
    /// holds — a final `SSH_MSG_DISCONNECT` — is still to be sent.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Accept bytes received from the peer, returning how many were taken.
    /// A short count is back-pressure: take what [`Self::next_received`] yields, then
    /// offer the rest. However large a packet the peer declares, its bytes are
    /// always taken while it is the one at the head.
    ///
    /// # Errors
    ///
    /// [`TransportError::Closed`], or [`TransportError::OutOfMemory`], which
    /// closes the connection.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<usize, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        self.release_lent();
        match self.inbound.push_slice(bytes) {
            Ok(taken) => Ok(taken),
            Err(_) => Err(self.fail(TransportError::OutOfMemory)),
        }
    }

    /// The next thing the peer sent, or `None` until more bytes arrive.
    ///
    /// What is returned borrows the transport's buffer and is released by the
    /// next call. `SSH_MSG_IGNORE` never surfaces.
    ///
    /// # Errors
    ///
    /// Any [`TransportError`]. Each one closes the connection, queueing an
    /// `SSH_MSG_DISCONNECT` that says why where the peer can still be told.
    pub fn next_received(&mut self) -> Result<Option<Received<'_>>, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        self.release_lent();
        if self.peer.is_none() {
            let peer_role = match self.role {
                Role::Client => Role::Server,
                Role::Server => Role::Client,
            };
            match ident::next_line(self.inbound.pending(), peer_role, self.line_scanned) {
                Ok(Line::Incomplete { scanned }) => {
                    self.line_scanned = scanned;
                    return Ok(None);
                }
                Ok(Line::Banner { text, consumed }) => {
                    let text = text.len();
                    self.line_scanned = 0;
                    self.banner_lines += 1;
                    if self.banner_lines > MAX_BANNER_LINES {
                        return Err(self.fail(TransportError::Ident(IdentError::TooManyLines)));
                    }
                    self.release = consumed;
                    return Ok(Some(Received::Banner(&self.inbound.pending()[..text])));
                }
                Ok(Line::Ident { text, consumed }) => match Ident::parse(text) {
                    Ok(ident) => {
                        self.peer = Some(ident);
                        self.inbound.consume(consumed);
                    }
                    Err(err) => return Err(self.fail(TransportError::Ident(err))),
                },
                Err(err) => return Err(self.fail(TransportError::Ident(err))),
            }
        }
        loop {
            if self.kex.awaiting_strictness() {
                return Err(self.fail(TransportError::OutOfPhase));
            }
            let (framed, payload, sequence) = match self.opener.open(self.inbound.pending_mut()) {
                Ok(Opened::Need(_)) => return Ok(None),
                Ok(Opened::Packet {
                    framed,
                    payload,
                    sequence,
                }) => (framed, payload, sequence),
                Err(err) => return Err(self.fail(TransportError::Packet(err))),
            };
            let message = self.inbound.pending()[payload.start];
            if let Err(err) = self.admit(message) {
                return Err(self.fail(err));
            }
            // Ignored by definition, whatever its body holds.
            if message == msg::IGNORE {
                self.inbound.consume(framed);
                continue;
            }
            let Some(parsed) = parse(sequence, &self.inbound.pending()[payload.clone()]) else {
                return Err(self.fail(TransportError::Malformed(message)));
            };
            match parsed {
                Parsed::NewKeys => self.peer_newkeys(),
                Parsed::Disconnected { .. } => self.close(),
                Parsed::Message { .. } if message == msg::KEXINIT => {
                    self.kex.theirs = match self.kex.theirs {
                        Theirs::AwaitingFirst => Theirs::FirstExchange { kexinit: sequence },
                        _ => Theirs::Rekeying,
                    };
                }
                Parsed::Message { .. } | Parsed::Debug { .. } | Parsed::Unimplemented { .. } => {}
            }
            self.release = framed;
            return Ok(Some(self.lend(parsed, payload)));
        }
    }

    /// Whether the phase allows the peer to have sent `message`.
    fn admit(&self, message: u8) -> Result<(), TransportError> {
        let kex = &self.kex;
        if kex.theirs.initial() && kex.strict == Strictness::Strict {
            let exchange = message == msg::DISCONNECT
                || message == msg::NEWKEYS
                || matches!(Block::of(message), Block::KeyExchangeMethod);
            if !exchange {
                return Err(TransportError::StrictKex);
            }
        }
        let exchanging = kex.theirs.exchanging();
        let allowed = match message {
            msg::KEXINIT => !exchanging,
            msg::NEWKEYS => exchanging && kex.pending_in.is_some(),
            _ if matches!(Block::of(message), Block::KeyExchangeMethod) => exchanging,
            _ if kex.theirs.initial() => permitted_before_keys(message),
            _ if exchanging => msg::permitted_during_kex(message),
            _ => true,
        };
        if allowed {
            Ok(())
        } else {
            Err(TransportError::Unexpected(message))
        }
    }

    /// Switch the receiving direction to the keys installed for it.
    fn peer_newkeys(&mut self) {
        if let Some(keys) = self.kex.pending_in.take() {
            let reset = self.kex.strict == Strictness::Strict;
            self.opener.rekey(&keys, reset, self.rekey_limit);
        }
        self.kex.theirs = Theirs::Idle;
    }

    /// Lend out what [`Self::next_received`] parsed, from the bytes still
    /// buffered.
    fn lend(&self, parsed: Parsed, payload: Range<usize>) -> Received<'_> {
        let bytes = &self.inbound.pending()[payload];
        match parsed {
            Parsed::Message { sequence } => Received::Message {
                sequence,
                payload: bytes,
            },
            Parsed::NewKeys => Received::NewKeys,
            Parsed::Debug {
                always_display,
                message,
                language,
            } => Received::Debug {
                always_display,
                message: &bytes[message],
                language: &bytes[language],
            },
            Parsed::Unimplemented { sequence } => Received::Unimplemented { sequence },
            Parsed::Disconnected {
                reason,
                description,
                language,
            } => Received::Disconnected {
                reason,
                description: &bytes[description],
                language: &bytes[language],
            },
        }
    }

    fn release_lent(&mut self) {
        if self.release > 0 {
            self.inbound.consume(self.release);
            self.release = 0;
        }
    }

    /// Queue a message: number `msg`, then the concatenation of `body`.
    ///
    /// It is sealed at once when it can be. Otherwise it waits, in order: an
    /// ordinary message while the key-exchange gate or the padding reserve
    /// keeps it back, and the key exchange's own messages — which never wait
    /// behind ordinary ones — only for padding.
    ///
    /// # Errors
    ///
    /// [`SendError`]; only [`SendError::Failed`] closes the connection.
    pub fn send(&mut self, msg: u8, body: &[&[u8]]) -> Result<(), SendError> {
        if self.closed {
            return Err(SendError::Closed);
        }
        if msg == msg::DISCONNECT || msg == msg::NEWKEYS {
            return Err(SendError::Reserved);
        }
        let ours = self.kex.ours;
        let out_of_phase = msg == 0
            || (ours == Ours::Fresh && msg != msg::KEXINIT)
            || (msg == msg::KEXINIT && !matches!(ours, Ours::Fresh | Ours::Idle))
            || (matches!(Block::of(msg), Block::KeyExchangeMethod) && ours != Ours::Exchanging);
        if out_of_phase {
            return Err(SendError::OutOfPhase);
        }
        let len = payload_len(body).ok_or(SendError::TooLarge)?;
        let framing = self.sealer.framing(len).map_err(|_| SendError::TooLarge)?;
        if is_priority(msg) {
            if !self.held_exchange.is_empty() || self.seal(msg, body, &framing, 0)? == Placed::Short
            {
                self.hold(true, msg, body, len)?;
            }
            if msg == msg::KEXINIT {
                self.kex.ours = Ours::Exchanging;
                self.rekey_asked = false;
            }
        } else if self.must_hold(msg)
            || self.seal(msg, body, &framing, PADDING_FLOOR)? == Placed::Short
        {
            self.hold(false, msg, body, len)?;
        }
        Ok(())
    }

    /// Whether an ordinary message must wait behind something already held
    /// or behind the key-exchange gate.
    fn must_hold(&self, msg: u8) -> bool {
        !self.held.is_empty()
            || !self.held_exchange.is_empty()
            || (self.kex.ours == Ours::Exchanging && !msg::permitted_during_kex(msg))
    }

    /// Bytes sealed but not taken, plus payloads held.
    #[must_use]
    pub const fn outbound_backlog(&self) -> usize {
        self.outbound.len() + self.held.len() + self.held_exchange.len()
    }

    /// Whether messages are being held back. The layer above stops producing
    /// bulk data while they are.
    #[must_use]
    pub const fn is_holding(&self) -> bool {
        !self.held.is_empty() || !self.held_exchange.is_empty()
    }

    /// Queue a payload to be sealed later: in the exchange's own queue when
    /// `exchange`, otherwise the ordinary one.
    fn hold(
        &mut self,
        exchange: bool,
        msg: u8,
        body: &[&[u8]],
        len: usize,
    ) -> Result<(), SendError> {
        if self.outbound_backlog() + HELD_HEADER + len > MAX_BACKLOG {
            return Err(SendError::Backlogged);
        }
        let declared = u32::try_from(len).map_err(|_| SendError::TooLarge)?;
        let queue = if exchange {
            &mut self.held_exchange
        } else {
            &mut self.held
        };
        let slot = match queue.append_slot(HELD_HEADER + len) {
            Ok(slot) => slot,
            Err(QueueError::Full) => return Err(SendError::Backlogged),
            Err(QueueError::Alloc(_)) => return Err(SendError::OutOfMemory),
        };
        slot[..HELD_HEADER].copy_from_slice(&declared.to_be_bytes());
        gather(&mut slot[HELD_HEADER..], msg, body);
        Ok(())
    }

    /// Seal one message now if the reserve covers its padding with `floor`
    /// bytes to spare, keeping the backlog within [`MAX_BACKLOG`].
    fn seal(
        &mut self,
        msg: u8,
        body: &[&[u8]],
        framing: &Framing,
        floor: usize,
    ) -> Result<Placed, SendError> {
        if self.outbound_backlog() + framing.total > MAX_BACKLOG {
            return Err(SendError::Backlogged);
        }
        let sealed = seal_into(
            &mut self.outbound,
            &mut self.sealer,
            &mut self.reserve,
            framing,
            floor,
            |payload| gather(payload, msg, body),
        );
        match sealed {
            Ok(()) => Ok(Placed::Sealed),
            Err(Sealing::Short) => Ok(Placed::Short),
            Err(Sealing::Refused(err)) => Err(err),
            Err(Sealing::Fatal(err)) => Err(SendError::Failed(self.fail(err))),
        }
    }

    /// Seal what the held queues can release now: the exchange's own
    /// messages first, then ordinary ones, each queue in order, stopping at a
    /// message the gate still keeps back, a reserve too short to pad the next
    /// one, or an outbound queue with no room for it.
    fn flush_held(&mut self) -> Result<(), TransportError> {
        while let Some((msg, payload)) = held_front(self.held_exchange.pending()) {
            let len = payload.len();
            let framing = self.sealer.framing(len).map_err(TransportError::Packet)?;
            let sealed = seal_into(
                &mut self.outbound,
                &mut self.sealer,
                &mut self.reserve,
                &framing,
                0,
                |slot| slot.copy_from_slice(payload),
            );
            match sealed {
                Ok(()) => {
                    self.held_exchange.consume(HELD_HEADER + len);
                    if msg == msg::NEWKEYS {
                        self.switch_outbound();
                    }
                }
                Err(Sealing::Short | Sealing::Refused(_)) => return Ok(()),
                Err(Sealing::Fatal(err)) => return Err(err),
            }
        }
        while let Some((msg, payload)) = held_front(self.held.pending()) {
            if self.kex.ours == Ours::Exchanging && !msg::permitted_during_kex(msg) {
                break;
            }
            let len = payload.len();
            let framing = self.sealer.framing(len).map_err(TransportError::Packet)?;
            // The record moves from one queue to the other, so only the
            // outbound queue's room can refuse it; a refusal waits for output
            // to be taken.
            let sealed = seal_into(
                &mut self.outbound,
                &mut self.sealer,
                &mut self.reserve,
                &framing,
                PADDING_FLOOR,
                |slot| slot.copy_from_slice(payload),
            );
            match sealed {
                Ok(()) => self.held.consume(HELD_HEADER + len),
                Err(Sealing::Short | Sealing::Refused(_)) => break,
                Err(Sealing::Fatal(err)) => return Err(err),
            }
        }
        Ok(())
    }

    /// Send `SSH_MSG_NEWKEYS` and switch the sending direction to the keys
    /// installed for it — at once, or when padding lets it be sealed —
    /// releasing whatever the exchange held back.
    ///
    /// # Errors
    ///
    /// [`SendError::OutOfPhase`] outside an exchange, before
    /// [`Self::install_keys`], or a second time; [`SendError::Failed`] when
    /// sealing failed.
    pub fn send_newkeys(&mut self) -> Result<(), SendError> {
        if self.closed {
            return Err(SendError::Closed);
        }
        if self.kex.ours != Ours::Exchanging || self.kex.pending_out.is_none() {
            return Err(SendError::OutOfPhase);
        }
        let framing = self.sealer.framing(1).map_err(|_| SendError::TooLarge)?;
        if !self.held_exchange.is_empty()
            || self.seal(msg::NEWKEYS, &[], &framing, 0)? == Placed::Short
        {
            self.hold(true, msg::NEWKEYS, &[], 1)?;
            self.kex.ours = Ours::Finishing;
            return Ok(());
        }
        self.switch_outbound();
        match self.flush_held() {
            Ok(()) => Ok(()),
            Err(err) => Err(SendError::Failed(self.fail(err))),
        }
    }

    /// Our `SSH_MSG_NEWKEYS` is sealed: every later packet goes under the
    /// keys installed for it.
    fn switch_outbound(&mut self) {
        if let Some(keys) = self.kex.pending_out.take() {
            let reset = self.kex.strict == Strictness::Strict;
            self.sealer.rekey(&keys, reset, self.rekey_limit);
        }
        self.kex.ours = Ours::Idle;
    }

    /// Sealed bytes waiting to be sent, oldest first.
    #[must_use]
    pub fn pending_output(&self) -> &[u8] {
        self.outbound.pending()
    }

    /// Record that the first `n` bytes of [`Self::pending_output`] were sent.
    /// Room freed lets held messages out.
    ///
    /// # Errors
    ///
    /// A sealing failure while releasing held messages, which closes the
    /// connection.
    pub fn consume_output(&mut self, n: usize) -> Result<(), TransportError> {
        self.outbound.consume(n);
        if self.closed || !self.is_holding() {
            return Ok(());
        }
        self.flush_held().map_err(|err| self.fail(err))
    }

    /// Random padding bytes the transport would like, or zero while its
    /// reserve is above the low-water mark. The host answers from the kernel
    /// CSPRNG with [`Self::supply_padding`]; the figure stays non-zero, rather
    /// than being reported once, until it is answered.
    #[must_use]
    pub fn padding_wanted(&self) -> usize {
        if self.closed || self.reserve.len() >= PADDING_LOW_WATER {
            0
        } else {
            PADDING_RESERVE - self.reserve.len()
        }
    }

    /// Add random bytes to the padding reserve, returning how many it took,
    /// and let out whatever was held for want of them.
    ///
    /// # Errors
    ///
    /// [`TransportError::Closed`], or a sealing failure while releasing held
    /// messages, which closes the connection.
    pub fn supply_padding(&mut self, bytes: &[u8]) -> Result<usize, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let taken = self.reserve.push_slice(bytes);
        match self.flush_held() {
            Ok(()) => Ok(taken),
            Err(err) => Err(self.fail(err)),
        }
    }

    /// Settle strict key exchange, once, straight after the peer's first
    /// `SSH_MSG_KEXINIT`: `strict` when both sides advertised it.
    ///
    /// # Errors
    ///
    /// [`TransportError::StrictKex`] when strict and the peer's
    /// `SSH_MSG_KEXINIT` was not the first packet it sent;
    /// [`TransportError::OutOfPhase`] at any other time. Both close the
    /// connection.
    pub fn set_strict(&mut self, strict: bool) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let Theirs::FirstExchange { kexinit } = self.kex.theirs else {
            return Err(self.fail(TransportError::OutOfPhase));
        };
        if self.kex.strict != Strictness::Undecided {
            return Err(self.fail(TransportError::OutOfPhase));
        }
        if !strict {
            self.kex.strict = Strictness::Lax;
            return Ok(());
        }
        if kexinit != 0 {
            return Err(self.fail(TransportError::StrictKex));
        }
        self.kex.strict = Strictness::Strict;
        Ok(())
    }

    /// Hand in the keys an exchange derived: `outbound` takes effect when
    /// this end sends `SSH_MSG_NEWKEYS`, `inbound` when the peer's arrives.
    ///
    /// # Errors
    ///
    /// [`TransportError::OutOfPhase`] unless both sides' `SSH_MSG_KEXINIT`
    /// have been exchanged, neither `SSH_MSG_NEWKEYS` has, no keys are
    /// already waiting, and — in the first exchange — strictness is settled.
    /// It closes the connection.
    pub fn install_keys(&mut self, outbound: Keys, inbound: Keys) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let kex = &self.kex;
        if kex.ours != Ours::Exchanging
            || !kex.theirs.exchanging()
            || kex.pending_out.is_some()
            || kex.pending_in.is_some()
            || kex.strict == Strictness::Undecided
        {
            return Err(self.fail(TransportError::OutOfPhase));
        }
        self.kex.pending_out = Some(outbound);
        self.kex.pending_in = Some(inbound);
        Ok(())
    }

    /// Whether a key exchange is under way in either direction.
    #[must_use]
    pub const fn kex_in_progress(&self) -> bool {
        matches!(self.kex.ours, Ours::Exchanging | Ours::Finishing) || self.kex.theirs.exchanging()
    }

    /// Whether this end should start a key exchange: either direction's keys
    /// have carried what RFC 4344 allows, or the host's rekey interval
    /// elapsed. Cleared by sending `SSH_MSG_KEXINIT`.
    #[must_use]
    pub fn rekey_due(&self) -> bool {
        !self.closed
            && self.kex.ours == Ours::Idle
            && self.sealer.is_keyed()
            && (self.rekey_asked || self.sealer.rekey_due() || self.opener.rekey_due())
    }

    /// The host's rekey interval elapsed.
    pub fn rekey_interval_elapsed(&mut self) {
        if self.sealer.is_keyed() && self.kex.ours == Ours::Idle {
            self.rekey_asked = true;
        }
    }

    /// Bound the bytes each direction's keys may carry below what their
    /// cipher allows (`RekeyLimit`). Applies to keys installed from now on.
    pub fn set_rekey_limit(&mut self, bytes: u64) {
        self.rekey_limit = Some(bytes);
    }

    /// Close the connection, telling the peer `reason` and `description`
    /// (clipped to a short line) where padding allows.
    pub fn disconnect(&mut self, reason: DisconnectReason, description: &str) {
        if self.closed {
            return;
        }
        self.farewell(reason, description);
        self.close();
    }

    /// Queue a final `SSH_MSG_DISCONNECT`, best effort: a connection that
    /// cannot seal one closes without it.
    fn farewell(&mut self, reason: DisconnectReason, description: &str) {
        let text = clip(description, MAX_DISCONNECT_TEXT).as_bytes();
        let Ok(text_len) = u32::try_from(text.len()) else {
            return;
        };
        let code = reason.code().to_be_bytes();
        let declared = text_len.to_be_bytes();
        let language = 0u32.to_be_bytes();
        let body: [&[u8]; 4] = [&code, &declared, text, &language];
        let Some(framing) = payload_len(&body).and_then(|len| self.sealer.framing(len).ok()) else {
            return;
        };
        // Neither the backlog nor the floor holds the farewell back, and a
        // failure here — padding short included — has nothing left to report
        // to.
        let _ = seal_into(
            &mut self.outbound,
            &mut self.sealer,
            &mut self.reserve,
            &framing,
            0,
            |payload| gather(payload, msg::DISCONNECT, &body),
        );
    }

    /// End the connection after a fatal `err`, telling the peer why where it
    /// can be told, and hand `err` back.
    fn fail(&mut self, err: TransportError) -> TransportError {
        if !self.closed {
            if let Some((reason, text)) = farewell(err) {
                self.farewell(reason, text);
            }
            self.close();
        }
        err
    }

    fn close(&mut self) {
        self.closed = true;
        self.held.clear();
        self.held_exchange.clear();
        self.kex.pending_out = None;
        self.kex.pending_in = None;
    }
}

/// How a seal failed: short of padding, refused with nothing changed, or
/// fatally.
enum Sealing {
    Short,
    Refused(SendError),
    Fatal(TransportError),
}

/// What became of a message offered for sealing now.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Placed {
    Sealed,
    /// The reserve could not pad it; it is to wait.
    Short,
}

/// Bytes of the payload `body` makes behind a message number, or `None` past
/// the address space.
fn payload_len(body: &[&[u8]]) -> Option<usize> {
    body.iter()
        .try_fold(1usize, |len, part| len.checked_add(part.len()))
}

/// Write message number `msg` and then `body` into `payload`, which is
/// exactly [`payload_len`] bytes long.
fn gather(payload: &mut [u8], msg: u8, body: &[&[u8]]) {
    payload[0] = msg;
    let mut at = 1;
    for part in body {
        payload[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
}

/// The held record at the front of `held`: its message number and payload.
fn held_front(held: &[u8]) -> Option<(u8, &[u8])> {
    let (declared, rest) = held.split_first_chunk::<HELD_HEADER>()?;
    let len = usize::try_from(u32::from_be_bytes(*declared)).ok()?;
    let payload = rest.get(..len)?;
    Some((*payload.first()?, payload))
}

/// Frame, pad, and seal one payload at the tail of `outbound`, the payload
/// written by `write`, leaving everything as it was on any failure. The one
/// sealing path every message takes; `floor` is the reserve it must leave
/// untouched.
fn seal_into(
    outbound: &mut ByteQueue,
    sealer: &mut Sealer,
    reserve: &mut RingBuf<u8, PADDING_RESERVE>,
    framing: &Framing,
    floor: usize,
    write: impl FnOnce(&mut [u8]),
) -> Result<(), Sealing> {
    if sealer.is_keyed() && reserve.len() < framing.padding + floor {
        return Err(Sealing::Short);
    }
    let start = outbound.len();
    let frame = match outbound.append_slot(framing.total) {
        Ok(frame) => frame,
        Err(QueueError::Full) => return Err(Sealing::Refused(SendError::Backlogged)),
        Err(QueueError::Alloc(_)) => return Err(Sealing::Refused(SendError::OutOfMemory)),
    };
    write(&mut frame[framing.payload_range()]);
    let padding = &mut frame[framing.padding_range()];
    if sealer.is_keyed() {
        reserve.peek_slice(0, padding);
        reserve.discard_front(padding.len());
    } else {
        padding.fill(0);
    }
    if let Err(err) = sealer.seal(frame, framing) {
        outbound.truncate(start);
        return Err(Sealing::Fatal(TransportError::Packet(err)));
    }
    Ok(())
}

impl core::fmt::Debug for Transport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Transport")
            .field("role", &self.role)
            .field("closed", &self.closed)
            .field("kex_in_progress", &self.kex_in_progress())
            .field("strict", &self.kex.strict)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
