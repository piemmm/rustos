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
//! - The index is keyed. A remote peer chooses the addresses this table is
//!   keyed by, so it is hashed under the caller's per-boot key: an
//!   unpredictable one denies an attacker a set of addresses that all land in
//!   the same bucket and turn every transmit into a scan of the table.
//! - A confirmation ([`NeighborTable::confirm`]) for an address with no
//!   entry is ignored — an unsolicited reply never creates state.
//! - Only [`NeighborTable::lookup`] (this host is sending) and
//!   [`NeighborTable::learn`] (a solicitation carrying the peer's own
//!   binding was received) create entries.

use alloc::vec::Vec;

use tairix_abi::driver::net::MacAddress;
use tairix_abi::time::Duration64;
use tairix_collections::LruMap;
use tairix_hash::{BuildSipHash13, HashSeed};

use crate::addr::IpAddr;
use crate::timeutil::{nanos, NEVER};

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
    /// No entry could be made: every entry is mid-resolution, or the
    /// allocator refused one. The packet is refused (fail closed), not
    /// queued.
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
///
/// Neither the address nor a last-use stamp is here: the address is the
/// index's key, and the recency order the index maintains is what eviction
/// reads, so both are held once.
#[derive(Copy, Clone, Debug)]
struct Entry {
    phase: Phase,
    /// Next timed transition, in nanoseconds ([`NEVER`] when the state
    /// has none).
    deadline: u128,
    /// Solicitations/probes sent in the current state.
    attempts: u8,
}

/// The bounded, provider-agnostic neighbour cache.
///
/// See the [module docs](self) for the driving contract. The intended
/// caller pattern per received event or send attempt is: call the
/// event's method ([`lookup`](Self::lookup), [`confirm`](Self::confirm),
/// [`learn`](Self::learn), …), then call [`advance`](Self::advance) and
/// perform the returned actions, then re-arm the caller's one-shot timer
/// from [`next_deadline`](Self::next_deadline).
#[derive(Debug)]
pub struct NeighborTable {
    /// Address to entry, with the recency order eviction takes maintained in
    /// constant time alongside it.
    entries: LruMap<IpAddr, Entry, BuildSipHash13>,
    capacity: usize,
    config: NeighborConfig,
}

impl NeighborTable {
    /// A table holding at most `capacity` entries, keyed under `key`.
    ///
    /// `key` is the caller's per-boot hash key. A platform whose CSPRNG never
    /// seeded has none and names [`HashSeed::UNKEYED`], which still resolves
    /// every neighbour — only the bucket a peer's address lands in becomes
    /// predictable, and the table's fixed capacity bounds what that costs.
    #[must_use]
    pub fn new(capacity: usize, config: NeighborConfig, key: HashSeed) -> Self {
        Self {
            entries: LruMap::with_hasher(BuildSipHash13::with_seed(key)),
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
    ///
    /// An observation, not a use: it leaves the recency order alone, so a
    /// diagnostic read never changes which entry eviction takes.
    #[must_use]
    pub fn entry(&self, ip: IpAddr) -> Option<(NeighborState, Option<MacAddress>)> {
        self.entries.peek(&ip).map(|entry| match entry.phase {
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
        // A sender's lookup is the use the recency order tracks, so this is
        // the one accessor that refreshes it.
        let Some(entry) = self.entries.get_mut(&ip) else {
            if !self.insert(
                ip,
                Entry {
                    phase: Phase::Incomplete,
                    // Due immediately: the next `advance` emits the first
                    // multicast solicitation.
                    deadline: now,
                    attempts: 0,
                },
            ) {
                return LookupResult::TableFull;
            }
            return LookupResult::Pending;
        };
        let Entry {
            phase,
            deadline,
            attempts,
        } = entry;
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
        // A reply is the peer's traffic, not this host's, so it updates the
        // entry without counting as a use.
        let Some(entry) = self.entries.peek_mut(&ip) else {
            return;
        };
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
    ///
    /// Unlike the other events this takes no `now`: a learned binding is the
    /// peer's traffic rather than this host's, and it arms no timer — a fresh
    /// entry enters at the newest end of the recency order by being created,
    /// and refreshing an existing one has never counted as a use.
    pub fn learn(&mut self, ip: IpAddr, mac: MacAddress) {
        match self.entries.peek_mut(&ip) {
            Some(entry) => {
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
                self.insert(
                    ip,
                    Entry {
                        phase: Phase::Resolved {
                            mac,
                            reach: Reach::Stale,
                        },
                        deadline: NEVER,
                        attempts: 0,
                    },
                );
            }
        }
    }

    /// Record upper-layer forward progress (e.g. a TCP ACK of new data)
    /// as a reachability confirmation (RFC 4861 §7.3.1).
    pub fn upper_layer_confirmation(&mut self, ip: IpAddr, now: Duration64) {
        let now = nanos(now);
        let reachable = nanos(self.config.reachable_time);
        if let Some(entry) = self.entries.peek_mut(&ip) {
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
        self.entries.remove(&ip);
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
            .iter_lru()
            .map(|(_, entry)| entry.deadline)
            .filter(|&deadline| deadline != NEVER)
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
        // A transition is the table's own timer firing, not a use, so the
        // survivors keep the recency order they had.
        self.entries.retain(|&ip, entry| {
            if entry.deadline > now {
                return true;
            }
            match entry.phase {
                Phase::Incomplete => {
                    if entry.attempts < config.max_multicast_solicit {
                        entry.attempts += 1;
                        entry.deadline = now + retrans;
                        actions.push(NeighborAction::SolicitMulticast { ip });
                        true
                    } else {
                        actions.push(NeighborAction::Unreachable { ip });
                        false
                    }
                }
                Phase::Resolved { mac, reach } => match reach {
                    Reach::Reachable => {
                        entry.phase = Phase::Resolved {
                            mac,
                            reach: Reach::Stale,
                        };
                        entry.deadline = NEVER;
                        true
                    }
                    Reach::Delay => {
                        entry.phase = Phase::Resolved {
                            mac,
                            reach: Reach::Probe,
                        };
                        entry.attempts = 1;
                        entry.deadline = now + retrans;
                        actions.push(NeighborAction::SolicitUnicast { ip, mac });
                        true
                    }
                    Reach::Probe => {
                        if entry.attempts < config.max_unicast_solicit {
                            entry.attempts += 1;
                            entry.deadline = now + retrans;
                            actions.push(NeighborAction::SolicitUnicast { ip, mac });
                            true
                        } else {
                            actions.push(NeighborAction::Unreachable { ip });
                            false
                        }
                    }
                    // Stale never has a deadline; nothing is due.
                    Reach::Stale => true,
                },
            }
        });
        actions
    }

    /// Store `entry` for `ip`, evicting the least-recently-used entry that is
    /// not mid-resolution when full. Returns `false` (nothing stored) when
    /// every entry is mid-resolution — churn an attacker could force must
    /// never evict live resolution state — or when the allocator refused the
    /// entry, which is the same fail-closed answer.
    fn insert(&mut self, ip: IpAddr, entry: Entry) -> bool {
        if self.entries.len() >= self.capacity {
            let victim = self
                .entries
                .iter_lru()
                .find(|(_, held)| !matches!(held.phase, Phase::Incomplete))
                .map(|(ip, _)| *ip);
            let Some(victim) = victim else {
                return false;
            };
            self.entries.remove(&victim);
        }
        self.entries.try_insert(ip, entry).is_ok()
    }
}

#[cfg(test)]
#[path = "neigh_tests.rs"]
mod tests;
