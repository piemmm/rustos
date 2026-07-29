//! aarch64 machine-takeover mechanism (`plans/NEW-SUPERVISOR.md` §9 Stage
//! B).
//!
//! The Arch HAL [`MachineTakeover`] body for the ARMv8-A `virt` / Raspberry
//! Pi 4 port: the irreversible, one-way sequence the pre-boot Supervisor's
//! `memtest` drives to test **all** of RAM. It is deliberately the only
//! public surface of this module — the takeover `static` is private and
//! reachable solely through the supervisor-gated [`machine_takeover_handle`],
//! which the downstream boot wrapper calls from its
//! `KernelArch::machine_takeover` accessor.
//!
//! # Where the cross-CPU quiesce lives
//!
//! Stopping every *other* CPU is **not** done here: it is architecture-neutral
//! (a stop request, the directed IPI, the boot-published liveness/ack tables)
//! and is driven by the caller before this body runs — the Supervisor's
//! `drive_takeover` calls `tairix_arch_api::quiesce_others`, which returns
//! [`TakeoverError::CpuQuiesceTimeout`] fail-closed if a peer will not halt, so
//! this body is only ever reached once this core is the sole one running. The
//! one per-silicon half of quiesce — *parking* a stopped core — lives in the
//! IPI receive path (`crate::preempt::on_ipi_interrupt`), not here.
//!
//! # The aarch64 sequence
//!
//! [`Aarch64MachineTakeover::take_over`] performs, in order and without ever
//! returning on success (every other CPU already quiesced):
//!
//! 1. **Mask interrupts** (`DAIFSet` — all of debug/SError/IRQ/FIQ) so
//!    nothing preempts the solitary core, and **stop the lockup watchdog**
//!    by disabling its `CNTV` virtual-timer cadence (`CNTV_CTL_EL0 = 0`).
//! 2. **Switch onto a reserved stack** the sweep will not overwrite and run
//!    the caller's `sweep` (the arch-neutral whole-RAM test of every *usable*
//!    frame, which renders progress to the console). The Supervisor's
//!    `memtest` sweep tests all of RAM continuously and never returns; the
//!    operator ends the run by resetting the board. Should a `sweep` ever
//!    return, the core parks in a masked `wfi` halt rather than resume kernel
//!    code — the machine has been torn down.
//!
//! # Why the MMU stays on
//!
//! The takeover deliberately does **not** disable the stage-1 MMU. The kernel
//! runs under an *identity* map (`virtual == physical` over the discovered
//! RAM gigapages), and the sweep reaches every physical frame through that
//! same identity map (`KernelArch::direct_phys_map`), so no flattening is
//! needed to address RAM directly. Disabling the MMU would be actively wrong
//! on real ARMv8-A silicon: with `SCTLR_EL1.M == 0` every data access is
//! **Device-nGnRnE**, where an unaligned access faults unconditionally — the
//! framebuffer console, `memcpy`/`memset`, and the sweep's own bookkeeping
//! all issue unaligned accesses, so an MMU-off sweep takes an alignment fault
//! with interrupts masked and wedges the board. (A permissive emulator that
//! ignores Device-memory alignment rules hides this, which is exactly how the
//! MMU-off form passed under QEMU while locking a Raspberry Pi 4.) Keeping the
//! MMU on leaves the identity mappings Normal cacheable and alignment-safe;
//! the arch-neutral engine still tests genuine DRAM cells because it flushes
//! each tested word to the point of coherency between the write and the
//! read-back through [`PhysMap::clean_invalidate`](tairix_kernel_mem::PhysMap),
//! which this port backs with real `dc civac` cache maintenance.
//!
//! The one region the sweep cannot test is the memory it executes from — the
//! kernel image and its reserved stack — because a continuous run must keep
//! that resident image intact to go on running, exactly as a running memtest86
//! cannot test its own resident code.
//!
//! The takeover needs no reset conduit: it never resets the machine itself
//! (the operator power-cycles or resets the board), so it is available on
//! every aarch64 board regardless of whether the firmware tree declared a
//! `/psci` node.

use tairix_arch_api::{MachineTakeover, TakeoverError};

/// Size of the reserved takeover stack, in bytes (64 KiB — the same
/// generous headroom the boot stack reserves, matching the riscv64 port).
/// It lives in `.bss` inside the kernel image, so the sweep (which tests
/// only *usable* frames) never overwrites it.
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
// core the takeover has quiesced every other CPU away from; there is no
// concurrent access and it is never read as a typed value.
unsafe impl Sync for TakeoverStack {}

/// The one reserved takeover stack. `.bss`-resident (zeroed), inside the
/// kernel image, so it survives the usable-RAM sweep.
static TAKEOVER_STACK: TakeoverStack = TakeoverStack {
    bytes: core::cell::UnsafeCell::new([0u8; TAKEOVER_STACK_BYTES]),
};

/// The aarch64 machine-takeover handle (`plans/NEW-SUPERVISOR.md` §9).
///
/// A zero-sized unit: the takeover needs no per-instance state (the
/// reserved stack is `static`). Held by the downstream `KernelArch` wrapper
/// behind the supervisor-gated accessor and never constructed elsewhere.
pub struct Aarch64MachineTakeover;

/// The single `'static` takeover handle. Private to the crate.
static AARCH64_TAKEOVER: Aarch64MachineTakeover = Aarch64MachineTakeover;

/// Hand back the `'static` aarch64 machine-takeover handle.
///
/// The downstream boot wrapper's `KernelArch::machine_takeover` — itself
/// gated on the supervisor-only `TakeoverGrant` — is the only caller, so
/// the takeover mechanism stays reachable exclusively from the confirmed
/// `memtest` path. The takeover never resets the board itself (the operator
/// does), so it needs no reset conduit and is available regardless of the
/// firmware's PSCI facilities.
#[must_use]
pub fn machine_takeover_handle() -> &'static (dyn MachineTakeover + Sync) {
    &AARCH64_TAKEOVER
}

extern "C" {
    /// Install the reserved stack and tail-call
    /// [`tairix_arch_aarch64_takeover_continue`] (`takeover.s`). Never
    /// returns.
    fn _takeover_switch_stack(thin: usize, stack_top: usize) -> !;
}

impl MachineTakeover for Aarch64MachineTakeover {
    unsafe fn take_over(&self, sweep: &mut dyn FnMut()) -> TakeoverError {
        // Every other CPU has already been quiesced by the architecture-neutral
        // caller (the Supervisor's `drive_takeover` runs the cross-CPU stop
        // handshake before this is ever reached), so this core is the only one
        // running. This body owns only the single-CPU tear-down that follows.

        // 1. Mask every interrupt (DAIF: debug, SError, IRQ, FIQ) so nothing
        //    preempts the solitary core, and stop the lockup watchdog by
        //    disabling its virtual-timer cadence — masking `DAIF.F` already
        //    prevents its (Group-0/FIQ) sample from being taken, and
        //    clearing `CNTV_CTL_EL0` also stops the timer condition itself.
        // SAFETY: `DAIFSet` and the `CNTV_CTL_EL0` write are well-defined
        // EL1 operations; this is the deliberate, confirmed tear-down the
        // caller's `TakeoverGrant` authorises.
        unsafe {
            core::arch::asm!(
                "msr DAIFSet, #0xf",
                "msr CNTV_CTL_EL0, xzr",
                options(nostack, preserves_flags),
            );
        }

        // The MMU stays on: the kernel's identity map already resolves every
        // physical frame `virtual == physical` and is Normal cacheable and
        // alignment-safe, whereas an MMU-off EL1 would make every access
        // Device-nGnRnE and fault the sweep's unaligned accesses (see the
        // module docs). The arch-neutral engine still tests real DRAM: it
        // flushes each tested word to the point of coherency around the
        // read-back through the direct map's `dc civac` maintenance.

        // 2. Switch onto the reserved stack and run the sweep, which never
        //    returns. The sweep handle is reached through a thin pointer to
        //    the caller's `&mut dyn FnMut()`, which lives on the caller's
        //    stack and is read once at entry (before the sweep destroys
        //    usable RAM).
        let mut sweep_ref: &mut dyn FnMut() = sweep;
        let thin = core::ptr::addr_of_mut!(sweep_ref) as usize;
        let stack_top = core::ptr::addr_of!(TAKEOVER_STACK) as usize + TAKEOVER_STACK_BYTES;
        // SAFETY: `_takeover_switch_stack` installs the 16-byte-aligned top
        // of the reserved `.bss` takeover stack and tail-calls
        // `tairix_arch_aarch64_takeover_continue(thin)`; `thin` addresses a
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
/// Reached only from [`Aarch64MachineTakeover::take_over`] after interrupts
/// are masked (the MMU stays on under the identity map). `thin` is a live
/// pointer to the caller's `&mut dyn FnMut()` sweep handle, whose environment
/// resides in reserved memory the sweep does not destroy.
#[no_mangle]
unsafe extern "C" fn tairix_arch_aarch64_takeover_continue(thin: usize) -> ! {
    // SAFETY: `thin` points at the live `&mut dyn FnMut()` the caller placed
    // on its stack; reconstructing and calling it runs the
    // architecture-neutral whole-RAM sweep over every usable frame.
    let sweep = unsafe { &mut *(thin as *mut &mut dyn FnMut()) };
    sweep();
    // The Supervisor's `memtest` sweep loops until the operator resets the
    // board, so control never reaches here. If a future finite sweep ever
    // returned, the machine has been torn down (every other CPU quiesced,
    // usable RAM overwritten) and must not resume kernel code, so park the
    // sole core.
    loop {
        // SAFETY: interrupts are masked; `wfi` merely idles the parked core
        // until the operator resets the machine.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}
