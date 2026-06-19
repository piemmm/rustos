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
//! (on-demand and reactive driver loads, `AGENTS.md` §18.3 / §18.4), so the
//! one device must back **two concurrent partition windows**. This module
//! is that primitive: a [`SharedBlock`] owns the device behind a
//! `lib/sync` [`SpinLock`] and hands out as many [`SharedBlockHandle`]s as
//! there are windows, each of which is itself a [`Block`]. Every read /
//! write / discard takes the lock for the duration of the single device
//! operation, so concurrent windows on different CPUs are serialised
//! (`AGENTS.md` §4 — SMP from day one, explicit synchronisation).
//!
//! The device's [`BlockGeometry`] is immutable for the life of a disk, so
//! it is queried **once** at construction and cached: [`SharedBlock::geometry`]
//! and the handle's [`Block::geometry`] are then lock-free and infallible,
//! keeping that hot, frequently-read value off the lock (`AGENTS.md`
//! §2.16). Geometry is the only cached value; every byte-moving operation
//! goes to the device under the lock.
//!
//! A plain [`SpinLock`] (not the IRQ-safe variant) is correct here: block
//! I/O is driven from task / kthread context — the device IRQ only *wakes*
//! the waiting kthread, it never issues a `read_blocks` from inside the
//! handler — so the lock is never taken from an interrupt
//! (`docs/src/architecture/sync.md`). The critical section is one device
//! operation and contention is low (at most the unlock window vs. the
//! driver-store window), matching the [`SpinLock`] use case.
//!
//! Architecture-neutral (`AGENTS.md` §2.2 / §2.20): the layer names no
//! device type and no architecture — it is generic over any [`Block`], so
//! every port shares this one definition and the per-device bring-up
//! (virtio-blk, EMMC2, …) wraps its brought-up device in it.

use rustos_abi::driver::block::{Block, BlockGeometry, DeviceHealth, DiscardCapability};
use rustos_abi::driver::BufferClass;
use rustos_abi::DriverError;
use rustos_kernel_core::CooperativeYield;
use rustos_sync::SpinLock;

/// A [`Block`] device shared behind a lock so several concurrent windows
/// can drive it, each device operation serialised.
///
/// Construct one with [`SharedBlock::new`] (which queries and caches the
/// device geometry once), then call [`SharedBlock::handle`] for each
/// independent consumer. Each returned [`SharedBlockHandle`] borrows the
/// `SharedBlock`, is itself a [`Block`], and may be layered under a
/// `rustos_partition::PartitionBlock` window exactly like a bare device.
///
/// `SharedBlock<B>` is [`Sync`] when `B: Send` (the [`SpinLock`] makes the
/// interior access exclusive), so it may be shared by `&` across CPUs.
pub struct SharedBlock<B: Block> {
    /// The owned device. Every byte-moving operation locks this for the
    /// duration of one device call (`AGENTS.md` §4).
    device: SpinLock<B>,
    /// The device geometry, queried once at construction. Immutable for the
    /// life of a disk, so it is served lock-free (`AGENTS.md` §2.16).
    geometry: BlockGeometry,
}

impl<B: Block> SharedBlock<B> {
    /// Wrap `device` for shared access, caching its geometry.
    ///
    /// # Errors
    ///
    /// Propagates [`Block::geometry`]'s error if the device geometry could
    /// not be queried — the device is never wrapped on a geometry fault, so
    /// no handle can be handed out for an unusable device (fail closed,
    /// `AGENTS.md` §5.4 / §2.9).
    pub fn new(device: B) -> Result<Self, DriverError> {
        let geometry = device.geometry()?;
        Ok(Self {
            device: SpinLock::new(device),
            geometry,
        })
    }

    /// The cached device geometry (lock-free; see the module docs).
    #[must_use]
    pub fn geometry(&self) -> BlockGeometry {
        self.geometry
    }

    /// A new independent window onto the shared device. Each handle is a
    /// [`Block`]; the underlying device serialises their operations.
    #[must_use]
    pub fn handle(&self) -> SharedBlockHandle<'_, B> {
        SharedBlockHandle { shared: self }
    }
}

/// One window onto a [`SharedBlock`]. It is itself a [`Block`]: every
/// operation locks the shared device for the duration of the single device
/// call, so concurrent handles never interleave a device operation.
///
/// Handles are cheap (a borrow of the [`SharedBlock`]) and independent: two
/// handles may be open at once — e.g. the read-only `/System` driver-store
/// window and the encrypted-root unlock window over the one boot disk.
pub struct SharedBlockHandle<'a, B: Block> {
    shared: &'a SharedBlock<B>,
}

impl<B: Block> Block for SharedBlockHandle<'_, B> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        // Served from the cache: immutable for the life of the disk, so no
        // lock and no device round-trip (`AGENTS.md` §2.16).
        Ok(self.shared.geometry)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.shared.device.lock().read_blocks(lba, buf)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.shared.device.lock().write_blocks(lba, buf)
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
/// driver loads (`AGENTS.md` §18.3 / §18.4), and `/System` must stay mounted
/// anyway so other subsystems can reach it.
///
/// This service is how that mount outlives the unlock **without** promoting
/// the device backing (DMA pool, MMIO map, IRQ waiter, virtio host) to
/// `'static`. It is run as the body of a never-returning kernel-service
/// kthread (`AGENTS.md` §17.1 — "a continuous service never returns"): the
/// kthread's device bring-up call chain stays suspended on its own coroutine
/// stack, so the *borrowed* backing those frames own stays live for free,
/// and [`Self::hold`] parks the kthread for life owning the [`SharedBlock`].
/// Every `/System` read goes through a fresh [`SharedBlockHandle`] from
/// [`Self::window`], serialised against any concurrent window by the
/// `SharedBlock` lock (`AGENTS.md` §4).
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
    /// the `SharedBlock` lock serialises device operations across windows
    /// (`AGENTS.md` §4), so the autoload window and the encrypted-root unlock
    /// window never interleave a device operation.
    #[must_use]
    pub fn window(&self) -> SharedBlockHandle<'_, B> {
        self.shared.handle()
    }

    /// Hold the `/System` mount for the life of the system: park the calling
    /// kernel-service kthread forever, owning the [`SharedBlock`].
    ///
    /// This never returns. The service owns the [`SharedBlock`] (and through
    /// it the boot disk) for the whole park, while the kthread's bring-up
    /// frames stay suspended beneath this call, keeping the borrowed device
    /// backing live. It **parks** rather than yields, so it consumes no CPU
    /// while there is no work (`AGENTS.md` §2.1 — never a busy-yield loop);
    /// a spurious wake simply re-parks. D2b wakes this kthread to serve
    /// `driver_store_load` reads through [`Self::window`], after which it
    /// re-parks here.
    pub fn hold(self, coop: &CooperativeYield<'_>) -> ! {
        loop {
            coop.park();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_abi::driver::block::HealthSnapshot;

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
            if len == 0 || len % bs != 0 {
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
