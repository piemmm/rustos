//! x86_64 `syscall`/`sysret` entry path (Stage 3a (c6)).
//!
//! This module owns the per-CPU machine state required to take a
//! user-space `syscall` instruction, marshal its register-passed
//! arguments into the architecture-neutral
//! `tairix_kernel_syscall::RawArgs` layout, and return to user space
//! via `sysretq`. The architecture-neutral dispatcher (validation,
//! capability checks, audit) is owned by `kernel/syscall` and is
//! re-used verbatim — this crate never duplicates the
//! `SYSCALL_TABLE_HASH` validation surface.
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
//!    its Rust trampoline `tairix_arch_x86_64_syscall_dispatch`,
//!    both gated to `target_os = "none"`. The trampoline forwards
//!    the syscall to a binary-installed callback (mirroring the
//!    [`crate::preempt`] timer-callback design); the (c7) binary
//!    glue is the only writer of that callback and wires it to a
//!    real `tairix_kernel_syscall::Dispatcher`.
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
//! | [`IA32_FMASK`] | `0xC000_0084` | Bits to clear in `RFLAGS` on entry (`IF`/`TF`/`DF`/`AC`/`NT`/`RF`/`VM`/`IOPL`). |
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
//! [`fmask_value`] clears `IF`, `TF`, `DF`, `AC`, `NT`, `RF`,
//! `VM`, and `IOPL`. The motivations are:
//!
//! * `IF` — entry must be interrupt-free until the kernel has swapped to
//!   its GS base and pivoted onto the kernel stack. The entry stub then
//!   issues a single `sti` in that well-defined kernel context so the
//!   *syscall body* runs with device interrupts deliverable (a long,
//!   non-blocking syscall must not monopolise the CPU with interrupts
//!   masked), and `cli`s again before restoring the user frame. The kernel
//!   stays non-preemptible: an interrupt taken at CPL=0 during the body
//!   only latches a reschedule (honoured at return-to-user), it never
//!   switches away mid-syscall.
//! * `TF` — drop a stray single-step before kernel code runs.
//! * `DF` — System V AMD64 ABI requires `DF=0` at function entry.
//! * `AC` — defence against SMAP bypass / explicit alignment quirks.
//! * `IOPL` — a user task must never carry I/O privilege into ring 0;
//!   clearing `IOPL` on entry means kernel code always runs at
//!   `IOPL=0` regardless of the caller's `RFLAGS` (matches Linux's
//!   `MSR_SYSCALL_MASK`; `tests/SECURITY.md` §5, CWE-696).
//! * `NT`, `RF`, `VM` — task-switching and virtual-8086 holdovers
//!   that have no meaning in long mode and must not affect kernel
//!   state.
//!
//! # Why a callback?
//!
//! `kernel/arch/x86_64` is dep-light by design (one production dep,
//! `tairix-abi`, see `Cargo.toml`). Pulling in `kernel/syscall` here
//! would invert the layering — the dispatcher already depends on
//! `kernel/sec`, `kernel/sched`, `lib/log`, and `lib/crypto`. The
//! arch port instead exposes a single atomic callback slot
//! ([`set_dispatch_callback`]); the binary (Stage 3a (c7)) installs
//! a thin shim that constructs a `RawArgs` from the
//! `[u64; SYSCALL_MAX_ARGS]` the stub builds (the two are
//! `#[repr(transparent)]`-compatible) and forwards into
//! `Dispatcher::dispatch`. Argument validation, capability checks,
//! and audit emission all stay in `kernel/syscall`.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use tairix_abi::SYSCALL_MAX_ARGS;

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
/// (bit 9, `0x200`), `DF` (bit 10, `0x400`), `IOPL` (bits 12..13,
/// `0x3000`), `NT` (bit 14, `0x4000`), `RF` (bit 16, `0x1_0000`),
/// `VM` (bit 17, `0x2_0000`), `AC` (bit 18, `0x4_0000`). See
/// module-level docs for the rationale. The numeric value
/// `0x7_7700` is the bitwise OR of those flags.
pub const RFLAGS_MASK: u64 = 0x7_7700;

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
/// canonical layout expected by [`tairix_abi`]'s syscall ABI.
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
/// loads the kernel stack pointer from `gs:0` and stashes the user
/// `%rsp` into `gs:8` only **transiently** — it is read straight back
/// out and pushed onto *this task's* kernel-stack frame (beside the
/// frame-resident saved RIP `%rcx`/RFLAGS `%r11`) before any
/// cooperative switch can occur. The durable user-`%rsp` save is the
/// per-task frame slot, not `gs:8`: a task parked mid-handler by a
/// cooperative `yield`/`wait` would otherwise have a *shared* per-CPU
/// `gs:8` overwritten by another task's syscall entry before it
/// resumed (`plans/PI.md` X2). `gs:8` is therefore live only in the
/// window between the entry `swapgs` and the first kernel-stack push.
///
/// `#[repr(C, align(16))]` ensures the offsets are stable across
/// targets and that the field addresses inherit 16-byte alignment
/// for the System V stack the trampoline calls Rust on.
#[repr(C, align(16))]
#[derive(Debug)]
pub struct SyscallTls {
    /// Top of the kernel stack to switch to on entry.
    pub kernel_rsp0: u64,
    /// Transient stash for the user `%rsp`, live only between the entry
    /// `swapgs` and the first kernel-stack push (the durable save is the
    /// per-task kernel-stack frame — see the type docs).
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
/// `gs:8` to transiently stash the user `%rsp` before pushing it onto
/// the per-task kernel-stack frame.
pub const USER_RSP_SAVE_OFFSET: usize = 8;

/// First non-canonical address above the lower (user) half of the
/// x86_64 48-bit virtual address space.
///
/// Gated to the bare-metal target or host tests: only
/// [`validate_kernel_rsp0`] consumes it, and that in turn is only wired
/// into the freestanding [`install_kernel_rsp0`] (mirrors the
/// `crate::percpu` stack-top helpers' gating).
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
const CANONICAL_LOWER_LIMIT: u64 = 0x0000_8000_0000_0000;
/// First address of the canonical higher half (kernel space); also the
/// lowest valid kernel `RSP0`.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
const CANONICAL_HIGHER_BASE: u64 = 0xFFFF_8000_0000_0000;

/// `true` if `addr` is a canonical 48-bit x86_64 virtual address — bits
/// `63:47` are all equal (Intel SDM Vol 1 §3.3.7.1). The CPU `#GP`s (or,
/// for some MSR loads, faults at the consuming instruction) on a
/// non-canonical address.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
const fn is_canonical(addr: u64) -> bool {
    addr < CANONICAL_LOWER_LIMIT || addr >= CANONICAL_HIGHER_BASE
}

/// Validate a kernel `RSP0` before it is installed for `syscall` entry.
///
/// On a `syscall` from ring 3 the entry stub pivots onto this stack
/// (loaded from the per-CPU TLS via `swapgs`). A hostile or buggy value
/// is a stack-pivot / privilege-boundary vector (
/// CVE-2019-1125 class): a **non-canonical** top faults inside kernel
/// entry, and a **user-range** top would run kernel entry on
/// attacker-controlled memory. The stack top must therefore be non-null,
/// 16-byte aligned (System V AMD64), canonical, and in the kernel
/// (higher) half. Anything else is rejected fail-closed.
///
/// # Errors
///
/// [`crate::percpu::InitError::InvalidKernelStackPointer`] if `rsp0` is
/// null, misaligned, non-canonical, or in the user half.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
pub(crate) const fn validate_kernel_rsp0(rsp0: u64) -> Result<(), crate::percpu::InitError> {
    if rsp0 == 0 || !rsp0.is_multiple_of(16) {
        return Err(crate::percpu::InitError::InvalidKernelStackPointer);
    }
    // The kernel half is by definition canonical, but check canonicity
    // explicitly so the intent is legible and a future split of the two
    // constants cannot silently admit a non-canonical address.
    if !is_canonical(rsp0) || rsp0 < CANONICAL_HIGHER_BASE {
        return Err(crate::percpu::InitError::InvalidKernelStackPointer);
    }
    Ok(())
}

/// Why a user-supplied `(ptr, len)` buffer failed the copy-from-user
/// boundary check ([`validate_user_buffer`]).
///
/// Each variant names one fail-closed reason; the
/// caller maps it to its public `Errno` and never proceeds with the
/// access.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserBufferError {
    /// The base pointer is null.
    Null,
    /// The base pointer is non-canonical — it lands in the 48-bit
    /// address hole, so the CPU would `#GP` dereferencing it.
    NonCanonical,
    /// The base pointer, or any byte of the buffer, lies in the kernel
    /// (canonical higher) half. A user buffer must live entirely in the
    /// lower half (ret2usr / `copy_from_user` class).
    KernelRange,
    /// `ptr + len` overflows the 64-bit address space (CWE-190): the
    /// length wraps the buffer back over the start.
    Overflows,
}

/// Validate a user-supplied `(ptr, len)` buffer before the kernel copies
/// to or from it (the `copy_from_user` / `copy_to_user` boundary / `tests/SECURITY.md` §5, CWE-367 / CWE-822).
///
/// A user task hands the kernel raw 64-bit register values. Before the
/// kernel may touch the buffer it must prove the whole `[ptr, ptr + len)`
/// range is a legitimate *user* address window. This rejects, fail-closed:
///
/// * a **null** base ([`UserBufferError::Null`]);
/// * a **non-canonical** base in the 48-bit hole, which would `#GP` on
///   access ([`UserBufferError::NonCanonical`]);
/// * a base — or a buffer end — in the **kernel half**, the classic
///   ret2usr / kernel-pointer-confusion vector
///   ([`UserBufferError::KernelRange`]);
/// * a length that makes `ptr + len` **wrap** the address space
///   ([`UserBufferError::Overflows`]).
///
/// `len == 0` is accepted for any in-range, canonical, non-null base: an
/// empty copy touches no memory. The exclusive end `ptr + len` may equal
/// the first non-user address (one past the last user byte) but not
/// exceed it.
///
/// This is the host-testable validator the Stage-6 `copy_from_user` fault
/// path gates on (per-access page-fault fix-up is Stage-6); landing it
/// now pins the boundary semantics as a real conformance target
/// (`tests/SECURITY.md` §5 — "land host validators now").
///
/// # Errors
///
/// A [`UserBufferError`] naming the first invariant the buffer breaks.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
pub const fn validate_user_buffer(ptr: u64, len: u64) -> Result<(), UserBufferError> {
    if ptr == 0 {
        return Err(UserBufferError::Null);
    }
    if ptr >= CANONICAL_HIGHER_BASE {
        return Err(UserBufferError::KernelRange);
    }
    // Above the user half but below the kernel half is the non-canonical
    // hole; `is_canonical` already covers it, but naming the case keeps
    // the diagnostic precise.
    if !is_canonical(ptr) || ptr >= CANONICAL_LOWER_LIMIT {
        return Err(UserBufferError::NonCanonical);
    }
    let Some(end) = ptr.checked_add(len) else {
        return Err(UserBufferError::Overflows);
    };
    // The exclusive end may sit exactly on the first non-user address
    // (one past the last byte) but a buffer that reaches any further has
    // crossed out of the user half.
    if end > CANONICAL_LOWER_LIMIT {
        return Err(UserBufferError::KernelRange);
    }
    Ok(())
}

// --- Caller-provided per-CPU syscall TLS storage --------------------

/// Published base of the registered [`SyscallTlsStorage::tls`] array
/// (`null` until a storage is registered, so the per-CPU TLS entry
/// points fail closed before registration).
static SYSCALL_TLS_BASE: AtomicPtr<SyscallTls> = AtomicPtr::new(core::ptr::null_mut());

/// Number of logical-CPU TLS slots the registered storage covers (`0`
/// until a storage is registered — every index is out of range, so an
/// unregistered system fails closed).
static SYSCALL_TLS_LEN: AtomicUsize = AtomicUsize::new(0);

/// Set-once guard so a second [`SyscallTlsStorage::register`] is refused
/// rather than silently re-pointing the live TLS slice.
static SYSCALL_TLS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Failure mode of [`SyscallTlsStorage::register`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SyscallTlsStorageError {
    /// Storage was already registered; the slot is set-once per boot
    /// (no silent re-pointing of the live arena).
    AlreadyRegistered,
}

/// Caller-owned, `&'static` per-CPU [`SyscallTls`] arena, sized by the
/// constructing caller for its machine (the per-CPU
/// syscall-TLS arena is derived from the-discovered logical-CPU
/// count, never a fixed `const` ceiling baked into the arch crate).
///
/// The const parameter `N` is the number of logical CPUs the caller
/// sizes for, matching the [`crate::percpu::PerCpuStorage`] it registers
/// alongside. The arch crate stays allocator-free, so the caller places
/// the storage in a `static` (allocator-free bins) or a leaked
/// allocation and publishes it through [`SyscallTlsStorage::register`]
/// before the first `install_kernel_rsp0`.
#[repr(C, align(16))]
pub struct SyscallTlsStorage<const N: usize> {
    /// Per-CPU syscall TLS blocks, one slot per logical CPU. The
    /// `UnsafeCell` is load-bearing: `install_kernel_rsp0` /
    /// `set_kernel_rsp0` and the `swapgs`-relative entry stub mutate a
    /// slot through the published base while the storage is only
    /// borrowed `&'static` (shared), so the interior mutability is what
    /// makes those writes sound *and* keeps the `static` in writable
    /// memory rather than read-only `.rodata`.
    tls: UnsafeCell<[SyscallTls; N]>,
}

// SAFETY: the `UnsafeCell<[SyscallTls; N]>` is mutated only through the
// published base, and each CPU owns its own slot (the `cpu_index` it was
// brought up with); no slot is shared mutably across threads/CPUs, so the
// storage is `Sync`.
unsafe impl<const N: usize> Sync for SyscallTlsStorage<N> {}

impl<const N: usize> SyscallTlsStorage<N> {
    /// A zeroed arena of `N` syscall-TLS blocks. `const` so the
    /// allocator-free bins can place it in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tls: UnsafeCell::new([const { SyscallTls::ZERO }; N]),
        }
    }

    /// Publish this arena to the per-CPU syscall-TLS entry points, then
    /// return the covered CPU count `N`. Must be called on the boot CPU,
    /// exactly once, before any `install_kernel_rsp0`.
    ///
    /// # Errors
    ///
    /// [`SyscallTlsStorageError::AlreadyRegistered`] on the second
    /// publish (set-once per boot — never silently re-points the live
    /// arena).
    pub fn register(&'static self) -> Result<usize, SyscallTlsStorageError> {
        if SYSCALL_TLS_REGISTERED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SyscallTlsStorageError::AlreadyRegistered);
        }
        SYSCALL_TLS_BASE.store(self.tls.get().cast::<SyscallTls>(), Ordering::Release);
        SYSCALL_TLS_LEN.store(N, Ordering::Release);
        Ok(N)
    }
}

impl<const N: usize> Default for SyscallTlsStorage<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of logical-CPU TLS slots the registered [`SyscallTlsStorage`]
/// covers (`0` until a storage is registered). Diagnostic observer.
#[must_use]
pub fn registered_syscall_cpu_count() -> usize {
    SYSCALL_TLS_LEN.load(Ordering::Acquire)
}

/// Raw pointer to the registered per-CPU [`SyscallTls`] slot for
/// `cpu_index`, or `None` if `cpu_index` is out of range or no storage
/// is registered yet (fail closed).
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
fn syscall_tls_ptr(cpu_index: usize) -> Option<*mut SyscallTls> {
    if cpu_index >= SYSCALL_TLS_LEN.load(Ordering::Acquire) {
        return None;
    }
    let base = SYSCALL_TLS_BASE.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    // SAFETY: a non-zero `SYSCALL_TLS_LEN` (checked above) is published
    // in the same `register` call that stores the non-null base from a
    // `&'static SyscallTlsStorage`'s `tls` array of that length, and
    // `cpu_index < len`, so `base.add(cpu_index)` is in bounds.
    Some(unsafe { base.add(cpu_index) })
}

#[cfg(test)]
fn reset_syscall_tls_storage_for_tests() {
    SYSCALL_TLS_REGISTERED.store(false, Ordering::Release);
    SYSCALL_TLS_LEN.store(0, Ordering::Release);
    SYSCALL_TLS_BASE.store(core::ptr::null_mut(), Ordering::Release);
}

/// Return the linear address of the per-CPU [`SyscallTls`] slot for
/// `cpu_index`, populating its `kernel_rsp0` field with `kernel_rsp0`.
///
/// The returned address is the value the caller writes to
/// `IA32_KERNEL_GS_BASE` on that CPU.
///
/// # Errors
///
/// * [`crate::percpu::InitError::CpuIndexOutOfRange`] if `cpu_index` is
///   outside the registered [`SyscallTlsStorage`] (or no storage is
///   registered).
/// * [`crate::percpu::InitError::InvalidKernelStackPointer`] if
///   `kernel_rsp0` is null, not 16-byte aligned, non-canonical, or in
///   the user half (stack-pivot / CVE-2019-1125).
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
    // Fail closed before registration or for an out-of-range index: the registered storage's published
    // length is the only bound, not a baked-in `MAX_CPUS`.
    let slot = syscall_tls_ptr(cpu_index).ok_or(crate::percpu::InitError::CpuIndexOutOfRange)?;
    // Reject a non-canonical / user-range / misaligned stack top before
    // it can ever be loaded by `syscall` entry.
    validate_kernel_rsp0(kernel_rsp0)?;
    // SAFETY: caller's contract pins `cpu_index` to this CPU; no
    // other CPU writes to the same slot. `slot` points inside the
    // `&'static` registered storage (proved by `syscall_tls_ptr`).
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*slot).kernel_rsp0), kernel_rsp0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*slot).user_rsp_save), 0);
        Ok(slot as u64)
    }
}

/// Repoint *both* per-CPU kernel-entry stacks for `cpu_index` — the
/// `syscall` entry stack ([`SyscallTls::kernel_rsp0`], `gs:0`) **and** the
/// trap entry stack (`TSS.RSP0`) — at `kernel_rsp0`, without touching
/// `IA32_KERNEL_GS_BASE` or the `user_rsp_save` slot.
///
/// This is the per-resume half of the user-kthread `pre_resume` hook
/// (`plans/PI.md` §X, D2b-2b-A P-1c). [`install_kernel_rsp0`] runs once per
/// CPU at boot and returns the address to load into `IA32_KERNEL_GS_BASE`;
/// thereafter the `gs:` base is fixed and only the *value* in the slot must
/// change. When the scheduler is about to resume a user kthread, the kernel
/// stack the next entry from that task must pivot onto is **that task's
/// own** kernel stack — on aarch64 the EL1 trap reuses the running
/// kthread's `SP_EL1` implicitly, but on x86_64 a user→kernel transition
/// reads a per-CPU field: `syscall`/`sysret` reads `gs:0` (this module's
/// TLS) and an **interrupt or exception** (e.g. the P-1c preemption timer,
/// a `#PF`/`#GP`) reads `TSS.RSP0`. The two are one and the same per-task
/// stack (as on Linux), so they are repointed **together** here — one
/// definition that cannot diverge, rather than each
/// resume site repeating two writes. Repointing only one would let an
/// involuntary preemption (or fault) of one task land on another task's —
/// or the boot — kernel stack and corrupt it (a correctness *and* isolation
/// defect).
///
/// The value is validated exactly as [`install_kernel_rsp0`] validates it
/// ([`validate_kernel_rsp0`]: non-null, 16-byte aligned, canonical, kernel
/// half) before either field is written, so a hostile or buggy stack top is
/// rejected fail-closed rather than installed as a
/// stack-pivot vector. The `TSS.RSP0` repoint is freestanding-only (the TSS
/// is real hardware state); a host test exercises the `gs:0` path and the
/// shared validator.
///
/// # Errors
///
/// * [`crate::percpu::InitError::CpuIndexOutOfRange`] if `cpu_index` is
///   outside the registered [`SyscallTlsStorage`] (or no storage is
///   registered).
/// * [`crate::percpu::InitError::InvalidKernelStackPointer`] if
///   `kernel_rsp0` is null, misaligned, non-canonical, or in the user half.
/// * [`crate::percpu::InitError::NotInitialised`] (freestanding) if the
///   per-CPU `TSS` slot for `cpu_index` has not been finalised by
///   [`crate::percpu::init`] — never the case at resume time.
///
/// Indexes the registered [`SyscallTlsStorage`] on every target: a host
/// test registers a backing first, so the same bound-then-validate-then-
/// write path the bare-metal resume takes is exercised on the host.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
pub fn set_kernel_rsp0(cpu_index: usize, kernel_rsp0: u64) -> Result<(), crate::percpu::InitError> {
    // Fail closed before registration or for an out-of-range index.
    let slot = syscall_tls_ptr(cpu_index).ok_or(crate::percpu::InitError::CpuIndexOutOfRange)?;
    // Reject a non-canonical / user-range / misaligned stack top before it
    // can ever be loaded by `syscall` entry.
    validate_kernel_rsp0(kernel_rsp0)?;
    // SAFETY: the resume runs on the dispatcher's context on this CPU, which
    // is the only writer of its own slot, so the write does not race. Only
    // the `kernel_rsp0` field is touched; `user_rsp_save` is left to the
    // entry stub. `slot` points inside the `&'static` registered storage.
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*slot).kernel_rsp0), kernel_rsp0);
    }
    // Repoint the trap entry stack (`TSS.RSP0`) at the *same* per-task kernel
    // stack, so an interrupt/exception taken from ring 3 (the P-1c preemption
    // timer, a `#PF`/`#GP`) lands on this task's own kernel stack — not the
    // boot-time `TSS.RSP0` a concurrently parked task would also use. The two
    // user→kernel entry paths share one stack, so they are
    // repointed in lock-step here.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        // SAFETY: this runs on the dispatcher's context on `cpu_index` (the
        // resuming CPU) with interrupts disabled (the kernel issues no `sti`),
        // so no ring-3→ring-0 delivery races the `TSS.RSP0` write;
        // `install_tss_rsp0` re-validates the (already-validated) stack top and
        // fails closed if this CPU's `percpu::init` has not finalised the slot.
        unsafe {
            crate::percpu::install_tss_rsp0(cpu_index, kernel_rsp0)?;
        }
    }
    Ok(())
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
/// (see [`tairix_arch_x86_64_syscall_dispatch`]'s rustdoc); a
/// silent return would be an "open by default" failure per
/// . Storage is gated to the freestanding target — the
/// host build never reads or writes it (matches the
/// [`crate::preempt::set_timer_callback`] pattern).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static SYSCALL_DISPATCH_CALLBACK: AtomicUsize = AtomicUsize::new(0);

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
    SYSCALL_DISPATCH_CALLBACK.store(cb as usize, Ordering::Release);
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
            Some(unsafe { core::mem::transmute::<usize, SyscallDispatchFn>(raw) })
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
/// [`crate::interrupts`] takes for its default ISR).
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
unsafe extern "C" fn tairix_arch_x86_64_syscall_dispatch(
    number: u64,
    args_ptr: *const [u64; SYSCALL_MAX_ARGS],
) -> u64 {
    let raw = SYSCALL_DISPATCH_CALLBACK.load(Ordering::Acquire);
    if raw == 0 {
        // Fail-closed: a syscall reached the trampoline before the
        // binary installed its dispatcher. Continuing would mean
        // returning an unspecified value to user space, which is
        // exactly the "open by default" failure
        // forbid.
        crate::qemu_exit::exit_failure();
    }
    // SAFETY: see `dispatch_callback` — every store round-trips a
    // valid `SyscallDispatchFn`.
    let cb: SyscallDispatchFn = unsafe { core::mem::transmute::<usize, SyscallDispatchFn>(raw) };
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
/// 2. Transiently stash the user `%rsp` into `gs:USER_RSP_SAVE_OFFSET`
///    and load the kernel `%rsp` from `gs:KERNEL_RSP0_OFFSET`.
/// 3. Push the user `%rsp` (read straight back out of `gs:8`) so its
///    **durable** save lives on *this task's* kernel-stack frame, not
///    the shared per-CPU `gs:8` slot a concurrent task's syscall would
///    clobber across a cooperative mid-handler park (`plans/PI.md` X2).
///    Then push the user `RFLAGS` (`%r11`) and saved RIP (`%rcx`) so
///    they survive the Rust call (System V allows callees to clobber
///    both). The user-`%rsp` slot doubles as the System V alignment pad,
///    so the frame size — hence the "rsp ≡ 0 (mod 16) at `call`" rule —
///    is unchanged.
/// 4. Build the [`SYSCALL_MAX_ARGS`]-wide argument array on the
///    kernel stack from `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`.
/// 5. Set up the System V args: `%rdi = syscall number (saved rax)`,
///    `%rsi = &args[0]`. Call [`tairix_arch_x86_64_syscall_dispatch`].
/// 6. The return value is in `%rax` already — leave it.
/// 7. Pop the arg array back into `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`
///    (restoring the caller's argument registers — the user-side trap
///    stub declares only `rax`/`rcx`/`r11` clobbered, so handing back
///    dispatch residue would both miscompile the caller and leak
///    kernel register contents to ring 3), then pop `%r11` + `%rcx`.
/// 8. Restore user `%rsp` with a single `popq %rsp` from the frame
///    slot, `swapgs`, `sysretq`.
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
        // 2. Transiently stash user rsp, load kernel rsp.
        "movq %rsp, %gs:8",
        "movq %gs:0, %rsp",
        // 3. Durably save the user rsp on this task's kernel frame (the
        //    slot also serves as the System V alignment pad), then
        //    preserve user RIP (rcx) and RFLAGS (r11).
        "pushq %gs:8",
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
        // 6. Run the syscall body with device interrupts deliverable. The
        //    CPU cleared IF via IA32_FMASK on entry; we are now in a
        //    well-defined kernel context (swapgs done, pivoted onto this
        //    task's kernel stack, user RIP/RFLAGS/rsp frame-resident), so an
        //    interrupt taken here is taken at CPL=0 and pushes its frame onto
        //    this same kernel stack, services its source, and `iret`s back
        //    here — it never switches stacks (no IST for these vectors) and
        //    never reschedules the non-preemptible kernel (the ISRs gate
        //    preemption on the saved ring-3 CS; a tick taken in ring 0 only
        //    latches its reschedule, honoured at return-to-user in
        //    `completion_outcome`). This stops a long, non-blocking syscall
        //    body from monopolising the CPU with interrupts masked. Re-mask
        //    (`cli`) before restoring the user frame so `sysretq` returns to
        //    ring 3 with the entry residue gone; the user RFLAGS (with its
        //    own IF) is restored from `%r11` by `sysretq`.
        "sti",
        "call {dispatch}",
        "cli",
        // 7. Restore the caller's argument registers from the arg array
        //    (never a bare stack drop: the user-side trap stub promises
        //    the compiler only rax/rcx/r11 change across `syscall`, and
        //    the dispatch residue these registers hold here is kernel
        //    state that must not leak to ring 3), then r11 + rcx.
        "popq %rdi",
        "popq %rsi",
        "popq %rdx",
        "popq %r10",
        "popq %r8",
        "popq %r9",
        "popq %r11",
        "popq %rcx",
        // 8. Restore user rsp from the frame slot and return to user space.
        "popq %rsp",
        "swapgs",
        "sysretq",
        dispatch = sym tairix_arch_x86_64_syscall_dispatch,
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
/// `cpu_index` is outside the registered [`SyscallTlsStorage`] (or no
/// storage is registered).
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
    (u64::from(hi) << 32) | u64::from(lo)
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
    // Splitting the 64-bit MSR value into eax:edx is the documented
    // wrmsr ABI (Intel SDM Vol 2B §4.3); each half is exactly 32 bits
    // by construction, so the `as u32` truncations are lossless.
    #[allow(clippy::cast_possible_truncation)]
    let lo = value as u32;
    // The MSR takes the value as two halves; the shift moves the high 32 bits
    // into range, so the pair reconstructs it exactly.
    #[allow(clippy::cast_possible_truncation)]
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
        // IF, TF, DF, AC, NT, RF, VM, IOPL.
        assert_eq!(m & 0x0000_0200, 0x0000_0200, "IF");
        assert_eq!(m & 0x0000_0100, 0x0000_0100, "TF");
        assert_eq!(m & 0x0000_0400, 0x0000_0400, "DF");
        assert_eq!(m & 0x0004_0000, 0x0004_0000, "AC");
        assert_eq!(m & 0x0000_4000, 0x0000_4000, "NT");
        assert_eq!(m & 0x0001_0000, 0x0001_0000, "RF");
        assert_eq!(m & 0x0002_0000, 0x0002_0000, "VM");
        assert_eq!(m & 0x0000_3000, 0x0000_3000, "IOPL");
        // No other bits should be set — anything else would be an
        // undocumented effect the kernel must justify.
        let documented = 0x0000_0200
            | 0x0000_0100
            | 0x0000_0400
            | 0x0004_0000
            | 0x0000_4000
            | 0x0001_0000
            | 0x0002_0000
            | 0x0000_3000;
        assert_eq!(m, documented);
    }

    #[test]
    fn fmask_neutralises_an_adversarial_user_rflags() {
        // `tests/SECURITY.md` §5 (CWE-696): the CPU computes the kernel
        // entry `RFLAGS` as `user_rflags & !IA32_FMASK`. A malicious
        // user must not be able to carry `AC=1` (SMAP bypass), `DF=1`
        // (string-op direction), or a non-zero `IOPL` (ring-0 I/O
        // privilege) past the boundary.
        let malicious: u64 = 0x0004_0000 // AC
            | 0x0000_0400 // DF
            | 0x0000_3000 // IOPL = 3
            | 0x0000_0100 // TF
            | 0x0000_0002; // the always-set reserved bit 1
        let kernel_rflags = malicious & !fmask_value();
        assert_eq!(kernel_rflags & 0x0004_0000, 0, "AC must be masked off");
        assert_eq!(kernel_rflags & 0x0000_0400, 0, "DF must be masked off");
        assert_eq!(kernel_rflags & 0x0000_3000, 0, "IOPL must be masked off");
        assert_eq!(kernel_rflags & 0x0000_0100, 0, "TF must be masked off");
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
        // host, so `set_dispatch_callback` is a no-op stub there; the
        // getter must consistently report "none" so a future regression
        // that quietly enables host-side storage is caught.
        set_dispatch_callback(host_dispatch_noop);
        assert!(dispatch_callback().is_none());
    }

    // kernel-RSP0 stack-pivot validation (CVE-2019-1125) ----
    //
    // `install_kernel_rsp0` itself is freestanding-only (it writes the
    // per-CPU TLS arena), but its input check is the pure, host-testable
    // `validate_kernel_rsp0`. These pin the fail-closed behaviour the
    // QEMU stack-pivot test (tests/SECURITY.md §5) gates on.

    use crate::percpu::InitError;

    #[test]
    fn validate_kernel_rsp0_accepts_a_canonical_aligned_kernel_stack() {
        // A 16-byte-aligned address in the canonical higher half.
        assert_eq!(validate_kernel_rsp0(0xFFFF_8000_0010_0000), Ok(()));
        assert_eq!(validate_kernel_rsp0(CANONICAL_HIGHER_BASE), Ok(()));
    }

    #[test]
    fn validate_kernel_rsp0_rejects_null_and_misaligned() {
        assert_eq!(
            validate_kernel_rsp0(0),
            Err(InitError::InvalidKernelStackPointer)
        );
        // Higher-half but not 16-byte aligned.
        assert_eq!(
            validate_kernel_rsp0(0xFFFF_8000_0010_0008),
            Err(InitError::InvalidKernelStackPointer)
        );
    }

    #[test]
    fn validate_kernel_rsp0_rejects_a_user_range_stack() {
        // A 16-byte-aligned, canonical *lower-half* (user) address: a
        // stack-pivot vector — kernel entry must never run on it.
        assert_eq!(
            validate_kernel_rsp0(0x0000_7FFF_FFF0_0000),
            Err(InitError::InvalidKernelStackPointer)
        );
    }

    #[test]
    fn validate_kernel_rsp0_rejects_a_non_canonical_stack() {
        // Inside the non-canonical hole (bits 63:47 not all equal),
        // 16-byte aligned. The CPU would fault loading it.
        assert_eq!(
            validate_kernel_rsp0(0x0001_0000_0000_0000),
            Err(InitError::InvalidKernelStackPointer)
        );
    }

    #[test]
    fn is_canonical_matches_the_48_bit_rule() {
        assert!(is_canonical(0));
        assert!(is_canonical(CANONICAL_LOWER_LIMIT - 1));
        assert!(!is_canonical(CANONICAL_LOWER_LIMIT));
        assert!(!is_canonical(CANONICAL_HIGHER_BASE - 1));
        assert!(is_canonical(CANONICAL_HIGHER_BASE));
        assert!(is_canonical(u64::MAX));
    }

    // -- X1 per-resume kernel-RSP0 repoint (plans/PI.md §X) --------------
    //
    // `set_kernel_rsp0` indexes the registered `SyscallTlsStorage` and, for
    // an in-range slot, applies the same fail-closed stack-pivot check as
    // `install_kernel_rsp0` before it repoints the slot. Driving it through
    // a host-registered backing exercises the bound-then-validate-then-write
    // path the bare-metal resume takes. Registration is global set-once, so
    // all the per-resume assertions live in one test that owns it.
    #[test]
    fn set_kernel_rsp0_indexes_registered_storage_and_validates_stack_top() {
        // Declared first so the static precedes the statements that drive it.
        static STORAGE: SyscallTlsStorage<4> = SyscallTlsStorage::new();
        static STORAGE2: SyscallTlsStorage<2> = SyscallTlsStorage::new();

        reset_syscall_tls_storage_for_tests();

        // Before any storage is registered the repoint fails closed rather
        // than dereferencing a null base — even
        // for an otherwise-valid stack top.
        assert_eq!(registered_syscall_cpu_count(), 0);
        assert_eq!(
            set_kernel_rsp0(0, 0xFFFF_8000_0010_0000),
            Err(InitError::CpuIndexOutOfRange)
        );

        assert_eq!(STORAGE.register(), Ok(4));
        assert_eq!(registered_syscall_cpu_count(), 4);

        // A canonical, 16-byte-aligned kernel-half stack top is accepted.
        assert_eq!(set_kernel_rsp0(0, 0xFFFF_8000_0010_0000), Ok(()));
        assert_eq!(set_kernel_rsp0(3, CANONICAL_HIGHER_BASE), Ok(()));

        // Null / misaligned / user-range / non-canonical tops are rejected
        // fail-closed for an in-range slot (the stack-pivot guard).
        assert_eq!(
            set_kernel_rsp0(0, 0),
            Err(InitError::InvalidKernelStackPointer)
        );
        assert_eq!(
            set_kernel_rsp0(0, 0xFFFF_8000_0010_0008),
            Err(InitError::InvalidKernelStackPointer)
        );
        assert_eq!(
            set_kernel_rsp0(0, 0x0000_7FFF_FFF0_0000),
            Err(InitError::InvalidKernelStackPointer)
        );
        assert_eq!(
            set_kernel_rsp0(0, 0x0001_0000_0000_0000),
            Err(InitError::InvalidKernelStackPointer)
        );

        // An out-of-range index is rejected before the stack-top check.
        assert_eq!(
            set_kernel_rsp0(4, 0xFFFF_8000_0010_0000),
            Err(InitError::CpuIndexOutOfRange)
        );

        // Registration is set-once: a second backing is refused.
        assert_eq!(
            STORAGE2.register(),
            Err(SyscallTlsStorageError::AlreadyRegistered)
        );

        reset_syscall_tls_storage_for_tests();
    }

    // copy_from_user user-buffer validation (CWE-367 / CWE-822) ----
    //
    // `validate_user_buffer` is the pure, host-testable boundary check the
    // Stage-6 `copy_from_user` fault path gates on (per-access page-fault
    // fix-up is Stage-6). These pin the fail-closed semantics.

    #[test]
    fn validate_user_buffer_accepts_an_in_range_user_window() {
        // A page-aligned buffer well inside the user (lower) half.
        assert_eq!(validate_user_buffer(0x1000, 0x4000), Ok(()));
        // A zero-length buffer at a valid base touches nothing.
        assert_eq!(validate_user_buffer(0x1000, 0), Ok(()));
        // The exclusive end may sit exactly one past the last user byte.
        assert_eq!(
            validate_user_buffer(CANONICAL_LOWER_LIMIT - 0x1000, 0x1000),
            Ok(())
        );
    }

    #[test]
    fn validate_user_buffer_rejects_a_null_base() {
        assert_eq!(validate_user_buffer(0, 0), Err(UserBufferError::Null));
        assert_eq!(validate_user_buffer(0, 0x10), Err(UserBufferError::Null));
    }

    #[test]
    fn validate_user_buffer_rejects_a_kernel_range_base() {
        // A canonical higher-half (kernel) base: the ret2usr /
        // kernel-pointer-confusion vector.
        assert_eq!(
            validate_user_buffer(CANONICAL_HIGHER_BASE, 0x10),
            Err(UserBufferError::KernelRange)
        );
        assert_eq!(
            validate_user_buffer(0xFFFF_FFFF_FFFF_FFF0, 0),
            Err(UserBufferError::KernelRange)
        );
    }

    #[test]
    fn validate_user_buffer_rejects_a_non_canonical_base() {
        // Inside the 48-bit hole (≥ user limit but < kernel base).
        assert_eq!(
            validate_user_buffer(CANONICAL_LOWER_LIMIT, 0x10),
            Err(UserBufferError::NonCanonical)
        );
        assert_eq!(
            validate_user_buffer(CANONICAL_HIGHER_BASE - 1, 0),
            Err(UserBufferError::NonCanonical)
        );
    }

    #[test]
    fn validate_user_buffer_rejects_a_buffer_that_crosses_out_of_the_user_half() {
        // A valid base whose length pushes the end past the user half:
        // the buffer would straddle the non-canonical hole.
        assert_eq!(
            validate_user_buffer(CANONICAL_LOWER_LIMIT - 0x1000, 0x2000),
            Err(UserBufferError::KernelRange)
        );
    }

    #[test]
    fn validate_user_buffer_rejects_a_length_that_wraps() {
        // `ptr + len` overflows u64 (CWE-190) — the length wraps the
        // buffer back over its own start.
        assert_eq!(
            validate_user_buffer(0x1000, u64::MAX),
            Err(UserBufferError::Overflows)
        );
    }
}
