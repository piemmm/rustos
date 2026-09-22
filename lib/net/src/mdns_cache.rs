//! The per-interface multicast DNS record cache (RFC 6762 §10).
//!
//! Everything in here arrives unsolicited from an unauthenticated peer that
//! chooses its own arrival rate, so the cache is shaped by that rather than
//! by convenience.
//!
//! # Indexed, because a scan is a denial-of-service channel
//!
//! Every arriving record must be found or placed in constant time. A linear
//! sweep of the table per record would let a peer choose our CPU cost — a
//! few hundred 255-octet name comparisons per packet, at whatever rate it
//! likes — so records are chained under a hash of their owner name and type.
//! That hash is **keyed** with a per-boot secret the caller supplies: a peer
//! chooses the names, and an unkeyed hash would let it choose a set that all
//! land in one chain and restore the scan it was meant to prevent.
//!
//! # Bounded, and fair within the bound
//!
//! [`super::MAX_RECORDS`] and [`super::MAX_RECORDS_PER_SOURCE`] are fixed
//! security bounds, never capacities that grow with the segment. The
//! per-source bound is what makes the global one fair: a peer at its own
//! ceiling evicts its **own** oldest record, so shouting costs the shouter
//! its cache and nobody else theirs. Only when the table is globally full
//! *and* the arriving peer is under its own ceiling does the
//! least-recently-used record go, whoever sent it.
//!
//! # Per-interface, and never merged
//!
//! One cache per interface, with no path between them. A record learned on
//! a coffee-shop network can never answer a question scoped to the wired
//! LAN, because the two caches do not know about each other.

use alloc::vec::Vec;
use core::hash::{Hash, Hasher};

use tairix_abi::time::{Duration64, NANOS_PER_SEC};
use tairix_collections::HashMap;
use tairix_hash::{BuildFastHash, BuildSipHash13, HashSeed};
use tairix_inline::{ArrayVec, IntrusiveList, Link, LinkStore};

use crate::addr::IpAddr;
use crate::dns::{Name, RecordType};
use crate::timeutil::{nanos, NEVER};

use super::{Record, MAX_QUESTIONS, MAX_RECORDS, MAX_RECORDS_PER_SOURCE};

/// The fractions of a record's original TTL at which a querier with a live
/// interest re-asks, so the answer is refreshed before it expires (RFC 6762
/// §5.2).
const REFRESH_PERCENTS: [u32; 4] = [80, 85, 90, 95];

/// What happened to the cache when a record was folded in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Learned {
    /// A record the cache did not hold was added.
    Added,
    /// A record the cache already held had its lifetime renewed.
    Refreshed,
    /// A goodbye (TTL 0, RFC 6762 §10.1) retired a held record.
    Retired,
    /// Nothing changed: a goodbye for a record that was not held, or a
    /// record the bounds refused.
    Ignored,
}

/// One held record and the lifetime the cache tracks for it.
#[derive(Clone, Copy, Debug)]
pub struct CachedRecord {
    /// The record as received, with the TTL its sender gave it.
    pub record: Record,
    /// The address that sent it.
    pub source: IpAddr,
    /// When it was received.
    pub received: Duration64,
    /// When it expires.
    pub expires: Duration64,
}

/// A cache slot. `Option<Slot>` is the free/occupied distinction, so a freed
/// slot cannot be walked into: the link store answers `None` for it and
/// every list operation refuses the index.
#[derive(Clone, Debug)]
struct Slot {
    record: Record,
    source: IpAddr,
    /// The keyed hash of (owner name, type) this slot is chained under.
    key: u64,
    received: u128,
    expires: u128,
    /// The next re-query instant while some question is watching this
    /// record, or [`NEVER`].
    refresh_at: u128,
    /// How many of [`REFRESH_PERCENTS`] have passed.
    refresh_stage: u8,
    /// Whether a live question covers this record; only a watched record
    /// arms a refresh, so passively cached records cost no wakeups.
    watched: bool,
    bucket_link: Link,
    source_link: Link,
    recency_link: Link,
}

/// A view of the slot array through one of its three link fields.
///
/// A [`Link`] carries the node's on-a-list state inside it, so a node can be
/// on only one list per link; three lists thread the same slots through
/// three fields, and each gets its own store view.
macro_rules! link_view {
    ($name:ident, $field:ident) => {
        struct $name<'a>(&'a mut [Option<Slot>]);

        impl LinkStore for $name<'_> {
            fn link(&self, index: usize) -> Option<&Link> {
                self.0.get(index)?.as_ref().map(|slot| &slot.$field)
            }

            fn link_mut(&mut self, index: usize) -> Option<&mut Link> {
                self.0.get_mut(index)?.as_mut().map(|slot| &mut slot.$field)
            }
        }
    };
}

link_view!(BucketView, bucket_link);
link_view!(SourceView, source_link);
link_view!(RecencyView, recency_link);

/// The bounded, indexed record cache of one interface.
#[derive(Debug)]
pub struct RecordCache {
    slots: Vec<Option<Slot>>,
    /// Indices of slots that hold nothing, as a stack.
    free: Vec<u32>,
    /// Chain head per keyed (name, type) hash.
    buckets: HashMap<u64, IntrusiveList, BuildFastHash>,
    /// Chain head per source address, which is also that source's count.
    sources: HashMap<IpAddr, IntrusiveList, BuildSipHash13>,
    /// Most-recently touched at the front; the back is the eviction
    /// candidate.
    recency: IntrusiveList,
    /// Keys the (name, type) hash; a peer chooses the names.
    index_hash: BuildSipHash13,
    /// The names and types a live question covers.
    ///
    /// Held here rather than derived from the slots, because the normal
    /// order is to ask first and learn second: a question raised before any
    /// answer arrives must still arm the refresh schedule of the answers
    /// when they do.
    watched: ArrayVec<(Name, RecordType), MAX_QUESTIONS>,
    /// The earliest of every slot's expiry and armed refresh, or
    /// [`NEVER`].
    earliest: u128,
    live: usize,
}

impl RecordCache {
    /// An empty cache whose index is keyed with `key`.
    ///
    /// The key must be the per-boot secret wherever a peer can choose the
    /// names cached: [`HashSeed::UNKEYED`] names the predictable choice and
    /// is only for a test.
    #[must_use]
    pub fn new(key: HashSeed) -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            buckets: HashMap::with_hasher(BuildFastHash::new()),
            sources: HashMap::with_hasher(BuildSipHash13::with_seed(key)),
            recency: IntrusiveList::new(),
            index_hash: BuildSipHash13::with_seed(key),
            watched: ArrayVec::new(),
            earliest: NEVER,
            live: 0,
        }
    }

    /// The number of records held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// The number of records attributable to `source`.
    #[must_use]
    pub fn len_from(&self, source: IpAddr) -> usize {
        self.sources.get(&source).map_or(0, IntrusiveList::len)
    }

    /// The next instant [`Self::advance`] has work to do, or `None`.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        (self.earliest != NEVER).then(|| crate::timeutil::from_nanos(self.earliest))
    }

    /// Fold a received record into the cache.
    ///
    /// A TTL of zero is a goodbye (RFC 6762 §10.1): RFC 6762 §10.1 asks that
    /// the record be held for one further second rather than dropped at
    /// once, so a peer that withdraws a name and immediately re-announces it
    /// does not leave every querier with a gap. The cache-flush bit (RFC
    /// 6762 §10.2) retires every *other* record this source holds at the
    /// same name and type, which is how a unique record replaces rather than
    /// accumulates.
    pub fn learn(&mut self, now: Duration64, record: &Record, source: IpAddr) -> Learned {
        let now_ns = nanos(now);
        let key = self.key_of(&record.name, record.record_type());

        if record.ttl == 0 {
            return self.retire(now_ns, key, record, source);
        }

        if record.cache_flush {
            self.flush_others(now_ns, key, record, source);
        }

        if let Some(index) = self.find(key, record) {
            self.renew(index, now_ns, record.ttl);
            return Learned::Refreshed;
        }

        if !self.reserve_chains(key, source) {
            return Learned::Ignored;
        }
        let Some(index) = self.allocate(source) else {
            return Learned::Ignored;
        };
        self.place(index, key, now_ns, record, source);
        Learned::Added
    }

    /// Every held record answering `name`/`record_type`, most recently
    /// touched first.
    pub fn lookup(&self, name: &Name, record_type: RecordType) -> Matches<'_> {
        let key = self.key_of(name, record_type);
        Matches {
            slots: &self.slots,
            next: self.buckets.get(&key).and_then(IntrusiveList::front),
            name: *name,
            record_type,
        }
    }

    /// Whether the cache holds a record identical to `record` (ignoring TTL
    /// and the cache-flush bit) whose remaining lifetime is more than half
    /// the TTL `record` carries.
    ///
    /// The RFC 6762 §7.1 test for whether a known answer in a query
    /// suppresses the answer a responder would otherwise give.
    #[must_use]
    pub fn holds_fresher(&self, now: Duration64, record: &Record) -> bool {
        let key = self.key_of(&record.name, record.record_type());
        let Some(index) = self.find(key, record) else {
            return false;
        };
        let Some(Some(slot)) = self.slots.get(index) else {
            return false;
        };
        let remaining = slot.expires.saturating_sub(nanos(now));
        remaining * 2 > u128::from(record.ttl) * u128::from(NANOS_PER_SEC)
    }

    /// Mark whether a live question covers `name`/`record_type`, which is
    /// what arms (or disarms) the RFC 6762 §5.2 refresh schedule for the
    /// records held there.
    ///
    /// Only a watched record wakes the caller before it expires: a cache
    /// full of records nobody asked about costs no timers.
    pub fn set_watched(&mut self, name: &Name, record_type: RecordType, watched: bool) {
        let held = self
            .watched
            .iter()
            .position(|(held, held_type)| held == name && *held_type == record_type);
        match (watched, held) {
            (true, None) => {
                // Past the bound the question simply refreshes nothing: it
                // still asks on its own schedule, so the cost is a re-query
                // rather than a wrong answer.
                let _ = self.watched.try_push((*name, record_type));
            }
            (false, Some(index)) => {
                self.watched.remove(index);
            }
            _ => {}
        }
        let key = self.key_of(name, record_type);
        let mut next = self.buckets.get(&key).and_then(IntrusiveList::front);
        while let Some(index) = next {
            let Some(Some(slot)) = self.slots.get_mut(index) else {
                break;
            };
            next = slot.bucket_link.next();
            if slot.record.name != *name || slot.record.record_type() != record_type {
                continue;
            }
            slot.watched = watched;
            slot.refresh_at = if watched {
                refresh_instant(slot.received, slot.expires, slot.refresh_stage)
            } else {
                NEVER
            };
        }
        self.recompute_earliest();
    }

    /// Expire what `now` has reached and report which watched records are
    /// due a refreshing re-query.
    ///
    /// Expired records are dropped before the refresh points are collected,
    /// so a caller never re-asks about something it has just forgotten.
    pub fn advance(&mut self, now: Duration64, due: &mut dyn FnMut(&Name, RecordType)) {
        let now_ns = nanos(now);
        if now_ns < self.earliest {
            return;
        }
        for index in 0..self.slots.len() {
            let expired = self
                .slots
                .get(index)
                .and_then(Option::as_ref)
                .is_some_and(|slot| slot.expires <= now_ns);
            if expired {
                self.remove(index);
            }
        }
        for index in 0..self.slots.len() {
            let Some(slot) = self.slots.get_mut(index).and_then(Option::as_mut) else {
                continue;
            };
            if slot.refresh_at > now_ns {
                continue;
            }
            slot.refresh_stage = slot.refresh_stage.saturating_add(1);
            slot.refresh_at = refresh_instant(slot.received, slot.expires, slot.refresh_stage);
            due(&slot.record.name, slot.record.record_type());
        }
        self.recompute_earliest();
    }

    /// Drop every held record — what an interface going down does, since a
    /// record learned on a link is meaningless once that link is gone.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.watched.clear();
        self.free.clear();
        self.buckets.clear();
        self.sources.clear();
        self.recency = IntrusiveList::new();
        self.earliest = NEVER;
        self.live = 0;
    }

    /// The keyed hash a record's (owner name, type) is chained under.
    fn key_of(&self, name: &Name, record_type: RecordType) -> u64 {
        let mut hasher = core::hash::BuildHasher::build_hasher(&self.index_hash);
        name.hash(&mut hasher);
        hasher.write_u16(record_type.value());
        hasher.finish()
    }

    /// The slot holding a record identical to `record`, or `None`.
    fn find(&self, key: u64, record: &Record) -> Option<usize> {
        let mut next = self.buckets.get(&key).and_then(IntrusiveList::front);
        while let Some(index) = next {
            let slot = self.slots.get(index)?.as_ref()?;
            if slot.record.same_record(record) {
                return Some(index);
            }
            next = slot.bucket_link.next();
        }
        None
    }

    /// Retire a record a goodbye withdrew, keeping it for the one further
    /// second RFC 6762 §10.1 asks for.
    fn retire(&mut self, now_ns: u128, key: u64, record: &Record, source: IpAddr) -> Learned {
        let Some(index) = self.find(key, record) else {
            return Learned::Ignored;
        };
        // Only the source that asserted a record may withdraw it; otherwise
        // any peer on the segment could delete a neighbour's advertisement.
        let Some(Some(slot)) = self.slots.get_mut(index) else {
            return Learned::Ignored;
        };
        if slot.source != source {
            return Learned::Ignored;
        }
        slot.expires = slot
            .expires
            .min(now_ns.saturating_add(u128::from(NANOS_PER_SEC)));
        slot.refresh_at = NEVER;
        self.earliest = self.earliest.min(slot.expires);
        Learned::Retired
    }

    /// Retire this source's other records at the same name and type, which
    /// is what the cache-flush bit asks for.
    ///
    /// Scoped to the announcing source on purpose: the bit asserts sole
    /// ownership, and honouring it across sources would let any peer flush a
    /// neighbour's records by claiming their name.
    fn flush_others(&mut self, now_ns: u128, key: u64, record: &Record, source: IpAddr) {
        let one_second = u128::from(NANOS_PER_SEC);
        let flush_at = now_ns.saturating_add(one_second);
        let mut next = self.buckets.get(&key).and_then(IntrusiveList::front);
        while let Some(index) = next {
            let Some(Some(slot)) = self.slots.get_mut(index) else {
                break;
            };
            next = slot.bucket_link.next();
            // A record received within the last second is spared: an
            // announcement of several records at one name arrives as several
            // messages, and flushing on the first would delete the rest.
            if slot.source != source
                || now_ns.saturating_sub(slot.received) < one_second
                || slot.record.name != record.name
                || slot.record.record_type() != record.record_type()
                || slot.record.data == record.data
            {
                continue;
            }
            slot.expires = slot.expires.min(flush_at);
            slot.refresh_at = NEVER;
            self.earliest = self.earliest.min(slot.expires);
        }
    }

    /// Renew a held record's lifetime and restart its refresh schedule.
    fn renew(&mut self, index: usize, now_ns: u128, ttl: u32) {
        let Some(Some(slot)) = self.slots.get_mut(index) else {
            return;
        };
        slot.received = now_ns;
        slot.expires = now_ns.saturating_add(ttl_nanos(ttl));
        slot.record.ttl = ttl;
        slot.refresh_stage = 0;
        slot.refresh_at = if slot.watched {
            refresh_instant(slot.received, slot.expires, 0)
        } else {
            NEVER
        };
        self.earliest = self.earliest.min(slot.refresh_at.min(slot.expires));
        let _ = self
            .recency
            .move_to_front(&mut RecencyView(&mut self.slots), index);
    }

    /// A slot for a record from `source`, evicting under the bounds if one
    /// is needed, or `None` when the cache refuses the record.
    fn allocate(&mut self, source: IpAddr) -> Option<usize> {
        if self.len_from(source) >= MAX_RECORDS_PER_SOURCE {
            // The shouter pays: its own oldest record goes, never a
            // neighbour's.
            let oldest = self
                .sources
                .get_mut(&source)
                .and_then(|chain| chain.back())?;
            self.remove(oldest);
        } else if self.live >= MAX_RECORDS {
            let oldest = self.recency.back()?;
            self.remove(oldest);
        }
        if let Some(index) = self.free.pop() {
            return Some(index as usize);
        }
        if self.slots.len() >= MAX_RECORDS {
            return None;
        }
        self.slots.try_reserve(1).ok()?;
        self.slots.push(None);
        Some(self.slots.len() - 1)
    }

    /// Fill an allocated slot and index it.
    fn place(&mut self, index: usize, key: u64, now_ns: u128, record: &Record, source: IpAddr) {
        let expires = now_ns.saturating_add(ttl_nanos(record.ttl));
        let watched = self.is_watched(&record.name, record.record_type());
        let refresh_at = if watched {
            refresh_instant(now_ns, expires, 0)
        } else {
            NEVER
        };
        self.slots[index] = Some(Slot {
            record: *record,
            source,
            key,
            received: now_ns,
            expires,
            refresh_at,
            refresh_stage: 0,
            watched,
            bucket_link: Link::UNLINKED,
            source_link: Link::UNLINKED,
            recency_link: Link::UNLINKED,
        });
        self.live += 1;
        self.earliest = self.earliest.min(refresh_at.min(expires));

        if let Some(bucket) = self.buckets.get_mut(&key) {
            let _ = bucket.push_front(&mut BucketView(&mut self.slots), index);
        }
        if let Some(chain) = self.sources.get_mut(&source) {
            let _ = chain.push_front(&mut SourceView(&mut self.slots), index);
        }
        let _ = self
            .recency
            .push_front(&mut RecencyView(&mut self.slots), index);
    }

    /// Create the bucket and source chain heads a record needs, before any
    /// slot is taken or evicted, so a refused allocation leaves the cache
    /// exactly as it was.
    fn reserve_chains(&mut self, key: u64, source: IpAddr) -> bool {
        let bucket_added = if self.buckets.get(&key).is_none() {
            if self.buckets.try_insert(key, IntrusiveList::new()).is_err() {
                return false;
            }
            true
        } else {
            false
        };
        if self.sources.get(&source).is_none()
            && self
                .sources
                .try_insert(source, IntrusiveList::new())
                .is_err()
        {
            if bucket_added {
                self.buckets.remove(&key);
            }
            return false;
        }
        true
    }

    /// Whether a live question covers this name and type, so a newly placed
    /// record arms the refresh schedule at once.
    fn is_watched(&self, name: &Name, record_type: RecordType) -> bool {
        self.watched
            .iter()
            .any(|(held, held_type)| held == name && *held_type == record_type)
    }

    /// Unlink and free the slot at `index`.
    fn remove(&mut self, index: usize) {
        let Some(Some(slot)) = self.slots.get(index) else {
            return;
        };
        let (key, source) = (slot.key, slot.source);
        if let Some(bucket) = self.buckets.get_mut(&key) {
            let _ = bucket.unlink(&mut BucketView(&mut self.slots), index);
            if bucket.is_empty() {
                self.buckets.remove(&key);
            }
        }
        if let Some(chain) = self.sources.get_mut(&source) {
            let _ = chain.unlink(&mut SourceView(&mut self.slots), index);
            if chain.is_empty() {
                self.sources.remove(&source);
            }
        }
        let _ = self
            .recency
            .unlink(&mut RecencyView(&mut self.slots), index);
        self.slots[index] = None;
        self.live -= 1;
        if self.free.try_reserve(1).is_ok() {
            // The index fits: the slot array is bounded by MAX_RECORDS.
            if let Ok(index) = u32::try_from(index) {
                self.free.push(index);
            }
        }
    }

    /// Recompute the folded deadline from the slots.
    ///
    /// A full pass, which is what the bound buys: the slot count is fixed at
    /// [`MAX_RECORDS`] whatever the segment does, so this is a constant, and
    /// it runs once per timer event rather than once per arriving record.
    fn recompute_earliest(&mut self) {
        self.earliest = self
            .slots
            .iter()
            .flatten()
            .map(|slot| slot.expires.min(slot.refresh_at))
            .min()
            .unwrap_or(NEVER);
    }
}

/// Records the cache holds for one name and type.
#[derive(Clone, Debug)]
pub struct Matches<'a> {
    slots: &'a [Option<Slot>],
    next: Option<usize>,
    name: Name,
    record_type: RecordType,
}

impl Iterator for Matches<'_> {
    type Item = CachedRecord;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(index) = self.next {
            let slot = self.slots.get(index)?.as_ref()?;
            self.next = slot.bucket_link.next();
            if slot.record.name != self.name || slot.record.record_type() != self.record_type {
                continue;
            }
            return Some(CachedRecord {
                record: slot.record,
                source: slot.source,
                received: crate::timeutil::from_nanos(slot.received),
                expires: crate::timeutil::from_nanos(slot.expires),
            });
        }
        None
    }
}

impl core::iter::FusedIterator for Matches<'_> {}

/// A TTL in seconds as monotonic nanoseconds.
fn ttl_nanos(ttl: u32) -> u128 {
    u128::from(ttl) * u128::from(NANOS_PER_SEC)
}

/// The instant a watched record's `stage`-th re-query is due, or [`NEVER`]
/// once every point has passed (RFC 6762 §5.2).
fn refresh_instant(received: u128, expires: u128, stage: u8) -> u128 {
    let Some(percent) = REFRESH_PERCENTS.get(usize::from(stage)) else {
        return NEVER;
    };
    let lifetime = expires.saturating_sub(received);
    received.saturating_add(lifetime * u128::from(*percent) / 100)
}

#[cfg(test)]
#[path = "mdns_cache_tests.rs"]
mod tests;
