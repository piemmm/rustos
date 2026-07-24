//! Routing: longest-prefix match, default routers, source selection,
//! and path MTU (RFC 4861 §6.3.6, RFC 6724, RFC 8201).
//!
//! One generic binary trie ([`RoutingTable`]) is instantiated for IPv4
//! and IPv6 through the [`RouteAddr`] bit view — there is no second
//! longest-prefix-match implementation. On-link determination is a
//! lookup whose route has no next hop. Default routers learned from
//! Router Advertisements live in the bounded [`DefaultRouterList`];
//! destination path MTUs learned from Packet Too Big live in the
//! bounded [`PathMtuCache`]; and [`select_source`] is the RFC 6724
//! source-address selection over a caller-supplied candidate set.

use alloc::vec::Vec;

use tairix_abi::time::{Duration64, NANOS_PER_SEC};

use crate::addr::{Ipv4Addr, Ipv6Addr, Ipv6Scope};
use crate::ipv6::IPV6_MIN_MTU;

/// An address type the generic routing trie can walk bit by bit.
pub trait RouteAddr: Copy + Eq {
    /// Number of address bits (32 or 128).
    const BITS: u8;

    /// The address as its leading bits, most significant first, in the
    /// low `BITS` of the returned value.
    fn to_bits(self) -> u128;
}

impl RouteAddr for Ipv4Addr {
    const BITS: u8 = 32;

    fn to_bits(self) -> u128 {
        u128::from(u32::from_be_bytes(self.octets()))
    }
}

impl RouteAddr for Ipv6Addr {
    const BITS: u8 = 128;

    fn to_bits(self) -> u128 {
        u128::from_be_bytes(self.octets())
    }
}

/// Extract bit `index` (0 = most significant) of `addr`.
fn bit<A: RouteAddr>(addr: A, index: u8) -> usize {
    ((addr.to_bits() >> (A::BITS - 1 - index)) & 1) as usize
}

/// A validated network prefix: `prefix_len` leading bits of `addr`,
/// with every host bit zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Prefix<A: RouteAddr> {
    addr: A,
    prefix_len: u8,
}

impl<A: RouteAddr> Prefix<A> {
    /// Build a prefix, refusing a length beyond the family's bits or
    /// an address with set host bits (an ambiguous prefix is a config
    /// error, not something to silently mask).
    #[must_use]
    pub fn new(addr: A, prefix_len: u8) -> Option<Self> {
        if prefix_len > A::BITS {
            return None;
        }
        let host_bits = u32::from(A::BITS - prefix_len);
        if host_bits < 128 && addr.to_bits() & ((1u128 << host_bits) - 1) != 0 {
            return None;
        }
        Some(Self { addr, prefix_len })
    }

    /// The prefix address.
    #[must_use]
    pub fn addr(&self) -> A {
        self.addr
    }

    /// The prefix length in bits.
    #[must_use]
    pub fn prefix_len(&self) -> u8 {
        self.prefix_len
    }

    /// True when `addr` falls within this prefix.
    #[must_use]
    pub fn contains(&self, addr: A) -> bool {
        if self.prefix_len == 0 {
            return true;
        }
        let shift = u32::from(A::BITS - self.prefix_len);
        (addr.to_bits() >> shift) == (self.addr.to_bits() >> shift)
    }
}

/// One route: a prefix, its next hop (`None` = the destination is
/// on-link), and caller metadata (interface id, source of the route).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Route<A: RouteAddr, M> {
    /// The destination prefix.
    pub prefix: Prefix<A>,
    /// Next-hop router, or `None` for an on-link prefix.
    pub next_hop: Option<A>,
    /// Caller metadata carried with the route.
    pub metadata: M,
}

/// One trie node: two children and an optional route index.
#[derive(Clone, Copy, Debug)]
struct Node {
    children: [Option<u32>; 2],
    route: Option<u32>,
}

const EMPTY_NODE: Node = Node {
    children: [None, None],
    route: None,
};

/// The generic longest-prefix-match table: a binary trie over address
/// bits, one node per prefix bit, lookup in `O(BITS)` regardless of
/// route count. Removal prunes emptied nodes onto a free list, so
/// route churn never grows the arena.
#[derive(Clone, Debug)]
pub struct RoutingTable<A: RouteAddr, M> {
    nodes: Vec<Node>,
    free: Vec<u32>,
    routes: Vec<Route<A, M>>,
}

impl<A: RouteAddr, M> Default for RoutingTable<A, M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: RouteAddr, M> RoutingTable<A, M> {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: alloc::vec![EMPTY_NODE],
            free: Vec::new(),
            routes: Vec::new(),
        }
    }

    /// Number of routes held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// True when no routes are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Insert (or replace, for an existing identical prefix) a route.
    pub fn insert(&mut self, prefix: Prefix<A>, next_hop: Option<A>, metadata: M) {
        let mut node = 0usize;
        for index in 0..prefix.prefix_len {
            let branch = bit(prefix.addr, index);
            node = if let Some(child) = self.nodes[node].children[branch] {
                child as usize
            } else {
                let child = self.alloc_node();
                self.nodes[node].children[branch] = Some(child);
                child as usize
            };
        }
        let route = Route {
            prefix,
            next_hop,
            metadata,
        };
        if let Some(slot) = self.nodes[node].route {
            self.routes[slot as usize] = route;
        } else {
            let slot = u32::try_from(self.routes.len()).unwrap_or(u32::MAX);
            self.routes.push(route);
            self.nodes[node].route = Some(slot);
        }
    }

    /// Remove the route with exactly this prefix, pruning emptied trie
    /// nodes. Returns `true` when a route was removed.
    pub fn remove(&mut self, prefix: Prefix<A>) -> bool {
        // Walk down, recording the path for pruning.
        let mut path: Vec<(usize, usize)> = Vec::new();
        let mut node = 0usize;
        for index in 0..prefix.prefix_len {
            let branch = bit(prefix.addr, index);
            match self.nodes[node].children[branch] {
                Some(child) => {
                    path.push((node, branch));
                    node = child as usize;
                }
                None => return false,
            }
        }
        let Some(slot) = self.nodes[node].route else {
            return false;
        };
        self.nodes[node].route = None;
        self.remove_route_slot(slot);
        // Prune childless, routeless nodes from the leaf upward (the
        // root is never pruned).
        let mut current = node;
        while let Some((parent, branch)) = path.pop() {
            let n = &self.nodes[current];
            if n.route.is_some() || n.children[0].is_some() || n.children[1].is_some() {
                break;
            }
            self.nodes[parent].children[branch] = None;
            self.free.push(u32::try_from(current).unwrap_or(u32::MAX));
            current = parent;
        }
        true
    }

    /// The longest-prefix route matching `addr`, if any.
    #[must_use]
    pub fn lookup(&self, addr: A) -> Option<&Route<A, M>> {
        let mut best = self.nodes[0].route;
        let mut node = 0usize;
        for index in 0..A::BITS {
            let branch = bit(addr, index);
            match self.nodes[node].children[branch] {
                Some(child) => {
                    node = child as usize;
                    if let Some(route) = self.nodes[node].route {
                        best = Some(route);
                    }
                }
                None => break,
            }
        }
        best.map(|slot| &self.routes[slot as usize])
    }

    /// True when `addr` matches an on-link route (its longest match
    /// has no next hop).
    #[must_use]
    pub fn is_on_link(&self, addr: A) -> bool {
        self.lookup(addr).is_some_and(|r| r.next_hop.is_none())
    }

    /// Iterate over every route (in no particular order).
    pub fn iter(&self) -> impl Iterator<Item = &Route<A, M>> {
        self.routes.iter()
    }

    fn alloc_node(&mut self) -> u32 {
        if let Some(index) = self.free.pop() {
            self.nodes[index as usize] = EMPTY_NODE;
            index
        } else {
            let index = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
            self.nodes.push(EMPTY_NODE);
            index
        }
    }

    /// Remove `slot` from the route arena with `swap_remove`, fixing
    /// the trie's reference to the moved route.
    fn remove_route_slot(&mut self, slot: u32) {
        let last = u32::try_from(self.routes.len() - 1).unwrap_or(u32::MAX);
        self.routes.swap_remove(slot as usize);
        if slot != last {
            // The former last route now lives at `slot`; repoint the
            // node that referenced it.
            let moved = &self.routes[slot as usize];
            let mut node = 0usize;
            for index in 0..moved.prefix.prefix_len {
                let branch = bit(moved.prefix.addr, index);
                match self.nodes[node].children[branch] {
                    Some(child) => node = child as usize,
                    None => return,
                }
            }
            self.nodes[node].route = Some(slot);
        }
    }
}

/// One default router learned from a Router Advertisement.
#[derive(Clone, Copy, Debug)]
struct DefaultRouter {
    addr: Ipv6Addr,
    /// Expiry deadline in monotonic nanoseconds.
    deadline: u128,
}

/// The bounded default-router list (RFC 4861 §5.1, §6.3.4).
///
/// Routers enter and refresh through [`DefaultRouterList::update`]
/// (from a validated Router Advertisement's lifetime), age out through
/// [`DefaultRouterList::advance`], and are chosen by
/// [`DefaultRouterList::select`] — reachable routers first, then
/// round-robin among the rest so an unreachable router is not
/// hammered (RFC 4861 §6.3.6). Bounded: when full, a new router is
/// ignored rather than evicting a live one an attacker's RA flood
/// would churn.
#[derive(Clone, Debug)]
pub struct DefaultRouterList {
    routers: Vec<DefaultRouter>,
    capacity: usize,
    /// Rotation cursor for the round-robin fallback.
    cursor: usize,
}

impl DefaultRouterList {
    /// A list holding at most `capacity` routers.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            routers: Vec::new(),
            capacity,
            cursor: 0,
        }
    }

    /// Number of live routers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.routers.len()
    }

    /// True when no routers are known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routers.is_empty()
    }

    /// Forget every learned router, keeping the capacity bound. Used
    /// when IPv6 is administratively disabled on the interface.
    pub fn clear(&mut self) {
        self.routers.clear();
        self.cursor = 0;
    }

    /// Apply a Router Advertisement's `router_lifetime` for `router`:
    /// zero removes it (the router resigned), a non-zero lifetime
    /// inserts or refreshes it. A new router beyond capacity is
    /// ignored (fail closed against RA floods).
    pub fn update(&mut self, router: Ipv6Addr, lifetime_secs: u16, now: Duration64) {
        let position = self.routers.iter().position(|r| r.addr == router);
        if lifetime_secs == 0 {
            if let Some(index) = position {
                self.routers.swap_remove(index);
            }
            return;
        }
        let deadline =
            nanos(now).saturating_add(u128::from(lifetime_secs) * u128::from(NANOS_PER_SEC));
        match position {
            Some(index) => self.routers[index].deadline = deadline,
            None if self.routers.len() < self.capacity => {
                self.routers.push(DefaultRouter {
                    addr: router,
                    deadline,
                });
            }
            None => {}
        }
    }

    /// Drop routers whose lifetime has expired.
    pub fn advance(&mut self, now: Duration64) {
        let now = nanos(now);
        self.routers.retain(|r| r.deadline > now);
    }

    /// When the earliest lifetime expires, for the caller's one-shot
    /// timer.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let earliest = self.routers.iter().map(|r| r.deadline).min()?;
        let clamped = u64::try_from(earliest).unwrap_or(u64::MAX);
        Some(Duration64::from_nanos(clamped))
    }

    /// Choose a default router: any the caller knows to be reachable
    /// (its neighbour-cache fact) is preferred; otherwise routers are
    /// rotated round-robin so probes spread (RFC 4861 §6.3.6).
    pub fn select(&mut self, is_reachable: impl Fn(Ipv6Addr) -> bool) -> Option<Ipv6Addr> {
        if self.routers.is_empty() {
            return None;
        }
        if let Some(router) = self.routers.iter().find(|r| is_reachable(r.addr)) {
            return Some(router.addr);
        }
        self.cursor = (self.cursor + 1) % self.routers.len();
        Some(self.routers[self.cursor].addr)
    }
}

/// One learned path MTU.
#[derive(Clone, Copy, Debug)]
struct PathMtu {
    destination: Ipv6Addr,
    mtu: u32,
    /// Expiry deadline in monotonic nanoseconds.
    deadline: u128,
    /// Last use, for LRU eviction.
    last_used: u128,
}

/// The bounded per-destination path-MTU cache (RFC 8201).
///
/// IPv6 routers never fragment in flight: the sender bounds every
/// packet to the path MTU it learned from Packet Too Big messages,
/// starting from the link MTU and never below [`IPV6_MIN_MTU`].
/// Entries age out (RFC 8201 §5.3 — a stale, too-small estimate must
/// not stick forever), and the cache is bounded with LRU eviction.
#[derive(Clone, Debug)]
pub struct PathMtuCache {
    entries: Vec<PathMtu>,
    capacity: usize,
    lifetime: Duration64,
}

impl PathMtuCache {
    /// A cache of at most `capacity` destinations whose entries expire
    /// after `lifetime` (RFC 8201 recommends 10 minutes).
    #[must_use]
    pub fn new(capacity: usize, lifetime: Duration64) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
            lifetime,
        }
    }

    /// The path MTU to use for `destination`: the learned value, or
    /// `link_mtu` when none is cached. Never below [`IPV6_MIN_MTU`].
    pub fn mtu(&mut self, destination: Ipv6Addr, link_mtu: u32, now: Duration64) -> u32 {
        let now = nanos(now);
        let learned = self
            .entries
            .iter_mut()
            .find(|e| e.destination == destination && e.deadline > now)
            .map(|e| {
                e.last_used = now;
                e.mtu
            });
        let mtu = learned.map_or(link_mtu, |m| m.min(link_mtu));
        mtu.max(u32::try_from(IPV6_MIN_MTU).unwrap_or(u32::MAX))
    }

    /// Apply a Packet Too Big report for `destination`.
    ///
    /// Fails closed against forged reports: the value is clamped to
    /// [`IPV6_MIN_MTU`], must be a *reduction* of the current estimate
    /// (`link_mtu` when none is cached — RFC 8201 §4: an increase is
    /// never accepted from a report), and a full cache evicts the
    /// least-recently-used entry.
    pub fn packet_too_big(
        &mut self,
        destination: Ipv6Addr,
        reported: u32,
        link_mtu: u32,
        now: Duration64,
    ) {
        let floor = u32::try_from(IPV6_MIN_MTU).unwrap_or(u32::MAX);
        let clamped = reported.max(floor);
        let now_ns = nanos(now);
        let deadline = now_ns.saturating_add(nanos(self.lifetime));
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.destination == destination)
        {
            if clamped < entry.mtu {
                entry.mtu = clamped;
                entry.deadline = deadline;
                entry.last_used = now_ns;
            }
            return;
        }
        if clamped >= link_mtu {
            return;
        }
        if self.entries.len() >= self.capacity {
            let Some(victim) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(i, _)| i)
            else {
                return;
            };
            self.entries.swap_remove(victim);
        }
        self.entries.push(PathMtu {
            destination,
            mtu: clamped,
            deadline,
            last_used: now_ns,
        });
    }

    /// Drop expired entries (the estimate recovers to the link MTU).
    pub fn advance(&mut self, now: Duration64) {
        let now = nanos(now);
        self.entries.retain(|e| e.deadline > now);
    }

    /// When the earliest entry expires, for the caller's one-shot
    /// timer.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let earliest = self.entries.iter().map(|e| e.deadline).min()?;
        let clamped = u64::try_from(earliest).unwrap_or(u64::MAX);
        Some(Duration64::from_nanos(clamped))
    }
}

/// One candidate source address on the outgoing interface, with the
/// attributes RFC 6724's rules read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateAddr {
    /// The candidate address.
    pub addr: Ipv6Addr,
    /// The address is deprecated (its preferred lifetime lapsed).
    pub deprecated: bool,
    /// The prefix length of the subnet the address was formed from.
    pub prefix_len: u8,
}

/// RFC 6724 §2.1 default policy-table labels, for rule 6. The
/// precedence column drives *destination* ordering, which no caller
/// needs yet, so only labels are encoded (adding precedence later is
/// an in-place extension).
fn policy_label(addr: Ipv6Addr) -> u8 {
    let segments = addr.segments();
    if addr.is_loopback() {
        0
    } else if addr.to_ipv4_mapped().is_some() {
        4
    } else if segments[0] == 0x2002 {
        2
    } else if segments[0] == 0x2001 && segments[1] == 0 {
        5
    } else if segments[0] & 0xFE00 == 0xFC00 {
        13
    } else if segments[0] & 0xFFC0 == 0xFEC0 {
        11
    } else if RouteAddr::to_bits(addr) >> 32 == 0 {
        // ::/96 (IPv4-compatible, deprecated).
        3
    } else {
        1
    }
}

/// Length of the common prefix of two addresses, in bits, capped at
/// `cap` (RFC 6724 rule 8 compares no deeper than the subnet prefix).
fn common_prefix_len(a: Ipv6Addr, b: Ipv6Addr, cap: u8) -> u8 {
    let diff = RouteAddr::to_bits(a) ^ RouteAddr::to_bits(b);
    let len = u8::try_from(diff.leading_zeros()).unwrap_or(128);
    len.min(cap)
}

/// Choose the best source address for `destination` from `candidates`
/// per RFC 6724 §5.
///
/// The caller supplies the candidate set for the outgoing interface
/// (rule 5 — prefer addresses on the outgoing interface — is thereby
/// the caller's pre-filter). Implemented rules, in order: 1 (same
/// address), 2 (appropriate scope), 3 (avoid deprecated), 6 (matching
/// label), 8 (longest matching prefix). Rules 4 and 7 (home and
/// temporary addresses) read attributes this host does not have yet;
/// they slot in here when it does. Returns `None` for an empty
/// candidate set (fail closed: no fabricated source).
#[must_use]
pub fn select_source(candidates: &[CandidateAddr], destination: Ipv6Addr) -> Option<Ipv6Addr> {
    let dest_scope = Ipv6Scope::of(&destination);
    let dest_label = policy_label(destination);
    let mut best: Option<&CandidateAddr> = None;
    for candidate in candidates {
        let Some(current) = best else {
            best = Some(candidate);
            continue;
        };
        if better_source(candidate, current, destination, dest_scope, dest_label) {
            best = Some(candidate);
        }
    }
    best.map(|c| c.addr)
}

/// True when `a` beats `b` as a source for `destination` under the
/// implemented RFC 6724 rules.
fn better_source(
    a: &CandidateAddr,
    b: &CandidateAddr,
    destination: Ipv6Addr,
    dest_scope: Option<Ipv6Scope>,
    dest_label: u8,
) -> bool {
    // Rule 1: prefer the destination itself.
    if (a.addr == destination) != (b.addr == destination) {
        return a.addr == destination;
    }
    // Rule 2: prefer appropriate scope. With scopes ordered by the
    // RFC 4007 covering order: prefer the smaller scope that still
    // covers the destination; among too-small scopes prefer the
    // larger.
    if let Some(dest_scope) = dest_scope {
        let scope_a = Ipv6Scope::of(&a.addr);
        let scope_b = Ipv6Scope::of(&b.addr);
        if scope_a != scope_b {
            return match (scope_a, scope_b) {
                (Some(sa), Some(sb)) => {
                    if (sa < dest_scope) == (sb < dest_scope) {
                        // Both sufficient (prefer smaller) or both too
                        // small (prefer larger): smaller-or-larger by
                        // the same comparison direction.
                        if sa < dest_scope {
                            sa > sb
                        } else {
                            sa < sb
                        }
                    } else {
                        // One is too small: the other wins.
                        sb < dest_scope
                    }
                }
                // An unscoped candidate never beats a scoped one.
                (Some(_), None) => true,
                (None, _) => false,
            };
        }
    }
    // Rule 3: avoid deprecated addresses.
    if a.deprecated != b.deprecated {
        return !a.deprecated;
    }
    // Rule 6: prefer a label matching the destination's.
    let label_a = policy_label(a.addr) == dest_label;
    let label_b = policy_label(b.addr) == dest_label;
    if label_a != label_b {
        return label_a;
    }
    // Rule 8: longest matching prefix.
    common_prefix_len(a.addr, destination, a.prefix_len)
        > common_prefix_len(b.addr, destination, b.prefix_len)
}

/// Nanoseconds of a non-negative monotonic duration.
fn nanos(d: Duration64) -> u128 {
    let secs = u128::try_from(d.secs()).unwrap_or(0);
    secs * u128::from(NANOS_PER_SEC) + u128::from(d.subsec_nanos())
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
