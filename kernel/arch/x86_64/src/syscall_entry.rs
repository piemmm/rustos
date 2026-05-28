//! x86_64 `syscall`/`sysret` entry path (Stage 3a (c6)).
//!
//! This module owns the per-CPU machine state required to take a
//! user-space `syscall` instruction, marshal its register-passed
//! arguments into the architecture-neutral
//! `rustos_kernel_syscall::RawArgs` layout, and return to user space
//! via `sysretq`. The architecture-neutral dispatcher (validation,
//! capability checks, audit) is owned by `kernel/syscall` and is
//! re-used verbatim — this crate never duplicates the
//! `SYSCALL_TABLE_HASH` validation surface (AGENTS.md §2.2, §9).
//!
//! # Layout
//!
//! Three concerns are separated:
//!
//! 1. **Pure MSR-value math** ([`encode_star`], [`efer_with_sce`],
//!    [`fmask_value`], [`pack_raw_args`]). Host-testable, no
//!    architectural side effects.
//! 2. **Per-CPU state** — a fixed-size table of
//!    [`SyscallTls`] blocks (the kernel-stack top and a per-CPU save
//!    slot for the user `%rsp`). `IA32_KERNEL_GS_BASE` on each CPU
//!    points at *that CPU's* slot, so the assembly entry path can
//!    swap stacks with two `gs:`-relative moves.
//! 3. **Bare-metal entry path** — the naked `syscall_entry_stub` and
//!    its Rust trampoline `rustos_arch_x86_64_syscall_dispatch`,
//!    both gated to `target_os = "none"`. The trampoline forwards
//!    the syscall to a binary-installed callback (mirroring the
//!    [`crate::preempt`] timer-callback design); the (c7) binary
//!    glue is the only writer of that callback and wires it to a
//!    real `rustos_kernel_syscall::Dispatcher`.
//!
//! # MSR programming
//!
//! `init_local_syscalls` writes five MSRs (Intel SDM Vol 3A §5.8.8,
//! §2.7):
//!
//! | MSR | Address | Purpose |
//! | --- | --- | --- |
//! | [`IA32_EFER`] | `0xC000_0080` | Sets [`EFER_SCE`] (bit 0) to enable the `syscall`/`sysret` instructions. |
//! | [`IA32_STAR`] | `0xC000_0081` | Encodes the kernel CS/SS pair (entry) and the user CS/SS triplet (sysret). |
//! | [`IA32_LSTAR`] | `0xC000_0082` | Linear address of the syscall entry stub. |
//! | [`IA32_FMASK`] | `0xC000_0084` | Bits to clear in `RFLAGS` on entry (`IF`/`TF`/`DF`/`AC`/`NT`/`RF`/`VM`). |
//! | [`IA32_KERNEL_GS_BASE`] | `0xC000_0102` | Per-CPU [`SyscallTls`] address (swapped in by `swapgs`). |
//!
//! `STAR[47:32]` is loaded into `CS` on entry (and `STAR[47:32] + 8`
//! into `SS`). On 64-bit `sysret`, `STAR[63:48] + 16` is loaded into
//! `CS` and `STAR[63:48] + 8` into `SS` — so the user-CS field passed
//! to [`encode_star`] is the *base*, i.e. the compat-mode user CS
//! selector that sits 16 bytes below the 64-bit user CS in the GDT.
//! Callers that only have a "64-bit user CS" should subtract 16
//! before calling.
//!
//! # `RFLAGS` mask
//!
//! [`fmask_value`] clears `IF`, `TF`, `DF`, `AC`, `NT`, `RF`, and
//! `VM`. The motivations are:
//!
//! * `IF` — entry must be non-preemptible until the kernel decides
//!   otherwise (matches the `cli` semantics every other ISR uses).
//! * `TF` — drop a stray single-step before kernel code runs.
//! * `DF` — System V AMD64 ABI requires `DF=0` at function entry.
//! * `AC` — defence against SMAP bypass / explicit alignment quirks.
//! * `NT`, `RF`, `VM` — task-switching and virtual-8086 holdovers
//!   that have no meaning in long mode and must not affect kernel
//!   state.
//!
//! # Why a callback?
//!
//! `kernel/arch/x86_64` is dep-light by design (one production dep,
//! `rustos-abi`, see `Cargo.toml`). Pulling in `kernel/syscall` here
//! would invert the layering — the dispatcher already depends on
//! `kernel/sec`, `kernel/sched`, `lib/log`, and `lib/crypto`. The
//! arch port instead exposes a single atomic callback slot
//! ([`set_dispatch_callback`]); the binary (Stage 3a (c7)) installs
//! a thin shim that constructs a `RawArgs` from the
//! `[u64; SYSCALL_MAX_ARGS]` the stub builds (the two are
//! `#[repr(transparent)]`-compatible) and forwards into
//! `Dispatcher::dispatch`. Argument validation, capability checks,
//! and audit emission all stay in `kernel/syscall`.

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::sync::atomic::{AtomicU64, Ordering};

use rustos_abi::SYSCALL_MAX_ARGS;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use crate::percpu::MAX_CPUS;

// --- MSR addresses (Intel SDM Vol 3A §2.7, §5.8.8) ------------------

/// Extended Feature Enable Register.
pub const IA32_EFER: u32 = 0xC000_0080;
/// Syscall target selector pair (kernel/user CS encoding).
pub const IA32_STAR: u32 = 0xC000_0081;
/// Long-mode syscall entry RIP.
pub const IA32_LSTAR: u32 = 0xC000_0082;
/// RFLAGS bits cleared on syscall entry.
pub const IA32_FMASK: u32 = 0xC000_0084;
/// Per-CPU GS base swapped in by `swapgs`.
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

// --- Bit constants --------------------------------------------------

/// `IA32_EFER.SCE` (System Call Enable) — bit 0.
///
/// Setting this enables `syscall`/`sysret`. Cleared on cold reset
/// (Intel SDM Vol 3A §2.2.1 Table 2-2).
pub const EFER_SCE: u64 = 1 << 0;

/// `RFLAGS` mask written to `IA32_FMASK`.
///
/// Bits cleared on syscall entry: `TF` (bit 8, `0x100`), `IF`
/// (bit 9, `0x200`), `DF` (bit 10, `0x400`), `NT` (bit 14,
/// `0x4000`), `RF` (bit 16, `0x1_0000`), `VM` (bit 17,
/// `0x2_0000`), `AC` (bit 18, `0x4_0000`). See module-level docs
/// for the rationale. The numeric value `0x7_4700` is the bitwise
/// OR of those seven flags.
pub const RFLAGS_MASK: u64 = 0x7_4700;

// --- MSR-value math (pure, host-testable) ---------------------------

/// Encode the `IA32_STAR` MSR value.
///
/// * `kernel_cs` populates bits 47..32 — the CPU loads `CS = kernel_cs`
///   and `SS = kernel_cs + 8` on syscall entry.
/// * `sysret_user_base` populates bits 63..48 — the CPU loads
///   `CS = sysret_user_base + 16` (long mode) and
///   `SS = sysret_user_base + 8` on `sysretq`. Callers that have a
///   "64-bit user CS" selector must pass `user_cs - 16`.
///
/// Bits 31..0 are reserved and written as zero; they hold the
/// 32-bit-mode `sysenter` CS, which the long-mode syscall path does
/// not use.
#[must_use]
pub const fn encode_star(kernel_cs: u16, sysret_user_base: u16) -> u64 {
    ((sysret_user_base as u64) << 48) | ((kernel_cs as u64) << 32)
}

/// Return `prev | EFER_SCE` — i.e. the value to write to `IA32_EFER`
/// after reading its current value, in order to enable `syscall`.
///
/// Preserves every other bit (`LME`, `LMA`, `NXE`, …) so callers
/// must not clobber other features the firmware/UEFI loader set up.
#[must_use]
pub const fn efer_with_sce(prev: u64) -> u64 {
    prev | EFER_SCE
}

/// Return the `IA32_FMASK` value as a `u64`. Constant by design but
/// exposed as a function so host tests can cross-check the bit set
/// against the documentation above.
#[must_use]
pub const fn fmask_value() -> u64 {
    RFLAGS_MASK
}

/// Pack the six System V AMD64 syscall-argument registers into the
/// canonical layout expected by [`rustos_abi`]'s syscall ABI.
///
/// The x86_64 `syscall` instruction passes the user-space arguments
/// in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` (System V — `r10` in
/// place of `rcx`, because `rcx` is clobbered by the instruction
/// itself to hold the saved RIP). The order returned here matches
/// the ABI definition pinned in `lib/abi/src/syscalls.rs`.
#[must_use]
pub const fn pack_raw_args(
    rdi: u64,
    rsi: u64,
    rdx: u64,
    r10: u64,
    r8: u64,
    r9: u64,
) -> [u64; SYSCALL_MAX_ARGS] {
    [rdi, rsi, rdx, r10, r8, r9]
}

// --- Per-CPU TLS layout ---------------------------------------------

/// Per-CPU thread-local-storage block addressed via `IA32_KERNEL_GS_BASE`.
///
/// `swapgs` on syscall entry exposes this block as `gs:`. The stub
/// loads the kernel stack pointer from `gs:0` and saves the user
/// `%rsp` into `gs:8`; on `sysretq` the user `%rsp` is restored from
/// the same slot before the matching `swapgs`.
///
/// `#[repr(C, align(16))]` ensures the offsets are stable across
/// targets and that the field addresses inherit 16-byte alignment
/// for the System V stack the trampoline calls Rust on.
#[repr(C, align(16))]
#[derive(Debug)]
pub struct SyscallTls {
    /// Top of the kernel stack to switch to on entry.
    pub kernel_rsp0: u64,
    /// Save slot for the user `%rsp` (between entry and `sysretq`).
    pub user_rsp_save: u64,
}

impl SyscallTls {
    /// Zero-initialised TLS slot. Used as the initial value of the
    /// static `PER_CPU_TLS` arena.
    pub const ZERO: Self = Self {
        kernel_rsp0: 0,
        user_rsp_save: 0,
    };
}

/// Offset of [`SyscallTls::kernel_rsp0`] — the entry stub uses
/// `gs:0` to load the kernel stack pointer.
pub const KERNEL_RSP0_OFFSET: usize = 0;
/// Offset of [`SyscallTls::user_rsp_save`] — the entry stub uses
/// `gs:8` to save/restore the user `%rsp`.
pub const USER_RSP_SAVE_OFFSET: usize = 8;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static mut PER_CPU_TLS: [SyscallTls; MAX_CPUS] = {
    const Z: SyscallTls = SyscallTls::ZERO;
    [Z; MAX_CPUS]
};

/// Return the linear address of the per-CPU [`SyscallTls`] slot for
/// `cpu_index`, populating its `kernel_rsp0` field with `kernel_rsp0`.
///
/// The returned address is the value the caller writes to
/// `IA32_KERNEL_GS_BASE` on that CPU.
///
/// # Errors
///
/// Returns [`crate::percpu::InitError::CpuIndexOutOfRange`] if
/// `cpu_index >= MAX_CPUS`.
///
/// # Safety
///
/// * `cpu_index` must be unique to *this* CPU and equal to the index
///   passed to [`crate::percpu::init`].
/// * `kernel_rsp0` must point one byte past the top of a kernel
///   stack reserved for syscall entries on this CPU (16-byte
///   aligned, at least one full page).
/// * The function must run before this CPU's first user-space
///   transition (otherwise a syscall would see a zero `kernel_rsp0`
///   and triple-fault).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use = "writing the returned address to IA32_KERNEL_GS_BASE is the contract"]
pub unsafe fn install_kernel_rsp0(
    cpu_index: usize,
    kernel_rsp0: u64,
) -> Result<u64, crate::percpu::InitError> {
    if cpu_index >= MAX_CPUS {
        return Err(crate::percpu::InitError::CpuIndexOutOfRange);
    }
    // SAFETY: caller's contract pins `cpu_index` to this CPU; no
    // other CPU writes to the same slot.
    unsafe {
        let base = core::ptr::addr_of_mut!(PER_CPU_TLS) as *mut SyscallTls;
        let slot = base.add(cpu_index);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*slot).kernel_rsp0), kernel_rsp0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*slot).user_rsp_save), 0);
        Ok(slot as u64)
    }
}

// --- Dispatcher callback storage ------------------------------------

/// Signature of the Rust callback the syscall trampoline forwards
/// each `syscall` instruction to.
///
/// `number` is the value the user placed in `%rax`; `args_ptr` is
/// a pointer to a `[u64; SYSCALL_MAX_ARGS]` on the kernel stack
/// (laid out by [`pack_raw_args`]). The callback must read up to
/// `args_ptr` synchronously — the array lives only for the duration
/// of the call. The return value is placed in `%rax` and returned to
/// user space by `sysretq`.
pub type SyscallDispatchFn =
    extern "C" fn(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64;

/// Atomically-stored function pointer for the dispatch callback.
///
/// `0` is the "no callback installed" sentinel. The trampoline
/// fail-closes via [`crate::qemu_exit::exit_failure`] in that case
/// (see [`rustos_arch_x86_64_syscall_dispatch`]'s rustdoc); a
/// silent return would be an "open by default" failure per
/// AGENTS.md §7. Storage is gated to the freestanding target — the
/// host build never reads or writes it (matches the
/// [`crate::preempt::set_timer_callback`] pattern).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static SYSCALL_DISPATCH_CALLBACK: AtomicU64 = AtomicU64::new(0);

/// Install the per-binary dispatch callback. Called once during
/// kernel boot, before `init_local_syscalls` enables `syscall`
/// on any CPU.
///
/// Storing a `fn` pointer (not a closure) keeps the callback safe
/// to invoke from a freshly-switched kernel stack — there is no
/// captured environment that could be `Drop`-ped from under us.
///
/// On the host target this function is an explicit no-op: the
/// callback storage is gated to `target_os = "none"`. Host unit
/// tests that need to verify the install path do so against the
/// pure-Rust [`dispatch_callback`] getter, which mirrors the same
/// gating.
pub fn set_dispatch_callback(cb: SyscallDispatchFn) {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    SYSCALL_DISPATCH_CALLBACK.store(cb as usize as u64, Ordering::Release);
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    let _ = cb;
}

/// Read back the currently-installed dispatch callback. Test-only;
/// used by the (c7) binary commit to verify its install.
///
/// Always returns `None` on the host target — the callback storage
/// is gated to `target_os = "none"`; the round-trip is exercised by
/// the QEMU integration test in the (c7) binary commit.
#[must_use]
pub fn dispatch_callback() -> Option<SyscallDispatchFn> {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        let raw = SYSCALL_DISPATCH_CALLBACK.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            // SAFETY: every store into `SYSCALL_DISPATCH_CALLBACK`
            // originates from `set_dispatch_callback`, which round-
            // trips a valid `SyscallDispatchFn` pointer.
            Some(unsafe { core::mem::transmute::<usize, SyscallDispatchFn>(raw as usize) })
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        None
    }
}

// --- Rust trampoline (called from the naked stub) -------------------

/// Rust trampoline called by the naked assembly entry stub.
///
/// The dispatch callback **must** be installed via
/// [`set_dispatch_callback`] before [`init_local_syscalls`] enables
/// `syscall` on any CPU — this is part of the safety contract of
/// `init_local_syscalls`. If the trampoline is nevertheless reached
/// without a callback, the kernel fail-closes through
/// [`crate::qemu_exit::exit_failure`] (the same posture
/// [`crate::interrupts`] takes for its default ISR — AGENTS.md §10).
///
/// # Safety
///
/// * Must only be invoked from `syscall_entry_stub`. Calling it from
///   arbitrary Rust would read a stack-local `[u64; SYSCALL_MAX_ARGS]`
///   that does not exist.
/// * `args_ptr` must point at a `[u64; SYSCALL_MAX_ARGS]` that lives
///   for the duration of the call.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn rustos_arch_x86_64_syscall_dispatch(
    number: u64,
    args_ptr: *const [u64; SYSCALL_MAX_ARGS],
) -> u64 {
    let raw = SYSCALL_DISPATCH_CALLBACK.load(Ordering::Acquire);
    if raw == 0 {
        // Fail-closed: a syscall reached the trampoline before the
        // binary installed its dispatcher. Continuing would mean
        // returning an unspecified value to user space, which is
        // exactly the "open by default" failure AGENTS.md §7 / §10
        // forbid.
        crate::qemu_exit::exit_failure();
    }
    // SAFETY: see `dispatch_callback` — every store round-trips a
    // valid `SyscallDispatchFn`.
    let cb: SyscallDispatchFn =
        unsafe { core::mem::transmute::<usize, SyscallDispatchFn>(raw as usize) };
    cb(number, args_ptr)
}

// --- Naked entry stub ----------------------------------------------

/// Linear address of [`syscall_entry_stub`]. The value to write to
/// `IA32_LSTAR` on every CPU. Only meaningful on the freestanding
/// target.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub fn syscall_entry_addr() -> u64 {
    syscall_entry_stub as *const () as usize as u64
}

/// The single `IA32_LSTAR` target for every CPU in the system.
///
/// Sequence:
///
/// 1. `swapgs` — `%gs` now points at this CPU's [`SyscallTls`].
/// 2. Save the user `%rsp` into `gs:USER_RSP_SAVE_OFFSET` and load
///    the kernel `%rsp` from `gs:KERNEL_RSP0_OFFSET`.
/// 3. Push the user `RFLAGS` (`%r11`) and saved RIP (`%rcx`) so they
///    survive the Rust call (System V allows callees to clobber both).
///    Add an alignment padding slot so the System V "rsp ≡ 0 (mod 16)
///    at `call`" rule holds.
/// 4. Build the [`SYSCALL_MAX_ARGS`]-wide argument array on the
///    kernel stack from `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`.
/// 5. Set up the System V args: `%rdi = syscall number (saved rax)`,
///    `%rsi = &args[0]`. Call [`rustos_arch_x86_64_syscall_dispatch`].
/// 6. The return value is in `%rax` already — leave it.
/// 7. Pop the arg array + padding + `%r11` + `%rcx` in reverse order.
/// 8. Restore user `%rsp` from `gs:USER_RSP_SAVE_OFFSET`, `swapgs`,
///    `sysretq`.
///
/// # Safety
///
/// Only the CPU's `syscall` instruction may transfer control here.
/// Direct calls from Rust are undefined behaviour — there is no
/// return address on the stack and `%rcx`/`%r11` are interpreted as
/// the user-space saved RIP/RFLAGS, not callee-saved scratch.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn syscall_entry_stub() {
    core::arch::naked_asm!(
        // 1. Switch to kernel GS.
        "swapgs",
        // 2. Save user rsp, load kernel rsp.
        "movq %rsp, %gs:8",
        "movq %gs:0, %rsp",
        // 3. Preserve user RIP (rcx) and RFLAGS (r11) + alignment pad.
        "pushq $0",
        "pushq %rcx",
        "pushq %r11",
        // 4. Build args[5..0] on the stack.
        "pushq %r9",
        "pushq %r8",
        "pushq %r10",
        "pushq %rdx",
        "pushq %rsi",
        "pushq %rdi",
        // 5. Call Rust trampoline: rdi=number (was rax), rsi=&args[0].
        "movq %rsp, %rsi",
        "movq %rax, %rdi",
        "call {dispatch}",
        // 7. Tear down args + saved registers + pad.
        "addq $48, %rsp",   // 6 * 8 = pop args[0..6]
        "popq %r11",
        "popq %rcx",
        "addq $8, %rsp",    // padding
        // 8. Restore user rsp and return to user space.
        "movq %gs:8, %rsp",
        "swapgs",
        "sysretq",
        dispatch = sym rustos_arch_x86_64_syscall_dispatch,
        options(att_syntax),
    )
}

// --- Init ----------------------------------------------------------

/// Initialise `syscall`/`sysret` on the calling CPU.
///
/// Programs `IA32_EFER.SCE`, `IA32_STAR`, `IA32_LSTAR`, `IA32_FMASK`
/// and `IA32_KERNEL_GS_BASE` from the [`encode_star`] /
/// [`fmask_value`] / [`syscall_entry_addr`] / [`install_kernel_rsp0`]
/// outputs.
///
/// # Errors
///
/// Returns [`crate::percpu::InitError::CpuIndexOutOfRange`] if
/// `cpu_index >= MAX_CPUS`.
///
/// # Safety
///
/// * `cpu_index` must equal this CPU's [`crate::percpu::init`] index.
/// * `kernel_cs` and `sysret_user_base` must be valid GDT selectors
///   (see [`encode_star`]).
/// * `kernel_rsp0` must satisfy [`install_kernel_rsp0`]'s stack-top
///   contract.
/// * Must run with interrupts disabled and *before* this CPU
///   transitions to user space.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn init_local_syscalls(
    cpu_index: usize,
    kernel_cs: u16,
    sysret_user_base: u16,
    kernel_rsp0: u64,
) -> Result<(), crate::percpu::InitError> {
    // SAFETY: forwarded — caller's contract guarantees uniqueness of
    // `cpu_index` to this CPU and the stack-top validity of
    // `kernel_rsp0`.
    let tls_addr = unsafe { install_kernel_rsp0(cpu_index, kernel_rsp0)? };

    // SAFETY: each `wrmsr` writes a fixed, host-tested constant or a
    // caller-checked value. The MSRs touched (EFER/STAR/LSTAR/FMASK
    // /KERNEL_GS_BASE) are documented in Intel SDM Vol 3A §2.7,
    // §5.8.8 and have no cross-CPU side effects. Reading and ORing
    // EFER preserves bits (LME/LMA/NXE/…) the firmware set up.
    unsafe {
        let efer = rdmsr(IA32_EFER);
        wrmsr(IA32_EFER, efer_with_sce(efer));
        wrmsr(IA32_STAR, encode_star(kernel_cs, sysret_user_base));
        wrmsr(IA32_LSTAR, syscall_entry_addr());
        wrmsr(IA32_FMASK, fmask_value());
        wrmsr(IA32_KERNEL_GS_BASE, tls_addr);
    }

    Ok(())
}

// --- MSR primitives ------------------------------------------------

/// Read MSR `msr` into a 64-bit value.
///
/// # Safety
///
/// * The current privilege level must be 0 (CPL=0). `rdmsr` `#GP`s
///   otherwise.
/// * The MSR address must be implemented on this CPU (the four
///   syscall MSRs and `KERNEL_GS_BASE` are mandatory on every
///   long-mode CPU since AMD64 1.0 / Intel 64).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: see function-level contract.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Write `value` to MSR `msr`.
///
/// # Safety
///
/// As for [`rdmsr`], plus: `value` must be valid for the addressed
/// MSR (see Intel SDM Vol 4 for the per-MSR encoding). The five MSRs
/// this module writes are constructed by host-tested encoders.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    let lo = value as u32;
    let hi = (value >> 32) as u32;
    // SAFETY: see function-level contract.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") lo,
            in("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
}

// --- Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn msr_addresses_match_intel_sdm() {
        // Intel SDM Vol 4 Table 2-2 ("IA-32 Architectural MSRs").
        assert_eq!(IA32_EFER, 0xC000_0080);
        assert_eq!(IA32_STAR, 0xC000_0081);
        assert_eq!(IA32_LSTAR, 0xC000_0082);
        assert_eq!(IA32_FMASK, 0xC000_0084);
        assert_eq!(IA32_KERNEL_GS_BASE, 0xC000_0102);
    }

    #[test]
    fn encode_star_packs_kernel_and_user_selectors() {
        // Kernel CS = 0x08, user base = 0x18 (so user 64-bit CS is at
        // selector 0x28 per the sysret +16 quirk). The encoded value
        // must place 0x18 in bits 63..48 and 0x08 in bits 47..32.
        let v = encode_star(0x0008, 0x0018);
        assert_eq!((v >> 48) & 0xFFFF, 0x0018);
        assert_eq!((v >> 32) & 0xFFFF, 0x0008);
        // Bits 31..0 are reserved-zero.
        assert_eq!(v & 0xFFFF_FFFF, 0);
    }

    #[test]
    fn encode_star_preserves_high_bits() {
        // The full 16-bit selector encoding (incl. RPL, TI) must
        // round-trip — no field truncation to 13 bits.
        let v = encode_star(0xABCD, 0x1234);
        assert_eq!((v >> 48) & 0xFFFF, 0x1234);
        assert_eq!((v >> 32) & 0xFFFF, 0xABCD);
    }

    #[test]
    fn efer_with_sce_sets_bit_zero_and_preserves_rest() {
        // LME=bit 8, LMA=bit 10, NXE=bit 11 — a typical firmware
        // EFER value after UEFI hand-off.
        let prev = (1 << 8) | (1 << 10) | (1 << 11);
        let next = efer_with_sce(prev);
        assert_eq!(next & 1, 1, "SCE bit must be set");
        assert_eq!(next & !1, prev, "other bits must be unchanged");
    }

    #[test]
    fn fmask_clears_documented_rflags_bits() {
        let m = fmask_value();
        // IF, TF, DF, AC, NT, RF, VM.
        assert_eq!(m & 0x0000_0200, 0x0000_0200, "IF");
        assert_eq!(m & 0x0000_0100, 0x0000_0100, "TF");
        assert_eq!(m & 0x0000_0400, 0x0000_0400, "DF");
        assert_eq!(m & 0x0004_0000, 0x0004_0000, "AC");
        assert_eq!(m & 0x0000_4000, 0x0000_4000, "NT");
        assert_eq!(m & 0x0001_0000, 0x0001_0000, "RF");
        assert_eq!(m & 0x0002_0000, 0x0002_0000, "VM");
        // No other bits should be set — anything else would be an
        // undocumented effect the kernel must justify.
        let documented = 0x0000_0200
            | 0x0000_0100
            | 0x0000_0400
            | 0x0004_0000
            | 0x0000_4000
            | 0x0001_0000
            | 0x0002_0000;
        assert_eq!(m, documented);
    }

    #[test]
    fn pack_raw_args_orders_system_v_registers() {
        let a = pack_raw_args(0x11, 0x22, 0x33, 0x44, 0x55, 0x66);
        assert_eq!(a, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    }

    #[test]
    fn pack_raw_args_width_matches_abi_constant() {
        // The packed array width is the ABI-frozen constant; a
        // mismatch would be a layering bug (we would silently lose
        // arguments past the sixth).
        assert_eq!(SYSCALL_MAX_ARGS, 6);
        let a = pack_raw_args(0, 0, 0, 0, 0, 0);
        assert_eq!(a.len(), SYSCALL_MAX_ARGS);
    }

    #[test]
    fn syscall_tls_layout_is_pinned() {
        // The naked entry stub uses literal `gs:0` and `gs:8`. The
        // offsets must match this struct's layout exactly or the
        // stub would corrupt user state.
        assert_eq!(offset_of!(SyscallTls, kernel_rsp0), KERNEL_RSP0_OFFSET);
        assert_eq!(offset_of!(SyscallTls, user_rsp_save), USER_RSP_SAVE_OFFSET);
        assert_eq!(size_of::<SyscallTls>(), 16);
        // 16-byte alignment ensures the kernel stack pointer the
        // stub loads inherits the System V ABI requirement.
        assert_eq!(align_of::<SyscallTls>(), 16);
    }

    /// Module-level callback used by `dispatch_callback_on_host_is_none`.
    /// Hoisted out of the test body so `extern "C" fn` items don't
    /// trigger `clippy::items_after_statements`.
    extern "C" fn host_dispatch_noop(_n: u64, _a: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
        0
    }

    #[test]
    fn dispatch_callback_on_host_is_none() {
        // The freestanding-target storage is `cfg`-gated out on the
        // host. `set_dispatch_callback` is callable on the host (it's
        // a no-op stub mirroring `preempt::set_timer_callback`); the
        // getter must consistently report "none" so a future regression
        // that quietly enables host-side storage is caught.
        set_dispatch_callback(host_dispatch_noop);
        assert!(dispatch_callback().is_none());
    }
}
