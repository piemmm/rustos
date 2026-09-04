//! x86_64 page-fault (`#PF`, vector 14) entry + settable fault hook.
//!
//! The production IDT ([`crate::interrupts`]) routes every vector at
//! `percpu::init` time through the fail-closed default thunk; every
//! architecturally-defined exception vector is then overwritten with its
//! own entry ([`crate::exceptions`]) and the LAPIC timer / external-IRQ
//! vectors with their dedicated stubs. A page fault is the one exception
//! the kernel can *resolve* — a demand-paged file mapping, or a fault
//! inside the guarded user-copy window — so vector 14 keeps its own
//! resumable entry here rather than sharing the diverging exception tail.
//! This module is that entry, the packed [`crate::fault::exception_syndrome`]
//! every
//! exception reports through, and a single settable fault observer the
//! kernel cannot otherwise reach.
//!
//! It is the x86_64 analogue of the riscv64 ([`crate`]'s sibling
//! `tairix_arch_riscv64::fault`) and aarch64
//! (`tairix_arch_aarch64::fault`) synchronous-fault hooks, with the same
//! three-tier posture:
//!
//! * A **resolvable ring-3 data fault**
//!   ([`crate::fault::is_resolvable_user_fault`]) is offered to the
//!   installed [`crate::fault::UserFaultResolveFn`] first — the
//!   demand-paged file-mapping path. A resolved fault returns through the
//!   entry's full GPR restore and `iretq`, retrying the faulting
//!   instruction against the now-resident page.
//! * Any **other ring-3 fault** — an instruction-fetch `#PF` (a wild
//!   jump), or a data fault with no resolver installed — is the running
//!   task's own and is charged to it through the installed
//!   [`crate::fault::UserFaultTerminateFn`], which kills the task and
//!   leaves the CPU running other work. The same slot serves the ring-3
//!   tail of every other exception vector ([`crate::exceptions`]), so one
//!   task's bad instruction can never park a core.
//! * Everything left is the **kernel's own** and unrecoverable in this
//!   kernel slice, so the default posture is to fail closed (never
//!   silently reset). A single fault handler may be installed through
//!   [`crate::fault::set_fault_handler`] before any fault can fire; the
//!   dedicated `#PF` entry then invokes it with the decoded error code,
//!   the linear address (`CR2`), and the faulting instruction pointer.
//!   With no observer installed the entry preserves the exact fail-closed
//!   behaviour the default thunk had (a `#PF` halts the binary through
//!   `qemu_exit::exit_failure`).
//!
//! The faulting address lives in `CR2` on x86_64 (it is *not* pushed on
//! the stack), so the dedicated entry captures it before any further
//! fault could clobber it. The fatal handler **must not return** — see
//! [`crate::fault::FaultHandlerFn`].
//!
//! # No global mutable state
//!
//! The slot is set-once, backed by an atomic the entry reads without a
//! lock; a second publish fails closed. The error-code
//! decode and the slot build on the host, so their unit tests run under
//! `cargo test`; only the dedicated entry stub and the `CR2` read it
//! feeds the handler are gated to the freestanding x86_64 target.

use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_arch_api::backtrace::UserRegisterFrame;

/// IDT vector the CPU raises for a page fault (`#PF`, Intel SDM Vol 3A
/// Table 6-1).
pub const PAGE_FAULT_VECTOR: u8 = 14;

/// `#PF` error-code bit `P` (bit 0): `0` = the access referenced a
/// not-present page, `1` = a page-level protection violation
/// (Intel SDM Vol 3A §4.7).
pub const PF_ERR_PRESENT: u64 = 1 << 0;

/// `#PF` error-code bit `W/R` (bit 1): `1` = the access was a write.
pub const PF_ERR_WRITE: u64 = 1 << 1;

/// `#PF` error-code bit `U/S` (bit 2): `1` = the access originated at
/// CPL 3 (user mode).
pub const PF_ERR_USER: u64 = 1 << 2;

/// `#PF` error-code bit `RSVD` (bit 3): `1` = a reserved bit was set in
/// a paging-structure entry on the translation path.
pub const PF_ERR_RESERVED: u64 = 1 << 3;

/// `#PF` error-code bit `I/D` (bit 4): `1` = the fault was an
/// instruction fetch.
pub const PF_ERR_INSTR: u64 = 1 << 4;

/// `true` iff the fault referenced a **not-present** page (error-code
/// `P` bit clear) — the cause raised when user code touches an address
/// the active page tables do not map, e.g. a use-after-unmap.
#[must_use]
pub const fn is_not_present(error_code: u64) -> bool {
    error_code & PF_ERR_PRESENT == 0
}

/// `true` iff the fault originated in user mode (error-code `U/S` bit
/// set).
#[must_use]
pub const fn is_user(error_code: u64) -> bool {
    error_code & PF_ERR_USER != 0
}

/// `true` iff the faulting access was a write (error-code `W/R` bit set).
#[must_use]
pub const fn is_write(error_code: u64) -> bool {
    error_code & PF_ERR_WRITE != 0
}

/// `true` iff a `#PF` with this error code is a **user-mode data**
/// access (read or write, not an instruction fetch) — the class the
/// dedicated `#PF` entry offers to the installed [`UserFaultResolveFn`].
/// A kernel-mode fault (`U/S` clear) is never offered: it is never file
/// backing (the kernel copy path resolves its own misses in software),
/// and an instruction fetch is never file backing either (a file mapping
/// is never executable). A write in this class is offered but never
/// *resolved* — see [`is_resolvable_user_fault`] — the resolver kills
/// the faulting task instead, so a store to a read-only mapping (or any
/// wild write) costs the task, never the CPU.
#[must_use]
pub const fn is_user_data_fault(error_code: u64) -> bool {
    is_user(error_code) && error_code & PF_ERR_INSTR == 0
}

/// `true` iff a `#PF` with this error code may actually be *resolved* by
/// making a page resident: a **user-mode, not-present, read data**
/// access — the only shape a demand-paged file-mapping fault can take.
/// Everything else in the offered class is fatal to the task:
///
/// * a protection violation (`P` set) cannot be fixed by making a page
///   resident;
/// * a write can never be made valid — a file mapping is read-only, and
///   resolving a write fault as "already resident, retry" would
///   re-execute the store into an endless fault storm instead of
///   killing the task.
#[must_use]
pub const fn is_resolvable_user_fault(error_code: u64) -> bool {
    is_user(error_code)
        && is_not_present(error_code)
        && error_code & (PF_ERR_WRITE | PF_ERR_INSTR) == 0
}

/// Bit position of the vector field in the packed exception syndrome
/// ([`exception_syndrome`]).
const SYNDROME_VECTOR_SHIFT: u32 = 32;

/// Bit set in a packed exception syndrome when the exception was taken
/// from ring 3 rather than from kernel mode.
const SYNDROME_FROM_USER: u64 = 1 << 40;

/// Pack an x86_64 exception into the neutral fault syndrome word.
///
/// x86_64 has no single cause register: the cause is the *vector*, and
/// only some vectors push an error code. The two are folded into one word
/// so the neutral `(syndrome, address, pc)` triple every port reports can
/// carry both — the error code in bits `0..32` and the vector in bits
/// `32..40`, with bit `40` set when the exception came from ring 3.
///
/// The error code occupies the low half deliberately: it keeps
/// [`is_not_present`] / [`is_user`] / [`is_write`] valid decoders of a
/// `#PF` syndrome, so a handler that only ever provokes page faults reads
/// the same bits it always did. Those decoders are meaningful **only** when
/// [`syndrome_vector`] reports [`PAGE_FAULT_VECTOR`]; for any other vector
/// the error code's bits carry that vector's own meaning (a selector for
/// `#TS`/`#NP`/`#SS`/`#GP`, zero for `#DF` and `#AC`) or nothing at all.
#[must_use]
pub const fn exception_syndrome(vector: u8, error_code: u64, from_user: bool) -> u64 {
    // A hardware error code is 32 bits wide (Intel SDM Vol 3A §6.13), so
    // the low half holds it losslessly; mask rather than trust the caller.
    let code = error_code & 0xFFFF_FFFF;
    let user = if from_user { SYNDROME_FROM_USER } else { 0 };
    code | ((vector as u64) << SYNDROME_VECTOR_SHIFT) | user
}

/// The IDT vector a packed [`exception_syndrome`] names.
#[must_use]
pub const fn syndrome_vector(syndrome: u64) -> u8 {
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY-INVARIANT: the field is 8 bits wide by construction
    // (`exception_syndrome` shifts a `u8` into `32..40`), so the mask makes
    // the narrowing lossless.
    let vector = ((syndrome >> SYNDROME_VECTOR_SHIFT) & 0xFF) as u8;
    vector
}

/// The hardware error code a packed [`exception_syndrome`] carries, or `0`
/// for a vector that pushes none.
#[must_use]
pub const fn syndrome_error_code(syndrome: u64) -> u64 {
    syndrome & 0xFFFF_FFFF
}

/// `true` when a packed [`exception_syndrome`] records an exception taken
/// from ring 3.
#[must_use]
pub const fn syndrome_from_user(syndrome: u64) -> bool {
    syndrome & SYNDROME_FROM_USER != 0
}

/// Signature of the fault handler an exception entry invokes.
///
/// `syndrome` is the packed [`exception_syndrome`] naming the vector, the
/// hardware error code, and the privilege level the exception came from;
/// for a `#PF` its low half is the architectural error code, so
/// [`is_not_present`] / [`is_user`] / [`is_write`] decode it directly.
/// `faulting_addr` is the faulting linear address read from `CR2` for a
/// `#PF` and `0` for every other vector, which pushes no faulting address.
/// `rip` is the PC of the faulting instruction. The handler **must not
/// return**: this kernel slice has no fix-up logic to resume the faulting
/// instruction, so a return would re-trap forever. Test handlers report
/// the outcome to QEMU through [`crate::qemu_exit`].
pub type FaultHandlerFn = extern "C" fn(syndrome: u64, faulting_addr: u64, rip: u64) -> !;

/// Slot holding the installed fault handler as a raw function pointer
/// (`0` = none installed).
static FAULT_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Failure modes of [`set_fault_handler`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetFaultHandlerError {
    /// A handler was already published; the slot is set-once per boot.
    AlreadyInstalled,
}

/// Install the page-fault observer.
///
/// Must be called once, on the boot CPU, before any fault can fire.
///
/// # Errors
///
/// [`SetFaultHandlerError::AlreadyInstalled`] on the second publish.
pub fn set_fault_handler(cb: FaultHandlerFn) -> Result<(), SetFaultHandlerError> {
    let raw = cb as usize;
    FAULT_HANDLER
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetFaultHandlerError::AlreadyInstalled)
}

/// Read back the installed fault handler, if any. The dedicated `#PF`
/// entry calls this on a page fault; it is also a test/diagnostic
/// observer.
#[must_use]
pub fn fault_handler() -> Option<FaultHandlerFn> {
    let raw = FAULT_HANDLER.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: every value stored into the slot round-trips a valid
        // `FaultHandlerFn` through `set_fault_handler`; function pointers
        // are `usize`-sized so the transmute is lossless.
        Some(unsafe { core::mem::transmute::<usize, FaultHandlerFn>(raw) })
    }
}

#[cfg(test)]
fn clear_fault_handler_for_tests() {
    // Test-only: lets back-to-back host tests reinstall a handler.
    // Production code never clears the slot.
    FAULT_HANDLER.store(0, Ordering::Release);
}

/// Signature of the user-fault resolver the dedicated `#PF` entry offers
/// a ring-3 data fault to before the fatal path.
///
/// `faulting_addr` is the `CR2` faulting linear address and `write` the
/// `#PF` error-code `W/R` verdict (`true` = the access was a store). A
/// `true` return means the fault is dealt with and the entry simply
/// returns — the interrupt frame's `RIP` still points at the faulting
/// instruction, so the `iretq` retries the access against the
/// now-resident page; only a read is ever resolved this way (file
/// mappings are read-only). A `false` return means the fault was not
/// (and will never be) resolvable and the entry falls through to the
/// fatal [`FaultHandlerFn`] path. The callback may also *not return* for
/// the faulting task: when the fault is fatal to the task alone — every
/// write, and any unresolvable read — the binary's callback suspends it
/// into the scheduler with an exit action and the entry's call never
/// completes on that stack — exactly like a rescheduling syscall, so the
/// task dies and the CPU never halts. Like every trap-path callback it
/// is a bare `extern "C" fn` with no captured environment.
///
/// `regs` is a pointer to the faulting user register frame the `#PF`
/// dispatcher built from the saved GPR block and the interrupt frame's
/// user `rsp` (or null if unavailable), threaded so the resolver can
/// record a post-mortem crash record with a backtrace. The callee narrows
/// it to `Option<&UserRegisterFrame>` and never dereferences a null
/// pointer.
pub type UserFaultResolveFn =
    extern "C" fn(faulting_addr: u64, write: bool, regs: *const UserRegisterFrame) -> bool;

/// Slot holding the installed user-fault resolver as a raw function
/// pointer (`0` = none installed).
static USER_FAULT_RESOLVER: AtomicUsize = AtomicUsize::new(0);

/// Install the user-fault resolver.
///
/// Must be called once, on the boot CPU, before user space is entered
/// (the syscall-dispatch ordering contract). Without one installed every
/// ring-3 `#PF` takes the fatal path — fail closed, exactly as before
/// demand paging existed.
///
/// # Errors
///
/// [`SetFaultHandlerError::AlreadyInstalled`] on the second publish.
pub fn set_user_fault_resolver(cb: UserFaultResolveFn) -> Result<(), SetFaultHandlerError> {
    let raw = cb as usize;
    USER_FAULT_RESOLVER
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetFaultHandlerError::AlreadyInstalled)
}

/// Read back the installed user-fault resolver, if any. The dedicated
/// `#PF` entry calls this on a resolvable ring-3 data fault; it is also
/// a test/diagnostic observer.
#[must_use]
pub fn user_fault_resolver() -> Option<UserFaultResolveFn> {
    let raw = USER_FAULT_RESOLVER.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: every value stored into the slot round-trips a valid
        // `UserFaultResolveFn` through `set_user_fault_resolver`; function
        // pointers are `usize`-sized so the transmute is lossless.
        Some(unsafe { core::mem::transmute::<usize, UserFaultResolveFn>(raw) })
    }
}

#[cfg(test)]
fn clear_user_fault_resolver_for_tests() {
    // Test-only: lets back-to-back host tests reinstall a resolver.
    // Production code never clears the slot.
    USER_FAULT_RESOLVER.store(0, Ordering::Release);
}

/// Signature of the user-fault **terminator** an exception entry calls for
/// a ring-3 exception it can neither treat as a syscall nor resolve as a
/// demand-paged fault — an instruction-fetch `#PF` (a wild jump), an
/// invalid opcode (`#UD`), a general-protection violation (`#GP`), an
/// alignment check (`#AC`), and the like.
///
/// Unlike [`UserFaultResolveFn`] this never resolves and the entry never
/// retries: the faulting instruction is genuinely unrecoverable, so the
/// callback records the task's crash exit and reclaims it, then suspends it
/// into the scheduler with an exit action — the call never completes on
/// that stack, exactly like the fatal branch of a resolver. That keeps a
/// user task's own bad instruction from parking the whole CPU. A `false`
/// return means the exception could not be attributed to a running task
/// (none current, or no published user kthread), so the entry falls through
/// to its fatal [`FaultHandlerFn`]/park path — a genuine kernel-level
/// failure, not a user one.
///
/// `fault_pc` is the interrupted `rip` (the offending instruction), and
/// `regs` the captured ring-3 register frame (or null), threaded so the
/// termination can record a post-mortem crash record with a backtrace. Like
/// every trap-path callback it is a bare `extern "C" fn` with no captured
/// environment.
pub type UserFaultTerminateFn =
    extern "C" fn(fault_pc: u64, regs: *const UserRegisterFrame) -> bool;

/// Slot holding the installed user-fault terminator as a raw function
/// pointer (`0` = none installed).
static USER_FAULT_TERMINATOR: AtomicUsize = AtomicUsize::new(0);

/// Install the user-fault terminator.
///
/// Must be called once, on the boot CPU, before user space is entered
/// (beside [`set_user_fault_resolver`]). Without one installed an
/// unrecoverable ring-3 exception takes the fatal path (park) — fail
/// closed, exactly as before this path existed, so the omission can never
/// silently continue running a task over an unhandled exception.
///
/// # Errors
///
/// [`SetFaultHandlerError::AlreadyInstalled`] on the second publish.
pub fn set_user_fault_terminator(cb: UserFaultTerminateFn) -> Result<(), SetFaultHandlerError> {
    let raw = cb as usize;
    USER_FAULT_TERMINATOR
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetFaultHandlerError::AlreadyInstalled)
}

/// Read back the installed user-fault terminator, if any. The exception
/// entries call this for an unrecoverable ring-3 exception; also a
/// test/diagnostic observer.
#[must_use]
pub fn user_fault_terminator() -> Option<UserFaultTerminateFn> {
    let raw = USER_FAULT_TERMINATOR.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: every value stored into the slot round-trips a valid
        // `UserFaultTerminateFn` through `set_user_fault_terminator`;
        // function pointers are `usize`-sized so the transmute is lossless.
        Some(unsafe { core::mem::transmute::<usize, UserFaultTerminateFn>(raw) })
    }
}

#[cfg(test)]
fn clear_user_fault_terminator_for_tests() {
    // Test-only: lets back-to-back host tests reinstall a terminator.
    // Production code never clears the slot.
    USER_FAULT_TERMINATOR.store(0, Ordering::Release);
}

// --- Freestanding dedicated `#PF` entry ----------------------------

/// Linear address of the dedicated `#PF` ISR stub, for
/// [`crate::percpu::install_vector`].
///
/// Only meaningful on the freestanding target — the symbol is the
/// `#[unsafe(naked)]` stub below.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub fn page_fault_isr_addr() -> u64 {
    page_fault_isr as *const () as usize as u64
}

/// Dedicated `#PF` (vector 14) ISR stub — **resumable**.
///
/// On entry the CPU has pushed the hardware error code and the 5-word
/// [`crate::interrupts::InterruptStackFrame`] on the destination stack
/// (RSP0 for a ring-3 fault), so `%rsp` points at the error code and
/// `[%rsp + 8]` at the faulting `rip`. The stub saves the 15
/// architectural GPRs in the [`crate::interrupts::SavedRegs`] order
/// (the same order `define_isr!` pins), marshals `(error_code, CR2,
/// rip, &frame.rip)` into the `SysV` argument registers, and calls
/// [`tairix_arch_x86_64_page_fault_dispatch`]. The dispatcher *returns
/// only when the fault was dealt with* — a resolved ring-3 demand-paging
/// fault (the frame's untouched `RIP` re-runs the faulting instruction,
/// which now succeeds) or a kernel-mode fault inside the guarded
/// user-copy window (the dispatcher rewrote the frame's `RIP` to the
/// copy's fix-up, so the `iretq` resumes there and the copy reports the
/// fault as an error). The stub then restores the GPRs, drops the
/// hardware error code, and `iretq`s. Any other fault never returns
/// from the dispatcher (the fatal path diverges), so a stale resume is
/// impossible.
///
/// `CR2` is read after the GPR saves (a register is needed to hold it)
/// but before any access that could itself fault: pushes to the
/// always-mapped per-CPU kernel stack cannot raise `#PF`.
///
/// Stack alignment: the CPU 16-aligns `%rsp` before pushing the frame on
/// a stack switch (ring 3 -> RSP0), so after the error code + 5-word
/// frame (48 bytes) `%rsp` is 16-aligned on entry, and after the 15 GPR
/// pushes (120 bytes) it is ≡ 8 (mod 16). The `subq $8` re-aligns it so
/// the `call` lands the `SysV` callee with `%rsp ≡ 8 (mod 16)` after its
/// return-address push — the System V AMD64 §3.2.2 entry state.
///
/// # Safety
///
/// Only the CPU's IDT may invoke this symbol (installed via
/// [`crate::percpu::install_vector`] on [`PAGE_FAULT_VECTOR`]). Calling
/// it directly from Rust is undefined behaviour because it expects the
/// CPU-pushed error code + interrupt frame on the stack, not a return
/// address.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn page_fault_isr() {
    core::arch::naked_asm!(
        "pushq %rax",
        "pushq %rcx",
        "pushq %rdx",
        "pushq %rbx",
        "pushq %rbp",
        "pushq %rsi",
        "pushq %rdi",
        "pushq %r8",
        "pushq %r9",
        "pushq %r10",
        "pushq %r11",
        "pushq %r12",
        "pushq %r13",
        "pushq %r14",
        "pushq %r15",
        // %rdi <- error code (above the 15 saved GPRs = 120 bytes),
        // %rsi <- CR2 (faulting linear address), %rdx <- faulting rip,
        // %rcx <- address of the frame's RIP slot, so the dispatcher can
        // redirect a kernel-mode fault inside the guarded user-copy
        // window to the copy's fix-up (the `iretq` below then resumes
        // there).
        "movq 120(%rsp), %rdi",
        "mov %cr2, %rsi",
        "movq 128(%rsp), %rdx",
        "leaq 128(%rsp), %rcx",
        // %r8 <- &SavedRegs (the base of the 15-GPR block, = %rsp before
        // the alignment pad), %r9 <- the interrupted user %rsp from the CPU
        // iret frame (at 152(%rsp): error 120, rip 128, cs 136, rflags 144,
        // rsp 152), so the dispatcher can build the faulting register frame.
        "movq %rsp, %r8",
        "movq 152(%rsp), %r9",
        "subq $8, %rsp",
        "call {dispatch}",
        "addq $8, %rsp",
        // The dispatcher returned: the fault is resolved. Restore the
        // interrupted GPRs, drop the hardware error code, and retry the
        // faulting instruction.
        "popq %r15",
        "popq %r14",
        "popq %r13",
        "popq %r12",
        "popq %r11",
        "popq %r10",
        "popq %r9",
        "popq %r8",
        "popq %rdi",
        "popq %rsi",
        "popq %rbp",
        "popq %rbx",
        "popq %rdx",
        "popq %rcx",
        "popq %rax",
        "addq $8, %rsp",
        "iretq",
        dispatch = sym tairix_arch_x86_64_page_fault_dispatch,
        options(att_syntax),
    )
}

/// Rust dispatcher the dedicated `#PF` stub calls.
///
/// A **kernel-mode** fault whose `rip` lies inside the guarded
/// user-copy window ([`crate::uaccess`]) is redirected to the copy's
/// fix-up by rewriting the interrupt frame's `RIP` slot (`rip_slot`)
/// and returning: the stub's restore + `iretq` resume at the fix-up,
/// which reports the fault to the copy's caller as an error. Every
/// other kernel-mode fault stays on the fatal path.
///
/// A ring-3 data fault ([`is_user_data_fault`], read or write) is
/// offered to the installed [`UserFaultResolveFn`] first, with the
/// error-code `W/R` verdict: a `true` return means the faulting page is
/// now resident (reads only — a write is never resolved, the resolver
/// kills the faulting task instead), and this function returns so the
/// stub restores the GPRs and `iretq`s into a retry of
/// the faulting instruction.
///
/// Every **other** ring-3 fault goes to the installed
/// [`UserFaultTerminateFn`], which kills the task and never returns for
/// it: an instruction-fetch `#PF` is never file backing (a file mapping is
/// never executable), so a wild jump is unrecoverable but is still one
/// task's mistake — parking the CPU for it would turn a process fault into
/// a machine-wide denial of service. A ring-3 data fault with no resolver
/// installed reaches the terminator on the same grounds.
///
/// Only the kernel's own failures are fatal, and those **never return**:
/// the installed [`FaultHandlerFn`] observes them, or, with none
/// installed, the fail-closed default halts the binary through
/// [`crate::qemu_exit::exit_failure`] — exactly the posture the
/// non-resumable entry had. A ring-3 fault reaches it only when the
/// resolver or terminator could not attribute the fault to a running task
/// at all.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
extern "C" fn tairix_arch_x86_64_page_fault_dispatch(
    error_code: u64,
    faulting_addr: u64,
    rip: u64,
    rip_slot: *mut u64,
    saved: *const crate::interrupts::SavedRegs,
    user_rsp: u64,
) {
    if is_user(error_code) {
        // A *data* access may be demand-paged, so it goes to the resolver,
        // whose verdict is then final: a `false` means the fault was not
        // attributable to a running task at all, which is the kernel's own
        // failure and not a second thing to charge the task for. Every
        // other ring-3 fault is charged straight to the task, which the
        // terminator kills without returning.
        let resolver = if is_user_data_fault(error_code) {
            user_fault_resolver()
        } else {
            None
        };
        // SAFETY: `saved` is the stub-provided address of the live 15-GPR
        // `SavedRegs` block on this kernel stack, and the gate above proved
        // the fault came from ring 3, so the callback runs on this task's
        // own trap control flow under the in-handler GS convention.
        unsafe {
            match resolver {
                Some(resolve) => {
                    if with_ring3_context(saved, rip, user_rsp, |regs| {
                        resolve(faulting_addr, is_write(error_code), regs)
                    }) {
                        return;
                    }
                }
                None => {
                    if let Some(terminate) = user_fault_terminator() {
                        let _ =
                            with_ring3_context(saved, rip, user_rsp, |regs| terminate(rip, regs));
                    }
                }
            }
        }
    } else if let Some(fixup) = crate::uaccess::kernel_fixup_for(rip) {
        // A kernel-mode page fault inside the guarded user-copy window: the
        // validated copy's software proof was violated underneath it.
        // Rewrite the frame's RIP so the stub's `iretq` resumes at the
        // copy's fix-up and the copy returns an error to its caller instead
        // of taking the CPU down.
        // SAFETY: `rip_slot` is the stub-provided address of the live
        // interrupt frame's RIP word; the fix-up address is a real
        // instruction in this image.
        unsafe {
            *rip_slot = fixup;
        }
        return;
    }
    match fault_handler() {
        Some(handler) => handler(
            exception_syndrome(PAGE_FAULT_VECTOR, error_code, is_user(error_code)),
            faulting_addr,
            rip,
        ),
        None => crate::qemu_exit::exit_failure(),
    }
}

/// Run `call` on the faulting ring-3 register frame under the in-handler
/// GS convention — the one bracket every user-fault callback is invoked
/// through, from the dedicated `#PF` entry and from the ring-3 tail of
/// every other exception vector ([`crate::exceptions`]).
///
/// An interrupt gate taken from ring 3 does *not* swap GS, and every
/// user-fault callback may reschedule (park on filesystem I/O, or suspend a
/// killed task with an exit action), which requires the kernel GS base — so
/// the pair brackets exactly one call. A callback that suspends the task
/// never returns here and the park machinery owns the convention from that
/// point, exactly as on the LAPIC-timer preemption path; otherwise the user
/// GS is restored, so the `iretq` or the fatal tail proceeds under the same
/// GS it would have without a callback.
///
/// The register frame is built from the saved GPR block, the faulting
/// `rip`, and the interrupt frame's user `rsp`, and lives on this kernel
/// stack for the duration of the call, so the callback can record a
/// post-mortem crash record with a backtrace.
///
/// # Safety
///
/// * `saved` must point to the live 15-GPR
///   [`crate::interrupts::SavedRegs`] block the exception stub persisted on
///   this kernel stack.
/// * The exception must have been taken from ring 3, so each `swapgs` is
///   balanced against the gate that performed none.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub(crate) unsafe fn with_ring3_context<R>(
    saved: *const crate::interrupts::SavedRegs,
    rip: u64,
    user_rsp: u64,
    call: impl FnOnce(*const UserRegisterFrame) -> R,
) -> R {
    // SAFETY: `swapgs` is privileged and runs in ring 0 here; it touches
    // only the GS-base/`KERNEL_GS_BASE` swap, no memory or flags.
    unsafe {
        core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
    }
    // SAFETY: the caller guarantees `saved` addresses the live saved block.
    let frame = unsafe { user_register_frame(saved, rip, user_rsp) };
    let out = call(&raw const frame);
    // SAFETY: as above — the matching swap restoring the user GS.
    unsafe {
        core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
    }
    out
}

/// Build the faulting user register frame from the saved GPR block, the
/// faulting `rip`, and the interrupted user `rsp`.
///
/// `pc` is `rip`, `sp` is the user `rsp` from the CPU iret frame, and the
/// frame pointer is `rbp`; the System V AMD64 frame layout
/// ([`crate::backtrace::Backtracer::LAYOUT`]) drives the crash-path fp
/// walk, so the frame is marked `fp_valid`.
///
/// # Safety
///
/// `saved` must point to the live 15-GPR [`crate::interrupts::SavedRegs`]
/// block the `#PF` stub persisted on this kernel stack; it is read once
/// here and outlives the read.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe fn user_register_frame(
    saved: *const crate::interrupts::SavedRegs,
    rip: u64,
    user_rsp: u64,
) -> UserRegisterFrame {
    use tairix_arch_api::backtrace::RegisterSnapshot;
    // SAFETY: the caller guarantees `saved` addresses the live saved block.
    let s = unsafe { &*saved };
    let snapshot = RegisterSnapshot::new(rip, user_rsp, s.rbp)
        .with("rax", s.rax)
        .with("rbx", s.rbx)
        .with("rcx", s.rcx)
        .with("rdx", s.rdx)
        .with("rsi", s.rsi)
        .with("rdi", s.rdi)
        .with("rbp", s.rbp)
        .with("r8", s.r8)
        .with("r9", s.r9)
        .with("r10", s.r10)
        .with("r11", s.r11)
        .with("r12", s.r12)
        .with("r13", s.r13)
        .with("r14", s.r14)
        .with("r15", s.r15);
    UserRegisterFrame::new(snapshot, crate::backtrace::Backtracer::LAYOUT, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syndrome_round_trips_the_vector_error_code_and_privilege() {
        let syndrome = exception_syndrome(13, 0x1234_5678, true);
        assert_eq!(syndrome_vector(syndrome), 13);
        assert_eq!(syndrome_error_code(syndrome), 0x1234_5678);
        assert!(syndrome_from_user(syndrome));

        let kernel = exception_syndrome(6, 0, false);
        assert_eq!(syndrome_vector(kernel), 6);
        assert_eq!(syndrome_error_code(kernel), 0);
        assert!(!syndrome_from_user(kernel));
    }

    #[test]
    fn a_page_fault_syndrome_still_decodes_through_the_error_code_helpers() {
        // The error code occupies the low half precisely so a handler that
        // provokes only page faults keeps reading the same bits.
        let code = PF_ERR_USER | PF_ERR_WRITE;
        let syndrome = exception_syndrome(PAGE_FAULT_VECTOR, code, true);
        assert_eq!(syndrome_vector(syndrome), PAGE_FAULT_VECTOR);
        assert!(is_not_present(syndrome));
        assert!(is_user(syndrome));
        assert!(is_write(syndrome));
        assert!(is_resolvable_user_fault(exception_syndrome(
            PAGE_FAULT_VECTOR,
            PF_ERR_USER,
            true
        )));
    }

    #[test]
    fn a_wider_than_32_bit_error_code_cannot_reach_the_vector_field() {
        // Fail closed on a malformed input rather than corrupt the vector
        // a reader decodes the record by.
        let syndrome = exception_syndrome(8, u64::MAX, false);
        assert_eq!(syndrome_vector(syndrome), 8);
        assert_eq!(syndrome_error_code(syndrome), 0xFFFF_FFFF);
        assert!(!syndrome_from_user(syndrome));
    }

    #[test]
    fn page_fault_vector_matches_intel_sdm() {
        // Intel SDM Vol 3A Table 6-1: #PF is vector 14.
        assert_eq!(PAGE_FAULT_VECTOR, 14);
    }

    #[test]
    fn error_code_bits_match_intel_sdm() {
        // Intel SDM Vol 3A §4.7 Figure 4-12.
        assert_eq!(PF_ERR_PRESENT, 1);
        assert_eq!(PF_ERR_WRITE, 2);
        assert_eq!(PF_ERR_USER, 4);
        assert_eq!(PF_ERR_RESERVED, 8);
        assert_eq!(PF_ERR_INSTR, 16);
    }

    #[test]
    fn not_present_is_the_cleared_present_bit() {
        // A bare not-present supervisor read is error code 0.
        assert!(is_not_present(0));
        // A not-present user write keeps P clear but sets W and U/S.
        assert!(is_not_present(PF_ERR_WRITE | PF_ERR_USER));
        // A protection violation has P set, so it is *not* not-present.
        assert!(!is_not_present(PF_ERR_PRESENT));
    }

    #[test]
    fn only_user_not_present_read_data_faults_are_resolvable() {
        // The demand-paged file-mapping shape: ring 3, not-present, read,
        // data access.
        assert!(is_resolvable_user_fault(PF_ERR_USER));
        // A kernel-mode fault is never offered.
        assert!(!is_resolvable_user_fault(0));
        // A protection violation cannot be fixed by residency.
        assert!(!is_resolvable_user_fault(PF_ERR_USER | PF_ERR_PRESENT));
        // A write to a read-only file mapping must kill the task, not
        // retry forever against a resident page.
        assert!(!is_resolvable_user_fault(PF_ERR_USER | PF_ERR_WRITE));
        // An instruction fetch is never file backing.
        assert!(!is_resolvable_user_fault(PF_ERR_USER | PF_ERR_INSTR));
    }

    #[test]
    fn user_data_faults_are_offered_reads_and_writes_alike() {
        // Regression (the M1 file-map vertical's `store` role): the offer
        // gate admits ring-3 reads *and* writes — a write is offered so
        // the resolver kills the faulting task; before this gate existed a
        // user store to a read-only mapping fell to the fatal path and
        // could halt the whole CPU.
        assert!(is_user_data_fault(PF_ERR_USER));
        assert!(is_user_data_fault(PF_ERR_USER | PF_ERR_WRITE));
        assert!(is_user_data_fault(
            PF_ERR_USER | PF_ERR_PRESENT | PF_ERR_WRITE
        ));
        // Kernel-mode faults and instruction fetches are never offered.
        assert!(!is_user_data_fault(0));
        assert!(!is_user_data_fault(PF_ERR_WRITE));
        assert!(!is_user_data_fault(PF_ERR_USER | PF_ERR_INSTR));
    }

    extern "C" fn host_user_fault_resolver(
        _faulting_addr: u64,
        _write: bool,
        _regs: *const UserRegisterFrame,
    ) -> bool {
        false
    }

    #[test]
    fn user_fault_resolver_slot_is_set_once_and_round_trips() {
        clear_user_fault_resolver_for_tests();
        assert!(user_fault_resolver().is_none());
        set_user_fault_resolver(host_user_fault_resolver).expect("first install");
        assert_eq!(
            user_fault_resolver().map(|f| f as usize),
            Some(host_user_fault_resolver as UserFaultResolveFn as usize)
        );
        assert_eq!(
            set_user_fault_resolver(host_user_fault_resolver),
            Err(SetFaultHandlerError::AlreadyInstalled)
        );
        clear_user_fault_resolver_for_tests();
    }

    extern "C" fn host_user_fault_terminator(
        _fault_pc: u64,
        _regs: *const UserRegisterFrame,
    ) -> bool {
        false
    }

    #[test]
    fn user_fault_terminator_slot_is_set_once_and_round_trips() {
        clear_user_fault_terminator_for_tests();
        assert!(user_fault_terminator().is_none());
        set_user_fault_terminator(host_user_fault_terminator).expect("first install");
        assert_eq!(
            user_fault_terminator().map(|f| f as usize),
            Some(host_user_fault_terminator as UserFaultTerminateFn as usize)
        );
        assert_eq!(
            set_user_fault_terminator(host_user_fault_terminator),
            Err(SetFaultHandlerError::AlreadyInstalled)
        );
        clear_user_fault_terminator_for_tests();
    }

    /// The terminator, not the resolver, owns a ring-3 instruction-fetch
    /// `#PF`: a wild jump is never file backing, so offering it to the
    /// resolver would leave it on the fatal path and park the CPU for one
    /// task's mistake.
    #[test]
    fn an_instruction_fetch_is_a_user_fault_the_resolver_never_sees() {
        let wild_jump = PF_ERR_USER | PF_ERR_INSTR;
        assert!(is_user(wild_jump));
        assert!(!is_user_data_fault(wild_jump));
        assert!(!is_resolvable_user_fault(wild_jump));
        // Kernel-mode instruction fetches stay the kernel's own.
        assert!(!is_user(PF_ERR_INSTR));
    }

    #[test]
    fn user_and_write_decode_independently() {
        assert!(is_user(PF_ERR_USER));
        assert!(!is_user(PF_ERR_WRITE));
        assert!(is_write(PF_ERR_WRITE));
        assert!(!is_write(PF_ERR_USER));
        // A user-mode not-present write sets both U/S and W.
        let code = PF_ERR_USER | PF_ERR_WRITE;
        assert!(is_user(code) && is_write(code) && is_not_present(code));
    }

    extern "C" fn host_fault_handler(_error_code: u64, _faulting_addr: u64, _rip: u64) -> ! {
        panic!("host test handler must never be invoked");
    }

    // Both the set-once and the round-trip assertions mutate the single
    // process-wide `FAULT_HANDLER` slot, so they live in one test: cargo
    // runs `#[test]`s in parallel threads and two of them clearing and
    // reinstalling the same static would race (no flaky
    // tests).
    #[test]
    fn slot_is_set_once_and_round_trips() {
        clear_fault_handler_for_tests();
        assert!(fault_handler().is_none());

        set_fault_handler(host_fault_handler).expect("first install");
        let got = fault_handler().expect("handler present");
        assert_eq!(
            got as *const () as usize,
            host_fault_handler as *const () as usize
        );

        assert_eq!(
            set_fault_handler(host_fault_handler),
            Err(SetFaultHandlerError::AlreadyInstalled)
        );
        clear_fault_handler_for_tests();
    }
}
