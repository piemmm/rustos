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

/// `console_count` syscall number (`AGENTS.md` §2.2, as above).
const NUM_CONSOLE_COUNT: u64 = SyscallNumber::CONSOLE_COUNT.as_u16() as u64;

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
}
