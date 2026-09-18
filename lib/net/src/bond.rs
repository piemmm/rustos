//! Link-aggregation (bond) engine (`plans/NETWORK.md` §6.3).
//!
//! A bond is a virtual interface `netstack` composes over two or more
//! member NICs: any driver serving the frame-ring seam participates with
//! zero driver changes, because aggregation is a stack construct, not a
//! device feature. This module is the pure, address-agnostic decision
//! core — which member should carry a transmit, when to fail over, and
//! when a peer must relearn the bond's location — exactly as [`neigh`]
//! and [`mcast`] are pure decision cores driven by injected time and
//! events. It owns no addresses, no routes, and no I/O; the composing
//! interface folds the checksums and drives the rings.
//!
//! # Health and failover
//!
//! Member health is link-state driven (the `DeviceFacts` link report over
//! the ring seam) with a deliberate anti-flap discipline:
//!
//! - A member that loses its link becomes ineligible **immediately**, so
//!   the transmit path fails over within one link-down report — never a
//!   polling delay.
//! - A member that regains its link is admitted only after it has been
//!   continuously up for one `monitor_interval` (the RFC-neutral
//!   equivalent of a bonding driver's up-delay). This is the "failback is
//!   deliberate, never flapping" rule: a recovered `primary` reclaims the
//!   transmit path one interval after it comes back, not the instant a
//!   flapping link reports up.
//!
//! The monitor is tickless: the one-shot deadline is armed only while a member
//! is up but not yet admitted, and is unarmed once the set is stable, so a
//! steady bond costs no timer wakeups.
//!
//! # Modes
//!
//! - [`BondMode::ActiveBackup`] — one transmitting member at a time, with
//!   ordered failover to the next eligible member. A declared `primary`
//!   makes it a deliberate failover interface.
//! - [`BondMode::Balance`] — a flow-hashed transmit spread across the
//!   eligible members: one flow stays on one member (so a TCP stream
//!   never reorders across links) while that member stays eligible.
//!
//! [`neigh`]: crate::neigh
//! [`mcast`]: crate::mcast

use alloc::vec::Vec;
use core::hash::Hasher;

use tairix_abi::driver::net::LinkState;
use tairix_abi::time::Duration64;
use tairix_hash::{HashSeed, SipHash13};

use crate::timeutil::{from_nanos, nanos, NEVER};

/// Largest number of member NICs a single bond aggregates.
///
/// The single definition is the `netstack-v1` wire bound
/// ([`tairix_abi::net_ipc::NET_BOND_MAX_MEMBERS`]); this engine, the
/// `network.conf` grammar (`lib/netconfig`), and the bond-configuration
/// message all key to it, so the engine, the store, and the wire can never
/// disagree on the limit.
pub const MAX_BOND_MEMBERS: usize = tairix_abi::net_ipc::NET_BOND_MAX_MEMBERS;

/// The stable identity of a bond member: the member interface's name, as
/// the composing interface table keys it. The engine treats it as an
/// opaque token — it never interprets the bytes.
pub type MemberId = [u8; 16];

/// A bond's transmit policy (`plans/NETWORK.md` §6.3).
///
/// A closed set; LACP/802.3ad is a future in-place extension, not speculated
/// here.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub enum BondMode {
    /// One transmitting member at a time, with ordered failover to the
    /// next eligible member (the default).
    #[default]
    ActiveBackup,
    /// Flow-hashed transmit spread across the eligible members.
    Balance,
}

/// A refusal from a [`Bond`] membership mutation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BondError {
    /// Enrolling this member would exceed [`MAX_BOND_MEMBERS`].
    TooManyMembers,
    /// A member with this id is already enrolled.
    DuplicateMember,
    /// No member with this id is enrolled.
    UnknownMember,
}

/// An observable transition the composing interface must act on.
///
/// A mutation recomputes the selection once, so it produces **at most one**
/// of these. The two announcing variants prompt the interface to emit a
/// gratuitous ARP / unsolicited Neighbour Advertisement so peers learn which
/// member carries the bond's MAC; [`BondEvent::WentDown`] carries no
/// gratuitous traffic — there is no member to send it on.
///
/// [`BondEvent::CameUp`] is distinct from [`BondEvent::PathChanged`]: a bond
/// acquiring its first eligible member has no previous path to fail over
/// from, so conflating the two makes a healthy bond indistinguishable from a
/// degraded one to anything keying on a failover.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BondEvent {
    /// The bond acquired its first eligible member: it can transmit where
    /// before it could not. Peers must learn the path to the bond's MAC.
    CameUp,
    /// The transmit path changed while the bond could **already** transmit:
    /// the active member changed (active-backup) or the eligible set changed
    /// (balance). A member dying is a failover; a recovered `primary`
    /// reclaiming the path is a deliberate failback. Peers must relearn the
    /// path to the bond's MAC.
    PathChanged,
    /// The bond lost its last eligible member; transmit now fails closed
    /// until a member recovers.
    WentDown,
}

/// One enrolled member NIC and its committed health.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Member {
    /// The member interface's stable name.
    id: MemberId,
    /// Last link-state report from the member's device.
    link_up: bool,
    /// Whether the member has been admitted to carry traffic. A link-up
    /// member is admitted only after it has been continuously up for one
    /// monitor interval (the anti-flap up-delay); a link-down member is
    /// un-admitted immediately.
    admitted: bool,
    /// Nanosecond instant the member last became link-up, or [`NEVER`]
    /// when it is down. The admission deadline is this plus the monitor
    /// interval.
    up_since: u128,
}

/// Immutable construction parameters for a [`Bond`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BondConfig {
    /// The transmit policy.
    pub mode: BondMode,
    /// The member-health monitor interval — the anti-flap up-delay a
    /// recovered member must stay up for before it is readmitted.
    pub monitor_interval: Duration64,
    /// The member that reclaims the transmit path whenever it is eligible
    /// (`active-backup` failover-interface semantics). `None` leaves the
    /// current active in place until it fails.
    pub primary: Option<MemberId>,
}

/// The pure link-aggregation decision core.
///
/// Construct with [`Bond::new`], enrol members with [`Bond::add_member`],
/// feed link reports through [`Bond::set_member_link`], advance the
/// monitor with [`Bond::advance`], and query the transmit member with
/// [`Bond::transmit_member`]. Every mutation returns the one
/// [`BondEvent`] it produced, if any; the engine performs no I/O.
#[derive(Clone, Debug)]
pub struct Bond {
    mode: BondMode,
    monitor_interval: u128,
    primary: Option<MemberId>,
    members: Vec<Member>,
    /// The current transmitting member in [`BondMode::ActiveBackup`].
    /// Unused (always `None`) in [`BondMode::Balance`], which spreads
    /// across the whole eligible ring.
    active: Option<MemberId>,
    /// The current eligible members, in enrolment order, used as the
    /// flow-hash ring in [`BondMode::Balance`]. Also the diff basis for
    /// balance-mode path-change detection.
    ring: Vec<MemberId>,
}

impl Bond {
    /// Construct an empty bond with the given policy.
    #[must_use]
    pub fn new(config: &BondConfig) -> Self {
        Self {
            mode: config.mode,
            monitor_interval: nanos(config.monitor_interval),
            primary: config.primary,
            members: Vec::new(),
            active: None,
            ring: Vec::new(),
        }
    }

    /// Enrol a member NIC. New members start link-down and ineligible;
    /// feed a link report through [`Bond::set_member_link`] to bring one
    /// into service.
    ///
    /// # Errors
    ///
    /// [`BondError::TooManyMembers`] past [`MAX_BOND_MEMBERS`];
    /// [`BondError::DuplicateMember`] if `id` is already enrolled.
    pub fn add_member(&mut self, id: MemberId) -> Result<(), BondError> {
        if self.members.iter().any(|m| m.id == id) {
            return Err(BondError::DuplicateMember);
        }
        if self.members.len() >= MAX_BOND_MEMBERS {
            return Err(BondError::TooManyMembers);
        }
        self.members.push(Member {
            id,
            link_up: false,
            admitted: false,
            up_since: NEVER,
        });
        Ok(())
    }

    /// Remove a member, returning the transmit-path transition it caused.
    ///
    /// # Errors
    ///
    /// [`BondError::UnknownMember`] if `id` is not enrolled.
    pub fn remove_member(&mut self, id: MemberId) -> Result<Option<BondEvent>, BondError> {
        let index = self
            .members
            .iter()
            .position(|m| m.id == id)
            .ok_or(BondError::UnknownMember)?;
        self.members.remove(index);
        Ok(self.recompute())
    }

    /// Record a member's link-state report and recompute the transmit
    /// path. A link-down report un-admits the member immediately (fast
    /// failover); a link-up report starts its anti-flap up-delay and does
    /// not admit it until [`Bond::advance`] confirms the interval elapsed.
    ///
    /// An unknown `id` is ignored (no member, no change), so a stale
    /// report from a removed member is harmless.
    pub fn set_member_link(
        &mut self,
        id: MemberId,
        link: LinkState,
        now: Duration64,
    ) -> Option<BondEvent> {
        let now_nanos = nanos(now);
        let member = self.members.iter_mut().find(|m| m.id == id)?;
        match link {
            LinkState::Up => {
                if !member.link_up {
                    member.link_up = true;
                    member.up_since = now_nanos;
                    // Admission waits for the monitor interval; a member
                    // that reports up is not yet trusted to carry traffic.
                }
            }
            LinkState::Down => {
                member.link_up = false;
                member.admitted = false;
                member.up_since = NEVER;
            }
        }
        self.recompute()
    }

    /// Advance the health monitor: admit members whose up-delay has
    /// elapsed and recompute the transmit path. Returns the transition it
    /// caused — a [`BondEvent::CameUp`] on the first admission, a
    /// [`BondEvent::PathChanged`] when a readmitted member reclaims the
    /// path. Re-arm the one-shot monitor timer from
    /// [`Bond::next_deadline`] after calling this.
    pub fn advance(&mut self, now: Duration64) -> Option<BondEvent> {
        let now_nanos = nanos(now);
        for member in &mut self.members {
            if member.link_up
                && !member.admitted
                && now_nanos.saturating_sub(member.up_since) >= self.monitor_interval
            {
                member.admitted = true;
            }
        }
        self.recompute()
    }

    /// The next instant [`Bond::advance`] has admission work to do — the
    /// earliest pending up-delay expiry — or `None` when the member set is
    /// stable (tickless: no member is waiting to be admitted).
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        self.members
            .iter()
            .filter(|m| m.link_up && !m.admitted && m.up_since != NEVER)
            .map(|m| m.up_since.saturating_add(self.monitor_interval))
            .min()
            .map(from_nanos)
    }

    /// The member that should carry a transmit for the given flow hash, or
    /// `None` when the bond has no eligible member (transmit fails closed).
    ///
    /// In [`BondMode::ActiveBackup`] the single active member carries
    /// every flow (the hash is ignored); in [`BondMode::Balance`] the flow
    /// hash selects one member from the eligible ring so a given flow
    /// always stays on one member while that member remains eligible.
    #[must_use]
    pub fn transmit_member(&self, flow_hash: u64) -> Option<MemberId> {
        match self.mode {
            BondMode::ActiveBackup => self.active,
            BondMode::Balance => {
                if self.ring.is_empty() {
                    None
                } else {
                    let index = usize::try_from(flow_hash % self.ring.len() as u64).unwrap_or(0);
                    self.ring.get(index).copied()
                }
            }
        }
    }

    /// The single active member in [`BondMode::ActiveBackup`], for the
    /// `state:net/<bond>/active-member` observability read. `None` in
    /// [`BondMode::Balance`] (which has no single active member — read the
    /// per-member eligibility instead) and when the bond is down.
    #[must_use]
    pub fn active_member(&self) -> Option<MemberId> {
        match self.mode {
            BondMode::ActiveBackup => self.active,
            BondMode::Balance => None,
        }
    }

    /// The bond's transmit policy.
    #[must_use]
    pub fn mode(&self) -> BondMode {
        self.mode
    }

    /// The configured `primary` member, if any.
    #[must_use]
    pub fn primary(&self) -> Option<MemberId> {
        self.primary
    }

    /// Every enrolled member's id, in enrolment order.
    #[must_use]
    pub fn member_ids(&self) -> Vec<MemberId> {
        self.members.iter().map(|m| m.id).collect()
    }

    /// The number of members currently eligible to carry traffic.
    #[must_use]
    pub fn eligible_count(&self) -> usize {
        self.members.iter().filter(|m| m.admitted).count()
    }

    /// Whether the bond has at least one eligible member (can transmit).
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.members.iter().any(|m| m.admitted)
    }

    /// Whether the named member is currently eligible to carry traffic,
    /// or `None` if it is not enrolled.
    #[must_use]
    pub fn is_member_eligible(&self, id: MemberId) -> Option<bool> {
        self.members.iter().find(|m| m.id == id).map(|m| m.admitted)
    }

    /// Whether the named member's link is currently up, or `None` if it is
    /// not enrolled.
    #[must_use]
    pub fn is_member_link_up(&self, id: MemberId) -> Option<bool> {
        self.members.iter().find(|m| m.id == id).map(|m| m.link_up)
    }

    /// Change the transmit policy at runtime (config reload). Returns the
    /// transmit-path transition it caused.
    pub fn set_mode(&mut self, mode: BondMode) -> Option<BondEvent> {
        if self.mode == mode {
            return None;
        }
        // Captured before the reset below: clearing the selection state
        // would otherwise make a switch on a transmitting bond read as a
        // bring-up rather than the path change it is.
        let could_transmit = self.can_transmit();
        self.mode = mode;
        // Reset the per-mode selection state so the diff in `recompute`
        // reflects the new policy from a clean slate.
        self.active = None;
        self.ring.clear();
        self.recompute_from(could_transmit)
    }

    /// Change the configured `primary` at runtime (config reload). Returns
    /// the transmit-path transition it caused.
    pub fn set_primary(&mut self, primary: Option<MemberId>) -> Option<BondEvent> {
        self.primary = primary;
        self.recompute()
    }

    /// Change the health-monitor interval at runtime (config reload). The
    /// new interval governs future admissions (a member already awaiting
    /// readmission keeps counting from when it came up); it changes no
    /// committed health, so it emits no events. Re-arm the one-shot monitor
    /// timer from [`Bond::next_deadline`] after calling this.
    pub fn set_monitor_interval(&mut self, monitor_interval: Duration64) {
        self.monitor_interval = nanos(monitor_interval);
    }

    /// Recompute the transmit selection from the committed member health
    /// and name the transition it produced. The single point where
    /// selection policy lives, so both modes and every mutator share one
    /// definition.
    fn recompute(&mut self) -> Option<BondEvent> {
        let could_transmit = self.can_transmit();
        self.recompute_from(could_transmit)
    }

    /// [`Self::recompute`] against an explicit "could transmit before"
    /// basis, for [`Self::set_mode`], which must clear the per-mode
    /// selection state before the new policy is applied to it.
    fn recompute_from(&mut self, could_transmit: bool) -> Option<BondEvent> {
        let new_ring: Vec<MemberId> = self
            .members
            .iter()
            .filter(|m| m.admitted)
            .map(|m| m.id)
            .collect();
        let selection_moved = match self.mode {
            BondMode::ActiveBackup => {
                let new_active = self.select_active(&new_ring);
                let moved = new_active != self.active;
                self.active = new_active;
                self.ring = new_ring;
                moved
            }
            BondMode::Balance => {
                let moved = new_ring != self.ring;
                self.ring = new_ring;
                moved
            }
        };
        if !selection_moved {
            return None;
        }
        Self::transition(could_transmit, self.can_transmit())
    }

    /// Whether the current selection can carry a transmit, under the
    /// current mode — the state each [`BondEvent`] is a transition of.
    fn can_transmit(&self) -> bool {
        match self.mode {
            BondMode::ActiveBackup => self.active.is_some(),
            BondMode::Balance => !self.ring.is_empty(),
        }
    }

    /// Name the transition between two transmit-capability states. Total in
    /// both arguments, so a selection that moved while the bond could not
    /// transmit either side of it reports nothing rather than faulting.
    fn transition(could_transmit: bool, can_transmit: bool) -> Option<BondEvent> {
        match (could_transmit, can_transmit) {
            (false, true) => Some(BondEvent::CameUp),
            (true, false) => Some(BondEvent::WentDown),
            (true, true) => Some(BondEvent::PathChanged),
            (false, false) => None,
        }
    }

    /// Choose the active member in [`BondMode::ActiveBackup`]: a declared,
    /// eligible `primary` always reclaims the path (deliberate failback);
    /// otherwise the current active is kept while it stays eligible (no
    /// needless path change); otherwise the first eligible member in
    /// enrolment order (ordered failover); otherwise `None`.
    fn select_active(&self, eligible: &[MemberId]) -> Option<MemberId> {
        if let Some(primary) = self.primary {
            if eligible.contains(&primary) {
                return Some(primary);
            }
        }
        if let Some(active) = self.active {
            if eligible.contains(&active) {
                return Some(active);
            }
        }
        eligible.first().copied()
    }
}

/// A deterministic flow hash over a transport 4-tuple, for
/// [`BondMode::Balance`] member selection. The address octets are passed
/// as opaque byte slices so the hash is address-family agnostic (the
/// caller supplies v4 or v6 octets); the same 4-tuple always yields the
/// same value under one `key`, so a flow stays on one member.
///
/// Keyed, because the tuple is a remote peer's to choose: with a predictable
/// hash an attacker computes tuples that all select one member and lands
/// every flow of an attack on a single link, which is the balance the bond
/// exists to provide. `key` is the process's own hash key, so it is a value
/// the peer cannot observe.
#[must_use]
pub fn flow_hash(key: HashSeed, src: &[u8], dst: &[u8], src_port: u16, dst_port: u16) -> u64 {
    let mut hasher = SipHash13::new(key);
    hasher.write(src);
    hasher.write(dst);
    hasher.write_u16(src_port);
    hasher.write_u16(dst_port);
    hasher.finish()
}

#[cfg(test)]
#[path = "bond_tests.rs"]
mod tests;
