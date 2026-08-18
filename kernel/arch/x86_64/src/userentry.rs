//! x86_64 implementation of the Arch HAL "enter user mode" surface
//! ([`tairix_arch_api::EnterUser`]).
//!
//! Dropping a freshly built process image into ring 3 is the `iretq`
//! sequence: build the interrupt-return frame the CPU pops on `iretq`
//! (`SS`, user `RSP`, `RFLAGS`, `CS`, `RIP` — from the top of the
//! kernel stack down) from the ring-3 GDT selectors
//! ([`crate::gdt::USER_CS_INDEX`] / [`crate::gdt::USER_DS_INDEX`], both
//! at RPL 3), place the first-argument value in `rdi` (the System V
//! AMD64 first integer register), and `iretq`. This is the one
//! definition of that sequence; the CC2/CC3 QEMU
//! verticals reach it through the HAL rather than copying the `asm!`
//! block.
//!
//! # The thread pointer
//!
//! x86_64's psABI thread pointer is the **`FS` base**, and unlike aarch64's
//! `TPIDR_EL0` and riscv64's `tp` it is not a user-writable register: with
//! `CR4.FSGSBASE` off, ring 3 cannot program it and `wrmsr IA32_FS_BASE` is a
//! CPL-0 instruction. The kernel therefore *owns* each thread's value: it is
//! programmed here at entry and reprogrammed by
//! [`crate::userentry::set_user_thread_pointer`] before every switch into the
//! thread, out of the thread's own switch-in hook (`plans/THREADS.md`
//! decision 7). The syscall
//! entry stub never touches `FS`, so nothing else has to save or restore it.
//!
//! # Interrupt and GS state on entry
//!
//! `RFLAGS` is built with `IF` **set**, so ring 3 runs with interrupts
//! enabled and is therefore preemptible: the periodic LAPIC-timer IRQ the
//! production boot arms (`crate::preempt::init_local_preempt`) is taken in
//! user mode and drives the ring-3 preempt point
//! (`plans/PI.md` D2b-2b-A P-1c) — the x86_64 analogue of aarch64's
//! preemptible-EL0 `SPSR` and riscv64's U-mode supervisor-timer rule. The
//! *kernel* stays non-preemptible: it never executes `sti`, so it always
//! runs with `IF == 0`, and a maskable timer IRQ is only ever *taken* once
//! this `iretq` lands in ring 3 (the dispatcher gates the preempt point on
//! the interrupted `CS` regardless). Only the LAPIC timer is unmasked at
//! boot; device IRQs stay masked at the IO-APIC until a driver binds, so
//! enabling `IF` in ring 3 admits no other interrupt source yet.
//!
//! `iretq` does **not** swap `GS`. The production syscall entry stub
//! (`crate::syscall_entry::syscall_entry_stub`) `swapgs`es on entry and
//! again on exit, so during normal ring-0 execution the per-CPU
//! [`SyscallTls`](crate::syscall_entry::SyscallTls) block lives in
//! `IA32_KERNEL_GS_BASE` (programmed by
//! `syscall_entry::init_local_syscalls`, freestanding-only)
//! while the active `GS` base holds the user value. Entering ring 3
//! therefore leaves `IA32_KERNEL_GS_BASE` untouched and already correct:
//! the next `syscall`'s `swapgs` recovers the kernel TLS exactly as it
//! does for a ring-3 program that the kernel never re-entered. This
//! port adds no `swapgs` of its own.

use tairix_arch_api::{EnterUser, UserEntry};

/// x86_64 implementation of the Arch HAL "enter user mode" surface.
///
/// Zero-sized: the `iretq` transition needs no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct UserMode;

impl UserMode {
    /// Construct the x86_64 enter-user handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// The single, `'static` [`UserMode`] the kernel borrows as this port's
/// `&'static dyn EnterUser` handle.
///
/// A process's [`UserMode`] handle is carried alongside its address space so a
/// thread created later is entered through the same transition its first
/// thread was, with no per-arch producer of its own
/// (`plans/THREADS.md` decision 9). The type is zero-sized, so one shared
/// instance serves every CPU.
pub static USER_MODE: UserMode = UserMode::new();

impl EnterUser for UserMode {
    unsafe fn enter_user(&self, regs: UserEntry) -> ! {
        // SAFETY: the caller's `EnterUser::enter_user` contract
        // guarantees `regs.entry` is a ring-3-executable VA and
        // `regs.stack_pointer` a ring-3-writable stack top in the active
        // address space, and that the syscall/exception entry path is
        // installed.
        // SAFETY (thread pointer): `IA32_FS_BASE` is a per-thread scratch
        // base the kernel itself never dereferences, so any value is safe;
        // this runs at CPL 0 on the CPU about to enter the thread.
        unsafe { set_user_thread_pointer(regs.tls_base) };
        unsafe { enter_ring3(regs.entry, regs.stack_pointer, regs.arg0) }
    }
}

/// Ring-3 user code selector — `(USER_CS_INDEX << 3) | RPL 3`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const USER_CS: u64 = ((crate::gdt::USER_CS_INDEX << 3) | 3) as u64;
/// Ring-3 stack/data selector — `(USER_DS_INDEX << 3) | RPL 3`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const USER_SS: u64 = ((crate::gdt::USER_DS_INDEX << 3) | 3) as u64;
/// `RFLAGS` for the `iretq` frame: bit 1 is the architecturally
/// reserved-one bit; `IF` (bit 9) is **set** so ring 3 runs with
/// interrupts enabled and the periodic LAPIC timer can preempt a runaway
/// user task (`plans/PI.md` D2b-2b-A P-1c). The kernel itself never sets
/// `IF` (it issues no `sti`), so it stays non-preemptible; this only makes
/// *user* mode interruptible (parity with aarch64's preemptible-EL0 `SPSR`
/// and riscv64's U-mode supervisor-timer rule).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const USER_RFLAGS: u64 = (1 << 1) | (1 << 9);

/// The `IA32_FS_BASE` MSR (Intel SDM Vol 4 §2.1): the base the `fs:` segment
/// prefix adds, and the x86_64 psABI thread pointer.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const IA32_FS_BASE: u32 = 0xC000_0100;

/// Program the calling CPU's user thread pointer (`IA32_FS_BASE`) to
/// `tls_base`.
///
/// Called at user entry and again from a thread's switch-in hook, because the
/// register is privileged: ring 3 cannot maintain it itself and the kernel
/// never saves it in a trap frame, so the value has to be (re)installed by the
/// side that knows which thread is about to run.
///
/// # Safety
///
/// The caller must run at CPL 0 on the CPU that is about to execute the thread
/// `tls_base` belongs to. The value itself needs no guarantee: the kernel never
/// dereferences `FS`, so a nonsensical base can only fault that thread's own
/// accesses.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn set_user_thread_pointer(tls_base: u64) {
    // `wrmsr` takes the 64-bit value as two 32-bit halves in `edx:eax`
    // (Intel SDM Vol 2B §4.3); the masks split it exactly.
    let lo = (tls_base & 0xFFFF_FFFF) as u32;
    let hi = ((tls_base >> 32) & 0xFFFF_FFFF) as u32;
    // SAFETY: `wrmsr` writes `edx:eax` to the MSR named in `ecx`.
    // `IA32_FS_BASE` is unconditionally present in long mode and accepts any
    // canonical base; the instruction touches no memory and is privileged
    // (CPL 0, which this function's contract requires). A non-canonical value
    // would `#GP` here rather than corrupt anything, and only a value the
    // calling thread chose for itself can reach this.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_FS_BASE,
            in("eax") lo,
            in("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Host substitute: there is no `IA32_FS_BASE` to program off the bare-metal
/// target. Never linked into a kernel image and never reached on the host (the
/// `threads_qemu_x86_64` vertical proves each thread presents its own thread
/// pointer).
///
/// # Safety
///
/// Carries the same contract as the bare-metal definition above, so the two
/// `cfg` arms present one `unsafe` API. The host body is inert.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub unsafe fn set_user_thread_pointer(tls_base: u64) {
    let _ = tls_base;
}

/// Drop to ring 3 at `entry` with stack pointer `sp` and `rdi` set.
///
/// # Safety
///
/// See [`EnterUser::enter_user`]: `entry` must be a valid
/// ring-3-executable virtual address, `sp` a valid ring-3-writable
/// stack top, the GDT must carry the user code/data descriptors at
/// [`crate::gdt::USER_CS_INDEX`] / [`crate::gdt::USER_DS_INDEX`], and
/// the TSS `RSP0` plus the syscall/exception entry path must be
/// installed. Diverges via `iretq`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe fn enter_ring3(entry: u64, sp: u64, arg0: u64) -> ! {
    // SAFETY: the-sanctioned assembly carve-out (no Rust spelling for
    // `iretq` or the interrupt-return frame). The five `push`es build
    // the long-mode `iretq` frame on the kernel stack in the order the
    // CPU pops it (RIP last-pushed/first-popped, then CS, RFLAGS, RSP,
    // SS — SDM Vol 3A §6.14.3): pushing SS, RSP, RFLAGS, CS, RIP places
    // them at the correct offsets. `rdi` carries the first-argument
    // value (System V AMD64). `iretq` performs the documented ring-0 →
    // ring-3 transition. The caller's safety contract guarantees the
    // mapped entry/stack and the installed selectors/TSS.
    // `options(noreturn)` matches the divergence.
    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {sp}",
            "push {rflags}",
            "push {cs}",
            "push {entry}",
            "iretq",
            ss = in(reg) USER_SS,
            sp = in(reg) sp,
            rflags = in(reg) USER_RFLAGS,
            cs = in(reg) USER_CS,
            entry = in(reg) entry,
            in("rdi") arg0,
            options(noreturn),
        );
    }
}

/// Host substitute: the `iretq` transition is meaningful only on the
/// bare-metal x86_64 target, so the host build cannot perform it. It is
/// never linked into a kernel image and never reached on the host (the
/// QEMU verticals exercise the real transition).
///
/// # Safety
///
/// Never call on the host; see [`EnterUser::enter_user`] for the
/// bare-metal contract.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
unsafe fn enter_ring3(_entry: u64, _sp: u64, _arg0: u64) -> ! {
    unreachable!("enter_ring3 is only meaningful on the bare-metal x86_64 target")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_mode_handle_is_object_safe() {
        let port = UserMode::new();
        let _: &dyn EnterUser = &port;
    }
}
