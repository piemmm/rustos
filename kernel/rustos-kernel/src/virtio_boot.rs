//! Live virtio-PCI boot wiring (Stage 4.D Item 4).
//!
//! [`virtio_pci_walk`](crate::virtio_pci_walk) turns a bus into a
//! [`PciTransport`], [`virtio_factory`](crate::virtio_factory) mints a
//! per-driver DMA host, and `userland/system/drvhost` runs the signed
//! `.rxe`. This module is the seam that joins the three into the single
//! sequence the production boot pipeline performs for a virtio-class
//! device: build the capability-gated [`KernelMmioMapper`], provision
//! the device's register windows into a [`PciTransport`], build a
//! [`KernelVirtioFactory`], and hand both to a live [`Host`].
//!
//! # Why a scope/callback
//!
//! The mapper, factory, and host all borrow the same boot resources and
//! each other, so none can outlive a single boot frame. Returning a
//! `Host` that borrows a locally-built factory is impossible; instead
//! [`provision_and_run`] constructs every piece on its own stack frame
//! and lends the assembled `Host` (plus the provisioned transport) to a
//! `body` closure. The host, factory, and the per-driver DMA pools it
//! mints are all reclaimed when the closure returns — no driver retains
//! a register window or DMA mapping past its load (`AGENTS.md` §4).

use rustos_abi::driver::msix::MsixBus;
use rustos_abi::driver::virtio_pci::VirtioPciBus;
use rustos_abi::{IrqHandle, MsiMessage};
use rustos_crypto::Ed25519PublicKey;
use rustos_drv_bus_virtio::{KernelMmioMapper, PciTransport};
use rustos_drvhost::{EntryResolver, Host, HostConfig, ImageSource};
use rustos_kernel_irq::{IrqTable, IrqWaiter};
use rustos_kernel_mem::{FrameAllocator, MmioMap, PageTableOps, PhysMap, VirtAddr};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_log::Sink;

use crate::virtio_factory::{KernelVirtioFactory, KernelVirtioFactoryConfig};
use crate::virtio_pci_walk::{provision_virtio_pci, VirtioPciWalkError};

/// Borrowed boot resources [`provision_and_run`] needs to bring a
/// virtio-PCI device all the way up to a loadable driver host.
///
/// Every field is borrowed for `'k`; the assembled host and the pools
/// it mints live only for the duration of the `body` closure passed to
/// [`provision_and_run`]. `P` is the page-table backend shared by the
/// MMIO map and each minted per-driver address space.
pub struct VirtioBootConfig<'k, 'p, P: PageTableOps> {
    /// PCI bus the device is provisioned from, reached only through the
    /// frozen [`VirtioPciBus`] ABI seam.
    pub bus: &'k dyn VirtioPciBus,
    /// The same PCI bus reached through the frozen [`MsixBus`] ABI seam,
    /// used to route the device's interrupt once the transport is up.
    /// In production this is the same `Pci` object as [`Self::bus`].
    pub msix: &'k dyn MsixBus,
    /// Modern virtio PCI device ID (`0x1040 + virtio_device_type`).
    pub device_id: u16,
    /// Per-driver MMIO map the device's four register windows are
    /// mapped into. Retains every window for the host's lifetime.
    pub mmio: &'k mut MmioMap<'p, P>,
    /// Physical frame allocator the per-driver DMA pool draws from.
    pub frames: &'k FrameAllocator,
    /// Direct physical map the DMA pool reaches its frames through.
    pub phys: &'k dyn PhysMap,
    /// Capabilities of the bus-driver task; every MMIO map and DMA
    /// allocation is audited against this set.
    pub caller: &'k TaskCapabilities,
    /// Audit sink for every map/DMA/load decision.
    pub audit: &'k dyn Sink,
    /// Kernel IRQ table the device's interrupt line is bound in.
    pub irq: &'k IrqTable,
    /// Handle for the device's bound interrupt line.
    pub irq_handle: IrqHandle,
    /// MSI-X table entry to program with [`Self::msi_message`].
    pub msix_entry: u16,
    /// Architecture-built MSI message delivering the bound interrupt's
    /// vector. Built by the arch layer (e.g.
    /// `rustos_arch_x86_64::irq::msi_message`) from the vector the
    /// [`Self::irq_handle`]'s line maps to; copied verbatim into the
    /// device's MSI-X table entry by [`MsixBus::route_msix`].
    pub msi_message: MsiMessage,
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
    /// Resolver turning a verified manifest into a `register` entry.
    pub resolver: &'k dyn EntryResolver,
}

/// Provision the virtio-PCI device described by `config`, assemble a
/// live [`Host`] whose [`virtio_host`](rustos_abi::DriverHost::virtio_host)
/// factory mints a per-driver DMA host, and lend both the host and the
/// provisioned [`PciTransport`] to `body`.
///
/// `make_table` is invoked once per minted driver host to produce the
/// empty page table backing that driver's private address space; it
/// must return a fresh, empty table each time (`AGENTS.md` §4).
///
/// # Errors
///
/// Returns [`VirtioPciWalkError`] if the device cannot be found on the
/// bus or its register windows cannot be mapped (e.g. the caller lacks
/// `CAP_MMIO_MAP`); the host is never constructed in that case.
pub fn provision_and_run<P, F, R>(
    config: VirtioBootConfig<'_, '_, P>,
    make_table: F,
    body: impl FnOnce(&mut Host<'_>, &PciTransport) -> R,
) -> Result<R, VirtioPciWalkError>
where
    P: PageTableOps,
    F: Fn() -> P,
{
    let VirtioBootConfig {
        bus,
        device_id,
        mmio,
        frames,
        phys,
        caller,
        audit,
        irq,
        irq_handle,
        msix,
        msix_entry,
        msi_message,
        waiter,
        pool_base,
        pool_pages,
        trusted_signers,
        syscall_table_hash,
        accepted_abi_version,
        source,
        resolver,
    } = config;

    let transport = {
        let mapper = KernelMmioMapper::new(mmio, caller, audit);
        let provision = provision_virtio_pci(bus, device_id, &mapper)?;
        msix.route_msix(provision.bdf, msix_entry, msi_message, &mapper)
            .map_err(VirtioPciWalkError::RouteMsix)?;
        provision.transport
    };

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
        resolver,
        sink: audit,
        virtio_host_factory: Some(&factory),
    });

    Ok(body(&mut host, &transport))
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::Cell;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use alloc::vec::Vec;
    use ed25519_dalek::{Signer, SigningKey};
    use rustos_abi::driver::bus::{Bus, BusDevice};
    use rustos_abi::driver::msix::MsixBus;
    use rustos_abi::driver::virtio_pci::{
        VirtioPciBus, VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_DEVICE, VIRTIO_PCI_CFG_ISR,
        VIRTIO_PCI_CFG_NOTIFY, VIRTIO_PCI_VENDOR_ID,
    };
    use rustos_abi::{
        CapabilityId, DriverError, DriverHandle, DriverHost, DriverKind, DriverManifest, Errno,
        MmioMapError, MmioMapper, MsiMessage, RegisterWindow, DRIVER_MANIFEST_MAGIC,
    };
    use rustos_caps::CapabilitySet;
    use rustos_drv_bus_virtio::transport_pci::common;
    use rustos_drvhost::{DriverEntry, EntryResolver, ImageSource};
    use rustos_kernel_irq::{IrqTable, IrqWaitAbort, IrqWaiter};
    use rustos_kernel_mem::{
        bootinfo::{BootMemoryMap, MemoryRegion, RegionKind},
        AddressSpace, FrameAllocator, HostPageTable, MmioMap, PhysAddr, SimPhysMap, VirtAddr,
        PAGE_SIZE,
    };
    use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
    use rustos_kernel_sec::identity::UserId;
    use rustos_log::{Event, Sink};

    const VIRTIO_BLK_DEVICE_ID: u16 = 0x1042;
    const TARGET_BDF: u64 = 0x0000_0800;
    const NOTIFY_MULTIPLIER: u32 = 4;
    const OWNER: TaskId = TaskId(77);

    /// MSI-X table entry + message the arch layer hands the boot wiring
    /// to route the device's interrupt. The address/data encoding is
    /// opaque here (the arch encoding is tested in `rustos-arch-x86_64`);
    /// the wiring test only asserts the pair reaches `route_msix`.
    const MSIX_ENTRY: u16 = 0;
    const MSI_MESSAGE: MsiMessage = MsiMessage {
        address: 0xFEE0_0000,
        data: 0x0000_0030,
    };

    /// Deterministic signing seed so the trust anchor is stable.
    const SEED: [u8; 32] = [7u8; 32];

    /// Synthetic physical base of the device's register block; the
    /// `SimPhysMap` backing the MMIO map covers `[BASE, BASE + 0x4000)`.
    const MMIO_PHYS_BASE: u64 = 0xFEBD_0000;

    /// Records whether the loaded driver observed a working virtio host
    /// and allocated a zeroed DMA slab through it.
    static DMA_OK: AtomicBool = AtomicBool::new(false);
    /// Length of the slab the driver allocated, for the test to assert.
    static DMA_LEN: AtomicUsize = AtomicUsize::new(0);

    /// Physical base + length the device advertises for each virtio
    /// configuration structure, all inside the `SimPhysMap` window.
    fn window(cfg_type: u8) -> Option<(u64, usize)> {
        match cfg_type {
            VIRTIO_PCI_CFG_COMMON => Some((MMIO_PHYS_BASE, common::CFG_LEN)),
            VIRTIO_PCI_CFG_NOTIFY => Some((MMIO_PHYS_BASE + 0x1000, 0x10)),
            VIRTIO_PCI_CFG_ISR => Some((MMIO_PHYS_BASE + 0x2000, 0x4)),
            VIRTIO_PCI_CFG_DEVICE => Some((MMIO_PHYS_BASE + 0x3000, 0x8)),
            _ => None,
        }
    }

    /// Mock virtio-PCI bus backed by a `SimPhysMap`: enumerates a single
    /// virtio-blk function and resolves each register window through the
    /// supplied kernel mapper.
    struct SimBus {
        devices: Vec<BusDevice>,
        /// Whether [`MsixBus::route_msix`] should succeed.
        route_ok: bool,
        /// `(bdf, entry, message)` recorded by the last successful
        /// [`MsixBus::route_msix`] call, for the test to assert the
        /// boot wiring routed the device's interrupt.
        routed: Cell<Option<(u64, u16, MsiMessage)>>,
    }

    impl SimBus {
        fn new(devices: Vec<BusDevice>) -> Self {
            Self {
                devices,
                route_ok: true,
                routed: Cell::new(None),
            }
        }
    }

    impl Bus for SimBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.len() < self.devices.len() {
                return Err(DriverError::BufferTooSmall);
            }
            out[..self.devices.len()].copy_from_slice(&self.devices);
            Ok(self.devices.len())
        }
    }

    impl VirtioPciBus for SimBus {
        fn map_virtio_window(
            &self,
            _bdf: u64,
            cfg_type: u8,
            mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            let (phys, len) = window(cfg_type).ok_or(DriverError::NotFound)?;
            mapper
                .map_window(phys, len)
                .map_err(MmioMapError::as_driver_error)
        }

        fn notify_off_multiplier(&self, _bdf: u64) -> Result<u32, DriverError> {
            Ok(NOTIFY_MULTIPLIER)
        }
    }

    impl MsixBus for SimBus {
        fn route_msix(
            &self,
            bdf: u64,
            entry: u16,
            message: MsiMessage,
            _mapper: &dyn MmioMapper,
        ) -> Result<(), DriverError> {
            if !self.route_ok {
                return Err(DriverError::PermissionDenied);
            }
            self.routed.set(Some((bdf, entry, message)));
            Ok(())
        }
    }

    /// Idle waiter: the wiring test never parks on `notify_wait`.
    struct IdleWaiter;
    impl IrqWaiter for IdleWaiter {
        fn now_ns(&self) -> u64 {
            0
        }
        fn yield_now(&self) -> Result<(), IrqWaitAbort> {
            Ok(())
        }
    }

    /// No-op audit sink: the wiring tests assert behaviour through the
    /// returned values and the DMA latch, not the audit stream.
    struct Recorder;
    impl Recorder {
        fn new() -> Self {
            Self
        }
    }
    impl Sink for Recorder {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    /// In-memory `.rxe` source returning a single baked image.
    struct OneImage {
        image: Vec<u8>,
    }
    impl ImageSource for OneImage {
        fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
            buf.extend_from_slice(&self.image);
            Ok(())
        }
    }

    /// Resolver binding every manifest to [`virtio_register`].
    struct ToVirtioRegister;
    impl EntryResolver for ToVirtioRegister {
        fn resolve(&self, _manifest: &DriverManifest, _payload: &[u8]) -> Option<DriverEntry> {
            Some(virtio_register as DriverEntry)
        }
    }

    /// Driver entry point: pull the minted virtio host out of the view
    /// and allocate a zeroed DMA slab through it, proving the factory
    /// was wired into the live host.
    fn virtio_register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
        let vh = host.virtio_host().ok_or(DriverError::Unsupported)?;
        let slab = vh.alloc_dma_zeroed(PAGE_SIZE)?;
        if slab.as_bytes().iter().all(|b| *b == 0) {
            DMA_OK.store(true, Ordering::SeqCst);
            DMA_LEN.store(slab.len(), Ordering::SeqCst);
        }
        DriverHandle::from_raw(0x00C0_FFEE)
    }

    /// Build a signed `.rxe` image requesting `caps`, matching the
    /// verifier in `drvhost::host`.
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
            reserved0: 0,
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

    #[test]
    fn provisions_transport_and_loads_dma_capable_driver() {
        DMA_OK.store(false, Ordering::SeqCst);
        DMA_LEN.store(0, Ordering::SeqCst);

        // --- MMIO side: SimPhysMap-backed map for the register windows.
        let mmio_sim = SimPhysMap::new(PhysAddr::new(MMIO_PHYS_BASE), 0x4000);
        let mut mmio = MmioMap::new(
            AddressSpace::new(HostPageTable::new()),
            VirtAddr::new(0x6000_0000),
            32,
            &mmio_sim,
        )
        .expect("mmio map constructs");

        // --- DMA side: frames + their direct map.
        let dma_pages = 32;
        let frames = FrameAllocator::new(&usable_map(dma_pages)).expect("frames");
        let dma_sim = SimPhysMap::new(PhysAddr::new(PAGE_SIZE as u64 * 16), dma_pages * PAGE_SIZE);

        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA], &sink);
        let irq = IrqTable::new(31);
        let irq_handle = irq.bind(7, OWNER).expect("bind device line").handle;
        let waiter = IdleWaiter;

        // --- drvhost trust + image.
        let signing_key = SigningKey::from_bytes(&SEED);
        let pubkey = pubkey_of(&signing_key);
        let trusted = [pubkey];
        let syscall_hash = [0x5Au8; 32];
        let image = build_signed_image(&signing_key, syscall_hash, &[CapabilityId::MEM_DMA]);
        let source = OneImage { image };
        let resolver = ToVirtioRegister;

        let bus = SimBus::new(alloc::vec![
            dev(0x8086, 0x29C0, 0x0000_0000),
            dev(VIRTIO_PCI_VENDOR_ID, VIRTIO_BLK_DEVICE_ID, TARGET_BDF),
        ]);

        let config = VirtioBootConfig {
            bus: &bus,
            msix: &bus,
            device_id: VIRTIO_BLK_DEVICE_ID,
            mmio: &mut mmio,
            frames: &frames,
            phys: &dma_sim,
            caller: &caller,
            audit: &sink,
            irq: &irq,
            irq_handle,
            msix_entry: MSIX_ENTRY,
            msi_message: MSI_MESSAGE,
            waiter: &waiter,
            pool_base: VirtAddr::new(0x2000_0000),
            pool_pages: 16,
            trusted_signers: &trusted,
            syscall_table_hash: syscall_hash,
            accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
            source: &source,
            resolver: &resolver,
        };

        let mut caller_caps = CapabilitySet::empty();
        caller_caps.insert(CapabilityId::DRV_LOAD);
        caller_caps.insert(CapabilityId::MEM_DMA);

        let multiplier = provision_and_run(config, HostPageTable::new, |host, transport| {
            assert_eq!(transport.windows().notify_off_multiplier, NOTIFY_MULTIPLIER);
            host.load("/System/Drivers/virtio-blk.rxe", &caller_caps)
                .expect("driver loads");
            assert_eq!(host.loaded_count(), 1);
            transport.windows().notify_off_multiplier
        })
        .expect("provisioning succeeds");

        assert_eq!(multiplier, NOTIFY_MULTIPLIER);
        assert!(
            DMA_OK.load(Ordering::SeqCst),
            "driver allocated a zeroed slab"
        );
        assert_eq!(DMA_LEN.load(Ordering::SeqCst), PAGE_SIZE);
        // The MMIO mapper handed out exactly the four virtio windows.
        assert_eq!(mmio.live(), 4);
        // The boot wiring routed the device's MSI-X interrupt for the
        // located function with the arch-supplied entry + message.
        assert_eq!(
            bus.routed.get(),
            Some((TARGET_BDF, MSIX_ENTRY, MSI_MESSAGE))
        );
    }

    #[test]
    fn missing_device_fails_closed_without_building_host() {
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
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA], &sink);
        let irq = IrqTable::new(31);
        let irq_handle = irq.bind(7, OWNER).expect("bind").handle;
        let waiter = IdleWaiter;
        let signing_key = SigningKey::from_bytes(&SEED);
        let trusted = [pubkey_of(&signing_key)];
        let source = OneImage { image: Vec::new() };
        let resolver = ToVirtioRegister;

        // Bus with no virtio function present.
        let bus = SimBus::new(alloc::vec![dev(0x8086, 0x29C0, 0)]);

        let config = VirtioBootConfig {
            bus: &bus,
            msix: &bus,
            device_id: VIRTIO_BLK_DEVICE_ID,
            mmio: &mut mmio,
            frames: &frames,
            phys: &dma_sim,
            caller: &caller,
            audit: &sink,
            irq: &irq,
            irq_handle,
            msix_entry: MSIX_ENTRY,
            msi_message: MSI_MESSAGE,
            waiter: &waiter,
            pool_base: VirtAddr::new(0x2000_0000),
            pool_pages: 16,
            trusted_signers: &trusted,
            syscall_table_hash: [0x5Au8; 32],
            accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
            source: &source,
            resolver: &resolver,
        };

        let err = provision_and_run(config, HostPageTable::new, |_host, _t| ())
            .expect_err("no virtio function");
        assert_eq!(err, VirtioPciWalkError::NoVirtioFunction);
        // Nothing was mapped: the walk failed before any window request.
        assert_eq!(mmio.live(), 0);
        // The interrupt was never routed for a device that was not found.
        assert_eq!(bus.routed.get(), None);
    }

    #[test]
    fn msix_routing_failure_fails_closed_after_windows_map() {
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
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA], &sink);
        let irq = IrqTable::new(31);
        let irq_handle = irq.bind(7, OWNER).expect("bind").handle;
        let waiter = IdleWaiter;
        let signing_key = SigningKey::from_bytes(&SEED);
        let trusted = [pubkey_of(&signing_key)];
        let source = OneImage { image: Vec::new() };
        let resolver = ToVirtioRegister;

        // The device is present, but its MSI-X routing is refused.
        let mut bus = SimBus::new(alloc::vec![
            dev(0x8086, 0x29C0, 0x0000_0000),
            dev(VIRTIO_PCI_VENDOR_ID, VIRTIO_BLK_DEVICE_ID, TARGET_BDF),
        ]);
        bus.route_ok = false;

        let config = VirtioBootConfig {
            bus: &bus,
            msix: &bus,
            device_id: VIRTIO_BLK_DEVICE_ID,
            mmio: &mut mmio,
            frames: &frames,
            phys: &dma_sim,
            caller: &caller,
            audit: &sink,
            irq: &irq,
            irq_handle,
            msix_entry: MSIX_ENTRY,
            msi_message: MSI_MESSAGE,
            waiter: &waiter,
            pool_base: VirtAddr::new(0x2000_0000),
            pool_pages: 16,
            trusted_signers: &trusted,
            syscall_table_hash: [0x5Au8; 32],
            accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
            source: &source,
            resolver: &resolver,
        };

        let err = provision_and_run(config, HostPageTable::new, |_host, _t| ())
            .expect_err("routing refused");
        assert_eq!(
            err,
            VirtioPciWalkError::RouteMsix(DriverError::PermissionDenied)
        );
        // The transport's four windows were mapped before routing was
        // attempted; the `body` closure (which loads the driver) is
        // never reached once routing fails.
        assert_eq!(mmio.live(), 4);
    }

    fn dev(vendor: u16, device: u16, address: u64) -> BusDevice {
        BusDevice {
            vendor: u32::from(vendor),
            device: u32::from(device),
            class: 0x0100,
            reserved0: 0,
            address,
        }
    }

    fn pubkey_of(sk: &SigningKey) -> Ed25519PublicKey {
        Ed25519PublicKey::from_bytes(&sk.verifying_key().to_bytes()).expect("valid key")
    }
}
