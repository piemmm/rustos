//! The neighbour cache: one bounded RFC 4861 §7.3.2 state machine.
//!
//! Both address-resolution protocols drive this one table: ARP for IPv4
//! (RFC 826, with the cache semantics RFC 1122 §2.3.2.1 requires) and
//! Neighbour Discovery for IPv6 (RFC 4861). The providers differ only in
//! the wire messages they parse; the states, transitions, timers, and
//! bounds live here once, so the two families cannot drift.
//!
//! # Design
//!
//! The table is pure and deterministic: every method takes `now`
//! explicitly (a monotonic [`Duration64`] since an arbitrary epoch) and
//! I/O is expressed as returned *actions* ([`NeighborAction`]) the caller
//! performs — send a solicitation, treat the neighbour as unreachable.
//! The caller owns the timer: [`NeighborTable::next_deadline`] reports
//! when the earliest timed transition is due and
//! [`NeighborTable::advance`] performs every transition that `now` has
//! reached, returning the actions they produced. A lookup that creates
//! an entry leaves its first solicitation due immediately, so all
//! outbound traffic flows through the one `advance` channel.
//!
//! # Security
//!
//! The cache is a spoofing target, so it is bounded and conservative:
//!
//! - Fixed capacity, chosen at construction. When full, insertion evicts
//!   the least-recently-used entry that is not mid-resolution; if every
//!   entry is mid-resolution the insert is refused (fail closed) rather
//!   than evicting state an attacker could churn.
//! - A confirmation ([`NeighborTable::confirm`]) for an address with no
//!   entry is ignored — an unsolicited reply never creates state.
//! - Only [`NeighborTable::lookup`] (this host is sending) and
//!   [`NeighborTable::learn`] (a solicitation carrying the peer's own
//!   binding was received) create entries.

use alloc::vec::Vec;

use rustos_abi::driver::net::MacAddress;
use rustos_abi::time::{Duration64, NANOS_PER_SEC};

use crate::addr::IpAddr;

/// Reachability state of one cache entry, per RFC 4861 §7.3.2.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NeighborState {
    /// Address resolution is in progress; no link-layer address yet.
    Incomplete,
    /// A reachability confirmation arrived within the reachable window.
    Reachable,
    /// The link-layer address is known but its freshness has lapsed.
    Stale,
    /// Traffic was sent to a stale entry; probing is deferred briefly to
    /// give upper-layer confirmation a chance to arrive.
    Delay,
    /// Unicast reachability probes are being sent.
    Probe,
}

/// What the caller must do as a consequence of a table transition.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NeighborAction {
    /// Send a multicast (or broadcast, for ARP) solicitation for `ip`.
    SolicitMulticast {
        /// The address being resolved.
        ip: IpAddr,
    },
    /// Send a unicast reachability probe to `mac` asking about `ip`.
    SolicitUnicast {
        /// The address being probed.
        ip: IpAddr,
        /// The cached link-layer address to probe at.
        mac: MacAddress,
    },
    /// Resolution or probing exhausted its retries; the entry is gone.
    /// The caller surfaces unreachability (and flushes queued packets).
    Unreachable {
        /// The address that failed resolution.
        ip: IpAddr,
    },
}

/// Outcome of a [`NeighborTable::lookup`] by a sender.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LookupResult {
    /// Transmit to this link-layer address now.
    Send(MacAddress),
    /// Resolution is in progress; queue the packet and wait.
    Pending,
    /// The table is full of mid-resolution entries; the packet is
    /// refused (fail closed), not queued.
    TableFull,
}

/// Timer and retry parameters, per RFC 4861 §10 defaults.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NeighborConfig {
    /// How long a confirmation keeps an entry `Reachable`.
    pub reachable_time: Duration64,
    /// Delay before the first unicast probe after `Delay` entry.
    pub delay_first_probe: Duration64,
    /// Interval between solicitations/probes.
    pub retrans_timer: Duration64,
    /// Multicast solicitations before resolution fails.
    pub max_multicast_solicit: u8,
    /// Unicast probes before an entry is discarded.
    pub max_unicast_solicit: u8,
}

impl Default for NeighborConfig {
    fn default() -> Self {
        Self {
            reachable_time: Duration64::from_secs(30),
            delay_first_probe: Duration64::from_secs(5),
            retrans_timer: Duration64::from_secs(1),
            max_multicast_solicit: 3,
            max_unicast_solicit: 3,
        }
    }
}

/// Nanoseconds of a non-negative monotonic duration.
fn nanos(d: Duration64) -> u128 {
    // Monotonic time never goes negative; clamp defensively so a
    // malformed input saturates instead of wrapping.
    let secs = u128::try_from(d.secs()).unwrap_or(0);
    secs * u128::from(NANOS_PER_SEC) + u128::from(d.subsec_nanos())
}

/// Deadline value meaning "no timed transition pending".
const NEVER: u128 = u128::MAX;

/// Reachability of a resolved entry — [`NeighborState`] minus
/// `Incomplete`, so "resolved but addressless" is unrepresentable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Reach {
    Reachable,
    Stale,
    Delay,
    Probe,
}

/// Resolution phase of one entry: either resolving (no link-layer
/// address exists yet) or resolved (an address always exists).
#[derive(Copy, Clone, Debug)]
enum Phase {
    Incomplete,
    Resolved { mac: MacAddress, reach: Reach },
}

/// One cache entry.
#[derive(Copy, Clone, Debug)]
struct Entry {
    ip: IpAddr,
    phase: Phase,
    /// Next timed transition, in nanoseconds ([`NEVER`] when the state
    /// has none).
    deadline: u128,
    /// Solicitations/probes sent in the current state.
    attempts: u8,
    /// Last use by a sender, for LRU eviction.
    last_used: u128,
}

/// The bounded, provider-agnostic neighbour cache.
///
/// See the [module docs](self) for the driving contract. The intended
/// caller pattern per received event or send attempt is: call the
/// event's method ([`lookup`](Self::lookup), [`confirm`](Self::confirm),
/// [`learn`](Self::learn), …), then call [`advance`](Self::advance) and
/// perform the returned actions, then re-arm the caller's one-shot timer
/// from [`next_deadline`](Self::next_deadline).
#[derive(Clone, Debug)]
pub struct NeighborTable {
    entries: Vec<Entry>,
    capacity: usize,
    config: NeighborConfig,
}

impl NeighborTable {
    /// A table holding at most `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize, config: NeighborConfig) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
            config,
        }
    }

    /// Number of live entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the table holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The construction-time entry bound.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Current state and cached link-layer address of `ip`'s entry.
    #[must_use]
    pub fn entry(&self, ip: IpAddr) -> Option<(NeighborState, Option<MacAddress>)> {
        self.find(ip).map(|i| match self.entries[i].phase {
            Phase::Incomplete => (NeighborState::Incomplete, None),
            Phase::Resolved { mac, reach } => {
                let state = match reach {
                    Reach::Reachable => NeighborState::Reachable,
                    Reach::Stale => NeighborState::Stale,
                    Reach::Delay => NeighborState::Delay,
                    Reach::Probe => NeighborState::Probe,
                };
                (state, Some(mac))
            }
        })
    }

    /// Resolve `ip` for transmission at time `now`.
    ///
    /// Creates an `Incomplete` entry (due for its first solicitation on
    /// the next [`advance`](Self::advance)) when none exists, and drives
    /// the RFC 4861 §7.3.3 `Reachable`→`Stale`→`Delay` staleness
    /// transitions for entries that do. A `Stale` entry's address is
    /// still returned for transmission ("last known good") while
    /// reachability is re-verified.
    pub fn lookup(&mut self, ip: IpAddr, now: Duration64) -> LookupResult {
        let now = nanos(now);
        let delay = nanos(self.config.delay_first_probe);
        let Some(index) = self.find(ip) else {
            if !self.insert(Entry {
                ip,
                phase: Phase::Incomplete,
                // Due immediately: the next `advance` emits the first
                // multicast solicitation.
                deadline: now,
                attempts: 0,
                last_used: now,
            }) {
                return LookupResult::TableFull;
            }
            return LookupResult::Pending;
        };
        let Entry {
            phase,
            deadline,
            attempts,
            last_used,
            ..
        } = &mut self.entries[index];
        *last_used = now;
        let Phase::Resolved { mac, reach } = phase else {
            return LookupResult::Pending;
        };
        if *reach == Reach::Reachable && now >= *deadline {
            *reach = Reach::Stale;
            *deadline = NEVER;
        }
        if *reach == Reach::Stale {
            *reach = Reach::Delay;
            *deadline = now + delay;
            *attempts = 0;
        }
        LookupResult::Send(*mac)
    }

    /// Process a reachability confirmation for `ip` claiming `mac` —
    /// an ARP reply, or a Neighbour Advertisement with its `solicited`
    /// and `override` flags (RFC 4861 §7.2.5).
    ///
    /// A confirmation for an address with no entry is ignored: an
    /// unsolicited reply never creates cache state.
    pub fn confirm(
        &mut self,
        ip: IpAddr,
        mac: MacAddress,
        solicited: bool,
        is_override: bool,
        now: Duration64,
    ) {
        let now = nanos(now);
        let reachable = nanos(self.config.reachable_time);
        let Some(index) = self.find(ip) else {
            return;
        };
        let entry = &mut self.entries[index];
        match entry.phase {
            Phase::Incomplete => {
                let reach = if solicited {
                    Reach::Reachable
                } else {
                    Reach::Stale
                };
                entry.phase = Phase::Resolved { mac, reach };
                entry.deadline = if solicited { now + reachable } else { NEVER };
                entry.attempts = 0;
            }
            Phase::Resolved { mac: cached, reach } => {
                let changed = cached != mac;
                if changed && !is_override {
                    // A non-override advertisement carrying a different
                    // address only degrades a Reachable entry to Stale
                    // (RFC 4861 §7.2.5); the cached address is kept.
                    if reach == Reach::Reachable {
                        entry.phase = Phase::Resolved {
                            mac: cached,
                            reach: Reach::Stale,
                        };
                        entry.deadline = NEVER;
                    }
                    return;
                }
                let reach = if solicited {
                    Reach::Reachable
                } else if changed {
                    Reach::Stale
                } else {
                    reach
                };
                entry.phase = Phase::Resolved { mac, reach };
                if solicited {
                    entry.deadline = now + reachable;
                    entry.attempts = 0;
                } else if changed {
                    entry.deadline = NEVER;
                    entry.attempts = 0;
                }
            }
        }
    }

    /// Record the sender binding of a received solicitation (an ARP
    /// request's sender fields, an NS source link-layer option —
    /// RFC 4861 §7.2.3).
    ///
    /// Creates a `Stale` entry, or refreshes an existing entry's
    /// link-layer address to `Stale` when it changed. When the table is
    /// full of mid-resolution entries the binding is not recorded; the
    /// neighbour is resolved on demand instead.
    pub fn learn(&mut self, ip: IpAddr, mac: MacAddress, now: Duration64) {
        let now = nanos(now);
        match self.find(ip) {
            Some(index) => {
                let entry = &mut self.entries[index];
                let refresh = match entry.phase {
                    Phase::Incomplete => true,
                    Phase::Resolved { mac: cached, .. } => cached != mac,
                };
                if refresh {
                    entry.phase = Phase::Resolved {
                        mac,
                        reach: Reach::Stale,
                    };
                    entry.deadline = NEVER;
                    entry.attempts = 0;
                }
            }
            None => {
                self.insert(Entry {
                    ip,
                    phase: Phase::Resolved {
                        mac,
                        reach: Reach::Stale,
                    },
                    deadline: NEVER,
                    attempts: 0,
                    last_used: now,
                });
            }
        }
    }

    /// Record upper-layer forward progress (e.g. a TCP ACK of new data)
    /// as a reachability confirmation (RFC 4861 §7.3.1).
    pub fn upper_layer_confirmation(&mut self, ip: IpAddr, now: Duration64) {
        let now = nanos(now);
        let reachable = nanos(self.config.reachable_time);
        if let Some(index) = self.find(ip) {
            let entry = &mut self.entries[index];
            if let Phase::Resolved { mac, .. } = entry.phase {
                entry.phase = Phase::Resolved {
                    mac,
                    reach: Reach::Reachable,
                };
                entry.deadline = now + reachable;
                entry.attempts = 0;
            }
        }
    }

    /// Adopt router-advertised timing (RFC 4861 §6.3.4): a Router
    /// Advertisement's non-zero `Reachable Time` / `Retrans Timer`
    /// values are the ones hosts should use for reachability aging
    /// and solicitation spacing; a zero field (`None` here) leaves
    /// the current value. Applies to transitions from this call on;
    /// deadlines already armed keep their original spacing.
    pub fn set_timing(
        &mut self,
        reachable_time: Option<Duration64>,
        retrans_timer: Option<Duration64>,
    ) {
        if let Some(reachable) = reachable_time {
            self.config.reachable_time = reachable;
        }
        if let Some(retrans) = retrans_timer {
            self.config.retrans_timer = retrans;
        }
    }

    /// Drop `ip`'s entry (interface reconfiguration, admin flush).
    pub fn remove(&mut self, ip: IpAddr) {
        if let Some(index) = self.find(ip) {
            self.entries.swap_remove(index);
        }
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// When the earliest timed transition is due, for the caller's
    /// one-shot timer. `None` when nothing is pending.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let earliest = self
            .entries
            .iter()
            .map(|e| e.deadline)
            .filter(|&d| d != NEVER)
            .min()?;
        let clamped = u64::try_from(earliest).unwrap_or(u64::MAX);
        Some(Duration64::from_nanos(clamped))
    }

    /// Perform every timed transition due at `now`, returning the
    /// solicitations and failures the caller must act on.
    pub fn advance(&mut self, now: Duration64) -> Vec<NeighborAction> {
        let now = nanos(now);
        let retrans = nanos(self.config.retrans_timer);
        let config = self.config;
        let mut actions = Vec::new();
        let mut index = 0;
        while index < self.entries.len() {
            let entry = &mut self.entries[index];
            if entry.deadline > now {
                index += 1;
                continue;
            }
            match entry.phase {
                Phase::Incomplete => {
                    if entry.attempts < config.max_multicast_solicit {
                        entry.attempts += 1;
                        entry.deadline = now + retrans;
                        actions.push(NeighborAction::SolicitMulticast { ip: entry.ip });
                        index += 1;
                    } else {
                        actions.push(NeighborAction::Unreachable { ip: entry.ip });
                        self.entries.swap_remove(index);
                    }
                }
                Phase::Resolved { mac, reach } => match reach {
                    Reach::Reachable => {
                        entry.phase = Phase::Resolved {
                            mac,
                            reach: Reach::Stale,
                        };
                        entry.deadline = NEVER;
                        index += 1;
                    }
                    Reach::Delay => {
                        entry.phase = Phase::Resolved {
                            mac,
                            reach: Reach::Probe,
                        };
                        entry.attempts = 1;
                        entry.deadline = now + retrans;
                        actions.push(NeighborAction::SolicitUnicast { ip: entry.ip, mac });
                        index += 1;
                    }
                    Reach::Probe => {
                        if entry.attempts < config.max_unicast_solicit {
                            entry.attempts += 1;
                            entry.deadline = now + retrans;
                            actions.push(NeighborAction::SolicitUnicast { ip: entry.ip, mac });
                            index += 1;
                        } else {
                            actions.push(NeighborAction::Unreachable { ip: entry.ip });
                            self.entries.swap_remove(index);
                        }
                    }
                    // Stale never has a deadline; nothing is due.
                    Reach::Stale => index += 1,
                },
            }
        }
        actions
    }

    fn find(&self, ip: IpAddr) -> Option<usize> {
        self.entries.iter().position(|e| e.ip == ip)
    }

    /// Store `entry`, evicting the least-recently-used entry that is
    /// not mid-resolution when full. Returns `false` (nothing stored)
    /// when every entry is mid-resolution — churn an attacker could
    /// force must never evict live resolution state.
    fn insert(&mut self, entry: Entry) -> bool {
        if self.entries.len() < self.capacity {
            self.entries.push(entry);
            return true;
        }
        let victim = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !matches!(e.phase, Phase::Incomplete))
            .min_by_key(|(_, e)| e.last_used)
            .map(|(i, _)| i);
        match victim {
            Some(index) => {
                self.entries[index] = entry;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
#[path = "neigh_tests.rs"]
mod tests;
