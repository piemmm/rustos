//! Fragment reassembly for IPv4 and IPv6 (RFC 791, RFC 8200, RFC 8900).
//!
//! One [`Reassembler`] serves both families: the key ([`FragKey`])
//! carries the family-specific identification, and the hole-filling,
//! budgets, and timers are the shared machinery. Like every stateful
//! engine in this crate it is pure and `now`-driven: fragments go in
//! through [`Reassembler::push`], expiry is performed by
//! [`Reassembler::advance`], and the caller re-arms its one-shot timer
//! from [`Reassembler::next_deadline`].
//!
//! # Security
//!
//! Reassembly state is attacker-fillable, so every dimension is
//! bounded and fails closed:
//!
//! - **Overlap means drop.** Overlapping fragments (including exact
//!   duplicates) have no legitimate modern sender and are the classic
//!   evasion/poisoning vector (RFC 8900; RFC 8200 §4.5 makes them a
//!   MUST-drop for IPv6). The whole datagram's state is discarded.
//! - **Budgets.** Buffered bytes are capped per source address and
//!   globally; datagram count is capped globally. Overflow evicts the
//!   oldest datagram (of the offending source first, then globally);
//!   a fragment that still cannot fit is refused.
//! - **Fixed shape rules.** A non-final fragment whose length is not a
//!   multiple of 8, a fragment extending beyond 65 535 bytes, a final
//!   length that contradicts an earlier final fragment, or more pieces
//!   than [`ReassemblyConfig::max_fragments`] all reject fail-closed.

use alloc::vec::Vec;

use tairix_abi::time::{Duration64, NANOS_PER_SEC};

use crate::addr::IpAddr;

/// Largest reassembled payload either family can describe (both length
/// fields are 16-bit).
pub const MAX_DATAGRAM: usize = 65_535;

/// Identity of one datagram under reassembly.
///
/// IPv4 keys are `(source, destination, identification, protocol)`
/// (RFC 791 §3.2, with the id widened into the shared field); IPv6 keys
/// are `(source, destination, identification)` with `protocol` 0 —
/// the v6 fragment id is per-pair, not per-protocol (RFC 8200 §4.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FragKey {
    /// Source address of the fragments.
    pub source: IpAddr,
    /// Destination address of the fragments.
    pub destination: IpAddr,
    /// Datagram identification (v4's 16 bits widened).
    pub identification: u32,
    /// v4 upper-layer protocol; 0 for v6.
    pub protocol: u8,
}

/// Reassembly bounds and timers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReassemblyConfig {
    /// How long an incomplete datagram may wait for its pieces
    /// (RFC 791 / RFC 8200: 60 seconds).
    pub timeout: Duration64,
    /// Most fragments one datagram may arrive in.
    pub max_fragments: usize,
    /// Most buffered bytes attributed to one source address.
    pub per_source_budget: usize,
    /// Most buffered bytes across all sources.
    pub global_budget: usize,
    /// Most datagrams under reassembly at once.
    pub max_datagrams: usize,
}

impl Default for ReassemblyConfig {
    fn default() -> Self {
        Self {
            timeout: Duration64::from_secs(60),
            max_fragments: 64,
            per_source_budget: 256 * 1024,
            global_budget: 1024 * 1024,
            max_datagrams: 64,
        }
    }
}

/// Why a fragment was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    /// The fragment overlaps already-received bytes; the whole
    /// datagram's state was dropped (RFC 8900).
    Overlap,
    /// The fragment lies beyond [`MAX_DATAGRAM`], is a non-final
    /// fragment with a non-multiple-of-8 length, is empty, or
    /// contradicts the known final length.
    Malformed,
    /// The datagram would exceed [`ReassemblyConfig::max_fragments`].
    TooManyFragments,
    /// No budget remains even after evicting older datagrams.
    BudgetExceeded,
}

/// Outcome of pushing one fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PushOutcome {
    /// All pieces arrived: the reassembled payload.
    Complete(Vec<u8>),
    /// Recorded; more pieces are needed.
    Pending,
    /// Refused (and for [`RejectReason::Overlap`], the datagram's
    /// state was dropped).
    Rejected(RejectReason),
}

/// A datagram that timed out, for the caller's ICMP Time Exceeded
/// decision (only a datagram whose first fragment arrived may be
/// reported — RFC 792 / RFC 4443 §3.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpiredDatagram {
    /// The datagram's identity.
    pub key: FragKey,
    /// The zero-offset fragment had arrived.
    pub first_fragment_received: bool,
}

/// One received, non-overlapping byte range.
#[derive(Clone, Copy, Debug)]
struct Range {
    start: usize,
    end: usize,
}

/// One datagram under reassembly.
#[derive(Clone, Debug)]
struct Datagram {
    key: FragKey,
    /// Payload bytes received so far, placed at their offsets.
    buffer: Vec<u8>,
    /// Received ranges, sorted by start, non-overlapping.
    ranges: Vec<Range>,
    /// Total payload length, once the final fragment arrived.
    total: Option<usize>,
    /// Expiry deadline in monotonic nanoseconds.
    deadline: u128,
    /// Creation time, for oldest-first eviction.
    created: u128,
}

impl Datagram {
    /// Bytes this datagram holds against the budgets.
    fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// True when every byte of a known total has arrived.
    fn complete(&self) -> bool {
        let Some(total) = self.total else {
            return false;
        };
        self.ranges.len() == 1 && self.ranges[0].start == 0 && self.ranges[0].end == total
    }
}

/// The bounded, dual-stack fragment reassembler.
#[derive(Clone, Debug)]
pub struct Reassembler {
    datagrams: Vec<Datagram>,
    config: ReassemblyConfig,
}

impl Reassembler {
    /// A reassembler with the given bounds.
    #[must_use]
    pub fn new(config: ReassemblyConfig) -> Self {
        Self {
            datagrams: Vec::new(),
            config,
        }
    }

    /// Number of datagrams under reassembly.
    #[must_use]
    pub fn len(&self) -> usize {
        self.datagrams.len()
    }

    /// True when nothing is under reassembly.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.datagrams.is_empty()
    }

    /// Total buffered bytes across all datagrams.
    #[must_use]
    pub fn buffered_bytes(&self) -> usize {
        self.datagrams.iter().map(Datagram::buffered).sum()
    }

    /// Record one fragment: `offset` bytes into the reassembled
    /// payload, `more` per the wire flag, `data` the fragment payload.
    ///
    /// Returns the reassembled payload when this piece completes the
    /// datagram. See [`RejectReason`] for the fail-closed refusals.
    pub fn push(
        &mut self,
        key: FragKey,
        offset: usize,
        more: bool,
        data: &[u8],
        now: Duration64,
    ) -> PushOutcome {
        let now = nanos(now);
        // Shape rules first: they need no state.
        let Some(end) = offset.checked_add(data.len()) else {
            return PushOutcome::Rejected(RejectReason::Malformed);
        };
        if data.is_empty() || end > MAX_DATAGRAM || (more && data.len() % 8 != 0) {
            return PushOutcome::Rejected(RejectReason::Malformed);
        }
        let index = if let Some(index) = self.find(key) {
            index
        } else if let Some(index) = self.admit(key, now) {
            index
        } else {
            return PushOutcome::Rejected(RejectReason::BudgetExceeded);
        };
        let datagram = &mut self.datagrams[index];
        // Final-length consistency.
        if !more {
            match datagram.total {
                Some(total) if total != end => {
                    self.datagrams.swap_remove(index);
                    return PushOutcome::Rejected(RejectReason::Malformed);
                }
                _ => datagram.total = Some(end),
            }
        }
        if let Some(total) = datagram.total {
            // A piece beyond the final length contradicts it.
            if end > total {
                self.datagrams.swap_remove(index);
                return PushOutcome::Rejected(RejectReason::Malformed);
            }
        }
        // Overlap (including exact duplicates) drops the datagram.
        if datagram
            .ranges
            .iter()
            .any(|r| offset < r.end && r.start < end)
        {
            self.datagrams.swap_remove(index);
            return PushOutcome::Rejected(RejectReason::Overlap);
        }
        if datagram.ranges.len() >= self.config.max_fragments {
            self.datagrams.swap_remove(index);
            return PushOutcome::Rejected(RejectReason::TooManyFragments);
        }
        // Budget the growth, evicting older datagrams if needed.
        let growth = end.saturating_sub(datagram.buffer.len());
        if growth > 0 && !self.make_room(key, growth) {
            return PushOutcome::Rejected(RejectReason::BudgetExceeded);
        }
        // Eviction moves entries; re-find this datagram by key. It was
        // never an eviction candidate, so absence is a refusal, not a
        // panic (fail closed).
        let Some(index) = self.find(key) else {
            return PushOutcome::Rejected(RejectReason::BudgetExceeded);
        };
        let datagram = &mut self.datagrams[index];
        if end > datagram.buffer.len() {
            datagram.buffer.resize(end, 0);
        }
        datagram.buffer[offset..end].copy_from_slice(data);
        insert_range(&mut datagram.ranges, Range { start: offset, end });
        if datagram.complete() {
            let datagram = self.datagrams.swap_remove(index);
            let mut payload = datagram.buffer;
            payload.truncate(datagram.total.unwrap_or(payload.len()));
            return PushOutcome::Complete(payload);
        }
        PushOutcome::Pending
    }

    /// Drop every datagram whose deadline has passed, returning them
    /// for the caller's ICMP Time Exceeded decision.
    pub fn advance(&mut self, now: Duration64) -> Vec<ExpiredDatagram> {
        let now = nanos(now);
        let mut expired = Vec::new();
        let mut index = 0;
        while index < self.datagrams.len() {
            if self.datagrams[index].deadline <= now {
                let datagram = self.datagrams.swap_remove(index);
                expired.push(ExpiredDatagram {
                    key: datagram.key,
                    first_fragment_received: datagram.ranges.first().is_some_and(|r| r.start == 0),
                });
            } else {
                index += 1;
            }
        }
        expired
    }

    /// When the earliest expiry is due, for the caller's one-shot
    /// timer. `None` when nothing is pending.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let earliest = self.datagrams.iter().map(|d| d.deadline).min()?;
        let clamped = u64::try_from(earliest).unwrap_or(u64::MAX);
        Some(Duration64::from_nanos(clamped))
    }

    fn find(&self, key: FragKey) -> Option<usize> {
        self.datagrams.iter().position(|d| d.key == key)
    }

    /// Admit a new datagram, evicting the oldest when the count cap is
    /// reached. Returns its index, or `None` when nothing can be
    /// evicted (a zero cap).
    fn admit(&mut self, key: FragKey, now: u128) -> Option<usize> {
        if self.config.max_datagrams == 0 {
            return None;
        }
        while self.datagrams.len() >= self.config.max_datagrams {
            self.evict_oldest_except(None, None)?;
        }
        self.datagrams.push(Datagram {
            key,
            buffer: Vec::new(),
            ranges: Vec::new(),
            total: None,
            deadline: now.saturating_add(nanos(self.config.timeout)),
            created: now,
        });
        Some(self.datagrams.len() - 1)
    }

    /// Make `growth` bytes of budget available for the datagram keyed
    /// `protected` by evicting oldest-first: the source's own other
    /// datagrams against the per-source budget, then any other datagram
    /// against the global budget. Returns `false` when the budgets
    /// still cannot fit the growth.
    fn make_room(&mut self, protected: FragKey, growth: usize) -> bool {
        let source = protected.source;
        let over_source = |datagrams: &[Datagram], config: &ReassemblyConfig| {
            let used: usize = datagrams
                .iter()
                .filter(|d| d.key.source == source)
                .map(Datagram::buffered)
                .sum();
            used.saturating_add(growth) > config.per_source_budget
        };
        let over_global = |datagrams: &[Datagram], config: &ReassemblyConfig| {
            let used: usize = datagrams.iter().map(Datagram::buffered).sum();
            used.saturating_add(growth) > config.global_budget
        };
        while over_source(&self.datagrams, &self.config) {
            if self
                .evict_oldest_except(Some(protected), Some(source))
                .is_none()
            {
                return false;
            }
        }
        while over_global(&self.datagrams, &self.config) {
            if self.evict_oldest_except(Some(protected), None).is_none() {
                return false;
            }
        }
        true
    }

    /// Evict the oldest datagram, skipping `protected`, optionally
    /// restricted to `source`.
    fn evict_oldest_except(
        &mut self,
        protected: Option<FragKey>,
        source: Option<IpAddr>,
    ) -> Option<()> {
        let victim = self
            .datagrams
            .iter()
            .enumerate()
            .filter(|(_, d)| protected != Some(d.key) && source.map_or(true, |s| d.key.source == s))
            .min_by_key(|(_, d)| d.created)
            .map(|(i, _)| i)?;
        self.datagrams.swap_remove(victim);
        Some(())
    }
}

/// Insert `range` into a sorted, non-overlapping list, merging
/// adjacent (never overlapping — the caller rejected those) ranges.
fn insert_range(ranges: &mut Vec<Range>, range: Range) {
    let at = ranges
        .iter()
        .position(|r| r.start > range.start)
        .unwrap_or(ranges.len());
    ranges.insert(at, range);
    // Merge neighbours that now touch.
    let mut i = at.saturating_sub(1);
    while i + 1 < ranges.len() {
        if ranges[i].end == ranges[i + 1].start {
            ranges[i].end = ranges[i + 1].end;
            ranges.remove(i + 1);
        } else {
            i += 1;
        }
    }
}

/// Nanoseconds of a non-negative monotonic duration.
fn nanos(d: Duration64) -> u128 {
    let secs = u128::try_from(d.secs()).unwrap_or(0);
    secs * u128::from(NANOS_PER_SEC) + u128::from(d.subsec_nanos())
}

#[cfg(test)]
#[path = "frag_tests.rs"]
mod tests;
