//! Activity grouping: named groups of live processes that move, pause, and
//! close together (`plans/NEW-TASKBAR.md` T12).
//!
//! An activity exists only as long as this service instance runs: it is a
//! grouping of the *live* processes the service is watching right now, keyed
//! by their never-reused [`ProcId`], never anything persisted to disk. A
//! process belongs to at most one activity at a time; grouping it into a
//! different one moves it, and a group whose last member leaves — because it
//! was moved out or the process itself exited — dissolves rather than
//! lingering empty.
//!
//! The bounds below ([`MAX_ACTIVITIES`], [`MAX_ACTIVITY_MEMBERS`],
//! [`ACTIVITY_NAME_MAX`]) are UI-scale validation bounds on an
//! interactively-built structure, not a growable resource capacity: a human
//! grouping their own open tasks has no use for more than a handful of named
//! activities, so every mutation that would exceed one fails closed with a
//! typed [`Errno`] rather than growing without limit.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{Errno, ProcId};

use crate::model::display_name;
use crate::sample::ProcessSummary;

/// The most activities this service will track at once.
pub const MAX_ACTIVITIES: usize = 12;

/// The most processes a single activity will hold at once.
pub const MAX_ACTIVITY_MEMBERS: usize = 32;

/// The longest an activity's trimmed display name may be, in characters.
pub const ACTIVITY_NAME_MAX: usize = 48;

/// One process grouped into an activity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Member {
    /// The process's stable, never-reused instance identity.
    pub proc_id: ProcId,
    /// The process's scheduler task id, for the actions that signal it.
    pub pid: u64,
    /// A display copy of the process's name, refreshed whenever this member
    /// is (re-)joined so a stale reading is never shown after a rename.
    pub name: String,
}

/// One named group of processes, never empty while it exists.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Group {
    id: u64,
    name: String,
    paused: bool,
    members: Vec<Member>,
}

/// One activity as the model builder reads it.
#[derive(Copy, Clone, Debug)]
pub struct ActivityView<'a> {
    /// The activity's stable identity, independent of its position in the
    /// list.
    pub id: u64,
    /// The activity's display name.
    pub name: &'a str,
    /// Whether the activity is currently paused.
    pub paused: bool,
    /// The activity's members, in join order.
    pub members: &'a [Member],
}

/// The service-held grouping state: every activity this instance currently
/// tracks, and the never-reused id the next one created will take.
///
/// This is session-lifetime, in-memory state: a live view of live processes
/// has no meaningful persistence, so it is rebuilt empty on every service
/// start rather than saved and reloaded.
#[derive(Clone, Debug)]
pub struct Activities {
    groups: Vec<Group>,
    next_id: u64,
}

impl Default for Activities {
    fn default() -> Self {
        Self::new()
    }
}

impl Activities {
    /// No activities yet; the first one created takes id `1`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            groups: Vec::new(),
            next_id: 1,
        }
    }

    /// How many activities currently exist.
    #[must_use]
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether no activity currently exists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Whether another activity may still be created
    /// ([`SwitchboardModel::can_create_activity`](crate::view::SwitchboardModel::can_create_activity)).
    #[must_use]
    pub fn can_create(&self) -> bool {
        self.groups.len() < MAX_ACTIVITIES
    }

    /// The activity id at `index`, or `None` for an out-of-range index (fail
    /// closed).
    #[must_use]
    pub fn id_at(&self, index: usize) -> Option<u64> {
        self.groups.get(index).map(|group| group.id)
    }

    /// The index of the activity `proc_id` currently belongs to, or `None`
    /// when it is ungrouped.
    #[must_use]
    pub fn group_index_of(&self, proc_id: ProcId) -> Option<usize> {
        self.groups
            .iter()
            .position(|group| group.members.iter().any(|member| member.proc_id == proc_id))
    }

    /// Iterate every activity, in stable creation-then-mutation order — the
    /// same order the model builder renders them in.
    pub fn iter(&self) -> impl Iterator<Item = ActivityView<'_>> {
        self.groups.iter().map(|group| ActivityView {
            id: group.id,
            name: group.name.as_str(),
            paused: group.paused,
            members: group.members.as_slice(),
        })
    }

    /// Detach `proc_id` from whichever activity currently holds it,
    /// dissolving that activity if it becomes empty. A never-grouped id is a
    /// no-op.
    fn detach(&mut self, proc_id: ProcId) {
        let Some(index) = self.group_index_of(proc_id) else {
            return;
        };
        self.groups[index]
            .members
            .retain(|member| member.proc_id != proc_id);
        if self.groups[index].members.is_empty() {
            self.groups.remove(index);
        }
    }

    /// Create a new activity containing exactly `first`, auto-named
    /// `"Activity {id}"` (unique by construction, since ids are never
    /// reused).
    ///
    /// `first` is detached from any activity it already belongs to first, so
    /// the single-membership invariant holds even when the caller grouped an
    /// already-grouped task into a fresh activity.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when [`MAX_ACTIVITIES`] activities already
    /// exist.
    pub fn create(&mut self, first: Member) -> Result<u64, Errno> {
        if self.groups.len() >= MAX_ACTIVITIES {
            return Err(Errno::OutOfRange);
        }
        self.detach(first.proc_id);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.groups.push(Group {
            id,
            name: format!("Activity {id}"),
            paused: false,
            members: alloc::vec![first],
        });
        Ok(id)
    }

    /// Assign `member` to the activity at `index`, moving it out of any
    /// other activity it currently belongs to (dissolving that one if it
    /// empties). Idempotent when `member` already belongs to `index`: its
    /// stored copy is simply refreshed.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an out-of-range `index`, or when `index`'s
    /// activity is already at [`MAX_ACTIVITY_MEMBERS`] and `member` is not
    /// already one of them.
    pub fn assign(&mut self, index: usize, member: Member) -> Result<(), Errno> {
        let target_id = self.groups.get(index).ok_or(Errno::OutOfRange)?.id;
        let target = self
            .groups
            .iter_mut()
            .find(|group| group.id == target_id)
            .expect("just looked up by index");
        if let Some(existing) = target
            .members
            .iter_mut()
            .find(|existing| existing.proc_id == member.proc_id)
        {
            *existing = member;
            return Ok(());
        }
        if target.members.len() >= MAX_ACTIVITY_MEMBERS {
            return Err(Errno::OutOfRange);
        }
        // `member` is not a member of `target` (checked above), so detaching
        // it from wherever else it may be cannot dissolve `target` itself —
        // the group we are about to push into is guaranteed to survive.
        self.detach(member.proc_id);
        let target = self
            .groups
            .iter_mut()
            .find(|group| group.id == target_id)
            .expect("target cannot vanish: it never contained the detached member");
        target.members.push(member);
        Ok(())
    }

    /// Remove the process `proc_id` from its activity, if any, dissolving
    /// the activity if it empties. Returns whether it had been grouped.
    pub fn unassign(&mut self, proc_id: ProcId) -> bool {
        let had = self.group_index_of(proc_id).is_some();
        self.detach(proc_id);
        had
    }

    /// Rename the activity at `index` to `name`, trimmed of ASCII
    /// whitespace.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an out-of-range `index`;
    /// [`Errno::LengthOutOfRange`] for a name that is empty after trimming or
    /// longer than [`ACTIVITY_NAME_MAX`] characters (rejected, never
    /// truncated); [`Errno::AlreadyExists`] when another activity already
    /// carries the trimmed name (renaming to the activity's own current name
    /// is allowed).
    pub fn rename(&mut self, index: usize, name: &str) -> Result<(), Errno> {
        let trimmed = name.trim_matches(|ch: char| ch.is_ascii_whitespace());
        if trimmed.is_empty() || trimmed.chars().count() > ACTIVITY_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if index >= self.groups.len() {
            return Err(Errno::OutOfRange);
        }
        let collides = self
            .groups
            .iter()
            .enumerate()
            .any(|(other, group)| other != index && group.name == trimmed);
        if collides {
            return Err(Errno::AlreadyExists);
        }
        self.groups[index].name = String::from(trimmed);
        Ok(())
    }

    /// Set the activity at `index`'s paused flag. Returns whether `index`
    /// named a real activity (fail closed on an out-of-range one).
    pub fn set_paused(&mut self, index: usize, paused: bool) -> bool {
        let Some(group) = self.groups.get_mut(index) else {
            return false;
        };
        group.paused = paused;
        true
    }

    /// Remove the activity at `index`, returning its members. `None` for an
    /// out-of-range index (fail closed).
    pub fn close(&mut self, index: usize) -> Option<Vec<Member>> {
        if index >= self.groups.len() {
            return None;
        }
        Some(self.groups.remove(index).members)
    }

    /// Prune every member not present in `live`, dissolving any activity
    /// this empties.
    ///
    /// Callers must call this only on a sample whose process list actually
    /// succeeded: a degraded, honestly-empty process list must never be
    /// mistaken for "every process exited", which would wipe every activity
    /// on a transient `sysinfo` failure.
    pub fn retain_live(&mut self, live: &BTreeSet<ProcId>) {
        self.groups.retain_mut(|group| {
            group
                .members
                .retain(|member| live.contains(&member.proc_id));
            !group.members.is_empty()
        });
    }

    /// Refresh every joined member's stored display name from `processes`,
    /// the sample just gathered. A member not found in `processes` keeps its
    /// last-known name (this is not the pruning pass; that is
    /// [`Self::retain_live`]).
    pub fn refresh_names(&mut self, processes: &[ProcessSummary]) {
        for group in &mut self.groups {
            for member in &mut group.members {
                if let Some(process) = processes
                    .iter()
                    .find(|process| process.proc_id == member.proc_id)
                {
                    member.name = display_name(&process.name);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "activities_tests.rs"]
mod tests;
