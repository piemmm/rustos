//! The authenticated session handshake.
//!
//! Two messages, in the clear, before anything else is sent:
//!
//! ```text
//! client -> realm   Hello        version, client ephemeral key, client nonce
//! realm  -> client  ServerHello  realm ephemeral key, realm nonce,
//!                                realm identity key, signature
//! ```
//!
//! and a third the realm sends instead of the second when it will not
//! proceed — a `Refused` naming the reason, because a refusal before the
//! keys exist still has to say why.
//!
//! Both sides then agree over X25519, hash the exact bytes they exchanged
//! into a transcript, and derive their two directional record keys from the
//! agreement keyed over that transcript. The realm's Ed25519 signature covers
//! the transcript, so it authenticates *this* exchange and cannot be lifted
//! onto another; the ephemeral keys mean a later compromise of the realm's
//! identity key does not open a recorded session.
//!
//! # Who is authenticated, and when
//!
//! The handshake authenticates the **realm**, not the player: the client
//! pins the realm's identity key on first connect and a change is surfaced,
//! so a credential cannot be harvested by a substituted server. The *player*
//! authenticates afterwards, inside the encrypted session, with an
//! `Authenticate` message — and where that carries a key rather than a
//! password it signs [`client_auth_payload`], which is this transcript, so
//! the proof is worthless anywhere else.
//!
//! # Signing stays outside
//!
//! `lib/crypto` exposes signature *verification* only, by design: private
//! keys live behind the capability authority, not in a wrapper any crate can
//! link. [`respond`] therefore takes a signer, so the realm's identity secret
//! never enters this crate.

use tairix_crypto::{
    derive_key, hmac_sha256, Ed25519PublicKey, Ed25519Signature, Sha256Digest, Sha256Stream,
    X25519PublicKey, X25519SecretKey, ED25519_PUBLIC_KEY_LEN, ED25519_SIGNATURE_LEN,
    SHA256_OUTPUT_LEN, X25519_PUBLIC_KEY_LEN, X25519_SECRET_LEN,
};

use crate::bounds::{HANDSHAKE_MAGIC, PROTOCOL_VERSION};
use crate::codec::{Reader, Writer};
use crate::error::{DisconnectReason, WireError};
use crate::session::SessionKeys;

/// Domain separator mixed into the transcript, so a hash from this protocol
/// can never collide with one from another.
const TRANSCRIPT_CONTEXT: &[u8] = b"wintersun-net v1 transcript";

/// What the realm's identity key signs.
const REALM_AUTH_CONTEXT: &[u8] = b"wintersun-net v1 realm identity";

/// What an account key signs to authenticate a player.
const CLIENT_AUTH_CONTEXT: &[u8] = b"wintersun-net v1 account identity";

/// Context separating the client-to-realm record key from its sibling.
const CLIENT_TO_SERVER_CONTEXT: &[u8] = b"wintersun-net v1 client-to-server records";

/// Context separating the realm-to-client record key from its sibling.
const SERVER_TO_CLIENT_CONTEXT: &[u8] = b"wintersun-net v1 server-to-client records";

/// A transcript hash: the exact handshake bytes both sides saw.
pub type Transcript = Sha256Digest;

/// Bytes of the per-side random each handshake message carries, so a reused
/// ephemeral key still yields a fresh transcript.
const NONCE_LEN: usize = 32;

/// Bytes of the header every handshake message carries.
const HEADER_LEN: usize = 4 + 2 + 2;

const KIND_HELLO: u16 = 1;
const KIND_SERVER_HELLO: u16 = 2;
const KIND_REFUSED: u16 = 3;

/// Encoded length of a client `Hello`.
pub const HELLO_LEN: usize = HEADER_LEN + X25519_PUBLIC_KEY_LEN + NONCE_LEN;

/// Encoded length of a realm `ServerHello`.
pub const SERVER_HELLO_LEN: usize =
    HEADER_LEN + X25519_PUBLIC_KEY_LEN + NONCE_LEN + ED25519_PUBLIC_KEY_LEN + ED25519_SIGNATURE_LEN;

/// Encoded length of a realm `Refused`.
pub const REFUSED_LEN: usize = HEADER_LEN + 2;

/// The longest handshake message, so a peer can size one buffer for all of
/// them and read no more than that before deciding.
pub const MAX_HANDSHAKE_LEN: usize = SERVER_HELLO_LEN;

/// Bytes of a `ServerHello` covered by the realm's signature: everything but
/// the signature itself.
const SERVER_HELLO_SIGNED_LEN: usize = SERVER_HELLO_LEN - ED25519_SIGNATURE_LEN;

/// Why a handshake did not produce a session.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HandshakeError {
    /// The peer's message did not decode.
    Malformed(WireError),
    /// The peer speaks a version this build does not.
    ProtocolVersion,
    /// The realm's signature did not verify, or its ephemeral key
    /// contributes nothing to the agreement.
    Authentication,
    /// The realm's identity key differs from the one pinned on first
    /// connect. The player is told; it is never quietly accepted.
    IdentityChanged {
        /// The key the client had pinned.
        pinned: [u8; ED25519_PUBLIC_KEY_LEN],
        /// The key this realm presented.
        presented: [u8; ED25519_PUBLIC_KEY_LEN],
    },
    /// The realm refused the connection, and said why.
    Refused(DisconnectReason),
}

impl HandshakeError {
    /// The reason the connection ends.
    #[must_use]
    pub const fn disconnect_reason(&self) -> DisconnectReason {
        match self {
            Self::Malformed(err) => err.disconnect_reason(),
            Self::ProtocolVersion => DisconnectReason::ProtocolVersion,
            Self::Authentication => DisconnectReason::HandshakeFailed,
            Self::IdentityChanged { .. } => DisconnectReason::RealmIdentityChanged,
            Self::Refused(reason) => *reason,
        }
    }
}

impl From<WireError> for HandshakeError {
    fn from(err: WireError) -> Self {
        Self::Malformed(err)
    }
}

/// Whether the realm's identity key was already known.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Pinning {
    /// It matched the key the client had pinned.
    Matched,
    /// The client had none, so this one is pinned now. The caller records it
    /// and tells the player which realm identity it just trusted.
    FirstUse,
}

/// A completed handshake: the record keys, the transcript both sides agreed,
/// and the realm identity that was authenticated.
pub struct Established {
    /// The two directional record keys.
    pub keys: SessionKeys,
    /// The transcript, which an account key signs to authenticate a player.
    pub transcript: Transcript,
    /// The realm's identity key.
    pub realm_identity: [u8; ED25519_PUBLIC_KEY_LEN],
    /// Whether that key was already pinned.
    pub pinning: Pinning,
}

/// A client's half of the handshake, between sending `Hello` and receiving
/// the realm's answer.
///
/// It owns the ephemeral secret and the exact `Hello` bytes it sent, because
/// the transcript is over what was *sent*, not over what can be rebuilt.
pub struct Initiator {
    secret: X25519SecretKey,
    hello: [u8; HELLO_LEN],
}

impl Initiator {
    /// Build the `Hello` to send.
    ///
    /// `ephemeral_secret` and `nonce` come from the caller's CSPRNG. The
    /// secret is ephemeral: a fresh one per connection is what gives a
    /// recorded session forward secrecy, and reusing one across connections
    /// forfeits it.
    #[must_use]
    pub fn start(
        ephemeral_secret: [u8; X25519_SECRET_LEN],
        nonce: [u8; NONCE_LEN],
    ) -> (Self, [u8; HELLO_LEN]) {
        let secret = X25519SecretKey::from_bytes(ephemeral_secret);
        let mut hello = [0u8; HELLO_LEN];
        let mut w = Writer::new(&mut hello);
        // Every field is fixed-width and the buffer is sized from them, so
        // these writes cannot fail; folding the results keeps a future field
        // that does not fit from silently truncating the message instead.
        let written = write_header(&mut w, KIND_HELLO)
            .and_then(|()| w.bytes(secret.public_key().as_bytes()))
            .and_then(|()| w.bytes(&nonce));
        debug_assert!(written.is_ok(), "the buffer is sized from these fields");
        (Self { secret, hello }, hello)
    }

    /// Consume the realm's answer.
    ///
    /// `pinned` is the realm identity the client recorded on a previous
    /// connect, if any.
    ///
    /// # Errors
    ///
    /// [`HandshakeError`] for a malformed or refusing answer, a signature
    /// that does not verify, a non-contributory realm key, or an identity
    /// that differs from the pinned one.
    pub fn finish(
        self,
        answer: &[u8],
        pinned: Option<&[u8; ED25519_PUBLIC_KEY_LEN]>,
    ) -> Result<Established, HandshakeError> {
        if answer.len() > MAX_HANDSHAKE_LEN {
            return Err(HandshakeError::Malformed(WireError::BoundExceeded));
        }
        let mut r = Reader::new(answer);
        let kind = read_header(&mut r)?;
        if kind == KIND_REFUSED {
            let reason = DisconnectReason::from_u16(r.u16()?)?;
            r.finish()?;
            return Err(HandshakeError::Refused(reason));
        }
        if kind != KIND_SERVER_HELLO {
            return Err(HandshakeError::Malformed(WireError::UnknownDiscriminant));
        }
        let realm_ephemeral = r.array::<X25519_PUBLIC_KEY_LEN>()?;
        let _realm_nonce = r.array::<NONCE_LEN>()?;
        let realm_identity = r.array::<ED25519_PUBLIC_KEY_LEN>()?;
        let signature = r.array::<ED25519_SIGNATURE_LEN>()?;
        r.finish()?;

        let pinning = match pinned {
            None => Pinning::FirstUse,
            Some(known) if known == &realm_identity => Pinning::Matched,
            Some(known) => {
                return Err(HandshakeError::IdentityChanged {
                    pinned: *known,
                    presented: realm_identity,
                })
            }
        };

        let signed = answer
            .get(..SERVER_HELLO_SIGNED_LEN)
            .ok_or(HandshakeError::Malformed(WireError::Truncated))?;
        let transcript = transcript_of(&self.hello, signed);

        let key = Ed25519PublicKey::from_bytes(&realm_identity)
            .map_err(|_| HandshakeError::Authentication)?;
        key.verify(
            &realm_auth_payload(&transcript),
            &Ed25519Signature::from_bytes(signature),
        )
        .map_err(|_| HandshakeError::Authentication)?;

        let keys = schedule(&self.secret, &realm_ephemeral, &transcript)?;
        Ok(Established {
            keys,
            transcript,
            realm_identity,
            pinning,
        })
    }
}

/// What a realm sends back, and what it keeps.
///
/// There is always something to send: a refusal that says nothing is a
/// connection the player cannot diagnose. So the bytes are a field rather
/// than a per-variant payload, and a gateway writes [`Response::to_send`]
/// before it looks at the outcome at all.
pub struct Response {
    /// Whether the session came up.
    pub outcome: Outcome,
    /// The message to send, in a buffer sized for the longer of the two.
    message: [u8; SERVER_HELLO_LEN],
    len: usize,
}

impl Response {
    /// The bytes to write to the peer, whichever way it went.
    #[must_use]
    pub fn to_send(&self) -> &[u8] {
        // `len` is set from a message this module built, so it is in range;
        // an empty answer would still be a refusal to send, never a panic.
        self.message.get(..self.len).unwrap_or(&[])
    }
}

/// Whether a handshake produced a session.
pub enum Outcome {
    /// It did; this is the realm's half.
    Accepted(Established),
    /// It did not, for this reason.
    Refused(DisconnectReason),
}

/// Answer a client's `Hello`.
///
/// `sign` is the realm's identity signer; it is handed the payload to sign
/// and returns the signature, so the identity secret stays with whatever
/// holds it. `identity_public` is the matching public key, which the client
/// pins.
#[must_use]
pub fn respond(
    hello: &[u8],
    ephemeral_secret: [u8; X25519_SECRET_LEN],
    nonce: [u8; NONCE_LEN],
    identity_public: &[u8; ED25519_PUBLIC_KEY_LEN],
    sign: impl FnOnce(&[u8]) -> [u8; ED25519_SIGNATURE_LEN],
) -> Response {
    match try_respond(hello, ephemeral_secret, nonce, identity_public, sign) {
        Ok(response) => response,
        Err(err) => refuse(err.disconnect_reason()),
    }
}

// Both handshake answers fit the one response buffer, which is what lets a
// gateway hold a single reply for either outcome.
const _: () = assert!(REFUSED_LEN <= SERVER_HELLO_LEN);
const _: () = assert!(SERVER_HELLO_LEN <= MAX_HANDSHAKE_LEN);
const _: () = assert!(HELLO_LEN <= MAX_HANDSHAKE_LEN);

/// Build a standalone refusal, for a realm turning a peer away before or
/// outside a handshake attempt (a connection cap, a ban, a shutdown).
#[must_use]
pub fn refuse(reason: DisconnectReason) -> Response {
    let mut message = [0u8; SERVER_HELLO_LEN];
    let mut w = Writer::new(&mut message);
    let written = write_header(&mut w, KIND_REFUSED).and_then(|()| w.u16(reason.as_u16()));
    debug_assert!(written.is_ok(), "a refusal always fits its own buffer");
    Response {
        outcome: Outcome::Refused(reason),
        message,
        len: REFUSED_LEN,
    }
}

fn try_respond(
    hello: &[u8],
    ephemeral_secret: [u8; X25519_SECRET_LEN],
    nonce: [u8; NONCE_LEN],
    identity_public: &[u8; ED25519_PUBLIC_KEY_LEN],
    sign: impl FnOnce(&[u8]) -> [u8; ED25519_SIGNATURE_LEN],
) -> Result<Response, HandshakeError> {
    if hello.len() != HELLO_LEN {
        return Err(HandshakeError::Malformed(if hello.len() < HELLO_LEN {
            WireError::Truncated
        } else {
            WireError::TrailingBytes
        }));
    }
    let mut r = Reader::new(hello);
    if read_header(&mut r)? != KIND_HELLO {
        return Err(HandshakeError::Malformed(WireError::UnknownDiscriminant));
    }
    let client_ephemeral = r.array::<X25519_PUBLIC_KEY_LEN>()?;
    let _client_nonce = r.array::<NONCE_LEN>()?;
    r.finish()?;

    let secret = X25519SecretKey::from_bytes(ephemeral_secret);
    let mut message = [0u8; SERVER_HELLO_LEN];
    {
        let mut w = Writer::new(&mut message);
        let written = write_header(&mut w, KIND_SERVER_HELLO)
            .and_then(|()| w.bytes(secret.public_key().as_bytes()))
            .and_then(|()| w.bytes(&nonce))
            .and_then(|()| w.bytes(identity_public));
        debug_assert!(written.is_ok(), "the buffer is sized from these fields");
    }
    let signed = message
        .get(..SERVER_HELLO_SIGNED_LEN)
        .ok_or(HandshakeError::Malformed(WireError::Truncated))?;
    let transcript = transcript_of(hello, signed);
    let signature = sign(&realm_auth_payload(&transcript));
    message
        .get_mut(SERVER_HELLO_SIGNED_LEN..)
        .ok_or(HandshakeError::Malformed(WireError::Truncated))?
        .copy_from_slice(&signature);

    let keys = schedule(&secret, &client_ephemeral, &transcript)?.swapped();
    Ok(Response {
        outcome: Outcome::Accepted(Established {
            keys,
            transcript,
            realm_identity: *identity_public,
            pinning: Pinning::Matched,
        }),
        message,
        len: SERVER_HELLO_LEN,
    })
}

/// The payload an account key signs to authenticate a player on this session.
///
/// It is the transcript under its own domain separator, so the proof binds to
/// this exchange and to nothing else: a captured `Authenticate` replayed onto
/// another session verifies against a different transcript and fails.
#[must_use]
pub fn client_auth_payload(
    transcript: &Transcript,
) -> [u8; CLIENT_AUTH_CONTEXT.len() + SHA256_OUTPUT_LEN] {
    payload(CLIENT_AUTH_CONTEXT, transcript)
}

/// The payload the realm's identity key signs.
#[must_use]
fn realm_auth_payload(
    transcript: &Transcript,
) -> [u8; REALM_AUTH_CONTEXT.len() + SHA256_OUTPUT_LEN] {
    payload(REALM_AUTH_CONTEXT, transcript)
}

fn payload<const N: usize>(context: &[u8], transcript: &Transcript) -> [u8; N] {
    let mut out = [0u8; N];
    // `N` is the context's own length plus the transcript's; the slicing
    // cannot fall short, and folding the writes keeps it total if it ever
    // could.
    let mut w = Writer::new(&mut out);
    let written = w.bytes(context).and_then(|()| w.bytes(transcript));
    debug_assert!(written.is_ok(), "N is the sum of the two lengths");
    out
}

/// Hash the exact bytes the two sides exchanged.
fn transcript_of(hello: &[u8], server_hello_signed: &[u8]) -> Transcript {
    let mut hash = Sha256Stream::new();
    hash.update(TRANSCRIPT_CONTEXT);
    hash.update(hello);
    hash.update(server_hello_signed);
    hash.finalize()
}

/// Derive the two directional record keys from the agreement over the
/// transcript.
///
/// HKDF's own two steps, in its own order. The agreement output is a curve
/// coordinate rather than a uniform bit string, so **extraction** condenses
/// it under the transcript as the salt — that is what the salt-keys-the-MAC
/// order is for — and **expansion** then draws one key per direction under a
/// distinct context. Neither key is ever the agreement output itself.
///
/// The result is ordered from the initiator's point of view; the responder
/// swaps it.
fn schedule(
    secret: &X25519SecretKey,
    peer: &[u8; X25519_PUBLIC_KEY_LEN],
    transcript: &Transcript,
) -> Result<SessionKeys, HandshakeError> {
    let shared = secret
        .agree(&X25519PublicKey::from_bytes(*peer))
        .map_err(|_| HandshakeError::Authentication)?;
    let master = hmac_sha256(transcript, shared.as_bytes());
    let keys = SessionKeys::new(
        derive_key(&master, CLIENT_TO_SERVER_CONTEXT),
        derive_key(&master, SERVER_TO_CLIENT_CONTEXT),
    );
    Ok(keys)
}

fn write_header(w: &mut Writer<'_>, kind: u16) -> Result<(), WireError> {
    w.u32(HANDSHAKE_MAGIC)?;
    w.u16(PROTOCOL_VERSION)?;
    w.u16(kind)
}

fn read_header(r: &mut Reader<'_>) -> Result<u16, HandshakeError> {
    if r.u32()? != HANDSHAKE_MAGIC {
        return Err(HandshakeError::Malformed(WireError::UnknownDiscriminant));
    }
    if r.u16()? != PROTOCOL_VERSION {
        return Err(HandshakeError::ProtocolVersion);
    }
    Ok(r.u16()?)
}

#[cfg(test)]
mod tests;
