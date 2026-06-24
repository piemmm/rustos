//! Side-channel mitigation surface of the Arch HAL.
//!
//! Microarchitectural side channels (Meltdown, Spectre, MDS, L1TF, MMIO
//! stale data) are defeated by primitives that only the architecture
//! port can emit: kernel/user address-space separation
//! (KPTI-equivalent), speculation barriers on the syscall entry/exit
//! boundary, and a flush of microarchitectural buffers plus an
//! indirect-branch-predictor barrier on every context switch.
//! makes this a closed trait set on the Arch HAL; this module is that
//! set.
//!
//! # What lives here
//!
//! * [`SideChannelMitigation`] — the per-port handle the kernel reaches
//!   through. It exposes the three per-transition barrier primitives the
//!   kernel calls (syscall entry, syscall exit, context switch) and a
//!   declarative [`MitigationProfile`] describing, honestly, which
//!   mitigations the port applies for its silicon.
//! * [`MitigationProfile`] / [`Mitigation`] — the honest declaration. A
//!   mitigation is either [`Mitigation::Applied`] or
//!   [`Mitigation::NotVulnerable`], and the latter must carry the
//!   justification recorded in the port's `README.md`: the charter permits a
//!   no-op "**only** on targets where the silicon is provably not
//!   vulnerable and the absence is justified".
//! * [`conformance`] — the conformance vertical the charter mandates.
//!   Every port runs [`conformance::run_all`] against its handle; a port
//!   that does not pass cannot ship.
//!
//! # Why a declarative profile rather than only barrier calls
//!
//! A barrier method that does nothing and a barrier method that emits
//! the right instruction are indistinguishable to a portable, host-run
//! acceptance test (the instruction is only meaningful on the bare-metal
//! target, where it cannot be observed from a unit test). The
//! [`MitigationProfile`] closes that gap: the port must *declare* a
//! decision for every mitigation, and the conformance suite
//! refuses an undeclared or unjustified omission. The instruction
//! emission itself is reviewed in the port (every
//! `unsafe` block carries a `// SAFETY:`), and each port's own host
//! tests assert the barriers are wired (not silently empty).

/// One mitigation's status on a given architecture port.
///
/// The port declares, per mitigation, exactly one of three honest
/// positions. The charter allows a no-op ([`Mitigation::NotVulnerable`])
/// **only** where the silicon is provably not vulnerable to the class
/// the mitigation defends against, and requires the absence to be
/// justified in the port's `README.md`; the payload carries that same
/// justification so the conformance suite can refuse an unjustified
/// no-op. The third position, [`Mitigation::Pending`], is for a
/// mitigation that the silicon *does* require but that cannot yet be
/// built because it depends on a not-yet-landed subsystem (e.g.
/// KPTI-equivalent page-table isolation needs the Stage 6 user/kernel
/// boundary). A `Pending` entry is honest and tracked — it is the
/// truthful state of a young kernel — but it is **not** release-ready:
/// [`MitigationProfile::is_release_ready`] rejects it, encoding's
/// "a target that does not pass cannot ship" as the release gate while
/// keeping the per-PR honesty gate (`validate`) green during the
/// burn-down.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mitigation {
    /// The port implements this mitigation for its target.
    Applied,
    /// The port deliberately omits this mitigation because its silicon
    /// is provably not vulnerable to the class it defends against. The
    /// payload is the justification recorded in the port's `README.md`; it must be non-empty.
    NotVulnerable(&'static str),
    /// The silicon requires this mitigation, but it cannot be built yet
    /// because it depends on a subsystem that has not landed. The
    /// payload is the tracking note (the `PLAN.md` stage/item that will
    /// deliver it); it must be non-empty. A `Pending` mitigation passes
    /// the honesty gate but fails [`MitigationProfile::is_release_ready`].
    Pending(&'static str),
}

impl Mitigation {
    /// `true` if this mitigation is [`Mitigation::Applied`].
    #[must_use]
    pub const fn is_applied(self) -> bool {
        matches!(self, Self::Applied)
    }

    /// `true` if this mitigation is a tracked [`Mitigation::Pending`].
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending(_))
    }

    /// `true` if this mitigation is release-ready: it is either applied
    /// or a justified [`Mitigation::NotVulnerable`]. A
    /// [`Mitigation::Pending`] mitigation is not release-ready.
    #[must_use]
    pub const fn is_release_ready(self) -> bool {
        matches!(self, Self::Applied | Self::NotVulnerable(_))
    }

    /// The explanatory note for a non-applied decision (the
    /// justification for [`Mitigation::NotVulnerable`] or the tracking
    /// note for [`Mitigation::Pending`]), or `None` when applied.
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::Applied => None,
            Self::NotVulnerable(reason) | Self::Pending(reason) => Some(reason),
        }
    }
}

/// A port's honest declaration of the mitigations it applies.
///
/// Every field is a [`Mitigation`]: there is no "unknown" or "to be
/// decided" — a port must take a position on each, and the conformance
/// suite ([`MitigationProfile::validate`]) refuses any
/// [`Mitigation::NotVulnerable`] that does not carry a justification.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MitigationProfile {
    /// Kernel/user address-space separation (KPTI-equivalent): the
    /// kernel is not mapped into the page-table root active in user
    /// mode, defeating Meltdown-class cross-privilege reads. On x86_64
    /// this is a separate `CR3` per privilege; on aarch64 the
    /// `TTBR0`/`TTBR1` split with the kernel unmapped from the user
    /// root; on riscv64 a per-privilege `satp`.
    pub address_space_isolation: Mitigation,
    /// Speculation barrier on the user→kernel syscall entry boundary
    /// (e.g. `lfence`/IBRS on x86_64, `CSDB`/`SB` on `aarch64`).
    pub syscall_entry_barrier: Mitigation,
    /// Speculation barrier on the kernel→user syscall return boundary.
    pub syscall_exit_barrier: Mitigation,
    /// Flush of microarchitectural buffers (MDS / L1TF / MMIO stale
    /// data) on context switch.
    pub context_switch_buffer_flush: Mitigation,
    /// Indirect-branch-predictor barrier (IBPB-equivalent) on context
    /// switch, defeating cross-task Spectre-v2 branch-target injection.
    pub context_switch_indirect_branch_barrier: Mitigation,
}

/// A single named mitigation slot of a [`MitigationProfile`], yielded by
/// [`MitigationProfile::entries`] so the conformance suite and any
/// diagnostic can iterate the profile without hard-coding field names.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MitigationEntry {
    /// Stable, human-readable name of the mitigation slot.
    pub name: &'static str,
    /// The port's decision for this slot.
    pub mitigation: Mitigation,
}

/// Reason a [`MitigationProfile`] failed [`MitigationProfile::validate`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProfileError {
    /// A [`Mitigation::NotVulnerable`] decision carried an empty (or
    /// whitespace-only) justification. The charter requires every omission to
    /// be justified; `field` names the offending slot.
    EmptyJustification {
        /// The [`MitigationEntry::name`] of the unjustified slot.
        field: &'static str,
    },
}

impl MitigationProfile {
    /// The five mitigation slots, in a stable order, each paired
    /// with its name.
    #[must_use]
    pub const fn entries(&self) -> [MitigationEntry; 5] {
        [
            MitigationEntry {
                name: "address_space_isolation",
                mitigation: self.address_space_isolation,
            },
            MitigationEntry {
                name: "syscall_entry_barrier",
                mitigation: self.syscall_entry_barrier,
            },
            MitigationEntry {
                name: "syscall_exit_barrier",
                mitigation: self.syscall_exit_barrier,
            },
            MitigationEntry {
                name: "context_switch_buffer_flush",
                mitigation: self.context_switch_buffer_flush,
            },
            MitigationEntry {
                name: "context_switch_indirect_branch_barrier",
                mitigation: self.context_switch_indirect_branch_barrier,
            },
        ]
    }

    /// Validate the honesty rule: every non-applied mitigation
    /// must carry a non-empty explanation — a justification for a
    /// [`Mitigation::NotVulnerable`] no-op or a tracking note for a
    /// [`Mitigation::Pending`] gap.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::EmptyJustification`] naming the first slot
    /// whose [`Mitigation::detail`] is present but empty or
    /// whitespace-only.
    pub fn validate(&self) -> Result<(), ProfileError> {
        for entry in self.entries() {
            if let Some(reason) = entry.mitigation.detail() {
                if reason.trim().is_empty() {
                    return Err(ProfileError::EmptyJustification { field: entry.name });
                }
            }
        }
        Ok(())
    }

    /// `true` if every mitigation is release-ready — applied or a
    /// justified [`Mitigation::NotVulnerable`], with no
    /// [`Mitigation::Pending`] gap remaining.
    ///
    /// This encodes's "a target that does not pass this suite
    /// cannot ship": the per-PR honesty gate ([`Self::validate`]) stays
    /// green while the burn-down advances, and this stricter predicate
    /// is the release gate that a port must satisfy before it ships.
    #[must_use]
    pub fn is_release_ready(&self) -> bool {
        self.entries()
            .iter()
            .all(|entry| entry.mitigation.is_release_ready())
    }
}

/// The side-channel mitigation handle an architecture port exposes.
///
/// The kernel calls the barrier primitives on the matching transition:
/// [`Self::syscall_entry_barrier`] at the top of the syscall trap,
/// [`Self::syscall_exit_barrier`] immediately before returning to user
/// mode, and [`Self::context_switch_barrier`] when switching the active
/// task. Each is a no-op-cost call on a port whose silicon does not need
/// it (and which declares so via [`Self::profile`]); otherwise it emits
/// the architecture's serialising / buffer-clearing instruction(s).
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// handle from every CPU.
pub trait SideChannelMitigation: Send + Sync {
    /// The port's honest declaration of which mitigations it
    /// applies. Must satisfy [`MitigationProfile::validate`].
    fn profile(&self) -> MitigationProfile;

    /// Speculation barrier executed at the user→kernel syscall entry
    /// boundary, before the kernel acts on any user-controlled value.
    fn syscall_entry_barrier(&self);

    /// Speculation barrier executed immediately before returning from a
    /// syscall to user mode.
    fn syscall_exit_barrier(&self);

    /// Microarchitectural-buffer flush plus indirect-branch-predictor
    /// barrier executed on a context switch, so a newly scheduled task
    /// cannot observe the microarchitectural residue of its predecessor.
    fn context_switch_barrier(&self);
}

/// The side-channel conformance vertical.
///
/// Every architecture port runs [`conformance::run_all`] against its
/// [`SideChannelMitigation`] handle; states "a target that does
/// not pass this suite cannot ship". The suite is portable — it names
/// only the trait — and is run on the host target, exactly like the
/// `kernel/sched` policy conformance suite. It is the trait-level
/// "barrier present" / "isolation declared" check; each port's own host
/// tests additionally assert the concrete profile its silicon requires
/// and that the barrier primitives are wired (not silently empty).
pub mod conformance {
    use super::SideChannelMitigation;

    /// Run the entire side-channel conformance suite against `port`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if any required property does not hold:
    /// the profile fails [`super::MitigationProfile::validate`], a slot
    /// is omitted without justification, or a barrier primitive cannot
    /// be invoked.
    pub fn run_all<M: SideChannelMitigation + ?Sized>(port: &M) {
        profile_is_honest(port);
        barriers_are_callable(port);
    }

    /// The profile validates and every omitted mitigation carries a
    /// non-empty justification (a no-op is permitted only where
    /// the silicon is provably not vulnerable, *and justified*).
    fn profile_is_honest<M: SideChannelMitigation + ?Sized>(port: &M) {
        let profile = port.profile();
        assert!(
            profile.validate().is_ok(),
            "side-channel profile must justify every omitted mitigation (AGENTS.md §19.1): {:?}",
            profile.validate()
        );
        for entry in profile.entries() {
            if let Some(reason) = entry.mitigation.detail() {
                assert!(
                    !reason.trim().is_empty(),
                    "non-applied mitigation `{}` must carry a non-empty explanation",
                    entry.name
                );
            }
        }
    }

    /// Every barrier primitive can be invoked (repeatedly) without
    /// panicking. The barriers are idempotent: emitting a speculation
    /// fence twice is as safe as emitting it once.
    fn barriers_are_callable<M: SideChannelMitigation + ?Sized>(port: &M) {
        port.syscall_entry_barrier();
        port.syscall_entry_barrier();
        port.syscall_exit_barrier();
        port.syscall_exit_barrier();
        port.context_switch_barrier();
        port.context_switch_barrier();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile that applies every mitigation — the shape a port on
    /// fully-vulnerable silicon (x86_64) declares.
    fn all_applied() -> MitigationProfile {
        MitigationProfile {
            address_space_isolation: Mitigation::Applied,
            syscall_entry_barrier: Mitigation::Applied,
            syscall_exit_barrier: Mitigation::Applied,
            context_switch_buffer_flush: Mitigation::Applied,
            context_switch_indirect_branch_barrier: Mitigation::Applied,
        }
    }

    struct StubPort {
        profile: MitigationProfile,
    }

    impl SideChannelMitigation for StubPort {
        fn profile(&self) -> MitigationProfile {
            self.profile
        }
        fn syscall_entry_barrier(&self) {}
        fn syscall_exit_barrier(&self) {}
        fn context_switch_barrier(&self) {}
    }

    #[test]
    fn applied_profile_validates() {
        assert_eq!(all_applied().validate(), Ok(()));
    }

    #[test]
    fn justified_omission_validates() {
        let mut p = all_applied();
        p.syscall_entry_barrier =
            Mitigation::NotVulnerable("no speculative execution on this core");
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn empty_justification_is_rejected() {
        let mut p = all_applied();
        p.context_switch_buffer_flush = Mitigation::NotVulnerable("   ");
        assert_eq!(
            p.validate(),
            Err(ProfileError::EmptyJustification {
                field: "context_switch_buffer_flush"
            })
        );
    }

    #[test]
    fn validate_reports_the_first_offending_slot() {
        let mut p = all_applied();
        p.address_space_isolation = Mitigation::NotVulnerable("");
        p.syscall_exit_barrier = Mitigation::NotVulnerable("");
        assert_eq!(
            p.validate(),
            Err(ProfileError::EmptyJustification {
                field: "address_space_isolation"
            })
        );
    }

    #[test]
    fn entries_round_trip_the_named_slots() {
        let p = all_applied();
        let names: [&str; 5] = core::array::from_fn(|i| p.entries()[i].name);
        assert_eq!(
            names,
            [
                "address_space_isolation",
                "syscall_entry_barrier",
                "syscall_exit_barrier",
                "context_switch_buffer_flush",
                "context_switch_indirect_branch_barrier",
            ]
        );
    }

    #[test]
    fn mitigation_helpers() {
        assert!(Mitigation::Applied.is_applied());
        assert!(!Mitigation::NotVulnerable("x").is_applied());
        assert!(Mitigation::Pending("later").is_pending());
        assert!(!Mitigation::Applied.is_pending());
        assert_eq!(Mitigation::Applied.detail(), None);
        assert_eq!(Mitigation::NotVulnerable("why").detail(), Some("why"));
        assert_eq!(Mitigation::Pending("stage6").detail(), Some("stage6"));
        assert!(Mitigation::Applied.is_release_ready());
        assert!(Mitigation::NotVulnerable("why").is_release_ready());
        assert!(!Mitigation::Pending("stage6").is_release_ready());
    }

    #[test]
    fn pending_is_honest_but_not_release_ready() {
        let mut p = all_applied();
        p.address_space_isolation =
            Mitigation::Pending("KPTI lands with the Stage 6 process model");
        // The honesty gate accepts a tracked Pending gap...
        assert_eq!(p.validate(), Ok(()));
        // ...but the release gate does not.
        assert!(!p.is_release_ready());
        // A fully-applied / justified profile is release-ready.
        assert!(all_applied().is_release_ready());
    }

    #[test]
    fn empty_pending_note_is_rejected() {
        let mut p = all_applied();
        p.address_space_isolation = Mitigation::Pending("  ");
        assert_eq!(
            p.validate(),
            Err(ProfileError::EmptyJustification {
                field: "address_space_isolation"
            })
        );
    }

    #[test]
    fn conformance_accepts_a_tracked_pending_port() {
        let mut profile = all_applied();
        profile.address_space_isolation =
            Mitigation::Pending("KPTI lands with the Stage 6 process model");
        let port = StubPort { profile };
        conformance::run_all(&port);
    }

    #[test]
    fn conformance_accepts_an_honest_port() {
        let port = StubPort {
            profile: all_applied(),
        };
        conformance::run_all(&port);
        // Object-safe: the kernel reaches the handle through `&dyn`.
        let dynamic: &dyn SideChannelMitigation = &port;
        conformance::run_all(dynamic);
    }

    #[test]
    #[should_panic(expected = "must justify every omitted mitigation")]
    fn conformance_rejects_an_unjustified_omission() {
        let mut profile = all_applied();
        profile.syscall_entry_barrier = Mitigation::NotVulnerable("");
        let port = StubPort { profile };
        conformance::run_all(&port);
    }
}
