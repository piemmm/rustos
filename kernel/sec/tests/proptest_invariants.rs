//! Property tests for the `kernel/sec` security invariants.
//!
//! Mirrors `AGENTS.md` §7 "property tests" guidance: the lib-level
//! delegation invariant is asserted in `lib/caps`; this file lifts the
//! analogous invariant to the **task** level, exactly as the Stage 2.4
//! brief requires.
//!
//! Two properties are checked across randomised inputs:
//!
//! 1. For any (`user_grant`, `manifest_request`) pair, the *effective* set
//!    produced by [`TaskCapabilities::derive`] is a subset of *both*
//!    inputs — in particular of the user grant. This is the kernel-level
//!    statement of the "no ambient authority" rule.
//! 2. For any successful [`TaskCapabilities::delegate`] call, the
//!    post-call effective set is a subset of the pre-call effective set.
//!    A delegation that would widen the effective set must be refused
//!    with [`Errno::DelegationWiden`] and must leave the effective set
//!    unchanged.

use proptest::prelude::*;
use rustos_abi::{CapabilityId, Errno};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{TaskCapabilities, TaskId, UserId};
use rustos_log::{Event, Sink};

/// Sink that throws away every event it receives. The property test is
/// only interested in the *invariant*; the per-decision audit emission
/// is exhaustively covered by the negative unit tests in the lib.
struct NullSink;

impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// Generate an arbitrary [`CapabilitySet`] built from the well-known
/// `abi-v1` capability identifiers. Using only the well-known set keeps
/// the search space small and the test fast, while still covering every
/// branch the production code can take.
fn caps_strategy() -> impl Strategy<Value = CapabilitySet> {
    let universe = [
        CapabilityId::FS_MOUNT,
        CapabilityId::NET_RAW,
        CapabilityId::DRV_LOAD,
        CapabilityId::DRV_KERNEL,
        CapabilityId::USER_ADMIN,
        CapabilityId::TIME_SET,
        CapabilityId::IPC_BIND_PRIVILEGED,
        CapabilityId::AUDIT_READ,
        CapabilityId::AUDIT_WRITE,
    ];
    proptest::collection::vec(0u8..=8, 0..=9).prop_map(move |selections| {
        let mut s = CapabilitySet::empty();
        for sel in selections {
            s.insert(universe[sel as usize]);
        }
        s
    })
}

proptest! {
    /// **Property 1.** The derived effective set is a subset of both
    /// inputs. Mirrors the `lib/caps` delegation invariant at the task
    /// level (Stage 2.4 brief).
    #[test]
    fn derived_effective_is_subset_of_user_grant_and_manifest(
        user_grant in caps_strategy(),
        manifest_request in caps_strategy(),
    ) {
        let t = TaskCapabilities::derive(
            TaskId(0),
            UserId(1),
            user_grant,
            manifest_request,
            &NullSink,
        );
        prop_assert!(t.effective().is_subset_of(&user_grant));
        prop_assert!(t.effective().is_subset_of(&manifest_request));
    }

    /// **Property 2.** Delegation never widens; a refused delegation
    /// leaves the effective set unchanged.
    #[test]
    fn delegation_never_widens(
        user_grant in caps_strategy(),
        manifest_request in caps_strategy(),
        requested in caps_strategy(),
    ) {
        let mut t = TaskCapabilities::derive(
            TaskId(0),
            UserId(1),
            user_grant,
            manifest_request,
            &NullSink,
        );
        let before = *t.effective();
        match t.delegate(&requested, &NullSink) {
            Ok(()) => {
                // The new effective set must be a subset of the old.
                prop_assert!(t.effective().is_subset_of(&before));
                // And equal to the requested set (the contract of
                // `CapabilitySet::delegate`).
                prop_assert_eq!(*t.effective(), requested);
            }
            Err(err) => {
                // The only failure mode is widening.
                prop_assert_eq!(err, Errno::DelegationWiden);
                // And the effective set must be unchanged on refusal.
                prop_assert_eq!(*t.effective(), before);
            }
        }
    }
}
