//! The freestanding aarch64 test kernel: prove the non-maskable FIQ watchdog
//! self-sample observes a `DAIF.I`-masked kernel busy-spin (`plans/WATCHDOG.md`
//! B3, `plans/OPEN-DEFECTS.md` D13).

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use tairix_arch_aarch64::kernel_arch::timer_frequency_hz;
use tairix_arch_aarch64::{
    exceptions, gic, handle_panic_via_serial, qemu_exit, watchdog, SERIAL_SINK,
};
use tairix_arch_api::{CpuId, FeatureSupport};
use tairix_fdt::Fdt;
use tairix_log::{log, Event, EventId, Level};

// The canonical QEMU `virt` device tree, dumped and embedded at build time.
include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

/// Watchdog cadence for the test: ~10 ms one-shot (`counter_hz / 100`), so a
/// non-maskable FIQ sample fires well within the masked busy-spin under QEMU
/// TCG without a slow ~1 s wait. Re-armed by `on_watchdog_interrupt`.
const CADENCE_DIVISOR: u64 = 100;

/// Address window (bytes) around the masked-spin marker function the sampled PC
/// must fall in. The marker is a tiny `#[inline(never)]` leaf, so a generous
/// 4 KiB window bounds it with margin while still rejecting an unrelated PC.
const SPIN_WINDOW: u64 = 0x1000;

/// Hard busy-spin cap: if no FIQ self-sample fires (the capability was wrongly
/// reported, or `DAIF.F` was not clear), the marker gives up so the post-loop
/// check reports a clear failure rather than relying on the harness timeout.
const MAX_SPINS: u64 = 40_000_000_000;

/// `SPSR_EL1.I` bit — the IRQ-mask state of the interrupted context. If set,
/// the sampled code was running with `DAIF.I` masked, the property under test.
const SPSR_I_MASK_BIT: u64 = 1 << 7;

/// Set by the FIQ self-sample callback once it captures a live sample.
static SAMPLED: AtomicBool = AtomicBool::new(false);

/// The live interrupted PC (`ELR_EL1`) the self-sample captured.
static SAMPLE_PC: AtomicU64 = AtomicU64::new(0);

/// The live interrupted `SPSR_EL1` the self-sample captured (carries the
/// interrupted context's IRQ-mask bit and exception level).
static SAMPLE_SPSR: AtomicU64 = AtomicU64::new(0);

/// The first frame of the captured `capture_sample_backtrace` (which records
/// the interrupted PC first), plus its length, to prove the live-stack unwind
/// ran and named the same PC.
static SAMPLE_BT0: AtomicU64 = AtomicU64::new(0);
static SAMPLE_BT_LEN: AtomicUsize = AtomicUsize::new(0);

/// Set once the scenario has been driven so a re-entry cannot re-run it.
static TEST_DRIVEN: AtomicBool = AtomicBool::new(false);

/// Stable audit-event ids for the QEMU transcript.
const TEST_START: EventId = EventId(4330);
const TEST_PROBED: EventId = EventId(4331);
const TEST_PASS: EventId = EventId(4332);

/// Semihosting failure codes, distinct per failure site.
const FAIL_REENTRY: u16 = 1;
const FAIL_FDT: u16 = 2;
const FAIL_ZERO_FREQ: u16 = 3;
const FAIL_GIC: u16 = 4;
const FAIL_PROBE_UNSUPPORTED: u16 = 5;
const FAIL_NO_SAMPLE: u16 = 6;
const FAIL_IRQ_NOT_MASKED: u16 = 7;
const FAIL_NOT_KERNEL: u16 = 8;
const FAIL_PC_OUT_OF_RANGE: u16 = 9;
const FAIL_BT_EMPTY: u16 = 10;
const FAIL_BT_MISMATCH: u16 = 11;

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
fn fiq_selfsample_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_serial(info)
}

/// Read the current `DAIF` mask state.
fn read_daif() -> u64 {
    let daif: u64;
    // SAFETY: reading `DAIF` has no side effects and is always permitted at EL1.
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack, preserves_flags));
    }
    daif
}

/// Mask `DAIF.I` (IRQ) on the calling CPU, leaving `DAIF.F` (FIQ) untouched.
fn mask_irq() {
    // SAFETY: setting the IRQ-mask bit is always permitted at EL1 and touches
    // no memory. `daifset` immediate bit 1 is the I (IRQ) bit.
    unsafe {
        core::arch::asm!("msr daifset, #2", options(nomem, nostack, preserves_flags));
    }
}

/// The watchdog cadence callback the port's FIQ dispatcher invokes on every
/// Group-0 self-sample. It captures a **live** snapshot of the interrupted
/// context — exactly what the production `production_watchdog_dispatch` reads —
/// and records it for the post-run assertions. A `fn` with no captured
/// environment, safe to invoke from interrupt context.
extern "C" fn fiq_sample(_cpu: CpuId, frame: *const u64) {
    // The interrupted PC / PSTATE (valid until the handler's `eret`).
    let pc = watchdog::read_elr_el1();
    let spsr = watchdog::read_spsr_el1();
    SAMPLE_PC.store(pc, Ordering::Relaxed);
    SAMPLE_SPSR.store(spsr, Ordering::Relaxed);

    // Unwind the live interrupted stack (fail-closed, bounded, never faults).
    let mut bt = [0u64; 8];
    // SAFETY: `frame` is the live saved register frame the FIQ dispatcher
    // forwarded from `tairix_aarch64_trap_common`, so the backtrace indices
    // are in range and the walk runs over this CPU's own kernel stack.
    let n = unsafe { watchdog::capture_sample_backtrace(frame, &mut bt) };
    SAMPLE_BT0.store(*bt.first().unwrap_or(&0), Ordering::Relaxed);
    SAMPLE_BT_LEN.store(n, Ordering::Relaxed);

    // Publish last, release: the marker's acquire load then sees all fields.
    SAMPLED.store(true, Ordering::Release);
}

/// Busy-spin, issuing no yield and no syscall, until the FIQ self-sample fires.
///
/// Called with `DAIF.I` masked, so the *only* thing that can interrupt it is a
/// non-maskable FIQ — the D13 masked-section wedge shape. `#[inline(never)]`
/// and `extern "C"` so it owns a stable, addressable body the sampled PC must
/// fall within. Returns the iteration count (unused; keeps the loop live).
#[inline(never)]
extern "C" fn masked_spin() -> u64 {
    let mut spins = 0u64;
    while !SAMPLED.load(Ordering::Acquire) {
        spins += 1;
        if spins >= MAX_SPINS {
            break;
        }
        core::hint::spin_loop();
    }
    spins
}

/// Boot entry point — the symbol the arch crate's boot trampoline calls.
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
        "aarch64 FIQ masked-section self-sample test: starting",
    );

    // 1. Discover the board from the embedded `virt` device tree: GICv2 bases
    //    and the generic-timer rate (no hard-coded board constants).
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

    // 2. EL1 vector table + GICv2 bring-up.
    // SAFETY: called once on the boot CPU with a stack established and before
    // any interrupt source is armed.
    unsafe {
        exceptions::init_vectors();
        gic::init();
    }

    // 3. Probe non-secure FIQ (Group 0) deliverability — the production,
    //    empirical, fail-closed capability the D13 masked-section sampler
    //    consumes. On the QEMU `virt` single-Security-state GIC this is
    //    `Supported`; on a two-Security-state GIC (a real Pi 4 GIC-400) it is
    //    `Unsupported` and the masked-section sampler is genuinely
    //    unavailable, so this test cannot run there (the shippable buddy
    //    detector is the design for that hardware).
    // SAFETY: boot CPU during bring-up; the GIC is up (`gic::init`) and the
    // vector table is installed (`init_vectors`), with IRQs still masked.
    let support = unsafe { watchdog::probe_fiq_deliverability(counter_hz) };
    if !matches!(support, FeatureSupport::Supported) {
        qemu_exit::exit_failure(FAIL_PROBE_UNSUPPORTED);
    }
    note(
        TEST_PROBED,
        "aarch64 FIQ self-sample test: Group-0/FIQ deliverability Supported",
    );

    // 4. Install the self-sample callback and arm a short Group-0 (FIQ)
    //    cadence on this CPU. `init_local_watchdog` routes the cadence PPI to
    //    Group 0 (the probe confirmed delivery) and clears `DAIF.F` so the FIQ
    //    can fire in thread-mode kernel code.
    watchdog::set_watchdog_callback(fiq_sample);
    let interval = (counter_hz / CADENCE_DIVISOR).max(1);
    // SAFETY: the GIC and vector table are up (steps 2/3), the callback is
    // installed (above), and the probe already enabled Group 0 as FIQ on this
    // CPU; this records the cadence interval, enables the PPI, arms the
    // one-shot, and routes it to Group 0.
    unsafe {
        watchdog::init_local_watchdog(interval);
    }

    // 5. Mask `DAIF.I` and busy-spin. Boot already runs IRQ-masked, but mask
    //    explicitly so the property under test is intentional and airtight: the
    //    only interrupt that can fire in the marker is a non-maskable FIQ.
    mask_irq();
    let spin_lo = masked_spin as extern "C" fn() -> u64 as usize as u64;
    let _spins = masked_spin();

    // 6. The FIQ self-sample must have fired while the marker spun.
    if !SAMPLED.load(Ordering::Acquire) {
        qemu_exit::exit_failure(FAIL_NO_SAMPLE);
    }
    let spsr = SAMPLE_SPSR.load(Ordering::Relaxed);
    let pc = SAMPLE_PC.load(Ordering::Relaxed);
    let bt0 = SAMPLE_BT0.load(Ordering::Relaxed);
    let bt_len = SAMPLE_BT_LEN.load(Ordering::Relaxed);

    // (b) The interrupted context had `DAIF.I` masked — the FIQ reached a
    //     section the maskable IRQ cadence never could (the D13 blind spot).
    if spsr & SPSR_I_MASK_BIT == 0 {
        qemu_exit::exit_failure(FAIL_IRQ_NOT_MASKED);
    }
    // (c) The sample interrupted kernel (EL1) context, not a user task.
    if !watchdog::spsr_in_kernel(spsr) {
        qemu_exit::exit_failure(FAIL_NOT_KERNEL);
    }
    // (d) The sampled PC lands inside the masked-spin marker — the self-sample
    //     named the exact section the core was stuck in (`sampled=live`).
    if pc < spin_lo || pc >= spin_lo.wrapping_add(SPIN_WINDOW) {
        qemu_exit::exit_failure(FAIL_PC_OUT_OF_RANGE);
    }
    // The live-stack unwind ran and recorded the interrupted PC as its top
    // frame (`capture_sample_backtrace` records `ELR_EL1` first).
    if bt_len == 0 {
        qemu_exit::exit_failure(FAIL_BT_EMPTY);
    }
    if bt0 != pc {
        qemu_exit::exit_failure(FAIL_BT_MISMATCH);
    }

    // Sanity: `DAIF.I` is still masked at this point (we never unmasked it),
    // proving the FIQ fired *through* the mask rather than after it lifted.
    let _ = read_daif();

    note(
        TEST_PASS,
        "aarch64 FIQ self-sample test: live sample named the DAIF.I-masked spin",
    );
    qemu_exit::exit_success();
}
