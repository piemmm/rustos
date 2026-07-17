//! wasm32 syscall entry path.
//!
//! A bare-metal port enters the kernel on a privileged instruction
//! (`syscall`/`sysret` on x86_64, `ecall` on riscv64). wasm32 has no
//! privilege boundary inside a single instance; a user thread crosses
//! into the kernel by *calling* the kernel module's exported syscall
//! entry with the syscall number and its argument array. This module
//! owns the architecture-specific slice of that path:
//!
//! * Marshalling the argument words into the architecture-neutral
//!   `tairix_abi` `[u64; SYSCALL_MAX_ARGS]` layout — the same layout the
//!   bare-metal ports build (one ABI, no duplication).
//! * The dispatch callback the entry forwards each syscall to, mirroring
//!   the bare-metal ports' design. The architecture-neutral validation /
//!   capability / audit dispatcher lives in `kernel/syscall` and is
//!   installed by the downstream boot module; the arch port never
//!   re-implements it.
//!
//! # Host testability
//!
//! The argument packing, the callback storage, and [`dispatch_syscall`]
//! all build and are unit-tested on the host.

use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_abi::SYSCALL_MAX_ARGS;

/// Pack the syscall argument words into the canonical `tairix_abi`
/// layout. The order matches the ABI definition pinned in
/// `lib/abi/src/syscalls.rs`, identical to the bare-metal ports'
/// `pack_raw_args` (one ABI).
#[must_use]
pub const fn pack_raw_args(
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
) -> [u64; SYSCALL_MAX_ARGS] {
    [a0, a1, a2, a3, a4, a5]
}

/// Signature of the Rust callback the entry path forwards each syscall
/// to. `number` is the user's syscall number; `args_ptr` points at a
/// `[u64; SYSCALL_MAX_ARGS]` the entry built on its stack. The return
/// value is the syscall result. Identical to the bare-metal ports'
/// `SyscallDispatchFn` so the boot module installs one shim shape.
pub type SyscallDispatchFn =
    extern "C" fn(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64;

/// Atomically-stored dispatch callback (`0` = none installed).
static SYSCALL_DISPATCH_CALLBACK: AtomicUsize = AtomicUsize::new(0);

/// Install the per-binary dispatch callback. Called once during boot,
/// before user space runs. Storing a `fn` (not a closure) keeps it safe
/// to invoke from the syscall entry.
pub fn set_dispatch_callback(cb: SyscallDispatchFn) {
    SYSCALL_DISPATCH_CALLBACK.store(cb as usize, Ordering::Release);
}

/// Read back the installed dispatch callback, if any. Test/diagnostic.
#[must_use]
pub fn dispatch_callback() -> Option<SyscallDispatchFn> {
    let raw = SYSCALL_DISPATCH_CALLBACK.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: every store into `SYSCALL_DISPATCH_CALLBACK`
        // round-trips a valid `SyscallDispatchFn` through
        // `set_dispatch_callback`.
        Some(unsafe { core::mem::transmute::<usize, SyscallDispatchFn>(raw) })
    }
}

#[cfg(test)]
fn clear_dispatch_for_tests() {
    SYSCALL_DISPATCH_CALLBACK.store(0, Ordering::Release);
}

/// Dispatch a syscall to the installed callback.
///
/// Packs `args` into the canonical `tairix_abi` layout and forwards them
/// with `number` to the dispatch callback, returning its result. Returns
/// `None` when no callback is installed — the entry treats that as a
/// fail-closed condition, exactly as the bare-metal
/// ports do.
#[must_use]
pub fn dispatch_syscall(number: u64, args: [u64; SYSCALL_MAX_ARGS]) -> Option<u64> {
    let cb = dispatch_callback()?;
    Some(cb(number, &args))
}

#[cfg(test)]
#[path = "syscall_entry_tests.rs"]
mod tests;
