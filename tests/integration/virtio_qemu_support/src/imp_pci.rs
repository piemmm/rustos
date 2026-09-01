//! Freestanding (`x86_64-unknown-none`) virtio-PCI bring-up.
//!
//! The device-agnostic lifecycle and the per-device tails live in
//! [`crate::common`]; this module owns only the x86_64-specific bring-up
//! that produces a [`PciTransport`] and an interrupt path: the
//! `mechanism_one(x86_port_io())` PCI walk, the four virtio register-window maps
//! through the `CAP_MMIO_MAP`-gated [`KernelMmioMapper`], MSI-X routing
//! off the boot-assigned vector, and the `sti; hlt; cli` IRQ park.

use tairix_abi::CapabilityId;
use tairix_arch_x86_64::irq::{global_routing, msi_message};
use tairix_arch_x86_64::pio::x86_port_io;
use tairix_arch_x86_64::qemu_exit;
use tairix_arch_x86_64::smp::bsp_lapic_id;
use tairix_caps::CapabilitySet;
use tairix_drv_bus_virtio::PciTransport;
use tairix_kernel::x86_64::arch_wrapper::{published_irq_table, published_memory_map};
use tairix_kernel::SERIAL_SINK;
use tairix_kernel::{KernelVirtioFactory, KernelVirtioFactoryConfig};
use tairix_kernel_irq::{IrqWaitAbort, IrqWaiter};
use tairix_kernel_mem::{
    AddressSpace, DirectPhysMap, DmaPool, FrameAllocator, HostPageTable, MmioMap, VirtAddr,
};
use tairix_kernel_sec::captable::{ProcessId, TaskCapabilities};
use tairix_kernel_sec::identity::UserId;
use tairix_kernel_virtio::{KernelMmioMapper, KernelVirtioHost};
use tairix_log::{Event, EventId, Level, Sink};
use tairix_pci::mechanism_one;
use tairix_virtio::{PoolId, VirtioHost, VirtioHostFactory};

use crate::common::{
    carve_dma_map, drive_driver_lifecycle, QemuEnv, ScenarioConfig, IDENTITY_LIMIT,
};

/// Re-export so the verticals can name the concrete transport for the
/// shared device-tail turbofish without their own `tairix-drv-bus-virtio`
/// dependency.
pub use tairix_drv_bus_virtio::PciTransport as ScenarioTransport;

// Re-exports the `define_boot_harness!` macro expands against via
// `$crate::...`, so a consumer needs only this one dependency.
// (`BOOT_COMPLETED_EVENT_ID` is re-exported at the crate root through
// `pub use common::*`.)
#[doc(hidden)]
pub use tairix_kernel::{boot, handle_panic_via_kernel_core};
#[doc(hidden)]
pub use tairix_kernel::{SerialSink as HarnessSerialSink, SERIAL_SINK as HARNESS_SERIAL_SINK};
#[doc(hidden)]
pub use tairix_log::{
    Event as HarnessEvent, EventId as HarnessEventId, Level as HarnessLevel, Sink as HarnessSink,
};

use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
use tairix_kernel::FreeListAllocator;

// --- Bump-allocator-backed `#[global_allocator]` ---------------------

/// Static heap for the bump allocator. Mirrors the production binary.
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by the module-private `HEAP` static, shared by
/// every x86_64 virtio QEMU test that links this crate.
///
/// Public so the `define_boot_harness!` entry point can hand it to the boot
/// pipeline, which wires the growable-heap source into it.
///
/// SAFETY: the `HEAP` static outlives the binary; the allocator is the
/// only consumer. Identical justification to the other QEMU test bins.
#[global_allocator]
pub static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

// --- Stable identifiers ----------------------------------------------

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

/// Synthetic owner process id for the bus-driver context.
const TASK: ProcessId = ProcessId(0x5b1);

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

// --- QEMU environment ------------------------------------------------

/// x86_64 [`QemuEnv`]: serial breadcrumbs over the kernel COM1 sink, exit
/// through the `isa-debug-exit` device.
struct PciEnv;

impl QemuEnv for PciEnv {
    fn log(&self, msg: &str) {
        SERIAL_SINK.write_event(&Event {
            level: Level::Info,
            id: MILESTONE_ID,
            message: msg,
            fields: &[],
        });
    }

    fn fail(&self, msg: &str) -> ! {
        self.log(msg);
        qemu_exit::exit_failure()
    }

    fn succeed(&self) -> ! {
        qemu_exit::exit_success()
    }

    fn audit_sink(&self) -> &'static dyn Sink {
        &SERIAL_SINK
    }
}

// --- IRQ waiter ------------------------------------------------------

/// [`IrqWaiter`] that parks the CPU on `hlt` between readiness checks.
struct HltWaiter;
impl IrqWaiter for HltWaiter {
    fn now_ns(&self) -> u64 {
        rdtsc()
    }

    fn yield_now(&self, _deadline_ns: u64) -> Result<(), IrqWaitAbort> {
        // Park with the canonical race-free `sti; hlt` idiom, then
        // disable interrupts again on wake. The completion path is
        // lock-free on the IRQ side, so the ISR can never deadlock a
        // parked waiter; the periodic LAPIC timer bounds every park so a
        // missed edge still re-checks readiness.
        //
        // The caller's deadline needs no timer of its own here: the park
        // ends on the next interrupt whatever happens, so control always
        // returns to the wait loop, which compares the clock against that
        // deadline itself. A park that could sleep past the deadline would
        // have to register it instead.
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

/// Read the timestamp counter (monotonic-ish clock for the wait loop).
fn rdtsc() -> u64 {
    // SAFETY: `rdtsc` is unprivileged on every x86_64 CPU TAIRiX supports
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

// --- Shared scenario -------------------------------------------------

/// Perform the x86_64 virtio-PCI bring-up for the modern function whose
/// PCI device id is `device_id` (`0x1040 + virtio type`), then drive the
/// shared `load → reload → device round-trip → unload` lifecycle with
/// `body` as the per-device tail. Never returns.
pub fn run_virtio_pci_scenario<F>(device_id: u16, cfg: &ScenarioConfig<'_>, body: F) -> !
where
    F: FnOnce(&dyn QemuEnv, PciTransport, &dyn VirtioHost) -> Result<(), &'static str>,
{
    use tairix_abi::driver::msix::MsixBus;

    let env = PciEnv;
    env.log(cfg.start_msg);

    // Disable interrupts for the whole scenario; the waiter re-enables
    // them only across its `hlt` park.
    // SAFETY: see `cli`.
    unsafe { cli() };

    let Some(table) = published_irq_table() else {
        env.fail("no published IrqTable");
    };
    let Some(memmap) = published_memory_map() else {
        env.fail("no published memory map");
    };

    // 1. Per-device DMA: carve high frames + the boot identity map.
    let Some(dma_map) = carve_dma_map(memmap, CARVE_PAGES) else {
        env.fail("no carveable DMA region");
    };
    let Ok(frames) = FrameAllocator::new(&dma_map) else {
        env.fail("frame allocator build");
    };
    let phys = DirectPhysMap::identity(IDENTITY_LIMIT);

    // 2. Bus-driver task capability context.
    let mut grants = CapabilitySet::empty();
    grants.insert(CapabilityId::MMIO_MAP);
    grants.insert(CapabilityId::MEM_DMA);
    grants.insert(CapabilityId::DRV_LOAD);
    let caller = TaskCapabilities::derive(TASK, UserId(0), grants, grants, &SERIAL_SINK);

    // 3. Bind the device's (masked) GSI and build its MSI message.
    let Ok(bind) = table.bind(DEVICE_GSI, TASK) else {
        env.fail("bind device GSI");
    };
    let handle = bind.handle;
    let Some(vector) = global_routing().vector_for_gsi(DEVICE_GSI) else {
        env.fail("no vector for GSI");
    };
    let msi = msi_message(vector, bsp_lapic_id());

    // 4. Walk PCI, map the four virtio register windows, route MSI-X.
    //    The x86_64 architecture port supplies the `PortIo` backend the
    //    bus driver drives through the `tairix_abi::PortIo` seam.
    let bus = mechanism_one(x86_port_io());
    let Ok(mut mmio) = MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(MMIO_VBASE),
        MMIO_CAP_PAGES,
        &phys,
    ) else {
        env.fail("MMIO map construct");
    };
    let mut transport = {
        let mapper = KernelMmioMapper::new(&mut mmio, &caller, &SERIAL_SINK);
        let Ok(prov) =
            tairix_kernel::provision_virtio_pci(&bus, device_id, &mapper, PciTransport::new)
        else {
            env.fail("virtio-PCI provisioning walk");
        };
        if bus.route_msix(prov.bdf, MSIX_ENTRY, msi, &mapper).is_err() {
            env.fail("route MSI-X");
        }
        prov.transport
    };
    env.log("virtio-qemu: transport provisioned, MSI-X routed");

    // 5. Mint the per-device DMA host the driver allocates through.
    let space = AddressSpace::new(HostPageTable::new());
    let Ok(pool) = DmaPool::new(space, VirtAddr::new(POOL_VBASE), POOL_PAGES, &frames, &phys)
    else {
        env.fail("DMA pool construct");
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

    // 6. Mint the per-driver factory, enable MSI-X on every queue, then
    //    drive the shared lifecycle with `body` against the reloaded
    //    driver.
    let factory = KernelVirtioFactory::new(
        KernelVirtioFactoryConfig {
            frames: &frames,
            phys: &phys,
            caller: &caller,
            audit: &SERIAL_SINK,
            irq: table,
            irq_handle: handle,
            waiter: &waiter,
            pool_base: VirtAddr::new(POOL_VBASE),
            pool_pages: POOL_PAGES,
        },
        HostPageTable::new,
    );
    transport.enable_msix(MSIX_ENTRY);
    let factory: &dyn VirtioHostFactory = &factory;
    drive_driver_lifecycle(&env, cfg, factory, transport, &vhost, body)
}

// --- Boot harness ----------------------------------------------------

/// Generate the freestanding boot harness for an x86_64 virtio QEMU test
/// bin: the audit-observer `Sink` that drives `$scenario` once on
/// `BootCompleted`, the `#[panic_handler]` bridge, and the `kernel_main`
/// entry point that runs the production boot pipeline with the observer
/// installed.
///
/// `$scenario` must be a `fn() -> !`. Invoke exactly once at the crate
/// root of the freestanding bin.
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

        /// Forward to the shared panic bridge in `tairix_kernel`.
        #[panic_handler]
        fn virtio_qemu_panic(info: &::core::panic::PanicInfo<'_>) -> ! {
            $crate::handle_panic_via_kernel_core(info)
        }

        /// Boot entry point — the production `tairix-kernel` surface with
        /// the audit observer sink in place.
        #[no_mangle]
        pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
            $crate::boot(
                multiboot_info,
                &$crate::ALLOCATOR,
                &$crate::HARNESS_SERIAL_SINK,
                &AUDIT_SINK,
                $crate::HarnessLevel::Info,
            )
        }
    };
}
