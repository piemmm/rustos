//! `rustos-rt` — the pure-Rust userland runtime.
//!
//! This is the runtime a **first-party RustOS program written in Rust** links:
//! it provides the program's `_start` entry trampoline, idiomatic `abi-v1`
//! syscall wrappers, the [`entry!`] macro that names the program's `main`, and
//! the panic handler. RustOS is Rust-only, so its own
//! programs use this runtime and never the C ABI.
//!
//! # Relationship to the C ABI (`crt0` + `abi-sys`)
//!
//! `rustos-crt0` and `rustos-abi-sys` are the curated *System runtime / C ABI*
//! class: a libc-equivalent that exists **solely** so
//! a program **not** written in Rust (C, …) can call `abi-v1`. They are not
//! for RustOS's own code. `rustos-rt` is the Rust counterpart; both build on
//! the one shared syscall trap (`rustos-abi-trap`), so the
//! trap assembly is not duplicated.
//!
//! # Not a privileged path
//!
//! The wrappers add **no** authority. Every capability check and input
//! validation happens kernel-side, on the far side of the trap; a Rust program reaches no syscall it could not reach otherwise.
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
//! startup vector, installs the per-process stack canary,
//! calls `main`, and routes its return value through the `exit` syscall.
//!
//! # Targets
//!
//! The `_start` trampoline, stack-canary symbols, and panic handler are
//! compiled in only for the three native Tier-1 targets, gated on a
//! build-script-emitted `rt_native_<arch>` cfg (`build.rs`) rather than a
//! target-architecture predicate, so the instruction-set choice stays out of
//! the source tree the `cfg-check` guards. On the host only the
//! host-testable syscall-wrapper marshalling is compiled.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::input::KeyInput;
use rustos_abi::waitset::{WaitSetOp, WaitSourceKind};
use rustos_abi::{
    BootId, FileStat, HwNode, LimitKind, MapFlags, OpenFlags, ResourceLimit, SyscallNumber,
    TerminalSize, Time64, WallClockReading, WallTimeState, BOOT_ID_LEN, CONSOLE_INHERIT,
    SPAWN_UID_INHERIT, STDERR, STDIN, STDINFO, STDOUT, TERMINAL_SIZE_WIRE_LEN,
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
// free.
#[cfg(any(rt_native, test))]
mod heap;

/// `exit` syscall number, read from the `abi-v1` source of truth so this
/// crate can never disagree with the table.
const NUM_EXIT: u64 = SyscallNumber::EXIT.as_u16() as u64;

/// `stream_write` syscall number (as above).
const NUM_STREAM_WRITE: u64 = SyscallNumber::STREAM_WRITE.as_u16() as u64;

/// `stream_read` syscall number (as above).
const NUM_STREAM_READ: u64 = SyscallNumber::STREAM_READ.as_u16() as u64;

/// `yield` syscall number (as above).
const NUM_YIELD: u64 = SyscallNumber::YIELD.as_u16() as u64;

/// `spawn` syscall number (as above).
const NUM_SPAWN: u64 = SyscallNumber::SPAWN.as_u16() as u64;

/// `mem_map` syscall number (as above).
const NUM_MEM_MAP: u64 = SyscallNumber::MEM_MAP.as_u16() as u64;

/// `mem_unmap` syscall number (as above).
const NUM_MEM_UNMAP: u64 = SyscallNumber::MEM_UNMAP.as_u16() as u64;

/// `mmio_map` syscall number (as above).
const NUM_MMIO_MAP: u64 = SyscallNumber::MMIO_MAP.as_u16() as u64;

/// `dma_alloc` syscall number (as above).
const NUM_DMA_ALLOC: u64 = SyscallNumber::DMA_ALLOC.as_u16() as u64;

/// `dma_free` syscall number (as above).
const NUM_DMA_FREE: u64 = SyscallNumber::DMA_FREE.as_u16() as u64;

/// `wait` syscall number (as above).
const NUM_WAIT: u64 = SyscallNumber::WAIT.as_u16() as u64;

/// `ipc_send` syscall number (as above).
const NUM_IPC_SEND: u64 = SyscallNumber::IPC_SEND.as_u16() as u64;

/// `rlimit_get` syscall number (as above).
const NUM_RLIMIT_GET: u64 = SyscallNumber::RLIMIT_GET.as_u16() as u64;

/// `rlimit_set` syscall number (as above).
const NUM_RLIMIT_SET: u64 = SyscallNumber::RLIMIT_SET.as_u16() as u64;

/// `users_db_read` syscall number (as above).
const NUM_USERS_DB_READ: u64 = SyscallNumber::USERS_DB_READ.as_u16() as u64;

/// `users_db_wait` syscall number (as above).
const NUM_USERS_DB_WAIT: u64 = SyscallNumber::USERS_DB_WAIT.as_u16() as u64;

/// `console_count` syscall number (as above).
const NUM_CONSOLE_COUNT: u64 = SyscallNumber::CONSOLE_COUNT.as_u16() as u64;

/// `stream_echo` syscall number (as above).
const NUM_STREAM_ECHO: u64 = SyscallNumber::STREAM_ECHO.as_u16() as u64;

/// `key_inject` syscall number (as above).
const NUM_KEY_INJECT: u64 = SyscallNumber::KEY_INJECT.as_u16() as u64;

/// `display_acquire` syscall number (as above).
const NUM_DISPLAY_ACQUIRE: u64 = SyscallNumber::DISPLAY_ACQUIRE.as_u16() as u64;

/// `display_release` syscall number (as above).
const NUM_DISPLAY_RELEASE: u64 = SyscallNumber::DISPLAY_RELEASE.as_u16() as u64;

/// `keyboard_read` syscall number (as above).
const NUM_KEYBOARD_READ: u64 = SyscallNumber::KEYBOARD_READ.as_u16() as u64;

/// `resource_grants` syscall number (as above).
const NUM_RESOURCE_GRANTS: u64 = SyscallNumber::RESOURCE_GRANTS.as_u16() as u64;

/// `clock_get` syscall number (as above).
const NUM_CLOCK_GET: u64 = SyscallNumber::CLOCK_GET.as_u16() as u64;

/// `hw_tree_read` syscall number (as above).
const NUM_HW_TREE_READ: u64 = SyscallNumber::HW_TREE_READ.as_u16() as u64;

/// `hw_tree_wait` syscall number (as above).
const NUM_HW_TREE_WAIT: u64 = SyscallNumber::HW_TREE_WAIT.as_u16() as u64;

/// `ipc_call` syscall number (as above).
const NUM_IPC_CALL: u64 = SyscallNumber::IPC_CALL.as_u16() as u64;

/// `irq_bind` syscall number (as above).
const NUM_IRQ_BIND: u64 = SyscallNumber::IRQ_BIND.as_u16() as u64;

/// `irq_wait` syscall number (as above).
const NUM_IRQ_WAIT: u64 = SyscallNumber::IRQ_WAIT.as_u16() as u64;

/// `call_create` syscall number (as above).
const NUM_CALL_CREATE: u64 = SyscallNumber::CALL_CREATE.as_u16() as u64;

/// `call_recv` syscall number (as above).
const NUM_CALL_RECV: u64 = SyscallNumber::CALL_RECV.as_u16() as u64;

/// `call_reply` syscall number (as above).
const NUM_CALL_REPLY: u64 = SyscallNumber::CALL_REPLY.as_u16() as u64;

/// `log_emit` syscall number (as above).
const NUM_LOG_EMIT: u64 = SyscallNumber::LOG_EMIT.as_u16() as u64;

/// `hw_emit_node` syscall number (as above).
const NUM_HW_EMIT_NODE: u64 = SyscallNumber::HW_EMIT_NODE.as_u16() as u64;

/// `hw_remove_node` syscall number (as above).
const NUM_HW_REMOVE_NODE: u64 = SyscallNumber::HW_REMOVE_NODE.as_u16() as u64;

/// `shm_create` syscall number (as above).
const NUM_SHM_CREATE: u64 = SyscallNumber::SHM_CREATE.as_u16() as u64;

/// `shm_map` syscall number (as above).
const NUM_SHM_MAP: u64 = SyscallNumber::SHM_MAP.as_u16() as u64;

/// `shm_unmap` syscall number (as above).
const NUM_SHM_UNMAP: u64 = SyscallNumber::SHM_UNMAP.as_u16() as u64;

/// `waitset_create` syscall number (as above).
const NUM_WAITSET_CREATE: u64 = SyscallNumber::WAITSET_CREATE.as_u16() as u64;

/// `waitset_ctl` syscall number (as above).
const NUM_WAITSET_CTL: u64 = SyscallNumber::WAITSET_CTL.as_u16() as u64;

/// `waitset_wait` syscall number (as above).
const NUM_WAITSET_WAIT: u64 = SyscallNumber::WAITSET_WAIT.as_u16() as u64;

/// `msi_alloc` syscall number (as above).
const NUM_MSI_ALLOC: u64 = SyscallNumber::MSI_ALLOC.as_u16() as u64;

/// `fs_open` syscall number (as above).
const NUM_FS_OPEN: u64 = SyscallNumber::FS_OPEN.as_u16() as u64;

/// `fs_close` syscall number (as above).
const NUM_FS_CLOSE: u64 = SyscallNumber::FS_CLOSE.as_u16() as u64;

/// `fs_read` syscall number (as above).
const NUM_FS_READ: u64 = SyscallNumber::FS_READ.as_u16() as u64;

/// `fs_write` syscall number (as above).
const NUM_FS_WRITE: u64 = SyscallNumber::FS_WRITE.as_u16() as u64;

/// `fs_readdir` syscall number (as above).
const NUM_FS_READDIR: u64 = SyscallNumber::FS_READDIR.as_u16() as u64;

/// `fs_stat` syscall number (as above).
const NUM_FS_STAT: u64 = SyscallNumber::FS_STAT.as_u16() as u64;

/// `fs_truncate` syscall number (as above).
const NUM_FS_TRUNCATE: u64 = SyscallNumber::FS_TRUNCATE.as_u16() as u64;

/// `fs_sync` syscall number (as above).
const NUM_FS_SYNC: u64 = SyscallNumber::FS_SYNC.as_u16() as u64;

/// `fs_mkdir` syscall number (as above).
const NUM_FS_MKDIR: u64 = SyscallNumber::FS_MKDIR.as_u16() as u64;

/// `fs_unlink` syscall number (as above).
const NUM_FS_UNLINK: u64 = SyscallNumber::FS_UNLINK.as_u16() as u64;
const NUM_FS_RENAME: u64 = SyscallNumber::FS_RENAME.as_u16() as u64;

/// `call_peer_origin` syscall number (as above).
const NUM_CALL_PEER_ORIGIN: u64 = SyscallNumber::CALL_PEER_ORIGIN.as_u16() as u64;

/// `wall_time_get` syscall number (as above).
const NUM_WALL_TIME_GET: u64 = SyscallNumber::WALL_TIME_GET.as_u16() as u64;

/// `wall_time_set` syscall number (as above).
const NUM_WALL_TIME_SET: u64 = SyscallNumber::WALL_TIME_SET.as_u16() as u64;

/// `boot_id_get` syscall number (as above).
const NUM_BOOT_ID_GET: u64 = SyscallNumber::BOOT_ID_GET.as_u16() as u64;

/// `sysinfo_introspect` syscall number (as above).
const NUM_SYSINFO_INTROSPECT: u64 = SyscallNumber::SYSINFO_INTROSPECT.as_u16() as u64;

/// `terminal_size` syscall number (as above).
const NUM_TERMINAL_SIZE: u64 = SyscallNumber::TERMINAL_SIZE.as_u16() as u64;

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
/// terminating syscall, not a busy-wait.
pub fn exit(code: i32) -> ! {
    loop {
        // SAFETY: `raw_syscall` is always safe to invoke — the kernel
        // validates the call on the far side of the trap.
        // `exit` consumes the exit code in arg 0 and takes no memory operand.
        unsafe {
            let _ = raw_syscall(NUM_EXIT, [i32_arg(code), 0, 0, 0, 0, 0]);
        }
    }
}

/// Write `bytes` to the calling process's standard stream `fd`
/// (`SyscallNumber::STREAM_WRITE`), returning the number of bytes the
/// kernel accepted.
///
/// The shared core of [`stdout`], [`stderr`], and [`stdinfo`]: the
/// program names only the inherited descriptor, never a device, so the
/// same binary works whatever the spawner backed the stream with (device independence is a property of the stream layer). The kernel
/// resolves `fd` against the caller's descriptor table and validates the
/// `(buf, len)` pair against the caller's address space before reading it; a short write (fewer than `bytes.len()`) is valid,
/// so the caller loops.
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the count never exceeds `bytes.len()`.
fn stream_write(fd: u32, bytes: &[u8]) -> usize {
    let ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(buf, len)` against the caller's address space before touching it. `bytes` is a live shared `&[u8]` for the duration
    // of the call, so the `(ptr, len)` pair denotes readable memory.
    let written = unsafe {
        raw_syscall(
            NUM_STREAM_WRITE,
            [u64::from(fd), ptr, bytes.len() as u64, 0, 0, 0],
        )
    };
    written as usize
}

/// Write `bytes` to standard output (fd 1), returning the
/// number of bytes the kernel accepted. The program's primary data
/// output; a short write is valid, so the caller loops.
#[must_use]
pub fn stdout(bytes: &[u8]) -> usize {
    stream_write(STDOUT, bytes)
}

/// Write `bytes` to standard error (fd 2): errors,
/// warnings, and diagnostics. Returns the number of bytes accepted.
#[must_use]
pub fn stderr(bytes: &[u8]) -> usize {
    stream_write(STDERR, bytes)
}

/// Write `bytes` to the standard information stream (fd 3): optional, ignorable structured advisory metadata. Returns the
/// number of bytes accepted (zero when no consumer is attached — fd 3 is
/// best-effort and must never affect correctness).
#[must_use]
pub fn stdinfo(bytes: &[u8]) -> usize {
    stream_write(STDINFO, bytes)
}

/// Read up to `buf.len()` bytes from standard input (fd 0) into `buf` (`SyscallNumber::STREAM_READ`), returning the number of
/// bytes read.
///
/// The kernel resolves fd 0 against the caller's descriptor table and
/// validates the `(buf, len)` pair against the caller's address space
/// before writing it. The stream *backing* owns
/// blocking: a read with no pending input parks the caller in the
/// kernel until input arrives, so a successful read returns at least one
/// byte. A short read (fewer bytes than `buf.len()`) is valid, so the
/// caller loops for more.
///
/// The kernel encodes a failure as a negative register (`-errno`, the
/// standard `abi-v1` convention) — e.g. fd 0 is not a readable stream, or
/// the buffer pointer faults. A reader handed a `&mut [u8]` has no way to
/// surface an `Errno`, and an unread input stream is indistinguishable from
/// end-of-input from the program's side (the *backing* owns blocking),
/// so this reports a failure as a zero-length read. The count is also
/// clamped to `buf.len()` as defence in depth, so a buggy kernel count can
/// never drive an out-of-bounds slice in the caller.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the clamped count never exceeds `buf.len()`.
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 stream-read encoding (count ≥ 0, else -errno).
#[allow(clippy::cast_sign_loss)] // The negative (`-errno`) case returns early above; the cast runs only when `read >= 0`.
pub fn stdin(buf: &mut [u8]) -> usize {
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(buf, len)` against the caller's address space before touching it. `buf` is a live exclusive `&mut [u8]` for the
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
/// console (`SyscallNumber::STREAM_ECHO` — terminal local
/// echo), returning the raw signed register (`0` on success, else
/// `-errno`).
///
/// Console echo defaults to **on**, so an interactive user sees what they
/// type at a [`stdin`] read. A program suppresses it around a secret it
/// must not render — login disables echo before reading a password and
/// re-enables it afterwards (never echo a credential).
/// Requires `CAP_CONSOLE_READ`; the kernel performs the echo itself as part
/// of the read line discipline, so no `CAP_CONSOLE_WRITE` is needed. A
/// build with no console wired, or an fd 0 that is not a readable stream,
/// fails closed with `-errno`; the wrapper surfaces it
/// verbatim so the caller decides how to react.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn set_echo(enabled: bool) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates the capability and resolves fd 0
    // before touching any state.
    let ret = unsafe {
        raw_syscall(
            NUM_STREAM_ECHO,
            [u64::from(STDIN), u64::from(enabled), 0, 0, 0, 0],
        )
    };
    ret as i64
}

/// Inject one decoded keyboard `record` into the kernel input-focus arbiter
/// (`SyscallNumber::KEY_INJECT`, `plans/PI.md` P11 — input
/// follows the surface owner), returning the raw signed register (the bytes
/// consumed when non-negative, else `-errno`).
///
/// The producer-side call a keyboard-input driver issues after decoding a
/// directly attached keyboard into a [`KeyInput`] key edge: the kernel
/// validates `CAP_INPUT_INJECT` and the `(buf, len)` pair against the
/// caller's address space, decodes the record fail-closed,
/// and routes it by who holds input focus — a *press* encoded to the focused
/// text console's tty bytes, or the whole record delivered to the desktop
/// keyboard channel. The driver no longer chooses the encoding or the
/// destination. A malformed record or an unwired arbiter
/// fails closed with `-errno`; the wrapper surfaces the
/// raw signed value so the caller decides how to react.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn key_inject(record: &KeyInput) -> i64 {
    let bytes = record.to_le_bytes();
    let ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_INPUT_INJECT` and the `(buf, len)` pair against the caller's
    // address space before reading it. `bytes` is a live
    // stack array for the duration of the call, so the `(ptr, len)` pair
    // denotes readable memory.
    let ret = unsafe { raw_syscall(NUM_KEY_INJECT, [ptr, bytes.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Acquire ownership of the display and claim keyboard input focus
/// (`SyscallNumber::DISPLAY_ACQUIRE`,
/// `plans/PI.md` P11), returning `0` on success or `-errno`.
///
/// The compositing window manager calls this when it takes over the screen:
/// the kernel input-focus arbiter switches its foreground to the desktop
/// keyboard channel, so subsequently injected key edges are delivered as
/// [`KeyInput`] records the manager drains with [`keyboard_read`]. Requires
/// `CAP_DISPLAY` (owning the display is privileged).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn display_acquire() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates `CAP_DISPLAY` before touching state.
    let ret = unsafe { raw_syscall(NUM_DISPLAY_ACQUIRE, [0, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Release the display and return keyboard input focus to the text console
/// (`SyscallNumber::DISPLAY_RELEASE`,
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
/// `buf` (`SyscallNumber::KEYBOARD_READ`, `plans/PI.md`
/// P11), returning the raw signed register (the bytes written — one
/// [`KeyInput`] record's [`KeyInput::WIRE_LEN`], or `0` when the channel is
/// momentarily drained — when non-negative, else `-errno`).
///
/// The principal that owns the display (the window manager) drains the
/// records the arbiter routed to it while it held focus. The kernel
/// validates `CAP_INPUT_READ` and the `(buf, len)` pair against the caller's
/// address space; a `buf` shorter than
/// [`KeyInput::WIRE_LEN`] fails closed with `-errno`. A
/// zero return is a valid empty read, so the caller loops.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn keyboard_read(buf: &mut [u8]) -> i64 {
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_INPUT_READ` and the `(buf, len)` pair against the caller's address
    // space before writing it. `buf` is a live exclusive
    // `&mut [u8]` for the duration of the call, so the `(ptr, len)` pair
    // denotes writable memory.
    let ret = unsafe { raw_syscall(NUM_KEYBOARD_READ, [ptr, buf.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Enumerate the device-resource grants the kernel minted for the calling
/// driver task into `buf` (`SyscallNumber::RESOURCE_GRANTS`, `plans/PI.md` P10 chunk 5d-2), returning the raw signed
/// register: the total number of bytes written — consecutive
/// [`rustos_abi::hwtree::GrantedResource`] records — when non-negative, else
/// `-errno`.
///
/// A driver process calls this once at start-up to learn the unforgeable
/// handles it passes to [`mmio_map`] / [`dma_alloc`]. It needs no capability
/// (a task reads only its *own* grants); the kernel validates the
/// `(buf, len)` pair against the caller's address space before writing it. A `buf` too small for the whole grant set fails closed
/// with `-errno` (`BufferTooSmall`), so size it for the
/// matched node's resource count; a task with no grants returns `0`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn resource_grants(buf: &mut [u8]) -> i64 {
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(buf, len)` pair against the caller's address space before writing
    // it. `buf` is a live exclusive `&mut [u8]` for the
    // duration of the call, so the `(ptr, len)` pair denotes writable memory.
    let ret = unsafe { raw_syscall(NUM_RESOURCE_GRANTS, [ptr, buf.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Allocate a message-signalled interrupt (MSI) vector for a PCI function
/// (`SyscallNumber::MSI_ALLOC`), returning the
/// [`rustos_abi::MsiAllocation`] the kernel minted — the virtual interrupt
/// line plus the doorbell `(address, data)` to program into the function's
/// MSI capability.
///
/// A user-space **bus** driver wiring a PCI function for MSI calls this; it
/// is gated by [`rustos_abi::CapabilityId::IRQ_BIND`] (the same privilege the
/// driver needs to `irq_bind` the returned line). The kernel grants the
/// caller a device resource for the line, so it may both `irq_bind` it and
/// forward it as an [`rustos_abi::hwtree::HwResource::irq`] onto a child node
/// it publishes — never ambient authority.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure — most
/// commonly `NotImplemented` on a platform with no MSI controller, or
/// `OutOfRange` when the vector space is exhausted — and treats a malformed
/// short reply as a fail-closed error rather than a usable value.
pub fn msi_alloc() -> Result<rustos_abi::MsiAllocation, i64> {
    let mut buf = [0u8; rustos_abi::MsiAllocation::WIRE_LEN];
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_IRQ_BIND` and the `(buf, len)` pair against the caller's address
    // space before writing the encoded allocation. `buf` is a live exclusive
    // local for the duration of the call, so the pair denotes writable memory.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-or-`-errno` encoding.
    let ret = unsafe { raw_syscall(NUM_MSI_ALLOC, [ptr, buf.len() as u64, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // A success that wrote fewer bytes than the record needs is malformed;
    // fail closed rather than decode a partial doorbell. `ret` is
    // non-negative here, so the `try_from` only rejects an impossible value.
    match usize::try_from(ret) {
        Ok(written) if written >= rustos_abi::MsiAllocation::WIRE_LEN => {}
        _ => return Err(-(rustos_abi::Errno::BufferTooSmall as i64)),
    }
    rustos_abi::MsiAllocation::from_bytes(&buf).map_err(|e| -(e as i64))
}

/// Publish a discovered child device `node` into the live hardware tree
/// (`SyscallNumber::HW_EMIT_NODE`), returning the raw signed register: the
/// **kernel-assigned node id** (`≥ 0`) once published, else `-errno`.
///
/// The store owns identity — the emitter cannot choose the id — so this
/// returned id is the one way the emitter learns what it published, which it
/// needs to later retract the node with [`hw_remove_node`] (a USB host
/// controller removing the interface node it emitted on a port-down). A
/// caller that does not retract its nodes (most bus drivers) may ignore it.
///
/// A user-space **bus** driver (a PCIe root complex, a USB host) calls this
/// once per device it enumerates, so the device manager autoloads the
/// matching driver in turn — recursive, data-driven discovery, never a
/// compiled-in list. It is gated by
/// [`rustos_abi::CapabilityId::HW_EMIT`], and the kernel admits the node only
/// when every [`rustos_abi::hwtree::HwResource`] it requests is covered by one
/// of the calling driver's own minted grants, so a child can never carry more
/// authority than its emitter (no ambient authority). A
/// malformed node, an unknown parent, or an out-of-grant resource fails closed
/// with `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 0-or-`-errno` encoding.
pub fn hw_emit_node(node: &HwNode) -> i64 {
    let bytes = node.to_le_bytes();
    let ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_HW_EMIT` and reads the `(ptr, len)` pair against the caller's
    // address space before decoding it. `bytes` is a live
    // owned array for the duration of the call, so the pair denotes readable
    // memory.
    let ret = unsafe { raw_syscall(NUM_HW_EMIT_NODE, [ptr, bytes.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Remove a previously-published child device node — and its whole subtree —
/// from the live hardware tree (`SyscallNumber::HW_REMOVE_NODE`), returning the raw signed register: `0` once removed, else
/// `-errno`.
///
/// The symmetric counterpart of [`hw_emit_node`]: a user-space **bus** driver
/// that published a device with [`hw_emit_node`] calls this when the device
/// goes away (a USB port-down, a PCIe hot-remove), so the device manager
/// unloads the driver bound to the vanished node. It is
/// gated by the same [`rustos_abi::CapabilityId::HW_EMIT`], and the kernel
/// retires `node_id` **only** when its parent is the calling driver's own
/// matched node — a child the caller itself published — together with every
/// descendant, so a driver can never remove a node it does not own
/// (no ambient authority). An unknown id, or a node the
/// caller does not own, fails closed with `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 0-or-`-errno` encoding.
pub fn hw_remove_node(node_id: u32) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_HW_EMIT` and resolves `node_id` against the live tree on the far
    // side of the trap. The call passes no memory operand —
    // `node_id` is a scalar in arg 0.
    let ret = unsafe { raw_syscall(NUM_HW_REMOVE_NODE, [u64::from(node_id), 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Yield the calling task's CPU back to the scheduler (`SyscallNumber::YIELD`).
///
/// A cooperative reschedule point: the kernel suspends the caller, runs
/// another runnable task, and returns here when the caller is next
/// dispatched. It requires no capability, takes no arguments, and returns
/// nothing (`abi-v1` `yield` is `() -> ()`). A program that must let a
/// sibling run — without a blocking syscall to wait on — calls this rather
/// than spinning.
pub fn yield_now() {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `yield` takes
    // no arguments and no memory operand, so all six argument registers are
    // zero; the kernel ignores its return value.
    unsafe {
        let _ = raw_syscall(NUM_YIELD, [0, 0, 0, 0, 0, 0]);
    }
}

/// Read the kernel monotonic clock, in nanoseconds
/// (`SyscallNumber::CLOCK_GET`).
///
/// Returns a monotonically non-decreasing nanosecond reading from a clock
/// whose epoch is unspecified — only differences between readings are
/// meaningful. It requires no capability (`clock_get` is callable by every
/// task); a caller without [`CapabilityId::TIME_HIRES`] reads it floored to
/// [`rustos_abi::time::COARSE_CLOCK_GRANULARITY_NS`] (one microsecond), since
/// a sub-microsecond timer is a side-channel primitive the kernel withholds
/// from untrusted callers. The wrapper performs no
/// coarsening of its own — the value it returns is exactly what the kernel
/// handed back.
///
/// [`CapabilityId::TIME_HIRES`]: rustos_abi::CapabilityId::TIME_HIRES
#[must_use]
pub fn clock_get() -> u64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `clock_get`
    // takes no arguments and no memory operand, so all six argument registers
    // are zero; its result is the `U64` nanosecond reading.
    unsafe { raw_syscall(NUM_CLOCK_GET, [0, 0, 0, 0, 0, 0]) }
}

/// Park, yielding the CPU, until `now()` reaches `deadline_ns`.
///
/// The shared core of [`ClockDelay`]'s
/// [`delay_us`](rustos_abi::Delay::delay_us): it reads the monotonic clock
/// through `now` and surrenders the CPU through `yield_fn` between reads, so
/// it is a cooperative wait rather than a hard spin. A
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
/// each rolling its own over [`clock_get`].
///
/// The wait is cooperative: [`delay_us`](rustos_abi::Delay::delay_us) yields
/// the CPU to other runnable tasks between clock reads instead of busy-spinning. It carries no authority — `clock_get` needs no
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
        // never approaches that, but the wait must not silently shorten.
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
/// `exec`-style hand-off).
///
/// The child's standard streams attach to the **caller's own** console
/// ([`rustos_abi::CONSOLE_INHERIT`]): a spawned session
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
/// and hides no error.
#[must_use]
pub fn spawn(path: &[u8]) -> i64 {
    // Inherit both the caller's console and its attested credential: a
    // spawned session member runs as the same user, on the same console, as
    // its parent.
    spawn_raw(path, CONSOLE_INHERIT, SPAWN_UID_INHERIT)
}

/// The shared `SyscallNumber::SPAWN` trap the [`spawn`], [`spawn_at`], and
/// [`spawn_as`] wrappers issue: one raw call site so the argument layout is
/// defined once (`console` in slot 2, `target_uid` in slot 3).
///
/// `console` is [`rustos_abi::CONSOLE_INHERIT`] or an installed console index;
/// `target_uid` is [`rustos_abi::SPAWN_UID_INHERIT`] (start under the caller's
/// own credential) or a concrete uid to switch to (which the kernel gates on
/// `CAP_SPAWN_AS_USER`). The kernel encodes the result as a signed register:
/// a non-negative value is the new PID, a negative value is `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 spawn-result encoding (PID ≥ 0, else -errno).
fn spawn_raw(path: &[u8], console: u64, target_uid: u32) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(path, len)` against the caller's address space before touching it.
    // `path` is a live shared `&[u8]` for the duration of the call, so the
    // `(ptr, len)` pair denotes readable memory.
    let ret = unsafe {
        raw_syscall(
            NUM_SPAWN,
            [ptr, path.len() as u64, console, u64::from(target_uid), 0, 0],
        )
    };
    ret as i64
}

/// Spawn the embedded program at `path` on the installed console `console`
/// **as the user `target_uid`** (`SyscallNumber::SPAWN`,
/// `PREREQUISITES.md` P-C, spawn-as-user).
///
/// The credential-switching form: the kernel resolves `target_uid`'s full
/// credential (uid, primary gid, supplementary groups) from the authoritative
/// identity table and drops the child into it, so the child runs under an
/// authoritative identity the caller chose but never fabricated. This requires
/// the caller to hold `CAP_SPAWN_AS_USER` and fails closed with `-errno`
/// (`PermissionDenied`) otherwise, or when `target_uid` names no account. Its
/// intended caller is `login`, which authenticates a user and then starts
/// their shell under the authenticated uid. Pass
/// [`rustos_abi::CONSOLE_INHERIT`] for `console` to keep the child on the
/// caller's own console. A running process can never change its *own*
/// identity (there is no setuid-self).
#[must_use]
pub fn spawn_as(path: &[u8], console: u64, target_uid: u32) -> i64 {
    spawn_raw(path, console, target_uid)
}

/// Spawn the embedded program registered under the absolute `path` with
/// its standard streams attached to the installed console `console`
/// (`SyscallNumber::SPAWN`, `plans/PI.md` P11).
///
/// The console-selecting form of [`spawn`]: `console` names an index in
/// the kernel's installed console list (its length is reported by
/// [`console_count`]); an index with no installed console fails closed
/// with `-errno` (`NotFound`). PID 1 `init` uses this to start one login
/// session per discovered text console — the video console and the UART
/// are separate session contexts.
#[must_use]
pub fn spawn_at(path: &[u8], console: u32) -> i64 {
    // A specific console, but the caller's own credential (no user switch):
    // PID 1 launching one login per console runs each as the same principal
    // it runs as.
    spawn_raw(path, u64::from(console), SPAWN_UID_INHERIT)
}

/// Report how many system text consoles are installed
/// (`SyscallNumber::CONSOLE_COUNT`, `plans/PI.md` P11).
///
/// Requires `CAP_CONSOLE_WRITE`. The count is the index space
/// [`spawn_at`]'s `console` argument selects from; PID 1 `init` uses it
/// to start one login session per discovered console. The kernel encodes
/// the result as a signed register: a non-negative value is the count,
/// a negative value is `-errno` (the wrapper surfaces it verbatim).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
pub fn console_count() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates the capability before any state
    // is touched.
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
/// executable (W^X); mapping one's own isolated space
/// grants no further authority, so no capability is required.
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: a non-negative value is the base address
/// of the new region, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`) — a frame exhaustion is
/// reported as [`rustos_abi::Errno::OutOfMemory`] (deterministic OOM, never a panic). The wrapper surfaces that raw signed
/// value so the caller decides how to react; it adds no authority and hides
/// no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 mem_map-result encoding (base ≥ 0, else -errno).
pub fn mem_map(len: usize, flags: MapFlags, addr_hint: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `mem_map`
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
/// The kernel zeroes the frames it reclaims (secret
/// hygiene) and fails closed when `(base, len)` does not name a region the
/// caller mapped. Returns `0` on success or `-errno`
/// (recover the [`rustos_abi::Errno`] discriminant as `-ret`), following the
/// standard `abi-v1` signed-result convention; the wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 mem_unmap-result encoding (0, else -errno).
pub fn mem_unmap(base: u64, len: usize) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(base, len)` range against the caller's own address space before
    // unmapping it. No user pointer is dereferenced.
    let ret = unsafe { raw_syscall(NUM_MEM_UNMAP, [base, len as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Map the `[offset, offset + len)` sub-region of a **granted** device MMIO
/// window into the calling driver's own address space
/// (`SyscallNumber::MMIO_MAP`, `plans/PI.md` P10 chunk 5d-0).
///
/// `handle` is an unforgeable, kernel-issued device-resource grant handle —
/// never a raw physical address: the kernel resolves it
/// **owner-checked against the calling task**, confirms it names a memory
/// window, confirms `[offset, offset + len)` lies wholly inside that window,
/// and maps only that sub-region — caching disabled, never executable. A forged or another driver's handle
/// resolves to nothing and is refused, as is a sub-region escaping the grant.
/// Mapping a bounded sub-region (not the whole grant) is what lets a driver
/// granted a large outbound bus aperture map just the single BAR it
/// enumerated rather than the entire window. The call
/// carries `CAP_MMIO_MAP` (enforced by the kernel before any state is
/// touched).
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the base virtual address of
/// the newly mapped sub-region, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`). The wrapper surfaces that
/// raw signed value so the caller decides how to react; it adds no authority
/// and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 mmio_map-result encoding (base ≥ 0, else -errno).
pub fn mmio_map(handle: u64, offset: u64, len: usize) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `mmio_map`
    // dereferences no user pointer; it resolves the grant handle and maps the
    // requested sub-region into the caller's own space, returning its base.
    let ret = unsafe { raw_syscall(NUM_MMIO_MAP, [handle, offset, len as u64, 0, 0, 0]) };
    ret as i64
}

/// Allocate a coherent DMA buffer for the calling driver, bounded by a
/// granted device DMA constraint (`SyscallNumber::DMA_ALLOC`, `plans/PI.md`
/// P10 chunk 5d-0).
///
/// `handle` is an unforgeable, kernel-issued device-resource grant handle —
/// never a raw physical address: the kernel resolves it
/// **owner-checked against the calling task**, confirms it names a DMA
/// constraint, carves a physically-contiguous, zeroed, coherent buffer of
/// `len` bytes whose physical extent lies within the grant's addressing
/// limit, maps it `RW`, non-executable,
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
/// no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 dma_alloc-result encoding (base ≥ 0, else -errno).
pub fn dma_alloc(handle: u64, len: usize, device_out: &mut u64) -> i64 {
    let ptr = (device_out as *mut u64) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `device_out`
    // is a live exclusive `&mut u64` for the duration of the call, so the
    // pointer denotes writable memory the kernel may fill with the
    // device-visible base; the kernel validates it against the caller's own
    // address space before writing.
    let ret = unsafe { raw_syscall(NUM_DMA_ALLOC, [handle, len as u64, ptr, 0, 0, 0]) };
    ret as i64
}

/// Release a coherent DMA buffer previously carved by [`dma_alloc`]
/// (`SyscallNumber::DMA_FREE`) — the symmetric free a long-running driver
/// calls so each transfer's bounce buffers are reclaimed rather than leaked
/// until the process exits.
///
/// `handle` is the same unforgeable, kernel-issued DMA-constraint grant
/// handle the matching [`dma_alloc`] was called with, and `cpu_va` is the
/// base **user virtual address** that `dma_alloc` returned. The kernel
/// resolves the handle **owner-checked against the calling task**, confirms
/// it names a DMA constraint, and releases the buffer based at `cpu_va` from
/// the caller's own address space, zeroing every backing byte (zero-on-free)
/// before the frames return to the allocator. A forged or foreign handle, or
/// a `cpu_va` that is not the base of a live carve, fails closed without
/// releasing anything. The call carries `CAP_MEM_DMA` (enforced by the kernel
/// before any state is touched).
///
/// Returns `0` on success, or `-errno` (recover the [`rustos_abi::Errno`]
/// discriminant as `-ret`). The wrapper surfaces the raw signed value and
/// hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 dma_free-result encoding (0, else -errno).
pub fn dma_free(handle: u64, cpu_va: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `dma_free` dereferences no user
    // pointer; it resolves the grant handle and releases the carve named by
    // `cpu_va` from the caller's own address space.
    let ret = unsafe { raw_syscall(NUM_DMA_FREE, [handle, cpu_va, 0, 0, 0, 0]) };
    ret as i64
}

/// Bind interrupt `line` to the calling task, minting an unforgeable
/// [`rustos_abi::IrqHandle`] (`SyscallNumber::IRQ_BIND`).
///
/// `line` is the architecture interrupt-line identifier the driver received
/// as an [`HwResourceKind::Irq`](rustos_abi::hwtree::HwResourceKind) grant on
/// its matched node — a discovered value, never a board
/// constant. The call carries `CAP_IRQ_BIND` (enforced by the kernel before
/// any state is touched); the minted handle is re-keyed to the calling task,
/// so only this task can `irq_wait` on it.
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the raw `IrqHandle`, and a
/// negative value is `-errno` (recover the [`rustos_abi::Errno`] discriminant
/// as `-ret`). The wrapper surfaces that raw signed value; it adds no
/// authority and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 irq_bind-result encoding (handle ≥ 0, else -errno).
pub fn irq_bind(line: u32) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `irq_bind`
    // dereferences no user pointer; it records the binding and returns a
    // handle.
    let ret = unsafe { raw_syscall(NUM_IRQ_BIND, [u64::from(line), 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Park the calling task until the interrupt bound to `handle` fires, the
/// `timeout_ns` deadline elapses, or the binding disappears
/// (`SyscallNumber::IRQ_WAIT`).
///
/// `handle` is the [`rustos_abi::IrqHandle`] a prior [`irq_bind`] minted for
/// this task; the kernel re-checks the binding owner-side on every call and parks the task off the run queue between polls (no
/// busy-wait). Pass `u64::MAX` for an effectively unbounded
/// wait. The kernel re-arms the bound line on the driver's behalf across the
/// park (the driver holds no controller access), so an interrupt-driven
/// driver loops `irq_wait` → drain → `irq_wait` without touching hardware
/// interrupt-controller state.
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: `0` on a fire, and a negative value is `-errno`
/// (`Errno::TimedOut` on the deadline, `Errno::NotFound` for a forged or
/// released handle — recover the discriminant as `-ret`). The wrapper
/// surfaces that raw signed value and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 irq_wait-result encoding (0, else -errno).
pub fn irq_wait(handle: u64, timeout_ns: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the handle owner-side on the far side of the trap.
    // `irq_wait` dereferences no user pointer.
    let ret = unsafe { raw_syscall(NUM_IRQ_WAIT, [handle, timeout_ns, 0, 0, 0, 0]) };
    ret as i64
}

/// Wait for a child process to exit, reaping it and reading back its exit
/// code (`SyscallNumber::WAIT`, `plans/SPAWN.md` SP6).
///
/// `pid` is either a specific child's PID or [`rustos_abi::WAIT_PID_ANY`] to
/// wait for whichever of the caller's children exits next. On success the
/// kernel writes the reaped child's exit code into `status` and returns its
/// PID. A process may only wait on its **own** children; the kernel
/// validates the parent/child relationship and fails closed.
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: a non-negative value is the reaped child's
/// PID, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`) — `status` is left
/// untouched on a negative result. The wrapper surfaces that raw signed
/// value so the caller decides how to react; it adds no authority and hides
/// no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 wait-result encoding (PID ≥ 0, else -errno).
pub fn wait(pid: i32, status: &mut i32) -> i64 {
    let ptr = (status as *mut i32) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `status` pointer against the caller's address space before
    // writing the exit code to it. `status` is a live
    // exclusive `&mut i32` for the duration of the call, so the pointer
    // denotes writable memory the kernel may fill.
    let ret = unsafe { raw_syscall(NUM_WAIT, [i32_arg(pid), ptr, 0, 0, 0, 0]) };
    ret as i64
}

/// Read the calling process's effective limit for resource `kind`
/// (`SyscallNumber::RLIMIT_GET`).
///
/// On success the kernel writes the encoded [`ResourceLimit`] into a local
/// buffer this wrapper decodes and returns. Reading one's own limit grants
/// no authority and needs no capability. The
/// kernel encodes a failure as a negative register (`-errno`, the standard
/// `abi-v1` convention); the wrapper surfaces it as `Err(-ret)` (the raw
/// negative value) and hides no error.
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
    // the encoded limit to it. `buf` is a live exclusive
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
/// (`SyscallNumber::RLIMIT_SET`).
///
/// The wrapper encodes `value` into a local buffer the kernel reads. A
/// process may freely *lower* a bound, but *raising* a hard bound above the
/// inherited ceiling requires [`rustos_abi::CapabilityId::RLIMIT_RAISE`]. Returns `0` on success or `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`), the standard `abi-v1`
/// signed-result convention; the wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn rlimit_set(kind: LimitKind, value: ResourceLimit) -> i64 {
    let buf = value.encode();
    let ptr = buf.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `value` pointer against the caller's address space before reading
    // the encoded limit from it. `buf` is a live local
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
/// authority. Sizing `buf` at the format's own
/// 64 KiB maximum (`rustos-users` `MAX_DB_LEN`) always suffices: a
/// buffer smaller than the database is refused whole — a credential
/// database is never truncated.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure: the
/// caller lacks the capability, no database is held (no root volume, or
/// the boot read refused the record — the caller fails closed and
/// refuses every login), or `buf` is too small.
pub fn users_db_read(buf: &mut [u8]) -> Result<usize, i64> {
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(buf, len)` pair against the caller's address space before
    // writing to it. `buf` is a live exclusive
    // `&mut [u8]` for the duration of the call, so the pair denotes
    // writable memory the kernel may fill.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe { raw_syscall(NUM_USERS_DB_READ, [ptr, len, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice in the caller, exactly as `stdin` clamps.
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
/// before enqueueing — the wrapper adds no
/// authority. A spawned driver process uses this to report its
/// `register()` outcome back to the driver host on the reply endpoint
/// handed to it through its startup args (`PLAN.md` Stage 4.HW).
///
/// Returns `0` on success or `-errno` (recover the [`rustos_abi::Errno`]
/// discriminant as `-ret`), the standard `abi-v1` signed-result
/// convention; the wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn ipc_send(endpoint: u64, payload: &[u8]) -> i64 {
    let ptr = payload.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(ptr, len)` pair against the caller's address space before
    // reading it. `payload` is a live shared `&[u8]` for
    // the duration of the call, so the pair denotes readable memory.
    let ret = unsafe { raw_syscall(NUM_IPC_SEND, [endpoint, ptr, payload.len() as u64, 0, 0, 0]) };
    ret as i64
}

/// Read the discovered hardware tree the kernel built at boot into `buf`
/// (`SyscallNumber::HW_TREE_READ`),
/// returning the number of bytes copied.
///
/// The copied bytes are a [`rustos_abi::HwTreeHeader`] (the store's
/// current generation and the node count) followed by that many
/// [`rustos_abi::HwNode`] records, which the caller decodes with the
/// fail-closed `from_bytes` parsers. The generation in the header is the
/// value to pass to [`hw_tree_wait`] to block until the tree next changes.
/// Gated kernel-side on [`rustos_abi::CapabilityId::SYSINFO_HW`] — the
/// privileged global hardware view; the
/// wrapper adds no authority.
///
/// The whole inventory is copied or none: a buffer smaller than the
/// snapshot is refused with `BufferTooSmall` rather than truncated, so the caller grows `buf` and retries (the node
/// count is a discovered capacity, not a fixed ceiling).
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
    // writing to it. `buf` is a live exclusive
    // `&mut [u8]` for the duration of the call, so the pair denotes
    // writable memory the kernel may fill.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe { raw_syscall(NUM_HW_TREE_READ, [ptr, len, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice in the caller, exactly as `users_db_read` clamps.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(buf.len()))
}

/// Block until the discovered hardware tree changes past
/// `last_generation` (`SyscallNumber::HW_TREE_WAIT` —
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
/// is wired. The wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn hw_tree_wait(last_generation: u64, timeout_ns: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. Both
    // arguments are scalars; the call reads no caller memory.
    let ret = unsafe { raw_syscall(NUM_HW_TREE_WAIT, [last_generation, timeout_ns, 0, 0, 0, 0]) };
    ret as i64
}

/// Block until the system user database leaves its *pending*
/// (still-being-unlocked) state (`SyscallNumber::USERS_DB_WAIT`, `plans/PI.md` P11 — the reactive companion to
/// [`users_db_read`]).
///
/// Under design B `login` is spawned before the in-kernel unlock kthread
/// mounts the encrypted root, so an early [`users_db_read`] reports
/// `WouldBlock` — the live-but-not-ready signal. Rather than re-reading in
/// a yield loop (a busy spin that audited one ERROR per poll), the caller blocks here: the kernel parks it off the run queue and
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
/// wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn users_db_wait(timeout_ns: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. The single
    // argument is a scalar; the call reads no caller memory.
    let ret = unsafe { raw_syscall(NUM_USERS_DB_WAIT, [timeout_ns, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Make a synchronous capability-checked call to the kernel-owned IPC call
/// endpoint `endpoint`: post `request`, block until the reply arrives, and
/// copy it into `reply` (`SyscallNumber::IPC_CALL`;
/// Design D D2b). Returns the number of reply bytes written.
///
/// The kernel enforces the endpoint's required send capability against the
/// caller before posting (no ambient authority), copies
/// `request` in and the reply out through the validated boundary, and blocks
/// the caller cooperatively until the reply arrives, never busy-spinning. The first consumer is the reactive device manager
/// reading the read-only `/System` driver store over
/// [`rustos_abi::driver_store::DRIVER_STORE_ENDPOINT`].
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure: a missing
/// send capability (`PermissionDenied`), an unknown or destroyed endpoint
/// (`NotFound`), an oversize request (`MessageTooLarge`), a reply larger than
/// `reply` (`BufferTooSmall`), or no call-endpoint registry wired
/// (`NotImplemented`). The wrapper hides no error.
pub fn ipc_call(endpoint: u64, request: &[u8], reply: &mut [u8]) -> Result<usize, i64> {
    let req_ptr = request.as_ptr() as usize as u64;
    let reply_ptr = reply.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `(ptr, len)` pairs against the caller's address space before
    // touching them. `request` is a live shared `&[u8]`
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
    // count can never drive an out-of-bounds slice in the caller, exactly as `hw_tree_read` clamps.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(reply.len()))
}

/// Emit one pre-encoded diagnostic [`rustos_abi::LogRecordRef`] wire image to
/// the kernel's diagnostic log sink (`SyscallNumber::LOG_EMIT`).
///
/// Most callers use the higher-level [`LogSink`] rather than this raw form.
/// The kernel verifies the caller holds `CAP_LOG_EMIT`, copies and fully
/// validates the record, and emits it through the same sink it routes its own
/// records through (the serial UART on a debug build, the video console on
/// release), attributing it to the calling task.
///
/// Returns `0` on success, or the raw negative kernel result (`-errno`): a
/// missing `CAP_LOG_EMIT` (`PermissionDenied`), a malformed or oversize
/// record (`LengthOutOfRange` / `OutOfRange`), or a faulting pointer
/// (`BadAddress`). The wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn log_emit(record: &[u8]) -> i64 {
    let ptr = record.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(ptr, len)` pair against the caller's address space before reading
    // it. `record` is a live shared `&[u8]` for the
    // duration of the call.
    let ret = unsafe { raw_syscall(NUM_LOG_EMIT, [ptr, record.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Clamp `s` to at most `max` bytes at a UTF-8 character boundary.
///
/// The diagnostic log is best-effort: rather than drop a
/// whole record whose message or field exceeds the `abi-v1` bound, [`LogSink`]
/// trims it to the largest valid prefix so the line still reaches the log.
fn clamp_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A [`rustos_log::Sink`] that marshals each structured event to the kernel's
/// diagnostic log sink through [`log_emit`].
///
/// This is how a first-party service routes its `rustos_log` diagnostics to
/// the system log — the serial UART on a debug build — instead of writing
/// them to `stderr` (fd 2), which on a framebuffer-console board lands on the
/// screen rather than the captured serial line. The
/// emitting task must hold `CAP_LOG_EMIT`; without it the kernel refuses the
/// call and the record is dropped (the sink is best-effort and never panics).
///
/// A message or field that exceeds the `abi-v1` record bounds is clamped to
/// the largest valid prefix and excess fields past
/// [`rustos_abi::LOG_FIELDS_MAX`] are dropped, so an over-long record still
/// reaches the log rather than being discarded whole.
#[derive(Debug, Default, Copy, Clone)]
pub struct LogSink;

impl rustos_log::Sink for LogSink {
    fn write_event(&self, event: &rustos_log::Event<'_>) {
        // Marshal the borrowed fields into the `(key, value)` pairs the
        // encoder takes, clamping keys/strings to their bound and dropping any
        // field past `LOG_FIELDS_MAX` (best-effort). A `Str` value longer than
        // the per-field encoded bound is trimmed so the record still encodes
        // rather than being dropped whole; non-string values are fixed-size.
        let mut pairs: [(&str, rustos_abi::FieldValue<'_>); rustos_abi::LOG_FIELDS_MAX] =
            [("", rustos_abi::FieldValue::Null); rustos_abi::LOG_FIELDS_MAX];
        let field_count = event.fields.len().min(rustos_abi::LOG_FIELDS_MAX);
        for (slot, field) in pairs.iter_mut().zip(event.fields.iter()).take(field_count) {
            let value = match field.value {
                // Leave room for the value's tag + length prefix.
                rustos_abi::FieldValue::Str(s) => {
                    rustos_abi::FieldValue::Str(clamp_utf8(s, rustos_abi::LOG_FIELD_VALUE_MAX - 3))
                }
                other => other,
            };
            *slot = (clamp_utf8(field.key, rustos_abi::LOG_FIELD_KEY_MAX), value);
        }
        let message = clamp_utf8(event.message, rustos_abi::LOG_MESSAGE_MAX);

        let mut buf = [0u8; rustos_abi::LOG_RECORD_MAX];
        if let Ok(len) = rustos_abi::encode_log_record(
            &mut buf,
            event.level.as_u8(),
            event.id.0,
            message,
            &pairs[..field_count],
        ) {
            // Best-effort: a refused or faulting emit drops the record rather
            // than surfacing an error a `Sink` cannot return.
            let _ = log_emit(&buf[..len]);
        }
    }
}

/// Create and register a kernel-owned synchronous call endpoint the calling
/// task then *serves* (`SyscallNumber::CALL_CREATE`;
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
/// error.
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
    // through the validated boundary.
    let send_bytes = send_caps.to_le_bytes();
    let recv_bytes = recv_caps.to_le_bytes();
    let send_ptr = send_bytes.as_ptr() as usize as u64;
    let recv_ptr = recv_bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `CapabilitySet` pointers against the caller's address space before
    // reading them. `send_bytes`/`recv_bytes` live for the
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
/// blocking until one arrives (`SyscallNumber::CALL_RECV`;
/// Design D D3 — the server-side receive half).
///
/// On success the request payload is copied into `buf`, the per-call ticket
/// (to answer with [`call_reply`]) is written to `ticket_out`, and the number
/// of request bytes is returned. The kernel parks the caller cooperatively
/// until a request is posted, never busy-spinning.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`): a request larger than
/// `buf` (`BufferTooSmall`, left queued), a missing receive capability or a
/// foreign endpoint (`PermissionDenied`), or an unknown/destroyed endpoint
/// (`NotFound`). The wrapper hides no error.
pub fn call_recv(endpoint: u64, buf: &mut [u8], ticket_out: &mut u64) -> Result<usize, i64> {
    let buf_ptr = buf.as_mut_ptr() as usize as u64;
    let ticket_ptr = (ticket_out as *mut u64) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both pointers against the caller's address space before touching them. `buf` is a live exclusive `&mut [u8]` and
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
    // count can never drive an out-of-bounds slice.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(buf.len()))
}

/// Answer one received call on an endpoint this task owns, releasing the
/// blocked caller (`SyscallNumber::CALL_REPLY`; Design D D3
/// — the server-side reply half).
///
/// `ticket` is the value [`call_recv`] wrote; `reply` is the reply payload.
/// Returns `0` on success, or the raw negative kernel result (`-errno`): a
/// reply larger than the endpoint's `max_reply` (`MessageTooLarge`), an
/// unknown or already-answered ticket or unknown endpoint (`NotFound`), or a
/// missing receive capability / foreign endpoint (`PermissionDenied`). The
/// wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn call_reply(endpoint: u64, ticket: u64, reply: &[u8]) -> i64 {
    let reply_ptr = reply.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // reply `(ptr, len)` pair against the caller's address space before
    // reading it. `reply` is a live shared `&[u8]` for the
    // duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_CALL_REPLY,
            [endpoint, ticket, reply_ptr, reply.len() as u64, 0, 0],
        )
    };
    ret as i64
}

/// Read the kernel-attested [`rustos_abi::Origin`] of the caller whose
/// in-service call this server is currently handling
/// (`SyscallNumber::CALL_PEER_ORIGIN`; P-C).
///
/// `endpoint` is a call endpoint this task owns; `ticket` is the value a prior
/// [`call_recv`] returned for a call still in service. On success the caller's
/// attested origin wire image is copied into `out` and its byte length
/// returned; decode it with [`rustos_abi::Origin::from_bytes`]. The origin is
/// filled by the kernel from the posting task's own state, so a caller cannot
/// forge it.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`): a buffer shorter than
/// [`rustos_abi::ORIGIN_WIRE_LEN`] (`BufferTooSmall`), a missing receive
/// capability or a foreign endpoint (`PermissionDenied`), or an unknown
/// endpoint or a ticket not in service (`NotFound`). The wrapper hides no
/// error.
pub fn call_peer_origin(endpoint: u64, ticket: u64, out: &mut [u8]) -> Result<usize, i64> {
    let out_ptr = out.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before writing.
    // `out` is a live exclusive `&mut [u8]` for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe {
        raw_syscall(
            NUM_CALL_PEER_ORIGIN,
            [endpoint, ticket, out_ptr, out.len() as u64, 0, 0],
        )
    } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(out.len()))
}

/// Read the kernel's wall-clock time and its provenance state
/// (`SyscallNumber::WALL_TIME_GET`; P-D).
///
/// Returns a [`WallClockReading`] — an absolute [`Time64`] instant plus a
/// [`WallTimeState`] saying how trustworthy it is. Unprivileged, like
/// [`clock_get`]. Before a trusted source has set the clock the reading is
/// the Unix epoch tagged [`WallTimeState::Unset`]; ordering must always rest
/// on the monotonic [`clock_get`], never on this value.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`): the kernel writes the
/// reading into a stack buffer here, so the only failures are a clock that is
/// not wired (`NotImplemented`) or a malformed decode (`OutOfRange` /
/// `BufferTooSmall`, which a correct kernel never produces). The wrapper
/// hides no error.
pub fn wall_time() -> Result<WallClockReading, i64> {
    let mut buf = [0u8; WallClockReading::WIRE_LEN];
    let out_ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before writing.
    // `buf` is a live exclusive local for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret =
        unsafe { raw_syscall(NUM_WALL_TIME_GET, [out_ptr, buf.len() as u64, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // The kernel returns the wire length; decode it (fail closed on a
    // malformed image — never inventing a time).
    WallClockReading::from_bytes(&buf).map_err(|e| -i64::from(e.as_i32()))
}

/// Set the kernel's wall-clock time from a trusted source
/// (`SyscallNumber::WALL_TIME_SET`; P-D).
///
/// `time` is the absolute [`Time64`] instant and `state` is the provenance to
/// record — [`WallTimeState::Firmware`], [`WallTimeState::Trusted`], or
/// [`WallTimeState::Adjusted`] (passing [`WallTimeState::Unset`] is rejected).
/// The monotonic clock is unaffected; only the wall offset and state change.
/// Carries `CAP_TIME_SET` (enforced kernel-side).
///
/// Returns `0` on success, or the raw negative kernel result (`-errno`): a
/// missing capability (`PermissionDenied`), a non-settable state
/// (`OutOfRange`), or a clock that is not wired (`NotImplemented`). The
/// wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn wall_time_set(time: Time64, state: WallTimeState) -> i64 {
    let bytes = time.to_le_bytes();
    let time_ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before reading.
    // `bytes` is a live local for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_WALL_TIME_SET,
            [
                time_ptr,
                bytes.len() as u64,
                u64::from(state.as_u8()),
                0,
                0,
                0,
            ],
        )
    };
    ret as i64
}

/// Read the kernel's per-boot identifier ([`BootId`])
/// (`SyscallNumber::BOOT_ID_GET`; P-E).
///
/// Returns the 16-byte [`BootId`] the kernel minted for this boot — a public
/// per-boot nonce, stable within a boot and fresh across boots. Unprivileged,
/// like [`clock_get`].
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`). The notable case is
/// `EntropyNotReady`: a port whose random subsystem could not be seeded in
/// time has no boot id, and the kernel fails closed rather than return the
/// all-zero [`BootId::UNSET`] sentinel. The wrapper hides no error.
pub fn boot_id() -> Result<BootId, i64> {
    let mut buf = [0u8; BOOT_ID_LEN];
    let out_ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before writing.
    // `buf` is a live exclusive local for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret =
        unsafe { raw_syscall(NUM_BOOT_ID_GET, [out_ptr, buf.len() as u64, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // The kernel returns the wire length; decode it (fail closed on a
    // malformed image — never inventing an id).
    BootId::from_bytes(&buf).map_err(|e| -i64::from(e.as_i32()))
}

/// Read the **unfiltered, global** kernel introspection view
/// (`SyscallNumber::SYSINFO_INTROSPECT`; P-C).
///
/// `domain` is a [`rustos_abi::IntrospectDomain`] discriminant; `arg` is the
/// domain-specific selector (a record offset for the paged domains, unused
/// otherwise); `buf` receives the encoded records and returns the byte count
/// written. For the per-task-limits domain the target task's 128-bit
/// [`rustos_abi::ProcId`] is written into `buf` on entry (a `u64` `arg` cannot
/// carry it).
///
/// Gated kernel-side on [`rustos_abi::CapabilityId::SYSINFO_INTROSPECT`],
/// held only by the `sysinfod` broker — the kernel returns the whole system's
/// state and never narrows by principal; the wrapper adds no authority. The
/// whole answer or none: an undersized buffer is refused with `BufferTooSmall`
/// rather than truncated mid-record.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`): the caller lacks the
/// capability, no introspection source is wired (`NotImplemented`), the domain
/// is unknown (`OutOfRange`), the target task does not exist (`NotFound`), or
/// `buf` is too small (`BufferTooSmall`).
pub fn sysinfo_introspect(domain: u32, arg: u64, buf: &mut [u8]) -> Result<usize, i64> {
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(buf, len)` pair against the caller's address space before touching it.
    // `buf` is a live exclusive `&mut [u8]` for the duration of the call, so
    // the pair denotes memory the kernel may read (the target id on entry) and
    // fill (the encoded answer).
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe {
        raw_syscall(
            NUM_SYSINFO_INTROSPECT,
            [u64::from(domain), arg, ptr, len, 0, 0],
        )
    } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice in the caller, exactly as
    // `hw_tree_read` clamps.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(buf.len()))
}

/// Read the character-cell geometry of the text console backing standard
/// stream `fd` (`SyscallNumber::TERMINAL_SIZE`; P-C — the `top` terminal UI).
///
/// `fd` is a standard descriptor the caller owns (typically [`STDOUT`]).
/// Unprivileged, like [`clock_get`]: a program may always ask how big its own
/// terminal is.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`). The notable case is
/// `NotImplemented`: the kernel reports a size only for a console whose grid
/// it actually knows (a framebuffer text console). For a byte-stream console
/// (a UART), whose remote terminal size the kernel cannot attest, the call
/// fails closed and the caller applies the conventional fallback — the kernel
/// never fabricates a size. An `fd` that is not an open stream fails
/// `NotFound`. The wrapper hides no error.
pub fn terminal_size(fd: u32) -> Result<TerminalSize, i64> {
    let mut buf = [0u8; TERMINAL_SIZE_WIRE_LEN];
    let out_ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before writing.
    // `buf` is a live exclusive local for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe {
        raw_syscall(
            NUM_TERMINAL_SIZE,
            [u64::from(fd), out_ptr, buf.len() as u64, 0, 0, 0],
        )
    } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // The kernel returns the wire length; decode it (fail closed on a
    // malformed image — never inventing a size).
    TerminalSize::from_bytes(&buf).map_err(|e| -i64::from(e.as_i32()))
}

/// Create a kernel-owned, zeroed, cross-process shared-memory region and map
/// it into the calling task (`SyscallNumber::SHM_CREATE`; `plans/USB.md`
/// U3a2 — the URB data-buffer primitive).
///
/// `len` is the region length in bytes; the kernel allocates a
/// physically-contiguous, zeroed region, maps it cacheable `RW`/non-exec,
/// guard-bracketed, into the caller's own address space, records the caller
/// as its owner, and mints the owner the matching per-region
/// [`HwResourceKind::Shared`](rustos_abi::hwtree::HwResourceKind) grant so it
/// may forward the region onto a node it emits. The region id is written to
/// `id_out`. The call carries `CAP_SHM` (enforced kernel-side before any
/// state is touched).
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the base virtual address of
/// the newly mapped region, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`) — `id_out` is left untouched
/// on a negative result. The wrapper surfaces that raw signed value; it adds
/// no authority and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 shm_create-result encoding (base ≥ 0, else -errno).
pub fn shm_create(len: usize, id_out: &mut u64) -> i64 {
    let id_ptr = (id_out as *mut u64) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `id_out` is a live exclusive
    // `&mut u64` for the duration of the call, so the pointer denotes writable
    // memory the kernel may fill with the new region's id; the kernel
    // validates it against the caller's own address space before writing.
    let ret = unsafe { raw_syscall(NUM_SHM_CREATE, [len as u64, id_ptr, 0, 0, 0, 0]) };
    ret as i64
}

/// Map a **granted** shared-memory region into the calling task
/// (`SyscallNumber::SHM_MAP`; `plans/USB.md` U3a2).
///
/// `handle` is an unforgeable, kernel-issued per-region grant handle — never
/// a raw address: the kernel resolves it **owner-checked against the calling
/// task**, confirms it names a shared region, maps the *same* frames cacheable
/// `RW`/non-exec, guard-bracketed, into the caller's own address space, and
/// bumps the region's reference count so neither holder frees frames the other
/// still maps. A forged or wrong-kind handle resolves to nothing and is
/// refused. The call carries `CAP_SHM` (enforced kernel-side).
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the base virtual address of
/// the mapping, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`). The wrapper surfaces that
/// raw signed value; it adds no authority and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 shm_map-result encoding (base ≥ 0, else -errno).
pub fn shm_map(handle: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `shm_map` dereferences no user
    // pointer; it resolves the grant handle and maps the region into the
    // caller's own space, returning its base.
    let ret = unsafe { raw_syscall(NUM_SHM_MAP, [handle, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Release the calling task's mapping of a shared-memory region
/// (`SyscallNumber::SHM_UNMAP`; `plans/USB.md` U3a2).
///
/// `(base, len)` is the mapping a prior [`shm_create`] or [`shm_map`]
/// returned; the kernel validates the range against the caller's own address
/// space, unmaps it, and drops the caller's reference to the region — the
/// region's frames are scrubbed and freed only when the last reference is
/// dropped. Returns `0` on success, or the raw negative kernel result
/// (`-errno`). The wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn shm_unmap(base: u64, len: usize) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(base, len)` range against the caller's own address space before
    // unmapping it. No user pointer is dereferenced.
    let ret = unsafe { raw_syscall(NUM_SHM_UNMAP, [base, len as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Create a kernel **wait-set**: a multiplexing object that observes the
/// readiness of several event sources so one task can service them all
/// without a busy-poll (`SyscallNumber::WAITSET_CREATE`; `plans/USB.md` U3a3
/// — the asynchronous host-controller event loop).
///
/// Needs no capability of its own: a wait-set only ever observes resources
/// the caller already holds, each owner-checked when it is added with
/// [`waitset_ctl`]. The kernel encodes the result as a signed register: a
/// non-negative value is the wait-set handle, and a negative value is
/// `-errno`. The wrapper surfaces that raw signed value unchanged.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 waitset_create-result encoding (handle ≥ 0, else -errno).
pub fn waitset_create() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; `waitset_create` takes
    // no arguments and dereferences no user pointer. It mints a handle.
    let ret = unsafe { raw_syscall(NUM_WAITSET_CREATE, [0, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Add or remove a member of a wait-set (`SyscallNumber::WAITSET_CTL`;
/// `plans/USB.md` U3a3).
///
/// `set` is the handle [`waitset_create`] minted. `op` is [`WaitSetOp::Add`]
/// or [`WaitSetOp::Del`]; `kind` selects whether `id` names an IPC call
/// endpoint the caller serves ([`WaitSourceKind::Endpoint`]) or an
/// [`IrqHandle`](rustos_abi::IrqHandle) the caller bound
/// ([`WaitSourceKind::Irq`]); `token` is the caller's opaque value reported by
/// [`waitset_wait`] when this member is ready. On `Add` the kernel resolves
/// and **owner-checks** the named resource against the calling task before
/// recording it, so the set can never observe authority the caller lacks.
///
/// Returns `0` on success, or the raw negative kernel result (`-errno`): an
/// unowned/unknown resource or wrong `(kind, id)` (`NotFound`), a duplicate
/// member (`AlreadyExists`), or an unknown `op`/`kind` (`OutOfRange`). The
/// wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn waitset_ctl(set: u64, op: WaitSetOp, kind: WaitSourceKind, id: u64, token: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; `waitset_ctl` dereferences
    // no user pointer — it resolves `id` against the caller's own resources and
    // records the membership change.
    let ret = unsafe {
        raw_syscall(
            NUM_WAITSET_CTL,
            [
                set,
                u64::from(op.as_u32()),
                u64::from(kind.as_u32()),
                id,
                token,
                0,
            ],
        )
    };
    ret as i64
}

/// Block until a member of a wait-set is ready (`SyscallNumber::WAITSET_WAIT`;
/// `plans/USB.md` U3a3).
///
/// `set` is the handle [`waitset_create`] minted; `timeout_ns` is the relative
/// deadline in nanoseconds (`u64::MAX` for no timeout). The kernel parks the
/// caller off the run queue (never busy-spinning), re-arms each IRQ member's
/// line, and — on the first ready member — writes that member's `token` to
/// `token_out` and returns. An IRQ member's fired edge is consumed exactly
/// like [`irq_wait`]; an endpoint member's readiness is a non-consuming peek
/// drained by [`call_recv`].
///
/// Returns `0` when a member became ready (its token is in `token_out`), or the
/// raw negative kernel result (`-errno`): `TimedOut` on the deadline, or
/// `NotFound` for a forged set handle. The wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn waitset_wait(set: u64, timeout_ns: u64, token_out: &mut u64) -> i64 {
    let token_ptr = (token_out as *mut u64) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; `token_out` is a live
    // exclusive `&mut u64` for the duration of the call, so the pointer denotes
    // writable memory the kernel may fill with the ready member's token; the
    // kernel validates it against the caller's own address space before writing.
    let ret = unsafe { raw_syscall(NUM_WAITSET_WAIT, [set, timeout_ns, token_ptr, 0, 0, 0]) };
    ret as i64
}

/// Recover a usable byte count from a raw `abi-v1` count-result register,
/// clamping to `cap` as defence in depth.
///
/// The kernel encodes a filesystem count result as the standard signed
/// register (count ≥ 0, else `-errno`). A negative value is surfaced as the
/// raw `Err(-errno)`; a non-negative value is clamped to `cap` so a buggy or
/// hostile kernel count can never drive an out-of-bounds slice in the caller
/// (the same posture [`stdin`] and [`users_db_read`] take).
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
#[allow(clippy::cast_possible_truncation)]
#[allow(clippy::cast_sign_loss)]
fn count_result(ret: u64, cap: usize) -> Result<usize, i64> {
    let ret = ret as i64;
    if ret < 0 {
        return Err(ret);
    }
    Ok((ret as usize).min(cap))
}

/// Open the file or directory at the absolute `path` with `flags`
/// (`SyscallNumber::FS_OPEN`), returning the new descriptor number.
///
/// The kernel resolves and authorises `path` through its secured VFS under
/// the caller's kernel-attested identity, applying the
/// create/exclusive/truncate/directory semantics [`OpenFlags`] encodes and
/// every per-inode owner/mode/ACL/capability and mount-flag check; the entry
/// itself is gated on [`rustos_abi::CapabilityId::FS_ACCESS`]. A refused open
/// never produces a descriptor. This is the descriptor-producing primitive
/// the higher-level [`File`] / [`Dir`] wrappers build on; a program names a
/// descriptor, never a device.
///
/// Returns the descriptor (≥ 0) or `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`), the standard `abi-v1`
/// signed-result convention; the wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 fd-result encoding (fd ≥ 0, else -errno).
pub fn fs_open(path: &[u8], flags: OpenFlags) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(ptr, len)` pair against the caller's address space before reading
    // it. `path` is a live shared `&[u8]` for the duration of the call, so the
    // pair denotes readable memory.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_OPEN,
            [ptr, path.len() as u64, u64::from(flags.bits()), 0, 0, 0],
        )
    };
    ret as i64
}

/// Release the caller's open descriptor `fd` (`SyscallNumber::FS_CLOSE`).
///
/// Idempotent from the program's side only in that closing a number the
/// caller does not hold fails closed with `NotFound`; a descriptor resolves
/// only for the task that opened it. Returns `0` on success or `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_close(fd: u32) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; `fs_close` takes no
    // memory operand, only the descriptor number the kernel resolves against
    // the caller's own table.
    let ret = unsafe { raw_syscall(NUM_FS_CLOSE, [u64::from(fd), 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Read up to `buf.len()` bytes from the open descriptor `fd` at byte
/// `offset` into `buf` (`SyscallNumber::FS_READ`), returning the number read
/// (`0` at end of file).
///
/// A single syscall transfers at most [`rustos_abi::FS_IO_MAX`] bytes; a
/// larger `buf` is split across successive calls by [`File::read_at`]. The
/// kernel resolves `fd` against the caller's own descriptor table (a forged
/// or foreign number fails closed), enforces the handle was opened for
/// reading, and validates the `(buf, len)` pair against the caller's address
/// space before writing it.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`): the descriptor is not the
/// caller's, was not opened for reading, the buffer faults, or no filesystem
/// is mounted (`NotImplemented`).
pub fn fs_read(fd: u32, offset: u64, buf: &mut [u8]) -> Result<usize, i64> {
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(buf, len)` pair against the caller's address space before writing it.
    // `buf` is a live exclusive `&mut [u8]` for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_READ,
            [u64::from(fd), offset, ptr, buf.len() as u64, 0, 0],
        )
    };
    count_result(ret, buf.len())
}

/// Write `data` to the open descriptor `fd` at byte `offset`
/// (`SyscallNumber::FS_WRITE`), returning the number of bytes written.
///
/// If the handle was opened with [`OpenFlags::APPEND`] the kernel ignores
/// `offset` and appends at the current end of file. A single syscall
/// transfers at most [`rustos_abi::FS_IO_MAX`] bytes; a larger `data` is split
/// across successive calls by [`File::write_at`]. The kernel resolves `fd`
/// against the caller's own table, enforces the handle was opened for
/// writing, honours the mount's `ro` flag, and validates the `(buf, len)`
/// pair before reading it.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`): the descriptor is not the
/// caller's, was not opened for writing, the mount is read-only, the buffer
/// faults, or no filesystem is mounted (`NotImplemented`).
pub fn fs_write(fd: u32, offset: u64, data: &[u8]) -> Result<usize, i64> {
    let ptr = data.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before reading it.
    // `data` is a live shared `&[u8]` for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_WRITE,
            [u64::from(fd), offset, ptr, data.len() as u64, 0, 0],
        )
    };
    count_result(ret, data.len())
}

/// Read the directory listing of the open directory descriptor `fd` into
/// `buf` (`SyscallNumber::FS_READDIR`), returning the number of bytes the
/// packed [`rustos_abi::DirEntry`] stream occupies.
///
/// The whole listing is delivered or none: a buffer smaller than the packed
/// stream is refused with `BufferTooSmall` rather than truncated, so the
/// caller grows `buf` and retries (the entry count is a discovered capacity,
/// not a fixed ceiling). Walk the returned prefix with
/// [`rustos_abi::DirEntry::decode`] — or use [`Dir::read`], which owns the
/// buffer.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`): the descriptor is not the
/// caller's, the node is not a directory the caller may list, `buf` is too
/// small, or no filesystem is mounted (`NotImplemented`).
pub fn fs_readdir(fd: u32, buf: &mut [u8]) -> Result<usize, i64> {
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(buf, len)` pair against the caller's address space before writing it.
    // `buf` is a live exclusive `&mut [u8]` for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_READDIR,
            [u64::from(fd), ptr, buf.len() as u64, 0, 0, 0],
        )
    };
    count_result(ret, buf.len())
}

/// Read the structural metadata of the open descriptor `fd`
/// (`SyscallNumber::FS_STAT`).
///
/// The kernel fills the caller's [`FileStat`]-sized buffer from the VFS's
/// authorised view of the node; an undersized buffer fails closed. Prefer the
/// typed [`File::stat`], which decodes the record.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`): the descriptor is not the
/// caller's, the buffer is too small or faults, or no filesystem is mounted
/// (`NotImplemented`).
pub fn fs_stat_raw(fd: u32, out: &mut [u8]) -> Result<usize, i64> {
    let ptr = out.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(buf, len)` pair against the caller's address space before writing it.
    // `out` is a live exclusive `&mut [u8]` for the duration of the call.
    let ret = unsafe { raw_syscall(NUM_FS_STAT, [u64::from(fd), ptr, out.len() as u64, 0, 0, 0]) };
    count_result(ret, out.len())
}

/// Set the length of the regular file open at descriptor `fd` to `size`
/// bytes (`SyscallNumber::FS_TRUNCATE`).
///
/// Truncation is a write: the handle must have been opened for writing, the
/// mount must be writable, and the node must be a regular file — each checked
/// kernel-side. Returns `0` on success or `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_truncate(fd: u32, size: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; `fs_truncate` takes no
    // memory operand, only the descriptor and the new size.
    let ret = unsafe { raw_syscall(NUM_FS_TRUNCATE, [u64::from(fd), size, 0, 0, 0, 0]) };
    ret as i64
}

/// Flush the mounted filesystem's pending writes to its backing store
/// (`SyscallNumber::FS_SYNC`).
///
/// `fd` must be one of the caller's own live handles on the mounted volume (a
/// forged or foreign number fails closed); the flush itself is
/// filesystem-wide. Returns `0` on success or `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_sync(fd: u32) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; `fs_sync` takes no
    // memory operand, only the descriptor proving the caller holds a live
    // handle on the mounted volume.
    let ret = unsafe { raw_syscall(NUM_FS_SYNC, [u64::from(fd), 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Create a directory at the absolute `path` (`SyscallNumber::FS_MKDIR`).
///
/// The kernel resolves the parent and authorises the create through the
/// secured VFS under the caller's attested identity (an existing path, a
/// read-only mount, or a permission denial fails closed). Returns `0` on
/// success or `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_mkdir(path: &[u8]) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before reading it.
    // `path` is a live shared `&[u8]` for the duration of the call.
    let ret = unsafe { raw_syscall(NUM_FS_MKDIR, [ptr, path.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Remove the file or empty directory at the absolute `path`
/// (`SyscallNumber::FS_UNLINK`).
///
/// The kernel authorises the removal through the secured VFS under the
/// caller's attested identity (a missing path, a non-empty directory, a
/// read-only mount, or a permission denial fails closed). Returns `0` on
/// success or `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_unlink(path: &[u8]) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before reading it.
    // `path` is a live shared `&[u8]` for the duration of the call.
    let ret = unsafe { raw_syscall(NUM_FS_UNLINK, [ptr, path.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Move the file or directory at absolute `src` to absolute `dst`
/// (`SyscallNumber::FS_RENAME`).
///
/// Both paths must resolve under the same mounted volume. The kernel
/// authorises the move through the secured VFS under the caller's attested
/// identity (a missing source, a non-empty directory destination, a
/// directory-into-its-own-subtree move, a read-only mount, a cross-mount
/// move, or a permission denial fails closed). Returns `0` on success or
/// `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_rename(src: &[u8], dst: &[u8]) -> i64 {
    let src_ptr = src.as_ptr() as usize as u64;
    let dst_ptr = dst.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `(ptr, len)` pairs against the caller's address space before
    // reading them. `src`/`dst` are live shared slices for the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_RENAME,
            [src_ptr, src.len() as u64, dst_ptr, dst.len() as u64, 0, 0],
        )
    };
    ret as i64
}

/// An open file or directory handle: an owned descriptor that issues
/// [`fs_close`] when dropped, so a handle is never leaked.
///
/// Construct one with [`File::open`] (or the [`open`]/[`create`]/[`open_dir`]
/// free functions). The handle's access is fixed by the [`OpenFlags`] it was
/// opened with: a [`File::read_at`] against a handle opened without
/// [`OpenFlags::READ`], or a [`File::write_at`] without [`OpenFlags::WRITE`],
/// fails closed kernel-side. A program holds a descriptor, never a device.
#[derive(Debug)]
pub struct File {
    fd: u32,
}

impl File {
    /// Open `path` with `flags`, returning the owned handle.
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) the [`fs_open`] syscall
    /// returns on any refusal.
    pub fn open(path: &[u8], flags: OpenFlags) -> Result<Self, i64> {
        let ret = fs_open(path, flags);
        if ret < 0 {
            return Err(ret);
        }
        // A non-negative `fs_open` result is a descriptor number, which the
        // kernel always reports within `u32` (the descriptor space the
        // per-process table allocates from); the conversion is exact.
        let fd =
            u32::try_from(ret).map_err(|_| -i64::from(rustos_abi::Errno::OutOfRange.as_i32()))?;
        Ok(Self { fd })
    }

    /// The raw descriptor number this handle owns.
    #[must_use]
    pub fn fd(&self) -> u32 {
        self.fd
    }

    /// Read into the whole of `buf` starting at byte `offset`, splitting the
    /// transfer into [`rustos_abi::FS_IO_MAX`]-sized syscalls, and return the
    /// number of bytes read (short of `buf.len()` at end of file).
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) of the first failing
    /// [`fs_read`].
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let mut done = 0;
        while done < buf.len() {
            let n = fs_read(self.fd, offset + done as u64, &mut buf[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    /// Write the whole of `data` starting at byte `offset` (or appending, if
    /// the handle was opened with [`OpenFlags::APPEND`]), splitting the
    /// transfer into [`rustos_abi::FS_IO_MAX`]-sized syscalls, and return the
    /// number of bytes written.
    ///
    /// Stops early — returning the partial count — if a [`fs_write`] makes no
    /// progress, so a stalled write never loops forever.
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) of the first failing
    /// [`fs_write`].
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, i64> {
        let mut done = 0;
        while done < data.len() {
            let n = fs_write(self.fd, offset + done as u64, &data[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    /// Report this handle's structural metadata.
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) of the [`fs_stat_raw`]
    /// syscall, or [`rustos_abi::Errno::BufferTooSmall`] encoded as `-errno`
    /// if the kernel returns a short record.
    pub fn stat(&self) -> Result<FileStat, i64> {
        let mut buf = [0u8; FileStat::WIRE_LEN];
        let n = fs_stat_raw(self.fd, &mut buf)?;
        if n < FileStat::WIRE_LEN {
            return Err(-i64::from(rustos_abi::Errno::BufferTooSmall.as_i32()));
        }
        FileStat::decode(&buf).map_err(|e| -i64::from(e.as_i32()))
    }

    /// Set this file's length to `size` bytes.
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) of the [`fs_truncate`]
    /// syscall.
    pub fn truncate(&self, size: u64) -> Result<(), i64> {
        let ret = fs_truncate(self.fd, size);
        if ret < 0 {
            Err(ret)
        } else {
            Ok(())
        }
    }

    /// Flush the mounted filesystem's pending writes to its backing store.
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) of the [`fs_sync`] syscall.
    pub fn sync(&self) -> Result<(), i64> {
        let ret = fs_sync(self.fd);
        if ret < 0 {
            Err(ret)
        } else {
            Ok(())
        }
    }
}

impl Drop for File {
    fn drop(&mut self) {
        // Release the descriptor on the way out so a handle is never leaked.
        // A close failure has no continuation here (the handle is gone either
        // way), so the result is intentionally discarded.
        let _ = fs_close(self.fd);
    }
}

/// An open directory handle wrapping a [`File`] opened with
/// [`OpenFlags::DIRECTORY`].
///
/// [`Dir::read`] reads the packed [`rustos_abi::DirEntry`] stream into the
/// caller's buffer; walk it with [`rustos_abi::DirEntry::decode`].
#[derive(Debug)]
pub struct Dir {
    file: File,
}

impl Dir {
    /// The raw descriptor number this directory handle owns.
    #[must_use]
    pub fn fd(&self) -> u32 {
        self.file.fd()
    }

    /// Read the whole directory listing into `buf` as a packed
    /// [`rustos_abi::DirEntry`] stream, returning the number of bytes it
    /// occupies.
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) of the [`fs_readdir`]
    /// syscall — in particular `BufferTooSmall` (encoded as `-errno`) when the
    /// listing does not fit, so the caller grows `buf` and retries.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, i64> {
        fs_readdir(self.file.fd(), buf)
    }
}

/// Open the existing file at the absolute `path` for reading.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`) of [`File::open`].
pub fn open(path: &[u8]) -> Result<File, i64> {
    File::open(path, OpenFlags::READ)
}

/// Create (or truncate) the file at the absolute `path` for writing,
/// creating it if absent.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`) of [`File::open`].
pub fn create(path: &[u8]) -> Result<File, i64> {
    File::open(
        path,
        OpenFlags::WRITE
            .union(OpenFlags::CREATE)
            .union(OpenFlags::TRUNCATE),
    )
}

/// Open the directory at the absolute `path` for listing.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`) of [`File::open`].
pub fn open_dir(path: &[u8]) -> Result<Dir, i64> {
    let file = File::open(path, OpenFlags::DIRECTORY)?;
    Ok(Dir { file })
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
    // The trap seam lives in `rustos-abi-trap` (the single trap home) and is reached here through the `host-seam`
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
        // The plain `spawn` keeps the child on the caller's own console and
        // under the caller's own credential (both inherit sentinels).
        assert_eq!(args[2], CONSOLE_INHERIT);
        assert_eq!(args[3], u64::from(SPAWN_UID_INHERIT));
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn spawn_as_marshals_the_console_and_target_uid() {
        let path = *b"/Apps/Shell.app/Run";
        let (number, args) = capture(9, || {
            // login starting a user's shell on the inherited console under a
            // switched-to uid.
            assert_eq!(spawn_as(&path, CONSOLE_INHERIT, 1000), 9);
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], CONSOLE_INHERIT);
        assert_eq!(args[3], 1000);
        assert_eq!(&args[4..], &[0, 0]);
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
        // `spawn_at` switches no user: the caller's own credential (inherit).
        assert_eq!(args[3], u64::from(SPAWN_UID_INHERIT));
        assert_eq!(&args[4..], &[0, 0]);
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
            assert_eq!(wait(rustos_abi::WAIT_PID_ANY, &mut status), 3);
        });
        assert_eq!(number, NUM_WAIT);
        // `WAIT_PID_ANY` (-1) sign-extends to all-ones in the argument register.
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
        // A deadline already reached must not yield even once (no needless reschedule).
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
        // into an out-of-bounds slice.
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

    #[test]
    fn shm_create_marshals_len_and_the_id_out_pointer() {
        let mut id = 0u64;
        let (number, args) = capture(0x4000, || {
            assert_eq!(shm_create(0x2000, &mut id), 0x4000);
        });
        assert_eq!(number, NUM_SHM_CREATE);
        assert_eq!(args[0], 0x2000); // length
        assert_eq!(args[1], core::ptr::addr_of_mut!(id) as usize as u64); // id_out
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn shm_create_surfaces_negative_errno_encoding() {
        let mut id = 0u64;
        let want = -i64::from(rustos_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(shm_create(0x1000, &mut id), want);
        });
    }

    #[test]
    fn shm_map_marshals_the_handle() {
        let (number, args) = capture(0x8000, || {
            assert_eq!(shm_map(0xDEAD), 0x8000);
        });
        assert_eq!(number, NUM_SHM_MAP);
        assert_eq!(args[0], 0xDEAD);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn shm_unmap_marshals_base_and_len() {
        let (number, args) = capture(0, || {
            assert_eq!(shm_unmap(0x9000, 0x2000), 0);
        });
        assert_eq!(number, NUM_SHM_UNMAP);
        assert_eq!(args[0], 0x9000);
        assert_eq!(args[1], 0x2000);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn waitset_create_marshals_no_arguments() {
        let (number, args) = capture(7, || {
            assert_eq!(waitset_create(), 7);
        });
        assert_eq!(number, NUM_WAITSET_CREATE);
        assert_eq!(args, [0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn waitset_ctl_marshals_set_op_kind_id_and_token() {
        let (number, args) = capture(0, || {
            assert_eq!(
                waitset_ctl(3, WaitSetOp::Add, WaitSourceKind::Irq, 0x1234, 0xAA),
                0
            );
        });
        assert_eq!(number, NUM_WAITSET_CTL);
        assert_eq!(args[0], 3); // set handle
        assert_eq!(args[1], u64::from(WaitSetOp::Add.as_u32()));
        assert_eq!(args[2], u64::from(WaitSourceKind::Irq.as_u32()));
        assert_eq!(args[3], 0x1234); // resource id
        assert_eq!(args[4], 0xAA); // token
        assert_eq!(args[5], 0);
    }

    #[test]
    fn waitset_wait_marshals_set_timeout_and_the_token_out_pointer() {
        let mut token = 0u64;
        let (number, args) = capture(0, || {
            assert_eq!(waitset_wait(5, u64::MAX, &mut token), 0);
        });
        assert_eq!(number, NUM_WAITSET_WAIT);
        assert_eq!(args[0], 5); // set handle
        assert_eq!(args[1], u64::MAX); // timeout
        assert_eq!(args[2], core::ptr::addr_of_mut!(token) as usize as u64); // token_out
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn waitset_wait_surfaces_negative_errno_encoding() {
        let mut token = 0u64;
        let want = -i64::from(rustos_abi::Errno::TimedOut.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(waitset_wait(5, 0, &mut token), want);
        });
    }

    // --- filesystem wrappers (PREREQUISITES.md P-A) -----------------------

    #[test]
    fn fs_open_marshals_path_flags_and_returns_the_descriptor() {
        let path = b"/System/Logs/boot";
        let flags = OpenFlags::READ.union(OpenFlags::WRITE);
        let (number, args) = capture(4, || {
            assert_eq!(fs_open(path, flags), 4);
        });
        assert_eq!(number, NUM_FS_OPEN);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], u64::from(flags.bits()));
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn fs_open_surfaces_negative_errno_encoding() {
        let want = -i64::from(rustos_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(fs_open(b"/x", OpenFlags::READ), want);
        });
    }

    #[test]
    fn fs_close_marshals_the_descriptor() {
        let (number, args) = capture(0, || {
            assert_eq!(fs_close(7), 0);
        });
        assert_eq!(number, NUM_FS_CLOSE);
        assert_eq!(args[0], 7);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn fs_read_marshals_fd_offset_pointer_and_len() {
        let mut buf = [0u8; 16];
        let ptr = buf.as_mut_ptr() as usize as u64;
        let (number, args) = capture(16, || {
            assert_eq!(fs_read(4, 0x1000, &mut buf), Ok(16));
        });
        assert_eq!(number, NUM_FS_READ);
        assert_eq!(args[0], 4);
        assert_eq!(args[1], 0x1000);
        assert_eq!(args[2], ptr);
        assert_eq!(args[3], 16);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_read_clamps_an_oversized_count_to_the_buffer_length() {
        let mut buf = [0u8; 8];
        let (_, _) = capture(9999, || {
            assert_eq!(fs_read(4, 0, &mut buf), Ok(8));
        });
    }

    #[test]
    fn fs_read_surfaces_negative_errno_encoding() {
        let mut buf = [0u8; 4];
        let want = -i64::from(rustos_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(fs_read(4, 0, &mut buf), Err(want));
        });
    }

    #[test]
    fn fs_write_marshals_fd_offset_pointer_and_len() {
        let data = *b"record\n";
        let (number, args) = capture(7, || {
            assert_eq!(fs_write(5, 0x20, &data), Ok(7));
        });
        assert_eq!(number, NUM_FS_WRITE);
        assert_eq!(args[0], 5);
        assert_eq!(args[1], 0x20);
        assert_eq!(args[2], data.as_ptr() as usize as u64);
        assert_eq!(args[3], data.len() as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_readdir_marshals_fd_pointer_and_len() {
        let mut buf = [0u8; 64];
        let ptr = buf.as_mut_ptr() as usize as u64;
        let (number, args) = capture(20, || {
            assert_eq!(fs_readdir(6, &mut buf), Ok(20));
        });
        assert_eq!(number, NUM_FS_READDIR);
        assert_eq!(args[0], 6);
        assert_eq!(args[1], ptr);
        assert_eq!(args[2], 64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn fs_readdir_surfaces_buffer_too_small() {
        let mut buf = [0u8; 4];
        let want = -i64::from(rustos_abi::Errno::BufferTooSmall.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(fs_readdir(6, &mut buf), Err(want));
        });
    }

    #[test]
    fn fs_stat_raw_marshals_fd_pointer_and_len() {
        let mut buf = [0u8; FileStat::WIRE_LEN];
        let ptr = buf.as_mut_ptr() as usize as u64;
        let (number, args) = capture(FileStat::WIRE_LEN as u64, || {
            assert_eq!(fs_stat_raw(4, &mut buf), Ok(FileStat::WIRE_LEN));
        });
        assert_eq!(number, NUM_FS_STAT);
        assert_eq!(args[0], 4);
        assert_eq!(args[1], ptr);
        assert_eq!(args[2], FileStat::WIRE_LEN as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn fs_truncate_marshals_fd_and_size() {
        let (number, args) = capture(0, || {
            assert_eq!(fs_truncate(4, 0x4000), 0);
        });
        assert_eq!(number, NUM_FS_TRUNCATE);
        assert_eq!(args[0], 4);
        assert_eq!(args[1], 0x4000);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_sync_marshals_the_descriptor() {
        let (number, args) = capture(0, || {
            assert_eq!(fs_sync(4), 0);
        });
        assert_eq!(number, NUM_FS_SYNC);
        assert_eq!(args[0], 4);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn fs_mkdir_marshals_path_pointer_and_len() {
        let path = b"/System/Logs/runtime";
        let (number, args) = capture(0, || {
            assert_eq!(fs_mkdir(path), 0);
        });
        assert_eq!(number, NUM_FS_MKDIR);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_unlink_marshals_path_pointer_and_len() {
        let path = b"/System/Logs/old";
        let (number, args) = capture(0, || {
            assert_eq!(fs_unlink(path), 0);
        });
        assert_eq!(number, NUM_FS_UNLINK);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_rename_marshals_both_paths_and_lens() {
        let src = b"/System/Logs/old";
        let dst = b"/System/Logs/new";
        let (number, args) = capture(0, || {
            assert_eq!(fs_rename(src, dst), 0);
        });
        assert_eq!(number, NUM_FS_RENAME);
        assert_eq!(args[0], src.as_ptr() as usize as u64);
        assert_eq!(args[1], src.len() as u64);
        assert_eq!(args[2], dst.as_ptr() as usize as u64);
        assert_eq!(args[3], dst.len() as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn file_open_returns_the_handle_and_drop_closes_it() {
        seam::arm(4);
        let file = File::open(b"/System/Logs/boot", OpenFlags::READ).expect("open succeeds");
        assert_eq!(file.fd(), 4);
        // Dropping the handle releases the descriptor through `fs_close`.
        drop(file);
        let (number, args) = seam::last_call().expect("drop issues a close");
        assert_eq!(number, NUM_FS_CLOSE);
        assert_eq!(args[0], 4);
    }

    #[test]
    fn file_open_surfaces_the_negative_errno() {
        let want = -i64::from(rustos_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        seam::arm(neg);
        assert_eq!(File::open(b"/missing", OpenFlags::READ).err(), Some(want));
    }

    #[test]
    fn file_read_at_issues_a_single_call_for_a_small_buffer() {
        let file = File { fd: 9 };
        let mut buf = [0u8; 8];
        seam::arm(8);
        assert_eq!(file.read_at(0x80, &mut buf), Ok(8));
        let (number, args) = seam::last_call().expect("a read was issued");
        assert_eq!(number, NUM_FS_READ);
        assert_eq!(args[0], 9);
        assert_eq!(args[1], 0x80);
        assert_eq!(args[3], 8);
        core::mem::forget(file);
    }

    #[test]
    fn file_stat_decodes_the_record() {
        let stat = FileStat {
            kind: rustos_abi::FileKind::Regular,
            size: 1234,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
        };
        let mut wire = [0u8; FileStat::WIRE_LEN];
        stat.encode(&mut wire).expect("encode");
        // Arm the seam to report the encoded record by pointing the kernel's
        // copy-out at the test's buffer is not possible here (the host seam
        // records, it does not write), so prove the short-record guard instead.
        let file = File { fd: 9 };
        seam::arm(0); // a zero-length stat result trips the short-record guard
        let want = -i64::from(rustos_abi::Errno::BufferTooSmall.as_i32());
        assert_eq!(file.stat(), Err(want));
        core::mem::forget(file);
    }

    #[test]
    fn create_requests_write_create_truncate() {
        let want = -i64::from(rustos_abi::Errno::NotImplemented.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (number, args) = capture(neg, || {
            assert_eq!(create(b"/System/Logs/seg").err(), Some(want));
        });
        assert_eq!(number, NUM_FS_OPEN);
        let flags = OpenFlags::from_bits(u32::try_from(args[2]).expect("flag bits fit u32"))
            .expect("create requests a legal flag combination");
        assert!(flags.contains(OpenFlags::WRITE));
        assert!(flags.contains(OpenFlags::CREATE));
        assert!(flags.contains(OpenFlags::TRUNCATE));
    }

    #[test]
    fn open_dir_requests_the_directory_flag() {
        let want = -i64::from(rustos_abi::Errno::NotImplemented.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (number, args) = capture(neg, || {
            assert_eq!(open_dir(b"/System/Logs").map(|_| ()), Err(want));
        });
        assert_eq!(number, NUM_FS_OPEN);
        let flags = OpenFlags::from_bits(u32::try_from(args[2]).expect("flag bits fit u32"))
            .expect("open_dir requests a legal flag combination");
        assert!(flags.contains(OpenFlags::DIRECTORY));
    }
}
