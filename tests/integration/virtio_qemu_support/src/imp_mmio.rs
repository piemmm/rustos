//! Freestanding (`riscv64gc-unknown-none-elf`) virtio-MMIO bring-up for
//! the QEMU `virt` board.
//!
//! The device-agnostic lifecycle and the per-device tails live in
//! [`crate::common`]; this module owns only the riscv64-specific bring-up
//! that produces an [`MmioTransport`] and an interrupt path: build the
//! `virt`-board virtio-MMIO bus from the published device tree, provision
//! the transport through the `CAP_MMIO_MAP`-gated [`KernelMmioMapper`],
//! walk the DTB for the device's interrupt source, bind it into the
//! IRQ table the production boot published, arm it through the
//! boot-built PLIC controller, and park on a race-free `wfi`.

use tairix_abi::{CapabilityId, IrqHandle};
use tairix_arch_riscv64::plic::VolatilePlicMmio;
use tairix_arch_riscv64::{qemu_exit, SERIAL_SINK};
use tairix_caps::CapabilitySet;
use tairix_drv_bus_mmio::virtio_mmio_bus_from_dtb;
use tairix_drv_bus_virtio::MmioTransport;
use tairix_fdt::Fdt;
use tairix_kernel_irq::{IrqTable, IrqWaitAbort, IrqWaiter};
use tairix_kernel_mem::{
    AddressSpace, DirectPhysMap, DmaPool, FrameAllocator, HostPageTable, MmioMap, VirtAddr,
};
use tairix_kernel_sec::captable::{TaskCapabilities, TaskId};
use tairix_kernel_sec::identity::UserId;
use tairix_kernel_virtio::{
    provision_virtio_mmio, KernelMmioMapper, KernelVirtioFactory, KernelVirtioFactoryConfig,
    KernelVirtioHost,
};
use tairix_log::{Event, EventId, Level, Sink};
use tairix_test_riscv64_boot::{
    plic_controller, published_dtb, published_irq_table, published_memory_map, PlicIrqController,
};
use tairix_virtio::{PoolId, VirtioHost, VirtioHostFactory};

/// Re-export so the verticals name the concrete transport for the shared
/// device-tail turbofish under the same name as the PCI vertical.
pub use tairix_drv_bus_virtio::MmioTransport as ScenarioTransport;

// Re-exports the `define_mmio_boot_harness!` macro expands against via
// `$crate::...`. (`BOOT_COMPLETED_EVENT_ID` is re-exported at the crate
// root through `pub use common::*`.)
#[doc(hidden)]
pub use tairix_arch_riscv64::handle_panic_via_serial;
#[doc(hidden)]
pub use tairix_arch_riscv64::{
    SerialSink as HarnessSerialSink, SERIAL_SINK as HARNESS_SERIAL_SINK,
};
#[doc(hidden)]
pub use tairix_log::{
    Event as HarnessEvent, EventId as HarnessEventId, Level as HarnessLevel, Sink as HarnessSink,
};
#[doc(hidden)]
pub use tairix_test_riscv64_boot::boot;

use crate::common::{
    carve_dma_map, drive_driver_lifecycle, dtb_total_size, QemuEnv, ScenarioConfig, IDENTITY_LIMIT,
};

use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};

// --- Bump-allocator-backed `#[global_allocator]` ---------------------

/// Static boot heap for the bump allocator.
///
/// Placed in the linker's dedicated `.heap` (NOLOAD) section — same as
/// the riscv64 boot test bin — so the boot trampoline does not zero its
/// 64 MiB and the boot pipeline excludes it from the usable
/// physical-memory map.
#[link_section = ".heap"]
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by the module-private `HEAP` static, shared by
/// every riscv64 virtio QEMU test that links this crate.
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary; the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

// --- Stable identifiers ----------------------------------------------

/// Milestone event id namespace for the shared serial breadcrumbs.
const MILESTONE_ID: EventId = EventId(9100);

// --- Bring-up parameters ---------------------------------------------

/// Synthetic owner task id for the bus-driver context.
const TASK: TaskId = TaskId(0x5b2);

/// Capacity, in pages, of each per-device DMA window.
const POOL_PAGES: usize = 64;

/// Pages carved from high RAM for the per-device DMA allocator. Sized
/// like the x86_64 vertical: the direct-driving pool plus the transient
/// pool the `.rxe` load mints, with slack. At 4 KiB pages this is 1 MiB,
/// carved from the very top of RAM, comfortably above the firmware
/// device-tree blob OpenSBI leaves near the top.
const CARVE_PAGES: usize = 256;

/// Base virtual address of each minted DMA window (bookkeeping only; the
/// driver reaches buffers through the identity map, so this address only
/// keys the pool's slot bitmap).
const POOL_VBASE: u64 = 0x2000_0000;

/// Base virtual address of the MMIO register-window map (bookkeeping; the
/// window is reached through the identity map).
const MMIO_VBASE: u64 = 0x6000_0000;

/// Capacity, in pages, of the MMIO register-window map.
const MMIO_CAP_PAGES: usize = 64;

/// `sstatus.SIE` — supervisor global interrupt-enable (bit 1). The
/// race-free `wfi` park clears it across the readiness check + `wfi` and
/// restores it after, so a completion that lands in that window is held
/// *pending* (not taken) until `wfi` has been entered — closing the
/// lost-wake-up window without a bounding timer.
const SSTATUS_SIE: u64 = 1 << 1;

/// virtio-MMIO transport `compatible` string.
const VIRTIO_MMIO_COMPATIBLE: &str = "virtio,mmio";

// --- QEMU environment ------------------------------------------------

/// riscv64 [`QemuEnv`]: serial breadcrumbs over the SBI console sink,
/// exit through the `SiFive` Test finisher device.
struct MmioEnv;

impl QemuEnv for MmioEnv {
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
        qemu_exit::exit_failure(1)
    }

    fn succeed(&self) -> ! {
        qemu_exit::exit_success()
    }

    fn audit_sink(&self) -> &'static dyn Sink {
        &SERIAL_SINK
    }
}

// --- Device-tree helpers ---------------------------------------------

/// Find the PLIC interrupt source of the `virtio,mmio` slot whose `reg`
/// base equals `slot_base` (its single `interrupts` cell).
fn device_interrupt(dtb: &Fdt<'_>, slot_base: u64) -> Option<u32> {
    for node in dtb.nodes() {
        let node = node.ok()?;
        if !node.is_compatible(VIRTIO_MMIO_COMPATIBLE) {
            continue;
        }
        let reg = node.property("reg")?;
        if reg.read_be_u64(0).ok()? != slot_base {
            continue;
        }
        return node.property("interrupts")?.read_be_u32(0).ok();
    }
    None
}

// --- IRQ waiter ------------------------------------------------------

/// [`IrqWaiter`] that parks the boot hart on a race-free `wfi`.
///
/// Before parking it unmasks the device's PLIC source (a prior
/// [`IrqTable::fire`] dropped its priority to zero) so the next
/// completion can deliver. The park itself clears `sstatus.SIE`, re-reads
/// the line's ready flag, parks on `wfi` only if still not ready, then
/// restores `SIE`. Clearing `SIE` makes a completion that lands between
/// the check and the `wfi` *pending* rather than taken, so `wfi` observes
/// it and wakes — no edge is lost and no bounding timer is needed
/// (no unbounded sleep loop, no hack).
struct WfiWaiter {
    plic: &'static PlicIrqController<VolatilePlicMmio>,
    source: u32,
    table: &'static IrqTable,
    handle: IrqHandle,
}

impl IrqWaiter for WfiWaiter {
    fn now_ns(&self) -> u64 {
        // The host waits with the `u64::MAX` unbounded sentinel, so the
        // exact value is immaterial; a fixed reading never reaches the
        // saturated deadline.
        0
    }

    fn yield_now(&self) -> Result<(), IrqWaitAbort> {
        // The loop only yields when the line is not yet ready. Unmask the
        // source so the next completion delivers.
        let _ = self.plic.unmask(self.source);
        // SAFETY: clearing `sstatus.SIE` masks interrupt *taking* (not
        // pending); `wfi` still wakes on a pending enabled interrupt;
        // restoring `SIE` lets the trap fire. The sequence is the
        // canonical race-free park: a completion arriving after the
        // ready re-check is held pending until `wfi` is entered.
        unsafe {
            core::arch::asm!("csrc sstatus, {}", in(reg) SSTATUS_SIE, options(nomem, nostack));
            if !self.table.ready_for(self.handle) {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
            core::arch::asm!("csrs sstatus, {}", in(reg) SSTATUS_SIE, options(nomem, nostack));
        }
        Ok(())
    }
}

/// Bind this device's interrupt into the external-IRQ path the production
/// boot pipeline already stood up, and arm the source.
///
/// The boot harness runs the full `boot_riscv64` pipeline before this
/// scenario, and that pipeline builds the single-context S-mode PLIC
/// controller from the firmware device tree, publishes the kernel
/// [`IrqTable`], installs the S-mode external-interrupt dispatcher
/// (claim → [`IrqTable::fire`] → complete → wake), and enables `sie.SEIE`.
/// The set-once trap dispatch and the one-controller-per-boot rule mean the
/// scenario must **reuse** that path, not build a second: it resolves the
/// device's PLIC source from the device tree, binds it into the published
/// table, and arms it through the published controller. The `wfi` waiter
/// manages `sstatus.SIE` itself, and the loaded driver acknowledges the
/// device-level virtio-MMIO interrupt through its transport, so no
/// scenario-owned dispatch is needed. Any failure flips QEMU failure via
/// `env`.
fn arm_external_irq(
    env: &MmioEnv,
    dtb: &Fdt<'_>,
    slot_base: u64,
) -> (
    &'static IrqTable,
    &'static PlicIrqController<VolatilePlicMmio>,
    IrqHandle,
    u32,
) {
    let Some(table) = published_irq_table() else {
        env.fail("kernel IRQ table unpublished");
    };
    let Some(controller) = plic_controller() else {
        env.fail("kernel PLIC controller unpublished");
    };
    let Some(source) = device_interrupt(dtb, slot_base) else {
        env.fail("no device interrupt in DTB");
    };
    let Ok(bind) = table.bind(source, TASK) else {
        env.fail("bind device source");
    };
    if controller.arm(source).is_err() {
        env.fail("arm PLIC source");
    }
    (table, controller, bind.handle, source)
}

// --- Shared scenario -------------------------------------------------

/// Perform the riscv64 `virt`-board virtio-MMIO bring-up for the device
/// whose bare virtio type id is `device_id` (block = 2, net = 1), then
/// drive the shared `load → reload → device round-trip → unload`
/// lifecycle with `body` as the per-device tail. Never returns.
pub fn run_virtio_mmio_scenario<F>(device_id: u32, cfg: &ScenarioConfig<'_>, body: F) -> !
where
    F: FnOnce(&dyn QemuEnv, MmioTransport, &dyn VirtioHost) -> Result<(), &'static str>,
{
    let env = MmioEnv;
    env.log(cfg.start_msg);

    let Some(dtb_ptr) = published_dtb() else {
        env.fail("no published DTB");
    };
    let Some(memmap) = published_memory_map() else {
        env.fail("no published memory map");
    };

    // 1. Form a `&[u8]` over the exact device-tree blob.
    // SAFETY: `dtb_ptr` is the verbatim OpenSBI `a1` the boot pipeline
    // published; it addresses a valid flattened device tree that lives
    // for the life of the guest, and the carved DMA region is taken from
    // the top of RAM, clear of the blob.
    let dtb_len = unsafe { dtb_total_size(dtb_ptr) };
    // SAFETY: as above; `dtb_len` is the blob's self-described size.
    let dtb_bytes = unsafe { core::slice::from_raw_parts(dtb_ptr as *const u8, dtb_len) };
    let Ok(dtb) = Fdt::new(dtb_bytes) else {
        env.fail("DTB parse");
    };

    // 2. Per-device DMA: carve high frames + the boot identity map.
    let Some(dma_map) = carve_dma_map(memmap, CARVE_PAGES) else {
        env.fail("no carveable DMA region");
    };
    let Ok(frames) = FrameAllocator::new(&dma_map) else {
        env.fail("frame allocator build");
    };
    let phys = DirectPhysMap::identity(IDENTITY_LIMIT);

    // 3. Bus-driver task capability context.
    let mut grants = CapabilitySet::empty();
    grants.insert(CapabilityId::MMIO_MAP);
    grants.insert(CapabilityId::MEM_DMA);
    grants.insert(CapabilityId::DRV_LOAD);
    let caller = TaskCapabilities::derive(TASK, UserId(0), grants, grants, &SERIAL_SINK);

    // 4. Build the `virt`-board MMIO bus and provision the transport.
    // SAFETY: the `virt` board enters S-mode with paging off (`satp == 0`),
    // so the virtio-MMIO aperture the device tree describes is
    // identity-mapped and exclusively the bus's to read.
    let Ok(bus) = (unsafe { virtio_mmio_bus_from_dtb(dtb_bytes) }) else {
        env.fail("virtio-MMIO bus construct");
    };
    let Ok(mut mmio) = MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(MMIO_VBASE),
        MMIO_CAP_PAGES,
        &phys,
    ) else {
        env.fail("MMIO map construct");
    };
    let (transport, slot_base) = {
        let mapper = KernelMmioMapper::new(&mut mmio, &caller, &SERIAL_SINK);
        let Ok(prov) = provision_virtio_mmio(&bus, device_id, &mapper, MmioTransport::new) else {
            env.fail("virtio-MMIO provisioning walk");
        };
        (prov.transport, prov.base)
    };
    env.log("virtio-qemu: MMIO transport provisioned");

    // 5. Bind + arm this device's interrupt in the boot-published PLIC
    //    path (the production dispatch is already installed).
    let (table, controller, handle, source) = arm_external_irq(&env, &dtb, slot_base);
    env.log("virtio-qemu: PLIC source armed on the published table");

    // 6. Mint the per-device DMA host the driver allocates through.
    let space = AddressSpace::new(HostPageTable::new());
    let Ok(pool) = DmaPool::new(space, VirtAddr::new(POOL_VBASE), POOL_PAGES, &frames, &phys)
    else {
        env.fail("DMA pool construct");
    };
    let waiter = WfiWaiter {
        plic: controller,
        source,
        table,
        handle,
    };
    let vhost = KernelVirtioHost::new(
        pool,
        &caller,
        &SERIAL_SINK,
        PoolId::fresh(),
        table,
        handle,
        &waiter,
    );

    // 7. Mint the per-driver factory, then drive the shared lifecycle
    //    with `body` against the reloaded driver.
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
    let factory: &dyn VirtioHostFactory = &factory;
    drive_driver_lifecycle(&env, cfg, factory, transport, &vhost, body)
}

// --- Boot harness ----------------------------------------------------

/// Generate the freestanding boot harness for a riscv64 virtio-MMIO QEMU
/// test bin: the audit-observer `Sink` that drives `$scenario` once on
/// `BootCompleted`, the `#[panic_handler]` bridge, and the
/// `kernel_main(hartid, dtb)` entry point that runs the production
/// riscv64 boot pipeline with the observer installed.
///
/// `$scenario` must be a `fn() -> !`. Invoke exactly once at the crate
/// root of the freestanding bin.
#[macro_export]
macro_rules! define_mmio_boot_harness {
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

        /// Forward to the shared riscv64 panic bridge.
        #[panic_handler]
        fn virtio_qemu_mmio_panic(info: &::core::panic::PanicInfo<'_>) -> ! {
            $crate::handle_panic_via_serial(info)
        }

        /// Boot entry point — the production `tairix-arch-riscv64` surface
        /// with the audit observer sink in place.
        #[no_mangle]
        pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
            $crate::boot(
                hartid,
                dtb,
                &$crate::HARNESS_SERIAL_SINK,
                &AUDIT_SINK,
                $crate::HarnessLevel::Info,
            )
        }
    };
}
