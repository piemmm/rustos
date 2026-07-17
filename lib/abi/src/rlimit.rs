//! Resource-limit ABI.
//!
//! TAIRiX sizes resource *capacities* from the hardware discovered at boot
//! and grows them on demand; it never hard-wires a `const` ceiling that caps
//! a large machine or wastes a small one. On top of those
//! discovered defaults a principal may *impose* a lower ceiling on itself or
//! its children — the TAIRiX equivalent of POSIX `ulimit`/`rlimit`.
//! This module fixes the *contract* for that facility: the closed set of
//! limited resources ([`LimitKind`]), the soft/hard pair carried for each
//! ([`ResourceLimit`]), and its little-endian wire encoding. The kernel-side
//! enforcement (per-task storage, inheritance, intersection on delegation,
//! and the [`crate::CapabilityId::RLIMIT_RAISE`] gate on raising a hard
//! bound) lives in `kernel/core`; the `rlimit_get` / `rlimit_set` syscalls
//! ([`crate::SyscallNumber::RLIMIT_GET`], [`crate::SyscallNumber::RLIMIT_SET`])
//! carry these values across the boundary.
//!
//! # The contract
//!
//! Each limited resource has a soft and a hard bound. A process may lower
//! its own soft bound freely and may lower its hard bound freely, but
//! *raising* a hard bound — or setting any bound above the inherited
//! ceiling — requires [`crate::CapabilityId::RLIMIT_RAISE`]. Limits
//! are inherited across spawn and are *intersected*, never widened, on
//! delegation ([`ResourceLimit::intersect`]); this mirrors the capability
//! delegation rule.
//!
//! This is a *capacity* limit facility. It must never be used to loosen the
//! fixed security/format *bounds* on untrusted input — those stay
//! fixed and fail closed.

use crate::le::{put_u64, read_u64};
use crate::Errno;

/// A bound value meaning "no limit imposed".
///
/// The largest `u64`; a soft or hard bound set to this leaves the resource
/// governed only by the discovered, growable default policy. It is
/// the resource-limit analogue of POSIX `RLIM_INFINITY`.
pub const RLIMIT_INFINITY: u64 = u64::MAX;

/// The closed, versioned set of resources a [`ResourceLimit`] can govern.
///
/// The discriminants are part of the `abi-v1` contract: an existing variant
/// may not be renumbered or removed, and a new resource takes the next free
/// discriminant and bumps [`LimitKind::COUNT`]. The set is deliberately
/// small — only resources whose *capacity* a principal may legitimately cap
/// below the system default appear here; fixed security/format bounds
/// never do.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum LimitKind {
    /// Maximum total bytes of anonymous memory the process may map into its
    /// own address space (the `mem_map` capacity, `plans/SPAWN.md` SP5).
    AddressSpaceBytes = 0,
    /// Maximum number of open standard-stream descriptors the process may
    /// hold (the descriptor-table capacity).
    OpenStreams = 1,
    /// Maximum number of live child processes the process may have spawned
    /// (the `spawn` fan-out capacity, `plans/SPAWN.md` SP3).
    Processes = 2,
    /// Maximum size, in bytes, of a single task's stack (the per-task stack
    /// capacity).
    StackBytes = 3,
    /// Maximum bytes of anonymous memory the process may hold pinned —
    /// exempt from the compressed `ramzip` tier and any future lower swap
    /// tier — through the `mem_pin` syscall (`plans/STRESSTEST.md` ST2).
    ///
    /// The bound caps the whole pinned footprint (mapped anonymous bytes
    /// plus committed stack): a `mem_pin` whose current footprint exceeds
    /// the soft bound fails closed, and while pinned the same bound caps
    /// further anonymous growth, so an abusive pin cannot exempt
    /// unbounded memory from pressure management.
    PinnedMemoryBytes = 4,
}

impl LimitKind {
    /// Number of distinct [`LimitKind`] variants assigned in `abi-v1`.
    ///
    /// Equals one past the largest discriminant; a per-task limit array is
    /// sized by this constant so adding a variant grows the storage in step.
    pub const COUNT: usize = 5;

    /// Every [`LimitKind`] in discriminant order.
    ///
    /// Lets callers iterate the closed set without an index-to-discriminant
    /// cast; its length is [`COUNT`](Self::COUNT), so adding a variant
    /// without extending this array fails to compile.
    pub const ALL: [Self; Self::COUNT] = [
        Self::AddressSpaceBytes,
        Self::OpenStreams,
        Self::Processes,
        Self::StackBytes,
        Self::PinnedMemoryBytes,
    ];

    /// Every [`LimitKind`] in discriminant order, paired with its canonical
    /// `ulimit` name.
    ///
    /// Single source of truth for [`name`](Self::name), [`from_name`](Self::from_name),
    /// and [`from_u32`](Self::from_u32), so the spellings
    /// and the discriminant↔name mapping can never disagree. The names are
    /// part of the frozen `abi-v1` contract.
    const NAMED: [(Self, &'static str); Self::COUNT] = [
        (Self::AddressSpaceBytes, "address-space-bytes"),
        (Self::OpenStreams, "open-streams"),
        (Self::Processes, "processes"),
        (Self::StackBytes, "stack-bytes"),
        (Self::PinnedMemoryBytes, "pinned-memory-bytes"),
    ];

    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Decode a raw discriminant, failing closed on an unassigned value.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] if `raw` is not an `abi-v1`
    /// [`LimitKind`] discriminant (validate every input,
    /// fail closed).
    pub const fn from_u32(raw: u32) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::AddressSpaceBytes),
            1 => Ok(Self::OpenStreams),
            2 => Ok(Self::Processes),
            3 => Ok(Self::StackBytes),
            4 => Ok(Self::PinnedMemoryBytes),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Canonical `ulimit` name of this resource.
    ///
    /// The exact spelling [`from_name`](Self::from_name) accepts, so a name
    /// round-trips back to the same kind.
    #[must_use]
    pub fn name(self) -> &'static str {
        // Every variant is present in `NAMED`, so the lookup always
        // succeeds; the fallback never executes but keeps the function
        // total without a panic.
        Self::NAMED
            .iter()
            .find(|(kind, _)| *kind == self)
            .map_or("", |(_, name)| *name)
    }

    /// The [`LimitKind`] with canonical `ulimit` name `name`, or [`None`].
    ///
    /// The match is exact and case-sensitive; an unknown name denotes
    /// nothing (fail closed).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::NAMED
            .iter()
            .find(|(_, candidate)| *candidate == name)
            .map(|(kind, _)| *kind)
    }
}

/// A soft/hard resource-limit pair as carried across the ABI.
///
/// `soft <= hard` is the well-formedness invariant ([`is_well_formed`]);
/// [`RLIMIT_INFINITY`] in either field means "no limit". The struct is
/// `#[repr(C)]` so a non-Rust program sees the same two little-endian
/// `uint64_t` fields the kernel exchanges.
///
/// [`is_well_formed`]: ResourceLimit::is_well_formed
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ResourceLimit {
    /// The soft bound: the effective ceiling the kernel enforces. A process
    /// may lower it freely; raising it (up to `hard`) needs no capability.
    pub soft: u64,
    /// The hard bound: the ceiling `soft` may not exceed. Lowering it is
    /// free; raising it requires [`crate::CapabilityId::RLIMIT_RAISE`].
    pub hard: u64,
}

impl ResourceLimit {
    /// Length, in bytes, of the little-endian wire encoding.
    pub const WIRE_LEN: usize = 16;

    /// A limit with both bounds [`RLIMIT_INFINITY`] (no ceiling imposed).
    pub const UNLIMITED: Self = Self {
        soft: RLIMIT_INFINITY,
        hard: RLIMIT_INFINITY,
    };

    /// Construct a limit, validating `soft <= hard`.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] if `soft > hard` — a soft bound above
    /// its hard bound is meaningless and is rejected before it can be stored
    /// (validate every input, fail closed).
    pub const fn new(soft: u64, hard: u64) -> Result<Self, Errno> {
        if soft > hard {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { soft, hard })
    }

    /// Whether `soft <= hard`.
    #[must_use]
    pub const fn is_well_formed(self) -> bool {
        self.soft <= self.hard
    }

    /// The tighter of two limits: the minimum of the soft bounds and the
    /// minimum of the hard bounds.
    ///
    /// Used to inherit/delegate a limit without ever *widening* it: a child
    /// or delegate receives `self.intersect(parent)`, so neither bound can
    /// exceed the inherited ceiling (mirroring the capability
    /// delegation rule). The result is well-formed whenever both
    /// inputs are.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        let soft = if self.soft < other.soft {
            self.soft
        } else {
            other.soft
        };
        let hard = if self.hard < other.hard {
            self.hard
        } else {
            other.hard
        };
        Self { soft, hard }
    }

    /// Serialise into a fixed [`WIRE_LEN`](Self::WIRE_LEN)-byte buffer.
    #[must_use]
    pub fn encode(self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.soft);
        put_u64(&mut out, 8, self.hard);
        out
    }

    /// Decode from a little-endian byte slice.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `bytes` is shorter than
    ///   [`WIRE_LEN`](Self::WIRE_LEN).
    /// * [`Errno::OutOfRange`] if the decoded pair is not well-formed
    ///   (`soft > hard`).
    ///
    /// Both failures are fail-closed: a malformed buffer never yields a
    /// usable limit.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let soft = read_u64(bytes, 0);
        let hard = read_u64(bytes, 8);
        Self::new(soft, hard)
    }
}

#[cfg(test)]
mod tests {
    use super::{LimitKind, ResourceLimit, RLIMIT_INFINITY};
    use crate::Errno;

    #[test]
    fn limit_kind_discriminants_are_frozen() {
        // The numeric discriminants are part of abi-v1; do not renumber.
        assert_eq!(LimitKind::AddressSpaceBytes.as_u32(), 0);
        assert_eq!(LimitKind::OpenStreams.as_u32(), 1);
        assert_eq!(LimitKind::Processes.as_u32(), 2);
        assert_eq!(LimitKind::StackBytes.as_u32(), 3);
        assert_eq!(LimitKind::PinnedMemoryBytes.as_u32(), 4);
        assert_eq!(LimitKind::COUNT, 5);
    }

    #[test]
    fn from_u32_round_trips_and_fails_closed() {
        // `ALL` is dense and in discriminant order: index i maps to raw i.
        for (i, kind) in LimitKind::ALL.iter().enumerate() {
            let raw = u32::try_from(i).expect("small index");
            assert_eq!(kind.as_u32(), raw);
            assert_eq!(LimitKind::from_u32(raw), Ok(*kind));
        }
        let past = u32::try_from(LimitKind::COUNT).expect("small count");
        assert_eq!(LimitKind::from_u32(past), Err(Errno::OutOfRange));
        assert_eq!(LimitKind::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn names_are_frozen_and_round_trip() {
        assert_eq!(LimitKind::Processes.name(), "processes");
        for kind in LimitKind::ALL {
            assert_eq!(LimitKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(LimitKind::from_name("nope"), None);
        assert_eq!(LimitKind::from_name("Processes"), None);
        assert_eq!(LimitKind::from_name(""), None);
    }

    #[test]
    fn new_rejects_soft_above_hard() {
        assert_eq!(ResourceLimit::new(10, 5), Err(Errno::OutOfRange));
        assert!(ResourceLimit::new(5, 10).is_ok());
        assert!(ResourceLimit::new(7, 7).is_ok());
    }

    #[test]
    fn unlimited_is_infinity_in_both_bounds() {
        assert_eq!(ResourceLimit::UNLIMITED.soft, RLIMIT_INFINITY);
        assert_eq!(ResourceLimit::UNLIMITED.hard, RLIMIT_INFINITY);
        assert!(ResourceLimit::UNLIMITED.is_well_formed());
    }

    #[test]
    fn intersect_never_widens() {
        let a = ResourceLimit::new(100, 200).expect("well-formed");
        let b = ResourceLimit::new(50, 300).expect("well-formed");
        let i = a.intersect(b);
        assert_eq!(i.soft, 50);
        assert_eq!(i.hard, 200);
        assert!(i.is_well_formed());
        // Intersection is commutative and idempotent.
        assert_eq!(i, b.intersect(a));
        assert_eq!(a.intersect(a), a);
        // Intersecting with UNLIMITED leaves the tighter limit unchanged.
        assert_eq!(a.intersect(ResourceLimit::UNLIMITED), a);
    }

    #[test]
    fn encode_decode_round_trips() {
        let limit = ResourceLimit::new(0x1234, 0x5678_9ABC).expect("well-formed");
        let bytes = limit.encode();
        assert_eq!(bytes.len(), ResourceLimit::WIRE_LEN);
        assert_eq!(ResourceLimit::decode(&bytes), Ok(limit));
    }

    #[test]
    fn decode_fails_closed_on_short_buffer() {
        let short = [0u8; ResourceLimit::WIRE_LEN - 1];
        assert_eq!(ResourceLimit::decode(&short), Err(Errno::LengthOutOfRange));
        assert_eq!(ResourceLimit::decode(&[]), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn decode_fails_closed_on_malformed_pair() {
        // soft (10) > hard (5): a hand-built buffer the kernel must reject.
        let mut bytes = [0u8; ResourceLimit::WIRE_LEN];
        bytes[0] = 10;
        bytes[8] = 5;
        assert_eq!(ResourceLimit::decode(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn decode_ignores_trailing_bytes() {
        // A longer buffer is accepted; only the first WIRE_LEN bytes matter.
        let limit = ResourceLimit::new(1, 2).expect("well-formed");
        let mut bytes = [0xFFu8; ResourceLimit::WIRE_LEN + 4];
        bytes[..ResourceLimit::WIRE_LEN].copy_from_slice(&limit.encode());
        assert_eq!(ResourceLimit::decode(&bytes), Ok(limit));
    }
}
