//! The bounded per-inode attribute store and its self-identifying encoding.
//!
//! An [`AttrSet`] is an ordered collection of unique `(key, value)` pairs held
//! to the fixed security bounds in the crate root. A filesystem driver keeps
//! it as one copy-on-write metadata block: it [`decode`](AttrSet::decode)s the
//! block on read, mutates the set, then [`encode`](AttrSet::encode)s it back
//! into the block. The encoding is length-prefixed and self-identifying
//! (magic, version, and entry count), so a truncated, corrupt, or
//! out-of-bounds block is rejected fail-closed rather than misread.

use alloc::vec::Vec;

use crate::key::AttrKey;
use crate::{MetadataError, ATTRS_PER_INODE, TOTAL_ATTR_BYTES, VALUE_MAX};

/// Magic in the first four bytes of an encoded attribute set: `"AXS1"` little
/// end first. Distinguishes an attribute-set payload from any other block
/// content and pins the layout revision alongside [`ENCODING_VERSION`].
const ENCODING_MAGIC: u32 = 0x3153_5841;

/// On-disk encoding version understood by this build. A set encoded by a
/// different version is refused rather than misread.
const ENCODING_VERSION: u16 = 1;

/// Fixed bytes of the set header: magic (4) + version (2) + count (2).
const SET_HEADER_LEN: usize = 8;

/// Fixed bytes of one entry header: key length (2) + value length (2) +
/// flags (1) + reserved (3).
const ENTRY_HEADER_LEN: usize = 8;

/// Per-attribute flag bits. Unknown bits are rejected, so the set is closed
/// and evolved in place.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct AttrFlags(u8);

impl AttrFlags {
    /// The attribute is system-managed rather than user-authored.
    pub const SYSTEM: AttrFlags = AttrFlags(1 << 0);
    /// The attribute is excluded from backups / archive by default.
    pub const NO_BACKUP: AttrFlags = AttrFlags(1 << 1);

    /// Every defined flag bit; any bit outside this mask is reserved.
    const KNOWN_MASK: u8 = Self::SYSTEM.0 | Self::NO_BACKUP.0;

    /// No flags set.
    #[must_use]
    pub const fn empty() -> AttrFlags {
        AttrFlags(0)
    }

    /// The raw flag bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Build flags from raw bits, failing closed if any reserved bit is set.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Corrupt`] if `raw` has a bit outside the known mask.
    pub const fn from_bits(raw: u8) -> Result<AttrFlags, MetadataError> {
        if raw & !Self::KNOWN_MASK != 0 {
            return Err(MetadataError::Corrupt);
        }
        Ok(AttrFlags(raw))
    }

    /// Whether `other`'s bits are all set in `self`.
    #[must_use]
    pub const fn contains(self, other: AttrFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One `(key, flags, value)` attribute. The value is an opaque byte string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttrEntry {
    key: AttrKey,
    flags: AttrFlags,
    value: Vec<u8>,
}

impl AttrEntry {
    /// The entry's key.
    #[must_use]
    pub fn key(&self) -> &AttrKey {
        &self.key
    }

    /// The entry's flags.
    #[must_use]
    pub fn flags(&self) -> AttrFlags {
        self.flags
    }

    /// The entry's opaque value bytes.
    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.value
    }

    /// The number of logical bytes this entry contributes to the per-inode
    /// [`TOTAL_ATTR_BYTES`] budget: its key bytes plus its value bytes.
    fn logical_bytes(&self) -> usize {
        self.key.as_bytes().len() + self.value.len()
    }
}

/// An ordered set of unique attributes, bounded by the crate's fixed security
/// limits. Keys are unique by exact bytes (case-sensitive).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttrSet {
    entries: Vec<AttrEntry>,
}

impl AttrSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> AttrSet {
        AttrSet {
            entries: Vec::new(),
        }
    }

    /// The number of attributes in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the set holds no attributes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The summed logical key + value bytes across every attribute.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.entries.iter().map(AttrEntry::logical_bytes).sum()
    }

    /// The value of the attribute named exactly by `key`, or `None`.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|entry| entry.key.as_bytes() == key)
            .map(AttrEntry::value)
    }

    /// Iterate the entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &AttrEntry> {
        self.entries.iter()
    }

    /// Insert or replace the attribute named `key`.
    ///
    /// Validates the key grammar, the value length, and — treating a replace
    /// as removing the old entry first — the per-inode count and total-bytes
    /// bounds. The whole call is rejected on any violation; nothing partial is
    /// applied.
    ///
    /// # Errors
    ///
    /// * [`MetadataError::ValueTooLong`] if `value` exceeds [`VALUE_MAX`].
    /// * [`MetadataError::TooManyAttributes`] if a *new* key would exceed
    ///   [`ATTRS_PER_INODE`].
    /// * [`MetadataError::TotalBytesExceeded`] if the resulting total would
    ///   exceed [`TOTAL_ATTR_BYTES`].
    /// * the key-grammar errors from [`AttrKey::parse`].
    pub fn set(&mut self, key: &[u8], flags: AttrFlags, value: &[u8]) -> Result<(), MetadataError> {
        if value.len() > VALUE_MAX {
            return Err(MetadataError::ValueTooLong);
        }
        let parsed = AttrKey::parse(key)?;
        let existing = self
            .entries
            .iter()
            .position(|entry| entry.key.as_bytes() == key);
        let new_entry_bytes = key.len() + value.len();
        let projected_total = if let Some(idx) = existing {
            self.total_bytes() - self.entries[idx].logical_bytes() + new_entry_bytes
        } else {
            if self.entries.len() >= ATTRS_PER_INODE {
                return Err(MetadataError::TooManyAttributes);
            }
            self.total_bytes() + new_entry_bytes
        };
        if projected_total > TOTAL_ATTR_BYTES {
            return Err(MetadataError::TotalBytesExceeded);
        }
        let entry = AttrEntry {
            key: parsed,
            flags,
            value: value.to_vec(),
        };
        match existing {
            Some(idx) => self.entries[idx] = entry,
            None => self.entries.push(entry),
        }
        Ok(())
    }

    /// Remove the attribute named exactly by `key`, returning whether one was
    /// present.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.key.as_bytes() != key);
        self.entries.len() != before
    }

    /// Serialise the set into its self-identifying on-disk encoding.
    ///
    /// The returned bytes are what a filesystem driver writes verbatim into
    /// one metadata block. The length is bounded by the fixed set header, the
    /// per-entry headers ([`ATTRS_PER_INODE`] of them), and
    /// [`TOTAL_ATTR_BYTES`] of key + value bytes, so it fits a single
    /// 4 `KiB`-block volume's metadata payload.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&ENCODING_MAGIC.to_le_bytes());
        out.extend_from_slice(&ENCODING_VERSION.to_le_bytes());
        let count = u16::try_from(self.entries.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&count.to_le_bytes());
        for entry in &self.entries {
            let key = entry.key.as_bytes();
            let key_len = u16::try_from(key.len()).unwrap_or(u16::MAX);
            let value_len = u16::try_from(entry.value.len()).unwrap_or(u16::MAX);
            out.extend_from_slice(&key_len.to_le_bytes());
            out.extend_from_slice(&value_len.to_le_bytes());
            out.push(entry.flags.bits());
            out.extend_from_slice(&[0u8; 3]);
            out.extend_from_slice(key);
            out.extend_from_slice(&entry.value);
        }
        out
    }

    /// Parse a set from its on-disk encoding, validating every field against
    /// the grammar and the fixed bounds.
    ///
    /// Trailing bytes after the last entry (a block's zero padding) are
    /// ignored. A short, mistyped, over-count, out-of-bounds, or
    /// duplicate-key encoding is rejected fail-closed.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Corrupt`] on any structural violation; the
    /// key-grammar and bound errors from [`AttrSet::set`] for the contents.
    pub fn decode(bytes: &[u8]) -> Result<AttrSet, MetadataError> {
        if bytes.len() < SET_HEADER_LEN {
            return Err(MetadataError::Corrupt);
        }
        if read_u32(bytes, 0) != ENCODING_MAGIC || read_u16(bytes, 4) != ENCODING_VERSION {
            return Err(MetadataError::Corrupt);
        }
        let count = usize::from(read_u16(bytes, 6));
        if count > ATTRS_PER_INODE {
            return Err(MetadataError::Corrupt);
        }
        let mut set = AttrSet::new();
        let mut off = SET_HEADER_LEN;
        for _ in 0..count {
            if off + ENTRY_HEADER_LEN > bytes.len() {
                return Err(MetadataError::Corrupt);
            }
            let key_len = usize::from(read_u16(bytes, off));
            let value_len = usize::from(read_u16(bytes, off + 2));
            let flags = AttrFlags::from_bits(bytes[off + 4])?;
            off += ENTRY_HEADER_LEN;
            let key_end = off.checked_add(key_len).ok_or(MetadataError::Corrupt)?;
            let value_end = key_end
                .checked_add(value_len)
                .ok_or(MetadataError::Corrupt)?;
            if value_end > bytes.len() {
                return Err(MetadataError::Corrupt);
            }
            let key = &bytes[off..key_end];
            let value = &bytes[key_end..value_end];
            if set.get(key).is_some() {
                return Err(MetadataError::Corrupt);
            }
            set.set(key, flags, value)?;
            off = value_end;
        }
        Ok(set)
    }
}

fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
