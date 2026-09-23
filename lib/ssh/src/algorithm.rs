//! The ciphers and MACs the packet layer frames (`plans/SSH.md` §4), and the
//! key material one direction of a connection is keyed with.
//!
//! Each algorithm's name, lengths, and framing properties are defined once
//! here, so negotiation, key derivation, and the packet layer cannot disagree
//! about what a name means. Nothing outside the plan's admitted set has a
//! variant: an algorithm TAIRiX refuses is not a disabled option here, it does
//! not exist.

use zeroize::Zeroize;

/// The longest cipher key any [`Cipher`] takes: `chacha20-poly1305`'s two
/// 256-bit keys.
pub const MAX_KEY_LEN: usize = 64;

/// The longest initial IV any [`Cipher`] takes: an AES-CTR counter block.
pub const MAX_IV_LEN: usize = 16;

/// The longest integrity key any [`Mac`] takes.
pub const MAX_MAC_KEY_LEN: usize = 64;

/// The longest authentication tag any cipher or MAC appends.
pub const MAX_TAG_LEN: usize = 64;

/// An encryption algorithm.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Cipher {
    /// `chacha20-poly1305@openssh.com`.
    ChaCha20Poly1305,
    /// `aes128-gcm@openssh.com` (RFC 5647).
    Aes128Gcm,
    /// `aes256-gcm@openssh.com` (RFC 5647).
    Aes256Gcm,
    /// `aes128-ctr` (RFC 4344).
    Aes128Ctr,
    /// `aes192-ctr` (RFC 4344).
    Aes192Ctr,
    /// `aes256-ctr` (RFC 4344).
    Aes256Ctr,
}

impl Cipher {
    /// Every cipher, in OpenSSH's default order of preference.
    pub const ALL: [Self; 6] = [
        Self::ChaCha20Poly1305,
        Self::Aes128Gcm,
        Self::Aes256Gcm,
        Self::Aes128Ctr,
        Self::Aes192Ctr,
        Self::Aes256Ctr,
    ];

    /// The name negotiation uses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ChaCha20Poly1305 => "chacha20-poly1305@openssh.com",
            Self::Aes128Gcm => "aes128-gcm@openssh.com",
            Self::Aes256Gcm => "aes256-gcm@openssh.com",
            Self::Aes128Ctr => "aes128-ctr",
            Self::Aes192Ctr => "aes192-ctr",
            Self::Aes256Ctr => "aes256-ctr",
        }
    }

    /// The cipher `name` denotes, if it is one this engine frames.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|cipher| cipher.name().as_bytes() == name)
    }

    /// Key bytes the key exchange must derive.
    #[must_use]
    pub const fn key_len(self) -> usize {
        match self {
            Self::ChaCha20Poly1305 => 64,
            Self::Aes128Gcm | Self::Aes128Ctr => 16,
            Self::Aes192Ctr => 24,
            Self::Aes256Gcm | Self::Aes256Ctr => 32,
        }
    }

    /// Initial-IV bytes the key exchange must derive.
    #[must_use]
    pub const fn iv_len(self) -> usize {
        match self {
            Self::ChaCha20Poly1305 => 0,
            Self::Aes128Gcm | Self::Aes256Gcm => 12,
            Self::Aes128Ctr | Self::Aes192Ctr | Self::Aes256Ctr => 16,
        }
    }

    /// The block size packets are padded to a multiple of.
    #[must_use]
    pub const fn block_len(self) -> usize {
        match self {
            Self::ChaCha20Poly1305 => 8,
            _ => 16,
        }
    }

    /// Bytes of authentication tag the cipher appends itself: non-zero
    /// exactly for the authenticated ciphers, which take no separate MAC.
    #[must_use]
    pub const fn tag_len(self) -> usize {
        match self {
            Self::ChaCha20Poly1305 | Self::Aes128Gcm | Self::Aes256Gcm => 16,
            Self::Aes128Ctr | Self::Aes192Ctr | Self::Aes256Ctr => 0,
        }
    }

    /// Whether the cipher authenticates as well as encrypts.
    #[must_use]
    pub const fn is_aead(self) -> bool {
        self.tag_len() != 0
    }

    /// Bytes one key may protect before RFC 4344 §3.2 asks for a rekey:
    /// `2^32` blocks for a 128-bit block cipher, and the gigabyte RFC 4253
    /// §9 recommends for the rest — the bounds OpenSSH applies.
    #[must_use]
    pub const fn rekey_bytes(self) -> u64 {
        match self {
            Self::ChaCha20Poly1305 => 1 << 30,
            _ => 16 << 32,
        }
    }
}

/// A message authentication algorithm, for the ciphers that do not carry
/// their own.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Mac {
    /// `hmac-sha2-256-etm@openssh.com`: encrypt-then-MAC.
    HmacSha256Etm,
    /// `hmac-sha2-512-etm@openssh.com`: encrypt-then-MAC.
    HmacSha512Etm,
    /// `hmac-sha2-256` (RFC 6668): encrypt-and-MAC.
    HmacSha256,
    /// `hmac-sha2-512` (RFC 6668): encrypt-and-MAC.
    HmacSha512,
}

impl Mac {
    /// Every MAC, in OpenSSH's default order of preference.
    pub const ALL: [Self; 4] = [
        Self::HmacSha256Etm,
        Self::HmacSha512Etm,
        Self::HmacSha256,
        Self::HmacSha512,
    ];

    /// The name negotiation uses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::HmacSha256Etm => "hmac-sha2-256-etm@openssh.com",
            Self::HmacSha512Etm => "hmac-sha2-512-etm@openssh.com",
            Self::HmacSha256 => "hmac-sha2-256",
            Self::HmacSha512 => "hmac-sha2-512",
        }
    }

    /// The MAC `name` denotes, if it is one this engine frames.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mac| mac.name().as_bytes() == name)
    }

    /// Integrity-key bytes the key exchange must derive.
    #[must_use]
    pub const fn key_len(self) -> usize {
        self.tag_len()
    }

    /// Bytes of tag appended to each packet.
    #[must_use]
    pub const fn tag_len(self) -> usize {
        match self {
            Self::HmacSha256Etm | Self::HmacSha256 => 32,
            Self::HmacSha512Etm | Self::HmacSha512 => 64,
        }
    }

    /// Whether the tag covers the ciphertext and a plaintext length
    /// (encrypt-then-MAC) rather than the plaintext (encrypt-and-MAC).
    #[must_use]
    pub const fn is_etm(self) -> bool {
        matches!(self, Self::HmacSha256Etm | Self::HmacSha512Etm)
    }
}

/// Why key material was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum KeyError {
    /// A key, IV, or integrity key was not the length its algorithm takes.
    Length,
    /// An authenticated cipher was paired with a MAC, or another cipher
    /// with none.
    Pairing,
}

/// The algorithms and key material one direction of a connection is keyed
/// with, as the key exchange derived them (RFC 4253 §7.2).
///
/// Every byte is zeroed on drop.
pub struct Keys {
    pub(crate) cipher: Cipher,
    pub(crate) mac: Option<Mac>,
    pub(crate) key: [u8; MAX_KEY_LEN],
    pub(crate) iv: [u8; MAX_IV_LEN],
    pub(crate) integrity: [u8; MAX_MAC_KEY_LEN],
}

impl Keys {
    /// Key one direction with `cipher` and, for a cipher that does not
    /// authenticate, `mac`.
    ///
    /// # Errors
    ///
    /// [`KeyError::Pairing`] when `mac` is present for an authenticated
    /// cipher or absent for another; [`KeyError::Length`] when a slice is not
    /// the length its algorithm takes (an authenticated cipher takes an empty
    /// `integrity`).
    pub fn new(
        cipher: Cipher,
        mac: Option<Mac>,
        key: &[u8],
        iv: &[u8],
        integrity: &[u8],
    ) -> Result<Self, KeyError> {
        if cipher.is_aead() == mac.is_some() {
            return Err(KeyError::Pairing);
        }
        let integrity_len = mac.map_or(0, Mac::key_len);
        if key.len() != cipher.key_len()
            || iv.len() != cipher.iv_len()
            || integrity.len() != integrity_len
        {
            return Err(KeyError::Length);
        }
        let mut keys = Self {
            cipher,
            mac,
            key: [0; MAX_KEY_LEN],
            iv: [0; MAX_IV_LEN],
            integrity: [0; MAX_MAC_KEY_LEN],
        };
        keys.key[..key.len()].copy_from_slice(key);
        keys.iv[..iv.len()].copy_from_slice(iv);
        keys.integrity[..integrity.len()].copy_from_slice(integrity);
        Ok(keys)
    }

    /// The cipher.
    #[must_use]
    pub const fn cipher(&self) -> Cipher {
        self.cipher
    }

    /// The MAC, for a cipher that does not authenticate.
    #[must_use]
    pub const fn mac(&self) -> Option<Mac> {
        self.mac
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        self.key.zeroize();
        self.iv.zeroize();
        self.integrity.zeroize();
    }
}

impl core::fmt::Debug for Keys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The material itself is never printed.
        f.debug_struct("Keys")
            .field("cipher", &self.cipher)
            .field("mac", &self.mac)
            .finish_non_exhaustive()
    }
}
