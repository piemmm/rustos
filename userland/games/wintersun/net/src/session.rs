//! The sealed record transport.
//!
//! Once the handshake has agreed two directional keys, every message travels
//! as one record:
//!
//! ```text
//! [ plaintext length : u32 LE ] [ ciphertext ] [ Poly1305 tag : 16 ]
//! ```
//!
//! The nonce is the record's sequence number in its direction, and the
//! cleartext length header is the associated data. That one arrangement is
//! what refuses the whole family of frame attacks without a single extra
//! check:
//!
//! * **Reordered or replayed** — the nonce is the sequence the receiver
//!   expects next, so a record presented out of turn is opened under the
//!   wrong nonce and does not authenticate.
//! * **Truncated or extended** — the length is associated data, so a record
//!   whose header no longer matches its body does not authenticate.
//! * **Oversize** — the length is refused against the fixed record bound
//!   before a single body byte is read, so a hostile header buys no work.
//! * **Reflected** — the two directions use different keys, so a record sent
//!   back at its sender does not open.
//!
//! A record that fails any of these **ends the session**. The failure latches:
//! every later call on the same session refuses with the same reason. A
//! transport that skipped a bad record and carried on would let an attacker
//! probe it indefinitely.
//!
//! # What is wiped, and when
//!
//! The keys are wiped when the session drops. An opened record's plaintext is
//! wiped when its [`OpenRecord`] drops — unconditionally, because the one
//! message that carries a password looks like every other message from the
//! outside, and a receive buffer lives for the whole session.

use tairix_crypto::{open, seal, AeadKey, AeadNonce, AEAD_NONCE_LEN, AEAD_TAG_LEN};
use zeroize::Zeroize;

use crate::bounds::{MAX_PLAINTEXT_LEN, MAX_RECORD_LEN, RECORD_HEADER_LEN};
use crate::error::DisconnectReason;

/// A refusal from the record transport.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    /// The record's declared plaintext length is past the fixed bound.
    RecordTooLarge,
    /// The buffer holds fewer bytes than the record's header promises, or
    /// fewer than a header.
    Incomplete,
    /// The record did not authenticate.
    Authentication,
    /// The sequence for this direction is exhausted.
    SequenceExhausted,
    /// The output buffer cannot hold the record being sealed.
    BufferTooSmall,
    /// The session already ended, for this reason.
    Ended(DisconnectReason),
}

impl SessionError {
    /// The reason the connection ends.
    #[must_use]
    pub const fn disconnect_reason(self) -> DisconnectReason {
        match self {
            Self::RecordTooLarge => DisconnectReason::RecordTooLarge,
            Self::Incomplete | Self::Authentication => DisconnectReason::RecordAuthentication,
            Self::SequenceExhausted => DisconnectReason::SequenceExhausted,
            Self::BufferTooSmall => DisconnectReason::Internal,
            Self::Ended(reason) => reason,
        }
    }
}

/// The two directional record keys a handshake produced, in
/// client-to-server then server-to-client order.
///
/// Wiped on drop.
pub struct SessionKeys {
    client_to_server: AeadKey,
    server_to_client: AeadKey,
}

impl SessionKeys {
    /// Pair two keys.
    #[must_use]
    pub const fn new(client_to_server: AeadKey, server_to_client: AeadKey) -> Self {
        Self {
            client_to_server,
            server_to_client,
        }
    }

    /// The same pair from the realm's point of view: it sends what the
    /// client receives.
    #[must_use]
    pub fn swapped(self) -> Self {
        Self {
            client_to_server: self.server_to_client,
            server_to_client: self.client_to_server,
        }
    }

    /// The key this end seals with.
    #[must_use]
    pub const fn sending(&self) -> &AeadKey {
        &self.client_to_server
    }

    /// The key this end opens with.
    #[must_use]
    pub const fn receiving(&self) -> &AeadKey {
        &self.server_to_client
    }
}

impl Drop for SessionKeys {
    fn drop(&mut self) {
        self.client_to_server.zeroize();
        self.server_to_client.zeroize();
    }
}

/// One end of a sealed session.
///
/// Sequence numbers are per direction and never reused. The session latches
/// its first failure, so an authentication failure genuinely ends it rather
/// than being something the caller may choose to ignore.
pub struct Session {
    send_key: AeadKey,
    recv_key: AeadKey,
    send_sequence: u64,
    recv_sequence: u64,
    ended: Option<DisconnectReason>,
}

impl Session {
    /// Take up the keys a handshake produced.
    #[must_use]
    pub fn new(keys: SessionKeys) -> Self {
        let session = Self {
            send_key: *keys.sending(),
            recv_key: *keys.receiving(),
            send_sequence: 0,
            recv_sequence: 0,
            ended: None,
        };
        // Taking ownership is the point: the handshake's copy is wiped here,
        // so the keys exist in exactly one place for the rest of the session.
        drop(keys);
        session
    }

    /// The reason the session ended, once it has.
    #[must_use]
    pub const fn ended(&self) -> Option<DisconnectReason> {
        self.ended
    }

    /// End the session deliberately, so later calls refuse consistently with
    /// one the transport ended itself.
    pub const fn end(&mut self, reason: DisconnectReason) {
        if self.ended.is_none() {
            self.ended = Some(reason);
        }
    }

    /// How many bytes a record carrying `plaintext_len` bytes occupies.
    ///
    /// # Errors
    ///
    /// [`SessionError::RecordTooLarge`] past the fixed bound.
    pub const fn record_len(plaintext_len: usize) -> Result<usize, SessionError> {
        if plaintext_len > MAX_PLAINTEXT_LEN {
            return Err(SessionError::RecordTooLarge);
        }
        Ok(RECORD_HEADER_LEN + plaintext_len + AEAD_TAG_LEN)
    }

    /// Read a record's total length from its first [`RECORD_HEADER_LEN`]
    /// bytes, so a reader knows how much to await — and refuses an
    /// impossible length before awaiting anything.
    ///
    /// # Errors
    ///
    /// [`SessionError::Incomplete`] for a short header,
    /// [`SessionError::RecordTooLarge`] for a length past the bound.
    pub fn peek_record_len(header: &[u8]) -> Result<usize, SessionError> {
        let bytes: [u8; RECORD_HEADER_LEN] = header
            .get(..RECORD_HEADER_LEN)
            .ok_or(SessionError::Incomplete)?
            .try_into()
            .map_err(|_| SessionError::Incomplete)?;
        let declared =
            usize::try_from(u32::from_le_bytes(bytes)).map_err(|_| SessionError::RecordTooLarge)?;
        Self::record_len(declared)
    }

    /// Seal `plaintext` into `out`, returning the record's length.
    ///
    /// # Errors
    ///
    /// [`SessionError::Ended`] once the session has ended,
    /// [`SessionError::RecordTooLarge`] for an over-long plaintext,
    /// [`SessionError::BufferTooSmall`] for a short output, and
    /// [`SessionError::SequenceExhausted`] at the end of the sequence.
    pub fn seal_record(&mut self, plaintext: &[u8], out: &mut [u8]) -> Result<usize, SessionError> {
        if let Some(reason) = self.ended {
            return Err(SessionError::Ended(reason));
        }
        let total = Self::record_len(plaintext.len())?;
        let room = out.get_mut(..total).ok_or(SessionError::BufferTooSmall)?;
        // The last sequence value is reserved rather than used, so a wrap —
        // which would repeat a nonce and void the cipher — cannot happen.
        if self.send_sequence == u64::MAX {
            return Err(self.fail(SessionError::SequenceExhausted));
        }
        let Ok(declared) = u32::try_from(plaintext.len()) else {
            return Err(SessionError::RecordTooLarge);
        };

        let (header, rest) = room.split_at_mut(RECORD_HEADER_LEN);
        header.copy_from_slice(&declared.to_le_bytes());
        let aad = [header[0], header[1], header[2], header[3]];
        let (body, tag_out) = rest.split_at_mut(plaintext.len());
        body.copy_from_slice(plaintext);

        let nonce = nonce_for(self.send_sequence);
        let Ok(tag) = seal(&self.send_key, &nonce, &aad, body) else {
            // Only an input the cipher cannot accept reaches here, and the
            // record bound already excludes every such input; failing the
            // session is the fail-closed answer rather than retrying.
            return Err(self.fail(SessionError::Authentication));
        };
        tag_out.copy_from_slice(&tag);
        self.send_sequence += 1;
        Ok(total)
    }

    /// Open one whole record in place.
    ///
    /// `record` is the record exactly as it arrived, header included. On
    /// success the returned guard borrows the decrypted plaintext and wipes
    /// it when dropped.
    ///
    /// # Errors
    ///
    /// [`SessionError::Ended`] once the session has ended,
    /// [`SessionError::RecordTooLarge`], [`SessionError::Incomplete`], or
    /// [`SessionError::Authentication`] — and each of the last three ends the
    /// session, because a peer that sends one is not one to keep listening to.
    pub fn open_record<'a>(
        &mut self,
        record: &'a mut [u8],
    ) -> Result<OpenRecord<'a>, SessionError> {
        if let Some(reason) = self.ended {
            return Err(SessionError::Ended(reason));
        }
        if record.len() > MAX_RECORD_LEN {
            return Err(self.fail(SessionError::RecordTooLarge));
        }
        let total = match Self::peek_record_len(record) {
            Ok(total) => total,
            Err(err) => return Err(self.fail(err)),
        };
        if record.len() != total {
            return Err(self.fail(SessionError::Incomplete));
        }
        if self.recv_sequence == u64::MAX {
            return Err(self.fail(SessionError::SequenceExhausted));
        }

        let (header, rest) = record.split_at_mut(RECORD_HEADER_LEN);
        let aad = [header[0], header[1], header[2], header[3]];
        let body_len = total - RECORD_HEADER_LEN - AEAD_TAG_LEN;
        let (body, tag_bytes) = rest.split_at_mut(body_len);
        let mut tag = [0u8; AEAD_TAG_LEN];
        tag.copy_from_slice(tag_bytes);

        let nonce = nonce_for(self.recv_sequence);
        if open(&self.recv_key, &nonce, &aad, body, &tag).is_err() {
            return Err(self.fail(SessionError::Authentication));
        }
        self.recv_sequence += 1;
        Ok(OpenRecord { plaintext: body })
    }

    /// Latch the first failure and return it, so a caller cannot carry on
    /// past a record that did not authenticate.
    fn fail(&mut self, err: SessionError) -> SessionError {
        self.end(err.disconnect_reason());
        err
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.send_key.zeroize();
        self.recv_key.zeroize();
    }
}

/// A decrypted record's plaintext, wiped when it drops.
///
/// Wiped unconditionally: the record carrying an account password is
/// indistinguishable from any other until it has been decoded, and the
/// buffer beneath it is reused for the life of the session.
pub struct OpenRecord<'a> {
    plaintext: &'a mut [u8],
}

impl OpenRecord<'_> {
    /// The decrypted message bytes.
    #[must_use]
    pub fn plaintext(&self) -> &[u8] {
        self.plaintext
    }
}

impl Drop for OpenRecord<'_> {
    fn drop(&mut self) {
        self.plaintext.zeroize();
    }
}

/// The nonce for a record: its sequence in its direction, left-padded.
///
/// One record, one nonce, per key — and the keys differ by direction, so the
/// pair can never repeat within a session.
fn nonce_for(sequence: u64) -> AeadNonce {
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    let counter = sequence.to_le_bytes();
    // The counter occupies the low eight bytes; the high four stay zero.
    nonce[..counter.len()].copy_from_slice(&counter);
    nonce
}

#[cfg(test)]
mod tests;
