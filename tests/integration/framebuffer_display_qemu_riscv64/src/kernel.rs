//! Freestanding (`riscv64gc-unknown-none-elf`) half of the
//! framebuffer-display QEMU vertical.
//!
//! Boots the production riscv64 `virt`-board pipeline (via
//! [`rustos_test_riscv64_boot::boot`]) with an audit-observer sink in
//! place. On `AuditEvent::BootCompleted` it:
//!
//! 1. programs QEMU's `ramfb` over the `fw_cfg` MMIO DMA interface so a
//!    static page-aligned scan-out surface in guest RAM becomes a real
//!    framebuffer device the host scans out from (the boot hand-off);
//! 2. assembles the parsed geometry into a
//!    [`rustos_drv_display_framebuffer::FramebufferConfig`];
//! 3. loads the signed framebuffer display `.rxe` through
//!    [`rustos_drvhost::Host`] (the §8 load gate) and drives it through
//!    `load -> use -> unload -> reload`, where "use" maps the surface
//!    through the capability-gated [`rustos_kernel_virtio::KernelMmioMapper`]
//!    and `present`s a frame; a second independently-mapped window reads
//!    the pixels back to confirm they landed in the scan-out memory.
//!
//! The `fw_cfg`/`ramfb` bring-up is test-harness-specific (it
//! synthesises the device QEMU scans out), mirroring how the virtio
//! verticals own their PLIC + trap bring-up in the test support crate
//! rather than in production kernel code.

mod scenario;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use rustos_arch_riscv64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
use rustos_bumpalloc::{BumpAllocator, Heap, HEAP_BYTES};
use rustos_log::{Event, EventId, Sink};
use rustos_test_riscv64_boot::boot;

/// Static boot heap, in the linker's NOLOAD `.heap` section so the boot
/// trampoline neither zeroes nor includes it in the usable memory map.
#[link_section = ".heap"]
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: BumpAllocator =
    unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

/// `EventId(4004)` — `AuditEvent::BootCompleted`. Pinned by the
/// `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

/// Latch so the scenario runs exactly once.
static SCENARIO_RAN: AtomicBool = AtomicBool::new(false);

/// Audit observer: replays every event through the serial sink and, on
/// `BootCompleted`, drives the framebuffer scenario exactly once.
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

/// Forward to the shared riscv64 panic bridge.
#[panic_handler]
fn framebuffer_qemu_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_serial(info)
}

/// Boot entry point — the production `rustos-arch-riscv64` surface with
/// the audit observer sink in place.
#[no_mangle]
pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
    boot(hartid, dtb, &SERIAL_SINK, &AUDIT_SINK)
}
