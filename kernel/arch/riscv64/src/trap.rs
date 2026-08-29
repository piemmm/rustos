//! riscv64 S-mode trap vector and external-interrupt dispatch seam.
//!
//! Stage 4.D Item 4 (riscv64 external-IRQ trap glue). This module owns
//! the architecture-specific surface for taking a supervisor-mode trap
//! on the QEMU `virt` board:
//!
//! * The `stvec` trap vector (`tairix_riscv64_trap_vector`, published
//!   by the `global_asm!` block below) that saves the interrupted
//!   context's caller-saved registers, calls the Rust handler, restores,
//!   and `sret`s.
//! * `init_traps`, which points `stvec` at that vector (direct mode)
//!   and enables supervisor external interrupts (`sie.SEIE`) plus the
//!   supervisor global interrupt-enable (`sstatus.SIE`).
//! * The one-shot-published dispatch callback ([`set_trap_dispatch`])
//!   the Rust handler invokes on a supervisor *external* interrupt. The
//!   callback is responsible for claiming the pending source from the
//!   [`crate::plic::PlicController`], forwarding it to
//!   `tairix_kernel_irq::IrqTable::fire` (which masks the source before
//!   the waiter observes `ready`), and completing the claim.
//!
//! # Mask-before-wake
//!
//! Like the x86_64 dispatcher, the Rust handler here does **not** mask
//! the line itself — it forwards to the installed dispatcher, which
//! calls `IrqTable::fire`, which in turn calls the
//! [`crate::plic::PlicController`]'s `mask` *before* setting `ready`
//! (`docs/src/security/irq.md`). The PLIC claim/complete handshake
//! belongs to the dispatcher because only it holds the controller
//! reference.
//!
//! # No global mutable state
//!
//! The dispatch slot is set-once at boot, backed by an atomic so the
//! trap path reads it without a lock; a second publish fails closed.
//!
//! (`tairix_riscv64_trap_vector`, `init_traps`, and the Rust handler are
//! gated to the freestanding riscv64 target, so they are plain text on
//! host doc builds; the dispatch slot and `scause` decode build on the
//! host so their unit tests run under `cargo test`.)

use core::sync::atomic::{AtomicUsize, Ordering};

/// Caller-saved integer registers plus the return-state CSRs saved by
/// `tairix_riscv64_trap_vector` before it calls the Rust handler, laid
/// out to match the store/load offsets in `trap.s` exactly.
///
/// The asm reserves a [`TRAP_FRAME_BYTES`]-byte frame and stores `ra`,
/// `t0`–`t6`, `a0`–`a7`, the callee-saved set, then `sepc`, `sstatus`, the
/// interrupted `sp`, and the interrupted `tp` at the byte offsets the field
/// order below reproduces. The Rust handler
/// receives a `*mut TrapFrame` (the saved-frame `sp`) so it can read the
/// user's `ecall` arguments from `a0`–`a7`, write the return value back
/// into `a0`, and advance the saved [`TrapFrame::sepc`] past the `ecall`
/// (the asm epilogue reloads `sepc`/`sstatus`/`sp` from the frame, so a
/// task parked mid-handler resumes at its own return state rather than
/// whatever the live CSRs hold after another task ran). The `offset_of!`
/// asserts in the unit tests pin the layout against `trap.s`; a desync
/// fails the host build.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrapFrame {
    /// Return address (x1).
    pub ra: u64,
    /// Temporary t0 (x5).
    pub t0: u64,
    /// Temporary t1 (x6).
    pub t1: u64,
    /// Temporary t2 (x7).
    pub t2: u64,
    /// Temporary t3 (x28).
    pub t3: u64,
    /// Temporary t4 (x29).
    pub t4: u64,
    /// Temporary t5 (x30).
    pub t5: u64,
    /// Temporary t6 (x31).
    pub t6: u64,
    /// Argument / return register a0 (x10).
    pub a0: u64,
    /// Argument register a1 (x11).
    pub a1: u64,
    /// Argument register a2 (x12).
    pub a2: u64,
    /// Argument register a3 (x13).
    pub a3: u64,
    /// Argument register a4 (x14).
    pub a4: u64,
    /// Argument register a5 (x15).
    pub a5: u64,
    /// Argument register a6 (x16).
    pub a6: u64,
    /// Argument register / syscall number a7 (x17).
    pub a7: u64,
    /// Saved frame pointer s0 (x8) — the callee-saved register the
    /// user-fault crash backtrace follows.
    pub s0: u64,
    /// Saved callee-saved register s1 (x9).
    pub s1: u64,
    /// Saved callee-saved register s2 (x18).
    pub s2: u64,
    /// Saved callee-saved register s3 (x19).
    pub s3: u64,
    /// Saved callee-saved register s4 (x20).
    pub s4: u64,
    /// Saved callee-saved register s5 (x21).
    pub s5: u64,
    /// Saved callee-saved register s6 (x22).
    pub s6: u64,
    /// Saved callee-saved register s7 (x23).
    pub s7: u64,
    /// Saved callee-saved register s8 (x24).
    pub s8: u64,
    /// Saved callee-saved register s9 (x25).
    pub s9: u64,
    /// Saved callee-saved register s10 (x26).
    pub s10: u64,
    /// Saved callee-saved register s11 (x27).
    pub s11: u64,
    /// Saved `sepc`: the interrupted PC (the `ecall` address on the
    /// syscall path). The handler advances it past the `ecall` so the
    /// asm epilogue's `sret` resumes at the following instruction.
    pub sepc: u64,
    /// Saved `sstatus`: the interrupted privilege/interrupt state. Its
    /// `SPP` bit (8) selects the asm epilogue's U-mode vs S-mode return.
    pub sstatus: u64,
    /// Saved interrupted stack pointer: the user `sp` for a trap from
    /// U-mode (restored before `sret`), or the kernel `sp` for a nested
    /// S-mode trap (unused on the S-return path).
    pub user_sp: u64,
    /// Saved interrupted thread pointer (`tp`, x4): the *user's* own value
    /// for a trap from U-mode, or this hart's kernel `tp` for a nested
    /// S-mode trap.
    ///
    /// `tp` is both the RISC-V psABI thread pointer U-mode code may write
    /// freely and this port's per-hart kernel anchor
    /// ([`crate::smp::current_hartid`]), so the vector spills the
    /// interrupted value here and reloads the kernel's from the task's trap
    /// anchor before the handler runs. Making it frame-resident is what lets
    /// U-mode keep a thread pointer of its own across every trap and every
    /// context switch: the frame lives on the trapping task's *own* kernel
    /// stack, so the value is per-task by construction.
    pub user_tp: u64,
}

/// Byte size of the per-trap frame `trap.s` reserves — the value of its
/// `TRAP_FRAME_SIZE` `.equ`.
///
/// Equal to [`size_of::<TrapFrame>()`](core::mem::size_of) and a multiple of
/// the riscv64 ABI's 16-byte stack alignment. `trap_layout_tests.rs` parses
/// the `.equ` out of `trap.s` and compares it here, so the asm and the Rust
/// view cannot drift.
pub const TRAP_FRAME_BYTES: u64 = 256;

/// Byte width of the per-task **trap anchor** `sscratch` points at while a
/// task runs in U-mode — the value of `trap.s`'s `TRAP_ANCHOR_BYTES` `.equ`.
///
/// The anchor is the kernel-only region at the top of a task's kernel-stack
/// window: the kernel `tp` of the hart the task is running on
/// ([`TRAP_ANCHOR_KTP_OFFSET`]) followed by the task's floating-point state
/// ([`crate::fpstate::TrapAnchor`]). The trap frame is built immediately
/// below it. Keeping the register file here rather than in a separately
/// allocated per-task block is what lets it switch with the stack: no
/// allocation, and no per-CPU publication for the trap path to consult.
///
/// Both arming sites — [`crate::userentry`]'s entry sequence and the
/// vector's U-return path — publish the running hart's kernel `tp` into the
/// anchor before arming `sscratch`, which is what stops a U-mode-written `tp`
/// from ever reaching the kernel's per-CPU resolution.
pub const TRAP_ANCHOR_BYTES: u64 = size_of::<crate::fpstate::TrapAnchor>() as u64;

/// Offset within the trap anchor of the hart's kernel `tp` — the value of
/// `trap.s`'s `OFF_ANCHOR_KTP` `.equ`.
pub const TRAP_ANCHOR_KTP_OFFSET: u64 =
    core::mem::offset_of!(crate::fpstate::TrapAnchor, kernel_tp) as u64;

/// Offset within the trap anchor of the word recording whether the task owns
/// floating-point state.
///
/// The anchor is carved out of raw kernel stack, so the entry sequence must
/// zero this before the first trap can read it — an uninitialised non-zero
/// value would have the return path reload the register file from garbage.
pub const TRAP_ANCHOR_FP_OWNED_OFFSET: u64 =
    TRAP_ANCHOR_FP_OFFSET + crate::fpstate::FP_OWNED_OFFSET;

/// Offset within the trap anchor of the floating-point area.
const TRAP_ANCHOR_FP_OFFSET: u64 = core::mem::offset_of!(crate::fpstate::TrapAnchor, fp) as u64;

/// The riscv64 ABI stack alignment every frame and anchor bound respects.
const STACK_ALIGN: u64 = 16;

// The frame is built immediately below the anchor, so an anchor width that is
// not a multiple of the ABI alignment would misalign every trap frame, and a
// kernel `tp` word that did not fit inside the anchor would be written into the
// frame the vector is about to build.
const _: () = assert!(TRAP_FRAME_BYTES.is_multiple_of(STACK_ALIGN));
const _: () = assert!(TRAP_ANCHOR_BYTES.is_multiple_of(STACK_ALIGN));
const _: () = assert!(TRAP_ANCHOR_KTP_OFFSET + 8 <= TRAP_ANCHOR_BYTES);

/// `scause` bit set when the trap is an interrupt (cleared for a
/// synchronous exception). The remaining bits hold the cause code.
pub const SCAUSE_INTERRUPT_BIT: u64 = 1 << 63;

/// `scause` cause code for a Supervisor External Interrupt (the cause
/// the PLIC raises). Privileged-spec table 4.2.
pub const SCAUSE_SUPERVISOR_EXTERNAL: u64 = 9;

/// `scause` cause code for an illegal instruction. Privileged-spec table 4.2.
///
/// Also raised for a floating-point access while `sstatus.FS` is `Off`, which
/// is how a task's first use of the unit is detected.
pub const SCAUSE_ILLEGAL_INSTRUCTION: u64 = 2;

/// `sie.SEIE` — supervisor external interrupt enable (bit 9).
pub const SIE_SEIE: u64 = 1 << 9;

/// `sstatus.SIE` — supervisor global interrupt enable (bit 1).
pub const SSTATUS_SIE: u64 = 1 << 1;

/// `sstatus.SPP` — supervisor previous privilege (bit 8). Set when the
/// trap was taken from S-mode, clear when it was taken from U-mode. The
/// timer-preemption path gates on this so a tick taken in the
/// non-preemptible kernel never switches a half-completed critical
/// section away (watch-out).
pub const SSTATUS_SPP: u64 = 1 << 8;

/// `true` iff a trap whose saved `sstatus` is `sstatus` was taken from
/// U-mode (the `SPP` bit is clear). The U-mode preemption point fires
/// only for such a tick.
#[must_use]
pub const fn trap_came_from_user(sstatus: u64) -> bool {
    (sstatus & SSTATUS_SPP) == 0
}

/// `true` iff `scause` denotes a supervisor external interrupt — the
/// interrupt bit is set *and* the cause code is
/// [`SCAUSE_SUPERVISOR_EXTERNAL`].
#[must_use]
pub const fn is_supervisor_external_interrupt(scause: u64) -> bool {
    (scause & SCAUSE_INTERRUPT_BIT) != 0
        && (scause & !SCAUSE_INTERRUPT_BIT) == SCAUSE_SUPERVISOR_EXTERNAL
}

/// Signature of the installed external-interrupt dispatcher.
///
/// Invoked from the trap handler with interrupts disabled (the trap
/// entered with `sstatus.SIE` cleared by hardware). The dispatcher must
/// claim the pending PLIC source, forward it to
/// `tairix_kernel_irq::IrqTable::fire`, and complete the claim. It must
/// not allocate, block, or re-enter the scheduler.
pub type TrapDispatchFn = extern "C" fn();

/// Slot holding the installed dispatcher as a raw function pointer.
static TRAP_DISPATCH_FN: AtomicUsize = AtomicUsize::new(0);

/// Failure modes of [`set_trap_dispatch`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetDispatchError {
    /// A dispatcher was already published; the slot is set-once per
    /// boot.
    AlreadyInstalled,
}

/// Install the external-interrupt dispatcher.
///
/// # Errors
///
/// [`SetDispatchError::AlreadyInstalled`] on the second publish.
pub fn set_trap_dispatch(cb: TrapDispatchFn) -> Result<(), SetDispatchError> {
    let raw = cb as usize;
    TRAP_DISPATCH_FN
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetDispatchError::AlreadyInstalled)
}

/// Address of the installed dispatcher (`0` if none). Test/diagnostic
/// observer.
#[must_use]
pub fn trap_dispatch_addr() -> usize {
    TRAP_DISPATCH_FN.load(Ordering::Acquire)
}

#[cfg(test)]
fn clear_trap_dispatch_for_tests() {
    // Test-only: lets back-to-back host tests reinstall a dispatcher.
    // Production code never clears the slot.
    TRAP_DISPATCH_FN.store(0, Ordering::Release);
}

// --- Freestanding trap vector + Rust handler ----------------------

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
core::arch::global_asm!(include_str!("trap.s"));

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
extern "C" {
    /// S-mode trap vector published by `trap.s`. Installed into `stvec`
    /// by [`install_trap_vector`] (and thus by [`init_traps`]); never
    /// called from Rust.
    fn tairix_riscv64_trap_vector();
}

/// Point `stvec` at the trap vector (direct mode) **without** enabling
/// any interrupt source.
///
/// This is the synchronous-trap half of [`init_traps`]: it installs the
/// vector and the `sscratch == 0` S-mode invariant the vector relies on,
/// so a synchronous trap (a `ecall`, a page fault) is taken to the
/// handler, but leaves `sie`/`sstatus.SIE` untouched. The production
/// boot pipeline (`tairix_kernel::riscv64::boot`, `plans/PI.md` RV-P2)
/// uses it to catch a fault during the paged bring-up and to route the
/// `ecall` syscall path, mirroring the aarch64 port's vector-only
/// `exceptions::init_vectors`. A consumer that also wants to take
/// asynchronous interrupts calls [`init_traps`] instead (the vector-install logic has one definition).
///
/// # Safety
///
/// Must be called on the boot hart after a stack is established. Writing
/// `stvec`/`sscratch` has no memory side effects beyond the named CSRs.
/// The caller must have installed the syscall dispatch callback
/// ([`crate::syscall_entry::set_dispatch_callback`]) before user code
/// can `ecall`, or the handler fails closed.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn install_trap_vector() {
    // Arm the fault-windowed user copy alongside the vector: the two are
    // one mechanism (the handler below redirects an in-window fault to
    // the copy's fix-up), so no consumer can install the vector without
    // the recovery. The install is idempotent for this routine; a
    // conflicting occupant is a boot-order defect the hart must not run
    // past (fail closed).
    if crate::uaccess::install().is_err() {
        crate::kernel_arch::halt_current_hart();
    }
    let base = tairix_riscv64_trap_vector as *const () as usize;
    // SAFETY: `base` is the 4-byte-aligned address of the asm trap
    // vector (direct mode encodes mode 0 in the low two bits, which are
    // zero by the `.align 2`). Writing `stvec` and clearing `sscratch`
    // has no memory side effects beyond those CSRs.
    unsafe {
        core::arch::asm!("csrw stvec, {}", in(reg) base, options(nomem, nostack));
        // Establish the S-mode `sscratch == 0` invariant the trap vector
        // relies on to recognise a nested S-mode trap before any
        // interrupt source is armed (`trap.s`).
        core::arch::asm!("csrw sscratch, zero", options(nomem, nostack));
    }
}

/// Point `stvec` at the trap vector (direct mode) and enable supervisor
/// external interrupts.
///
/// # Safety
///
/// Must be called once, on the boot hart, after a stack is established
/// and before any interrupt source is armed. Enabling `sstatus.SIE`
/// makes the hart take interrupts; the caller must have installed the
/// dispatcher ([`set_trap_dispatch`]) and a valid `stvec`-aligned vector
/// first (this function installs the latter).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn init_traps() {
    // SAFETY: contract forwarded to the caller (boot hart, stack
    // established). `install_trap_vector` writes `stvec`/`sscratch`.
    unsafe {
        install_trap_vector();
    }
    // Firmware hands S-mode `FS = Dirty`, so the kernel would otherwise
    // inherit a floating-point unit it has no per-task place to save. Turning
    // it off makes "the kernel uses no floating point" a fault rather than an
    // assumption, and each task's own `FS` comes from its trap frame.
    // SAFETY: changes only S-mode floating-point enablement on this hart.
    unsafe { crate::fpstate::set_live_fs(crate::fpstate::Fs::Off) };
    // SAFETY: setting `sie.SEIE` and `sstatus.SIE` is the documented
    // S-mode interrupt-enable sequence; neither has memory side effects
    // beyond the named CSRs, and the caller asserts the dispatcher is
    // installed before any source is armed.
    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) SIE_SEIE, options(nomem, nostack));
        core::arch::asm!("csrs sstatus, {}", in(reg) SSTATUS_SIE, options(nomem, nostack));
    }
}

/// Enable (`enabled = true`) or mask (`enabled = false`) S-mode interrupt
/// taking on the calling hart by toggling `sstatus.SIE`.
///
/// This is the riscv64 backing of
/// `tairix_kernel_core::KernelArch::set_device_irqs`: the scheduler
/// dispatch loop enables S-mode interrupts so it runs every in-kernel
/// task/kthread preemptively, and masks them only
/// around the idle park and before halt. Enabling `sstatus.SIE` in S-mode
/// is safe because [`tairix_riscv64_trap_handler`] gates preemption on the
/// saved `SPP` — a timer tick taken in S-mode runs its (lock-free)
/// accounting but never reschedules the kernel, and a supervisor
/// external interrupt forwards to the lock-free PLIC dispatcher.
///
/// # Safety
///
/// Toggling `sstatus.SIE` only changes the global S-mode interrupt-enable.
/// The caller must have installed the trap vector ([`init_traps`] /
/// [`install_trap_vector`]) before enabling, so a taken interrupt
/// dispatches through a valid handler.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn set_supervisor_interrupts(enabled: bool) {
    // SAFETY: `csrs`/`csrc sstatus` set/clear only `SSTATUS_SIE`, with no
    // memory side effects beyond the named CSR.
    unsafe {
        if enabled {
            core::arch::asm!("csrs sstatus, {}", in(reg) SSTATUS_SIE, options(nomem, nostack));
        } else {
            core::arch::asm!("csrc sstatus, {}", in(reg) SSTATUS_SIE, options(nomem, nostack));
        }
    }
}

/// Park the hart on `wfi` until the next interrupt becomes pending, then
/// return with the interrupt-mask state unchanged — the tickless idle
/// park.
///
/// The dispatch loop calls this with `sstatus.SIE` already cleared (it
/// masked S-mode interrupts to close the park/wake race and drained any
/// already-flagged wake). `wfi` wakes on a pending interrupt even with
/// `SIE == 0`, so an edge that asserts after the drain but before this
/// call is not lost; the loop then re-enables interrupts
/// ([`set_supervisor_interrupts`]), *taking* the pending one — its
/// lock-free handler flags the deferred wake the next
/// `drain_pending_wakes` consumes.
///
/// # Safety
///
/// `wfi` is a hint with no architectural side effects. The caller must
/// have installed the trap vector ([`init_traps`] /
/// [`install_trap_vector`]) and a timer source so an interrupt can become
/// pending and wake the hart.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn wait_for_interrupt() {
    // SAFETY: `wfi` suspends until a pending interrupt and has no memory
    // side effects; it leaves `sstatus.SIE` unchanged (masked as found).
    unsafe {
        core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
    }
}

/// Read `stval` — the faulting address of the trap being handled.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn read_stval() -> u64 {
    let stval: u64;
    // SAFETY: reading `stval` has no side effects.
    unsafe {
        core::arch::asm!("csrr {}, stval", out(reg) stval, options(nomem, nostack));
    }
    stval
}

/// Build the faulting-thread [`tairix_arch_api::backtrace::UserRegisterFrame`]
/// from the saved [`TrapFrame`].
///
/// `pc` is the saved `sepc`, `sp` the interrupted `user_sp`, and the frame
/// pointer is `s0` (the callee-saved register the vector now persists), so
/// the crash path can follow the riscv64 frame-pointer chain
/// ([`crate::backtrace::Backtracer::LAYOUT`]); the frame is marked
/// `fp_valid`. The full integer register file is carried by name.
///
/// # Safety
///
/// `frame` must point to the live [`TrapFrame`] the asm vector built, valid
/// for the duration of the read.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
unsafe fn user_register_frame(
    frame: *const TrapFrame,
) -> tairix_arch_api::backtrace::UserRegisterFrame {
    use tairix_arch_api::backtrace::{RegisterSnapshot, UserRegisterFrame};
    // SAFETY: the caller guarantees `frame` addresses the live saved frame.
    let f = unsafe { &*frame };
    let snapshot = RegisterSnapshot::new(f.sepc, f.user_sp, f.s0)
        .with("ra", f.ra)
        .with("t0", f.t0)
        .with("t1", f.t1)
        .with("t2", f.t2)
        .with("t3", f.t3)
        .with("t4", f.t4)
        .with("t5", f.t5)
        .with("t6", f.t6)
        .with("a0", f.a0)
        .with("a1", f.a1)
        .with("a2", f.a2)
        .with("a3", f.a3)
        .with("a4", f.a4)
        .with("a5", f.a5)
        .with("a6", f.a6)
        .with("a7", f.a7)
        .with("s0", f.s0)
        .with("s1", f.s1)
        .with("s2", f.s2)
        .with("s3", f.s3)
        .with("s4", f.s4)
        .with("s5", f.s5)
        .with("s6", f.s6)
        .with("s7", f.s7)
        .with("s8", f.s8)
        .with("s9", f.s9)
        .with("s10", f.s10)
        .with("s11", f.s11);
    UserRegisterFrame::new(snapshot, crate::backtrace::Backtracer::LAYOUT, true)
}

/// Rust entry invoked by the asm trap vector.
///
/// Reads `scause` and dispatches:
///
/// * A U-mode environment call (`ecall`) is forwarded to the installed
///   [`crate::syscall_entry`] dispatch callback, after which `sepc` is
///   advanced past the 4-byte `ecall` so `sret` resumes at the next
///   instruction. If no syscall dispatcher is installed the hart fails
///   closed rather than returning an unspecified
///   value to user space.
/// * A supervisor external interrupt forwards to the installed PLIC
///   dispatcher (claim → `IrqTable::fire` → complete).
/// * A supervisor timer interrupt drives the scheduler tick.
/// * An S-mode load/store page fault whose saved `sepc` lies inside the
///   guarded user-copy fault window ([`crate::uaccess`]) is redirected
///   to the copy's fix-up (the frame's `sepc` is rewritten), so the copy
///   returns an error instead of the hart halting.
/// * Any other synchronous exception cannot be resumed — an `sret` would
///   loop on the faulting instruction — so [`fatal_exception`] charges it to
///   whoever caused it: one taken from U-mode kills the running task through
///   [`crate::fault::UserFaultTerminateFn`] and the hart carries on, while
///   one taken from S-mode parks the hart (never silently reset).
///
/// `frame` is the saved-register frame the asm vector built; the
/// syscall path reads the user's `a0`–`a7` from it and writes the
/// return value back into `a0`.
///
/// # Safety
///
/// Only callable from `tairix_riscv64_trap_vector`, which has saved the
/// interrupted context's caller-saved registers and passes `sp` as
/// `frame`. `frame` therefore points at a valid [`TrapFrame`] live for
/// the duration of the call.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn tairix_riscv64_trap_handler(frame: *mut TrapFrame) {
    // Bracket the whole body so every one of its return paths — including a
    // cooperative park that runs another task in between — hands the task back
    // its own floating-point registers. A body that diverges (the task is
    // killed) skips the return half, which is correct: there is no task left
    // to hand anything to.
    // SAFETY: `frame` is the live saved-register frame the vector passed; the
    // anchor sits immediately above it, which is where the vector swapped
    // `sscratch` from.
    let from_user = trap_came_from_user(unsafe { (*frame).sstatus });
    let anchor = unsafe { anchor_of(frame) };
    if from_user {
        // SAFETY: the interrupted task's live anchor and its frame's saved
        // `sstatus`, with the trap confirmed to have come from U-mode.
        unsafe { crate::fpstate::on_trap_from_user(anchor, &mut (*frame).sstatus) };
    }
    // SAFETY: as the outer contract: `frame` is valid for this trap.
    unsafe { trap_body(frame, from_user, anchor) };
    if from_user {
        // SAFETY: as above.
        unsafe { crate::fpstate::on_return_to_user(anchor, &mut (*frame).sstatus) };
    }
}

/// The task's trap anchor, which the vector placed immediately above the frame.
///
/// # Safety
///
/// `frame` must be a live U-mode trap frame the vector built.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
unsafe fn anchor_of(frame: *mut TrapFrame) -> *mut crate::fpstate::TrapAnchor {
    frame.wrapping_add(1).cast()
}

/// Whether the illegal instruction at `pc` reaches the floating-point unit.
///
/// Read through the guarded copy window, so an unreadable page answers `false`
/// and leaves the fault fatal. The 32-bit read is tried first and a 16-bit
/// parcel second, because a compressed instruction can sit in the last two
/// bytes of a mapped page.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn illegal_instruction_needs_fp(pc: u64) -> bool {
    let Some(copy) = tairix_arch_api::uaccess::guarded_copy() else {
        return false;
    };
    let mut parcel = [0u8; 4];
    for len in [4usize, 2] {
        // SAFETY: `parcel` is valid for four writable bytes and cannot overlap
        // a user address; the window absorbs a fault reading `pc` and reports
        // it as a non-zero return, whose destination bytes we discard.
        if unsafe { copy(parcel.as_mut_ptr(), pc as *const u8, len) } == 0 {
            return crate::fpstate::touches_fp_state(&parcel[..len]);
        }
    }
    false
}

/// The trap handler proper.
///
/// # Safety
///
/// As [`tairix_riscv64_trap_handler`]; `anchor` must be that frame's anchor.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
unsafe fn trap_body(
    frame: *mut TrapFrame,
    from_user_entry: bool,
    anchor: *mut crate::fpstate::TrapAnchor,
) {
    let scause: u64;
    // SAFETY: reading `scause` has no side effects.
    unsafe {
        core::arch::asm!("csrr {}, scause", out(reg) scause, options(nomem, nostack));
    }

    if (scause & SCAUSE_INTERRUPT_BIT) == 0 {
        if crate::syscall_entry::is_ecall_from_user(scause) {
            // Run the syscall body with S-mode interrupts deliverable. The
            // hart cleared `sstatus.SIE` on trap entry; the asm vector has
            // now saved the caller-saved registers and the frame-resident
            // return state (`sepc`/`sstatus`), and the vector recognises a
            // nested S-mode trap (the `sscratch == 0` invariant), so a
            // supervisor timer/external interrupt taken here saves its own
            // frame, services its source, and `sret`s back to this handler.
            // This is what stops a long, non-blocking syscall body (e.g. a
            // bootstrap-floor `fs_*` MMIO wait) from monopolising the hart
            // with interrupts masked. The kernel stays non-preemptible: an
            // interrupt taken in S-mode latches its reschedule (honoured at
            // return-to-user in `completion_outcome`) rather than switching
            // away mid-critical-section, enforced by the saved-`SPP` gate
            // below. Re-mask before returning so the epilogue `sret`s to the
            // interrupted user frame with `SIE` off.
            // SAFETY: the trap vector is installed and the syscall/IPC
            // dispatch and IRQ paths are wired before any U-mode code can
            // `ecall`, so a taken interrupt dispatches correctly;
            // `set_supervisor_interrupts` only toggles `sstatus.SIE`.
            unsafe {
                set_supervisor_interrupts(true);
            }
            // SAFETY: `frame` is the live saved-register frame the asm
            // vector passed; the syscall path reads `a0`–`a7` and writes
            // the result into `a0`.
            let dispatched = crate::syscall_entry::dispatch_ecall(unsafe { &mut *frame });
            // SAFETY: as above — restoring the S-mode interrupt mask before
            // the epilogue restores the interrupted user frame.
            unsafe {
                set_supervisor_interrupts(false);
            }
            if !dispatched {
                // A syscall reached the handler before the binary
                // installed its dispatcher — fail closed.
                crate::kernel_arch::halt_current_hart();
            }
            // Advance the *saved* `sepc` past the `ecall` so the asm
            // epilogue's `sret` resumes at the following instruction
            // instead of re-trapping. The epilogue reloads `sepc` from
            // the frame, so writing the live CSR here would be lost
            // across a cooperative mid-handler park (the whole point of
            // making the return state frame-resident).
            // SAFETY: `frame` is the live saved-register frame.
            unsafe {
                (*frame).sepc = (*frame)
                    .sepc
                    .wrapping_add(crate::syscall_entry::ECALL_INSTR_LEN);
            }
            return;
        }
        // Software A/D update (the cold-page referenced bit,
        // `plans/SWAPSWAPSWAP.md`): under Svade an access to a valid leaf
        // whose Accessed (or, for a store, Dirty) bit is clear raises a
        // page fault rather than updating the bit in the walk. Set the
        // bit(s) back on the faulting leaf and return so the epilogue's
        // `sret` retries the access (the saved `sepc` still points at the
        // faulting instruction — the hart does not advance it for a page
        // fault). All three page-fault classes and both privilege modes
        // are handled here — the cleared-A leaf may be the running task's
        // own page (U-mode) or one the kernel touches on its behalf
        // (S-mode). A leaf that does not permit the access (a genuine
        // permission fault shares the same `scause`) or is absent leaves
        // `set_accessed_flag_in_active` returning `false`, so the fault
        // falls through to the resolver / fatal path unchanged (fail
        // closed). On a Svadu part the hardware sets A/D in the walk and
        // this fault never fires, so the branch is inert there.
        let ad_kind = if crate::fault::is_store_page_fault(scause) {
            Some(crate::paging::AccessKind::Store)
        } else if crate::fault::is_load_page_fault(scause) {
            Some(crate::paging::AccessKind::Load)
        } else if crate::fault::is_instruction_page_fault(scause) {
            Some(crate::paging::AccessKind::Instruction)
        } else {
            None
        };
        if let Some(kind) = ad_kind {
            if crate::paging::set_accessed_flag_in_active(read_stval(), kind) {
                return;
            }
        }

        // A load or store/AMO page fault taken from U-mode is offered to
        // the installed resolver before the fatal path, with the store
        // verdict. For a *load* it may be a demand-paged file-mapping
        // fault: `true` means the faulting page is now resident;
        // returning leaves the saved `sepc` on the faulting instruction,
        // so the asm epilogue's `sret` retries the access. A *store* is
        // never resolved (file mappings are read-only; resolving it would
        // retry the store forever) — the resolver kills the faulting task
        // instead, so a store to a read-only mapping or any wild write
        // costs the task, never the hart. A fault fatal to the task alone
        // never returns from the resolver (the callback suspends the task
        // into the scheduler with an exit action); `false` falls through
        // to the fatal path below, exactly as with no resolver installed
        // (fail closed).
        let write_fault = crate::fault::is_store_page_fault(scause);
        if crate::fault::is_load_page_fault(scause) || write_fault {
            // SAFETY: `frame` is the live saved-register frame; reading
            // its saved `sstatus`/`sepc` is sound.
            let from_user = trap_came_from_user(unsafe { (*frame).sstatus });
            if from_user {
                if let Some(resolver) = crate::fault::user_fault_resolver() {
                    // Capture the faulting U-mode register frame from the
                    // saved trap frame so the resolver can record a
                    // post-mortem crash record with a backtrace. It lives on
                    // this kernel stack across the resolver call.
                    // SAFETY: `frame` is the live saved-register frame the
                    // asm vector passed, which now saves the callee-saved
                    // set (incl. s0=fp) as well as the caller-saved GPRs.
                    let user_frame = unsafe { user_register_frame(frame) };
                    if resolver(read_stval(), write_fault, &raw const user_frame) {
                        return;
                    }
                }
            } else if let Some(fixup) =
                // SAFETY: reading the frame's saved `sepc` is sound.
                crate::uaccess::kernel_fixup_for(unsafe { (*frame).sepc })
            {
                // An S-mode data page fault inside the guarded user-copy
                // window: the validated copy's software proof was violated
                // underneath it. Resume at the copy's fix-up so it returns
                // an error to the caller instead of taking the hart down
                // (the epilogue reloads `sepc` from the frame).
                // SAFETY: `frame` is the live saved-register frame; the
                // fix-up address is a real instruction in this image.
                unsafe {
                    (*frame).sepc = fixup;
                }
                return;
            }
        }

        // A task's first floating-point instruction: it runs with FP off and
        // owning no register state, so the access traps here. Give it a zeroed
        // file and return without advancing the saved `sepc`, so the epilogue
        // retries the instruction. `adopt_on_first_use` refuses a task that
        // already owns state, which is what stops a genuinely illegal FP
        // encoding from retrying forever.
        if scause == SCAUSE_ILLEGAL_INSTRUCTION && from_user_entry {
            // SAFETY: reading the frame's saved `sepc` is a plain field load.
            let pc = unsafe { (*frame).sepc };
            // SAFETY: `anchor` is this frame's anchor, per the contract.
            if illegal_instruction_needs_fp(pc)
                && unsafe { crate::fpstate::adopt_on_first_use(anchor) }
            {
                return;
            }
        }

        // Any other synchronous exception (a page fault, an access fault,
        // an illegal instruction) cannot be resumed: returning would
        // re-execute the faulting instruction forever.
        // SAFETY: `frame` is the live saved-register frame the asm vector
        // passed, valid for the rest of this trap.
        unsafe { fatal_exception(scause, frame) }
    }

    // Whether the interrupted context was U-mode (the saved `SPP == 0`),
    // read once so every interrupt kind below shares the one preemption
    // gate. `frame` is the live saved-register frame the asm vector
    // passed; reading its saved `sstatus` is a plain field load.
    // SAFETY: `frame` is the live saved-register frame.
    let from_user = trap_came_from_user(unsafe { (*frame).sstatus });

    if crate::preempt::is_supervisor_timer_interrupt(scause) {
        // Supervisor timer interrupt: drive the scheduler tick and
        // re-arm the SBI timer (which acknowledges `sip.STIP`).
        crate::preempt::on_timer_interrupt();
    } else if crate::preempt::is_supervisor_software_interrupt(scause) {
        // Supervisor software interrupt: a directed IPI raised by the
        // SBI IPI extension. Acknowledge it (clear `sip.SSIP`) and run
        // the installed IPI callback.
        crate::preempt::on_software_interrupt();
    } else if is_supervisor_external_interrupt(scause) {
        let raw = TRAP_DISPATCH_FN.load(Ordering::Acquire);
        if raw != 0 {
            // SAFETY: every value stored into the slot round-trips a
            // valid `TrapDispatchFn` through `set_trap_dispatch`;
            // function pointers are `usize`-sized so the transmute is
            // lossless.
            let cb: TrapDispatchFn = unsafe { core::mem::transmute::<usize, TrapDispatchFn>(raw) };
            cb();
        }
        // With no dispatcher installed the source stays pending and
        // masked at the controller until one is; the timer interrupt or
        // a later claim drains it. A pre-dispatch external interrupt is
        // impossible in practice (the boot path installs the dispatcher
        // before arming any line).
    }

    // Involuntary preemption (`plans/PI.md` D2b-2b-A P-1b), honoured on
    // return to U-mode for **any** interrupt — a timer quantum expiry, a
    // directed reschedule IPI, or a device (external) interrupt that woke
    // a higher-priority task. This is the riscv64 analogue of an OS
    // honouring `need_resched` on trap-return-to-user: without it a
    // CPU-bound U-mode task that never issues a syscall (and, being the
    // sole runnable task, has no tickless quantum armed) could never be
    // forced back into the scheduler when a device interrupt made new work
    // runnable. The source's own handler above already acknowledged its
    // pending bit / re-armed the timer, so the context switch happens with
    // no interrupt line pending. The callback consults the per-hart
    // need-resched latch and only switches away when a reschedule is
    // actually owed, so an interrupt that woke nothing returns straight to
    // U-mode. A tick taken from S-mode never preempts — the kernel is
    // non-preemptible, and the saved `SPP` gate enforces that even if
    // `sstatus.SIE` is ever enabled in S-mode; its reschedule is latched
    // and honoured at the interrupted syscall's completion instead.
    if from_user && (scause & SCAUSE_INTERRUPT_BIT) != 0 {
        crate::preempt::on_u_mode_preempt_point(crate::smp::current_hartid());
    }
}

/// The unrecoverable tail of the synchronous-exception path.
///
/// A U-mode exception is the running task's fault, so the installed
/// [`crate::fault::UserFaultTerminateFn`] kills that task and never returns
/// — a wild jump or an illegal instruction costs its process, never the
/// machine. A `false` return means the exception could not be attributed to
/// a running task; that, and every S-mode exception, is the kernel's own and
/// genuinely unrecoverable, so the installed [`crate::fault::FaultHandlerFn`]
/// gets it (the memory-isolation vertical installs one to confirm an attacker
/// faulted on an isolated address) and otherwise the hart parks — never a
/// silent reset. With no terminator installed a U-mode fault takes that same
/// fatal path, so a missing install can only be safe (fail closed).
///
/// # Safety
///
/// `frame` must point to the live [`TrapFrame`] the asm vector built, valid
/// for the duration of the call.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
unsafe fn fatal_exception(scause: u64, frame: *const TrapFrame) -> ! {
    // Read the faulting address before any callback runs: unlike `sepc`,
    // `stval` is not frame-resident, so a nested S-mode trap taken inside a
    // callback would leave the handler below reporting that trap's address.
    let stval = read_stval();
    // SAFETY: the caller guarantees `frame` addresses the live saved frame.
    let (sstatus, sepc) = unsafe { ((*frame).sstatus, (*frame).sepc) };
    if trap_came_from_user(sstatus) {
        if let Some(terminate) = crate::fault::user_fault_terminator() {
            // The frame lives on this kernel stack across the call, so the
            // terminator can record a post-mortem crash record with a
            // backtrace.
            // SAFETY: as above.
            let user_frame = unsafe { user_register_frame(frame) };
            let _ = terminate(sepc, &raw const user_frame);
        }
    }
    if let Some(handler) = crate::fault::fault_handler() {
        handler(scause, stval, sepc);
    }
    crate::kernel_arch::halt_current_hart();
}

/// The ordering invariant of this port's two `sret` sequences, pinned
/// against their sources: the vector's U-return epilogue and the U-mode
/// entry both mask S-mode interrupts before they arm `sscratch`.
#[cfg(test)]
#[path = "sret_tests.rs"]
mod sret_tests;

/// The trap-frame / trap-anchor layout and the `tp` discipline, pinned
/// against `trap.s` itself so the assembly and this module's view of it
/// cannot drift.
#[cfg(test)]
#[path = "trap_layout_tests.rs"]
mod trap_layout_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_external_interrupt_is_recognised() {
        // Interrupt bit set, cause 9.
        assert!(is_supervisor_external_interrupt(
            SCAUSE_INTERRUPT_BIT | SCAUSE_SUPERVISOR_EXTERNAL
        ));
    }

    #[test]
    fn synchronous_exception_with_code_9_is_not_an_external_interrupt() {
        // Same code, but interrupt bit clear → it is exception 9
        // (store/AMO access fault), not the PLIC external interrupt.
        assert!(!is_supervisor_external_interrupt(
            SCAUSE_SUPERVISOR_EXTERNAL
        ));
    }

    #[test]
    fn other_interrupt_causes_are_rejected() {
        // Supervisor timer interrupt is cause 5 with the interrupt bit.
        assert!(!is_supervisor_external_interrupt(SCAUSE_INTERRUPT_BIT | 5));
        // Supervisor software interrupt is cause 1.
        assert!(!is_supervisor_external_interrupt(SCAUSE_INTERRUPT_BIT | 1));
    }

    #[test]
    fn enable_bit_constants_match_privileged_spec() {
        assert_eq!(SIE_SEIE, 0x200);
        assert_eq!(SSTATUS_SIE, 0x2);
        assert_eq!(SSTATUS_SPP, 0x100);
        assert_eq!(SCAUSE_INTERRUPT_BIT, 0x8000_0000_0000_0000);
    }

    #[test]
    fn trap_origin_is_read_from_the_spp_bit() {
        // SPP clear → the trap came from U-mode (preemption point fires).
        assert!(trap_came_from_user(SSTATUS_SIE));
        assert!(trap_came_from_user(0));
        // SPP set → the trap came from S-mode (the kernel; never preempts).
        assert!(!trap_came_from_user(SSTATUS_SPP));
        assert!(!trap_came_from_user(SSTATUS_SPP | SSTATUS_SIE));
    }

    extern "C" fn host_dispatch_cb() {}

    #[test]
    fn set_trap_dispatch_fails_closed_on_second_install() {
        clear_trap_dispatch_for_tests();
        set_trap_dispatch(host_dispatch_cb).expect("first install");
        assert_eq!(
            set_trap_dispatch(host_dispatch_cb),
            Err(SetDispatchError::AlreadyInstalled)
        );
        clear_trap_dispatch_for_tests();
    }

    #[test]
    fn trap_dispatch_addr_round_trips_installed_fn() {
        clear_trap_dispatch_for_tests();
        set_trap_dispatch(host_dispatch_cb).expect("install");
        assert_eq!(trap_dispatch_addr(), host_dispatch_cb as *const () as usize);
        clear_trap_dispatch_for_tests();
    }
}
