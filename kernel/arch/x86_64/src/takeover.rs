//! x86_64 machine-takeover mechanism (`plans/NEW-SUPERVISOR.md` §9 Stage B).
//!
//! The Arch HAL [`MachineTakeover`] body for the x86_64 PC-class port: the
//! irreversible, one-way sequence the pre-boot Supervisor's `memtest`
//! drives to test **all** of RAM. It is deliberately the only public surface
//! of this module — the takeover `static` is private and reachable solely
//! through the supervisor-gated [`machine_takeover_handle`], which the
//! downstream boot wrapper calls from its `KernelArch::machine_takeover`
//! accessor.
//!
//! # Where the cross-CPU quiesce lives
//!
//! Stopping every *other* CPU is **not** done here: it is architecture-neutral
//! (a stop request, the directed IPI on `TIMER_VECTOR`, the boot-published
//! liveness/ack tables) and is driven by the caller before this body runs —
//! the Supervisor's `drive_takeover` calls `tairix_arch_api::quiesce_others`,
//! which returns [`TakeoverError::CpuQuiesceTimeout`] fail-closed if a peer
//! will not halt, so this body is only ever reached once this CPU is the sole
//! one running. The one per-silicon half of quiesce — *parking* a stopped
//! CPU — lives in the reschedule-IPI receive path
//! (`crate::preempt`'s timer dispatch), not here.
//!
//! # The x86_64 sequence
//!
//! [`X86MachineTakeover::take_over`] performs, in order and without ever
//! returning on success (every other CPU already quiesced):
//!
//! 1. **Mask interrupts** (`cli`) so nothing preempts the solitary CPU. The
//!    x86_64 port wires no lockup watchdog, so there is none to stop.
//! 2. **Switch onto a reserved stack** the sweep will not overwrite. It lives
//!    in the kernel image's `.bss` (reserved), which every address space maps
//!    in the higher half, so the switch is safe under whatever `%cr3` was
//!    active when the Supervisor was entered.
//! 3. **Install the reserved boot page tables** (`%cr3 = boot_pml4`). Unlike
//!    riscv64/aarch64, long mode cannot drop paging, so instead of flattening
//!    the MMU the takeover switches to the boot page tables — which live
//!    entirely in `.boot.bss` (reserved) and map both the higher-half kernel
//!    window (through which the sweep reaches physical RAM) and the low
//!    identity window — so the sweep never depends on a page-table frame in
//!    the *usable* RAM it is about to destroy.
//! 4. **Run the sweep** (the arch-neutral whole-RAM test of every *usable*
//!    frame, which renders progress to the console) on the reserved stack.
//!    The Supervisor's `memtest` sweep tests all of RAM continuously and
//!    never returns; the operator ends the run by resetting the machine.
//!    Should a `sweep` ever return, the CPU parks in a masked `hlt` loop
//!    rather than resume kernel code — the machine has been torn down.
//!
//! The one region the sweep cannot test is the memory it executes from — the
//! kernel image and its reserved stack — because a continuous run must keep
//! that resident image intact to go on running, exactly as a running memtest86
//! cannot test its own resident code.
//!
//! This body owns no pre-teardown refusal of its own; the quiesce refusal is
//! the caller's and is fail-closed there.

use tairix_arch_api::{MachineTakeover, TakeoverError};

extern "C" {
    /// The boot PML4 (`boot.s`, `.boot.bss`), linked 1:1 in low memory so its
    /// symbol address **is** its physical address — the reserved `%cr3` the
    /// sweep runs under.
    static boot_pml4: u8;
}

/// Size of the reserved takeover stack, in bytes (64 KiB — matching the
/// riscv64/aarch64 ports and the boot stack's headroom). It lives in the
/// kernel image's `.bss` (reserved), so the sweep (which tests only *usable*
/// frames) never overwrites it.
const TAKEOVER_STACK_BYTES: usize = 64 * 1024;

/// The reserved takeover stack the sweep runs on.
///
/// `UnsafeCell` because the CPU writes through it via `%rsp` while the Rust
/// aliasing model would otherwise treat a plain `static` as immutable; it is
/// never read as a Rust value, only used as raw stack backing. A takeover
/// happens at most once per boot, so there is no concurrent access.
#[repr(C, align(16))]
struct TakeoverStack {
    bytes: core::cell::UnsafeCell<[u8; TAKEOVER_STACK_BYTES]>,
}

// SAFETY: the stack is only ever used as raw `%rsp` backing on the single CPU
// the takeover has quiesced every other CPU away from; there is no concurrent
// access and it is never read as a typed value.
unsafe impl Sync for TakeoverStack {}

/// The one reserved takeover stack. `.bss`-resident (zeroed), inside the
/// kernel image, so it survives the usable-RAM sweep.
static TAKEOVER_STACK: TakeoverStack = TakeoverStack {
    bytes: core::cell::UnsafeCell::new([0u8; TAKEOVER_STACK_BYTES]),
};

/// The x86_64 machine-takeover handle (`plans/NEW-SUPERVISOR.md` §9).
///
/// A zero-sized unit: the takeover needs no per-instance state (the reserved
/// stack and the boot page tables are all `static`/linker-provided). Held by
/// the downstream `KernelArch` wrapper behind the supervisor-gated accessor
/// and never constructed elsewhere.
pub struct X86MachineTakeover;

/// The single `'static` takeover handle. Private to the crate: the only way
/// to reach it is [`machine_takeover_handle`], which the downstream boot
/// wrapper calls from its supervisor-gated `KernelArch::machine_takeover`.
static X86_TAKEOVER: X86MachineTakeover = X86MachineTakeover;

/// Hand back the `'static` x86_64 machine-takeover handle.
///
/// The downstream boot wrapper's `KernelArch::machine_takeover` — itself gated
/// on the supervisor-only `TakeoverGrant` — is the only caller, so the
/// takeover mechanism stays reachable exclusively from the confirmed
/// `memtest` path.
#[must_use]
pub fn machine_takeover_handle() -> &'static (dyn MachineTakeover + Sync) {
    &X86_TAKEOVER
}

extern "C" {
    /// Install the reserved stack and tail-call
    /// [`tairix_arch_x86_64_takeover_continue`] (`takeover.s`). Never returns.
    fn _takeover_switch_stack(thin: usize, stack_top: usize) -> !;
}

impl MachineTakeover for X86MachineTakeover {
    unsafe fn take_over(&self, sweep: &mut dyn FnMut()) -> TakeoverError {
        // Every other CPU has already been quiesced by the architecture-neutral
        // caller (the Supervisor's `drive_takeover` runs the cross-CPU stop
        // handshake before this is ever reached), so this CPU is the only one
        // running. This body owns only the single-CPU tear-down that follows.

        // 1. Mask interrupts so nothing preempts the solitary CPU. There is
        //    no lockup watchdog wired on this port, so there is none to stop.
        // SAFETY: `cli` clears only `RFLAGS.IF`; this is the deliberate,
        // confirmed tear-down the caller's `TakeoverGrant` authorises.
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        }

        // 2. Switch onto the reserved stack and run the sweep, which never
        //    returns. The sweep handle is reached through a thin pointer to
        //    the caller's `&mut dyn FnMut()`, read once at entry (on the
        //    still-live caller stack) before the switch. The reserved stack is
        //    `.bss`, mapped in the higher half under whatever `%cr3` is active,
        //    so the switch is valid before the boot page tables are installed
        //    (step 3, in the continuation).
        let mut sweep_ref: &mut dyn FnMut() = sweep;
        let thin = core::ptr::addr_of_mut!(sweep_ref) as usize;
        let stack_top = core::ptr::addr_of!(TAKEOVER_STACK) as usize + TAKEOVER_STACK_BYTES;
        // SAFETY: `_takeover_switch_stack` installs the 16-byte-aligned top of
        // the reserved `.bss` takeover stack and tail-calls
        // `tairix_arch_x86_64_takeover_continue(thin)`; `thin` addresses a
        // live `&mut dyn FnMut()` in reserved memory. It never returns.
        unsafe { _takeover_switch_stack(thin, stack_top) }
    }
}

/// Install the reserved boot page tables and run the whole-RAM sweep on the
/// reserved stack; never returns. Entered from `_takeover_switch_stack` with
/// `%rsp` already on the reserved takeover stack.
///
/// # Safety
///
/// Reached only from [`X86MachineTakeover::take_over`] after interrupts are
/// masked. `thin` is a live pointer to the caller's `&mut dyn FnMut()` sweep
/// handle, whose environment resides in reserved memory the sweep does not
/// destroy.
#[no_mangle]
unsafe extern "C" fn tairix_arch_x86_64_takeover_continue(thin: usize) -> ! {
    // 3. Install the reserved boot page tables (`%cr3 = boot_pml4`). They live
    //    in `.boot.bss` (reserved) and map both the higher-half kernel window
    //    the sweep writes physical RAM through and the low identity window, so
    //    nothing the sweep destroys is depended upon. `boot_pml4`'s symbol
    //    address is its physical address (linked 1:1 in low memory).
    let boot_cr3 = core::ptr::addr_of!(boot_pml4) as u64;
    // SAFETY: `boot_pml4` is the boot page-table root the kernel itself booted
    // on; loading it re-establishes the reserved mapping. Its higher-half
    // window maps this continuation's code and the reserved stack, so
    // execution and the stack survive the load; `mov`-to-`%cr3` flushes the
    // non-global TLB.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) boot_cr3, options(nostack, preserves_flags));
    }

    // 4. Run the whole-RAM sweep over every usable frame.
    // SAFETY: `thin` points at the live `&mut dyn FnMut()` the caller placed
    // on its stack; reconstructing and calling it runs the architecture-neutral
    // whole-RAM sweep, which reads/writes only reserved state and the
    // physical RAM it is meant to destroy.
    let sweep = unsafe { &mut *(thin as *mut &mut dyn FnMut()) };
    sweep();

    // The Supervisor's `memtest` sweep loops until the operator resets the
    // machine, so control never reaches here. If a future finite sweep ever
    // returned, the machine has been torn down (usable RAM overwritten) and
    // must not resume kernel code, so park the sole CPU.
    loop {
        // SAFETY: interrupts are masked; `hlt` merely idles the parked CPU
        // until the operator resets the machine.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
