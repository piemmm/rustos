//! Generic copy-on-write B-tree over fixed-size records keyed by `u64`
//! (`docs/src/filesystem/arxfs-spec.md` §4 / §6 — copy-on-write metadata trees).
//!
//! One implementation backs both Stage-2 trees (no
//! duplicated structure):
//!
//! * the **inode tree**, keyed by inode number, whose value is a packed
//!   256-byte inode record, and
//! * a per-file **extent tree**, keyed by a file's logical block offset,
//!   whose value is the physical run that backs it.
//!
//! Each tree node is one self-identifying metadata block ([`BlockType::Btree`],
//! `header` module). It is a B+-tree: a leaf (`level == 0`) stores
//! `(key, value)` pairs in key order; an internal node stores
//! `(separator_key, child)` pairs where `separator_key` is the smallest key in
//! `child`, so a lookup descends to the last child whose separator is `<= key`.
//!
//! Mutations are copy-on-write: a touched node is rewritten through
//! [`cow_meta`](crate::ARXFS::cow_meta) to a fresh (or transaction-private)
//! block and the change bubbles up to a (possibly new) root, so a node
//! reachable from the committed root is never overwritten in place
//! (`docs/src/filesystem/arxfs-spec.md` §2). Overflowing nodes split and underflowing nodes
//! borrow from or merge with a sibling, so the tree grows and shrinks without
//! a fixed record cap. Every path is `Result`-based and panic-free; there is no `unsafe`.
//!
//! # Iteration and mutation are both bounded
//!
//! No path here recurses, and none holds more than a fixed handful of node
//! buffers, so the stack and the resident bytes an operation needs are set by
//! the block size rather than by the tree's depth or its record count.
//!
//! [`TreeWalk`] is the only way to read more than one entry: it yields at most
//! one leaf node's entries per step into its own block-sized buffer. Its
//! position is a single key, so a walk both survives mutation of the tree
//! between steps and can be persisted and resumed in a later call.
//! [`NodeTrail`] turns that key-order walk into the node enumeration the
//! free-space rebuild and whole-tree freeing need, holding one path instead of
//! a node list.
//!
//! [`ARXFS::btree_insert`] and [`ARXFS::btree_remove`] descend once recording
//! the path, edit the leaf, then walk back up rewriting each ancestor in turn
//! through the [`TreeEdit`] scratch the mount lends them. Only one node is
//! being edited at a time — plus the adjacent pair a split, borrow, or merge
//! moves entries between — and every edit happens in place in a node buffer,
//! so nothing is decoded per record.

use alloc::vec::Vec;

use tairix_abi::DriverError;

use crate::header::{BlockType, HEADER_LEN};
use crate::{as_u32, rd_u32, rd_u64, wr_u32, Block, ARXFS, MAX_BLOCK_SIZE};

/// Node payload byte offsets, relative to the start of the block buffer
/// (the node payload begins right after the [`HEADER_LEN`] block header).
pub(crate) const N_COUNT: usize = HEADER_LEN;
pub(crate) const N_LEVEL: usize = HEADER_LEN + 4;
pub(crate) const N_ENTRIES: usize = HEADER_LEN + 8;

/// Internal-entry stride: separator key (`u64`) plus child pointer (`u64`).
pub(crate) const INTERNAL_STRIDE: usize = 16;

/// Deepest node level a descent will accept.
///
/// This is a validation bound, not a capacity. The narrowest legal geometry —
/// a 512-byte block, whose internal node holds 23 entries and rebalances down
/// to 11 — spans every block of a 2^48-block device within 16 levels, so no
/// well-formed tree approaches this. Refusing a deeper level is what stops a
/// child pointer that leads back to an ancestor from descending forever.
pub(crate) const MAX_TREE_LEVEL: u32 = 32;

/// Path slots a descent records: one per level, root first.
const PATH_SLOTS: usize = MAX_TREE_LEVEL as usize + 1;

/// Widest entry stride any of the driver's trees uses.
///
/// A [`TreeEdit`] node buffer carries one entry of this width past the block,
/// so an insert can lay a full node out and then split it — no second layout
/// path for the overflowing case, and no third buffer.
const MAX_ENTRY_STRIDE: usize = 8 + crate::MAX_TREE_VALUE_LEN;

/// Static description of one B-tree's record shape, so the single node code
/// serves trees with different value widths and block owners.
#[derive(Copy, Clone)]
pub(crate) struct TreeSpec {
    /// Value width in bytes stored beside each key in a leaf.
    pub value_len: usize,
    /// Owner object recorded in every node block's header.
    pub owner: u64,
}

impl TreeSpec {
    /// Leaf-entry stride: key (`u64`) plus the value.
    fn leaf_stride(self) -> usize {
        8 + self.value_len
    }
}

/// A node-sized scratch buffer, fallibly allocated: a filesystem read or
/// mutation degrades to an error rather than aborting the system on a refused
/// allocation.
fn node_buf(len: usize) -> Result<Vec<u8>, DriverError> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|_| DriverError::NoSpace)?;
    buf.resize(len, 0);
    Ok(buf)
}

fn node_count(buf: &[u8]) -> usize {
    rd_u32(buf, N_COUNT) as usize
}

fn node_level(buf: &[u8]) -> u32 {
    rd_u32(buf, N_LEVEL)
}

/// The smallest key held in node buffer `buf` (its first entry's key); both
/// leaf and internal entries begin with the key, so the stride is irrelevant.
fn node_min_key(buf: &[u8]) -> u64 {
    rd_u64(buf, N_ENTRIES)
}

/// The smallest key of a node that must not be empty.
///
/// # Errors
///
/// [`DriverError::DeviceFault`] when the node holds nothing. Only a node this
/// operation just emptied may be empty, and it is refilled or merged away
/// before its parent names it, so an empty node reaching here is a tree shape
/// the driver never wrote.
fn node_required_min(buf: &[u8]) -> Result<u64, DriverError> {
    if node_count(buf) == 0 {
        return Err(DriverError::DeviceFault);
    }
    Ok(node_min_key(buf))
}

/// Key of entry `at`. The caller's index comes from a count validated against
/// the block's capacity, so the field is inside the buffer.
fn entry_key(buf: &[u8], at: usize, stride: usize) -> u64 {
    rd_u64(buf, N_ENTRIES + at * stride)
}

/// Separator key of internal entry `at`.
fn internal_key(buf: &[u8], at: usize) -> u64 {
    entry_key(buf, at, INTERNAL_STRIDE)
}

/// Child pointer of internal entry `at`.
fn internal_child(buf: &[u8], at: usize) -> u64 {
    rd_u64(buf, N_ENTRIES + at * INTERNAL_STRIDE + 8)
}

/// The `slots` entry slots of `buf` as one checked region.
///
/// Every edit that moves entries takes its bytes this way, so an index the
/// buffer cannot hold is a fail-closed device fault rather than a panic.
fn entry_region(buf: &mut [u8], slots: usize, stride: usize) -> Result<&mut [u8], DriverError> {
    let end = slots
        .checked_mul(stride)
        .and_then(|len| N_ENTRIES.checked_add(len))
        .ok_or(DriverError::DeviceFault)?;
    buf.get_mut(N_ENTRIES..end).ok_or(DriverError::DeviceFault)
}

/// Overwrite entry `at` with `(key, value)`, leaving the entry count alone.
fn node_set_entry(
    buf: &mut [u8],
    at: usize,
    stride: usize,
    key: u64,
    value: &[u8],
) -> Result<(), DriverError> {
    if at >= node_count(buf) || value.len() + 8 != stride {
        return Err(DriverError::DeviceFault);
    }
    let region = entry_region(buf, at + 1, stride)?;
    let slot = region
        .get_mut(at * stride..)
        .ok_or(DriverError::DeviceFault)?;
    slot[..8].copy_from_slice(&key.to_le_bytes());
    slot[8..].copy_from_slice(value);
    Ok(())
}

/// Point internal entry `at` at `child`, under separator `key`.
fn node_set_child(buf: &mut [u8], at: usize, key: u64, child: u64) -> Result<(), DriverError> {
    node_set_entry(buf, at, INTERNAL_STRIDE, key, &child.to_le_bytes())
}

/// Shift entries `[at, count)` up one slot and write `(key, value)` into the
/// gap, so the node holds one entry more.
///
/// A node already at capacity spills into the slack a [`TreeEdit`] buffer
/// carries past the block; the caller splits it before it is written.
fn node_insert_entry(
    buf: &mut [u8],
    stride: usize,
    at: usize,
    key: u64,
    value: &[u8],
) -> Result<(), DriverError> {
    let count = node_count(buf);
    if at > count || value.len() + 8 != stride {
        return Err(DriverError::DeviceFault);
    }
    let region = entry_region(buf, count + 1, stride)?;
    region.copy_within(at * stride..count * stride, (at + 1) * stride);
    let slot = region
        .get_mut(at * stride..(at + 1) * stride)
        .ok_or(DriverError::DeviceFault)?;
    slot[..8].copy_from_slice(&key.to_le_bytes());
    slot[8..].copy_from_slice(value);
    wr_u32(buf, N_COUNT, as_u32(count + 1));
    Ok(())
}

/// Drop entry `at`, closing the gap behind it.
fn node_remove_entry(buf: &mut [u8], stride: usize, at: usize) -> Result<(), DriverError> {
    let count = node_count(buf);
    if at >= count {
        return Err(DriverError::DeviceFault);
    }
    let region = entry_region(buf, count, stride)?;
    region.copy_within((at + 1) * stride.., at * stride);
    wr_u32(buf, N_COUNT, as_u32(count - 1));
    Ok(())
}

/// Move entry `from` of `src` to index `to` of `dst`: one entry crossing
/// between adjacent siblings, which is what a borrow is.
fn node_move_entry(
    src: &mut [u8],
    from: usize,
    dst: &mut [u8],
    to: usize,
    stride: usize,
) -> Result<(), DriverError> {
    if from >= node_count(src) {
        return Err(DriverError::DeviceFault);
    }
    let base = N_ENTRIES + from * stride;
    let slot = src
        .get(base..base + stride)
        .ok_or(DriverError::DeviceFault)?;
    let key = rd_u64(slot, 0);
    let value = slot.get(8..).ok_or(DriverError::DeviceFault)?;
    node_insert_entry(dst, stride, to, key, value)?;
    node_remove_entry(src, stride, from)
}

/// Append every entry of `src` to `dst`: the merge of an adjacent pair.
fn node_append(dst: &mut [u8], src: &[u8], stride: usize) -> Result<(), DriverError> {
    let held = node_count(dst);
    let moving = node_count(src);
    let total = held.checked_add(moving).ok_or(DriverError::DeviceFault)?;
    let base = N_ENTRIES + held * stride;
    let end = base
        .checked_add(moving * stride)
        .ok_or(DriverError::DeviceFault)?;
    let from = src
        .get(N_ENTRIES..N_ENTRIES + moving * stride)
        .ok_or(DriverError::DeviceFault)?;
    dst.get_mut(base..end)
        .ok_or(DriverError::DeviceFault)?
        .copy_from_slice(from);
    wr_u32(dst, N_COUNT, as_u32(total));
    Ok(())
}

/// Where `key` sits in leaf `buf`: the index of an exact match, or the index
/// it would be inserted at, and which of the two it is.
///
/// # Errors
///
/// [`DriverError::DeviceFault`] when the leaf's keys do not ascend. Such a
/// leaf would hide an existing key from this search — inserting a duplicate —
/// and would leave the parent naming a separator the subtree does not start
/// at, so it is refused rather than edited.
fn leaf_search(
    buf: &[u8],
    count: usize,
    stride: usize,
    key: u64,
) -> Result<(usize, bool), DriverError> {
    let mut at = count;
    let mut found = false;
    for i in 0..count {
        let k = entry_key(buf, i, stride);
        if i > 0 && k <= entry_key(buf, i - 1, stride) {
            return Err(DriverError::DeviceFault);
        }
        if at == count {
            if k == key {
                at = i;
                found = true;
            } else if k > key {
                at = i;
            }
        }
    }
    Ok((at, found))
}

/// What one level of a mutation hands to the level above it.
struct Ascent {
    /// The node's address after its copy-on-write.
    phys: u64,
    /// Its smallest key, or `None` for a node a remove emptied.
    min: Option<u64>,
    /// Entries it holds, which is what tells the parent whether it underflowed.
    count: usize,
    /// The promoted separator and new right sibling of a node that split.
    split: Option<(u64, u64)>,
}

/// Where a descent ended up: the nodes it passed through, root first, and the
/// smallest key that begins a subtree *after* the one it landed in.
///
/// That second value is the key a walk resumes at once the landing leaf is
/// exhausted, and its absence means the landing leaf is the tree's last.
struct Descent {
    path: [u64; PATH_SLOTS],
    depth: usize,
    next_subtree: Option<u64>,
}

impl Descent {
    const fn new() -> Self {
        Self {
            path: [0; PATH_SLOTS],
            depth: 0,
            next_subtree: None,
        }
    }

    /// The node recorded at `slot`, root first. A slot the descent did not
    /// reach holds no node, so asking for one is refused rather than answered
    /// with a stale or zeroed address.
    fn path_at(&self, slot: usize) -> Result<u64, DriverError> {
        self.path
            .get(..self.depth)
            .and_then(|reached| reached.get(slot))
            .copied()
            .ok_or(DriverError::DeviceFault)
    }
}

/// Index of the last of `count` entries whose key is `<= key`, or `0` when
/// `key` precedes them all: the child an internal node's search descends into.
/// `key_at` reads the key of one entry, so a descent and the mutation path's
/// walk back up share this one definition and cannot disagree about which
/// child a key belongs to.
fn child_index(
    count: usize,
    key: u64,
    key_at: impl Fn(usize) -> u64,
) -> Result<usize, DriverError> {
    if count == 0 {
        // An internal node with no children cannot be searched, and treating
        // it as "child 0" would follow a zeroed pointer.
        return Err(DriverError::DeviceFault);
    }
    let mut chosen = 0usize;
    for i in 0..count {
        if key_at(i) <= key {
            chosen = i;
        } else {
            break;
        }
    }
    Ok(chosen)
}

/// Scratch one mutation borrows for its duration: the node it is editing, the
/// adjacent pair a split, borrow, or merge moves entries between, and the path
/// it descended.
///
/// The mount holds one and lends it out, so a steady-state insert or remove
/// allocates nothing; and because every node buffer lives here, the entry
/// point's stack frame is a few hundred bytes whatever the tree's depth.
pub(crate) struct TreeEdit {
    /// The node being edited: the leaf, then each ancestor in turn.
    node: Vec<u8>,
    /// The lower of an adjacent sibling pair.
    lo: Vec<u8>,
    /// The higher of that pair, and the node a split fills.
    hi: Vec<u8>,
    descent: Descent,
}

impl TreeEdit {
    /// A scratch for a tree of `block_size`-byte nodes.
    ///
    /// # Errors
    ///
    /// [`DriverError::NoSpace`] when a node buffer cannot be allocated; a
    /// mutation holding one allocates nothing thereafter.
    pub(crate) fn new(block_size: usize) -> Result<Self, DriverError> {
        let len = block_size
            .checked_add(MAX_ENTRY_STRIDE)
            .ok_or(DriverError::NoSpace)?;
        Ok(Self {
            node: node_buf(len)?,
            lo: node_buf(len)?,
            hi: node_buf(len)?,
            descent: Descent::new(),
        })
    }
}

impl<B: Block> ARXFS<B> {
    /// Maximum leaf entries that fit one node block.
    fn btree_leaf_cap(&self, spec: TreeSpec) -> usize {
        (self.block_size - N_ENTRIES) / spec.leaf_stride()
    }

    /// Maximum internal entries (separator + child) that fit one node block.
    fn btree_internal_cap(&self) -> usize {
        (self.block_size - N_ENTRIES) / INTERNAL_STRIDE
    }

    /// Entries one node block holds at `level`.
    fn btree_node_cap(&self, level: u32, spec: TreeSpec) -> usize {
        if level == 0 {
            self.btree_leaf_cap(spec)
        } else {
            self.btree_internal_cap()
        }
    }

    /// Minimum entries a non-root node keeps before it borrows or merges.
    fn btree_min(&self, level: u32, spec: TreeSpec) -> usize {
        (self.btree_node_cap(level, spec) / 2).max(1)
    }

    /// Entry stride of the node in `buf`: a leaf stores the tree's value, an
    /// internal node a child pointer. Taken from the node rather than passed
    /// in, so no caller can address one with the other's stride.
    fn btree_stride(buf: &[u8], spec: TreeSpec) -> usize {
        if node_level(buf) == 0 {
            spec.leaf_stride()
        } else {
            INTERNAL_STRIDE
        }
    }

    /// Zero a fresh node buffer and stamp its `level`/`count`.
    pub(crate) fn btree_init_node(&self, buf: &mut [u8], level: u32, count: usize) {
        for byte in &mut buf[HEADER_LEN..self.block_size] {
            *byte = 0;
        }
        wr_u32(buf, N_LEVEL, level);
        wr_u32(buf, N_COUNT, as_u32(count));
    }

    /// Read the node at `phys` into `buf` and validate the two shapes every
    /// other reader then trusts: that it sits at the level the caller
    /// expects, and that its entry count fits the block.
    ///
    /// Levels strictly decrease on the way down and increase by one on the way
    /// back up, so a child pointer leading back to an ancestor is refused here
    /// instead of descending forever, and an entry count wider than the block
    /// would otherwise index past the buffer. Both are impossible in a tree
    /// this driver wrote, which is why meeting one is a fail-closed device
    /// fault rather than a repair.
    fn btree_read_node(
        &mut self,
        phys: u64,
        expect_level: Option<u32>,
        spec: TreeSpec,
        buf: &mut [u8],
    ) -> Result<(u32, usize), DriverError> {
        self.read_meta(phys, BlockType::Btree, buf)?;
        let level = node_level(buf);
        if level > MAX_TREE_LEVEL || expect_level.is_some_and(|expect| expect != level) {
            return Err(DriverError::DeviceFault);
        }
        let count = node_count(buf);
        if count > self.btree_node_cap(level, spec) {
            return Err(DriverError::DeviceFault);
        }
        Ok((level, count))
    }

    /// Descend from `root` to the leaf that would hold `key`, leaving that
    /// leaf's bytes in `buf` and returning its entry count.
    ///
    /// `trace`, when given, records the path taken and the smallest key
    /// beginning a later subtree, which is what lets a walk step to the next
    /// leaf, a mutation rewrite each ancestor on the way back up, and a caller
    /// report the nodes it entered. The descent is the one place a search
    /// chooses a child, so a lookup, a walk, and a mutation can never disagree
    /// about where a key lives.
    fn btree_descend(
        &mut self,
        root: u64,
        key: u64,
        spec: TreeSpec,
        buf: &mut [u8],
        mut trace: Option<&mut Descent>,
    ) -> Result<usize, DriverError> {
        if let Some(trace) = trace.as_deref_mut() {
            trace.depth = 0;
            trace.next_subtree = None;
        }
        let mut phys = root;
        let mut expect_level = None;
        loop {
            let (level, count) = self.btree_read_node(phys, expect_level, spec, buf)?;
            if let Some(trace) = trace.as_deref_mut() {
                let slot = trace.depth;
                if slot >= PATH_SLOTS {
                    return Err(DriverError::DeviceFault);
                }
                trace.path[slot] = phys;
                trace.depth = slot + 1;
            }
            if level == 0 {
                return Ok(count);
            }
            let chosen = child_index(count, key, |i| internal_key(buf, i))?;
            if let Some(trace) = trace.as_deref_mut() {
                if chosen + 1 < count {
                    // A separator is the smallest key in its child, so the one
                    // after the chosen child starts the next subtree. Deeper
                    // levels overwrite shallower ones, leaving the tightest
                    // bound the path offers.
                    trace.next_subtree = Some(internal_key(buf, chosen + 1));
                }
            }
            phys = internal_child(buf, chosen);
            expect_level = Some(level - 1);
        }
    }

    /// Look up `key`, returning its value bytes when present.
    pub(crate) fn btree_get(
        &mut self,
        root: u64,
        key: u64,
        spec: TreeSpec,
    ) -> Result<Option<Vec<u8>>, DriverError> {
        if root == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let count = self.btree_descend(root, key, spec, &mut buf, None)?;
        let stride = spec.leaf_stride();
        for i in 0..count {
            let base = N_ENTRIES + i * stride;
            if rd_u64(&buf, base) == key {
                return Ok(Some(buf[base + 8..base + stride].to_vec()));
            }
        }
        Ok(None)
    }

    /// Look up the entry with the largest key `<= key` (a "floor" query),
    /// returning `(key, value)`. Extent trees use it to find the run that
    /// covers a logical block whose offset may fall inside the run.
    pub(crate) fn btree_get_floor(
        &mut self,
        root: u64,
        key: u64,
        spec: TreeSpec,
    ) -> Result<Option<(u64, Vec<u8>)>, DriverError> {
        if root == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let count = self.btree_descend(root, key, spec, &mut buf, None)?;
        let stride = spec.leaf_stride();
        let mut found: Option<(u64, Vec<u8>)> = None;
        for i in 0..count {
            let base = N_ENTRIES + i * stride;
            let k = rd_u64(&buf, base);
            if k <= key {
                found = Some((k, buf[base + 8..base + stride].to_vec()));
            } else {
                break;
            }
        }
        Ok(found)
    }

    /// Borrow the mount's mutation scratch.
    ///
    /// Allocates one when the mount has not needed it yet — a read-only handle
    /// never mutates, so it never pays for one — and when a mutation already
    /// holds it, so the call is total rather than conditional on nothing else
    /// being in flight.
    fn btree_take_edit(&mut self) -> Result<TreeEdit, DriverError> {
        match self.tree_edit.take() {
            Some(edit) => Ok(edit),
            None => TreeEdit::new(self.block_size),
        }
    }

    /// Return the scratch, keeping one for the mount so the next mutation
    /// allocates nothing.
    fn btree_put_edit(&mut self, edit: TreeEdit) {
        if self.tree_edit.is_none() {
            self.tree_edit = Some(edit);
        }
    }

    /// Zero the payload past the node's entries and copy-on-write it to
    /// `phys`, returning its new address.
    ///
    /// The tail zeroing keeps a node that shrank from carrying its old entries
    /// onto the device, so a node's bytes depend only on what it holds.
    /// Refusing a count the block cannot fit is the last guard before a node
    /// reaches the media.
    fn btree_write_node(
        &mut self,
        phys: u64,
        buf: &mut [u8],
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        let stride = Self::btree_stride(buf, spec);
        let used = node_count(buf)
            .checked_mul(stride)
            .and_then(|len| N_ENTRIES.checked_add(len))
            .ok_or(DriverError::DeviceFault)?;
        if used > self.block_size {
            return Err(DriverError::DeviceFault);
        }
        for byte in &mut buf[used..self.block_size] {
            *byte = 0;
        }
        self.cow_meta(phys, buf, BlockType::Btree, spec.owner, 0)
    }

    /// Write `buf` back to `phys` and report what its parent must record.
    ///
    /// `buf` still holds the published node's payload afterwards — sealing
    /// rewrites only the block header — which is what lets the caller read the
    /// node it just wrote without a device round trip.
    fn btree_publish(
        &mut self,
        buf: &mut [u8],
        phys: u64,
        spec: TreeSpec,
    ) -> Result<Ascent, DriverError> {
        let count = node_count(buf);
        let min = (count > 0).then(|| node_min_key(buf));
        let phys = self.btree_write_node(phys, buf, spec)?;
        Ok(Ascent {
            phys,
            min,
            count,
            split: None,
        })
    }

    /// Publish the node being edited, splitting it into `edit.hi` first when
    /// the edit pushed it past its block's capacity.
    fn btree_publish_split(
        &mut self,
        edit: &mut TreeEdit,
        phys: u64,
        spec: TreeSpec,
    ) -> Result<Ascent, DriverError> {
        let level = node_level(&edit.node);
        let separator = if node_count(&edit.node) > self.btree_node_cap(level, spec) {
            let mid = node_count(&edit.node) / 2;
            Some(self.btree_split_off(&mut edit.node, &mut edit.hi, spec, mid)?)
        } else {
            None
        };
        let mut up = self.btree_publish(&mut edit.node, phys, spec)?;
        if let Some(separator) = separator {
            up.split = Some((separator, self.btree_write_node(0, &mut edit.hi, spec)?));
        }
        Ok(up)
    }

    /// Move entries `[mid, count)` of `buf` into the fresh node `dst` at the
    /// same level, returning the smallest key now in `dst` — the separator its
    /// parent records for it.
    fn btree_split_off(
        &self,
        buf: &mut [u8],
        dst: &mut [u8],
        spec: TreeSpec,
        mid: usize,
    ) -> Result<u64, DriverError> {
        let count = node_count(buf);
        if mid == 0 || mid >= count {
            return Err(DriverError::DeviceFault);
        }
        let stride = Self::btree_stride(buf, spec);
        let moving = count - mid;
        let from = buf
            .get(N_ENTRIES + mid * stride..N_ENTRIES + count * stride)
            .ok_or(DriverError::DeviceFault)?;
        self.btree_init_node(dst, node_level(buf), moving);
        dst.get_mut(N_ENTRIES..N_ENTRIES + moving * stride)
            .ok_or(DriverError::DeviceFault)?
            .copy_from_slice(from);
        wr_u32(buf, N_COUNT, as_u32(mid));
        Ok(node_min_key(dst))
    }

    /// Insert or replace `key -> value`, returning the (possibly new) root.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] when `value` is not the tree's record
    /// width; [`DriverError::DeviceFault`] on a node that fails to
    /// authenticate or a tree shape no driver-written tree can have;
    /// [`DriverError::NoSpace`] when the volume cannot back the copy-on-write
    /// or the tree would grow past [`MAX_TREE_LEVEL`].
    pub(crate) fn btree_insert(
        &mut self,
        root: u64,
        key: u64,
        value: &[u8],
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        if value.len() != spec.value_len {
            return Err(DriverError::LengthOutOfRange);
        }
        let mut edit = self.btree_take_edit()?;
        let out = self.btree_insert_into(&mut edit, root, key, value, spec);
        self.btree_put_edit(edit);
        out
    }

    /// The body of [`Self::btree_insert`], holding the borrowed scratch.
    ///
    /// One descent records the path; the leaf takes the record in place; then
    /// each ancestor in turn is re-read, has the child's separator and pointer
    /// refreshed, adopts any promoted split, and is copy-on-written. Nothing
    /// recurses and nothing but the scratch is held, so an insert costs the
    /// same stack at any depth and decodes nothing per record.
    fn btree_insert_into(
        &mut self,
        edit: &mut TreeEdit,
        root: u64,
        key: u64,
        value: &[u8],
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        let stride = spec.leaf_stride();
        if root == 0 {
            self.btree_init_node(&mut edit.node, 0, 0);
            node_insert_entry(&mut edit.node, stride, 0, key, value)?;
            return self.btree_write_node(0, &mut edit.node, spec);
        }
        let count = self.btree_descend(root, key, spec, &mut edit.node, Some(&mut edit.descent))?;
        let ancestors = edit
            .descent
            .depth
            .checked_sub(1)
            .ok_or(DriverError::DeviceFault)?;
        let (at, replace) = leaf_search(&edit.node, count, stride, key)?;
        if replace {
            node_set_entry(&mut edit.node, at, stride, key, value)?;
        } else {
            node_insert_entry(&mut edit.node, stride, at, key, value)?;
        }

        let leaf = edit.descent.path_at(ancestors)?;
        let mut up = self.btree_publish_split(edit, leaf, spec)?;
        // The leaf sits at level 0, so the node at `slot` sits at
        // `ancestors - slot` and the root, which the loop leaves in `up`, at
        // `ancestors`.
        for (child_level, slot) in (0..ancestors).rev().enumerate() {
            let parent = edit.descent.path_at(slot)?;
            let (_, held) =
                self.btree_read_node(parent, Some(as_u32(child_level + 1)), spec, &mut edit.node)?;
            let ci = child_index(held, key, |i| internal_key(&edit.node, i))?;
            // An insert never empties a node, so the child always has a
            // smallest key to be named by.
            let separator = up.min.ok_or(DriverError::DeviceFault)?;
            node_set_child(&mut edit.node, ci, separator, up.phys)?;
            if let Some((promoted, right)) = up.split {
                node_insert_entry(
                    &mut edit.node,
                    INTERNAL_STRIDE,
                    ci + 1,
                    promoted,
                    &right.to_le_bytes(),
                )?;
            }
            up = self.btree_publish_split(edit, parent, spec)?;
        }
        match up.split {
            None => Ok(up.phys),
            Some(_) => self.btree_grow_root(edit, as_u32(ancestors + 1), &up, spec),
        }
    }

    /// Publish a fresh root one level above a root that split.
    fn btree_grow_root(
        &mut self,
        edit: &mut TreeEdit,
        level: u32,
        up: &Ascent,
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        let (promoted, right) = up.split.ok_or(DriverError::DeviceFault)?;
        let left_min = up.min.ok_or(DriverError::DeviceFault)?;
        if level > MAX_TREE_LEVEL {
            // A tree deeper than any descent will follow could not be read
            // back, so it is refused rather than written.
            return Err(DriverError::NoSpace);
        }
        self.btree_init_node(&mut edit.node, level, 0);
        node_insert_entry(
            &mut edit.node,
            INTERNAL_STRIDE,
            0,
            left_min,
            &up.phys.to_le_bytes(),
        )?;
        node_insert_entry(
            &mut edit.node,
            INTERNAL_STRIDE,
            1,
            promoted,
            &right.to_le_bytes(),
        )?;
        self.btree_write_node(0, &mut edit.node, spec)
    }

    /// Remove `key` if present, returning the (possibly new, possibly `0`)
    /// root.
    ///
    /// # Errors
    ///
    /// As [`Self::btree_insert`], less the record-width check.
    pub(crate) fn btree_remove(
        &mut self,
        root: u64,
        key: u64,
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        if root == 0 {
            return Ok(0);
        }
        let mut edit = self.btree_take_edit()?;
        let out = self.btree_remove_from(&mut edit, root, key, spec);
        self.btree_put_edit(edit);
        out
    }

    /// The body of [`Self::btree_remove`], holding the borrowed scratch.
    ///
    /// The mirror of [`Self::btree_insert_into`]: one descent, the leaf drops
    /// the record in place, then each ancestor is re-read and rewritten,
    /// rebalancing a child the removal left below its minimum occupancy before
    /// it does.
    fn btree_remove_from(
        &mut self,
        edit: &mut TreeEdit,
        root: u64,
        key: u64,
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        let stride = spec.leaf_stride();
        let count = self.btree_descend(root, key, spec, &mut edit.node, Some(&mut edit.descent))?;
        let ancestors = edit
            .descent
            .depth
            .checked_sub(1)
            .ok_or(DriverError::DeviceFault)?;
        let (at, found) = leaf_search(&edit.node, count, stride, key)?;
        if !found {
            return Ok(root);
        }
        node_remove_entry(&mut edit.node, stride, at)?;

        let leaf = edit.descent.path_at(ancestors)?;
        let mut up = self.btree_publish(&mut edit.node, leaf, spec)?;
        for (child_level, slot) in (0..ancestors).rev().enumerate() {
            let child_level = as_u32(child_level);
            let parent = edit.descent.path_at(slot)?;
            let (_, held) =
                self.btree_read_node(parent, Some(child_level + 1), spec, &mut edit.node)?;
            let ci = child_index(held, key, |i| internal_key(&edit.node, i))?;
            // A child the removal emptied has no key left to name it, so its
            // separator stands until the rebalance below or the root collapse
            // takes the entry away.
            let separator = up.min.unwrap_or_else(|| internal_key(&edit.node, ci));
            node_set_child(&mut edit.node, ci, separator, up.phys)?;
            if up.count < self.btree_min(child_level, spec) && held >= 2 {
                self.btree_rebalance(edit, ci, child_level, spec)?;
            }
            up = self.btree_publish(&mut edit.node, parent, spec)?;
        }
        self.btree_collapse_root(edit, &up, spec)
    }

    /// Restore child `ci`'s minimum occupancy by borrowing from or merging
    /// with its adjacent sibling, editing the parent in `edit.node` to match.
    ///
    /// The pair is staged in key order in `edit.lo` and `edit.hi`, so a borrow
    /// is one direction of a single entry move and a merge always folds the
    /// higher node into the lower — whichever side of the pair the underfull
    /// child is on.
    fn btree_rebalance(
        &mut self,
        edit: &mut TreeEdit,
        ci: usize,
        level: u32,
        spec: TreeSpec,
    ) -> Result<(), DriverError> {
        // The leftmost child has only a right sibling; every other pairs with
        // the one below it.
        let lo_index = ci.saturating_sub(1);
        let hi_index = lo_index + 1;
        if hi_index >= node_count(&edit.node) {
            return Err(DriverError::DeviceFault);
        }
        let lo_phys = internal_child(&edit.node, lo_index);
        let hi_phys = internal_child(&edit.node, hi_index);
        let (_, lo_count) = self.btree_read_node(lo_phys, Some(level), spec, &mut edit.lo)?;
        let (_, hi_count) = self.btree_read_node(hi_phys, Some(level), spec, &mut edit.hi)?;
        let stride = Self::btree_stride(&edit.lo, spec);
        let child_is_lo = ci == lo_index;
        let donor = if child_is_lo { hi_count } else { lo_count };
        if donor > self.btree_min(level, spec) {
            if child_is_lo {
                node_move_entry(&mut edit.hi, 0, &mut edit.lo, lo_count, stride)?;
            } else {
                node_move_entry(&mut edit.lo, lo_count - 1, &mut edit.hi, 0, stride)?;
            }
            let lo_min = node_required_min(&edit.lo)?;
            let hi_min = node_required_min(&edit.hi)?;
            let lo = self.btree_write_node(lo_phys, &mut edit.lo, spec)?;
            let hi = self.btree_write_node(hi_phys, &mut edit.hi, spec)?;
            node_set_child(&mut edit.node, lo_index, lo_min, lo)?;
            return node_set_child(&mut edit.node, hi_index, hi_min, hi);
        }
        // Neither side can spare an entry: fold the pair into its lower node,
        // so the parent loses exactly one entry whichever side underflowed.
        node_append(&mut edit.lo, &edit.hi, stride)?;
        let lo_min = node_required_min(&edit.lo)?;
        let lo = self.btree_write_node(lo_phys, &mut edit.lo, spec)?;
        self.free_meta(hi_phys);
        node_set_child(&mut edit.node, lo_index, lo_min, lo)?;
        node_remove_entry(&mut edit.node, INTERNAL_STRIDE, hi_index)
    }

    /// Reduce a root the removal left degenerate, returning the tree's root.
    ///
    /// An emptied leaf root leaves no tree at all, and an internal root down to
    /// one child is replaced by that child — repeatedly, so a root is always
    /// either a leaf or names two or more subtrees. `edit.node` still holds the
    /// root as it was just written, which is what makes the common case (no
    /// collapse) free of a device read.
    fn btree_collapse_root(
        &mut self,
        edit: &mut TreeEdit,
        up: &Ascent,
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        let mut phys = up.phys;
        let mut level = node_level(&edit.node);
        let mut count = up.count;
        loop {
            if level == 0 {
                if count == 0 {
                    self.free_meta(phys);
                    return Ok(0);
                }
                return Ok(phys);
            }
            if count != 1 {
                return Ok(phys);
            }
            let only = internal_child(&edit.node, 0);
            self.free_meta(phys);
            phys = only;
            (level, count) = self.btree_read_node(phys, Some(level - 1), spec, &mut edit.node)?;
        }
    }
}

impl<B: Block> ARXFS<B> {
    /// Parse the level and, for an internal node, the child pointers out of an
    /// already-verified node buffer `buf`. Used by the scrub verifying walk,
    /// which authenticates each node itself (counting any companion repair)
    /// before descending, so it must not re-read the node through the
    /// repair-on-read [`crate::ARXFS::read_meta`] path. A leaf returns level
    /// `0` and no children.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] when the node claims a level past
    /// [`MAX_TREE_LEVEL`] or more entries than its block holds — shapes no
    /// tree this driver wrote can have, so they are refused rather than
    /// silently truncated.
    pub(crate) fn btree_node_children(&self, buf: &[u8]) -> Result<(u32, Vec<u64>), DriverError> {
        let level = node_level(buf);
        if level > MAX_TREE_LEVEL {
            return Err(DriverError::DeviceFault);
        }
        let count = node_count(buf);
        if level == 0 {
            return Ok((0, Vec::new()));
        }
        if count > self.btree_internal_cap() {
            return Err(DriverError::DeviceFault);
        }
        let mut children = Vec::new();
        children
            .try_reserve_exact(count)
            .map_err(|_| DriverError::NoSpace)?;
        for i in 0..count {
            children.push(rd_u64(buf, N_ENTRIES + i * INTERNAL_STRIDE + 8));
        }
        Ok((level, children))
    }

    /// Advance `walk` to the tree's next leaf entries, returning whether it
    /// yielded any.
    ///
    /// One call reads one root-to-leaf path and yields the entries of that
    /// leaf at or after the walk's position — at most one node's worth,
    /// already in the walk's buffer, so nothing is allocated per step and a
    /// caller's resident bytes do not grow with the tree. The walk then stands
    /// at the first key of the next leaf, which it reaches by descending
    /// afresh: that is what lets a caller mutate the tree, or stop and resume
    /// from a persisted key ([`TreeWalk::seek`]), between calls.
    ///
    /// Every step either ends the walk or leaves it standing at a strictly
    /// higher key, so a walk over any tree — including a corrupt one — always
    /// terminates.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] when a node fails to authenticate or the
    /// tree's shape is impossible (a level that does not decrease, an entry
    /// count wider than the block, keys that do not ascend within a leaf). A
    /// walk never ends early and quietly on a corrupt tree, because a caller
    /// freeing or accounting for every entry would then miss some.
    pub(crate) fn btree_next_leaf(
        &mut self,
        root: u64,
        spec: TreeSpec,
        walk: &mut TreeWalk,
    ) -> Result<bool, DriverError> {
        let stride = spec.leaf_stride();
        walk.stride = stride;
        walk.first = 0;
        walk.count = 0;
        if root == 0 {
            walk.stop();
            return Ok(false);
        }
        while let Some(key) = walk.next {
            let count =
                self.btree_descend(root, key, spec, &mut walk.buf, Some(&mut walk.descent))?;
            let first = (0..count).find(|i| entry_key(&walk.buf, *i, stride) >= key);
            let Some(first) = first else {
                // The landing leaf holds only keys below the position, so the
                // next entry — if there is one — starts the following subtree.
                match walk.descent.next_subtree {
                    None => {
                        walk.stop();
                        return Ok(false);
                    }
                    // The descent stops at the first separator above the
                    // position, so a later subtree always starts after it and
                    // this retry cannot stand still.
                    Some(bound) => walk.next = Some(bound),
                }
                continue;
            };
            walk.first = first;
            walk.count = count - first;
            let last = entry_key(&walk.buf, count - 1, stride);
            walk.next = match last.checked_add(1) {
                // The largest key a tree can hold was just yielded.
                None => None,
                // Keys ascend within a leaf, so the last of the entries this
                // step yields cannot be below the position it started at. A
                // leaf that says otherwise cannot be walked in order at all:
                // stepping past its last key would skip the entries above it.
                Some(after) if after <= key => return Err(DriverError::DeviceFault),
                Some(after) => walk.descent.next_subtree.map(|bound| bound.max(after)),
            };
            return Ok(true);
        }
        walk.stop();
        Ok(false)
    }
}

/// A bounded, resumable walk over a tree's leaf entries in key order.
///
/// The walk owns one node-sized buffer and holds its position as a single
/// key, so it costs the same whether the tree has ten entries or a hundred
/// million: [`ARXFS::btree_next_leaf`] fills the buffer with one leaf's worth
/// of entries, and the caller reads them through [`Self::entries`] before
/// asking for the next. Because the position is a key rather than a pointer
/// into the tree, a caller may mutate the tree between steps, and may stop at
/// an entry and persist that entry's key to [`Self::seek`] back to in a later
/// call — a walk resumed that way yields exactly what an uninterrupted one
/// would.
pub(crate) struct TreeWalk {
    /// The current leaf's bytes; exactly one filesystem block wide.
    buf: Vec<u8>,
    descent: Descent,
    /// Index within the leaf of the first yielded entry.
    first: usize,
    count: usize,
    stride: usize,
    /// The key the next step starts at, or `None` once the tree is exhausted.
    next: Option<u64>,
}

impl TreeWalk {
    /// A walk over a tree of `block_size`-byte nodes, positioned before every
    /// key.
    ///
    /// # Errors
    ///
    /// [`DriverError::NoSpace`] when the one node-sized buffer cannot be
    /// allocated; the walk allocates nothing thereafter.
    pub(crate) fn new(block_size: usize) -> Result<Self, DriverError> {
        Ok(Self {
            buf: node_buf(block_size)?,
            descent: Descent::new(),
            first: 0,
            count: 0,
            stride: 0,
            next: Some(0),
        })
    }

    /// Position the walk at `key`, so the next step yields the first entry at
    /// or after it.
    pub(crate) fn seek(&mut self, key: u64) {
        self.next = Some(key);
        self.first = 0;
        self.count = 0;
    }

    /// Position the walk before every key, for reuse over another tree.
    pub(crate) fn restart(&mut self) {
        self.seek(0);
        self.descent.depth = 0;
        self.descent.next_subtree = None;
    }

    /// The last step's entries, in key order.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (u64, &[u8])> + '_ {
        let (first, stride) = (self.first, self.stride);
        (0..self.count).map(move |i| {
            let base = N_ENTRIES + (first + i) * stride;
            (rd_u64(&self.buf, base), &self.buf[base + 8..base + stride])
        })
    }

    /// The nodes the last step descended through, root first.
    pub(crate) fn path(&self) -> &[u64] {
        &self.descent.path[..self.descent.depth]
    }

    /// End the walk: no further step yields anything. A caller that has just
    /// consumed the largest key a tree can hold stops here rather than
    /// looking for a key beyond it.
    pub(crate) fn stop(&mut self) {
        self.next = None;
        self.descent.depth = 0;
        self.descent.next_subtree = None;
    }
}

/// Which nodes a key-order walk has entered and finished with.
///
/// A walk visits paths in key order, so the moment a level of the path
/// changes, the node that stood there has no keys left beneath it and every
/// deeper level of the new path is a node just entered. That turns the leaf
/// walk into a node enumeration for the callers that need one — the
/// free-space rebuild marking every node used, freeing a whole tree — with a
/// single path's worth of state instead of the tree's node list, and without a
/// second traversal to maintain.
pub(crate) struct NodeTrail {
    open: [u64; PATH_SLOTS],
    open_depth: usize,
    entered: [u64; PATH_SLOTS],
    entered_len: usize,
    closed: [u64; PATH_SLOTS],
    closed_len: usize,
}

impl NodeTrail {
    pub(crate) const fn new() -> Self {
        Self {
            open: [0; PATH_SLOTS],
            open_depth: 0,
            entered: [0; PATH_SLOTS],
            entered_len: 0,
            closed: [0; PATH_SLOTS],
            closed_len: 0,
        }
    }

    /// Move the trail onto `path`, recomputing [`Self::entered`] and
    /// [`Self::closed`].
    pub(crate) fn advance(&mut self, path: &[u64]) {
        let path = &path[..path.len().min(PATH_SLOTS)];
        let common = path
            .iter()
            .zip(&self.open[..self.open_depth])
            .take_while(|(new, open)| new == open)
            .count();
        self.closed_len = self.open_depth - common;
        for (slot, node) in self.open[common..self.open_depth].iter().rev().enumerate() {
            self.closed[slot] = *node;
        }
        self.entered_len = path.len() - common;
        self.entered[..self.entered_len].copy_from_slice(&path[common..]);
        self.open[..path.len()].copy_from_slice(path);
        self.open_depth = path.len();
    }

    /// Nodes the last [`Self::advance`] entered, shallowest first.
    pub(crate) fn entered(&self) -> &[u64] {
        &self.entered[..self.entered_len]
    }

    /// Nodes whose subtree the last [`Self::advance`] finished, deepest first.
    pub(crate) fn closed(&self) -> &[u64] {
        &self.closed[..self.closed_len]
    }

    /// The nodes still open, deepest first, leaving the trail empty: what a
    /// caller freeing a tree has left to free once the walk ends.
    pub(crate) fn close(&mut self) -> &[u64] {
        self.advance(&[]);
        self.closed()
    }
}
