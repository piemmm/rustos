//! The kernel block-device sharing layer (Design D, D2a —
//! `.junie/next-pi-prompt.md`): one whole-disk [`Block`] device driven
//! concurrently through serialised handles.
//!
//! The boot path brings up exactly **one** bootstrap-floor block device
//! (`crate::root_storage` / `crate::aarch64::root_unlock`), yet two
//! independent consumers need to read it: the read-only signed-bundle
//! `/System` driver store (`crate::root_mount::autoload_system_drivers`)
//! and the encrypted-root unlock window
//! (`crate::root_mount::unlock_root_disk_interactively`). Until now they
//! used the one device *sequentially* — the `/System` window was borrowed,
//! dropped, then the device was moved by value into the unlock — so the
//! `/System` mount could not outlive the unlock call.
//!
//! Design D needs the `/System` store reachable for the life of the system
//! (on-demand and reactive driver loads), so the
//! one device must back **two concurrent partition windows**. This module
//! is that primitive: a [`SharedBlock`] owns the device behind a
//! scheduler-blocking [`SleepLock`] and hands out as many
//! [`SharedBlockHandle`]s as there are windows, each of which is itself a
//! [`Block`]. Every read / write / discard takes the lock for the duration
//! of the single device operation, so concurrent windows on different CPUs
//! are serialised (SMP from day one, explicit synchronisation).
//!
//! The device's [`BlockGeometry`] is immutable for the life of a disk, so
//! it is queried **once** at construction and cached: [`SharedBlock::geometry`]
//! and the handle's [`Block::geometry`] are then lock-free and infallible,
//! keeping that hot, frequently-read value off the lock. Geometry is the only cached value; every byte-moving operation
//! goes to the device under the lock.
//!
//! A **sleeping** [`SleepLock`] is required here, not a `lib/sync` spin lock:
//! a device operation may **park** the calling task across the controller's
//! completion interrupt ([`Block::read_blocks`] parks on the device IRQ), and
//! the lock is held for the duration of that operation. A spin lock held
//! across such a park is a defect — a second window contending for the same
//! disk would busy-spin on a holder that is asleep (forbidden busy-waiting),
//! or deadlock a single CPU outright. The [`SleepLock`] instead parks the
//! contending window off the run queue and wakes it when the holder releases,
//! so two concurrent windows (the `/System` driver-store window and the
//! encrypted-root unlock window) can safely drive one disk
//! (`docs/src/architecture/sync.md`).
//!
//! Architecture-neutral: the layer names no
//! device type and no architecture — it is generic over any [`Block`], so
//! every port shares this one definition and the per-device bring-up
//! (virtio-blk, EMMC2, …) wraps its brought-up device in it.

use alloc::sync::Arc;
use core::ops::Deref;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, DiscardCapability};
use tairix_abi::driver::BufferClass;
use tairix_abi::DriverError;
use tairix_kernel_core::{CooperativeYield, SleepLock};

/// A [`Block`] device shared behind a lock so several concurrent windows
/// can drive it, each device operation serialised.
///
/// Construct one with [`SharedBlock::new`] (which queries and caches the
/// device geometry once), then call [`SharedBlock::handle`] for each
/// independent consumer. Each returned [`SharedBlockHandle`] borrows the
/// `SharedBlock`, is itself a [`Block`], and may be layered under a
/// `tairix_partition::PartitionBlock` window exactly like a bare device.
///
/// `SharedBlock<B>` is [`Sync`] when `B: Send` (the [`SleepLock`] makes the
/// interior access exclusive), so it may be shared by `&` across CPUs.
pub struct SharedBlock<B: Block> {
    /// The owned device. Every byte-moving operation locks this for the
    /// duration of one device call. A sleeping lock because that operation
    /// may be held across a completion-IRQ park.
    device: SleepLock<B>,
    /// The device geometry, queried once at construction. Immutable for the
    /// life of a disk, so it is served lock-free.
    geometry: BlockGeometry,
    /// The device's declared class, read once at construction. A pure
    /// property of the hardware, so it is served lock-free like the
    /// geometry, and forwarded rather than defaulted so a consumer above
    /// this sharing boundary still derives the real device's I/O budget.
    class: BlkDeviceClass,
}

impl<B: Block> SharedBlock<B> {
    /// Wrap `device` for shared access, caching its geometry.
    ///
    /// # Errors
    ///
    /// Propagates [`Block::geometry`]'s error if the device geometry could
    /// not be queried — the device is never wrapped on a geometry fault, so
    /// no handle can be handed out for an unusable device (fail closed).
    pub fn new(device: B) -> Result<Self, DriverError> {
        let geometry = device.geometry()?;
        let class = device.device_class();
        Ok(Self {
            device: SleepLock::new(device),
            geometry,
            class,
        })
    }

    /// The cached device geometry (lock-free; see the module docs).
    #[must_use]
    pub fn geometry(&self) -> BlockGeometry {
        self.geometry
    }

    /// The device's declared class (lock-free; see the module docs).
    #[must_use]
    pub fn device_class(&self) -> BlkDeviceClass {
        self.class
    }

    /// A new independent window onto the shared device, borrowing it. Each
    /// handle is a [`Block`]; the underlying device serialises their
    /// operations.
    #[must_use]
    pub fn handle(&self) -> BorrowedBlockWindow<'_, B> {
        SharedBlockHandle { shared: self }
    }

    /// A new independent window that **owns** a counted reference to the
    /// shared device, so it may outlive the scope that opened it.
    ///
    /// This is the window a runtime mount takes: the mounted filesystem
    /// driver is stored in the mount registry and long outlives the attach
    /// call, and several mounts of one disk's partitions must reach the same
    /// device — the shared client and its single data window — rather than
    /// each opening a private one. The last window dropped releases the
    /// device.
    #[must_use]
    pub fn owned_handle(self: &Arc<Self>) -> OwnedBlockWindow<B> {
        SharedBlockHandle {
            shared: Arc::clone(self),
        }
    }
}

// SAFETY: `SharedBlock` is the kernel's disk-sharing boundary, and it is the
// single place that vouches for sharing a brought-up block device across the
// tasks that drive it (disk access is a common,
// capability-checked kernel service, reached by the disk-owning kthread, the
// driver-store serve kthread, and the encrypted-root unlock kthread over one
// `&'static SharedBlock`). The auto-derived `Send`/`Sync` are conservatively
// withheld because a concrete `B` (e.g. `VirtioBlk`) holds raw `NonNull`
// pointers into the device's MMIO register window and DMA region. Asserting
// `Send + Sync` here is sound because:
//   1. **Exclusive access.** Every byte-moving operation on the contained
//      device goes through `self.device.lock()` (the cached `geometry` and
//      `class` are the only lock-free fields, and both are immutable `Copy`
//      values), so `B` is never touched by two tasks at once — there is no
//      data race on `B`'s interior, including any non-atomic bookkeeping it
//      keeps. The `SleepLock` grants exclusive ownership for the whole
//      operation, even one held across a completion-IRQ park, parking a
//      second contender rather than spinning.
//   2. **Location-independent backing.** `B`'s `!Send` parts are raw pointers
//      into globally-valid device memory: an MMIO register window and a DMA
//      slab, both reachable from any CPU/task through the kernel's identity
//      map. They are not tied to the task that mapped them, so moving the
//      handle's *reference* between tasks observes the same device bytes.
//   3. **Lifetime.** A `&'static SharedBlock` only ever wraps a device whose
//      backing was boot-leaked to `'static` (`kernel/tairix-kernel`'s
//      `root_unlock`), so the device and its pointers outlive every handle.
// This is the irreducible `unsafe` for in-kernel device sharing; it is
// confined to this one boundary type rather than scattered across the virtio
// transport/host (encapsulated behind a safe API).
unsafe impl<B: Block> Send for SharedBlock<B> {}
// SAFETY: as for `Send` above — `&SharedBlock` hands out `SharedBlockHandle`s
// whose every device op locks `self.device` (a sleeping lock that parks rather
// than spins on contention), so concurrent `&` access from multiple tasks is
// serialised down to one device operation at a time.
unsafe impl<B: Block> Sync for SharedBlock<B> {}

/// One window onto a [`SharedBlock`], reached through `R`. It is itself a
/// [`Block`]: every operation locks the shared device for the duration of the
/// single device call, so concurrent handles never interleave a device
/// operation.
///
/// Handles are independent and several may be open at once — e.g. the
/// read-only `/System` driver-store window and the encrypted-root unlock
/// window over the one boot disk, or one window per mounted partition of a
/// runtime-attached disk.
///
/// `R` is how the window reaches the shared device, and is the only
/// difference between the two flavours: [`BorrowedBlockWindow`] holds a plain
/// `&SharedBlock` for a window that lives inside the scope that opened it,
/// [`OwnedBlockWindow`] holds an `Arc` for one that must outlive it. Both go
/// through this single [`Block`] implementation, so the two flavours cannot
/// drift apart in what they serialise.
pub struct SharedBlockHandle<R> {
    shared: R,
}

/// A [`SharedBlockHandle`] that borrows the shared device for the scope that
/// opened it.
pub type BorrowedBlockWindow<'a, B> = SharedBlockHandle<&'a SharedBlock<B>>;

/// A [`SharedBlockHandle`] that holds a counted reference to the shared
/// device, so the window may outlive the scope that opened it.
pub type OwnedBlockWindow<B> = SharedBlockHandle<Arc<SharedBlock<B>>>;

impl<B: Block, R: Deref<Target = SharedBlock<B>>> Block for SharedBlockHandle<R> {
    /// The shared device's own class, so a consumer reached through this
    /// window serves the real hardware's I/O budget rather than the
    /// unclassified default. Served from the cache, like the geometry.
    fn device_class(&self) -> BlkDeviceClass {
        self.shared.class
    }

    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        // Served from the cache: immutable for the life of the disk, so no
        // lock and no device round-trip.
        Ok(self.shared.geometry)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.shared.device.lock().read_blocks(lba, buf)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.shared.device.lock().write_blocks(lba, buf)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.shared.device.lock().flush()
    }

    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.shared
            .device
            .lock()
            .read_blocks_with_class(lba, buf, class)
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.shared
            .device
            .lock()
            .write_blocks_with_class(lba, buf, class)
    }

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        self.shared.device.lock().discard_capability()
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        self.shared.device.lock().discard(lba, blocks)
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        self.shared.device.lock().device_health()
    }
}

/// The long-lived kernel service that owns the boot disk's [`SharedBlock`]
/// and keeps the read-only `/System` driver store mounted for the life of
/// the system (Design D, D2a-2 — `.junie/next-pi-prompt.md`).
///
/// The bootstrap floor brings up exactly **one** block device, which the
/// boot path uses for two things at once — autoloading the signed `/System`
/// driver store and unlocking the encrypted root — as concurrent windows
/// onto that one disk ([`SharedBlock`]). Under Design D the `/System` store
/// must stay reachable *after* boot for on-demand and reactive (hotplug)
/// driver loads, and `/System` must stay mounted
/// anyway so other subsystems can reach it.
///
/// The device backing (DMA pool, MMIO map, IRQ waiter, virtio host) is
/// boot-leaked to `'static` (`kernel/tairix-kernel`'s `root_unlock`), so this
/// service — and the [`SharedBlock`] it owns — is itself leaked to `'static`
/// and shared by `&'static` across two independent preemptive tasks: the
/// disk-owning **driver-store serve** task (which binds and answers the store
/// IPC endpoint, independent of the user-data passphrase) and the
/// **encrypted-root unlock** task. Each reaches the disk through a fresh
/// [`SharedBlockHandle`] from [`Self::window`], serialised against the other
/// by the `SharedBlock` lock (disk access is a common,
/// capability-checked kernel service). [`Self::hold`] parks the calling task
/// for life when it has no endpoint to serve.
pub struct DriverStoreService<B: Block> {
    shared: SharedBlock<B>,
}

impl<B: Block> DriverStoreService<B> {
    /// Take ownership of the boot disk's [`SharedBlock`] as the driver store.
    #[must_use]
    pub fn new(shared: SharedBlock<B>) -> Self {
        Self { shared }
    }

    /// A fresh read-only window onto the boot disk holding the `/System`
    /// driver store. Each call hands out an independent [`SharedBlockHandle`];
    /// the `SharedBlock` lock serialises device operations across windows, so the autoload window and the encrypted-root unlock
    /// window never interleave a device operation.
    #[must_use]
    pub fn window(&self) -> BorrowedBlockWindow<'_, B> {
        self.shared.handle()
    }

    /// Park the calling kernel-service task for life.
    ///
    /// This never returns. Used on the fail-closed fallback when there is no
    /// `/System` store endpoint to serve (no volume, or the bind failed): the
    /// disk-owning task still owns the leaked `'static` disk, so it parks
    /// rather than exiting, and an `ipc_call` to the unbound store endpoint
    /// fails closed with `NotFound` rather than blocking.
    /// It **parks** rather than yields, so it consumes no CPU while idle
    /// (never a busy-yield loop); a spurious wake re-parks.
    /// Takes `&self` because the service is a leaked `'static` value shared by
    /// reference, never owned by one frame.
    pub fn hold(&self, coop: &CooperativeYield<'_>) -> ! {
        loop {
            coop.park();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::driver::block::HealthSnapshot;

    /// A minimal in-memory [`Block`] over a fixed byte store, with a
    /// staging buffer so the classified-read scrub contract can be
    /// exercised, and a recorded discard log. Mirrors the
    /// `lib/abi::driver::block` test device.
    struct MemBlock {
        geo: BlockGeometry,
        store: [u8; 1024],
        staging: [u8; 64],
        scrubbed: bool,
        discarded: [(u64, u64); 4],
        recorded: usize,
        health: DeviceHealth,
    }

    impl MemBlock {
        fn new() -> Self {
            Self {
                geo: BlockGeometry {
                    block_size: 64,
                    block_count: 16,
                },
                store: [0u8; 1024],
                staging: [0u8; 64],
                scrubbed: false,
                discarded: [(0, 0); 4],
                recorded: 0,
                health: DeviceHealth::Unavailable,
            }
        }

        fn span(&self, lba: u64, len: usize) -> Result<usize, DriverError> {
            let bs = self.geo.block_size as usize;
            if len == 0 || !len.is_multiple_of(bs) {
                return Err(DriverError::BufferTooSmall);
            }
            let blocks = (len / bs) as u64;
            if lba.saturating_add(blocks) > self.geo.block_count {
                return Err(DriverError::LengthOutOfRange);
            }
            let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * bs;
            Ok(start)
        }
    }

    impl Block for MemBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(self.geo)
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            let start = self.span(lba, buf.len())?;
            buf.copy_from_slice(&self.store[start..start + buf.len()]);
            Ok(())
        }

        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
            let start = self.span(lba, buf.len())?;
            self.store[start..start + buf.len()].copy_from_slice(buf);
            Ok(())
        }

        fn read_blocks_with_class(
            &mut self,
            lba: u64,
            buf: &mut [u8],
            class: BufferClass,
        ) -> Result<(), DriverError> {
            let start = self.span(lba, buf.len())?;
            self.staging[..buf.len()].copy_from_slice(&self.store[start..start + buf.len()]);
            buf.copy_from_slice(&self.staging[..buf.len()]);
            if class.is_sensitive() {
                self.staging.fill(0);
                self.scrubbed = true;
            } else {
                self.scrubbed = false;
            }
            Ok(())
        }

        fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
            Ok(DiscardCapability {
                supported: true,
                granularity_blocks: 1,
                max_blocks_per_request: 0,
            })
        }

        fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
            if lba.saturating_add(blocks) > self.geo.block_count {
                return Err(DriverError::LengthOutOfRange);
            }
            if self.recorded < self.discarded.len() {
                self.discarded[self.recorded] = (lba, blocks);
                self.recorded += 1;
            }
            Ok(())
        }

        fn device_health(&self) -> Result<DeviceHealth, DriverError> {
            Ok(self.health)
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    #[test]
    fn geometry_is_cached_and_served_lock_free() {
        let shared = SharedBlock::new(MemBlock::new()).expect("geometry queried at construction");
        assert_eq!(shared.geometry().block_size, 64);
        assert_eq!(shared.geometry().block_count, 16);
        // The handle's `geometry()` reports the same cached value.
        let handle = shared.handle();
        assert_eq!(handle.geometry().unwrap(), shared.geometry());
    }

    #[test]
    fn a_geometry_fault_refuses_to_wrap_the_device() {
        // A device that faults on `geometry` is never wrapped, so no handle
        // can be handed out for an unusable device (fail closed).
        struct FaultyGeometry;
        impl Block for FaultyGeometry {
            fn geometry(&self) -> Result<BlockGeometry, DriverError> {
                Err(DriverError::DeviceFault)
            }
            fn read_blocks(&mut self, _: u64, _: &mut [u8]) -> Result<(), DriverError> {
                Err(DriverError::Unsupported)
            }
            fn write_blocks(&mut self, _: u64, _: &[u8]) -> Result<(), DriverError> {
                Err(DriverError::Unsupported)
            }
            fn flush(&mut self) -> Result<(), DriverError> {
                Err(DriverError::Unsupported)
            }
        }
        assert_eq!(
            SharedBlock::new(FaultyGeometry).err(),
            Some(DriverError::DeviceFault)
        );
    }

    #[test]
    fn a_handle_round_trips_reads_and_writes_to_the_shared_device() {
        let shared = SharedBlock::new(MemBlock::new()).unwrap();
        let mut handle = shared.handle();
        let payload = [0xABu8; 64];
        handle.write_blocks(2, &payload).unwrap();
        let mut readback = [0u8; 64];
        handle.read_blocks(2, &mut readback).unwrap();
        assert_eq!(readback, payload);
    }

    #[test]
    fn two_concurrent_handles_observe_the_one_underlying_device() {
        // The whole point of the layer: a write through one window is
        // visible through a second window over the same shared device —
        // exactly the `/System` store window and the unlock window sharing
        // one boot disk.
        let shared = SharedBlock::new(MemBlock::new()).unwrap();
        let mut writer = shared.handle();
        let mut reader = shared.handle();
        let payload = [0x5Au8; 64];
        writer.write_blocks(5, &payload).unwrap();
        let mut readback = [0u8; 64];
        reader.read_blocks(5, &mut readback).unwrap();
        assert_eq!(
            readback, payload,
            "the second window sees the first's write"
        );
    }

    #[test]
    fn out_of_range_and_short_buffers_fail_closed_through_a_handle() {
        let shared = SharedBlock::new(MemBlock::new()).unwrap();
        let mut handle = shared.handle();
        let mut tiny = [0u8; 8];
        assert_eq!(
            handle.read_blocks(0, &mut tiny),
            Err(DriverError::BufferTooSmall)
        );
        let mut buf = [0u8; 64];
        assert_eq!(
            handle.read_blocks(100, &mut buf),
            Err(DriverError::LengthOutOfRange)
        );
    }

    #[test]
    fn the_classified_read_scrub_contract_is_forwarded() {
        let shared = SharedBlock::new(MemBlock::new()).unwrap();
        let mut handle = shared.handle();
        let mut buf = [0u8; 64];
        handle
            .read_blocks_with_class(0, &mut buf, BufferClass::Sensitive)
            .unwrap();
        // The device's staging scrub fired through the forwarding handle.
        assert!(shared.device.lock().scrubbed);
    }

    #[test]
    fn discard_and_health_are_forwarded() {
        let mut device = MemBlock::new();
        device.health = DeviceHealth::Available(HealthSnapshot {
            power_on_hours: 1,
            unsafe_shutdowns: 0,
            media_errors: 0,
            reallocated_sectors: 0,
            pending_sectors: 0,
            uncorrectable_sectors: 0,
            crc_errors: 0,
            percentage_used: 0,
            available_spare: 100,
            temperature_kelvin: 300,
            critical_warning: false,
        });
        let shared = SharedBlock::new(device).unwrap();
        let mut handle = shared.handle();
        assert!(handle.discard_capability().unwrap().supported);
        handle.discard(2, 2).unwrap();
        assert_eq!(shared.device.lock().discarded[0], (2, 2));
        assert!(matches!(
            handle.device_health().unwrap(),
            DeviceHealth::Available(_)
        ));
    }

    #[test]
    fn an_owned_window_outlives_the_scope_that_opened_it() {
        // The runtime-mount case: the filesystem driver built over the
        // window is stored in the mount registry and long outlives the
        // attach call that opened it, so the window must own its reference
        // to the device rather than borrow it.
        let mut window = {
            let shared = Arc::new(SharedBlock::new(MemBlock::new()).unwrap());
            shared.owned_handle()
        };
        let payload = [0x7Eu8; 64];
        window.write_blocks(3, &payload).unwrap();
        let mut readback = [0u8; 64];
        window.read_blocks(3, &mut readback).unwrap();
        assert_eq!(readback, payload);
    }

    #[test]
    fn owned_windows_drive_the_one_shared_device() {
        // Two mounted partitions of one disk: each holds its own window,
        // both reach the one device, and the lock serialises them.
        let shared = Arc::new(SharedBlock::new(MemBlock::new()).unwrap());
        let mut first = shared.owned_handle();
        let mut second = shared.owned_handle();
        let payload = [0x2Bu8; 64];
        first.write_blocks(4, &payload).unwrap();
        let mut readback = [0u8; 64];
        second.read_blocks(4, &mut readback).unwrap();
        assert_eq!(
            readback, payload,
            "the second window sees the first's write"
        );
    }

    #[test]
    fn the_device_is_released_when_the_last_owned_window_drops() {
        // The device (and, in production, its client and window hold) must
        // go when the disk's last mount does — not linger.
        let shared = Arc::new(SharedBlock::new(MemBlock::new()).unwrap());
        let first = shared.owned_handle();
        let second = shared.owned_handle();
        assert_eq!(Arc::strong_count(&shared), 3);
        drop(first);
        drop(second);
        assert_eq!(
            Arc::strong_count(&shared),
            1,
            "dropping the windows releases their references"
        );
    }

    #[test]
    fn an_owned_window_reports_the_shared_device_class_and_geometry() {
        // Both window flavours answer from the one cache, so a consumer
        // above the sharing boundary still derives the real device's I/O
        // budget rather than the unclassified default.
        let shared = Arc::new(SharedBlock::new(MemBlock::new()).unwrap());
        let window = shared.owned_handle();
        assert_eq!(window.device_class(), shared.device_class());
        assert_eq!(window.geometry().unwrap(), shared.geometry());
    }

    #[test]
    fn the_driver_store_service_hands_out_windows_onto_the_one_disk() {
        // The service owns the boot disk's `SharedBlock` and serves the
        // `/System` store through independent windows: a write through one
        // window is visible through a second, exactly as the boot autoload
        // window and the encrypted-root unlock window share one disk.
        let service = DriverStoreService::new(SharedBlock::new(MemBlock::new()).unwrap());
        let mut writer = service.window();
        let payload = [0x3Cu8; 64];
        writer.write_blocks(7, &payload).unwrap();
        let mut reader = service.window();
        let mut readback = [0u8; 64];
        reader.read_blocks(7, &mut readback).unwrap();
        assert_eq!(
            readback, payload,
            "a second service window sees the first window's write"
        );
    }
}
