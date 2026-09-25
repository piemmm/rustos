//! Custody of DMA memory a dead driver's device may still master
//! (`plans/OPEN-DEFECTS.md` D167, D225).
//!
//! With no IOMMU a device keeps whatever bus addresses it was handed, whatever
//! became of the driver that handed them over. A torn-down owner therefore
//! surrenders its DMA carves here instead of to the allocator, and they return
//! to the allocator only once the device is proven quiet: a later driver
//! instance for the same hardware-tree node has reset it and says so, or the
//! node was surprise-removed because its device is gone.
//!
//! Every block remembers the admission generation of the driver that carved
//! it, and a reset frees the blocks of every earlier generation, whether they
//! are held already or surrendered later — the space that carved them may be
//! dropped after its successor has started. Three kernel-enforced facts make
//! the proofs sound. A node has at most one live driver, so a reset by the
//! live one postdates every transfer an earlier one programmed. A node id is
//! never reissued within a boot, so a reset or a removal speaks only for the
//! device the memory was handed to. And the quarantine learns every removal
//! and opens a record only for a node the tree still holds, so no carve is
//! taken for a node the tree has dropped.
//!
//! A node's record lives while the node is in the tree, so a driver carving
//! and freeing one buffer at a time never rebuilds it; once the node has left
//! the tree the record goes as soon as nothing is held or reserved for it.
//! Each carve reserves room for its own surrender, so recording a block at
//! teardown never allocates.

use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_collections::HashMap;
use tairix_hash::BuildFastHash;
use tairix_kernel_mem::{AllocError, DmaBlock, DmaCustody, DmaError, FrameAllocator, PhysMap};
use tairix_sync::SpinLock;

use crate::devres::DmaQuarantineFacility;
use crate::hwtree::HwNodeLiveness;

/// One surrendered block and the generation of the driver that carved it.
#[derive(Clone, Copy)]
struct Held {
    generation: u64,
    block: DmaBlock,
}

/// Where a node stands in the hardware tree. Only ever advances.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    /// In the tree: carves are taken.
    Present,
    /// Removed in order while its device may still run: no carve is taken,
    /// and what is held waits for a reset.
    Detached,
    /// Surprise-removed: the device can master nothing, so everything is
    /// freeable and no carve is taken.
    Gone,
}

/// What the quarantine knows about one node.
struct NodeCustody {
    /// Surrendered blocks not yet freed. Its spare capacity always covers
    /// `reserved`, so a surrender never grows it.
    held: Vec<Held>,
    /// Blocks carved for the node and neither surrendered nor freed yet.
    reserved: usize,
    /// Blocks carved by a generation below this are freeable: a driver
    /// admitted as it reset the device. Generations start at one, so the
    /// initial zero frees nothing.
    reset_below: u64,
    standing: Standing,
}

impl NodeCustody {
    const fn new() -> Self {
        Self {
            held: Vec::new(),
            reserved: 0,
            reset_below: 0,
            standing: Standing::Present,
        }
    }

    fn frees(&self, generation: u64) -> bool {
        self.standing == Standing::Gone || generation < self.reset_below
    }

    /// Nothing can reach the record any more and no carve may reopen it.
    fn is_spent(&self) -> bool {
        self.standing != Standing::Present && self.reserved == 0 && self.held.is_empty()
    }
}

/// The kernel's DMA quarantine.
pub struct DmaQuarantine {
    frames: &'static FrameAllocator,
    physmap: &'static (dyn PhysMap + Sync),
    devices: &'static dyn HwNodeLiveness,
    nodes: SpinLock<HashMap<u32, NodeCustody, BuildFastHash>>,
}

impl DmaQuarantine {
    /// A quarantine returning blocks to `frames`, scrubbing them through the
    /// kernel direct map `physmap` first, and opening custody only for nodes
    /// `devices` reports live.
    #[must_use]
    pub fn new(
        frames: &'static FrameAllocator,
        physmap: &'static (dyn PhysMap + Sync),
        devices: &'static dyn HwNodeLiveness,
    ) -> Self {
        Self {
            frames,
            physmap,
            devices,
            nodes: SpinLock::new(HashMap::with_hasher(BuildFastHash::new())),
        }
    }

    /// Bytes held for `node` and not yet freed.
    #[cfg(test)]
    fn held_bytes(&self, node: u32) -> u64 {
        self.nodes.lock().get(&node).map_or(0, |custody| {
            custody
                .held
                .iter()
                .map(|held| held.block.len() as u64)
                .sum()
        })
    }

    /// Nodes the quarantine keeps a record for.
    #[cfg(test)]
    pub(crate) fn tracked_nodes(&self) -> usize {
        self.nodes.lock().len()
    }

    /// Scrub `block` and return it to the allocator, reporting whether it
    /// went back. A block the direct map cannot reach, or the allocator
    /// refuses, stays allocated: nothing unscrubbed is ever freed, and frames
    /// kept from reuse are as safe as held ones.
    fn free(&self, block: DmaBlock) -> bool {
        let len = block.len();
        let start = block.frame.start();
        let Some(ptr) = self.physmap.translate(start, len) else {
            return false;
        };
        // SAFETY: the direct map translated exactly `len` bytes of the block's
        // own frames, which no process maps and no device may still master —
        // the caller established that before choosing to free.
        unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0, len) };
        self.physmap.clean_invalidate(start, len);
        self.frames.free_order(block.frame, block.order).is_ok()
    }

    /// Apply `advance` to `node`'s record and free what it now covers,
    /// returning the bytes returned to the allocator.
    ///
    /// Each block leaves the record under the lock and is scrubbed outside it,
    /// so a large release never stalls another driver's carve or teardown.
    /// Freeable blocks are sought from the tail, where `hold` appends, so the
    /// search is constant per block when, as usual, all of them are freeable.
    fn quiet(&self, node: u32, advance: impl Fn(&mut NodeCustody)) -> u64 {
        let mut freed = 0;
        loop {
            let block = {
                let mut nodes = self.nodes.lock();
                let Some(custody) = nodes.get_mut(&node) else {
                    return freed;
                };
                advance(custody);
                let Some(index) = custody
                    .held
                    .iter()
                    .rposition(|held| custody.frees(held.generation))
                else {
                    if custody.is_spent() {
                        nodes.remove(&node);
                    }
                    return freed;
                };
                custody.held.swap_remove(index).block
            };
            if self.free(block) {
                freed += block.len() as u64;
            }
        }
    }

    /// Mark `node` as having left the tree with `standing`.
    fn remove(&self, node: u32, standing: Standing) -> u64 {
        self.quiet(node, |custody| {
            custody.standing = custody.standing.max(standing);
        })
    }
}

impl DmaCustody for DmaQuarantine {
    fn reserve(&self, node: u32) -> Result<(), DmaError> {
        let mut nodes = self.nodes.lock();
        if !nodes.contains_key(&node) {
            // A live node's record outlives its idle spells and a removal
            // marks the one it finds, so only a record's first carve consults
            // the tree — under the lock a removal's own update takes, so a
            // removal either precedes this check or finds the record it opens.
            if !self.devices.is_live(node) {
                return Err(DmaError::DeviceGone);
            }
            nodes
                .try_insert(node, NodeCustody::new())
                .map_err(|_| DmaError::Alloc(AllocError::OutOfMemory))?;
        }
        let Some(custody) = nodes.get_mut(&node) else {
            return Err(DmaError::Alloc(AllocError::OutOfMemory));
        };
        if custody.standing != Standing::Present {
            return Err(DmaError::DeviceGone);
        }
        custody
            .held
            .try_reserve(custody.reserved + 1)
            .map_err(|_| DmaError::Alloc(AllocError::OutOfMemory))?;
        custody.reserved += 1;
        Ok(())
    }

    fn unreserve(&self, node: u32) {
        let mut nodes = self.nodes.lock();
        let Some(custody) = nodes.get_mut(&node) else {
            return;
        };
        custody.reserved = custody.reserved.saturating_sub(1);
        if custody.is_spent() {
            nodes.remove(&node);
        }
    }

    fn hold(&self, node: u32, generation: u64, block: DmaBlock) {
        let freeable = {
            let mut nodes = self.nodes.lock();
            // A block no reservation stands behind is a broken invariant, and
            // one the reservation left no room for could only be recorded by
            // allocating: either way it stays allocated for good.
            let Some(custody) = nodes.get_mut(&node).filter(|custody| custody.reserved > 0) else {
                return;
            };
            custody.reserved -= 1;
            let freeable = custody.frees(generation);
            if !freeable && custody.held.len() < custody.held.capacity() {
                custody.held.push(Held { generation, block });
            }
            if custody.is_spent() {
                nodes.remove(&node);
            }
            freeable
        };
        if freeable {
            self.free(block);
        }
    }
}

impl DmaQuarantineFacility for DmaQuarantine {
    fn release(&self, node: u32, generation: u64) -> Result<u64, Errno> {
        Ok(self.quiet(node, |custody| {
            custody.reset_below = custody.reset_below.max(generation);
        }))
    }

    fn retire(&self, node: u32) -> Result<u64, Errno> {
        Ok(self.remove(node, Standing::Gone))
    }

    fn detach(&self, node: u32) {
        self.remove(node, Standing::Detached);
    }
}

#[cfg(test)]
mod tests;
