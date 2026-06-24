//! The freestanding aarch64 test kernel: arm the PL031 RTC (a GICv2 SPI)
//! and prove its interrupt reaches a Rust handler through the EL1 IRQ
//! path and the kernel/irq mask-before-wake table.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use rustos_abi::IrqHandle;
use rustos_arch_aarch64::gic::{self, GicController, Gicv2, VolatileGicMmio, MAX_INTID};
use rustos_arch_aarch64::{exceptions, handle_panic_via_serial, qemu_exit, SERIAL_SINK};
use rustos_kalloc::FreeListAllocator;
use rustos_kernel_irq::{IrqController, IrqTable, MaskError, WaitStep};
use rustos_kernel_sec::TaskId;
use rustos_log::{log, Event, EventId, Level};

/// Bump heap backing the `IrqTable` allocations (one `bind`, two
/// per-line flag vectors). A few hundred bytes suffice; 256 KiB is
/// generous headroom. It lives in `.bss` (zeroed by the boot
/// trampoline); the table's `Vec`s are populated explicitly.
const HEAP_SIZE: usize = 256 * 1024;

/// Page-aligned backing store for the bump heap.
#[repr(C, align(4096))]
struct HeapStore([u8; HEAP_SIZE]);

static mut HEAP: HeapStore = HeapStore([0; HEAP_SIZE]);

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_SIZE) };

// --- PL031 RTC (the `virt` board's GICv2 SPI device) --------------

/// MMIO base of the QEMU `virt` board's PL031 real-time clock.
const PL031_BASE: usize = 0x0901_0000;
/// `RTCDR` — current count (read-only), offset 0x000.
const RTCDR: usize = 0x000;
/// `RTCMR` — match register, offset 0x004. The RTC raises its interrupt
/// when the count reaches this value.
const RTCMR: usize = 0x004;
/// `RTCIMSC` — interrupt mask set/clear (bit 0), offset 0x010. Writing 1
/// unmasks the RTC interrupt.
const RTCIMSC: usize = 0x010;
/// `RTCICR` — interrupt clear (bit 0), offset 0x01C. Writing 1 clears the
/// pending RTC interrupt at the device.
const RTCICR: usize = 0x01C;

/// GIC INTID of the PL031 RTC on the `virt` board: the RTC is shared-
/// peripheral interrupt 2 (`a15irqmap[VIRT_RTC] = 2`), so its INTID is
/// `MIN_SPI_INTID + 2`.
const RTC_INTID: u32 = gic::MIN_SPI_INTID + 2;

/// CPU-interface target bitmask routing the SPI to the boot CPU (CPU 0).
const CPU0_TARGET: u8 = 0b0000_0001;

/// Synthesised owner for the IRQ binding. No real task runs in this
/// test; the bind only needs an opaque attribution id.
const OWNER: TaskId = TaskId(0);

/// Stable audit-event ids for the QEMU transcript.
const TEST_START: EventId = EventId(4260);
const TEST_PASS: EventId = EventId(4261);

/// Semihosting failure codes, distinct per failure site.
const FAIL_REENTRY: u16 = 1;
const FAIL_BIND: u16 = 2;
const FAIL_DISPATCH_INSTALL: u16 = 3;
const FAIL_NOT_MASKED: u16 = 4;
const FAIL_WAIT: u16 = 5;

/// Set once the scenario has been driven so a re-entry cannot re-run it.
static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

/// Raw pointer to the [`IrqTable`] built in [`kernel_main`], published
/// for the interrupt-context dispatcher to reach without a captured
/// environment. `0` until set; written before IRQs are unmasked.
static TABLE_PTR: AtomicUsize = AtomicUsize::new(0);

/// The kernel/irq controller bridge over the aarch64 [`GicController`].
///
/// the charter forbids the architecture crate from depending on `kernel/irq`,
/// so this bridge — the aarch64 analogue of x86_64's `IoApicController`
/// `IrqController` impl — lives in the test crate (which may depend on
/// both). `mask` delegates to the HAL [`rustos_arch_api::IrqController`]
/// `mask` (which clears the distributor enable bit and emits the
/// `SeqCst` mask-before-wake fence); the only error the GIC controller
/// produces is "INTID out of range", mapped to [`MaskError::OutOfRange`].
struct GicBridge {
    ctrl: GicController<VolatileGicMmio>,
}

/// The bridge instance. Const-constructible (the GIC controller holds
/// only a zero-sized MMIO handle and the max-INTID bound), so it lives in
/// a `static` the interrupt-context dispatcher can reference.
static BRIDGE: GicBridge = GicBridge {
    ctrl: GicController::new(Gicv2::new(VolatileGicMmio), MAX_INTID),
};

impl IrqController for GicBridge {
    fn mask(&self, line: u32) -> Result<(), MaskError> {
        rustos_arch_api::IrqController::mask(&self.ctrl, line).map_err(|_| MaskError::OutOfRange)
    }
}

fn note(id: EventId, message: &'static str) {
    log(
        &SERIAL_SINK,
        &Event {
            level: Level::Info,
            id,
            message,
            fields: &[],
        },
    );
}

/// Forward to the shared aarch64 panic bridge (parks the CPU; the run
/// then times out and the harness reports the failure).
#[panic_handler]
fn irq_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_serial(info)
}

/// Read the PL031 `RTCDR` count.
fn rtc_read_count() -> u32 {
    // SAFETY: `PL031_BASE + RTCDR` is the fixed `virt`-board RTC data
    // register; a 32-bit read has no side effects.
    unsafe { core::ptr::read_volatile((PL031_BASE + RTCDR) as *const u32) }
}

/// Arm the PL031 match interrupt to fire one tick (≈ 1 s) from now and
/// unmask it at the device.
fn rtc_arm() {
    let next = rtc_read_count().wrapping_add(1);
    // SAFETY: `RTCMR` / `RTCIMSC` are the fixed `virt`-board RTC match
    // and interrupt-mask registers; the writes program a one-shot match
    // interrupt and touch no Rust-managed memory.
    unsafe {
        core::ptr::write_volatile((PL031_BASE + RTCMR) as *mut u32, next);
        core::ptr::write_volatile((PL031_BASE + RTCIMSC) as *mut u32, 1);
    }
}

/// Clear the PL031 interrupt at the device so it deasserts the GIC line.
fn rtc_clear() {
    // SAFETY: `RTCICR` is the fixed `virt`-board RTC interrupt-clear
    // register; writing bit 0 clears the pending interrupt.
    unsafe {
        core::ptr::write_volatile((PL031_BASE + RTCICR) as *mut u32, 1);
    }
}

/// `true` iff the GIC distributor enable bit for `intid` is set (the line
/// is unmasked). Reads `GICD_ISENABLER` directly — the evidence path for
/// the mask-before-wake assertion.
fn gic_line_enabled(intid: u32) -> bool {
    let off = gic::current().0 + gic::isenabler_offset(intid);
    // SAFETY: `off` is a distributor register within the discovered
    // GICv2 distributor window (the `virt` default here); a 32-bit read
    // has no side effects.
    let word = unsafe { core::ptr::read_volatile(off as *const u32) };
    word & gic::isenabler_bit(intid) != 0
}

/// The device-IRQ dispatcher the EL1 IRQ path forwards a non-timer INTID
/// to. It clears the RTC source and forwards the line to
/// [`IrqTable::fire`] over the [`BRIDGE`] — which masks the GIC line
/// *before* the table sets the per-line ready flag (mask-before-wake).
///
/// Runs in interrupt context with IRQs masked (the PE masked them on
/// exception entry); it allocates nothing and takes no lock the waiter
/// holds (`IrqTable::fire` is lock-free).
extern "C" fn rtc_dispatch(intid: u32) {
    if intid != RTC_INTID {
        return;
    }
    rtc_clear();
    let raw = TABLE_PTR.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: `TABLE_PTR` is set to a pointer to the `IrqTable`
        // owned by `kernel_main`'s stack frame before IRQs are unmasked;
        // `kernel_main` never returns (it exits through semihosting), so
        // the pointee is live for the whole program.
        let table = unsafe { &*(raw as *const IrqTable) };
        let _ = table.fire(RTC_INTID, &BRIDGE);
    }
}

/// Boot entry point — the symbol the arch crate's `boot.s` trampoline
/// calls (via `rustos_arch_aarch64_main`).
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    if TEST_DRIVEN
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        qemu_exit::exit_failure(FAIL_REENTRY);
    }

    note(
        TEST_START,
        "aarch64 device-IRQ test: arming the PL031 RTC SPI",
    );

    // 1. Build the kernel-neutral IRQ table and bind the RTC line, then
    //    publish a pointer to it for the interrupt-context dispatcher.
    let table = IrqTable::new(RTC_INTID);
    let Ok(bind) = table.bind(RTC_INTID, OWNER) else {
        note(TEST_START, "IrqTable::bind rejected the RTC line");
        qemu_exit::exit_failure(FAIL_BIND);
    };
    let handle: IrqHandle = bind.handle;
    TABLE_PTR.store(core::ptr::addr_of!(table) as usize, Ordering::Release);

    // 2. Install the device-IRQ dispatcher before any source can fire.
    if exceptions::set_device_irq_dispatch(rtc_dispatch).is_err() {
        note(TEST_START, "device-IRQ dispatcher already installed");
        qemu_exit::exit_failure(FAIL_DISPATCH_INSTALL);
    }

    // 3. EL1 vector table + GICv2 bring-up.
    // SAFETY: called once on the boot CPU with a stack established and
    // before any source is armed; the dispatcher is installed.
    unsafe {
        exceptions::init_vectors();
        gic::init();
    }

    // 4. Route the RTC SPI to CPU 0 (the new `GICD_ITARGETSR` write — the
    //    SPI never delivers without it) and enable it at the
    //    distributor, then arm the RTC match and unmask IRQs at the PE.
    // SAFETY: the distributor is enabled (step 3); these program the
    //    fixed `virt`-board GICv2 windows for the RTC SPI.
    unsafe {
        gic::route_spi(RTC_INTID, CPU0_TARGET);
        gic::enable_ppi(RTC_INTID);
    }
    rtc_arm();
    // SAFETY: the vector table, dispatcher, and GIC routing are in place,
    // so an incoming RTC SPI dispatches through the installed path.
    unsafe {
        exceptions::enable_irq();
    }

    // 5. Wait for the device interrupt to drive the table to `Ready`. No
    //    deadline (`u64::MAX`) — the harness wall-clock budget is the
    //    backstop, so a line that never fires times out and is reported
    //    as a failure, never a false pass.
    loop {
        match table.try_wait_step(handle, OWNER, 0, u64::MAX) {
            WaitStep::Ready => break,
            WaitStep::Continue => {
                // SAFETY: `wfi` parks until the next interrupt; the RTC
                // SPI wakes it and re-evaluates the wait step.
                unsafe {
                    core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
                }
            }
            WaitStep::TimedOut | WaitStep::NotFound => {
                // Unreachable with an infinite deadline and a live handle,
                // but fail closed rather than spin.
                note(TEST_START, "unexpected wait-step outcome");
                qemu_exit::exit_failure(FAIL_WAIT);
            }
        }
    }

    // 6. Mask-before-wake evidence: the dispatcher's `IrqTable::fire`
    //    masked the GIC line through the bridge *before* it set the ready
    //    flag the loop above observed, so the line must now read disabled.
    if gic_line_enabled(RTC_INTID) {
        note(
            TEST_START,
            "GIC line still enabled after wake (mask-before-wake violated)",
        );
        qemu_exit::exit_failure(FAIL_NOT_MASKED);
    }

    note(
        TEST_PASS,
        "aarch64 device-IRQ test: RTC SPI reached the handler, line masked",
    );
    qemu_exit::exit_success();
}
