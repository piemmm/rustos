//! The `WinterSun` realm wire protocol: what a client and a realm say to each
//! other, and how the channel they say it over is established.
//!
//! This is a **game's** protocol, not an operating system's. It is held to
//! the same discipline as the user/kernel ABI — versioned, fixed-width,
//! little-endian, total bounded decode, fail closed, fuzzed — but it lives
//! here rather than in `lib/abi`, because `lib/abi` is the contract between
//! userland and the kernel and a realm's messages are no part of it.
//!
//! # The four layers
//!
//! * [`handshake`] — two plaintext messages that agree the session: X25519
//!   for the agreement, the realm's Ed25519 identity signing the transcript,
//!   HMAC-SHA256 over that transcript as the key schedule. The realm's
//!   identity is pinned on first connect and a change is surfaced.
//! * [`session`] — the sealed record transport: ChaCha20-Poly1305, one
//!   sequence-numbered nonce per record per direction, the length header as
//!   associated data. Reorder, replay, truncation, oversize and reflection
//!   are all refused by that construction, and each ends the session.
//! * [`client`] and [`server`] — the message vocabulary each direction may
//!   send, and its total decode.
//! * [`value`] — the identifiers, geometry, entity state, world edits and
//!   play events messages are built from.
//!
//! # What the wire will not carry
//!
//! The realm is authoritative and the client is assumed hostile, so there is
//! no message in which a client asserts where it is, what it hit, or what it
//! owns — only intents, which the realm validates. The world is a pure
//! function of its seed, so terrain is never transmitted; only what the seed
//! cannot predict is. And what a realm keeps secret — a dungeon's interior,
//! an unopened container, an undetected trap, a player outside your
//! awareness — has no encoding here at all, which is a stronger guarantee
//! than not sending it.
//!
//! # Bounds
//!
//! Every count and length in [`bounds`] is a security bound on untrusted
//! input, not a capacity. They do not scale with the machine and they do not
//! move to admit a frame. The record bound is derived from the widest message
//! the encoders can produce, so it can never refuse honest traffic.
//!
//! # Example
//!
//! Sealing an intent and opening it at the other end:
//!
//! ```
//! use tairix_wintersun_net::bounds::MAX_RECORD_LEN;
//! use tairix_wintersun_net::client::{ClientMessage, Intent, IntentKind};
//! use tairix_wintersun_net::session::{Session, SessionKeys};
//! use tairix_wintersun_net::value::{Direction, TickInstant, TickPhase};
//!
//! // Two ends of one handshake's output: the realm holds the same pair
//! // swapped, because it sends what the client receives.
//! let (c2s, s2c) = ([7u8; 32], [9u8; 32]);
//! let mut client = Session::new(SessionKeys::new(c2s, s2c));
//! let mut realm = Session::new(SessionKeys::new(c2s, s2c).swapped());
//!
//! let intent = ClientMessage::Intent(Intent {
//!     sequence: 1,
//!     sampled: TickInstant { tick: 90_113, phase: TickPhase(0x4000) },
//!     kind: IntentKind::Move(Direction::new(-23_170, -23_170).expect("a unit diagonal")),
//! });
//!
//! let mut plaintext = [0u8; 64];
//! let n = intent.encode(&mut plaintext).expect("fits");
//! let mut record = [0u8; MAX_RECORD_LEN];
//! let len = client.seal_record(&plaintext[..n], &mut record).expect("sealed");
//!
//! let open = realm.open_record(&mut record[..len]).expect("opened");
//! assert_eq!(ClientMessage::decode(open.plaintext()).expect("decoded"), intent);
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod bounds;
pub mod client;
pub mod codec;
pub mod error;
pub mod handshake;
pub mod server;
pub mod session;
pub mod value;

pub use bounds::{MAX_PLAINTEXT_LEN, MAX_RECORD_LEN, PROTOCOL_VERSION};
pub use client::ClientMessage;
pub use error::{DisconnectReason, WireError};
pub use handshake::{Established, HandshakeError, Initiator, Outcome, Pinning, Response};
pub use server::ServerMessage;
pub use session::{OpenRecord, Session, SessionError, SessionKeys};
