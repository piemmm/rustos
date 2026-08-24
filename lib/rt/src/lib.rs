//! `tairix-rt` — the pure-Rust userland runtime.
//!
//! This is the runtime a **first-party TAIRiX program written in Rust** links:
//! it provides the program's `_start` entry trampoline, idiomatic `abi-v1`
//! syscall wrappers, the [`entry!`] macro that names the program's `main`, and
//! the panic handler. TAIRiX is Rust-only, so its own
//! programs use this runtime and never the C ABI.
//!
//! # Relationship to the C ABI (`crt0` + `abi-sys`)
//!
//! `tairix-crt0` and `tairix-abi-sys` are the curated *System runtime / C ABI*
//! class: a libc-equivalent that exists **solely** so
//! a program **not** written in Rust (C, …) can call `abi-v1`. They are not
//! for TAIRiX's own code. `tairix-rt` is the Rust counterpart; both build on
//! the one shared syscall trap (`tairix-abi-trap`), so the
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
//!     tairix_rt::stream_write(b"hello\n");
//!     0
//! }
//!
//! tairix_rt::entry!(main);
//! ```
//!
//! `tairix-rt` provides `_start`, which validates the kernel-supplied
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

extern crate alloc;

use tairix_abi::elevate::{
    elevate_endpoint, ElevateReply, ElevateRequest, ELEVATE_MAX_REQUEST, ELEVATE_REPLY_LEN,
};
use tairix_abi::input::{KeyInput, PointerInput};
pub use tairix_abi::seat::ReleaseSurface;
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::{
    BootFacts, BootId, BootSession, CapabilityId, Errno, FileStat, HwNode, HwRemoveFlags,
    InputMode, LimitKind, MapFlags, OpenFlags, Origin, PowerAction, RandomFlags, ResourceLimit,
    SchedPriority, Signal, SignalIntakeOp, SyscallNumber, TerminalSize, Time64, WaitFlags,
    WaitStatus, WallClockReading, WallTimeState, BOOT_ID_LEN, CONSOLE_INHERIT, ORIGIN_WIRE_LEN,
    SPAWN_UID_INHERIT, STDIN, TERMINAL_SIZE_WIRE_LEN,
};
use tairix_abi_trap::raw_syscall;
use tairix_util::secret::Wiped;

#[cfg(rt_native)]
mod start;

mod startup;

pub mod cachereport;

pub mod io;

pub mod net;

pub mod pressure;

pub mod sync;

pub mod thread;

pub use startup::{arg, arg_count, args, cpu_features, env, env_count, env_var};

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
const NUM_PIPE_CREATE: u64 = SyscallNumber::PIPE_CREATE.as_u16() as u64;

/// `pty_create` syscall number (as above).
const NUM_PTY_CREATE: u64 = SyscallNumber::PTY_CREATE.as_u16() as u64;

/// `pty_set_size` syscall number (as above).
const NUM_PTY_SET_SIZE: u64 = SyscallNumber::PTY_SET_SIZE.as_u16() as u64;

/// `mem_map` syscall number (as above).
const NUM_MEM_MAP: u64 = SyscallNumber::MEM_MAP.as_u16() as u64;

/// `mem_unmap` syscall number (as above).
const NUM_MEM_UNMAP: u64 = SyscallNumber::MEM_UNMAP.as_u16() as u64;

/// `mem_pin` syscall number (as above).
const NUM_MEM_PIN: u64 = SyscallNumber::MEM_PIN.as_u16() as u64;
/// `signal_intake` syscall number (as above).
const NUM_SIGNAL_INTAKE: u64 = SyscallNumber::SIGNAL_INTAKE.as_u16() as u64;
/// `cap_query` syscall number (as above).
const NUM_CAP_QUERY: u64 = SyscallNumber::CAP_QUERY.as_u16() as u64;
/// `sched_set_realtime` syscall number (as above).
const NUM_SCHED_SET_REALTIME: u64 = SyscallNumber::SCHED_SET_REALTIME.as_u16() as u64;

/// `sched_set_priority` syscall number (as above).
const NUM_SCHED_SET_PRIORITY: u64 = SyscallNumber::SCHED_SET_PRIORITY.as_u16() as u64;

/// `system_power` syscall number (as above).
const NUM_SYSTEM_POWER: u64 = SyscallNumber::SYSTEM_POWER.as_u16() as u64;

/// `mem_unpin` syscall number (as above).
const NUM_MEM_UNPIN: u64 = SyscallNumber::MEM_UNPIN.as_u16() as u64;

/// `file_map` syscall number (as above).
const NUM_FILE_MAP: u64 = SyscallNumber::FILE_MAP.as_u16() as u64;

/// `file_unmap` syscall number (as above).
const NUM_FILE_UNMAP: u64 = SyscallNumber::FILE_UNMAP.as_u16() as u64;

/// `volume_attach` syscall number (as above).
const NUM_VOLUME_ATTACH: u64 = SyscallNumber::VOLUME_ATTACH.as_u16() as u64;

/// `volume_detach` syscall number (as above).
const NUM_VOLUME_DETACH: u64 = SyscallNumber::VOLUME_DETACH.as_u16() as u64;

/// `mmio_map` syscall number (as above).
const NUM_MMIO_MAP: u64 = SyscallNumber::MMIO_MAP.as_u16() as u64;

/// `dma_alloc` syscall number (as above).
const NUM_DMA_ALLOC: u64 = SyscallNumber::DMA_ALLOC.as_u16() as u64;

/// `dma_free` syscall number (as above).
const NUM_DMA_FREE: u64 = SyscallNumber::DMA_FREE.as_u16() as u64;

/// `wait` syscall number (as above).
const NUM_WAIT: u64 = SyscallNumber::WAIT.as_u16() as u64;

/// `signal` syscall number (as above).
const NUM_SIGNAL: u64 = SyscallNumber::SIGNAL.as_u16() as u64;

/// `console_foreground` syscall number (as above).
const NUM_CONSOLE_FOREGROUND: u64 = SyscallNumber::CONSOLE_FOREGROUND.as_u16() as u64;

/// `ipc_send` syscall number (as above).
const NUM_IPC_SEND: u64 = SyscallNumber::IPC_SEND.as_u16() as u64;
/// Raw number of the `port_resolve` syscall.
const NUM_PORT_RESOLVE: u64 = SyscallNumber::PORT_RESOLVE.as_u16() as u64;

/// `rlimit_get` syscall number (as above).
const NUM_RLIMIT_GET: u64 = SyscallNumber::RLIMIT_GET.as_u16() as u64;

/// `rlimit_set` syscall number (as above).
const NUM_RLIMIT_SET: u64 = SyscallNumber::RLIMIT_SET.as_u16() as u64;

/// `users_db_read` syscall number (as above).
const NUM_USERS_DB_READ: u64 = SyscallNumber::USERS_DB_READ.as_u16() as u64;

/// `users_db_wait` syscall number (as above).
const NUM_USERS_DB_WAIT: u64 = SyscallNumber::USERS_DB_WAIT.as_u16() as u64;

/// `users_admin` syscall number (as above).
const NUM_USERS_ADMIN: u64 = SyscallNumber::USERS_ADMIN.as_u16() as u64;

/// `console_count` syscall number (as above).
const NUM_CONSOLE_COUNT: u64 = SyscallNumber::CONSOLE_COUNT.as_u16() as u64;

/// `stream_input_mode` syscall number (as above).
const NUM_STREAM_INPUT_MODE: u64 = SyscallNumber::STREAM_INPUT_MODE.as_u16() as u64;

/// `terminal_purge` syscall number (as above).
const NUM_TERMINAL_PURGE: u64 = SyscallNumber::TERMINAL_PURGE.as_u16() as u64;

/// `key_inject` syscall number (as above).
const NUM_KEY_INJECT: u64 = SyscallNumber::KEY_INJECT.as_u16() as u64;

/// `display_acquire` syscall number (as above).
const NUM_DISPLAY_ACQUIRE: u64 = SyscallNumber::DISPLAY_ACQUIRE.as_u16() as u64;

/// `display_release` syscall number (as above).
const NUM_DISPLAY_RELEASE: u64 = SyscallNumber::DISPLAY_RELEASE.as_u16() as u64;

/// `keyboard_read` syscall number (as above).
const NUM_KEYBOARD_READ: u64 = SyscallNumber::KEYBOARD_READ.as_u16() as u64;

/// `pointer_inject` syscall number (as above).
const NUM_POINTER_INJECT: u64 = SyscallNumber::POINTER_INJECT.as_u16() as u64;

/// `pointer_read` syscall number (as above).
const NUM_POINTER_READ: u64 = SyscallNumber::POINTER_READ.as_u16() as u64;

/// `seat_switch` syscall number (as above).
const NUM_SEAT_SWITCH: u64 = SyscallNumber::SEAT_SWITCH.as_u16() as u64;

/// `seat_revoke` syscall number (as above).
const NUM_SEAT_REVOKE: u64 = SyscallNumber::SEAT_REVOKE.as_u16() as u64;

/// `resource_grants` syscall number (as above).
const NUM_RESOURCE_GRANTS: u64 = SyscallNumber::RESOURCE_GRANTS.as_u16() as u64;

/// `clock_get` syscall number (as above).
const NUM_CLOCK_GET: u64 = SyscallNumber::CLOCK_GET.as_u16() as u64;

/// `boot_session_get` syscall number (as above).
const NUM_BOOT_SESSION_GET: u64 = SyscallNumber::BOOT_SESSION_GET.as_u16() as u64;

/// `self_origin` syscall number (as above).
const NUM_SELF_ORIGIN: u64 = SyscallNumber::SELF_ORIGIN.as_u16() as u64;

/// `hw_tree_read` syscall number (as above).
const NUM_HW_TREE_READ: u64 = SyscallNumber::HW_TREE_READ.as_u16() as u64;

/// `hw_tree_wait` syscall number (as above).
const NUM_HW_TREE_WAIT: u64 = SyscallNumber::HW_TREE_WAIT.as_u16() as u64;

/// `random_get` syscall number (as above).
const NUM_RANDOM_GET: u64 = SyscallNumber::RANDOM_GET.as_u16() as u64;

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

/// `call_post` syscall number (as above).
const NUM_CALL_POST: u64 = SyscallNumber::CALL_POST.as_u16() as u64;

/// `call_reap` syscall number (as above).
const NUM_CALL_REAP: u64 = SyscallNumber::CALL_REAP.as_u16() as u64;

/// `call_cancel` syscall number (as above).
const NUM_CALL_CANCEL: u64 = SyscallNumber::CALL_CANCEL.as_u16() as u64;

/// `log_emit` syscall number (as above).
const NUM_LOG_EMIT: u64 = SyscallNumber::LOG_EMIT.as_u16() as u64;

/// `hw_emit_node` syscall number (as above).
const NUM_HW_EMIT_NODE: u64 = SyscallNumber::HW_EMIT_NODE.as_u16() as u64;

/// `hw_remove_node` syscall number (as above).
const NUM_HW_REMOVE_NODE: u64 = SyscallNumber::HW_REMOVE_NODE.as_u16() as u64;

/// `hw_node_health` syscall number (as above).
const NUM_HW_NODE_HEALTH: u64 = SyscallNumber::HW_NODE_HEALTH.as_u16() as u64;

/// `hw_self_node` syscall number (as above).
const NUM_HW_SELF_NODE: u64 = SyscallNumber::HW_SELF_NODE.as_u16() as u64;

/// `shm_create` syscall number (as above).
const NUM_SHM_CREATE: u64 = SyscallNumber::SHM_CREATE.as_u16() as u64;

/// `shm_map` syscall number (as above).
const NUM_SHM_MAP: u64 = SyscallNumber::SHM_MAP.as_u16() as u64;

/// `shm_unmap` syscall number (as above).
const NUM_SHM_UNMAP: u64 = SyscallNumber::SHM_UNMAP.as_u16() as u64;

/// `shm_grant` syscall number (as above).
const NUM_SHM_GRANT: u64 = SyscallNumber::SHM_GRANT.as_u16() as u64;

/// `call_grant` syscall number (as above).
const NUM_CALL_GRANT: u64 = SyscallNumber::CALL_GRANT.as_u16() as u64;

/// `call_peer_seat` syscall number (as above).
const NUM_CALL_PEER_SEAT: u64 = SyscallNumber::CALL_PEER_SEAT.as_u16() as u64;

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

/// `fs_symlink` syscall number (as above).
const NUM_FS_SYMLINK: u64 = SyscallNumber::FS_SYMLINK.as_u16() as u64;

/// `fs_readlink` syscall number (as above).
const NUM_FS_READLINK: u64 = SyscallNumber::FS_READLINK.as_u16() as u64;
const NUM_FS_LINK: u64 = SyscallNumber::FS_LINK.as_u16() as u64;
const NUM_FS_REALPATH: u64 = SyscallNumber::FS_REALPATH.as_u16() as u64;

/// `fs_set_mode` syscall number (as above).
const NUM_FS_SET_MODE: u64 = SyscallNumber::FS_SET_MODE.as_u16() as u64;

/// `fs_set_owner` syscall number (as above).
const NUM_FS_SET_OWNER: u64 = SyscallNumber::FS_SET_OWNER.as_u16() as u64;

/// `fs_attr_*` syscall numbers (as above).
const NUM_FS_ATTR_GET: u64 = SyscallNumber::FS_ATTR_GET.as_u16() as u64;
const NUM_FS_ATTR_SET: u64 = SyscallNumber::FS_ATTR_SET.as_u16() as u64;
const NUM_FS_ATTR_LIST: u64 = SyscallNumber::FS_ATTR_LIST.as_u16() as u64;
const NUM_FS_ATTR_REMOVE: u64 = SyscallNumber::FS_ATTR_REMOVE.as_u16() as u64;
/// `port_bind` syscall number (as above).
const NUM_PORT_BIND: u64 = SyscallNumber::PORT_BIND.as_u16() as u64;
/// `ipc_recv` syscall number (as above).
const NUM_IPC_RECV: u64 = SyscallNumber::IPC_RECV.as_u16() as u64;

/// `call_peer_origin` syscall number (as above).
const NUM_CALL_PEER_ORIGIN: u64 = SyscallNumber::CALL_PEER_ORIGIN.as_u16() as u64;

/// `wall_time_get` syscall number (as above).
const NUM_WALL_TIME_GET: u64 = SyscallNumber::WALL_TIME_GET.as_u16() as u64;

/// `wall_time_set` syscall number (as above).
const NUM_WALL_TIME_SET: u64 = SyscallNumber::WALL_TIME_SET.as_u16() as u64;

/// `boot_id_get` syscall number (as above).
const NUM_BOOT_ID_GET: u64 = SyscallNumber::BOOT_ID_GET.as_u16() as u64;

/// `boot_facts_get` syscall number (as above).
const NUM_BOOT_FACTS_GET: u64 = SyscallNumber::BOOT_FACTS_GET.as_u16() as u64;

/// `sysinfo_introspect` syscall number (as above).
const NUM_SYSINFO_INTROSPECT: u64 = SyscallNumber::SYSINFO_INTROSPECT.as_u16() as u64;

/// `terminal_size` syscall number (as above).
const NUM_TERMINAL_SIZE: u64 = SyscallNumber::TERMINAL_SIZE.as_u16() as u64;

/// `fs_chdir` syscall number (as above).
const NUM_FS_CHDIR: u64 = SyscallNumber::FS_CHDIR.as_u16() as u64;

/// `fs_getcwd` syscall number (as above).
const NUM_FS_GETCWD: u64 = SyscallNumber::FS_GETCWD.as_u16() as u64;

/// `resource_open` syscall number (as above).
const NUM_RESOURCE_OPEN: u64 = SyscallNumber::RESOURCE_OPEN.as_u16() as u64;

/// `fd_grant` syscall number (as above).
const NUM_FD_GRANT: u64 = SyscallNumber::FD_GRANT.as_u16() as u64;

/// `fd_redeem` syscall number (as above).
const NUM_FD_REDEEM: u64 = SyscallNumber::FD_REDEEM.as_u16() as u64;

/// `thread_create` syscall number (as above).
const NUM_THREAD_CREATE: u64 = SyscallNumber::THREAD_CREATE.as_u16() as u64;

/// `thread_exit` syscall number (as above).
const NUM_THREAD_EXIT: u64 = SyscallNumber::THREAD_EXIT.as_u16() as u64;

/// `futex_wait` syscall number (as above).
const NUM_FUTEX_WAIT: u64 = SyscallNumber::FUTEX_WAIT.as_u16() as u64;

/// `futex_wake` syscall number (as above).
const NUM_FUTEX_WAKE: u64 = SyscallNumber::FUTEX_WAKE.as_u16() as u64;

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

/// Write `bytes` to the descriptor `fd` (`SyscallNumber::STREAM_WRITE`),
/// returning the number of bytes the kernel accepted or the [`Errno`] it
/// refused with.
///
/// The one write primitive the whole userland runtime is built on: the
/// program names only a descriptor it already holds, never a device, so the
/// same binary works whatever the spawner backed the stream with (device
/// independence is a property of the stream layer, not the program). The
/// kernel resolves `fd` against the caller's descriptor table and validates
/// the `(buf, len)` pair against the caller's address space before reading
/// it; a short write (fewer than `bytes.len()`) is valid, so the caller
/// loops.
///
/// A refusal — a missing `CAP_CONSOLE_WRITE`, a descriptor opened read-only,
/// a broken pipe, a faulting buffer — is surfaced as its `Errno` rather than
/// collapsed to a zero count, so a caller can never mistake a failure for a
/// stream that simply accepted nothing. The count is clamped to
/// `bytes.len()` as defence in depth, so a buggy kernel count can never
/// drive an out-of-bounds slice in the caller — exactly as
/// [`stream_read_result`] clamps.
///
/// # Errors
///
/// The [`Errno`] the kernel encoded in its negative result.
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the clamped count never exceeds `bytes.len()`.
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 stream-write encoding (count >= 0, else -errno).
#[allow(clippy::cast_sign_loss)] // The negative (`-errno`) case returns early above; the cast runs only when `written >= 0`.
pub(crate) fn stream_write_result(fd: u32, bytes: &[u8]) -> Result<usize, Errno> {
    let ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(buf, len)` against the caller's address space before touching it. `bytes` is a live shared `&[u8]` for the duration
    // of the call, so the `(ptr, len)` pair denotes readable memory.
    let written = unsafe {
        raw_syscall(
            NUM_STREAM_WRITE,
            [u64::from(fd), ptr, bytes.len() as u64, 0, 0, 0],
        )
    } as i64;
    if written < 0 {
        return Err(Errno::from_syscall(written));
    }
    Ok((written as usize).min(bytes.len()))
}

/// Read up to `buf.len()` bytes from the descriptor `fd`
/// (`SyscallNumber::STREAM_READ`) into `buf`, waiting at most `timeout_ns`
/// nanoseconds (`0` waits indefinitely), and return the number of bytes read
/// or the [`Errno`] the kernel refused with.
///
/// The one read primitive the whole userland runtime is built on: the same
/// code path serves fd 0 and any file / pipe / tty / resource-backed
/// descriptor the process holds. The kernel resolves `fd` against the
/// caller's descriptor table and validates the `(buf, len)` pair against the
/// caller's address space before writing it; the stream *backing* owns
/// blocking, so a read with no pending input parks the caller rather than
/// spinning. A short read (fewer than `buf.len()`) is valid, so the caller
/// loops for more.
///
/// `Ok(0)` therefore means end-of-input and nothing else: a refusal (fd not
/// readable, a faulted buffer, [`Errno::TimedOut`] when a bound elapsed with
/// no input) is surfaced as its `Errno`, so a consumer can never silently
/// truncate its input on a failure it mistook for EOF. The count is clamped
/// to `buf.len()` as defence in depth.
///
/// # Errors
///
/// The [`Errno`] the kernel encoded in its negative result.
#[allow(clippy::cast_possible_truncation)] // usize == u64 on every native target; the clamped count never exceeds `buf.len()`.
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 stream-read encoding (count >= 0, else -errno).
#[allow(clippy::cast_sign_loss)] // The negative (`-errno`) case returns early above; the cast runs only when `read >= 0`.
pub(crate) fn stream_read_result(fd: u32, buf: &mut [u8], timeout_ns: u64) -> Result<usize, Errno> {
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(buf, len)` against the caller's address space before touching it. `buf` is a live exclusive `&mut [u8]` for the
    // duration of the call, so the `(ptr, len)` pair denotes writable
    // memory the kernel may fill.
    let read =
        unsafe { raw_syscall(NUM_STREAM_READ, [u64::from(fd), ptr, len, timeout_ns, 0, 0]) } as i64;
    if read < 0 {
        return Err(Errno::from_syscall(read));
    }
    Ok((read as usize).min(buf.len()))
}

/// Set the read line discipline of standard input (fd 0)
/// (`SyscallNumber::STREAM_INPUT_MODE`), returning the raw signed register
/// (`0` on success, else `-errno`).
///
/// The console defaults to [`InputMode::Cooked`], so an interactive user
/// sees what they type at an [`io::Stdin`] read. A program reading a secret it
/// must not render selects [`InputMode::Secret`] (echo suppressed, the
/// activity indicator shown instead — login's password read); a full-screen
/// program that paints its own display selects [`InputMode::Raw`] (echo
/// suppressed, nothing drawn). Either way the caller restores
/// [`InputMode::Cooked`] when it is done, so the next program on the console
/// sees the interactive default.
/// Requires `CAP_CONSOLE_READ`; the kernel performs the echo/indicator
/// itself as part of the read line discipline, so no `CAP_CONSOLE_WRITE` is
/// needed. A build with no console wired, or an fd 0 that is not a readable
/// stream, fails closed with `-errno`; the wrapper surfaces it
/// verbatim so the caller decides how to react.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn set_input_mode(mode: InputMode) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates the capability and resolves fd 0
    // before touching any state.
    let ret = unsafe {
        raw_syscall(
            NUM_STREAM_INPUT_MODE,
            [u64::from(STDIN), u64::from(mode.as_u32()), 0, 0, 0, 0],
        )
    };
    ret as i64
}

/// Discard everything a finished session left on the terminal behind standard
/// input (`SyscallNumber::TERMINAL_PURGE`), returning the raw signed register
/// (`0` on success, else `-errno`).
///
/// The session boundary of a shared terminal: the caller has just watched a
/// session end on this terminal and is about to hand it to whoever comes next,
/// so nothing the session left — the retained screen (including the one it was
/// not showing), the scrollback of a remote emulator, the keystrokes it typed
/// ahead but never read — may still be there. The read line discipline returns
/// to [`InputMode::Cooked`], so the next reader starts from the interactive
/// default.
///
/// Requires `CAP_CONSOLE_READ` **and** `CAP_CONSOLE_WRITE` (the purge discards
/// queued input as well as retained output) and, like every other terminal
/// control, admits only the terminal's controlling owner. A build with no
/// console wired, or an fd 0 that is not a readable stream, fails closed with
/// `-errno`; the wrapper surfaces it verbatim so the caller decides how to
/// react — a purge it could not perform is a fact its caller must see, never
/// one to swallow.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn purge_terminal() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates both capabilities and resolves fd 0
    // before touching any state.
    let ret = unsafe { raw_syscall(NUM_TERMINAL_PURGE, [u64::from(STDIN), 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Inject one decoded keyboard `record` for seat `seat` into the kernel
/// input-focus arbiter (`SyscallNumber::KEY_INJECT`, `plans/PI.md` P11 —
/// input follows the surface owner), returning the raw signed register (the
/// bytes consumed when non-negative, else `-errno`).
///
/// The producer-side call a keyboard-input driver issues after decoding a
/// directly attached keyboard into a [`KeyInput`] key edge: `seat` names
/// the seat the keyboard belongs to (`SEAT_PRIMARY` for the boot seat's
/// directly attached keyboard); the kernel validates `CAP_INPUT_INJECT`,
/// the seat id (`NotFound` for an unknown seat), and the `(buf, len)` pair
/// against the caller's address space, decodes the record fail-closed,
/// and routes it by who holds that seat — a *press* encoded to the seat's
/// foreground text console's tty bytes, or the whole record delivered to
/// its desktop keyboard channel. The driver no longer chooses the encoding
/// or the destination. A malformed record or an unwired arbiter
/// fails closed with `-errno`; the wrapper surfaces the
/// raw signed value so the caller decides how to react.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn key_inject(seat: u64, record: &KeyInput) -> i64 {
    let bytes = record.to_le_bytes();
    let ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_INPUT_INJECT`, the seat id, and the `(buf, len)` pair against
    // the caller's address space before reading it. `bytes` is a live
    // stack array for the duration of the call, so the `(ptr, len)` pair
    // denotes readable memory.
    let ret = unsafe { raw_syscall(NUM_KEY_INJECT, [seat, ptr, bytes.len() as u64, 0, 0, 0]) };
    ret as i64
}

/// Acquire ownership of seat `seat` — one display with its keyboard — as an
/// exclusive, owner-tracked lease (`SyscallNumber::DISPLAY_ACQUIRE`,
/// `plans/DISPLAY.md`), returning the minted lease's generation (`>= 1`)
/// on success or `-errno`.
///
/// The compositing window manager calls this when it takes over a screen
/// (`SEAT_PRIMARY` for the boot seat; further seats are minted per
/// discovered display node and enumerated through `SEAT_LIST`): the kernel
/// records the calling task as that seat's owner, so key edges injected for
/// the seat are delivered as [`KeyInput`] records the owner drains with
/// [`keyboard_read`], and the returned generation is the client's lease
/// handle — the display present right is derived from it
/// (`plans/DISPLAY.md` D4), so a stale pre-revoke handle can never be
/// mistaken for the live grant. An unknown seat id fails closed
/// (`NotFound`), a seat held by another task refuses the claim (`SeatBusy`
/// — ownership is never displaced), and a repeat acquire by the holder is
/// refused (`AlreadyExists`). Requires `CAP_DISPLAY` (owning a seat is
/// privileged).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 generation-or-errno encoding (generation >= 1, else -errno).
pub fn display_acquire(seat: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates `CAP_DISPLAY` before touching state.
    let ret = unsafe { raw_syscall(NUM_DISPLAY_ACQUIRE, [seat, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Release seat `seat` and return its keyboard input to the text console
/// (`SyscallNumber::DISPLAY_RELEASE`,
/// `plans/DISPLAY.md`), returning `0` on success or `-errno`.
///
/// `next` states what becomes of the seat's screen: the text console takes
/// it back ([`ReleaseSurface::Text`]), or it is held cleared for the
/// graphical presenter taking over ([`ReleaseSurface::Handover`]) so the gap
/// shows neither this session's pixels nor a replay of a text screen.
///
/// The inverse of [`display_acquire`]; requires `CAP_DISPLAY`. The release
/// is owner-checked: an unknown seat id fails closed (`NotFound`) and a
/// caller that does not hold the seat is refused (`SeatNotOwner`;
/// `SeatRevoked` once, after an administrative eviction) rather than
/// flipping the seat out from under its owner.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn display_release(seat: u64, next: ReleaseSurface) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates `CAP_DISPLAY` before touching state.
    let ret = unsafe { raw_syscall(NUM_DISPLAY_RELEASE, [seat, next.as_u64(), 0, 0, 0, 0]) };
    ret as i64
}

/// Switch a seat's foreground session — retarget which installed text
/// console an unowned seat's input drains to
/// (`SyscallNumber::SEAT_SWITCH`, `plans/DISPLAY.md` D3), returning `0` on
/// success or `-errno`.
///
/// The seat manager (`seatmgr`) calls this to move the foreground across
/// sessions — the `chvt` analogue. Requires `CAP_SEAT_ADMIN` (the
/// seat-multiplexing authority); the kernel validates the seat id and the
/// console index against the installed topology and refuses an unknown
/// either with `NotFound`, and every switch is audit-logged. A held seat
/// keeps routing to its owner until the lease ends.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn seat_switch(seat_id: u64, console: u32) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates `CAP_SEAT_ADMIN` and both indices
    // before touching state.
    let ret = unsafe { raw_syscall(NUM_SEAT_SWITCH, [seat_id, u64::from(console), 0, 0, 0, 0]) };
    ret as i64
}

/// Forcibly revoke a seat's current lease — evict a wedged or
/// switched-away owner (`SyscallNumber::SEAT_REVOKE`, `plans/DISPLAY.md`
/// D3), returning `0` on success or `-errno`.
///
/// The seat manager (`seatmgr`) calls this to reclaim a seat. Requires
/// `CAP_SEAT_ADMIN`; the kernel validates the seat id (`NotFound` for an
/// unknown seat), refuses an unowned seat (`SeatNotOwner` — there is no
/// lease to revoke), and audit-logs every eviction with the evicted
/// owner's task id. The evicted owner's next owner-gated call fails
/// closed with the distinct `SeatRevoked`, so the loss is observable.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0 on success, else -errno).
pub fn seat_revoke(seat_id: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates `CAP_SEAT_ADMIN` and the seat id
    // before touching state.
    let ret = unsafe { raw_syscall(NUM_SEAT_REVOKE, [seat_id, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Read one decoded keyboard event from seat `seat`'s keyboard channel into
/// `buf` (`SyscallNumber::KEYBOARD_READ`, `plans/PI.md`
/// P11), returning the raw signed register (the bytes written — one
/// [`KeyInput`] record's [`KeyInput::WIRE_LEN`], or `0` when the channel is
/// momentarily drained — when non-negative, else `-errno`).
///
/// The task that owns the seat (the window manager) drains the
/// records the kernel routed to it while it held the seat. The kernel
/// validates `CAP_INPUT_READ`, the seat id (`NotFound` for an unknown
/// seat), owner-gates the drain against that seat's live lease (a
/// non-owner is refused with `SeatNotOwner` /
/// `SeatRevoked`), and validates the `(buf, len)` pair against the caller's
/// address space; a `buf` shorter than
/// [`KeyInput::WIRE_LEN`] fails closed with `-errno`. A
/// zero return is a valid empty read, so the caller loops.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn keyboard_read(seat: u64, buf: &mut [u8]) -> i64 {
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_INPUT_READ`, the seat id, and the `(buf, len)` pair against the
    // caller's address space before writing it. `buf` is a live exclusive
    // `&mut [u8]` for the duration of the call, so the `(ptr, len)` pair
    // denotes writable memory.
    let ret = unsafe { raw_syscall(NUM_KEYBOARD_READ, [seat, ptr, buf.len() as u64, 0, 0, 0]) };
    ret as i64
}

/// Inject one decoded pointer `record` for seat `seat` into the kernel seat
/// registry (`SyscallNumber::POINTER_INJECT`, `plans/PI.md` P11 — the
/// pointer analogue of [`key_inject`]), returning the raw signed register
/// (the bytes consumed when non-negative, else `-errno`).
///
/// The producer-side call a pointer-input driver issues after decoding a
/// discovered pointing device into a [`PointerInput`] event: `seat` names
/// the seat the device belongs to (`SEAT_PRIMARY` for the boot seat); the
/// kernel validates `CAP_INPUT_INJECT`, the seat id (`NotFound` for an
/// unknown seat), and the `(buf, len)` pair against the caller's address
/// space, decodes the record fail-closed, and routes it by who holds that
/// seat — the whole record delivered to a held seat's pointer channel, or
/// consumed and discarded while the seat is unowned (the text console has
/// no pointer consumer). The driver never chooses the destination. A
/// malformed record or an unwired registry fails closed with `-errno`; the
/// wrapper surfaces the raw signed value so the caller decides how to
/// react.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn pointer_inject(seat: u64, record: &PointerInput) -> i64 {
    let bytes = record.to_le_bytes();
    let ptr = bytes.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_INPUT_INJECT`, the seat id, and the `(buf, len)` pair against
    // the caller's address space before reading it. `bytes` is a live
    // stack array for the duration of the call, so the `(ptr, len)` pair
    // denotes readable memory.
    let ret = unsafe { raw_syscall(NUM_POINTER_INJECT, [seat, ptr, bytes.len() as u64, 0, 0, 0]) };
    ret as i64
}

/// Read one decoded pointer event from seat `seat`'s pointer channel into
/// `buf` (`SyscallNumber::POINTER_READ`, `plans/PI.md` P11 — the pointer
/// analogue of [`keyboard_read`]), returning the raw signed register (the
/// bytes written — one [`PointerInput`] record's
/// [`PointerInput::WIRE_LEN`], or `0` when the channel is momentarily
/// drained — when non-negative, else `-errno`).
///
/// The task that owns the seat (the window manager) drains the records the
/// kernel routed to it while it held the seat. The kernel validates
/// `CAP_INPUT_READ`, the seat id (`NotFound` for an unknown seat),
/// owner-gates the drain against that seat's live lease (a non-owner is
/// refused with `SeatNotOwner` / `SeatRevoked`), and validates the
/// `(buf, len)` pair against the caller's address space; a `buf` shorter
/// than [`PointerInput::WIRE_LEN`] fails closed with `-errno`. A zero
/// return is a valid empty read, so the caller loops.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-or-errno encoding (count ≥ 0, else -errno).
pub fn pointer_read(seat: u64, buf: &mut [u8]) -> i64 {
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_INPUT_READ`, the seat id, and the `(buf, len)` pair against the
    // caller's address space before writing it. `buf` is a live exclusive
    // `&mut [u8]` for the duration of the call, so the `(ptr, len)` pair
    // denotes writable memory.
    let ret = unsafe { raw_syscall(NUM_POINTER_READ, [seat, ptr, buf.len() as u64, 0, 0, 0]) };
    ret as i64
}

/// Enumerate the device-resource grants the kernel minted for the calling
/// driver task into `buf` (`SyscallNumber::RESOURCE_GRANTS`, `plans/PI.md` P10 chunk 5d-2), returning the raw signed
/// register: the total number of bytes written — consecutive
/// [`tairix_abi::hwtree::GrantedResource`] records — when non-negative, else
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
/// [`tairix_abi::MsiAllocation`] the kernel minted — the virtual interrupt
/// line plus the doorbell `(address, data)` to program into the function's
/// MSI capability.
///
/// A user-space **bus** driver wiring a PCI function for MSI calls this; it
/// is gated by [`tairix_abi::CapabilityId::IRQ_BIND`] (the same privilege the
/// driver needs to `irq_bind` the returned line). The kernel grants the
/// caller a device resource for the line, so it may both `irq_bind` it and
/// forward it as an [`tairix_abi::hwtree::HwResource::irq`] onto a child node
/// it publishes — never ambient authority.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure — most
/// commonly `NotImplemented` on a platform with no MSI controller, or
/// `OutOfRange` when the vector space is exhausted — and treats a malformed
/// short reply as a fail-closed error rather than a usable value.
pub fn msi_alloc() -> Result<tairix_abi::MsiAllocation, i64> {
    let mut buf = [0u8; tairix_abi::MsiAllocation::WIRE_LEN];
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
        Ok(written) if written >= tairix_abi::MsiAllocation::WIRE_LEN => {}
        _ => return Err(-(tairix_abi::Errno::BufferTooSmall as i64)),
    }
    tairix_abi::MsiAllocation::from_bytes(&buf).map_err(|e| -(e as i64))
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
/// [`tairix_abi::CapabilityId::HW_EMIT`], and the kernel admits the node only
/// when every [`tairix_abi::hwtree::HwResource`] it requests is covered by one
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
/// gated by the same [`tairix_abi::CapabilityId::HW_EMIT`], and the kernel
/// retires `node_id` **only** when its parent is the calling driver's own
/// matched node — a child the caller itself published — together with every
/// descendant, so a driver can never remove a node it does not own
/// (no ambient authority). An unknown id, or a node the
/// caller does not own, fails closed with `-errno`.
///
/// `flags` selects the removal posture. [`HwRemoveFlags::empty`] is a
/// **surprise removal** — a device that physically vanished — and always
/// proceeds. [`HwRemoveFlags::ORDERLY`] is the **stop-if-idle** posture an
/// administrator uses to retire a still-present device (stopping an assembled
/// RAID array): the kernel refuses with [`Errno::Busy`] (`-errno`), removing
/// nothing, while a volume is still attached on a block-service endpoint the
/// node declares, so a live mounted volume is never turned into a surprise
/// removal.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 0-or-`-errno` encoding.
pub fn hw_remove_node(node_id: u32, flags: HwRemoveFlags) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_HW_EMIT`, decodes `flags`, and resolves `node_id` against the live
    // tree on the far side of the trap. The call passes no memory operand —
    // `node_id` and the flag word are scalars in args 0 and 1.
    let ret = unsafe {
        raw_syscall(
            NUM_HW_REMOVE_NODE,
            [u64::from(node_id), u64::from(flags.bits()), 0, 0, 0, 0],
        )
    };
    ret as i64
}

/// Publish the fault-domain `health` of the interior node the calling driver
/// owns into the live hardware tree (`SyscallNumber::HW_NODE_HEALTH`),
/// returning the raw signed register: `0` once recorded, else `-errno`.
///
/// A bus/hub/controller driver turns a controller-wide blip into *one*
/// fault-domain event by reporting its own node's
/// [`tairix_abi::blkio::FaultDomainState`] here; the device manager's
/// reactive watch then reacts to a coherent recovery episode across the
/// subtree rather than N spurious child removals. This is a **distinct**
/// signal from [`hw_remove_node`] (surprise removal): the node stays
/// present, only its health changes, so a merely-recovering subtree is never
/// torn down. It is gated by the same [`tairix_abi::CapabilityId::HW_EMIT`],
/// and the kernel records the health of the calling driver's *own* matched
/// node (no ambient authority — a driver cannot report another's health). An
/// out-of-range health, a caller with no loaded node, or an absent node
/// fails closed with `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 0-or-`-errno` encoding.
pub fn hw_node_health(health: tairix_abi::blkio::FaultDomainState) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_HW_EMIT`, the health discriminant, and resolves the caller's own
    // node on the far side of the trap. The call passes no memory operand —
    // the health discriminant is a scalar in arg 0.
    let ret = unsafe {
        raw_syscall(
            NUM_HW_NODE_HEALTH,
            [u64::from(health.as_u8()), 0, 0, 0, 0, 0],
        )
    };
    ret as i64
}

/// Report the hardware-tree node id the calling driver was autoloaded for
/// (`SyscallNumber::HW_SELF_NODE`), returning the raw signed register: the
/// caller's own node id (`≥ 0`) on success, else `-errno`.
///
/// A leaf block driver uses this to locate itself in the discovered topology
/// so it can read the published [`tairix_abi::blkio::FaultDomainState`] of its
/// parent bus/hub/controller chain
/// ([`tairix_abi::hwtree::ancestor_imposed_status`]) and attribute a
/// controller-wide blip to the fault domain rather than to the disk. It needs
/// no capability (learning one's *own* node id is the unprivileged
/// self-identity baseline), and the kernel resolves the node from the caller's
/// task id — never a caller-supplied id, so a driver only ever learns its own
/// identity, never the global tree. A caller with no matched node (not an
/// autoloaded driver) fails closed with `-errno` (`NotFound`).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 id-or-`-errno` encoding.
pub fn hw_self_node() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel resolves the
    // caller's own matched node on the far side of the trap. The call takes no
    // arguments and no memory operand, so all six argument registers are zero.
    let ret = unsafe { raw_syscall(NUM_HW_SELF_NODE, [0, 0, 0, 0, 0, 0]) };
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
/// [`tairix_abi::time::COARSE_CLOCK_GRANULARITY_NS`] (one microsecond), since
/// a sub-microsecond timer is a side-channel primitive the kernel withholds
/// from untrusted callers. The wrapper performs no
/// coarsening of its own — the value it returns is exactly what the kernel
/// handed back.
///
/// [`CapabilityId::TIME_HIRES`]: tairix_abi::CapabilityId::TIME_HIRES
#[must_use]
pub fn clock_get() -> u64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `clock_get`
    // takes no arguments and no memory operand, so all six argument registers
    // are zero; its result is the `U64` nanosecond reading.
    unsafe { raw_syscall(NUM_CLOCK_GET, [0, 0, 0, 0, 0, 0]) }
}

/// Read the operator's one-boot login choice
/// (`SyscallNumber::BOOT_SESSION_GET`).
///
/// Reports what the operator asked for at the pre-boot Supervisor's
/// `continue text` / `continue gui`, or [`BootSession::Unset`] when this boot
/// made no choice — in which case the stored login-type default decides. It
/// requires no capability: the choice names no account, grants no authority,
/// and reveals no secret.
///
/// Fails closed: a value the kernel returns that is not a known session (an
/// error, or a discriminant this ABI does not define) reads as
/// [`BootSession::Unset`], so an unreadable answer defers to the stored
/// default rather than forcing a session.
#[must_use]
pub fn boot_session() -> BootSession {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `boot_session_get` takes no
    // arguments and no memory operand, so all six argument registers are
    // zero; its result is the `U64` session discriminant.
    let raw = unsafe { raw_syscall(NUM_BOOT_SESSION_GET, [0, 0, 0, 0, 0, 0]) };
    BootSession::from_u64(raw).unwrap_or_default()
}

/// Yield-loop until `now()` reaches `deadline_ns` — the **degraded
/// fallback** of [`ClockDelay`]'s [`delay_us`](tairix_abi::Delay::delay_us),
/// used only when the kernel refuses the sleep wait-set ([`sleep_waitset`])
/// so a timed park is unavailable: it reads the monotonic clock through
/// `now` and surrenders the CPU through `yield_fn` between reads, keeping
/// the timed contract without a hard spin. A deadline already in the past
/// returns immediately without yielding. The generic seams keep the loop
/// host-testable against a deterministic clock without issuing a real trap.
fn spin_until_ns(deadline_ns: u64, mut now: impl FnMut() -> u64, mut yield_fn: impl FnMut()) {
    while now() < deadline_ns {
        yield_fn();
    }
}

/// Park, off-CPU, until `now()` reaches `deadline_ns`.
///
/// The core of [`ClockDelay`]'s [`delay_us`](tairix_abi::Delay::delay_us):
/// between clock reads it blocks through `park` (a kernel timed park for
/// the remaining nanoseconds), so the task sleeps instead of yielding in a
/// loop. A spurious early wake re-parks for the remainder; a deadline
/// already in the past returns immediately without parking. The generic
/// seams keep the loop host-testable against a deterministic clock.
fn park_until_ns(deadline_ns: u64, mut now: impl FnMut() -> u64, mut park: impl FnMut(u64)) {
    loop {
        let reading = now();
        if reading >= deadline_ns {
            return;
        }
        park(deadline_ns - reading);
    }
}

/// The process's lazily created sleep wait-set handle, stored plus one so
/// `0` means "not yet created". A wait-set with **no members** parked with a
/// timeout is a pure kernel timed park: no member can ever become ready, so
/// `waitset_wait` blocks the task until the deadline elapses. Cached per
/// process so a delay costs one syscall, not a create per call.
static SLEEP_WAITSET_PLUS_ONE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// The process's sleep wait-set (created on first use), or `None` when the
/// kernel refuses one — the caller then degrades to the cooperative
/// yield wait rather than losing the timed contract.
fn sleep_waitset() -> Option<u64> {
    use core::sync::atomic::Ordering;
    let cached = SLEEP_WAITSET_PLUS_ONE.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached - 1);
    }
    let set = waitset_create();
    if set < 0 {
        return None;
    }
    #[allow(clippy::cast_sign_loss)] // `set >= 0` is the wait-set handle.
    let set = set as u64;
    SLEEP_WAITSET_PLUS_ONE.store(set + 1, Ordering::Relaxed);
    Some(set)
}

/// Nanoseconds in one microsecond — the [`ClockDelay`] conversion factor.
const NANOS_PER_MICRO: u64 = 1_000;

/// Park, off the CPU, for at least `duration_ns` nanoseconds.
///
/// The runtime's one timed park, and the timed counterpart of
/// [`park_forever`]: the task blocks on the process's memberless sleep
/// wait-set, so the kernel's one-shot timer wakes it — no yield loop and no
/// periodic wakes, and a spurious early wake re-parks for the remainder. A
/// zero duration returns without parking. Only when the kernel refuses a
/// wait-set does it degrade to the cooperative yield wait, which keeps the
/// timed contract rather than shortening it.
///
/// [`ClockDelay`]'s driver-facing `delay_us` is this same park; anything
/// that already holds a nanosecond span — an animation's next frame, a
/// timed retry — waits here directly rather than rounding through
/// microseconds.
pub fn park_ns(duration_ns: u64) {
    // Compute the deadline from the clock the wait re-checks, saturating so
    // a reading near `u64::MAX` can never wrap the deadline below `now`
    // (which would return instantly); the monotonic clock realistically
    // never approaches that, but the wait must not silently shorten.
    let deadline = clock_get().saturating_add(duration_ns);
    match sleep_waitset() {
        Some(set) => park_until_ns(deadline, clock_get, |remaining_ns| {
            // The set has no members, so this is a pure timed park; the
            // outer loop re-checks the clock, so an early return (a
            // torn-down set) still honours the deadline.
            let mut token = 0u64;
            let _ = waitset_wait(set, remaining_ns, &mut token);
        }),
        // The kernel refused a wait-set (handle exhaustion): degrade to the
        // cooperative yield wait rather than shortening the timed contract.
        None => spin_until_ns(deadline, clock_get, yield_now),
    }
}

/// The userland [`Delay`](tairix_abi::Delay) implementation: timed waits and
/// a monotonic clock backed by the [`clock_get`] syscall.
///
/// A driver process (or any program) that must honour a hardware-dictated
/// settle window — a PCIe link train, a USB hub power-on-good / reset-recovery
/// window — hands one of these to the bring-up code that takes a
/// [`Delay`](tairix_abi::Delay). It lives here, in the one userland runtime,
/// so every driver process shares a single clock-backed `Delay` rather than
/// each rolling its own over [`clock_get`].
///
/// The wait genuinely sleeps: [`delay_us`](tairix_abi::Delay::delay_us) is
/// [`park_ns`] over the microsecond window, so the task blocks off the CPU
/// rather than yielding in a loop. It carries no authority — `clock_get`
/// and the wait-set need no capability — and holds no state, so it is
/// `Copy` and trivially shareable.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClockDelay;

impl ClockDelay {
    /// A new clock-backed delay. Equivalent to [`ClockDelay::default`].
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl tairix_abi::Delay for ClockDelay {
    fn delay_us(&self, us: u32) {
        park_ns(u64::from(us).saturating_mul(NANOS_PER_MICRO));
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
/// ([`tairix_abi::CONSOLE_INHERIT`]): a spawned session
/// member (login's shell, a shell's job) stays on the console its parent
/// was driving. To start a process on a *different* installed console —
/// PID 1 launching one login per console (`plans/PI.md` P11) — use
/// [`spawn_at`].
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: a non-negative value is the new PID, and a
/// negative value is `-errno` (recover the [`tairix_abi::Errno`]
/// discriminant as `-ret`). The wrapper surfaces that raw signed value so
/// the caller decides how to react to a failed spawn — it adds no authority
/// and hides no error.
#[must_use]
pub fn spawn(path: &[u8]) -> i64 {
    // Inherit both the caller's console and its attested credential: a
    // spawned session member runs as the same user, on the same console, as
    // its parent. No startup-strings block: the child receives its
    // registered default arguments and an empty environment.
    spawn_raw(path, CONSOLE_INHERIT, SPAWN_UID_INHERIT, &[])
}

/// Spawn the embedded program at `path` handing the child the argument
/// vector `args` and environment `env` the caller chose
/// (`SyscallNumber::SPAWN`, `plans/APPS.md` §8 — the shell's launch form).
///
/// The strings are encoded into one `tairix_abi::process` startup-vector
/// block (the `PSV1` format the kernel writes into the child's image) and
/// handed to the kernel, which bounds, stages, and re-validates the block
/// before building the child's own copy — the strings are data and carry
/// no authority, and the kernel mints the child's stack canary itself.
/// Passing empty `args` and `env` is a deliberate choice: the child then
/// starts with an empty argument vector and environment, unlike [`spawn`],
/// whose child receives the program's registered default arguments.
/// Environment entries follow the conventional `NAME=value` byte spelling
/// ([`env_var`] splits at the first `=`).
///
/// `console` is [`tairix_abi::CONSOLE_INHERIT`] or an installed console
/// index; `target_uid` is [`tairix_abi::SPAWN_UID_INHERIT`] or a concrete
/// uid to switch to (kernel-gated on `CAP_SPAWN_AS_USER`), exactly as for
/// [`spawn_at`] and [`spawn_as`]. Over-long or over-many strings fail
/// closed with `-errno` from the shared encoder before the kernel is ever
/// entered.
#[must_use]
pub fn spawn_with(
    path: &[u8],
    console: u64,
    target_uid: u32,
    args: &[&[u8]],
    env: &[&[u8]],
) -> i64 {
    let len = match tairix_abi::process_start_encoded_len(args, env) {
        Ok(len) => len,
        Err(err) => return -i64::from(err.as_i32()),
    };
    let mut block = alloc::vec![0u8; len];
    // The canary and cpu-features fields are the kernel's to mint for the
    // child (the migration-safe common set, from the boot-time detection); the
    // encoder requires values, so carry zero and the kernel ignores them.
    if let Err(err) = tairix_abi::process_start_write_into(&mut block, args, env, 0, 0) {
        return -i64::from(err.as_i32());
    }
    spawn_raw(path, console, target_uid, &block)
}

/// Spawn the embedded program at `path` with an explicit
/// [`tairix_abi::SpawnAttach`]
/// block — the child's credential, base console, and per-descriptor wires
/// — plus the argument vector and environment the caller chose
/// (`SyscallNumber::SPAWN`, `plans/SPAWN.md` SP10: the shell's redirection
/// and pipeline launch form).
///
/// The attach block wires the child's fd 0–3 onto pre-opened descriptors
/// of the **caller's own** open table — files ([`fs_open`]), resources
/// ([`resource_open`]), or pipe ends ([`pipe_create`]) — each owner-checked
/// kernel-side before any child state exists; a forged or wrong-direction
/// handle refuses the spawn whole. The strings travel exactly as in
/// [`spawn_with`].
#[must_use]
pub fn spawn_attached(
    path: &[u8],
    attach: &tairix_abi::SpawnAttach,
    args: &[&[u8]],
    env: &[&[u8]],
) -> i64 {
    let len = match tairix_abi::process_start_encoded_len(args, env) {
        Ok(len) => len,
        Err(err) => return -i64::from(err.as_i32()),
    };
    let mut block = alloc::vec![0u8; len];
    // The canary and cpu-features fields are the kernel's to mint for the
    // child (the migration-safe common set, from the boot-time detection); the
    // encoder requires values, so carry zero and the kernel ignores them.
    if let Err(err) = tairix_abi::process_start_write_into(&mut block, args, env, 0, 0) {
        return -i64::from(err.as_i32());
    }
    spawn_encoded(path, &attach.to_le_bytes(), &block)
}

/// The shared `SyscallNumber::SPAWN` trap the [`spawn`], [`spawn_at`],
/// [`spawn_as`], [`spawn_with`], and [`spawn_attached`] wrappers issue:
/// one raw call site so the argument layout is defined once (the attach
/// block in slots 2/3, the optional startup-strings block in slots 4/5).
///
/// `console` is [`tairix_abi::CONSOLE_INHERIT`] or an installed console index;
/// `target_uid` is [`tairix_abi::SPAWN_UID_INHERIT`] (start under the caller's
/// own credential) or a concrete uid to switch to (which the kernel gates on
/// `CAP_SPAWN_AS_USER`). The pair is carried in an all-`Inherit` attach
/// block (`plans/SPAWN.md` SP10); an empty `strings` slice means "no
/// block" (the zero/zero pair), so the child receives the program's
/// registered default arguments. The kernel encodes the result as a signed
/// register: a non-negative value is the new PID, a negative value is
/// `-errno`.
#[must_use]
fn spawn_raw(path: &[u8], console: u64, target_uid: u32, strings: &[u8]) -> i64 {
    let attach = tairix_abi::SpawnAttach {
        target_uid,
        console,
        ..tairix_abi::SpawnAttach::INHERIT
    };
    spawn_encoded(path, &attach.to_le_bytes(), strings)
}

/// The one raw `SyscallNumber::SPAWN` call site: `(path, attach, strings)`
/// marshalled into the six ABI registers.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 spawn-result encoding (PID ≥ 0, else -errno).
fn spawn_encoded(path: &[u8], attach: &[u8], strings: &[u8]) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    let (strings_ptr, strings_len) = if strings.is_empty() {
        (0, 0)
    } else {
        (strings.as_ptr() as usize as u64, strings.len() as u64)
    };
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `(path, len)`, `(attach, attach_len)`, and `(strings, strings_len)`
    // against the caller's address space before touching them. All three
    // slices are live shared `&[u8]`s for the duration of the call, so
    // each `(ptr, len)` pair denotes readable memory.
    let ret = unsafe {
        raw_syscall(
            NUM_SPAWN,
            [
                ptr,
                path.len() as u64,
                attach.as_ptr() as usize as u64,
                attach.len() as u64,
                strings_ptr,
                strings_len,
            ],
        )
    };
    ret as i64
}

/// Create a pipe — a bounded, kernel-buffered unidirectional byte stream —
/// returning `(read_fd, write_fd)`, two descriptors of the calling
/// process's **own** open table (`SyscallNumber::PIPE_CREATE`,
/// `plans/SPAWN.md` SP10).
///
/// The ends are read/written through [`fs_read`] / [`fs_write`] (a pipe
/// ignores the file offset) and closed through [`fs_close`]. A read on an
/// empty pipe blocks until bytes arrive or every write end is closed (then
/// end-of-stream, `0`); a write to a full pipe blocks until space frees,
/// and a write with no reader left fails with
/// [`tairix_abi::Errno::BrokenPipe`]. An end is handed to a spawned child
/// through a [`tairix_abi::FdWire::Handle`] wire in [`spawn_attached`]'s
/// attach block. Unprivileged: a pipe reaches only the caller's own table.
///
/// # Errors
///
/// The raw negative `-errno` register on refusal (recover the
/// [`tairix_abi::Errno`] as `-ret`).
pub fn pipe_create() -> Result<(u32, u32), i64> {
    let mut fds = [0u32; 2];
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the out-pointer against the caller's address space before writing
    // the two descriptors. `fds` is live exclusive memory for the call.
    let ret = unsafe {
        raw_syscall(
            NUM_PIPE_CREATE,
            [fds.as_mut_ptr() as usize as u64, 0, 0, 0, 0, 0],
        )
    };
    #[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno encoding.
    let signed = ret as i64;
    if signed < 0 {
        return Err(signed);
    }
    Ok((fds[0], fds[1]))
}

/// Create a pseudo-terminal — one kernel object joining a **master** end
/// and a **slave** end whose slave carries a console-class line discipline
/// — returning `(master_fd, slave_fd)`, two descriptors of the calling
/// process's **own** open table, at the initial geometry `rows`×`cols`
/// (`SyscallNumber::PTY_CREATE`, `plans/PTY.md`).
///
/// The graphical terminal hosts its shell over a pty instead of two raw
/// pipes, so the shell sees a real tty: local echo, canonical line editing,
/// `Ctrl-C`/`Ctrl-Z` job control, the raw/cooked/secret mode switch, a
/// queryable window size, and `ONLCR` newline cooking. Both ends are opened
/// read/write and served by [`fs_read`] / [`fs_write`] (a pty ignores the
/// file offset) and closed through [`fs_close`]; a read on an empty ring
/// blocks until bytes arrive or the peer closes, and a write with no peer
/// left fails with [`tairix_abi::Errno::BrokenPipe`]. The slave is handed to
/// a spawned shell through a [`tairix_abi::FdWire::Handle`] wire in
/// [`spawn_attached`]'s attach block. Unprivileged: a pty reaches only the
/// caller's own table.
///
/// # Errors
///
/// The raw negative `-errno` register on refusal (recover the
/// [`tairix_abi::Errno`] as `-ret`): a zero or oversized `rows`/`cols` is
/// [`tairix_abi::Errno::OutOfRange`].
pub fn pty_create(rows: u16, cols: u16) -> Result<(u32, u32), i64> {
    let mut fds = [0u32; 2];
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the out-pointer against the caller's address space before writing
    // the two descriptors. `fds` is live exclusive memory for the call.
    let ret = unsafe {
        raw_syscall(
            NUM_PTY_CREATE,
            [
                fds.as_mut_ptr() as usize as u64,
                u64::from(rows),
                u64::from(cols),
                0,
                0,
                0,
            ],
        )
    };
    #[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno encoding.
    let signed = ret as i64;
    if signed < 0 {
        return Err(signed);
    }
    Ok((fds[0], fds[1]))
}

/// Set the character-cell geometry of the pseudo-terminal `master_fd` is the
/// **master** end of, to `rows`×`cols` (`SyscallNumber::PTY_SET_SIZE`,
/// `plans/PTY.md`).
///
/// The graphical terminal's window-resize path — the tty `TIOCSWINSZ`
/// analogue: when the user drag-resizes the terminal window the emulator
/// recomputes the new character grid and calls this so the shared window size
/// both pty ends observe ([`terminal_size`]) tracks the real window, and the
/// shell's prompt sizing and any full-screen program re-lay-out. Unprivileged:
/// it reaches only the caller's own pty.
///
/// # Errors
///
/// The raw negative `-errno` register on refusal (recover the
/// [`tairix_abi::Errno`] as `-ret`): a zero or oversized `rows`/`cols` is
/// [`tairix_abi::Errno::OutOfRange`], and an `fd` that is not a pty **master**
/// of the caller is [`tairix_abi::Errno::NotFound`].
pub fn pty_set_size(master_fd: u32, rows: u16, cols: u16) -> Result<(), i64> {
    // SAFETY: `raw_syscall` is always safe to invoke; the call carries no
    // pointers and the kernel validates the descriptor and geometry before
    // touching any state.
    let ret = unsafe {
        raw_syscall(
            NUM_PTY_SET_SIZE,
            [
                u64::from(master_fd),
                u64::from(rows),
                u64::from(cols),
                0,
                0,
                0,
            ],
        )
    };
    #[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno encoding.
    let signed = ret as i64;
    if signed < 0 {
        return Err(signed);
    }
    Ok(())
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
/// [`tairix_abi::CONSOLE_INHERIT`] for `console` to keep the child on the
/// caller's own console. A running process can never change its *own*
/// identity (there is no setuid-self).
#[must_use]
pub fn spawn_as(path: &[u8], console: u64, target_uid: u32) -> i64 {
    spawn_raw(path, console, target_uid, &[])
}

/// Spawn the embedded program registered under the absolute `path` with
/// its standard streams attached to the installed console `console`
/// (`SyscallNumber::SPAWN`, `plans/PI.md` P11).
///
/// The console-selecting form of [`spawn`]: `console` names an index in
/// the kernel's installed console list (its length is reported by
/// [`console_count`]); an index with no installed console fails closed
/// with `-errno` (`NotFound`). PID 1 `init` uses this to start one login
/// session per installed text console (the video console when a display
/// is active, else the discovered UART).
#[must_use]
pub fn spawn_at(path: &[u8], console: u32) -> i64 {
    // A specific console, but the caller's own credential (no user switch):
    // PID 1 launching one login per console runs each as the same principal
    // it runs as.
    spawn_raw(path, u64::from(console), SPAWN_UID_INHERIT, &[])
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
/// [`tairix_abi::Errno`] discriminant as `-ret`) — a frame exhaustion is
/// reported as [`tairix_abi::Errno::OutOfMemory`] (deterministic OOM, never a panic). The wrapper surfaces that raw signed
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
/// (recover the [`tairix_abi::Errno`] discriminant as `-ret`), following the
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

/// Mark the calling process's entire anonymous memory — current and
/// future — as pinned: ineligible for the compressed `ramzip` tier and any
/// future lower swap tier (`SyscallNumber::MEM_PIN`,
/// `plans/STRESSTEST.md` ST2).
///
/// Gated by `CAP_MEM_PIN` and bounded by the caller's effective
/// `pinned-memory-bytes` limit; both refusals surface as `-errno`
/// (`PermissionDenied` / `OutOfRange`) so the caller can report the
/// refusal and continue unpinned — for a monitor or load controller the
/// pin is incidental, never fatal. Returns `0` on success (already pinned
/// is success), following the standard `abi-v1` signed-result convention;
/// the wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn mem_pin() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel checks
    // the capability and the pinned-memory bound on the far side of the
    // trap. No user pointer is passed.
    let ret = unsafe { raw_syscall(NUM_MEM_PIN, [0; 6]) };
    ret as i64
}

/// Create a second thread of execution inside the calling process's own
/// address space (`SyscallNumber::THREAD_CREATE`, `plans/THREADS.md` T3b),
/// returning its thread id or `-errno`.
///
/// `entry` is the address the new thread begins executing at and `arg` the
/// value placed in its first-argument register; `stack_len` is the user-stack
/// size to reserve, or [`tairix_abi::THREAD_STACK_DEFAULT`] for the caller's
/// effective `stack-bytes` bound; `tls_base` is the thread's initial thread
/// pointer (`0` for none); `clear_on_exit` is a naturally aligned `u32` in the
/// caller's own memory the kernel zeroes and futex-wakes when this thread
/// dies (`0` for none).
///
/// The **kernel** reserves the thread's stack and the unbacked guard page
/// below it, and releases the whole region when the thread dies — so the
/// runtime owns no stack memory, a stack overrun faults deterministically, and
/// a detached thread cannot leak its stack. Unprivileged: the new thread runs
/// in the caller's own isolated space under the caller's own capability record,
/// so it grants no authority.
///
/// This is the raw marshalling wrapper. [`thread::Thread::spawn`] is the safe
/// surface a program uses: it owns the closure transfer, the join rendezvous,
/// and the thread's own `thread_exit`.
///
/// # Safety
///
/// `entry` must name a function in an executable mapping of this process that
/// obeys the freestanding thread-entry contract: it takes `arg` in the
/// first-argument register and **never returns** (there is no return address to
/// resume — it must end by calling [`thread_exit`] or `exit`). `clear_on_exit`,
/// when non-zero, must remain a live, naturally aligned `u32` this process owns
/// until the thread dies, because the kernel writes it at that moment; freeing
/// it earlier would let the kernel zero four bytes of unrelated memory.
/// `tls_base`, when non-zero, must name readable memory of this process.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 thread_create-result encoding (tid > 0, else -errno).
pub unsafe fn thread_create(
    entry: u64,
    arg: u64,
    stack_len: usize,
    tls_base: u64,
    clear_on_exit: u64,
) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // every argument on the far side of the trap, probing `entry`, `tls_base`,
    // and `clear_on_exit` against this process's own mappings before it admits
    // the thread. The obligations this wrapper's own contract adds are the
    // caller's (an entry that never returns, a clear-on-exit word that outlives
    // the thread).
    let ret = unsafe {
        raw_syscall(
            NUM_THREAD_CREATE,
            [entry, arg, stack_len as u64, tls_base, clear_on_exit, 0],
        )
    };
    ret as i64
}

/// End the calling thread without ending its siblings
/// (`SyscallNumber::THREAD_EXIT`, `plans/THREADS.md` T3b).
///
/// Never returns. The thread's clear-on-exit word is zeroed and futex-woken
/// (releasing a joiner), its stack and per-thread kernel state are released,
/// and it is reaped. The **last** thread of a process to end is a process exit
/// with status `0` — exactly what falling off the end of `main` gives.
///
/// A correct kernel never returns control here; should it nonetheless do so,
/// this must not return to a caller that has no continuation, so it re-issues
/// the call. That is a fail-closed loop over a terminating syscall, not a
/// busy-wait.
pub fn thread_exit() -> ! {
    loop {
        // SAFETY: `raw_syscall` is always safe to invoke — the kernel
        // validates the call on the far side of the trap. `thread_exit` takes
        // no arguments and dereferences no user pointer.
        unsafe {
            let _ = raw_syscall(NUM_THREAD_EXIT, [0; 6]);
        }
    }
}

/// Block until the 32-bit word at `uaddr` is woken, unless it no longer holds
/// `expected` (`SyscallNumber::FUTEX_WAIT`, `plans/THREADS.md` decision 5).
///
/// `timeout_ns` is a relative timeout, [`u64::MAX`] for none. Returns `0` when
/// woken, or `-errno`: [`tairix_abi::Errno::WouldBlock`] when the word does not
/// hold `expected` (the caller re-tests and retries — the lost-wake-up race
/// closing, not a failure), [`tairix_abi::Errno::TimedOut`] when the timeout
/// elapses, and [`tairix_abi::Errno::Interrupted`] when the thread is being
/// terminated.
///
/// A wake is **advisory**: the caller always re-tests its own condition, so a
/// spurious wake costs one loop iteration. This is the one blocking primitive
/// [`sync::Mutex`], [`sync::Condvar`], and [`thread::JoinHandle::join`] are
/// built over, which is what keeps an uncontended lock pure user-space atomics.
///
/// # Safety
///
/// `uaddr` must name a naturally aligned, live `u32` in this process's own
/// address space for the duration of the call: the kernel reads that word to
/// decide whether to park.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub unsafe fn futex_wait(uaddr: u64, expected: u32, timeout_ns: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `uaddr`'s alignment and resolves it against this process's own space
    // before reading it. Keeping the word live for the call is the caller's
    // obligation, stated above.
    let ret = unsafe {
        raw_syscall(
            NUM_FUTEX_WAIT,
            [uaddr, u64::from(expected), timeout_ns, 0, 0, 0],
        )
    };
    ret as i64
}

/// Wake up to `count` threads of the calling process blocked in
/// [`futex_wait`] on `uaddr` (`SyscallNumber::FUTEX_WAKE`), returning how many
/// were woken or `-errno`.
///
/// Waiters are released oldest-first, so a `count` of 1 is a genuine wake-one
/// rather than a thundering herd, and repeated contention cannot move an older
/// waiter behind newer arrivals. [`u32::MAX`] wakes all of them. Waking nobody
/// is success: a waiter that has not parked yet re-tests the word itself.
///
/// # Safety
///
/// `uaddr` must be a naturally aligned address in this process's own address
/// space — the same word the waiters named. The kernel dereferences nothing
/// here (the key is `(process, address)`), so an address naming no live word
/// simply wakes no one.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 count-result encoding (count >= 0, else -errno).
pub unsafe fn futex_wake(uaddr: u64, count: u32) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the futex key is
    // `(this process, uaddr)`, resolved from the caller's kernel-attested
    // record, so a wake can never reach another principal's waiters.
    let ret = unsafe { raw_syscall(NUM_FUTEX_WAKE, [uaddr, u64::from(count), 0, 0, 0, 0]) };
    ret as i64
}

/// Clear the calling process's [`mem_pin`] mark, restoring its anonymous
/// memory's eligibility for the swap tiers (`SyscallNumber::MEM_UNPIN`,
/// `plans/STRESSTEST.md` ST2).
///
/// Unprivileged (releasing one's own exemption grants nothing) and
/// idempotent: already unpinned is success. Returns `0` on success or
/// `-errno`, following the standard `abi-v1` signed-result convention;
/// the wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn mem_unpin() -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel only
    // clears the caller's own pin mark. No user pointer is passed.
    let ret = unsafe { raw_syscall(NUM_MEM_UNPIN, [0; 6]) };
    ret as i64
}

/// Operate on the calling process's own signal intake — the fail-closed
/// signal-observation opt-in (`SyscallNumber::SIGNAL_INTAKE`,
/// `plans/STRESSTEST.md` ST3).
///
/// With the intake enabled ([`SignalIntakeOp::Enable`]), a
/// termination-request signal (`Interrupt`/`Terminate`) delivered to the
/// process is recorded as one pending observable event instead of
/// terminating it; the process parks on a wait-set member of kind
/// [`tairix_abi::WaitSourceKind::Signal`] ([`waitset_ctl`], id `0`) and
/// drains with [`SignalIntakeOp::Take`], which returns the drained
/// signal's wire discriminant. `Kill` stays unconditionally fatal, and a
/// second termination request while one is pending undrained escalates to
/// the default terminate path (`^C ^C` still kills). Unprivileged and
/// audited per call.
///
/// Returns the non-negative op result (`0`, or `Take`'s drained
/// discriminant) or `-errno` (recover the [`tairix_abi::Errno`]
/// discriminant as `-ret`): `WouldBlock` for a `Take` with nothing
/// pending or a `Disable` with an undrained observation, `NotFound` for a
/// `Take` without the opt-in. The wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 value-result encoding (≥ 0, else -errno).
pub fn signal_intake(op: SignalIntakeOp) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel
    // validates the op and acts only on the caller's own intake. No user
    // pointer is passed.
    let ret = unsafe { raw_syscall(NUM_SIGNAL_INTAKE, [u64::from(op.as_u32()), 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Query whether the calling process's own effective capability set holds
/// `cap` (`SyscallNumber::CAP_QUERY`).
///
/// A pure, unaudited observer, like [`clock_get`]: the kernel consults only
/// the caller's own already-established set (never another principal's) and
/// grants nothing by being asked. Returns `true` when held, `false`
/// otherwise; the wrapper hides no error because the syscall itself has none
/// to report.
#[must_use]
pub fn cap_query(cap: CapabilityId) -> bool {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the capability id and reads only the caller's own effective set on the
    // far side of the trap. No user pointer is passed.
    let ret = unsafe { raw_syscall(NUM_CAP_QUERY, [u64::from(cap.as_u16()), 0, 0, 0, 0, 0]) };
    ret == 1
}

/// Set the calling task's scheduling class — enter (`realtime` true) or
/// leave (false) the strict-priority real-time band
/// (`SyscallNumber::SCHED_SET_REALTIME`, `plans/USB.md`).
///
/// A real-time task is dispatched ahead of every time-shared task on its
/// CPU and is never preempted by one, so a CPU-bound workload cannot delay
/// its wake — the guarantee an interrupt-serving driver needs (the
/// microkernel threaded-IRQ / `SCHED_FIFO` analogue). The whole call is
/// gated by `CAP_SCHED_REALTIME` in both directions; the usual caller
/// elevates itself once at start-up, then blocks on its device IRQ, so
/// every subsequent wake is strict-priority.
///
/// Returns `0` on success (setting the class the task already holds is
/// success) or `-errno` (recover the [`tairix_abi::Errno`] discriminant as
/// `-ret`): `PermissionDenied` without the capability. The wrapper hides no
/// error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn sched_set_realtime(realtime: bool) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel checks the
    // capability and acts only on the caller's own task on the far side of
    // the trap. No user pointer is passed.
    let ret = unsafe { raw_syscall(NUM_SCHED_SET_REALTIME, [u64::from(realtime), 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Map `len` bytes of the open file `fd`, starting at the page-aligned
/// file byte `offset`, into the calling process's **own** address space as
/// a demand-paged, read-only private mapping
/// (`SyscallNumber::FILE_MAP` — the `mmap(2)` shape).
///
/// No page is read at call time: the kernel backs each page on first
/// access, so a mapping may exceed RAM by orders of magnitude (a
/// multi-terabyte file is viewed through the pages actually touched). `fd`
/// must be open for reading and filesystem-backed; every demand-paged read
/// is re-checked by the secured VFS under the mapping-time identity, and
/// the mapping survives a later `fs_close(fd)`. Touching a page wholly
/// at/past end-of-file terminates the process (the `SIGBUS` analogue), so
/// callers bound their accesses by the file size (`fs_stat`). The mapping
/// is never writable and never executable (W^X).
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: a non-negative value is the base address
/// of the new region and a negative value is `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`). The wrapper surfaces
/// that raw signed value; it adds no authority and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 file_map-result encoding (base ≥ 0, else -errno).
pub fn file_map(fd: u32, offset: u64, len: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `file_map` dereferences no user
    // pointer; it reserves a region in the caller's own space and returns
    // its base, so no memory operand is passed.
    let ret = unsafe { raw_syscall(NUM_FILE_MAP, [u64::from(fd), offset, len, 0, 0, 0]) };
    ret as i64
}

/// Release the whole file mapping of `len` bytes based at `base` previously
/// returned by [`file_map`] from the calling process's own address space
/// (`SyscallNumber::FILE_UNMAP`).
///
/// Only the exact whole region can be released; pages never touched were
/// never backed and cost nothing, and the kernel zeroes the frames it
/// reclaims (secret hygiene). Returns `0` on success or `-errno` (recover
/// the [`tairix_abi::Errno`] discriminant as `-ret`), following the
/// standard `abi-v1` signed-result convention; the wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 file_unmap-result encoding (0, else -errno).
pub fn file_unmap(base: u64, len: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(base, len)` pair against the caller's own recorded mappings
    // before any teardown. No user pointer is dereferenced.
    let ret = unsafe { raw_syscall(NUM_FILE_UNMAP, [base, len, 0, 0, 0, 0]) };
    ret as i64
}

/// Attach a filesystem driver to a runtime block source and publish the
/// volume's root (`SyscallNumber::VOLUME_ATTACH`, `plans/DEVICES.md` D3b).
///
/// `request` is an encoded [`tairix_abi::volume::VolumeAttachRequest`]
/// naming the block-service endpoint + shared data window the caller holds
/// as grants, the probed partition extent, the filesystem type, and the
/// catalog name. Requires `CAP_FS_MOUNT`; the kernel re-validates every
/// field against live state and fails closed. Returns `0` on success or
/// `-errno` (recover the [`tairix_abi::Errno`] discriminant as `-ret`).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn volume_attach(request: &[u8]) -> i64 {
    let ptr = request.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(ptr, len)` pair against the caller's address space before
    // reading it. `request` is a live shared `&[u8]` for the duration of
    // the call.
    let ret = unsafe { raw_syscall(NUM_VOLUME_ATTACH, [ptr, request.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Detach a runtime-attached volume: flush it, retract its mount, and
/// unpublish its root (`SyscallNumber::VOLUME_DETACH`, `plans/DEVICES.md`
/// D3b).
///
/// `request` is an encoded [`tairix_abi::volume::VolumeDetachRequest`]
/// (the volume's stable 16-byte identity plus the force byte). Requires
/// `CAP_FS_MOUNT`; only a volume attached through [`volume_attach`] can be
/// detached. A plain detach fails closed on a flush error (or an
/// unavailable, surprise-removed volume) rather than discarding
/// uncommitted data; a force detach retracts the volume anyway,
/// deliberately discarding the retained set with its own audit event.
/// Returns `0` on success or `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn volume_detach(request: &[u8]) -> i64 {
    let ptr = request.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(ptr, len)` pair against the caller's address space before
    // reading it. `request` is a live shared `&[u8]` for the duration of
    // the call.
    let ret = unsafe { raw_syscall(NUM_VOLUME_DETACH, [ptr, request.len() as u64, 0, 0, 0, 0]) };
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
/// [`tairix_abi::Errno`] discriminant as `-ret`). The wrapper surfaces that
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
/// [`tairix_abi::Errno`] discriminant as `-ret`) — `device_out` is left
/// untouched on a negative result. The wrapper surfaces that raw signed
/// value so the caller decides how to react; it adds no authority and hides
/// no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 dma_alloc-result encoding (base ≥ 0, else -errno).
pub fn dma_alloc(handle: u64, len: usize, device_out: &mut u64) -> i64 {
    let ptr = core::ptr::from_mut::<u64>(device_out) as usize as u64;
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
/// Returns `0` on success, or `-errno` (recover the [`tairix_abi::Errno`]
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
/// [`tairix_abi::IrqHandle`] (`SyscallNumber::IRQ_BIND`).
///
/// `line` is the architecture interrupt-line identifier the driver received
/// as an [`HwResourceKind::Irq`](tairix_abi::hwtree::HwResourceKind) grant on
/// its matched node — a discovered value, never a board
/// constant. The call carries `CAP_IRQ_BIND` (enforced by the kernel before
/// any state is touched); the minted handle is re-keyed to the calling task,
/// so only this task can `irq_wait` on it.
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the raw `IrqHandle`, and a
/// negative value is `-errno` (recover the [`tairix_abi::Errno`] discriminant
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
/// `handle` is the [`tairix_abi::IrqHandle`] a prior [`irq_bind`] minted for
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

/// Wait for a child-process event, reading back the typed status record
/// (`SyscallNumber::WAIT`, `plans/SPAWN.md` SP6/SP9).
///
/// `pid` is either a specific child's PID or [`tairix_abi::WAIT_PID_ANY`] to
/// wait for whichever of the caller's children reports next. On success the
/// kernel writes a [`tairix_abi::WaitStatusRecord`] — decoded here into the
/// typed [`WaitStatus`] (`Exited` for a reaped child, `Stopped` for a child
/// halted by [`Signal::Stop`] when `flags` carries
/// [`WaitFlags::STOPPED`]) — and returns the reported child's PID. A
/// process may only wait on its **own** children; the kernel validates the
/// parent/child relationship and fails closed.
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: a non-negative value is the reported
/// child's PID, and a negative value is `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`) — `status` is left
/// untouched on a negative result. A record the kernel wrote but this
/// wrapper cannot decode is refused as `-OutOfRange` rather than guessed
/// at (fail closed). The wrapper surfaces the raw signed value so the
/// caller decides how to react; it adds no authority and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 wait-result encoding (PID ≥ 0, else -errno).
pub fn wait(pid: i32, status: &mut WaitStatus, flags: WaitFlags) -> i64 {
    let mut record = tairix_abi::WaitStatusRecord::default();
    let ptr = core::ptr::addr_of_mut!(record) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `status` pointer against the caller's address space before
    // writing the record to it. `record` is a live exclusive local for the
    // duration of the call, so the pointer denotes writable memory the
    // kernel may fill.
    let ret = unsafe {
        raw_syscall(
            NUM_WAIT,
            [i32_arg(pid), ptr, u64::from(flags.bits()), 0, 0, 0],
        )
    };
    let ret = ret as i64;
    if ret >= 0 {
        match record.decode() {
            Ok(decoded) => *status = decoded,
            Err(err) => return -i64::from(err.as_i32()),
        }
    }
    ret
}

/// Poll for a child process without blocking (`SyscallNumber::WAIT` with
/// [`WaitFlags::NONBLOCK`]) — the non-blocking companion to [`wait`].
///
/// This is the reap a shell's job control performs to report finished
/// background jobs before the next prompt, and PID 1 `init` uses to reap the
/// session without parking. `pid` is a specific child's PID or
/// [`tairix_abi::WAIT_PID_ANY`]. If a matching child has already exited it is
/// reaped: its exit code is written into `status` and its PID returned. If a
/// matching child is still running the kernel does **not** block; it returns
/// the raw negative encoding of [`tairix_abi::Errno::WouldBlock`] and leaves
/// `status` untouched.
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the reaped child's PID, and a
/// negative value is `-errno` (recover the [`tairix_abi::Errno`] discriminant
/// as `-ret`) — `Errno::WouldBlock` means "no child ready yet", any other
/// negative value is a genuine failure (e.g. `NotFound`, no such child). The
/// wrapper surfaces that raw signed value so the caller decides how to react;
/// it adds no authority and hides no error.
#[must_use]
pub fn try_wait(pid: i32, status: &mut WaitStatus) -> i64 {
    wait(pid, status, WaitFlags::NONBLOCK)
}

/// Block until the child selected by `pid` **terminates**, reap it, and
/// report its exit code — the simple form of [`wait`] for a parent with no
/// job control (PID 1 reaping a session, login reaping a shell).
///
/// Never reports a stopped child (it passes no [`WaitFlags::STOPPED`]), so
/// `code` is always an exit code. Returns the reaped child's PID, or the
/// `-errno` encoding exactly as [`wait`] does; `code` is untouched on a
/// negative result.
#[must_use]
pub fn wait_exit(pid: i32, code: &mut i32) -> i64 {
    let mut status = WaitStatus::Exited(0);
    let ret = wait(pid, &mut status, WaitFlags::empty());
    if ret >= 0 {
        match status {
            WaitStatus::Exited(exit_code) => *code = exit_code,
            // Unreachable without the STOPPED flag; refuse rather than
            // fabricate an exit code from a stop report.
            WaitStatus::Stopped(_) => return -i64::from(tairix_abi::Errno::OutOfRange.as_i32()),
        }
    }
    ret
}

/// Deliver control signal `signal` to process `pid`
/// (`SyscallNumber::SIGNAL`, `plans/SPAWN.md` SP7, `plans/NEW-TASKBAR.md`
/// T11).
///
/// The kernel identifies the sender from its own current-task slot (never a
/// caller-supplied identity) and settles the target in precedence order: a
/// live **child** of the caller needs no capability, a target owned by the
/// caller's **own principal** needs none either, and only a caller holding
/// `CAP_PROC_CONTROL` may signal a process belonging to a *different*
/// principal. Everything else fails closed, and the cross-principal decision
/// is audited whichever way it goes.
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: `0` on success, and a negative value is `-errno`
/// (recover the [`tairix_abi::Errno`] discriminant as `-ret`) —
/// `Errno::NotFound` when `pid` names no live task, `Errno::PermissionDenied`
/// when the caller holds no authority over another principal's process, and
/// `Errno::NotImplemented` until the kernel's signal producer is installed.
/// The wrapper surfaces that raw signed value; it adds no authority and hides
/// no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn signal(pid: i32, signal: Signal) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel authorises
    // the target and validates the signal value on the far side of the trap.
    // `signal` dereferences no user pointer.
    let ret = unsafe {
        raw_syscall(
            NUM_SIGNAL,
            [i32_arg(pid), u64::from(signal.as_u32()), 0, 0, 0, 0],
        )
    };
    ret as i64
}

/// Move process `pid` to the time-shared scheduling service level
/// `priority` (`SyscallNumber::SCHED_SET_PRIORITY`, `plans/NEW-TASKBAR.md`
/// T12).
///
/// The kernel identifies the caller from its own current-task slot (never a
/// caller-supplied identity) and settles the target exactly as [`signal`]
/// does: a live **child** of the caller needs no capability, a target owned
/// by the caller's **own principal** needs none either, and only a caller
/// holding `CAP_PROC_CONTROL` may act on a process belonging to a
/// *different* principal. **Raising** the level (toward
/// [`SchedPriority::High`]) additionally requires `CAP_PROC_CONTROL`
/// whatever the target rule, so no user can weight their own work above
/// other principals' fair share; lowering and re-stating the current level
/// follow the plain target rule. Cross-principal outcomes and every raise
/// attempt are audited.
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: `0` on success, and a negative value is
/// `-errno` (recover the [`tairix_abi::Errno`] discriminant as `-ret`) —
/// `Errno::NotFound` when `pid` names no live task,
/// `Errno::PermissionDenied` when the caller holds no authority over the
/// target or the raise, and `Errno::NotImplemented` until the kernel's
/// scheduler control is installed. The wrapper surfaces that raw signed
/// value; it adds no authority and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn sched_set_priority(pid: i32, priority: SchedPriority) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel authorises
    // the target and validates the level value on the far side of the trap.
    // `sched_set_priority` dereferences no user pointer.
    let ret = unsafe {
        raw_syscall(
            NUM_SCHED_SET_PRIORITY,
            [i32_arg(pid), u64::from(priority.as_u32()), 0, 0, 0, 0],
        )
    };
    ret as i64
}

/// Power the machine off or restart it
/// (`SyscallNumber::SYSTEM_POWER`, `plans/NEW-TASKBAR.md` T13).
///
/// The kernel identifies the caller from its own current-task slot (never a
/// caller-supplied identity) and admits the transition only to a holder of
/// `CAP_SYSTEM_POWER` — stopping the machine ends every other principal's
/// session, so it is an administrator's authority. It then flushes every
/// mounted volume before asking the platform to stop: a volume that will
/// not flush aborts the transition and the machine keeps running, rather
/// than losing writes that never reached stable media.
///
/// **This call returns only when the transition was refused.** A transition
/// the platform performs never comes back, so there is no success value to
/// report. The kernel encodes a refusal as a signed register following the
/// standard `abi-v1` convention: a negative value is `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`) —
/// `Errno::PermissionDenied` when the caller does not hold the capability,
/// the flush's own error when a volume could not be flushed, and
/// `Errno::NotSupported` on a port with no power-off or reset primitive.
/// The wrapper surfaces that raw signed value; it adds no authority and
/// hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn system_power(action: PowerAction) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel checks the
    // caller's capability and decodes the action on the far side of the
    // trap. `system_power` dereferences no user pointer.
    let ret = unsafe {
        raw_syscall(
            NUM_SYSTEM_POWER,
            [u64::from(action.as_u32()), 0, 0, 0, 0, 0],
        )
    };
    ret as i64
}

/// Grant (or release) the console's controlling (foreground) ownership —
/// the exclusive drain right on its input queue and the target the
/// cooked-mode line discipline delivers `^C`/`^Z` to
/// (`SyscallNumber::CONSOLE_FOREGROUND`, `plans/SPAWN.md` SP9,
/// `plans/DISPLAY.md` D5 — the `tcsetpgrp` analogue).
///
/// `fd` is a readable inherited standard-stream descriptor naming the
/// console (the shell passes [`tairix_abi::STDIN`]); `pid` is a live child
/// of the caller to make the owner, or `0` to release. While an owner is
/// recorded, only it may `stream_read` or `stream_input_mode` that
/// console — every other task sees `Errno::NotForeground`. The kernel
/// authorises the child through the same parent/child bookkeeping
/// `wait`/`signal` use, owner/granter-checks the transition itself (a
/// bystander can neither take nor clear the ownership), and fails closed
/// on everything else.
///
/// The kernel encodes the result as a signed register following the
/// standard `abi-v1` convention: `0` on success, and a negative value is
/// `-errno`. The wrapper surfaces that raw signed value; it adds no
/// authority and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn console_foreground(fd: u32, pid: i32) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel resolves
    // `fd` against the caller's own descriptor table and authorises `pid`
    // on the far side of the trap. `console_foreground` dereferences no
    // user pointer.
    let ret = unsafe {
        raw_syscall(
            NUM_CONSOLE_FOREGROUND,
            [u64::from(fd), i32_arg(pid), 0, 0, 0, 0],
        )
    };
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
/// inherited ceiling requires [`tairix_abi::CapabilityId::RLIMIT_RAISE`]. Returns `0` on success or `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`), the standard `abi-v1`
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
/// caller parses with the fail-closed `tairix-users` parser. Gated
/// kernel-side on [`tairix_abi::CapabilityId::USERS_READ`] — only the
/// authentication principal (login) holds it; the wrapper adds no
/// authority. Sizing `buf` at the format's own
/// 64 KiB maximum (`tairix-users` `MAX_DB_LEN`) always suffices: a
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

/// Apply one typed user/group administration request
/// (`SyscallNumber::USERS_ADMIN`, `plans/CAPABILITY_USE.md` CU4),
/// returning the response byte count written into `out` (`0` for a
/// mutating operation).
///
/// `req` carries one encoded
/// [`tairix_abi::users_admin::UsersAdminRequest`] record (built with
/// its `encode_into`, so both sides share one layout definition); a
/// list operation's response is written into `out` and decoded with
/// the matching `decode_user_list` / `decode_group_list`. Gated
/// kernel-side on [`tairix_abi::CapabilityId::USER_ADMIN`] — the
/// account-administration authority — with the finer never-widen /
/// last-administrator / format rules enforced in the kernel engine;
/// the wrapper adds no authority.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure: the
/// caller lacks the capability, the request is malformed, a rule
/// refused the edit, or `out` is too small for a list response.
pub fn users_admin(req: &[u8], out: &mut [u8]) -> Result<usize, i64> {
    let req_ptr = req.as_ptr() as usize as u64;
    let req_len = req.len() as u64;
    let out_ptr = out.as_mut_ptr() as usize as u64;
    let out_cap = out.len() as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `(ptr, len)` pairs against the caller's address space before
    // touching them. `req` is a live shared borrow and `out` a live
    // exclusive borrow for the duration of the call, so the pairs denote
    // readable and writable memory respectively.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret =
        unsafe { raw_syscall(NUM_USERS_ADMIN, [req_ptr, req_len, out_ptr, out_cap, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice in the caller, exactly
    // as `users_db_read` clamps.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(out.len()))
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
/// Returns `0` on success or `-errno` (recover the [`tairix_abi::Errno`]
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

/// Bind an asynchronous IPC message port owned by the calling task
/// (`SyscallNumber::PORT_BIND`) — the receive half of
/// [`ipc_send`]/[`ipc_recv`]: an app binds its window-event mailbox here,
/// then parks on it through a wait-set member of kind
/// [`tairix_abi::WaitSourceKind::Port`]. A sender whose message the bounded
/// mailbox refused parks on the other side of the same port, through
/// [`tairix_abi::WaitSourceKind::PortRoom`].
///
/// The kernel bounds `max_payload` and `capacity` (fail-closed memory
/// bounds), requires `CAP_IPC_BIND_PRIVILEGED` for a reserved well-known
/// id (squat protection), refuses an id that is already bound
/// (`AlreadyExists`), records the kernel-trusted caller as the port's
/// owner — the only task that may receive from it — and tears the port
/// down when that owner exits. The wrapper adds no authority.
///
/// Returns `0` on success or `-errno` (recover the [`tairix_abi::Errno`]
/// discriminant as `-ret`), the standard `abi-v1` signed-result
/// convention; the wrapper hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn port_bind(endpoint: u64, max_payload: usize, capacity: usize) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; every argument is a
    // plain scalar the kernel validates — no user memory is named.
    let ret = unsafe {
        raw_syscall(
            NUM_PORT_BIND,
            [endpoint, max_payload as u64, capacity as u64, 0, 0, 0],
        )
    };
    ret as i64
}

/// Receive the oldest delivered message from a port this task bound
/// (`SyscallNumber::IPC_RECV`), copying the payload into `buf` and the
/// sender's kernel-attested [`tairix_abi::Origin`] wire image —
/// snapshotted at send time, never the sender's claim — into
/// `sender_out`, so the caller authenticates each message's principal
/// fail-closed (decode with [`tairix_abi::Origin::from_bytes`]).
///
/// Only the port's owner may receive; the kernel owner-gates every drain.
/// Returns the payload length on success. An empty mailbox is the
/// retryable `WouldBlock` — the caller parks on its wait-set, never a
/// poll loop — and a buffer smaller than the queued message is refused
/// with `BufferTooSmall`, leaving the message queued for a retry.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`) on failure; the
/// wrapper hides no error.
pub fn ipc_recv(
    endpoint: u64,
    buf: &mut [u8],
    sender_out: &mut [u8; tairix_abi::ORIGIN_WIRE_LEN],
) -> Result<usize, i64> {
    let ptr = buf.as_mut_ptr() as usize as u64;
    let sender_ptr = sender_out.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `(ptr, len)` pairs against the caller's address space before
    // writing. `buf` and `sender_out` are live exclusive borrows for the
    // duration of the call, so both pairs denote writable memory.
    let ret = unsafe {
        raw_syscall(
            NUM_IPC_RECV,
            [endpoint, ptr, buf.len() as u64, sender_ptr, 0, 0],
        )
    };
    #[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 signed-result encoding.
    let signed = ret as i64;
    if signed < 0 {
        return Err(signed);
    }
    // A count the address width cannot hold is refused, never truncated
    // into a shorter, decodable-looking record.
    usize::try_from(signed).map_err(|_| -i64::from(tairix_abi::Errno::LengthOutOfRange as i32))
}

/// Resolve a published port name to its live IPC endpoint id
/// (`SyscallNumber::PORT_RESOLVE`), returning the endpoint to pass to
/// [`ipc_send`], or the raw negative kernel result (`-errno`) on failure:
/// a malformed name, or no port published under it (`NotFound`).
///
/// Resolution grants nothing — every send to the returned endpoint is
/// still capability-checked kernel-side; the wrapper adds no authority.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 signed-result encoding (value, else -errno).
pub fn port_resolve(name: &[u8]) -> i64 {
    let ptr = name.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(ptr, len)` pair against the caller's address space and the
    // port-name grammar before consulting the registry. `name` is a live
    // shared `&[u8]` for the duration of the call, so the pair denotes
    // readable memory.
    let ret = unsafe { raw_syscall(NUM_PORT_RESOLVE, [ptr, name.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Read the discovered hardware tree the kernel built at boot into `buf`
/// (`SyscallNumber::HW_TREE_READ`),
/// returning the number of bytes copied.
///
/// The copied bytes are a [`tairix_abi::HwTreeHeader`] (the store's
/// current generation and the node count) followed by that many
/// [`tairix_abi::HwNode`] records, which the caller decodes with the
/// fail-closed `from_bytes` parsers. The generation in the header is the
/// value to pass to [`hw_tree_wait`] to block until the tree next changes.
/// Gated kernel-side on [`tairix_abi::CapabilityId::SYSINFO_HW`] — the
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
/// [`tairix_abi::CapabilityId::SYSINFO_HW`], the same privilege as reading
/// the tree; the wrapper adds no authority.
///
/// Returns `0` once the tree has changed, or `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`): `-TimedOut` if the
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

/// Fill `buf` with cryptographically secure random bytes from the kernel random
/// subsystem (`SyscallNumber::RANDOM_GET`), returning the number of bytes
/// written.
///
/// The bytes are CSPRNG output, never raw entropy, and are drawn behind
/// the single kernel random subsystem — no component rolls its own. With
/// [`RandomFlags::empty`] the draw blocks through a required reseed once
/// the generator is initialised; with [`RandomFlags::NON_BLOCKING`] it
/// returns `-EntropyNotReady` rather than waiting. The wrapper adds no
/// authority: unprivileged callers may draw random bytes.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`): `-EntropyNotReady`
/// when a non-blocking draw cannot be served yet, or a hard failure if the
/// entropy source is genuinely unavailable. The wrapper hides no error.
pub fn random_get(buf: &mut [u8], flags: RandomFlags) -> Result<usize, i64> {
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // the `(buf, len)` pair against the caller's address space before
    // writing to it. `buf` is a live exclusive `&mut [u8]` for the
    // duration of the call, so the pair denotes writable memory the kernel
    // may fill.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret =
        unsafe { raw_syscall(NUM_RANDOM_GET, [ptr, len, u64::from(flags.bits()), 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer so a buggy
    // count can never drive an out-of-bounds slice in the caller.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(buf.len()))
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
/// [`tairix_abi::CapabilityId::USERS_READ`], the same privilege as reading
/// the database; the wrapper adds no authority.
///
/// Returns `0` once the database is no longer pending (the caller re-reads
/// and re-classifies it), or `-errno` (recover the [`tairix_abi::Errno`]
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
/// [`tairix_abi::driver_store::DRIVER_STORE_ENDPOINT`].
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

/// Post `request` to the call endpoint `endpoint` **without blocking**, arming
/// a per-request deadline, and return the ticket correlating its reply
/// (`SyscallNumber::CALL_POST`; `plans/FIX-IO.md` IO1 — the asynchronous half
/// of [`ipc_call`]).
///
/// `deadline_ns` is the relative timeout after which [`call_reap`] reports a
/// timeout (`u64::MAX` = no deadline). The caller multiplexes many outstanding
/// requests on a wait-set ([`waitset_ctl`] with
/// [`tairix_abi::WaitSourceKind::CallReply`]) and reaps each with
/// [`call_reap`], so one wedged device never parks the caller (the head-of-line
/// freedom a single blocking [`ipc_call`] cannot give).
///
/// # Errors
///
/// The raw negative kernel result (`-errno`): a missing send capability
/// (`PermissionDenied`), an unknown or destroyed endpoint (`NotFound`), an
/// oversize request (`MessageTooLarge`), an over-capacity endpoint
/// (`LengthOutOfRange`), or a faulting pointer (`BadAddress`).
pub fn call_post(endpoint: u64, request: &[u8], deadline_ns: u64) -> Result<u64, i64> {
    let req_ptr = request.as_ptr() as usize as u64;
    let mut ticket: u64 = 0;
    let ticket_ptr = core::ptr::addr_of_mut!(ticket) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // request `(ptr, len)` pair and the `ticket_out` pointer against the
    // caller's address space before touching them. `request` is a live shared
    // `&[u8]` and `ticket` a live local for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    let ret = unsafe {
        raw_syscall(
            NUM_CALL_POST,
            [
                endpoint,
                req_ptr,
                request.len() as u64,
                ticket_ptr,
                deadline_ns,
                0,
            ],
        )
    } as i64;
    if ret < 0 {
        return Err(ret);
    }
    Ok(ticket)
}

/// Reap the reply to a request posted with [`call_post`], **without blocking**
/// (`SyscallNumber::CALL_REAP`; `plans/FIX-IO.md` IO1). Returns the number of
/// reply bytes copied into `reply`.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`): the reply is still pending
/// (`WouldBlock`), the deadline elapsed (`TimedOut` — the ticket is retired), a
/// cancelled/torn-down/foreign ticket (`NotFound`), or a reply larger than
/// `reply` (`BufferTooSmall`). The wrapper hides no error, so the caller
/// distinguishes "retry" (`WouldBlock`) from "fail closed" (the rest).
pub fn call_reap(endpoint: u64, ticket: u64, reply: &mut [u8]) -> Result<usize, i64> {
    let reply_ptr = reply.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // reply `(ptr, len)` pair against the caller's address space before writing
    // it. `reply` is a live exclusive `&mut [u8]` for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    let ret = unsafe {
        raw_syscall(
            NUM_CALL_REAP,
            [endpoint, ticket, reply_ptr, reply.len() as u64, 0, 0],
        )
    } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // Defence in depth: clamp the kernel's count to the buffer, exactly as
    // [`ipc_call`] does.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    Ok((ret as usize).min(reply.len()))
}

/// Withdraw one outstanding request posted with [`call_post`]
/// (`SyscallNumber::CALL_CANCEL`; `plans/FIX-IO.md` IO1). Returns `0` if the
/// caller's call was cancelled.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`): a foreign, unknown, or
/// already-completed ticket (`NotFound`), or an unknown endpoint (`NotFound`).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn call_cancel(endpoint: u64, ticket: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the call reads no caller
    // memory (both arguments are scalars).
    let ret = unsafe { raw_syscall(NUM_CALL_CANCEL, [endpoint, ticket, 0, 0, 0, 0]) };
    ret as i64
}

/// Emit one pre-encoded diagnostic [`tairix_abi::LogRecordRef`] wire image to
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

/// A [`tairix_log::Sink`] that marshals each structured event to the kernel's
/// diagnostic log sink through [`log_emit`].
///
/// This is how a first-party service routes its `tairix_log` diagnostics to
/// the system log — the serial UART on a debug build — instead of writing
/// them to `stderr` (fd 2), which on a framebuffer-console board lands on the
/// screen rather than the captured serial line. The
/// emitting task must hold `CAP_LOG_EMIT`; without it the kernel refuses the
/// call and the record is dropped (the sink is best-effort and never panics).
///
/// A message or field that exceeds the `abi-v1` record bounds is clamped to
/// the largest valid prefix and excess fields past
/// [`tairix_abi::LOG_FIELDS_MAX`] are dropped, so an over-long record still
/// reaches the log rather than being discarded whole.
#[derive(Debug, Default, Copy, Clone)]
pub struct LogSink;

impl tairix_log::Sink for LogSink {
    fn write_event(&self, event: &tairix_log::Event<'_>) {
        // Marshal the borrowed fields into the `(key, value)` pairs the
        // encoder takes, clamping keys/strings to their bound and dropping any
        // field past `LOG_FIELDS_MAX` (best-effort). A `Str` value longer than
        // the per-field encoded bound is trimmed so the record still encodes
        // rather than being dropped whole; non-string values are fixed-size.
        let mut pairs: [(&str, tairix_abi::FieldValue<'_>); tairix_abi::LOG_FIELDS_MAX] =
            [("", tairix_abi::FieldValue::Null); tairix_abi::LOG_FIELDS_MAX];
        let field_count = event.fields.len().min(tairix_abi::LOG_FIELDS_MAX);
        for (slot, field) in pairs.iter_mut().zip(event.fields.iter()).take(field_count) {
            let value = match field.value {
                // Leave room for the value's tag + length prefix.
                tairix_abi::FieldValue::Str(s) => {
                    tairix_abi::FieldValue::Str(clamp_utf8(s, tairix_abi::LOG_FIELD_VALUE_MAX - 3))
                }
                other => other,
            };
            *slot = (clamp_utf8(field.key, tairix_abi::LOG_FIELD_KEY_MAX), value);
        }
        let message = clamp_utf8(event.message, tairix_abi::LOG_MESSAGE_MAX);

        let mut buf = [0u8; tairix_abi::LOG_RECORD_MAX];
        if let Ok(len) = tairix_abi::encode_log_record(
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
/// restricted-sender endpoint (non-empty `send_caps`) — or any **reserved**
/// well-known service id ([`tairix_abi::ipc::is_reserved_endpoint`], which a
/// squatter could otherwise claim ahead of the service) — requires
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
    send_caps: &tairix_caps::CapabilitySet,
    recv_caps: &tairix_caps::CapabilitySet,
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
    call_recv_with_flags(
        endpoint,
        buf,
        ticket_out,
        tairix_abi::CallRecvFlags::empty(),
    )
}

/// Receive the next request posted to a call endpoint this task owns,
/// **without blocking** (`SyscallNumber::CALL_RECV` with
/// [`tairix_abi::CallRecvFlags::NON_BLOCKING`]).
///
/// The mode a wait-set-driven event loop uses after a member endpoint
/// reported ready: the readiness peek is not a guarantee — the queued call
/// may have been cancelled because its poster exited — and a loop serving
/// several sources must never park on one of them. On success the request
/// payload is copied into `buf`, the per-call ticket is written to
/// `ticket_out`, and the number of request bytes is returned.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`): an empty queue
/// (`WouldBlock` — benign for an event loop, simply nothing to service), a
/// request larger than `buf` (`BufferTooSmall`, left queued), a missing
/// receive capability or a foreign endpoint (`PermissionDenied`), or an
/// unknown/destroyed endpoint (`NotFound`). The wrapper hides no error.
pub fn call_recv_nonblock(
    endpoint: u64,
    buf: &mut [u8],
    ticket_out: &mut u64,
) -> Result<usize, i64> {
    call_recv_with_flags(
        endpoint,
        buf,
        ticket_out,
        tairix_abi::CallRecvFlags::NON_BLOCKING,
    )
}

/// The one `CALL_RECV` invocation both receive modes share.
fn call_recv_with_flags(
    endpoint: u64,
    buf: &mut [u8],
    ticket_out: &mut u64,
    flags: tairix_abi::CallRecvFlags,
) -> Result<usize, i64> {
    let buf_ptr = buf.as_mut_ptr() as usize as u64;
    let ticket_ptr = core::ptr::from_mut::<u64>(ticket_out) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both pointers against the caller's address space before touching them. `buf` is a live exclusive `&mut [u8]` and
    // `ticket_out` a live `&mut u64` for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret = unsafe {
        raw_syscall(
            NUM_CALL_RECV,
            [
                endpoint,
                buf_ptr,
                buf.len() as u64,
                ticket_ptr,
                u64::from(flags.bits()),
                0,
            ],
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

/// Read the kernel-attested [`tairix_abi::Origin`] of the caller whose
/// in-service call this server is currently handling
/// (`SyscallNumber::CALL_PEER_ORIGIN`; P-C).
///
/// `endpoint` is a call endpoint this task owns; `ticket` is the value a prior
/// [`call_recv`] returned for a call still in service. On success the caller's
/// attested origin wire image is copied into `out` and its byte length
/// returned; decode it with [`tairix_abi::Origin::from_bytes`]. The origin is
/// filled by the kernel from the posting task's own state, so a caller cannot
/// forge it.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`): a buffer shorter than
/// [`tairix_abi::ORIGIN_WIRE_LEN`] (`BufferTooSmall`), a missing receive
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

/// Read the kernel's boot-static machine summary ([`BootFacts`])
/// (`SyscallNumber::BOOT_FACTS_GET`).
///
/// Returns the machine facts the kernel minted once at boot from
/// kernel-attested state: the CPU architecture, the boot CPU's discovered
/// model name, the number of processor cores brought under the scheduler,
/// and the installed physical memory the boot path discovered.
/// Unprivileged, like [`boot_id`] — the facts are the machine's public
/// shape, never live state or a secret.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`). The notable case is
/// `NotImplemented`: a boot path that never installed the facts (the kernel
/// fails closed rather than fabricate a machine shape). The wrapper hides no
/// error.
pub fn boot_facts() -> Result<BootFacts, i64> {
    let mut buf = [0u8; BootFacts::WIRE_LEN];
    let out_ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before writing.
    // `buf` is a live exclusive local for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret =
        unsafe { raw_syscall(NUM_BOOT_FACTS_GET, [out_ptr, buf.len() as u64, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // The kernel returns the wire length; decode it (fail closed on a
    // malformed image — never inventing a machine shape).
    BootFacts::from_bytes(&buf).map_err(|e| -i64::from(e.as_i32()))
}

/// Read the calling task's own kernel-attested [`Origin`]
/// (`SyscallNumber::SELF_ORIGIN`).
///
/// Returns the caller's own [`Origin`] — trust domain, owning uid/gid, task
/// id, process-instance [`tairix_abi::ProcId`], and the non-secret
/// effective-capability summary. This is the self-directed twin of
/// [`call_peer_origin`]: where that lets a server learn the identity of the
/// *peer* it is servicing, this lets a task learn its *own*. Every field is
/// built by the kernel from the caller's own task record, so it cannot be
/// forged. Unprivileged, like [`boot_id`] — a task may always learn its own
/// identity.
///
/// # Errors
///
/// Returns the raw negative kernel result (`-errno`): the kernel writes the
/// origin into a stack buffer here, so the only failures are a malformed
/// decode (`OutOfRange` / `BufferTooSmall`, which a correct kernel never
/// produces). The wrapper hides no error.
pub fn self_origin() -> Result<Origin, i64> {
    let mut buf = [0u8; ORIGIN_WIRE_LEN];
    let out_ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before writing.
    // `buf` is a live exclusive local for the duration of the call.
    #[allow(clippy::cast_possible_wrap)]
    // The kernel guarantees the i64 count-result encoding (count ≥ 0, else -errno).
    let ret =
        unsafe { raw_syscall(NUM_SELF_ORIGIN, [out_ptr, buf.len() as u64, 0, 0, 0, 0]) } as i64;
    if ret < 0 {
        return Err(ret);
    }
    // The kernel returns the wire length; decode it (fail closed on a
    // malformed image — never inventing an identity).
    Origin::from_bytes(&buf).map_err(|e| -i64::from(e.as_i32()))
}

/// One elevation request, posted to the console's supervisor.
///
/// Post `request` to the elevation broker serving this console and block until
/// the exchange resolves.
///
/// It reads the caller's kernel-attested console from [`self_origin`], names
/// the per-console rendezvous, and performs the synchronous IPC call.
/// Unprivileged: the gate is the re-authentication itself (performed by the
/// broker), exactly as the login prompt is reachable by anyone at the
/// keyboard.
///
/// # Security
///
/// An [`ElevateRequest`] carries a plaintext password —
/// [`Run`](ElevateRequest::Run), [`Verify`](ElevateRequest::Verify), and
/// [`Launch`](ElevateRequest::Launch) all do.
/// The encoded request therefore lives in a [`Wiped`] buffer, which erases
/// itself when the scope ends: the value returned, the `?` that returned
/// early, and an unwind all erase it alike, so no future edit can grow an
/// exit path that leaves a password on the stack. The erase is volatile, so
/// an optimiser cannot drop it as a store nobody reads.
///
/// The reply carries no secret, so it needs no such treatment.
///
/// # Errors
///
/// Returns the [`Errno`] naming the failure: no console to elevate on, an
/// encoding error, a transport failure, a protocol mismatch, or the broker's
/// own refusal (for example `PermissionDenied` on a wrong password).
pub fn elevate(request: &ElevateRequest<'_>) -> Result<ElevateReply, Errno> {
    let console = self_origin().map_err(errno_from_raw)?.console();
    let endpoint = elevate_endpoint(console)?;
    let mut buf = Wiped::<ELEVATE_MAX_REQUEST>::new();
    elevate_exchange(endpoint, request, &mut buf[..])
}

/// Encode `request` into `buf`, post it to `endpoint`, and decode the reply.
///
/// Split out from [`elevate`] so the exchange can be driven against a
/// caller-owned buffer whose contents a test can inspect once the call has
/// returned. Erasing that buffer is the caller's guard, never this function's
/// business: keeping the two apart is what lets the test prove the erase
/// happens on the failing paths too.
fn elevate_exchange(
    endpoint: u64,
    request: &ElevateRequest<'_>,
    buf: &mut [u8],
) -> Result<ElevateReply, Errno> {
    let len = request.encode(buf)?;
    let mut reply = [0u8; ELEVATE_REPLY_LEN];
    let reply_len = ipc_call(endpoint, &buf[..len], &mut reply).map_err(errno_from_raw)?;
    ElevateReply::decode(&reply[..reply_len])
}

/// Convert a raw negative kernel result (`-errno`) into an [`Errno`].
///
/// A syscall that fails returns its error as a negated discriminant, so this is
/// the one place the raw register becomes a typed error. Every consumer of a
/// raw result — this runtime's own wrappers and the driver programs that issue
/// syscalls directly — recovers its `Errno` here, so a refusal cannot be read
/// one way in one program and another way in the next.
///
/// Anything the `abi-v1` source of truth does not recognise — a code this build
/// has no variant for, a magnitude too large for an `i32`, or a non-negative
/// value handed in by mistake — fails closed as
/// [`Errno::NotImplemented`]: this build genuinely cannot say what the kernel
/// meant. It deliberately never becomes [`Errno::NotFound`], which asserts that
/// a named object does not exist and which callers act on (by creating it, or
/// by treating an absence as benign); an unreadable result must not be able to
/// masquerade as that answer.
#[must_use]
pub fn errno_from_raw(raw: i64) -> Errno {
    raw.checked_neg()
        .and_then(|code| i32::try_from(code).ok())
        .and_then(Errno::from_i32)
        .unwrap_or(Errno::NotImplemented)
}

/// Read the **unfiltered, global** kernel introspection view
/// (`SyscallNumber::SYSINFO_INTROSPECT`; P-C).
///
/// `domain` is a [`tairix_abi::IntrospectDomain`] discriminant; `arg` is the
/// domain-specific selector (a record offset for the paged domains, unused
/// otherwise); `buf` receives the encoded records and returns the byte count
/// written. For the per-task-limits domain the target task's 128-bit
/// [`tairix_abi::ProcId`] is written into `buf` on entry (a `u64` `arg` cannot
/// carry it).
///
/// Gated kernel-side on [`tairix_abi::CapabilityId::SYSINFO_INTROSPECT`],
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
/// `fd` is a standard descriptor the caller owns (typically
/// [`tairix_abi::STDOUT`]).
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
/// [`HwResourceKind::Shared`](tairix_abi::hwtree::HwResourceKind) grant so it
/// may forward the region onto a node it emits. The region id is written to
/// `id_out`. The call carries `CAP_SHM` (enforced kernel-side before any
/// state is touched).
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the base virtual address of
/// the newly mapped region, and a negative value is `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`) — `id_out` is left untouched
/// on a negative result. The wrapper surfaces that raw signed value; it adds
/// no authority and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 shm_create-result encoding (base ≥ 0, else -errno).
pub fn shm_create(len: usize, id_out: &mut u64) -> i64 {
    let id_ptr = core::ptr::from_mut::<u64>(id_out) as usize as u64;
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
/// refused. On success the region's byte length — the kernel's own record,
/// never the granting task's claim, so a server sizes its view of the shared
/// bytes from the kernel's answer — is written to `len_out`. The call carries
/// `CAP_SHM` (enforced kernel-side).
///
/// The kernel encodes the result as a signed register following the standard
/// `abi-v1` convention: a non-negative value is the base virtual address of
/// the mapping, and a negative value is `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`) — `len_out` is left untouched
/// on a negative result. The wrapper surfaces that raw signed value; it adds
/// no authority and hides no error.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 shm_map-result encoding (base ≥ 0, else -errno).
pub fn shm_map(handle: u64, len_out: &mut u64) -> i64 {
    let len_ptr = core::ptr::from_mut::<u64>(len_out) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke — the kernel validates
    // the call on the far side of the trap. `len_out` is a live exclusive
    // `&mut u64` for the duration of the call, so the pointer denotes
    // writable memory the kernel may fill with the mapped region's byte
    // length; the kernel validates it against the caller's own address space
    // before writing.
    let ret = unsafe { raw_syscall(NUM_SHM_MAP, [handle, len_ptr, 0, 0, 0, 0]) };
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

/// Grant the serving task of call endpoint `endpoint` the right to map the
/// shared-memory region `region` the caller owns
/// (`SyscallNumber::SHM_GRANT`, `plans/DISPLAY.md` D7a — the display client
/// hands its frame buffer to the display service), returning the minted
/// grant handle (≥ 1) or `-errno`.
///
/// The kernel requires `CAP_SHM`, confirms the caller itself holds a
/// `Shared` grant covering `region` (delegation never widens authority),
/// resolves the **live serving task** of `endpoint` at grant time — never a
/// caller-supplied (recyclable) PID — and mints that task its own
/// unforgeable handle for the region; the mint is audited. The caller
/// forwards the returned handle in-band (an IPC request field); it resolves
/// only when presented by the recipient task's own [`shm_map`], so the
/// number is useless to a bystander. An unknown region, a region the caller
/// cannot map, or an unknown endpoint fails closed with `-errno`
/// (`NotFound`).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 handle-or-errno encoding (handle ≥ 1, else -errno).
pub fn shm_grant(region: u64, endpoint: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the call carries no
    // pointers, and the kernel validates `CAP_SHM`, the caller's own region
    // grant, and the endpoint before minting anything.
    let ret = unsafe { raw_syscall(NUM_SHM_GRANT, [region, endpoint, 0, 0, 0, 0]) };
    ret as i64
}

/// Grant the serving task of call endpoint `recipient` the right to *call*
/// call endpoint `endpoint`, which the caller already holds
/// (`SyscallNumber::CALL_GRANT`, `plans/FIX-IO.md` `IO6b` — the endpoint
/// sibling of [`shm_grant`], so a composing service can drive the several
/// member devices an array is made of), returning the minted grant handle
/// (≥ 1) or `-errno`.
///
/// The kernel requires `CAP_IPC_ENDPOINT`, confirms the caller itself holds
/// an `Endpoint` grant covering `endpoint` **before** reading any endpoint
/// state (delegation never widens authority), resolves the **live serving
/// task** of `recipient` at grant time — never a caller-supplied
/// (recyclable) PID — and mints that task its own unforgeable handle; the
/// mint is audited. The caller forwards the returned handle in-band (an IPC
/// request field). A grant the caller does not hold and an unknown
/// recipient endpoint both fail closed with `-errno` (`NotFound`), so the
/// reply confirms nothing about foreign endpoints.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 handle-or-errno encoding (handle ≥ 1, else -errno).
pub fn call_grant(endpoint: u64, recipient: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the call carries no
    // pointers, and the kernel validates `CAP_IPC_ENDPOINT`, the caller's own
    // endpoint grant, and the recipient endpoint before minting anything.
    let ret = unsafe { raw_syscall(NUM_CALL_GRANT, [endpoint, recipient, 0, 0, 0, 0]) };
    ret as i64
}

/// Delegate the caller's own filesystem descriptor `fd` to the live task
/// `pid` as a one-shot grant bounded above by `write_ceiling` bytes,
/// returning the minted handle (≥ 1) or `-errno`
/// (`SyscallNumber::FD_GRANT`, `plans/CAPABILITY_USE.md` CU6 — the file
/// picker's user-mediated hand-off; `plans/APPDATA.md` §3.8 — the app-data
/// service's blob descriptor).
///
/// Requires `CAP_FS_ACCESS`; the mint is audited. `pid` must come from a
/// kernel-attested source (`call_peer_origin`) — task ids are never
/// reused, so the grant lands on the intended process or fails closed
/// (`NotFound`). The kernel captures the *caller's* identity and effective
/// capability set with the descriptor's path, so every operation on the
/// redeemed descriptor is re-authorised under the grantor's authority.
///
/// The delegation carries the descriptor's **own** read/write access and no
/// more, so it never widens what the grantor opened. `write_ceiling` is the
/// highest file length the recipient may write or truncate to; it must be
/// zero for a read-only descriptor and non-zero for a writable one, so an
/// unbounded writable delegation cannot be minted at all. A descriptor that
/// names a directory, or that is not a plain file backing, fails closed
/// with `-errno` (`OutOfRange`).
///
/// The caller forwards the returned handle in-band (e.g. a window-channel
/// event field, or an app-data reply); it resolves only when presented by
/// the recipient's own [`fd_redeem`], so the number is useless to a
/// bystander. [`File::from_delegation`] is the owned redemption.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 handle-or-errno encoding (handle ≥ 1, else -errno).
pub fn fd_grant(fd: u32, pid: u64, write_ceiling: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the call carries no
    // pointers, and the kernel validates `CAP_FS_ACCESS`, the caller's own
    // descriptor, the ceiling against that descriptor's access, and the
    // recipient's liveness before minting anything.
    let ret = unsafe { raw_syscall(NUM_FD_GRANT, [u64::from(fd), pid, write_ceiling, 0, 0, 0]) };
    ret as i64
}

/// Redeem an [`fd_grant`] handle minted to the calling task, installing
/// the delegated file into the caller's own open table and returning the
/// fresh descriptor number (≥ 0) or `-errno`
/// (`SyscallNumber::FD_REDEEM`).
///
/// Unprivileged: receiving user-mediated, already-checked authority is the
/// point of the delegation — every later read is still VFS-checked under
/// the grantor's captured identity. One-shot: the grant is consumed only
/// when the descriptor allocation succeeds, so a refused redemption leaves
/// it intact and a redeemed handle can never be redeemed twice. A handle
/// minted to another task fails closed with `-errno` (`NotFound`),
/// indistinguishable from one that never existed.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 fd-or-errno encoding (fd ≥ 0, else -errno).
pub fn fd_redeem(handle: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the call carries no
    // pointers, and the kernel resolves the handle owner-bound before
    // installing anything.
    let ret = unsafe { raw_syscall(NUM_FD_REDEEM, [handle, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// Ask whether the in-flight caller of served call endpoint `endpoint`
/// (ticket `ticket`) holds seat `seat`'s live lease
/// (`SyscallNumber::CALL_PEER_SEAT`, `plans/DISPLAY.md` D7a — the display
/// service's per-present check), returning the live lease generation
/// (≥ 1) or `-errno`.
///
/// Valid only between [`call_recv`] and [`call_reply`] on an endpoint the
/// caller owns and may receive from (the [`call_peer_origin`] window): a
/// server learns seat facts only about a task it is actively servicing, so
/// seat ownership is never enumerable through this path. The kernel reads
/// the seat's **live** lease at check time — a revocation between two
/// frames refuses the very next present. Refusals are typed and
/// fail-closed: `SeatNotOwner` (unowned or another task holds it),
/// `SeatRevoked` (the peer's unacknowledged eviction), `NotFound` (no such
/// seat, endpoint, or in-service ticket), `PermissionDenied` (the caller
/// does not own the endpoint or lacks its receive capability).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 generation-or-errno encoding (generation ≥ 1, else -errno).
pub fn call_peer_seat(endpoint: u64, ticket: u64, seat: u64) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke — the call carries no
    // pointers, and the kernel gates it on endpoint ownership + receive
    // capability before reading any seat state.
    let ret = unsafe { raw_syscall(NUM_CALL_PEER_SEAT, [endpoint, ticket, seat, 0, 0, 0]) };
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
/// endpoint the caller serves ([`WaitSourceKind::Endpoint`]), an
/// [`IrqHandle`](tairix_abi::IrqHandle) the caller bound
/// ([`WaitSourceKind::Irq`]), a child of the caller awaiting reap — a
/// PID or [`tairix_abi::waitset::WAITSET_CHILD_ANY`]
/// ([`WaitSourceKind::Child`]), a seat whose live lease the caller holds
/// via `display_acquire` ([`WaitSourceKind::SeatInput`], ready on queued
/// keyboard/pointer input *and* on losing the lease, so a revocation is
/// observed rather than parked through), a message port the caller bound
/// via [`port_bind`] ([`WaitSourceKind::Port`], ready on a delivered
/// message awaiting [`ipc_recv`]), room in a port the caller may *send* to
/// ([`WaitSourceKind::PortRoom`], ready when an [`ipc_send`] would not be
/// refused for want of room, so a sender holding a message the receiver
/// must not lose parks instead of dropping it), or a pipe read end of the
/// caller's own open table ([`WaitSourceKind::Stream`] — the descriptor
/// from [`pipe_create`], ready on buffered bytes or end-of-stream, drained
/// by [`fs_read`]); `token` is the caller's opaque
/// value reported by [`waitset_wait`] when this member is ready. On `Add`
/// the kernel resolves and **owner-checks** the named resource against the
/// calling task before recording it — for [`WaitSourceKind::PortRoom`] the
/// *send*-authority check `ipc_send` applies, its caller being the sender
/// rather than the binder — so the set can never observe authority the
/// caller lacks.
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
    let token_ptr = core::ptr::from_mut::<u64>(token_out) as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; `token_out` is a live
    // exclusive `&mut u64` for the duration of the call, so the pointer denotes
    // writable memory the kernel may fill with the ready member's token; the
    // kernel validates it against the caller's own address space before writing.
    let ret = unsafe { raw_syscall(NUM_WAITSET_WAIT, [set, timeout_ns, token_ptr, 0, 0, 0]) };
    ret as i64
}

/// Park the calling task off the run queue for the life of the process.
///
/// The park a **resident** program with no further work runs — a bus driver
/// that must stay alive so its grants keep the hardware it brought up
/// claimed, but which serves no requests and waits on no device event. It
/// parks on an empty wait-set with no timeout: the kernel holds the task
/// off the run queue and a spurious wake merely re-parks, so the task
/// consumes no CPU for the life of the system. A `loop { yield_now() }`
/// stand-in is the cooperative busy-poll the charter forbids: a
/// perpetually-runnable task pegs a core and pollutes the run-queue load.
///
/// Returns only on failure, with the raw negative kernel result (`-errno`)
/// from the wait-set call that failed — the caller exits fail-loud with its
/// reason rather than falling back to a spin.
#[must_use = "park_forever returning means the park failed; the caller must exit fail-loud"]
pub fn park_forever() -> i64 {
    let set = waitset_create();
    if set < 0 {
        return set;
    }
    #[allow(clippy::cast_sign_loss)] // `set >= 0` checked above; it is a kernel-minted handle.
    let set = set as u64;
    let mut token = 0u64;
    loop {
        // An empty membership with no timeout parks indefinitely; the kernel
        // absorbs spurious wakes internally. A `0` return (a ready member on
        // an empty set) is impossible by construction, so any return at all
        // is surfaced to the caller as the park having failed.
        let ret = waitset_wait(set, u64::MAX, &mut token);
        if ret != 0 {
            return ret;
        }
    }
}

/// Recover a usable byte count from a raw `abi-v1` count-result register,
/// clamping to `cap` as defence in depth.
///
/// The kernel encodes a filesystem count result as the standard signed
/// register (count ≥ 0, else `-errno`). A negative value is surfaced as the
/// raw `Err(-errno)`; a non-negative value is clamped to `cap` so a buggy or
/// hostile kernel count can never drive an out-of-bounds slice in the caller
/// (the same posture the [`io`] stream primitives and [`users_db_read`] take).
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
/// itself is gated on [`tairix_abi::CapabilityId::FS_ACCESS`]. A refused open
/// never produces a descriptor. This is the descriptor-producing primitive
/// the higher-level [`File`] / [`Dir`] wrappers build on; a program names a
/// descriptor, never a device.
///
/// Returns the descriptor (≥ 0) or `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`), the standard `abi-v1`
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

/// Resolve the resource reference `reference` (e.g. `b"sys:random"`) and open
/// it to a new descriptor with `flags` (`SyscallNumber::RESOURCE_OPEN`),
/// returning the new descriptor number.
///
/// A resource reference names a typed non-filesystem resource
/// (`plans/ALIAS.md`) — there is no `/dev`, `/proc`, or `/sys` — so this is
/// the resource analogue of [`fs_open`]: the kernel parses the reference with
/// the shared reference parser and resolves it through its capability-checked
/// namespace resolver under the caller's kernel-attested identity
/// (authorisation is per namespace, so an unprivileged resource such as
/// `sys:random` needs no capability). The descriptor it returns is read and
/// written with [`fs_read`] / [`fs_write`] and released with [`fs_close`],
/// exactly as a file handle is, but its backing is the resolved resource
/// rather than a path — so the higher-level [`File`] wrapper drives it too. A
/// malformed, unknown, or unauthorised reference never produces a descriptor.
///
/// Returns the descriptor (≥ 0) or `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 fd-result encoding (fd ≥ 0, else -errno).
pub fn resource_open(reference: &[u8], flags: OpenFlags) -> i64 {
    let ptr = reference.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before reading it.
    // `reference` is a live shared `&[u8]` for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_RESOURCE_OPEN,
            [
                ptr,
                reference.len() as u64,
                u64::from(flags.bits()),
                0,
                0,
                0,
            ],
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
/// A single syscall transfers at most [`tairix_abi::FS_IO_MAX`] bytes; a
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
/// transfers at most [`tairix_abi::FS_IO_MAX`] bytes; a larger `data` is split
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
/// packed [`tairix_abi::DirEntry`] stream occupies.
///
/// The whole listing is delivered or none: a buffer smaller than the packed
/// stream is refused with `BufferTooSmall` rather than truncated, so the
/// caller grows `buf` and retries (the entry count is a discovered capacity,
/// not a fixed ceiling). Walk the returned prefix with
/// [`tairix_abi::DirEntry::decode`] — or use [`Dir::read`], which owns the
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
/// With [`UnlinkFlags::DIRECTORY`](tairix_abi::UnlinkFlags::DIRECTORY) the
/// removal succeeds only when the name is an (empty) directory — the atomic
/// `rmdir` posture, decided by the filesystem under its own lock; a
/// non-directory is refused with the dedicated `Errno::NotADirectory`.
/// [`UnlinkFlags::empty`](tairix_abi::UnlinkFlags::empty) removes the named
/// file or (empty) directory.
///
/// The kernel authorises the removal through the secured VFS under the
/// caller's attested identity (a missing path, a non-empty directory, a
/// read-only mount, or a permission denial fails closed). Returns `0` on
/// success or `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_unlink(path: &[u8], flags: tairix_abi::UnlinkFlags) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before reading it.
    // `path` is a live shared `&[u8]` for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_UNLINK,
            [ptr, path.len() as u64, u64::from(flags.bits()), 0, 0, 0],
        )
    };
    ret as i64
}

/// Move the file or directory at absolute `src` to absolute `dst`
/// (`SyscallNumber::FS_RENAME`).
///
/// Both paths must resolve under the same mounted volume. The kernel
/// authorises the move through the secured VFS under the caller's attested
/// identity (a missing source, a non-empty directory destination, a
/// directory-into-its-own-subtree move, a read-only mount, or a permission
/// denial fails closed; a cross-mount move is refused with the dedicated
/// `Errno::CrossVolume`, the `EXDEV` equivalent a mover falls back to
/// copy-then-remove on). Returns `0` on success or `-errno`.
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

/// Create a symbolic link at the absolute `link` whose stored target is
/// `target` (`SyscallNumber::FS_SYMLINK`, the `symlink(2)` shape).
///
/// `target` is the link's **body**, not a path the kernel walks: it is stored
/// verbatim, may be relative, may carry `.`/`..`, and is never resolved at
/// creation — so the call authorises only the right to create a name in the
/// link's own parent, and the resulting link may legitimately dangle. It
/// carries at most [`tairix_abi::FS_SYMLINK_MAX`] bytes and must be valid
/// UTF-8 that satisfies the kernel's link-target grammar; anything else is
/// refused rather than stored.
///
/// Creating a link grants no authority over what it names: every later *use*
/// re-authorises each component under the caller's attested identity. A
/// format with no link object type refuses with `Errno::NotSupported` rather
/// than approximating one. Returns `0` on success or `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_symlink(target: &[u8], link: &[u8]) -> i64 {
    let target_ptr = target.as_ptr() as usize as u64;
    let link_ptr = link.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `(ptr, len)` pairs against the caller's address space before
    // reading them. `target`/`link` are live shared slices for the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_SYMLINK,
            [
                target_ptr,
                target.len() as u64,
                link_ptr,
                link.len() as u64,
                0,
                0,
            ],
        )
    };
    ret as i64
}

/// Add the absolute `link` as a second name for the node the absolute
/// `existing` already names (`SyscallNumber::FS_LINK`, the `link(2)` shape).
///
/// A hard link, not a symbolic one: both names reach the same node, so a
/// write through either is visible through the other and the node's storage
/// survives until the last name is unlinked. With an empty `flags` **neither
/// final component is followed** — the node that gains a name is the one
/// spelled, never one a symbolic link redirects to (POSIX `link()`).
/// [`LinkFlags::FOLLOW`](tairix_abi::LinkFlags::FOLLOW) is the
/// `linkat(AT_SYMLINK_FOLLOW)` posture, resolving the existing name's final
/// link; the new name is never followed under either.
///
/// Both paths must lie on one mounted volume (`Errno::CrossVolume`
/// otherwise), a directory is refused (`Errno::IsADirectory`), a node whose
/// format-recorded name count would overflow is refused
/// (`Errno::TooManyLinks`), and a format holding one name per node refuses
/// with `Errno::NotSupported`. The new name confers no authority the caller
/// did not already hold over the node. Returns `0` on success or `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_link(existing: &[u8], link: &[u8], flags: tairix_abi::LinkFlags) -> i64 {
    let existing_ptr = existing.as_ptr() as usize as u64;
    let link_ptr = link.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `(ptr, len)` pairs against the caller's address space before
    // reading them. `existing`/`link` are live shared slices for the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_LINK,
            [
                existing_ptr,
                existing.len() as u64,
                link_ptr,
                link.len() as u64,
                u64::from(flags.bits()),
                0,
            ],
        )
    };
    ret as i64
}

/// Read the stored target of the symbolic link at the absolute `path` into
/// `out` (`SyscallNumber::FS_READLINK`, the `readlink(2)` shape).
///
/// The final component is never followed — the call is about the link
/// itself — and the target comes back exactly as it was stored, still
/// unresolved. Returns the target's byte length on success or `-errno`:
/// `Errno::OutOfRange` when `path` names anything but a symbolic link,
/// `Errno::NotSupported` on a mount whose format stores no links, and
/// `Errno::BufferTooSmall` when `out` cannot hold the whole target — never a
/// truncated one, which would name somewhere else entirely, so the caller
/// retries with a larger buffer.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (length, else -errno).
pub fn fs_readlink(path: &[u8], out: &mut [u8]) -> i64 {
    let path_ptr = path.as_ptr() as usize as u64;
    let out_ptr = out.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `(ptr, len)` pairs against the caller's address space before
    // touching them, and writes at most `out.len()` bytes. Both slices are
    // live for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_READLINK,
            [path_ptr, path.len() as u64, out_ptr, out.len() as u64, 0, 0],
        )
    };
    ret as i64
}

/// Canonicalise the absolute `path` into `out` (`SyscallNumber::FS_REALPATH`,
/// the `realpath(3)` shape).
///
/// The kernel resolves every component itself — each symbolic link followed,
/// each `..` applied to the nodes the walk really traversed, search
/// permission required on every directory passed through — so the answer
/// holds no `.`, no `..`, and no link, and is a path the same kernel would
/// accept back. A tool must **not** canonicalise for itself: a userland walk
/// that disagreed by one rule would print a path the kernel resolves
/// elsewhere.
///
/// `mode` chooses how much of the path must exist: the three readings are
/// GNU's `-e`, `-f`, and `-m`. Returns the canonical path's byte length on
/// success or `-errno`: `Errno::NotFound` when `mode` requires a component
/// that is absent, `Errno::LinkLoop` for a cycle or an over-budget chain,
/// and `Errno::BufferTooSmall` when `out` cannot hold the whole path —
/// never a prefix, which would name a different node, so the caller retries
/// with a larger buffer.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (length, else -errno).
pub fn fs_realpath(path: &[u8], out: &mut [u8], mode: tairix_abi::RealpathMode) -> i64 {
    let path_ptr = path.as_ptr() as usize as u64;
    let out_ptr = out.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // both `(ptr, len)` pairs against the caller's address space before
    // touching them, and writes at most `out.len()` bytes. Both slices are
    // live for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_REALPATH,
            [
                path_ptr,
                path.len() as u64,
                out_ptr,
                out.len() as u64,
                u64::from(mode.as_u32()),
                0,
            ],
        )
    };
    ret as i64
}

/// Set the permission bits of the file or directory at the absolute `path`
/// to `mode` (`SyscallNumber::FS_SET_MODE`, the `chmod(2)` shape).
///
/// `mode` carries at most [`tairix_abi::FS_MODE_MASK`] (the
/// owner/group/other `rwx` triads plus the setuid/setgid/sticky bits); any
/// higher bit is refused at dispatch with `Errno::OutOfRange` — never
/// silently masked. The kernel authorises the change through the secured
/// VFS under the caller's attested identity: only the inode's **owner** may
/// change its mode, the covering mount must be writable, and ownership,
/// ACL, and capability gate are untouched. Returns `0` on success or
/// `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_set_mode(path: &[u8], mode: u32) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before reading it.
    // `path` is a live shared `&[u8]` for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_SET_MODE,
            [ptr, path.len() as u64, u64::from(mode), 0, 0, 0],
        )
    };
    ret as i64
}

/// Set the owning user and/or group of the file or directory at the
/// absolute `path` to `uid` / `gid` (`SyscallNumber::FS_SET_OWNER`, the
/// `chown(2)` / `chgrp(2)` shape).
///
/// Pass [`tairix_abi::FS_OWNER_UNCHANGED`] for either field to leave it
/// unchanged (so an owner-only or group-only change touches only the field
/// it names); a call leaving both is a no-op. The kernel authorises the
/// change through the secured VFS under the caller's attested identity:
/// reassigning the **uid**, or setting a **gid** the caller is not a member
/// of, requires `CAP_FS_CHOWN`; otherwise only the node's owner may change
/// the group, and only to a group they belong to. Any successful change
/// clears the setuid bit (and the setgid bit of a group-executable node),
/// and the covering mount must be writable. Returns `0` on success or
/// `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_set_owner(path: &[u8], uid: u32, gid: u32) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before reading it.
    // `path` is a live shared `&[u8]` for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_SET_OWNER,
            [ptr, path.len() as u64, u64::from(uid), u64::from(gid), 0, 0],
        )
    };
    ret as i64
}

/// Read the extended attribute `key` of the file or directory at the
/// absolute `path` into `value_out` (`SyscallNumber::FS_ATTR_GET`, the
/// `getxattr(2)` shape).
///
/// `key` is a `lib/fsmeta`-grammar `namespace.rest` key of
/// `1..=`[`tairix_abi::FS_ATTR_KEY_MAX`] bytes. Returns the value's byte
/// count (a value may be empty), or `-errno`: `Errno::NoData` when the node
/// carries no such attribute, `Errno::BufferTooSmall` when the value does
/// not fit `value_out` (never truncated), `Errno::NotSupported` on a mount
/// whose format stores no attributes, and the usual path/permission
/// refusals. The kernel authorises through the secured VFS: read permission
/// on the node, privileged namespaces refused.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding.
pub fn fs_attr_get(path: &[u8], key: &[u8], value_out: &mut [u8]) -> i64 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // every `(ptr, len)` pair against the caller's address space before
    // touching it. All three buffers are live for the duration of the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_ATTR_GET,
            [
                path.as_ptr() as usize as u64,
                path.len() as u64,
                key.as_ptr() as usize as u64,
                key.len() as u64,
                value_out.as_mut_ptr() as usize as u64,
                value_out.len() as u64,
            ],
        )
    };
    ret as i64
}

/// Set (insert or replace) the extended attribute `key` of the file or
/// directory at the absolute `path` to `value`
/// (`SyscallNumber::FS_ATTR_SET`, the `setxattr(2)` shape).
///
/// `value` carries at most [`tairix_abi::FS_ATTR_VALUE_MAX`] opaque bytes;
/// a larger payload is refused at dispatch with `Errno::LengthOutOfRange`.
/// The write is one copy-on-write transaction. Returns `0` on success or
/// `-errno` (`Errno::NoSpace` at the per-inode bounds,
/// `Errno::NotSupported` on a mount without attribute storage, and the
/// usual path/permission refusals — write permission on the node, a
/// writable mount, privileged namespaces refused).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding.
pub fn fs_attr_set(path: &[u8], key: &[u8], value: &[u8]) -> i64 {
    // SAFETY: as `fs_attr_get`; all three buffers are live shared slices.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_ATTR_SET,
            [
                path.as_ptr() as usize as u64,
                path.len() as u64,
                key.as_ptr() as usize as u64,
                key.len() as u64,
                value.as_ptr() as usize as u64,
                value.len() as u64,
            ],
        )
    };
    ret as i64
}

/// Yield the `index`-th visible extended-attribute key of the file or
/// directory at the absolute `path` into `key_out`
/// (`SyscallNumber::FS_ATTR_LIST`).
///
/// Returns the key's byte count, `0` once `index` is past the last visible
/// attribute (a real key is never empty), or `-errno`. Keys whose namespace
/// the caller may not read are omitted, never revealed.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding.
pub fn fs_attr_list(path: &[u8], index: u64, key_out: &mut [u8]) -> i64 {
    // SAFETY: as `fs_attr_get`; both buffers are live for the call.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_ATTR_LIST,
            [
                path.as_ptr() as usize as u64,
                path.len() as u64,
                index,
                key_out.as_mut_ptr() as usize as u64,
                key_out.len() as u64,
                0,
            ],
        )
    };
    ret as i64
}

/// Remove the extended attribute `key` from the file or directory at the
/// absolute `path` (`SyscallNumber::FS_ATTR_REMOVE`, the `removexattr(2)`
/// shape).
///
/// Returns `0` on success or `-errno` (`Errno::NoData` when no such
/// attribute is stored, `Errno::NotSupported` on a mount without attribute
/// storage, and the usual path/permission refusals).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding.
pub fn fs_attr_remove(path: &[u8], key: &[u8]) -> i64 {
    // SAFETY: as `fs_attr_get`; both buffers are live shared slices.
    let ret = unsafe {
        raw_syscall(
            NUM_FS_ATTR_REMOVE,
            [
                path.as_ptr() as usize as u64,
                path.len() as u64,
                key.as_ptr() as usize as u64,
                key.len() as u64,
                0,
                0,
            ],
        )
    };
    ret as i64
}

/// Change the calling process's working directory to `path`
/// (`SyscallNumber::FS_CHDIR`).
///
/// `path` may be absolute or relative to the current working directory. The
/// kernel resolves and normalises it with the shared path parser, then
/// re-authorises it as a searchable directory through the secured VFS under
/// the caller's attested identity; only on success does it become the new
/// working directory (against which later relative [`fs_open`] paths
/// resolve). A path that is not a searchable directory fails closed and
/// leaves the working directory unchanged. Gated on
/// [`tairix_abi::CapabilityId::FS_ACCESS`]. Returns `0` on success or
/// `-errno`.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 errno-result encoding (0, else -errno).
pub fn fs_chdir(path: &[u8]) -> i64 {
    let ptr = path.as_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(ptr, len)` pair against the caller's address space before reading it.
    // `path` is a live shared `&[u8]` for the duration of the call.
    let ret = unsafe { raw_syscall(NUM_FS_CHDIR, [ptr, path.len() as u64, 0, 0, 0, 0]) };
    ret as i64
}

/// Read the calling process's working directory — a normalised absolute
/// path — into `buf` (`SyscallNumber::FS_GETCWD`), returning the number of
/// bytes written.
///
/// The whole path is delivered or none: a `buf` too small to hold it fails
/// closed with `BufferTooSmall` (the path is never truncated); the caller
/// grows `buf` and retries. Needs no capability.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`): the buffer is too small
/// (`BufferTooSmall`), the buffer pointer faults, or no filesystem is
/// mounted (`NotImplemented`).
pub fn fs_getcwd(buf: &mut [u8]) -> Result<usize, i64> {
    let ptr = buf.as_mut_ptr() as usize as u64;
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates the
    // `(buf, len)` pair against the caller's address space before writing it.
    // `buf` is a live exclusive `&mut [u8]` for the duration of the call.
    let ret = unsafe { raw_syscall(NUM_FS_GETCWD, [ptr, buf.len() as u64, 0, 0, 0, 0]) };
    count_result(ret, buf.len())
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
    /// Open the named `target` with `flags`, returning the owned handle.
    ///
    /// The one shared spelling rule
    /// ([`tairix_resref::names_resource_reference`]) decides which world the
    /// name belongs to *before* any lookup: a filesystem path (absolute,
    /// dot-relative, an `Alias:/path` alias form, or any unregistered
    /// prefix) opens through [`fs_open`], while a resource reference
    /// (`sys:random`, `sys:null`, …) opens through [`resource_open`], the
    /// kernel's capability-checked resource resolver. Because the routing
    /// lives here — in the one open-by-name path every first-party program
    /// links — a resource reference works wherever a program accepts a file
    /// name; no tool carries a private copy of the rule.
    ///
    /// A spelling that names a reference — well-formed or not — is *never*
    /// retried as a filesystem lookup: the kernel resolver refuses a
    /// malformed or unauthorised reference and the refusal stands (fail
    /// closed), so a typo like `sys:null@` cannot silently read a file. A
    /// real on-disk file whose name contains `:` stays reachable as
    /// `./name`; a name that is not UTF-8 can never spell a reference and is
    /// always a path.
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) the [`fs_open`] or
    /// [`resource_open`] syscall returns on any refusal.
    pub fn open(target: &[u8], flags: OpenFlags) -> Result<Self, i64> {
        if core::str::from_utf8(target).is_ok_and(tairix_resref::names_resource_reference) {
            return Self::open_resource(target, flags);
        }
        Self::from_open_result(fs_open(target, flags))
    }

    /// Resolve the resource reference `reference` (e.g. `b"sys:random"`) and
    /// open it with `flags`, returning the owned handle.
    ///
    /// [`File::open`] routes here by spelling; this constructor is for a
    /// caller that has already classified its target (a shell holding a
    /// parsed redirection target) and must not re-run the rule.
    ///
    /// The returned handle reads and writes through the same [`File::read_at`]
    /// / [`File::write_at`] path a file handle uses; a resource is a
    /// sequential stream, so the byte offset those pass is ignored by the
    /// backing (each read of `sys:random` yields fresh bytes; `sys:null` reads
    /// as end of stream and accepts any write).
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) the [`resource_open`] syscall
    /// returns on any refusal (a malformed, unknown, or unauthorised
    /// reference, or one requesting access the resource does not offer).
    pub fn open_resource(reference: &[u8], flags: OpenFlags) -> Result<Self, i64> {
        Self::from_open_result(resource_open(reference, flags))
    }

    /// Redeem the one-shot delegation `handle` minted to this task and take
    /// ownership of the descriptor it installs ([`fd_redeem`]).
    ///
    /// The one owned redemption: a caller that redeemed by hand would have to
    /// remember its own [`fs_close`] on every path out, and the descriptor a
    /// delegation installs is exactly as leakable as one [`File::open`]
    /// returns. What the handle conveys was fixed when it was minted — the
    /// grantor's captured identity, the access it had opened, and, for a
    /// writable delegation, its extent ceiling — so this adds no authority and
    /// takes no flags.
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) of the redemption:
    /// `NotFound` for a handle that was never minted to this task, was
    /// already redeemed, or was minted to another (forgery is
    /// indistinguishable from absence), and `OutOfRange` on descriptor-space
    /// exhaustion, which leaves the grant pending for a retry.
    pub fn from_delegation(handle: u64) -> Result<Self, i64> {
        Self::from_open_result(fd_redeem(handle))
    }

    /// Wrap an open-family syscall result (`fs_open` / `resource_open`) as an
    /// owned handle, passing a negative `-errno` through unchanged.
    ///
    /// A non-negative result is a descriptor number, which the kernel always
    /// reports within `u32` (the descriptor space the per-process table
    /// allocates from); the conversion is exact.
    fn from_open_result(ret: i64) -> Result<Self, i64> {
        if ret < 0 {
            return Err(ret);
        }
        let fd =
            u32::try_from(ret).map_err(|_| -i64::from(tairix_abi::Errno::OutOfRange.as_i32()))?;
        Ok(Self { fd })
    }

    /// The raw descriptor number this handle owns.
    #[must_use]
    pub fn fd(&self) -> u32 {
        self.fd
    }

    /// Read into the whole of `buf` starting at byte `offset`, splitting the
    /// transfer into [`tairix_abi::FS_IO_MAX`]-sized syscalls, and return the
    /// number of bytes read (short of `buf.len()` at end of file).
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) of the first failing
    /// [`fs_read`].
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, i64> {
        io::Read::read_fill(&mut PositionalIo::new(self, offset), buf).map_err(positional_errno)
    }

    /// Write the whole of `data` starting at byte `offset` (or appending, if
    /// the handle was opened with [`OpenFlags::APPEND`]), splitting the
    /// transfer into [`tairix_abi::FS_IO_MAX`]-sized syscalls, and return the
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
        io::Write::write_drain(&mut PositionalIo::new(self, offset), data).map_err(positional_errno)
    }

    /// Report this handle's structural metadata.
    ///
    /// # Errors
    ///
    /// The raw negative kernel result (`-errno`) of the [`fs_stat_raw`]
    /// syscall, or [`tairix_abi::Errno::BufferTooSmall`] encoded as `-errno`
    /// if the kernel returns a short record.
    pub fn stat(&self) -> Result<FileStat, i64> {
        let mut buf = [0u8; FileStat::WIRE_LEN];
        let n = fs_stat_raw(self.fd, &mut buf)?;
        if n < FileStat::WIRE_LEN {
            return Err(-i64::from(tairix_abi::Errno::BufferTooSmall.as_i32()));
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

impl io::Read for File {
    /// Read from the **shared open-file-description cursor**, advancing it —
    /// the same sequential vocabulary every other descriptor speaks. Two
    /// handles cloned from one description (a spawn wire, a delegation)
    /// therefore walk the file together rather than each restarting it.
    ///
    /// Use [`File::read_at`] for a positional read that leaves the cursor
    /// alone.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Stream::new(self.fd).read(buf)
    }
}

impl io::Write for File {
    /// Write at the shared open-file-description cursor, advancing it (or at
    /// the end of file, for a handle opened with [`OpenFlags::APPEND`]).
    ///
    /// Use [`File::write_at`] for a positional write that leaves the cursor
    /// alone.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Stream::new(self.fd).write(buf)
    }
}

/// A [`io::Read`] / [`io::Write`] view of a [`File`] at an explicit,
/// self-advancing byte position.
///
/// This is what lets the positional helpers ([`File::read_at`],
/// [`File::write_at`]) reuse the one fill/drain loop in [`io::Read`] /
/// [`io::Write`] instead of carrying a second copy of it: the loop calls
/// back through the positional traps, and the adapter — not the loop — keeps
/// track of where the next chunk goes. It never touches the descriptor's
/// shared cursor.
struct PositionalIo<'a> {
    file: &'a File,
    offset: u64,
}

impl<'a> PositionalIo<'a> {
    /// A view of `file` starting at byte `offset`.
    const fn new(file: &'a File, offset: u64) -> Self {
        Self { file, offset }
    }

    /// Account `n` transferred bytes. Saturating: a position at the end of
    /// the 64-bit range stops advancing rather than wrapping onto the start
    /// of the file, and the caller's loop then makes no further progress and
    /// ends.
    fn advance(&mut self, n: usize) {
        self.offset = self.offset.saturating_add(n as u64);
    }
}

impl io::Read for PositionalIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = fs_read(self.file.fd, self.offset, buf).map_err(syscall_io_error)?;
        self.advance(n);
        Ok(n)
    }
}

impl io::Write for PositionalIo<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = fs_write(self.file.fd, self.offset, buf).map_err(syscall_io_error)?;
        self.advance(n);
        Ok(n)
    }
}

/// Wrap a raw negative kernel result (`-errno`) as the I/O layer's error.
fn syscall_io_error(ret: i64) -> io::Error {
    io::Error::Os(Errno::from_syscall(ret))
}

/// Unwrap an I/O-layer error back to the raw negative kernel result
/// (`-errno`) the positional [`File`] helpers report.
///
/// Only a kernel refusal can reach here — the positional adapter raises
/// nothing else and the fill/drain loops add no error of their own — so the
/// kernel's own code always survives the round trip.
fn positional_errno(err: io::Error) -> i64 {
    -i64::from(err.as_errno().as_i32())
}

/// An open directory handle wrapping a [`File`] opened with
/// [`OpenFlags::DIRECTORY`].
///
/// [`Dir::read`] reads the packed [`tairix_abi::DirEntry`] stream into the
/// caller's buffer; walk it with [`tairix_abi::DirEntry::decode`].
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
    /// [`tairix_abi::DirEntry`] stream, returning the number of bytes it
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

/// Initial byte size of the [`read_dir_all`] listing buffer: one page covers
/// a typical directory, and `BufferTooSmall` grows it from there.
const DIR_STREAM_INITIAL: usize = 4096;

/// Fill a growing buffer from a `BufferTooSmall`-signalling reader.
///
/// The one buffer-growth retry policy shared by every whole-transfer read
/// (today [`read_dir_all`]): start at `initial` bytes, double towards the
/// hard ceiling `max` each time `read` refuses with `BufferTooSmall`
/// (encoded as `-errno`), and return the exact bytes of the first successful
/// read. The policy is total: the buffer strictly grows on every retry and
/// stops at `max`, so the loop always terminates — a refusal at the ceiling
/// (or any other error) surfaces unchanged.
///
/// A reader that reports more bytes used than the buffer it was handed is
/// refused with `OutOfRange` rather than trusted: the count shapes a slice
/// the caller will parse, so it is validated like any other boundary input.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`) of the failing `read`, or
/// `-OutOfRange` for an over-reporting reader.
pub fn read_all_growing(
    initial: usize,
    max: usize,
    mut read: impl FnMut(&mut [u8]) -> Result<usize, i64>,
) -> Result<alloc::vec::Vec<u8>, i64> {
    let too_small = -i64::from(tairix_abi::Errno::BufferTooSmall.as_i32());
    let mut buf = alloc::vec![0u8; initial.min(max).max(1)];
    loop {
        match read(&mut buf) {
            Ok(used) => {
                if used > buf.len() {
                    return Err(-i64::from(tairix_abi::Errno::OutOfRange.as_i32()));
                }
                buf.truncate(used);
                return Ok(buf);
            }
            Err(ret) if ret == too_small && buf.len() < max => {
                let next = buf.len().saturating_mul(2).min(max);
                buf.resize(next, 0);
            }
            Err(ret) => return Err(ret),
        }
    }
}

/// Read the whole directory listing at the absolute `path`, returning the
/// packed [`tairix_abi::DirEntry`] stream sized to its exact byte length —
/// walk it with [`tairix_abi::fs::DirEntries`].
///
/// The one directory-listing call every tool shares (`ls`, the filesystem
/// browser): an [`open_dir`] resolve-and-authorise, then the
/// [`read_all_growing`] retry policy against the kernel's own per-transfer
/// staging cap ([`tairix_abi::fs::FS_IO_MAX`]), so no consumer re-derives
/// the grow loop.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`) of the failing `fs_open` or
/// `fs_readdir` syscall.
pub fn read_dir_all(path: &[u8]) -> Result<alloc::vec::Vec<u8>, i64> {
    let dir = open_dir(path)?;
    read_all_growing(DIR_STREAM_INITIAL, tairix_abi::fs::FS_IO_MAX, |buf| {
        dir.read(buf)
    })
}

/// Bytes of one `fs_read` while streaming a whole file.
///
/// A whole-file consumer reads documents of megabytes — a wallpaper master, a
/// program catalog — so the staging buffer is sized to keep the syscall count
/// proportionate to the file. Reading such a document a kilobyte at a time
/// costs thousands of traps and, on real storage, seconds; sixty-four kibibytes
/// is a hundredfold fewer while staying well inside the kernel's own
/// per-transfer cap ([`tairix_abi::fs::FS_IO_MAX`]).
const FILE_STREAM_CHUNK: usize = 64 * 1024;

// One staging buffer must be transferable by one syscall, and non-empty, or
// the read below could not make progress. Held at compile time so the read
// itself needs no runtime clamp.
const _: () = assert!(FILE_STREAM_CHUNK > 0 && FILE_STREAM_CHUNK <= tairix_abi::fs::FS_IO_MAX);

/// Read the open descriptor `fd` from its start until end-of-file, stopping one
/// chunk past `cap`.
///
/// The one whole-file streaming policy every consumer shares, so no caller
/// re-derives the chunk size and none can quietly pick a slower one. Answering
/// *past* the cap rather than truncating at it is what lets a caller tell an
/// oversize document from one that exactly fits: a length above `cap` is the
/// whole-document refusal to state, never a silently shortened answer the
/// caller would go on to parse.
///
/// # Errors
///
/// The raw negative kernel result (`-errno`) of the failing `fs_read`.
pub fn read_fd_to_end(fd: u32, cap: usize) -> Result<alloc::vec::Vec<u8>, i64> {
    let mut bytes = alloc::vec::Vec::new();
    let mut chunk = alloc::vec![0u8; FILE_STREAM_CHUNK];
    while bytes.len() <= cap {
        let offset = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        // `fs_read` holds its answer inside the buffer it was handed, so the
        // count always names a prefix of `chunk`.
        let taken = fs_read(fd, offset, &mut chunk)?;
        let Some(read) = chunk.get(..taken).filter(|read| !read.is_empty()) else {
            break;
        };
        bytes.extend_from_slice(read);
    }
    Ok(bytes)
}

/// Define the program's entry point.
///
/// `$entry` must be a `fn() -> i32`; the macro exports the runtime's
/// `__tairix_rt_main` symbol (which `_start` calls) so it invokes `$entry` and
/// hands its return value to the runtime, which routes it through `exit`.
/// Invoke it exactly once, at the crate root of a `#![no_main]` program.
#[macro_export]
macro_rules! entry {
    ($entry:path) => {
        // `#[no_mangle]` exports the fixed symbol `_start` resolves; the item
        // is private (no `pub`) so it needs no rustdoc and exports nothing to
        // the program's own namespace beyond the symbol the runtime links.
        #[no_mangle]
        fn __tairix_rt_main() -> i32 {
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
    // The trap seam lives in `tairix-abi-trap` (the single trap home) and is reached here through the `host-seam`
    // dev-dependency feature; production builds never compile it.
    use tairix_abi::SYSCALL_MAX_ARGS;
    use tairix_abi_trap::seam;

    /// Run `call` with the seam armed to return `ret`, returning the recorded
    /// `(number, args)`.
    fn capture(ret: u64, call: impl FnOnce()) -> (u64, [u64; SYSCALL_MAX_ARGS]) {
        seam::arm(ret);
        call();
        seam::last_call().expect("the wrapper must issue exactly one trap")
    }

    /// The negative register the kernel encodes `errno` as.
    fn refusal(errno: Errno) -> u64 {
        u64::from_ne_bytes((-i64::from(errno.as_i32())).to_ne_bytes())
    }

    #[test]
    fn every_errno_the_abi_defines_round_trips_through_the_raw_result() {
        // Walk the discriminant space rather than restating a list of variants,
        // so a newly appended errno is covered the moment it is added.
        let mut recovered = 0usize;
        for code in 1..=256i32 {
            if let Some(errno) = Errno::from_i32(code) {
                assert_eq!(
                    errno_from_raw(-i64::from(code)),
                    errno,
                    "the raw refusal -{code} must recover its own variant"
                );
                recovered += 1;
            }
        }
        assert!(
            recovered > 20,
            "the discriminant walk must actually reach the defined errnos, found {recovered}"
        );
    }

    #[test]
    fn a_success_value_is_not_mistaken_for_an_error_code() {
        // A non-negative result is a success the caller should never have
        // handed to the conversion; it must not surface as a plausible-looking
        // refusal a caller would act on.
        for raw in [0i64, 1, 7, 4096, i64::MAX] {
            assert_eq!(errno_from_raw(raw), Errno::NotImplemented);
        }
    }

    #[test]
    fn an_out_of_range_negative_fails_closed_rather_than_naming_a_wrong_errno() {
        // An unknown code, a magnitude no `i32` can hold, and the one value
        // that cannot be negated at all: each is unreadable, and each says so
        // rather than claiming an object is absent.
        for raw in [
            -(i64::from(i32::MAX) + 1),
            -(i64::from(u32::MAX) + 12),
            i64::MIN,
            -100_000,
        ] {
            assert_eq!(
                errno_from_raw(raw),
                Errno::NotImplemented,
                "the unreadable result {raw} must not become a real errno"
            );
            assert_ne!(
                errno_from_raw(raw),
                Errno::NotFound,
                "and never the absence a caller acts on"
            );
        }
    }

    #[test]
    fn stream_write_marshals_fd_pointer_and_len() {
        let buffer = *b"hello\n";
        let (number, args) = capture(6, || {
            assert_eq!(stream_write_result(tairix_abi::STDOUT, &buffer), Ok(6));
        });
        assert_eq!(number, NUM_STREAM_WRITE);
        assert_eq!(args[0], u64::from(tairix_abi::STDOUT));
        assert_eq!(args[1], buffer.as_ptr() as usize as u64);
        assert_eq!(args[2], 6);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn stream_write_marshals_whichever_descriptor_it_is_given() {
        let buffer = *b"warn\n";
        let (number, args) = capture(5, || {
            assert_eq!(stream_write_result(tairix_abi::STDERR, &buffer), Ok(5));
        });
        assert_eq!(number, NUM_STREAM_WRITE);
        assert_eq!(args[0], u64::from(tairix_abi::STDERR));
        // An ordinary descriptor takes the identical path: one vocabulary.
        let (number, args) = capture(5, || {
            assert_eq!(stream_write_result(7, &buffer), Ok(5));
        });
        assert_eq!(number, NUM_STREAM_WRITE);
        assert_eq!(args[0], 7);
    }

    #[test]
    fn stream_write_surfaces_the_kernel_refusal_rather_than_a_zero_count() {
        // A refused write (missing `CAP_CONSOLE_WRITE`, a read-only
        // descriptor, a broken pipe) must reach the caller as its own code:
        // reporting `0` would make the failure indistinguishable from a sink
        // that merely accepted nothing, and `write_all` would report the
        // wrong reason.
        let buffer = [0u8; 16];
        let (_, _) = capture(refusal(Errno::PermissionDenied), || {
            assert_eq!(
                stream_write_result(tairix_abi::STDOUT, &buffer),
                Err(Errno::PermissionDenied)
            );
        });
        let (_, _) = capture(refusal(Errno::BrokenPipe), || {
            assert_eq!(stream_write_result(9, &buffer), Err(Errno::BrokenPipe));
        });
    }

    #[test]
    fn stream_read_surfaces_the_kernel_refusal_rather_than_end_of_input() {
        // The defect this replaces: a failure reported as `Ok(0)` reads as
        // clean end-of-input, so a consumer silently truncates its input.
        let mut buffer = [0u8; 16];
        let (number, args) = capture(refusal(Errno::NotFound), || {
            assert_eq!(stream_read_result(4, &mut buffer, 0), Err(Errno::NotFound));
        });
        assert_eq!(number, NUM_STREAM_READ);
        assert_eq!(args[0], 4);
        // A genuine end-of-input is still an honest zero-length read.
        let (_, _) = capture(0, || {
            assert_eq!(stream_read_result(4, &mut buffer, 0), Ok(0));
        });
    }

    #[test]
    fn stream_read_marshals_its_timeout_bound() {
        let mut buffer = [0u8; 8];
        let (number, args) = capture(3, || {
            assert_eq!(stream_read_result(0, &mut buffer, 250), Ok(3));
        });
        assert_eq!(number, NUM_STREAM_READ);
        assert_eq!(args[3], 250);
    }

    #[test]
    fn stream_transfers_clamp_an_oversized_count_to_the_buffer_length() {
        // Defence in depth: a count larger than the buffer (a buggy kernel)
        // is clamped so the caller can never index past the slice it owns.
        let buffer = [0u8; 4];
        let (_, _) = capture(93, || {
            assert_eq!(stream_write_result(tairix_abi::STDOUT, &buffer), Ok(4));
        });
        let mut inbound = [0u8; 4];
        let (_, _) = capture(93, || {
            assert_eq!(
                stream_read_result(tairix_abi::STDIN, &mut inbound, 0),
                Ok(4)
            );
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
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(ipc_send(7, &payload), want);
        });
    }

    #[test]
    fn the_elevation_request_buffer_is_erased_on_every_path() {
        const PASSWORD: &str = "correct horse battery staple";
        let request = ElevateRequest::Verify { password: PASSWORD };

        // The broker answered. The exchange really did put the plaintext on
        // the stack, and the guard the caller wraps it in takes it away.
        seam::arm(u64::try_from(ELEVATE_REPLY_LEN).expect("reply length fits"));
        let mut buf = Wiped::<ELEVATE_MAX_REQUEST>::new();
        assert!(
            elevate_exchange(0x1234, &request, &mut buf[..]).is_ok(),
            "the armed seam answers the call"
        );
        assert!(
            contains(&buf[..], PASSWORD.as_bytes()),
            "the encoded request carried the plaintext password"
        );
        buf.wipe();
        assert_eq!(buf[..], [0u8; ELEVATE_MAX_REQUEST][..]);

        // The broker refused. A refusal leaves exactly the same plaintext
        // behind — this is the path a wrong password takes — and the same
        // guard erases it.
        let refused = -i64::from(Errno::PermissionDenied.as_i32());
        seam::arm(u64::from_ne_bytes(refused.to_ne_bytes()));
        let mut buf = Wiped::<ELEVATE_MAX_REQUEST>::new();
        assert_eq!(
            elevate_exchange(0x1234, &request, &mut buf[..]),
            Err(Errno::PermissionDenied)
        );
        assert!(contains(&buf[..], PASSWORD.as_bytes()));
        buf.wipe();
        assert_eq!(buf[..], [0u8; ELEVATE_MAX_REQUEST][..]);
    }

    /// Whether `haystack` contains `needle` anywhere.
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|run| run == needle)
    }

    #[test]
    fn port_bind_marshals_endpoint_and_bounds() {
        let (number, args) = capture(0, || {
            assert_eq!(port_bind(0x5EAD_0001, 40, 8), 0);
        });
        assert_eq!(number, NUM_PORT_BIND);
        assert_eq!(args[0], 0x5EAD_0001);
        assert_eq!(args[1], 40);
        assert_eq!(args[2], 8);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn port_bind_surfaces_negative_errno_encoding() {
        // `AlreadyExists` (a clashing id) is encoded as the two's-complement
        // negation; the wrapper hands that signed value back unchanged.
        let want = -i64::from(tairix_abi::Errno::AlreadyExists.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(port_bind(0x5EAD_0001, 40, 8), want);
        });
    }

    #[test]
    fn ipc_recv_marshals_endpoint_buffers_and_sender_out() {
        let mut buf = [0u8; 40];
        let mut sender = [0u8; tairix_abi::ORIGIN_WIRE_LEN];
        let buf_ptr = buf.as_mut_ptr() as usize as u64;
        let sender_ptr = sender.as_mut_ptr() as usize as u64;
        let (number, args) = capture(12, || {
            assert_eq!(ipc_recv(0x5EAD_0001, &mut buf, &mut sender), Ok(12));
        });
        assert_eq!(number, NUM_IPC_RECV);
        assert_eq!(args[0], 0x5EAD_0001);
        assert_eq!(args[1], buf_ptr);
        assert_eq!(args[2], 40);
        assert_eq!(args[3], sender_ptr);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn ipc_recv_surfaces_negative_errno_encoding() {
        // `WouldBlock` (an empty mailbox) is the retryable signal the
        // caller parks on; the wrapper hands the signed value back.
        let mut buf = [0u8; 8];
        let mut sender = [0u8; tairix_abi::ORIGIN_WIRE_LEN];
        let want = -i64::from(tairix_abi::Errno::WouldBlock.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(ipc_recv(9, &mut buf, &mut sender), Err(want));
        });
    }

    #[test]
    fn port_resolve_marshals_name_pointer_and_len() {
        let name = *b"desktop.pointer";
        let (number, args) = capture(7, || {
            assert_eq!(port_resolve(&name), 7);
        });
        assert_eq!(number, NUM_PORT_RESOLVE);
        assert_eq!(args[0], name.as_ptr() as usize as u64);
        assert_eq!(args[1], name.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn port_resolve_surfaces_negative_errno_encoding() {
        // `NotFound` (nothing published under the name) comes back as the
        // two's-complement negation; the wrapper hands it back unchanged.
        let name = *b"desktop.pointer";
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(port_resolve(&name), want);
        });
    }

    #[test]
    fn stream_read_marshals_fd_pointer_and_len() {
        let mut buffer = [0u8; 16];
        let ptr = buffer.as_mut_ptr() as usize as u64;
        let (number, args) = capture(7, || {
            assert_eq!(stream_read_result(STDIN, &mut buffer, 0), Ok(7));
        });
        assert_eq!(number, NUM_STREAM_READ);
        assert_eq!(args[0], u64::from(STDIN));
        assert_eq!(args[1], ptr);
        assert_eq!(args[2], 16);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn spawn_marshals_path_pointer_len_and_inherit() {
        let path = *b"/System/Commands/elsh.app/Run";
        let (number, args) = capture(7, || {
            assert_eq!(spawn(&path), 7);
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        // Slots 2/3 carry the encoded attach block (`plans/SPAWN.md`
        // SP10): a live pointer of exactly the fixed length. The block's
        // bytes are freed when the wrapper returns (the seam records only
        // the registers); content fidelity — the plain `spawn` carrying
        // both inherit sentinels — is the `SpawnAttach` codec's contract,
        // covered by its `tairix_abi` round-trip tests.
        assert_ne!(args[2], 0);
        assert_eq!(args[3], tairix_abi::SPAWN_ATTACH_LEN as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn spawn_attached_marshals_the_attach_and_strings_blocks() {
        let path = *b"/System/Commands/wc.app/Run";
        let attach = tairix_abi::SpawnAttach {
            wires: [
                tairix_abi::FdWire::Handle(4),
                tairix_abi::FdWire::Inherit,
                tairix_abi::FdWire::Inherit,
                tairix_abi::FdWire::Inherit,
            ],
            ..tairix_abi::SpawnAttach::INHERIT
        };
        let args: [&[u8]; 1] = [b"wc"];
        let (number, raw) = capture(21, || {
            assert_eq!(spawn_attached(&path, &attach, &args, &[]), 21);
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(raw[0], path.as_ptr() as usize as u64);
        assert_eq!(raw[1], path.len() as u64);
        assert_ne!(raw[2], 0);
        assert_eq!(raw[3], tairix_abi::SPAWN_ATTACH_LEN as u64);
        assert_ne!(raw[4], 0);
        let expected_len = tairix_abi::process_start_encoded_len(&args, &[]).expect("sized") as u64;
        assert_eq!(raw[5], expected_len);
    }

    #[test]
    fn pipe_create_marshals_the_out_pointer_and_decodes_the_pair() {
        let (number, args) = capture(0, || {
            // The seam returns 0 (success) without writing the out-param,
            // so the decoded pair is the zeroed default — the marshalling,
            // not the kernel's fd choice, is under test.
            assert_eq!(pipe_create(), Ok((0, 0)));
        });
        assert_eq!(number, NUM_PIPE_CREATE);
        assert_ne!(args[0], 0);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
        // A refusal surfaces the negative errno register verbatim.
        let neg =
            u64::from_ne_bytes((-i64::from(tairix_abi::Errno::BadAddress.as_i32())).to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(
                pipe_create(),
                Err(-i64::from(tairix_abi::Errno::BadAddress.as_i32()))
            );
        });
    }

    #[test]
    fn spawn_with_marshals_the_encoded_startup_strings_block() {
        let path = *b"/System/Commands/man.app/Run";
        let args: [&[u8]; 2] = [b"man", b"ps"];
        let envs: [&[u8]; 1] = [b"LANG=fr-FR"];
        let (number, raw) = capture(11, || {
            assert_eq!(
                spawn_with(&path, CONSOLE_INHERIT, SPAWN_UID_INHERIT, &args, &envs),
                11
            );
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(raw[0], path.as_ptr() as usize as u64);
        assert_eq!(raw[1], path.len() as u64);
        assert_ne!(raw[2], 0);
        assert_eq!(raw[3], tairix_abi::SPAWN_ATTACH_LEN as u64);
        // Slots 4/5 carry the encoded `PSV1` block: a live (non-null)
        // pointer whose length is exactly what the one shared encoder
        // computes for these strings. The block's own bytes are freed when
        // `spawn_with` returns (the seam records only the registers), so
        // content fidelity is the encoder's contract, covered by the
        // `tairix_abi::process` round-trip tests — asserting the marshalled
        // shape here is the wrapper's whole obligation.
        assert_ne!(raw[4], 0);
        let expected_len =
            tairix_abi::process_start_encoded_len(&args, &envs).expect("sized") as u64;
        assert_eq!(raw[5], expected_len);
    }

    #[test]
    fn spawn_as_marshals_the_attach_block() {
        let path = *b"/System/Commands/elsh.app/Run";
        let (number, args) = capture(9, || {
            // login starting a user's shell on the inherited console under a
            // switched-to uid — both selectors travel inside the attach
            // block (its codec's round-trip tests cover the content).
            assert_eq!(spawn_as(&path, CONSOLE_INHERIT, 1000), 9);
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_ne!(args[2], 0);
        assert_eq!(args[3], tairix_abi::SPAWN_ATTACH_LEN as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn spawn_at_marshals_the_attach_block() {
        let path = *b"/System/Services/login.app/Run";
        let (number, args) = capture(8, || {
            assert_eq!(spawn_at(&path, 1), 8);
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        // The console index travels inside the attach block.
        assert_ne!(args[2], 0);
        assert_eq!(args[3], tairix_abi::SPAWN_ATTACH_LEN as u64);
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
    fn set_input_mode_marshals_stdin_fd_and_the_mode() {
        // Each mode marshals fd 0 and its own wire discriminant.
        for mode in [InputMode::Cooked, InputMode::Secret, InputMode::Raw] {
            let (number, args) = capture(0, || {
                assert_eq!(set_input_mode(mode), 0);
            });
            assert_eq!(number, NUM_STREAM_INPUT_MODE);
            assert_eq!(args[0], u64::from(STDIN));
            assert_eq!(args[1], u64::from(mode.as_u32()));
            assert_eq!(&args[2..], &[0, 0, 0, 0]);
        }
    }

    #[test]
    fn key_inject_marshals_the_seat_record_pointer_and_len() {
        use tairix_abi::input::{KeyValue, Modifiers};
        let record = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        };
        let want = i64::try_from(KeyInput::WIRE_LEN).expect("WIRE_LEN fits an i64");
        let (number, args) = capture(KeyInput::WIRE_LEN as u64, || {
            assert_eq!(key_inject(3, &record), want);
        });
        assert_eq!(number, NUM_KEY_INJECT);
        // arg 0 is the seat id; arg 1 the record buffer pointer; arg 2 its
        // WIRE_LEN.
        assert_eq!(args[0], 3);
        assert_ne!(args[1], 0);
        assert_eq!(args[2], KeyInput::WIRE_LEN as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn key_inject_surfaces_negative_errno_encoding() {
        use tairix_abi::input::{KeyValue, Modifiers};
        // An unwired arbiter refuses the inject with `NotImplemented`; the
        // wrapper surfaces the raw `-errno` register.
        let record = KeyInput::Pressed {
            key: KeyValue::Char('x'),
            modifiers: Modifiers::default(),
        };
        let want = -i64::from(tairix_abi::Errno::NotImplemented.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(key_inject(0, &record), want);
        });
    }

    #[test]
    fn display_acquire_and_release_marshal_the_seat_id() {
        // A successful acquire returns the minted lease generation.
        let (number, args) = capture(1, || {
            assert_eq!(display_acquire(3), 1);
        });
        assert_eq!(number, NUM_DISPLAY_ACQUIRE);
        assert_eq!(args, [3, 0, 0, 0, 0, 0]);

        let (number, args) = capture(0, || {
            assert_eq!(display_release(3, ReleaseSurface::Text), 0);
        });
        assert_eq!(number, NUM_DISPLAY_RELEASE);
        assert_eq!(args, [3, 0, 0, 0, 0, 0]);

        // The hand-over disposition reaches the kernel as the second
        // argument: a release that drops it would replay the text console
        // over the gap between two graphical sessions.
        let (number, args) = capture(0, || {
            assert_eq!(display_release(3, ReleaseSurface::Handover), 0);
        });
        assert_eq!(number, NUM_DISPLAY_RELEASE);
        assert_eq!(args, [3, ReleaseSurface::Handover.as_u64(), 0, 0, 0, 0]);
    }

    #[test]
    fn seat_switch_and_revoke_marshal_their_arguments() {
        let (number, args) = capture(0, || {
            assert_eq!(seat_switch(0, 2), 0);
        });
        assert_eq!(number, NUM_SEAT_SWITCH);
        assert_eq!(args, [0, 2, 0, 0, 0, 0]);

        let (number, args) = capture(0, || {
            assert_eq!(seat_revoke(0), 0);
        });
        assert_eq!(number, NUM_SEAT_REVOKE);
        assert_eq!(args, [0; 6]);

        // The wrapper surfaces the raw `-errno` register unchanged.
        let want = -i64::from(tairix_abi::Errno::SeatNotOwner.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(seat_revoke(0), want);
        });
    }

    #[test]
    fn keyboard_read_marshals_the_seat_buffer_pointer_and_len() {
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        let want = i64::try_from(KeyInput::WIRE_LEN).expect("WIRE_LEN fits an i64");
        let (number, args) = capture(KeyInput::WIRE_LEN as u64, || {
            assert_eq!(keyboard_read(3, &mut buf), want);
        });
        assert_eq!(number, NUM_KEYBOARD_READ);
        assert_eq!(args[0], 3);
        assert_ne!(args[1], 0);
        assert_eq!(args[2], KeyInput::WIRE_LEN as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn pointer_inject_marshals_the_seat_record_pointer_and_len() {
        let record = PointerInput::MovedBy { dx: 5, dy: -9 };
        let want = i64::try_from(PointerInput::WIRE_LEN).expect("WIRE_LEN fits an i64");
        let (number, args) = capture(PointerInput::WIRE_LEN as u64, || {
            assert_eq!(pointer_inject(3, &record), want);
        });
        assert_eq!(number, NUM_POINTER_INJECT);
        // arg 0 is the seat id; arg 1 the record buffer pointer; arg 2 its
        // WIRE_LEN.
        assert_eq!(args[0], 3);
        assert_ne!(args[1], 0);
        assert_eq!(args[2], PointerInput::WIRE_LEN as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn pointer_read_marshals_the_seat_buffer_pointer_and_len() {
        let mut buf = [0u8; PointerInput::WIRE_LEN];
        let want = i64::try_from(PointerInput::WIRE_LEN).expect("WIRE_LEN fits an i64");
        let (number, args) = capture(PointerInput::WIRE_LEN as u64, || {
            assert_eq!(pointer_read(3, &mut buf), want);
        });
        assert_eq!(number, NUM_POINTER_READ);
        assert_eq!(args[0], 3);
        assert_ne!(args[1], 0);
        assert_eq!(args[2], PointerInput::WIRE_LEN as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn pointer_read_surfaces_negative_errno_encoding() {
        // A non-owner's drain is refused with `SeatNotOwner`; the wrapper
        // surfaces the raw `-errno` register unchanged.
        let mut buf = [0u8; PointerInput::WIRE_LEN];
        let want = -i64::from(tairix_abi::Errno::SeatNotOwner.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(pointer_read(0, &mut buf), want);
        });
    }

    #[test]
    fn shm_grant_marshals_the_region_and_endpoint() {
        let (number, args) = capture(5, || {
            assert_eq!(shm_grant(42, 0xD15_1001), 5);
        });
        assert_eq!(number, NUM_SHM_GRANT);
        assert_eq!(args[0], 42);
        assert_eq!(args[1], 0xD15_1001);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn shm_grant_surfaces_negative_errno_encoding() {
        // A region the caller does not hold is refused with `NotFound`; the
        // wrapper surfaces the raw `-errno` register unchanged.
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(shm_grant(42, 0xD15_1001), want);
        });
    }

    #[test]
    fn call_peer_seat_marshals_endpoint_ticket_and_seat() {
        let (number, args) = capture(3, || {
            assert_eq!(call_peer_seat(0xD15_1001, 9, 0), 3);
        });
        assert_eq!(number, NUM_CALL_PEER_SEAT);
        assert_eq!(args[0], 0xD15_1001);
        assert_eq!(args[1], 9);
        assert_eq!(args[2], 0);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn call_peer_seat_surfaces_negative_errno_encoding() {
        // The evicted peer's check is refused with the distinct
        // `SeatRevoked`; the wrapper surfaces the raw `-errno` register
        // unchanged so the service can refuse the present with the typed
        // cause.
        let want = -i64::from(tairix_abi::Errno::SeatRevoked.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(call_peer_seat(0xD15_1001, 9, 0), want);
        });
    }

    #[test]
    fn set_input_mode_surfaces_negative_errno_encoding() {
        // A console-less build refuses the mode change with
        // `NotImplemented`; the wrapper surfaces the raw `-errno` register
        // unchanged.
        let want = -i64::from(tairix_abi::Errno::NotImplemented.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(set_input_mode(InputMode::Cooked), want);
        });
    }

    #[test]
    fn spawn_surfaces_negative_errno_encoding() {
        // `NotFound` (7) is encoded by the kernel as the two's-complement
        // negation; the wrapper hands that signed value back unchanged. The
        // register carries the raw bit pattern, so reinterpret rather than
        // sign-loss-cast it.
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
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
        let want = -i64::from(tairix_abi::Errno::OutOfMemory.as_i32());
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
    fn mem_pin_issues_the_pin_syscall_with_no_arguments() {
        let (number, args) = capture(0, || {
            assert_eq!(mem_pin(), 0);
        });
        assert_eq!(number, NUM_MEM_PIN);
        assert_eq!(&args, &[0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn signal_intake_marshals_each_op() {
        for op in [
            SignalIntakeOp::Enable,
            SignalIntakeOp::Disable,
            SignalIntakeOp::Take,
        ] {
            let (number, args) = capture(0, || {
                assert_eq!(signal_intake(op), 0);
            });
            assert_eq!(number, NUM_SIGNAL_INTAKE);
            assert_eq!(args[0], u64::from(op.as_u32()));
            assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
        }
    }

    #[test]
    fn signal_intake_surfaces_negative_errno_encoding() {
        // An empty intake's `Take` surfaces the typed non-blocking answer
        // unchanged so the caller parks on its wait-set, never a poll loop.
        let want = -i64::from(tairix_abi::Errno::WouldBlock.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(signal_intake(SignalIntakeOp::Take), want);
        });
    }

    #[test]
    fn mem_pin_surfaces_negative_errno_encoding() {
        // A refused pin (no capability, or over the pinned-memory bound)
        // surfaces unchanged so the caller can degrade gracefully.
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(mem_pin(), want);
        });
    }

    #[test]
    fn sched_set_realtime_marshals_the_class_boolean() {
        // Entering the real-time class marshals `1`; leaving marshals `0`.
        let (number, args) = capture(0, || {
            assert_eq!(sched_set_realtime(true), 0);
        });
        assert_eq!(number, NUM_SCHED_SET_REALTIME);
        assert_eq!(args[0], 1);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);

        let (number, args) = capture(0, || {
            assert_eq!(sched_set_realtime(false), 0);
        });
        assert_eq!(number, NUM_SCHED_SET_REALTIME);
        assert_eq!(args[0], 0);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn sched_set_realtime_surfaces_negative_errno_encoding() {
        // A refused entry (no capability) surfaces unchanged so the driver
        // can report the refusal rather than silently run time-shared.
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(sched_set_realtime(true), want);
        });
    }

    #[test]
    fn mem_unpin_issues_the_unpin_syscall_with_no_arguments() {
        let (number, args) = capture(0, || {
            assert_eq!(mem_unpin(), 0);
        });
        assert_eq!(number, NUM_MEM_UNPIN);
        assert_eq!(&args, &[0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn file_map_marshals_fd_offset_and_len() {
        let base = 0x30_0000_0000u64;
        let want = i64::try_from(base).expect("base fits an i64");
        let (number, args) = capture(base, || {
            assert_eq!(file_map(7, 0x3000, 0x4001), want);
        });
        assert_eq!(number, NUM_FILE_MAP);
        assert_eq!(args[0], 7);
        assert_eq!(args[1], 0x3000);
        assert_eq!(args[2], 0x4001);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn file_unmap_marshals_base_and_len_and_surfaces_errno() {
        let base = 0x30_0000_0000u64;
        let (number, args) = capture(0, || {
            assert_eq!(file_unmap(base, 0x4001), 0);
        });
        assert_eq!(number, NUM_FILE_UNMAP);
        assert_eq!(args[0], base);
        assert_eq!(args[1], 0x4001);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);

        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(file_unmap(base, 0x4001), want);
        });
    }

    #[test]
    fn mem_unmap_surfaces_negative_errno_encoding() {
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(mem_unmap(0x10_0100_0000, 0x1000), want);
        });
    }

    /// The `-errno` the wrapper reports when the "kernel" (the seam, which
    /// writes nothing) claims success but the local record is undecodable —
    /// the fail-closed decode path a mocked success always takes.
    fn undecodable_record() -> i64 {
        -i64::from(tairix_abi::Errno::OutOfRange.as_i32())
    }

    #[test]
    fn wait_marshals_pid_flags_and_record_pointer() {
        let mut status = WaitStatus::Exited(0);
        // The seam cannot write the status record, so a mocked "success"
        // leaves the zeroed (reserved-kind) record in place and the wrapper
        // refuses it fail-closed rather than fabricating a status.
        let (number, args) = capture(5, || {
            assert_eq!(
                wait(9, &mut status, WaitFlags::STOPPED),
                undecodable_record()
            );
        });
        assert_eq!(number, NUM_WAIT);
        assert_eq!(args[0], 9);
        // The record pointer names the wrapper's local record (non-null).
        assert_ne!(args[1], 0);
        // The caller's flag set reaches the register verbatim.
        assert_eq!(args[2], u64::from(WaitFlags::STOPPED.bits()));
        assert_eq!(&args[3..], &[0, 0, 0]);
        // The out-status was left untouched by the refused decode.
        assert_eq!(status, WaitStatus::Exited(0));
    }

    #[test]
    fn wait_marshals_wait_any_as_a_sign_extended_minus_one() {
        let mut status = WaitStatus::Exited(0);
        let (number, args) = capture(3, || {
            assert_eq!(
                wait(tairix_abi::WAIT_PID_ANY, &mut status, WaitFlags::empty()),
                undecodable_record()
            );
        });
        assert_eq!(number, NUM_WAIT);
        // `WAIT_PID_ANY` (-1) sign-extends to all-ones in the argument register.
        assert_eq!(args[0], u64::MAX);
        // A blocking wait with no options carries an empty flag set.
        assert_eq!(args[2], 0);
    }

    #[test]
    fn wait_surfaces_negative_errno_encoding() {
        // `NotFound` (no such child) is encoded as the two's-complement
        // negation; the wrapper hands that signed value back unchanged and
        // leaves `status` untouched.
        let mut status = WaitStatus::Exited(7);
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(wait(9, &mut status, WaitFlags::empty()), want);
        });
        assert_eq!(status, WaitStatus::Exited(7));
    }

    #[test]
    fn try_wait_marshals_the_nonblock_flag() {
        let mut status = WaitStatus::Exited(0);
        let (number, args) = capture(5, || {
            assert_eq!(try_wait(9, &mut status), undecodable_record());
        });
        assert_eq!(number, NUM_WAIT);
        assert_eq!(args[0], 9);
        assert_ne!(args[1], 0);
        // The only difference from a blocking `wait` is the NONBLOCK flag in
        // the third argument slot.
        assert_eq!(args[2], u64::from(WaitFlags::NONBLOCK.bits()));
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn try_wait_surfaces_would_block_encoding() {
        // A still-running child is reported as the two's-complement negation
        // of `WouldBlock`; the wrapper hands that signed value back unchanged
        // so the caller can retry rather than treating it as a hard failure.
        let mut status = WaitStatus::Exited(0);
        let want = -i64::from(tairix_abi::Errno::WouldBlock.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(try_wait(9, &mut status), want);
        });
    }

    #[test]
    fn console_foreground_marshals_fd_and_signed_pid() {
        let (number, args) = capture(0, || {
            assert_eq!(console_foreground(STDIN, 9), 0);
        });
        assert_eq!(number, NUM_CONSOLE_FOREGROUND);
        assert_eq!(args[0], u64::from(STDIN));
        assert_eq!(args[1], 9);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
        // Clearing passes the `0` sentinel through unchanged.
        let (_, args) = capture(0, || {
            assert_eq!(console_foreground(STDIN, 0), 0);
        });
        assert_eq!(args[1], 0);
    }

    #[test]
    fn console_foreground_surfaces_negative_errno_encoding() {
        // A non-child target is refused with `NotFound`; the wrapper hands
        // the signed encoding back unchanged.
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(console_foreground(STDIN, 9), want);
        });
    }

    #[test]
    fn signal_marshals_pid_and_signal_discriminant() {
        let (number, args) = capture(0, || {
            assert_eq!(signal(9, Signal::Continue), 0);
        });
        assert_eq!(number, NUM_SIGNAL);
        assert_eq!(args[0], 9);
        assert_eq!(args[1], u64::from(Signal::Continue.as_u32()));
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn signal_surfaces_negative_errno_encoding() {
        // `NotImplemented` (no producer installed yet) is encoded as the
        // two's-complement negation; the wrapper hands it back unchanged.
        let want = -i64::from(tairix_abi::Errno::NotImplemented.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(signal(9, Signal::Kill), want);
        });
    }

    #[test]
    fn sched_set_priority_marshals_pid_and_level_discriminant() {
        let (number, args) = capture(0, || {
            assert_eq!(sched_set_priority(9, SchedPriority::Low), 0);
        });
        assert_eq!(number, NUM_SCHED_SET_PRIORITY);
        assert_eq!(args[0], 9);
        assert_eq!(args[1], u64::from(SchedPriority::Low.as_u32()));
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn sched_set_priority_surfaces_negative_errno_encoding() {
        // A raise without `CAP_PROC_CONTROL` is refused; the wrapper hands
        // the signed encoding back unchanged.
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(sched_set_priority(9, SchedPriority::High), want);
        });
    }

    #[test]
    fn system_power_marshals_the_action_discriminant() {
        for action in [PowerAction::PowerOff, PowerAction::Restart] {
            let (number, args) = capture(0, || {
                let _ = system_power(action);
            });
            assert_eq!(number, NUM_SYSTEM_POWER);
            assert_eq!(args[0], u64::from(action.as_u32()));
            assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
        }
    }

    #[test]
    fn system_power_surfaces_negative_errno_encoding() {
        // A caller without `CAP_SYSTEM_POWER` is refused and the call comes
        // back; the wrapper hands the signed encoding on unchanged.
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(system_power(PowerAction::PowerOff), want);
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
        let want = -i64::from(tairix_abi::Errno::OutOfRange.as_i32());
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
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
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
    fn boot_session_issues_a_zero_arg_trap_and_maps_the_reading() {
        let (number, args) = capture(BootSession::Graphical.as_u64(), || {
            assert_eq!(boot_session(), BootSession::Graphical);
        });
        assert_eq!(number, NUM_BOOT_SESSION_GET);
        // `boot_session_get` takes no arguments and no memory operand.
        assert_eq!(args, [0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn boot_session_fails_closed_on_an_unknown_or_error_reading() {
        // A discriminant this ABI does not define, and a negative (error)
        // return, both read as "no choice" so the stored default decides —
        // never a fabricated session.
        capture(9, || assert_eq!(boot_session(), BootSession::Unset));
        capture(u64::MAX, || assert_eq!(boot_session(), BootSession::Unset));
    }

    #[test]
    fn clock_delay_now_us_floors_nanoseconds_to_microseconds() {
        use tairix_abi::Delay;
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
    fn park_until_ns_returns_immediately_for_a_past_deadline() {
        // A deadline already reached must not park even once (no needless
        // kernel round-trip).
        let mut parks = 0u32;
        park_until_ns(100, || 100, |_| parks += 1);
        assert_eq!(parks, 0);
        park_until_ns(50, || 100, |_| parks += 1);
        assert_eq!(parks, 0);
    }

    #[test]
    fn park_until_ns_grants_the_full_remainder_and_reparks_on_an_early_wake() {
        // The park is granted exactly the remaining window, so the kernel's
        // one-shot deadline wakes the task once; a spurious early wake
        // re-parks for what is left rather than returning short.
        let clock = core::cell::Cell::new(0u64);
        let granted = core::cell::RefCell::new(alloc::vec::Vec::new());
        park_until_ns(
            1_000,
            || clock.get(),
            |remaining| {
                granted.borrow_mut().push(remaining);
                // The first wake is spurious (the clock is only half way);
                // the second reaches the deadline.
                clock.set(if clock.get() == 0 { 400 } else { 1_000 });
            },
        );
        assert_eq!(granted.into_inner(), alloc::vec![1_000, 600]);
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
        let want = -i64::from(tairix_abi::Errno::BufferTooSmall.as_i32());
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
        let want = -i64::from(tairix_abi::Errno::TimedOut.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(hw_tree_wait(3, 0), want);
        });
    }

    #[test]
    fn random_get_marshals_buffer_flags_and_clamps_the_count() {
        let mut buf = [0u8; 16];
        let ptr = buf.as_mut_ptr() as usize as u64;
        let (number, args) = capture(16, || {
            assert_eq!(random_get(&mut buf, RandomFlags::NON_BLOCKING), Ok(16));
        });
        assert_eq!(number, NUM_RANDOM_GET);
        assert_eq!(args[0], ptr);
        assert_eq!(args[1], 16);
        assert_eq!(args[2], u64::from(RandomFlags::NON_BLOCKING.bits()));
        assert_eq!(&args[3..], &[0, 0, 0]);
        // Defence in depth: a count past the buffer is clamped.
        let (_, _) = capture(9999, || {
            assert_eq!(random_get(&mut buf, RandomFlags::empty()), Ok(16));
        });
    }

    #[test]
    fn random_get_surfaces_negative_errno_encoding() {
        // `EntropyNotReady` (a non-blocking draw before the RNG is seeded)
        // is encoded as the two's-complement negation; hand it back.
        let mut buf = [0u8; 8];
        let want = -i64::from(tairix_abi::Errno::EntropyNotReady.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(random_get(&mut buf, RandomFlags::NON_BLOCKING), Err(want));
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
    fn users_admin_marshals_both_buffers_and_surfaces_errors() {
        let req = [1u8, 0, 9, 0];
        let mut out = [0u8; 32];
        let req_ptr = req.as_ptr() as usize as u64;
        let out_ptr = out.as_mut_ptr() as usize as u64;
        // A list response of 5 bytes comes back as the clamped count.
        let (number, args) = capture(5, || {
            assert_eq!(users_admin(&req, &mut out), Ok(5));
        });
        assert_eq!(number, NUM_USERS_ADMIN);
        assert_eq!(args[0], req_ptr);
        assert_eq!(args[1], req.len() as u64);
        assert_eq!(args[2], out_ptr);
        assert_eq!(args[3], out.len() as u64);
        assert_eq!(&args[4..], &[0, 0]);

        // A refusal surfaces the raw negative errno unchanged.
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        // The capture takes the raw register the kernel would return, which
        // for a refusal is the negated errno's bit pattern.
        #[allow(clippy::cast_sign_loss)]
        let (_, _) = capture(want as u64, || {
            assert_eq!(users_admin(&req, &mut out), Err(want));
        });

        // Defence in depth: a count past the buffer is clamped.
        let (_, _) = capture(1_000, || {
            assert_eq!(users_admin(&req, &mut out), Ok(out.len()));
        });
    }

    #[test]
    fn users_db_wait_surfaces_negative_errno_encoding() {
        // `TimedOut` is encoded as the two's-complement negation; the
        // wrapper hands that signed value back unchanged.
        let want = -i64::from(tairix_abi::Errno::TimedOut.as_i32());
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
                    tairix_abi::driver_store::DRIVER_STORE_ENDPOINT,
                    &request,
                    &mut reply
                ),
                Ok(12)
            );
        });
        assert_eq!(number, NUM_IPC_CALL);
        assert_eq!(args[0], tairix_abi::driver_store::DRIVER_STORE_ENDPOINT);
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
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
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
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(shm_create(0x1000, &mut id), want);
        });
    }

    #[test]
    fn shm_map_marshals_the_handle_and_the_len_out_pointer() {
        let mut len = 0u64;
        let len_ptr = core::ptr::addr_of_mut!(len) as usize as u64;
        let (number, args) = capture(0x8000, || {
            assert_eq!(shm_map(0xDEAD, &mut len), 0x8000);
        });
        assert_eq!(number, NUM_SHM_MAP);
        assert_eq!(args[0], 0xDEAD);
        assert_eq!(args[1], len_ptr);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
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
        let want = -i64::from(tairix_abi::Errno::TimedOut.as_i32());
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
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(fs_open(b"/x", OpenFlags::READ), want);
        });
    }

    #[test]
    fn resource_open_marshals_reference_flags_and_returns_the_descriptor() {
        let reference = b"sys:random";
        let flags = OpenFlags::READ;
        let (number, args) = capture(5, || {
            assert_eq!(resource_open(reference, flags), 5);
        });
        assert_eq!(number, NUM_RESOURCE_OPEN);
        assert_eq!(args[0], reference.as_ptr() as usize as u64);
        assert_eq!(args[1], reference.len() as u64);
        assert_eq!(args[2], u64::from(flags.bits()));
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn resource_open_surfaces_negative_errno_encoding() {
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(resource_open(b"sys:nope", OpenFlags::READ), want);
        });
    }

    // --- File::open routing (the one shared spelling rule) -----------------
    //
    // `File::open` is the one open-by-name path every first-party program
    // uses, so the routing between the filesystem and the kernel's resource
    // resolver is asserted here — never re-tested (or re-implemented) per
    // tool. Each opened handle is forgotten inside the capture closure so
    // its drop's `fs_close` trap cannot overwrite the recorded open; the
    // seam holds no real descriptor to leak.

    #[test]
    fn file_open_routes_paths_to_fs_open() {
        // Absolute, dot-escaped, alias-form (`:` followed by `/`), and
        // unregistered-prefix spellings are all filesystem paths; a file
        // whose name contains `:` stays reachable as `./name`.
        for path in [
            b"/Users/root/notes".as_slice(),
            b"./sys:random".as_slice(),
            b"sys:/x".as_slice(),
            b"home:file".as_slice(),
        ] {
            let (number, args) = capture(4, || {
                let file = File::open(path, OpenFlags::READ).expect("armed open must succeed");
                core::mem::forget(file);
            });
            assert_eq!(number, NUM_FS_OPEN, "{path:?} must be a filesystem open");
            assert_eq!(args[0], path.as_ptr() as usize as u64);
            assert_eq!(args[1], path.len() as u64);
        }
    }

    #[test]
    fn file_open_routes_a_reference_to_resource_open() {
        let reference = b"sys:random";
        let (number, args) = capture(5, || {
            let file = File::open(reference, OpenFlags::READ).expect("armed open must succeed");
            assert_eq!(file.fd(), 5);
            core::mem::forget(file);
        });
        assert_eq!(number, NUM_RESOURCE_OPEN);
        assert_eq!(args[0], reference.as_ptr() as usize as u64);
        assert_eq!(args[1], reference.len() as u64);
        assert_eq!(args[2], u64::from(OpenFlags::READ.bits()));
    }

    #[test]
    fn file_open_never_retries_a_malformed_reference_as_a_path() {
        // A registered-namespace spelling that is not a well-formed
        // reference still goes to the resource resolver, whose refusal
        // stands — a typo can never fall back to a filesystem lookup.
        // The kernel maps a malformed reference to `OutOfRange`.
        let want = -i64::from(tairix_abi::Errno::OutOfRange.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (number, _) = capture(neg, || {
            assert_eq!(File::open(b"sys:null@", OpenFlags::READ).unwrap_err(), want);
        });
        assert_eq!(number, NUM_RESOURCE_OPEN);
    }

    #[test]
    fn file_open_treats_a_non_utf8_name_as_a_path() {
        // A reference is UTF-8 by construction; raw bytes are a path.
        let path = b"sys:\xffrandom";
        let (number, _) = capture(4, || {
            let file = File::open(path, OpenFlags::READ).expect("armed open must succeed");
            core::mem::forget(file);
        });
        assert_eq!(number, NUM_FS_OPEN);
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
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
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
        let want = -i64::from(tairix_abi::Errno::BufferTooSmall.as_i32());
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
            assert_eq!(fs_unlink(path, tairix_abi::UnlinkFlags::empty()), 0);
        });
        assert_eq!(number, NUM_FS_UNLINK);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_set_mode_marshals_path_and_mode() {
        let path = b"/Users/me/notes.txt";
        let (number, args) = capture(0, || {
            assert_eq!(fs_set_mode(path, 0o640), 0);
        });
        assert_eq!(number, NUM_FS_SET_MODE);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], 0o640);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn fs_set_owner_marshals_path_uid_and_gid() {
        let path = b"/Users/me/notes.txt";
        let (number, args) = capture(0, || {
            assert_eq!(fs_set_owner(path, 1000, tairix_abi::FS_OWNER_UNCHANGED), 0);
        });
        assert_eq!(number, NUM_FS_SET_OWNER);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], 1000);
        assert_eq!(args[3], u64::from(tairix_abi::FS_OWNER_UNCHANGED));
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_attr_calls_marshal_path_key_and_buffers() {
        let path = b"/Users/me/notes.txt";
        let key = b"user.comment";
        let mut out = [0u8; 16];

        let (number, args) = capture(0, || {
            assert_eq!(fs_attr_get(path, key, &mut out), 0);
        });
        assert_eq!(number, NUM_FS_ATTR_GET);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], key.as_ptr() as usize as u64);
        assert_eq!(args[3], key.len() as u64);
        assert_eq!(args[4], out.as_ptr() as usize as u64);
        assert_eq!(args[5], out.len() as u64);

        let value = b"hi";
        let (number, args) = capture(0, || {
            assert_eq!(fs_attr_set(path, key, value), 0);
        });
        assert_eq!(number, NUM_FS_ATTR_SET);
        assert_eq!(args[4], value.as_ptr() as usize as u64);
        assert_eq!(args[5], value.len() as u64);

        let (number, args) = capture(0, || {
            assert_eq!(fs_attr_list(path, 3, &mut out), 0);
        });
        assert_eq!(number, NUM_FS_ATTR_LIST);
        assert_eq!(args[2], 3);
        assert_eq!(args[3], out.as_ptr() as usize as u64);
        assert_eq!(args[4], out.len() as u64);
        assert_eq!(args[5], 0);

        let (number, args) = capture(0, || {
            assert_eq!(fs_attr_remove(path, key), 0);
        });
        assert_eq!(number, NUM_FS_ATTR_REMOVE);
        assert_eq!(args[2], key.as_ptr() as usize as u64);
        assert_eq!(args[3], key.len() as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_unlink_marshals_the_directory_flag() {
        let path = b"/Users/me/empty";
        let (number, args) = capture(0, || {
            assert_eq!(fs_unlink(path, tairix_abi::UnlinkFlags::DIRECTORY), 0);
        });
        assert_eq!(number, NUM_FS_UNLINK);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(
            args[2],
            u64::from(tairix_abi::UnlinkFlags::DIRECTORY.bits())
        );
        assert_eq!(&args[3..], &[0, 0, 0]);
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
    fn fs_symlink_marshals_the_target_then_the_link() {
        let target = b"../real/name";
        let link = b"/Users/me/alias";
        let (number, args) = capture(0, || {
            assert_eq!(fs_symlink(target, link), 0);
        });
        assert_eq!(number, NUM_FS_SYMLINK);
        assert_eq!(args[0], target.as_ptr() as usize as u64);
        assert_eq!(args[1], target.len() as u64);
        assert_eq!(args[2], link.as_ptr() as usize as u64);
        assert_eq!(args[3], link.len() as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_readlink_marshals_the_path_then_the_output_buffer() {
        let path = b"/Users/me/alias";
        let mut out = [0u8; 32];
        let (number, args) = capture(12, || {
            assert_eq!(fs_readlink(path, &mut out), 12);
        });
        assert_eq!(number, NUM_FS_READLINK);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], out.as_ptr() as usize as u64);
        assert_eq!(args[3], out.len() as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_realpath_marshals_the_path_the_buffer_and_the_mode() {
        let path = b"/Users/me/alias";
        let mut out = [0u8; 64];
        for mode in [
            tairix_abi::RealpathMode::Existing,
            tairix_abi::RealpathMode::Final,
            tairix_abi::RealpathMode::Missing,
        ] {
            let (number, args) = capture(9, || {
                assert_eq!(fs_realpath(path, &mut out, mode), 9);
            });
            assert_eq!(number, NUM_FS_REALPATH);
            assert_eq!(args[0], path.as_ptr() as usize as u64);
            assert_eq!(args[1], path.len() as u64);
            assert_eq!(args[2], out.as_ptr() as usize as u64);
            assert_eq!(args[3], out.len() as u64);
            assert_eq!(args[4], u64::from(mode.as_u32()));
            assert_eq!(&args[5..], &[0]);
        }
    }

    #[test]
    fn fs_realpath_surfaces_negative_errno_encoding() {
        // A prefix of a canonical path names a different node, so an
        // undersized buffer is refused whole rather than truncated.
        let want = -i64::from(tairix_abi::Errno::BufferTooSmall.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let mut out = [0u8; 1];
        let (_, _) = capture(neg, || {
            assert_eq!(
                fs_realpath(
                    b"/Users/me/alias",
                    &mut out,
                    tairix_abi::RealpathMode::Existing
                ),
                want
            );
        });
    }

    #[test]
    fn fs_readlink_surfaces_negative_errno_encoding() {
        // An undersized buffer is refused whole, never truncated.
        let want = -i64::from(tairix_abi::Errno::BufferTooSmall.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let mut out = [0u8; 1];
        let (_, _) = capture(neg, || {
            assert_eq!(fs_readlink(b"/Users/me/alias", &mut out), want);
        });
    }

    #[test]
    fn fs_chdir_marshals_path_pointer_and_len() {
        let path = b"/Users/bob/Documents";
        let (number, args) = capture(0, || {
            assert_eq!(fs_chdir(path), 0);
        });
        assert_eq!(number, NUM_FS_CHDIR);
        assert_eq!(args[0], path.as_ptr() as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_chdir_surfaces_negative_errno_encoding() {
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(fs_chdir(b"/nope"), want);
        });
    }

    #[test]
    fn fs_getcwd_marshals_buffer_pointer_and_len() {
        let mut buf = [0u8; 64];
        let ptr = buf.as_mut_ptr() as usize as u64;
        let (number, args) = capture(11, || {
            assert_eq!(fs_getcwd(&mut buf), Ok(11));
        });
        assert_eq!(number, NUM_FS_GETCWD);
        assert_eq!(args[0], ptr);
        assert_eq!(args[1], 64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_getcwd_surfaces_buffer_too_small() {
        let mut buf = [0u8; 2];
        let want = -i64::from(tairix_abi::Errno::BufferTooSmall.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (_, _) = capture(neg, || {
            assert_eq!(fs_getcwd(&mut buf), Err(want));
        });
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
        let want = -i64::from(tairix_abi::Errno::NotFound.as_i32());
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
            kind: tairix_abi::FileKind::Regular,
            nlink: 1,
            size: 1234,
            allocated: 4096,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            id: tairix_abi::FileId::NONE,
            times: tairix_abi::NodeTimes::default(),
        };
        let mut wire = [0u8; FileStat::WIRE_LEN];
        stat.encode(&mut wire).expect("encode");
        // Arm the seam to report the encoded record by pointing the kernel's
        // copy-out at the test's buffer is not possible here (the host seam
        // records, it does not write), so prove the short-record guard instead.
        let file = File { fd: 9 };
        seam::arm(0); // a zero-length stat result trips the short-record guard
        let want = -i64::from(tairix_abi::Errno::BufferTooSmall.as_i32());
        assert_eq!(file.stat(), Err(want));
        core::mem::forget(file);
    }

    #[test]
    fn create_requests_write_create_truncate() {
        let want = -i64::from(tairix_abi::Errno::NotImplemented.as_i32());
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
        let want = -i64::from(tairix_abi::Errno::NotImplemented.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (number, args) = capture(neg, || {
            assert_eq!(open_dir(b"/System/Logs").map(|_| ()), Err(want));
        });
        assert_eq!(number, NUM_FS_OPEN);
        let flags = OpenFlags::from_bits(u32::try_from(args[2]).expect("flag bits fit u32"))
            .expect("open_dir requests a legal flag combination");
        assert!(flags.contains(OpenFlags::DIRECTORY));
    }

    #[test]
    fn read_fd_to_end_stages_a_whole_chunk_per_syscall() {
        // The regression guard for the desktop's wallpaper read: staging a
        // kilobyte at a time cost one syscall per kilobyte, thousands of them
        // for a wallpaper master. The transfer length the call asks for is what
        // decides that, so it is asserted rather than the bytes returned.
        let want = -i64::from(tairix_abi::Errno::NotImplemented.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (number, args) = capture(neg, || {
            assert_eq!(read_fd_to_end(7, 8 * 1024 * 1024).err(), Some(want));
        });
        assert_eq!(number, NUM_FS_READ);
        assert_eq!(args[0], 7);
        assert_eq!(args[1], 0, "the first read starts at the file's own start");
        assert_eq!(
            args[3],
            u64::try_from(FILE_STREAM_CHUNK).expect("the chunk fits u64")
        );
    }

    #[test]
    fn read_fd_to_end_answers_empty_at_end_of_file() {
        seam::arm(0);
        assert_eq!(read_fd_to_end(3, 4096), Ok(alloc::vec::Vec::new()));
    }

    #[test]
    fn read_fd_to_end_stops_one_chunk_past_the_cap() {
        // Every read reports a full staging buffer, so the loop only ends on
        // the cap: it must terminate, and answer *past* the cap so the caller
        // can tell an oversize document from one that exactly fits.
        seam::arm(u64::try_from(FILE_STREAM_CHUNK).expect("the chunk fits u64"));
        let cap = FILE_STREAM_CHUNK + 1;
        let got = read_fd_to_end(3, cap).expect("a reporting read succeeds");
        assert!(got.len() > cap, "the answer states the oversize");
        assert_eq!(got.len(), 2 * FILE_STREAM_CHUNK);
    }

    #[test]
    fn read_fd_to_end_surfaces_the_read_refusal_unchanged() {
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        seam::arm(u64::from_ne_bytes(want.to_ne_bytes()));
        assert_eq!(read_fd_to_end(3, 4096).err(), Some(want));
    }

    #[test]
    fn read_all_growing_returns_the_exact_bytes_of_a_first_fit() {
        let got = read_all_growing(8, 64, |buf| {
            buf[..3].copy_from_slice(b"abc");
            Ok(3)
        })
        .expect("a fitting read succeeds");
        assert_eq!(got, b"abc");
    }

    #[test]
    fn read_all_growing_doubles_until_the_listing_fits() {
        let too_small = -i64::from(tairix_abi::Errno::BufferTooSmall.as_i32());
        let mut sizes = alloc::vec::Vec::new();
        let got = read_all_growing(4, 64, |buf| {
            sizes.push(buf.len());
            if buf.len() < 10 {
                return Err(too_small);
            }
            buf[..10].copy_from_slice(b"0123456789");
            Ok(10)
        })
        .expect("the grown read succeeds");
        assert_eq!(got, b"0123456789");
        assert_eq!(sizes, [4, 8, 16]);
    }

    #[test]
    fn read_all_growing_gives_up_at_the_ceiling() {
        let too_small = -i64::from(tairix_abi::Errno::BufferTooSmall.as_i32());
        let mut calls = 0;
        let got = read_all_growing(4, 16, |_| {
            calls += 1;
            Err(too_small)
        });
        // 4 → 8 → 16, then the refusal at the ceiling surfaces unchanged.
        assert_eq!(got, Err(too_small));
        assert_eq!(calls, 3);
    }

    #[test]
    fn read_all_growing_surfaces_other_errors_unchanged() {
        let denied = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        let mut calls = 0;
        let got = read_all_growing(4, 64, |_| {
            calls += 1;
            Err(denied)
        });
        assert_eq!(got, Err(denied));
        assert_eq!(calls, 1);
    }

    #[test]
    fn read_all_growing_refuses_an_over_reporting_reader() {
        let want = -i64::from(tairix_abi::Errno::OutOfRange.as_i32());
        assert_eq!(read_all_growing(4, 64, |buf| Ok(buf.len() + 1)), Err(want));
    }

    #[test]
    fn read_all_growing_starts_no_smaller_than_one_byte() {
        // A zero `initial` must not wedge the doubling; the reader still
        // sees a real buffer.
        let got = read_all_growing(0, 8, |buf| {
            assert!(!buf.is_empty());
            buf[0] = b'x';
            Ok(1)
        })
        .expect("a fitting read succeeds");
        assert_eq!(got, b"x");
    }

    #[test]
    fn read_dir_all_propagates_the_open_refusal() {
        let want = -i64::from(tairix_abi::Errno::PermissionDenied.as_i32());
        let neg = u64::from_ne_bytes(want.to_ne_bytes());
        let (number, _) = capture(neg, || {
            assert_eq!(read_dir_all(b"/System/Security").err(), Some(want));
        });
        assert_eq!(number, NUM_FS_OPEN);
    }
}
