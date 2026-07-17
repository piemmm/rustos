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

use alloc::vec::Vec;

use rustos_abi::DriverError;

use crate::header::{BlockType, HEADER_LEN};
use crate::{rd_u64, wr_u64, Block, ARXFS, MAX_BLOCK_SIZE};

/// Node payload byte offsets, relative to the start of the block buffer
/// (the node payload begins right after the [`HEADER_LEN`] block header).
const N_COUNT: usize = HEADER_LEN;
const N_LEVEL: usize = HEADER_LEN + 4;
const N_ENTRIES: usize = HEADER_LEN + 8;

/// Internal-entry stride: separator key (`u64`) plus child pointer (`u64`).
const INTERNAL_STRIDE: usize = 16;

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
        let mut phys = root;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        loop {
            self.read_meta(phys, BlockType::Btree, &mut buf)?;
            let count = node_count(&buf);
            if node_level(&buf) == 0 {
                let stride = spec.leaf_stride();
                for i in 0..count {
                    let base = N_ENTRIES + i * stride;
                    if rd_u64(&buf, base) == key {
                        return Ok(Some(buf[base + 8..base + stride].to_vec()));
                    }
                }
                return Ok(None);
            }
            phys = Self::btree_child_for(&buf, count, key);
        }
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
        let mut phys = root;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        loop {
            self.read_meta(phys, BlockType::Btree, &mut buf)?;
            let count = node_count(&buf);
            if node_level(&buf) == 0 {
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
                return Ok(found);
            }
            phys = Self::btree_child_for(&buf, count, key);
        }
    }

    /// The child pointer of internal node `buf` that covers `key`: the last
    /// separator `<= key`, or the first child when `key` precedes them all.
    fn btree_child_for(buf: &[u8], count: usize, key: u64) -> u64 {
        let mut chosen = 0usize;
        for i in 0..count {
            let base = N_ENTRIES + i * INTERNAL_STRIDE;
            if rd_u64(buf, base) <= key {
                chosen = i;
            } else {
                break;
            }
        }
        rd_u64(buf, N_ENTRIES + chosen * INTERNAL_STRIDE + 8)
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
        // Choose the covering child and recurse.
        let mut ci = 0usize;
        for i in 0..count {
            if rd_u64(&buf, N_ENTRIES + i * INTERNAL_STRIDE) <= key {
                ci = i;
            } else {
                break;
            }
        }
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
        let mut ci = 0usize;
        for (i, (k, _)) in entries.iter().enumerate() {
            if *k <= key {
                ci = i;
            } else {
                break;
            }
        }
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

    /// Collect the physical address of every node in the tree (pre-order),
    /// for the mount-time free-space rebuild and for freeing a whole tree.
    pub(crate) fn btree_collect_nodes(
        &mut self,
        root: u64,
        spec: TreeSpec,
    ) -> Result<Vec<u64>, DriverError> {
        let mut out = Vec::new();
        if root != 0 {
            self.btree_collect_nodes_rec(root, spec, &mut out)?;
        }
        Ok(out)
    }

    fn btree_collect_nodes_rec(
        &mut self,
        phys: u64,
        spec: TreeSpec,
        out: &mut Vec<u64>,
    ) -> Result<(), DriverError> {
        out.push(phys);
        let (level, entries) = self.btree_load_entries(phys, spec)?;
        if level > 0 {
            for (_, v) in &entries {
                self.btree_collect_nodes_rec(child_ptr(v), spec, out)?;
            }
        }
        Ok(())
    }

    /// Parse the level and, for an internal node, the child pointers out of an
    /// already-verified node buffer `buf`. Used by the scrub verifying walk,
    /// which authenticates each node itself (counting any companion repair)
    /// before recursing, so it must not re-read the node through the
    /// repair-on-read [`crate::ARXFS::read_meta`] path. A leaf returns level
    /// `0` and no children.
    pub(crate) fn btree_node_children(&self, buf: &[u8]) -> (u32, Vec<u64>) {
        let level = node_level(buf);
        let count = node_count(buf);
        if level == 0 {
            return (0, Vec::new());
        }
        let cap = self.btree_internal_cap();
        let mut children = Vec::with_capacity(count.min(cap));
        for i in 0..count.min(cap) {
            let base = N_ENTRIES + i * INTERNAL_STRIDE;
            if base + INTERNAL_STRIDE > self.block_size {
                break;
            }
            children.push(rd_u64(buf, base + 8));
        }
        (level, children)
    }

    /// Collect every `(key, value)` leaf entry of the tree, in key order.
    pub(crate) fn btree_collect_entries(
        &mut self,
        root: u64,
        spec: TreeSpec,
    ) -> Result<NodeEntries, DriverError> {
        let mut out = Vec::new();
        if root != 0 {
            self.btree_collect_entries_rec(root, spec, &mut out)?;
        }
        Ok(out)
    }

    fn btree_collect_entries_rec(
        &mut self,
        phys: u64,
        spec: TreeSpec,
        out: &mut NodeEntries,
    ) -> Result<(), DriverError> {
        let (level, entries) = self.btree_load_entries(phys, spec)?;
        if level == 0 {
            out.extend(entries);
        } else {
            for (_, v) in &entries {
                self.btree_collect_entries_rec(child_ptr(v), spec, out)?;
            }
        }
        Ok(())
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
