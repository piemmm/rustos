//! The freestanding aarch64 test kernel: discover the PL031 RTC's GICv2
//! SPI from the embedded `virt` device tree, then prove that SPI wakes an
//! in-kernel service kthread parked on it through
//! [`rustos_kernel_core::KthreadIrqWaiter`] under the live eevdf scheduler.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::sync::Arc;

use rustos_abi::IrqHandle;
use rustos_arch_aarch64::context_hal::ContextSwitchHal;
use rustos_arch_aarch64::fdt::gic_device_intid;
use rustos_arch_aarch64::gic::{self, GicController, Gicv2, VolatileGicMmio, MAX_INTID};
use rustos_arch_aarch64::kernel_arch::timer_frequency_hz;
use rustos_arch_aarch64::{
    exceptions, handle_panic_via_serial, qemu_exit, Aarch64Arch, Aarch64ArchStorage, SERIAL_SINK,
};
use rustos_fdt::Fdt;
use rustos_kernel_core::{spawn_kthread, CooperativeYield, KthreadIrqWaiter, YielderHandle};
use rustos_kernel_irq::{block_until_ready, IrqController, IrqTable, MaskError, WaitOutcome};
use rustos_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
use rustos_kernel_sec::TaskId;
use rustos_log::{log, Event, EventId, Level};

// The canonical QEMU `virt` device tree, dumped and embedded at build time.
include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

/// The single-core slice runs logical CPU 0 on the boot core.
const BOOT_CPU: rustos_arch_api::CpuId = 0;

/// Bump heap backing the leaked `IrqTable`, the scheduler, the arch
/// handle, and the kthread's kernel stack. 2 MiB is generous headroom; it
/// lives in `.bss` (zeroed by the boot trampoline).
const HEAP_SIZE: usize = 2 * 1024 * 1024;

/// Page-aligned backing store for the bump heap.
#[repr(C, align(4096))]
struct HeapStore([u8; HEAP_SIZE]);

static mut HEAP: HeapStore = HeapStore([0; HEAP_SIZE]);

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: rustos_kalloc::FreeListAllocator = unsafe {
    rustos_kalloc::FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_SIZE)
};

// --- PL031 RTC (the `virt` board's GICv2 SPI device) --------------

/// MMIO base of the QEMU `virt` board's PL031 real-time clock. Used only
/// to *arm* and *clear* the device; the SPI line it raises is discovered
/// from the device tree, not assumed here.
const PL031_BASE: usize = 0x0901_0000;
/// `RTCDR` — current count (read-only), offset 0x000.
const RTCDR: usize = 0x000;
/// `RTCMR` — match register, offset 0x004.
const RTCMR: usize = 0x004;
/// `RTCIMSC` — interrupt mask set/clear (bit 0), offset 0x010.
const RTCIMSC: usize = 0x010;
/// `RTCICR` — interrupt clear (bit 0), offset 0x01C.
const RTCICR: usize = 0x01C;

/// CPU-interface target bitmask routing the SPI to the boot CPU (CPU 0).
const CPU0_TARGET: u8 = 0b0000_0001;

/// Synthesised owner for the IRQ binding — the kthread's attribution id.
const OWNER: TaskId = TaskId(1);

/// Stable audit-event ids for the QEMU transcript.
const TEST_START: EventId = EventId(4300);
const TEST_SPAWNED: EventId = EventId(4301);
const TEST_PASS: EventId = EventId(4302);

/// Semihosting failure codes, distinct per failure site.
const FAIL_REENTRY: u16 = 1;
const FAIL_FDT: u16 = 2;
const FAIL_ZERO_FREQ: u16 = 3;
const FAIL_GIC: u16 = 4;
const FAIL_NO_RTC: u16 = 5;
const FAIL_NO_SPI: u16 = 6;
const FAIL_BIND: u16 = 7;
const FAIL_DISPATCH_INSTALL: u16 = 8;
const FAIL_SCHED_NEW: u16 = 9;
const FAIL_SPAWN: u16 = 10;
const FAIL_DEADLOCK: u16 = 11;
const FAIL_NOT_WOKEN: u16 = 12;
const FAIL_NOT_MASKED: u16 = 13;

/// Set once the scenario has been driven so a re-entry cannot re-run it.
static TEST_DRIVEN: AtomicBool = AtomicBool::new(false);

/// Set by the kthread once [`block_until_ready`] returned
/// [`WaitOutcome::Ready`] — the device SPI woke the parked kthread.
static WOKEN: AtomicBool = AtomicBool::new(false);

/// Raw pointer to the leaked [`IrqTable`], published for the
/// interrupt-context dispatcher to reach without a captured environment.
/// `0` until set; written before IRQs are unmasked.
static TABLE_PTR: AtomicUsize = AtomicUsize::new(0);

/// Cooperative-loop watchdog: maximum `step` iterations before the test
/// declares the workload deadlocked. Sized generously for QEMU TCG; the
/// RTC fires within ~1 s of wall-clock, so this is only a backstop against
/// a kthread that never parks (the harness wall-clock budget is the
/// ultimate one).
const MAX_STEPS: u64 = 200_000_000;

/// The kernel/irq controller bridge over the aarch64 [`GicController`].
///
/// the charter forbids the architecture crate from depending on `kernel/irq`, so
/// this bridge — the same shape as x86_64's `IoApicController` and the
/// production `GicIrqController` — lives in the test crate (which may
/// depend on both). `mask` delegates to the HAL
/// [`rustos_arch_api::IrqController`] `mask` (which clears the distributor
/// enable bit and emits the `SeqCst` mask-before-wake fence).
struct GicBridge {
    ctrl: GicController<VolatileGicMmio>,
}

/// The bridge instance. Const-constructible (the GIC controller holds only
/// a zero-sized MMIO handle and the max-INTID bound), so it lives in a
/// `static` the interrupt-context dispatcher can reference.
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

/// Forward to the shared aarch64 panic bridge (parks the CPU; the run then
/// times out and the harness reports the failure).
#[panic_handler]
fn irq_kthread_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
    // SAFETY: `RTCMR` / `RTCIMSC` are the fixed `virt`-board RTC match and
    // interrupt-mask registers; the writes program a one-shot match
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
    // SAFETY: `off` is a distributor register within the discovered GICv2
    // distributor window (the `virt` default here); a 32-bit read has no
    // side effects.
    let word = unsafe { core::ptr::read_volatile(off as *const u32) };
    word & gic::isenabler_bit(intid) != 0
}

/// The discovered RTC SPI INTID, published so the interrupt-context
/// dispatcher can compare the acknowledged INTID without re-discovering.
static RTC_INTID: AtomicUsize = AtomicUsize::new(0);

/// The device-IRQ dispatcher the EL1 IRQ path forwards a non-timer INTID
/// to. It clears the RTC source and forwards the line to
/// [`IrqTable::fire`] over the [`BRIDGE`] — which masks the GIC line
/// *before* the table sets the per-line ready flag (mask-before-wake).
///
/// Runs in interrupt context with IRQs masked; it allocates nothing and
/// takes no lock the waiter holds (`IrqTable::fire` is lock-free).
extern "C" fn rtc_dispatch(intid: u32) {
    if intid as usize != RTC_INTID.load(Ordering::Acquire) {
        return;
    }
    rtc_clear();
    let raw = TABLE_PTR.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: `TABLE_PTR` is the address of the leaked `'static`
        // `IrqTable`, set before IRQs are unmasked and live for the whole
        // program.
        let table = unsafe { &*(raw as *const IrqTable) };
        let _ = table.fire(intid, &BRIDGE);
    }
}

/// Boot entry point — the symbol the arch crate's `boot.s` trampoline
/// calls (via `rustos_arch_aarch64_main`).
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    if TEST_DRIVEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        qemu_exit::exit_failure(FAIL_REENTRY);
    }

    note(
        TEST_START,
        "aarch64 device-SPI -> parked-kthread test: starting",
    );

    // 1. Discover the board from the embedded `virt` device tree: GICv2
    //    bases, the generic-timer rate, and — the INCREMENT (1) piece under
    //    test — the PL031 RTC's GICv2 SPI line from its `interrupts`
    //    property (never a hard-coded INTID).
    let Ok(fdt) = Fdt::new(DTB_BLOB) else {
        qemu_exit::exit_failure(FAIL_FDT);
    };
    let counter_hz = timer_frequency_hz(&fdt);
    if counter_hz == 0 {
        qemu_exit::exit_failure(FAIL_ZERO_FREQ);
    }
    if gic::configure_from_fdt(&fdt).is_none() {
        qemu_exit::exit_failure(FAIL_GIC);
    }
    let Some(rtc) = fdt
        .nodes()
        .filter_map(Result::ok)
        .find(|n| n.is_compatible("arm,pl031"))
    else {
        qemu_exit::exit_failure(FAIL_NO_RTC);
    };
    let Some(rtc_intid) = gic_device_intid(&rtc) else {
        qemu_exit::exit_failure(FAIL_NO_SPI);
    };
    RTC_INTID.store(rtc_intid as usize, Ordering::Release);

    // 2. Build the kernel-neutral IRQ table sized to the discovered line,
    //    bind it, leak it `'static`, and publish its pointer for the
    //    interrupt-context dispatcher.
    let table: &'static IrqTable = Box::leak(Box::new(IrqTable::new(rtc_intid)));
    let Ok(bind) = table.bind(rtc_intid, OWNER) else {
        qemu_exit::exit_failure(FAIL_BIND);
    };
    let handle: IrqHandle = bind.handle;
    TABLE_PTR.store(core::ptr::addr_of!(*table) as usize, Ordering::Release);

    // 3. Install the device-IRQ dispatcher before any source can fire.
    if exceptions::set_device_irq_dispatch(rtc_dispatch).is_err() {
        qemu_exit::exit_failure(FAIL_DISPATCH_INSTALL);
    }

    // 4. EL1 vector table + GICv2 bring-up.
    // SAFETY: called once on the boot CPU with a stack established and
    // before any source is armed; the dispatcher is installed.
    unsafe {
        exceptions::init_vectors();
        gic::init();
    }

    // 5. Build the live eevdf scheduler over the arch port, cloning the
    //    arch handle so the kthread's monotonic clock can read it.
    // Per-CPU bookkeeping backing for this single-CPU vertical.
    static ARCH_STORAGE: Aarch64ArchStorage<1> = Aarch64ArchStorage::new();
    let arch = Arc::new(Aarch64Arch::new(&ARCH_STORAGE, BOOT_CPU, counter_hz));
    let clock_arch = arch.clone();
    let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
        qemu_exit::exit_failure(FAIL_SCHED_NEW);
    };

    // 6. Admit the waiter as an in-kernel service kthread. Its body blocks
    //    on the bound line through the shared `block_until_ready` loop,
    //    driven by `KthreadIrqWaiter` over its cooperative yield handle —
    //    the exact path the INCREMENT (2) root-unlock kthread will take. No
    //    deadline (`u64::MAX`): the harness wall-clock budget is the
    //    backstop, so a line that never fires times out and is reported as
    //    a failure, never a false pass.
    let spawned = spawn_kthread(
        &sched,
        ContextSwitchHal::new(),
        BOOT_CPU,
        Priority::Normal,
        move |yielder| {
            let mut handle_obj = YielderHandle::new(yielder);
            let coop = CooperativeYield::new(&mut handle_obj);
            let waiter = KthreadIrqWaiter::new(&coop, || clock_arch.monotonic_ns());
            match block_until_ready(table, handle, OWNER, u64::MAX, &waiter) {
                WaitOutcome::Ready => {
                    WOKEN.store(true, Ordering::SeqCst);
                }
                // Unreachable with an infinite deadline and a live handle;
                // leave `WOKEN` clear so the drain reports failure.
                WaitOutcome::TimedOut | WaitOutcome::NotFound | WaitOutcome::Aborted(_) => {}
            }
        },
    );
    if spawned.is_err() {
        qemu_exit::exit_failure(FAIL_SPAWN);
    }
    note(
        TEST_SPAWNED,
        "aarch64 device-SPI test: waiter kthread parked on the RTC SPI",
    );

    // 7. Route the RTC SPI to CPU 0 (the `GICD_ITARGETSR` write — the SPI
    //    never delivers without it) and enable it at the distributor, then
    //    arm the RTC match and unmask IRQs at the PE.
    // SAFETY: the distributor is enabled (step 4); these program the fixed
    //    `virt`-board GICv2 windows for the RTC SPI, and the vector table +
    //    dispatcher are installed so an incoming SPI dispatches correctly.
    unsafe {
        gic::route_spi(rtc_intid, CPU0_TARGET);
        gic::enable_ppi(rtc_intid);
        rtc_arm();
        exceptions::enable_irq();
    }

    // 8. Cooperative dispatch loop. The kthread parks in
    //    `block_until_ready`, yielding each iteration; while it spins the
    //    RTC SPI arrives, the EL1 IRQ path masks the line and sets the
    //    ready flag, and the kthread's next poll observes `Ready` and
    //    returns — draining the run queue.
    let mut steps = 0u64;
    while sched.live_task_count() != 0 && steps < MAX_STEPS {
        let _ = sched.step(BOOT_CPU);
        steps += 1;
    }
    if sched.live_task_count() != 0 {
        qemu_exit::exit_failure(FAIL_DEADLOCK);
    }

    // 9. The kthread exited; it must have done so because the SPI woke it.
    if !WOKEN.load(Ordering::SeqCst) {
        qemu_exit::exit_failure(FAIL_NOT_WOKEN);
    }

    // 10. Mask-before-wake evidence: the dispatcher's `IrqTable::fire`
    //     masked the GIC line through the bridge *before* it set the ready
    //     flag the kthread observed, so the line must now read disabled.
    if gic_line_enabled(rtc_intid) {
        qemu_exit::exit_failure(FAIL_NOT_MASKED);
    }

    note(
        TEST_PASS,
        "aarch64 device-SPI test: RTC SPI woke the parked kthread, line masked",
    );
    qemu_exit::exit_success();
}
