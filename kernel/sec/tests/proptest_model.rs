//! Stateful property model for `kernel/sec` (Bronze).
//!
//! the charter requires the capability-critical paths to carry a `proptest`-style
//! stateful model. `kernel/sec/tests/proptest_invariants.rs` already checks
//! the two lib-level delegation properties in isolation; this model lifts
//! them to the **registry** level: a randomised sequence of commands drives a
//! live [`CapTable`] of [`TaskCapabilities`] against an independent reference
//! model, asserting after every command that
//!
//! * a derived task's effective set is exactly `user_grant ∩ manifest_request`
//!   and therefore a subset of both (no ambient authority),
//! * [`TaskCapabilities::delegate`] never widens the effective set and a
//!   refused delegation leaves it untouched,
//! * [`TaskCapabilities::revoke`] only ever shrinks the effective set, and
//! * the registry's contents (membership, cardinality, per-task effective
//!   set) match the model.
//!
//! Unlike the fuzz harnesses this generates structured command
//! sequences and lets proptest **shrink** any counterexample.
//!
//! ## Wall-clock budget
//!
//! The shared `tairix_fuzzseed::prop::drive` runner owns the seed/budget
//! policy (one definition): a plain `cargo test` runs [`SMOKE_CASES`]
//! sequences **once** from a fresh, logged seed; `cargo xtask proptest --soak`
//! exports `TAIRIX_PROPTEST_BUDGET_SECS` and the runner repeats
//! [`BUDGET_BATCH_CASES`] batches off the same continuing RNG until the
//! deadline. The seed is logged at the start of each run (pinnable via
//! `--seed`), so a fresh-seed counterexample is still reproducible.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tairix_abi::CapabilityId;
use tairix_caps::CapabilitySet;
use tairix_kernel_sec::{CapTable, TaskCapabilities, TaskId, UserId};
use tairix_log::{Event, Sink};

/// Sequences run by a plain `cargo test` (no budget set).
const SMOKE_CASES: u32 = 256;
/// Sequences per batch under a wall-clock budget.
const BUDGET_BATCH_CASES: u32 = 256;
/// Highest capability id drawn by the model.
const CAP_MAX: u16 = 12;
/// Number of distinct task slots the registry juggles.
const TASKS: u64 = 4;

/// Sink that discards events; this model checks invariants, not audit text
/// (the per-decision audit emission is covered by the unit tests).
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

fn cap(id: u16) -> CapabilityId {
    CapabilityId::from_raw(id).expect("id within CAPABILITY_ID_MAX")
}

fn build(ids: &[u16]) -> CapabilitySet {
    let mut s = CapabilitySet::empty();
    for &id in ids {
        s.insert(cap(id));
    }
    s
}

fn to_model(set: &CapabilitySet) -> BTreeSet<u16> {
    set.iter().map(CapabilityId::as_u16).collect()
}

/// One operation on the registry under test.
#[derive(Clone, Debug)]
enum Cmd {
    /// Derive a fresh task and insert it (replacing any prior record).
    Insert {
        task: u64,
        user_grant: Vec<u16>,
        manifest: Vec<u16>,
    },
    Delegate {
        task: u64,
        requested: Vec<u16>,
    },
    Revoke {
        task: u64,
        cap: u16,
    },
    Remove {
        task: u64,
    },
}

/// Reference image of one task's authority.
struct TaskModel {
    user_grant: BTreeSet<u16>,
    manifest: BTreeSet<u16>,
    effective: BTreeSet<u16>,
}

fn id_vec() -> impl Strategy<Value = Vec<u16>> {
    prop::collection::vec(0u16..=CAP_MAX, 0..=6)
}

fn task_id() -> impl Strategy<Value = u64> {
    0u64..TASKS
}

fn command() -> impl Strategy<Value = Cmd> {
    prop_oneof![
        (task_id(), id_vec(), id_vec()).prop_map(|(task, user_grant, manifest)| Cmd::Insert {
            task,
            user_grant,
            manifest,
        }),
        (task_id(), id_vec()).prop_map(|(task, requested)| Cmd::Delegate { task, requested }),
        (task_id(), 0u16..=CAP_MAX).prop_map(|(task, cap)| Cmd::Revoke { task, cap }),
        task_id().prop_map(|task| Cmd::Remove { task }),
    ]
}

fn program() -> impl Strategy<Value = Vec<Cmd>> {
    prop::collection::vec(command(), 0..=48)
}

#[test]
fn captable_tracks_reference_model() {
    tairix_fuzzseed::prop::drive(
        "captable_tracks_reference_model",
        SMOKE_CASES,
        BUDGET_BATCH_CASES,
        program(),
        |cmds| check_captable(&cmds),
    );
}

/// The per-program check `drive` runs, split out so the `#[test]` wrapper
/// stays small (clippy `too_many_lines`).
fn check_captable(cmds: &[Cmd]) -> Result<(), TestCaseError> {
    let sink = NullSink;
    let mut table = CapTable::new();
    let mut model: BTreeMap<u64, TaskModel> = BTreeMap::new();

    for c in cmds {
        match c {
            Cmd::Insert {
                task,
                user_grant,
                manifest,
            } => {
                let ug = build(user_grant);
                let mf = build(manifest);
                let caps = TaskCapabilities::derive(TaskId(*task), UserId(1), ug, mf, &sink);
                // Derive must intersect: effective ⊆ both inputs.
                prop_assert!(caps.effective().is_subset_of(&ug));
                prop_assert!(caps.effective().is_subset_of(&mf));
                table.insert(caps);

                let ug_m: BTreeSet<u16> = user_grant.iter().copied().collect();
                let mf_m: BTreeSet<u16> = manifest.iter().copied().collect();
                let eff_m: BTreeSet<u16> = ug_m.intersection(&mf_m).copied().collect();
                model.insert(
                    *task,
                    TaskModel {
                        user_grant: ug_m,
                        manifest: mf_m,
                        effective: eff_m,
                    },
                );
            }
            Cmd::Delegate { task, requested } => {
                let req = build(requested);
                let req_m: BTreeSet<u16> = requested.iter().copied().collect();
                let live = table.caps_for_mut(TaskId(*task));
                let entry = model.get_mut(task);
                match (live, entry) {
                    (Some(caps), Some(state)) => {
                        let before = to_model(caps.effective());
                        let res = caps.delegate(&req, &sink);
                        if req_m.is_subset(&state.effective) {
                            prop_assert!(res.is_ok());
                            prop_assert_eq!(to_model(caps.effective()), req_m.clone());
                            state.effective = req_m;
                        } else {
                            prop_assert!(res.is_err());
                            // Refused delegation leaves the set untouched.
                            prop_assert_eq!(to_model(caps.effective()), before);
                        }
                    }
                    (None, None) => {}
                    _ => return Err(TestCaseError::fail("registry/model membership diverged")),
                }
            }
            Cmd::Revoke { task, cap: c } => {
                let live = table.caps_for_mut(TaskId(*task));
                let entry = model.get_mut(task);
                match (live, entry) {
                    (Some(caps), Some(state)) => {
                        let before = to_model(caps.effective());
                        let was = caps.revoke(cap(*c), &sink);
                        prop_assert_eq!(was, state.effective.remove(c));
                        // Revoke only ever shrinks the effective set.
                        prop_assert!(caps
                            .effective()
                            .is_subset_of(&build(&before.iter().copied().collect::<Vec<_>>())));
                        prop_assert_eq!(to_model(caps.effective()), state.effective.clone());
                    }
                    (None, None) => {}
                    _ => return Err(TestCaseError::fail("registry/model membership diverged")),
                }
            }
            Cmd::Remove { task } => {
                let live = table.remove(TaskId(*task)).is_some();
                let modelled = model.remove(task).is_some();
                prop_assert_eq!(live, modelled);
            }
        }

        // Registry-wide invariants after each command.
        prop_assert_eq!(table.len(), model.len());
        prop_assert_eq!(table.is_empty(), model.is_empty());
        for (task, state) in &model {
            let caps = table
                .caps_for(TaskId(*task))
                .ok_or_else(|| TestCaseError::fail("modelled task missing from registry"))?;
            prop_assert_eq!(to_model(caps.effective()), state.effective.clone());
            // The upstream bounds never change; effective stays within them.
            prop_assert_eq!(to_model(caps.user_grant()), state.user_grant.clone());
            prop_assert_eq!(to_model(caps.manifest_request()), state.manifest.clone());
        }
    }
    Ok(())
}
