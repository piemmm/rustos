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

use rustos_abi::input::KeyInput;
use rustos_abi::{
    LimitKind, MapFlags, ResourceLimit, SyscallNumber, CONSOLE_INHERIT, STDERR, STDIN, STDINFO,
    STDOUT,
};
use rustos_abi_trap::raw_syscall;

#[cfg(rt_native)]
mod start;

mod startup;

pub use startup::{arg, arg_count};

// The `mem_map`-backed global allocator. Compiled for the native targets that
// register it as the `#[global_allocator]`, and for host unit tests of its pure
// `HeapState` bookkeeping. A plain host build (no allocator to register, no
// tests) needs neither, so the module is left out there to keep it dead-code
// free (`AGENTS.md` §2.14).
#[cfg(any(rt_native, test))]
mod heap;

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

/// `mem_map` syscall number (`AGENTS.md` §2.2, as above).
const NUM_MEM_MAP: u64 = SyscallNumber::MEM_MAP.as_u16() as u64;

/// `mem_unmap` syscall number (`AGENTS.md` §2.2, as above).
const NUM_MEM_UNMAP: u64 = SyscallNumber::MEM_UNMAP.as_u16() as u64;

/// `mmio_map` syscall number (`AGENTS.md` §2.2, as above).
const NUM_MMIO_MAP: u64 = SyscallNumber::MMIO_MAP.as_u16() as u64;

/// `dma_alloc` syscall number (`AGENTS.md` §2.2, as above).
const NUM_DMA_ALLOC: u64 = SyscallNumber::DMA_ALLOC.as_u16() as u64;

/// `wait` syscall number (`AGENTS.md` §2.2, as above).
const NUM_WAIT: u64 = SyscallNumber::WAIT.as_u16() as u64;

/// `ipc_send` syscall number (`AGENTS.md` §2.2, as above).
const NUM_IPC_SEND: u64 = SyscallNumber::IPC_SEND.as_u16() as u64;

/// `rlimit_get` syscall number (`AGENTS.md` §2.2, as above).
const NUM_RLIMIT_GET: u64 = SyscallNumber::RLIMIT_GET.as_u16() as u64;

/// `rlimit_set` syscall number (`AGENTS.md` §2.2, as above).
const NUM_RLIMIT_SET: u64 = SyscallNumber::RLIMIT_SET.as_u16() as u64;

/// `users_db_read` syscall number (`AGENTS.md` §2.2, as above).
const NUM_USERS_DB_READ: u64 = SyscallNumber::USERS_DB_READ.as_u16() as u64;

/// `users_db_wait` syscall number (`AGENTS.md` §2.2, as above).
const NUM_USERS_DB_WAIT: u64 = SyscallNumber::USERS_DB_WAIT.as_u16() as u64;

/// `console_count` syscall number (`AGENTS.md` §2.2, as above).
const NUM_CONSOLE_COUNT: u64 = SyscallNumber::CONSOLE_COUNT.as_u16() as u64;

/// `stream_echo` syscall number (`AGENTS.md` §2.2, as above).
const NUM_STREAM_ECHO: u64 = SyscallNumber::STREAM_ECHO.as_u16() as u64;

/// `key_inject` syscall number (`AGENTS.md` §2.2, as above).
const NUM_KEY_INJECT: u64 = SyscallNumber::KEY_INJECT.as_u16() as u64;

/// `display_acquire` syscall number (`AGENTS.md` §2.2, as above).
const NUM_DISPLAY_ACQUIRE: u64 = SyscallNumber::DISPLAY_ACQUIRE.as_u16() as u64;

/// `display_release` syscall number (`AGENTS.md` §2.2, as above).
const NUM_DISPLAY_RELEASE: u64 = SyscallNumber::DISPLAY_RELEASE.as_u16() as u64;

/// `keyboard_read` syscall number (`AGENTS.md` §2.2, as above).
const NUM_KEYBOARD_READ: u64 = SyscallNumber::KEYBOARD_READ.as_u16() as u64;

/// `resource_grants` syscall number (`AGENTS.md` §2.2, as above).
const NUM_RESOURCE_GRANTS: u64 = SyscallNumber::RESOURCE_GRANTS.as_u16() as u64;

/// `clock_get` syscall number (`AGENTS.md` §2.2, as above).
const NUM_CLOCK_GET: u64 = SyscallNumber::CLOCK_GET.as_u16() as u64;

/// `hw_tree_read` syscall number (`AGENTS.md` §2.2, as above).
const NUM_HW_TREE_READ: u64 = SyscallNumber::HW_TREE_READ.as_u16() as u64;

/// `hw_tree_wait` syscall number (`AGENTS.md` §2.2, as above).
const NUM_HW_TREE_WAIT: u64 = SyscallNumber::HW_TREE_WAIT.as_u16() as u64;

/// `ipc_call` syscall number (`AGENTS.md` §2.2, as above).
const NUM_IPC_CALL: u64 = SyscallNumber::IPC_CALL.as_u16() as u64;

/// `irq_bind` syscall number (`AGENTS.md` §2.2, as above).
const NUM_IRQ_BIND: u64 = SyscallNumber::IRQ_BIND.as_u16() as u64;

/// `irq_wait` syscall number (`AGENTS.md` §2.2, as above).
const NUM_IRQ_WAIT: u64 = SyscallNumber::IRQ_WAIT.as_u16() as u64;

/// `call_create` syscall number (`AGENTS.md` §2.2, as above).
const NUM_CALL_CREATE: u64 = SyscallNumber::CALL_CREATE.as_u16() as u64;

/// `call_recv` syscall number (`AGENTS.md` §2.2, as above).
const NUM_CALL_RECV: u64 = SyscallNumber::CALL_RECV.as_u16() as u64;

/// `call_reply` syscall number (`AGENTS.md` §2.2, as above).
const NUM_CALL_REPLY: u64 = SyscallNumber::CALL_REPLY.as_u16() as u64;

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
/// before writing it (`AGENTS.md` §5.4). The stream *backing* owns
/// blocking (§20): a read with no pending input parks the caller in the
/// kernel until input arrives, so a successful read returns at least one
/// byte. A short read (fewer bytes than `buf.len()`) is valid, so the
/// caller loops for more.
///
/// The kernel encodes a failure as a negative register (`-errno`, the
/// standard `abi-v1` convention) — e.g. fd 0 is not a readable stream, or
/// the buffer pointer faults. A reader handed a `&mut [u8]` has no way to
/// surface an `Errno`, and an unread input stream is indistinguishable from
/// end-of-input from the program's side (the *backing* owns blocking, §20),
/// so this reports a failure as a zero-length read. The count is also
/// clamped to `buf.len()` as defence in depth, so a buggy kernel count can
/// never drive an out-of-bounds slice in the caller (`AGENTS.md` §5.4).
#[must_use]
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the clamped count never exceeds `buf.len()`.
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 stream-read encoding (count ≥ 0, else -errno).
#[allow(clippy::cast_sign_loss)] // The negative (`-errno`) case returns early above; the cast runs only when `read >= 0`.
pub fn stdin(buf: &mut [u8]) -> usize {
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(buf, len)` against the caller's address space before touching it
    // (`AGENTS.md` §5.4). `buf` is a live exclusive `&mut [u8]` for the
    // duration of the call, so the `(ptr, len)` pair denotes writable
    // memory the kernel may fill.
    let read =
        unsafe { raw_syscall(NUM_STREAM_READ, [u64::from(STDIN), ptr, len, 0, 0, 0]) } as i64;
    if read < 0 {
        return 0;
    }
    (read as usize).min(buf.len())
}

/// Set whether standard input (fd 0) echoes the bytes it reads back to its
/// console (`SyscallNumber::STREAM_ECHO`, `AGENTS.md` §20 — terminal local
/// echo), returning the raw signed register (`0` on success, else
/// `-errno`).
///
/// Console echo defaults to **on**, so an interactive user sees what they
/// type at a [`stdin`] read. A program suppresses it around a secret it
/// must not render — login disables echo before reading a password and
/// re-enables it afterwards (`AGENTS.md` §5.4 — never echo a credential).
/// Requires `CAP_CONSOLE_READ`; the kernel performs the echo itself as part
/// of the read line discipline, so no `CAP_CONSOLE_WRITE` is needed. A
/// build with no console wired, or an fd 0 that is not a readable stream,
/// fails closed with `-errno` (`AGENTS.md` §2.9); the wrapper surfaces it
/// verbatim so the caller decides how to react.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn set_echo(enabled: bool) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates the capability and resolves fd 0
    // before touching any state (`AGENTS.md` §5.4).
    let ret = unsafe {
        raw_syscall(
            NUM_STREAM_ECHO,
            [u64::from(STDIN), u64::from(enabled), 0, 0, 0, 0],
        )
    };
    ret as i64
}

/// Inject one decoded keyboard `record` into the kernel input-focus arbiter
/// (`SyscallNumber::KEY_INJECT`, `AGENTS.md` §20, `plans/PI.md` P11 — input
/// follows the surface owner), returning the raw signed register (the bytes
/// consumed when non-negative, else `-errno`).
///
/// The producer-side call a keyboard-input driver issues after decoding a
/// directly attached keyboard into a [`KeyInput`] key edge: the kernel
/// validates `CAP_INPUT_INJECT` and the `(buf, len)` pair against the
/// caller's address space (`AGENTS.md` §5.4), decodes the record fail-closed,
/// and routes it by who holds input focus — a *press* encoded to the focused
/// text console's tty bytes, or the whole record delivered to the desktop
/// keyboard channel. The driver no longer chooses the encoding or the
/// destination (`AGENTS.md` §17.4). A malformed record or an unwired arbiter
/// fails closed with `-errno` (`AGENTS.md` §2.9); the wrapper surfaces the
/// raw signed value so the caller decides how to react.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn key_inject(record: &KeyInput) -> i64 {
    let bytes = record.to_le_bytes();
    let ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_INPUT_INJECT` and the `(buf, len)` pair against the caller's
    // address space before reading it (`AGENTS.md` §5.4). `bytes` is a live
    // stack array for the duration of the call, so the `(ptr, len)` pair
    // denotes readable memory.
    let ret = unsafe { raw_syscall(NUM_KEY_INJECT, [ptr, bytes.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Acquire ownership of the display and claim keyboard input focus
/// (`SyscallNumber::DISPLAY_ACQUIRE`, `AGENTS.md` §10 / §17.3 / §20,
/// `plans/PI.md` P11), returning `0` on success or `-errno`.
///
/// The compositing window manager calls this when it takes over the screen:
/// the kernel input-focus arbiter switches its foreground to the desktop
/// keyboard channel, so subsequently injected key edges are delivered as
/// [`KeyInput`] records the manager drains with [`keyboard_read`]. Requires
/// `CAP_DISPLAY` (`AGENTS.md` §4 — owning the display is privileged).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn display_acquire() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates `CAP_DISPLAY` before touching state.
    let ret = unsafe { raw_syscall(NUM_DISPLAY_ACQUIRE, [0, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Release the display and return keyboard input focus to the text console
/// (`SyscallNumber::DISPLAY_RELEASE`, `AGENTS.md` §10 / §17.3 / §20,
/// `plans/PI.md` P11), returning `0` on success or `-errno`.
///
/// The inverse of [`display_acquire`]; requires `CAP_DISPLAY`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn display_release() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates `CAP_DISPLAY` before touching state.
    let ret = unsafe { raw_syscall(NUM_DISPLAY_RELEASE, [0, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Read one decoded keyboard event from the kernel keyboard channel into
/// `buf` (`SyscallNumber::KEYBOARD_READ`, `AGENTS.md` §10, `plans/PI.md`
/// P11), returning the raw signed register (the bytes written — one
/// [`KeyInput`] record's [`KeyInput::WIRE_LEN`], or `0` when the channel is
/// momentarily drained — when non-negative, else `-errno`).
///
/// The principal that owns the display (the window manager) drains the
/// records the arbiter routed to it while it held focus. The kernel
/// validates `CAP_INPUT_READ` and the `(buf, len)` pair against the caller's
/// address space (`AGENTS.md` §5.4); a `buf` shorter than
/// [`KeyInput::WIRE_LEN`] fails closed with `-errno` (`AGENTS.md` §2.9). A
/// zero return is a valid empty read, so the caller loops.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn keyboard_read(buf: &mut [u8]) -> i64 {
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_INPUT_READ` and the `(buf, len)` pair against the caller's address
    // space before writing it (`AGENTS.md` §5.4). `buf` is a live exclusive
    // `&mut [u8]` for the duration of the call, so the `(ptr, len)` pair
    // denotes writable memory.
    let ret = unsafe { raw_syscall(NUM_KEYBOARD_READ, [ptr, buf.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Enumerate the device-resource grants the kernel minted for the calling
/// driver task into `buf` (`SyscallNumber::RESOURCE_GRANTS`, `AGENTS.md` §4 /
/// §18.3 / §20, `plans/PI.md` P10 chunk 5d-2), returning the raw signed
/// register: the total number of bytes written — consecutive
/// [`rustos_abi::hwtree::GrantedResource`] records — when non-negative, else
/// `-errno`.
///
/// A driver process calls this once at start-up to learn the unforgeable
/// handles it passes to [`mmio_map`] / [`dma_alloc`]. It needs no capability
/// (a task reads only its *own* grants); the kernel validates the
/// `(buf, len)` pair against the caller's address space before writing it
/// (`AGENTS.md` §5.4). A `buf` too small for the whole grant set fails closed
/// with `-errno` (`BufferTooSmall`, `AGENTS.md` §2.9), so size it for the
/// matched node's resource count; a task with no grants returns `0`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn resource_grants(buf: &mut [u8]) -> i64 {
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(buf, len)` pair against the caller's address space before writing
    // it (`AGENTS.md` §5.4). `buf` is a live exclusive `&mut [u8]` for the
    // duration of the call, so the `(ptr, len)` pair denotes writable memory.
    let ret = unsafe { raw_syscall(NUM_RESOURCE_GRANTS, [ptr, buf.len() as u64, 0, 0, 0, 0]) };
    ret as i64
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

/// Read the kernel monotonic clock, in nanoseconds
/// (`SyscallNumber::CLOCK_GET`, `AGENTS.md` §21).
///
/// Returns a monotonically non-decreasing nanosecond reading from a clock
/// whose epoch is unspecified — only differences between readings are
/// meaningful. It requires no capability (`clock_get` is callable by every
/// task); a caller without [`CapabilityId::TIME_HIRES`] reads it floored to
/// [`rustos_abi::time::COARSE_CLOCK_GRANULARITY_NS`] (one microsecond), since
/// a sub-microsecond timer is a side-channel primitive the kernel withholds
/// from untrusted callers (`AGENTS.md` §19.1). The wrapper performs no
/// coarsening of its own — the value it returns is exactly what the kernel
/// handed back.
///
/// [`CapabilityId::TIME_HIRES`]: rustos_abi::CapabilityId::TIME_HIRES
#[must_use]
pub fn clock_get() -> u64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap (`AGENTS.md` §5.4). `clock_get`
    // takes no arguments and no memory operand, so all six argument registers
    // are zero; its result is the `U64` nanosecond reading.
    unsafe { raw_syscall(NUM_CLOCK_GET, [0, 0, 0, 0, 0, 0]) }
}

/// Park, yielding the CPU, until `now()` reaches `deadline_ns`.
///
/// The shared core of [`ClockDelay`]'s
/// [`delay_us`](rustos_abi::Delay::delay_us): it reads the monotonic clock
/// through `now` and surrenders the CPU through `yield_fn` between reads, so
/// it is a cooperative wait rather than a hard spin (`AGENTS.md` §2.1). A
/// deadline already in the past returns immediately without yielding. The
/// generic seams keep the loop host-testable against a deterministic clock
/// without issuing a real trap.
fn spin_until_ns(deadline_ns: u64, mut now: impl FnMut() -> u64, mut yield_fn: impl FnMut()) {
    while now() < deadline_ns {
        yield_fn();
    }
}

/// Nanoseconds in one microsecond — the [`ClockDelay`] conversion factor.
const NANOS_PER_MICRO: u64 = 1_000;

/// The userland [`Delay`](rustos_abi::Delay) implementation: timed waits and
/// a monotonic clock backed by the [`clock_get`] syscall.
///
/// A driver process (or any program) that must honour a hardware-dictated
/// settle window — a PCIe link train, a USB hub power-on-good / reset-recovery
/// window — hands one of these to the bring-up code that takes a
/// [`Delay`](rustos_abi::Delay). It lives here, in the one userland runtime,
/// so every driver process shares a single clock-backed `Delay` rather than
/// each rolling its own over [`clock_get`] (`AGENTS.md` §2.2).
///
/// The wait is cooperative: [`delay_us`](rustos_abi::Delay::delay_us) yields
/// the CPU to other runnable tasks between clock reads instead of busy-spinning
/// (`AGENTS.md` §2.1). It carries no authority — `clock_get` needs no
/// capability — and holds no state, so it is `Copy` and trivially shareable.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClockDelay;

impl ClockDelay {
    /// A new clock-backed delay. Equivalent to [`ClockDelay::default`].
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl rustos_abi::Delay for ClockDelay {
    fn delay_us(&self, us: u32) {
        // Compute the deadline from the clock the loop polls, saturating so a
        // reading near `u64::MAX` can never wrap the deadline below `now`
        // (which would return instantly); the monotonic clock realistically
        // never approaches that, but the wait must not silently shorten
        // (`AGENTS.md` §2.9).
        let deadline = clock_get().saturating_add(u64::from(us).saturating_mul(NANOS_PER_MICRO));
        spin_until_ns(deadline, clock_get, yield_now);
    }

    fn now_us(&self) -> u64 {
        // Floor the nanosecond reading to whole microseconds, matching the
        // microsecond resolution the `Delay` contract specifies; integer
        // division never exceeds the true reading, so the sequence stays
        // monotonically non-decreasing.
        clock_get() / NANOS_PER_MICRO
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
/// The child's standard streams attach to the **caller's own** console
/// ([`rustos_abi::CONSOLE_INHERIT`], `AGENTS.md` §20): a spawned session
/// member (login's shell, a shell's job) stays on the console its parent
/// was driving. To start a process on a *different* installed console —
/// PID 1 launching one login per console (`plans/PI.md` P11) — use
/// [`spawn_at`].
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
    let ret = unsafe {
        raw_syscall(
            NUM_SPAWN,
            [ptr, path.len() as u64, CONSOLE_INHERIT, 0, 0, 0],
        )
    };
    ret as i64
}

/// Spawn the embedded program registered under the absolute `path` with
/// its standard streams attached to the installed console `console`
/// (`SyscallNumber::SPAWN`, `AGENTS.md` §20, `plans/PI.md` P11).
///
/// The console-selecting form of [`spawn`]: `console` names an index in
/// the kernel's installed console list (its length is reported by
/// [`console_count`]); an index with no installed console fails closed
/// with `-errno` (`NotFound`). PID 1 `init` uses this to start one login
/// session per discovered text console — the video console and the UART
/// are separate session contexts.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 spawn-result encoding (PID ≥ 0, else -errno).
pub fn spawn_at(path: &[u8], console: u32) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(path, len)` against the caller's address space and `console`
    // against the installed console list before touching any state
    // (`AGENTS.md` §5.4). `path` is a live shared `&[u8]` for the duration
    // of the call, so the `(ptr, len)` pair denotes readable memory.
    let ret = unsafe {
        raw_syscall(
            NUM_SPAWN,
            [ptr, path.len() as u64, u64::from(console), 0, 0, 0],
        )
    };
    ret as i64
}

/// Report how many system text consoles are installed
/// (`SyscallNumber::CONSOLE_COUNT`, `AGENTS.md` §20, `plans/PI.md` P11).
///
/// Requires `CAP_CONSOLE_WRITE`. The count is the index space
/// [`spawn_at`]'s `console` argument selects from; PID 1 `init` uses it
/// to start one login session per discovered console. The kernel encodes
/// the result as a signed register: a non-negative value is the count,
/// a negative value is `-errno` (the wrapper surfaces it verbatim,
/// `AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
pub fn console_count() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates the capability before any state
    // is touched (`AGENTS.md` §5.4).
    let ret = unsafe { raw_syscall(NUM_CONSOLE_COUNT, [0, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Map `len` bytes of fresh, zeroed anonymous `RW` memory into the calling
/// process's **own** address space (`SyscallNumber::MEM_MAP`,
/// `plans/SPAWN.md` SP5).
///
/// `flags` ([`MapFlags`]) selects placement: with [`MapFlags::FIXED`] the
/// kernel maps the region at exactly `addr_hint` (page-aligned, a free
/// range) or fails closed; otherwise `addr_hint` is advisory and `0` means
/// "kernel chooses". The region is zeroed before it is visible and is never
/// executable (`AGENTS.md` §19.2 — W^X); mapping one's own isolated space
/// grants no further authority, so no capability is required
/// (`AGENTS.md` §16.6 / §4).
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: a non-negative value is the base address
/// of the new region, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`) — a frame exhaustion is
/// reported as [`rustos_abi::Errno::OutOfMemory`] (`AGENTS.md` §4 —
/// deterministic OOM, never a panic). The wrapper surfaces that raw signed
/// value so the caller decides how to react; it adds no authority and hides
/// no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 mem_map-result encoding (base ≥ 0, else -errno).
pub fn mem_map(len: usize, flags: MapFlags, addr_hint: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap (`AGENTS.md` §5.4). `mem_map`
    // dereferences no user pointer; it maps the region into the caller's own
    // space and returns its base, so no memory operand is passed.
    let ret = unsafe {
        raw_syscall(
            NUM_MEM_MAP,
            [len as u64, u64::from(flags.bits()), addr_hint, 0, 0, 0],
        )
    };
    ret as i64
}

/// Release the region of `len` bytes based at `base` previously returned by
/// [`mem_map`] from the calling process's own address space
/// (`SyscallNumber::MEM_UNMAP`, `plans/SPAWN.md` SP5).
///
/// The kernel zeroes the frames it reclaims (`AGENTS.md` §4 — secret
/// hygiene) and fails closed when `(base, len)` does not name a region the
/// caller mapped (`AGENTS.md` §5.4). Returns `0` on success or `-errno`
/// (recover the [`rustos_abi::Errno`] discriminant as `-ret`), following the
/// standard `abi-v1` signed-result convention; the wrapper hides no error
/// (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 mem_unmap-result encoding (0, else -errno).
pub fn mem_unmap(base: u64, len: usize) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(base, len)` range against the caller's own address space before
    // unmapping it (`AGENTS.md` §5.4). No user pointer is dereferenced.
    let ret = unsafe { raw_syscall(NUM_MEM_UNMAP, [base, len as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Map a **granted** device MMIO window into the calling driver's own
/// address space (`SyscallNumber::MMIO_MAP`, `plans/PI.md` P10 chunk 5d-0).
///
/// `handle` is an unforgeable, kernel-issued device-resource grant handle —
/// never a raw physical address (`AGENTS.md` §4): the kernel resolves it
/// **owner-checked against the calling task**, confirms it names a memory
/// window, and maps only that region — caching disabled, never executable
/// (`AGENTS.md` §5.4 / §18.3 / §19.2). A forged or another driver's handle
/// resolves to nothing and is refused. The call carries `CAP_MMIO_MAP`
/// (enforced by the kernel before any state is touched).
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the base virtual address of
/// the newly mapped window, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`). The wrapper surfaces that
/// raw signed value so the caller decides how to react; it adds no authority
/// and hides no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 mmio_map-result encoding (base ≥ 0, else -errno).
pub fn mmio_map(handle: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap (`AGENTS.md` §5.4). `mmio_map`
    // dereferences no user pointer; it resolves the grant handle and maps the
    // window into the caller's own space, returning its base.
    let ret = unsafe { raw_syscall(NUM_MMIO_MAP, [handle, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Allocate a coherent DMA buffer for the calling driver, bounded by a
/// granted device DMA constraint (`SyscallNumber::DMA_ALLOC`, `plans/PI.md`
/// P10 chunk 5d-0).
///
/// `handle` is an unforgeable, kernel-issued device-resource grant handle —
/// never a raw physical address (`AGENTS.md` §4): the kernel resolves it
/// **owner-checked against the calling task**, confirms it names a DMA
/// constraint, carves a physically-contiguous, zeroed, coherent buffer of
/// `len` bytes whose physical extent lies within the grant's addressing
/// limit (`AGENTS.md` §5.4 / §18.3), maps it `RW`, non-executable,
/// guard-bracketed into the caller's own address space, writes the buffer's
/// **device-visible** base address to `device_out`, and returns the base
/// **user virtual address** the driver's CPU accesses go through. The call
/// carries `CAP_MEM_DMA` (enforced by the kernel before any state is
/// touched).
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the buffer's base virtual
/// address, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`) — `device_out` is left
/// untouched on a negative result. The wrapper surfaces that raw signed
/// value so the caller decides how to react; it adds no authority and hides
/// no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 dma_alloc-result encoding (base ≥ 0, else -errno).
pub fn dma_alloc(handle: u64, len: usize, device_out: &mut u64) -> i64 {
    let ptr = (device_out as *mut u64) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap (`AGENTS.md` §5.4). `device_out`
    // is a live exclusive `&mut u64` for the duration of the call, so the
    // pointer denotes writable memory the kernel may fill with the
    // device-visible base; the kernel validates it against the caller's own
    // address space before writing.
    let ret = unsafe { raw_syscall(NUM_DMA_ALLOC, [handle, len as u64, ptr, 0, 0, 0]) };
    ret as i64
}

/// Bind interrupt `line` to the calling task, minting an unforgeable
/// [`rustos_abi::IrqHandle`] (`SyscallNumber::IRQ_BIND`, `AGENTS.md` §5.2).
///
/// `line` is the architecture interrupt-line identifier the driver received
/// as an [`HwResourceKind::Irq`](rustos_abi::hwtree::HwResourceKind) grant on
/// its matched node (`AGENTS.md` §18.3) — a discovered value, never a board
/// constant. The call carries `CAP_IRQ_BIND` (enforced by the kernel before
/// any state is touched); the minted handle is re-keyed to the calling task,
/// so only this task can `irq_wait` on it (`AGENTS.md` §5.4).
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the raw `IrqHandle`, and a
/// negative value is `-errno` (recover the [`rustos_abi::Errno`] discriminant
/// as `-ret`). The wrapper surfaces that raw signed value; it adds no
/// authority and hides no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 irq_bind-result encoding (handle ≥ 0, else -errno).
pub fn irq_bind(line: u32) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap (`AGENTS.md` §5.4). `irq_bind`
    // dereferences no user pointer; it records the binding and returns a
    // handle.
    let ret = unsafe { raw_syscall(NUM_IRQ_BIND, [u64::from(line), 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Park the calling task until the interrupt bound to `handle` fires, the
/// `timeout_ns` deadline elapses, or the binding disappears
/// (`SyscallNumber::IRQ_WAIT`, `AGENTS.md` §5.2).
///
/// `handle` is the [`rustos_abi::IrqHandle`] a prior [`irq_bind`] minted for
/// this task; the kernel re-checks the binding owner-side on every call
/// (`AGENTS.md` §5.4) and parks the task off the run queue between polls (no
/// busy-wait, `AGENTS.md` §2.1). Pass `u64::MAX` for an effectively unbounded
/// wait. The kernel re-arms the bound line on the driver's behalf across the
/// park (the driver holds no controller access, §4), so an interrupt-driven
/// driver loops `irq_wait` → drain → `irq_wait` without touching hardware
/// interrupt-controller state.
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: `0` on a fire, and a negative value is `-errno`
/// (`Errno::TimedOut` on the deadline, `Errno::NotFound` for a forged or
/// released handle — recover the discriminant as `-ret`). The wrapper
/// surfaces that raw signed value and hides no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 irq_wait-result encoding (0, else -errno).
pub fn irq_wait(handle: u64, timeout_ns: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the handle owner-side on the far side of the trap (`AGENTS.md` §5.4).
    // `irq_wait` dereferences no user pointer.
    let ret = unsafe { raw_syscall(NUM_IRQ_WAIT, [handle, timeout_ns, 0, 0, 0, 0]) };
    ret as i64
}

/// Wait for a child process to exit, reaping it and reading back its exit
/// code (`SyscallNumber::WAIT`, `plans/SPAWN.md` SP6).
///
/// `pid` is either a specific child's PID or [`rustos_abi::WAIT_ANY`] to
/// wait for whichever of the caller's children exits next. On success the
/// kernel writes the reaped child's exit code into `status` and returns its
/// PID. A process may only wait on its **own** children; the kernel
/// validates the parent/child relationship and fails closed (`AGENTS.md`
/// §4 / §5.4).
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: a non-negative value is the reaped child's
/// PID, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`) — `status` is left
/// untouched on a negative result. The wrapper surfaces that raw signed
/// value so the caller decides how to react; it adds no authority and hides
/// no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 wait-result encoding (PID ≥ 0, else -errno).
pub fn wait(pid: i32, status: &mut i32) -> i64 {
    let ptr = (status as *mut i32) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `status` pointer against the caller's address space before
    // writing the exit code to it (`AGENTS.md` §5.4). `status` is a live
    // exclusive `&mut i32` for the duration of the call, so the pointer
    // denotes writable memory the kernel may fill.
    let ret = unsafe { raw_syscall(NUM_WAIT, [i32_arg(pid), ptr, 0, 0, 0, 0]) };
    ret as i64
}

/// Read the calling process's effective limit for resource `kind`
/// (`SyscallNumber::RLIMIT_GET`, `AGENTS.md` §24.3).
///
/// On success the kernel writes the encoded [`ResourceLimit`] into a local
/// buffer this wrapper decodes and returns. Reading one's own limit grants
/// no authority and needs no capability (`AGENTS.md` §16.6 / §24.3). The
/// kernel encodes a failure as a negative register (`-errno`, the standard
/// `abi-v1` convention); the wrapper surfaces it as `Err(-ret)` (the raw
/// negative value) and hides no error (`AGENTS.md` §2.9).
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure, including
/// the case where the kernel returned a malformed limit (`soft > hard`),
/// which fails closed rather than yielding a usable value.
pub fn rlimit_get(kind: LimitKind) -> Result<ResourceLimit, i64> {
    let mut buf = [0u8; ResourceLimit::WIRE_LEN];
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `out` pointer against the caller's address space before writing
    // the encoded limit to it (`AGENTS.md` §5.4). `buf` is a live exclusive
    // local for the duration of the call, so the pointer denotes writable
    // memory the kernel may fill.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 errno-result encoding (0, else -errno).
    let ret =
        unsafe { raw_syscall(NUM_RLIMIT_GET, [u64::from(kind.as_u32()), ptr, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // The kernel reported success, so the buffer holds a well-formed encoded
    // limit. Defence in depth: decode validates `soft <= hard` and fails
    // closed, so a buggy kernel cannot hand back a malformed pair.
    ResourceLimit::decode(&buf).map_err(|e| -i64::from(e.as_i32()))
}

/// Install the calling process's limit for resource `kind`
/// (`SyscallNumber::RLIMIT_SET`, `AGENTS.md` §24.3).
///
/// The wrapper encodes `value` into a local buffer the kernel reads. A
/// process may freely *lower* a bound, but *raising* a hard bound above the
/// inherited ceiling requires [`rustos_abi::CapabilityId::RLIMIT_RAISE`]
/// (§24.3). Returns `0` on success or `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`), the standard `abi-v1`
/// signed-result convention; the wrapper hides no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn rlimit_set(kind: LimitKind, value: ResourceLimit) -> i64 {
    let buf = value.encode();
    let ptr = buf.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `value` pointer against the caller's address space before reading
    // the encoded limit from it (`AGENTS.md` §5.4). `buf` is a live local
    // for the duration of the call, so the pointer denotes readable memory.
    let ret = unsafe { raw_syscall(NUM_RLIMIT_SET, [u64::from(kind.as_u32()), ptr, 0, 0, 0, 0]) };
    ret as i64
}

/// Read the system user database (`/System/Security/Users`) the kernel
/// loaded at boot into `buf` (`SyscallNumber::USERS_DB_READ`,
/// `plans/PI.md` P11), returning the number of bytes copied.
///
/// The copied bytes are the database's exact `users-v1` text, which the
/// caller parses with the fail-closed `rustos-users` parser. Gated
/// kernel-side on [`rustos_abi::CapabilityId::USERS_READ`] — only the
/// authentication principal (login) holds it; the wrapper adds no
/// authority (`AGENTS.md` §5.4). Sizing `buf` at the format's own
/// 64 KiB maximum (`rustos-users` `MAX_DB_LEN`) always suffices: a
/// buffer smaller than the database is refused whole — a credential
/// database is never truncated (`AGENTS.md` §2.9).
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure: the
/// caller lacks the capability, no database is held (no root volume, or
/// the boot read refused the record — the caller fails closed and
/// refuses every login, `AGENTS.md` §5.4.5), or `buf` is too small.
pub fn users_db_read(buf: &mut [u8]) -> Result<usize, i64> {
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(buf, len)` pair against the caller's address space before
    // writing to it (`AGENTS.md` §5.4). `buf` is a live exclusive
    // `&mut [u8]` for the duration of the call, so the pair denotes
    // writable memory the kernel may fill.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe { raw_syscall(NUM_USERS_DB_READ, [ptr, len, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice in the caller
    // (`AGENTS.md` §5.4), exactly as `stdin` clamps.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(buf.len()))
}

/// Send `payload` to the IPC endpoint `endpoint`
/// (`SyscallNumber::IPC_SEND`).
///
/// The kernel resolves `endpoint` against the live named-port registry,
/// bounds the payload against the port's advertised maximum, copies it in
/// through the validated `copy_from_user` boundary, and enforces the
/// port's required send capability against the **caller's** effective set
/// before enqueueing (`AGENTS.md` §5.2 / §5.4) — the wrapper adds no
/// authority. A spawned driver process uses this to report its
/// `register()` outcome back to the driver host on the reply endpoint
/// handed to it through its startup args (`PLAN.md` Stage 4.HW).
///
/// Returns `0` on success or `-errno` (recover the [`rustos_abi::Errno`]
/// discriminant as `-ret`), the standard `abi-v1` signed-result
/// convention; the wrapper hides no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn ipc_send(endpoint: u64, payload: &[u8]) -> i64 {
    let ptr = payload.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(ptr, len)` pair against the caller's address space before
    // reading it (`AGENTS.md` §5.4). `payload` is a live shared `&[u8]` for
    // the duration of the call, so the pair denotes readable memory.
    let ret = unsafe { raw_syscall(NUM_IPC_SEND, [endpoint, ptr, payload.len() as u64, 0, 0, 0]) };
    ret as i64
}

/// Read the discovered hardware tree the kernel built at boot into `buf`
/// (`SyscallNumber::HW_TREE_READ`, `AGENTS.md` §16.6 / §18.1 / §18.4),
/// returning the number of bytes copied.
///
/// The copied bytes are a [`rustos_abi::HwTreeHeader`] (the store's
/// current generation and the node count) followed by that many
/// [`rustos_abi::HwNode`] records, which the caller decodes with the
/// fail-closed `from_bytes` parsers. The generation in the header is the
/// value to pass to [`hw_tree_wait`] to block until the tree next changes.
/// Gated kernel-side on [`rustos_abi::CapabilityId::SYSINFO_HW`] — the
/// privileged global hardware view (`AGENTS.md` §16.6 / §18.4); the
/// wrapper adds no authority.
///
/// The whole inventory is copied or none: a buffer smaller than the
/// snapshot is refused with `BufferTooSmall` rather than truncated
/// (`AGENTS.md` §2.9), so the caller grows `buf` and retries (the node
/// count is a discovered capacity, not a fixed ceiling — `AGENTS.md`
/// §24.1).
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure: the
/// caller lacks the capability, no hardware-tree store is wired
/// (`NotImplemented`), or `buf` is too small (`BufferTooSmall`).
pub fn hw_tree_read(buf: &mut [u8]) -> Result<usize, i64> {
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(buf, len)` pair against the caller's address space before
    // writing to it (`AGENTS.md` §5.4). `buf` is a live exclusive
    // `&mut [u8]` for the duration of the call, so the pair denotes
    // writable memory the kernel may fill.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe { raw_syscall(NUM_HW_TREE_READ, [ptr, len, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice in the caller
    // (`AGENTS.md` §5.4), exactly as `users_db_read` clamps.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(buf.len()))
}

/// Block until the discovered hardware tree changes past
/// `last_generation` (`SyscallNumber::HW_TREE_WAIT`, `AGENTS.md` §18.4 —
/// reactive re-match and hotplug).
///
/// `last_generation` is the generation the caller last observed through
/// [`hw_tree_read`]'s header; `timeout_ns` bounds the wait
/// (`u64::MAX` for an effectively unbounded block). The kernel blocks the
/// caller cooperatively until the store's generation differs — a node was
/// seeded, appended, or removed — then returns `0`, so the caller
/// re-reads the tree and re-matches. Gated kernel-side on
/// [`rustos_abi::CapabilityId::SYSINFO_HW`], the same privilege as reading
/// the tree; the wrapper adds no authority.
///
/// Returns `0` once the tree has changed, or `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`): `-TimedOut` if the
/// deadline elapses first, or `-NotImplemented` if no hardware-tree store
/// is wired. The wrapper hides no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn hw_tree_wait(last_generation: u64, timeout_ns: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap (`AGENTS.md` §5.4). Both
    // arguments are scalars; the call reads no caller memory.
    let ret = unsafe { raw_syscall(NUM_HW_TREE_WAIT, [last_generation, timeout_ns, 0, 0, 0, 0]) };
    ret as i64
}

/// Block until the system user database leaves its *pending*
/// (still-being-unlocked) state (`SyscallNumber::USERS_DB_WAIT`,
/// `AGENTS.md` §5.1, `plans/PI.md` P11 — the reactive companion to
/// [`users_db_read`]).
///
/// Under design B `login` is spawned before the in-kernel unlock kthread
/// mounts the encrypted root, so an early [`users_db_read`] reports
/// `WouldBlock` — the live-but-not-ready signal. Rather than re-reading in
/// a yield loop (a busy spin that audited one ERROR per poll, `AGENTS.md`
/// §2.1), the caller blocks here: the kernel parks it off the run queue and
/// wakes it the instant the unlock reaches a terminal outcome (a database is
/// installed, or the unlock gives up), so the next [`users_db_read`] returns
/// the database or the inert `NotImplemented`. `timeout_ns` bounds the wait
/// (`u64::MAX` for an effectively unbounded block). Gated kernel-side on
/// [`rustos_abi::CapabilityId::USERS_READ`], the same privilege as reading
/// the database; the wrapper adds no authority.
///
/// Returns `0` once the database is no longer pending (the caller re-reads
/// and re-classifies it), or `-errno` (recover the [`rustos_abi::Errno`]
/// discriminant as `-ret`): `-TimedOut` if the deadline elapses first. The
/// wrapper hides no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn users_db_wait(timeout_ns: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap (`AGENTS.md` §5.4). The single
    // argument is a scalar; the call reads no caller memory.
    let ret = unsafe { raw_syscall(NUM_USERS_DB_WAIT, [timeout_ns, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Make a synchronous capability-checked call to the kernel-owned IPC call
/// endpoint `endpoint`: post `request`, block until the reply arrives, and
/// copy it into `reply` (`SyscallNumber::IPC_CALL`, `AGENTS.md` §5.2 / §5.4;
/// Design D D2b). Returns the number of reply bytes written.
///
/// The kernel enforces the endpoint's required send capability against the
/// caller before posting (`AGENTS.md` §5.2 — no ambient authority), copies
/// `request` in and the reply out through the validated boundary, and blocks
/// the caller cooperatively until the reply arrives, never busy-spinning
/// (`AGENTS.md` §2.1). The first consumer is the reactive device manager
/// reading the read-only `/System` driver store over
/// [`rustos_abi::driver_store::DRIVER_STORE_ENDPOINT`].
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure: a missing
/// send capability (`PermissionDenied`), an unknown or destroyed endpoint
/// (`NotFound`), an oversize request (`MessageTooLarge`), a reply larger than
/// `reply` (`BufferTooSmall`), or no call-endpoint registry wired
/// (`NotImplemented`). The wrapper hides no error (`AGENTS.md` §2.9).
pub fn ipc_call(endpoint: u64, request: &[u8], reply: &mut [u8]) -> Result<usize, i64> {
    let req_ptr = request.as_ptr() as usize as u64;
    let reply_ptr = reply.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `(ptr, len)` pairs against the caller's address space before
    // touching them (`AGENTS.md` §5.4). `request` is a live shared `&[u8]`
    // and `reply` a live exclusive `&mut [u8]` for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe {
        raw_syscall(
            NUM_IPC_CALL,
            [
                endpoint,
                req_ptr,
                request.len() as u64,
                reply_ptr,
                reply.len() as u64,
                0,
            ],
        )
    } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice in the caller
    // (`AGENTS.md` §5.4), exactly as `hw_tree_read` clamps.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(reply.len()))
}

/// Create and register a kernel-owned synchronous call endpoint the calling
/// task then *serves* (`SyscallNumber::CALL_CREATE`, `AGENTS.md` §5.2 / §5.4;
/// Design D D3 — the server half of [`ipc_call`]).
///
/// `endpoint` is the well-known id callers name in [`ipc_call`]; `send_caps`
/// is the capability a caller must hold to post and `recv_caps` the
/// capability this task must hold to [`call_recv`]/[`call_reply`];
/// `max_request`/`max_reply`/`capacity` bound the endpoint. Binding a
/// restricted-sender endpoint (non-empty `send_caps`) requires
/// `CAP_IPC_BIND_PRIVILEGED`, enforced kernel-side.
///
/// Returns `0` on success, or the raw negative kernel result (`-errno`): a
/// missing bind capability (`PermissionDenied`), an id already bound
/// (`AlreadyExists`), oversize bounds (`LengthOutOfRange`), or no
/// call-endpoint registry wired (`NotImplemented`). The wrapper hides no
/// error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn call_create(
    endpoint: u64,
    send_caps: &rustos_caps::CapabilitySet,
    recv_caps: &rustos_caps::CapabilitySet,
    max_request: usize,
    max_reply: usize,
    capacity: usize,
) -> i64 {
    // Marshal both capability sets to their fixed `WIRE_LEN` images on the
    // stack and hand the kernel their pointers; the kernel copies them in
    // through the validated boundary (`AGENTS.md` §5.4).
    let send_bytes = send_caps.to_le_bytes();
    let recv_bytes = recv_caps.to_le_bytes();
    let send_ptr = send_bytes.as_ptr() as usize as u64;
    let recv_ptr = recv_bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `CapabilitySet` pointers against the caller's address space before
    // reading them (`AGENTS.md` §5.4). `send_bytes`/`recv_bytes` live for the
    // duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_CALL_CREATE,
            [
                endpoint,
                send_ptr,
                recv_ptr,
                max_request as u64,
                max_reply as u64,
                capacity as u64,
            ],
        )
    };
    ret as i64
}

/// Receive the next request posted to a call endpoint this task owns,
/// blocking until one arrives (`SyscallNumber::CALL_RECV`, `AGENTS.md` §5.4;
/// Design D D3 — the server-side receive half).
///
/// On success the request payload is copied into `buf`, the per-call ticket
/// (to answer with [`call_reply`]) is written to `ticket_out`, and the number
/// of request bytes is returned. The kernel parks the caller cooperatively
/// until a request is posted, never busy-spinning (`AGENTS.md` §2.1).
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`): a request larger than
/// `buf` (`BufferTooSmall`, left queued), a missing receive capability or a
/// foreign endpoint (`PermissionDenied`), or an unknown/destroyed endpoint
/// (`NotFound`). The wrapper hides no error (`AGENTS.md` §2.9).
pub fn call_recv(endpoint: u64, buf: &mut [u8], ticket_out: &mut u64) -> Result<usize, i64> {
    let buf_ptr = buf.as_mut_ptr() as usize as u64;
    let ticket_ptr = (ticket_out as *mut u64) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both pointers against the caller's address space before touching them
    // (`AGENTS.md` §5.4). `buf` is a live exclusive `&mut [u8]` and
    // `ticket_out` a live `&mut u64` for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe {
        raw_syscall(
            NUM_CALL_RECV,
            [endpoint, buf_ptr, buf.len() as u64, ticket_ptr, 0, 0],
        )
    } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice (`AGENTS.md` §5.4).
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(buf.len()))
}

/// Answer one received call on an endpoint this task owns, releasing the
/// blocked caller (`SyscallNumber::CALL_REPLY`, `AGENTS.md` §5.4; Design D D3
/// — the server-side reply half).
///
/// `ticket` is the value [`call_recv`] wrote; `reply` is the reply payload.
/// Returns `0` on success, or the raw negative kernel result (`-errno`): a
/// reply larger than the endpoint's `max_reply` (`MessageTooLarge`), an
/// unknown or already-answered ticket or unknown endpoint (`NotFound`), or a
/// missing receive capability / foreign endpoint (`PermissionDenied`). The
/// wrapper hides no error (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn call_reply(endpoint: u64, ticket: u64, reply: &[u8]) -> i64 {
    let reply_ptr = reply.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // reply `(ptr, len)` pair against the caller's address space before
    // reading it (`AGENTS.md` §5.4). `reply` is a live shared `&[u8]` for the
    // duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_CALL_REPLY,
            [endpoint, ticket, reply_ptr, reply.len() as u64, 0, 0],
        )
    };
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
    fn ipc_send_marshals_endpoint_pointer_and_len() {
        let payload = *b"reply-record";
        let (number, args) = capture(0, || {
            assert_eq!(ipc_send(42, &payload), 0);
        });
        assert_eq!(number, NUM_IPC_SEND);
        assert_eq!(args[0], 42);
        assert_eq!(args[1], payload.as_ptr() as usize as u64);
        assert_eq!(args[2], payload.len() as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn ipc_send_surfaces_negative_errno_encoding() {
        // `NotFound` (unbound endpoint) is encoded as the two's-complement
        // negation; the wrapper hands that signed value back unchanged.
        let payload = [0u8; 4];
        let want = -i64::from(rustos_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(ipc_send(7, &payload), want);
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
    fn stdin_reports_a_negative_errno_as_end_of_input() {
        // A failure (fd 0 not readable, faulting buffer) is encoded as a
        // negative register; a `&mut [u8]` reader cannot carry an `Errno`, so
        // it surfaces as a zero-length read (end of input), never a huge
        // count that would slice out of bounds.
        let mut buffer = [0u8; 16];
        let neg =
            u64::from_ne_bytes((-i64::from(rustos_abi::Errno::NotFound.as_i32())).to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(stdin(&mut buffer), 0);
        });
    }

    #[test]
    fn stdin_clamps_an_oversized_count_to_the_buffer_length() {
        // Defence in depth: a count larger than the buffer (a buggy kernel)
        // is clamped so the caller can never index past `buf.len()`.
        let mut buffer = [0u8; 16];
        let (_, _) = capture(99, || {
            assert_eq!(stdin(&mut buffer), 16);
        });
    }

    #[test]
    fn spawn_marshals_path_pointer_len_and_inherit() {
        let path = *b"/Apps/Shell.app/Run";
        let (number, args) = capture(7, || {
            assert_eq!(spawn(&path), 7);
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        // The plain `spawn` keeps the child on the caller's own console.
        assert_eq!(args[2], CONSOLE_INHERIT);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn spawn_at_marshals_the_console_index() {
        let path = *b"/System/Services/login";
        let (number, args) = capture(8, || {
            assert_eq!(spawn_at(&path, 1), 8);
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], 1);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn console_count_marshals_no_arguments_and_surfaces_count() {
        let (number, args) = capture(2, || {
            assert_eq!(console_count(), 2);
        });
        assert_eq!(number, NUM_CONSOLE_COUNT);
        assert_eq!(args, [0; 6]);
    }

    #[test]
    fn set_echo_marshals_stdin_fd_and_the_enabled_flag() {
        // Enabling echo marshals fd 0 and a non-zero flag.
        let (number, args) = capture(0, || {
            assert_eq!(set_echo(true), 0);
        });
        assert_eq!(number, NUM_STREAM_ECHO);
        assert_eq!(args[0], u64::from(STDIN));
        assert_eq!(args[1], 1);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);

        // Disabling echo marshals fd 0 and a zero flag.
        let (_, args) = capture(0, || {
            assert_eq!(set_echo(false), 0);
        });
        assert_eq!(args[0], u64::from(STDIN));
        assert_eq!(args[1], 0);
    }

    #[test]
    fn key_inject_marshals_the_record_pointer_and_len() {
        use rustos_abi::input::{KeyValue, Modifiers};
        let record = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        };
        let want = i64::try_from(KeyInput::WIRE_LEN).expect("WIRE_LEN fits an i64");
        let (number, args) = capture(KeyInput::WIRE_LEN as u64, || {
            assert_eq!(key_inject(&record), want);
        });
        assert_eq!(number, NUM_KEY_INJECT);
        // arg 0 is the record buffer pointer; arg 1 is its WIRE_LEN.
        assert_ne!(args[0], 0);
        assert_eq!(args[1], KeyInput::WIRE_LEN as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn key_inject_surfaces_negative_errno_encoding() {
        use rustos_abi::input::{KeyValue, Modifiers};
        // An unwired arbiter refuses the inject with `NotImplemented`; the
        // wrapper surfaces the raw `-errno` register.
        let record = KeyInput::Pressed {
            key: KeyValue::Char('x'),
            modifiers: Modifiers::default(),
        };
        let want = -i64::from(rustos_abi::Errno::NotImplemented.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(key_inject(&record), want);
        });
    }

    #[test]
    fn display_acquire_and_release_marshal_no_arguments() {
        let (number, args) = capture(0, || {
            assert_eq!(display_acquire(), 0);
        });
        assert_eq!(number, NUM_DISPLAY_ACQUIRE);
        assert_eq!(args, [0; 6]);

        let (number, args) = capture(0, || {
            assert_eq!(display_release(), 0);
        });
        assert_eq!(number, NUM_DISPLAY_RELEASE);
        assert_eq!(args, [0; 6]);
    }

    #[test]
    fn keyboard_read_marshals_the_buffer_pointer_and_len() {
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        let want = i64::try_from(KeyInput::WIRE_LEN).expect("WIRE_LEN fits an i64");
        let (number, args) = capture(KeyInput::WIRE_LEN as u64, || {
            assert_eq!(keyboard_read(&mut buf), want);
        });
        assert_eq!(number, NUM_KEYBOARD_READ);
        assert_ne!(args[0], 0);
        assert_eq!(args[1], KeyInput::WIRE_LEN as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn set_echo_surfaces_negative_errno_encoding() {
        // A console-less build refuses the toggle with `NotImplemented`;
        // the wrapper surfaces the raw `-errno` register unchanged.
        let want = -i64::from(rustos_abi::Errno::NotImplemented.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(set_echo(true), want);
        });
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
    fn mem_map_marshals_len_flags_and_addr_hint() {
        // A FIXED placement at a page-aligned hint; the kernel returns the
        // base address, which the wrapper surfaces as a non-negative i64.
        let base = 0x10_0100_0000u64;
        let want = i64::try_from(base).expect("base fits an i64");
        let (number, args) = capture(base, || {
            assert_eq!(mem_map(0x2000, MapFlags::FIXED, base), want);
        });
        assert_eq!(number, NUM_MEM_MAP);
        assert_eq!(args[0], 0x2000);
        assert_eq!(args[1], u64::from(MapFlags::FIXED.bits()));
        assert_eq!(args[2], base);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn mem_map_surfaces_negative_errno_encoding() {
        // `OutOfMemory` is encoded by the kernel as the two's-complement
        // negation; the wrapper hands that signed value back unchanged.
        let want = -i64::from(rustos_abi::Errno::OutOfMemory.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(mem_map(0x1000, MapFlags::empty(), 0), want);
        });
    }

    #[test]
    fn mem_unmap_marshals_base_and_len() {
        let base = 0x10_0100_0000u64;
        let (number, args) = capture(0, || {
            assert_eq!(mem_unmap(base, 0x2000), 0);
        });
        assert_eq!(number, NUM_MEM_UNMAP);
        assert_eq!(args[0], base);
        assert_eq!(args[1], 0x2000);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn mem_unmap_surfaces_negative_errno_encoding() {
        let want = -i64::from(rustos_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(mem_unmap(0x10_0100_0000, 0x1000), want);
        });
    }

    #[test]
    fn wait_marshals_pid_and_status_pointer() {
        let mut status = 0i32;
        let ptr = core::ptr::addr_of_mut!(status) as usize as u64;
        // The kernel returns the reaped child's PID (non-negative).
        let (number, args) = capture(5, || {
            assert_eq!(wait(9, &mut status), 5);
        });
        assert_eq!(number, NUM_WAIT);
        assert_eq!(args[0], 9);
        assert_eq!(args[1], ptr);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn wait_marshals_wait_any_as_a_sign_extended_minus_one() {
        let mut status = 0i32;
        let (number, args) = capture(3, || {
            assert_eq!(wait(rustos_abi::WAIT_ANY, &mut status), 3);
        });
        assert_eq!(number, NUM_WAIT);
        // `WAIT_ANY` (-1) sign-extends to all-ones in the argument register.
        assert_eq!(args[0], u64::MAX);
    }

    #[test]
    fn wait_surfaces_negative_errno_encoding() {
        // `NotFound` (no such child) is encoded as the two's-complement
        // negation; the wrapper hands that signed value back unchanged.
        let mut status = 0i32;
        let want = -i64::from(rustos_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(wait(9, &mut status), want);
        });
    }

    #[test]
    fn rlimit_get_marshals_kind_and_pointer_and_decodes_result() {
        // The seam returns 0 (success) and leaves the buffer zeroed, so the
        // wrapper decodes a `{soft: 0, hard: 0}` limit and reports it.
        let (number, args) = capture(0, || {
            assert_eq!(
                rlimit_get(LimitKind::Processes),
                Ok(ResourceLimit::new(0, 0).expect("well-formed"))
            );
        });
        assert_eq!(number, NUM_RLIMIT_GET);
        assert_eq!(args[0], u64::from(LimitKind::Processes.as_u32()));
        assert_ne!(args[1], 0); // a non-null out pointer
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn rlimit_get_surfaces_negative_errno_encoding() {
        let want = -i64::from(rustos_abi::Errno::OutOfRange.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(rlimit_get(LimitKind::OpenStreams), Err(want));
        });
    }

    #[test]
    fn rlimit_set_marshals_kind_and_pointer() {
        let limit = ResourceLimit::new(0x1000, 0x2000).expect("well-formed");
        let (number, args) = capture(0, || {
            assert_eq!(rlimit_set(LimitKind::AddressSpaceBytes, limit), 0);
        });
        assert_eq!(number, NUM_RLIMIT_SET);
        assert_eq!(args[0], u64::from(LimitKind::AddressSpaceBytes.as_u32()));
        assert_ne!(args[1], 0); // a non-null value pointer
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn rlimit_set_surfaces_negative_errno_encoding() {
        let want = -i64::from(rustos_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(
                rlimit_set(LimitKind::StackBytes, ResourceLimit::UNLIMITED),
                want
            );
        });
    }

    #[test]
    fn i32_arg_sign_extends() {
        assert_eq!(i32_arg(0), 0);
        assert_eq!(i32_arg(1), 1);
        assert_eq!(i32_arg(-1), u64::MAX);
        assert_eq!(i32_arg(i32::MIN), 0xFFFF_FFFF_8000_0000);
    }

    #[test]
    fn clock_get_issues_a_zero_arg_trap_and_returns_the_reading() {
        let reading = 1_234_567_000u64;
        let (number, args) = capture(reading, || {
            assert_eq!(clock_get(), reading);
        });
        assert_eq!(number, NUM_CLOCK_GET);
        // `clock_get` takes no arguments and no memory operand.
        assert_eq!(args, [0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn clock_delay_now_us_floors_nanoseconds_to_microseconds() {
        use rustos_abi::Delay;
        // 1_999 ns floors to 1 µs — never rounds up past the true reading.
        let (number, _) = capture(1_999, || {
            assert_eq!(ClockDelay::new().now_us(), 1);
        });
        assert_eq!(number, NUM_CLOCK_GET);
    }

    #[test]
    fn spin_until_ns_returns_immediately_for_a_past_deadline() {
        // A deadline already reached must not yield even once (`AGENTS.md`
        // §2.1 — no needless reschedule).
        let mut yields = 0u32;
        spin_until_ns(100, || 100, || yields += 1);
        assert_eq!(yields, 0);
        // Strictly-past as well.
        spin_until_ns(50, || 100, || yields += 1);
        assert_eq!(yields, 0);
    }

    #[test]
    fn spin_until_ns_yields_until_the_clock_reaches_the_deadline() {
        // The clock advances by 250 ns per read; the loop must yield until it
        // is at least the 1_000 ns deadline, then stop.
        let clock = core::cell::Cell::new(0u64);
        let now = || {
            let t = clock.get();
            clock.set(t + 250);
            t
        };
        let mut yields = 0u32;
        spin_until_ns(1_000, now, || yields += 1);
        // Reads at 0,250,500,750 are below the deadline (4 yields); the read
        // at 1_000 stops the loop.
        assert_eq!(yields, 4);
    }

    #[test]
    fn hw_tree_read_marshals_the_buffer_pointer_and_len() {
        let mut buf = [0u8; 256];
        let (number, args) = capture(16, || {
            assert_eq!(hw_tree_read(&mut buf), Ok(16));
        });
        assert_eq!(number, NUM_HW_TREE_READ);
        assert_ne!(args[0], 0); // a non-null out pointer
        assert_eq!(args[1], 256);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn hw_tree_read_clamps_an_oversized_count_to_the_buffer_length() {
        // A kernel count larger than the buffer is clamped, never trusted
        // into an out-of-bounds slice (`AGENTS.md` §5.4).
        let mut buf = [0u8; 8];
        let (_, _) = capture(9999, || {
            assert_eq!(hw_tree_read(&mut buf), Ok(8));
        });
    }

    #[test]
    fn hw_tree_read_surfaces_negative_errno_encoding() {
        // `BufferTooSmall` is encoded as the two's-complement negation; the
        // wrapper hands that signed value back unchanged.
        let mut buf = [0u8; 4];
        let want = -i64::from(rustos_abi::Errno::BufferTooSmall.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(hw_tree_read(&mut buf), Err(want));
        });
    }

    #[test]
    fn hw_tree_wait_marshals_generation_and_timeout() {
        let (number, args) = capture(0, || {
            assert_eq!(hw_tree_wait(7, u64::MAX), 0);
        });
        assert_eq!(number, NUM_HW_TREE_WAIT);
        assert_eq!(args[0], 7);
        assert_eq!(args[1], u64::MAX);
        // No memory operand.
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn hw_tree_wait_surfaces_negative_errno_encoding() {
        // `TimedOut` is encoded as the two's-complement negation; the
        // wrapper hands that signed value back unchanged.
        let want = -i64::from(rustos_abi::Errno::TimedOut.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(hw_tree_wait(3, 0), want);
        });
    }

    #[test]
    fn users_db_wait_marshals_the_timeout() {
        let (number, args) = capture(0, || {
            assert_eq!(users_db_wait(u64::MAX), 0);
        });
        assert_eq!(number, NUM_USERS_DB_WAIT);
        assert_eq!(args[0], u64::MAX);
        // No memory operand; the only argument is the scalar timeout.
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn users_db_wait_surfaces_negative_errno_encoding() {
        // `TimedOut` is encoded as the two's-complement negation; the
        // wrapper hands that signed value back unchanged.
        let want = -i64::from(rustos_abi::Errno::TimedOut.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(users_db_wait(0), want);
        });
    }

    #[test]
    fn ipc_call_marshals_endpoint_and_both_buffers() {
        let request = [0xAAu8; 5];
        let mut reply = [0u8; 64];
        let (number, args) = capture(12, || {
            assert_eq!(
                ipc_call(
                    rustos_abi::driver_store::DRIVER_STORE_ENDPOINT,
                    &request,
                    &mut reply
                ),
                Ok(12)
            );
        });
        assert_eq!(number, NUM_IPC_CALL);
        assert_eq!(args[0], rustos_abi::driver_store::DRIVER_STORE_ENDPOINT);
        assert_ne!(args[1], 0); // request pointer
        assert_eq!(args[2], 5); // request len
        assert_ne!(args[3], 0); // reply pointer
        assert_eq!(args[4], 64); // reply capacity
        assert_eq!(args[5], 0);
    }

    #[test]
    fn ipc_call_clamps_an_oversized_count_to_the_reply_length() {
        let mut reply = [0u8; 8];
        let (_, _) = capture(9999, || {
            assert_eq!(ipc_call(1, &[], &mut reply), Ok(8));
        });
    }

    #[test]
    fn ipc_call_surfaces_negative_errno_encoding() {
        let mut reply = [0u8; 4];
        let want = -i64::from(rustos_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(ipc_call(1, &[1, 2], &mut reply), Err(want));
        });
    }
}
