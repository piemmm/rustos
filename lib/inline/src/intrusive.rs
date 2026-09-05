//! A doubly-linked list threaded through nodes the caller owns.

use core::fmt;
use core::iter::FusedIterator;

/// The largest index a [`Link`] can name.
///
/// A link is two words wide and carries its on-a-list state inside them,
/// which costs the two index values above this one. Nothing in the tree comes
/// near it: a node occupies at least a link's worth of storage, so a store of
/// `MAX_INDEX` nodes has no representable size on any Tier-1 target.
pub const MAX_INDEX: usize = usize::MAX - 2;

/// No node: a link end with no neighbour, or an empty list's ends.
const NONE: usize = usize::MAX;

/// Stored in both ends of a node that is on no list.
const OFF_LIST: usize = usize::MAX - 1;

/// Decode a stored end, both sentinels reading as "no node".
const fn node(end: usize) -> Option<usize> {
    if end > MAX_INDEX {
        None
    } else {
        Some(end)
    }
}

/// The pair of links one node parks in the caller's storage.
///
/// Two words, and they carry the on-a-list state as well as the neighbours: a
/// separate flag would round the link up to three words, and a free list with
/// one link per page cannot afford one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Link {
    prev: usize,
    next: usize,
}

impl Link {
    /// A link on no list. `const`, so an array of them can back a `static`.
    pub const UNLINKED: Self = Self {
        prev: OFF_LIST,
        next: OFF_LIST,
    };

    /// A link on no list.
    #[must_use]
    pub const fn new() -> Self {
        Self::UNLINKED
    }

    /// Whether some list holds this node.
    #[must_use]
    pub const fn is_linked(&self) -> bool {
        self.prev != OFF_LIST
    }

    /// The node before this one, or `None` at the front of a list — and also
    /// `None` when the node is on no list, which [`Self::is_linked`] is what
    /// distinguishes.
    #[must_use]
    pub const fn prev(&self) -> Option<usize> {
        node(self.prev)
    }

    /// The node after this one, under the same reading as [`Self::prev`].
    #[must_use]
    pub const fn next(&self) -> Option<usize> {
        node(self.next)
    }
}

impl Default for Link {
    fn default() -> Self {
        Self::UNLINKED
    }
}

/// The nodes a list threads through, addressed by index.
///
/// The store is the caller's: a bare `[Link]`, or its own slot type carrying a
/// link field. Several lists may share one store — a per-order free list array
/// is exactly that — and then the caller is the one that knows which list a
/// node is on and must unlink it from that one. [`IntrusiveList::unlink`]
/// refuses a node that claims an end this list does not hold, but an interior
/// node of a sibling list is indistinguishable from one of its own without a
/// list identity in every link, which would cost a word per node.
///
/// An implementation that answers inconsistently can make a list *wrong*, not
/// unsound: every index is bounds-checked and no link is dereferenced as a
/// pointer. That is what index-addressing buys.
pub trait LinkStore {
    /// The links of the node at `index`, or `None` when the store has no such
    /// node.
    fn link(&self, index: usize) -> Option<&Link>;

    /// As [`Self::link`], for writing.
    fn link_mut(&mut self, index: usize) -> Option<&mut Link>;
}

impl LinkStore for [Link] {
    fn link(&self, index: usize) -> Option<&Link> {
        self.get(index)
    }

    fn link_mut(&mut self, index: usize) -> Option<&mut Link> {
        self.get_mut(index)
    }
}

/// Why a splice was refused.
///
/// Every variant is reported *before* anything is written, so a refused
/// operation leaves the list and the store exactly as they were.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LinkError {
    /// The index is above [`MAX_INDEX`], which the link encoding reserves.
    IndexReserved,
    /// The store holds no node at that index.
    NoSuchNode,
    /// The node is already on a list, and a node is on at most one.
    AlreadyLinked,
    /// The node is on no list, or claims an end that this list does not hold.
    NotLinked,
    /// A link names a node the store does not hold, or a neighbour does not
    /// link back: the store's links have been corrupted, so the splice is
    /// refused rather than performed.
    Corrupt,
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IndexReserved => "node index is reserved by the link encoding",
            Self::NoSuchNode => "no such node in the store",
            Self::AlreadyLinked => "node is already on a list",
            Self::NotLinked => "node is not on this list",
            Self::Corrupt => "the store's links are inconsistent",
        })
    }
}

/// A doubly-linked list over nodes the caller owns, allocating nothing.
///
/// The list is a three-word header; the links live in the caller's store (see
/// [`LinkStore`]) and every operation takes it. That is what buys the property
/// an owning container cannot offer: a node is unlinked in constant time from
/// its index alone, with no search and no allocation, so a free list can drop
/// an arbitrary block and a recency list can move an arbitrary entry to the
/// front.
///
/// # Ordering
///
/// There is no policy of its own: the caller chooses the discipline by which
/// end it pushes and pops. [`push_back`](Self::push_back) with
/// [`pop_front`](Self::pop_front) is FIFO — first-come service for a wait set,
/// and the order that cannot starve a waiter.
/// [`push_front`](Self::push_front) with [`pop_back`](Self::pop_back) is
/// recency: most-recently-touched at the front,
/// [`move_to_front`](Self::move_to_front) is the touch, and the eviction
/// candidate is the back.
///
/// # Failure
///
/// Nothing here panics and nothing allocates. A splice that cannot be
/// performed consistently is refused with a [`LinkError`] before it writes, so
/// a store whose links have been corrupted stops the list rather than being
/// followed. The two pops answer with [`Option`], reporting empty for a front
/// the list cannot splice out — a caller drains with
/// `while let Some(i) = list.pop_front(store)`, never by counting
/// [`len`](Self::len) down.
#[derive(Debug, Eq, PartialEq)]
pub struct IntrusiveList {
    head: usize,
    tail: usize,
    len: usize,
}

impl IntrusiveList {
    /// An empty list. `const`, so `[const { IntrusiveList::new() }; N]` gives
    /// a per-order or per-bucket array of them, and one can back a `static`.
    ///
    /// The header is neither `Copy` nor `Clone` on purpose: a duplicate would
    /// name the same nodes, and unlinking through one would leave the other
    /// counting a node it no longer reaches.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            head: NONE,
            tail: NONE,
            len: 0,
        }
    }

    /// Nodes on the list.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the list holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The node at the front, or `None` when the list is empty.
    #[must_use]
    pub const fn front(&self) -> Option<usize> {
        node(self.head)
    }

    /// The node at the back, or `None` when the list is empty.
    #[must_use]
    pub const fn back(&self) -> Option<usize> {
        node(self.tail)
    }

    /// Link `index` in as the new front.
    ///
    /// # Errors
    ///
    /// [`LinkError::IndexReserved`] or [`LinkError::NoSuchNode`] when the
    /// store cannot name the node, [`LinkError::AlreadyLinked`] when some list
    /// already holds it, and [`LinkError::Corrupt`] when the store no longer
    /// holds an end this list is linked into.
    pub fn push_front<S>(&mut self, store: &mut S, index: usize) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        Self::admit(store, index, self.head)?;
        Self::write(store, index, NONE, self.head)?;
        match node(self.head) {
            Some(head) => Self::set_prev(store, head, index)?,
            None => self.tail = index,
        }
        self.head = index;
        self.count_up();
        Ok(())
    }

    /// Link `index` in as the new back.
    ///
    /// # Errors
    ///
    /// As [`Self::push_front`].
    pub fn push_back<S>(&mut self, store: &mut S, index: usize) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        Self::admit(store, index, self.tail)?;
        Self::write(store, index, self.tail, NONE)?;
        match node(self.tail) {
            Some(tail) => Self::set_next(store, tail, index)?,
            None => self.head = index,
        }
        self.tail = index;
        self.count_up();
        Ok(())
    }

    /// Link `index` in immediately after `anchor`.
    ///
    /// # Errors
    ///
    /// As [`Self::push_front`], plus [`LinkError::NotLinked`] when `anchor` is
    /// on no list or is a back this list does not hold, and
    /// [`LinkError::Corrupt`] when `anchor`'s follower does not link back to
    /// it.
    pub fn insert_after<S>(
        &mut self,
        store: &mut S,
        anchor: usize,
        index: usize,
    ) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        let after = self.anchor_next(store, anchor)?;
        if Self::read(store, index)?.is_linked() {
            return Err(LinkError::AlreadyLinked);
        }
        Self::write(store, index, anchor, after)?;
        Self::set_next(store, anchor, index)?;
        match node(after) {
            Some(follower) => Self::set_prev(store, follower, index)?,
            None => self.tail = index,
        }
        self.count_up();
        Ok(())
    }

    /// Link `index` in immediately before `anchor`.
    ///
    /// # Errors
    ///
    /// As [`Self::insert_after`], mirrored: `anchor` must not be a front this
    /// list does not hold, and its leader must link back to it.
    pub fn insert_before<S>(
        &mut self,
        store: &mut S,
        anchor: usize,
        index: usize,
    ) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        let before = self.anchor_prev(store, anchor)?;
        if Self::read(store, index)?.is_linked() {
            return Err(LinkError::AlreadyLinked);
        }
        Self::write(store, index, before, anchor)?;
        Self::set_prev(store, anchor, index)?;
        match node(before) {
            Some(leader) => Self::set_next(store, leader, index)?,
            None => self.head = index,
        }
        self.count_up();
        Ok(())
    }

    /// Unlink the front node and report it, or `None` when the list is empty
    /// or its front cannot be spliced out.
    pub fn pop_front<S>(&mut self, store: &mut S) -> Option<usize>
    where
        S: LinkStore + ?Sized,
    {
        let front = self.front()?;
        self.unlink(store, front).ok().map(|()| front)
    }

    /// Unlink the back node and report it, under the same reading as
    /// [`Self::pop_front`].
    pub fn pop_back<S>(&mut self, store: &mut S) -> Option<usize>
    where
        S: LinkStore + ?Sized,
    {
        let back = self.back()?;
        self.unlink(store, back).ok().map(|()| back)
    }

    /// Unlink `index`, in constant time and without a search.
    ///
    /// # Errors
    ///
    /// [`LinkError::IndexReserved`] or [`LinkError::NoSuchNode`] when the
    /// store cannot name the node, [`LinkError::NotLinked`] when it is on no
    /// list or claims an end this list does not hold, and
    /// [`LinkError::Corrupt`] when a neighbour is absent or does not link back
    /// to it.
    pub fn unlink<S>(&mut self, store: &mut S, index: usize) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        let link = Self::read(store, index)?;
        if !link.is_linked() {
            return Err(LinkError::NotLinked);
        }
        if link.prev == NONE && self.head != index {
            return Err(LinkError::NotLinked);
        }
        if link.next == NONE && self.tail != index {
            return Err(LinkError::NotLinked);
        }
        if let Some(leader) = link.prev() {
            if Self::reached(store, leader)?.next() != Some(index) {
                return Err(LinkError::Corrupt);
            }
        }
        if let Some(follower) = link.next() {
            if Self::reached(store, follower)?.prev() != Some(index) {
                return Err(LinkError::Corrupt);
            }
        }
        // A node counted twice would underflow this, so the count is reduced
        // only once the reduction is known to be sound.
        let len = self.len.checked_sub(1).ok_or(LinkError::NotLinked)?;

        match link.prev() {
            Some(leader) => Self::set_next(store, leader, link.next)?,
            None => self.head = link.next,
        }
        match link.next() {
            Some(follower) => Self::set_prev(store, follower, link.prev)?,
            None => self.tail = link.prev,
        }
        Self::write(store, index, OFF_LIST, OFF_LIST)?;
        self.len = len;
        Ok(())
    }

    /// Move `index` to the front — the recency touch, constant time.
    ///
    /// # Errors
    ///
    /// As [`Self::unlink`]. A node already at the front is left alone.
    pub fn move_to_front<S>(&mut self, store: &mut S, index: usize) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        if self.front() == Some(index) {
            return Ok(());
        }
        self.unlink(store, index)?;
        self.push_front(store, index)
    }

    /// Move `index` to the back, under the same reading as
    /// [`Self::move_to_front`].
    ///
    /// # Errors
    ///
    /// As [`Self::unlink`].
    pub fn move_to_back<S>(&mut self, store: &mut S, index: usize) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        if self.back() == Some(index) {
            return Ok(());
        }
        self.unlink(store, index)?;
        self.push_back(store, index)
    }

    /// Unlink every node, leaving each link unlinked, and report how many were
    /// detached.
    ///
    /// The list is emptied whatever the walk finds, so a store whose links have
    /// been corrupted mid-chain leaves the list holding nothing rather than a
    /// link it cannot follow — a caller comparing the count against the
    /// [`len`](Self::len) it had is what notices.
    pub fn clear<S>(&mut self, store: &mut S) -> usize
    where
        S: LinkStore + ?Sized,
    {
        let mut cursor = self.front();
        let mut detached = 0;
        while let Some(index) = cursor {
            if detached == self.len {
                break;
            }
            let Some(link) = store.link_mut(index) else {
                break;
            };
            cursor = link.next();
            *link = Link::UNLINKED;
            detached += 1;
        }
        self.head = NONE;
        self.tail = NONE;
        self.len = 0;
        detached
    }

    /// Walk the list front to back, or back to front through
    /// [`Iterator::rev`], yielding node indices and allocating nothing.
    ///
    /// The walk is bounded by [`len`](Self::len), so a store whose links have
    /// been corrupted into a cycle ends the iteration rather than spinning.
    #[must_use]
    pub fn iter<'a, S>(&self, store: &'a S) -> Iter<'a, S>
    where
        S: LinkStore + ?Sized,
    {
        Iter {
            store,
            front: self.head,
            back: self.tail,
            remaining: self.len,
        }
    }

    /// A distinct index is never counted twice, so the count cannot exceed the
    /// store's node count and the saturation is unreachable.
    fn count_up(&mut self) {
        self.len = self.len.saturating_add(1);
    }

    /// Check that this list may take `index`, and that the end it will splice
    /// against is still in the store. Nothing is written until every node the
    /// splice touches is known to exist, so a refusal changes nothing.
    fn admit<S>(store: &S, index: usize, against: usize) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        if Self::read(store, index)?.is_linked() {
            return Err(LinkError::AlreadyLinked);
        }
        if let Some(end) = node(against) {
            Self::reached(store, end)?;
        }
        Ok(())
    }

    /// Validate `anchor` as an insertion point and report the stored end that
    /// currently follows it.
    fn anchor_next<S>(&self, store: &S, anchor: usize) -> Result<usize, LinkError>
    where
        S: LinkStore + ?Sized,
    {
        let link = Self::read(store, anchor)?;
        if !link.is_linked() {
            return Err(LinkError::NotLinked);
        }
        match link.next() {
            None if self.tail != anchor => Err(LinkError::NotLinked),
            None => Ok(NONE),
            Some(follower) if Self::reached(store, follower)?.prev() != Some(anchor) => {
                Err(LinkError::Corrupt)
            }
            Some(_) => Ok(link.next),
        }
    }

    /// [`Self::anchor_next`] mirrored.
    fn anchor_prev<S>(&self, store: &S, anchor: usize) -> Result<usize, LinkError>
    where
        S: LinkStore + ?Sized,
    {
        let link = Self::read(store, anchor)?;
        if !link.is_linked() {
            return Err(LinkError::NotLinked);
        }
        match link.prev() {
            None if self.head != anchor => Err(LinkError::NotLinked),
            None => Ok(NONE),
            Some(leader) if Self::reached(store, leader)?.next() != Some(anchor) => {
                Err(LinkError::Corrupt)
            }
            Some(_) => Ok(link.prev),
        }
    }

    fn read<S>(store: &S, index: usize) -> Result<Link, LinkError>
    where
        S: LinkStore + ?Sized,
    {
        if index > MAX_INDEX {
            return Err(LinkError::IndexReserved);
        }
        store.link(index).copied().ok_or(LinkError::NoSuchNode)
    }

    /// Read a node the list already reaches: one of its own ends, or a
    /// neighbour a stored link names. Absence there is not the caller naming a
    /// bad index but a store that no longer holds what it was linked into.
    fn reached<S>(store: &S, index: usize) -> Result<Link, LinkError>
    where
        S: LinkStore + ?Sized,
    {
        store.link(index).copied().ok_or(LinkError::Corrupt)
    }

    fn write<S>(store: &mut S, index: usize, prev: usize, next: usize) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        *store.link_mut(index).ok_or(LinkError::NoSuchNode)? = Link { prev, next };
        Ok(())
    }

    fn set_prev<S>(store: &mut S, index: usize, prev: usize) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        store.link_mut(index).ok_or(LinkError::Corrupt)?.prev = prev;
        Ok(())
    }

    fn set_next<S>(store: &mut S, index: usize, next: usize) -> Result<(), LinkError>
    where
        S: LinkStore + ?Sized,
    {
        store.link_mut(index).ok_or(LinkError::Corrupt)?.next = next;
        Ok(())
    }
}

impl Default for IntrusiveList {
    fn default() -> Self {
        Self::new()
    }
}

/// A front-to-back walk over an [`IntrusiveList`], yielding node indices.
pub struct Iter<'a, S: LinkStore + ?Sized> {
    store: &'a S,
    front: usize,
    back: usize,
    remaining: usize,
}

impl<S: LinkStore + ?Sized> Iterator for Iter<'_, S> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.remaining == 0 {
            return None;
        }
        let Some(index) = node(self.front) else {
            self.exhaust();
            return None;
        };
        self.front = self.store.link(index).map_or(NONE, |link| link.next);
        self.remaining -= 1;
        if self.remaining == 0 {
            self.exhaust();
        }
        Some(index)
    }

    /// The list's length is the upper bound, never a promise: the store is the
    /// caller's, so a chain that ends early — a corrupted link, or a store
    /// this list was not threaded through — is a shorter walk rather than a
    /// spin. That is also why this is no `ExactSizeIterator`.
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.remaining))
    }
}

impl<S: LinkStore + ?Sized> DoubleEndedIterator for Iter<'_, S> {
    fn next_back(&mut self) -> Option<usize> {
        if self.remaining == 0 {
            return None;
        }
        let Some(index) = node(self.back) else {
            self.exhaust();
            return None;
        };
        self.back = self.store.link(index).map_or(NONE, |link| link.prev);
        self.remaining -= 1;
        if self.remaining == 0 {
            self.exhaust();
        }
        Some(index)
    }
}

impl<S: LinkStore + ?Sized> FusedIterator for Iter<'_, S> {}

impl<S: LinkStore + ?Sized> Iter<'_, S> {
    /// End the walk from both ends at once, so a chain that stopped early
    /// cannot be resumed and the remaining count cannot outlive it.
    fn exhaust(&mut self) {
        self.front = NONE;
        self.back = NONE;
        self.remaining = 0;
    }
}

#[cfg(test)]
#[path = "intrusive_tests.rs"]
mod tests;
