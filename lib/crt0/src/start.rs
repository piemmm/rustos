//! The per-architecture program entry trampoline (`_start`) — the
//! assembly carve-out — and the Rust startup driver it calls.
//!
//! # Why this module contains assembly (justification)
//!
//! RustOS is Rust-only; assembly is permitted only where the architecture
//! *strictly* requires it. A program's entry point is exactly such a case:
//! when the kernel transfers control to `_start` there is no C runtime yet —
//! the stack-pointer alignment the platform C ABI demands at a `call`
//! boundary, and reading the kernel-supplied startup-vector pointer out of
//! the entry register, have no Rust spelling. Each `_start` here does the
//! minimum: align the stack, carve a fixed scratch region from it, place the
//! startup-vector pointer and the scratch span in the C argument registers,
//! and `call` [`rust_crt0_start`], which performs the rest in plain Rust.
//!
//! Each `_start` is gated on a build-script-emitted `crt0_native_<arch>` cfg
//! (see `build.rs`) rather than a target-architecture predicate, so the
//! instruction-set choice stays out of the source tree the `cfg-check`
//! guards (mirroring `lib/abi-sys`).
//!
//! # Kernel → `_start` contract (`abi-v1`)
//!
//! The kernel transfers control to the program's `_start` with the C
//! integer-argument-0 register holding the base address of the
//! position-independent startup-vector block ([`rustos_abi::process`]) and a
//! valid, writable stack (the register is `rdi` on x86_64, `x0` on aarch64,
//! `a0` on riscv64 — the first integer argument on each platform's C ABI).

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int};

use rustos_abi::process::ProcessStartHeader;

use crate::{build_c_runtime, read_total_len};

/// Bytes of stack the `_start` trampoline carves as scratch for the C
/// runtime layout (`argv` / `envp` arrays plus NUL-terminated string copies).
///
/// Bounded and fixed: a hostile or buggy spawner cannot make crt0 reserve an
/// unbounded amount of stack, and a startup vector that does not fit fails
/// closed (`build_c_runtime` returns [`rustos_abi::Errno::BufferTooSmall`],
/// which [`rust_crt0_start`] turns into a non-zero exit). 1 MiB comfortably
/// holds any realistic command line while staying far below the smallest
/// program stack the loader provisions.
pub const STARTUP_SCRATCH_LEN: usize = 1 << 20;

/// Exit code crt0 terminates the program with when the kernel-supplied
/// startup vector cannot be validated or laid out. A non-zero, reserved code
/// (a fail-closed teardown) distinct from any value a
/// well-behaved `main` is likely to return.
pub const EXIT_BAD_STARTUP: c_int = 70;

// The hosted program's entry point, resolved at link time. A C program
// supplies `int main(int argc, char **argv, char **envp)`; crt0 calls it once
// the C runtime is set up and routes its return through `exit`.
extern "C" {
    fn main(argc: c_int, argv: *const *const c_char, envp: *const *const c_char) -> c_int;
}

/// Storage for the program's stack-protector guard.
///
/// This is the one global the stack-canary scheme inherently requires:
/// the compiler-inserted function prologue/epilogue of stack-protected code
/// reads the guard word from a fixed, well-known symbol (`__stack_chk_guard`,
/// the platform C-ABI convention). crt0 seeds it once, before any hosted
/// code runs, with the per-process random value the kernel placed in the
/// startup vector (entropy). It is wrapped in an
/// [`UnsafeCell`] rather than declared `static mut` so the write goes through
/// a single audited path with no aliasing `&mut`.
struct StackGuard(UnsafeCell<usize>);

// SAFETY: the guard is written exactly once, by `install_stack_canary`,
// before the hosted program's `main` runs and before any thread other than
// the initial one can exist; thereafter it is only read by the compiler's
// stack-check epilogues. There is no concurrent access to synchronise.
unsafe impl Sync for StackGuard {}

#[no_mangle]
static __stack_chk_guard: StackGuard = StackGuard(UnsafeCell::new(0));

/// Seed the program's stack-protector guard with the kernel-supplied
/// per-process canary.
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the low bits are the conventional guard layout.
fn install_stack_canary(canary: u64) {
    // The guard word is pointer-width; the canary is 64-bit. On the 64-bit
    // native targets they are the same width. Truncating to `usize` keeps the
    // low entropy bits, which is the conventional layout.
    let value = canary as usize;
    // SAFETY: see the `Sync` impl on `StackGuard`. This is the sole writer,
    // running single-threaded before any stack-protected hosted code, so the
    // write cannot race a read.
    unsafe {
        core::ptr::write(__stack_chk_guard.0.get(), value);
    }
}

/// Called by the compiler-inserted epilogue when a stack canary check fails.
///
/// A detected stack-buffer overflow is unrecoverable; crt0 terminates the
/// program through the `exit` syscall with a reserved non-zero code rather
/// than returning to corrupted state (fail closed).
#[no_mangle]
extern "C" fn __stack_chk_fail() -> ! {
    rustos_abi_sys::sys_exit(EXIT_BAD_STARTUP)
}

/// The Rust half of crt0, called by `_start` once the stack is aligned and a
/// scratch region carved.
///
/// `block_ptr` is the kernel-supplied startup-vector base; `scratch` /
/// `scratch_len` describe the stack region `_start` reserved. It validates
/// the startup vector, builds the C `argv` / `envp`, installs the stack
/// canary, calls the hosted `main`, and routes its return through `exit`.
/// It never returns (`exit` diverges); a failure to validate the startup
/// vector exits with [`EXIT_BAD_STARTUP`].
///
/// # Safety
///
/// `block_ptr` must point at a readable startup-vector block of at least its
/// declared `total_len` bytes, and `scratch` must point at `scratch_len`
/// writable bytes; `_start` upholds both from the kernel → `_start` contract.
#[no_mangle]
unsafe extern "C" fn rust_crt0_start(
    block_ptr: *const u8,
    scratch: *mut u8,
    scratch_len: usize,
) -> ! {
    // SAFETY: the caller guarantees `block_ptr` is readable for at least a
    // header; we read the declared length before forming the full slice.
    let header = unsafe { core::slice::from_raw_parts(block_ptr, ProcessStartHeader::WIRE_LEN) };
    let Ok(total_len) = read_total_len(header) else {
        rustos_abi_sys::sys_exit(EXIT_BAD_STARTUP);
    };

    // SAFETY: `read_total_len` validated the header; the contract guarantees
    // the whole `total_len`-byte block is readable. `scratch` is valid for
    // `scratch_len` bytes by the contract.
    let block = unsafe { core::slice::from_raw_parts(block_ptr, total_len) };
    let scratch = unsafe { core::slice::from_raw_parts_mut(scratch, scratch_len) };

    let Ok(runtime) = build_c_runtime(block, scratch) else {
        rustos_abi_sys::sys_exit(EXIT_BAD_STARTUP);
    };

    install_stack_canary(runtime.canary);

    // SAFETY: `runtime.argv` / `runtime.envp` are NULL-terminated C vectors
    // laid out in `scratch`, which outlives this call; `main` is the hosted
    // program's entry point resolved at link time.
    let code = unsafe { main(runtime.argc, runtime.argv, runtime.envp) };
    rustos_abi_sys::sys_exit(code)
}

#[cfg(crt0_native_x86_64)]
core::arch::global_asm!(
    // SysV AMD64: startup-vector pointer arrives in `rdi` (arg 0). Align the
    // stack to 16, carve the scratch span, pass its base in `rsi` (arg 1) and
    // its length in `rdx` (arg 2), then call the Rust driver. `_start` never
    // returns, so a trap (`ud2`) guards a buggy kernel that resumes here.
    ".global _start",
    "_start:",
    "and rsp, -16",
    "sub rsp, {scratch}",
    "mov rsi, rsp",
    "mov rdx, {scratch}",
    "call {entry}",
    "ud2",
    scratch = const STARTUP_SCRATCH_LEN,
    entry = sym rust_crt0_start,
);

#[cfg(crt0_native_aarch64)]
core::arch::global_asm!(
    // AAPCS64: startup-vector pointer arrives in `x0` (arg 0). Align the
    // stack to 16, carve the scratch span, pass its base in `x1` (arg 1) and
    // its length in `x2` (arg 2), then call the Rust driver. `brk #0` guards
    // a resume that must never happen.
    ".global _start",
    "_start:",
    "mov x9, sp",
    "and x9, x9, #-16",
    "sub x9, x9, #{scratch}",
    "mov sp, x9",
    "mov x1, sp",
    "mov x2, #{scratch}",
    "bl {entry}",
    "brk #0",
    scratch = const STARTUP_SCRATCH_LEN,
    entry = sym rust_crt0_start,
);

#[cfg(crt0_native_riscv64)]
core::arch::global_asm!(
    // RISC-V LP64: startup-vector pointer arrives in `a0` (arg 0). Align the
    // stack to 16, carve the scratch span, pass its base in `a1` (arg 1) and
    // its length in `a2` (arg 2), then call the Rust driver. `ebreak` guards
    // a resume that must never happen.
    ".global _start",
    "_start:",
    "andi sp, sp, -16",
    "li t0, {scratch}",
    "sub sp, sp, t0",
    "mv a1, sp",
    "mv a2, t0",
    "call {entry}",
    "ebreak",
    scratch = const STARTUP_SCRATCH_LEN,
    entry = sym rust_crt0_start,
);
