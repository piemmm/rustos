//! Freestanding (`x86_64-unknown-none`) implementation of the shared
//! virtio QEMU bring-up scaffolding. See the crate-level docs.

extern crate alloc;

use alloc::vec::Vec;

use rustos_abi::{CapabilityId, Errno, IrqHandle};
use rustos_arch_x86_64::irq::{global_routing, msi_message};
use rustos_arch_x86_64::qemu_exit;
use rustos_arch_x86_64::smp::bsp_lapic_id;
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_drv_bus_pci::x86_mechanism_one;
use rustos_drv_bus_virtio::{KernelMmioMapper, KernelVirtioHost, PciTransport, PoolId, VirtioHost};
use rustos_drvhost::{EntryResolver, Host, HostConfig, ImageSource};
use rustos_kernel::arch_wrapper::{published_irq_table, published_memory_map};
use rustos_kernel::SERIAL_SINK;
use rustos_kernel::{provision_virtio_pci, KernelVirtioFactory, KernelVirtioFactoryConfig};
use rustos_kernel_irq::{IrqTable, IrqWaitAbort, IrqWaiter};
use rustos_kernel_mem::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
use rustos_kernel_mem::{
    AddressSpace, DirectPhysMap, DmaPool, FrameAllocator, HostPageTable, MmioMap, PhysAddr,
    PhysMap, VirtAddr, PAGE_SIZE,
};
use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
use rustos_kernel_sec::identity::UserId;
use rustos_log::{Event, EventId, Level, Sink};

// Re-exports the `define_boot_harness!` macro expands against via
// `$crate::...`, so a consumer needs only this one dependency.
#[doc(hidden)]
pub use rustos_kernel::{boot, handle_panic_via_kernel_core};
#[doc(hidden)]
pub use rustos_kernel::{SerialSink as HarnessSerialSink, SERIAL_SINK as HARNESS_SERIAL_SINK};
#[doc(hidden)]
pub use rustos_log::{Event as HarnessEvent, EventId as HarnessEventId, Sink as HarnessSink};

use rustos_kernel::bumpalloc::{Heap, HEAP_BYTES};
use rustos_kernel::BumpAllocator;

// --- Bump-allocator-backed `#[global_allocator]` ---------------------

/// Static heap for the bump allocator. Mirrors the production binary.
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by the module-private `HEAP` static, shared by
/// every virtio QEMU test that links this crate.
///
/// SAFETY: the `HEAP` static outlives the binary; the allocator is the
/// only consumer. Identical justification to the other QEMU test bins.
#[global_allocator]
static ALLOCATOR: BumpAllocator =
    unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

// --- Stable identifiers ----------------------------------------------

/// `EventId(4004)` — `AuditEvent::BootCompleted`. Exposed so the
/// `define_boot_harness!` macro and consumers agree on the trigger.
pub const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

/// Milestone event id namespace for the shared serial breadcrumbs.
const MILESTONE_ID: EventId = EventId(9100);

// --- Bring-up parameters ---------------------------------------------

/// IO-APIC GSI bound in the `IrqTable`. The boot pipeline left every pin
/// masked; we never unmask it, so no IO-APIC delivery races the MSI-X
/// path. We only reuse the vector the boot pipeline assigned to this GSI
/// so the MSI-X interrupt resolves back to this binding through
/// `global_routing().vector_for_gsi`.
const DEVICE_GSI: u32 = 16;

/// MSI-X table entry the device's vector is programmed into. Every queue
/// shares it (see [`PciTransport::enable_msix`]), so a single bound
/// `IrqHandle` covers a multi-queue device (e.g. virtio-net rx + tx).
const MSIX_ENTRY: u16 = 0;

/// Upper bound of the boot `0..4 GiB` identity map.
const IDENTITY_LIMIT: u64 = 0x1_0000_0000;

/// Synthetic owner task id for the bus-driver context.
const TASK: TaskId = TaskId(0x5b1);

/// Capacity, in pages, of each per-device DMA window.
const POOL_PAGES: usize = 64;

/// Pages carved from high RAM for the per-device DMA allocator. Covers
/// the direct-driving pool plus the transient pool the `.rxe` load mints,
/// with slack.
const CARVE_PAGES: usize = 256;

/// Base virtual address of each minted DMA window (bookkeeping only;
/// access is through the identity map).
const POOL_VBASE: u64 = 0x2000_0000;

/// Base virtual address of the MMIO register-window map.
const MMIO_VBASE: u64 = 0x6000_0000;

/// Capacity, in pages, of the MMIO register-window map.
const MMIO_CAP_PAGES: usize = 64;

/// Fixed driver path fed to `Host::load`. The image bytes come from the
/// in-memory [`BakedSource`] regardless of path, so the concrete string
/// only has to be well-formed.
const DRIVER_PATH: &str = "/System/Drivers/driver.rxe";

// --- Serial breadcrumbs ----------------------------------------------

/// Emit an info-level milestone breadcrumb on the serial sink.
pub fn log(msg: &str) {
    SERIAL_SINK.write_event(&Event {
        level: Level::Info,
        id: MILESTONE_ID,
        message: msg,
        fields: &[],
    });
}

/// Log `msg` and flip QEMU's debug-exit device to failure. Never returns.
pub fn fail(msg: &str) -> ! {
    log(msg);
    qemu_exit::exit_failure()
}

// --- Memory-map carve ------------------------------------------------

/// Carve the top `pages` of the highest identity-mapped Usable region
/// into a single-region [`BootMemoryMap`] for the per-device DMA
/// allocator.
///
/// The carved sub-region sits at the top of RAM, away from the low
/// frames the boot pipeline and kernel heap consume, so the per-device
/// [`FrameAllocator`] never hands out a frame the live kernel is using.
/// It is bounded below `IDENTITY_LIMIT` so every frame it yields is
/// reachable through the [`DirectPhysMap`] identity map.
fn carve_dma_map(src: &BootMemoryMap, pages: usize) -> Option<BootMemoryMap> {
    let need = (pages as u64).checked_mul(PAGE_SIZE as u64)?;
    let mut best_end: Option<u64> = None;
    for r in src.regions() {
        if r.kind != RegionKind::Usable {
            continue;
        }
        let end = r.end()?.as_u64();
        let start = r.start.as_u64();
        if end > IDENTITY_LIMIT {
            continue;
        }
        if end.saturating_sub(start) < need {
            continue;
        }
        best_end = Some(best_end.map_or(end, |b| b.max(end)));
    }
    let end = best_end?;
    let carve_end = end & !(PAGE_SIZE as u64 - 1);
    let carve_start = carve_end.checked_sub(need)?;
    let mut m = BootMemoryMap::new();
    m.push(MemoryRegion {
        kind: RegionKind::Usable,
        start: PhysAddr::new(carve_start),
        length: need,
    });
    Some(m)
}

// --- IRQ waiter ------------------------------------------------------

/// [`IrqWaiter`] that parks the CPU on `hlt` between readiness checks.
struct HltWaiter;
impl IrqWaiter for HltWaiter {
    fn now_ns(&self) -> u64 {
        rdtsc()
    }

    fn yield_now(&self) -> Result<(), IrqWaitAbort> {
        // Park with the canonical race-free `sti; hlt` idiom, then
        // disable interrupts again on wake. The completion path is
        // lock-free on the IRQ side — `IrqTable::fire` and
        // `try_wait_step` synchronise only through per-line atomics
        // (`bound`/`ready`), so the ISR can never deadlock a parked
        // waiter on a shared `IrqTable` lock. Interrupts are nonetheless
        // kept *disabled* in task context and re-enabled only across this
        // halt: on a single CPU that confines every delivery (the routed
        // MSI-X completion vector and the periodic LAPIC timer) to a
        // well-defined point where the task holds no lock, and makes the
        // wake-up deterministic. x86 guarantees an interrupt pending at
        // `sti` is not taken until after the following `hlt` is entered,
        // so no wake-up is lost between the two; the periodic LAPIC timer
        // additionally bounds every park so a missed edge still re-checks
        // readiness.
        //
        // SAFETY: `sti`/`hlt`/`cli` are privileged but well-defined in
        // ring 0; the IDT is fully populated by the boot pipeline and the
        // only unmasked external source is the routed MSI-X vector (plus
        // the periodic timer). `preserves_flags` is intentionally omitted
        // because `sti`/`cli` modify `IF`.
        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
        Ok(())
    }
}

/// Disable maskable interrupts on the current CPU.
///
/// The boot pipeline runs with `IF` clear and never `sti`s, so interrupts
/// are already disabled when the `BootCompleted` observer hijacks this
/// CPU; this call is the defensive re-assertion. The scenario keeps
/// interrupts disabled in task context so that on a single CPU every
/// delivery is confined to the waiter's `hlt` park — when the task holds
/// no lock — and re-enables them only there (see [`HltWaiter::yield_now`]).
///
/// # Safety
///
/// `cli` is privileged but well-defined in ring 0 and clears only `IF`.
/// The scenario re-enables interrupts inside the park, so the device's
/// completion interrupt is still delivered.
unsafe fn cli() {
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
}

/// Read the timestamp counter (used only as a monotonic-ish clock for the
/// wait loop; the host waits with an unbounded deadline so the exact
/// frequency is immaterial).
fn rdtsc() -> u64 {
    // SAFETY: `rdtsc` is unprivileged on every x86_64 CPU RustOS supports
    // and has no architectural side effects.
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

// --- Signed `.rxe` load ----------------------------------------------

/// Image source returning the baked-in signed `.rxe` bytes regardless of
/// the requested path.
struct BakedSource<'a> {
    bytes: &'a [u8],
}
impl ImageSource for BakedSource<'_> {
    fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
        buf.extend_from_slice(self.bytes);
        Ok(())
    }
}

/// Per-vertical configuration for [`run_virtio_scenario`].
pub struct ScenarioConfig<'a> {
    /// Modern virtio PCI device id (`0x1040 + virtio device type`).
    pub device_id: u16,
    /// Signed `.rxe` image bytes for the vertical's driver.
    pub rxe_image: &'a [u8],
    /// Trust-anchor public key the `HostConfig` accepts.
    pub trusted_pubkey: [u8; 32],
    /// SHA-256 fingerprint of the host's syscall table.
    pub syscall_table_hash: [u8; 32],
    /// Resolver binding the verified manifest to the driver's `register`.
    pub resolver: &'a dyn EntryResolver,
    /// Breadcrumb logged at scenario start.
    pub start_msg: &'a str,
}

/// Load the signed `.rxe` through `rustos_drvhost::Host`, exercising
/// signature verification, capability gating, and the kernel virtio-host
/// factory. Any failure flips `qemu_exit::exit_failure`.
#[allow(clippy::too_many_arguments)]
fn load_signed_rxe(
    cfg: &ScenarioConfig<'_>,
    frames: &FrameAllocator,
    phys: &dyn PhysMap,
    caller: &TaskCapabilities,
    irq: &IrqTable,
    handle: IrqHandle,
    waiter: &dyn IrqWaiter,
) {
    let Ok(pubkey) = Ed25519PublicKey::from_bytes(&cfg.trusted_pubkey) else {
        fail("trust anchor decode");
    };
    let trusted = [pubkey];
    let mut load_caps = CapabilitySet::empty();
    load_caps.insert(CapabilityId::DRV_LOAD);
    load_caps.insert(CapabilityId::MEM_DMA);

    let source = BakedSource {
        bytes: cfg.rxe_image,
    };
    let factory = KernelVirtioFactory::new(
        KernelVirtioFactoryConfig {
            frames,
            phys,
            caller,
            audit: &SERIAL_SINK,
            irq,
            irq_handle: handle,
            waiter,
            pool_base: VirtAddr::new(POOL_VBASE),
            pool_pages: POOL_PAGES,
        },
        HostPageTable::new,
    );
    let mut host = Host::new(HostConfig {
        trusted_signers: &trusted,
        syscall_table_hash: cfg.syscall_table_hash,
        accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
        source: &source,
        resolver: cfg.resolver,
        sink: &SERIAL_SINK,
        virtio_host_factory: Some(&factory),
    });
    if host.load(DRIVER_PATH, &load_caps).is_err() {
        fail("signed .rxe load");
    }
    if host.loaded_count() != 1 {
        fail("unexpected loaded driver count");
    }
}

// --- Shared scenario -------------------------------------------------

/// Perform the device-agnostic virtio bring-up, then hand the
/// provisioned [`PciTransport`] and [`VirtioHost`] to `body`, which opens
/// the concrete driver and exercises the device. `body` returns `Ok(())`
/// on success or `Err(msg)` to flip QEMU failure with a breadcrumb; the
/// scenario flips success only when `body` returns `Ok`. Never returns.
///
/// MSI-X is enabled on the transport *before* `body` runs so the per-queue
/// `queue_msix_vector` is programmed during the driver's `open`-time queue
/// set-up (see [`PciTransport::enable_msix`]).
pub fn run_virtio_scenario<F>(cfg: &ScenarioConfig<'_>, body: F) -> !
where
    F: FnOnce(PciTransport, &dyn VirtioHost) -> Result<(), &'static str>,
{
    use rustos_abi::driver::msix::MsixBus;

    log(cfg.start_msg);

    // Disable interrupts for the whole scenario; the waiter re-enables
    // them only across its `hlt` park, confining single-CPU delivery to a
    // point where the task holds no lock.
    // SAFETY: see `cli`.
    unsafe { cli() };

    let Some(table) = published_irq_table() else {
        fail("no published IrqTable");
    };
    let Some(memmap) = published_memory_map() else {
        fail("no published memory map");
    };

    // 1. Per-device DMA: carve high frames + the boot identity map.
    let Some(dma_map) = carve_dma_map(memmap, CARVE_PAGES) else {
        fail("no carveable DMA region");
    };
    let Ok(frames) = FrameAllocator::new(&dma_map) else {
        fail("frame allocator build");
    };
    let phys = DirectPhysMap::identity(IDENTITY_LIMIT);

    // 2. Bus-driver task capability context.
    let mut grants = CapabilitySet::empty();
    grants.insert(CapabilityId::MMIO_MAP);
    grants.insert(CapabilityId::MEM_DMA);
    grants.insert(CapabilityId::DRV_LOAD);
    let caller = TaskCapabilities::derive(TASK, UserId(0), grants, grants, &SERIAL_SINK);

    // 3. Bind the device's (masked) GSI and build its MSI message from the
    //    vector the boot pipeline assigned to that GSI.
    let Ok(bind) = table.bind(DEVICE_GSI, TASK) else {
        fail("bind device GSI");
    };
    let handle = bind.handle;
    let Some(vector) = global_routing().vector_for_gsi(DEVICE_GSI) else {
        fail("no vector for GSI");
    };
    let msi = msi_message(vector, bsp_lapic_id());

    // 4. Walk PCI, map the four virtio register windows, route MSI-X.
    let bus = x86_mechanism_one();
    let Ok(mut mmio) = MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(MMIO_VBASE),
        MMIO_CAP_PAGES,
        &phys,
    ) else {
        fail("MMIO map construct");
    };
    let mut transport = {
        let mapper = KernelMmioMapper::new(&mut mmio, &caller, &SERIAL_SINK);
        let Ok(prov) = provision_virtio_pci(&bus, cfg.device_id, &mapper) else {
            fail("virtio-PCI provisioning walk");
        };
        if bus.route_msix(prov.bdf, MSIX_ENTRY, msi, &mapper).is_err() {
            fail("route MSI-X");
        }
        prov.transport
    };
    log("virtio-qemu: transport provisioned, MSI-X routed");

    // 5. Mint the per-device DMA host the driver allocates through.
    let space = AddressSpace::new(HostPageTable::new());
    let Ok(pool) = DmaPool::new(space, VirtAddr::new(POOL_VBASE), POOL_PAGES, &frames, &phys)
    else {
        fail("DMA pool construct");
    };
    let waiter = HltWaiter;
    let vhost = KernelVirtioHost::new(
        pool,
        &caller,
        &SERIAL_SINK,
        PoolId::fresh(),
        table,
        handle,
        &waiter,
    );

    // 6. Load the signed `.rxe` (signature + factory path).
    load_signed_rxe(cfg, &frames, &phys, &caller, table, handle, &waiter);
    log("virtio-qemu: signed .rxe loaded");

    // 7. Enable MSI-X on every queue, then drive the device. Interrupts
    //    stay disabled in task context and are enabled only inside the
    //    waiter's `sti; hlt` park (see `HltWaiter::yield_now`).
    transport.enable_msix(MSIX_ENTRY);

    match body(transport, &vhost) {
        Ok(()) => qemu_exit::exit_success(),
        Err(msg) => fail(msg),
    }
}

// --- Boot harness ----------------------------------------------------

/// Generate the freestanding boot harness for a virtio QEMU test bin: the
/// audit-observer `Sink` that drives `$scenario` once on `BootCompleted`,
/// the `#[panic_handler]` bridge, and the `kernel_main` entry point that
/// runs the production boot pipeline with the observer installed.
///
/// `$scenario` must be a `fn() -> !` (typically a thin wrapper around
/// [`run_virtio_scenario`]). Invoke exactly once at the crate root of the
/// freestanding bin.
#[macro_export]
macro_rules! define_boot_harness {
    ($scenario:path) => {
        /// Latch so the scenario runs exactly once.
        static SCENARIO_RAN: ::core::sync::atomic::AtomicBool =
            ::core::sync::atomic::AtomicBool::new(false);

        /// Audit observer: forwards every event to the serial sink and,
        /// on `BootCompleted`, drives the scenario exactly once.
        struct BootObserverSink;
        impl $crate::HarnessSink for BootObserverSink {
            fn write_event(&self, event: &$crate::HarnessEvent<'_>) {
                $crate::HarnessSerialSink::new().write_event(event);
                if event.id == $crate::BOOT_COMPLETED_EVENT_ID
                    && !SCENARIO_RAN.swap(true, ::core::sync::atomic::Ordering::SeqCst)
                {
                    $scenario();
                }
            }
        }

        static AUDIT_SINK: BootObserverSink = BootObserverSink;

        /// Forward to the shared panic bridge in `rustos_kernel`.
        #[panic_handler]
        fn virtio_qemu_panic(info: &::core::panic::PanicInfo<'_>) -> ! {
            $crate::handle_panic_via_kernel_core(info)
        }

        /// Boot entry point — the production `rustos-kernel` surface with
        /// the audit observer sink in place.
        #[no_mangle]
        pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
            $crate::boot(multiboot_info, &$crate::HARNESS_SERIAL_SINK, &AUDIT_SINK)
        }
    };
}
