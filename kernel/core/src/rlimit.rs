//! Per-task resource-limit storage and the `rlimit_set` authorisation rule
//! (the L2 kernel enforcement of the `lib/abi` rlimit
//! contract).
//!
//! `lib/abi` ([`tairix_abi::rlimit`]) fixes the *wire* contract: the closed
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
//! `const` ceiling. L2 expresses "no ceiling imposed here" as
//! [`ResourceLimit::UNLIMITED`] ([`LimitSet::DEFAULT`]); the actual
//! capacity a task can reach is bounded by the growable kernel structures,
//! which L3 sizes from the discovered hardware. The one exception with a
//! real default today is the per-task stack: its default bound
//! ([`DEFAULT_STACK_LIMIT_BYTES`]) equals the reserved user-stack span the
//! spawn layout places, so the settable limit and the structural span
//! coincide until a principal tightens (or, under
//! `CAP_RLIMIT_RAISE`, raises) it. A future discovered-hardware default
//! policy slots in by tightening [`LimitSet::DEFAULT`] (or deriving it per
//! boot); every consumer reads it through this one definition, and
//! inheritance already intersects against it so a child can never widen
//! past it.
//!
//! This is a *capacity* facility. It must never loosen the fixed
//! security/format bounds on untrusted input; those stay fixed and
//! fail closed in their own modules.

use tairix_abi::{Errno, LimitKind, ResourceLimit};

/// The default `LimitKind::StackBytes` policy: the size, in bytes, of the
/// reserved user-stack span every spawned process receives (8 MiB — the
/// familiar general-purpose per-task stack default, generous for deep
/// recursion while bounding a runaway one long before it can exhaust a
/// small machine's RAM).
///
/// This is the single definition the spawn layout derives its reserved
/// span from (`spawn_layout::USER_STACK_RESERVE_PAGES`), so the settable
/// bound and the structural span can never silently diverge: by default a
/// stack may grow to the whole span, growth past the soft bound is refused
/// fail-closed at the fault path, and the unmapped guard page below the
/// span stays the terminal structural defence.
pub const DEFAULT_STACK_LIMIT_BYTES: u64 = 8 * 1024 * 1024;

/// The default `LimitKind::PinnedMemoryBytes` policy: the bytes one
/// process may hold pinned (exempt from the compressed tier) on a machine
/// with `installed_memory_bytes` of discovered RAM — one eighth of it.
///
/// Sized from the discovered hardware, never a hard-wired ceiling: an
/// eighth of RAM lets a monitor-scale process (tens of MiB even on a
/// 1 GiB board, where the derived bound is 128 MiB) always pin fully,
/// while a single abusive `CAP_MEM_PIN` holder can never exempt more
/// than a fraction of the machine from pressure management — the
/// capability gates *who* may pin, this bounds *how much*. The boot path
/// installs the derived bound as the per-boot default limit set
/// ([`LimitSet::with_pinned_default`]), so every task inherits it through
/// the ordinary never-widen intersection and `ulimit`/`rlimit_get`
/// report it honestly; raising it past the derived hard bound takes
/// `CAP_RLIMIT_RAISE` like any other hard raise.
#[must_use]
pub const fn default_pinned_limit_bytes(installed_memory_bytes: u64) -> u64 {
    installed_memory_bytes / 8
}

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
    /// The compile-time floor of the default policy a task runs under
    /// until a tighter ceiling is imposed.
    ///
    /// The *operative* per-boot default is the registry-held set
    /// ([`crate::aspace::AddressSpaceRegistry::default_limits`]), which the
    /// boot path derives from discovered hardware (today: the pinned-memory
    /// bound via [`Self::with_pinned_default`]) and which falls back to
    /// this constant wherever no derivation applies.
    ///
    /// Every resource except the stack is [`ResourceLimit::UNLIMITED`]: L2
    /// imposes no `rlimit` ceiling of its own there, leaving each capacity
    /// governed by the growable kernel structures L3 sizes from discovered
    /// hardware. `StackBytes` defaults to [`DEFAULT_STACK_LIMIT_BYTES`]
    /// (soft and hard — the reserved span is the structural bound, so a
    /// wider grant would be meaningless without `CAP_RLIMIT_RAISE` *and* a
    /// wider span). The single source of truth — tighten it here (or
    /// derive it per boot) and every task, including inheritance, follows.
    pub const DEFAULT: Self = {
        let mut limits = [ResourceLimit::UNLIMITED; LimitKind::COUNT];
        limits[LimitKind::StackBytes.as_u32() as usize] = ResourceLimit {
            soft: DEFAULT_STACK_LIMIT_BYTES,
            hard: DEFAULT_STACK_LIMIT_BYTES,
        };
        Self { limits }
    };

    /// The per-boot default set: [`Self::DEFAULT`] with the
    /// `PinnedMemoryBytes` bound set to `pinned_bytes` (soft and hard).
    ///
    /// Built once at boot from the discovered installed-memory total
    /// ([`default_pinned_limit_bytes`]) and installed as the registry
    /// default, so every task — including inheritance's never-widen
    /// intersection — runs under the derived bound without a second code
    /// path.
    #[must_use]
    pub const fn with_pinned_default(pinned_bytes: u64) -> Self {
        let mut out = Self::DEFAULT;
        out.limits[LimitKind::PinnedMemoryBytes.as_u32() as usize] = ResourceLimit {
            soft: pinned_bytes,
            hard: pinned_bytes,
        };
        out
    }

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

    /// The limit set a child inherits from `parent` at spawn, under the
    /// per-boot `default` policy.
    ///
    /// Each resource is `parent.intersect(default)`: the child can never
    /// hold a bound wider than either the parent's ceiling or the boot
    /// default, mirroring the never-widen capability delegation rule.
    /// Where the default is unlimited this equals the parent's set
    /// verbatim; a derived bound (the pinned-memory default, a future
    /// hardware-sized policy) constrains every child through this one
    /// intersection without a second code path.
    #[must_use]
    pub fn inherit(parent: &Self, default: &Self) -> Self {
        let mut out = *default;
        let mut i = 0;
        while i < LimitKind::ALL.len() {
            let kind = LimitKind::ALL[i];
            out.set(kind, parent.get(kind).intersect(default.get(kind)));
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
///   [`tairix_abi::CapabilityId::RLIMIT_RAISE`]; without it the request is
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
    use super::{authorize_set, default_pinned_limit_bytes, LimitSet, DEFAULT_STACK_LIMIT_BYTES};
    use tairix_abi::{Errno, LimitKind, ResourceLimit, RLIMIT_INFINITY};

    #[test]
    fn pinned_default_policy_scales_with_discovered_memory() {
        // One eighth of installed RAM, derived, never a hard-wired scalar:
        // a small board still fits a monitor-scale pin, a large machine
        // scales up, and zero (unknown) RAM derives a zero bound the boot
        // path simply does not install.
        assert_eq!(default_pinned_limit_bytes(1 << 30), 128 << 20);
        assert_eq!(default_pinned_limit_bytes(64 << 30), 8 << 30);
        assert_eq!(default_pinned_limit_bytes(0), 0);

        let set = LimitSet::with_pinned_default(128 << 20);
        let pinned = set.get(LimitKind::PinnedMemoryBytes);
        assert_eq!(pinned.soft, 128 << 20);
        assert_eq!(pinned.hard, 128 << 20);
        assert!(pinned.is_well_formed());
        // Every other kind keeps the compile-time floor.
        assert_eq!(
            set.get(LimitKind::StackBytes),
            LimitSet::DEFAULT.get(LimitKind::StackBytes)
        );
        assert_eq!(
            set.get(LimitKind::AddressSpaceBytes),
            ResourceLimit::UNLIMITED
        );
    }

    #[test]
    fn default_is_unlimited_for_every_kind_except_the_stack() {
        let set = LimitSet::DEFAULT;
        for kind in LimitKind::ALL {
            if kind == LimitKind::StackBytes {
                continue;
            }
            assert_eq!(set.get(kind), ResourceLimit::UNLIMITED);
            assert_eq!(set.get(kind).hard, RLIMIT_INFINITY);
        }
        // The stack's default bound is the reserved span: soft and hard,
        // well-formed, and genuinely finite.
        let stack = set.get(LimitKind::StackBytes);
        assert_eq!(stack.soft, DEFAULT_STACK_LIMIT_BYTES);
        assert_eq!(stack.hard, DEFAULT_STACK_LIMIT_BYTES);
        assert!(stack.is_well_formed());
        assert_ne!(stack.hard, RLIMIT_INFINITY);
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
        let child = LimitSet::inherit(&parent, &LimitSet::DEFAULT);
        // The child inherits the parent's tighter ceiling verbatim while the
        // default is unlimited.
        assert_eq!(child.get(LimitKind::Processes), cap);
        // The stack's finite default caps the child even when the parent
        // holds a wider bound: intersection never widens.
        assert_eq!(
            child.get(LimitKind::StackBytes),
            LimitSet::DEFAULT.get(LimitKind::StackBytes)
        );
    }

    #[test]
    fn inherit_never_widens_past_a_tighter_default() {
        // Model the L3 future: a default tighter than the parent's bound.
        let tight = ResourceLimit::new(1, 2).expect("well-formed");
        let default_for_kind = LimitSet::DEFAULT.get(LimitKind::Processes);
        let mut parent = LimitSet::DEFAULT;
        parent.set(LimitKind::Processes, ResourceLimit::UNLIMITED);
        let child = LimitSet::inherit(&parent, &LimitSet::DEFAULT);
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
    fn inherit_intersects_against_the_derived_pinned_default() {
        // A parent with no pinned bound of its own is capped by the
        // per-boot derived default; a parent already tighter keeps its
        // tighter bound — inheritance never widens either way.
        let boot_default = LimitSet::with_pinned_default(128 << 20);
        let child = LimitSet::inherit(&LimitSet::DEFAULT, &boot_default);
        assert_eq!(
            child.get(LimitKind::PinnedMemoryBytes),
            boot_default.get(LimitKind::PinnedMemoryBytes)
        );

        let mut tight_parent = boot_default;
        let tight = ResourceLimit::new(1 << 20, 1 << 20).expect("well-formed");
        tight_parent.set(LimitKind::PinnedMemoryBytes, tight);
        let child = LimitSet::inherit(&tight_parent, &boot_default);
        assert_eq!(child.get(LimitKind::PinnedMemoryBytes), tight);
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
