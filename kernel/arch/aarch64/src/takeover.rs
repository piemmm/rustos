//! aarch64 machine-takeover mechanism (`plans/NEW-SUPERVISOR.md` §9 Stage
//! B).
//!
//! The Arch HAL [`MachineTakeover`] body for the ARMv8-A `virt` / Raspberry
//! Pi 4 port: the irreversible, one-way sequence the pre-boot Supervisor's
//! `memtest full` drives to test **all** of RAM. It is deliberately the only
//! public surface of this module — the takeover `static` is private and
//! reachable solely through the supervisor-gated
//! [`machine_takeover_handle`], which the downstream boot wrapper calls from
//! its `KernelArch::machine_takeover` accessor.
//!
//! # The aarch64 sequence
//!
//! [`Aarch64MachineTakeover::take_over`] performs, in order and without ever
//! returning on success:
//!
//! 1. **Confirm this is the only running core.** The production images boot
//!    a single core and install no secondary entry, so there is nothing to
//!    quiesce; the step still *verifies* it and fails closed
//!    ([`TakeoverError::CpuQuiesceTimeout`]) rather than assuming it — a
//!    non-zero [`crate::smp::secondary_entry_addr`] means SMP was wired
//!    without teaching this takeover to cooperatively stop the secondaries.
//! 2. **Resolve the reset conduit.** The reset instruction (`hvc`/`smc`) is
//!    the discovered PSCI conduit, published by [`machine_takeover_handle`].
//!    With none known there is no way to reset, so the takeover refuses
//!    fail-closed ([`TakeoverError::NotSupported`]) before touching anything.
//! 3. **Mask interrupts** (`DAIFSet` — all of debug/SError/IRQ/FIQ) so
//!    nothing preempts the solitary core, and **stop the lockup watchdog**
//!    by disabling its `CNTV` virtual-timer cadence (`CNTV_CTL_EL0 = 0`).
//! 4. **Clean+invalidate** the kernel-image region's cache lines to the
//!    point of coherency so RAM holds the current bytes, then **flatten
//!    paging** by writing the known MMU-off `SCTLR_EL1`
//!    ([`crate::paging::SCTLR_MMU_OFF`], clearing `M`/`C`/`I`). The kernel
//!    runs under an *identity* map (`virtual == physical`), so dropping the
//!    MMU leaves every address resolving to the same physical byte; every
//!    access is then Normal Non-cacheable, so the destructive sweep reaches
//!    RAM directly.
//! 5. **Switch onto a reserved stack** the sweep will not overwrite and run
//!    the caller's `sweep` (the arch-neutral destructive test of every
//!    *usable* frame, which renders progress to the console).
//! 6. **Test the region the sweep could not** — the kernel image and the
//!    stack it ran on, `[__kernel_start, __kernel_end)` — with the
//!    relocatable, register-only `_takeover_stub` copied into a just-swept
//!    usable page, then reset via the resolved PSCI conduit. The stub never
//!    touches the firmware/DTB reserved region below the kernel image, so
//!    the reset call still works.
//!
//! On any pre-destructive refusal `take_over` returns the [`TakeoverError`]
//! with the machine untouched and `sweep` un-run (fail closed, never a
//! panic).

use core::sync::atomic::{AtomicU8, Ordering};

use tairix_arch_api::{MachineTakeover, TakeoverError};

use crate::fdt::PsciMethod;

extern "C" {
    /// First byte of the kernel image
    /// (`kernel/arch/aarch64/link/aarch64-*.ld`). The inclusive lower bound
    /// of the region the relocated stub self-tests — everything below it is
    /// firmware / DTB / low reserved RAM the stub must never touch.
    static __kernel_start: u8;
    /// One past the end of the kernel image + boot heap. The exclusive upper
    /// bound of the stub's self-test region; 4 KiB-aligned by the linker.
    static __kernel_end: u8;
}

/// Conduit code stored in [`TAKEOVER_CONDUIT`]: no conduit resolved yet
/// (the takeover refuses fail-closed until one is published).
const CONDUIT_UNSET: u8 = 0xFF;
/// Conduit code for a `hvc #0` reset (PSCI at EL2 — QEMU `virt` default).
const CONDUIT_HVC: u8 = 0;
/// Conduit code for a `smc #0` reset (PSCI at EL3).
const CONDUIT_SMC: u8 = 1;

/// The discovered reset conduit, published by [`machine_takeover_handle`]
/// from the boot path's `/psci` `method` before the handle is used.
static TAKEOVER_CONDUIT: AtomicU8 = AtomicU8::new(CONDUIT_UNSET);

/// Size of the reserved takeover stack, in bytes (64 KiB — the same
/// generous headroom the boot stack reserves, matching the riscv64 port).
/// It lives in `.bss` inside the kernel image, so the sweep (which tests
/// only *usable* frames) never overwrites it; the relocated stub tests it
/// as part of the kernel-image region *after* the sweep has finished using
/// it.
const TAKEOVER_STACK_BYTES: usize = 64 * 1024;

/// The reserved takeover stack the sweep runs on.
///
/// `UnsafeCell` because the CPU writes through it via `sp` while the Rust
/// aliasing model would otherwise treat a plain `static` as immutable; it
/// is never read as a Rust value, only used as raw stack backing. A
/// takeover happens at most once per boot (the machine resets), so there is
/// no concurrent access.
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

/// Round `addr` up to the next multiple of `align` (a power of two).
///
/// Operates in `usize` because its only caller works in native addresses
/// (the kernel-image bounds and the scratch page), so there is no lossy
/// `u64`↔`usize` cast on the destructive path.
const fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

/// The aarch64 machine-takeover handle (`plans/NEW-SUPERVISOR.md` §9).
///
/// A zero-sized unit: the takeover needs no per-instance state (the
/// reserved stack, the kernel-image bounds, the relocatable stub, and the
/// resolved conduit are all `static`/linker-provided). Held by the
/// downstream `KernelArch` wrapper behind the supervisor-gated accessor and
/// never constructed elsewhere.
pub struct Aarch64MachineTakeover;

/// The single `'static` takeover handle. Private to the crate.
static AARCH64_TAKEOVER: Aarch64MachineTakeover = Aarch64MachineTakeover;

/// Publish the discovered PSCI reset conduit and hand back the `'static`
/// aarch64 machine-takeover handle.
///
/// The downstream boot wrapper's `KernelArch::machine_takeover` — itself
/// gated on the supervisor-only `TakeoverGrant` — is the only caller, so
/// the destructive mechanism stays reachable exclusively from the confirmed
/// `memtest full` path. It threads the conduit the boot path discovered from
/// the `/psci` node so the relocated stub can reset through it (there is no
/// fixed reset instruction on aarch64 the way riscv64 has the SBI ecall).
#[must_use]
pub fn machine_takeover_handle(method: PsciMethod) -> &'static (dyn MachineTakeover + Sync) {
    let code = match method {
        PsciMethod::Hvc => CONDUIT_HVC,
        PsciMethod::Smc => CONDUIT_SMC,
    };
    TAKEOVER_CONDUIT.store(code, Ordering::Release);
    &AARCH64_TAKEOVER
}

extern "C" {
    /// The relocatable, register-only kernel-image self-test + reset stub
    /// (`takeover.s`). Its address and [`_takeover_stub_end`] bound the
    /// bytes copied into the scratch page.
    fn _takeover_stub();
    /// One past the last byte of [`_takeover_stub`].
    fn _takeover_stub_end();
    /// Install the reserved stack and tail-call
    /// [`tairix_arch_aarch64_takeover_continue`] (`takeover.s`). Never
    /// returns.
    fn _takeover_switch_stack(thin: usize, stack_top: usize) -> !;
}

impl MachineTakeover for Aarch64MachineTakeover {
    unsafe fn take_over(&self, sweep: &mut dyn FnMut()) -> TakeoverError {
        // 1. Confirm this is the only running core. The production images
        //    are single-core and install no secondary entry, so
        //    `secondary_entry_addr()` is 0 and there is nothing to quiesce.
        //    A non-zero entry means SMP bring-up was wired without teaching
        //    this takeover to cooperatively stop the secondaries, so it
        //    refuses fail-closed rather than destroy RAM another core is
        //    still using. (Logical CPU 1 is the first secondary; the exact
        //    id is cosmetic on a path that cannot occur in a single-core
        //    image.)
        if crate::smp::secondary_entry_addr() != 0 {
            return TakeoverError::CpuQuiesceTimeout { cpu: 1 };
        }

        // 2. Resolve the reset conduit the boot path discovered. Without one
        //    there is no way to reset the board, so refuse fail-closed
        //    before masking interrupts or touching paging.
        let conduit = TAKEOVER_CONDUIT.load(Ordering::Acquire);
        if conduit == CONDUIT_UNSET {
            return TakeoverError::NotSupported;
        }

        // 3. Mask every interrupt (DAIF: debug, SError, IRQ, FIQ) so nothing
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

        // 4. Clean+invalidate the kernel-image region's cache lines to the
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

        // 5. Switch onto the reserved stack and run the sweep, then test the
        //    kernel-image region and reset — none of which returns. The
        //    sweep handle is reached through a thin pointer to the caller's
        //    `&mut dyn FnMut()`, which lives on the caller's stack and is
        //    read once at entry (before the sweep destroys usable RAM).
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

/// Run the destructive sweep on the reserved stack, then test the region it
/// executed from and reset. Entered from `_takeover_switch_stack` with `sp`
/// already installed on the reserved takeover stack; never returns.
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
    // architecture-neutral destructive sweep over every usable frame.
    let sweep = unsafe { &mut *(thin as *mut &mut dyn FnMut()) };
    sweep();
    // The sweep tested every *usable* frame. The one region it could not
    // touch is the memory it ran from — the kernel image and this stack.
    // SAFETY: the sweep has completed; relocating the register-only stub
    // into a swept usable page and jumping to it tests that region and
    // resets, never returning.
    unsafe { relocate_stub_and_reset() }
}

/// Copy the relocatable stub into a scratch usable page and jump to it to
/// test `[__kernel_start, __kernel_end)` and reset. Never returns.
///
/// # Safety
///
/// Called only from [`tairix_arch_aarch64_takeover_continue`] after the
/// usable-RAM sweep, with interrupts masked and paging flattened. The
/// scratch page is the first page above the kernel image — RAM outside the
/// self-test region, executable under the MMU-off regime — and is
/// overwritten wholesale, so it must not hold anything still needed.
unsafe fn relocate_stub_and_reset() -> ! {
    let start = core::ptr::addr_of!(__kernel_start) as usize;
    let end = core::ptr::addr_of!(__kernel_end) as usize;
    // Scratch page: the first page above the kernel image. It lies outside
    // the `[start, end)` region the stub destroys, is plain RAM on both
    // boards (well below `ram_end`), and is executable under the MMU-off
    // regime. `__kernel_end` is already 4 KiB-aligned.
    let scratch = align_up(end, crate::paging::PAGE_SIZE);
    let stub = _takeover_stub as *const () as usize;
    let stub_end = _takeover_stub_end as *const () as usize;
    let len = stub_end - stub;
    let conduit = usize::from(TAKEOVER_CONDUIT.load(Ordering::Acquire));
    // SAFETY: `[stub, stub_end)` is the relocatable stub in the (still
    // intact) kernel image; `scratch` is a distinct page of RAM. The two do
    // not overlap. Copying its bytes then `dsb`/`ic iallu`/`isb` makes the
    // copy fetchable (I-cache is off, but the barrier ordering is required
    // before the branch); the final `br` enters the copy with the self-test
    // bounds in `x0`/`x1` and the conduit code in `x2` (bound as explicit
    // input registers so the destination register can never alias them) and
    // never returns (the stub resets the machine).
    unsafe {
        core::ptr::copy_nonoverlapping(stub as *const u8, scratch as *mut u8, len);
        core::arch::asm!(
            "dsb sy",
            "ic iallu",
            "dsb sy",
            "isb",
            "br {dst}",
            in("x0") start,
            in("x1") end,
            in("x2") conduit,
            dst = in(reg) scratch,
            options(noreturn, nostack),
        );
    }
}
