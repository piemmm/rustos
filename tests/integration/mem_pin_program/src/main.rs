//! STRESSTEST `ST2` memory-pinning fixture: a minimal, separately-linked
//! pure-Rust EL0 program built once and driven in five argv-selected roles.
//!
//! The consuming vertical (`tests/integration/mem_pin_qemu_aarch64`)
//! registers this one `rxe` under role-selecting argument vectors
//! (`tairix_rt::arg(1)`) whose numeric parameters it derives from the one
//! shared stack policy (`spawn_layout`), so no policy constant is ever
//! duplicated into this program:
//!
//! * **`parent`** — reaps the `deny` and `pin` children through the
//!   production `spawn` + `wait` syscalls, then lowers its **own**
//!   `pinned-memory-bytes` bound, pins itself, and spawns `child`: the
//!   child inherits the lowered *limit* but never the pin *mark*, so its
//!   over-budget map must succeed. Exits `0` only when every step behaved.
//! * **`deny`** — runs with no `CAP_MEM_PIN`: `mem_pin` must be refused
//!   `PermissionDenied` (the dispatcher gate, end to end through the real
//!   trap), while the ungated `mem_unpin` still answers success.
//! * **`pin <bound> <within> <over>`** — lowers its own
//!   `pinned-memory-bytes` bound to `bound` through `rlimit_set`, pins
//!   itself twice (idempotent success), asserts a `mem_map` of `over`
//!   bytes is refused `OutOfRange` (the budget, not the producer), maps
//!   `within` bytes successfully, unpins, and asserts the formerly refused
//!   `over` map now succeeds — the bound binds exactly while pinned.
//! * **`child <over>`** — maps `over` bytes (past the parent's pinned
//!   budget) and exits `0`: the pin mark is process-scoped state a spawn
//!   never inherits.
//! * **`ipc`** — repeatedly blocks on the migration test's private call
//!   endpoint, validates each echoed reply, and checks callee-saved integer,
//!   FP, stack, control-flow, and address-space-local state across every
//!   return.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt` (`_start`, stack canary, panic handler, syscall wrappers),
//! never the C ABI. Built position-independent and converted to an `rxe`
//! blob by the consuming test's build script. On the host it is an inert
//! stub so `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use core::sync::atomic::{AtomicU64, Ordering};

    use tairix_abi::{Errno, LimitKind, MapFlags, ResourceLimit};

    /// Private endpoint installed only by the consuming migration chassis.
    const MIGRATION_ENDPOINT: u64 = 0x4d49_4752_4154_4501;
    /// Repeated block/wake cycles in one bounded run.
    const IPC_ROUNDS: u64 = 64;
    /// Address-space-local state that must survive every migration.
    static IPC_STATE: AtomicU64 = AtomicU64::new(0);

    /// The signed `-errno` value a refused syscall surfaces for `err`.
    fn neg(err: Errno) -> i64 {
        -i64::from(err.as_i32())
    }

    /// Parse a decimal `u64` argument, or `None` on any malformed byte
    /// (fail closed — a wiring defect must fail the role, never default).
    fn parse_u64(bytes: &[u8]) -> Option<u64> {
        if bytes.is_empty() {
            return None;
        }
        let mut acc: u64 = 0;
        for &b in bytes {
            if !b.is_ascii_digit() {
                return None;
            }
            acc = acc.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
        }
        Some(acc)
    }

    /// Parse a decimal byte-count argument as the pointer-width length the
    /// map syscall takes, or `None` when it is malformed or larger than this
    /// target's address space can express (fail closed — never truncated).
    fn parse_len(bytes: &[u8]) -> Option<usize> {
        usize::try_from(parse_u64(bytes)?).ok()
    }

    /// The `deny` role body: with no `CAP_MEM_PIN` the pin must be refused
    /// by the dispatcher gate, while the ungated unpin still answers
    /// success (it only narrows the caller's own state).
    fn deny() -> i32 {
        if tairix_rt::mem_pin() != neg(Errno::PermissionDenied) {
            return 20;
        }
        if tairix_rt::mem_unpin() != 0 {
            return 21;
        }
        0
    }

    /// The `pin` role body: the full bound/pin/map/unpin dance under a
    /// self-lowered `pinned-memory-bytes` budget.
    fn pin() -> i32 {
        let Some(bound) = tairix_rt::arg(2).and_then(parse_u64) else {
            return 30;
        };
        let Some(within) = tairix_rt::arg(3).and_then(parse_len) else {
            return 31;
        };
        let Some(over) = tairix_rt::arg(4).and_then(parse_len) else {
            return 32;
        };
        let Ok(limit) = ResourceLimit::new(bound, bound) else {
            return 33;
        };
        // Lowering one's own bound needs no capability.
        if tairix_rt::rlimit_set(LimitKind::PinnedMemoryBytes, limit) != 0 {
            return 34;
        }
        if tairix_rt::mem_pin() != 0 {
            return 35;
        }
        // Already pinned is success: the caller is in the requested state.
        if tairix_rt::mem_pin() != 0 {
            return 36;
        }
        // Past the budget: refused closed by the bound, before the
        // producer is reached.
        if tairix_rt::mem_map(over, MapFlags::empty(), 0) != neg(Errno::OutOfRange) {
            return 37;
        }
        // Inside the budget: a genuine mapping.
        if tairix_rt::mem_map(within, MapFlags::empty(), 0) < 0 {
            return 38;
        }
        if tairix_rt::mem_unpin() != 0 {
            return 39;
        }
        // Unpinned, the same request must now reach the producer and
        // succeed — the bound binds exactly while pinned.
        if tairix_rt::mem_map(over, MapFlags::empty(), 0) < 0 {
            return 40;
        }
        0
    }

    /// The `child` role body: spawned by the *pinned* parent, this process
    /// starts unpinned (the mark is never inherited), so a map past the
    /// parent's pinned budget must succeed.
    fn child() -> i32 {
        let Some(over) = tairix_rt::arg(2).and_then(parse_len) else {
            return 50;
        };
        if tairix_rt::mem_map(over, MapFlags::empty(), 0) < 0 {
            return 51;
        }
        0
    }

    /// Perform one real synchronous call and validate its echo plus the
    /// process-local state carried across the block/wake boundary.
    extern "C" fn ipc_roundtrip(round: u64) -> u64 {
        if IPC_STATE.load(Ordering::Acquire) != round {
            return 61;
        }
        let request = round.to_le_bytes();
        let mut reply = [0u8; 8];
        let Ok(len) = tairix_rt::ipc_call(MIGRATION_ENDPOINT, &request, &mut reply) else {
            return 62;
        };
        if len != reply.len() || reply != request {
            return 63;
        }
        IPC_STATE.store(round + 1, Ordering::Release);
        0
    }

    /// Call [`ipc_roundtrip`] while pinning sentinels in AAPCS64 callee-saved
    /// integer/FP registers and checking the stack returns to the same address.
    #[cfg(mem_pin_aarch64)]
    fn checked_ipc_roundtrip(round: u64) -> u64 {
        let result: u64;
        // SAFETY: this is an aarch64-only test of the ABI state that must
        // survive a blocking syscall migration. The frame is 16-byte aligned,
        // saves/restores the callee-saved registers it temporarily owns, and
        // calls a valid `extern "C"` Rust function through `sym`.
        unsafe {
            core::arch::asm!(
                "sub sp, sp, #32",
                "str x19, [sp, #0]",
                "str d8, [sp, #8]",
                "mov x9, sp",
                "str x9, [sp, #16]",
                "movz x19, #0x7788",
                "movk x19, #0x5566, lsl #16",
                "movk x19, #0x3344, lsl #32",
                "movk x19, #0x1122, lsl #48",
                "movz x9, #0x2d18",
                "movk x9, #0x5444, lsl #16",
                "movk x9, #0x21fb, lsl #32",
                "movk x9, #0x4009, lsl #48",
                "fmov d8, x9",
                "bl {call}",
                "mov x11, x0",
                "movz x9, #0x7788",
                "movk x9, #0x5566, lsl #16",
                "movk x9, #0x3344, lsl #32",
                "movk x9, #0x1122, lsl #48",
                "cmp x19, x9",
                "b.ne 2f",
                "fmov x10, d8",
                "movz x9, #0x2d18",
                "movk x9, #0x5444, lsl #16",
                "movk x9, #0x21fb, lsl #32",
                "movk x9, #0x4009, lsl #48",
                "cmp x10, x9",
                "b.ne 2f",
                "ldr x9, [sp, #16]",
                "mov x10, sp",
                "cmp x10, x9",
                "b.ne 2f",
                "mov x0, x11",
                "b 3f",
                "2:",
                "mov x0, #69",
                "3:",
                "ldr x19, [sp, #0]",
                "ldr d8, [sp, #8]",
                "add sp, sp, #32",
                call = sym ipc_roundtrip,
                inlateout("x0") round => result,
                clobber_abi("C"),
            );
        }
        result
    }

    /// Repeated blocking-IPC migration role.
    fn ipc() -> i32 {
        for round in 0..IPC_ROUNDS {
            #[cfg(mem_pin_aarch64)]
            let result = checked_ipc_roundtrip(round);
            #[cfg(not(mem_pin_aarch64))]
            let result = ipc_roundtrip(round);
            if result != 0 {
                // Every round code is small by construction; one that is not
                // an exit code at all is itself a failure worth reporting.
                return i32::try_from(result).unwrap_or(65);
            }
            // Leave a bounded observation window in EL0 after each reply so
            // the kernel chassis can attest which CPU resumed this task.
            let mut spins = 1_000_000u64;
            while spins != 0 {
                spins = core::hint::black_box(spins - 1);
            }
        }
        if IPC_STATE.load(Ordering::Acquire) != IPC_ROUNDS {
            return 64;
        }
        0
    }

    /// Spawn the child registered at `path` and reap it, asserting it
    /// exited with `0`. Returns `0` on success or `fail_code` (+1/+2) on a
    /// spawn, wait, or exit-code failure.
    fn run_child(path: &[u8], fail_code: i32) -> i32 {
        let Ok(pid) = i32::try_from(tairix_rt::spawn(path)) else {
            return fail_code;
        };
        if pid <= 0 {
            return fail_code;
        }
        let mut code = 0i32;
        if tairix_rt::wait_exit(pid, &mut code) < 0 {
            return fail_code + 1;
        }
        if code != 0 {
            return fail_code + 2;
        }
        0
    }

    /// The `parent` role body: drive `deny` and `pin`, then pin itself
    /// under a lowered bound and prove a spawned child starts unpinned.
    fn parent() -> i32 {
        let Some(bound) = tairix_rt::arg(2).and_then(parse_u64) else {
            return 6;
        };
        let failed = run_child(b"/bin/mp-deny", 10);
        if failed != 0 {
            return failed;
        }
        let failed = run_child(b"/bin/mp-pin", 13);
        if failed != 0 {
            return failed;
        }
        // Pin the parent itself under a lowered budget, then spawn the
        // child: the child inherits the lowered *limit* through the
        // ordinary never-widen intersection but never the pin *mark*, so
        // its over-budget map succeeds while the parent's own would be
        // refused.
        let Ok(limit) = ResourceLimit::new(bound, bound) else {
            return 7;
        };
        if tairix_rt::rlimit_set(LimitKind::PinnedMemoryBytes, limit) != 0 {
            return 8;
        }
        if tairix_rt::mem_pin() != 0 {
            return 9;
        }
        let failed = run_child(b"/bin/mp-child", 16);
        if failed != 0 {
            return failed;
        }
        if tairix_rt::mem_unpin() != 0 {
            return 19;
        }
        0
    }

    /// Program entry point: dispatch on the role argument the registry entry
    /// pinned (`arg(1)`). An absent or unknown role is a wiring defect and a
    /// distinct failure code (fail closed, never a default role).
    fn main() -> i32 {
        match tairix_rt::arg(1) {
            Some(b"parent") => parent(),
            Some(b"deny") => deny(),
            Some(b"pin") => pin(),
            Some(b"child") => child(),
            Some(b"ipc") => ipc(),
            _ => 5,
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the
// crate building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
