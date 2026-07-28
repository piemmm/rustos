//! riscv64 machine-takeover mechanism (`plans/NEW-SUPERVISOR.md` §9 Stage
//! B).
//!
//! The Arch HAL [`MachineTakeover`] body for the QEMU `virt` / SiFive
//! `virt`-class port: the irreversible, one-way sequence the pre-boot
//! Supervisor's `memtest full` drives to test **all** of RAM. It is
//! deliberately the *only* public surface of this module — the takeover
//! `static` is private and reachable solely through the supervisor-gated
//! `KernelArch::machine_takeover` accessor the downstream boot wrapper
//! implements.
//!
//! # The riscv64 sequence
//!
//! [`RiscvMachineTakeover::take_over`] performs, in order and without ever
//! returning on success:
//!
//! 1. **Confirm this is the only running hart.** The production `virt`
//!    image is single-hart and starts no secondary, so there is nothing to
//!    quiesce; the step still *verifies* it — every other hart the port
//!    could address must report the SBI HSM `STOPPED` state — and fails
//!    closed ([`TakeoverError::CpuQuiesceTimeout`]) rather than assuming it.
//! 2. **Mask S-mode interrupts** (`sstatus.SIE = 0`, `sie = 0`) so nothing
//!    preempts the solitary hart. There is no lockup watchdog wired on this
//!    port, so there is none to stop.
//! 3. **Flatten paging** to bare mode (`satp = 0`). The kernel runs under an
//!    Sv39 *identity* map (`virtual == physical`), so dropping to bare mode
//!    leaves every address — the running `pc`, the boot page tables, the
//!    console MMIO — resolving to the same physical byte; nothing moves.
//! 4. **Switch onto a reserved stack** the sweep will not overwrite and run
//!    the caller's `sweep` (the arch-neutral destructive test of every
//!    *usable* frame, which renders progress to the console).
//! 5. **Test the region the sweep could not** — the kernel image and the
//!    stack the sweep ran on, `[__kernel_image_start, __kernel_end)` — with
//!    the relocatable, register-only `_takeover_stub` copied into a
//!    just-swept usable page, then **SBI System-Reset**. The stub never
//!    touches the OpenSBI firmware below the kernel image, so the reset
//!    ecall still works.
//!
//! On any pre-destructive refusal `take_over` returns the [`TakeoverError`]
//! with the machine untouched and `sweep` un-run (fail closed, never a
//! panic).

use tairix_arch_api::{MachineTakeover, TakeoverError};

extern "C" {
    /// One past the end of the kernel image + boot heap
    /// (`kernel/arch/riscv64/link/riscv64-virt.ld`). The exclusive upper
    /// bound of the region the relocated stub self-tests.
    static __kernel_end: u8;
    /// First byte of the kernel image (the `_start` trampoline load
    /// address, `0x8020_0000`). The inclusive lower bound of the stub's
    /// self-test region — everything below it is OpenSBI firmware the stub
    /// must never touch.
    static __kernel_image_start: u8;
}

/// Size of the reserved takeover stack, in bytes (64 KiB — the same
/// generous headroom the boot stack reserves). It lives in `.bss` inside
/// the kernel image, so the sweep (which tests only *usable* frames) never
/// overwrites it; the relocated stub tests it as part of the kernel-image
/// region *after* the sweep has finished using it.
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
// hart the takeover has quiesced every other CPU away from; there is no
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

/// The riscv64 machine-takeover handle (`plans/NEW-SUPERVISOR.md` §9).
///
/// A zero-sized unit: the takeover needs no per-instance state (the
/// reserved stack, the kernel-image bounds, and the relocatable stub are
/// all `static`/linker-provided). Held by the downstream `KernelArch`
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
/// the destructive mechanism stays reachable exclusively from the confirmed
/// `memtest full` path.
#[must_use]
pub fn machine_takeover_handle() -> &'static (dyn MachineTakeover + Sync) {
    &RISCV_TAKEOVER
}

extern "C" {
    /// The relocatable, register-only kernel-image self-test + reset stub
    /// (`takeover.s`). Its address and [`_takeover_stub_end`] bound the
    /// bytes copied into the scratch page.
    fn _takeover_stub();
    /// One past the last byte of [`_takeover_stub`].
    fn _takeover_stub_end();
    /// Install the reserved stack and tail-call
    /// [`tairix_arch_riscv64_takeover_continue`] (`takeover.s`). Never
    /// returns.
    fn _takeover_switch_stack(thin: usize, stack_top: usize) -> !;
}

impl MachineTakeover for RiscvMachineTakeover {
    unsafe fn take_over(&self, sweep: &mut dyn FnMut()) -> TakeoverError {
        // 1. Confirm this is the only running hart. The production `virt`
        //    image is single-hart and installs no secondary entry, so
        //    `secondary_entry_addr()` is 0 and there is nothing to quiesce.
        //    A non-zero entry means SMP bring-up was wired without teaching
        //    this takeover to cooperatively stop the secondaries, so it
        //    refuses fail-closed rather than destroy RAM another hart is
        //    still using. (Logical CPU 1 is the first secondary; the exact
        //    id is cosmetic on a path that cannot occur in the shipped
        //    single-hart image.)
        if crate::smp::secondary_entry_addr() != 0 {
            return TakeoverError::CpuQuiesceTimeout { cpu: 1 };
        }

        // 2. Mask S-mode interrupts so nothing preempts the solitary hart:
        //    clear `sstatus.SIE`, then disable every S-mode interrupt
        //    source (`sie = 0`). There is no lockup watchdog wired on this
        //    port, so there is none to stop.
        // 3. Flatten paging to bare mode (`satp = 0`) and flush the TLB.
        //    The kernel runs identity-mapped (virtual == physical), so
        //    every address keeps resolving to the same physical byte.
        // SAFETY: all four are well-defined S-mode CSR operations. Masking
        // interrupts and flattening paging are the deliberate, confirmed
        // tear-down the caller's `TakeoverGrant` authorises. `sfence.vma`
        // (both operands `x0`) discards the stale Sv39 translations so the
        // bare-mode regime is in force before the next fetch; because the
        // map was identity, `pc`/`sp`/MMIO all keep their addresses.
        unsafe {
            core::arch::asm!(
                "csrci sstatus, 2",
                "csrw sie, zero",
                "csrw satp, zero",
                "sfence.vma",
                options(nostack, preserves_flags),
            );
        }

        // 4. Switch onto the reserved stack and run the sweep, then test the
        //    kernel-image region and reset — none of which returns. The
        //    sweep handle is reached through a thin pointer to the caller's
        //    `&mut dyn FnMut()`, which lives on the caller's (reserved)
        //    stack and so survives the usable-RAM sweep.
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

/// Run the destructive sweep on the reserved stack, then test the region it
/// executed from and reset. Entered from `_takeover_switch_stack` with `sp`
/// already installed on the reserved takeover stack; never returns.
///
/// # Safety
///
/// Reached only from [`RiscvMachineTakeover::take_over`] after interrupts
/// are masked and paging is flattened. `thin` is a live pointer to the
/// caller's `&mut dyn FnMut()` sweep handle, whose environment resides in
/// reserved memory the sweep does not destroy.
#[no_mangle]
unsafe extern "C" fn tairix_arch_riscv64_takeover_continue(thin: usize) -> ! {
    // SAFETY: `thin` points at the live `&mut dyn FnMut()` the caller
    // placed on its reserved stack; reconstructing and calling it runs the
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
/// test `[__kernel_image_start, __kernel_end)` and reset. Never returns.
///
/// # Safety
///
/// Called only from [`tairix_arch_riscv64_takeover_continue`] after the
/// usable-RAM sweep, with interrupts masked and paging flattened. The
/// scratch page is the first page above the kernel image — RAM outside the
/// self-test region, executable under the bare regime — and is overwritten
/// wholesale, so it must not hold anything still needed.
unsafe fn relocate_stub_and_reset() -> ! {
    let start = core::ptr::addr_of!(__kernel_image_start) as usize;
    let end = core::ptr::addr_of!(__kernel_end) as usize;
    // Scratch page: the first page above the kernel image. It lies outside
    // the `[start, end)` region the stub destroys, is plain RAM on the
    // `virt` board (well below `ram_end`), and is executable under the
    // `satp = 0` bare regime. `__kernel_end` is already 4 KiB-aligned.
    let scratch = align_up(end, crate::paging::PAGE_SIZE);
    let stub = _takeover_stub as *const () as usize;
    let stub_end = _takeover_stub_end as *const () as usize;
    let len = stub_end - stub;
    // SAFETY: `[stub, stub_end)` is the relocatable stub in the (still
    // intact) kernel image; `scratch` is a distinct page of RAM. The two do
    // not overlap. Copying its bytes then `fence.i` makes the copy fetchable
    // on this hart (single-hart, so a local instruction-fence suffices);
    // the final `jr` enters the copy with the self-test bounds in `a0`/`a1`
    // (bound as explicit input registers so the destination register can
    // never alias them) and never returns (the stub resets the machine).
    unsafe {
        core::ptr::copy_nonoverlapping(stub as *const u8, scratch as *mut u8, len);
        core::arch::asm!(
            "fence.i",
            "jr {dst}",
            in("a0") start,
            in("a1") end,
            dst = in(reg) scratch,
            options(noreturn, nostack),
        );
    }
}
