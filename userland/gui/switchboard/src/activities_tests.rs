//! Unit tests for [`Activities`].

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{Errno, ProcId};

use super::{Activities, Member, ACTIVITY_NAME_MAX, MAX_ACTIVITIES, MAX_ACTIVITY_MEMBERS};

/// A distinct [`ProcId`] for test fixture `n`, so members compare unequal.
fn proc(n: u8) -> ProcId {
    ProcId::from_raw([n; 16])
}

fn member(n: u8, pid: u64, name: &str) -> Member {
    Member {
        proc_id: proc(n),
        pid,
        name: String::from(name),
    }
}

#[test]
fn create_takes_the_first_member_and_auto_names_by_id() {
    let mut activities = Activities::new();
    let id = activities.create(member(1, 10, "alpha")).expect("room");
    assert_eq!(id, 1);
    assert_eq!(activities.len(), 1);
    let view = activities.iter().next().expect("one activity");
    assert_eq!(view.id, 1);
    assert_eq!(view.name, "Activity 1");
    assert!(!view.paused);
    assert_eq!(view.members.len(), 1);
    assert_eq!(view.members[0].pid, 10);
}

#[test]
fn ids_are_monotonic_and_never_reused_after_a_close() {
    let mut activities = Activities::new();
    let first = activities.create(member(1, 10, "a")).expect("room");
    activities.close(0);
    let second = activities.create(member(2, 20, "b")).expect("room");
    assert_eq!(first, 1);
    assert_eq!(second, 2, "the closed activity's id 1 is never reissued");
}

#[test]
fn create_is_bounded_at_max_activities() {
    let mut activities = Activities::new();
    for n in 0..MAX_ACTIVITIES {
        let pid = u64::try_from(n).expect("small n");
        activities
            .create(member(u8::try_from(n).expect("small n"), pid, "a"))
            .expect("room for up to the bound");
    }
    assert_eq!(activities.len(), MAX_ACTIVITIES);
    let refusal = activities.create(member(200, 999, "overflow"));
    assert_eq!(refusal, Err(Errno::OutOfRange));
    assert_eq!(
        activities.len(),
        MAX_ACTIVITIES,
        "the refused create is a no-op"
    );
}

#[test]
fn assigning_a_member_moves_it_out_of_its_previous_activity() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    activities.create(member(2, 20, "b")).expect("room");
    activities
        .assign(1, member(1, 10, "a"))
        .expect("room in the second activity");
    // The first activity had exactly one member, so moving it out
    // dissolved that activity; the second activity is now at index 0.
    assert_eq!(activities.group_index_of(proc(1)), Some(0));
    assert_eq!(activities.len(), 1);
    let survivor = activities.iter().next().expect("one activity");
    assert_eq!(survivor.id, 2, "the member landed in the second activity");
    assert_eq!(survivor.members.len(), 2);
}

#[test]
fn assigning_a_members_own_activity_is_idempotent_and_refreshes_its_name() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "old-name")).expect("room");
    activities
        .assign(0, member(1, 10, "new-name"))
        .expect("already a member");
    assert_eq!(activities.len(), 1);
    let view = activities.iter().next().expect("one activity");
    assert_eq!(view.members.len(), 1);
    assert_eq!(view.members[0].name, "new-name");
}

#[test]
fn assign_is_bounded_at_max_activity_members() {
    let mut activities = Activities::new();
    activities.create(member(0, 0, "seed")).expect("room");
    for n in 1..u8::try_from(MAX_ACTIVITY_MEMBERS).expect("bound fits a u8") {
        activities
            .assign(0, member(n, u64::from(n), "a"))
            .expect("room for up to the bound");
    }
    let view = activities.iter().next().expect("one activity");
    assert_eq!(view.members.len(), MAX_ACTIVITY_MEMBERS);
    let overflow = u8::try_from(MAX_ACTIVITY_MEMBERS).expect("bound fits a u8");
    let refusal = activities.assign(0, member(overflow, 999, "overflow"));
    assert_eq!(refusal, Err(Errno::OutOfRange));
}

#[test]
fn assign_to_an_out_of_range_activity_is_refused() {
    let mut activities = Activities::new();
    let refusal = activities.assign(0, member(1, 10, "a"));
    assert_eq!(refusal, Err(Errno::OutOfRange));
}

#[test]
fn unassign_dissolves_the_activity_when_it_empties() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    assert!(activities.unassign(proc(1)));
    assert!(activities.is_empty());
}

#[test]
fn unassign_leaves_a_still_populated_activity_intact() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    activities.assign(0, member(2, 20, "b")).expect("room");
    assert!(activities.unassign(proc(1)));
    assert_eq!(activities.len(), 1);
    assert_eq!(activities.group_index_of(proc(2)), Some(0));
}

#[test]
fn unassigning_a_never_grouped_process_is_a_harmless_no_op() {
    let mut activities = Activities::new();
    assert!(!activities.unassign(proc(9)));
}

#[test]
fn rename_trims_whitespace_and_accepts_a_valid_name() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    activities.rename(0, "  Focus  ").expect("valid rename");
    let view = activities.iter().next().expect("one activity");
    assert_eq!(view.name, "Focus");
}

#[test]
fn rename_to_the_activitys_own_current_name_is_allowed() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    activities.rename(0, "Activity 1").expect("same name");
}

#[test]
fn rename_refuses_an_empty_or_whitespace_only_name() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    assert_eq!(activities.rename(0, "   "), Err(Errno::LengthOutOfRange));
    assert_eq!(activities.rename(0, ""), Err(Errno::LengthOutOfRange));
}

#[test]
fn rename_refuses_a_name_longer_than_the_bound() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    let over_long = "x".repeat(ACTIVITY_NAME_MAX + 1);
    assert_eq!(
        activities.rename(0, &over_long),
        Err(Errno::LengthOutOfRange)
    );
    let exactly_at_bound = "x".repeat(ACTIVITY_NAME_MAX);
    assert!(activities.rename(0, &exactly_at_bound).is_ok());
}

#[test]
fn rename_refuses_a_duplicate_of_another_activitys_name() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    activities.create(member(2, 20, "b")).expect("room");
    activities.rename(0, "Shared").expect("first rename");
    assert_eq!(activities.rename(1, "Shared"), Err(Errno::AlreadyExists));
}

#[test]
fn rename_of_an_out_of_range_activity_is_refused() {
    let mut activities = Activities::new();
    assert_eq!(activities.rename(0, "anything"), Err(Errno::OutOfRange));
}

#[test]
fn set_paused_flips_the_flag_and_reports_success() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    assert!(activities.set_paused(0, true));
    assert!(activities.iter().next().expect("one activity").paused);
    assert!(activities.set_paused(0, false));
    assert!(!activities.iter().next().expect("one activity").paused);
}

#[test]
fn set_paused_on_an_out_of_range_activity_fails_closed() {
    let mut activities = Activities::new();
    assert!(!activities.set_paused(0, true));
}

#[test]
fn close_removes_the_activity_and_returns_its_members() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    activities.assign(0, member(2, 20, "b")).expect("room");
    let members = activities.close(0).expect("a real activity");
    assert_eq!(members.len(), 2);
    assert!(activities.is_empty());
    assert_eq!(activities.group_index_of(proc(1)), None);
}

#[test]
fn close_of_an_out_of_range_activity_is_none() {
    let mut activities = Activities::new();
    assert_eq!(activities.close(0), None);
}

#[test]
fn retain_live_prunes_members_not_in_the_set_and_dissolves_emptied_groups() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "a")).expect("room");
    activities.assign(0, member(2, 20, "b")).expect("room");
    activities.create(member(3, 30, "c")).expect("room");

    let mut live = alloc::collections::BTreeSet::new();
    live.insert(proc(1));
    // proc(2) and proc(3) have exited; proc(1)'s activity survives with
    // one member, and the second activity (only proc(3)) dissolves.
    activities.retain_live(&live);

    assert_eq!(activities.len(), 1);
    let view = activities.iter().next().expect("one activity remains");
    assert_eq!(view.members.len(), 1);
    assert_eq!(view.members[0].pid, 10);
}

#[test]
fn refresh_names_updates_joined_members_and_leaves_others_untouched() {
    use tairix_abi::sysinfo::ProcessState;

    use crate::sample::ProcessSummary;

    let mut activities = Activities::new();
    activities.create(member(1, 10, "stale")).expect("room");

    let processes = alloc::vec![ProcessSummary {
        pid: 10,
        proc_id: proc(1),
        name: Vec::from(*b"fresh"),
        state: ProcessState::Running,
        uid: 1000,
        mem_bytes: 0,
        priority: tairix_abi::SchedPriority::Normal,
        cpu_permille: None,
    }];
    activities.refresh_names(&processes);
    let view = activities.iter().next().expect("one activity");
    assert_eq!(view.members[0].name, "fresh");
}

#[test]
fn refresh_names_leaves_an_unjoined_members_name_as_is() {
    let mut activities = Activities::new();
    activities.create(member(1, 10, "kept")).expect("room");
    activities.refresh_names(&[]);
    let view = activities.iter().next().expect("one activity");
    assert_eq!(view.members[0].name, "kept");
}

#[test]
fn can_create_is_false_only_at_the_activity_bound() {
    let mut activities = Activities::new();
    for n in 0..MAX_ACTIVITIES {
        assert!(activities.can_create());
        let pid = u64::try_from(n).expect("small n");
        activities
            .create(member(u8::try_from(n).expect("small n"), pid, "a"))
            .expect("room");
    }
    assert!(!activities.can_create());
}

#[test]
fn id_at_and_group_index_of_fail_closed_on_an_out_of_range_query() {
    let activities = Activities::new();
    assert_eq!(activities.id_at(0), None);
    assert_eq!(activities.group_index_of(proc(1)), None);
}
