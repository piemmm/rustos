//! Custody of DMA memory a dead driver's device may still master
//! (`plans/OPEN-DEFECTS.md` D167).
//!
//! With no IOMMU a device keeps whatever bus addresses it was handed, whatever
//! became of the driver that handed them over. A torn-down space therefore
//! surrenders its DMA carves here instead of to the allocator, and they return
//! to the allocator only once the device is proven quiet: a later driver
//! instance for the same hardware-tree node has reset it and says so, or the
//! node is retired because its device is gone.
//!
//! Every block remembers the admission generation of the driver that carved
//! it, and quieting a node frees the blocks below a generation bound, whether
//! they are held already or surrendered later — the space that carved them may
//! be dropped after its successor has started. The bound is sound because a
//! successor is admitted only once its predecessor's last thread is down, so a
//! reset by any later instance postdates every transfer an earlier one could
//! have programmed. Retiring a node bounds at the generation high-water mark
//! rather than freeing everything for good, because a node id is reused once
//! the node holding the highest one is removed, and a driver for the new
//! device is always admitted above the mark.

use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_collections::HashMap;
use tairix_hash::BuildFastHash;
use tairix_kernel_mem::{AllocError, DmaBlock, DmaCustody, DmaError, FrameAllocator, PhysMap};
use tairix_sync::SpinLock;

use crate::devres::DmaQuarantineFacility;

/// One surrendered block and the generation of the driver that carved it.
#[derive(Clone, Copy)]
struct Held {
    generation: u64,
    block: DmaBlock,
}

/// What the quarantine knows about one node.
#[derive(Default)]
struct NodeCustody {
    /// Surrendered blocks not yet freed.
    held: Vec<Held>,
    /// Spaces bound to the node that have not yet surrendered.
    bound: usize,
    /// Blocks carved by a generation below this are freeable; `None` until
    /// the node is first quieted.
    quiet_below: Option<u64>,
}

impl NodeCustody {
    fn freeable(&self, generation: u64) -> bool {
        self.quiet_below.is_some_and(|bound| generation < bound)
    }

    /// No space can surrender into the record any more and it holds nothing,
    /// so it can go: a later binding starts a fresh one.
    fn is_idle(&self) -> bool {
        self.bound == 0 && self.held.is_empty()
    }
}

/// The kernel's DMA quarantine.
pub struct DmaQuarantine {
    frames: &'static FrameAllocator,
    physmap: &'static (dyn PhysMap + Sync),
    nodes: SpinLock<HashMap<u32, NodeCustody, BuildFastHash>>,
}

impl DmaQuarantine {
    /// A quarantine returning blocks to `frames`, scrubbing them through the
    /// kernel direct map `physmap` first.
    #[must_use]
    pub fn new(frames: &'static FrameAllocator, physmap: &'static (dyn PhysMap + Sync)) -> Self {
        Self {
            frames,
            physmap,
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
    fn tracked_nodes(&self) -> usize {
        self.nodes.lock().len()
    }

    /// Scrub `block` and return it to the allocator; a block the direct map
    /// cannot reach stays allocated, since nothing unscrubbed is ever freed.
    fn free(&self, block: DmaBlock) {
        let len = block.len();
        let start = block.frame.start();
        let Some(ptr) = self.physmap.translate(start, len) else {
            return;
        };
        // SAFETY: the direct map translated exactly `len` bytes of the block's
        // own frames, which no process maps and no device may still master —
        // the caller established that before choosing to free.
        unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0, len) };
        self.physmap.clean_invalidate(start, len);
        // A refused free leaves the frames allocated, which keeps them from
        // reuse just as holding them would.
        let _ = self.frames.free_order(block.frame, block.order);
    }

    /// Raise `node`'s quiet bound to at least `below` and free what it now
    /// covers, returning the bytes freed.
    ///
    /// Each block leaves the record under the lock and is scrubbed outside it,
    /// so a large release never stalls another driver's carve or teardown.
    /// Freeable blocks are sought from the tail, where `hold` appends, so the
    /// search is constant per block when, as usual, all of them are freeable.
    fn quiet(&self, node: u32, below: u64) -> u64 {
        let mut freed = 0;
        loop {
            let block = {
                let mut nodes = self.nodes.lock();
                let Some(custody) = nodes.get_mut(&node) else {
                    return freed;
                };
                let bound = custody.quiet_below.map_or(below, |q| q.max(below));
                custody.quiet_below = Some(bound);
                let Some(index) = custody
                    .held
                    .iter()
                    .rposition(|held| held.generation < bound)
                else {
                    if custody.is_idle() {
                        nodes.remove(&node);
                    }
                    return freed;
                };
                custody.held.swap_remove(index).block
            };
            freed += block.len() as u64;
            self.free(block);
        }
    }
}

/// What becomes of a block surrendered to a node.
enum Arrival {
    /// Recorded until the node is quieted.
    Held,
    /// Already covered by the node's quiet bound.
    Freeable,
    /// Could not be recorded: its frames stay allocated for good.
    Unrecorded,
}

impl DmaCustody for DmaQuarantine {
    fn bind(&self, node: u32) -> Result<(), DmaError> {
        let mut nodes = self.nodes.lock();
        if let Some(custody) = nodes.get_mut(&node) {
            custody.bound += 1;
            return Ok(());
        }
        nodes
            .try_insert(
                node,
                NodeCustody {
                    bound: 1,
                    ..NodeCustody::default()
                },
            )
            .map(|_| ())
            .map_err(|_| DmaError::Alloc(AllocError::OutOfMemory))
    }

    fn hold(&self, node: u32, generation: u64, block: DmaBlock) {
        let arrival = {
            let mut nodes = self.nodes.lock();
            // A space surrenders only to a node it bound, so a missing record
            // is a broken invariant: keep the block from reuse regardless.
            match nodes.get_mut(&node) {
                None => Arrival::Unrecorded,
                Some(custody) if custody.freeable(generation) => Arrival::Freeable,
                Some(custody) => {
                    if custody.held.try_reserve(1).is_ok() {
                        custody.held.push(Held { generation, block });
                        Arrival::Held
                    } else {
                        Arrival::Unrecorded
                    }
                }
            }
        };
        if matches!(arrival, Arrival::Freeable) {
            self.free(block);
        }
    }

    fn unbind(&self, node: u32) {
        let mut nodes = self.nodes.lock();
        let Some(custody) = nodes.get_mut(&node) else {
            return;
        };
        custody.bound = custody.bound.saturating_sub(1);
        if custody.is_idle() {
            nodes.remove(&node);
        }
    }
}

impl DmaQuarantineFacility for DmaQuarantine {
    fn release(&self, node: u32, generation: u64) -> Result<u64, Errno> {
        Ok(self.quiet(node, generation))
    }

    fn retire(&self, node: u32, through_generation: u64) -> Result<u64, Errno> {
        Ok(self.quiet(node, through_generation.saturating_add(1)))
    }
}

#[cfg(test)]
mod tests;
