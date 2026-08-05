//! TRIM/discard: return freed space to the device, safely
//! (`docs/src/filesystem/arxfs-spec.md` §11, §15.10).
//!
//! Freed blocks enter a transient pending-discard queue (a field of
//! [`crate::ARXFS`]) as a committed transaction reclaims them
//! ([`crate::ARXFS::finish_txn`]). [`crate::ARXFS::trim`] later issues
//! the discards, leaning on the seams the earlier stages built rather
//! than inventing a second free-tracking mechanism:
//!
//! * **Safety.** A queued block is discarded **only** if it is still
//!   free at trim time. The mount-time free-space rebuild marks every
//!   block reachable from the committed root — including every reflink
//!   target and every deduped chunk at refcount >= 1 — as *used*, so a
//!   free block is, by construction, unreachable from every retained
//!   root, snapshot, reflink, deduped extent, and recovery root.
//!   A block that was freed and then reallocated is *used* again by
//!   trim time and is skipped. Discard therefore never destroys data
//!   reachable from any retained root.
//! * **Batched, aligned, rate-limited.** Still-free blocks are
//!   coalesced into contiguous runs, each run is aligned **inward** to
//!   the device's discard granularity, and at most [`TRIM_BATCH_RANGES`]
//!   runs are issued per [`crate::ARXFS::trim`] call; the remainder
//!   stays queued for the next call.
//! * **No zero-readback assumption.** `ARXFS` never reads a discarded
//!   block expecting zeroes; discarded blocks are free and are fully
//!   rewritten (header + integrity + crypto) before they are ever read
//!   again.
//! * **Recorded, not failed.** A device without discard support is
//!   recorded in the [`TrimReport`] and the queue is drained without
//!   error. There is no `nodiscard`/`trim=off` mode.
//!
//! The queue is rebuildable, transient state: a crash mid-trim
//! leaves a mountable volume and never loses live data.

use alloc::vec::Vec;

use tairix_abi::driver::block::Block;
use tairix_abi::{CapabilityId, CapabilityQuery, DriverError};
use tairix_log::{log, Event, EventId, Level, Sink};

use crate::scrub::{ARXFS_RANGE_END, ARXFS_RANGE_START};
use crate::{ARXFS, RING_BLOCKS};

/// A trim pass found nothing to discard (the queue held no still-free
/// block).
pub const TRIM_CLEAN: EventId = EventId(12_030);
/// A trim pass discarded one or more block ranges to the device.
pub const TRIM_DISCARDED: EventId = EventId(12_031);
/// A trim pass ran on a device without discard support; the queue was
/// drained and the outcome recorded, not failed.
pub const TRIM_UNSUPPORTED: EventId = EventId(12_032);
/// A trim pass was refused because the caller lacks `CAP_FS_MOUNT`.
pub const TRIM_DENIED: EventId = EventId(12_033);

/// Every discard event identifier falls inside the reserved `arxfs`
/// range so the stable IDs audit-log consumers rely on never collide
/// with another subsystem.
const _: () = {
    assert!(TRIM_CLEAN.0 >= ARXFS_RANGE_START && TRIM_CLEAN.0 < ARXFS_RANGE_END);
    assert!(TRIM_DISCARDED.0 >= ARXFS_RANGE_START && TRIM_DISCARDED.0 < ARXFS_RANGE_END);
    assert!(TRIM_UNSUPPORTED.0 >= ARXFS_RANGE_START && TRIM_UNSUPPORTED.0 < ARXFS_RANGE_END);
    assert!(TRIM_DENIED.0 >= ARXFS_RANGE_START && TRIM_DENIED.0 < ARXFS_RANGE_END);
};

/// Most coalesced block ranges one [`crate::ARXFS::trim`] call issues
/// before returning, leaving any remainder queued. Rate-limits a trim
/// pass so it cannot monopolise the device.
pub const TRIM_BATCH_RANGES: usize = 64;

/// The structured outcome of a [`crate::ARXFS::trim`] pass
/// (`docs/src/filesystem/arxfs-spec.md` §11).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct TrimReport {
    /// Whether the backing device supports discard. When `false` the
    /// queue was drained and nothing was issued (recorded, not failed).
    pub supported: bool,
    /// Number of contiguous, granularity-aligned ranges discarded.
    pub ranges_discarded: u64,
    /// Total number of logical blocks discarded.
    pub blocks_discarded: u64,
    /// Blocks dropped from the queue because they had been reallocated
    /// (no longer free) by trim time — skipped, never discarded.
    pub blocks_skipped_in_use: u64,
    /// Blocks left queued because the per-call batch limit
    /// ([`TRIM_BATCH_RANGES`]) was reached, or were trimmed off a run's
    /// unaligned edges. A later trim call drains them.
    pub blocks_deferred: u64,
}

impl TrimReport {
    /// Log the closing outcome of a trim pass through `sink` with a
    /// stable event ID.
    fn log_outcome(&self, sink: &dyn Sink) {
        let (level, id, message) = if !self.supported {
            (
                Level::Info,
                TRIM_UNSUPPORTED,
                "arxfs trim: device has no discard support; queue drained",
            )
        } else if self.blocks_discarded > 0 {
            (
                Level::Info,
                TRIM_DISCARDED,
                "arxfs trim discarded freed ranges",
            )
        } else {
            (Level::Info, TRIM_CLEAN, "arxfs trim: nothing to discard")
        };
        log(
            sink,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }
}

/// Round `value` up to the next multiple of `gran` (`gran >= 1`).
fn align_up(value: u64, gran: u64) -> u64 {
    value.div_ceil(gran).saturating_mul(gran)
}

/// Round `value` down to the previous multiple of `gran` (`gran >= 1`).
fn align_down(value: u64, gran: u64) -> u64 {
    (value / gran).saturating_mul(gran)
}

impl<B: Block> ARXFS<B> {
    /// Issue a TRIM/discard pass over the pending-discard queue, returning
    /// the freed-but-now-unreachable blocks to the backing device
    /// (`docs/src/filesystem/arxfs-spec.md` §11).
    ///
    /// Only blocks that are **still free** at call time are discarded, so a
    /// block that was freed and then reallocated is skipped, never discarded:
    /// discard can never destroy data reachable from a retained root,
    /// snapshot, reflink, or deduped extent. Still-free blocks are
    /// coalesced into contiguous runs, aligned inward to the device's discard
    /// granularity, and at most [`TRIM_BATCH_RANGES`] runs are issued before
    /// the call returns, leaving any remainder queued. A device without
    /// discard support is recorded in the returned [`TrimReport`] and the
    /// queue drained — recorded, not failed. The closing outcome is
    /// logged to `sink` with a stable event ID.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if `caps` does not grant
    ///   [`CapabilityId::FS_MOUNT`] (fail-closed).
    /// * [`DriverError::DeviceFault`] if the device reports a discard fault
    ///   (never a panic).
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::FS_MOUNT`].
    pub fn trim(
        &mut self,
        caps: &dyn CapabilityQuery,
        sink: &dyn Sink,
    ) -> Result<TrimReport, DriverError> {
        if !caps.holds(CapabilityId::FS_MOUNT) {
            log(
                sink,
                &Event {
                    level: Level::Warn,
                    id: TRIM_DENIED,
                    message: "arxfs trim denied: missing CAP_FS_MOUNT",
                    fields: &[],
                },
            );
            return Err(DriverError::PermissionDenied);
        }
        // Discard rewrites the device, so a read-only handle refuses before it
        // asks the device anything.
        self.deny_if_read_only()?;
        let cap = self.block.discard_capability()?;
        let mut report = TrimReport::default();
        if !cap.supported {
            // Recorded, not failed: drain the queue (the device cannot
            // reclaim) and report unsupported.
            self.allocator_mut()?.pending_discard.clear();
            report.supported = false;
            report.log_outcome(sink);
            return Ok(report);
        }
        report.supported = true;
        let gran = cap.granularity_blocks.max(1);

        // Keep only blocks that are still free; a reallocated block is now
        // reachable from the committed root and must not be discarded.
        let queued = core::mem::take(&mut self.allocator_mut()?.pending_discard);
        let mut free_blocks: Vec<u64> = Vec::with_capacity(queued.len());
        for block in queued {
            if block < RING_BLOCKS || block >= self.total_blocks {
                continue;
            }
            if self.bit_used(block)? {
                report.blocks_skipped_in_use += 1;
            } else {
                free_blocks.push(block);
            }
        }
        free_blocks.sort_unstable();
        free_blocks.dedup();

        // Coalesce contiguous runs of still-free blocks.
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for block in free_blocks {
            match ranges.last_mut() {
                Some(last) if last.0 + last.1 == block => last.1 += 1,
                _ => ranges.push((block, 1)),
            }
        }

        let mut issued = 0usize;
        for (start, len) in ranges {
            let end = start + len;
            if issued >= TRIM_BATCH_RANGES {
                // Rate limit reached: requeue the whole run for next pass.
                self.requeue_range(start, end, &mut report);
                continue;
            }
            let aligned_start = align_up(start, gran);
            let aligned_end = align_down(end, gran);
            if aligned_start >= aligned_end {
                // The run is shorter than one granularity window: nothing can
                // be aligned out of it; requeue for when a neighbour extends
                // it.
                self.requeue_range(start, end, &mut report);
                continue;
            }
            // Requeue the unaligned head and tail edges.
            self.requeue_range(start, aligned_start, &mut report);
            self.requeue_range(aligned_end, end, &mut report);

            let mut cursor = aligned_start;
            while cursor < aligned_end {
                let mut chunk = aligned_end - cursor;
                if cap.max_blocks_per_request != 0 {
                    chunk = chunk.min(align_down(cap.max_blocks_per_request, gran).max(gran));
                }
                self.block.discard(cursor, chunk)?;
                report.ranges_discarded += 1;
                report.blocks_discarded += chunk;
                cursor += chunk;
            }
            issued += 1;
        }
        report.log_outcome(sink);
        Ok(report)
    }

    /// Requeue every block in `[start, end)` for a later trim pass. Used for
    /// run edges trimmed off by granularity alignment and for runs deferred
    /// by the per-call batch limit.
    fn requeue_range(&mut self, start: u64, end: u64, report: &mut TrimReport) {
        for block in start..end {
            self.enqueue_discard(block);
            report.blocks_deferred += 1;
        }
    }

    /// mkfs-time full-range discard (`docs/src/filesystem/arxfs-spec.md`
    /// §11 mkfs flow): tell a discard-capable device the whole volume is free
    /// before the encrypted structures are laid down. A device without discard
    /// support is recorded (the returned `bool` is `false`), never failed.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the device reports a discard fault.
    pub(crate) fn mkfs_discard(&mut self) -> Result<bool, DriverError> {
        let cap = self.block.discard_capability()?;
        if !cap.supported {
            return Ok(false);
        }
        let gran = cap.granularity_blocks.max(1);
        let end = align_down(self.total_blocks, gran);
        let mut cursor = 0u64;
        while cursor < end {
            let mut chunk = end - cursor;
            if cap.max_blocks_per_request != 0 {
                chunk = chunk.min(align_down(cap.max_blocks_per_request, gran).max(gran));
            }
            self.block.discard(cursor, chunk)?;
            cursor += chunk;
        }
        Ok(true)
    }

    /// Number of blocks currently queued for discard. Test/inspection aid.
    #[cfg(test)]
    pub(crate) fn pending_discard_count(&self) -> usize {
        self.allocator()
            .map_or(0, |alloc| alloc.pending_discard.len())
    }
}
