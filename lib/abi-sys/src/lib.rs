//! `tairix-abi-sys` — the C-callable `abi-v1` syscall stub runtime.
//!
//! This crate is the implementation behind the generated C header
//! (`include/tairix/tairix_syscall.h`, produced by `cargo xtask c-header`).
//! It exports one `extern "C"` function per `abi-v1` syscall, named
//! `tairix_sys_<name>` (for example `tairix_sys_ipc_send`), each of which marshals
//! its arguments into the per-architecture syscall registers, issues the
//! trap, and returns the kernel's result. A program **not** written in Rust
//! (C first, then any language with a C FFI) links this runtime to reach the
//! TAIRiX kernel.
//!
//! It is the curated `/System/Libraries/` class *System runtime / C ABI*: deliberately minimal — it marshals to the kernel and
//! nothing more — and dynamically linked, so one security update covers every
//! consumer. See `plans/CCOMPAT.md` (stage CC2) for the staged build plan and
//! its security posture.
//!
//! # Not a privileged path
//!
//! These stubs add **no** authority (`plans/CCOMPAT.md`). Every capability check and every input validation happens kernel-side,
//! on the far side of the trap, exactly as for a Rust caller; a C program
//! reaches no syscall it could not reach in Rust and gains nothing by being C.
//! Because the kernel re-validates every argument and fails closed, no
//! argument value passed to a `tairix_sys_*` function can cause undefined
//! behaviour, so the stubs are safe `extern "C"` functions.
//!
//! # Symbol naming
//!
//! Each entry point is pinned to the stable symbol `tairix_sys_<name>` with
//! `#[export_name = …]` so the Rust compiler does not mangle it (`extern "C"`
//! alone fixes only the calling convention, not the symbol name). The Rust
//! item names are free to be idiomatic; only the exported symbol is frozen.
//!
//! # Panic-free boundary
//!
//! An unwind across an `extern "C"` boundary is undefined behaviour, so every
//! entry point is panic-free: each performs only constant-index array writes
//! and infallible integer casts before issuing the trap. Errors are reported
//! as the kernel's `int32_t` `TAIRIX_E_*` codes in the return value, never as a
//! panic.
//!
//! # Targets
//!
//! The user→kernel trap itself lives once, in `tairix-abi-trap`: this crate only marshals each call into register form
//! and hands it to [`tairix_abi_trap::raw_syscall`]. The trap instruction is
//! compiled in only for the three native Tier-1 targets (`x86_64`, `aarch64`,
//! `riscv64`); `wasm32` has no trap instruction and is out of scope for this
//! runtime (`plans/CCOMPAT.md` §1). On the host the entry points still build
//! and link (the marshalling logic is host-tested through the trap crate's
//! injectable `host-seam`), but there is no kernel to service the call.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use core::ffi::c_void;

use tairix_abi::{SyscallNumber, SYSCALL_MAX_ARGS};

use tairix_abi_trap::raw_syscall;

// Syscall numbers, read from the `abi-v1` source of truth so this crate can
// never disagree with the frozen table.
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
const NUM_STREAM_WRITE: u64 = SyscallNumber::STREAM_WRITE.as_u16() as u64;
const NUM_STREAM_READ: u64 = SyscallNumber::STREAM_READ.as_u16() as u64;
const NUM_SPAWN: u64 = SyscallNumber::SPAWN.as_u16() as u64;
const NUM_MEM_MAP: u64 = SyscallNumber::MEM_MAP.as_u16() as u64;
const NUM_MEM_UNMAP: u64 = SyscallNumber::MEM_UNMAP.as_u16() as u64;
const NUM_MEM_PIN: u64 = SyscallNumber::MEM_PIN.as_u16() as u64;
const NUM_MEM_UNPIN: u64 = SyscallNumber::MEM_UNPIN.as_u16() as u64;
const NUM_SIGNAL_INTAKE: u64 = SyscallNumber::SIGNAL_INTAKE.as_u16() as u64;
const NUM_SCHED_SET_REALTIME: u64 = SyscallNumber::SCHED_SET_REALTIME.as_u16() as u64;
const NUM_FILE_MAP: u64 = SyscallNumber::FILE_MAP.as_u16() as u64;
const NUM_FILE_UNMAP: u64 = SyscallNumber::FILE_UNMAP.as_u16() as u64;
const NUM_VOLUME_ATTACH: u64 = SyscallNumber::VOLUME_ATTACH.as_u16() as u64;
const NUM_VOLUME_DETACH: u64 = SyscallNumber::VOLUME_DETACH.as_u16() as u64;
const NUM_WAIT: u64 = SyscallNumber::WAIT.as_u16() as u64;
const NUM_RLIMIT_GET: u64 = SyscallNumber::RLIMIT_GET.as_u16() as u64;
const NUM_RLIMIT_SET: u64 = SyscallNumber::RLIMIT_SET.as_u16() as u64;
const NUM_USERS_DB_READ: u64 = SyscallNumber::USERS_DB_READ.as_u16() as u64;
const NUM_USERS_DB_WAIT: u64 = SyscallNumber::USERS_DB_WAIT.as_u16() as u64;
const NUM_USERS_ADMIN: u64 = SyscallNumber::USERS_ADMIN.as_u16() as u64;
const NUM_CONSOLE_COUNT: u64 = SyscallNumber::CONSOLE_COUNT.as_u16() as u64;
const NUM_STREAM_INPUT_MODE: u64 = SyscallNumber::STREAM_INPUT_MODE.as_u16() as u64;

/// `console_foreground` syscall number (as above).
const NUM_CONSOLE_FOREGROUND: u64 = SyscallNumber::CONSOLE_FOREGROUND.as_u16() as u64;
const NUM_KEY_INJECT: u64 = SyscallNumber::KEY_INJECT.as_u16() as u64;
const NUM_DISPLAY_ACQUIRE: u64 = SyscallNumber::DISPLAY_ACQUIRE.as_u16() as u64;
const NUM_DISPLAY_RELEASE: u64 = SyscallNumber::DISPLAY_RELEASE.as_u16() as u64;
const NUM_KEYBOARD_READ: u64 = SyscallNumber::KEYBOARD_READ.as_u16() as u64;
const NUM_POINTER_INJECT: u64 = SyscallNumber::POINTER_INJECT.as_u16() as u64;
const NUM_POINTER_READ: u64 = SyscallNumber::POINTER_READ.as_u16() as u64;
const NUM_SEAT_SWITCH: u64 = SyscallNumber::SEAT_SWITCH.as_u16() as u64;
const NUM_SEAT_REVOKE: u64 = SyscallNumber::SEAT_REVOKE.as_u16() as u64;
const NUM_MMIO_MAP: u64 = SyscallNumber::MMIO_MAP.as_u16() as u64;
const NUM_DMA_ALLOC: u64 = SyscallNumber::DMA_ALLOC.as_u16() as u64;
const NUM_DMA_FREE: u64 = SyscallNumber::DMA_FREE.as_u16() as u64;
const NUM_RESOURCE_GRANTS: u64 = SyscallNumber::RESOURCE_GRANTS.as_u16() as u64;
const NUM_HW_TREE_READ: u64 = SyscallNumber::HW_TREE_READ.as_u16() as u64;
const NUM_HW_TREE_WAIT: u64 = SyscallNumber::HW_TREE_WAIT.as_u16() as u64;
const NUM_IPC_CALL: u64 = SyscallNumber::IPC_CALL.as_u16() as u64;
const NUM_CALL_CREATE: u64 = SyscallNumber::CALL_CREATE.as_u16() as u64;
const NUM_CALL_RECV: u64 = SyscallNumber::CALL_RECV.as_u16() as u64;
const NUM_CALL_REPLY: u64 = SyscallNumber::CALL_REPLY.as_u16() as u64;
const NUM_LOG_EMIT: u64 = SyscallNumber::LOG_EMIT.as_u16() as u64;
const NUM_HW_EMIT_NODE: u64 = SyscallNumber::HW_EMIT_NODE.as_u16() as u64;
const NUM_HW_REMOVE_NODE: u64 = SyscallNumber::HW_REMOVE_NODE.as_u16() as u64;
const NUM_MSI_ALLOC: u64 = SyscallNumber::MSI_ALLOC.as_u16() as u64;
const NUM_SHM_CREATE: u64 = SyscallNumber::SHM_CREATE.as_u16() as u64;
const NUM_SHM_MAP: u64 = SyscallNumber::SHM_MAP.as_u16() as u64;
const NUM_SHM_UNMAP: u64 = SyscallNumber::SHM_UNMAP.as_u16() as u64;
const NUM_SHM_GRANT: u64 = SyscallNumber::SHM_GRANT.as_u16() as u64;
const NUM_CALL_PEER_SEAT: u64 = SyscallNumber::CALL_PEER_SEAT.as_u16() as u64;
const NUM_WAITSET_CREATE: u64 = SyscallNumber::WAITSET_CREATE.as_u16() as u64;
const NUM_WAITSET_CTL: u64 = SyscallNumber::WAITSET_CTL.as_u16() as u64;
const NUM_WAITSET_WAIT: u64 = SyscallNumber::WAITSET_WAIT.as_u16() as u64;
const NUM_FS_OPEN: u64 = SyscallNumber::FS_OPEN.as_u16() as u64;
const NUM_FS_CLOSE: u64 = SyscallNumber::FS_CLOSE.as_u16() as u64;
const NUM_FS_READ: u64 = SyscallNumber::FS_READ.as_u16() as u64;
const NUM_FS_WRITE: u64 = SyscallNumber::FS_WRITE.as_u16() as u64;
const NUM_FS_READDIR: u64 = SyscallNumber::FS_READDIR.as_u16() as u64;
const NUM_FS_STAT: u64 = SyscallNumber::FS_STAT.as_u16() as u64;
const NUM_FS_TRUNCATE: u64 = SyscallNumber::FS_TRUNCATE.as_u16() as u64;
const NUM_FS_SYNC: u64 = SyscallNumber::FS_SYNC.as_u16() as u64;
const NUM_FS_MKDIR: u64 = SyscallNumber::FS_MKDIR.as_u16() as u64;
const NUM_FS_UNLINK: u64 = SyscallNumber::FS_UNLINK.as_u16() as u64;
const NUM_FS_RENAME: u64 = SyscallNumber::FS_RENAME.as_u16() as u64;
const NUM_FS_SET_MODE: u64 = SyscallNumber::FS_SET_MODE.as_u16() as u64;
const NUM_FS_SET_OWNER: u64 = SyscallNumber::FS_SET_OWNER.as_u16() as u64;
const NUM_FS_ATTR_GET: u64 = SyscallNumber::FS_ATTR_GET.as_u16() as u64;
const NUM_FS_ATTR_SET: u64 = SyscallNumber::FS_ATTR_SET.as_u16() as u64;
const NUM_FS_ATTR_LIST: u64 = SyscallNumber::FS_ATTR_LIST.as_u16() as u64;
const NUM_FS_ATTR_REMOVE: u64 = SyscallNumber::FS_ATTR_REMOVE.as_u16() as u64;
const NUM_PORT_RESOLVE: u64 = SyscallNumber::PORT_RESOLVE.as_u16() as u64;
const NUM_PORT_BIND: u64 = SyscallNumber::PORT_BIND.as_u16() as u64;
const NUM_CALL_PEER_ORIGIN: u64 = SyscallNumber::CALL_PEER_ORIGIN.as_u16() as u64;
const NUM_WALL_TIME_GET: u64 = SyscallNumber::WALL_TIME_GET.as_u16() as u64;
const NUM_WALL_TIME_SET: u64 = SyscallNumber::WALL_TIME_SET.as_u16() as u64;
const NUM_BOOT_ID_GET: u64 = SyscallNumber::BOOT_ID_GET.as_u16() as u64;
const NUM_BOOT_FACTS_GET: u64 = SyscallNumber::BOOT_FACTS_GET.as_u16() as u64;
const NUM_SYSINFO_INTROSPECT: u64 = SyscallNumber::SYSINFO_INTROSPECT.as_u16() as u64;
const NUM_TERMINAL_SIZE: u64 = SyscallNumber::TERMINAL_SIZE.as_u16() as u64;
const NUM_SIGNAL: u64 = SyscallNumber::SIGNAL.as_u16() as u64;
const NUM_FS_CHDIR: u64 = SyscallNumber::FS_CHDIR.as_u16() as u64;
const NUM_FS_GETCWD: u64 = SyscallNumber::FS_GETCWD.as_u16() as u64;
const NUM_RESOURCE_OPEN: u64 = SyscallNumber::RESOURCE_OPEN.as_u16() as u64;
const NUM_SELF_ORIGIN: u64 = SyscallNumber::SELF_ORIGIN.as_u16() as u64;
const NUM_PIPE_CREATE: u64 = SyscallNumber::PIPE_CREATE.as_u16() as u64;
const NUM_FD_GRANT: u64 = SyscallNumber::FD_GRANT.as_u16() as u64;
const NUM_FD_REDEEM: u64 = SyscallNumber::FD_REDEEM.as_u16() as u64;

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

/// Decode the kernel's raw result register as an `Errno`/`int32_t` (`TAIRIX_E_*`)
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
#[export_name = "tairix_sys_yield"]
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
/// fail-closed loop over the terminating syscall, not a busy-wait.
#[export_name = "tairix_sys_exit"]
pub extern "C" fn sys_exit(code: i32) -> ! {
    loop {
        // SAFETY: see `sys_yield`. `exit` consumes the exit code in arg 0.
        unsafe {
            let _ = raw_syscall(NUM_EXIT, [i32_arg(code), 0, 0, 0, 0, 0]);
        }
    }
}

/// `ipc_send`: send `len` bytes at `buf` to endpoint `endpoint`
/// (`SyscallNumber::IPC_SEND`). Returns a `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_ipc_send"]
pub extern "C" fn sys_ipc_send(endpoint: u64, buf: *mut c_void, len: usize) -> i32 {
    // SAFETY: see `sys_yield`. The kernel validates `(buf, len)` against the
    // caller's address space before touching it.
    unsafe {
        ret_i32(raw_syscall(
            NUM_IPC_SEND,
            [endpoint, ptr_arg(buf), len as u64, 0, 0, 0],
        ))
    }
}

/// `ipc_recv`: receive the oldest delivered message from the port
/// `endpoint` this task bound (`SyscallNumber::IPC_RECV`): up to `len`
/// payload bytes are copied into `buf` and the sender's kernel-attested
/// origin record (exactly `TAIRIX_ORIGIN_WIRE_LEN` bytes, snapshotted at
/// send time — never the sender's claim) into `sender_out`, so the
/// receiver authenticates each message's principal. Returns the payload
/// length, or a negative `TAIRIX_E_*` code reinterpreted into the result
/// (the `tairix_sys_stream_read` convention). Only the port's owner may
/// receive; an empty mailbox is the retryable `TAIRIX_E_WOULD_BLOCK`.
#[must_use]
#[export_name = "tairix_sys_ipc_recv"]
pub extern "C" fn sys_ipc_recv(
    endpoint: u64,
    buf: *mut c_void,
    len: usize,
    sender_out: *mut c_void,
) -> u64 {
    // SAFETY: see `sys_ipc_send`. The kernel validates both `(buf, len)`
    // and the origin-sized `sender_out` against the caller's address
    // space before writing.
    unsafe {
        raw_syscall(
            NUM_IPC_RECV,
            [
                endpoint,
                ptr_arg(buf),
                len as u64,
                ptr_arg(sender_out),
                0,
                0,
            ],
        )
    }
}

/// `port_bind`: bind an asynchronous IPC message port owned by the
/// calling task (`SyscallNumber::PORT_BIND`) — the receive half of
/// `tairix_sys_ipc_send`/`tairix_sys_ipc_recv`. `max_payload` and `capacity`
/// are fail-closed bounds the kernel re-checks; a reserved well-known id
/// requires `TAIRIX_CAP_IPC_BIND_PRIVILEGED`, and an id already bound is
/// refused. The port is torn down when its owner exits. Returns a
/// `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_port_bind"]
pub extern "C" fn sys_port_bind(endpoint: u64, max_payload: usize, capacity: usize) -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced; every
    // argument is a plain scalar the kernel validates.
    unsafe {
        ret_i32(raw_syscall(
            NUM_PORT_BIND,
            [endpoint, max_payload as u64, capacity as u64, 0, 0, 0],
        ))
    }
}

/// `cap_query`: report whether the caller holds capability `cap`
/// (`SyscallNumber::CAP_QUERY`). Returns `1` if held, `0` otherwise.
#[must_use]
#[export_name = "tairix_sys_cap_query"]
pub extern "C" fn sys_cap_query(cap: u16) -> u32 {
    // SAFETY: see `sys_yield`.
    unsafe { ret_u32(raw_syscall(NUM_CAP_QUERY, [u64::from(cap), 0, 0, 0, 0, 0])) }
}

/// `cap_delegate`: delegate a (necessarily narrower) capability set described
/// at `request` to the task named by `handle` (`SyscallNumber::CAP_DELEGATE`).
/// Returns a `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_cap_delegate"]
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
/// (`SyscallNumber::CAP_REVOKE`). Returns a `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_cap_revoke"]
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
#[export_name = "tairix_sys_clock_get"]
pub extern "C" fn sys_clock_get() -> u64 {
    // SAFETY: see `sys_yield`.
    unsafe { raw_syscall(NUM_CLOCK_GET, NO_ARGS) }
}

/// `irq_bind`: bind the calling task to hardware interrupt `line`
/// (`SyscallNumber::IRQ_BIND`). Returns the opaque 64-bit `IrqHandle`.
#[must_use]
#[export_name = "tairix_sys_irq_bind"]
pub extern "C" fn sys_irq_bind(line: u32) -> u64 {
    // SAFETY: see `sys_yield`.
    unsafe { raw_syscall(NUM_IRQ_BIND, [u64::from(line), 0, 0, 0, 0, 0]) }
}

/// `irq_wait`: wait up to `timeout_ns` nanoseconds for the interrupt bound to
/// `handle` (`SyscallNumber::IRQ_WAIT`). Returns a `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_irq_wait"]
pub extern "C" fn sys_irq_wait(handle: u64, timeout_ns: u64) -> i32 {
    // SAFETY: see `sys_yield`.
    unsafe { ret_i32(raw_syscall(NUM_IRQ_WAIT, [handle, timeout_ns, 0, 0, 0, 0])) }
}

/// `random_get`: fill `len` bytes at `buf` with random bytes, honouring
/// `flags` (`SyscallNumber::RANDOM_GET`). Returns the number of bytes written.
#[must_use]
#[export_name = "tairix_sys_random_get"]
pub extern "C" fn sys_random_get(buf: *mut c_void, len: usize, flags: u32) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, len)`.
    unsafe {
        raw_syscall(
            NUM_RANDOM_GET,
            [ptr_arg(buf), len as u64, u64::from(flags), 0, 0, 0],
        )
    }
}

/// `stream_write`: write `len` bytes at `buf` to the calling process's
/// standard stream `fd` (`SyscallNumber::STREAM_WRITE`). Returns the number
/// of bytes written.
///
/// The kernel resolves `fd` against the caller's inherited descriptor table — the descriptor, not an ambient device, is the
/// authority — and validates the `(buf, len)` pair against the caller's
/// address space before touching it. A short write (fewer
/// than `len`) is valid, so the caller loops.
#[must_use]
#[export_name = "tairix_sys_stream_write"]
pub extern "C" fn sys_stream_write(fd: u32, buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, len)`.
    unsafe {
        raw_syscall(
            NUM_STREAM_WRITE,
            [u64::from(fd), ptr_arg(buf), len as u64, 0, 0, 0],
        )
    }
}

/// `stream_read`: read up to `len` bytes from the calling process's
/// standard stream `fd` into `buf` (`SyscallNumber::STREAM_READ`). Returns
/// the number of bytes read.
///
/// The kernel resolves `fd` against the caller's inherited descriptor table and validates the `(buf, len)` pair against the
/// caller's address space before writing it. The read
/// counterpart of `stream_write`: a short read (fewer than `len`, possibly
/// zero when no input is pending) is valid, so the caller loops.
/// `timeout_ns` bounds how long a read with no pending input may wait:
/// `0` waits indefinitely, and a non-zero bound fails with
/// `TAIRIX_E_TIMED_OUT` once it elapses with no input.
#[must_use]
#[export_name = "tairix_sys_stream_read"]
pub extern "C" fn sys_stream_read(fd: u32, buf: *mut c_void, len: usize, timeout_ns: u64) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, len)`.
    unsafe {
        raw_syscall(
            NUM_STREAM_READ,
            [u64::from(fd), ptr_arg(buf), len as u64, timeout_ns, 0, 0],
        )
    }
}

/// `spawn`: spawn a new process from the embedded program named by the
/// absolute path `(path, path_len)` (`SyscallNumber::SPAWN`). Returns the
/// new process's PID, or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// Requires `CAP_PROC_SPAWN`; the kernel validates the capability and the
/// `(path, path_len)` pair against the caller's address space before
/// reading it. The caller keeps running — this is a
/// true concurrent spawn, not an `exec`-style hand-off (`plans/SPAWN.md`
/// SP3).
///
/// `(attach, attach_len)` optionally carry the child's *attach block*: a
/// non-null `attach` names an encoded `SpawnAttach` block (`plans/SPAWN.md`
/// SP10) selecting the child's credential (`TAIRIX_SPAWN_UID_INHERIT` keeps
/// the caller's own; a concrete uid requires `TAIRIX_CAP_SPAWN_AS_USER` —
/// there is no setuid-self), the console its base descriptor table comes
/// from (`TAIRIX_CONSOLE_INHERIT` keeps the caller's own table; any other
/// value names an installed console index, see `tairix_sys_console_count`),
/// and one wire per standard descriptor — wiring the child's fd 0/1/2/3
/// onto pre-opened files, resources, or pipe ends of the caller's own open
/// table, each owner-checked fail-closed. Pass NULL and `0` for "no
/// block": full inherit (the caller's own credential and table).
///
/// `(strings, strings_len)` optionally carry the child's startup strings: a
/// non-null `strings` names an encoded `tairix_process_start_*` startup-vector
/// block (the `PSV1` format) holding the argument vector and environment the
/// caller chose for the child. The kernel bounds, stages, and parses the
/// block fail-closed; the strings are data and grant nothing, and the kernel
/// mints the child's stack canary itself, ignoring the block's. Pass NULL
/// and `0` for "no block": the child then receives the program's registered
/// default arguments and an empty environment.
#[must_use]
#[export_name = "tairix_sys_spawn"]
pub extern "C" fn sys_spawn(
    path: *mut c_void,
    path_len: usize,
    attach: *mut c_void,
    attach_len: usize,
    strings: *mut c_void,
    strings_len: usize,
) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(path, path_len)`,
    // the optional `(attach, attach_len)` block (parsing it fail-closed
    // and owner-checking every named handle), and the optional
    // `(strings, strings_len)` block.
    unsafe {
        raw_syscall(
            NUM_SPAWN,
            [
                ptr_arg(path),
                path_len as u64,
                ptr_arg(attach),
                attach_len as u64,
                ptr_arg(strings),
                strings_len as u64,
            ],
        )
    }
}

/// `pipe_create`: create a pipe — a bounded, kernel-buffered unidirectional
/// byte stream — and write its two new descriptors through `out` (the read
/// end first, then the write end, two `uint32_t`s)
/// (`SyscallNumber::PIPE_CREATE`, `plans/SPAWN.md` SP10). Returns a
/// `TAIRIX_E_*` code.
///
/// Unprivileged: both descriptors land in the caller's own open table (the
/// same number space `tairix_sys_fs_open` allocates from) and are read,
/// written, and closed through `tairix_sys_fs_read` / `tairix_sys_fs_write` /
/// `tairix_sys_fs_close` (a pipe ignores the file offset). A read on an empty
/// pipe blocks until bytes arrive or every write end is closed (then
/// end-of-stream, `0`); a write to a full pipe blocks until space frees,
/// and a write with no reader left fails closed with `TAIRIX_E_BROKEN_PIPE`.
/// An end is handed to a child by naming its descriptor in the spawn
/// attach block (`tairix_sys_spawn`).
#[must_use]
#[export_name = "tairix_sys_pipe_create"]
pub extern "C" fn sys_pipe_create(out: *mut c_void) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `out` against the
    // caller's address space before writing the two descriptors.
    unsafe { ret_i32(raw_syscall(NUM_PIPE_CREATE, [ptr_arg(out), 0, 0, 0, 0, 0])) }
}

/// `console_count`: report how many system text consoles are installed
/// (`SyscallNumber::CONSOLE_COUNT`). Returns the count,
/// or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// Gated kernel-side on `TAIRIX_CAP_CONSOLE_WRITE`. The count is the index
/// space `tairix_sys_spawn`'s `console` argument selects from — each entry
/// is an independent text console with its own session context
/// (`plans/PI.md` P11).
#[must_use]
#[export_name = "tairix_sys_console_count"]
pub extern "C" fn sys_console_count() -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here.
    unsafe { raw_syscall(NUM_CONSOLE_COUNT, [0, 0, 0, 0, 0, 0]) }
}

/// `stream_input_mode`: set the console read line discipline of the input
/// stream `fd` (`SyscallNumber::STREAM_INPUT_MODE`). `mode` is a
/// `tairix_input_mode_t` discriminant: `1` (cooked — echo on, the interactive
/// default), `2` (secret — echo off, the activity indicator shown instead),
/// or `3` (raw — echo off, nothing drawn; a full-screen program paints its
/// own display). The reserved `0` and every unknown value fail closed.
/// Returns a `TAIRIX_E_*` code.
///
/// Gated kernel-side on `TAIRIX_CAP_CONSOLE_READ`; the kernel performs the
/// echo/indicator itself as part of the read line discipline, so no
/// `TAIRIX_CAP_CONSOLE_WRITE` is needed. A program that changes the mode
/// restores cooked before it exits, so the next program on the console sees
/// the interactive default.
#[must_use]
#[export_name = "tairix_sys_stream_input_mode"]
pub extern "C" fn sys_stream_input_mode(fd: u32, mode: u32) -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here.
    unsafe {
        ret_i32(raw_syscall(
            NUM_STREAM_INPUT_MODE,
            [u64::from(fd), u64::from(mode), 0, 0, 0, 0],
        ))
    }
}

/// `key_inject`: inject one decoded keyboard key edge at `buf` (a
/// `tairix_key_input_t` record of `len` bytes) for seat `seat` into the kernel
/// input-focus arbiter (`SyscallNumber::KEY_INJECT`, `plans/PI.md`
/// P11 — input follows the surface owner). Returns the number of bytes
/// consumed, or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// The producer-side call a keyboard-input driver issues after decoding a
/// directly attached keyboard into a key edge; `seat` names the seat the
/// keyboard belongs to (`0` for the boot seat) and an unknown id is
/// refused with `TAIRIX_E_NOT_FOUND`. Gated kernel-side on
/// `TAIRIX_CAP_INPUT_INJECT`; the kernel validates the capability and the
/// `(buf, len)` pair against the caller's address space before reading it, decodes the record fail-closed, and routes it by who
/// currently holds that seat — the driver no longer chooses the encoding
/// or the destination.
#[must_use]
#[export_name = "tairix_sys_key_inject"]
pub extern "C" fn sys_key_inject(seat: u64, buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`. The kernel validates `CAP_INPUT_INJECT`,
    // the seat id, and the `(buf, len)` pair against the caller's address
    // space before reading it.
    unsafe { raw_syscall(NUM_KEY_INJECT, [seat, ptr_arg(buf), len as u64, 0, 0, 0]) }
}

/// `display_acquire`: acquire ownership of seat `seat` — one display with
/// its keyboard — as an exclusive, owner-tracked lease
/// (`SyscallNumber::DISPLAY_ACQUIRE`, `plans/DISPLAY.md`). Returns the
/// minted lease's generation (`>= 1`) when non-negative, else a negative
/// `TAIRIX_E_*` code.
///
/// The compositing window manager calls this when it takes over a screen
/// (`0` for the boot seat; further seats are minted per discovered display
/// node): the kernel records the calling task as that seat's owner, so key
/// edges injected for the seat are delivered as records the owner drains
/// with [`sys_keyboard_read`], and the returned generation is the client's
/// lease handle — the present right is derived from it, so a stale
/// pre-revoke handle can never be mistaken for the live grant
/// (`plans/DISPLAY.md` D4). An unknown seat id is refused with
/// `TAIRIX_E_NOT_FOUND`, a seat held by another task refuses the claim
/// (`TAIRIX_E_SEAT_BUSY` — ownership is never displaced), and a repeat acquire
/// by the holder is refused (`TAIRIX_E_ALREADY_EXISTS`). Gated kernel-side on
/// `TAIRIX_CAP_DISPLAY` (owning a seat is privileged).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // The kernel guarantees the i64 generation-or-errno encoding (generation >= 1, else -errno).
#[export_name = "tairix_sys_display_acquire"]
pub extern "C" fn sys_display_acquire(seat: u64) -> i64 {
    // SAFETY: see `sys_yield`. The call carries no pointers; the kernel
    // validates `CAP_DISPLAY` before touching any state.
    let ret = unsafe { raw_syscall(NUM_DISPLAY_ACQUIRE, [seat, 0, 0, 0, 0, 0]) };
    ret as i64
}

/// `display_release`: release seat `seat` and return its keyboard input to
/// the text console (`SyscallNumber::DISPLAY_RELEASE`, `plans/DISPLAY.md`). Returns a `TAIRIX_E_*` code (`0` on
/// success).
///
/// The inverse of [`sys_display_acquire`]; gated kernel-side on
/// `TAIRIX_CAP_DISPLAY` and owner-checked: an unknown seat id is refused with
/// `TAIRIX_E_NOT_FOUND` and a caller that does not hold the seat is refused
/// (`TAIRIX_E_SEAT_NOT_OWNER`; `TAIRIX_E_SEAT_REVOKED` once, after an
/// administrative eviction) rather than flipping the seat out from under
/// its owner.
#[must_use]
#[export_name = "tairix_sys_display_release"]
pub extern "C" fn sys_display_release(seat: u64) -> i32 {
    // SAFETY: see `sys_yield`. The call carries no pointers; the kernel
    // validates `CAP_DISPLAY` before touching any state.
    unsafe { ret_i32(raw_syscall(NUM_DISPLAY_RELEASE, [seat, 0, 0, 0, 0, 0])) }
}

/// `seat_switch`: switch a seat's foreground session — retarget which
/// installed text console an unowned seat's input drains to
/// (`SyscallNumber::SEAT_SWITCH`, `plans/DISPLAY.md` D3). Returns a
/// `TAIRIX_E_*` code (`0` on success).
///
/// The seat manager calls this to move the foreground across sessions —
/// the `chvt` analogue. Gated kernel-side on `TAIRIX_CAP_SEAT_ADMIN` (the
/// seat-multiplexing authority); an unknown seat id or console index is
/// refused with `TAIRIX_E_NOT_FOUND` before any state changes, and every
/// switch is audit-logged.
#[must_use]
#[export_name = "tairix_sys_seat_switch"]
pub extern "C" fn sys_seat_switch(seat_id: u64, console: u32) -> i32 {
    // SAFETY: see `sys_yield`. The call carries no pointers; the kernel
    // validates `CAP_SEAT_ADMIN` and both indices before touching state.
    unsafe {
        ret_i32(raw_syscall(
            NUM_SEAT_SWITCH,
            [seat_id, u64::from(console), 0, 0, 0, 0],
        ))
    }
}

/// `seat_revoke`: forcibly revoke a seat's current lease — evict a wedged
/// or switched-away owner (`SyscallNumber::SEAT_REVOKE`,
/// `plans/DISPLAY.md` D3). Returns a `TAIRIX_E_*` code (`0` on success).
///
/// Gated kernel-side on `TAIRIX_CAP_SEAT_ADMIN`. An unknown seat is refused
/// with `TAIRIX_E_NOT_FOUND`, an unowned seat with `TAIRIX_E_SEAT_NOT_OWNER`
/// (there is no lease to revoke), and every eviction is audit-logged with
/// the evicted owner's task id. The evicted owner's next owner-gated call
/// fails closed with the distinct `TAIRIX_E_SEAT_REVOKED`.
#[must_use]
#[export_name = "tairix_sys_seat_revoke"]
pub extern "C" fn sys_seat_revoke(seat_id: u64) -> i32 {
    // SAFETY: see `sys_yield`. The call carries no pointers; the kernel
    // validates `CAP_SEAT_ADMIN` and the seat id before touching state.
    unsafe { ret_i32(raw_syscall(NUM_SEAT_REVOKE, [seat_id, 0, 0, 0, 0, 0])) }
}

/// `keyboard_read`: read one decoded keyboard event from seat `seat`'s
/// keyboard channel into `buf` (a buffer of `len` bytes, at least one
/// `tairix_key_input_t` record) (`SyscallNumber::KEYBOARD_READ`, `plans/PI.md` P11). Returns the number of bytes written — one
/// record, or `0` when the channel is momentarily drained — or a `TAIRIX_E_*`
/// code reinterpreted into the result.
///
/// The task that owns the seat (the window manager) drains the
/// records the kernel routed to it while it held the seat. An unknown seat
/// id is refused with `TAIRIX_E_NOT_FOUND`. Gated
/// kernel-side on `TAIRIX_CAP_INPUT_READ` **and** owner-gated against that
/// seat's live lease (a non-owner is refused with `TAIRIX_E_SEAT_NOT_OWNER`
/// / `TAIRIX_E_SEAT_REVOKED`); the kernel validates the capability and the
/// `(buf, len)` pair against the caller's address space before writing it, and a buffer too small to hold a record fails closed.
#[must_use]
#[export_name = "tairix_sys_keyboard_read"]
pub extern "C" fn sys_keyboard_read(seat: u64, buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`. The kernel validates `CAP_INPUT_READ`,
    // the seat id, and the `(buf, len)` pair against the caller's address
    // space before writing it.
    unsafe { raw_syscall(NUM_KEYBOARD_READ, [seat, ptr_arg(buf), len as u64, 0, 0, 0]) }
}

/// `pointer_inject`: inject one decoded pointer event at `buf` (a
/// `tairix_pointer_input_t` record of `len` bytes) for seat `seat` into the
/// kernel seat registry (`SyscallNumber::POINTER_INJECT`, `plans/PI.md`
/// P11 — the pointer analogue of [`sys_key_inject`]). Returns the number
/// of bytes consumed, or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// The producer-side call a pointer-input driver issues after decoding a
/// discovered pointing device into an event; `seat` names the seat the
/// device belongs to (`0` for the boot seat) and an unknown id is refused
/// with `TAIRIX_E_NOT_FOUND`. Gated kernel-side on `TAIRIX_CAP_INPUT_INJECT`;
/// the kernel validates the capability and the `(buf, len)` pair against
/// the caller's address space before reading it, decodes the record
/// fail-closed, and routes it by who currently holds that seat — a held
/// seat's pointer channel, or consumed and discarded while unowned (the
/// text console has no pointer consumer). The driver never chooses the
/// destination.
#[must_use]
#[export_name = "tairix_sys_pointer_inject"]
pub extern "C" fn sys_pointer_inject(seat: u64, buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`. The kernel validates `CAP_INPUT_INJECT`,
    // the seat id, and the `(buf, len)` pair against the caller's address
    // space before reading it.
    unsafe {
        raw_syscall(
            NUM_POINTER_INJECT,
            [seat, ptr_arg(buf), len as u64, 0, 0, 0],
        )
    }
}

/// `pointer_read`: read one decoded pointer event from seat `seat`'s
/// pointer channel into `buf` (a buffer of `len` bytes, at least one
/// `tairix_pointer_input_t` record) (`SyscallNumber::POINTER_READ`,
/// `plans/PI.md` P11 — the pointer analogue of [`sys_keyboard_read`]).
/// Returns the number of bytes written — one record, or `0` when the
/// channel is momentarily drained — or a `TAIRIX_E_*` code reinterpreted into
/// the result.
///
/// The task that owns the seat (the window manager) drains the records the
/// kernel routed to it while it held the seat. An unknown seat id is
/// refused with `TAIRIX_E_NOT_FOUND`. Gated kernel-side on
/// `TAIRIX_CAP_INPUT_READ` **and** owner-gated against that seat's live lease
/// (a non-owner is refused with `TAIRIX_E_SEAT_NOT_OWNER` /
/// `TAIRIX_E_SEAT_REVOKED`); the kernel validates the capability and the
/// `(buf, len)` pair against the caller's address space before writing it,
/// and a buffer too small to hold a record fails closed.
#[must_use]
#[export_name = "tairix_sys_pointer_read"]
pub extern "C" fn sys_pointer_read(seat: u64, buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`. The kernel validates `CAP_INPUT_READ`,
    // the seat id, and the `(buf, len)` pair against the caller's address
    // space before writing it.
    unsafe { raw_syscall(NUM_POINTER_READ, [seat, ptr_arg(buf), len as u64, 0, 0, 0]) }
}

/// `resource_grants`: enumerate the device-resource grants the kernel minted
/// for the calling driver task into `buf` (a buffer of `len` bytes)
/// (`SyscallNumber::RESOURCE_GRANTS`,
/// `plans/PI.md` P10 chunk 5d-2). Returns the total number of bytes written
/// — consecutive `tairix_granted_resource` records — or a `TAIRIX_E_*` code
/// reinterpreted into the result.
///
/// A driver process calls this once at start-up to learn the unforgeable
/// handles it passes to [`sys_mmio_map`] / [`sys_dma_alloc`]. It needs no
/// capability (a task reads only its *own* grants); the kernel validates the
/// `(buf, len)` pair against the caller's address space before writing it, and a buffer too small for the whole grant set fails
/// closed.
#[must_use]
#[export_name = "tairix_sys_resource_grants"]
pub extern "C" fn sys_resource_grants(buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`. The kernel validates the `(buf, len)` pair
    // against the caller's address space before writing it.
    unsafe { raw_syscall(NUM_RESOURCE_GRANTS, [ptr_arg(buf), len as u64, 0, 0, 0, 0]) }
}

/// `mmio_map`: map the `[offset, offset + len)` sub-region of a granted
/// device MMIO register window into the calling driver's own address space
/// (`SyscallNumber::MMIO_MAP`, `plans/PI.md` P10
/// chunk 5d-0). Returns the base virtual address of the mapped sub-region,
/// or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// `handle` is an unforgeable, kernel-issued device-resource grant the driver
/// received for the hardware-tree node it binds — never a raw physical
/// address — and `[offset, offset + len)` names the sub-region of that grant
/// to map. The kernel resolves the handle against the calling task, confirms
/// it names a memory window and the sub-region lies wholly inside it, and
/// maps only that sub-region (caching disabled); a forged/non-owned handle, a
/// wrong-kind grant, a sub-region escaping the grant, or a build with no map
/// facility wired fails closed. Mapping a bounded
/// sub-region lets a driver granted a large outbound bus aperture map just
/// the single BAR it enumerated, not the whole window.
/// Gated kernel-side on `TAIRIX_CAP_MMIO_MAP`.
#[must_use]
#[export_name = "tairix_sys_mmio_map"]
pub extern "C" fn sys_mmio_map(handle: u64, offset: u64, len: usize) -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here; the
    // kernel resolves the grant handle against the caller and returns the
    // mapped sub-region's base virtual address.
    unsafe { raw_syscall(NUM_MMIO_MAP, [handle, offset, len as u64, 0, 0, 0]) }
}

/// `dma_alloc`: carve a coherent DMA buffer for the calling driver, bounded
/// by a granted device DMA constraint (`SyscallNumber::DMA_ALLOC`, `plans/PI.md` P10 chunk 5d-0). Writes the
/// buffer's device-visible base address to `device_out` and returns the base
/// virtual address of the mapping, or a `TAIRIX_E_*` code reinterpreted into the
/// result.
///
/// `handle` is an unforgeable, kernel-issued device-resource grant the driver
/// received for the hardware-tree node it binds — never a raw physical
/// address. The kernel resolves it against the calling task, confirms it
/// names a DMA constraint, carves a physically-contiguous, zeroed, coherent
/// region of `len` bytes whose physical extent lies within the grant's
/// addressing limit, and maps it `RW`, non-executable, into the caller's own
/// address space; a forged/non-owned handle, a wrong-kind grant, an
/// over-limit request, or a build with no DMA facility wired fails closed. Gated kernel-side on `TAIRIX_CAP_MEM_DMA`.
#[must_use]
#[export_name = "tairix_sys_dma_alloc"]
pub extern "C" fn sys_dma_alloc(handle: u64, len: usize, device_out: *mut c_void) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `device_out`
    // pointer against the caller's address space before writing the
    // device-visible base to it.
    unsafe {
        raw_syscall(
            NUM_DMA_ALLOC,
            [handle, len as u64, ptr_arg(device_out), 0, 0, 0],
        )
    }
}

/// `dma_free`: release a coherent DMA buffer previously carved by
/// [`sys_dma_alloc`] (`SyscallNumber::DMA_FREE`) — the symmetric free a
/// long-running driver calls so each transfer's bounce buffers are reclaimed
/// rather than leaked until the process exits. Returns a `TAIRIX_E_*` code.
///
/// `handle` is the same unforgeable, kernel-issued DMA-constraint grant the
/// matching [`sys_dma_alloc`] used, and `cpu_va` is the base virtual address
/// that `dma_alloc` returned. The kernel resolves the handle against the
/// calling task, confirms it names a DMA constraint, and releases the buffer
/// based at `cpu_va` from the caller's own address space, zeroing every
/// backing byte before the frames return to the allocator; a forged/non-owned
/// handle, a `cpu_va` that is not the base of a live carve, or a build with no
/// DMA facility wired fails closed. Gated kernel-side on `TAIRIX_CAP_MEM_DMA`.
#[must_use]
#[export_name = "tairix_sys_dma_free"]
pub extern "C" fn sys_dma_free(handle: u64, cpu_va: u64) -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here; `cpu_va`
    // is an opaque lookup key the kernel resolves against the caller's own
    // DMA window before releasing the carve.
    unsafe { ret_i32(raw_syscall(NUM_DMA_FREE, [handle, cpu_va, 0, 0, 0, 0])) }
}

/// `mem_map`: map `len` bytes of fresh anonymous `RW` memory into the
/// calling process's own address space, honouring `flags`
/// ([`tairix_abi::MapFlags`]) and the placement hint `addr_hint`
/// (`SyscallNumber::MEM_MAP`). Returns the base address of the new region.
///
/// The kernel validates every argument and fails closed;
/// the region is zeroed before it is visible and is never executable. An out-of-memory condition is reported as a
/// `TAIRIX_E_*` code reinterpreted into the result (`plans/SPAWN.md` SP5).
#[must_use]
#[export_name = "tairix_sys_mem_map"]
pub extern "C" fn sys_mem_map(len: usize, flags: u32, addr_hint: u64) -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here; the
    // kernel maps the region and returns its base.
    unsafe {
        raw_syscall(
            NUM_MEM_MAP,
            [len as u64, u64::from(flags), addr_hint, 0, 0, 0],
        )
    }
}

/// `mem_unmap`: release the region of `len` bytes based at `base` previously
/// returned by [`sys_mem_map`] from the calling process's own address space
/// (`SyscallNumber::MEM_UNMAP`). Returns a `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_mem_unmap"]
pub extern "C" fn sys_mem_unmap(base: u64, len: usize) -> i32 {
    // SAFETY: see `sys_yield`. The kernel validates the `(base, len)` range
    // against the caller's address space before unmapping it.
    unsafe { ret_i32(raw_syscall(NUM_MEM_UNMAP, [base, len as u64, 0, 0, 0, 0])) }
}

/// `mem_pin`: mark the calling process's entire anonymous memory — current
/// and future — as pinned, ineligible for the compressed `ramzip` tier and
/// any future lower swap tier (`SyscallNumber::MEM_PIN`,
/// `plans/STRESSTEST.md` ST2). Returns a `TAIRIX_E_*` code.
///
/// Gated kernel-side on `TAIRIX_CAP_MEM_PIN` and bounded by the caller's
/// effective pinned-memory limit; both refusals fail closed. Already
/// pinned is success. The pin is not inherited across spawn and is
/// cleared on exit.
#[must_use]
#[export_name = "tairix_sys_mem_pin"]
pub extern "C" fn sys_mem_pin() -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here; the
    // kernel checks the capability and the bound on the far side.
    unsafe { ret_i32(raw_syscall(NUM_MEM_PIN, [0; 6])) }
}

/// `mem_unpin`: clear the calling process's [`sys_mem_pin`] mark,
/// restoring its anonymous memory's eligibility for the swap tiers
/// (`SyscallNumber::MEM_UNPIN`). Returns a `TAIRIX_E_*` code.
///
/// Unprivileged — releasing the caller's own exemption grants nothing —
/// and idempotent: already unpinned is success.
#[must_use]
#[export_name = "tairix_sys_mem_unpin"]
pub extern "C" fn sys_mem_unpin() -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here; the
    // kernel only clears the caller's own pin mark.
    unsafe { ret_i32(raw_syscall(NUM_MEM_UNPIN, [0; 6])) }
}

/// `signal_intake`: operate on the calling process's own signal intake —
/// the fail-closed signal-observation opt-in (`SyscallNumber::SIGNAL_INTAKE`,
/// `plans/STRESSTEST.md` ST3). `op` is a `TAIRIX_SIGNAL_INTAKE_OP_*`
/// discriminant: enable (0) opts `Interrupt`/`Terminate` into observable
/// delivery, disable (1) restores the default terminate disposition, and
/// take (2) drains the one pending observed signal. Returns the
/// non-negative op result (`0`, or take's drained `TAIRIX_SIGNAL_*`
/// discriminant); an error is reported as a `TAIRIX_E_*` code reinterpreted
/// into the result (`TAIRIX_E_WOULD_BLOCK` for a take with nothing pending or
/// a disable with an undrained observation — a recorded termination
/// request is never silently discarded — and `TAIRIX_E_NOT_FOUND` for a take
/// without the opt-in).
///
/// Kernel-side the pending event is waitable through a wait-set member of
/// kind signal (id `0`); `TAIRIX_SIGNAL_KILL` is never observable or
/// maskable, and a second termination request while one is pending
/// undrained escalates to the default terminate path, so an opted-in
/// process that stops draining stays killable.
#[must_use]
#[export_name = "tairix_sys_signal_intake"]
pub extern "C" fn sys_signal_intake(op: u32) -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here; the
    // kernel validates the op and acts only on the caller's own intake.
    unsafe { raw_syscall(NUM_SIGNAL_INTAKE, [u64::from(op), 0, 0, 0, 0, 0]) }
}

/// `sched_set_realtime`: set the calling task's scheduling class — enter
/// (`realtime` non-zero) or leave (zero) the strict-priority real-time band
/// (`SyscallNumber::SCHED_SET_REALTIME`, `plans/USB.md`). Returns a
/// `TAIRIX_E_*` code.
///
/// A real-time task is dispatched ahead of every time-shared task on its
/// CPU and is never preempted by one, so a CPU-bound workload cannot delay
/// its wake — the guarantee an interrupt-serving driver needs. Gated
/// kernel-side on `TAIRIX_CAP_SCHED_REALTIME` in both directions and acts
/// only on the caller's own task; setting the class the task already holds
/// is success.
#[must_use]
#[export_name = "tairix_sys_sched_set_realtime"]
pub extern "C" fn sys_sched_set_realtime(realtime: u32) -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here; the
    // kernel checks the capability and reclasses only the caller's task.
    unsafe {
        ret_i32(raw_syscall(
            NUM_SCHED_SET_REALTIME,
            [u64::from(realtime), 0, 0, 0, 0, 0],
        ))
    }
}

/// `file_map`: map `len` bytes of the open, readable, filesystem-backed
/// file `fd`, starting at the page-aligned file byte `offset`, into the
/// calling process's own address space as a demand-paged, **read-only**
/// private mapping (`SyscallNumber::FILE_MAP` — the `mmap(2)` shape).
/// Returns the base address of the new region.
///
/// No page is read at call time: the kernel backs each page on first
/// access under the mapping-time identity, so a mapping may exceed RAM by
/// orders of magnitude. Touching a page wholly at/past end-of-file
/// terminates the process (the `SIGBUS` analogue); bound accesses by the
/// file size. The mapping is never writable and never executable. An
/// error is reported as a `TAIRIX_E_*` code reinterpreted into the result.
#[must_use]
#[export_name = "tairix_sys_file_map"]
pub extern "C" fn sys_file_map(fd: u32, offset: u64, len: u64) -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here; the
    // kernel reserves the region and returns its base.
    unsafe { raw_syscall(NUM_FILE_MAP, [u64::from(fd), offset, len, 0, 0, 0]) }
}

/// `file_unmap`: release the whole file mapping of `len` bytes based at
/// `base` previously returned by [`sys_file_map`] from the calling
/// process's own address space (`SyscallNumber::FILE_UNMAP`). Returns a
/// `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_file_unmap"]
pub extern "C" fn sys_file_unmap(base: u64, len: u64) -> i32 {
    // SAFETY: see `sys_yield`. The kernel validates the `(base, len)` pair
    // against the caller's own recorded mappings before any teardown.
    unsafe { ret_i32(raw_syscall(NUM_FILE_UNMAP, [base, len, 0, 0, 0, 0])) }
}

/// `volume_attach`: attach a filesystem driver to a runtime block source
/// and publish the volume's root (`SyscallNumber::VOLUME_ATTACH`).
/// `request`/`request_len` name an encoded volume-attach request frame;
/// requires `CAP_FS_MOUNT`. Returns a `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_volume_attach"]
pub extern "C" fn sys_volume_attach(request: *const u8, request_len: usize) -> i32 {
    // SAFETY: see `sys_yield`. The kernel validates the `(request,
    // request_len)` pair against the caller's address space before reading
    // it; a bad pointer is refused, never dereferenced here.
    unsafe {
        ret_i32(raw_syscall(
            NUM_VOLUME_ATTACH,
            [request as u64, request_len as u64, 0, 0, 0, 0],
        ))
    }
}

/// `volume_detach`: flush, unmount, and unpublish a runtime-attached
/// volume (`SyscallNumber::VOLUME_DETACH`). `request`/`request_len` name
/// an encoded volume-detach request frame (the volume's stable 16-byte
/// identity); requires `CAP_FS_MOUNT`. Returns a `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_volume_detach"]
pub extern "C" fn sys_volume_detach(request: *const u8, request_len: usize) -> i32 {
    // SAFETY: see `sys_yield`. The kernel validates the `(request,
    // request_len)` pair against the caller's address space before reading
    // it; a bad pointer is refused, never dereferenced here.
    unsafe {
        ret_i32(raw_syscall(
            NUM_VOLUME_DETACH,
            [request as u64, request_len as u64, 0, 0, 0, 0],
        ))
    }
}

/// `wait`: wait for a child-process event, writing the typed
/// `tairix_wait_status_t` record to `status` (`SyscallNumber::WAIT`). Returns
/// the reported child's PID, or a `TAIRIX_E_*` code reinterpreted into the
/// result.
///
/// `pid` is either a specific child's PID or [`tairix_abi::WAIT_PID_ANY`] to
/// wait for any child. `flags` is a [`tairix_abi::WaitFlags`] bit set:
/// `TAIRIX_WAIT_FLAG_NONBLOCK` (bit 0) polls instead of blocking, returning
/// `TAIRIX_E_WOULD_BLOCK` when a matching child has nothing to report;
/// `TAIRIX_WAIT_FLAG_STOPPED` (bit 1) also reports a child freshly stopped by
/// `TAIRIX_SIGNAL_STOP` — without reaping it. A process may
/// only wait on its **own** children; the kernel validates the parent/child
/// relationship and the `status` pointer before writing to it, and fails
/// closed (`plans/SPAWN.md` SP6/SP9).
#[must_use]
#[export_name = "tairix_sys_wait"]
pub extern "C" fn sys_wait(pid: i32, status: *mut c_void, flags: u32) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `status` pointer
    // against the caller's address space before writing the exit code to it.
    unsafe {
        raw_syscall(
            NUM_WAIT,
            [i32_arg(pid), ptr_arg(status), u64::from(flags), 0, 0, 0],
        )
    }
}

/// `rlimit_get`: read the calling process's effective limit for resource
/// `kind`, writing the encoded `tairix_resource_limit_t` to `out`
/// (`SyscallNumber::RLIMIT_GET`). Returns a `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_rlimit_get"]
pub extern "C" fn sys_rlimit_get(kind: u32, out: *mut c_void) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `out` pointer
    // against the caller's address space before writing the limit to it.
    unsafe {
        ret_i32(raw_syscall(
            NUM_RLIMIT_GET,
            [u64::from(kind), ptr_arg(out), 0, 0, 0, 0],
        ))
    }
}

/// `rlimit_set`: install the calling process's limit for resource `kind`
/// from the encoded `tairix_resource_limit_t` at `value`
/// (`SyscallNumber::RLIMIT_SET`). Returns a `TAIRIX_E_*` code; raising a hard
/// bound requires `CAP_RLIMIT_RAISE`.
#[must_use]
#[export_name = "tairix_sys_rlimit_set"]
pub extern "C" fn sys_rlimit_set(kind: u32, value: *mut c_void) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `value` pointer
    // against the caller's address space before reading the limit from it.
    unsafe {
        ret_i32(raw_syscall(
            NUM_RLIMIT_SET,
            [u64::from(kind), ptr_arg(value), 0, 0, 0, 0],
        ))
    }
}

/// `users_db_read`: copy the system user database the kernel loaded at boot
/// (`/System/Security/Users`, exact `users-v1` text) into the caller's
/// `(buf, len)` buffer (`SyscallNumber::USERS_DB_READ`). Returns the byte
/// count, or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// Gated kernel-side on `TAIRIX_CAP_USERS_READ` — only the authentication
/// principal (login) holds it. A buffer smaller
/// than the database is refused whole (`TAIRIX_E_BUFFER_TOO_SMALL`) — a
/// credential database is never truncated; sizing the
/// buffer at the format's 64 KiB maximum always suffices.
#[must_use]
#[export_name = "tairix_sys_users_db_read"]
pub extern "C" fn sys_users_db_read(buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `(buf, len)` pair
    // against the caller's address space before writing the text to it.
    unsafe { raw_syscall(NUM_USERS_DB_READ, [ptr_arg(buf), len as u64, 0, 0, 0, 0]) }
}

/// `users_admin`: apply one typed user/group administration request
/// (`SyscallNumber::USERS_ADMIN`). `req`/`req_len` carry one versioned
/// `users_admin` request record; `out`/`out_cap` receive a list
/// operation's response (mutating operations write nothing). Returns
/// the response byte count (`0` for a mutating operation), or a
/// `TAIRIX_E_*` code reinterpreted into the result.
///
/// Gated kernel-side on `TAIRIX_CAP_USER_ADMIN` — the account-
/// administration authority — with the finer never-widen /
/// last-administrator / format rules enforced in the kernel engine; the
/// stub adds no authority. Password material crosses only as a ready
/// salted PBKDF2 record built by the caller, and no operation ever
/// returns stored password material.
#[must_use]
#[export_name = "tairix_sys_users_admin"]
pub extern "C" fn sys_users_admin(
    req: *mut c_void,
    req_len: usize,
    out: *mut c_void,
    out_cap: usize,
) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates both `(ptr, len)`
    // pairs against the caller's address space before touching them.
    unsafe {
        raw_syscall(
            NUM_USERS_ADMIN,
            [
                ptr_arg(req),
                req_len as u64,
                ptr_arg(out),
                out_cap as u64,
                0,
                0,
            ],
        )
    }
}

/// `hw_tree_read`: copy the discovered hardware tree the kernel built at
/// boot into the caller's `(buf, len)` buffer
/// (`SyscallNumber::HW_TREE_READ`).
/// Returns the byte count, or a `TAIRIX_E_*` code reinterpreted into the
/// result.
///
/// The bytes are a `tairix_hw_tree_header_t` (the store's current generation
/// and node count) followed by that many `tairix_hw_node_t` records. The
/// generation in the header is the value to pass to `tairix_sys_hw_tree_wait`
/// to block until the tree next changes. Gated kernel-side on
/// `TAIRIX_CAP_SYSINFO_HW` — the privileged global hardware view. The whole inventory is copied or none: a
/// buffer smaller than the snapshot is refused with `TAIRIX_E_BUFFER_TOO_SMALL`
/// rather than truncated, so the caller grows `buf` and
/// retries.
#[must_use]
#[export_name = "tairix_sys_hw_tree_read"]
pub extern "C" fn sys_hw_tree_read(buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `(buf, len)` pair
    // against the caller's address space before writing the tree to it.
    unsafe { raw_syscall(NUM_HW_TREE_READ, [ptr_arg(buf), len as u64, 0, 0, 0, 0]) }
}

/// `users_db_wait`: block until the system user database leaves its pending
/// (still-being-unlocked) state (`SyscallNumber::USERS_DB_WAIT`) — the blocking companion to `tairix_sys_users_db_read`.
/// Returns a `TAIRIX_E_*` code.
///
/// `timeout_ns` bounds the wait (`UINT64_MAX` for an effectively unbounded
/// block). Returns `0` once the database is no longer pending (the caller
/// re-reads it with `tairix_sys_users_db_read`), or `TAIRIX_E_TIMED_OUT` if the
/// deadline elapses first. Gated kernel-side on `TAIRIX_CAP_USERS_READ`, the
/// same privilege as reading the database; a build with no users-database
/// service wired is never pending, so the wait returns `0` immediately.
#[must_use]
#[export_name = "tairix_sys_users_db_wait"]
pub extern "C" fn sys_users_db_wait(timeout_ns: u64) -> i32 {
    // SAFETY: see `sys_yield`. The single argument is a scalar; the call
    // reads no caller memory.
    unsafe { ret_i32(raw_syscall(NUM_USERS_DB_WAIT, [timeout_ns, 0, 0, 0, 0, 0])) }
}

/// `hw_tree_wait`: block until the discovered hardware tree changes past
/// `last_generation` (`SyscallNumber::HW_TREE_WAIT` —
/// reactive re-match and hotplug). Returns a `TAIRIX_E_*` code.
///
/// `last_generation` is the generation last observed through
/// `tairix_sys_hw_tree_read`'s header; `timeout_ns` bounds the wait
/// (`UINT64_MAX` for an effectively unbounded block). Returns `0` once the
/// tree has changed, `TAIRIX_E_TIMED_OUT` if the deadline elapses first, or
/// `TAIRIX_E_NOT_IMPLEMENTED` if no hardware-tree store is wired. Gated
/// kernel-side on `TAIRIX_CAP_SYSINFO_HW`, the same privilege as reading the
/// tree.
#[must_use]
#[export_name = "tairix_sys_hw_tree_wait"]
pub extern "C" fn sys_hw_tree_wait(last_generation: u64, timeout_ns: u64) -> i32 {
    // SAFETY: see `sys_yield`. Both arguments are scalars; the call reads no
    // caller memory.
    unsafe {
        ret_i32(raw_syscall(
            NUM_HW_TREE_WAIT,
            [last_generation, timeout_ns, 0, 0, 0, 0],
        ))
    }
}

/// `ipc_call`: make a synchronous capability-checked call to the kernel-owned
/// IPC call endpoint `endpoint` — post `request_len` bytes at `request`,
/// block until the reply arrives, and copy it into the `reply_cap`-byte
/// buffer at `reply` (`SyscallNumber::IPC_CALL`).
/// Returns the number of reply bytes written, or a `TAIRIX_E_*` code
/// reinterpreted into the result.
///
/// The kernel enforces the endpoint's required send capability against the
/// caller before posting (no ambient authority), copies
/// both buffers through the validated boundary, and blocks the caller
/// cooperatively until the reply arrives, never busy-spinning. A reply larger
/// than `reply_cap` fails closed with `TAIRIX_E_BUFFER_TOO_SMALL`; a missing
/// send capability, an unknown or destroyed endpoint, or no call-endpoint
/// registry wired each fail closed.
#[must_use]
#[export_name = "tairix_sys_ipc_call"]
pub extern "C" fn sys_ipc_call(
    endpoint: u64,
    request: *mut c_void,
    request_len: usize,
    reply: *mut c_void,
    reply_cap: usize,
) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates both `(ptr, len)` pairs
    // against the caller's address space before touching them.
    unsafe {
        raw_syscall(
            NUM_IPC_CALL,
            [
                endpoint,
                ptr_arg(request),
                request_len as u64,
                ptr_arg(reply),
                reply_cap as u64,
                0,
            ],
        )
    }
}

/// `call_create`: create and register a kernel-owned synchronous call
/// endpoint the calling task then serves (`SyscallNumber::CALL_CREATE`; the server half of `tairix_sys_ipc_call`).
///
/// `send_caps` and `recv_caps` each point at a 32-byte `tairix_capability_set_t`
/// wire image (the capability a caller must hold to post, and the capability
/// this task must hold to receive/reply). Binding a restricted-sender
/// endpoint requires `TAIRIX_CAP_IPC_BIND_PRIVILEGED`. Returns a `TAIRIX_E_*` code
/// (`0` on success).
#[must_use]
#[export_name = "tairix_sys_call_create"]
pub extern "C" fn sys_call_create(
    endpoint: u64,
    send_caps: *mut c_void,
    recv_caps: *mut c_void,
    max_request: usize,
    max_reply: usize,
    capacity: usize,
) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates both `CapabilitySet`
    // pointers against the caller's address space before reading them.
    unsafe {
        ret_i32(raw_syscall(
            NUM_CALL_CREATE,
            [
                endpoint,
                ptr_arg(send_caps),
                ptr_arg(recv_caps),
                max_request as u64,
                max_reply as u64,
                capacity as u64,
            ],
        ))
    }
}

/// `call_recv`: receive the next request posted to an endpoint this task owns,
/// blocking until one arrives (`SyscallNumber::CALL_RECV`). The request is
/// copied into the `buf_cap`-byte buffer at `buf`, the per-call ticket is
/// written to `*ticket_out`, and the request byte count is returned (or a
/// `TAIRIX_E_*` code reinterpreted into the result). `flags` carries the
/// `CallRecvFlags` bits: `0` blocks until a request arrives; the
/// non-blocking bit makes an empty queue return `TAIRIX_E_WOULD_BLOCK`
/// instead of parking (reserved bits are rejected fail-closed).
#[must_use]
#[export_name = "tairix_sys_call_recv"]
pub extern "C" fn sys_call_recv(
    endpoint: u64,
    buf: *mut c_void,
    buf_cap: usize,
    ticket_out: *mut c_void,
    flags: u32,
) -> u64 {
    // SAFETY: see `sys_ipc_call`; the kernel validates both pointers against
    // the caller's address space before touching them.
    unsafe {
        raw_syscall(
            NUM_CALL_RECV,
            [
                endpoint,
                ptr_arg(buf),
                buf_cap as u64,
                ptr_arg(ticket_out),
                u64::from(flags),
                0,
            ],
        )
    }
}

/// `call_reply`: answer one received call on an endpoint this task owns,
/// releasing the blocked caller (`SyscallNumber::CALL_REPLY`). `ticket` is the
/// value `tairix_sys_call_recv` wrote; `reply_len` bytes at `reply` are the reply
/// payload. Returns a `TAIRIX_E_*` code (`0` on success).
#[must_use]
#[export_name = "tairix_sys_call_reply"]
pub extern "C" fn sys_call_reply(
    endpoint: u64,
    ticket: u64,
    reply: *mut c_void,
    reply_len: usize,
) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the reply `(ptr, len)`
    // pair against the caller's address space before reading it.
    unsafe {
        ret_i32(raw_syscall(
            NUM_CALL_REPLY,
            [endpoint, ticket, ptr_arg(reply), reply_len as u64, 0, 0],
        ))
    }
}

/// `call_peer_origin`: read the kernel-attested `tairix_abi::Origin` of the
/// caller whose in-service call this server is handling
/// (`SyscallNumber::CALL_PEER_ORIGIN`). `ticket` is the value
/// `tairix_sys_call_recv` wrote; the caller's attested origin wire image is
/// written to the `origin_cap`-byte buffer at `origin` and its byte count
/// returned (or a `TAIRIX_E_*` code reinterpreted into the result). The origin is
/// filled by the kernel from the posting task's own state and cannot be
/// forged.
#[must_use]
#[export_name = "tairix_sys_call_peer_origin"]
pub extern "C" fn sys_call_peer_origin(
    endpoint: u64,
    ticket: u64,
    origin: *mut c_void,
    origin_cap: usize,
) -> u64 {
    // SAFETY: see `sys_call_recv`; the kernel validates the origin `(ptr, len)`
    // pair against the caller's address space before writing it.
    unsafe {
        raw_syscall(
            NUM_CALL_PEER_ORIGIN,
            [endpoint, ticket, ptr_arg(origin), origin_cap as u64, 0, 0],
        )
    }
}

/// `wall_time_get`: read the kernel wall-clock time and its provenance state
/// (`SyscallNumber::WALL_TIME_GET`). The current `tairix_abi::WallClockReading`
/// wire image (a `tairix_time64_t` instant plus a one-byte `WallTimeState`) is
/// written to the `out_cap`-byte buffer at `out` and its byte count returned
/// (or a `TAIRIX_E_*` code reinterpreted into the result). Unprivileged, like
/// `tairix_sys_clock_get`; before a trusted source sets it the reading is the
/// Unix epoch tagged `Unset`.
#[must_use]
#[export_name = "tairix_sys_wall_time_get"]
pub extern "C" fn sys_wall_time_get(out: *mut c_void, out_cap: usize) -> u64 {
    // SAFETY: see `sys_call_peer_origin`; the kernel validates the `(ptr, len)`
    // pair against the caller's address space before writing it.
    unsafe {
        raw_syscall(
            NUM_WALL_TIME_GET,
            [ptr_arg(out), out_cap as u64, 0, 0, 0, 0],
        )
    }
}

/// `wall_time_set`: set the kernel wall-clock time from a trusted source
/// (`SyscallNumber::WALL_TIME_SET`). `time` points at a little-endian
/// `tairix_time64_t` of `time_len` bytes; `state` is the `WallTimeState`
/// discriminant to record (`Firmware`/`Trusted`/`Adjusted` — `Unset` is
/// rejected). Requires `TAIRIX_CAP_TIME_SET`; the monotonic clock is unaffected.
/// Returns a `TAIRIX_E_*` code (`0` on success).
#[must_use]
#[export_name = "tairix_sys_wall_time_set"]
pub extern "C" fn sys_wall_time_set(time: *mut c_void, time_len: usize, state: u32) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `(ptr, len)` pair
    // against the caller's address space before reading it.
    unsafe {
        ret_i32(raw_syscall(
            NUM_WALL_TIME_SET,
            [ptr_arg(time), time_len as u64, u64::from(state), 0, 0, 0],
        ))
    }
}

/// `boot_id_get`: read the kernel's per-boot identifier
/// (`SyscallNumber::BOOT_ID_GET`). The 16-byte `tairix_abi::BootId` minted
/// for this boot is written to the `out_cap`-byte buffer at `out` and its
/// byte count returned (or a `TAIRIX_E_*` code reinterpreted into the result).
/// Unprivileged, like `tairix_sys_clock_get` — the boot id is a public per-boot
/// nonce, not a secret; if the kernel's random subsystem was not seeded in
/// time the call fails closed with `TAIRIX_E_ENTROPY_NOT_READY` rather than
/// return the all-zero sentinel.
#[must_use]
#[export_name = "tairix_sys_boot_id_get"]
pub extern "C" fn sys_boot_id_get(out: *mut c_void, out_cap: usize) -> u64 {
    // SAFETY: see `sys_wall_time_get`; the kernel validates the `(ptr, len)`
    // pair against the caller's address space before writing it.
    unsafe { raw_syscall(NUM_BOOT_ID_GET, [ptr_arg(out), out_cap as u64, 0, 0, 0, 0]) }
}

/// `boot_facts_get`: read the kernel's boot-static machine summary
/// (`SyscallNumber::BOOT_FACTS_GET`). The 64-byte `tairix_abi::BootFacts`
/// wire image — CPU architecture, the boot CPU's discovered model name,
/// processor-core count, and installed physical memory, minted once at
/// boot from kernel-attested state — is
/// written to the `out_cap`-byte buffer at `out` and its byte count returned
/// (or a `TAIRIX_E_*` code reinterpreted into the result). Unprivileged, like
/// `tairix_sys_boot_id_get` — the facts are the machine's public shape, never
/// live state or a secret. An undersized buffer fails closed with
/// `TAIRIX_E_BUFFER_TOO_SMALL`; a boot path that never installed the facts
/// fails closed with `TAIRIX_E_NOT_IMPLEMENTED`.
#[must_use]
#[export_name = "tairix_sys_boot_facts_get"]
pub extern "C" fn sys_boot_facts_get(out: *mut c_void, out_cap: usize) -> u64 {
    // SAFETY: see `sys_boot_id_get`; the kernel validates the `(out, out_cap)`
    // pair against the caller's address space before writing it.
    unsafe {
        raw_syscall(
            NUM_BOOT_FACTS_GET,
            [ptr_arg(out), out_cap as u64, 0, 0, 0, 0],
        )
    }
}

/// `self_origin`: read the calling task's own kernel-attested
/// `tairix_abi::Origin` (`SyscallNumber::SELF_ORIGIN`). The wire image is
/// written to the `out_cap`-byte buffer at `out` and its byte count returned
/// (or a `TAIRIX_E_*` code reinterpreted into the result). Unprivileged, like
/// `tairix_sys_boot_id_get` — a task may always learn its own identity, and the
/// origin is built from the caller's own kernel-held task record so it cannot
/// be forged. An undersized buffer fails closed with `TAIRIX_E_BUFFER_TOO_SMALL`.
#[must_use]
#[export_name = "tairix_sys_self_origin"]
pub extern "C" fn sys_self_origin(out: *mut c_void, out_cap: usize) -> u64 {
    // SAFETY: see `sys_boot_id_get`; the kernel validates the `(out, out_cap)`
    // pair against the caller's address space before writing it.
    unsafe { raw_syscall(NUM_SELF_ORIGIN, [ptr_arg(out), out_cap as u64, 0, 0, 0, 0]) }
}

/// `sysinfo_introspect`: read the unfiltered, global kernel introspection
/// view (`SyscallNumber::SYSINFO_INTROSPECT`). `domain` is a
/// `tairix_abi::IntrospectDomain` discriminant, `arg` is the domain-specific
/// selector (a record offset for the paged domains), and the encoded records
/// are written to the `out_cap`-byte buffer at `out`, whose byte count is
/// returned (or a `TAIRIX_E_*` code reinterpreted into the result). For the
/// per-task-limits domain the 16-byte target `tairix_abi::ProcId` is supplied
/// in `out` on entry.
///
/// Requires `TAIRIX_CAP_SYSINFO_INTROSPECT`, held only by the `sysinfod` broker:
/// the kernel returns the whole system's state and never narrows by principal.
/// The whole answer or none — an undersized buffer fails closed with
/// `TAIRIX_E_BUFFER_TOO_SMALL`.
#[must_use]
#[export_name = "tairix_sys_sysinfo_introspect"]
pub extern "C" fn sys_sysinfo_introspect(
    domain: u32,
    arg: u64,
    out: *mut c_void,
    out_cap: usize,
) -> u64 {
    // SAFETY: see `sys_boot_id_get`; the kernel validates the `(out, out_cap)`
    // pair against the caller's address space before reading the target id on
    // entry and writing the encoded answer.
    unsafe {
        raw_syscall(
            NUM_SYSINFO_INTROSPECT,
            [u64::from(domain), arg, ptr_arg(out), out_cap as u64, 0, 0],
        )
    }
}

/// `terminal_size`: read the character-cell geometry of the text console
/// backing standard stream `fd` (`SyscallNumber::TERMINAL_SIZE`). The encoded
/// `tairix_abi::TerminalSize` (two little-endian `u16`s: rows, then columns)
/// is written to the `out_cap`-byte buffer at `out` and its byte count
/// returned (or a `TAIRIX_E_*` code reinterpreted into the result).
///
/// Unprivileged, like `tairix_sys_clock_get` — a program may always ask how big
/// its own terminal is. The kernel reports a size only for a console whose
/// grid it actually knows (a framebuffer text console); for a byte-stream
/// console (a UART), whose remote-terminal size the kernel cannot attest, the
/// call fails closed with `TAIRIX_E_NOT_IMPLEMENTED` and the caller applies the
/// conventional fallback — the kernel never fabricates a size.
#[must_use]
#[export_name = "tairix_sys_terminal_size"]
pub extern "C" fn sys_terminal_size(fd: u32, out: *mut c_void, out_cap: usize) -> u64 {
    // SAFETY: see `sys_boot_id_get`; the kernel validates the `(out, out_cap)`
    // pair against the caller's address space before writing it.
    unsafe {
        raw_syscall(
            NUM_TERMINAL_SIZE,
            [u64::from(fd), ptr_arg(out), out_cap as u64, 0, 0, 0],
        )
    }
}

/// `log_emit`: emit one encoded diagnostic record (a `tairix_abi::log`
/// `LogRecord` wire image of `len` bytes at `record`) to the kernel's
/// diagnostic log sink (`SyscallNumber::LOG_EMIT`).
/// Requires `TAIRIX_CAP_LOG_EMIT`; the kernel validates and attributes the
/// record to the calling task. Returns a `TAIRIX_E_*` code (`0` on success).
#[must_use]
#[export_name = "tairix_sys_log_emit"]
pub extern "C" fn sys_log_emit(record: *mut c_void, len: usize) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the record `(ptr, len)`
    // pair against the caller's address space before reading it.
    unsafe {
        ret_i32(raw_syscall(
            NUM_LOG_EMIT,
            [ptr_arg(record), len as u64, 0, 0, 0, 0],
        ))
    }
}

/// `hw_emit_node`: publish one wire-encoded `tairix_abi::HwNode` (`len` bytes
/// at `node`) into the live hardware tree (`SyscallNumber::HW_EMIT_NODE`). A user-space bus driver calls this for each
/// device it enumerates so the device manager autoloads the matching driver.
/// Requires `TAIRIX_CAP_HW_EMIT`; the kernel decodes and validates the node and
/// admits it only when every resource it requests is covered by one of the
/// calling driver's own grants. Returns a `TAIRIX_E_*` code (`0` on success).
#[must_use]
#[export_name = "tairix_sys_hw_emit_node"]
pub extern "C" fn sys_hw_emit_node(node: *mut c_void, len: usize) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the node `(ptr, len)`
    // pair against the caller's address space before reading it.
    unsafe {
        ret_i32(raw_syscall(
            NUM_HW_EMIT_NODE,
            [ptr_arg(node), len as u64, 0, 0, 0, 0],
        ))
    }
}

/// `hw_remove_node`: remove the previously-published child node `node_id` —
/// and its whole subtree — from the live hardware tree
/// (`SyscallNumber::HW_REMOVE_NODE`). The symmetric
/// counterpart of `tairix_sys_hw_emit_node`: a user-space bus driver calls it
/// when a device it published goes away, so the device manager unloads the
/// driver bound to the vanished node. Requires `TAIRIX_CAP_HW_EMIT`; the kernel
/// retires the node only when it is a child the caller itself published
/// (no ambient authority). Returns a `TAIRIX_E_*` code (`0` on
/// success).
#[must_use]
#[export_name = "tairix_sys_hw_remove_node"]
pub extern "C" fn sys_hw_remove_node(node_id: u64) -> i32 {
    // SAFETY: `raw_syscall` is always safe to invoke; the kernel validates
    // `CAP_HW_EMIT` and resolves `node_id` against the live tree on the far
    // side of the trap. No memory operand is passed.
    unsafe { ret_i32(raw_syscall(NUM_HW_REMOVE_NODE, [node_id, 0, 0, 0, 0, 0])) }
}

/// `msi_alloc`: allocate a message-signalled interrupt (MSI) vector for a PCI
/// function and write the encoded `tairix_abi::MsiAllocation` (the virtual
/// interrupt line plus the doorbell address/data to program into the
/// function's MSI capability) into `out` (a buffer of `len` bytes)
/// (`SyscallNumber::MSI_ALLOC`). Returns the number of bytes written, or a
/// `TAIRIX_E_*` code reinterpreted into the result.
///
/// A user-space bus driver wiring a PCI function for MSI calls this; it is
/// gated kernel-side on `TAIRIX_CAP_IRQ_BIND` (the same privilege the driver
/// needs to `irq_bind` the returned line). The kernel grants the caller a
/// device resource for the line, so it may both bind it and forward it as an
/// IRQ resource onto a child node it publishes (no ambient authority); a
/// platform with no MSI controller fails closed.
#[must_use]
#[export_name = "tairix_sys_msi_alloc"]
pub extern "C" fn sys_msi_alloc(out: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_resource_grants`. The kernel validates the `(out, len)`
    // pair against the caller's address space before writing the encoded
    // allocation.
    unsafe { raw_syscall(NUM_MSI_ALLOC, [ptr_arg(out), len as u64, 0, 0, 0, 0]) }
}

/// `shm_create`: allocate a cross-process shared-memory region of `len`
/// bytes (rounded up to whole pages), map it into the calling process's own
/// address space, and write the new region's kernel-allocated, unforgeable
/// id to `id_out` (`SyscallNumber::SHM_CREATE`). Returns the base **user
/// virtual address** the region is mapped at (`RW`, non-executable,
/// cacheable, guard-bracketed), or a `TAIRIX_E_*` code reinterpreted into the
/// result.
///
/// The kernel zeroes the region before it is visible (no cross-process
/// leak), records the caller as its owner, and grants the caller the matching
/// per-region device resource so it may forward the region onto a child node
/// it publishes — never ambient authority. Gated kernel-side on `TAIRIX_CAP_SHM`;
/// a zero length, frame exhaustion, or a build with no shared-memory facility
/// fails closed.
#[must_use]
#[export_name = "tairix_sys_shm_create"]
pub extern "C" fn sys_shm_create(len: usize, id_out: *mut c_void) -> u64 {
    // SAFETY: see `sys_dma_alloc`; the kernel validates the `id_out` pointer
    // against the caller's address space before writing the region id to it.
    unsafe { raw_syscall(NUM_SHM_CREATE, [len as u64, ptr_arg(id_out), 0, 0, 0, 0]) }
}

/// `shm_map`: map a shared-memory region the kernel has **granted** the
/// calling task into its own address space (`SyscallNumber::SHM_MAP`).
/// Returns the base **user virtual address** the region is mapped at, or a
/// `TAIRIX_E_*` code reinterpreted into the result. On success the region's
/// byte length — the kernel's own record, never the granting task's claim —
/// is written to `len_out`; it is left untouched on failure.
///
/// `handle` is an unforgeable, kernel-issued device-resource grant the driver
/// received for the matched hardware-tree node it binds. The kernel resolves
/// it against the calling task, confirms it names a shared region, and maps
/// that region's existing frames into the caller's own address space; a
/// forged/non-owned handle, a wrong-kind grant, a torn-down region, or a build
/// with no shared-memory facility fails closed. Gated kernel-side on
/// `TAIRIX_CAP_SHM`.
#[must_use]
#[export_name = "tairix_sys_shm_map"]
pub extern "C" fn sys_shm_map(handle: u64, len_out: *mut c_void) -> u64 {
    // SAFETY: see `sys_dma_alloc`; the kernel validates the `len_out` pointer
    // against the caller's address space before writing the region's byte
    // length to it.
    unsafe { raw_syscall(NUM_SHM_MAP, [handle, ptr_arg(len_out), 0, 0, 0, 0]) }
}

/// `shm_unmap`: release the shared-memory mapping of `len` bytes based at
/// `base` the calling task established with [`sys_shm_create`] or
/// [`sys_shm_map`] (`SyscallNumber::SHM_UNMAP`). Returns a `TAIRIX_E_*` code.
///
/// The kernel validates the `(base, len)` names a shared mapping of the
/// calling task, tears down only that mapping's page-table entries, and drops
/// the caller's reference to the region; the region's frames are zeroed and
/// freed when the owner and every grantee have released it. Needs no
/// capability (the `mem_unmap` posture). A `(base, len)` that does not name a
/// live shared mapping of the caller fails closed.
#[must_use]
#[export_name = "tairix_sys_shm_unmap"]
pub extern "C" fn sys_shm_unmap(base: u64, len: usize) -> i32 {
    // SAFETY: see `sys_yield`. The kernel validates the `(base, len)` range
    // against the caller's address space before unmapping it.
    unsafe { ret_i32(raw_syscall(NUM_SHM_UNMAP, [base, len as u64, 0, 0, 0, 0])) }
}

/// `shm_grant`: grant the serving task of call endpoint `endpoint` the right
/// to map the shared-memory region `region` the caller owns
/// (`SyscallNumber::SHM_GRANT`). Returns the minted, unforgeable grant
/// handle (>= 1), or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// The kernel requires `TAIRIX_CAP_SHM`, confirms the caller itself holds a
/// grant covering the region (delegation never widens authority), and
/// resolves the recipient as the endpoint's live serving task at grant
/// time — never a caller-supplied PID. The caller forwards the handle
/// in-band; it resolves only through the recipient's own
/// [`sys_shm_map`], so the number is useless to a bystander. Audited.
#[must_use]
#[export_name = "tairix_sys_shm_grant"]
pub extern "C" fn sys_shm_grant(region: u64, endpoint: u64) -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced; the kernel
    // validates the capability, the caller's own region grant, and the
    // endpoint before minting anything.
    unsafe { raw_syscall(NUM_SHM_GRANT, [region, endpoint, 0, 0, 0, 0]) }
}

/// `call_peer_seat`: ask whether the in-flight caller of served call
/// endpoint `endpoint` (ticket `ticket`, the value `tairix_sys_call_recv`
/// wrote) holds seat `seat`'s live lease
/// (`SyscallNumber::CALL_PEER_SEAT`). Returns the live lease generation
/// (>= 1), or a `TAIRIX_E_*` code reinterpreted into the result
/// (`TAIRIX_E_SEAT_NOT_OWNER`, `TAIRIX_E_SEAT_REVOKED`, `TAIRIX_E_NOT_FOUND`,
/// `TAIRIX_E_PERMISSION_DENIED`).
///
/// Valid only between `tairix_sys_call_recv` and `tairix_sys_call_reply` on an
/// endpoint the caller owns and may receive from — the
/// `tairix_sys_call_peer_origin` window — so a server learns seat facts only
/// about a task it is actively servicing. The kernel reads the seat's
/// live lease at check time; a revocation between two frames refuses the
/// very next present.
#[must_use]
#[export_name = "tairix_sys_call_peer_seat"]
pub extern "C" fn sys_call_peer_seat(endpoint: u64, ticket: u64, seat: u64) -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced; the kernel
    // gates the call on endpoint ownership + receive capability before
    // reading any seat state.
    unsafe { raw_syscall(NUM_CALL_PEER_SEAT, [endpoint, ticket, seat, 0, 0, 0]) }
}

/// `waitset_create`: create a caller-owned wait-set that multiplexes the
/// readiness of several event sources (`SyscallNumber::WAITSET_CREATE`).
/// Returns the kernel-minted, opaque wait-set handle, or a `TAIRIX_E_*` code
/// reinterpreted into the result.
///
/// Takes no arguments and needs no capability: the set observes only resources
/// the caller already holds, each owner-checked when added. Members are
/// registered with [`sys_waitset_ctl`] and waited on with [`sys_waitset_wait`].
#[must_use]
#[export_name = "tairix_sys_waitset_create"]
pub extern "C" fn sys_waitset_create() -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is involved; the kernel mints a
    // handle for the calling task and returns it.
    unsafe { raw_syscall(NUM_WAITSET_CREATE, NO_ARGS) }
}

/// `waitset_ctl`: add or remove a member of wait-set `set`
/// (`SyscallNumber::WAITSET_CTL`). Returns a `TAIRIX_E_*` code.
///
/// `op` is a `TAIRIX_WAITSET_OP_*` value (`Add` / `Del`); `kind` is a
/// `TAIRIX_WAIT_SOURCE_*` value (`Endpoint` / `Irq` / `Child` / `SeatInput`);
/// `id` names the resource (an IPC call-endpoint id the caller serves, an
/// `IrqHandle` the caller bound, a child PID or the any-child sentinel, or a
/// seat id whose live lease the caller holds via `display_acquire` — the
/// seat member is ready on queued keyboard/pointer input *and* on losing
/// the lease, so a revocation is observed rather than parked through);
/// `token` is the caller's opaque tag reported back by [`sys_waitset_wait`]. On
/// `Add` the kernel resolves and owner-checks the named resource against the
/// calling task before recording it — never ambient authority; a resource the
/// caller does not own, a handle that is not the caller's own wait-set, an
/// unknown `op`/`kind`, or a duplicate/absent member fails closed.
#[must_use]
#[export_name = "tairix_sys_waitset_ctl"]
pub extern "C" fn sys_waitset_ctl(set: u64, op: u32, kind: u32, id: u64, token: u64) -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced; the kernel
    // validates the set handle and the named resource against the caller.
    unsafe {
        ret_i32(raw_syscall(
            NUM_WAITSET_CTL,
            [set, u64::from(op), u64::from(kind), id, token, 0],
        ))
    }
}

/// `waitset_wait`: block until any one member of wait-set `set` is ready,
/// writing the ready member's caller-chosen token to `token_out`
/// (`SyscallNumber::WAITSET_WAIT`). Returns a `TAIRIX_E_*` code (`0` on a ready
/// member, `TAIRIX_E_TIMED_OUT` when `timeout_ns` elapses first).
///
/// `timeout_ns` is a relative timeout, or `UINT64_MAX` for "no timeout". The
/// caller parks off the run queue between readiness checks — woken by an IPC
/// post to a member endpoint, a member IRQ firing, or the timeout — so an idle
/// service burns no CPU. Needs no capability.
#[must_use]
#[export_name = "tairix_sys_waitset_wait"]
pub extern "C" fn sys_waitset_wait(set: u64, timeout_ns: u64, token_out: *mut c_void) -> i32 {
    // SAFETY: see `sys_yield`. The kernel validates `token_out` against the
    // caller's address space before writing the ready member's token to it.
    unsafe {
        ret_i32(raw_syscall(
            NUM_WAITSET_WAIT,
            [set, timeout_ns, ptr_arg(token_out), 0, 0, 0],
        ))
    }
}

/// `fs_open`: open the file or directory at the absolute path
/// `(path, path_len)` (`SyscallNumber::FS_OPEN`). Returns a new per-process
/// file descriptor (at or above `TAIRIX_STD_STREAM_COUNT`), or a `TAIRIX_E_*` code
/// reinterpreted into the result.
///
/// `flags` is the `TAIRIX_OPEN_*` bit set ([`tairix_abi::OpenFlags`]). Requires
/// `TAIRIX_CAP_FS_ACCESS`; the kernel validates the capability and the
/// `(path, path_len)` pair against the caller's address space, then resolves
/// the path under the caller's real credentials so every per-inode and
/// mount-flag check stays kernel-side.
#[must_use]
#[export_name = "tairix_sys_fs_open"]
pub extern "C" fn sys_fs_open(path: *mut c_void, path_len: usize, flags: u32) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(path, path_len)`.
    unsafe {
        raw_syscall(
            NUM_FS_OPEN,
            [ptr_arg(path), path_len as u64, u64::from(flags), 0, 0, 0],
        )
    }
}

/// `fs_close`: release the open descriptor `fd` (`SyscallNumber::FS_CLOSE`).
/// Returns a `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_fs_close"]
pub extern "C" fn sys_fs_close(fd: u32) -> i32 {
    // SAFETY: see `sys_yield`. The kernel resolves `fd` against the caller's
    // descriptor table.
    unsafe { ret_i32(raw_syscall(NUM_FS_CLOSE, [u64::from(fd), 0, 0, 0, 0, 0])) }
}

/// `fs_read`: read up to `len` bytes from open file `fd` at byte `offset`
/// into `buf` (`SyscallNumber::FS_READ`). Returns the number of bytes read
/// (`0` at end of file), or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// The kernel resolves `fd`, re-authorises the read through the secured VFS,
/// and validates `(buf, len)` against the caller's address space before
/// writing it.
#[must_use]
#[export_name = "tairix_sys_fs_read"]
pub extern "C" fn sys_fs_read(fd: u32, offset: u64, buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, len)`.
    unsafe {
        raw_syscall(
            NUM_FS_READ,
            [u64::from(fd), offset, ptr_arg(buf), len as u64, 0, 0],
        )
    }
}

/// `fs_write`: write up to `len` bytes at `buf` to open file `fd` at byte
/// `offset` (`SyscallNumber::FS_WRITE`). Returns the number of bytes written,
/// or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// When the handle was opened `TAIRIX_OPEN_APPEND` the kernel ignores `offset`
/// and writes at the current end of file. The kernel validates `(buf, len)`
/// and re-authorises the write through the secured VFS.
#[must_use]
#[export_name = "tairix_sys_fs_write"]
pub extern "C" fn sys_fs_write(fd: u32, offset: u64, buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, len)`.
    unsafe {
        raw_syscall(
            NUM_FS_WRITE,
            [u64::from(fd), offset, ptr_arg(buf), len as u64, 0, 0],
        )
    }
}

/// `fs_readdir`: list the entries of open directory `fd` into `buf` as a
/// packed stream of [`tairix_abi::DirEntry`] records
/// (`SyscallNumber::FS_READDIR`). Returns the number of bytes written, or a
/// `TAIRIX_E_*` code reinterpreted into the result.
///
/// A buffer too small to hold the whole listing fails closed with
/// `TAIRIX_E_BUFFER_TOO_SMALL` (the listing is never truncated); the caller
/// grows `buf` and retries.
#[must_use]
#[export_name = "tairix_sys_fs_readdir"]
pub extern "C" fn sys_fs_readdir(fd: u32, buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, len)`.
    unsafe {
        raw_syscall(
            NUM_FS_READDIR,
            [u64::from(fd), ptr_arg(buf), len as u64, 0, 0, 0],
        )
    }
}

/// `fs_stat`: report the structural metadata of open handle `fd` as one
/// [`tairix_abi::FileStat`] record at `out` (`SyscallNumber::FS_STAT`).
/// Returns the number of bytes written, or a `TAIRIX_E_*` code reinterpreted
/// into the result.
///
/// A buffer too small fails closed with `TAIRIX_E_BUFFER_TOO_SMALL`.
#[must_use]
#[export_name = "tairix_sys_fs_stat"]
pub extern "C" fn sys_fs_stat(fd: u32, out: *mut c_void, out_len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(out, out_len)`.
    unsafe {
        raw_syscall(
            NUM_FS_STAT,
            [u64::from(fd), ptr_arg(out), out_len as u64, 0, 0, 0],
        )
    }
}

/// `fs_truncate`: set the length of open file `fd` to `size` bytes
/// (`SyscallNumber::FS_TRUNCATE`). Returns a `TAIRIX_E_*` code.
///
/// The kernel re-authorises the operation through the secured VFS; a
/// read-only mount, a directory handle, or a handle without write access
/// fails closed.
#[must_use]
#[export_name = "tairix_sys_fs_truncate"]
pub extern "C" fn sys_fs_truncate(fd: u32, size: u64) -> i32 {
    // SAFETY: see `sys_yield`.
    unsafe {
        ret_i32(raw_syscall(
            NUM_FS_TRUNCATE,
            [u64::from(fd), size, 0, 0, 0, 0],
        ))
    }
}

/// `fs_sync`: flush the filesystem backing open handle `fd` to its backing
/// store so prior writes are durable (`SyscallNumber::FS_SYNC`). Returns a
/// `TAIRIX_E_*` code.
#[must_use]
#[export_name = "tairix_sys_fs_sync"]
pub extern "C" fn sys_fs_sync(fd: u32) -> i32 {
    // SAFETY: see `sys_yield`.
    unsafe { ret_i32(raw_syscall(NUM_FS_SYNC, [u64::from(fd), 0, 0, 0, 0, 0])) }
}

/// `fs_mkdir`: create a directory at the absolute path `(path, path_len)`
/// (`SyscallNumber::FS_MKDIR`). Returns a `TAIRIX_E_*` code.
///
/// Requires `TAIRIX_CAP_FS_ACCESS`; resolution and the permission/mount-flag
/// model match `tairix_sys_fs_open`. The kernel validates `(path, path_len)`
/// against the caller's address space before reading it.
#[must_use]
#[export_name = "tairix_sys_fs_mkdir"]
pub extern "C" fn sys_fs_mkdir(path: *mut c_void, path_len: usize) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(path, path_len)`.
    unsafe {
        ret_i32(raw_syscall(
            NUM_FS_MKDIR,
            [ptr_arg(path), path_len as u64, 0, 0, 0, 0],
        ))
    }
}

/// `fs_unlink`: remove the file or empty directory at the absolute path
/// `(path, path_len)` (`SyscallNumber::FS_UNLINK`). Returns a `TAIRIX_E_*` code.
///
/// `flags` is the validated `TAIRIX_UNLINK_FLAG_*` word: `0` removes the named
/// file or (empty) directory; `TAIRIX_UNLINK_FLAG_DIRECTORY` restricts the
/// removal to an (empty) directory (the atomic `rmdir` posture — a
/// non-directory is refused with `TAIRIX_E_NOT_A_DIRECTORY`). A reserved bit
/// fails closed.
///
/// Requires `TAIRIX_CAP_FS_ACCESS`; resolution and the permission/mount-flag
/// model match `tairix_sys_fs_open`. A non-empty directory fails closed. The
/// kernel validates `(path, path_len)` against the caller's address space
/// before reading it.
#[must_use]
#[export_name = "tairix_sys_fs_unlink"]
pub extern "C" fn sys_fs_unlink(path: *mut c_void, path_len: usize, flags: u32) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(path, path_len)`
    // and rejects any reserved `flags` bit.
    unsafe {
        ret_i32(raw_syscall(
            NUM_FS_UNLINK,
            [ptr_arg(path), path_len as u64, u64::from(flags), 0, 0, 0],
        ))
    }
}

/// `fs_rename`: move the file or directory at the absolute path
/// `(src, src_len)` to the absolute path `(dst, dst_len)`
/// (`SyscallNumber::FS_RENAME`). Returns a `TAIRIX_E_*` code.
///
/// Requires `TAIRIX_CAP_FS_ACCESS`; resolution and the permission/mount-flag
/// model match `tairix_sys_fs_open`. Both paths must resolve under the same
/// mounted volume; a non-empty directory destination, a
/// directory-into-its-own-subtree move, or a cross-mount move fails closed.
/// The kernel validates both `(ptr, len)` pairs against the caller's
/// address space before reading them.
#[must_use]
#[export_name = "tairix_sys_fs_rename"]
pub extern "C" fn sys_fs_rename(
    src: *mut c_void,
    src_len: usize,
    dst: *mut c_void,
    dst_len: usize,
) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates both `(ptr, len)`.
    unsafe {
        ret_i32(raw_syscall(
            NUM_FS_RENAME,
            [
                ptr_arg(src),
                src_len as u64,
                ptr_arg(dst),
                dst_len as u64,
                0,
                0,
            ],
        ))
    }
}

/// `fs_set_mode`: set the permission bits of the file or directory at the
/// absolute path `(path, path_len)` to `mode` (`SyscallNumber::FS_SET_MODE`,
/// the `chmod(2)` shape). Returns a `TAIRIX_E_*` code.
///
/// `mode` carries at most `TAIRIX_FS_MODE_MASK` (the `rwx` triads plus the
/// setuid/setgid/sticky bits); any higher bit fails closed with
/// `TAIRIX_E_OUT_OF_RANGE` — never masked to a mode the caller did not ask
/// for. Requires `TAIRIX_CAP_FS_ACCESS`; resolution and the permission/
/// mount-flag model match `tairix_sys_fs_open`, and only the inode's owner may
/// change its mode. The kernel validates `(path, path_len)` against the
/// caller's address space before reading it.
#[must_use]
#[export_name = "tairix_sys_fs_set_mode"]
pub extern "C" fn sys_fs_set_mode(path: *mut c_void, path_len: usize, mode: u32) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(path, path_len)`
    // and rejects any `mode` bit above the permission mask.
    unsafe {
        ret_i32(raw_syscall(
            NUM_FS_SET_MODE,
            [ptr_arg(path), path_len as u64, u64::from(mode), 0, 0, 0],
        ))
    }
}

/// `fs_set_owner`: set the owning user and/or group of the file or
/// directory at the absolute path `(path, path_len)` to `uid` / `gid`
/// (`SyscallNumber::FS_SET_OWNER`, the `chown(2)` / `chgrp(2)` shape).
/// Returns a `TAIRIX_E_*` code.
///
/// Pass `TAIRIX_FS_OWNER_UNCHANGED` for either field to leave it unchanged.
/// Requires `TAIRIX_CAP_FS_ACCESS`; reassigning the **uid**, or setting a
/// **gid** the caller is not a member of, additionally requires
/// `TAIRIX_CAP_FS_CHOWN` — otherwise only the node's owner may change the
/// group, and only to a group they belong to. Any successful change clears
/// the setuid bit (and the setgid bit of a group-executable node) and the
/// covering mount must be writable. The kernel validates `(path, path_len)`
/// against the caller's address space before reading it.
#[must_use]
#[export_name = "tairix_sys_fs_set_owner"]
pub extern "C" fn sys_fs_set_owner(path: *mut c_void, path_len: usize, uid: u32, gid: u32) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(path, path_len)`.
    // The whole authority rule is enforced kernel-side by the secured VFS.
    unsafe {
        ret_i32(raw_syscall(
            NUM_FS_SET_OWNER,
            [
                ptr_arg(path),
                path_len as u64,
                u64::from(uid),
                u64::from(gid),
                0,
                0,
            ],
        ))
    }
}

/// `fs_attr_get`: read the extended attribute `(key, key_len)` of the file
/// or directory at the absolute path `(path, path_len)` into
/// `(value_out, value_out_len)` (`SyscallNumber::FS_ATTR_GET`, the
/// `getxattr(2)` shape). Returns the value's byte count, or a negative
/// `TAIRIX_E_*` code encoded in the `u64` (the `tairix_sys_spawn` convention):
/// `TAIRIX_E_NO_DATA` when no such attribute is stored, `TAIRIX_E_BUFFER_TOO_SMALL`
/// when the value does not fit (never truncated), `TAIRIX_E_NOT_SUPPORTED` on a
/// mount whose format stores no attributes. Requires `TAIRIX_CAP_FS_ACCESS`;
/// the key is a `namespace.rest` key of at most `TAIRIX_FS_ATTR_KEY_MAX` bytes,
/// the privileged namespaces are refused, and the caller needs read
/// permission on the node.
#[must_use]
#[export_name = "tairix_sys_fs_attr_get"]
pub extern "C" fn sys_fs_attr_get(
    path: *mut c_void,
    path_len: usize,
    key: *mut c_void,
    key_len: usize,
    value_out: *mut c_void,
    value_out_len: usize,
) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates every `(ptr, len)`
    // pair against the caller's address space before touching it.
    unsafe {
        raw_syscall(
            NUM_FS_ATTR_GET,
            [
                ptr_arg(path),
                path_len as u64,
                ptr_arg(key),
                key_len as u64,
                ptr_arg(value_out),
                value_out_len as u64,
            ],
        )
    }
}

/// `fs_attr_set`: set the extended attribute `(key, key_len)` of the file
/// or directory at the absolute path `(path, path_len)` to the opaque
/// bytes `(value, value_len)` (`SyscallNumber::FS_ATTR_SET`, the
/// `setxattr(2)` shape). Returns a `TAIRIX_E_*` code.
///
/// The value carries at most `TAIRIX_FS_ATTR_VALUE_MAX` bytes; a larger
/// payload fails closed with `TAIRIX_E_LENGTH_OUT_OF_RANGE`. Requires
/// `TAIRIX_CAP_FS_ACCESS`, write permission on the node, and a writable
/// mount; the privileged namespaces are refused.
#[must_use]
#[export_name = "tairix_sys_fs_attr_set"]
pub extern "C" fn sys_fs_attr_set(
    path: *mut c_void,
    path_len: usize,
    key: *mut c_void,
    key_len: usize,
    value: *mut c_void,
    value_len: usize,
) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates every `(ptr, len)`
    // pair and the key/value bounds before touching anything.
    unsafe {
        ret_i32(raw_syscall(
            NUM_FS_ATTR_SET,
            [
                ptr_arg(path),
                path_len as u64,
                ptr_arg(key),
                key_len as u64,
                ptr_arg(value),
                value_len as u64,
            ],
        ))
    }
}

/// `fs_attr_list`: yield the `index`-th visible extended-attribute key of
/// the file or directory at the absolute path `(path, path_len)` into
/// `(key_out, key_out_len)` (`SyscallNumber::FS_ATTR_LIST`). Returns the
/// key's byte count, `0` once `index` is past the last visible attribute,
/// or a negative `TAIRIX_E_*` code encoded in the `u64`. Keys the caller may
/// not read are omitted, never revealed. Requires `TAIRIX_CAP_FS_ACCESS` and
/// read permission on the node.
#[must_use]
#[export_name = "tairix_sys_fs_attr_list"]
pub extern "C" fn sys_fs_attr_list(
    path: *mut c_void,
    path_len: usize,
    index: u64,
    key_out: *mut c_void,
    key_out_len: usize,
) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates both `(ptr, len)`
    // pairs against the caller's address space before touching them.
    unsafe {
        raw_syscall(
            NUM_FS_ATTR_LIST,
            [
                ptr_arg(path),
                path_len as u64,
                index,
                ptr_arg(key_out),
                key_out_len as u64,
                0,
            ],
        )
    }
}

/// `fs_attr_remove`: remove the extended attribute `(key, key_len)` from
/// the file or directory at the absolute path `(path, path_len)`
/// (`SyscallNumber::FS_ATTR_REMOVE`, the `removexattr(2)` shape). Returns
/// a `TAIRIX_E_*` code: `TAIRIX_E_NO_DATA` when no such attribute is stored,
/// `TAIRIX_E_NOT_SUPPORTED` on a mount whose format stores no attributes.
/// Requires `TAIRIX_CAP_FS_ACCESS`, write permission on the node, and a
/// writable mount; the privileged namespaces are refused.
#[must_use]
#[export_name = "tairix_sys_fs_attr_remove"]
pub extern "C" fn sys_fs_attr_remove(
    path: *mut c_void,
    path_len: usize,
    key: *mut c_void,
    key_len: usize,
) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates both `(ptr, len)`
    // pairs and the key bound before touching anything.
    unsafe {
        ret_i32(raw_syscall(
            NUM_FS_ATTR_REMOVE,
            [
                ptr_arg(path),
                path_len as u64,
                ptr_arg(key),
                key_len as u64,
                0,
                0,
            ],
        ))
    }
}

/// `port_resolve`: resolve the published port name at `(name, name_len)` to
/// its live IPC endpoint id (`SyscallNumber::PORT_RESOLVE`). Returns the
/// endpoint id, or a negative `TAIRIX_E_*` code encoded in the `u64` (the
/// `tairix_sys_spawn` convention). Resolution grants nothing: every send to
/// the returned endpoint is still capability-checked kernel-side.
#[must_use]
#[export_name = "tairix_sys_port_resolve"]
pub extern "C" fn sys_port_resolve(name: *mut c_void, name_len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(name, name_len)`
    // against the caller's address space and the port-name grammar.
    unsafe {
        raw_syscall(
            NUM_PORT_RESOLVE,
            [ptr_arg(name), name_len as u64, 0, 0, 0, 0],
        )
    }
}

/// `signal`: deliver control signal `signal` (a `tairix_signal_t` discriminant)
/// to child process `pid` (`SyscallNumber::SIGNAL`). Returns a `TAIRIX_E_*`
/// code.
///
/// A process may signal only its **own** children; the kernel identifies the
/// sender from its own current-task slot, validates the parent/child
/// relationship and the signal value, and fails closed (`plans/SPAWN.md`
/// SP7). No capability is required.
#[must_use]
#[export_name = "tairix_sys_signal"]
pub extern "C" fn sys_signal(pid: i32, signal: u32) -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced; the kernel
    // validates the target child and the signal value on the far side of the
    // trap.
    unsafe {
        ret_i32(raw_syscall(
            NUM_SIGNAL,
            [i32_arg(pid), u64::from(signal), 0, 0, 0, 0],
        ))
    }
}

/// `console_foreground`: grant (or release) the controlling (foreground)
/// ownership of the console behind readable descriptor `fd` — the
/// exclusive drain right on its input queue and the child the cooked-mode
/// line discipline delivers `^C`/`^Z` to
/// (`SyscallNumber::CONSOLE_FOREGROUND`, the `tcsetpgrp` analogue,
/// `plans/DISPLAY.md` D5). Returns a `TAIRIX_E_*` code.
///
/// `pid` is a live child of the caller, or `0` to release. While an owner
/// is recorded, only it may `stream_read` or `stream_input_mode` that
/// console — every other task sees `TAIRIX_E_NOT_FOREGROUND`. Requires
/// `TAIRIX_CAP_CONSOLE_READ` (the same fd-scoped terminal-control gate
/// `stream_input_mode` carries); the kernel authorises the child through
/// the same parent/child bookkeeping `wait`/`signal` use,
/// owner/granter-checks the transition (a bystander can neither take nor
/// clear the ownership), and fails closed (`plans/SPAWN.md` SP9).
#[must_use]
#[export_name = "tairix_sys_console_foreground"]
pub extern "C" fn sys_console_foreground(fd: u32, pid: i32) -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced; the kernel
    // resolves `fd` against the caller's own descriptor table and
    // authorises `pid` on the far side of the trap.
    unsafe {
        ret_i32(raw_syscall(
            NUM_CONSOLE_FOREGROUND,
            [u64::from(fd), i32_arg(pid), 0, 0, 0, 0],
        ))
    }
}

/// `fs_chdir`: change the calling process's working directory to the
/// (absolute or cwd-relative) path `(path, path_len)`
/// (`SyscallNumber::FS_CHDIR`). Returns a `TAIRIX_E_*` code.
///
/// Requires `TAIRIX_CAP_FS_ACCESS`; the kernel validates `(path, path_len)`
/// against the caller's address space, resolves it (relative to the caller's
/// current working directory when it is not absolute), and re-authorises it
/// as a searchable directory under the caller's real credentials before it
/// becomes the new working directory. A path that is not a searchable
/// directory fails closed and leaves the working directory unchanged.
#[must_use]
#[export_name = "tairix_sys_fs_chdir"]
pub extern "C" fn sys_fs_chdir(path: *mut c_void, path_len: usize) -> i32 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(path, path_len)`.
    unsafe {
        ret_i32(raw_syscall(
            NUM_FS_CHDIR,
            [ptr_arg(path), path_len as u64, 0, 0, 0, 0],
        ))
    }
}

/// `fs_getcwd`: write the calling process's working directory — a normalised
/// absolute path — into `buf` (`SyscallNumber::FS_GETCWD`). Returns the
/// number of bytes written, or a `TAIRIX_E_*` code reinterpreted into the
/// result.
///
/// A buffer too small to hold the whole path fails closed with
/// `TAIRIX_E_BUFFER_TOO_SMALL` (the path is never truncated); the caller grows
/// `buf` and retries. Needs no capability.
#[must_use]
#[export_name = "tairix_sys_fs_getcwd"]
pub extern "C" fn sys_fs_getcwd(buf: *mut c_void, buf_len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, buf_len)`.
    unsafe { raw_syscall(NUM_FS_GETCWD, [ptr_arg(buf), buf_len as u64, 0, 0, 0, 0]) }
}

/// `resource_open`: resolve the resource reference `(reference,
/// reference_len)` and open it to a new descriptor
/// (`SyscallNumber::RESOURCE_OPEN`). Returns a new per-process descriptor (at
/// or above `TAIRIX_STD_STREAM_COUNT`), or a `TAIRIX_E_*` code reinterpreted into
/// the result.
///
/// A resource reference (e.g. `"sys:random"`) names a typed non-filesystem
/// resource; there is no `/dev`, `/proc`, or `/sys`. `flags` is the
/// `TAIRIX_OPEN_*` bit set ([`tairix_abi::OpenFlags`]). Authorisation is per
/// namespace inside the kernel resolver (an unprivileged resource needs no
/// capability); the kernel validates the `(reference, reference_len)` pair
/// against the caller's address space. The returned descriptor is read and
/// written with `tairix_sys_fs_read` / `tairix_sys_fs_write` and released with
/// `tairix_sys_fs_close`, exactly as a file descriptor is.
#[must_use]
#[export_name = "tairix_sys_resource_open"]
pub extern "C" fn sys_resource_open(
    reference: *mut c_void,
    reference_len: usize,
    flags: u32,
) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(reference,
    // reference_len)`.
    unsafe {
        raw_syscall(
            NUM_RESOURCE_OPEN,
            [
                ptr_arg(reference),
                reference_len as u64,
                u64::from(flags),
                0,
                0,
                0,
            ],
        )
    }
}

/// `fd_grant`: delegate the caller's own read-only filesystem descriptor
/// `fd` to the live task `pid` as a one-shot grant
/// (`SyscallNumber::FD_GRANT`). Returns the minted, unforgeable grant
/// handle (>= 1), or a `TAIRIX_E_*` code reinterpreted into the result.
///
/// The kernel requires `TAIRIX_CAP_FS_ACCESS`, confirms the caller itself
/// holds `fd` as a plain read-only, non-directory filesystem descriptor
/// (a pipe, resource, writable, or already-delegated descriptor is
/// refused — delegation never widens and never chains), captures the
/// caller's identity and effective capability set with the descriptor's
/// path, and confirms the recipient task is live (task ids are never
/// reused, so a pid from a kernel-attested source lands on exactly the
/// intended process). The caller forwards the handle in-band; it
/// resolves only through the recipient's own [`sys_fd_redeem`], so the
/// number is useless to a bystander. Audited.
#[must_use]
#[export_name = "tairix_sys_fd_grant"]
pub extern "C" fn sys_fd_grant(fd: u32, pid: u64) -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced; the kernel
    // validates the capability, the caller's own descriptor, and the
    // recipient's liveness before minting anything.
    unsafe { raw_syscall(NUM_FD_GRANT, [u64::from(fd), pid, 0, 0, 0, 0]) }
}

/// `fd_redeem`: redeem an `fd_grant` handle minted to the calling task,
/// installing the delegated file into the caller's own open table
/// (`SyscallNumber::FD_REDEEM`). Returns the fresh per-process descriptor
/// (at or above `TAIRIX_STD_STREAM_COUNT`), or a `TAIRIX_E_*` code
/// reinterpreted into the result.
///
/// Needs no capability: receiving user-mediated, already-checked
/// authority is the point of the delegation, and every later read of the
/// descriptor is still authorised kernel-side under the grantor's
/// captured identity. One-shot: the grant is consumed only when the
/// descriptor allocation succeeds, so a refused redemption leaves it
/// intact and a redeemed handle can never be redeemed twice. A handle
/// minted to another task fails closed with `TAIRIX_E_NOT_FOUND`,
/// indistinguishable from one that never existed. The descriptor is read
/// with `tairix_sys_fs_read` and released with `tairix_sys_fs_close`. Audited.
#[must_use]
#[export_name = "tairix_sys_fd_redeem"]
pub extern "C" fn sys_fd_redeem(handle: u64) -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced; the kernel
    // resolves the handle owner-bound before installing anything.
    unsafe { raw_syscall(NUM_FD_REDEEM, [handle, 0, 0, 0, 0, 0]) }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The trap seam lives in `tairix-abi-trap` (the single trap home) and is reached here through the `host-seam`
    // dev-dependency feature; production builds never compile it.
    use tairix_abi::SYSCALLS;
    use tairix_abi_trap::seam;

    /// The complete set of stubs this crate implements, paired with the
    /// `abi-v1` number and argument count each one marshals. The drift tests
    /// below cross-check this registry against the frozen `SYSCALLS` table so
    /// a new or changed syscall cannot silently escape the C surface (the
    /// "dense/complete" discipline of `errno_table_matches_the_frozen_enum`).
    const IMPLEMENTED: &[(u64, &str, u8)] = &[
        (NUM_YIELD, "yield", 0),
        (NUM_EXIT, "exit", 1),
        (NUM_IPC_SEND, "ipc_send", 3),
        (NUM_IPC_RECV, "ipc_recv", 4),
        (NUM_CAP_QUERY, "cap_query", 1),
        (NUM_CAP_DELEGATE, "cap_delegate", 2),
        (NUM_CAP_REVOKE, "cap_revoke", 2),
        (NUM_CLOCK_GET, "clock_get", 0),
        (NUM_IRQ_BIND, "irq_bind", 1),
        (NUM_IRQ_WAIT, "irq_wait", 2),
        (NUM_RANDOM_GET, "random_get", 3),
        (NUM_STREAM_WRITE, "stream_write", 3),
        (NUM_SPAWN, "spawn", 6),
        (NUM_STREAM_READ, "stream_read", 4),
        (NUM_MEM_MAP, "mem_map", 3),
        (NUM_MEM_UNMAP, "mem_unmap", 2),
        (NUM_FILE_MAP, "file_map", 3),
        (NUM_FILE_UNMAP, "file_unmap", 2),
        (NUM_VOLUME_ATTACH, "volume_attach", 2),
        (NUM_VOLUME_DETACH, "volume_detach", 2),
        (NUM_WAIT, "wait", 3),
        (NUM_RLIMIT_GET, "rlimit_get", 2),
        (NUM_RLIMIT_SET, "rlimit_set", 2),
        (NUM_USERS_DB_READ, "users_db_read", 2),
        (NUM_USERS_DB_WAIT, "users_db_wait", 1),
        (NUM_USERS_ADMIN, "users_admin", 4),
        (NUM_CONSOLE_COUNT, "console_count", 0),
        (NUM_STREAM_INPUT_MODE, "stream_input_mode", 2),
        (NUM_CONSOLE_FOREGROUND, "console_foreground", 2),
        (NUM_KEY_INJECT, "key_inject", 3),
        (NUM_DISPLAY_ACQUIRE, "display_acquire", 1),
        (NUM_DISPLAY_RELEASE, "display_release", 1),
        (NUM_KEYBOARD_READ, "keyboard_read", 3),
        (NUM_SEAT_SWITCH, "seat_switch", 2),
        (NUM_SEAT_REVOKE, "seat_revoke", 1),
        (NUM_MMIO_MAP, "mmio_map", 3),
        (NUM_DMA_ALLOC, "dma_alloc", 3),
        (NUM_DMA_FREE, "dma_free", 2),
        (NUM_RESOURCE_GRANTS, "resource_grants", 2),
        (NUM_HW_TREE_READ, "hw_tree_read", 2),
        (NUM_HW_TREE_WAIT, "hw_tree_wait", 2),
        (NUM_IPC_CALL, "ipc_call", 5),
        (NUM_CALL_CREATE, "call_create", 6),
        (NUM_CALL_RECV, "call_recv", 5),
        (NUM_CALL_REPLY, "call_reply", 4),
        (NUM_LOG_EMIT, "log_emit", 2),
        (NUM_HW_EMIT_NODE, "hw_emit_node", 2),
        (NUM_HW_REMOVE_NODE, "hw_remove_node", 1),
        (NUM_MSI_ALLOC, "msi_alloc", 2),
        (NUM_SHM_CREATE, "shm_create", 2),
        (NUM_SHM_MAP, "shm_map", 2),
        (NUM_SHM_UNMAP, "shm_unmap", 2),
        (NUM_SHM_GRANT, "shm_grant", 2),
        (NUM_CALL_PEER_SEAT, "call_peer_seat", 3),
        (NUM_WAITSET_CREATE, "waitset_create", 0),
        (NUM_WAITSET_CTL, "waitset_ctl", 5),
        (NUM_WAITSET_WAIT, "waitset_wait", 3),
        (NUM_FS_OPEN, "fs_open", 3),
        (NUM_FS_CLOSE, "fs_close", 1),
        (NUM_FS_READ, "fs_read", 4),
        (NUM_FS_WRITE, "fs_write", 4),
        (NUM_FS_READDIR, "fs_readdir", 3),
        (NUM_FS_STAT, "fs_stat", 3),
        (NUM_FS_TRUNCATE, "fs_truncate", 2),
        (NUM_FS_SYNC, "fs_sync", 1),
        (NUM_FS_MKDIR, "fs_mkdir", 2),
        (NUM_FS_UNLINK, "fs_unlink", 3),
        (NUM_FS_RENAME, "fs_rename", 4),
        (NUM_CALL_PEER_ORIGIN, "call_peer_origin", 4),
        (NUM_WALL_TIME_GET, "wall_time_get", 2),
        (NUM_WALL_TIME_SET, "wall_time_set", 3),
        (NUM_BOOT_ID_GET, "boot_id_get", 2),
        (NUM_BOOT_FACTS_GET, "boot_facts_get", 2),
        (NUM_SYSINFO_INTROSPECT, "sysinfo_introspect", 4),
        (NUM_TERMINAL_SIZE, "terminal_size", 3),
        (NUM_SIGNAL, "signal", 2),
        (NUM_FS_CHDIR, "fs_chdir", 2),
        (NUM_FS_GETCWD, "fs_getcwd", 2),
        (NUM_RESOURCE_OPEN, "resource_open", 3),
        (NUM_SELF_ORIGIN, "self_origin", 2),
        (NUM_PIPE_CREATE, "pipe_create", 1),
        (NUM_FS_SET_MODE, "fs_set_mode", 3),
        (NUM_FS_SET_OWNER, "fs_set_owner", 4),
        (NUM_FS_ATTR_GET, "fs_attr_get", 6),
        (NUM_FS_ATTR_SET, "fs_attr_set", 6),
        (NUM_FS_ATTR_LIST, "fs_attr_list", 5),
        (NUM_FS_ATTR_REMOVE, "fs_attr_remove", 4),
        (NUM_PORT_BIND, "port_bind", 3),
        (NUM_PORT_RESOLVE, "port_resolve", 2),
        (NUM_POINTER_INJECT, "pointer_inject", 3),
        (NUM_POINTER_READ, "pointer_read", 3),
        (NUM_FD_GRANT, "fd_grant", 2),
        (NUM_FD_REDEEM, "fd_redeem", 1),
        (NUM_MEM_PIN, "mem_pin", 0),
        (NUM_MEM_UNPIN, "mem_unpin", 0),
        (NUM_SIGNAL_INTAKE, "signal_intake", 1),
        (NUM_SCHED_SET_REALTIME, "sched_set_realtime", 1),
    ];

    #[test]
    fn registry_covers_exactly_the_frozen_table() {
        assert_eq!(
            IMPLEMENTED.len(),
            SYSCALLS.len(),
            "every abi-v1 syscall must have exactly one tairix_sys_* stub"
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
    fn sched_set_realtime_marshals_the_class_boolean() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_sched_set_realtime(1), 0);
        });
        assert_eq!(number, NUM_SCHED_SET_REALTIME);
        assert_eq!(args[0], 1);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);

        let (number, args) = capture(0, || {
            assert_eq!(sys_sched_set_realtime(0), 0);
        });
        assert_eq!(number, NUM_SCHED_SET_REALTIME);
        assert_eq!(args[0], 0);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
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
    fn ipc_recv_marshals_endpoint_pointer_len_and_sender_out() {
        let mut buffer = [0u8; 16];
        let mut sender = [0u8; tairix_abi::ORIGIN_WIRE_LEN];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let sender_ptr = sender.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            let _ = sys_ipc_recv(0x1234, ptr, 16, sender_ptr);
        });
        assert_eq!(number, NUM_IPC_RECV);
        assert_eq!(args[0], 0x1234);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 16);
        assert_eq!(args[3], sender_ptr as usize as u64);
    }

    #[test]
    fn port_bind_marshals_endpoint_and_bounds() {
        let (number, args) = capture(0, || {
            let _ = sys_port_bind(0x5EAD_0001, 40, 8);
        });
        assert_eq!(number, NUM_PORT_BIND);
        assert_eq!(args[0], 0x5EAD_0001);
        assert_eq!(args[1], 40);
        assert_eq!(args[2], 8);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn wall_time_get_marshals_out_pointer_and_capacity() {
        let mut buf = [0u8; 13];
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(13, || {
            assert_eq!(sys_wall_time_get(ptr, 13), 13);
        });
        assert_eq!(number, NUM_WALL_TIME_GET);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], 13);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn terminal_size_marshals_fd_out_pointer_and_capacity() {
        let mut buf = [0u8; 4];
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(4, || {
            assert_eq!(sys_terminal_size(1, ptr, 4), 4);
        });
        assert_eq!(number, NUM_TERMINAL_SIZE);
        assert_eq!(args[0], 1);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 4);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn wall_time_set_marshals_time_pointer_len_and_state() {
        let mut buf = [0u8; 12];
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_wall_time_set(ptr, 12, 2), 0);
        });
        assert_eq!(number, NUM_WALL_TIME_SET);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], 12);
        assert_eq!(args[2], 2);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn boot_id_get_marshals_out_pointer_and_capacity() {
        let mut buf = [0u8; 16];
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(16, || {
            assert_eq!(sys_boot_id_get(ptr, 16), 16);
        });
        assert_eq!(number, NUM_BOOT_ID_GET);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], 16);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn sysinfo_introspect_marshals_domain_arg_pointer_and_capacity() {
        let mut buf = [0u8; 96];
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(96, || {
            assert_eq!(sys_sysinfo_introspect(2, 5, ptr, 96), 96);
        });
        assert_eq!(number, NUM_SYSINFO_INTROSPECT);
        assert_eq!(args[0], 2);
        assert_eq!(args[1], 5);
        assert_eq!(args[2], ptr as usize as u64);
        assert_eq!(args[3], 96);
        assert_eq!(&args[4..], &[0, 0]);
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
    fn mmio_map_marshals_the_grant_handle_offset_and_len() {
        let (number, args) = capture(0x9000_0000, || {
            assert_eq!(sys_mmio_map(0x2A, 0x3D50_0000, 0x1000), 0x9000_0000);
        });
        assert_eq!(number, NUM_MMIO_MAP);
        assert_eq!(args[0], 0x2A);
        assert_eq!(args[1], 0x3D50_0000);
        assert_eq!(args[2], 0x1000);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn dma_alloc_marshals_handle_len_and_device_out_pointer() {
        let mut device = 0u64;
        let ptr = core::ptr::addr_of_mut!(device).cast::<c_void>();
        let (number, args) = capture(0xD000_2000, || {
            assert_eq!(sys_dma_alloc(0x2A, 0x2000, ptr), 0xD000_2000);
        });
        assert_eq!(number, NUM_DMA_ALLOC);
        assert_eq!(args[0], 0x2A);
        assert_eq!(args[1], 0x2000);
        assert_eq!(args[2], ptr as usize as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
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
    fn stream_write_marshals_fd_pointer_and_len() {
        let mut buffer = [0u8; 8];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(8, || {
            assert_eq!(sys_stream_write(1, ptr, 8), 8);
        });
        assert_eq!(number, NUM_STREAM_WRITE);
        assert_eq!(args[0], 1);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 8);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn stream_read_marshals_fd_pointer_len_and_timeout() {
        let mut buffer = [0u8; 8];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(5, || {
            assert_eq!(sys_stream_read(0, ptr, 8, 7_000_000), 5);
        });
        assert_eq!(number, NUM_STREAM_READ);
        assert_eq!(args[0], 0);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 8);
        assert_eq!(args[3], 7_000_000);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn spawn_marshals_path_pointer_len_and_absent_blocks() {
        let mut path = *b"/Apps/Child.app/Run";
        let ptr = path.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(7, || {
            assert_eq!(
                sys_spawn(
                    ptr,
                    path.len(),
                    core::ptr::null_mut(),
                    0,
                    core::ptr::null_mut(),
                    0
                ),
                7
            );
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        // NULL/0 attach and strings pairs marshal the "no block" (full
        // inherit / registered defaults) zero/zero shapes.
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn spawn_marshals_the_attach_and_startup_strings_blocks() {
        let mut path = *b"/Apps/Child.app/Run";
        let ptr = path.as_mut_ptr().cast::<c_void>();
        let mut attach = tairix_abi::SpawnAttach::INHERIT.to_le_bytes();
        let attach_ptr = attach.as_mut_ptr().cast::<c_void>();
        let mut block = *b"opaque-encoded-psv1-bytes";
        let block_ptr = block.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(9, || {
            assert_eq!(
                sys_spawn(
                    ptr,
                    path.len(),
                    attach_ptr,
                    attach.len(),
                    block_ptr,
                    block.len(),
                ),
                9
            );
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[2], attach_ptr as usize as u64);
        assert_eq!(args[3], attach.len() as u64);
        assert_eq!(args[4], block_ptr as usize as u64);
        assert_eq!(args[5], block.len() as u64);
    }

    #[test]
    fn pipe_create_marshals_the_out_pointer() {
        let mut fds = [0u32; 2];
        let ptr = fds.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_pipe_create(ptr), 0);
        });
        assert_eq!(number, NUM_PIPE_CREATE);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn console_count_marshals_no_arguments() {
        let (number, args) = capture(2, || {
            assert_eq!(sys_console_count(), 2);
        });
        assert_eq!(number, NUM_CONSOLE_COUNT);
        assert_eq!(args, [0; SYSCALL_MAX_ARGS]);
    }

    #[test]
    fn stream_input_mode_marshals_fd_and_mode() {
        // Each defined mode discriminant is marshalled verbatim; the
        // kernel, not the stub, validates it (the stub only marshals).
        for mode in [1u32, 2, 3] {
            let (number, args) = capture(0, || {
                assert_eq!(sys_stream_input_mode(0, mode), 0);
            });
            assert_eq!(number, NUM_STREAM_INPUT_MODE);
            assert_eq!(args[0], 0);
            assert_eq!(args[1], u64::from(mode));
            assert_eq!(&args[2..], &[0, 0, 0, 0]);
        }
    }

    #[test]
    fn key_inject_marshals_seat_pointer_and_len() {
        let mut record = [0u8; 8];
        let ptr = record.as_mut_ptr().cast::<c_void>();
        let len = record.len();
        // The kernel returns the number of bytes consumed.
        let (number, args) = capture(len as u64, || {
            assert_eq!(sys_key_inject(3, ptr, len), len as u64);
        });
        assert_eq!(number, NUM_KEY_INJECT);
        assert_eq!(args[0], 3);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], len as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn display_acquire_and_release_marshal_the_seat_id() {
        // A successful acquire returns the minted lease generation.
        let (number, args) = capture(1, || {
            assert_eq!(sys_display_acquire(3), 1);
        });
        assert_eq!(number, NUM_DISPLAY_ACQUIRE);
        assert_eq!(args, [3, 0, 0, 0, 0, 0]);

        let (number, args) = capture(0, || {
            assert_eq!(sys_display_release(3), 0);
        });
        assert_eq!(number, NUM_DISPLAY_RELEASE);
        assert_eq!(args, [3, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn seat_switch_and_revoke_marshal_their_arguments() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_seat_switch(0, 1), 0);
        });
        assert_eq!(number, NUM_SEAT_SWITCH);
        assert_eq!(args, [0, 1, 0, 0, 0, 0]);

        let (number, args) = capture(0, || {
            assert_eq!(sys_seat_revoke(0), 0);
        });
        assert_eq!(number, NUM_SEAT_REVOKE);
        assert_eq!(args, NO_ARGS);
    }

    #[test]
    fn keyboard_read_marshals_seat_pointer_and_len() {
        let mut buf = [0u8; 8];
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        let len = buf.len();
        // The kernel returns the number of bytes written (one record).
        let (number, args) = capture(len as u64, || {
            assert_eq!(sys_keyboard_read(3, ptr, len), len as u64);
        });
        assert_eq!(number, NUM_KEYBOARD_READ);
        assert_eq!(args[0], 3);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], len as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn resource_grants_marshals_pointer_and_len() {
        let mut buf = [0u8; 40];
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        let len = buf.len();
        // The kernel returns the number of bytes written (one record here).
        let (number, args) = capture(len as u64, || {
            assert_eq!(sys_resource_grants(ptr, len), len as u64);
        });
        assert_eq!(number, NUM_RESOURCE_GRANTS);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], len as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn hw_tree_read_marshals_pointer_and_len() {
        let mut buf = [0u8; 64];
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        let len = buf.len();
        // The kernel returns the number of bytes written (the snapshot).
        let (number, args) = capture(len as u64, || {
            assert_eq!(sys_hw_tree_read(ptr, len), len as u64);
        });
        assert_eq!(number, NUM_HW_TREE_READ);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], len as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn log_emit_marshals_pointer_and_len() {
        let mut record = [0u8; 16];
        let ptr = record.as_mut_ptr().cast::<c_void>();
        let len = record.len();
        // `0` is the success return (the record was accepted).
        let (number, args) = capture(0, || {
            assert_eq!(sys_log_emit(ptr, len), 0);
        });
        assert_eq!(number, NUM_LOG_EMIT);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], len as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn hw_tree_wait_marshals_generation_and_timeout() {
        // `0` is the success return (the tree changed); the arguments are the
        // last-observed generation and the timeout bound.
        let (number, args) = capture(0, || {
            assert_eq!(sys_hw_tree_wait(7, u64::MAX), 0);
        });
        assert_eq!(number, NUM_HW_TREE_WAIT);
        assert_eq!(args[0], 7);
        assert_eq!(args[1], u64::MAX);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn users_db_wait_marshals_the_timeout() {
        // `0` is the success return (the database is no longer pending); the
        // only argument is the scalar timeout bound.
        let (number, args) = capture(0, || {
            assert_eq!(sys_users_db_wait(u64::MAX), 0);
        });
        assert_eq!(number, NUM_USERS_DB_WAIT);
        assert_eq!(args[0], u64::MAX);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn users_admin_marshals_both_buffers() {
        let mut req = [1u8, 0, 9, 0];
        let mut out = [0u8; 32];
        let req_ptr = req.as_mut_ptr().cast::<c_void>();
        let out_ptr = out.as_mut_ptr().cast::<c_void>();
        // `0` is the mutating-operation success return (no response bytes).
        let (number, args) = capture(0, || {
            assert_eq!(sys_users_admin(req_ptr, req.len(), out_ptr, out.len()), 0);
        });
        assert_eq!(number, NUM_USERS_ADMIN);
        assert_eq!(args[0], req_ptr as usize as u64);
        assert_eq!(args[1], req.len() as u64);
        assert_eq!(args[2], out_ptr as usize as u64);
        assert_eq!(args[3], out.len() as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn ipc_call_marshals_endpoint_and_both_buffers() {
        let mut request = [0xAAu8; 5];
        let mut reply = [0u8; 64];
        let req_ptr = request.as_mut_ptr().cast::<c_void>();
        let reply_ptr = reply.as_mut_ptr().cast::<c_void>();
        // The kernel returns the number of reply bytes written.
        let (number, args) = capture(12, || {
            assert_eq!(
                sys_ipc_call(
                    tairix_abi::driver_store::DRIVER_STORE_ENDPOINT,
                    req_ptr,
                    request.len(),
                    reply_ptr,
                    reply.len()
                ),
                12
            );
        });
        assert_eq!(number, NUM_IPC_CALL);
        assert_eq!(args[0], tairix_abi::driver_store::DRIVER_STORE_ENDPOINT);
        assert_eq!(args[1], req_ptr as usize as u64);
        assert_eq!(args[2], request.len() as u64);
        assert_eq!(args[3], reply_ptr as usize as u64);
        assert_eq!(args[4], reply.len() as u64);
        assert_eq!(args[5], 0);
    }

    #[test]
    fn mem_map_marshals_len_flags_and_addr_hint() {
        let (number, args) = capture(0x4000, || {
            assert_eq!(sys_mem_map(0x2000, 1, 0x10_0000), 0x4000);
        });
        assert_eq!(number, NUM_MEM_MAP);
        assert_eq!(args[0], 0x2000);
        assert_eq!(args[1], 1);
        assert_eq!(args[2], 0x10_0000);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn mem_unmap_marshals_base_and_len() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_mem_unmap(0x4000, 0x2000), 0);
        });
        assert_eq!(number, NUM_MEM_UNMAP);
        assert_eq!(args[0], 0x4000);
        assert_eq!(args[1], 0x2000);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn shm_create_marshals_len_and_id_out_pointer() {
        let mut id = 0u64;
        let ptr = core::ptr::addr_of_mut!(id).cast::<c_void>();
        let (number, args) = capture(0x9000_0000, || {
            assert_eq!(sys_shm_create(0x2000, ptr), 0x9000_0000);
        });
        assert_eq!(number, NUM_SHM_CREATE);
        assert_eq!(args[0], 0x2000);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn shm_map_marshals_the_grant_handle_and_len_out_pointer() {
        let mut len = 0u64;
        let ptr = core::ptr::addr_of_mut!(len).cast::<c_void>();
        let (number, args) = capture(0x9000_4000, || {
            assert_eq!(sys_shm_map(0x2A, ptr), 0x9000_4000);
        });
        assert_eq!(number, NUM_SHM_MAP);
        assert_eq!(args[0], 0x2A);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn shm_unmap_marshals_base_and_len() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_shm_unmap(0x9000_4000, 0x2000), 0);
        });
        assert_eq!(number, NUM_SHM_UNMAP);
        assert_eq!(args[0], 0x9000_4000);
        assert_eq!(args[1], 0x2000);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn shm_grant_marshals_region_and_endpoint() {
        let (number, args) = capture(5, || {
            assert_eq!(sys_shm_grant(42, 0xD15_1001), 5);
        });
        assert_eq!(number, NUM_SHM_GRANT);
        assert_eq!(args[0], 42);
        assert_eq!(args[1], 0xD15_1001);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fd_grant_marshals_descriptor_and_recipient() {
        let (number, args) = capture(7, || {
            assert_eq!(sys_fd_grant(4, 0x2A), 7);
        });
        assert_eq!(number, NUM_FD_GRANT);
        assert_eq!(args[0], 4);
        assert_eq!(args[1], 0x2A);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fd_redeem_marshals_the_handle() {
        let (number, args) = capture(5, || {
            assert_eq!(sys_fd_redeem(7), 5);
        });
        assert_eq!(number, NUM_FD_REDEEM);
        assert_eq!(args[0], 7);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn call_peer_seat_marshals_endpoint_ticket_and_seat() {
        let (number, args) = capture(3, || {
            assert_eq!(sys_call_peer_seat(0xD15_1001, 9, 0), 3);
        });
        assert_eq!(number, NUM_CALL_PEER_SEAT);
        assert_eq!(args[0], 0xD15_1001);
        assert_eq!(args[1], 9);
        assert_eq!(args[2], 0);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn waitset_create_takes_no_args_and_returns_the_handle() {
        let (number, args) = capture(0x7700_0001, || {
            assert_eq!(sys_waitset_create(), 0x7700_0001);
        });
        assert_eq!(number, NUM_WAITSET_CREATE);
        assert_eq!(&args, &[0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn waitset_ctl_marshals_set_op_kind_id_and_token() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_waitset_ctl(0x7700_0001, 0, 1, 0xABCD, 0x55), 0);
        });
        assert_eq!(number, NUM_WAITSET_CTL);
        assert_eq!(args[0], 0x7700_0001);
        assert_eq!(args[1], 0);
        assert_eq!(args[2], 1);
        assert_eq!(args[3], 0xABCD);
        assert_eq!(args[4], 0x55);
        assert_eq!(args[5], 0);
    }

    #[test]
    fn waitset_wait_marshals_set_timeout_and_token_out_pointer() {
        let mut token = 0u64;
        let ptr = core::ptr::addr_of_mut!(token).cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_waitset_wait(0x7700_0001, u64::MAX, ptr), 0);
        });
        assert_eq!(number, NUM_WAITSET_WAIT);
        assert_eq!(args[0], 0x7700_0001);
        assert_eq!(args[1], u64::MAX);
        assert_eq!(args[2], ptr as usize as u64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn wait_marshals_pid_status_pointer_and_flags() {
        let mut status = 0i32;
        let ptr = core::ptr::addr_of_mut!(status).cast::<c_void>();
        // The kernel returns the reaped child's PID. A blocking wait carries
        // no flags.
        let (number, args) = capture(5, || {
            assert_eq!(sys_wait(9, ptr, 0), 5);
        });
        assert_eq!(number, NUM_WAIT);
        assert_eq!(args[0], 9);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 0);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn wait_marshals_the_nonblock_flag() {
        let mut status = 0i32;
        let ptr = core::ptr::addr_of_mut!(status).cast::<c_void>();
        let flags = tairix_abi::WaitFlags::NONBLOCK.bits();
        let (number, args) = capture(0, || {
            let _ = sys_wait(9, ptr, flags);
        });
        assert_eq!(number, NUM_WAIT);
        assert_eq!(args[2], u64::from(flags));
    }

    #[test]
    fn wait_sign_extends_wait_any() {
        let mut status = 0i32;
        let ptr = core::ptr::addr_of_mut!(status).cast::<c_void>();
        let (number, args) = capture(3, || {
            let _ = sys_wait(tairix_abi::WAIT_PID_ANY, ptr, 0);
        });
        assert_eq!(number, NUM_WAIT);
        // `WAIT_PID_ANY` (-1) sign-extends to all-ones in the argument register.
        assert_eq!(args[0], u64::MAX);
    }

    #[test]
    fn signal_marshals_pid_and_signal() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_signal(9, tairix_abi::Signal::Terminate.as_u32()), 0);
        });
        assert_eq!(number, NUM_SIGNAL);
        assert_eq!(args[0], 9);
        assert_eq!(args[1], u64::from(tairix_abi::Signal::Terminate.as_u32()));
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn signal_sign_extends_a_negative_pid() {
        // A negative PID sign-extends in the argument register per the I32
        // convention; the kernel rejects it (no child), but the marshalling
        // must be faithful.
        let (number, args) = capture(0, || {
            let _ = sys_signal(-1, tairix_abi::Signal::Kill.as_u32());
        });
        assert_eq!(number, NUM_SIGNAL);
        assert_eq!(args[0], u64::MAX);
    }

    #[test]
    fn rlimit_get_marshals_kind_and_pointer() {
        let mut limit = [0u8; 16];
        let ptr = limit.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_rlimit_get(2, ptr), 0);
        });
        assert_eq!(number, NUM_RLIMIT_GET);
        assert_eq!(args[0], 2);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn rlimit_set_marshals_kind_and_pointer() {
        let mut limit = [0u8; 16];
        let ptr = limit.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_rlimit_set(3, ptr), 0);
        });
        assert_eq!(number, NUM_RLIMIT_SET);
        assert_eq!(args[0], 3);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn errno_returns_are_truncated_to_int32() {
        // A kernel result whose low 32 bits encode a negative `TAIRIX_E_*` code
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

    #[test]
    fn fs_open_marshals_path_len_and_flags() {
        let mut path = *b"/System/Logs";
        let ptr = path.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0x100, || {
            assert_eq!(sys_fs_open(ptr, path.len(), 0x3), 0x100);
        });
        assert_eq!(number, NUM_FS_OPEN);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], 0x3);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn resource_open_marshals_reference_len_and_flags() {
        let mut reference = *b"sys:random";
        let ptr = reference.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0x105, || {
            assert_eq!(sys_resource_open(ptr, reference.len(), 0x1), 0x105);
        });
        assert_eq!(number, NUM_RESOURCE_OPEN);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], reference.len() as u64);
        assert_eq!(args[2], 0x1);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn fs_close_marshals_the_descriptor() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_fs_close(0x104), 0);
        });
        assert_eq!(number, NUM_FS_CLOSE);
        assert_eq!(args[0], 0x104);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn fs_read_marshals_fd_offset_pointer_and_len() {
        let mut buffer = [0u8; 32];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(32, || {
            assert_eq!(sys_fs_read(0x104, 0x1000, ptr, 32), 32);
        });
        assert_eq!(number, NUM_FS_READ);
        assert_eq!(args[0], 0x104);
        assert_eq!(args[1], 0x1000);
        assert_eq!(args[2], ptr as usize as u64);
        assert_eq!(args[3], 32);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_write_marshals_fd_offset_pointer_and_len() {
        let mut buffer = [0u8; 16];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(16, || {
            assert_eq!(sys_fs_write(0x104, 0x2000, ptr, 16), 16);
        });
        assert_eq!(number, NUM_FS_WRITE);
        assert_eq!(args[0], 0x104);
        assert_eq!(args[1], 0x2000);
        assert_eq!(args[2], ptr as usize as u64);
        assert_eq!(args[3], 16);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_readdir_marshals_fd_pointer_and_len() {
        let mut buffer = [0u8; 64];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(48, || {
            assert_eq!(sys_fs_readdir(0x104, ptr, 64), 48);
        });
        assert_eq!(number, NUM_FS_READDIR);
        assert_eq!(args[0], 0x104);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 64);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn fs_stat_marshals_fd_pointer_and_len() {
        let mut buffer = [0u8; 32];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(32, || {
            assert_eq!(sys_fs_stat(0x104, ptr, 32), 32);
        });
        assert_eq!(number, NUM_FS_STAT);
        assert_eq!(args[0], 0x104);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 32);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn fs_truncate_marshals_fd_and_size() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_fs_truncate(0x104, 0x4000), 0);
        });
        assert_eq!(number, NUM_FS_TRUNCATE);
        assert_eq!(args[0], 0x104);
        assert_eq!(args[1], 0x4000);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_sync_marshals_the_descriptor() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_fs_sync(0x104), 0);
        });
        assert_eq!(number, NUM_FS_SYNC);
        assert_eq!(args[0], 0x104);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn fs_mkdir_marshals_path_and_len() {
        let mut path = *b"/Storage/new";
        let ptr = path.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_fs_mkdir(ptr, path.len()), 0);
        });
        assert_eq!(number, NUM_FS_MKDIR);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_unlink_marshals_path_len_and_flags() {
        let mut path = *b"/Storage/old";
        let ptr = path.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_fs_unlink(ptr, path.len(), 0), 0);
        });
        assert_eq!(number, NUM_FS_UNLINK);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
        // The directory-only bit travels in the flags register.
        let dir_bit = u64::from(tairix_abi::UnlinkFlags::DIRECTORY.bits());
        let (number, args) = capture(0, || {
            assert_eq!(
                sys_fs_unlink(ptr, path.len(), tairix_abi::UnlinkFlags::DIRECTORY.bits()),
                0
            );
        });
        assert_eq!(number, NUM_FS_UNLINK);
        assert_eq!(args[2], dir_bit);
    }

    #[test]
    fn fs_set_mode_marshals_path_len_and_mode() {
        let mut path = *b"/Storage/file";
        let ptr = path.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_fs_set_mode(ptr, path.len(), 0o640), 0);
        });
        assert_eq!(number, NUM_FS_SET_MODE);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], 0o640);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn fs_set_owner_marshals_path_len_uid_and_gid() {
        let mut path = *b"/Storage/file";
        let ptr = path.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(
                sys_fs_set_owner(ptr, path.len(), tairix_abi::FS_OWNER_UNCHANGED, 42),
                0
            );
        });
        assert_eq!(number, NUM_FS_SET_OWNER);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], u64::from(tairix_abi::FS_OWNER_UNCHANGED));
        assert_eq!(args[3], 42);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_attr_stubs_marshal_path_key_and_buffers() {
        let mut path = *b"/Storage/file";
        let mut key = *b"user.comment";
        let mut value = *b"hi";
        let mut out = [0u8; 16];
        let path_ptr = path.as_mut_ptr().cast::<c_void>();
        let key_ptr = key.as_mut_ptr().cast::<c_void>();

        let (number, args) = capture(0, || {
            assert_eq!(
                sys_fs_attr_get(
                    path_ptr,
                    path.len(),
                    key_ptr,
                    key.len(),
                    out.as_mut_ptr().cast(),
                    out.len()
                ),
                0
            );
        });
        assert_eq!(number, NUM_FS_ATTR_GET);
        assert_eq!(args[0], path_ptr as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], key_ptr as usize as u64);
        assert_eq!(args[3], key.len() as u64);
        assert_eq!(args[5], out.len() as u64);

        let (number, args) = capture(0, || {
            assert_eq!(
                sys_fs_attr_set(
                    path_ptr,
                    path.len(),
                    key_ptr,
                    key.len(),
                    value.as_mut_ptr().cast(),
                    value.len()
                ),
                0
            );
        });
        assert_eq!(number, NUM_FS_ATTR_SET);
        assert_eq!(args[5], value.len() as u64);

        let (number, args) = capture(0, || {
            assert_eq!(
                sys_fs_attr_list(path_ptr, path.len(), 2, out.as_mut_ptr().cast(), out.len()),
                0
            );
        });
        assert_eq!(number, NUM_FS_ATTR_LIST);
        assert_eq!(args[2], 2);
        assert_eq!(args[4], out.len() as u64);
        assert_eq!(args[5], 0);

        let (number, args) = capture(0, || {
            assert_eq!(
                sys_fs_attr_remove(path_ptr, path.len(), key_ptr, key.len()),
                0
            );
        });
        assert_eq!(number, NUM_FS_ATTR_REMOVE);
        assert_eq!(args[2], key_ptr as usize as u64);
        assert_eq!(args[3], key.len() as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn port_resolve_marshals_name_pointer_and_len() {
        let mut name = *b"desktop.pointer";
        let ptr = name.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(7, || {
            assert_eq!(sys_port_resolve(ptr, name.len()), 7);
        });
        assert_eq!(number, NUM_PORT_RESOLVE);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], name.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_rename_marshals_both_paths_and_lens() {
        let mut src = *b"/Storage/old";
        let mut dst = *b"/Storage/new";
        let src_ptr = src.as_mut_ptr().cast::<c_void>();
        let dst_ptr = dst.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_fs_rename(src_ptr, src.len(), dst_ptr, dst.len()), 0);
        });
        assert_eq!(number, NUM_FS_RENAME);
        assert_eq!(args[0], src_ptr as usize as u64);
        assert_eq!(args[1], src.len() as u64);
        assert_eq!(args[2], dst_ptr as usize as u64);
        assert_eq!(args[3], dst.len() as u64);
        assert_eq!(&args[4..], &[0, 0]);
    }

    #[test]
    fn fs_chdir_marshals_path_and_len() {
        let mut path = *b"/Users/bob";
        let ptr = path.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(0, || {
            assert_eq!(sys_fs_chdir(ptr, path.len()), 0);
        });
        assert_eq!(number, NUM_FS_CHDIR);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn fs_getcwd_marshals_buffer_and_capacity() {
        let mut buffer = [0u8; 64];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(11, || {
            assert_eq!(sys_fs_getcwd(ptr, 64), 11);
        });
        assert_eq!(number, NUM_FS_GETCWD);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], 64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn console_foreground_marshals_fd_and_signed_pid() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_console_foreground(0, 9), 0);
        });
        assert_eq!(number, NUM_CONSOLE_FOREGROUND);
        assert_eq!(args[0], 0);
        assert_eq!(args[1], 9);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
        // The clear sentinel and a (refused kernel-side) negative pid both
        // marshal verbatim — the stub never filters, the kernel decides.
        let (_, args) = capture(0, || {
            assert_eq!(sys_console_foreground(0, 0), 0);
        });
        assert_eq!(args[1], 0);
        let (_, args) = capture(0, || {
            assert_eq!(sys_console_foreground(0, -1), 0);
        });
        // `-1` sign-extends to all-ones in the argument register.
        assert_eq!(args[1], u64::MAX);
    }
}
