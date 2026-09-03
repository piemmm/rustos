//! aarch64 lockup-watchdog cadence timer and cross-CPU recovery.
//!
//! The aarch64 port of the Arch HAL watchdog surface
//! ([`tairix_arch_api::watchdog`]). It drives the architecture-neutral
//! detector in `kernel/core` with a periodic *liveness sample* and raises
//! the cross-CPU recovery signal a detected lockup asks for.
//!
//! # The cadence sample
//!
//! Hard-lockup detection needs a heartbeat that *stops* when a CPU stops
//! taking interrupts, sampled often enough that a multi-second threshold
//! has margin. This module arms the EL1 **virtual** generic timer
//! (`CNTV_*_EL0`, GIC PPI `WATCHDOG_PPI`) as a ~1 Hz one-shot on every
//! online CPU — a channel independent of the physical-timer one-shot the
//! tickless preemption path owns (`crate::preempt`), so the two never
//! interfere. It is programmed through the *relative* down-counter
//! `CNTV_TVAL_EL0`, so it needs no absolute virtual-count offset
//! (`CNTVOFF_EL2`, UNKNOWN at boot) — only "fire this many ticks from
//! now".
//!
//! On the QEMU `virt` board and the Raspberry Pi 4 the kernel runs at EL1
//! **non-secure** on a **GICv2**, where FIQ (Group 0) is the secure-world
//! interrupt a non-secure kernel cannot route to. So the sample is
//! delivered as an ordinary **IRQ**, and hard-lockup detection is the
//! cross-CPU *buddy* kind: a CPU that stops taking its watchdog IRQ is
//! observed by another CPU that is still taking its own. This is the
//! correct and complete detector for GICv2 non-secure, where FIQ (the
//! only non-maskable channel) belongs to the secure world. A board that
//! *does* expose a non-maskable channel (a GICv3 core with `ICC_PMR`
//! priority masking) can deliver this same sample as a true pseudo-NMI
//! behind the unchanged HAL surface, with no `kernel/core` change.
//!
//! The interrupt is dispatched by [`crate::exceptions`]' IRQ path, which
//! recognises `WATCHDOG_PPI`, calls `on_watchdog_interrupt` (re-arm +
//! invoke the installed callback), and runs the GIC end-of-interrupt
//! handshake. The installed callback (a bin-supplied `extern "C"
//! fn(CpuId)`, the layering-clean analogue of the timer callback) reads
//! the interrupted `ELR_EL1`/`SPSR_EL1` through `read_elr_el1` /
//! `read_spsr_el1`, builds the neutral sample, and forwards it to
//! `kernel/core`'s `on_watchdog_tick`.
//!
//! # Recovery
//!
//! `Watchdog` implements [`tairix_arch_api::WatchdogArch`]: a soft
//! lockup is met with a reschedule IPI (the reschedule SGI
//! [`crate::preempt::IPI_SGI`]) so the offending CPU re-enters the
//! scheduler; a hard lockup is met with the same directed SGI as a
//! best-effort attention signal — a CPU that can still take an IRQ
//! recovers, and one that genuinely cannot is left for the loud report
//! the detector already emitted (honest, never a silent no-op).

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tairix_arch_api::{
    CpuId, FeatureSupport, RecoveryOutcome, StuckInterrupt, WatchdogArch, WatchdogKind,
};

/// GIC INTID of the EL1 **virtual** generic-timer private-peripheral
/// interrupt (the ARM Generic Timer raises the virtual timer on PPI 27).
/// Distinct from the physical-timer PPI [`crate::preempt::TIMER_PPI`] (30)
/// the preemption path uses, so the watchdog and preemption timers are
/// independent.
pub const WATCHDOG_PPI: u32 = 27;

/// GIC priority byte for the debug watchdog self-sample when it is routed
/// to **Group 0 (FIQ)**.
///
/// A numerically *higher* value than the mid-range `0x80`
/// [`crate::gic::enable_ppi`] gives every other PPI (including the
/// preemption-timer IRQ [`crate::preempt::TIMER_PPI`]), i.e. a strictly
/// *lower* priority. This is load-bearing on a GICv2 CPU interface running
/// with `GICC_CTLR.FIQEn` set: when the highest-priority *pending*
/// interrupt is a Group-0 (FIQ) line and FIQ is masked (`DAIF.F`, e.g.
/// while an EL0 task runs), the interface withholds the FIQ **and** will
/// not signal a lower-or-equal-priority Group-1 IRQ behind it — so an
/// equal-priority watchdog FIQ left pending-and-masked would hold off the
/// preemption-timer IRQ and stall scheduling entirely. Making the
/// observational self-sample strictly lower priority than the timer means a
/// pending, masked watchdog FIQ can never block preemption. It stays below
/// the fully-open `GICC_PMR` (`0xFF`) so it is still delivered, and a
/// Group-0 FIQ is signalled independently of any pending Group-1 IRQ, so
/// the masked-section self-sample still fires in the `DAIF.I`-masked kernel
/// wedge it exists to observe.
#[cfg(any(
    test,
    all(
        target_arch = "aarch64",
        target_os = "none",
        feature = "watchdog-diagnostics"
    )
))]
pub const WATCHDOG_FIQ_PRIORITY: u8 = 0xC0;

// Compile-time guard for the invariant WATCHDOG_FIQ_PRIORITY documents: the
// self-sample FIQ must be a strictly lower priority (numerically greater) than
// the mid-range priority the preemption-timer IRQ gets, yet stay below the
// fully-open GICC_PMR (0xFF) so it is still delivered. Regressing the ordering
// (e.g. back to an equal priority) fails the build here rather than silently
// reintroducing the stress-load preemption stall.
#[cfg(any(
    test,
    all(
        target_arch = "aarch64",
        target_os = "none",
        feature = "watchdog-diagnostics"
    )
))]
const _: () =
    assert!(WATCHDOG_FIQ_PRIORITY > crate::gic::MID_RANGE_PRIORITY && WATCHDOG_FIQ_PRIORITY < 0xFF);

/// `CNTV_CTL_EL0.ENABLE` (bit 0): start the virtual timer counting down.
pub const CNTV_CTL_ENABLE: u64 = 1 << 0;

/// `CNTV_CTL_EL0.IMASK` (bit 1): when set, the timer does not raise its
/// interrupt. Left clear so the timer condition reaches the GIC.
pub const CNTV_CTL_IMASK: u64 = 1 << 1;

/// The cadence interval in counter ticks (`0` until
/// `init_local_watchdog` records it). Uniform across CPUs — every core
/// shares one `CNTFRQ_EL0` — so one global copy, not a per-CPU slice.
static WATCHDOG_INTERVAL_TICKS: AtomicU64 = AtomicU64::new(0);

/// The callback the watchdog IRQ path forwards each cadence sample to,
/// packed into a `usize` so the path swaps it in without a lock. Set up
/// before the watchdog is armed; absent (`0`) the sample is a no-op (the
/// timer still re-arms), so an image that arms the watchdog without wiring
/// the detector simply keeps sampling harmlessly (fail-safe).
static WATCHDOG_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// The signature of the watchdog cadence callback: the sampled CPU's
/// [`CpuId`] and a pointer to the saved exception-register `frame` the trap
/// trampoline built (`x0`..`x30` at indices `0..=30`, plus `ELR`/`SPSR`/
/// `SP_EL0`). The frame lets the callback unwind the *interrupted* context
/// (its `x29`/`ELR`), which the live registers no longer hold once the
/// handler is running — the source of the pre-silence backtrace a
/// hard-lockup report renders.
pub type WatchdogCallbackFn = extern "C" fn(CpuId, *const u64);

/// Install the watchdog cadence callback.
///
/// Invoked from the watchdog IRQ path on every cadence sample with the
/// CPU's [`CpuId`] and the interrupted register `frame`. Storing a `fn`
/// (not a closure) keeps it safe to call from interrupt context: there is
/// no captured environment to drop mid-flight.
pub fn set_watchdog_callback(cb: WatchdogCallbackFn) {
    WATCHDOG_CALLBACK_FN.store(cb as usize, Ordering::Relaxed);
}

/// Read the currently-installed watchdog callback, if any. Test/diagnostic.
#[must_use]
pub fn watchdog_callback() -> Option<WatchdogCallbackFn> {
    let raw = WATCHDOG_CALLBACK_FN.load(Ordering::Relaxed);
    if raw == 0 {
        None
    } else {
        // SAFETY: every store into `WATCHDOG_CALLBACK_FN` round-trips a
        // valid `WatchdogCallbackFn` pointer through
        // `set_watchdog_callback`.
        Some(unsafe { core::mem::transmute::<usize, WatchdogCallbackFn>(raw) })
    }
}

/// Maximum frame-pointer links [`capture_sample_backtrace`] follows past
/// the interrupted PC before giving up, independent of the caller's buffer:
/// a hard bound so a corrupt chain can never spin the handler.
#[cfg(any(
    test,
    all(
        target_arch = "aarch64",
        target_os = "none",
        feature = "watchdog-diagnostics"
    )
))]
const MAX_BACKTRACE_WALK: usize = 64;

/// Return `true` iff `addr` currently translates as an EL1 stage-1
/// **readable** address.
///
/// Uses the `AT S1E1R` address-translation instruction and `PAR_EL1.F`
/// (bit 0: `0` = translation succeeded, `1` = fault) to prove a stack link
/// is mapped *before* [`capture_sample_backtrace`] dereferences it, so the
/// frame-pointer walk can never fault inside the watchdog interrupt handler
/// even when the interrupted context left a non-zero, aligned, yet unmapped
/// `x29` (early-boot assembly, a task entry trampoline, a corrupt stack).
/// `AT` only *translates* `addr`; it never reads the memory and cannot
/// fault. `PAR_EL1` is saved and restored so a translation the interrupted
/// context had in flight is never clobbered by the sample. Fail-closed: any
/// translation fault reports not-readable and the walk stops.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
unsafe fn el1_readable(addr: u64) -> bool {
    !crate::paging::par_faulted(crate::paging::translate_el1(addr, false))
}

/// The pure frame-pointer walk shared by [`capture_sample_backtrace`] and
/// its host tests. `pc` is recorded verbatim as `out[0]` (the interrupted
/// return PC is always authoritative); each caller's return address is then
/// followed through the AAPCS64 `x29` chain and recorded **only if it
/// passes every validity check**, so a bogus or stale frame record yields a
/// short honest chain rather than a plausible-looking garbage one.
///
/// All access to the interrupted stack is injected so this core is
/// host-testable with a fake stack and carries no `asm!`/linker dependency:
/// `readable(fp)` proves the 16-byte record at `fp` is safe to read;
/// `read_pair(fp)` returns `([fp], [fp+8])` = `(saved_fp, return_addr)`;
/// `in_text(ret)` proves `ret` lands in the kernel's executable text, which
/// is what rejects a stack data word misread as a return address.
///
/// Fail closed, never looping: the walk stops at a frame pointer that is
/// not strictly above `stack_floor` (the exception frame sits at the lowest
/// live stack address; every real caller is higher), a misaligned pointer,
/// an unmapped record, a null or non-text return address, a
/// non-strictly-increasing saved frame pointer, a full `out`, or
/// [`MAX_BACKTRACE_WALK`] links, whichever comes first.
#[cfg(any(
    test,
    all(
        target_arch = "aarch64",
        target_os = "none",
        feature = "watchdog-diagnostics"
    )
))]
fn walk_frames(
    pc: u64,
    mut fp: u64,
    stack_floor: u64,
    out: &mut [u64],
    mut readable: impl FnMut(u64) -> bool,
    mut read_pair: impl FnMut(u64) -> (u64, u64),
    in_text: impl Fn(u64) -> bool,
) -> usize {
    if out.is_empty() {
        return 0;
    }
    out[0] = pc;
    let mut n = 1;
    let mut steps = 0;
    while n < out.len() && steps < MAX_BACKTRACE_WALK {
        steps += 1;
        // A genuine frame record sits strictly above the exception frame on
        // the same downward-growing kernel stack and is 16-byte aligned;
        // anything else is a leaf/mid-prologue `x29` or corruption.
        if fp <= stack_floor || (fp & 0xf) != 0 {
            break;
        }
        if !readable(fp) {
            break;
        }
        let (next_fp, ret) = read_pair(fp);
        // A real return address is non-zero and lands in kernel text.
        // Rejecting a non-text word is what stops the walk emitting a stack
        // data slot as if it were a caller — the cause of the old
        // interleaved, untrustworthy chains.
        if ret == 0 || !in_text(ret) {
            break;
        }
        out[n] = ret;
        n += 1;
        // Callers sit at higher addresses; a non-increasing link is corrupt
        // or the stack base.
        if next_fp <= fp {
            break;
        }
        fp = next_fp;
    }
    n
}

// The kernel's executable-text bounds, from the linker-provided
// `__text_start`/`__text_end` symbols the aarch64 linker scripts bracket
// `.text` with. A watchdog backtrace accepts a return address only inside
// this range (see `in_kernel_text`).
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
extern "C" {
    static __text_start: u8;
    static __text_end: u8;
}

/// Return `true` iff `addr` lies within the kernel's executable text.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
fn in_kernel_text(addr: u64) -> bool {
    // SAFETY: taking the address of a linker-defined symbol is always
    // sound; `__text_start`/`__text_end` bracket the `.text` the linker
    // script emitted. They are never dereferenced.
    let start = core::ptr::addr_of!(__text_start) as u64;
    let end = core::ptr::addr_of!(__text_end) as u64;
    addr >= start && addr < end
}

/// Unwind the AAPCS64 frame-pointer chain of the context the saved `frame`
/// interrupted into `out` (innermost first: the interrupted PC, then each
/// validated caller's return address), returning how many entries were
/// written.
///
/// Runs on the sampled CPU itself, over the kernel stack the interrupted
/// context was executing on. The interrupted PC (`ELR_EL1`) is recorded
/// verbatim; each caller frame is followed and validated by [`walk_frames`]
/// — mapped (`AT S1E1R`, [`el1_readable`]), aligned, strictly above the
/// exception frame and strictly increasing, with a return address inside
/// the kernel's executable text ([`in_kernel_text`]) — so the walk is
/// **fail-closed and bounded**, never faults inside the interrupt handler,
/// and never emits a stack data word as if it were a caller.
///
/// # Safety
///
/// `frame` must be the live `[u64; …]` saved register frame `vectors.s`
/// built for this exception (so [`crate::exceptions::ELR_FRAME_INDEX`] and
/// [`crate::exceptions::FP_FRAME_INDEX`] are in range), and the caller must
/// only pass a frame whose interrupted context ran on *this* CPU's stack.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
pub unsafe fn capture_sample_backtrace(frame: *const u64, out: &mut [u64]) -> usize {
    if out.is_empty() || frame.is_null() {
        return 0;
    }
    // SAFETY: the caller guarantees `frame` is the live saved register
    // frame, so these indices are in range.
    let (pc, fp) = unsafe {
        (
            *frame.add(crate::exceptions::ELR_FRAME_INDEX),
            *frame.add(crate::exceptions::FP_FRAME_INDEX),
        )
    };
    // The saved exception frame sits at the lowest in-use address of this
    // CPU's kernel stack; every genuine caller frame is above it (the stack
    // grows down), so `frame` is the walk's stack floor.
    let stack_floor = frame as u64;
    walk_frames(
        pc,
        fp,
        stack_floor,
        out,
        // SAFETY: `el1_readable` only translates `addr` (`AT S1E1R`); it
        // never reads the memory and cannot fault.
        |addr| unsafe { el1_readable(addr) },
        // SAFETY: `walk_frames` calls this only after the `readable` seam
        // above proved `addr` mapped and readable at EL1, and `addr` is
        // 16-aligned, so `[addr]`/`[addr+8]` lie within its page and cannot
        // fault. AAPCS64 stores the caller's saved `x29` at `[addr]` and
        // the return address at `[addr+8]`.
        |addr| unsafe {
            let p = addr as *const u64;
            (*p, *p.add(1))
        },
        in_kernel_text,
    )
}

/// The recorded cadence interval in counter ticks (`0` if unset).
/// Test/diagnostic observer.
#[must_use]
pub fn watchdog_interval_ticks() -> u64 {
    WATCHDOG_INTERVAL_TICKS.load(Ordering::Relaxed)
}

/// `true` iff a saved `SPSR_EL1` describes an interrupted **kernel**
/// (EL1) context rather than an EL0 user task.
///
/// `SPSR_EL1.M[3:2]` holds the exception level of the interrupted state;
/// `0b00` is EL0. Any higher value means the sample interrupted kernel
/// code, which owes the scheduler progress even when it is the only
/// runnable context — the distinction the soft-lockup check needs to avoid
/// flagging a legitimate lone user task.
#[must_use]
pub const fn spsr_in_kernel(spsr: u64) -> bool {
    ((spsr >> 2) & 0b11) != 0
}

// --- Freestanding timer programming + dispatch ---------------------

/// Read the interrupted return PC `ELR_EL1` (valid throughout an
/// exception handler, until the `eret`).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn read_elr_el1() -> u64 {
    let elr: u64;
    // SAFETY: reading `ELR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, ELR_EL1", out(reg) elr, options(nomem, nostack, preserves_flags));
    }
    elr
}

/// Read the interrupted processor state `SPSR_EL1` (valid throughout an
/// exception handler, until the `eret`).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn read_spsr_el1() -> u64 {
    let spsr: u64;
    // SAFETY: reading `SPSR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, SPSR_EL1", out(reg) spsr, options(nomem, nostack, preserves_flags));
    }
    spsr
}

/// Arm the virtual timer one-shot to fire `interval` counter ticks from
/// now (relative `CNTV_TVAL_EL0`), with its interrupt unmasked.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn arm(interval: u64) {
    // SAFETY: `CNTV_TVAL_EL0`/`CNTV_CTL_EL0` are writable at EL1; setting
    // the relative down-counter and enabling the timer (IMASK clear) has
    // no effect beyond the system registers. The interval is clamped to at
    // least one tick so a degenerate `0` cannot arm the current instant
    // with no forward progress.
    unsafe {
        core::arch::asm!(
            "msr CNTV_TVAL_EL0, {interval}",
            "msr CNTV_CTL_EL0, {ctl}",
            interval = in(reg) interval.max(1),
            ctl = in(reg) CNTV_CTL_ENABLE,
            options(nomem, nostack),
        );
    }
}

/// Initialise the lockup watchdog on the calling CPU: record the cadence
/// `interval_ticks`, enable the virtual-timer PPI at the GIC, and arm the
/// first one-shot.
///
/// Unlike the tickless preemption timer this stays armed for the CPU's
/// lifetime — each sample re-arms the next ([`on_watchdog_interrupt`]) —
/// so every online CPU keeps a fresh liveness heartbeat and runs the
/// cross-CPU scan even when idle. The ~1 Hz cadence costs one timer
/// interrupt per second per core, negligible against normal execution.
///
/// # Safety
///
/// * `interval_ticks` must be the counter-tick count for the cadence
///   (`CNTFRQ_EL0` for ~1 s).
/// * The GIC must be initialised ([`crate::gic::init`]) and the vector
///   table installed ([`crate::exceptions::init_vectors`]); the caller
///   unmasks IRQs separately ([`crate::exceptions::enable_irq`]).
/// * The watchdog callback should be installed ([`set_watchdog_callback`])
///   first, though an absent callback is a fail-safe no-op.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn init_local_watchdog(interval_ticks: u64) {
    WATCHDOG_INTERVAL_TICKS.store(interval_ticks.max(1), Ordering::Relaxed);
    // SAFETY: the GIC distributor is enabled by the caller's contract;
    // enabling the virtual-timer PPI lets the armed one-shot reach the CPU.
    unsafe {
        crate::gic::enable_ppi(WATCHDOG_PPI);
    }
    arm(interval_ticks);
    // Debug watchdog: clear `DAIF.F` on this CPU so the non-maskable
    // Group-0/FIQ self-sample can fire in thread-mode kernel code
    // (`plans/WATCHDOG.md`) — the D13 `stress --cpu N` wedge lives in an
    // IRQ-masked thread-mode busy-spin the maskable IRQ cadence cannot
    // observe. Inert until a Group-0 source is routed, and compiled out of
    // shippable images.
    #[cfg(feature = "watchdog-diagnostics")]
    // SAFETY: the GIC and vector table are installed per this fn's
    // contract, so a taken FIQ dispatches through a real slot; clearing
    // `DAIF.F` only changes the FIQ mask.
    unsafe {
        crate::exceptions::enable_fiq_delivery();
    }
    // If the boot probe confirmed FIQ deliverability, route this CPU's
    // cadence PPI to Group 0 so its sample fires as a non-maskable FIQ,
    // observing a core wedged in a `DAIF.I`-masked section (`plans/
    // WATCHDOG.md`). Otherwise the PPI stays Group 1 (IRQ) and the
    // cross-CPU buddy detector runs unchanged (fail closed). The group and
    // CPU-interface control are banked per CPU, so every online core routes
    // its own; the distributor's Group-0 enable was set once by the probe.
    #[cfg(feature = "watchdog-diagnostics")]
    if fiq_cadence_enabled() {
        // SAFETY: GIC + vectors up per this fn's contract; this configures
        // only this CPU's banked group bit and CPU-interface control.
        unsafe {
            route_watchdog_group0();
        }
    }
}

/// Handle a virtual-timer watchdog interrupt on `cpu`: re-arm the next
/// one-shot and invoke the installed cadence callback.
///
/// Called only from [`crate::exceptions`]' IRQ path on [`WATCHDOG_PPI`],
/// with interrupts masked (the PE masked them on exception entry) and
/// before the GIC end-of-interrupt handshake. Re-arming first guarantees
/// the cadence continues even if the callback path is heavy. An absent
/// callback re-arms and returns (fail-safe).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub(crate) fn on_watchdog_interrupt(cpu: CpuId, frame: *const u64) {
    arm(WATCHDOG_INTERVAL_TICKS.load(Ordering::Relaxed));
    let raw = WATCHDOG_CALLBACK_FN.load(Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: every store into `WATCHDOG_CALLBACK_FN` round-trips a
        // valid `WatchdogCallbackFn` through `set_watchdog_callback`; the
        // callback carries no captured environment. `frame` is forwarded
        // from the trap handler's live saved register frame.
        let cb: WatchdogCallbackFn =
            unsafe { core::mem::transmute::<usize, WatchdogCallbackFn>(raw) };
        cb(cpu, frame);
    }
}

// --- Debug-only non-maskable FIQ masked-section self-sample -------------
//
// The buddy detector below is the shippable, complete design. For the debug
// image (`watchdog-diagnostics`) only, a non-maskable FIQ self-sample is
// added beside it to observe a core wedged in a `DAIF.I`-masked section the
// maskable IRQ cadence cannot see (the D13 `stress --cpu N` class,
// `plans/OPEN-DEFECTS.md`). Whether Group 0 / FIQ actually reaches the
// non-secure kernel is platform/firmware-owned — deliverable on a
// single-Security-state GIC (measured: QEMU `virt` with `secure=off`, the
// board default), secure on a two-Security-state GIC (QEMU `virt,secure=on`
// or a real Pi 4 GIC-400, where Group 0 belongs to the secure world) — so
// it is decided by an *empirical, fail-closed* boot probe and reported
// through the Arch-HAL capability honesty vocabulary
// (`plans/FIX-HARDWARE-FEATURES.md`). The watchdog is a consumer that
// chooses the FIQ cadence over the buddy detector from it.

/// Map the empirical FIQ-deliverability probe's outcome to the Arch-HAL
/// capability vocabulary. `delivered` is whether an FIQ was actually taken
/// in the deliberately `DAIF.I`-masked probe window.
///
/// A `true` outcome is [`FeatureSupport::Supported`]: Group 0 / FIQ reaches
/// the non-secure kernel, so the watchdog delivers its cadence as a
/// non-maskable self-sample. A `false` outcome is
/// [`FeatureSupport::Unsupported`] with the reason — on a two-Security-state
/// GIC that keeps Group 0 (FIQ) in the secure world (QEMU `virt,secure=on`
/// or a real Raspberry Pi 4 GIC-400) a non-secure kernel genuinely has no
/// such source, so the watchdog stays on the complete cross-CPU buddy
/// detector with no broken channel (fail closed). A single-Security-state
/// GIC (measured: the QEMU `virt` default, `secure=off`) delivers it, so the
/// probe returns `Supported` there and the debug image self-samples.
///
/// This decision is pure and always compiled (host-tested); the metal
/// probe that produces `delivered` is debug-only.
#[must_use]
pub const fn fiq_support_from_probe(delivered: bool) -> FeatureSupport {
    if delivered {
        FeatureSupport::Supported
    } else {
        FeatureSupport::Unsupported(
            "non-secure FIQ (Group 0) is not delivered to EL1 on this GIC; \
             the cross-CPU buddy watchdog is used instead",
        )
    }
}

/// The cadence PPI is delivered as a non-maskable FIQ only once this many
/// microarchitectural readings confirm delivery; the boot probe reads it.
/// `false` until an FIQ is actually taken (the deliverability probe clears
/// it, arms a Group-0 cadence, and waits on it).
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
static FIQ_TAKEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Cached FIQ-deliverability capability: `0` unprobed, `1` deliverable
/// (Supported), `2` not deliverable (Unsupported). Read by
/// [`fiq_deliverability`] / [`fiq_cadence_enabled`] after the boot probe.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
static FIQ_DELIVERABLE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Record that an FIQ was actually taken on this CPU. Called from the FIQ
/// dispatcher arm ([`crate::exceptions`]) for every FIQ, so the boot
/// deliverability probe observes delivery. One relaxed store.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
pub(crate) fn note_fiq_taken() {
    FIQ_TAKEN.store(true, Ordering::Relaxed);
}

/// The FIQ-deliverability capability decided by the boot probe, reported
/// through the Arch-HAL honesty vocabulary. [`FeatureSupport::Pending`]
/// before the probe has run.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
#[must_use]
pub fn fiq_deliverability() -> FeatureSupport {
    match FIQ_DELIVERABLE.load(Ordering::Relaxed) {
        1 => fiq_support_from_probe(true),
        2 => fiq_support_from_probe(false),
        _ => FeatureSupport::Pending("non-secure FIQ deliverability not yet probed"),
    }
}

/// `true` iff the boot probe *proved* a non-maskable FIQ is deliverable to
/// the non-secure kernel on this hardware (`FIQ_DELIVERABLE == 1`).
///
/// This is the single predicate that gates the debug build's entire
/// `DAIF.F`-unmask discipline, and it must be consulted at **run time**, not
/// approximated by the compile-time feature. Three consumers ask it:
/// [`init_local_watchdog`] (whether to route the cadence PPI to Group 0),
/// the syscall/fault handler (whether to leave `DAIF.F` clear for the
/// self-sample), and the `IrqSafeSpinLock` critical-section mask (likewise).
///
/// The feature being compiled in means the self-sample *code* exists; it does
/// **not** mean FIQ is the kernel's to take. On a two-Security-state GIC-400
/// (a Raspberry Pi 4, where Group 0 belongs to the secure world) the probe
/// returns `Unsupported` and this stays `false`, so the kernel keeps FIQ
/// masked exactly like a shippable build rather than exposing itself to
/// secure-world Group-0 FIQs it cannot service (fail closed). `false` until
/// the probe has run.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
pub fn fiq_cadence_enabled() -> bool {
    FIQ_DELIVERABLE.load(Ordering::Relaxed) == 1
}

/// Read the physical count `CNTPCT_EL0` (a free-running monotonic counter,
/// unaffected by `DAIF` masking) for the probe's bounded spin deadline.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
fn read_cntpct() -> u64 {
    let cnt: u64;
    // SAFETY: reading `CNTPCT_EL0` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, CNTPCT_EL0", out(reg) cnt, options(nomem, nostack, preserves_flags));
    }
    cnt
}

/// Route the watchdog cadence PPI to interrupt **Group 0** on the calling
/// CPU, so its sample is delivered as a non-maskable FIQ.
///
/// The interrupt-group word `GICD_IGROUPR0` and the CPU-interface control
/// `GICC_CTLR` are both **banked per CPU**, so every online core routes its
/// own; the distributor's global Group-0 enable is set once by the boot
/// probe. Called from [`init_local_watchdog`] only when
/// [`fiq_cadence_enabled`] (the probe confirmed delivery).
///
/// # Safety
///
/// The GIC must be initialised and the calling CPU's interface enabled
/// ([`crate::gic::init`]); the fixed windows are mapped and owned by the
/// kernel.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
unsafe fn route_watchdog_group0() {
    // SAFETY: GIC up per this fn's contract; this moves every interrupt
    // but the cadence PPI into Group 1 (IRQ), leaves the cadence PPI in
    // Group 0, and enables both groups + AckCtl + FIQEn on this CPU. Only
    // the cadence PPI is then delivered as an FIQ; the preemption timer and
    // device SPIs stay ordinary IRQs (without this the global FIQEn would
    // route them all to the unserviced FIQ vector and storm the core).
    unsafe {
        crate::gic::route_selfsample_fiq(WATCHDOG_PPI);
        // Drop the self-sample below the preemption timer's priority so a
        // pending-and-masked Group-0 FIQ can never hold off the timer IRQ
        // and stall scheduling (see [`WATCHDOG_FIQ_PRIORITY`]). Banked per
        // CPU, so set on every core that routes its own cadence to Group 0.
        crate::gic::set_ppi_priority(WATCHDOG_PPI, WATCHDOG_FIQ_PRIORITY);
    }
}

/// The bounded spin cap for [`probe_fiq_deliverability`]: a hard ceiling on
/// loop iterations that backs up the counter deadline so the probe can
/// never spin unbounded even with a broken counter (fail closed, §no-hacks).
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
const MAX_PROBE_SPINS: u64 = 100_000_000;

/// Probe, once on the boot CPU, whether the watchdog cadence can be
/// delivered as a **non-maskable FIQ** to the non-secure kernel, caching
/// and returning the capability.
///
/// Group 0 / FIQ routing to non-secure EL1 is platform/firmware-owned and
/// cannot be known a priori (it differs QEMU `virt` vs the Pi 4 GIC-400),
/// so this *measures* it: it routes the cadence PPI to Group 0, enables
/// Group 0 as FIQ, arms a short one-shot, deliberately masks `DAIF.I`
/// (leaving `DAIF.F` clear) so only a non-maskable FIQ can fire in the
/// window, and waits a **bounded** interval (a counter deadline backed by
/// [`MAX_PROBE_SPINS`]) for an FIQ to actually be taken
/// ([`note_fiq_taken`]). This is a hardware-handshake spin with a bounded
/// budget, not a steady-state busy-loop.
///
/// **Fail closed:** if no FIQ arrives (the secure-world case) the perturbed
/// group/enable state is restored *verbatim* from the values saved before
/// the probe — so the ordinary-IRQ enable bit is preserved on any GIC
/// security configuration — and the capability is
/// [`FeatureSupport::Unsupported`], leaving the complete cross-CPU buddy
/// detector in place with no broken channel. Idempotent: a second call
/// returns the cached capability.
///
/// # Safety
///
/// Must be called on the boot CPU during bring-up, after the GIC is up
/// ([`crate::gic::init`]) and the vector table is installed
/// ([`crate::exceptions::init_vectors`]), while the kernel still runs with
/// IRQs masked. `counter_hz` is `CNTFRQ_EL0` (used to size the short arm
/// and the deadline).
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
pub unsafe fn probe_fiq_deliverability(counter_hz: u64) -> FeatureSupport {
    // Idempotent: only probe once (the boot CPU).
    if FIQ_DELIVERABLE.load(Ordering::Relaxed) != 0 {
        return fiq_deliverability();
    }
    // A degenerate counter cannot size the probe window; fail closed.
    if counter_hz == 0 {
        FIQ_DELIVERABLE.store(2, Ordering::Relaxed);
        return fiq_deliverability();
    }
    // Record the cadence interval first, so an FIQ taken during the probe
    // re-arms the one-shot to ~1 s rather than a zero-tick storm.
    WATCHDOG_INTERVAL_TICKS.store(counter_hz, Ordering::Relaxed);

    // Save the interrupt-group / enable state the probe perturbs, to restore
    // verbatim on a fail-closed revert. The saved values already carry the
    // ordinary-IRQ enable bit, so restoring them is safe regardless of the
    // GIC's Security configuration.
    // SAFETY: GIC up per this fn's contract.
    let (saved_cpu_ctlr, saved_dist_ctlr) =
        unsafe { (crate::gic::read_gicc_ctlr(), crate::gic::read_gicd_ctlr()) };

    // SAFETY: GIC up. Route the cadence PPI to Group 0 (FIQ) on this CPU,
    // enable Group 0 at the distributor and CPU interface, and signal
    // Group 0 as FIQ. On a two-Security-state GIC these Secure-only bits are
    // RAO/WI from Non-secure EL1, so this is a harmless no-op there and the
    // wait below discovers FIQ is undeliverable.
    unsafe {
        crate::gic::enable_ppi(WATCHDOG_PPI);
        crate::gic::route_selfsample_fiq(WATCHDOG_PPI);
        // Same priority discipline as the steady-state routing: keep the
        // self-sample below the timer so probing never perturbs preemption
        // (see [`WATCHDOG_FIQ_PRIORITY`]).
        crate::gic::set_ppi_priority(WATCHDOG_PPI, WATCHDOG_FIQ_PRIORITY);
    }

    FIQ_TAKEN.store(false, Ordering::Relaxed);
    // Arm the cadence to fire very soon (~1 ms) so the probe window is short.
    arm((counter_hz / 1000).max(1));

    // Ensure FIQ is deliverable at the PE, then deliberately mask IRQ so
    // only a *non-maskable* FIQ can fire in the window. Boot already runs
    // IRQ-masked; save DAIF and restore it exactly afterwards.
    // SAFETY: clearing `DAIF.F` only unmasks FIQ; the vectors are installed.
    unsafe {
        crate::exceptions::enable_fiq_delivery();
    }
    let daif: u64;
    // SAFETY: reading DAIF and setting its IRQ-mask bit is always permitted
    // at EL1 and touches no memory.
    unsafe {
        core::arch::asm!(
            "mrs {0}, daif",
            "msr daifset, #{i}",
            out(reg) daif,
            i = const crate::exceptions::daif::I,
            options(nomem, nostack, preserves_flags),
        );
    }

    // Bounded wait: a counter deadline (~20 ms) backed by a hard iteration
    // cap. If an FIQ can reach us it fires within the ~1 ms arm above.
    let start = read_cntpct();
    let deadline = counter_hz / 50;
    let mut spins = 0u64;
    while !FIQ_TAKEN.load(Ordering::Relaxed) {
        if read_cntpct().wrapping_sub(start) >= deadline || spins >= MAX_PROBE_SPINS {
            break;
        }
        spins += 1;
        core::hint::spin_loop();
    }

    // Restore the exact prior IRQ-mask state.
    // SAFETY: writing back the captured DAIF value restores the prior mask.
    unsafe {
        core::arch::asm!("msr daif, {0}", in(reg) daif, options(nomem, nostack, preserves_flags));
    }

    let delivered = FIQ_TAKEN.load(Ordering::Relaxed);
    if delivered {
        FIQ_DELIVERABLE.store(1, Ordering::Relaxed);
    } else {
        // Fail closed: restore the perturbed group/enable state verbatim,
        // back to the buddy detector with no broken channel.
        // SAFETY: GIC up; these restore the saved register values.
        unsafe {
            crate::gic::set_group1(WATCHDOG_PPI);
            crate::gic::write_gicc_ctlr(saved_cpu_ctlr);
            crate::gic::write_gicd_ctlr(saved_dist_ctlr);
        }
        FIQ_DELIVERABLE.store(2, Ordering::Relaxed);
    }
    fiq_deliverability()
}

// --- Cross-CPU recovery -------------------------------------------

/// The aarch64 [`WatchdogArch`] recovery handle.
///
/// A zero-sized handle (the recovery mechanism is the GIC SGI path, which
/// holds no per-instance state), so it lives in a `static`
/// ([`AARCH64_WATCHDOG`]) the kernel installs by reference.
pub struct Watchdog;

/// The installed-by-reference recovery handle
/// ([`crate::kernel_arch::Aarch64Arch`] returns it to `kernel/core`).
pub static AARCH64_WATCHDOG: Watchdog = Watchdog;

impl WatchdogArch for Watchdog {
    fn request_recovery(&self, target: CpuId, kind: WatchdogKind) -> RecoveryOutcome {
        // Both a soft and a hard lockup are met with the directed
        // reschedule SGI: for a soft lockup it forces the offending CPU
        // back into the scheduler; for a hard lockup it is a best-effort
        // attention signal — a CPU still able to take an IRQ recovers, and
        // one that genuinely cannot is left for the loud report already
        // emitted (never a silent no-op). On a GICv2 non-secure kernel
        // there is no non-maskable channel to force a wedged core — that is
        // inherent to the hardware, and the loud cross-CPU report is the
        // complete answer for it (`plans/WATCHDOG.md`).
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            crate::gic::send_sgi(target);
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            let _ = target;
        }
        match kind {
            WatchdogKind::Soft => RecoveryOutcome::Rescheduled,
            WatchdogKind::Hard => RecoveryOutcome::AttentionRaised,
        }
    }

    fn stuck_interrupt(&self) -> Option<StuckInterrupt> {
        // The observer reads the distributor's globally-shared status: a
        // device SPI stuck active (its handler never completing, or the
        // line storming) is the "why" the hard-locked CPU's own stale
        // sample cannot give. SGIs/PPIs are banked per CPU and so are not
        // observable from here — only shared SPIs. Only a line that can
        // still reach a CPU is reported (active, or enabled-and-pending); a
        // masked line is skipped, since it cannot be the wedge. The reply's
        // active flag tells a live storm from an asserted-but-untaken line.
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            crate::gic::stuck_spi()
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            None
        }
    }

    fn in_flight_interrupt(&self, target: CpuId) -> tairix_arch_api::InFlightInterrupt {
        // The banked-line counterpart of `stuck_interrupt`'s shared-line
        // read: `GICD_ISACTIVER0` is banked per CPU, so an observer reading
        // it sees its *own* SGI/PPI state, never the wedged core's — and a
        // never-completed SGI or PPI is exactly what leaves that core's
        // interface running priority raised. The victim publishes what it
        // acknowledged at interrupt entry, so this reads it back.
        tairix_arch_api::watchdog::in_flight::read(target)
    }

    fn remote_pc_sample(&self, target: CpuId) -> tairix_arch_api::RemotePcSample {
        // The *code*-side "why" the stale pre-silence sample cannot give: a
        // read of the wedged core's PC over its discovered CoreSight
        // external-debug component (`EDPCSR`), which the victim cannot mask
        // and which does not halt it. Fails closed to `Unsupported` when no
        // debug base was discovered for `target` (the common case on a tree
        // that does not describe the debug components), so the detector keeps
        // the stale sample rather than a fabricated PC. The read is a pure
        // MMIO sequence (no lock, no block), safe from the sample path.
        crate::coresight::remote_pc_sample(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_ppi_is_the_virtual_timer_and_distinct_from_preemption() {
        assert_eq!(WATCHDOG_PPI, 27);
        assert_ne!(WATCHDOG_PPI, crate::preempt::TIMER_PPI);
    }

    #[test]
    fn cntv_ctl_bits_match_the_arm_spec() {
        assert_eq!(CNTV_CTL_ENABLE, 0b01);
        assert_eq!(CNTV_CTL_IMASK, 0b10);
    }

    #[test]
    fn spsr_el0_is_user_and_el1_is_kernel() {
        // SPSR_EL1.M[3:0]: 0b0000 = EL0t (user); 0b0101 = EL1h (kernel).
        assert!(!spsr_in_kernel(0b0000));
        assert!(spsr_in_kernel(0b0101));
        assert!(spsr_in_kernel(0b0100));
        // The mask/condition bits above M do not affect the verdict.
        assert!(!spsr_in_kernel(0x6000_0000));
        assert!(spsr_in_kernel(0x6000_0005));
    }

    #[test]
    fn callback_round_trips_through_the_slot() {
        extern "C" fn cb(_cpu: CpuId, _frame: *const u64) {}
        set_watchdog_callback(cb);
        let got = watchdog_callback().expect("callback installed");
        assert_eq!(got as usize, cb as *const () as usize);
        WATCHDOG_CALLBACK_FN.store(0, Ordering::Relaxed);
    }

    #[test]
    fn recovery_reports_the_signal_it_raised() {
        // On the host the SGI send is compiled out; the outcome still names
        // what a real send would have done for each kind.
        assert_eq!(
            AARCH64_WATCHDOG.request_recovery(1, WatchdogKind::Soft),
            RecoveryOutcome::Rescheduled
        );
        assert_eq!(
            AARCH64_WATCHDOG.request_recovery(1, WatchdogKind::Hard),
            RecoveryOutcome::AttentionRaised
        );
    }

    #[test]
    fn recovery_passes_the_arch_hal_conformance_vertical() {
        assert_eq!(
            tairix_arch_api::watchdog::conformance::run_all(&AARCH64_WATCHDOG, 0),
            Ok(())
        );
    }

    #[test]
    fn stuck_interrupt_is_none_off_metal() {
        // The distributor read is metal-only (it touches real GIC MMIO);
        // on the host the handle honestly reports no stuck line rather than
        // fabricating one, exactly as the recovery SGI compiles out.
        assert_eq!(AARCH64_WATCHDOG.stuck_interrupt(), None);
    }

    #[test]
    fn fiq_support_maps_the_probe_outcome_honestly() {
        // Delivered → Supported (release-ready), so the watchdog uses the
        // non-maskable FIQ cadence.
        assert_eq!(fiq_support_from_probe(true), FeatureSupport::Supported);
        assert!(fiq_support_from_probe(true).is_release_ready());
        // Not delivered → a justified Unsupported (release-ready, never
        // Pending): the port genuinely has no such source (secure-world
        // Group 0), and the buddy detector is used. The reason is non-empty.
        let no = fiq_support_from_probe(false);
        assert!(matches!(no, FeatureSupport::Unsupported(_)));
        assert!(no.is_release_ready());
        assert!(!no.is_pending());
        assert!(no.detail().is_some_and(|r| !r.trim().is_empty()));
    }

    // --- `walk_frames` (the pure backtrace core) --------------------------

    /// A fake kernel-text window for the tests: return addresses inside it
    /// are accepted, those outside (stack data words) are rejected.
    const TEXT_LO: u64 = 0x0040_0000;
    const TEXT_HI: u64 = 0x0050_0000;

    /// Build the `readable`/`read_pair` seams over a fake stack described as
    /// `(frame_addr, saved_fp, return_addr)` records. A frame pointer is
    /// "mapped" iff it names a record; reading it yields that record's
    /// `(saved_fp, return_addr)`.
    fn fake_stack(
        records: &[(u64, u64, u64)],
    ) -> (impl Fn(u64) -> bool + '_, impl Fn(u64) -> (u64, u64) + '_) {
        let readable = move |fp: u64| records.iter().any(|&(a, _, _)| a == fp);
        let read_pair = move |fp: u64| {
            records
                .iter()
                .find(|&&(a, _, _)| a == fp)
                .map_or((0, 0), |&(_, nf, ret)| (nf, ret))
        };
        (readable, read_pair)
    }

    #[test]
    fn walk_frames_records_pc_then_follows_a_valid_chain() {
        let records = [
            (0x1040_u64, 0x1080_u64, 0x0040_0100_u64),
            (0x1080, 0x10c0, 0x0040_0200),
            // Terminal frame: saved_fp == 0 stops the walk after its return
            // address is recorded.
            (0x10c0, 0x0, 0x0040_0300),
        ];
        let (readable, read_pair) = fake_stack(&records);
        let mut out = [0u64; 8];
        let n = walk_frames(
            0x0040_0050,
            0x1040,
            0x1000,
            &mut out,
            readable,
            read_pair,
            |a| (TEXT_LO..TEXT_HI).contains(&a),
        );
        assert_eq!(n, 4);
        assert_eq!(
            &out[..n],
            &[0x0040_0050, 0x0040_0100, 0x0040_0200, 0x0040_0300]
        );
    }

    #[test]
    fn walk_frames_rejects_a_non_text_return_address() {
        // The heart of the reliability fix: a mapped frame record whose
        // `[fp+8]` is a stack data word (not a code address) is *not*
        // emitted as a caller — the walk stops at the interrupted PC.
        let records = [(0x1040_u64, 0x1080_u64, 0x0000_2000_u64)];
        let (readable, read_pair) = fake_stack(&records);
        let mut out = [0u64; 8];
        let n = walk_frames(
            0x0040_0050,
            0x1040,
            0x1000,
            &mut out,
            readable,
            read_pair,
            |a| (TEXT_LO..TEXT_HI).contains(&a),
        );
        assert_eq!(n, 1);
        assert_eq!(&out[..n], &[0x0040_0050]);
    }

    #[test]
    fn walk_frames_stops_at_or_below_the_stack_floor() {
        // A frame pointer that is not strictly above the exception frame is
        // impossible on a downward-growing stack (a leaf/mid-prologue x29):
        // only the PC is recorded.
        let records = [(0x1000_u64, 0x1080_u64, 0x0040_0100_u64)];
        let (readable, read_pair) = fake_stack(&records);
        let mut out = [0u64; 8];
        let n = walk_frames(
            0x0040_0050,
            0x1000,
            0x1000,
            &mut out,
            readable,
            read_pair,
            |a| (TEXT_LO..TEXT_HI).contains(&a),
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn walk_frames_stops_at_a_misaligned_frame_pointer() {
        let records = [(0x1044_u64, 0x1080_u64, 0x0040_0100_u64)];
        let (readable, read_pair) = fake_stack(&records);
        let mut out = [0u64; 8];
        let n = walk_frames(
            0x0040_0050,
            0x1044,
            0x1000,
            &mut out,
            readable,
            read_pair,
            |a| (TEXT_LO..TEXT_HI).contains(&a),
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn walk_frames_stops_at_an_unmapped_record() {
        // No records: the first frame pointer is unmapped, so the walk
        // stops rather than dereferencing it (fail closed).
        let (readable, read_pair) = fake_stack(&[]);
        let mut out = [0u64; 8];
        let n = walk_frames(
            0x0040_0050,
            0x1040,
            0x1000,
            &mut out,
            readable,
            read_pair,
            |a| (TEXT_LO..TEXT_HI).contains(&a),
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn walk_frames_records_one_frame_then_stops_on_a_non_increasing_link() {
        // A self-referential saved_fp cannot loop: the return address is
        // recorded once, then the non-increasing link stops the walk.
        let records = [(0x1080_u64, 0x1080_u64, 0x0040_0100_u64)];
        let (readable, read_pair) = fake_stack(&records);
        let mut out = [0u64; 8];
        let n = walk_frames(
            0x0040_0050,
            0x1080,
            0x1000,
            &mut out,
            readable,
            read_pair,
            |a| (TEXT_LO..TEXT_HI).contains(&a),
        );
        assert_eq!(n, 2);
        assert_eq!(&out[..n], &[0x0040_0050, 0x0040_0100]);
    }

    #[test]
    fn walk_frames_is_bounded_by_the_output_buffer() {
        let records = [
            (0x1040_u64, 0x1080_u64, 0x0040_0100_u64),
            (0x1080, 0x10c0, 0x0040_0200),
            (0x10c0, 0x0, 0x0040_0300),
        ];
        let (readable, read_pair) = fake_stack(&records);
        let mut out = [0u64; 2];
        let n = walk_frames(
            0x0040_0050,
            0x1040,
            0x1000,
            &mut out,
            readable,
            read_pair,
            |a| (TEXT_LO..TEXT_HI).contains(&a),
        );
        assert_eq!(n, 2);
        assert_eq!(&out[..n], &[0x0040_0050, 0x0040_0100]);
    }

    #[test]
    fn walk_frames_empty_output_returns_zero() {
        let (readable, read_pair) = fake_stack(&[]);
        let mut out: [u64; 0] = [];
        let n = walk_frames(
            0x0040_0050,
            0x1040,
            0x1000,
            &mut out,
            readable,
            read_pair,
            |_| true,
        );
        assert_eq!(n, 0);
    }

    #[test]
    fn walk_frames_is_hard_bounded_against_a_long_chain() {
        // A chain longer than `MAX_BACKTRACE_WALK` with strictly-increasing,
        // in-text frames is capped by the step bound (never spins), so the
        // buffer never overflows regardless of how deep the stack claims to
        // be.
        let mut records = [(0u64, 0u64, 0u64); MAX_BACKTRACE_WALK + 8];
        for (i, rec) in records.iter_mut().enumerate() {
            let addr = 0x1000 + 16 * (i as u64 + 1);
            *rec = (addr, addr + 16, TEXT_LO + 16 * i as u64);
        }
        let (readable, read_pair) = fake_stack(&records);
        let mut out = [0u64; MAX_BACKTRACE_WALK + 16];
        let n = walk_frames(
            0x0040_0050,
            records[0].0,
            0x1000,
            &mut out,
            readable,
            read_pair,
            |a| (TEXT_LO..TEXT_HI).contains(&a),
        );
        assert_eq!(n, MAX_BACKTRACE_WALK + 1);
    }
}
