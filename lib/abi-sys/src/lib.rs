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
//! The user→kernel trap itself lives once, in `rustos-abi-trap`
//! (`AGENTS.md` §2.2): this crate only marshals each call into register form
//! and hands it to [`rustos_abi_trap::raw_syscall`]. The trap instruction is
//! compiled in only for the three native Tier-1 targets (`x86_64`, `aarch64`,
//! `riscv64`); `wasm32` has no trap instruction and is out of scope for this
//! runtime (`plans/CCOMPAT.md` §1). On the host the entry points still build
//! and link (the marshalling logic is host-tested through the trap crate's
//! injectable `host-seam`), but there is no kernel to service the call.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use core::ffi::c_void;

use rustos_abi::{SyscallNumber, SYSCALL_MAX_ARGS};

use rustos_abi_trap::raw_syscall;

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
const NUM_STREAM_WRITE: u64 = SyscallNumber::STREAM_WRITE.as_u16() as u64;
const NUM_STREAM_READ: u64 = SyscallNumber::STREAM_READ.as_u16() as u64;
const NUM_SPAWN: u64 = SyscallNumber::SPAWN.as_u16() as u64;
const NUM_MEM_MAP: u64 = SyscallNumber::MEM_MAP.as_u16() as u64;
const NUM_MEM_UNMAP: u64 = SyscallNumber::MEM_UNMAP.as_u16() as u64;
const NUM_WAIT: u64 = SyscallNumber::WAIT.as_u16() as u64;
const NUM_RLIMIT_GET: u64 = SyscallNumber::RLIMIT_GET.as_u16() as u64;
const NUM_RLIMIT_SET: u64 = SyscallNumber::RLIMIT_SET.as_u16() as u64;
const NUM_USERS_DB_READ: u64 = SyscallNumber::USERS_DB_READ.as_u16() as u64;
const NUM_CONSOLE_COUNT: u64 = SyscallNumber::CONSOLE_COUNT.as_u16() as u64;
const NUM_STREAM_ECHO: u64 = SyscallNumber::STREAM_ECHO.as_u16() as u64;
const NUM_KEY_INJECT: u64 = SyscallNumber::KEY_INJECT.as_u16() as u64;
const NUM_DISPLAY_ACQUIRE: u64 = SyscallNumber::DISPLAY_ACQUIRE.as_u16() as u64;
const NUM_DISPLAY_RELEASE: u64 = SyscallNumber::DISPLAY_RELEASE.as_u16() as u64;
const NUM_KEYBOARD_READ: u64 = SyscallNumber::KEYBOARD_READ.as_u16() as u64;
const NUM_MMIO_MAP: u64 = SyscallNumber::MMIO_MAP.as_u16() as u64;
const NUM_DMA_ALLOC: u64 = SyscallNumber::DMA_ALLOC.as_u16() as u64;
const NUM_RESOURCE_GRANTS: u64 = SyscallNumber::RESOURCE_GRANTS.as_u16() as u64;
const NUM_HW_TREE_READ: u64 = SyscallNumber::HW_TREE_READ.as_u16() as u64;
const NUM_HW_TREE_WAIT: u64 = SyscallNumber::HW_TREE_WAIT.as_u16() as u64;

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

/// `stream_write`: write `len` bytes at `buf` to the calling process's
/// standard stream `fd` (`SyscallNumber::STREAM_WRITE`). Returns the number
/// of bytes written.
///
/// The kernel resolves `fd` against the caller's inherited descriptor table
/// (`AGENTS.md` §20) — the descriptor, not an ambient device, is the
/// authority — and validates the `(buf, len)` pair against the caller's
/// address space before touching it (`AGENTS.md` §5.4). A short write (fewer
/// than `len`) is valid, so the caller loops.
#[must_use]
#[export_name = "ros_sys_stream_write"]
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
/// The kernel resolves `fd` against the caller's inherited descriptor table
/// (`AGENTS.md` §20) and validates the `(buf, len)` pair against the
/// caller's address space before writing it (`AGENTS.md` §5.4). The read
/// counterpart of `stream_write`: a short read (fewer than `len`, possibly
/// zero when no input is pending) is valid, so the caller loops.
#[must_use]
#[export_name = "ros_sys_stream_read"]
pub extern "C" fn sys_stream_read(fd: u32, buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(buf, len)`.
    unsafe {
        raw_syscall(
            NUM_STREAM_READ,
            [u64::from(fd), ptr_arg(buf), len as u64, 0, 0, 0],
        )
    }
}

/// `spawn`: spawn a new process from the embedded program named by the
/// absolute path `(path, path_len)` (`SyscallNumber::SPAWN`). Returns the
/// new process's PID, or a `ROS_E_*` code reinterpreted into the result.
///
/// Requires `CAP_PROC_SPAWN`; the kernel validates the capability and the
/// `(path, path_len)` pair against the caller's address space before
/// reading it (`AGENTS.md` §5.4). The caller keeps running — this is a
/// true concurrent spawn, not an `exec`-style hand-off (`plans/SPAWN.md`
/// SP3). `console` selects the child's standard-stream attachment
/// (`AGENTS.md` §20): `ROS_CONSOLE_INHERIT` keeps the child on the
/// caller's own console; any other value names an installed console index
/// (see `ros_sys_console_count`) and an index with no console fails
/// closed.
#[must_use]
#[export_name = "ros_sys_spawn"]
pub extern "C" fn sys_spawn(path: *mut c_void, path_len: usize, console: u64) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates `(path, path_len)`
    // and the console selector.
    unsafe {
        raw_syscall(
            NUM_SPAWN,
            [ptr_arg(path), path_len as u64, console, 0, 0, 0],
        )
    }
}

/// `console_count`: report how many system text consoles are installed
/// (`SyscallNumber::CONSOLE_COUNT`, `AGENTS.md` §20). Returns the count,
/// or a `ROS_E_*` code reinterpreted into the result.
///
/// Gated kernel-side on `ROS_CAP_CONSOLE_WRITE`. The count is the index
/// space `ros_sys_spawn`'s `console` argument selects from — each entry
/// is an independent text console with its own session context
/// (`plans/PI.md` P11).
#[must_use]
#[export_name = "ros_sys_console_count"]
pub extern "C" fn sys_console_count() -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here.
    unsafe { raw_syscall(NUM_CONSOLE_COUNT, [0, 0, 0, 0, 0, 0]) }
}

/// `stream_echo`: set whether the input stream `fd` echoes the bytes it
/// reads back to its console (`SyscallNumber::STREAM_ECHO`, `AGENTS.md`
/// §20 — terminal local echo). `enabled` is `0` to disable, non-zero to
/// enable. Returns a `ROS_E_*` code.
///
/// Gated kernel-side on `ROS_CAP_CONSOLE_READ`; the kernel performs the
/// echo itself as part of the read line discipline, so no
/// `ROS_CAP_CONSOLE_WRITE` is needed. Console echo defaults to **on**; a
/// program suppresses it around a secret it must not render (a password
/// prompt) and restores it afterwards (`AGENTS.md` §5.4).
#[must_use]
#[export_name = "ros_sys_stream_echo"]
pub extern "C" fn sys_stream_echo(fd: u32, enabled: u32) -> i32 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here.
    unsafe {
        ret_i32(raw_syscall(
            NUM_STREAM_ECHO,
            [u64::from(fd), u64::from(enabled), 0, 0, 0, 0],
        ))
    }
}

/// `key_inject`: inject one decoded keyboard key edge at `buf` (a
/// `ros_key_input_t` record of `len` bytes) into the kernel input-focus
/// arbiter (`SyscallNumber::KEY_INJECT`, `AGENTS.md` §20, `plans/PI.md`
/// P11 — input follows the surface owner). Returns the number of bytes
/// consumed, or a `ROS_E_*` code reinterpreted into the result.
///
/// The producer-side call a keyboard-input driver issues after decoding a
/// directly attached keyboard into a key edge. Gated kernel-side on
/// `ROS_CAP_INPUT_INJECT`; the kernel validates the capability and the
/// `(buf, len)` pair against the caller's address space before reading it
/// (`AGENTS.md` §5.4), decodes the record fail-closed, and routes it by who
/// currently holds input focus — the driver no longer chooses the encoding
/// or the destination (`AGENTS.md` §17.4).
#[must_use]
#[export_name = "ros_sys_key_inject"]
pub extern "C" fn sys_key_inject(buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`. The kernel validates `CAP_INPUT_INJECT`
    // and the `(buf, len)` pair against the caller's address space before
    // reading it (`AGENTS.md` §5.4).
    unsafe { raw_syscall(NUM_KEY_INJECT, [ptr_arg(buf), len as u64, 0, 0, 0, 0]) }
}

/// `display_acquire`: acquire ownership of the display and claim keyboard
/// input focus (`SyscallNumber::DISPLAY_ACQUIRE`, `AGENTS.md` §10 / §17.3 /
/// §20, `plans/PI.md` P11). Returns a `ROS_E_*` code (`0` on success).
///
/// The compositing window manager calls this when it takes over the screen:
/// the kernel input-focus arbiter switches its foreground to the desktop
/// keyboard channel, so injected key edges are delivered as records the
/// manager drains with [`sys_keyboard_read`]. Gated kernel-side on
/// `ROS_CAP_DISPLAY` (`AGENTS.md` §4 — owning the display is privileged).
#[must_use]
#[export_name = "ros_sys_display_acquire"]
pub extern "C" fn sys_display_acquire() -> i32 {
    // SAFETY: see `sys_yield`. The call carries no pointers; the kernel
    // validates `CAP_DISPLAY` before touching any state (`AGENTS.md` §5.4).
    unsafe { ret_i32(raw_syscall(NUM_DISPLAY_ACQUIRE, NO_ARGS)) }
}

/// `display_release`: release the display and return keyboard input focus to
/// the text console (`SyscallNumber::DISPLAY_RELEASE`, `AGENTS.md` §10 /
/// §17.3 / §20, `plans/PI.md` P11). Returns a `ROS_E_*` code (`0` on
/// success).
///
/// The inverse of [`sys_display_acquire`]; gated kernel-side on
/// `ROS_CAP_DISPLAY`.
#[must_use]
#[export_name = "ros_sys_display_release"]
pub extern "C" fn sys_display_release() -> i32 {
    // SAFETY: see `sys_yield`. The call carries no pointers; the kernel
    // validates `CAP_DISPLAY` before touching any state (`AGENTS.md` §5.4).
    unsafe { ret_i32(raw_syscall(NUM_DISPLAY_RELEASE, NO_ARGS)) }
}

/// `keyboard_read`: read one decoded keyboard event from the kernel keyboard
/// channel into `buf` (a buffer of `len` bytes, at least one
/// `ros_key_input_t` record) (`SyscallNumber::KEYBOARD_READ`, `AGENTS.md`
/// §10, `plans/PI.md` P11). Returns the number of bytes written — one
/// record, or `0` when the channel is momentarily drained — or a `ROS_E_*`
/// code reinterpreted into the result.
///
/// The principal that owns the display (the window manager) drains the
/// records the arbiter routed to it while it held focus. Gated kernel-side
/// on `ROS_CAP_INPUT_READ`; the kernel validates the capability and the
/// `(buf, len)` pair against the caller's address space before writing it
/// (`AGENTS.md` §5.4), and a buffer too small to hold a record fails closed
/// (`AGENTS.md` §2.9).
#[must_use]
#[export_name = "ros_sys_keyboard_read"]
pub extern "C" fn sys_keyboard_read(buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`. The kernel validates `CAP_INPUT_READ` and
    // the `(buf, len)` pair against the caller's address space before writing
    // it (`AGENTS.md` §5.4).
    unsafe { raw_syscall(NUM_KEYBOARD_READ, [ptr_arg(buf), len as u64, 0, 0, 0, 0]) }
}

/// `resource_grants`: enumerate the device-resource grants the kernel minted
/// for the calling driver task into `buf` (a buffer of `len` bytes)
/// (`SyscallNumber::RESOURCE_GRANTS`, `AGENTS.md` §4 / §18.3 / §20,
/// `plans/PI.md` P10 chunk 5d-2). Returns the total number of bytes written
/// — consecutive `ros_granted_resource` records — or a `ROS_E_*` code
/// reinterpreted into the result.
///
/// A driver process calls this once at start-up to learn the unforgeable
/// handles it passes to [`sys_mmio_map`] / [`sys_dma_alloc`]. It needs no
/// capability (a task reads only its *own* grants); the kernel validates the
/// `(buf, len)` pair against the caller's address space before writing it
/// (`AGENTS.md` §5.4), and a buffer too small for the whole grant set fails
/// closed (`AGENTS.md` §2.9).
#[must_use]
#[export_name = "ros_sys_resource_grants"]
pub extern "C" fn sys_resource_grants(buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`. The kernel validates the `(buf, len)` pair
    // against the caller's address space before writing it (`AGENTS.md` §5.4).
    unsafe { raw_syscall(NUM_RESOURCE_GRANTS, [ptr_arg(buf), len as u64, 0, 0, 0, 0]) }
}

/// `mmio_map`: map a granted device MMIO register window into the calling
/// driver's own address space (`SyscallNumber::MMIO_MAP`, `AGENTS.md` §4 /
/// §18.3, `plans/PI.md` P10 chunk 5d-0). Returns the base virtual address of
/// the mapping, or a `ROS_E_*` code reinterpreted into the result.
///
/// `handle` is an unforgeable, kernel-issued device-resource grant the driver
/// received for the hardware-tree node it binds — never a raw physical
/// address. The kernel resolves it against the calling task, confirms it
/// names a memory window, and maps only that region (caching disabled); a
/// forged/non-owned handle, a wrong-kind grant, or a build with no map
/// facility wired fails closed (`AGENTS.md` §2.9 / §5.4). Gated kernel-side
/// on `ROS_CAP_MMIO_MAP`.
#[must_use]
#[export_name = "ros_sys_mmio_map"]
pub extern "C" fn sys_mmio_map(handle: u64) -> u64 {
    // SAFETY: see `sys_yield`. No user pointer is dereferenced here; the
    // kernel resolves the grant handle against the caller and returns the
    // mapped base virtual address.
    unsafe { raw_syscall(NUM_MMIO_MAP, [handle, 0, 0, 0, 0, 0]) }
}

/// `dma_alloc`: carve a coherent DMA buffer for the calling driver, bounded
/// by a granted device DMA constraint (`SyscallNumber::DMA_ALLOC`,
/// `AGENTS.md` §4 / §18.3, `plans/PI.md` P10 chunk 5d-0). Writes the
/// buffer's device-visible base address to `device_out` and returns the base
/// virtual address of the mapping, or a `ROS_E_*` code reinterpreted into the
/// result.
///
/// `handle` is an unforgeable, kernel-issued device-resource grant the driver
/// received for the hardware-tree node it binds — never a raw physical
/// address. The kernel resolves it against the calling task, confirms it
/// names a DMA constraint, carves a physically-contiguous, zeroed, coherent
/// region of `len` bytes whose physical extent lies within the grant's
/// addressing limit, and maps it `RW`, non-executable, into the caller's own
/// address space; a forged/non-owned handle, a wrong-kind grant, an
/// over-limit request, or a build with no DMA facility wired fails closed
/// (`AGENTS.md` §2.9 / §5.4). Gated kernel-side on `ROS_CAP_MEM_DMA`.
#[must_use]
#[export_name = "ros_sys_dma_alloc"]
pub extern "C" fn sys_dma_alloc(handle: u64, len: usize, device_out: *mut c_void) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `device_out`
    // pointer against the caller's address space before writing the
    // device-visible base to it (`AGENTS.md` §5.4).
    unsafe {
        raw_syscall(
            NUM_DMA_ALLOC,
            [handle, len as u64, ptr_arg(device_out), 0, 0, 0],
        )
    }
}

/// `mem_map`: map `len` bytes of fresh anonymous `RW` memory into the
/// calling process's own address space, honouring `flags`
/// ([`rustos_abi::MapFlags`]) and the placement hint `addr_hint`
/// (`SyscallNumber::MEM_MAP`). Returns the base address of the new region.
///
/// The kernel validates every argument and fails closed (`AGENTS.md` §5.4);
/// the region is zeroed before it is visible and is never executable
/// (`AGENTS.md` §19.2). An out-of-memory condition is reported as a
/// `ROS_E_*` code reinterpreted into the result (`plans/SPAWN.md` SP5).
#[must_use]
#[export_name = "ros_sys_mem_map"]
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
/// (`SyscallNumber::MEM_UNMAP`). Returns a `ROS_E_*` code.
#[must_use]
#[export_name = "ros_sys_mem_unmap"]
pub extern "C" fn sys_mem_unmap(base: u64, len: usize) -> i32 {
    // SAFETY: see `sys_yield`. The kernel validates the `(base, len)` range
    // against the caller's address space before unmapping it.
    unsafe { ret_i32(raw_syscall(NUM_MEM_UNMAP, [base, len as u64, 0, 0, 0, 0])) }
}

/// `wait`: wait for a child of the calling process to exit, reaping it and
/// writing its exit code to `status` (`SyscallNumber::WAIT`). Returns the
/// reaped child's PID, or a `ROS_E_*` code reinterpreted into the result.
///
/// `pid` is either a specific child's PID or [`rustos_abi::WAIT_ANY`] to
/// wait for any child. A process may only wait on its **own** children; the
/// kernel validates the parent/child relationship and the `status` pointer
/// before writing to it (`AGENTS.md` §4 / §5.4), and fails closed
/// (`plans/SPAWN.md` SP6).
#[must_use]
#[export_name = "ros_sys_wait"]
pub extern "C" fn sys_wait(pid: i32, status: *mut c_void) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `status` pointer
    // against the caller's address space before writing the exit code to it.
    unsafe { raw_syscall(NUM_WAIT, [i32_arg(pid), ptr_arg(status), 0, 0, 0, 0]) }
}

/// `rlimit_get`: read the calling process's effective limit for resource
/// `kind`, writing the encoded `ros_resource_limit_t` to `out`
/// (`SyscallNumber::RLIMIT_GET`). Returns a `ROS_E_*` code (`AGENTS.md`
/// §24.3).
#[must_use]
#[export_name = "ros_sys_rlimit_get"]
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
/// from the encoded `ros_resource_limit_t` at `value`
/// (`SyscallNumber::RLIMIT_SET`). Returns a `ROS_E_*` code; raising a hard
/// bound requires `CAP_RLIMIT_RAISE` (`AGENTS.md` §24.3).
#[must_use]
#[export_name = "ros_sys_rlimit_set"]
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
/// count, or a `ROS_E_*` code reinterpreted into the result.
///
/// Gated kernel-side on `ROS_CAP_USERS_READ` — only the authentication
/// principal (login) holds it (`AGENTS.md` §4 / §5.4). A buffer smaller
/// than the database is refused whole (`ROS_E_BUFFER_TOO_SMALL`) — a
/// credential database is never truncated (`AGENTS.md` §2.9); sizing the
/// buffer at the format's 64 KiB maximum always suffices.
#[must_use]
#[export_name = "ros_sys_users_db_read"]
pub extern "C" fn sys_users_db_read(buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `(buf, len)` pair
    // against the caller's address space before writing the text to it.
    unsafe { raw_syscall(NUM_USERS_DB_READ, [ptr_arg(buf), len as u64, 0, 0, 0, 0]) }
}

/// `hw_tree_read`: copy the discovered hardware tree the kernel built at
/// boot into the caller's `(buf, len)` buffer
/// (`SyscallNumber::HW_TREE_READ`, `AGENTS.md` §16.6 / §18.1 / §18.4).
/// Returns the byte count, or a `ROS_E_*` code reinterpreted into the
/// result.
///
/// The bytes are a `ros_hw_tree_header_t` (the store's current generation
/// and node count) followed by that many `ros_hw_node_t` records. The
/// generation in the header is the value to pass to `ros_sys_hw_tree_wait`
/// to block until the tree next changes. Gated kernel-side on
/// `ROS_CAP_SYSINFO_HW` — the privileged global hardware view
/// (`AGENTS.md` §16.6 / §18.4). The whole inventory is copied or none: a
/// buffer smaller than the snapshot is refused with `ROS_E_BUFFER_TOO_SMALL`
/// rather than truncated (`AGENTS.md` §2.9), so the caller grows `buf` and
/// retries (`AGENTS.md` §24.1).
#[must_use]
#[export_name = "ros_sys_hw_tree_read"]
pub extern "C" fn sys_hw_tree_read(buf: *mut c_void, len: usize) -> u64 {
    // SAFETY: see `sys_ipc_send`; the kernel validates the `(buf, len)` pair
    // against the caller's address space before writing the tree to it.
    unsafe { raw_syscall(NUM_HW_TREE_READ, [ptr_arg(buf), len as u64, 0, 0, 0, 0]) }
}

/// `hw_tree_wait`: block until the discovered hardware tree changes past
/// `last_generation` (`SyscallNumber::HW_TREE_WAIT`, `AGENTS.md` §18.4 —
/// reactive re-match and hotplug). Returns a `ROS_E_*` code.
///
/// `last_generation` is the generation last observed through
/// `ros_sys_hw_tree_read`'s header; `timeout_ns` bounds the wait
/// (`UINT64_MAX` for an effectively unbounded block). Returns `0` once the
/// tree has changed, `ROS_E_TIMED_OUT` if the deadline elapses first, or
/// `ROS_E_NOT_IMPLEMENTED` if no hardware-tree store is wired. Gated
/// kernel-side on `ROS_CAP_SYSINFO_HW`, the same privilege as reading the
/// tree.
#[must_use]
#[export_name = "ros_sys_hw_tree_wait"]
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

#[cfg(test)]
mod tests {
    use super::*;
    // The trap seam lives in `rustos-abi-trap` (the single trap home,
    // `AGENTS.md` §2.2) and is reached here through the `host-seam`
    // dev-dependency feature; production builds never compile it.
    use rustos_abi::SYSCALLS;
    use rustos_abi_trap::seam;

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
        (NUM_STREAM_WRITE, "stream_write", 3),
        (NUM_SPAWN, "spawn", 3),
        (NUM_STREAM_READ, "stream_read", 3),
        (NUM_MEM_MAP, "mem_map", 3),
        (NUM_MEM_UNMAP, "mem_unmap", 2),
        (NUM_WAIT, "wait", 2),
        (NUM_RLIMIT_GET, "rlimit_get", 2),
        (NUM_RLIMIT_SET, "rlimit_set", 2),
        (NUM_USERS_DB_READ, "users_db_read", 2),
        (NUM_CONSOLE_COUNT, "console_count", 0),
        (NUM_STREAM_ECHO, "stream_echo", 2),
        (NUM_KEY_INJECT, "key_inject", 2),
        (NUM_DISPLAY_ACQUIRE, "display_acquire", 0),
        (NUM_DISPLAY_RELEASE, "display_release", 0),
        (NUM_KEYBOARD_READ, "keyboard_read", 2),
        (NUM_MMIO_MAP, "mmio_map", 1),
        (NUM_DMA_ALLOC, "dma_alloc", 3),
        (NUM_RESOURCE_GRANTS, "resource_grants", 2),
        (NUM_HW_TREE_READ, "hw_tree_read", 2),
        (NUM_HW_TREE_WAIT, "hw_tree_wait", 2),
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
    fn mmio_map_marshals_the_grant_handle() {
        let (number, args) = capture(0x9000_0000, || {
            assert_eq!(sys_mmio_map(0x2A), 0x9000_0000);
        });
        assert_eq!(number, NUM_MMIO_MAP);
        assert_eq!(args[0], 0x2A);
        assert_eq!(&args[1..], &[0, 0, 0, 0, 0]);
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
    fn stream_read_marshals_fd_pointer_and_len() {
        let mut buffer = [0u8; 8];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(5, || {
            assert_eq!(sys_stream_read(0, ptr, 8), 5);
        });
        assert_eq!(number, NUM_STREAM_READ);
        assert_eq!(args[0], 0);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(args[2], 8);
        assert_eq!(&args[3..], &[0, 0, 0]);
    }

    #[test]
    fn spawn_marshals_path_pointer_len_and_console() {
        let mut path = *b"/Apps/Child.app/Run";
        let ptr = path.as_mut_ptr().cast::<c_void>();
        let (number, args) = capture(7, || {
            assert_eq!(sys_spawn(ptr, path.len(), rustos_abi::CONSOLE_INHERIT), 7);
        });
        assert_eq!(number, NUM_SPAWN);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], path.len() as u64);
        assert_eq!(args[2], rustos_abi::CONSOLE_INHERIT);
        assert_eq!(&args[3..], &[0, 0, 0]);
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
    fn stream_echo_marshals_fd_and_enabled_flag() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_stream_echo(0, 1), 0);
        });
        assert_eq!(number, NUM_STREAM_ECHO);
        assert_eq!(args[0], 0);
        assert_eq!(args[1], 1);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);

        // A zero flag disables echo.
        let (_, args) = capture(0, || {
            assert_eq!(sys_stream_echo(0, 0), 0);
        });
        assert_eq!(args[1], 0);
    }

    #[test]
    fn key_inject_marshals_pointer_and_len() {
        let mut record = [0u8; 8];
        let ptr = record.as_mut_ptr().cast::<c_void>();
        let len = record.len();
        // The kernel returns the number of bytes consumed.
        let (number, args) = capture(len as u64, || {
            assert_eq!(sys_key_inject(ptr, len), len as u64);
        });
        assert_eq!(number, NUM_KEY_INJECT);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], len as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn display_acquire_and_release_marshal_no_arguments() {
        let (number, args) = capture(0, || {
            assert_eq!(sys_display_acquire(), 0);
        });
        assert_eq!(number, NUM_DISPLAY_ACQUIRE);
        assert_eq!(args, NO_ARGS);

        let (number, args) = capture(0, || {
            assert_eq!(sys_display_release(), 0);
        });
        assert_eq!(number, NUM_DISPLAY_RELEASE);
        assert_eq!(args, NO_ARGS);
    }

    #[test]
    fn keyboard_read_marshals_pointer_and_len() {
        let mut buf = [0u8; 8];
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        let len = buf.len();
        // The kernel returns the number of bytes written (one record).
        let (number, args) = capture(len as u64, || {
            assert_eq!(sys_keyboard_read(ptr, len), len as u64);
        });
        assert_eq!(number, NUM_KEYBOARD_READ);
        assert_eq!(args[0], ptr as usize as u64);
        assert_eq!(args[1], len as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
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
    fn wait_marshals_pid_and_status_pointer() {
        let mut status = 0i32;
        let ptr = core::ptr::addr_of_mut!(status).cast::<c_void>();
        // The kernel returns the reaped child's PID.
        let (number, args) = capture(5, || {
            assert_eq!(sys_wait(9, ptr), 5);
        });
        assert_eq!(number, NUM_WAIT);
        assert_eq!(args[0], 9);
        assert_eq!(args[1], ptr as usize as u64);
        assert_eq!(&args[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn wait_sign_extends_wait_any() {
        let mut status = 0i32;
        let ptr = core::ptr::addr_of_mut!(status).cast::<c_void>();
        let (number, args) = capture(3, || {
            let _ = sys_wait(rustos_abi::WAIT_ANY, ptr);
        });
        assert_eq!(number, NUM_WAIT);
        // `WAIT_ANY` (-1) sign-extends to all-ones in the argument register.
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
