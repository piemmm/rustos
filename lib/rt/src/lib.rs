//! `rustos-rt` — the pure-Rust userland runtime.
//!
//! This is the runtime a **first-party RustOS program written in Rust** links:
//! it provides the program's `_start` entry trampoline, idiomatic `abi-v1`
//! syscall wrappers, the [`entry!`] macro that names the program's `main`, and
//! the panic handler. RustOS is Rust-only (`AGENTS.md` §1), so its own
//! programs use this runtime and never the C ABI.
//!
//! # Relationship to the C ABI (`crt0` + `abi-sys`)
//!
//! `rustos-crt0` and `rustos-abi-sys` are the curated *System runtime / C ABI*
//! class (`AGENTS.md` §9, §16.4): a libc-equivalent that exists **solely** so
//! a program **not** written in Rust (C, …) can call `abi-v1`. They are not
//! for RustOS's own code. `rustos-rt` is the Rust counterpart; both build on
//! the one shared syscall trap (`rustos-abi-trap`, `AGENTS.md` §2.2), so the
//! trap assembly is not duplicated.
//!
//! # Not a privileged path
//!
//! The wrappers add **no** authority. Every capability check and input
//! validation happens kernel-side, on the far side of the trap (`AGENTS.md`
//! §5.4); a Rust program reaches no syscall it could not reach otherwise.
//!
//! # Using it
//!
//! A program is `#![no_std]`, `#![no_main]`, declares its `main`, and hands it
//! to [`entry!`]:
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! fn main() -> i32 {
//!     rustos_rt::stream_write(b"hello\n");
//!     0
//! }
//!
//! rustos_rt::entry!(main);
//! ```
//!
//! `rustos-rt` provides `_start`, which validates the kernel-supplied
//! startup vector, installs the per-process stack canary (`AGENTS.md` §19.2),
//! calls `main`, and routes its return value through the `exit` syscall.
//!
//! # Targets
//!
//! The `_start` trampoline, stack-canary symbols, and panic handler are
//! compiled in only for the three native Tier-1 targets, gated on a
//! build-script-emitted `rt_native_<arch>` cfg (`build.rs`) rather than a
//! target-architecture predicate, so the instruction-set choice stays out of
//! the source tree the §17.2 `cfg-check` guards. On the host only the
//! host-testable syscall-wrapper marshalling is compiled.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::{SyscallNumber, STDERR, STDIN, STDINFO, STDOUT};
use rustos_abi_trap::raw_syscall;

#[cfg(rt_native)]
mod start;

/// `exit` syscall number, read from the `abi-v1` source of truth so this
/// crate can never disagree with the table (`AGENTS.md` §2.2).
const NUM_EXIT: u64 = SyscallNumber::EXIT.as_u16() as u64;

/// `stream_write` syscall number (`AGENTS.md` §2.2, as above).
const NUM_STREAM_WRITE: u64 = SyscallNumber::STREAM_WRITE.as_u16() as u64;

/// `stream_read` syscall number (`AGENTS.md` §2.2, as above).
const NUM_STREAM_READ: u64 = SyscallNumber::STREAM_READ.as_u16() as u64;

/// `yield` syscall number (`AGENTS.md` §2.2, as above).
const NUM_YIELD: u64 = SyscallNumber::YIELD.as_u16() as u64;

/// `spawn` syscall number (`AGENTS.md` §2.2, as above).
const NUM_SPAWN: u64 = SyscallNumber::SPAWN.as_u16() as u64;

/// Marshal a 32-bit signed argument into its register value following the
/// `abi-v1` `I32` convention (sign-extend through `i64`).
#[inline]
#[allow(clippy::cast_sign_loss)] // Reinterpreting the sign-extended bit pattern is the documented I32 convention.
const fn i32_arg(value: i32) -> u64 {
    value as i64 as u64
}

/// Terminate the calling process with exit code `code` (`SyscallNumber::EXIT`).
///
/// This never returns. A correct kernel never returns control from `exit`;
/// should it nonetheless do so, this must not return to a caller that has no
/// continuation, so it re-issues `exit`. This is a fail-closed loop over the
/// terminating syscall, not a busy-wait (`AGENTS.md` §2.1).
pub fn exit(code: i32) -> ! {
    loop {
        // SAFETY: `raw_syscall` is always safe to invoke — the kernel
        // validates the call on the far side of the trap (`AGENTS.md` §5.4).
        // `exit` consumes the exit code in arg 0 and takes no memory operand.
        unsafe {
            let _ = raw_syscall(NUM_EXIT, [i32_arg(code), 0, 0, 0, 0, 0]);
        }
    }
}

/// Write `bytes` to the calling process's standard stream `fd`
/// (`SyscallNumber::STREAM_WRITE`), returning the number of bytes the
/// kernel accepted (`AGENTS.md` §20).
///
/// The shared core of [`stdout`], [`stderr`], and [`stdinfo`]: the
/// program names only the inherited descriptor, never a device, so the
/// same binary works whatever the spawner backed the stream with (§20 —
/// device independence is a property of the stream layer). The kernel
/// resolves `fd` against the caller's descriptor table and validates the
/// `(buf, len)` pair against the caller's address space before reading it
/// (`AGENTS.md` §5.4); a short write (fewer than `bytes.len()`) is valid,
/// so the caller loops.
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the count never exceeds `bytes.len()`.
fn stream_write(fd: u32, bytes: &[u8]) -> usize {
    let ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(buf, len)` against the caller's address space before touching it
    // (`AGENTS.md` §5.4). `bytes` is a live shared `&[u8]` for the duration
    // of the call, so the `(ptr, len)` pair denotes readable memory.
    let written = unsafe {
        raw_syscall(
            NUM_STREAM_WRITE,
            [u64::from(fd), ptr, bytes.len() as u64, 0, 0, 0],
        )
    };
    written as usize
}

/// Write `bytes` to standard output (fd 1, `AGENTS.md` §20), returning the
/// number of bytes the kernel accepted. The program's primary data
/// output; a short write is valid, so the caller loops.
#[must_use]
pub fn stdout(bytes: &[u8]) -> usize {
    stream_write(STDOUT, bytes)
}

/// Write `bytes` to standard error (fd 2, `AGENTS.md` §20): errors,
/// warnings, and diagnostics. Returns the number of bytes accepted.
#[must_use]
pub fn stderr(bytes: &[u8]) -> usize {
    stream_write(STDERR, bytes)
}

/// Write `bytes` to the standard information stream (fd 3, `AGENTS.md`
/// §20.1): optional, ignorable structured advisory metadata. Returns the
/// number of bytes accepted (zero when no consumer is attached — fd 3 is
/// best-effort and must never affect correctness).
#[must_use]
pub fn stdinfo(bytes: &[u8]) -> usize {
    stream_write(STDINFO, bytes)
}

/// Read up to `buf.len()` bytes from standard input (fd 0, `AGENTS.md`
/// §20) into `buf` (`SyscallNumber::STREAM_READ`), returning the number of
/// bytes read.
///
/// The kernel resolves fd 0 against the caller's descriptor table and
/// validates the `(buf, len)` pair against the caller's address space
/// before writing it (`AGENTS.md` §5.4). A short read (fewer bytes than
/// `buf.len()`, possibly zero when no input is pending) is valid, so the
/// caller loops.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the count never exceeds `buf.len()`.
pub fn stdin(buf: &mut [u8]) -> usize {
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(buf, len)` against the caller's address space before touching it
    // (`AGENTS.md` §5.4). `buf` is a live exclusive `&mut [u8]` for the
    // duration of the call, so the `(ptr, len)` pair denotes writable
    // memory the kernel may fill.
    let read = unsafe { raw_syscall(NUM_STREAM_READ, [u64::from(STDIN), ptr, len, 0, 0, 0]) };
    read as usize
}

/// Yield the calling task's CPU back to the scheduler (`SyscallNumber::YIELD`).
///
/// A cooperative reschedule point: the kernel suspends the caller, runs
/// another runnable task, and returns here when the caller is next
/// dispatched. It requires no capability, takes no arguments, and returns
/// nothing (`abi-v1` `yield` is `() -> ()`). A program that must let a
/// sibling run — without a blocking syscall to wait on — calls this rather
/// than spinning (`AGENTS.md` §2.1).
pub fn yield_now() {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap (`AGENTS.md` §5.4). `yield` takes
    // no arguments and no memory operand, so all six argument registers are
    // zero; the kernel ignores its return value.
    unsafe {
        let _ = raw_syscall(NUM_YIELD, [0, 0, 0, 0, 0, 0]);
    }
}

/// Spawn the embedded program registered under the absolute `path` as a
/// new, concurrently runnable process, returning its PID
/// (`SyscallNumber::SPAWN`, `plans/SPAWN.md` SP3).
///
/// Requires `CAP_PROC_SPAWN`; the kernel validates the capability and the
/// `(path, len)` pair against the caller's address space before reading it,
/// resolves the path against the kernel's embedded-program registry, builds
/// the child a fresh hardware-isolated address space, and admits it
/// **Ready** — the caller keeps running (a true concurrent spawn, not an
/// `exec`-style hand-off, `AGENTS.md` §4 / §5.4).
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: a non-negative value is the new PID, and a
/// negative value is `-errno` (recover the [`rustos_abi::Errno`]
/// discriminant as `-ret`). The wrapper surfaces that raw signed value so
/// the caller decides how to react to a failed spawn — it adds no authority
/// and hides no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 spawn-result encoding (PID ≥ 0, else -errno).
pub fn spawn(path: &[u8]) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(path, len)` against the caller's address space before touching it
    // (`AGENTS.md` §5.4). `path` is a live shared `&[u8]` for the duration
    // of the call, so the `(ptr, len)` pair denotes readable memory.
    let ret = unsafe { raw_syscall(NUM_SPAWN, [ptr, path.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Define the program's entry point.
///
/// `$entry` must be a `fn() -> i32`; the macro exports the runtime's
/// `__rustos_rt_main` symbol (which `_start` calls) so it invokes `$entry` and
/// hands its return value to the runtime, which routes it through `exit`.
/// Invoke it exactly once, at the crate root of a `#![no_main]` program.
#[macro_export]
macro_rules! entry {
    ($entry:path) => {
        // `#[no_mangle]` exports the fixed symbol `_start` resolves; the item
        // is private (no `pub`) so it needs no rustdoc and exports nothing to
        // the program's own namespace beyond the symbol the runtime links.
        #[no_mangle]
        fn __rustos_rt_main() -> i32 {
            // Bind through a `fn() -> i32` so a mis-typed entry is a clear
            // compile error here rather than a link-time mismatch.
            let entry: fn() -> i32 = $entry;
            entry()
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    // The trap seam lives in `rustos-abi-trap` (the single trap home,
    // `AGENTS.md` §2.2) and is reached here through the `host-seam`
    // dev-dependency feature; production builds never compile it.
    use rustos_abi::SYSCALL_MAX_ARGS;
    use rustos_abi_trap::seam;

    /// Run `call` with the seam armed to return `ret`, returning the recorded
    /// `(number, args)`.
    fn capture(ret: u64, call: impl FnOnce()) -> (u64, [u64; SYSCALL_MAX_ARGS]) {
        seam::arm(ret);
        call();
        seam::last_call().expect("the wrapper must issue exactly one trap")
    }

    #[test]
    fn stdout_marshals_fd_pointer_and_len() {
        let buffer = *b"hello\n";
        let (number, args) = capture(6, || {
            assert_eq!(stdout(&buffer), 6);
        });
        assert_eq!(number, NUM_STREAM_WRITE);
        assert_eq!(args[0], u64::from(STDOUT));
        assert_eq!(args[1], buffer.as_ptr() as usize as u64);
        assert_eq!(args[2], 6);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn stderr_and_stdinfo_marshal_their_fd() {
        let buffer = *b"warn\n";
        let (number, args) = capture(5, || {
            assert_eq!(stderr(&buffer), 5);
        });
        assert_eq!(number, NUM_STREAM_WRITE);
        assert_eq!(args[0], u64::from(STDERR));
        let (number, args) = capture(0, || {
            // fd 3 is best-effort: a zero return (no consumer) is valid.
            assert_eq!(stdinfo(&buffer), 0);
        });
        assert_eq!(number, NUM_STREAM_WRITE);
        assert_eq!(args[0], u64::from(STDINFO));
    }

    #[test]
    fn stdout_returns_the_kernel_accepted_count() {
        let buffer = [0u8; 16];
        let (_, _) = capture(10, || {
            assert_eq!(stdout(&buffer), 10);
        });
    }

    #[test]
    fn stdin_marshals_fd_pointer_and_len() {
        let mut buffer = [0u8; 16];
        let ptr = buffer.as_mut_ptr() as usize as u64;
        let (number, args) = capture(7, || {
            assert_eq!(stdin(&mut buffer), 7);
        });
        assert_eq!(number, NUM_STREAM_READ);
        assert_eq!(args[0], u64::from(STDIN));
        assert_eq!(args[1], ptr);
        assert_eq!(args[2], 16);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn stdin_returns_the_kernel_reported_count() {
        let mut buffer = [0u8; 16];
        let (_, _) = capture(3, || {
            assert_eq!(stdin(&mut buffer), 3);
        });
    }

    #[test]
    fn spawn_marshals_path_pointer_and_len() {
        let path = *b"/Apps/Shell.app/Run";
        let (number, args) = capture(7, || {
            assert_eq!(spawn(&path), 7);
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn spawn_surfaces_negative_errno_encoding() {
        // `NotFound` (7) is encoded by the kernel as the two's-complement
        // negation; the wrapper hands that signed value back unchanged. The
        // register carries the raw bit pattern, so reinterpret rather than
        // sign-loss-cast it.
        let want = -i64::from(rustos_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(spawn(b"/nope"), want);
        });
    }

    #[test]
    fn yield_now_issues_the_yield_syscall_with_no_arguments() {
        let (number, args) = capture(0, yield_now);
        assert_eq!(number, NUM_YIELD);
        assert_eq!(&args, &[0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn i32_arg_sign_extends() {
        assert_eq!(i32_arg(0), 0);
        assert_eq!(i32_arg(1), 1);
        assert_eq!(i32_arg(-1), u64::MAX);
        assert_eq!(i32_arg(i32::MIN), 0xFFFF_FFFF_8000_0000);
    }
}
