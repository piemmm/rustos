//! In-kernel [`DriverHost`] composition: MMIO **and** DMA.
//!
//! [`provision_and_run`](crate::provision_and_run) brings up a single
//! virtio-PCI device and hands the loaded driver a per-driver DMA host;
//! it deliberately maps the device's register windows itself and exposes
//! no [`MmioMapper`] to the driver. A non-virtio, DMA-driving controller
//! reached through a discovered register window — the VL805 xHCI behind
//! the BCM2711 PCIe root complex (`plans/PI.md` P10) is the motivating
//! case — needs the opposite: the loaded bus driver maps its *own*
//! register windows (the PCIe controller block, then the device BAR)
//! through the host, and also carves a device-shared DMA region.
//!
//! [`run_with_driver_host`] assembles the in-kernel [`DriverHost`] that
//! serves both: a capability-gated [`KernelMmioMapper`] reachable through
//! [`DriverHost::mmio_mapper`] and a [`KernelVirtioFactory`] reachable
//! through [`DriverHost::virtio_host`]. Both are built on the call's own
//! stack frame and lent to a `body` closure; the host, the factory, the
//! per-driver DMA pools it mints, and every register window the driver
//! mapped are all reclaimed when the closure returns (`AGENTS.md` §4 — no
//! driver retains a register window or DMA mapping past its load).
//!
//! # Why a scope/callback
//!
//! The mapper borrows the per-driver [`MmioMap`] mutably and the factory
//! borrows the frame allocator and direct physical map; the assembled
//! [`Host`] borrows both. None can outlive a single boot frame, so —
//! exactly as in [`provision_and_run`](crate::provision_and_run) — this
//! function constructs every piece on its own frame and lends the host to
//! the caller's `body` rather than trying to return a self-referential
//! value.
//!
//! [`MmioMapper`]: rustos_abi::MmioMapper
//! [`DriverHost`]: rustos_abi::DriverHost
//! [`DriverHost::mmio_mapper`]: rustos_abi::DriverHost::mmio_mapper
//! [`DriverHost::virtio_host`]: rustos_abi::DriverHost::virtio_host

use rustos_crypto::Ed25519PublicKey;
use rustos_drvhost::{DriverSpawner, Host, HostConfig, ImageSource};
use rustos_kernel_irq::{IrqTable, IrqWaiter};
use rustos_kernel_mem::{FrameAllocator, MmioMap, PageTable, PhysMap, VirtAddr};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_kernel_virtio::kernel_mmio::KernelMmioMapper;
use rustos_kernel_virtio::virtio_factory::{KernelVirtioFactory, KernelVirtioFactoryConfig};
use rustos_log::Sink;

/// Borrowed boot resources [`run_with_driver_host`] needs to assemble an
/// in-kernel [`Host`] that serves a loaded driver both an
/// [`MmioMapper`](rustos_abi::MmioMapper) and a per-driver
/// [`VirtioHost`](rustos_abi::driver::VirtioHost).
///
/// Every field is borrowed for `'k`; the assembled host and the pools it
/// mints live only for the duration of the `body` closure. `P` is the
/// page-table backend shared by the MMIO map and each minted per-driver
/// address space.
pub struct DriverHostConfig<'k, 'p, P: PageTable> {
    /// Per-driver MMIO map every register window the driver maps is
    /// allocated into. Retains every window for the host's lifetime;
    /// the kernel reclaims them when the map (and its address space) is
    /// torn down at unload.
    pub mmio: &'k mut MmioMap<'p, P>,
    /// Physical frame allocator the per-driver DMA pool draws from.
    pub frames: &'k FrameAllocator,
    /// Direct physical map the DMA pool reaches its frames through.
    pub phys: &'k dyn PhysMap,
    /// Capabilities of the bus-driver task; every MMIO map and DMA
    /// allocation is audited against this set, and the blocking IRQ
    /// wait keys on `caller.task()` (`AGENTS.md` §5.4 — forgery
    /// defence).
    pub caller: &'k TaskCapabilities,
    /// Audit sink for every map/DMA/load decision.
    pub audit: &'k dyn Sink,
    /// Kernel IRQ table the device's interrupt line is bound in.
    pub irq: &'k IrqTable,
    /// Handle for the device's bound interrupt line.
    pub irq_handle: rustos_abi::IrqHandle,
    /// Clock + cooperative-yield seam the blocking wait loop drives.
    pub waiter: &'k dyn IrqWaiter,
    /// Base virtual address of each minted per-driver DMA window.
    pub pool_base: VirtAddr,
    /// Capacity, in pages, of each minted per-driver DMA window.
    pub pool_pages: usize,
    /// Public keys whose `.rxe` signatures the host accepts.
    pub trusted_signers: &'k [Ed25519PublicKey],
    /// SHA-256 of the syscall table the host was built against.
    pub syscall_table_hash: [u8; 32],
    /// ABI version the host accepts.
    pub accepted_abi_version: u32,
    /// Storage backend supplying `.rxe` image bytes.
    pub source: &'k dyn ImageSource,
    /// Spawner completing a verified manifest's registration.
    pub spawner: &'k dyn DriverSpawner,
}

/// Assemble an in-kernel [`Host`] whose
/// [`mmio_mapper`](rustos_abi::DriverHost::mmio_mapper) maps register
/// windows through the capability-gated [`KernelMmioMapper`] and whose
/// [`virtio_host`](rustos_abi::DriverHost::virtio_host) factory mints a
/// per-driver DMA host, and lend the host to `body`.
///
/// `make_table` is invoked once per minted driver DMA host to produce
/// the empty page table backing that driver's private DMA address
/// space; it must return a fresh, empty table each time (`AGENTS.md`
/// §4).
///
/// The returned value is whatever `body` returns — typically the outcome
/// of `host.load(…)` for the bus driver that drives the discovered
/// controller. Capability and input checks stay kernel-side at the
/// mapper and the DMA gate (`AGENTS.md` §5.4); this function adds no
/// authority of its own.
pub fn run_with_driver_host<P, F, R>(
    config: DriverHostConfig<'_, '_, P>,
    make_table: F,
    body: impl FnOnce(&mut Host<'_>) -> R,
) -> R
where
    P: PageTable,
    F: Fn() -> P,
{
    let DriverHostConfig {
        mmio,
        frames,
        phys,
        caller,
        audit,
        irq,
        irq_handle,
        waiter,
        pool_base,
        pool_pages,
        trusted_signers,
        syscall_table_hash,
        accepted_abi_version,
        source,
        spawner,
    } = config;

    let mapper = KernelMmioMapper::new(mmio, caller, audit);
    let factory = KernelVirtioFactory::new(
        KernelVirtioFactoryConfig {
            frames,
            phys,
            caller,
            audit,
            irq,
            irq_handle,
            waiter,
            pool_base,
            pool_pages,
        },
        make_table,
    );

    let mut host = Host::new(HostConfig {
        trusted_signers,
        syscall_table_hash,
        accepted_abi_version,
        source,
        spawner,
        sink: audit,
        virtio_host_factory: Some(&factory),
        mmio_mapper: Some(&mapper),
    });

    body(&mut host)
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use alloc::vec::Vec;
    use ed25519_dalek::{Signer, SigningKey};
    use rustos_abi::{
        CapabilityId, DriverError, DriverHandle, DriverHost, DriverKind, DriverManifest, Errno,
        DRIVER_MANIFEST_MAGIC,
    };
    use rustos_caps::CapabilitySet;
    use rustos_drvhost::{SpawnContext, SpawnRegisterError};
    use rustos_kernel_irq::{IrqTable, IrqWaitAbort, IrqWaiter};
    use rustos_kernel_mem::{
        bootinfo::{BootMemoryMap, MemoryRegion, RegionKind},
        AddressSpace, FrameAllocator, HostPageTable, MmioMap, PhysAddr, SimPhysMap, VirtAddr,
        PAGE_SIZE,
    };
    use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
    use rustos_kernel_sec::identity::UserId;
    use rustos_log::{Event, Sink};

    const OWNER: TaskId = TaskId(88);
    const SEED: [u8; 32] = [9u8; 32];
    /// Physical base of the register block the driver maps; the
    /// `SimPhysMap` backing the MMIO map covers `[BASE, BASE + 0x4000)`.
    const MMIO_PHYS_BASE: u64 = 0xFEBD_0000;

    /// Latches the loaded driver flips to prove it reached both the
    /// MMIO mapper and the per-driver DMA host through the in-kernel
    /// `DriverHost`.
    static MMIO_OK: AtomicBool = AtomicBool::new(false);
    static MMIO_READBACK: AtomicU64 = AtomicU64::new(0);
    static DMA_OK: AtomicBool = AtomicBool::new(false);
    static DMA_LEN: AtomicUsize = AtomicUsize::new(0);

    struct IdleWaiter;
    impl IrqWaiter for IdleWaiter {
        fn now_ns(&self) -> u64 {
            0
        }
        fn yield_now(&self) -> Result<(), IrqWaitAbort> {
            Ok(())
        }
    }

    struct Recorder;
    impl Sink for Recorder {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    struct OneImage {
        image: Vec<u8>,
    }
    impl ImageSource for OneImage {
        fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
            buf.extend_from_slice(&self.image);
            Ok(())
        }
    }

    /// Spawner registering every verified manifest in-process through
    /// [`bus_register`].
    struct ToBusRegister;
    impl DriverSpawner for ToBusRegister {
        fn spawn_and_register(
            &self,
            ctx: &SpawnContext<'_>,
        ) -> Result<DriverHandle, SpawnRegisterError> {
            bus_register(ctx.host).map_err(SpawnRegisterError::Register)
        }
    }

    /// Driver entry point: map a register window through the host's
    /// MMIO mapper, round-trip a dword, and allocate a zeroed DMA slab
    /// through the host's virtio facility — exercising both halves of
    /// the in-kernel `DriverHost` the VL805 composition relies on.
    fn bus_register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
        let mapper = host.mmio_mapper().ok_or(DriverError::Unsupported)?;
        let window = mapper
            .map_window(MMIO_PHYS_BASE, 0x1000)
            .map_err(|_| DriverError::DeviceFault)?;
        window
            .write_u32(0x10, 0xCAFE_F00D)
            .map_err(|_| DriverError::DeviceFault)?;
        let read = window
            .read_u32(0x10)
            .map_err(|_| DriverError::DeviceFault)?;
        MMIO_READBACK.store(u64::from(read), Ordering::SeqCst);
        MMIO_OK.store(true, Ordering::SeqCst);

        let vh = host.virtio_host().ok_or(DriverError::Unsupported)?;
        let slab = vh.alloc_dma_zeroed(PAGE_SIZE)?;
        if slab.as_bytes().iter().all(|b| *b == 0) {
            DMA_OK.store(true, Ordering::SeqCst);
            DMA_LEN.store(slab.len(), Ordering::SeqCst);
        }
        DriverHandle::from_raw(0x00B5_F00D)
    }

    fn build_signed_image(
        signing_key: &SigningKey,
        syscall_table_hash: [u8; 32],
        caps: &[CapabilityId],
    ) -> Vec<u8> {
        let signer_pubkey: [u8; 32] = signing_key.verifying_key().to_bytes();
        let count = u16::try_from(caps.len()).expect("caps fit in u16");
        let mut manifest = DriverManifest {
            magic: DRIVER_MANIFEST_MAGIC,
            abi_version: rustos_abi::ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            bind_key_count: 0,
            capability_count: count,
            syscall_table_hash,
            signer_pubkey,
            signature: [0u8; 64],
        };
        let encoded = manifest.to_le_bytes();
        let mut cap_body = Vec::with_capacity(caps.len() * 2);
        for c in caps {
            cap_body.extend_from_slice(&c.as_u16().to_le_bytes());
        }
        let mut signed = Vec::new();
        signed.extend_from_slice(&encoded[..DriverManifest::WIRE_LEN - 64]);
        signed.extend_from_slice(&cap_body);
        manifest.signature = signing_key.sign(&signed).to_bytes();
        let mut out = Vec::new();
        out.extend_from_slice(&manifest.to_le_bytes());
        out.extend_from_slice(&cap_body);
        out
    }

    fn usable_map(pages: usize) -> BootMemoryMap {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(PAGE_SIZE as u64 * 16),
            length: (PAGE_SIZE * pages) as u64,
        });
        m
    }

    fn task_with(caps: &[CapabilityId], sink: &Recorder) -> TaskCapabilities {
        let mut set = CapabilitySet::empty();
        for c in caps {
            set.insert(*c);
        }
        TaskCapabilities::derive(OWNER, UserId(1000), set, set, sink)
    }

    fn pubkey_of(sk: &SigningKey) -> Ed25519PublicKey {
        Ed25519PublicKey::from_bytes(&sk.verifying_key().to_bytes()).expect("valid key")
    }

    #[test]
    fn host_serves_both_mmio_and_dma_to_a_loaded_driver() {
        MMIO_OK.store(false, Ordering::SeqCst);
        DMA_OK.store(false, Ordering::SeqCst);
        DMA_LEN.store(0, Ordering::SeqCst);
        MMIO_READBACK.store(0, Ordering::SeqCst);

        let mmio_sim = SimPhysMap::new(PhysAddr::new(MMIO_PHYS_BASE), 0x4000);
        let mut mmio = MmioMap::new(
            AddressSpace::new(HostPageTable::new()),
            VirtAddr::new(0x6000_0000),
            32,
            &mmio_sim,
        )
        .expect("mmio map constructs");

        let dma_pages = 32;
        let frames = FrameAllocator::new(&usable_map(dma_pages)).expect("frames");
        let dma_sim = SimPhysMap::new(PhysAddr::new(PAGE_SIZE as u64 * 16), dma_pages * PAGE_SIZE);

        let sink = Recorder;
        let caller = task_with(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA], &sink);
        let irq = IrqTable::new(31);
        let irq_handle = irq.bind(7, OWNER).expect("bind device line").handle;
        let waiter = IdleWaiter;

        let signing_key = SigningKey::from_bytes(&SEED);
        let trusted = [pubkey_of(&signing_key)];
        let syscall_hash = [0x3Cu8; 32];
        let image = build_signed_image(
            &signing_key,
            syscall_hash,
            &[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA],
        );
        let source = OneImage { image };
        let spawner = ToBusRegister;

        let config = DriverHostConfig {
            mmio: &mut mmio,
            frames: &frames,
            phys: &dma_sim,
            caller: &caller,
            audit: &sink,
            irq: &irq,
            irq_handle,
            waiter: &waiter,
            pool_base: VirtAddr::new(0x2000_0000),
            pool_pages: 16,
            trusted_signers: &trusted,
            syscall_table_hash: syscall_hash,
            accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
            source: &source,
            spawner: &spawner,
        };

        let mut caller_caps = CapabilitySet::empty();
        caller_caps.insert(CapabilityId::DRV_LOAD);
        caller_caps.insert(CapabilityId::MMIO_MAP);
        caller_caps.insert(CapabilityId::MEM_DMA);

        run_with_driver_host(config, HostPageTable::new, |host| {
            host.load("/System/Drivers/bus.rxe", &caller_caps)
                .expect("driver loads");
            assert_eq!(host.loaded_count(), 1);
        });

        assert!(MMIO_OK.load(Ordering::SeqCst), "driver mapped a window");
        assert_eq!(MMIO_READBACK.load(Ordering::SeqCst), 0xCAFE_F00D);
        assert!(DMA_OK.load(Ordering::SeqCst), "driver allocated a slab");
        assert_eq!(DMA_LEN.load(Ordering::SeqCst), PAGE_SIZE);
        // The driver mapped exactly its one register window.
        assert_eq!(mmio.live(), 1);
    }

    #[test]
    fn driver_without_mmio_cap_is_refused_fail_closed() {
        // This test reads only *local* state (the load result and its
        // own `MmioMap`); the shared `MMIO_OK`/`DMA_OK` latches are
        // owned by `host_serves_both_…` and must not be touched here,
        // or the two tests would race under the parallel runner.
        let mmio_sim = SimPhysMap::new(PhysAddr::new(MMIO_PHYS_BASE), 0x4000);
        let mut mmio = MmioMap::new(
            AddressSpace::new(HostPageTable::new()),
            VirtAddr::new(0x6000_0000),
            32,
            &mmio_sim,
        )
        .expect("mmio map constructs");
        let frames = FrameAllocator::new(&usable_map(32)).expect("frames");
        let dma_sim = SimPhysMap::new(PhysAddr::new(PAGE_SIZE as u64 * 16), 32 * PAGE_SIZE);
        let sink = Recorder;
        // The driver's task may DMA but was never granted MMIO_MAP, so
        // the in-kernel mapper refuses the window and register() fails
        // closed before any DMA is reached.
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let irq = IrqTable::new(31);
        let irq_handle = irq.bind(7, OWNER).expect("bind").handle;
        let waiter = IdleWaiter;
        let signing_key = SigningKey::from_bytes(&SEED);
        let trusted = [pubkey_of(&signing_key)];
        let syscall_hash = [0x3Cu8; 32];
        // The manifest may only request a subset of the caller's caps.
        let image = build_signed_image(&signing_key, syscall_hash, &[CapabilityId::MEM_DMA]);
        let source = OneImage { image };
        let spawner = ToBusRegister;

        let config = DriverHostConfig {
            mmio: &mut mmio,
            frames: &frames,
            phys: &dma_sim,
            caller: &caller,
            audit: &sink,
            irq: &irq,
            irq_handle,
            waiter: &waiter,
            pool_base: VirtAddr::new(0x2000_0000),
            pool_pages: 16,
            trusted_signers: &trusted,
            syscall_table_hash: syscall_hash,
            accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
            source: &source,
            spawner: &spawner,
        };

        let mut caller_caps = CapabilitySet::empty();
        caller_caps.insert(CapabilityId::DRV_LOAD);
        caller_caps.insert(CapabilityId::MEM_DMA);

        run_with_driver_host(config, HostPageTable::new, |host| {
            let err = host
                .load("/System/Drivers/bus.rxe", &caller_caps)
                .expect_err("missing MMIO_MAP must fail closed");
            // The driver's register() returned DeviceFault after the
            // mapper refused (CapabilityMissing → mapped to a window
            // error → DeviceFault in `bus_register`).
            assert!(matches!(
                err,
                rustos_drvhost::HostError::DriverRegisterFailed(_)
            ));
        });

        // No window survived a refused load: the mapper rolled back
        // when `map_mmio` refused, so register() never reached the DMA
        // carve (it returned at the mapper step). `mmio.live() == 0` is
        // the local, race-free witness.
        assert_eq!(mmio.live(), 0);
    }
}
