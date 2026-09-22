//! riscv64 machine-takeover mechanism (`plans/NEW-SUPERVISOR.md` §9 Stage
//! B).
//!
//! The Arch HAL [`MachineTakeover`] body for the QEMU `virt` / SiFive
//! `virt`-class port: the irreversible, one-way sequence the pre-boot
//! Supervisor's `memtest` drives to test **all** of RAM. It is
//! deliberately the *only* public surface of this module — the takeover
//! `static` is private and reachable solely through the supervisor-gated
//! `KernelArch::machine_takeover` accessor the downstream boot wrapper
//! implements.
//!
//! # Where the cross-CPU quiesce lives
//!
//! Stopping every *other* hart is **not** done here: it is architecture-neutral
//! (a stop request, the directed SBI IPI, the boot-published liveness/ack
//! tables) and is driven by the caller before this body runs — the
//! Supervisor's `drive_takeover` calls `tairix_arch_api::quiesce_others`,
//! which returns [`TakeoverError::CpuQuiesceTimeout`] fail-closed if a hart
//! will not halt, so this body is only ever reached once this hart is the sole
//! one running. The one per-silicon half of quiesce — *parking* a stopped
//! hart — lives in the IPI receive path
//! (`crate::preempt::on_software_interrupt`), not here.
//!
//! # The riscv64 sequence
//!
//! [`RiscvMachineTakeover::take_over`] performs, in order and without ever
//! returning on success (every other hart already quiesced):
//!
//! 1. **Install the reserved boot kernel root** (the permanent root the
//!    dispatcher parks every hart on). It is the only root whose tables
//!    live wholly in the kernel image's `.bss` rather than in
//!    allocator-backed frames, so the sweep cannot destroy the translation
//!    it is running under; it carries both the lower-half identity window
//!    and the direct physical map, so the `pc`, the console, and the RAM the
//!    sweep writes all keep resolving. With no root published this refuses
//!    [`TakeoverError::PrepareFailed`] before anything is torn down.
//! 2. **Mask S-mode interrupts** (`sstatus.SIE = 0`, `sie = 0`) so nothing
//!    preempts the solitary hart. There is no lockup watchdog wired on this
//!    port, so there is none to stop.
//! 3. **Switch onto a reserved stack** the sweep will not overwrite and run
//!    the caller's `sweep` (the arch-neutral whole-RAM test of every *usable*
//!    frame, which renders progress to the console). The Supervisor's
//!    `memtest` sweep tests all of RAM continuously and never returns; the
//!    operator ends the run by resetting the board. Should a `sweep` ever
//!    return, the hart parks in a masked `wfi` halt rather than resume kernel
//!    code — the machine has been torn down.
//!
//! # Why paging stays on
//!
//! Bare mode (`satp = 0`) was the earlier step 1, on the reasoning that an
//! identity-mapped kernel keeps every address under it. That stopped being
//! true when the direct physical map moved above the user region: the sweep
//! reaches RAM through the map, whose addresses bare mode resolves to
//! nowhere. Keeping paging on is also what the sibling ports do and for the
//! same reason — the requirement is not that paging be *off* but that the
//! sweep never depend on a page-table frame in the usable RAM it is about to
//! destroy, which installing the reserved boot root satisfies directly.
//! Bare mode additionally makes every access Device-typed on real silicon,
//! exactly the hazard the aarch64 port documents.
//!
//! The one region the sweep cannot test is the memory it executes from — the
//! kernel image and its reserved stack — because a continuous run must keep
//! that resident image intact to go on running, exactly as a running memtest86
//! cannot test its own resident code.
//!
//! The quiesce refusal is the caller's and is fail-closed there; the only
//! refusal this body owns is an unpublished boot root, above.

use tairix_arch_api::{MachineTakeover, TakeoverError};

/// Size of the reserved takeover stack, in bytes. It lives in `.bss` inside
/// the kernel image, so the sweep (which tests only *usable* frames) never
/// overwrites it.
const TAKEOVER_STACK_BYTES: usize = 64 * 1024;

/// The reserved takeover stack the sweep runs on.
///
/// `UnsafeCell` because the CPU writes through it via `sp` while the Rust
/// aliasing model would otherwise treat a plain `static` as immutable; it
/// is never read as a Rust value, only used as raw stack backing. A
/// takeover happens at most once per boot, so there is no concurrent access.
#[repr(C, align(16))]
struct TakeoverStack {
    bytes: core::cell::UnsafeCell<[u8; TAKEOVER_STACK_BYTES]>,
}

// SAFETY: the stack is only ever used as raw `sp` backing on the single
// hart the takeover has quiesced every other CPU away from; there is no
// concurrent access and it is never read as a typed value.
unsafe impl Sync for TakeoverStack {}

/// The one reserved takeover stack. `.bss`-resident (zeroed), inside the
/// kernel image, so it survives the usable-RAM sweep.
static TAKEOVER_STACK: TakeoverStack = TakeoverStack {
    bytes: core::cell::UnsafeCell::new([0u8; TAKEOVER_STACK_BYTES]),
};

/// The riscv64 machine-takeover handle (`plans/NEW-SUPERVISOR.md` §9).
///
/// A zero-sized unit: the takeover needs no per-instance state (the
/// reserved stack is `static`). Held by the downstream `KernelArch`
/// wrapper behind the supervisor-gated accessor and never constructed
/// elsewhere.
pub struct RiscvMachineTakeover;

/// The single `'static` takeover handle. Private to the crate: the only
/// way to reach it is [`machine_takeover_handle`], which the downstream
/// boot wrapper calls from its supervisor-gated `KernelArch::machine_takeover`.
static RISCV_TAKEOVER: RiscvMachineTakeover = RiscvMachineTakeover;

/// Hand back the `'static` riscv64 machine-takeover handle.
///
/// The downstream boot wrapper's `KernelArch::machine_takeover` — itself
/// gated on the supervisor-only `TakeoverGrant` — is the only caller, so
/// the takeover mechanism stays reachable exclusively from the confirmed
/// `memtest` path.
#[must_use]
pub fn machine_takeover_handle() -> &'static (dyn MachineTakeover + Sync) {
    &RISCV_TAKEOVER
}

extern "C" {
    /// Install the reserved stack and tail-call
    /// [`tairix_arch_riscv64_takeover_continue`] (`takeover.s`). Never
    /// returns.
    fn _takeover_switch_stack(thin: usize, stack_top: usize) -> !;
}

impl MachineTakeover for RiscvMachineTakeover {
    unsafe fn take_over(&self, sweep: &mut dyn FnMut()) -> TakeoverError {
        // Every other hart has already been quiesced by the
        // architecture-neutral caller (the Supervisor's `drive_takeover` runs
        // the cross-CPU stop handshake before this is ever reached), so this
        // hart is the only one running. This body owns only the single-hart
        // tear-down that follows.

        // 1. Install the reserved boot kernel root, so the translation the
        //    sweep runs under is one whose tables the sweep cannot destroy.
        //    Done first, and before any interrupt masking, because it is the
        //    one step that may refuse: it is the routine park every task
        //    suspend performs, so a refusal leaves the machine exactly as it
        //    was and the caller keeps its REPL.
        if !crate::paging::park_kernel_root() {
            // The mechanism reports only failure, so the audit payload is 0.
            return TakeoverError::PrepareFailed(0);
        }

        // 2. Mask S-mode interrupts so nothing preempts the solitary hart:
        //    clear `sstatus.SIE`, then disable every S-mode interrupt
        //    source (`sie = 0`).
        // SAFETY: both are well-defined S-mode CSR writes, and masking is the
        // deliberate, confirmed tear-down the caller's `TakeoverGrant`
        // authorises. Neither touches memory nor moves any translation.
        unsafe {
            core::arch::asm!(
                "csrci sstatus, 2",
                "csrw sie, zero",
                options(nostack, preserves_flags),
            );
        }

        // 3. Switch onto the reserved stack and run the sweep, which never
        //    returns. The sweep handle is reached through a thin pointer to
        //    the caller's `&mut dyn FnMut()`, which lives on the caller's
        //    (reserved) stack and so survives the usable-RAM sweep.
        let mut sweep_ref: &mut dyn FnMut() = sweep;
        let thin = core::ptr::addr_of_mut!(sweep_ref) as usize;
        let stack_top = core::ptr::addr_of!(TAKEOVER_STACK) as usize + TAKEOVER_STACK_BYTES;
        // SAFETY: `_takeover_switch_stack` installs the 16-byte-aligned top
        // of the reserved `.bss` takeover stack and tail-calls
        // `tairix_arch_riscv64_takeover_continue(thin)`; `thin` addresses a
        // live `&mut dyn FnMut()` in reserved memory. It never returns.
        unsafe { _takeover_switch_stack(thin, stack_top) }
    }
}

/// Run the whole-RAM sweep on the reserved stack; never returns. Entered
/// from `_takeover_switch_stack` with `sp` already installed on the reserved
/// takeover stack.
///
/// # Safety
///
/// Reached only from [`RiscvMachineTakeover::take_over`] after the reserved
/// boot root is installed and interrupts are masked. `thin` is a live pointer to the
/// caller's `&mut dyn FnMut()` sweep handle, whose environment resides in
/// reserved memory the sweep does not destroy.
#[no_mangle]
unsafe extern "C" fn tairix_arch_riscv64_takeover_continue(thin: usize) -> ! {
    // SAFETY: `thin` points at the live `&mut dyn FnMut()` the caller
    // placed on its reserved stack; reconstructing and calling it runs the
    // architecture-neutral whole-RAM sweep over every usable frame.
    let sweep = unsafe { &mut *(thin as *mut &mut dyn FnMut()) };
    sweep();
    // The Supervisor's `memtest` sweep loops until the operator resets the
    // board, so control never reaches here. If a future finite sweep ever
    // returned, the machine has been torn down (usable RAM overwritten) and
    // must not resume kernel code, so park the sole hart.
    loop {
        // SAFETY: interrupts are masked; `wfi` merely idles the parked hart
        // until the operator resets the machine.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}
