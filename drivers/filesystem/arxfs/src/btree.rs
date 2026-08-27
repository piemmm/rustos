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
//! # Iteration
//!
//! [`TreeWalk`] is the only way to read more than one entry: it yields at most
//! one leaf node's entries per step into its own block-sized buffer, so the
//! bytes a caller holds are bounded by the node size rather than by the tree,
//! whatever the volume's size. Its position is a single key, so a walk both
//! survives mutation of the tree between steps and can be persisted and
//! resumed in a later call. [`NodeTrail`] turns that key-order walk into the
//! node enumeration the free-space rebuild and whole-tree freeing need,
//! holding one path instead of a node list.

use alloc::vec::Vec;

use tairix_abi::DriverError;

use crate::header::{BlockType, HEADER_LEN};
use crate::{rd_u64, wr_u64, Block, ARXFS, MAX_BLOCK_SIZE};

/// Node payload byte offsets, relative to the start of the block buffer
/// (the node payload begins right after the [`HEADER_LEN`] block header).
const N_COUNT: usize = HEADER_LEN;
const N_LEVEL: usize = HEADER_LEN + 4;
const N_ENTRIES: usize = HEADER_LEN + 8;

/// Internal-entry stride: separator key (`u64`) plus child pointer (`u64`).
const INTERNAL_STRIDE: usize = 16;

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

/// A node's entries as `(key, value_bytes)` pairs (a leaf record or, for an
/// internal node, a child pointer).
pub(crate) type NodeEntries = Vec<(u64, Vec<u8>)>;

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn wr_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
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

/// Outcome of inserting into a subtree: the visited node's (possibly new)
/// physical address, and, when the node split, the separator key promoted to
/// the parent and the new right sibling's physical address.
struct InsertOutcome {
    node_phys: u64,
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
}

/// Index of the last of `count` entries whose key is `<= key`, or `0` when
/// `key` precedes them all: the child an internal node's search descends into.
/// `key_at` reads the key of one entry, so the buffer-backed search and the
/// decoded-entry search of the mutation path share this one definition.
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

impl<B: Block> ARXFS<B> {
    /// Maximum leaf entries that fit one node block.
    fn btree_leaf_cap(&self, spec: TreeSpec) -> usize {
        (self.block_size - N_ENTRIES) / spec.leaf_stride()
    }

    /// Maximum internal entries (separator + child) that fit one node block.
    fn btree_internal_cap(&self) -> usize {
        (self.block_size - N_ENTRIES) / INTERNAL_STRIDE
    }

    /// Minimum entries a non-root node keeps before it borrows or merges.
    fn btree_min(&self, level: u32, spec: TreeSpec) -> usize {
        let cap = if level == 0 {
            self.btree_leaf_cap(spec)
        } else {
            self.btree_internal_cap()
        };
        (cap / 2).max(1)
    }

    /// Zero a fresh node buffer and stamp its `level`/`count`.
    fn btree_init_node(&self, buf: &mut [u8], level: u32, count: usize) {
        for byte in &mut buf[HEADER_LEN..self.block_size] {
            *byte = 0;
        }
        wr_u32(buf, N_LEVEL, level);
        wr_u32(buf, N_COUNT, crate::as_u32(count));
    }

    /// Read the node at `phys` into `buf` and validate the two shapes every
    /// other reader then trusts: that it sits at the level the descent
    /// expects, and that its entry count fits the block.
    ///
    /// Levels strictly decrease on the way down, so a child pointer leading
    /// back to an ancestor is refused here instead of descending forever, and
    /// an entry count wider than the block would otherwise index past the
    /// buffer. Both are impossible in a tree this driver wrote, which is why
    /// meeting one is a fail-closed device fault rather than a repair.
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
        let cap = if level == 0 {
            self.btree_leaf_cap(spec)
        } else {
            self.btree_internal_cap()
        };
        if count > cap {
            return Err(DriverError::DeviceFault);
        }
        Ok((level, count))
    }

    /// Descend from `root` to the leaf that would hold `key`, leaving that
    /// leaf's bytes in `buf` and returning its entry count.
    ///
    /// `trace`, when given, records the path taken and the smallest key
    /// beginning a later subtree, which is what lets a walk step to the next
    /// leaf and report the nodes it entered. The descent is the one place a
    /// search chooses a child, so a lookup and a walk can never disagree
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
            let chosen = child_index(count, key, |i| rd_u64(buf, N_ENTRIES + i * INTERNAL_STRIDE))?;
            if let Some(trace) = trace.as_deref_mut() {
                if chosen + 1 < count {
                    // A separator is the smallest key in its child, so the one
                    // after the chosen child starts the next subtree. Deeper
                    // levels overwrite shallower ones, leaving the tightest
                    // bound the path offers.
                    trace.next_subtree =
                        Some(rd_u64(buf, N_ENTRIES + (chosen + 1) * INTERNAL_STRIDE));
                }
            }
            phys = rd_u64(buf, N_ENTRIES + chosen * INTERNAL_STRIDE + 8);
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

    /// Insert or replace `key -> value`, returning the (possibly new) root.
    pub(crate) fn btree_insert(
        &mut self,
        root: u64,
        key: u64,
        value: &[u8],
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        if root == 0 {
            let mut buf = [0u8; MAX_BLOCK_SIZE];
            self.btree_init_node(&mut buf, 0, 1);
            let base = N_ENTRIES;
            wr_u64(&mut buf, base, key);
            buf[base + 8..base + spec.leaf_stride()].copy_from_slice(value);
            return self.cow_meta(0, &mut buf, BlockType::Btree, spec.owner, 0);
        }
        let outcome = self.btree_insert_rec(root, key, value, spec)?;
        match outcome.split {
            None => Ok(outcome.node_phys),
            Some((sep, right)) => {
                let mut left_buf = [0u8; MAX_BLOCK_SIZE];
                self.read_meta(outcome.node_phys, BlockType::Btree, &mut left_buf)?;
                let left_min = node_min_key(&left_buf);
                let mut buf = [0u8; MAX_BLOCK_SIZE];
                self.btree_init_node(&mut buf, node_level(&left_buf) + 1, 2);
                wr_u64(&mut buf, N_ENTRIES, left_min);
                wr_u64(&mut buf, N_ENTRIES + 8, outcome.node_phys);
                wr_u64(&mut buf, N_ENTRIES + INTERNAL_STRIDE, sep);
                wr_u64(&mut buf, N_ENTRIES + INTERNAL_STRIDE + 8, right);
                self.cow_meta(0, &mut buf, BlockType::Btree, spec.owner, 0)
            }
        }
    }

    fn btree_insert_rec(
        &mut self,
        phys: u64,
        key: u64,
        value: &[u8],
        spec: TreeSpec,
    ) -> Result<InsertOutcome, DriverError> {
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(phys, BlockType::Btree, &mut buf)?;
        let count = node_count(&buf);
        if node_level(&buf) == 0 {
            return self.btree_insert_leaf(phys, &mut buf, count, key, value, spec);
        }
        let ci = child_index(count, key, |i| {
            rd_u64(&buf, N_ENTRIES + i * INTERNAL_STRIDE)
        })?;
        let child = rd_u64(&buf, N_ENTRIES + ci * INTERNAL_STRIDE + 8);
        let child_outcome = self.btree_insert_rec(child, key, value, spec)?;
        // Refresh the child's pointer and separator (its min key may shift if
        // the key landed before the old minimum).
        let mut child_buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(child_outcome.node_phys, BlockType::Btree, &mut child_buf)?;
        wr_u64(
            &mut buf,
            N_ENTRIES + ci * INTERNAL_STRIDE,
            node_min_key(&child_buf),
        );
        wr_u64(
            &mut buf,
            N_ENTRIES + ci * INTERNAL_STRIDE + 8,
            child_outcome.node_phys,
        );
        match child_outcome.split {
            None => {
                let new_phys = self.cow_meta(phys, &mut buf, BlockType::Btree, spec.owner, 0)?;
                Ok(InsertOutcome {
                    node_phys: new_phys,
                    split: None,
                })
            }
            Some((sep, right)) => {
                self.btree_insert_internal_entry(phys, &mut buf, count, ci + 1, sep, right, spec)
            }
        }
    }

    /// Insert `(key, value)` into leaf `buf` (already loaded from `phys`),
    /// splitting when it overflows.
    fn btree_insert_leaf(
        &mut self,
        phys: u64,
        buf: &mut [u8],
        count: usize,
        key: u64,
        value: &[u8],
        spec: TreeSpec,
    ) -> Result<InsertOutcome, DriverError> {
        let stride = spec.leaf_stride();
        // Replace an existing key in place.
        for i in 0..count {
            let base = N_ENTRIES + i * stride;
            if rd_u64(buf, base) == key {
                buf[base + 8..base + stride].copy_from_slice(value);
                let new_phys = self.cow_meta(phys, buf, BlockType::Btree, spec.owner, 0)?;
                return Ok(InsertOutcome {
                    node_phys: new_phys,
                    split: None,
                });
            }
        }
        // Gather the sorted entries plus the new one.
        let mut entries: Vec<(u64, Vec<u8>)> = Vec::with_capacity(count + 1);
        let mut inserted = false;
        for i in 0..count {
            let base = N_ENTRIES + i * stride;
            let k = rd_u64(buf, base);
            if !inserted && key < k {
                entries.push((key, value.to_vec()));
                inserted = true;
            }
            entries.push((k, buf[base + 8..base + stride].to_vec()));
        }
        if !inserted {
            entries.push((key, value.to_vec()));
        }
        let cap = self.btree_leaf_cap(spec);
        if entries.len() <= cap {
            self.btree_write_leaf(phys, buf, &entries, spec)
                .map(|node_phys| InsertOutcome {
                    node_phys,
                    split: None,
                })
        } else {
            let mid = entries.len() / 2;
            let sep = entries[mid].0;
            let mut right_buf = [0u8; MAX_BLOCK_SIZE];
            let left_phys = self.btree_write_leaf(phys, buf, &entries[..mid], spec)?;
            let right_phys = self.btree_write_leaf(0, &mut right_buf, &entries[mid..], spec)?;
            Ok(InsertOutcome {
                node_phys: left_phys,
                split: Some((sep, right_phys)),
            })
        }
    }

    /// Write leaf `entries` into `buf` and copy-on-write it to `phys`.
    fn btree_write_leaf(
        &mut self,
        phys: u64,
        buf: &mut [u8],
        entries: &[(u64, Vec<u8>)],
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        let stride = spec.leaf_stride();
        self.btree_init_node(buf, 0, entries.len());
        for (i, (k, v)) in entries.iter().enumerate() {
            let base = N_ENTRIES + i * stride;
            wr_u64(buf, base, *k);
            buf[base + 8..base + stride].copy_from_slice(v);
        }
        self.cow_meta(phys, buf, BlockType::Btree, spec.owner, 0)
    }

    /// Insert separator entry `(sep, child)` at index `at` of internal node
    /// `buf` (loaded from `phys`), splitting when it overflows.
    #[allow(clippy::too_many_arguments)]
    fn btree_insert_internal_entry(
        &mut self,
        phys: u64,
        buf: &mut [u8],
        count: usize,
        at: usize,
        sep: u64,
        child: u64,
        spec: TreeSpec,
    ) -> Result<InsertOutcome, DriverError> {
        let level = node_level(buf);
        let mut entries: Vec<(u64, u64)> = Vec::with_capacity(count + 1);
        for i in 0..count {
            let base = N_ENTRIES + i * INTERNAL_STRIDE;
            entries.push((rd_u64(buf, base), rd_u64(buf, base + 8)));
        }
        let at = at.min(entries.len());
        entries.insert(at, (sep, child));
        let cap = self.btree_internal_cap();
        if entries.len() <= cap {
            self.btree_write_internal(phys, buf, level, &entries, spec)
                .map(|node_phys| InsertOutcome {
                    node_phys,
                    split: None,
                })
        } else {
            let mid = entries.len() / 2;
            let promoted = entries[mid].0;
            let mut right_buf = [0u8; MAX_BLOCK_SIZE];
            let left_phys = self.btree_write_internal(phys, buf, level, &entries[..mid], spec)?;
            let right_phys =
                self.btree_write_internal(0, &mut right_buf, level, &entries[mid..], spec)?;
            Ok(InsertOutcome {
                node_phys: left_phys,
                split: Some((promoted, right_phys)),
            })
        }
    }

    /// Write internal `entries` (at tree `level`) into `buf` and
    /// copy-on-write it to `phys`.
    fn btree_write_internal(
        &mut self,
        phys: u64,
        buf: &mut [u8],
        level: u32,
        entries: &[(u64, u64)],
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        self.btree_init_node(buf, level, entries.len());
        for (i, (k, c)) in entries.iter().enumerate() {
            let base = N_ENTRIES + i * INTERNAL_STRIDE;
            wr_u64(buf, base, *k);
            wr_u64(buf, base + 8, *c);
        }
        self.cow_meta(phys, buf, BlockType::Btree, spec.owner, 0)
    }
}

impl<B: Block> ARXFS<B> {
    /// Load every entry of node `phys` as `(key, value_bytes)`. For a leaf the
    /// value is the record; for an internal node it is the 8-byte child
    /// pointer. Returns the node level alongside the entries, so the borrow /
    /// merge logic is identical for both node kinds.
    fn btree_load_entries(
        &mut self,
        phys: u64,
        spec: TreeSpec,
    ) -> Result<(u32, NodeEntries), DriverError> {
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(phys, BlockType::Btree, &mut buf)?;
        let level = node_level(&buf);
        let count = node_count(&buf);
        let stride = if level == 0 {
            spec.leaf_stride()
        } else {
            INTERNAL_STRIDE
        };
        let val_len = stride - 8;
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let base = N_ENTRIES + i * stride;
            entries.push((
                rd_u64(&buf, base),
                buf[base + 8..base + 8 + val_len].to_vec(),
            ));
        }
        Ok((level, entries))
    }

    /// Store `entries` at tree `level` into a node, copy-on-writing `phys`.
    fn btree_store_entries(
        &mut self,
        phys: u64,
        level: u32,
        entries: &[(u64, Vec<u8>)],
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let stride = if level == 0 {
            spec.leaf_stride()
        } else {
            INTERNAL_STRIDE
        };
        self.btree_init_node(&mut buf, level, entries.len());
        for (i, (k, v)) in entries.iter().enumerate() {
            let base = N_ENTRIES + i * stride;
            wr_u64(&mut buf, base, *k);
            buf[base + 8..base + stride].copy_from_slice(v);
        }
        self.cow_meta(phys, &mut buf, BlockType::Btree, spec.owner, 0)
    }

    /// Remove `key` if present, returning the (possibly new, possibly `0`)
    /// root. A root left with a single child collapses one level.
    pub(crate) fn btree_remove(
        &mut self,
        root: u64,
        key: u64,
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        if root == 0 {
            return Ok(0);
        }
        let new_root = self.btree_remove_rec(root, key, spec)?;
        let (level, entries) = self.btree_load_entries(new_root, spec)?;
        if level == 0 && entries.is_empty() {
            self.free_meta(new_root);
            return Ok(0);
        }
        if level > 0 && entries.len() == 1 {
            let child = child_ptr(&entries[0].1);
            self.free_meta(new_root);
            return Ok(child);
        }
        Ok(new_root)
    }

    fn btree_remove_rec(
        &mut self,
        phys: u64,
        key: u64,
        spec: TreeSpec,
    ) -> Result<u64, DriverError> {
        let (level, mut entries) = self.btree_load_entries(phys, spec)?;
        if level == 0 {
            let before = entries.len();
            entries.retain(|(k, _)| *k != key);
            if entries.len() == before {
                return Ok(phys);
            }
            return self.btree_store_entries(phys, 0, &entries, spec);
        }
        let ci = child_index(entries.len(), key, |i| entries[i].0)?;
        let child = child_ptr(&entries[ci].1);
        let new_child = self.btree_remove_rec(child, key, spec)?;
        let (child_level, child_entries) = self.btree_load_entries(new_child, spec)?;
        entries[ci] = (
            child_entries.first().map_or(entries[ci].0, |(k, _)| *k),
            child_ptr_bytes(new_child),
        );
        let min = self.btree_min(child_level, spec);
        if child_entries.len() < min && entries.len() >= 2 {
            self.btree_rebalance(&mut entries, ci, child_level, child_entries, spec)?;
        }
        self.btree_store_entries(phys, level, &entries, spec)
    }

    /// Restore the minimum-occupancy invariant for child `ci` by borrowing
    /// from or merging with an adjacent sibling. `child_entries` are the
    /// freshly loaded entries of child `ci`.
    fn btree_rebalance(
        &mut self,
        entries: &mut NodeEntries,
        ci: usize,
        child_level: u32,
        child_entries: NodeEntries,
        spec: TreeSpec,
    ) -> Result<(), DriverError> {
        let min = self.btree_min(child_level, spec);
        let child_phys = child_ptr(&entries[ci].1);
        if ci > 0 {
            let si = ci - 1;
            let left_phys = child_ptr(&entries[si].1);
            let (_, mut left) = self.btree_load_entries(left_phys, spec)?;
            let mut child = child_entries;
            if left.len() > min {
                let moved = left.pop().ok_or(DriverError::DeviceFault)?;
                child.insert(0, moved);
                let new_left = self.btree_store_entries(left_phys, child_level, &left, spec)?;
                let new_child = self.btree_store_entries(child_phys, child_level, &child, spec)?;
                entries[si] = (left[0].0, child_ptr_bytes(new_left));
                entries[ci] = (child[0].0, child_ptr_bytes(new_child));
            } else {
                left.extend(child);
                let new_left = self.btree_store_entries(left_phys, child_level, &left, spec)?;
                self.free_meta(child_phys);
                entries[si] = (left[0].0, child_ptr_bytes(new_left));
                entries.remove(ci);
            }
        } else {
            let si = ci + 1;
            let right_phys = child_ptr(&entries[si].1);
            let (_, mut right) = self.btree_load_entries(right_phys, spec)?;
            let mut child = child_entries;
            if right.len() > min {
                let moved = right.remove(0);
                child.push(moved);
                let new_child = self.btree_store_entries(child_phys, child_level, &child, spec)?;
                let new_right = self.btree_store_entries(right_phys, child_level, &right, spec)?;
                entries[ci] = (child[0].0, child_ptr_bytes(new_child));
                entries[si] = (right[0].0, child_ptr_bytes(new_right));
            } else {
                child.extend(right);
                let new_child = self.btree_store_entries(child_phys, child_level, &child, spec)?;
                self.free_meta(right_phys);
                entries[ci] = (child[0].0, child_ptr_bytes(new_child));
                entries.remove(si);
            }
        }
        Ok(())
    }

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
            let first = (0..count).find(|i| rd_u64(&walk.buf, N_ENTRIES + i * stride) >= key);
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
            let last = rd_u64(&walk.buf, N_ENTRIES + (count - 1) * stride);
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
        let mut buf = Vec::new();
        buf.try_reserve_exact(block_size)
            .map_err(|_| DriverError::NoSpace)?;
        buf.resize(block_size, 0);
        Ok(Self {
            buf,
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

/// Decode an internal entry's 8-byte value as a child pointer.
fn child_ptr(value: &[u8]) -> u64 {
    rd_u64(value, 0)
}

/// Encode a child pointer as an internal entry's 8-byte value.
fn child_ptr_bytes(phys: u64) -> Vec<u8> {
    phys.to_le_bytes().to_vec()
}
