//! Production [`DmaBank`] over the host's DMA allocation seam.
//!
//! [`SlabBank`] backs the enumeration engine's growable device-shared
//! memory with real, owned [`DmaSlab`] allocations: every
//! [`DmaBank::grow`] mints a fresh zeroed slab through the bus-neutral
//! [`DmaHost`] seam (the kernel re-checks the caller's DMA capability at
//! each allocation) and every [`DmaBank::release`] drops the slab, whose
//! drop shim returns the frames to the host. The engine therefore pays
//! for exactly the devices it is serving: attaching a device allocates
//! its region, detaching it frees the memory — the number of devices one
//! controller serves is bounded by its silicon and genuine memory
//! exhaustion, never a compile-time budget.
//!
//! Chunk base offsets in the bank's virtual offset space are monotonic
//! and never reused, so an offset kept past its chunk's release maps to
//! no chunk and every access through it fails closed rather than
//! aliasing a later allocation.

use alloc::vec::Vec;

use rustos_abi::driver::dma::{DmaHost, DmaSlab};
use rustos_abi::DriverError;

use crate::device::DmaBank;

/// Alignment of each chunk's base offset in the bank's virtual offset
/// space. Keeping chunk bases page-multiple-aligned means in-chunk layout
/// arithmetic (64-byte packing, page-aligned scratchpad pages) holds in
/// the virtual space exactly as it does chunk-relative.
const CHUNK_ALIGN: usize = 4096;

/// One live chunk: its virtual base offset and the owned slab backing it.
struct Chunk {
    base: usize,
    slab: DmaSlab,
}

/// A growable [`DmaBank`] whose chunks are owned [`DmaSlab`]s minted from
/// the host's [`DmaHost`] seam, optionally bounded by the controller's
/// inbound DMA aperture.
pub struct SlabBank<'h> {
    host: &'h dyn DmaHost,
    /// Exclusive device-visible upper bound every chunk must lie wholly
    /// below — the inbound DMA aperture the controller can reach. `None`
    /// when the platform imposes none.
    aperture_top: Option<u64>,
    /// Live chunks, ascending by virtual base (grows monotonically and
    /// removal preserves order), so lookup is a binary search.
    chunks: Vec<Chunk>,
    /// The next chunk's virtual base offset. Monotonic — released bases
    /// are never reused, so stale offsets fail closed.
    next_base: usize,
}

impl<'h> SlabBank<'h> {
    /// A bank allocating from `host` with no aperture bound.
    #[must_use]
    pub fn new(host: &'h dyn DmaHost) -> Self {
        Self {
            host,
            aperture_top: None,
            chunks: Vec::new(),
            next_base: 0,
        }
    }

    /// A bank allocating from `host` whose every chunk must lie wholly
    /// below the device-visible `aperture_top` (exclusive) — a chunk the
    /// controller could not reach is refused at allocation time, fail
    /// closed, never silently truncated.
    #[must_use]
    pub fn with_aperture(host: &'h dyn DmaHost, aperture_top: u64) -> Self {
        Self {
            host,
            aperture_top: Some(aperture_top),
            chunks: Vec::new(),
            next_base: 0,
        }
    }

    /// Index of the live chunk containing virtual offset `offset`.
    fn chunk_index(&self, offset: usize) -> Result<usize, DriverError> {
        let candidate = match self.chunks.binary_search_by(|c| c.base.cmp(&offset)) {
            Ok(index) => index,
            Err(0) => return Err(DriverError::OutOfRange),
            Err(insertion) => insertion - 1,
        };
        let chunk = &self.chunks[candidate];
        if offset < chunk.base + chunk.slab.len() {
            Ok(candidate)
        } else {
            Err(DriverError::OutOfRange)
        }
    }

    /// The chunk containing `[offset, offset + len)` wholly, with the
    /// in-slab offset of `offset` — the shared bounds check of `read` and
    /// `write`.
    fn locate(&mut self, offset: usize, len: usize) -> Result<(usize, usize), DriverError> {
        let index = self.chunk_index(offset)?;
        let chunk = &self.chunks[index];
        let inner = offset - chunk.base;
        let end = inner.checked_add(len).ok_or(DriverError::OutOfRange)?;
        if end > chunk.slab.len() {
            return Err(DriverError::OutOfRange);
        }
        Ok((index, inner))
    }
}

impl DmaBank for SlabBank<'_> {
    fn grow(&mut self, len: usize) -> Result<usize, DriverError> {
        if len == 0 {
            return Err(DriverError::OutOfRange);
        }
        // Reserve the bookkeeping slot first so a minted slab is never
        // stranded by a failed push (deterministic OOM either way).
        self.chunks
            .try_reserve(1)
            .map_err(|_| DriverError::LengthOutOfRange)?;
        let slab = self.host.alloc_dma_zeroed(len)?;
        // The xHCI structures require 64-byte alignment at minimum; the
        // hosts mint page-aligned slabs, so this only refuses a broken
        // allocator rather than a legitimate grant.
        if slab.phys() % 64 != 0 {
            return Err(DriverError::OutOfRange);
        }
        if let Some(top) = self.aperture_top {
            let end = slab
                .phys()
                .checked_add(len as u64)
                .ok_or(DriverError::OutOfRange)?;
            if end > top {
                // Dropping the slab returns it to the host; the refusal is
                // fail-closed, never a silently unreachable chunk.
                return Err(DriverError::OutOfRange);
            }
        }
        let base = self.next_base;
        self.next_base = base
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?
            .next_multiple_of(CHUNK_ALIGN);
        self.chunks.push(Chunk { base, slab });
        Ok(base)
    }

    fn release(&mut self, base: usize) -> Result<(), DriverError> {
        let index = self
            .chunks
            .iter()
            .position(|chunk| chunk.base == base)
            .ok_or(DriverError::NotFound)?;
        // Dropping the removed chunk's slab hands the frames back to the
        // host's pool (its drop shim issues the free).
        self.chunks.remove(index);
        Ok(())
    }

    fn phys_of(&self, offset: usize) -> Result<u64, DriverError> {
        let index = self.chunk_index(offset)?;
        let chunk = &self.chunks[index];
        Ok(chunk.slab.phys() + (offset - chunk.base) as u64)
    }

    fn read(&mut self, offset: usize, buf: &mut [u8]) -> Result<(), DriverError> {
        let (index, inner) = self.locate(offset, buf.len())?;
        let slab = &mut self.chunks[index].slab;
        // Invalidate the CPU's view of this range first, so a non-coherent
        // DMA master's writes (e.g. an event TRB the controller posted)
        // are read from memory rather than a stale cache line. A no-op on
        // a coherent interconnect / the mock host.
        slab.sync_range(inner, buf.len());
        buf.copy_from_slice(&slab.as_bytes()[inner..inner + buf.len()]);
        Ok(())
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DriverError> {
        let (index, inner) = self.locate(offset, bytes.len())?;
        let slab = &mut self.chunks[index].slab;
        slab.as_bytes_mut()[inner..inner + bytes.len()].copy_from_slice(bytes);
        // Clean this range to memory, so a non-coherent DMA master reads
        // the freshly published bytes (e.g. a command TRB) once the
        // doorbell is rung rather than stale memory. A no-op on a coherent
        // interconnect / the mock host.
        slab.sync_range(inner, bytes.len());
        Ok(())
    }
}
