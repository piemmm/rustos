//! Capability sets and the subset-only delegation operator.
//!
//! [`CapabilitySet`] is an ergonomic wrapper around [`BitSet256`] keyed by
//! [`CapabilityId`]. The wrapper exists for two reasons:
//!
//! 1. It makes "set of capabilities" a *type* the rest of the system can
//!    talk about; an opaque [`BitSet256`] could equally well be holding
//!    syscall numbers or scheduler priorities and would silently mix.
//! 2. It is the only place [`CapabilitySet::delegate`] lives, so the
//!    delegation invariant ("a delegated set is always a subset of the
//!    parent set") cannot be bypassed by mistake.

use tairix_abi::{CapabilityId, Errno};
use tairix_collections::BitSet256;

/// Set of capabilities held by a principal (task, manifest, or token).
///
/// Internally a 256-bit bitmap; cheap to copy and compare. `default()`
/// yields the empty set.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct CapabilitySet {
    bits: BitSet256,
}

impl CapabilitySet {
    /// The empty set.
    pub const EMPTY: Self = Self {
        bits: BitSet256::EMPTY,
    };

    /// Construct an empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self::EMPTY
    }

    /// Construct a set from raw backing words.
    ///
    /// Used by the wire-format decoders in [`crate::token`]; not intended
    /// for direct use by application code.
    #[must_use]
    pub const fn from_words(words: [u64; 4]) -> Self {
        Self {
            bits: BitSet256::from_words(words),
        }
    }

    /// Expose the raw backing words (little-endian, lowest-indexed first).
    #[must_use]
    pub const fn as_words(&self) -> &[u64; 4] {
        self.bits.as_words()
    }

    /// Length, in bytes, of the [`CapabilitySet`] wire form: the 256-bit
    /// bitmap as four little-endian `u64` words, lowest-indexed first.
    pub const WIRE_LEN: usize = 32;

    /// Encode the set into its little-endian wire form ([`Self::WIRE_LEN`]
    /// bytes): four `u64` words, lowest-indexed first.
    ///
    /// This is the single definition of the on-wire capability-set layout;
    /// [`crate::token::CapabilityToken`] embeds the same bytes, and the
    /// `cap_delegate` syscall copies them in.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let words = self.bits.as_words();
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..8].copy_from_slice(&words[0].to_le_bytes());
        out[8..16].copy_from_slice(&words[1].to_le_bytes());
        out[16..24].copy_from_slice(&words[2].to_le_bytes());
        out[24..32].copy_from_slice(&words[3].to_le_bytes());
        out
    }

    /// Decode a [`CapabilitySet`] from its little-endian wire form.
    ///
    /// Returns [`Errno::BufferTooSmall`] if `bytes` is shorter than
    /// [`Self::WIRE_LEN`]; the first [`Self::WIRE_LEN`] bytes are consumed.
    /// Every bit pattern is a representable set, so no further validation is
    /// needed — a set carrying a bit outside the parent's authority is
    /// rejected later by [`Self::delegate`] (fail closed).
    pub fn from_le_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let word = |offset: usize| {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[offset..offset + 8]);
            u64::from_le_bytes(buf)
        };
        Ok(Self::from_words([word(0), word(8), word(16), word(24)]))
    }

    /// Add a capability to the set.
    pub fn insert(&mut self, cap: CapabilityId) {
        self.bits.insert(cap.as_u16());
    }

    /// Remove a capability from the set.
    pub fn remove(&mut self, cap: CapabilityId) {
        self.bits.remove(cap.as_u16());
    }

    /// `true` if the set holds `cap`.
    #[must_use]
    pub fn contains(&self, cap: CapabilityId) -> bool {
        self.bits.contains(cap.as_u16())
    }

    /// `true` if the set contains no capabilities.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    /// Number of capabilities in the set.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.bits.len()
    }

    /// `true` if every capability in `self` is also in `other`.
    #[must_use]
    pub const fn is_subset_of(&self, other: &Self) -> bool {
        self.bits.is_subset_of(&other.bits)
    }

    /// Compute the union with `other`.
    #[must_use]
    pub const fn union(&self, other: &Self) -> Self {
        Self {
            bits: self.bits.union(&other.bits),
        }
    }

    /// Compute the intersection with `other`.
    #[must_use]
    pub const fn intersection(&self, other: &Self) -> Self {
        Self {
            bits: self.bits.intersection(&other.bits),
        }
    }

    /// Delegate `requested` out of `self`.
    ///
    /// Returns `requested` unchanged if it is a subset of `self`. If
    /// `requested` would *widen* the parent's authority (any bit set in
    /// `requested` but not in `self`) the call is rejected with
    /// [`Errno::DelegationWiden`]; this is the central security invariant
    /// of the capability system.
    pub fn delegate(&self, requested: &Self) -> Result<Self, Errno> {
        if requested.is_subset_of(self) {
            Ok(*requested)
        } else {
            Err(Errno::DelegationWiden)
        }
    }

    /// Revoke a single capability, returning the previously held state.
    ///
    /// Mirrors `HashSet::take` semantics for the bitmap: returns `true` if
    /// the capability had been present, `false` otherwise.
    pub fn revoke(&mut self, cap: CapabilityId) -> bool {
        let was_present = self.contains(cap);
        self.bits.remove(cap.as_u16());
        was_present
    }

    /// Iterate over the capabilities in the set in ascending order.
    #[must_use]
    pub fn iter(&self) -> CapabilitySetIter {
        CapabilitySetIter {
            inner: self.bits.iter(),
        }
    }
}

impl IntoIterator for &CapabilitySet {
    type Item = CapabilityId;
    type IntoIter = CapabilitySetIter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Expose a [`CapabilitySet`] to ABI-level host seams that gate on a
/// granted capability through `&dyn tairix_abi::CapabilityQuery` without
/// naming this crate (the `lib/abi -> lib/caps`
/// reverse edge is forbidden).
impl tairix_abi::CapabilityQuery for CapabilitySet {
    fn holds(&self, cap: CapabilityId) -> bool {
        self.contains(cap)
    }
}

/// Ascending iterator over the capabilities of a [`CapabilitySet`].
#[derive(Clone, Debug)]
pub struct CapabilitySetIter {
    inner: tairix_collections::BitSet256Iter,
}

impl Iterator for CapabilitySetIter {
    type Item = CapabilityId;

    fn next(&mut self) -> Option<Self::Item> {
        // `BitSet256Iter` only yields values < 256, which is exactly the
        // range `CapabilityId::from_raw` accepts; converting via the public
        // constructor keeps the invariant local rather than spread.
        loop {
            let raw = self.inner.next()?;
            if let Ok(id) = CapabilityId::from_raw(raw) {
                return Some(id);
            }
        }
    }
}

impl core::iter::FusedIterator for CapabilitySetIter {}

#[cfg(test)]
mod tests {
    use super::CapabilitySet;
    use tairix_abi::{CapabilityId, CapabilityQuery, Errno};

    #[test]
    fn capability_query_matches_contains() {
        let mut s = CapabilitySet::empty();
        s.insert(CapabilityId::MEM_DMA);
        let query: &dyn CapabilityQuery = &s;
        assert!(query.holds(CapabilityId::MEM_DMA));
        assert!(!query.holds(CapabilityId::NET_RAW));
        assert_eq!(
            query.holds(CapabilityId::MEM_DMA),
            s.contains(CapabilityId::MEM_DMA)
        );
    }

    fn parent() -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        s.insert(CapabilityId::FS_MOUNT);
        s.insert(CapabilityId::DRV_LOAD);
        s.insert(CapabilityId::AUDIT_READ);
        s
    }

    #[test]
    fn insert_remove_contains() {
        let mut s = CapabilitySet::empty();
        assert!(s.is_empty());
        s.insert(CapabilityId::NET_RAW);
        assert!(s.contains(CapabilityId::NET_RAW));
        assert!(!s.contains(CapabilityId::TIME_SET));
        assert_eq!(s.len(), 1);
        assert!(s.revoke(CapabilityId::NET_RAW));
        assert!(!s.revoke(CapabilityId::NET_RAW));
        assert!(s.is_empty());
    }

    #[test]
    fn delegation_accepts_subset() {
        let mut narrower = CapabilitySet::empty();
        narrower.insert(CapabilityId::FS_MOUNT);
        let delegated = parent().delegate(&narrower).expect("subset is allowed");
        assert!(delegated.contains(CapabilityId::FS_MOUNT));
        assert!(!delegated.contains(CapabilityId::DRV_LOAD));
        assert!(delegated.is_subset_of(&parent()));
    }

    #[test]
    fn delegation_rejects_widening() {
        let mut wider = parent();
        wider.insert(CapabilityId::USER_ADMIN);
        assert_eq!(parent().delegate(&wider), Err(Errno::DelegationWiden));
    }

    #[test]
    fn delegation_identity_is_allowed() {
        let p = parent();
        assert_eq!(p.delegate(&p), Ok(p));
    }

    #[test]
    fn delegation_invariant_holds_for_all_subsets() {
        // Exhaustive "property" test: every subset of the 8 well-known
        // capabilities is a legal delegation of the full set, and no
        // strict superset is.
        let universe: [CapabilityId; 8] = [
            CapabilityId::FS_MOUNT,
            CapabilityId::NET_RAW,
            CapabilityId::DRV_LOAD,
            CapabilityId::DRV_KERNEL,
            CapabilityId::USER_ADMIN,
            CapabilityId::TIME_SET,
            CapabilityId::IPC_BIND_PRIVILEGED,
            CapabilityId::AUDIT_READ,
        ];
        let mut full = CapabilitySet::empty();
        for cap in universe {
            full.insert(cap);
        }
        // Iterate over every one of the 2^8 subsets.
        for mask in 0u32..(1 << universe.len()) {
            let mut candidate = CapabilitySet::empty();
            for (i, cap) in universe.iter().enumerate() {
                if (mask >> i) & 1 == 1 {
                    candidate.insert(*cap);
                }
            }
            assert_eq!(full.delegate(&candidate), Ok(candidate));
            // Adding a capability outside `full` must always be rejected.
            let mut widened = candidate;
            widened.insert(CapabilityId::AUDIT_WRITE); // not in `universe`.
            if !full.contains(CapabilityId::AUDIT_WRITE) {
                assert_eq!(full.delegate(&widened), Err(Errno::DelegationWiden));
            }
        }
    }

    #[test]
    fn union_and_intersection_match_set_theory() {
        let mut a = CapabilitySet::empty();
        a.insert(CapabilityId::FS_MOUNT);
        a.insert(CapabilityId::NET_RAW);
        let mut b = CapabilitySet::empty();
        b.insert(CapabilityId::NET_RAW);
        b.insert(CapabilityId::DRV_LOAD);

        let u = a.union(&b);
        assert!(u.contains(CapabilityId::FS_MOUNT));
        assert!(u.contains(CapabilityId::NET_RAW));
        assert!(u.contains(CapabilityId::DRV_LOAD));
        assert_eq!(u.len(), 3);

        let i = a.intersection(&b);
        assert!(!i.contains(CapabilityId::FS_MOUNT));
        assert!(i.contains(CapabilityId::NET_RAW));
        assert!(!i.contains(CapabilityId::DRV_LOAD));
        assert_eq!(i.len(), 1);

        // Empty intersection.
        let disjoint = CapabilitySet::empty();
        assert!(a.intersection(&disjoint).is_empty());
        // Self-union is the identity.
        assert_eq!(a.union(&CapabilitySet::empty()), a);
    }

    #[test]
    fn into_iterator_borrow_matches_iter() {
        let set = parent();
        // `IntoIterator for &CapabilitySet` must produce the same sequence
        // as `CapabilitySet::iter`. Compare element-by-element without
        // allocating, since this crate is `no_std`.
        let mut a = set.iter();
        let mut b = (&set).into_iter();
        loop {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) => assert_eq!(x, y),
                (None, None) => break,
                _ => panic!("iter and IntoIterator disagree"),
            }
        }
    }

    #[test]
    fn wire_round_trip_preserves_the_set() {
        // The empty set, a typical multi-capability set, and a set with
        // a high bit index all survive an encode/decode round-trip.
        for set in [CapabilitySet::empty(), parent(), {
            let mut s = CapabilitySet::empty();
            s.insert(CapabilityId::AUDIT_WRITE);
            s.insert(CapabilityId::FS_MOUNT);
            s
        }] {
            let bytes = set.to_le_bytes();
            assert_eq!(bytes.len(), CapabilitySet::WIRE_LEN);
            assert_eq!(CapabilitySet::from_le_bytes(&bytes), Ok(set));
        }
    }

    #[test]
    fn wire_form_is_little_endian_words() {
        // `FS_MOUNT` is capability id 1, so it sets bit 1 of the first
        // little-endian word, landing in byte 0 with value 0b10.
        let mut s = CapabilitySet::empty();
        s.insert(CapabilityId::FS_MOUNT);
        assert_eq!(CapabilityId::FS_MOUNT.as_u16(), 1);
        let bytes = s.to_le_bytes();
        assert_eq!(bytes[0], 0b10);
        assert!(bytes[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn from_le_bytes_rejects_a_short_buffer() {
        let short = [0u8; CapabilitySet::WIRE_LEN - 1];
        assert_eq!(
            CapabilitySet::from_le_bytes(&short),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn from_le_bytes_ignores_trailing_bytes() {
        // A buffer longer than `WIRE_LEN` decodes from its first
        // `WIRE_LEN` bytes; trailing bytes are not consumed.
        let mut bytes = [0u8; CapabilitySet::WIRE_LEN + 8];
        bytes[CapabilitySet::WIRE_LEN] = 0xFF;
        assert_eq!(
            CapabilitySet::from_le_bytes(&bytes),
            Ok(CapabilitySet::empty())
        );
    }

    #[test]
    fn iteration_yields_each_capability_in_ascending_id_order() {
        let set = parent();
        let collected: [CapabilityId; 3] = {
            let mut it = set.iter();
            [it.next().unwrap(), it.next().unwrap(), it.next().unwrap()]
        };
        assert_eq!(collected[0], CapabilityId::FS_MOUNT);
        assert_eq!(collected[1], CapabilityId::DRV_LOAD);
        assert_eq!(collected[2], CapabilityId::AUDIT_READ);
        assert!(set.iter().nth(3).is_none());
    }
}
