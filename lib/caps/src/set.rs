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

use rustos_abi::{CapabilityId, Errno};
use rustos_collections::BitSet256;

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

/// Ascending iterator over the capabilities of a [`CapabilitySet`].
#[derive(Clone, Debug)]
pub struct CapabilitySetIter {
    inner: rustos_collections::BitSet256Iter,
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
    use rustos_abi::{CapabilityId, Errno};

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
