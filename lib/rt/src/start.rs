//! The per-architecture program entry trampoline (`_start`) — the
//! assembly carve-out — the Rust startup driver it calls, the stack-protector
//! symbols, and the panic handler.
//!
//! # Why this module contains assembly (justification)
//!
//! TAIRiX is Rust-only; assembly is permitted only where the architecture
//! *strictly* requires it. A program's entry point is exactly such a case:
//! when the kernel transfers control to `_start` there is no runtime yet —
//! the stack-pointer alignment a `call` boundary demands, and reading the
//! kernel-supplied startup-vector pointer out of the entry register, have no
//! Rust spelling. Each `_start` here does the minimum: align the stack, place
//! the startup-vector pointer in the integer-argument-0 register, and `call`
//! [`rust_rt_start`], which performs the rest in plain Rust.
//!
//! This is the Rust counterpart of `lib/crt0/src/start.rs`. Both reach the
//! kernel through the one shared trap (`tairix-abi-trap`); crt0 marshals the
//! startup vector into a C `argc`/`argv`/`envp` and calls a C `main`, whereas
//! this runtime calls the program's Rust `main` (named through
//! [`crate::entry!`]). TAIRiX's own programs use this runtime; crt0 exists
//! only for non-Rust programs.
//!
//! Each `_start` is gated on a build-script-emitted `rt_native_<arch>` cfg
//! (see `build.rs`) rather than a target-architecture predicate, so the
//! instruction-set choice stays out of the source tree the `cfg-check`
//! guards (mirroring `lib/crt0` and `lib/abi-trap`).
//!
//! # Kernel → `_start` contract (`abi-v1`)
//!
//! The kernel transfers control to the program's `_start` with the C
//! integer-argument-0 register holding the base address of the
//! position-independent startup-vector block ([`tairix_abi::process`]) and a
//! valid, writable stack (the register is `rdi` on x86_64, `x0` on aarch64,
//! `a0` on riscv64 — the first integer argument on each platform's C ABI).

use core::cell::UnsafeCell;
use core::panic::PanicInfo;

use tairix_abi::process::{ProcessStart, ProcessStartHeader};
use tairix_abi::Errno;

use crate::exit;

/// Exit code the runtime terminates with when the kernel-supplied startup
/// vector cannot be validated. A non-zero, reserved code (a fail-closed
/// teardown) distinct from any value a well-behaved `main`
/// is likely to return.
const EXIT_BAD_STARTUP: i32 = 70;

// The program's entry point, exported by `crate::entry!` and resolved at link
// time. The runtime calls it once the startup vector is validated and routes
// its return value through `exit`.
extern "Rust" {
    fn __tairix_rt_main() -> i32;
}

/// Storage for the program's stack-protector guard.
///
/// This is the one global the stack-canary scheme inherently requires:
/// the compiler-inserted function prologue/epilogue of stack-protected code
/// reads the guard word from a fixed, well-known symbol (`__stack_chk_guard`,
/// the platform C-ABI convention). The runtime seeds it once, before any
/// program code runs, with the per-process random value the kernel placed in
/// the startup vector (entropy). It is wrapped in an
/// [`UnsafeCell`] rather than declared `static mut` so the write goes through
/// a single audited path with no aliasing `&mut`.
struct StackGuard(UnsafeCell<usize>);

// SAFETY: the guard is written exactly once, by `install_stack_canary`,
// before the program's `main` runs and before any thread other than the
// initial one can exist; thereafter it is only read by the compiler's
// stack-check epilogues. There is no concurrent access to synchronise.
unsafe impl Sync for StackGuard {}

#[no_mangle]
static __stack_chk_guard: StackGuard = StackGuard(UnsafeCell::new(0));

/// Seed the program's stack-protector guard with the kernel-supplied
/// per-process canary.
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the low bits are the conventional guard layout.
fn install_stack_canary(canary: u64) {
    let value = canary as usize;
    // SAFETY: see the `Sync` impl on `StackGuard`. This is the sole writer,
    // running single-threaded before any stack-protected program code, so the
    // write cannot race a read.
    unsafe {
        core::ptr::write(__stack_chk_guard.0.get(), value);
    }
}

/// Called by the compiler-inserted epilogue when a stack canary check fails.
///
/// A detected stack-buffer overflow is unrecoverable; the runtime states the
/// reason on stderr (fail loud — a silent abnormal exit tells the user
/// nothing) and terminates the program through the `exit` syscall with a
/// reserved non-zero code rather than returning to corrupted state (fail
/// closed).
#[no_mangle]
extern "C" fn __stack_chk_fail() -> ! {
    write_stderr_best_effort(b"fatal: stack smashing detected\n");
    exit(EXIT_BAD_STARTUP)
}

/// Exit code the runtime terminates with on a panic — the same `101` the
/// hosted Rust runtime uses, distinct from the startup-validation failure.
const EXIT_PANIC: i32 = 101;

/// The largest panic report written to stderr, in bytes. A fixed bound keeps
/// the panic path allocation-free (the panic may itself be an out-of-memory
/// condition); a longer report is truncated, never lost.
const PANIC_REPORT_BYTES: usize = 512;

/// Write `bytes` to standard error, best-effort: a program dying abnormally
/// may have no attached stderr consumer, and the report must never turn the
/// teardown into a second failure.
fn write_stderr_best_effort(bytes: &[u8]) {
    let mut err = crate::io::Stderr;
    let _ = crate::io::Write::write_all(&mut err, bytes);
}

/// Panic handler: a hosted program has no unwinder, so a panic is an
/// unrecoverable fault. The runtime states the panic's message and location
/// on stderr first — an abnormal termination must say why, a silent non-zero
/// exit is a defect — through a fixed stack buffer ([`FixedFmtBuf`]: the
/// panic may be an out-of-memory condition, so this path never allocates),
/// then terminates through the `exit` syscall rather than returning to
/// corrupt state (fail closed). Programs are written to be panic-free; this
/// satisfies the `no_std` contract once and for all rt programs, so none
/// repeats it.
///
/// [`FixedFmtBuf`]: crate::io::FixedFmtBuf
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    use core::fmt::Write as _;
    let mut report = crate::io::FixedFmtBuf::<PANIC_REPORT_BYTES>::new();
    // `PanicInfo`'s `Display` prints "panicked at <location>:\n<message>" —
    // everything the report needs, within the workspace MSRV.
    let _ = write!(report, "{info}");
    let _ = report.write_str("\n");
    write_stderr_best_effort(report.as_bytes());
    exit(EXIT_PANIC)
}

/// Read the declared total startup-vector length from its header, so the
/// driver can size the full block slice before validating it.
fn read_total_len(header_bytes: &[u8]) -> Result<usize, Errno> {
    let header = ProcessStartHeader::from_bytes(header_bytes)?;
    usize::try_from(header.total_len).map_err(|_| Errno::LengthOutOfRange)
}

/// The Rust half of the runtime, called by `_start` once the stack is aligned.
///
/// `block_ptr` is the kernel-supplied startup-vector base. It validates the
/// startup vector, installs the stack canary, calls the program's `main`, and
/// routes its return through `exit`. It never returns (`exit` diverges); a
/// failure to validate the startup vector exits with [`EXIT_BAD_STARTUP`].
///
/// # Safety
///
/// `block_ptr` must point at a readable startup-vector block of at least its
/// declared `total_len` bytes; `_start` upholds this from the kernel →
/// `_start` contract.
#[no_mangle]
unsafe extern "C" fn rust_rt_start(block_ptr: *const u8) -> ! {
    // SAFETY: the caller guarantees `block_ptr` is readable for at least a
    // header; we read the declared length before forming the full slice.
    let header = unsafe { core::slice::from_raw_parts(block_ptr, ProcessStartHeader::WIRE_LEN) };
    let Ok(total_len) = read_total_len(header) else {
        exit(EXIT_BAD_STARTUP);
    };

    // SAFETY: `read_total_len` validated the header; the contract guarantees
    // the whole `total_len`-byte block is readable. The block lives in the
    // process's own image for the lifetime of the process (the kernel maps
    // it before `_start` and never unmaps it), so the `'static` borrow is
    // sound.
    let block: &'static [u8] = unsafe { core::slice::from_raw_parts(block_ptr, total_len) };
    let Ok(view) = ProcessStart::parse(block) else {
        exit(EXIT_BAD_STARTUP);
    };

    install_stack_canary(view.canary());
    // Publish the validated view so the program can read the arguments its
    // spawner chose for it (`crate::startup`).
    crate::startup::install(view);

    // SAFETY: `__tairix_rt_main` is the program's entry point, exported with a
    // matching Rust signature by `crate::entry!` and resolved at link time.
    let code = unsafe { __tairix_rt_main() };
    exit(code)
}

#[cfg(rt_native_x86_64)]
core::arch::global_asm!(
    // SysV AMD64: startup-vector pointer arrives in `rdi` (arg 0) and is
    // already where the Rust driver expects it. Align the stack to 16, then
    // call the driver. `_start` never returns, so `ud2` guards a buggy kernel
    // that resumes here.
    ".global _start",
    "_start:",
    "and rsp, -16",
    "call {entry}",
    "ud2",
    entry = sym rust_rt_start,
);

#[cfg(rt_native_aarch64)]
core::arch::global_asm!(
    // AAPCS64: startup-vector pointer arrives in `x0` (arg 0), already where
    // the Rust driver expects it. Align the stack to 16, then call the
    // driver. `brk #0` guards a resume that must never happen.
    ".global _start",
    "_start:",
    "mov x9, sp",
    "and x9, x9, #-16",
    "mov sp, x9",
    "bl {entry}",
    "brk #0",
    entry = sym rust_rt_start,
);

#[cfg(rt_native_riscv64)]
core::arch::global_asm!(
    // RISC-V LP64: startup-vector pointer arrives in `a0` (arg 0), already
    // where the Rust driver expects it. Align the stack to 16, then call the
    // driver. `ebreak` guards a resume that must never happen.
    ".global _start",
    "_start:",
    "andi sp, sp, -16",
    "call {entry}",
    "ebreak",
    entry = sym rust_rt_start,
);
