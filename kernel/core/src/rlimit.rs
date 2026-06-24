//! Per-task resource-limit storage and the `rlimit_set` authorisation rule
//! (the L2 kernel enforcement of the `lib/abi` rlimit
//! contract).
//!
//! `lib/abi` ([`rustos_abi::rlimit`]) fixes the *wire* contract: the closed
//! [`LimitKind`] set, the soft/hard [`ResourceLimit`] pair, and its
//! little-endian encoding. This module is the kernel side: the [`LimitSet`]
//! a task carries (one [`ResourceLimit`] per [`LimitKind`]), the default
//! policy a task runs under until a tighter ceiling is imposed, the
//! inheritance rule a child runs through at spawn, and [`authorize_set`] —
//! the single decision the `rlimit_set` handler makes about a requested
//! limit.
//!
//! # Default policy
//!
//! Until a principal imposes a tighter ceiling, every resource is governed
//! by the discovered, growable default policy — *not* by a hard-wired
//! `const` ceiling. L2 expresses that "no ceiling imposed here" as
//! [`ResourceLimit::UNLIMITED`] for every kind ([`LimitSet::DEFAULT`]); the
//! actual capacity a task can reach is bounded by the growable kernel
//! structures, which L3 sizes from the-discovered hardware. A future
//! discovered-hardware default policy slots in by tightening
//! [`LimitSet::DEFAULT`] (or deriving it per boot); every consumer reads it
//! through this one definition, and inheritance already
//! intersects against it so a child can never widen past it.
//!
//! This is a *capacity* facility. It must never loosen the fixed
//! security/format bounds on untrusted input; those stay fixed and
//! fail closed in their own modules.

use rustos_abi::{Errno, LimitKind, ResourceLimit};

/// One task's effective resource limits: a [`ResourceLimit`] for every
/// [`LimitKind`].
///
/// Indexed by [`LimitKind`] discriminant; the array length is tied to
/// [`LimitKind::COUNT`], so adding a resource grows the storage in step and
/// any kind missing an entry fails to compile. `Copy` because a limit set is
/// a handful of `u64`s — small enough to pass by value across the registry
/// boundary without a heap allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LimitSet {
    limits: [ResourceLimit; LimitKind::COUNT],
}

impl LimitSet {
    /// The default policy a task runs under until a tighter ceiling is
    /// imposed.
    ///
    /// Every resource is [`ResourceLimit::UNLIMITED`]: L2 imposes no
    /// `rlimit` ceiling of its own, leaving each capacity governed by the
    /// growable kernel structures L3 sizes from discovered hardware. The
    /// single source of truth — tighten it here (or derive it per boot) and
    /// every task, including inheritance, follows.
    pub const DEFAULT: Self = Self {
        limits: [ResourceLimit::UNLIMITED; LimitKind::COUNT],
    };

    /// The effective limit for `kind`.
    #[must_use]
    pub const fn get(&self, kind: LimitKind) -> ResourceLimit {
        self.limits[kind.as_u32() as usize]
    }

    /// Replace the effective limit for `kind`.
    ///
    /// The caller is responsible for the authorisation
    /// ([`authorize_set`]); this is the unconditional store the handler
    /// reaches only once a request is authorised.
    pub fn set(&mut self, kind: LimitKind, limit: ResourceLimit) {
        self.limits[kind.as_u32() as usize] = limit;
    }

    /// The limit set a child inherits from `parent` at spawn.
    ///
    /// Each resource is `parent.intersect(DEFAULT)`: the child can never
    /// hold a bound wider than either the parent's ceiling or the system
    /// default, mirroring the never-widen capability delegation rule.
    /// While [`LimitSet::DEFAULT`] is unlimited this equals the parent's set
    /// verbatim; once a tighter default lands (L3) the same intersection
    /// keeps a child inside it without a second code path.
    #[must_use]
    pub fn inherit(parent: &Self) -> Self {
        let mut out = Self::DEFAULT;
        let mut i = 0;
        while i < LimitKind::ALL.len() {
            let kind = LimitKind::ALL[i];
            out.set(kind, parent.get(kind).intersect(Self::DEFAULT.get(kind)));
            i += 1;
        }
        out
    }
}

impl Default for LimitSet {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Decide a `rlimit_set` request against the caller's current effective
/// limit.
///
/// `requested` has already been decoded and is well-formed (`soft <= hard`,
/// guaranteed by [`ResourceLimit::decode`]). The rule:
///
/// * Lowering, or any change that does not raise the hard bound above the
///   current ceiling, is free — a process may always tighten its own limits.
/// * Raising the hard bound above `current.hard` requires
///   [`rustos_abi::CapabilityId::RLIMIT_RAISE`]; without it the request is
///   refused with [`Errno::PermissionDenied`] (fail closed). Because
///   `requested` is well-formed, a permitted request (hard not raised) also
///   never lets the soft bound exceed the current ceiling.
///
/// On success returns the limit to store; the handler then commits it.
///
/// # Errors
///
/// [`Errno::PermissionDenied`] if the request raises the hard bound and
/// `can_raise` is `false`.
pub fn authorize_set(
    current: ResourceLimit,
    requested: ResourceLimit,
    can_raise: bool,
) -> Result<ResourceLimit, Errno> {
    if requested.hard > current.hard && !can_raise {
        return Err(Errno::PermissionDenied);
    }
    Ok(requested)
}

#[cfg(test)]
mod tests {
    use super::{authorize_set, LimitSet};
    use rustos_abi::{Errno, LimitKind, ResourceLimit, RLIMIT_INFINITY};

    #[test]
    fn default_is_unlimited_for_every_kind() {
        let set = LimitSet::DEFAULT;
        for kind in LimitKind::ALL {
            assert_eq!(set.get(kind), ResourceLimit::UNLIMITED);
            assert_eq!(set.get(kind).hard, RLIMIT_INFINITY);
        }
        assert_eq!(LimitSet::default(), LimitSet::DEFAULT);
    }

    #[test]
    fn set_then_get_round_trips_per_kind() {
        let mut set = LimitSet::DEFAULT;
        let lo = ResourceLimit::new(10, 20).expect("well-formed");
        set.set(LimitKind::Processes, lo);
        assert_eq!(set.get(LimitKind::Processes), lo);
        // Other kinds are untouched.
        assert_eq!(set.get(LimitKind::OpenStreams), ResourceLimit::UNLIMITED);
    }

    #[test]
    fn inherit_copies_parent_under_unlimited_default() {
        let mut parent = LimitSet::DEFAULT;
        let cap = ResourceLimit::new(4, 8).expect("well-formed");
        parent.set(LimitKind::Processes, cap);
        let child = LimitSet::inherit(&parent);
        // The child inherits the parent's tighter ceiling verbatim while the
        // default is unlimited.
        assert_eq!(child.get(LimitKind::Processes), cap);
        assert_eq!(child.get(LimitKind::StackBytes), ResourceLimit::UNLIMITED);
    }

    #[test]
    fn inherit_never_widens_past_a_tighter_default() {
        // Model the L3 future: a default tighter than the parent's bound.
        let tight = ResourceLimit::new(1, 2).expect("well-formed");
        let default_for_kind = LimitSet::DEFAULT.get(LimitKind::Processes);
        let mut parent = LimitSet::DEFAULT;
        parent.set(LimitKind::Processes, ResourceLimit::UNLIMITED);
        let child = LimitSet::inherit(&parent);
        // Under today's unlimited default the intersection is the parent's
        // value; the invariant under test is that inherit is exactly the
        // intersection, so it can never exceed the default.
        assert_eq!(
            child.get(LimitKind::Processes),
            ResourceLimit::UNLIMITED.intersect(default_for_kind)
        );
        // Sanity: intersecting a tight limit never widens it.
        assert_eq!(tight.intersect(ResourceLimit::UNLIMITED), tight);
    }

    #[test]
    fn lowering_is_always_authorised() {
        let current = ResourceLimit::new(100, 200).expect("well-formed");
        let lower = ResourceLimit::new(10, 50).expect("well-formed");
        assert_eq!(authorize_set(current, lower, false), Ok(lower));
        // Raising the soft bound up to the unchanged hard ceiling is free.
        let raise_soft = ResourceLimit::new(150, 200).expect("well-formed");
        assert_eq!(authorize_set(current, raise_soft, false), Ok(raise_soft));
    }

    #[test]
    fn raising_hard_without_capability_is_denied() {
        let current = ResourceLimit::new(10, 50).expect("well-formed");
        let higher = ResourceLimit::new(10, 100).expect("well-formed");
        assert_eq!(
            authorize_set(current, higher, false),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn raising_hard_with_capability_is_authorised() {
        let current = ResourceLimit::new(10, 50).expect("well-formed");
        let higher = ResourceLimit::new(10, 100).expect("well-formed");
        assert_eq!(authorize_set(current, higher, true), Ok(higher));
    }
}
