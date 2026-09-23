//! The binary packet protocol (RFC 4253 §6) under every framing the
//! [`crate::algorithm`] set needs, and the RFC 4344 rekey thresholds.
//!
//! A packet on the wire is
//!
//! ```text
//! uint32   packet_length
//! byte     padding_length
//! byte[n1] payload
//! byte[n2] padding
//! byte[m]  tag
//! ```
//!
//! and the framings differ in what is encrypted, what the tag covers, and
//! which bytes the padding aligns:
//!
//! | Framing | Length | Tag over | Padding aligns |
//! |---|---|---|---|
//! | none (before the first `NEWKEYS`) | clear | — | the whole packet to 8 |
//! | AES-CTR + `hmac-sha2-*` | encrypted | sequence number and plaintext | the whole packet to 16 |
//! | AES-CTR + `hmac-sha2-*-etm` | clear | sequence number, length, and ciphertext | all but the length to 16 |
//! | `aes*-gcm@openssh.com` | clear, as associated data | length and ciphertext | all but the length to 16 |
//! | `chacha20-poly1305@openssh.com` | encrypted under a second key | encrypted length and ciphertext | all but the length to 8 |
//!
//! Wherever the tag does not cover plaintext it is verified before anything
//! is decrypted, so a forged packet yields no plaintext at all. Every bound
//! here is a fixed validation bound on untrusted input: a packet declaring a
//! length past [`MAX_PACKET_LEN`] is refused before a byte of it is buffered,
//! and nothing a peer sends can move the bound.

use core::ops::Range;

use tairix_crypto::{
    chacha20_apply, chacha20_keystream, ct_eq, hmac_sha256_parts, hmac_sha512_parts, poly1305,
    Aes128Ctr, Aes128Gcm, Aes192Ctr, Aes256Ctr, Aes256Gcm, AesGcmNonce, ChaCha20Key, Poly1305Key,
};
use zeroize::Zeroize;

use crate::algorithm::{Cipher, Keys, Mac, MAX_TAG_LEN};

/// The largest `packet_length` accepted or sent. RFC 4253 §6.1 requires at
/// least 35 000 bytes of every implementation; this is OpenSSH's ceiling, so
/// anything a stock OpenSSH peer sends fits.
pub const MAX_PACKET_LEN: usize = 256 * 1024;

/// The least padding every packet carries (RFC 4253 §6).
pub const MIN_PADDING: usize = 4;

/// Bytes of the `packet_length` field.
pub const LENGTH_LEN: usize = 4;

/// The most bytes one packet occupies on the wire.
pub const MAX_FRAMED_LEN: usize = LENGTH_LEN + MAX_PACKET_LEN + MAX_TAG_LEN;

/// The most padding a packet framed by [`Sealer`] carries: a block less one,
/// on top of the minimum.
pub const MAX_PADDING: usize = 16 - 1 + MIN_PADDING;

/// Packets one key may carry in a direction before a rekey is due. RFC 4344
/// §3.1 asks for one within `2^32`; like OpenSSH this takes half, so the
/// exchange has room to finish.
pub const REKEY_PACKETS: u64 = 1 << 31;

/// Packets one key can carry at all: past this a 32-bit sequence number
/// repeats under the same key, which reuses a nonce or replays a MAC input.
const EPOCH_PACKETS: u64 = 1 << 32;

/// The block the unencrypted framing pads to.
const PLAIN_BLOCK: usize = 8;

/// `padding_length`, one payload byte, and the minimum padding: the least a
/// `packet_length` can say.
const MIN_PACKET_LEN: usize = 1 + 1 + MIN_PADDING;

/// Why a packet could not be framed or opened. Every variant but
/// [`PacketError::Frame`] ends the connection.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PacketError {
    /// A `packet_length` below the smallest packet, above
    /// [`MAX_PACKET_LEN`], or not aligned to the framing's block.
    Length,
    /// The MAC or authentication tag did not verify.
    Integrity,
    /// A `padding_length` below [`MIN_PADDING`] or leaving no payload.
    Padding,
    /// A payload too large for one packet.
    TooLarge,
    /// One more packet would reuse a sequence number under the current keys.
    SequenceExhausted,
    /// A cipher refused a run: a keystream or counter bound no working
    /// connection comes near.
    Cipher,
    /// The frame handed to [`Sealer::seal`] was not the length its
    /// [`Framing`] states.
    Frame,
}

/// How a payload is laid out on the wire under the current keys.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Framing {
    /// Payload bytes.
    pub payload: usize,
    /// Padding bytes.
    pub padding: usize,
    /// Wire bytes the whole packet takes, tag included.
    pub total: usize,
}

impl Framing {
    /// Where the payload sits in the frame.
    #[must_use]
    pub const fn payload_range(&self) -> Range<usize> {
        LENGTH_LEN + 1..LENGTH_LEN + 1 + self.payload
    }

    /// Where the padding sits in the frame.
    #[must_use]
    pub const fn padding_range(&self) -> Range<usize> {
        let start = LENGTH_LEN + 1 + self.payload;
        start..start + self.padding
    }
}

/// The RFC 5647 §7.1 nonce: a fixed field and a 64-bit invocation counter
/// incremented per packet.
struct GcmNonce {
    fixed: [u8; 4],
    invocation: u64,
}

impl GcmNonce {
    fn new(iv: &AesGcmNonce) -> Self {
        Self {
            fixed: window::<0, 4, 12>(iv),
            invocation: u64::from_be_bytes(window::<4, 8, 12>(iv)),
        }
    }

    fn current(&self) -> AesGcmNonce {
        let mut nonce = [0; 12];
        nonce[..4].copy_from_slice(&self.fixed);
        nonce[4..].copy_from_slice(&self.invocation.to_be_bytes());
        nonce
    }

    fn advance(&mut self) {
        self.invocation = self.invocation.wrapping_add(1);
    }
}

/// A cipher keyed for one direction, with its running state.
enum Crypt {
    /// Keys `K_2` (the payload and the Poly1305 key) and `K_1` (the length).
    ChaCha {
        main: ChaCha20Key,
        header: ChaCha20Key,
    },
    /// Expanded once per key, not per packet.
    Aes128Gcm(Aes128Gcm, GcmNonce),
    Aes256Gcm(Aes256Gcm, GcmNonce),
    Aes128Ctr(Aes128Ctr),
    Aes192Ctr(Aes192Ctr),
    Aes256Ctr(Aes256Ctr),
}

impl Drop for Crypt {
    fn drop(&mut self) {
        // The AES ciphers wipe their own expanded keys.
        match self {
            Self::ChaCha { main, header } => {
                main.zeroize();
                header.zeroize();
            }
            Self::Aes128Gcm(_, nonce) | Self::Aes256Gcm(_, nonce) => {
                nonce.fixed.zeroize();
                nonce.invocation.zeroize();
            }
            Self::Aes128Ctr(_) | Self::Aes192Ctr(_) | Self::Aes256Ctr(_) => {}
        }
    }
}

/// An HMAC keyed for one direction.
///
/// Keyed afresh per packet: the upstream keyed state is not wiped on drop,
/// so the key stays in these zeroizing arrays instead.
enum Integrity {
    Sha256([u8; 32]),
    Sha512([u8; 64]),
}

impl Drop for Integrity {
    fn drop(&mut self) {
        match self {
            Self::Sha256(key) => key.zeroize(),
            Self::Sha512(key) => key.zeroize(),
        }
    }
}

impl Integrity {
    /// The tag over the sequence number and `data`, into `out`.
    fn compute(&self, sequence: u32, data: &[u8], out: &mut [u8]) {
        let parts: [&[u8]; 2] = [&sequence.to_be_bytes(), data];
        match self {
            Self::Sha256(key) => out.copy_from_slice(&hmac_sha256_parts(key, &parts)),
            Self::Sha512(key) => out.copy_from_slice(&hmac_sha512_parts(key, &parts)),
        }
    }

    /// Whether `tag` is the tag over the sequence number and `data`,
    /// compared in constant time.
    fn verify(&self, sequence: u32, data: &[u8], tag: &[u8]) -> bool {
        let parts: [&[u8]; 2] = [&sequence.to_be_bytes(), data];
        match self {
            Self::Sha256(key) => ct_eq(&hmac_sha256_parts(key, &parts), tag),
            Self::Sha512(key) => ct_eq(&hmac_sha512_parts(key, &parts), tag),
        }
    }
}

/// `N` bytes of a key buffer from offset `AT`. The assertion makes the copy
/// unconditionally in bounds.
fn window<const AT: usize, const N: usize, const M: usize>(bytes: &[u8; M]) -> [u8; N] {
    const { assert!(AT + N <= M) };
    core::array::from_fn(|at| bytes[AT + at])
}

/// One direction's keys in running form.
struct Keyed {
    cipher: Cipher,
    crypt: Crypt,
    mac: Option<(Mac, Integrity)>,
    byte_limit: u64,
}

impl Keyed {
    fn new(keys: &Keys, byte_limit: Option<u64>) -> Self {
        let crypt = match keys.cipher {
            // OpenSSH derives 64 bytes and takes `K_2` first, then `K_1`.
            Cipher::ChaCha20Poly1305 => Crypt::ChaCha {
                main: window::<0, 32, 64>(&keys.key),
                header: window::<32, 32, 64>(&keys.key),
            },
            Cipher::Aes128Gcm => {
                let mut key = window::<0, 16, 64>(&keys.key);
                let crypt = Crypt::Aes128Gcm(
                    Aes128Gcm::new(&key),
                    GcmNonce::new(&window::<0, 12, 16>(&keys.iv)),
                );
                key.zeroize();
                crypt
            }
            Cipher::Aes256Gcm => {
                let mut key = window::<0, 32, 64>(&keys.key);
                let crypt = Crypt::Aes256Gcm(
                    Aes256Gcm::new(&key),
                    GcmNonce::new(&window::<0, 12, 16>(&keys.iv)),
                );
                key.zeroize();
                crypt
            }
            Cipher::Aes128Ctr => {
                let mut key = window::<0, 16, 64>(&keys.key);
                let crypt = Crypt::Aes128Ctr(Aes128Ctr::new(&key, &keys.iv));
                key.zeroize();
                crypt
            }
            Cipher::Aes192Ctr => {
                let mut key = window::<0, 24, 64>(&keys.key);
                let crypt = Crypt::Aes192Ctr(Aes192Ctr::new(&key, &keys.iv));
                key.zeroize();
                crypt
            }
            Cipher::Aes256Ctr => {
                let mut key = window::<0, 32, 64>(&keys.key);
                let crypt = Crypt::Aes256Ctr(Aes256Ctr::new(&key, &keys.iv));
                key.zeroize();
                crypt
            }
        };
        let mac = keys.mac.map(|mac| {
            let integrity = match mac {
                Mac::HmacSha256 | Mac::HmacSha256Etm => {
                    Integrity::Sha256(window::<0, 32, 64>(&keys.integrity))
                }
                Mac::HmacSha512 | Mac::HmacSha512Etm => {
                    Integrity::Sha512(window::<0, 64, 64>(&keys.integrity))
                }
            };
            (mac, integrity)
        });
        let bound = keys.cipher.rekey_bytes();
        Self {
            cipher: keys.cipher,
            crypt,
            mac,
            byte_limit: byte_limit.map_or(bound, |limit| limit.min(bound)),
        }
    }

    fn tag_len(&self) -> usize {
        self.cipher.tag_len() + self.mac.as_ref().map_or(0, |(mac, _)| mac.tag_len())
    }

    /// Whether the length field stands outside the padded, block-aligned
    /// region: true of every framing but counter mode with encrypt-and-MAC.
    fn length_outside_blocks(&self) -> bool {
        self.cipher.is_aead() || self.mac.as_ref().is_some_and(|(mac, _)| mac.is_etm())
    }

    /// The counter-mode cipher, if that is what this is.
    fn ctr(&mut self, buffer: &mut [u8]) -> Option<Result<(), PacketError>> {
        let applied = match &mut self.crypt {
            Crypt::Aes128Ctr(ctr) => ctr.apply(buffer),
            Crypt::Aes192Ctr(ctr) => ctr.apply(buffer),
            Crypt::Aes256Ctr(ctr) => ctr.apply(buffer),
            _ => return None,
        };
        Some(applied.map_err(|_| PacketError::Cipher))
    }

    /// Encrypt `body` (`packet_length` onward, padding included) in place
    /// and write its tag.
    fn seal(&mut self, sequence: u32, body: &mut [u8], tag: &mut [u8]) -> Result<(), PacketError> {
        match &mut self.crypt {
            Crypt::ChaCha { main, header } => {
                let nonce = u64::from(sequence).to_be_bytes();
                let mut poly_key: Poly1305Key = [0; 32];
                let sealed = chacha20_keystream(main, &nonce, 0, &mut poly_key)
                    .and_then(|()| chacha20_apply(header, &nonce, 0, &mut body[..LENGTH_LEN]))
                    .and_then(|()| chacha20_apply(main, &nonce, 1, &mut body[LENGTH_LEN..]));
                if sealed.is_ok() {
                    tag.copy_from_slice(&poly1305(&poly_key, body));
                }
                poly_key.zeroize();
                sealed.map_err(|_| PacketError::Cipher)
            }
            Crypt::Aes128Gcm(cipher, nonce) => {
                let (aad, rest) = body.split_at_mut(LENGTH_LEN);
                let sealed = cipher
                    .seal(&nonce.current(), aad, rest)
                    .map_err(|_| PacketError::Cipher)?;
                nonce.advance();
                tag.copy_from_slice(&sealed);
                Ok(())
            }
            Crypt::Aes256Gcm(cipher, nonce) => {
                let (aad, rest) = body.split_at_mut(LENGTH_LEN);
                let sealed = cipher
                    .seal(&nonce.current(), aad, rest)
                    .map_err(|_| PacketError::Cipher)?;
                nonce.advance();
                tag.copy_from_slice(&sealed);
                Ok(())
            }
            Crypt::Aes128Ctr(_) | Crypt::Aes192Ctr(_) | Crypt::Aes256Ctr(_) => {
                let etm = self.length_outside_blocks();
                if !etm {
                    self.integrity()?.compute(sequence, body, tag);
                }
                let region = if etm {
                    &mut body[LENGTH_LEN..]
                } else {
                    &mut *body
                };
                self.ctr(region).unwrap_or(Err(PacketError::Cipher))?;
                if etm {
                    self.integrity()?.compute(sequence, body, tag);
                }
                Ok(())
            }
        }
    }

    fn integrity(&self) -> Result<&Integrity, PacketError> {
        self.mac
            .as_ref()
            .map(|(_, integrity)| integrity)
            .ok_or(PacketError::Integrity)
    }

    /// The `packet_length` the first `first_len` received bytes declare.
    /// Counter mode with encrypt-and-MAC decrypts that first block in place;
    /// every other framing leaves the bytes as they arrived.
    fn peek_length(&mut self, sequence: u32, head: &mut [u8]) -> Result<usize, PacketError> {
        let mut length = [0; LENGTH_LEN];
        match &self.crypt {
            Crypt::ChaCha { header, .. } => {
                length.copy_from_slice(&head[..LENGTH_LEN]);
                let nonce = u64::from(sequence).to_be_bytes();
                chacha20_apply(header, &nonce, 0, &mut length).map_err(|_| PacketError::Cipher)?;
            }
            _ if self.length_outside_blocks() => length.copy_from_slice(&head[..LENGTH_LEN]),
            _ => {
                let block = self.cipher.block_len();
                self.ctr(&mut head[..block])
                    .unwrap_or(Err(PacketError::Cipher))?;
                length.copy_from_slice(&head[..LENGTH_LEN]);
            }
        }
        usize::try_from(u32::from_be_bytes(length)).map_err(|_| PacketError::Length)
    }

    /// Verify and decrypt a whole received packet in place, leaving the
    /// plaintext `packet_length` onward in `body`.
    fn open(&mut self, sequence: u32, body: &mut [u8], tag: &[u8]) -> Result<(), PacketError> {
        match &mut self.crypt {
            Crypt::ChaCha { main, header } => {
                let nonce = u64::from(sequence).to_be_bytes();
                let mut poly_key: Poly1305Key = [0; 32];
                let keyed = chacha20_keystream(main, &nonce, 0, &mut poly_key);
                let genuine = keyed.is_ok() && ct_eq(&poly1305(&poly_key, body), tag);
                poly_key.zeroize();
                keyed.map_err(|_| PacketError::Cipher)?;
                if !genuine {
                    return Err(PacketError::Integrity);
                }
                chacha20_apply(header, &nonce, 0, &mut body[..LENGTH_LEN])
                    .and_then(|()| chacha20_apply(main, &nonce, 1, &mut body[LENGTH_LEN..]))
                    .map_err(|_| PacketError::Cipher)
            }
            Crypt::Aes128Gcm(cipher, nonce) => {
                let (aad, rest) = body.split_at_mut(LENGTH_LEN);
                let tag = tag.try_into().map_err(|_| PacketError::Integrity)?;
                cipher
                    .open(&nonce.current(), aad, rest, tag)
                    .map_err(|_| PacketError::Integrity)?;
                nonce.advance();
                Ok(())
            }
            Crypt::Aes256Gcm(cipher, nonce) => {
                let (aad, rest) = body.split_at_mut(LENGTH_LEN);
                let tag = tag.try_into().map_err(|_| PacketError::Integrity)?;
                cipher
                    .open(&nonce.current(), aad, rest, tag)
                    .map_err(|_| PacketError::Integrity)?;
                nonce.advance();
                Ok(())
            }
            Crypt::Aes128Ctr(_) | Crypt::Aes192Ctr(_) | Crypt::Aes256Ctr(_) => {
                if self.length_outside_blocks() {
                    if !self.integrity()?.verify(sequence, body, tag) {
                        return Err(PacketError::Integrity);
                    }
                    self.ctr(&mut body[LENGTH_LEN..])
                        .unwrap_or(Err(PacketError::Cipher))
                } else {
                    let block = self.cipher.block_len();
                    self.ctr(&mut body[block..])
                        .unwrap_or(Err(PacketError::Cipher))?;
                    if self.integrity()?.verify(sequence, body, tag) {
                        Ok(())
                    } else {
                        Err(PacketError::Integrity)
                    }
                }
            }
        }
    }
}

/// The framing properties of one direction's current keys, or of the
/// unencrypted framing before any.
#[derive(Copy, Clone)]
struct Shape {
    block: usize,
    length_outside_blocks: bool,
    tag: usize,
    /// Received bytes needed before `packet_length` can be read.
    first: usize,
}

impl Shape {
    fn of(keys: Option<&Keyed>) -> Self {
        match keys {
            None => Self {
                block: PLAIN_BLOCK,
                length_outside_blocks: false,
                tag: 0,
                first: LENGTH_LEN,
            },
            Some(keyed) => {
                let outside = keyed.length_outside_blocks();
                Self {
                    block: keyed.cipher.block_len(),
                    length_outside_blocks: outside,
                    tag: keyed.tag_len(),
                    first: if outside {
                        LENGTH_LEN
                    } else {
                        keyed.cipher.block_len()
                    },
                }
            }
        }
    }

    /// The bytes the padding must align to a block multiple.
    const fn aligned(self, packet_len: usize) -> usize {
        if self.length_outside_blocks {
            packet_len
        } else {
            LENGTH_LEN + packet_len
        }
    }
}

/// A key epoch's position: the next sequence number, and what the current
/// keys have carried.
#[derive(Copy, Clone, Debug, Default)]
struct Epoch {
    sequence: u32,
    packets: u64,
    bytes: u64,
}

impl Epoch {
    /// Take the next sequence number, refusing one the current keys have
    /// already used.
    fn claim(&self) -> Result<u32, PacketError> {
        if self.packets >= EPOCH_PACKETS {
            Err(PacketError::SequenceExhausted)
        } else {
            Ok(self.sequence)
        }
    }

    fn record(&mut self, packet_len: usize) {
        self.sequence = self.sequence.wrapping_add(1);
        self.packets += 1;
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(LENGTH_LEN + packet_len).unwrap_or(u64::MAX));
    }

    fn rekeyed(&mut self, reset_sequence: bool) {
        if reset_sequence {
            self.sequence = 0;
        }
        self.packets = 0;
        self.bytes = 0;
    }

    fn rekey_due(&self, keys: Option<&Keyed>) -> bool {
        keys.is_some_and(|keyed| self.packets >= REKEY_PACKETS || self.bytes >= keyed.byte_limit)
    }
}

/// The sending direction: frames payloads under the current keys.
pub struct Sealer {
    keys: Option<Keyed>,
    epoch: Epoch,
}

impl Sealer {
    /// A direction with no keys yet, framing in the clear.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            keys: None,
            epoch: Epoch {
                sequence: 0,
                packets: 0,
                bytes: 0,
            },
        }
    }

    /// Whether packets are encrypted, and so must carry random padding.
    #[must_use]
    pub const fn is_keyed(&self) -> bool {
        self.keys.is_some()
    }

    /// The sequence number the next packet takes.
    #[must_use]
    pub const fn sequence(&self) -> u32 {
        self.epoch.sequence
    }

    /// How a payload of `payload_len` bytes is laid out: the least padding
    /// the framing allows, as OpenSSH chooses it.
    ///
    /// # Errors
    ///
    /// [`PacketError::TooLarge`] when the packet would pass
    /// [`MAX_PACKET_LEN`].
    pub fn framing(&self, payload_len: usize) -> Result<Framing, PacketError> {
        // Checked first, so the arithmetic below cannot overflow.
        if payload_len >= MAX_PACKET_LEN {
            return Err(PacketError::TooLarge);
        }
        let shape = Shape::of(self.keys.as_ref());
        let unpadded = shape.aligned(1 + payload_len);
        let mut padding = shape.block - unpadded % shape.block;
        if padding < MIN_PADDING {
            padding += shape.block;
        }
        let packet_len = 1 + payload_len + padding;
        if packet_len > MAX_PACKET_LEN {
            return Err(PacketError::TooLarge);
        }
        Ok(Framing {
            payload: payload_len,
            padding,
            total: LENGTH_LEN + packet_len + shape.tag,
        })
    }

    /// Seal `frame`, laid out by [`Self::framing`] with its payload and
    /// padding already in place, and take a sequence number for it.
    ///
    /// # Errors
    ///
    /// [`PacketError::Frame`] for a frame not of the framing's length,
    /// [`PacketError::SequenceExhausted`], or [`PacketError::Cipher`]. The
    /// sequence number is taken only on success.
    pub fn seal(&mut self, frame: &mut [u8], framing: &Framing) -> Result<(), PacketError> {
        let tag_len = Shape::of(self.keys.as_ref()).tag;
        let packet_len = 1 + framing.payload + framing.padding;
        if frame.len() != framing.total || framing.total != LENGTH_LEN + packet_len + tag_len {
            return Err(PacketError::Frame);
        }
        let sequence = self.epoch.claim()?;
        let (Ok(declared), Ok(padding)) =
            (u32::try_from(packet_len), u8::try_from(framing.padding))
        else {
            return Err(PacketError::Frame);
        };
        frame[..LENGTH_LEN].copy_from_slice(&declared.to_be_bytes());
        frame[LENGTH_LEN] = padding;
        let (body, tag) = frame.split_at_mut(LENGTH_LEN + packet_len);
        if let Some(keyed) = &mut self.keys {
            keyed.seal(sequence, body, tag)?;
        }
        self.epoch.record(packet_len);
        Ok(())
    }

    /// Switch to `keys` for every later packet — the effect of sending
    /// `SSH_MSG_NEWKEYS`. The old keys are wiped.
    pub fn rekey(&mut self, keys: &Keys, reset_sequence: bool, byte_limit: Option<u64>) {
        self.keys = Some(Keyed::new(keys, byte_limit));
        self.epoch.rekeyed(reset_sequence);
    }

    /// Whether the current keys have carried enough that RFC 4344 asks for
    /// a rekey.
    #[must_use]
    pub fn rekey_due(&self) -> bool {
        self.epoch.rekey_due(self.keys.as_ref())
    }
}

impl Default for Sealer {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`Opener::open`] found at the head of the received bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Opened {
    /// The packet is not all here: this many bytes at the head are needed.
    Need(usize),
    /// A whole packet, verified and decrypted in place.
    Packet {
        /// Head bytes the packet occupied.
        framed: usize,
        /// Where its payload sits in the head bytes.
        payload: Range<usize>,
        /// Its sequence number.
        sequence: u32,
    },
}

/// The receiving direction: opens packets under the current keys.
pub struct Opener {
    keys: Option<Keyed>,
    epoch: Epoch,
    /// The head packet's `packet_length` once read, so a counter-mode first
    /// block already decrypted in place is not decrypted twice.
    length: Option<usize>,
}

impl Opener {
    /// A direction with no keys yet, reading packets in the clear.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            keys: None,
            epoch: Epoch {
                sequence: 0,
                packets: 0,
                bytes: 0,
            },
            length: None,
        }
    }

    /// Whether packets arrive encrypted.
    #[must_use]
    pub const fn is_keyed(&self) -> bool {
        self.keys.is_some()
    }

    /// The sequence number the next packet takes.
    #[must_use]
    pub const fn sequence(&self) -> u32 {
        self.epoch.sequence
    }

    /// Open the packet at the front of `head`, the received bytes not yet
    /// consumed. Call again with the same bytes plus more after
    /// [`Opened::Need`]; after [`Opened::Packet`], drop its `framed` bytes
    /// before the next call.
    ///
    /// # Errors
    ///
    /// [`PacketError::Length`] as soon as the length is readable and wrong,
    /// [`PacketError::Integrity`], [`PacketError::Padding`],
    /// [`PacketError::SequenceExhausted`], or [`PacketError::Cipher`].
    pub fn open(&mut self, head: &mut [u8]) -> Result<Opened, PacketError> {
        let sequence = self.epoch.claim()?;
        let shape = Shape::of(self.keys.as_ref());
        let Some(packet_len) = self.head_length(sequence, shape, head)? else {
            return Ok(Opened::Need(shape.first));
        };
        let framed = LENGTH_LEN + packet_len + shape.tag;
        if head.len() < framed {
            return Ok(Opened::Need(framed));
        }
        let (body, rest) = head.split_at_mut(LENGTH_LEN + packet_len);
        if let Some(keyed) = &mut self.keys {
            keyed.open(sequence, body, &rest[..shape.tag])?;
        }
        self.length = None;
        let padding = usize::from(body[LENGTH_LEN]);
        if padding < MIN_PADDING || padding + 2 > packet_len {
            return Err(PacketError::Padding);
        }
        self.epoch.record(packet_len);
        let start = LENGTH_LEN + 1;
        Ok(Opened::Packet {
            framed,
            payload: start..start + packet_len - 1 - padding,
            sequence,
        })
    }

    /// The head packet's `packet_length`, read and checked once, or `None`
    /// until enough of it is here to read.
    fn head_length(
        &mut self,
        sequence: u32,
        shape: Shape,
        head: &mut [u8],
    ) -> Result<Option<usize>, PacketError> {
        if let Some(len) = self.length {
            return Ok(Some(len));
        }
        if head.len() < shape.first {
            return Ok(None);
        }
        let len = if let Some(keyed) = &mut self.keys {
            keyed.peek_length(sequence, head)?
        } else {
            let length = head
                .first_chunk::<LENGTH_LEN>()
                .ok_or(PacketError::Length)?;
            usize::try_from(u32::from_be_bytes(*length)).map_err(|_| PacketError::Length)?
        };
        if !(MIN_PACKET_LEN..=MAX_PACKET_LEN).contains(&len)
            || !shape.aligned(len).is_multiple_of(shape.block)
        {
            return Err(PacketError::Length);
        }
        self.length = Some(len);
        Ok(Some(len))
    }

    /// Switch to `keys` for every later packet — the effect of receiving
    /// `SSH_MSG_NEWKEYS`. The old keys are wiped.
    pub fn rekey(&mut self, keys: &Keys, reset_sequence: bool, byte_limit: Option<u64>) {
        self.keys = Some(Keyed::new(keys, byte_limit));
        self.epoch.rekeyed(reset_sequence);
        self.length = None;
    }

    /// Whether the current keys have carried enough that RFC 4344 asks for
    /// a rekey.
    #[must_use]
    pub fn rekey_due(&self) -> bool {
        self.epoch.rekey_due(self.keys.as_ref())
    }
}

impl Default for Opener {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "packet_tests.rs"]
mod tests;
