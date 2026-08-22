//! The store's ownership pin: the record that says which developer's data a
//! store directory holds.
//!
//! Ownership is pinned to the **publisher** — the developer's stable identity —
//! not to the key that signed the running build. A release re-signed with a
//! fresh build key therefore opens the same store, while a different developer
//! claiming the same bundle identifier is refused. The pin is written when a
//! store is first created (trust on first use) and compared on every open.
//!
//! The record is fixed-width and self-describing so a truncated or garbage
//! file is *refused* rather than read as some publisher: a pin that attests
//! nothing must never be mistaken for one that attests the caller.

use tairix_abi::appinfo::{PublisherId, PUBLISHER_ID_LEN};

/// Magic number identifying an ownership pin (`"AOWN"` little-endian).
const PIN_MAGIC: u32 = u32::from_le_bytes(*b"AOWN");

/// Version of the pin record layout.
const PIN_VERSION: u16 = 1;

/// Byte offset of the reserved pair; must be zero.
const RESERVED_OFFSET: usize = 6;

/// Byte offset of the publisher identity.
const PUBLISHER_OFFSET: usize = 8;

/// A store's ownership pin.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct OwnerPin {
    publisher: PublisherId,
}

impl OwnerPin {
    /// Encoded size of the record: magic (4), version (2), a reserved pair
    /// (2), then the publisher identity.
    pub const WIRE_LEN: usize = PUBLISHER_OFFSET + PUBLISHER_ID_LEN;

    /// Pin a store to `publisher`.
    #[must_use]
    pub const fn new(publisher: PublisherId) -> Self {
        Self { publisher }
    }

    /// The publisher this store belongs to.
    #[must_use]
    pub const fn publisher(&self) -> PublisherId {
        self.publisher
    }

    /// Encode the record for the volume.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[..4].copy_from_slice(&PIN_MAGIC.to_le_bytes());
        out[4..6].copy_from_slice(&PIN_VERSION.to_le_bytes());
        out[PUBLISHER_OFFSET..].copy_from_slice(self.publisher.as_bytes());
        out
    }

    /// Decode a pin record, or [`None`] for anything that is not exactly one.
    ///
    /// A wrong magic, an unknown version, a dirty reserved pair, a length that
    /// is not the record's, or the no-publisher sentinel all refuse: a store
    /// whose pin cannot be read attests no owner, and the caller then reaches
    /// nothing.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::WIRE_LEN {
            return None;
        }
        let magic: [u8; 4] = bytes[..4].try_into().ok()?;
        if u32::from_le_bytes(magic) != PIN_MAGIC {
            return None;
        }
        let version: [u8; 2] = bytes[4..6].try_into().ok()?;
        if u16::from_le_bytes(version) != PIN_VERSION {
            return None;
        }
        let reserved: [u8; 2] = bytes[RESERVED_OFFSET..PUBLISHER_OFFSET].try_into().ok()?;
        if u16::from_le_bytes(reserved) != 0 {
            return None;
        }
        let raw: [u8; PUBLISHER_ID_LEN] = bytes[PUBLISHER_OFFSET..].try_into().ok()?;
        let publisher = PublisherId::from_raw(raw);
        if publisher.is_none() {
            return None;
        }
        Some(Self { publisher })
    }
}

#[cfg(test)]
mod tests {
    use super::{OwnerPin, PIN_MAGIC, PUBLISHER_OFFSET, RESERVED_OFFSET};
    use tairix_abi::appinfo::{PublisherId, PUBLISHER_ID_LEN};

    fn publisher() -> PublisherId {
        PublisherId::from_raw([0x2A; PUBLISHER_ID_LEN])
    }

    #[test]
    fn a_pin_round_trips() {
        let pin = OwnerPin::new(publisher());
        let bytes = pin.encode();
        assert_eq!(bytes.len(), OwnerPin::WIRE_LEN);
        assert_eq!(OwnerPin::decode(&bytes), Some(pin));
        assert_eq!(
            OwnerPin::decode(&bytes).map(|pin| pin.publisher()),
            Some(publisher())
        );
    }

    #[test]
    fn anything_that_is_not_exactly_a_pin_is_refused() {
        let bytes = OwnerPin::new(publisher()).encode();

        // A short or long record.
        assert_eq!(OwnerPin::decode(&bytes[..bytes.len() - 1]), None);
        let mut long = [0u8; OwnerPin::WIRE_LEN + 1];
        long[..OwnerPin::WIRE_LEN].copy_from_slice(&bytes);
        assert_eq!(OwnerPin::decode(&long), None);
        assert_eq!(OwnerPin::decode(&[]), None);

        // A wrong magic, an unknown version, a dirty reserved pair.
        let mut wrong = bytes;
        wrong[0] ^= 0xFF;
        assert_eq!(OwnerPin::decode(&wrong), None);
        let mut future = bytes;
        future[4] = 2;
        assert_eq!(OwnerPin::decode(&future), None);
        let mut dirty = bytes;
        dirty[RESERVED_OFFSET] = 1;
        assert_eq!(OwnerPin::decode(&dirty), None);

        // The no-publisher sentinel attests nothing, so it is not a pin.
        let mut sentinel = bytes;
        for byte in &mut sentinel[PUBLISHER_OFFSET..] {
            *byte = 0;
        }
        assert_eq!(OwnerPin::decode(&sentinel), None);
    }

    #[test]
    fn a_zeroed_file_is_not_a_pin() {
        // The likeliest corruption shape — a file allocated but never written
        // — must not read as a valid record for the zero publisher.
        assert_eq!(OwnerPin::decode(&[0u8; OwnerPin::WIRE_LEN]), None);
        assert_ne!(PIN_MAGIC, 0);
    }
}
