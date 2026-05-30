//! Freestanding (`x86_64-unknown-none`) half of the virtio-blk-pci
//! integration test.
//!
//! The production `rustos-kernel` boot pipeline runs until
//! `AuditEvent::BootCompleted` (`EventId(4004)`). The audit Sink that
//! observes the event hijacks the boot CPU before `kernel_main`'s
//! trailing halt and drives a complete virtio-blk round-trip:
//!
//! 1. Carve a high, identity-mapped Usable sub-region from the
//!    published firmware memory map into a per-device
//!    [`FrameAllocator`], and build a [`DirectPhysMap`] over the boot
//!    `0..4 GiB` identity map.
//! 2. Walk PCI through the real-hardware bus (`x86_mechanism_one`),
//!    map the modern virtio-blk function's four register windows
//!    through the `CAP_MMIO_MAP`-gated [`KernelMmioMapper`], and build
//!    a [`PciTransport`].
//! 3. Bind a (masked) IO-APIC GSI in the published [`IrqTable`], reuse
//!    the boot-assigned vector for that GSI to build the device's MSI
//!    message, and route it into the function's MSI-X table.
//! 4. Mint a [`KernelVirtioHost`] over a per-device [`DmaPool`] drawn
//!    from the carved allocator.
//! 5. Load the signed virtio-blk `.rxe` through `rustos_drvhost::Host`
//!    (exercising signature + capability gating and the kernel virtio
//!    factory).
//! 6. Enable interrupts and drive [`VirtioBlk`]: read sector 0 and
//!    verify the harness-planted pattern, write a known pattern to
//!    sector 1, read it back, and verify it round-tripped.
//!
//! Any deviation flips [`qemu_exit::exit_failure`]; only the fully
//! successful path reaches [`qemu_exit::exit_success`].

extern crate alloc;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use alloc::vec::Vec;

use rustos_abi::driver::block::Block;
use rustos_abi::{CapabilityId, DriverManifest, Errno};
use rustos_arch_x86_64::irq::{global_routing, msi_message};
use rustos_arch_x86_64::qemu_exit;
use rustos_arch_x86_64::smp::bsp_lapic_id;
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_drv_bus_pci::x86_mechanism_one;
use rustos_drv_bus_virtio::{KernelMmioMapper, KernelVirtioHost, PoolId};
use rustos_drv_storage_virtio_blk::{self as virtio_blk, VirtioBlk};
use rustos_drvhost::{DriverEntry, EntryResolver, Host, HostConfig, ImageSource};
use rustos_kernel::arch_wrapper::{published_irq_table, published_memory_map};
use rustos_kernel::bumpalloc::{Heap, HEAP_BYTES};
use rustos_kernel::{boot, handle_panic_via_kernel_core, BumpAllocator, SerialSink, SERIAL_SINK};
use rustos_kernel::{provision_virtio_pci, KernelVirtioFactory, KernelVirtioFactoryConfig};
use rustos_kernel_irq::{IrqTable, IrqWaitAbort, IrqWaiter};
use rustos_kernel_mem::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
use rustos_kernel_mem::{
    AddressSpace, DirectPhysMap, DmaPool, FrameAllocator, HostPageTable, MmioMap, PhysAddr,
    VirtAddr, PAGE_SIZE,
};
use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
use rustos_kernel_sec::identity::UserId;
use rustos_log::{Event, EventId, Level, Sink};

use crate::fixture::{RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

// --- Bump-allocator-backed `#[global_allocator]` ---------------------

/// Static heap for the bump allocator. Mirrors the production binary.
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by the module-private `HEAP` static.
///
/// SAFETY: the `HEAP` static outlives the binary; the allocator is the
/// only consumer. Identical justification to the other QEMU test bins.
#[global_allocator]
static ALLOCATOR: BumpAllocator =
    unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

// --- Stable identifiers ----------------------------------------------

/// `EventId(4004)` — `AuditEvent::BootCompleted`.
const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

/// Latch so the scenario runs exactly once.
static SCENARIO_RAN: AtomicBool = AtomicBool::new(false);

// --- Test parameters -------------------------------------------------

/// Modern virtio-blk PCI device id (`0x1040 + virtio-blk`).
const VIRTIO_BLK_DEVICE_ID: u16 = 0x1042;

/// IO-APIC GSI bound in the `IrqTable`. The boot pipeline left every
/// pin masked; we never unmask it, so no IO-APIC delivery races the
/// MSI-X path. We only reuse the vector the boot pipeline assigned to
/// this GSI so the MSI-X interrupt resolves back to this binding
/// through `global_routing().gsi_for_vector`.
const DEVICE_GSI: u32 = 16;

/// MSI-X table entry the device's vector is programmed into.
const MSIX_ENTRY: u16 = 0;

/// Upper bound of the boot `0..4 GiB` identity map.
const IDENTITY_LIMIT: u64 = 0x1_0000_0000;

/// Synthetic owner task id for the bus-driver context.
const TASK: TaskId = TaskId(0x5b1);

/// Capacity, in pages, of each per-device DMA window.
const POOL_PAGES: usize = 64;

/// Pages carved from high RAM for the per-device DMA allocator. Covers
/// the direct-driving pool plus the transient pool the `.rxe` load
/// mints, with slack.
const CARVE_PAGES: usize = 256;

/// Base virtual address of each minted DMA window (bookkeeping only;
/// access is through the identity map).
const POOL_VBASE: u64 = 0x2000_0000;

/// Base virtual address of the MMIO register-window map.
const MMIO_VBASE: u64 = 0x6000_0000;

/// Capacity, in pages, of the MMIO register-window map.
const MMIO_CAP_PAGES: usize = 64;

/// Logical sector size.
const SECTOR_LEN: usize = 512;

/// Milestone event id namespace for this test's serial breadcrumbs.
const MILESTONE_ID: EventId = EventId(9100);

fn log(msg: &str) {
    SERIAL_SINK.write_event(&Event {
        level: Level::Info,
        id: MILESTONE_ID,
        message: msg,
        fields: &[],
    });
}

fn fail(msg: &str) -> ! {
    log(msg);
    qemu_exit::exit_failure()
}

// --- Memory-map carve ------------------------------------------------

/// Carve the top [`CARVE_PAGES`] of the highest identity-mapped Usable
/// region into a single-region [`BootMemoryMap`] for the per-device DMA
/// allocator.
///
/// The carved sub-region sits at the top of RAM, away from the low
/// frames the boot pipeline and kernel heap consume, so the per-device
/// [`FrameAllocator`] never hands out a frame the live kernel is using.
/// It is bounded below [`IDENTITY_LIMIT`] so every frame it yields is
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

// --- Sector patterns -------------------------------------------------

/// `true` if `sector` matches the pattern the host harness planted at
/// LBA 0 (`byte[i] == i mod 256`). Kept in sync with the
/// `plant_raw_disk` call in `tools/xtask/src/commands/qemu_tests.rs`.
fn sector0_matches(sector: &[u8; SECTOR_LEN]) -> bool {
    sector
        .iter()
        .enumerate()
        .all(|(i, b)| *b == (i & 0xFF) as u8)
}

/// Fill `sector` with the pattern the test writes to LBA 1
/// (`byte[i] = (i mod 256) xor 0xA5`) — distinct from the LBA-0 pattern
/// so a stale-read regression cannot pass by accident.
fn fill_sector1(sector: &mut [u8; SECTOR_LEN]) {
    for (i, b) in sector.iter_mut().enumerate() {
        *b = ((i & 0xFF) as u8) ^ 0xA5;
    }
}

// --- `.rxe` load fixtures --------------------------------------------

/// Image source returning the baked-in signed virtio-blk `.rxe`.
struct BakedSource;
impl ImageSource for BakedSource {
    fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
        buf.extend_from_slice(RXE_IMAGE);
        Ok(())
    }
}

/// Resolver binding every verified manifest to the virtio-blk driver's
/// `register` entry point.
struct ToVirtioBlk;
impl EntryResolver for ToVirtioBlk {
    fn resolve(&self, _manifest: &DriverManifest, _payload: &[u8]) -> Option<DriverEntry> {
        Some(virtio_blk::register as DriverEntry)
    }
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
        // waiter on a shared `IrqTable` lock. Interrupts are
        // nonetheless kept *disabled* in task context and re-enabled
        // only across this halt: on a single CPU that confines every
        // delivery (the routed MSI-X completion vector and the periodic
        // LAPIC timer) to a well-defined point where the task holds no
        // lock — in particular the completion ISR runs
        // `IoApicController::mask`, which takes a plain spinlock — and
        // makes the wake-up deterministic: the edge is taken inside the
        // `hlt`, and the very next loop iteration's `try_wait_step`
        // observes the `ready` flag `fire` set. x86 guarantees an
        // interrupt pending at `sti` is not taken until after the
        // following `hlt` is entered, so no wake-up is lost between the
        // two; the periodic LAPIC timer additionally bounds every park
        // so a missed edge still re-checks readiness.
        //
        // SAFETY: `sti`/`hlt`/`cli` are privileged but well-defined in
        // ring 0; the IDT is fully populated by the boot pipeline and
        // the only unmasked external source is the routed MSI-X vector
        // (plus the periodic timer). `preserves_flags` is intentionally
        // omitted because `sti`/`cli` modify `IF`.
        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
        Ok(())
    }
}

/// Disable maskable interrupts on the current CPU.
///
/// The boot pipeline runs with `IF` clear and never `sti`s (the
/// periodic LAPIC timer is armed but its ticks stay pending), so
/// interrupts are already disabled when the `BootCompleted` observer
/// hijacks this CPU; this call is the defensive re-assertion. The
/// scenario keeps interrupts disabled in task context so that on a
/// single CPU every delivery is confined to the waiter's `hlt` park —
/// when the task holds no lock — and re-enables them only there (see
/// [`HltWaiter::yield_now`]).
///
/// # Safety
///
/// `cli` is privileged but well-defined in ring 0 and clears only
/// `IF`. The scenario re-enables interrupts inside the park, so the
/// device's completion interrupt is still delivered.
unsafe fn cli() {
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
}

/// Read the timestamp counter (used only as a monotonic-ish clock for
/// the wait loop; the host waits with an unbounded deadline so the
/// exact frequency is immaterial).
fn rdtsc() -> u64 {
    // SAFETY: `rdtsc` is unprivileged on every x86_64 CPU RustOS
    // supports and has no architectural side effects.
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

/// Load the signed virtio-blk `.rxe` through `rustos_drvhost::Host`,
/// exercising signature verification, capability gating, and the kernel
/// virtio-host factory. Any failure flips `qemu_exit::exit_failure`.
fn load_signed_rxe(
    frames: &FrameAllocator,
    phys: &dyn rustos_kernel_mem::PhysMap,
    caller: &TaskCapabilities,
    irq: &IrqTable,
    handle: rustos_abi::IrqHandle,
    waiter: &dyn IrqWaiter,
) {
    let Ok(pubkey) = Ed25519PublicKey::from_bytes(&TRUSTED_SIGNER_PUBKEY) else {
        fail("trust anchor decode");
    };
    let trusted = [pubkey];
    let mut load_caps = CapabilitySet::empty();
    load_caps.insert(CapabilityId::DRV_LOAD);
    load_caps.insert(CapabilityId::MEM_DMA);

    let source = BakedSource;
    let resolver = ToVirtioBlk;
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
        syscall_table_hash: SYSCALL_TABLE_HASH,
        accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
        source: &source,
        resolver: &resolver,
        sink: &SERIAL_SINK,
        virtio_host_factory: Some(&factory),
    });
    if host
        .load("/System/Drivers/virtio-blk.rxe", &load_caps)
        .is_err()
    {
        fail("signed .rxe load");
    }
    if host.loaded_count() != 1 {
        fail("unexpected loaded driver count");
    }
}

// --- Scenario --------------------------------------------------------

/// Drive the full virtio-blk-pci round-trip and exit through QEMU's
/// debug-exit device. Never returns.
fn run_scenario() -> ! {
    use rustos_abi::driver::msix::MsixBus;

    log("virtio-blk-pci: scenario start");

    // Disable interrupts for the whole scenario; the waiter re-enables
    // them only across its `hlt` park. `IrqTable::fire`/`try_wait_step`
    // are lock-free (per-line atomics), so this is not needed to avoid
    // an IrqTable-lock deadlock; it keeps the single-CPU completion
    // wait deterministic by confining every interrupt delivery to the
    // park, where the task holds no lock.
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

    // 3. Bind the device's (masked) GSI and build its MSI message from
    //    the vector the boot pipeline assigned to that GSI.
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
    let mut mmio = match MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(MMIO_VBASE),
        MMIO_CAP_PAGES,
        &phys,
    ) {
        Ok(m) => m,
        Err(_) => fail("MMIO map construct"),
    };
    let mut transport = {
        let mapper = KernelMmioMapper::new(&mut mmio, &caller, &SERIAL_SINK);
        let prov = match provision_virtio_pci(&bus, VIRTIO_BLK_DEVICE_ID, &mapper) {
            Ok(p) => p,
            Err(_) => fail("virtio-PCI provisioning walk"),
        };
        if bus.route_msix(prov.bdf, MSIX_ENTRY, msi, &mapper).is_err() {
            fail("route MSI-X");
        }
        prov.transport
    };
    log("virtio-blk-pci: transport provisioned, MSI-X routed");

    // 5. Mint the per-device DMA host the driver allocates through.
    let space = AddressSpace::new(HostPageTable::new());
    let pool = match DmaPool::new(space, VirtAddr::new(POOL_VBASE), POOL_PAGES, &frames, &phys) {
        Ok(p) => p,
        Err(_) => fail("DMA pool construct"),
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

    // 6. Load the signed virtio-blk `.rxe` (signature + factory path).
    load_signed_rxe(&frames, &phys, &caller, table, handle, &waiter);
    log("virtio-blk-pci: signed .rxe loaded");

    // 7. Drive the device. Interrupts stay disabled in task context and
    //    are enabled only inside the waiter's `sti; hlt` park (see
    //    `HltWaiter::yield_now`): the IRQ completion path is lock-free,
    //    so confining delivery to the park is what makes the single-CPU
    //    wait deterministic — the completion edge lands at the `hlt`
    //    and the next `try_wait_step` consumes the `ready` flag.
    transport.enable_msix(MSIX_ENTRY);
    let mut blk = match VirtioBlk::open(transport, &vhost) {
        Ok(b) => b,
        Err(_) => fail("virtio-blk open"),
    };
    log("virtio-blk-pci: device online");

    // Read sector 0 and verify the harness-planted pattern.
    let mut s0 = [0u8; SECTOR_LEN];
    if blk.read_blocks(0, &mut s0).is_err() {
        fail("read sector 0");
    }
    if !sector0_matches(&s0) {
        fail("sector 0 pattern mismatch");
    }
    log("virtio-blk-pci: sector 0 verified");

    // Write a known pattern to sector 1, read it back, verify.
    let mut s1 = [0u8; SECTOR_LEN];
    fill_sector1(&mut s1);
    if blk.write_blocks(1, &s1).is_err() {
        fail("write sector 1");
    }
    let mut rb = [0u8; SECTOR_LEN];
    if blk.read_blocks(1, &mut rb).is_err() {
        fail("read-back sector 1");
    }
    if rb != s1 {
        fail("sector 1 round-trip mismatch");
    }
    log("virtio-blk-pci: sector 1 round-trip verified");

    qemu_exit::exit_success()
}

// --- Audit observer + entry point ------------------------------------

/// Audit observer sink: forwards every event to [`SerialSink`] and, on
/// `BootCompleted`, drives [`run_scenario`] exactly once.
struct BootObserverSink;
impl Sink for BootObserverSink {
    fn write_event(&self, event: &Event<'_>) {
        SerialSink::new().write_event(event);
        if event.id == BOOT_COMPLETED_EVENT_ID && !SCENARIO_RAN.swap(true, Ordering::SeqCst) {
            run_scenario();
        }
    }
}

static AUDIT_SINK: BootObserverSink = BootObserverSink;

/// Forward to the shared panic bridge in `rustos_kernel::panic_ctx`.
#[panic_handler]
fn virtio_blk_pci_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_kernel_core(info)
}

/// Boot entry point — same surface the production `rustos-kernel` bin
/// exposes, with our audit observer sink in place.
#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
    boot(multiboot_info, &SERIAL_SINK, &AUDIT_SINK)
}
