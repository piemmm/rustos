//! The open-addressing table both hash containers are built on.
//!
//! One control byte per slot, probed a [`GROUP_LEN`]-lane group at a time (see
//! [`crate::group`]). The layout is one allocation holding the slot array
//! followed by the control array, so a table costs one control byte plus one
//! slot per bucket and nothing else — no per-entry node, no pointer chasing.
//!
//! Groups are *aligned*: probing steps whole groups, never a byte offset into
//! one. That is what lets the control array end exactly at `buckets` bytes
//! with no wrap-around mirror of its head, and removes the paired-write
//! invariant an unaligned layout has to maintain on every control update.
//!
//! # Invariants
//!
//! * `buckets` is zero, or a power of two of at least [`GROUP_LEN`].
//! * Every control byte is initialised; a slot is initialised exactly when its
//!   control byte [`is_full`].
//! * A group holding an [`EMPTY`] lane has no probe chain passing *through*
//!   it. Insertion maintains this by only stepping past a group with no empty
//!   lane, and removal by only writing [`EMPTY`] into a group that already had
//!   one — otherwise it writes [`DELETED`], which keeps chains intact.
//! * `growth_left` is the number of further insertions the current allocation
//!   can take: the load-factor limit less the live entries and the tombstones.
//!   An unallocated table has none, so an insertion always allocates first and
//!   a table with no buckets is never written to.

use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr::{self, NonNull};

use crate::group::{is_full, scan, Group, DELETED, EMPTY, GROUP_LEN};
use crate::TryReserveError;

/// The seven-bit tag a full slot's control byte carries, taken from the top of
/// the hash so it is independent of the bits that choose the group.
#[inline]
#[allow(clippy::cast_possible_truncation)] // the shift leaves exactly seven bits
const fn tag_of(hash: u64) -> u8 {
    (hash >> 57) as u8
}

/// The load-factor limit: seven eighths of the buckets.
#[inline]
const fn limit_of(buckets: usize) -> usize {
    buckets - buckets / 8
}

/// The smallest legal bucket count whose load-factor limit admits `entries`.
fn buckets_for(entries: usize) -> Result<usize, TryReserveError> {
    let scaled = entries
        .checked_mul(8)
        .ok_or(TryReserveError::CapacityOverflow)?
        .div_ceil(7);
    let buckets = scaled
        .checked_next_power_of_two()
        .ok_or(TryReserveError::CapacityOverflow)?
        .max(GROUP_LEN);
    Ok(buckets)
}

/// Where a probe for a key ended.
pub(crate) enum Slot {
    /// The key is live in this slot.
    Occupied(usize),
    /// The key is absent and a new entry belongs in this slot.
    Vacant(usize),
}

/// The outcome of one probe, with the work it took.
///
/// `groups` is the deterministic work counter the performance gates assert on;
/// the production callers discard it and the compiler removes the increment.
pub(crate) struct Probe {
    /// Where the probe ended, or `None` when the chain was walked to
    /// exhaustion without finding the key or a free slot.
    pub(crate) slot: Option<Slot>,
    /// Control groups examined.
    pub(crate) groups: usize,
}

/// An open-addressing table of `T`, indexed by a caller-supplied hash.
pub(crate) struct RawTable<T> {
    /// Slot count: zero, or a power of two of at least [`GROUP_LEN`].
    buckets: usize,
    /// `buckets` control bytes, one per slot. Dangling while `buckets` is
    /// zero, which is sound because nothing loads a group from a table with no
    /// groups.
    ctrl: NonNull<u8>,
    /// `buckets` slots, initialised exactly where the control byte is full.
    slots: NonNull<T>,
    /// Live entries.
    items: usize,
    /// Insertions the current allocation can still take.
    growth_left: usize,
    /// The table owns its entries, so drop-check must see them.
    marker: PhantomData<T>,
}

// The table owns its `T`s outright; the raw pointers carry no shared state of
// their own, so it is exactly as transferable and as shareable as `T` is.
// SAFETY: sending the table sends every `T` it owns and nothing else.
unsafe impl<T: Send> Send for RawTable<T> {}
// SAFETY: `&RawTable<T>` hands out only `&T`, so sharing it shares only `T`s.
unsafe impl<T: Sync> Sync for RawTable<T> {}

impl<T> RawTable<T> {
    /// An empty table that has not allocated.
    pub(crate) const fn new() -> Self {
        Self {
            buckets: 0,
            ctrl: NonNull::dangling(),
            slots: NonNull::dangling(),
            items: 0,
            growth_left: 0,
            marker: PhantomData,
        }
    }

    /// Live entries.
    #[inline]
    pub(crate) const fn len(&self) -> usize {
        self.items
    }

    /// Entries the table can hold before it must rebuild.
    #[inline]
    pub(crate) const fn capacity(&self) -> usize {
        self.items + self.growth_left
    }

    /// Insertions the current allocation can still take.
    #[inline]
    pub(crate) const fn growth_left(&self) -> usize {
        self.growth_left
    }

    /// Slots, live or not.
    #[inline]
    pub(crate) const fn buckets(&self) -> usize {
        self.buckets
    }

    /// `true` if slot `index` holds a live entry.
    ///
    /// # Safety
    ///
    /// `index` must be below `self.buckets`.
    #[inline]
    pub(crate) unsafe fn is_occupied(&self, index: usize) -> bool {
        // SAFETY: the caller's bound puts the control byte inside the array.
        is_full(unsafe { self.ctrl_at(index) })
    }

    /// Bytes of heap the table currently holds — its resident footprint.
    pub(crate) fn allocated_bytes(&self) -> usize {
        if self.buckets == 0 {
            return 0;
        }
        Self::block(self.buckets).map_or(0, |(layout, _)| layout.size())
    }

    /// The one allocation's layout and the offset of the control array within
    /// it, for `buckets` slots.
    fn block(buckets: usize) -> Result<(Layout, usize), TryReserveError> {
        let slots = Layout::array::<T>(buckets).map_err(|_| TryReserveError::CapacityOverflow)?;
        let ctrl = Layout::array::<u8>(buckets).map_err(|_| TryReserveError::CapacityOverflow)?;
        let (whole, offset) = slots
            .extend(ctrl)
            .map_err(|_| TryReserveError::CapacityOverflow)?;
        Ok((whole.pad_to_align(), offset))
    }

    /// The control group at group index `group`.
    ///
    /// # Safety
    ///
    /// `group` must be below `self.buckets / GROUP_LEN`.
    #[inline]
    unsafe fn group_at(&self, group: usize) -> Group {
        // SAFETY: the caller's bound puts all `GROUP_LEN` bytes inside the
        // control array, and every control byte is initialised. The read is
        // by value so it borrows nothing across the control writes that follow
        // a probe.
        unsafe { ptr::read(self.ctrl.as_ptr().add(group * GROUP_LEN).cast::<Group>()) }
    }

    /// The address of slot `index`.
    ///
    /// # Safety
    ///
    /// `index` must be below `self.buckets`.
    #[inline]
    unsafe fn slot(&self, index: usize) -> *mut T {
        // SAFETY: the caller's bound puts the slot inside the slot array.
        unsafe { self.slots.as_ptr().add(index) }
    }

    /// The entry in slot `index`.
    ///
    /// # Safety
    ///
    /// `index` must be below `self.buckets` and its control byte must be full.
    #[inline]
    pub(crate) unsafe fn entry(&self, index: usize) -> &T {
        // SAFETY: a full control byte means the slot is initialised, and the
        // borrow is tied to `&self`.
        unsafe { &*self.slot(index) }
    }

    /// The entry in slot `index`, mutably.
    ///
    /// # Safety
    ///
    /// `index` must be below `self.buckets` and its control byte must be full.
    #[inline]
    pub(crate) unsafe fn entry_mut(&mut self, index: usize) -> &mut T {
        // SAFETY: as `entry`, and `&mut self` makes the borrow unique.
        unsafe { &mut *self.slot(index) }
    }

    /// Write `ctrl` into slot `index`'s control byte.
    ///
    /// # Safety
    ///
    /// `index` must be below `self.buckets`.
    #[inline]
    unsafe fn set_ctrl(&mut self, index: usize, ctrl: u8) {
        // SAFETY: the caller's bound puts the byte inside the control array.
        unsafe { self.ctrl.as_ptr().add(index).write(ctrl) }
    }

    /// Slot `index`'s control byte.
    ///
    /// # Safety
    ///
    /// `index` must be below `self.buckets`.
    #[inline]
    unsafe fn ctrl_at(&self, index: usize) -> u8 {
        // SAFETY: the caller's bound puts the byte inside the control array,
        // and every control byte is initialised.
        unsafe { self.ctrl.as_ptr().add(index).read() }
    }

    /// Walk the probe chain for `hash`, reporting where the key `eq` accepts
    /// lives or where a new entry for it belongs.
    ///
    /// The chain is walked to its end before a vacancy is reported, so the
    /// earliest tombstone on the chain is reused without ever hiding a live
    /// entry behind it.
    pub(crate) fn probe(&self, hash: u64, mut eq: impl FnMut(&T) -> bool) -> Probe {
        let groups = self.buckets / GROUP_LEN;
        let mask = groups.wrapping_sub(1);
        let tag = tag_of(hash);
        #[allow(clippy::cast_possible_truncation)] // the low bits are the point
        let mut group = (hash as usize) & mask;
        let mut vacancy = None;
        let mut examined = 0;

        // Triangular probing: the step grows by one group each time, which
        // visits every group of a power-of-two table exactly once.
        for step in 1..=groups {
            // SAFETY: `group` is masked to the group count, so every lane of
            // the load is inside the control array.
            let ctrl = unsafe { self.group_at(group) };
            let found = scan(&ctrl, tag);
            examined += 1;

            // The scan is a dispatched routine, so whether a slot may be read
            // or written is taken from the group already loaded rather than
            // from what that routine reported. A candidate that survived the
            // self-verify and is still wrong then costs a missed entry, never
            // a read of an uninitialised slot.
            let mut candidates = found.tag;
            while candidates != 0 {
                let lane = candidates.trailing_zeros() as usize;
                candidates &= candidates - 1;
                if !is_full(ctrl[lane]) {
                    continue;
                }
                let index = group * GROUP_LEN + lane;
                // SAFETY: the lane's control byte is full, so the slot is
                // initialised, and a masked group plus a lane below
                // `GROUP_LEN` is inside the table.
                if eq(unsafe { self.entry(index) }) {
                    return Probe {
                        slot: Some(Slot::Occupied(index)),
                        groups: examined,
                    };
                }
            }

            if vacancy.is_none() && found.free != 0 {
                let lane = found.free.trailing_zeros() as usize;
                if !is_full(ctrl[lane]) {
                    vacancy = Some(group * GROUP_LEN + lane);
                }
            }
            if found.empty != 0 {
                return Probe {
                    slot: vacancy.map(Slot::Vacant),
                    groups: examined,
                };
            }

            group = (group + step) & mask;
        }

        Probe {
            slot: vacancy.map(Slot::Vacant),
            groups: examined,
        }
    }

    /// Write `value` into the vacant slot `index`, which must have come from a
    /// [`Slot::Vacant`] on this table.
    ///
    /// # Safety
    ///
    /// `index` must be below `self.buckets` and its control byte must not be
    /// full.
    pub(crate) unsafe fn fill(&mut self, index: usize, hash: u64, value: T) {
        // SAFETY: the caller's bound puts the control byte inside the array.
        let was = unsafe { self.ctrl_at(index) };
        debug_assert!(
            was != EMPTY || self.growth_left > 0,
            "a fresh slot is only taken against reserved growth",
        );
        // SAFETY: as above; the slot is uninitialised, so the write does not
        // overwrite a live value.
        unsafe {
            self.slot(index).write(value);
            self.set_ctrl(index, tag_of(hash));
        }
        self.items += 1;
        // Filling a tombstone reuses capacity the tombstone already consumed;
        // only a fresh slot spends any.
        if was == EMPTY {
            self.growth_left -= 1;
        }
    }

    /// Take the entry out of the full slot `index`.
    ///
    /// # Safety
    ///
    /// `index` must be below `self.buckets` and its control byte must be full.
    pub(crate) unsafe fn take(&mut self, index: usize) -> T {
        // SAFETY: a full control byte means the slot holds an initialised
        // value, which the control-byte write below stops anyone reading
        // again.
        let value = unsafe { ptr::read(self.slot(index)) };
        let group = index / GROUP_LEN;
        // SAFETY: `index` is inside the table, so its group is too.
        let ctrl = unsafe { self.group_at(group) };
        // A group that already holds an empty lane has no probe chain running
        // through it, so emptying this slot cannot strand an entry further
        // along a chain. Otherwise the slot becomes a tombstone, which keeps
        // every chain through this group intact.
        // Only the empty mask is read, so the tag passed in is immaterial.
        let (mark, reclaimed) = if scan(&ctrl, 0).empty == 0 {
            (DELETED, 0)
        } else {
            (EMPTY, 1)
        };
        // SAFETY: `index` is inside the table.
        unsafe { self.set_ctrl(index, mark) };
        self.items -= 1;
        self.growth_left += reclaimed;
        value
    }

    /// Make room for `additional` further entries, rebuilding if the current
    /// allocation cannot take them.
    ///
    /// `hash` recomputes an entry's hash for the rebuild.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the allocation fails or the requested capacity
    /// cannot be represented. The table is left untouched.
    pub(crate) fn try_reserve(
        &mut self,
        additional: usize,
        hash: impl Fn(&T) -> u64,
    ) -> Result<(), TryReserveError> {
        if additional <= self.growth_left {
            return Ok(());
        }
        let wanted = self
            .items
            .checked_add(additional)
            .ok_or(TryReserveError::CapacityOverflow)?;
        self.rebuild(buckets_for(wanted)?, hash)
    }

    /// Move every entry into a fresh allocation of `buckets` slots, dropping
    /// the tombstones the old one accumulated.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the allocation fails. The table is left
    /// untouched.
    fn rebuild(&mut self, buckets: usize, hash: impl Fn(&T) -> u64) -> Result<(), TryReserveError> {
        debug_assert!(buckets.is_power_of_two() && buckets >= GROUP_LEN);
        debug_assert!(limit_of(buckets) >= self.items);

        let (layout, ctrl_offset) = Self::block(buckets)?;
        // SAFETY: `buckets` is at least `GROUP_LEN`, so the block holds at
        // least that many control bytes and its size is non-zero.
        let base = NonNull::new(unsafe { alloc::alloc::alloc(layout) })
            .ok_or(TryReserveError::AllocFailed)?;
        // SAFETY: `ctrl_offset` is the control array's offset inside the block
        // just allocated, so the pointer is inside it and non-null.
        let ctrl = unsafe { NonNull::new_unchecked(base.as_ptr().add(ctrl_offset)) };
        // SAFETY: the block holds `buckets` control bytes at `ctrl`; marking
        // them all empty is what makes them initialised.
        unsafe { ctrl.as_ptr().write_bytes(EMPTY, buckets) };

        let mut fresh = Self {
            buckets,
            ctrl,
            slots: base.cast::<T>(),
            items: 0,
            growth_left: limit_of(buckets),
            marker: PhantomData,
        };

        for index in 0..self.buckets {
            // SAFETY: `index` is inside this table.
            if !is_full(unsafe { self.ctrl_at(index) }) {
                continue;
            }
            // SAFETY: the control byte is full, so the slot holds an
            // initialised value. Marking the slot empty in the same breath is
            // what makes the move single-owner: whatever `hash` does next,
            // this table no longer claims the value.
            let value = unsafe {
                let value = ptr::read(self.slot(index));
                self.set_ctrl(index, EMPTY);
                value
            };
            self.items -= 1;
            let hash = hash(&value);
            // Every key in the old table is distinct, so no comparison is
            // needed; the chain ends at the first free lane.
            let Some(Slot::Vacant(target)) = fresh.probe(hash, |_| false).slot else {
                // Unreachable while the new table's limit admits every entry,
                // which `buckets_for` guarantees. Dropping the value here
                // rather than writing it somewhere unsound is the fail-closed
                // answer if that ever stops holding.
                debug_assert!(false, "rebuilt table has no vacancy");
                continue;
            };
            // SAFETY: `probe` reports a vacant slot inside `fresh` whose
            // control byte is not full.
            unsafe { fresh.fill(target, hash, value) };
        }

        // Every slot of the old table has been moved out and marked empty,
        // so `ManuallyDrop` suppresses only the walk, never a live value.
        let old = ManuallyDrop::new(core::mem::replace(self, fresh));
        Self::free_block(old.buckets, old.slots);
        Ok(())
    }

    /// Release the one allocation `buckets` slots at `slots` were carved from.
    fn free_block(buckets: usize, slots: NonNull<T>) {
        if buckets == 0 {
            return;
        }
        if let Ok((layout, _)) = Self::block(buckets) {
            // SAFETY: `layout` is recomputed from the same bucket count the
            // block was allocated with, and `slots` is that block's base.
            unsafe { alloc::alloc::dealloc(slots.as_ptr().cast::<u8>(), layout) };
        }
    }

    /// Drop every entry, keeping the allocation.
    pub(crate) fn clear(&mut self) {
        for index in 0..self.buckets {
            // SAFETY: `index` is inside this table.
            if is_full(unsafe { self.ctrl_at(index) }) {
                // SAFETY: the control byte is full, so the slot holds an
                // initialised value; it is marked empty below before anything
                // can read it again.
                unsafe { ptr::drop_in_place(self.slot(index)) };
            }
        }
        if self.buckets != 0 {
            // SAFETY: the control array holds exactly `buckets` bytes.
            unsafe { self.ctrl.as_ptr().write_bytes(EMPTY, self.buckets) };
        }
        self.items = 0;
        self.growth_left = limit_of(self.buckets);
    }

    /// An iterator over the addresses of the live entries.
    pub(crate) fn iter(&self) -> RawIter<T> {
        RawIter {
            ctrl: self.ctrl.as_ptr(),
            slots: self.slots.as_ptr(),
            index: 0,
            buckets: self.buckets,
            remaining: self.items,
        }
    }
}

impl<T> Drop for RawTable<T> {
    fn drop(&mut self) {
        self.clear();
        Self::free_block(self.buckets, self.slots);
    }
}

/// An iterator over the addresses of a table's live entries.
///
/// Yields raw pointers so the shared and unique views of a table share one
/// walk; the containers turn them into references of the right lifetime.
pub(crate) struct RawIter<T> {
    ctrl: *const u8,
    slots: *mut T,
    index: usize,
    buckets: usize,
    remaining: usize,
}

impl<T> Iterator for RawIter<T> {
    type Item = NonNull<T>;

    fn next(&mut self) -> Option<NonNull<T>> {
        while self.index < self.buckets {
            let index = self.index;
            self.index += 1;
            // SAFETY: `index` is below `buckets`, so the control byte is
            // inside the array the iterator was built from and initialised.
            if is_full(unsafe { *self.ctrl.add(index) }) {
                self.remaining -= 1;
                // SAFETY: a full control byte means the slot is initialised,
                // and it is inside the slot array.
                return Some(unsafe { NonNull::new_unchecked(self.slots.add(index)) });
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<T> ExactSizeIterator for RawIter<T> {
    fn len(&self) -> usize {
        self.remaining
    }
}

#[cfg(test)]
mod tests {
    use super::{buckets_for, limit_of, tag_of, RawTable, Slot};
    use crate::group::GROUP_LEN;

    #[test]
    fn bucket_sizing_admits_exactly_the_requested_entries() {
        for entries in 0..600usize {
            let buckets = buckets_for(entries).expect("representable");
            assert!(buckets.is_power_of_two());
            assert!(buckets >= GROUP_LEN);
            assert!(limit_of(buckets) >= entries, "{entries} entries");
            if buckets > GROUP_LEN {
                assert!(
                    limit_of(buckets / 2) < entries,
                    "{entries} entries fit a smaller table",
                );
            }
        }
    }

    #[test]
    fn bucket_sizing_reports_overflow_rather_than_wrapping() {
        assert!(buckets_for(usize::MAX).is_err());
    }

    #[test]
    fn a_tag_never_collides_with_a_control_marker() {
        for shift in 0..64 {
            assert!(super::is_full(tag_of(1u64 << shift)));
        }
        assert!(super::is_full(tag_of(u64::MAX)));
    }

    #[test]
    fn an_unallocated_table_probes_without_touching_memory() {
        let table = RawTable::<u64>::new();
        let probe = table.probe(0xdead_beef, |_| true);
        assert!(probe.slot.is_none());
        assert_eq!(probe.groups, 0);
        assert_eq!(table.allocated_bytes(), 0);
    }

    #[test]
    fn a_reserved_table_takes_and_returns_entries() {
        let mut table = RawTable::<u64>::new();
        table.try_reserve(1, |v| *v).expect("reserve");
        let Some(Slot::Vacant(index)) = table.probe(7, |v| *v == 7).slot else {
            panic!("a reserved table must offer a vacancy");
        };
        // SAFETY: `index` came from a vacancy this table reported.
        unsafe { table.fill(index, 7, 7) };
        assert_eq!(table.len(), 1);
        let Some(Slot::Occupied(found)) = table.probe(7, |v| *v == 7).slot else {
            panic!("the entry must be found again");
        };
        // SAFETY: `found` came from an occupancy this table reported.
        assert_eq!(unsafe { table.take(found) }, 7);
        assert_eq!(table.len(), 0);
    }
}
