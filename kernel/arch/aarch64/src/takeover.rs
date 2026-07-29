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
//! 2. **Clean+invalidate** the kernel-image region's cache lines to the
//!    point of coherency so RAM holds the current bytes, then **flatten
//!    paging** by writing the known MMU-off `SCTLR_EL1`
//!    ([`crate::paging::SCTLR_MMU_OFF`], clearing `M`/`C`/`I`). The kernel
//!    runs under an *identity* map (`virtual == physical`), so dropping the
//!    MMU leaves every address resolving to the same physical byte; every
//!    access is then Normal Non-cacheable, so the whole-RAM sweep reaches
//!    RAM directly.
//! 3. **Switch onto a reserved stack** the sweep will not overwrite and run
//!    the caller's `sweep` (the arch-neutral whole-RAM test of every *usable*
//!    frame, which renders progress to the console). The Supervisor's
//!    `memtest` sweep tests all of RAM continuously and never returns; the
//!    operator ends the run by resetting the board. Should a `sweep` ever
//!    return, the core parks in a masked `wfi` halt rather than resume kernel
//!    code — the machine has been torn down.
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

extern "C" {
    /// First byte of the kernel image
    /// (`kernel/arch/aarch64/link/aarch64-*.ld`). Everything below it is
    /// firmware / DTB / low reserved RAM.
    static __kernel_start: u8;
    /// One past the end of the kernel image + boot heap; 4 KiB-aligned by the
    /// linker.
    static __kernel_end: u8;
}

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
/// reserved stack and the kernel-image bounds are all `static`/linker-
/// provided). Held by the downstream `KernelArch` wrapper behind the
/// supervisor-gated accessor and never constructed elsewhere.
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

        // 2. Clean+invalidate the kernel-image region's cache lines to the
        //    point of coherency so RAM holds the current bytes before the
        //    MMU (and the data cache with it) goes away — after flattening,
        //    every access is Normal Non-cacheable and would miss a dirty
        //    line. The usable RAM about to be destroyed needs no cleaning.
        let start = core::ptr::addr_of!(__kernel_start) as usize;
        let end = core::ptr::addr_of!(__kernel_end) as usize;
        crate::paging::clean_invalidate_range_to_poc(start as u64, (end - start) as u64);

        // Flatten paging: write the known MMU-off `SCTLR_EL1` (M/C/I clear),
        // then invalidate the stale TLB and instruction cache. The kernel is
        // identity-mapped, so `pc`/`sp`/MMIO keep their addresses across the
        // switch; every access afterwards is Normal Non-cacheable.
        // SAFETY: `SCTLR_MMU_OFF` is the architecturally-defined MMU-off
        // control value the boot stub itself installs; the barriers order
        // the disable, and the identity map makes the very next fetch (still
        // at the same physical address) valid under the bare regime.
        unsafe {
            core::arch::asm!(
                "dsb sy",
                "msr SCTLR_EL1, {sctlr}",
                "isb",
                "tlbi vmalle1",
                "dsb sy",
                "ic iallu",
                "dsb sy",
                "isb",
                sctlr = in(reg) crate::paging::SCTLR_MMU_OFF,
                options(nostack, preserves_flags),
            );
        }

        // 3. Switch onto the reserved stack and run the sweep, which never
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
/// are masked and paging is flattened. `thin` is a live pointer to the
/// caller's `&mut dyn FnMut()` sweep handle, whose environment resides in
/// reserved memory the sweep does not destroy.
#[no_mangle]
unsafe extern "C" fn tairix_arch_aarch64_takeover_continue(thin: usize) -> ! {
    // SAFETY: `thin` points at the live `&mut dyn FnMut()` the caller placed
    // on its stack; reconstructing and calling it runs the
    // architecture-neutral whole-RAM sweep over every usable frame.
    let sweep = unsafe { &mut *(thin as *mut &mut dyn FnMut()) };
    sweep();
    // The Supervisor's `memtest` sweep loops until the operator resets the
    // board, so control never reaches here. If a future finite sweep ever
    // returned, the machine has been torn down (paging flattened, usable RAM
    // overwritten) and must not resume kernel code, so park the sole core.
    loop {
        // SAFETY: interrupts are masked and paging is flattened; `wfi` merely
        // idles the parked core until the operator resets the machine.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}
