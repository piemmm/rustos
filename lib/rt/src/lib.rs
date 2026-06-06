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
//!     rustos_rt::console_write(b"hello\n");
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

use rustos_abi::SyscallNumber;
use rustos_abi_trap::raw_syscall;

#[cfg(rt_native)]
mod start;

/// `exit` syscall number, read from the `abi-v1` source of truth so this
/// crate can never disagree with the table (`AGENTS.md` §2.2).
const NUM_EXIT: u64 = SyscallNumber::EXIT.as_u16() as u64;

/// `console_write` syscall number (`AGENTS.md` §2.2, as above).
const NUM_CONSOLE_WRITE: u64 = SyscallNumber::CONSOLE_WRITE.as_u16() as u64;

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

/// Write `bytes` to the system console (`SyscallNumber::CONSOLE_WRITE`),
/// returning the number of bytes the kernel accepted.
///
/// Requires `CAP_CONSOLE_WRITE`; the kernel validates the capability and the
/// `(buf, len)` pair against the caller's address space before reading it
/// (`AGENTS.md` §5.4). This is the privileged hardware console, not a
/// per-process stdout (`plans/PI.md` §P6).
#[must_use]
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the count never exceeds `bytes.len()`.
pub fn console_write(bytes: &[u8]) -> usize {
    let ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(buf, len)` against the caller's address space before touching it
    // (`AGENTS.md` §5.4). `bytes` is a live shared `&[u8]` for the duration
    // of the call, so the `(ptr, len)` pair denotes readable memory.
    let written = unsafe { raw_syscall(NUM_CONSOLE_WRITE, [ptr, bytes.len() as u64, 0, 0, 0, 0]) };
    written as usize
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
    fn console_write_marshals_pointer_and_len() {
        let buffer = *b"hello\n";
        let (number, args) = capture(6, || {
            assert_eq!(console_write(&buffer), 6);
        });
        assert_eq!(number, NUM_CONSOLE_WRITE);
        assert_eq!(args[0], buffer.as_ptr() as usize as u64);
        assert_eq!(args[1], 6);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn console_write_returns_the_kernel_accepted_count() {
        let buffer = [0u8; 16];
        let (_, _) = capture(10, || {
            assert_eq!(console_write(&buffer), 10);
        });
    }

    #[test]
    fn i32_arg_sign_extends() {
        assert_eq!(i32_arg(0), 0);
        assert_eq!(i32_arg(1), 1);
        assert_eq!(i32_arg(-1), u64::MAX);
        assert_eq!(i32_arg(i32::MIN), 0xFFFF_FFFF_8000_0000);
    }
}
