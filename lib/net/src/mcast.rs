//! The host multicast-membership engine, family-generic.
//!
//! A host that wishes to receive traffic for a multicast group must both
//! filter the group in on the receive path *and* announce its membership
//! to routers so the traffic is delivered to the link. IGMPv2 (RFC 2236,
//! IPv4) and MLDv2 (RFC 3810, IPv6) are the two announcement protocols;
//! their *host* behaviour — join, leave, and answering a router's query —
//! is one state machine, so this module defines it once and drives it
//! through a [`McastProtocol`] provider per family, exactly as
//! [`crate::neigh`] is one cache driven by ARP and Neighbour Discovery.
//!
//! The engine is pure and `now`-driven like every other stateful engine
//! here: membership calls mutate state and schedule timers, the caller
//! then drains due [`MembershipReport`]s with [`Membership::advance`] and
//! re-arms its one-shot timer from [`Membership::next_deadline`]. Turning
//! a report into a wire message is the [`crate::stack::Stack`]'s job (it
//! owns the family's [`crate::igmp`] / [`crate::mld`] framing); the engine
//! never touches the wire.
//!
//! Report timing is jittered to avoid a report implosion when many hosts
//! answer one query (RFC 2236 §3, RFC 3810 §6). The jitter is drawn from
//! a small non-cryptographic generator seeded from the interface's own
//! MAC, so two hosts pick different delays (which is all report
//! suppression needs) while the engine stays deterministic and replayable
//! for a given seed — it is *not* a security-sensitive draw (ISNs, ports,
//! and IDs use the CSPRNG elsewhere).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use rustos_abi::time::Duration64;

use crate::addr::{Ipv4Addr, Ipv6Addr, ALL_NODES};
use crate::timeutil::{from_nanos, nanos, NEVER};

/// Why a host is emitting a membership report.
///
/// The engine speaks these family-neutral reasons; the [`Stack`] maps
/// each to the concrete wire message its family uses (an IGMPv2
/// report/leave, an MLDv2 state-change or current-state record).
///
/// [`Stack`]: crate::stack::Stack
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportReason {
    /// A state change announcing the host has joined the group
    /// (IGMPv2 Membership Report; MLDv2 `CHANGE_TO_EXCLUDE` record).
    JoinGroup,
    /// A state change announcing the host has left the group
    /// (IGMPv2 Leave Group; MLDv2 `CHANGE_TO_INCLUDE` record).
    LeaveGroup,
    /// A current-state report answering a router's query
    /// (IGMPv2 Membership Report; MLDv2 `MODE_IS_EXCLUDE` record).
    QueryResponse,
}

/// One report the caller must transmit: the group and why.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MembershipReport<A> {
    /// The group the report concerns.
    pub group: A,
    /// The reason for the report.
    pub reason: ReportReason,
}

/// The per-family facts the generic engine needs: its timers, whether it
/// suppresses reports on hearing another host's, and which groups are
/// never reported.
pub trait McastProtocol {
    /// The multicast group address type of this family.
    type Addr: Copy + Ord;

    /// Robustness Variable: how many times an unsolicited state-change
    /// report is retransmitted (RFC 2236 §8.1 / RFC 3810 §9.1).
    const ROBUSTNESS: u32;

    /// Spacing between retransmissions of an unsolicited report
    /// (Unsolicited Report Interval).
    const UNSOLICITED_INTERVAL: Duration64;

    /// Response window assumed for a query that specifies none
    /// (Query Response Interval default).
    const DEFAULT_MAX_RESPONSE: Duration64;

    /// Whether a host cancels a pending query response on hearing
    /// another host report the same group (IGMPv2 yes, MLDv2 no).
    const SUPPRESSION: bool;

    /// Whether membership in `group` is announced at all. The all-hosts
    /// control groups are joined for reception but never reported
    /// (RFC 2236 §6 / RFC 3810 §6).
    fn is_reportable(group: &Self::Addr) -> bool;
}

/// IGMPv2 (RFC 2236) — the IPv4 provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Igmp;

impl McastProtocol for Igmp {
    type Addr = Ipv4Addr;
    const ROBUSTNESS: u32 = 2;
    const UNSOLICITED_INTERVAL: Duration64 = Duration64::from_secs(10);
    const DEFAULT_MAX_RESPONSE: Duration64 = Duration64::from_secs(10);
    const SUPPRESSION: bool = true;

    fn is_reportable(group: &Ipv4Addr) -> bool {
        // The 224.0.0.0/24 local-network-control block (all-systems,
        // all-routers, and their neighbours) is never reported.
        let o = group.octets();
        !(o[0] == 224 && o[1] == 0 && o[2] == 0)
    }
}

/// MLDv2 (RFC 3810) — the IPv6 provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mld;

impl McastProtocol for Mld {
    type Addr = Ipv6Addr;
    const ROBUSTNESS: u32 = 2;
    const UNSOLICITED_INTERVAL: Duration64 = Duration64::from_secs(1);
    const DEFAULT_MAX_RESPONSE: Duration64 = Duration64::from_secs(10);
    const SUPPRESSION: bool = false;

    fn is_reportable(group: &Ipv6Addr) -> bool {
        // The link-local all-nodes group is never reported.
        *group != ALL_NODES
    }
}

/// A refused membership operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinError {
    /// The bounded membership table is full; the join is refused rather
    /// than growing an attacker-influenced table without bound.
    CapacityExhausted,
}

/// A pending unsolicited state-change transmission (join or leave),
/// retransmitted [`McastProtocol::ROBUSTNESS`] times.
#[derive(Clone, Copy, Debug)]
struct StateChange {
    reason: ReportReason,
    remaining: u32,
    next_at: u128,
}

/// One group's membership record.
#[derive(Clone, Copy, Debug)]
struct Group {
    /// Join references (a group is joined once per independent joiner;
    /// leaving the last reference leaves the group).
    refs: u32,
    /// Deadline of a pending query-response report ([`NEVER`] when none).
    report_at: u128,
    /// A pending unsolicited state-change transmission, if any.
    change: Option<StateChange>,
}

impl Group {
    /// The earliest pending transition, or [`NEVER`].
    fn deadline(&self) -> u128 {
        let change = self.change.map_or(NEVER, |c| c.next_at);
        change.min(self.report_at)
    }
}

/// The bounded, family-generic host multicast-membership engine.
#[derive(Clone, Debug)]
pub struct Membership<P: McastProtocol> {
    groups: BTreeMap<P::Addr, Group>,
    capacity: usize,
    rng: u64,
}

impl<P: McastProtocol> Membership<P> {
    /// A membership table holding at most `capacity` groups, with report
    /// jitter seeded from `seed` (the interface MAC; see the module
    /// docs). A zero seed is replaced so the generator never sticks.
    #[must_use]
    pub fn new(capacity: usize, seed: u64) -> Self {
        Self {
            groups: BTreeMap::new(),
            capacity,
            rng: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// Number of groups currently joined (reference count above zero).
    #[must_use]
    pub fn len(&self) -> usize {
        self.groups.values().filter(|g| g.refs > 0).count()
    }

    /// True when no group is joined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The construction-time group bound.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// True when the host is a member of `group` (used to filter the
    /// receive path). A group draining its leave reports is no longer a
    /// member.
    #[must_use]
    pub fn is_member(&self, group: P::Addr) -> bool {
        self.groups.get(&group).is_some_and(|g| g.refs > 0)
    }

    /// Join `group` at time `now`.
    ///
    /// Returns `Ok(true)` when this is the first reference (the caller
    /// should expect state-change reports to flow from
    /// [`advance`](Self::advance)), `Ok(false)` when the group was
    /// already joined (only the reference count changed).
    ///
    /// # Errors
    ///
    /// [`JoinError::CapacityExhausted`] when a *new* group would exceed
    /// the table bound (fail closed). Re-joining an existing group never
    /// fails.
    pub fn join(&mut self, group: P::Addr, now: Duration64) -> Result<bool, JoinError> {
        if let Some(entry) = self.groups.get_mut(&group) {
            entry.refs = entry.refs.saturating_add(1);
            // A group re-joined while it was draining its leave reports
            // becomes a member again and re-announces its presence.
            if entry.refs == 1 {
                entry.change = state_change::<P>(ReportReason::JoinGroup, &group, now);
                entry.report_at = NEVER;
            }
            return Ok(false);
        }
        if self.groups.len() >= self.capacity {
            return Err(JoinError::CapacityExhausted);
        }
        self.groups.insert(
            group,
            Group {
                refs: 1,
                report_at: NEVER,
                change: state_change::<P>(ReportReason::JoinGroup, &group, now),
            },
        );
        Ok(true)
    }

    /// Leave `group` at time `now`.
    ///
    /// Returns `true` when the last reference was dropped (the host has
    /// left the group), `false` when the group is still referenced or
    /// was never joined.
    pub fn leave(&mut self, group: P::Addr, now: Duration64) -> bool {
        let Some(entry) = self.groups.get_mut(&group) else {
            return false;
        };
        if entry.refs == 0 {
            return false;
        }
        if entry.refs > 1 {
            entry.refs -= 1;
            return false;
        }
        entry.refs = 0;
        entry.report_at = NEVER;
        match state_change::<P>(ReportReason::LeaveGroup, &group, now) {
            // A reportable group announces its departure, then the record
            // is dropped once the leave reports drain (in `advance`).
            change @ Some(_) => entry.change = change,
            // A never-reported group (an all-hosts control group) simply
            // disappears.
            None => {
                self.groups.remove(&group);
            }
        }
        true
    }

    /// Handle a received Membership/Listener Query at time `now`.
    ///
    /// `group` is the queried group, or `None` for a General Query. A
    /// response is scheduled for every reportable member the query
    /// covers, after a random delay in `0..=max_response` (RFC 2236 §3 /
    /// RFC 3810 §6); a shorter already-pending response is kept.
    pub fn on_query(&mut self, group: Option<P::Addr>, max_response: Duration64, now: Duration64) {
        let window = if max_response == Duration64::ZERO {
            P::DEFAULT_MAX_RESPONSE
        } else {
            max_response
        };
        let window_nanos = nanos(window);
        let now_nanos = nanos(now);
        // Draw all jitter up front so the borrow of `self.groups` below
        // does not overlap the `&mut self` random draw.
        let targets: Vec<P::Addr> = match group {
            Some(g) => {
                if self.is_member(g) && P::is_reportable(&g) {
                    alloc::vec![g]
                } else {
                    Vec::new()
                }
            }
            None => self
                .groups
                .iter()
                .filter(|(g, entry)| entry.refs > 0 && P::is_reportable(g))
                .map(|(g, _)| *g)
                .collect(),
        };
        for target in targets {
            let deadline = now_nanos.saturating_add(self.jitter(window_nanos));
            if let Some(entry) = self.groups.get_mut(&target) {
                if entry.report_at == NEVER || deadline < entry.report_at {
                    entry.report_at = deadline;
                }
            }
        }
    }

    /// Note that another host reported `group`. Under a suppressing
    /// protocol (IGMPv2) this cancels our own pending response for the
    /// group, so a router hears only one report per group per link.
    pub fn on_report_seen(&mut self, group: P::Addr) {
        if !P::SUPPRESSION {
            return;
        }
        if let Some(entry) = self.groups.get_mut(&group) {
            entry.report_at = NEVER;
        }
    }

    /// Emit every report whose timer is due at `now`, advancing the
    /// retransmission and query-response timers.
    pub fn advance(&mut self, now: Duration64) -> Vec<MembershipReport<P::Addr>> {
        let now_nanos = nanos(now);
        let unsolicited = nanos(P::UNSOLICITED_INTERVAL);
        let mut reports = Vec::new();
        let mut drained = Vec::new();
        for (group, entry) in &mut self.groups {
            if let Some(change) = entry.change.as_mut() {
                if change.next_at <= now_nanos {
                    reports.push(MembershipReport {
                        group: *group,
                        reason: change.reason,
                    });
                    change.remaining -= 1;
                    if change.remaining == 0 {
                        let leaving = change.reason == ReportReason::LeaveGroup;
                        entry.change = None;
                        if leaving && entry.refs == 0 {
                            drained.push(*group);
                        }
                    } else {
                        change.next_at = now_nanos.saturating_add(unsolicited);
                    }
                }
            }
            if entry.report_at != NEVER && entry.report_at <= now_nanos {
                reports.push(MembershipReport {
                    group: *group,
                    reason: ReportReason::QueryResponse,
                });
                entry.report_at = NEVER;
            }
        }
        for group in drained {
            self.groups.remove(&group);
        }
        reports
    }

    /// The earliest pending report deadline across all groups.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let earliest = self
            .groups
            .values()
            .map(Group::deadline)
            .filter(|&d| d != NEVER)
            .min()?;
        Some(from_nanos(earliest))
    }

    /// Draw a jittered delay in `0..=max_nanos` from the non-crypto
    /// generator (see the module docs).
    fn jitter(&mut self, max_nanos: u128) -> u128 {
        if max_nanos == 0 {
            return 0;
        }
        // xorshift64: adequate, non-cryptographic spread for report jitter.
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        u128::from(x) % (max_nanos + 1)
    }
}

/// Build the initial state-change record for a `reason` on `group`, or
/// `None` when the group is never reported.
fn state_change<P: McastProtocol>(
    reason: ReportReason,
    group: &P::Addr,
    now: Duration64,
) -> Option<StateChange> {
    if !P::is_reportable(group) {
        return None;
    }
    Some(StateChange {
        reason,
        remaining: P::ROBUSTNESS,
        next_at: nanos(now),
    })
}

#[cfg(test)]
#[path = "mcast_tests.rs"]
mod tests;
