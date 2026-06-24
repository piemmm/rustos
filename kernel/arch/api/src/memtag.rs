//! Hardware memory-tagging surface of the Arch HAL.
//!
//! Use-after-free and a class of buffer over-runs are *temporal* and
//! *spatial* memory-safety bugs that hardware **memory tagging** turns
//! into a deterministic fault: every aligned granule of memory carries a
//! small tag, every pointer carries a matching tag, and a load or store
//! whose pointer tag does not match the granule tag faults. Re-tagging a
//! region on free (so a stale pointer keeps the *old* tag) is what
//! hardens use-after-free: the next access through the dangling pointer
//! mismatches and faults instead of silently reading or corrupting the
//! reallocated object.
//!
//! Only the architecture port can drive the tag-storage and tag-check
//! silicon (Arm MTE, SPARC ADI, the RISC-V tagging proposals), so
//! makes this a closed trait set on the Arch HAL; this module is that
//! set, modelled on the [`super::sidechannel`] surface.
//!
//! # What lives here
//!
//! * [`MemoryTagging`] — the per-port handle the kernel reaches through.
//!   It exposes the tag granule geometry, the architecture-neutral
//!   [`MemTag`] algebra used to rotate tags on (re)allocation, and a
//!   capability-checked region-retag primitive.
//! * [`TaggingProfile`] / [`Tagging`] — the honest declaration, exactly
//!   like [`super::sidechannel::MitigationProfile`]: a feature is
//!   [`Tagging::Supported`], [`Tagging::Unsupported`] (with a
//!   justification — the silicon genuinely lacks tagging), or
//!   [`Tagging::Pending`] (the silicon supports it but a not-yet-landed
//!   subsystem must wire it up).
//! * [`next_free_tag`] — the architecture-neutral tag rotation. It is
//!   pure, `const`, and shared by every consumer (the software
//!   tag-checking allocator in `kernel/mem` and the hardware ports) so
//!   the tag chosen for a re-allocation is guaranteed to differ from the
//!   tag a stale pointer still holds.: one definition,
//!   no duplication.
//! * [`conformance`] — the conformance vertical: every port runs
//!   [`conformance::run_all`] against its handle.

/// A memory tag: the small value stamped on both a pointer and the
/// granules it may legally address.
///
/// Hardware tags are narrow — Arm MTE uses 4 bits (16 values). The
/// architecture-neutral software tag-checking allocator uses the same
/// width so its behaviour matches what the hardware will enforce. The
/// stored value is always in `0..`[`TAG_COUNT`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct MemTag(u8);

/// Number of distinct tag values in the architecture-neutral tag space.
///
/// 16 mirrors Arm MTE's 4-bit tag. A port whose silicon offers a
/// different width still reports this neutral count to the allocator;
/// the port narrows to its hardware width internally.
pub const TAG_COUNT: u8 = 16;

impl MemTag {
    /// The tag a freshly-zeroed slot starts life with, before its first
    /// allocation rotates it (`MemTag(0)`).
    pub const INITIAL: MemTag = MemTag(0);

    /// Construct a tag from a raw value, wrapping into `0..`[`TAG_COUNT`].
    ///
    /// Wrapping (rather than rejecting) keeps the constructor total and
    /// `const`; an out-of-range input is folded, never a panic.
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value % TAG_COUNT)
    }

    /// The raw tag value, always in `0..`[`TAG_COUNT`].
    #[must_use]
    pub const fn value(self) -> u8 {
        self.0
    }
}

/// Choose a fresh tag for a (re)allocation that is guaranteed to differ
/// from `previous` whenever `tag_count >= 2`.
///
/// This is the core use-after-free hardening primitive: a region's tag
/// is rotated on every (re)allocation, so a pointer captured before the
/// region was freed keeps the *old* tag and mismatches the new one.
///
/// `tag_count` is clamped to `1..=`[`TAG_COUNT`]. A `tag_count` of `0`
/// or `1` describes an untagged port; the function then returns
/// [`MemTag::INITIAL`] (there is no second value to rotate to), and the
/// caller relies on its other defences (guard pages, W^X) rather than
/// tagging.
#[must_use]
pub const fn next_free_tag(previous: MemTag, tag_count: u8) -> MemTag {
    let count = if tag_count == 0 {
        1
    } else if tag_count > TAG_COUNT {
        TAG_COUNT
    } else {
        tag_count
    };
    if count < 2 {
        return MemTag::INITIAL;
    }
    MemTag((previous.0 + 1) % count)
}

/// One memory-tagging feature's status on a given port.
///
/// Mirrors [`super::sidechannel::Mitigation`]: a port takes exactly one
/// honest position per feature. [`Tagging::Unsupported`] is permitted
/// **only** where the silicon genuinely lacks the feature, and the
/// payload must record why (so the conformance suite can refuse an
/// unjustified claim). [`Tagging::Pending`] is for silicon that *does*
/// support tagging but where a not-yet-landed subsystem (the Stage 6
/// page-table `Tagged` attribute and the tag-check fault decode) must
/// wire it up; a `Pending` feature is honest and tracked but is not
/// release-ready.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Tagging {
    /// The port drives this tagging feature on its silicon.
    Supported,
    /// The port's silicon does not provide this feature. The payload is
    /// the justification recorded in the port's `README.md`; it must be
    /// non-empty.
    Unsupported(&'static str),
    /// The silicon supports this feature, but it cannot be enabled yet
    /// because it depends on a subsystem that has not landed. The
    /// payload is the tracking note (the `PLAN.md` stage/item that will
    /// deliver it); it must be non-empty.
    Pending(&'static str),
}

impl Tagging {
    /// `true` if this feature is [`Tagging::Supported`].
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    /// `true` if this feature is a tracked [`Tagging::Pending`].
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending(_))
    }

    /// `true` if this feature is release-ready: it is either supported or
    /// a justified [`Tagging::Unsupported`]. A [`Tagging::Pending`]
    /// feature is not release-ready.
    #[must_use]
    pub const fn is_release_ready(self) -> bool {
        matches!(self, Self::Supported | Self::Unsupported(_))
    }

    /// The explanatory note for a non-supported decision, or `None` when
    /// supported.
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::Supported => None,
            Self::Unsupported(reason) | Self::Pending(reason) => Some(reason),
        }
    }
}

/// A port's honest declaration of the tagging features it drives.
///
/// Two genuinely distinct properties, so two slots (no slot exists that the kernel does not need):
///
/// * [`Self::tag_storage`] — the CPU can store and read a per-granule
///   tag (Arm MTE `STG`/`LDG`, the [`MemoryTagging::set_region_tag`]
///   primitive). This is what lets the allocator stamp a region.
/// * [`Self::tag_check_faults`] — a load or store whose pointer tag does
///   not match the granule tag *faults*. This is the property that
///   actually catches a use-after-free at the hardware level; without
///   it, stored tags are inert.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TaggingProfile {
    /// Per-granule tag storage is available (e.g. Arm MTE `STG`/`LDG`).
    pub tag_storage: Tagging,
    /// A pointer/granule tag mismatch faults on access.
    pub tag_check_faults: Tagging,
}

/// A single named slot of a [`TaggingProfile`], yielded by
/// [`TaggingProfile::entries`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TaggingEntry {
    /// Stable, human-readable name of the slot.
    pub name: &'static str,
    /// The port's decision for this slot.
    pub tagging: Tagging,
}

/// Reason a [`TaggingProfile`] failed [`TaggingProfile::validate`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProfileError {
    /// A non-supported decision carried an empty (or whitespace-only)
    /// justification. The charter requires every omission to be justified;
    /// `field` names the offending slot.
    EmptyJustification {
        /// The [`TaggingEntry::name`] of the unjustified slot.
        field: &'static str,
    },
}

impl TaggingProfile {
    /// The two tagging slots, in a stable order, each paired with
    /// its name.
    #[must_use]
    pub const fn entries(&self) -> [TaggingEntry; 2] {
        [
            TaggingEntry {
                name: "tag_storage",
                tagging: self.tag_storage,
            },
            TaggingEntry {
                name: "tag_check_faults",
                tagging: self.tag_check_faults,
            },
        ]
    }

    /// Validate the honesty rule: every non-supported feature
    /// must carry a non-empty explanation — a justification for a
    /// [`Tagging::Unsupported`] claim or a tracking note for a
    /// [`Tagging::Pending`] gap.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::EmptyJustification`] naming the first slot
    /// whose [`Tagging::detail`] is present but empty or whitespace-only.
    pub fn validate(&self) -> Result<(), ProfileError> {
        for entry in self.entries() {
            if let Some(reason) = entry.tagging.detail() {
                if reason.trim().is_empty() {
                    return Err(ProfileError::EmptyJustification { field: entry.name });
                }
            }
        }
        Ok(())
    }

    /// `true` if every feature is release-ready — supported or a
    /// justified [`Tagging::Unsupported`], with no [`Tagging::Pending`]
    /// gap remaining.
    #[must_use]
    pub fn is_release_ready(&self) -> bool {
        self.entries()
            .iter()
            .all(|entry| entry.tagging.is_release_ready())
    }

    /// `true` if the port enforces use-after-free hardening *in
    /// hardware*: it both stores tags and faults on a mismatch.
    #[must_use]
    pub fn enforces_uaf_in_hardware(&self) -> bool {
        self.tag_storage.is_supported() && self.tag_check_faults.is_supported()
    }
}

/// The memory-tagging handle an architecture port exposes.
///
/// The kernel allocator rotates tags through [`Self::rotate_tag`] (pure,
/// architecture-neutral) and, on a port with hardware tag storage,
/// stamps the chosen tag onto a region with [`Self::set_region_tag`].
/// On a port without tag storage the latter is a no-op and the kernel
/// relies on the architecture-neutral *software* tag check in
/// `kernel/mem` plus its guard pages.
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// handle from every CPU.
pub trait MemoryTagging: Send + Sync {
    /// The port's honest declaration of which features it drives.
    /// Must satisfy [`TaggingProfile::validate`].
    fn profile(&self) -> TaggingProfile;

    /// Size in bytes of a tag granule — the span of memory a single tag
    /// covers. Arm MTE uses 16. An untagged port reports `1` (every byte
    /// is its own trivially-tagged granule). Must be a power of two and
    /// at least `1`.
    fn granule_bytes(&self) -> usize;

    /// Number of distinct tag values the allocator should rotate
    /// through. A tagged port reports its hardware width (e.g. `16`);
    /// an untagged port reports `1`. Must be in `1..=`[`TAG_COUNT`].
    fn tag_count(&self) -> u8;

    /// Choose a fresh tag for a (re)allocation that differs from
    /// `previous` (the use-after-free hardening rotation).
    ///
    /// Provided in terms of [`next_free_tag`] and [`Self::tag_count`] so
    /// every port shares the one rotation; a port has no reason to
    /// override it.
    fn rotate_tag(&self, previous: MemTag) -> MemTag {
        next_free_tag(previous, self.tag_count())
    }

    /// Stamp every granule of `base .. base + len` with `tag`.
    ///
    /// On a port with [`TaggingProfile::tag_storage`] supported this
    /// emits the architecture's store-tag instruction(s); on an untagged
    /// port it is a no-op (the default).
    ///
    /// # Safety
    ///
    /// `base .. base + len` must be a single live allocation owned by the
    /// caller, `base` must be aligned to and `len` a multiple of
    /// [`Self::granule_bytes`], and on a hardware-tagged port the region
    /// must be mapped tag-checked. The default no-op imposes none of
    /// these, but a port that stores tags relies on them.
    unsafe fn set_region_tag(&self, base: *mut u8, len: usize, tag: MemTag) {
        let _ = (base, len, tag);
    }
}

/// The memory-tagging conformance vertical.
///
/// Every architecture port runs [`conformance::run_all`] against its
/// [`MemoryTagging`] handle. The suite is portable — it names only the
/// trait — and runs on the host, exactly like the [`super::sidechannel`]
/// vertical: it is the trait-level "profile is honest" / "tag geometry
/// is sane" / "rotation actually rotates" check. Each port's own host
/// tests additionally pin the concrete profile its silicon requires.
pub mod conformance {
    use super::{MemTag, MemoryTagging, TAG_COUNT};

    /// Run the entire memory-tagging conformance suite against `port`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if any required property does not hold:
    /// the profile fails [`super::TaggingProfile::validate`], the tag
    /// geometry is invalid, the rotation fails to produce a distinct tag
    /// on a multi-tag port, or the retag primitive cannot be invoked.
    pub fn run_all<M: MemoryTagging + ?Sized>(port: &M) {
        profile_is_honest(port);
        geometry_is_sane(port);
        rotation_hardens_uaf(port);
        retag_is_callable(port);
    }

    /// The profile validates and every non-supported feature carries a
    /// non-empty justification (an `Unsupported` claim is
    /// permitted only where the silicon genuinely lacks tagging, *and
    /// justified*).
    fn profile_is_honest<M: MemoryTagging + ?Sized>(port: &M) {
        let profile = port.profile();
        assert!(
            profile.validate().is_ok(),
            "tagging profile must justify every non-supported feature (AGENTS.md §19.10): {:?}",
            profile.validate()
        );
        for entry in profile.entries() {
            if let Some(reason) = entry.tagging.detail() {
                assert!(
                    !reason.trim().is_empty(),
                    "non-supported feature `{}` must carry a non-empty explanation",
                    entry.name
                );
            }
        }
    }

    /// The granule is a power of two `>= 1` and the tag count is in
    /// `1..=`[`TAG_COUNT`].
    fn geometry_is_sane<M: MemoryTagging + ?Sized>(port: &M) {
        let granule = port.granule_bytes();
        assert!(granule >= 1, "tag granule must be at least one byte");
        assert!(
            granule.is_power_of_two(),
            "tag granule must be a power of two, got {granule}"
        );
        let count = port.tag_count();
        assert!(count >= 1, "tag count must be at least 1");
        assert!(
            count <= TAG_COUNT,
            "tag count {count} exceeds the neutral tag space {TAG_COUNT}"
        );
    }

    /// The rotation produces an in-range tag, and on a multi-tag port it
    /// produces a tag *distinct* from its input across the whole space —
    /// the property that makes a stale pointer's tag mismatch after the
    /// region is reallocated.
    fn rotation_hardens_uaf<M: MemoryTagging + ?Sized>(port: &M) {
        let count = port.tag_count();
        for raw in 0..count {
            let previous = MemTag::new(raw);
            let next = port.rotate_tag(previous);
            assert!(
                next.value() < count.max(1),
                "rotated tag {} out of range for count {count}",
                next.value()
            );
            if count >= 2 {
                assert_ne!(
                    next, previous,
                    "rotation must change the tag on a multi-tag port (UAF hardening)"
                );
            }
        }
    }

    /// The retag primitive can be invoked against a real, granule-aligned
    /// buffer without panicking (a no-op on an untagged port; the
    /// hardware store on a tagged one — itself a no-op on the host, where
    /// the store-tag instruction is `cfg`-gated out).
    fn retag_is_callable<M: MemoryTagging + ?Sized>(port: &M) {
        let granule = port.granule_bytes();
        let mut buf = [0u8; 256];
        let len = granule.min(buf.len());
        // SAFETY: `buf` is a single live stack allocation; we pass its
        // base and a length that is one granule (a multiple of the
        // granule and within `buf`). The call stamps tags on a port that
        // stores them and is a no-op otherwise; on the host the
        // store-tag instruction is `cfg`-gated out, so this cannot fault.
        unsafe {
            port.set_region_tag(buf.as_mut_ptr(), len, MemTag::new(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supported() -> TaggingProfile {
        TaggingProfile {
            tag_storage: Tagging::Supported,
            tag_check_faults: Tagging::Supported,
        }
    }

    struct StubPort {
        profile: TaggingProfile,
        granule: usize,
        count: u8,
    }

    impl MemoryTagging for StubPort {
        fn profile(&self) -> TaggingProfile {
            self.profile
        }
        fn granule_bytes(&self) -> usize {
            self.granule
        }
        fn tag_count(&self) -> u8 {
            self.count
        }
    }

    fn tagged_stub() -> StubPort {
        StubPort {
            profile: supported(),
            granule: 16,
            count: TAG_COUNT,
        }
    }

    fn untagged_stub() -> StubPort {
        StubPort {
            profile: TaggingProfile {
                tag_storage: Tagging::Unsupported("no memory-tagging silicon"),
                tag_check_faults: Tagging::Unsupported("no memory-tagging silicon"),
            },
            granule: 1,
            count: 1,
        }
    }

    #[test]
    fn memtag_wraps_into_range() {
        assert_eq!(MemTag::new(0).value(), 0);
        assert_eq!(MemTag::new(15).value(), 15);
        assert_eq!(MemTag::new(16).value(), 0);
        assert_eq!(MemTag::new(17).value(), 1);
        assert_eq!(MemTag::INITIAL, MemTag::new(0));
        assert_eq!(MemTag::default(), MemTag::INITIAL);
    }

    #[test]
    fn next_free_tag_always_differs_when_multi_tag() {
        for raw in 0..TAG_COUNT {
            let prev = MemTag::new(raw);
            let next = next_free_tag(prev, TAG_COUNT);
            assert_ne!(next, prev);
            assert!(next.value() < TAG_COUNT);
        }
    }

    #[test]
    fn next_free_tag_clamps_count() {
        // Over-wide counts clamp to TAG_COUNT.
        assert!(next_free_tag(MemTag::new(0), 200).value() < TAG_COUNT);
        // A single-tag (untagged) space cannot rotate.
        assert_eq!(next_free_tag(MemTag::new(0), 1), MemTag::INITIAL);
        assert_eq!(next_free_tag(MemTag::new(5), 0), MemTag::INITIAL);
    }

    #[test]
    fn next_free_tag_wraps_at_the_top() {
        assert_eq!(next_free_tag(MemTag::new(15), TAG_COUNT), MemTag::new(0));
        assert_eq!(next_free_tag(MemTag::new(3), 4), MemTag::new(0));
    }

    #[test]
    fn supported_profile_validates_and_is_release_ready() {
        let p = supported();
        assert_eq!(p.validate(), Ok(()));
        assert!(p.is_release_ready());
        assert!(p.enforces_uaf_in_hardware());
    }

    #[test]
    fn justified_unsupported_validates() {
        let p = untagged_stub().profile;
        assert_eq!(p.validate(), Ok(()));
        assert!(p.is_release_ready());
        assert!(!p.enforces_uaf_in_hardware());
    }

    #[test]
    fn empty_justification_is_rejected() {
        let p = TaggingProfile {
            tag_storage: Tagging::Unsupported("   "),
            tag_check_faults: Tagging::Supported,
        };
        assert_eq!(
            p.validate(),
            Err(ProfileError::EmptyJustification {
                field: "tag_storage"
            })
        );
    }

    #[test]
    fn empty_pending_note_is_rejected() {
        let p = TaggingProfile {
            tag_storage: Tagging::Supported,
            tag_check_faults: Tagging::Pending(""),
        };
        assert_eq!(
            p.validate(),
            Err(ProfileError::EmptyJustification {
                field: "tag_check_faults"
            })
        );
    }

    #[test]
    fn pending_is_honest_but_not_release_ready() {
        let p = TaggingProfile {
            tag_storage: Tagging::Supported,
            tag_check_faults: Tagging::Pending("Tagged page attribute lands in Stage 6"),
        };
        assert_eq!(p.validate(), Ok(()));
        assert!(!p.is_release_ready());
        // Storage without checking does not yet enforce UAF in hardware.
        assert!(!p.enforces_uaf_in_hardware());
    }

    #[test]
    fn tagging_helpers() {
        assert!(Tagging::Supported.is_supported());
        assert!(Tagging::Pending("x").is_pending());
        assert!(!Tagging::Supported.is_pending());
        assert_eq!(Tagging::Supported.detail(), None);
        assert_eq!(Tagging::Unsupported("why").detail(), Some("why"));
        assert!(Tagging::Unsupported("why").is_release_ready());
        assert!(!Tagging::Pending("later").is_release_ready());
    }

    #[test]
    fn entries_round_trip_the_named_slots() {
        let p = supported();
        let names: [&str; 2] = core::array::from_fn(|i| p.entries()[i].name);
        assert_eq!(names, ["tag_storage", "tag_check_faults"]);
    }

    #[test]
    fn conformance_accepts_a_tagged_port() {
        let port = tagged_stub();
        conformance::run_all(&port);
        let dynamic: &dyn MemoryTagging = &port;
        conformance::run_all(dynamic);
    }

    #[test]
    fn conformance_accepts_an_untagged_port() {
        conformance::run_all(&untagged_stub());
    }

    #[test]
    #[should_panic(expected = "must justify every non-supported feature")]
    fn conformance_rejects_an_unjustified_claim() {
        let port = StubPort {
            profile: TaggingProfile {
                tag_storage: Tagging::Unsupported(""),
                tag_check_faults: Tagging::Supported,
            },
            granule: 16,
            count: TAG_COUNT,
        };
        conformance::run_all(&port);
    }

    #[test]
    #[should_panic(expected = "tag granule must be a power of two")]
    fn conformance_rejects_a_non_power_of_two_granule() {
        let port = StubPort {
            profile: supported(),
            granule: 24,
            count: TAG_COUNT,
        };
        conformance::run_all(&port);
    }

    #[test]
    #[should_panic(expected = "exceeds the neutral tag space")]
    fn conformance_rejects_an_oversized_tag_count() {
        let port = StubPort {
            profile: supported(),
            granule: 16,
            count: TAG_COUNT + 1,
        };
        conformance::run_all(&port);
    }
}
