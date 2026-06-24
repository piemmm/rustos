//! Freestanding (`x86_64-unknown-none`) half of the vesa-display QEMU
//! vertical.
//!
//! Boots the production `rustos-kernel` pipeline with an audit-observer
//! sink in place. On `AuditEvent::BootCompleted` it:
//!
//! 1. programs QEMU's `ramfb` over the `fw_cfg` I/O-port DMA interface so a
//!    static page-aligned scan-out surface in guest RAM becomes a real
//!    framebuffer device the host scans out from;
//! 2. publishes a bootloader-captured VBE `ModeInfoBlock` describing that
//!    surface as the boot hand-off;
//! 3. loads the signed vesa display `.rxe` through
//!    [`rustos_drvhost::Host`] (the load gate) and drives it through
//!    `load -> use -> unload -> reload`, where "use" decodes the block
//!    with `VesaFramebuffer::open`, maps the surface through the
//!    capability-gated [`rustos_kernel_virtio::KernelMmioMapper`], and
//!    `present`s a frame; a second independently-mapped window reads the
//!    pixels back to confirm they landed in the scan-out memory.
//!
//! The `fw_cfg`/`ramfb` bring-up is test-harness-specific (it synthesises
//! the device QEMU scans out), mirroring how the riscv64 framebuffer
//! vertical owns its `fw_cfg` bring-up rather than placing it in
//! production kernel code.

mod ioport;
mod scenario;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use rustos_arch_x86_64::qemu_exit;
use rustos_kernel::kalloc::{Heap, HEAP_BYTES};
use rustos_kernel::{
    boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
};
use rustos_log::{Event, EventId, Sink};

/// Static heap backing the bump allocator. Sized identically to the
/// other x86_64 QEMU verticals' — the workload is the same shape (one
/// boot pipeline plus a handful of `Vec` allocations from the host's
/// load path and the `fw_cfg` directory scan).
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by [`HEAP`]. The pointer to `HEAP` outlives
/// the binary, and the allocator is the only consumer (deterministic OOM via `FreeListAllocator`).
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

/// `EventId(4004)` — `AuditEvent::BootCompleted`. Pinned by the
/// `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

/// Latch so the scenario runs exactly once.
static SCENARIO_RAN: AtomicBool = AtomicBool::new(false);

/// Audit observer: replays every event through the serial sink and, on
/// `BootCompleted`, drives the vesa scenario exactly once then flips
/// QEMU to `exit_success`.
struct BootObserverSink;

impl Sink for BootObserverSink {
    fn write_event(&self, event: &Event<'_>) {
        SerialSink::new().write_event(event);
        if event.id == BOOT_COMPLETED_EVENT_ID && !SCENARIO_RAN.swap(true, Ordering::SeqCst) {
            scenario::run();
            qemu_exit::exit_success();
        }
    }
}

static AUDIT_SINK: BootObserverSink = BootObserverSink;

/// Panic handler — forwards through `rustos_kernel`'s shared bridge.
#[panic_handler]
fn vesa_qemu_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_kernel_core(info)
}

/// Boot entry point — the production `rustos-kernel` surface with the
/// audit observer sink in place.
#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
    boot(multiboot_info, &SERIAL_SINK, &AUDIT_SINK)
}
