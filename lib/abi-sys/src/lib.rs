//! `rustos-abi-sys` — the C-callable `abi-v1` syscall stub runtime.
//!
//! This crate is the implementation behind the generated C header
//! (`include/rustos/rustos_syscall.h`, produced by `cargo xtask c-header`).
//! It exports one `extern "C"` function per `abi-v1` syscall, named
//! `ros_sys_<name>` (for example `ros_sys_ipc_send`), each of which marshals
//! its arguments into the per-architecture syscall registers, issues the
//! trap, and returns the kernel's result. A program **not** written in Rust
//! (C first, then any language with a C FFI) links this runtime to reach the
//! RustOS kernel.
//!
//! It is the curated `/System/Libraries/` class *System runtime / C ABI*
//! (`AGENTS.md` §16.4): deliberately minimal — it marshals to the kernel and
//! nothing more — and dynamically linked, so one security update covers every
//! consumer. See `plans/CCOMPAT.md` (stage CC2) for the staged build plan and
//! its security posture.
//!
//! # Not a privileged path
//!
//! These stubs add **no** authority (`AGENTS.md` §5.4 / `plans/CCOMPAT.md`
//! §4). Every capability check and every input validation happens kernel-side,
//! on the far side of the trap, exactly as for a Rust caller; a C program
//! reaches no syscall it could not reach in Rust and gains nothing by being C.
//! Because the kernel re-validates every argument and fails closed, no
//! argument value passed to a `ros_sys_*` function can cause undefined
//! behaviour, so the stubs are safe `extern "C"` functions.
//!
//! # Symbol naming (`AGENTS.md` §9)
//!
//! Each entry point is pinned to the stable symbol `ros_sys_<name>` with
//! `#[export_name = …]` so the Rust compiler does not mangle it (`extern "C"`
//! alone fixes only the calling convention, not the symbol name). The Rust
//! item names are free to be idiomatic; only the exported symbol is frozen.
//!
//! # Panic-free boundary (`AGENTS.md` §2.9)
//!
//! An unwind across an `extern "C"` boundary is undefined behaviour, so every
//! entry point is panic-free: each performs only constant-index array writes
//! and infallible integer casts before issuing the trap. Errors are reported
//! as the kernel's `int32_t` `ROS_E_*` codes in the return value, never as a
//! panic.
//!
//! # Targets
//!
//! The trap instruction is compiled in only for the three native Tier-1
//! targets (`x86_64`, `aarch64`, `riscv64`); see the `trap` module. `wasm32`
//! has no trap instruction and is out of scope for this runtime
//! (`plans/CCOMPAT.md` §1). On the host the entry points still build and link
//! (the marshalling logic is host-tested through an injectable seam), but
//! there is no kernel to service the call.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

mod trap;

use core::ffi::c_void;

use rustos_abi::{SyscallNumber, SYSCALL_MAX_ARGS};

use trap::raw_syscall;

// Syscall numbers, read from the `abi-v1` source of truth so this crate can
// never disagree with the frozen table (`AGENTS.md` §2.2).
const NUM_YIELD: u64 = SyscallNumber::YIELD.as_u16() as u64;
const NUM_EXIT: u64 = SyscallNumber::EXIT.as_u16() as u64;
const NUM_IPC_SEND: u64 = SyscallNumber::IPC_SEND.as_u16() as u64;
const NUM_IPC_RECV: u64 = SyscallNumber::IPC_RECV.as_u16() as u64;
const NUM_CAP_QUERY: u64 = SyscallNumber::CAP_QUERY.as_u16() as u64;
const NUM_CAP_DELEGATE: u64 = SyscallNumber::CAP_DELEGATE.as_u16() as u64;
const NUM_CAP_REVOKE: u64 = SyscallNumber::CAP_REVOKE.as_u16() as u64;
const NUM_CLOCK_GET: u64 = SyscallNumber::CLOCK_GET.as_u16() as u64;
const NUM_IRQ_BIND: u64 = SyscallNumber::IRQ_BIND.as_u16() as u64;
const NUM_IRQ_WAIT: u64 = SyscallNumber::IRQ_WAIT.as_u16() as u64;
const NUM_RANDOM_GET: u64 = SyscallNumber::RANDOM_GET.as_u16() as u64;
const NUM_CONSOLE_WRITE: u64 = SyscallNumber::CONSOLE_WRITE.as_u16() as u64;

/// Empty argument vector for the no-argument syscalls.
const NO_ARGS: [u64; SYSCALL_MAX_ARGS] = [0; SYSCALL_MAX_ARGS];

/// Marshal a user pointer into its register value.
///
/// The native targets are all 64-bit, so the pointer's address fits a `u64`
/// without loss; the kernel validates the pointer before dereferencing it.
#[inline]
fn ptr_arg(ptr: *mut c_void) -> u64 {
    ptr as usize as u64
}

/// Marshal a 32-bit signed argument into its register value.
///
/// The `abi-v1` `I32` argument convention requires the upper 32 bits to equal
/// the sign extension of the low 32 bits (`lib/abi/src/syscalls.rs`), so the
/// value is widened through `i64` before being reinterpreted as the register
/// bit pattern.
#[inline]
#[allow(clippy::cast_sign_loss)] // Reinterpreting the sign-extended bit pattern is the documented I32 convention.
const fn i32_arg(value: i32) -> u64 {
    value as i64 as u64
}

/// Decode the kernel's raw result register as an `Errno`/`int32_t` (`ROS_E_*`)
/// return value: the low 32 bits reinterpreted as a signed 32-bit code.
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // The low 32 bits ARE the int32_t result; taking them is the documented convention.
const fn ret_i32(raw: u64) -> i32 {
    raw as u32 as i32
}

/// Decode the kernel's raw result register as a `uint32_t` return value: the
/// low 32 bits.
#[inline]
#[allow(clippy::cast_possible_truncation)] // The low 32 bits ARE the uint32_t result; taking them is the documented convention.
const fn ret_u32(raw: u64) -> u32 {
    raw as u32
}

/// `yield`: yield the calling thread (`SyscallNumber::YIELD`).
#[export_name = "ros_sys_yield"]
pub extern "C" fn sys_yield() {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap; `yield` takes no arguments and
    // returns no value.
    unsafe {
        let _ = raw_syscall(NUM_YIELD, NO_ARGS);
    }
}

/// `exit`: terminate the calling process with `code` (`SyscallNumber::EXIT`).
///
/// This function does not return. A correct kernel never returns control from
/// `exit`; should it nonetheless do so, the stub must not return to its C
/// caller (which has no continuation), so it re-issues `exit`. This is a
/// fail-closed loop over the terminating syscall, not a busy-wait
/// (`AGENTS.md` §2.1).
#[export_name = "ros_sys_exit"]
pub extern "C" fn sys_exit(code: i32) -> ! {
    loop {
        // SAFETY: see `sys_yield`. `exit` consumes the exit code in arg 0.
        unsafe {
            let _ = raw_syscall(NUM_EXIT, [i32_arg(code), 0, 0, 0, 0, 0]);
        }
    }
}

/// `ipc_send`: send `len` bytes at `buf` to endpoint `endpoint`
/// (`SyscallNumber::IPC_SEND`). Returns a `ROS_E_*` code.
#[must_use]
#[export_name = "ros_sys_ipc_send"]
pub extern "C" fn sys_ipc_send(endpoint: u64, buf: *mut c_void, len: usize) -> i32 {
    // SAFETY: see `sys_yield`. The kernel validates `(buf, len)` against the
    // caller's address space before touching it (`AGENTS.md` §5.4).
    unsafe {
        ret_i32(raw_syscall(
            NUM_IPC_SEND,
            [endpoint, ptr_arg(buf), len as u64, 0, 0, 0],
        ))
    }
}

/// `ipc_recv`: receive up to `len` bytes from endpoint `endpoint` into `buf`
/// (`SyscallNumber::IPC_RECV`). Returns a `ROS_E_*` code.
#[must_use]
#[export_name = "ros_sys_ipc_recv"]
pub extern "C" fn sys_ipc_recv(endpoint: u64, buf: *mut c_void, len: usize) -> i32 {
    // SAFETY: see `sys_ipc_send`.
    unsafe {
        ret_i32(raw_syscall(
            NUM_IPC_RECV,
            [endpoint, ptr_arg(buf), len as u64, 0, 0, 0],
        ))
    }
}

/// `cap_query`: report whether the caller holds capability `cap`
/// (`SyscallNumber::CAP_QUERY`). Returns `1` if held, `0` otherwise.
#[must_use]
#[export_name = "ros_sys_cap_query"]
pub extern "C" fn sys_cap_query(cap: u16) -> u32 {
    // SAFETY: see `sys_yield`.
    unsafe { ret_u32(raw_syscall(NUM_CAP_QUERY, [u64::from(cap), 0, 0, 0, 0, 0])) }
}

/// `cap_delegate`: delegate a (necessarily narrower) capability set described
/// at `request` to the task named by `handle` (`SyscallNumber::CAP_DELEGATE`).
/// Returns a `ROS_E_*` code.
#[must_use]
#[export_name = "ros_sys_cap_delegate"]
pub extern "C" fn sys_cap_delegate(handle: u64, request: *mut c_void) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `request`.
    unsafe {
        ret_i32(raw_syscall(
            NUM_CAP_DELEGATE,
            [handle, ptr_arg(request), 0, 0, 0, 0],
        ))
    }
}

/// `cap_revoke`: revoke capability `cap` previously delegated via `handle`
/// (`SyscallNumber::CAP_REVOKE`). Returns a `ROS_E_*` code.
#[must_use]
#[export_name = "ros_sys_cap_revoke"]
pub extern "C" fn sys_cap_revoke(handle: u64, cap: u16) -> i32 {
    // SAFETY: see `sys_yield`.
    unsafe {
        ret_i32(raw_syscall(
            NUM_CAP_REVOKE,
            [handle, u64::from(cap), 0, 0, 0, 0],
        ))
    }
}

/// `clock_get`: read the monotonic clock (`SyscallNumber::CLOCK_GET`).
/// Returns the raw 64-bit clock value.
#[must_use]
#[export_name = "ros_sys_clock_get"]
pub extern "C" fn sys_clock_get() -> u64 {
    // SAFETY: see `sys_yield`.
    unsafe { raw_syscall(NUM_CLOCK_GET, NO_ARGS) }
}

/// `irq_bind`: bind the calling task to hardware interrupt `line`
/// (`SyscallNumber::IRQ_BIND`). Returns the opaque 64-bit `IrqHandle`.
#[must_use]
#[export_name = "ros_sys_irq_bind"]
pub extern "C" fn sys_irq_bind(line: u32) -> u64 {
    // SAFETY: see `sys_yield`.
    unsafe { raw_syscall(NUM_IRQ_BIND, [u64::from(line), 0, 0, 0, 0, 0]) }
}

/// `irq_wait`: wait up to `timeout_ns` nanoseconds for the interrupt bound to
/// `handle` (`SyscallNumber::IRQ_WAIT`). Returns a `ROS_E_*` code.
#[must_use]
#[export_name = "ros_sys_irq_wait"]
pub extern "C" fn sys_irq_wait(handle: u64, timeout_ns: u64) -> i32 {
    // SAFETY: see `sys_yield`.
    unsafe { ret_i32(raw_syscall(NUM_IRQ_WAIT, [handle, timeout_ns, 0, 0, 0, 0])) }
}

/// `random_get`: fill `len` bytes at `buf` with random bytes, honouring
/// `flags` (`SyscallNumber::RANDOM_GET`). Returns the number of bytes written.
#[must_use]
#[export_name = "ros_sys_random_get"]
pub extern "C" fn sys_random_get(buf: *mut c_void, len: usize, flags: u32) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, len)`.
    unsafe {
        raw_syscall(
            NUM_RANDOM_GET,
            [ptr_arg(buf), len as u64, u64::from(flags), 0, 0, 0],
        )
    }
}

/// `console_write`: write `len` bytes at `buf` to the system console
/// (`SyscallNumber::CONSOLE_WRITE`). Returns the number of bytes written.
///
/// Requires `CAP_CONSOLE_WRITE`; the kernel validates the capability and the
/// `(buf, len)` pair against the caller's address space before touching it
/// (`AGENTS.md` §5.4). This is the privileged hardware console, not a
/// per-process stdout (`plans/PI.md` P6).
#[must_use]
#[export_name = "ros_sys_console_write"]
pub extern "C" fn sys_console_write(buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, len)`.
    unsafe { raw_syscall(NUM_CONSOLE_WRITE, [ptr_arg(buf), len as u64, 0, 0, 0, 0]) }
}

/// Test-only trap seam: a per-thread injectable replacement for the real trap
/// instruction, used to assert the marshalling and return-decoding of every
/// `ros_sys_*` stub on the host without a kernel (`plans/CCOMPAT.md` CC2
/// "trap injected behind a seam"). Each test arms the seam with the value the
/// "kernel" should return, calls a stub, then inspects the recorded
/// `(number, args)`.
#[cfg(test)]
mod seam {
    use core::cell::Cell;

    use rustos_abi::SYSCALL_MAX_ARGS;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static RETURN_VALUE: Cell<u64> = const { Cell::new(0) };
        static LAST_CALL: Cell<Option<(u64, [u64; SYSCALL_MAX_ARGS])>> = const { Cell::new(None) };
    }

    /// Arm the seam for the current thread: the next trap returns `value` and
    /// its `(number, args)` are recorded for inspection.
    pub(crate) fn arm(value: u64) {
        RETURN_VALUE.with(|v| v.set(value));
        LAST_CALL.with(|c| c.set(None));
        ARMED.with(|a| a.set(true));
    }

    /// The `(number, args)` of the most recent trap on this thread, or `None`
    /// if no trap has been issued since [`arm`].
    pub(crate) fn last_call() -> Option<(u64, [u64; SYSCALL_MAX_ARGS])> {
        LAST_CALL.with(Cell::get)
    }

    /// Called by `trap::raw_syscall` on the host when `#[cfg(test)]`. Records
    /// the call and returns the armed value, or `None` when not armed (so the
    /// non-test sentinel path is still reachable).
    pub(crate) fn dispatch(number: u64, args: &[u64; SYSCALL_MAX_ARGS]) -> Option<u64> {
        if !ARMED.with(Cell::get) {
            return None;
        }
        LAST_CALL.with(|c| c.set(Some((number, *args))));
        Some(RETURN_VALUE.with(Cell::get))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_abi::SYSCALLS;

    /// The complete set of stubs this crate implements, paired with the
    /// `abi-v1` number and argument count each one marshals. The drift tests
    /// below cross-check this registry against the frozen `SYSCALLS` table so
    /// a new or changed syscall cannot silently escape the C surface (the
    /// "dense/complete" discipline of `errno_table_matches_the_frozen_enum`).
    const IMPLEMENTED: &[(u64, &str, u8)] = &[
        (NUM_YIELD, "yield", 0),
        (NUM_EXIT, "exit", 1),
        (NUM_IPC_SEND, "ipc_send", 3),
        (NUM_IPC_RECV, "ipc_recv", 3),
        (NUM_CAP_QUERY, "cap_query", 1),
        (NUM_CAP_DELEGATE, "cap_delegate", 2),
        (NUM_CAP_REVOKE, "cap_revoke", 2),
        (NUM_CLOCK_GET, "clock_get", 0),
        (NUM_IRQ_BIND, "irq_bind", 1),
        (NUM_IRQ_WAIT, "irq_wait", 2),
        (NUM_RANDOM_GET, "random_get", 3),
        (NUM_CONSOLE_WRITE, "console_write", 2),
    ];

    #[test]
    fn registry_covers_exactly_the_frozen_table() {
        assert_eq!(
            IMPLEMENTED.len(),
            SYSCALLS.len(),
            "every abi-v1 syscall must have exactly one ros_sys_* stub"
        );
        for spec in SYSCALLS {
            let number = u64::from(spec.number.as_u16());
            let entry = IMPLEMENTED
                .iter()
                .find(|(n, _, _)| *n == number)
                .unwrap_or_else(|| panic!("no stub registered for syscall {}", spec.name));
            assert_eq!(entry.1, spec.name, "stub name disagrees with abi-v1 table");
            assert_eq!(
                entry.2, spec.arg_count,
                "stub arg count for {} disagrees with abi-v1 table",
                spec.name
            );
        }
    }

    /// Run `call` with the seam armed to return `ret`, returning the recorded
    /// `(number, args)`.
    fn capture(ret: u64, call: impl FnOnce()) -> (u64, [u64; SYSCALL_MAX_ARGS]) {
        seam::arm(ret);
        call();
        seam::last_call().expect("the stub must issue exactly one trap")
    }

    #[test]
    fn yield_marshals_number_and_no_args() {
        let (number, args) = capture(0, || {
            sys_yield();
        });
        assert_eq!(number, NUM_YIELD);
        assert_eq!(args, NO_ARGS);
    }

    #[test]
    fn ipc_send_marshals_endpoint_pointer_and_len() {
        let mut buffer = [0u8; 8];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_ipc_send(0xABCD, ptr, 8), 0);
        });
        assert_eq!(number, NUM_IPC_SEND);
        assert_eq!(args[0], 0xABCD);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 8);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn ipc_recv_marshals_endpoint_pointer_and_len() {
        let mut buffer = [0u8; 16];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            let _ = sys_ipc_recv(0x1234, ptr, 16);
        });
        assert_eq!(number, NUM_IPC_RECV);
        assert_eq!(args[0], 0x1234);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 16);
    }

    #[test]
    fn cap_query_zero_extends_the_capability_id() {
        let (number, args) = capture(1, || {
            assert_eq!(sys_cap_query(0xBEEF), 1);
        });
        assert_eq!(number, NUM_CAP_QUERY);
        assert_eq!(args[0], 0xBEEF);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn cap_delegate_marshals_handle_and_pointer() {
        let mut descriptor = [0u8; 4];
        let ptr = descriptor.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            let _ = sys_cap_delegate(0x55, ptr);
        });
        assert_eq!(number, NUM_CAP_DELEGATE);
        assert_eq!(args[0], 0x55);
        assert_eq!(args[1], ptr as usize as u64);
    }

    #[test]
    fn cap_revoke_marshals_handle_and_capability() {
        let (number, args) = capture(0, || {
            let _ = sys_cap_revoke(0x77, 0x0102);
        });
        assert_eq!(number, NUM_CAP_REVOKE);
        assert_eq!(args[0], 0x77);
        assert_eq!(args[1], 0x0102);
    }

    #[test]
    fn clock_get_marshals_number_and_returns_the_full_u64() {
        let (number, args) = capture(0xDEAD_BEEF_F00D_CAFE, || {
            assert_eq!(sys_clock_get(), 0xDEAD_BEEF_F00D_CAFE);
        });
        assert_eq!(number, NUM_CLOCK_GET);
        assert_eq!(args, NO_ARGS);
    }

    #[test]
    fn irq_bind_zero_extends_line_and_returns_the_handle() {
        let (number, args) = capture(0x9090_9090_9090_9090, || {
            assert_eq!(sys_irq_bind(0xFFFF_FFFF), 0x9090_9090_9090_9090);
        });
        assert_eq!(number, NUM_IRQ_BIND);
        assert_eq!(args[0], 0xFFFF_FFFF);
    }

    #[test]
    fn irq_wait_marshals_handle_and_timeout() {
        let (number, args) = capture(0, || {
            let _ = sys_irq_wait(0x42, 1_000_000);
        });
        assert_eq!(number, NUM_IRQ_WAIT);
        assert_eq!(args[0], 0x42);
        assert_eq!(args[1], 1_000_000);
    }

    #[test]
    fn random_get_marshals_pointer_len_and_flags() {
        let mut buffer = [0u8; 32];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(32, || {
            assert_eq!(sys_random_get(ptr, 32, 1), 32);
        });
        assert_eq!(number, NUM_RANDOM_GET);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], 32);
        assert_eq!(args[2], 1);
    }

    #[test]
    fn console_write_marshals_pointer_and_len() {
        let mut buffer = [0u8; 8];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(8, || {
            assert_eq!(sys_console_write(ptr, 8), 8);
        });
        assert_eq!(number, NUM_CONSOLE_WRITE);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], 8);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn errno_returns_are_truncated_to_int32() {
        // A kernel result whose low 32 bits encode a negative `ROS_E_*` code
        // must reach the C caller as that `int32_t`, regardless of the upper
        // bits the result register happens to carry.
        let raw = 0xFFFF_FFFF_8000_0001u64;
        let (_, _) = capture(raw, || {
            assert_eq!(
                sys_ipc_send(0, core::ptr::null_mut(), 0),
                -2_147_483_647_i32
            );
        });
        let (_, _) = capture(raw, || {
            assert_eq!(sys_cap_query(0), 0x8000_0001u32);
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
