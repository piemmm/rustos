//! The write-back bound one writable ARXFS volume is mounted under
//! (`plans/ARXFS-WRITEBACK.md` §6).
//!
//! A copy-on-write filesystem holds its transaction's sealed blocks in RAM
//! until the commit sends them, so the commit costs one device write per
//! block rather than one per rewrite. Those blocks are **pinned**: they
//! exist nowhere else, so they can only be written out, never dropped —
//! which makes them the opposite of the reclaimable caches beside them
//! ([`crate::transform_cache`], [`crate::block_cache`]) and means nothing
//! may shrink the set behind the driver's back.
//!
//! What bounds it instead is assembled here, because all four parts are
//! the host's to know: the RAM the boot path discovered, the one system
//! pressure gauge every other cache samples, the memory-statistics
//! registry a pinned pool has to be visible through, and the one
//! machine-wide total every mounted volume's set draws a share of. The
//! driver holds the *policy* (how the ceiling falls with the band, how a
//! volume's share of the total is taken, how the dirty-age window shortens
//! with it) beside the window policy it already owned, so there is one
//! definition of "how much may this volume defer" rather than two that
//! can disagree across the driver boundary.
//!
//! Read-only mounts get none: a read-only handle refuses to stage a block
//! at all, so its set is provably empty and there is nothing to bound or
//! report.

use alloc::sync::Arc;

use tairix_abi::driver::block::Block;
use tairix_abi::DriverError;
use tairix_drv_fs_arxfs::ARXFS;
use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};
use tairix_reclaim::{MemoryPressure, PinnedAccounting, PinnedLedger, PinnedShare, ReclaimOwner};

/// The label the volume's pinned write-back row carries in the
/// System Information cache-ledger export.
const PINNED_LABEL: &str = "arxfs.writeback";

/// The one machine-wide total every mounted volume's dirty set draws on.
///
/// A ceiling derived per volume would let a machine's volumes pin a multiple
/// of what the machine has, and a dirty block is the one kind of memory
/// nothing can reclaim — so the volumes share this total and each takes a
/// share of it. One instance, for the life of the kernel.
static WRITEBACK_SHARE: PinnedShare = PinnedShare::new();

/// Audit event: a writable volume was mounted without a write-back bound
/// because the machine's discovered RAM cannot spare one device transfer
/// per volume. The mount is refused; the `volume` field names the handle.
const WRITEBACK_BOUND_REFUSED: EventId = EventId(4146);

/// Bound `fs`'s write-back cache to this machine, and register the volume's
/// pinned-memory row so its unwritten bytes are visible through the System
/// Information API.
///
/// `volume` is the stable per-boot mount handle the volume's other ledgers
/// are charged to, so an operator reading the cache rows sees one volume's
/// clean caches and its pinned write-back side by side.
///
/// # Errors
///
/// [`DriverError::NoSpace`] when the ceiling derived from discovered RAM
/// cannot hold one coalesced device transfer. Refusing here is deliberate:
/// a machine that cannot spare that much would otherwise mount and then
/// commit after every record, which is a wedge dressed as success.
pub fn bound_volume<B: Block>(
    fs: ARXFS<B>,
    volume: u64,
    pressure: &'static MemoryPressure,
    sink: &'static (dyn Sink + Sync),
) -> Result<ARXFS<B>, DriverError> {
    let pinned = Arc::new(PinnedAccounting::within(&WRITEBACK_SHARE));
    // Budget from discovered physical RAM, exactly as the volume's clean
    // caches are (the growable kernel heap's bootstrap size is no longer the
    // memory to size anything against); falls back to the bootstrap size
    // before RAM is published.
    let bounded = fs.with_writeback_bound(
        tairix_kernel_core::memstats::cache_backing_bytes(),
        pressure,
        Arc::clone(&pinned),
    );
    match bounded {
        Ok(fs) => {
            tairix_kernel_core::memstats::MEM_STATS.register_pinned_ledger(PinnedLedger::new(
                PINNED_LABEL,
                ReclaimOwner::FilesystemVolume { volume },
                pinned,
            ));
            Ok(fs)
        }
        Err(err) => {
            log(
                sink,
                &Event {
                    level: Level::Error,
                    id: WRITEBACK_BOUND_REFUSED,
                    message: "writeback-bound: discovered RAM cannot bound this volume's \
                              write-back cache",
                    fields: &[Field {
                        key: "volume",
                        value: FieldValue::UnsignedInt(volume),
                    }],
                },
            );
            Err(err)
        }
    }
}
