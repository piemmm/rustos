//! STRESSTEST `ST2` memory-pinning fixture: a minimal, separately-linked
//! pure-Rust EL0 program built once and driven in four argv-selected roles.
//!
//! The consuming vertical (`tests/integration/mem_pin_qemu_aarch64`)
//! registers this one `rxe` under role-selecting argument vectors
//! (`rustos_rt::arg(1)`) whose numeric parameters it derives from the one
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
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `rustos-rt` (`_start`, stack canary, panic handler, syscall wrappers),
//! never the C ABI. Built position-independent and converted to an `rxe`
//! blob by the consuming test's build script. On the host it is an inert
//! stub so `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::{Errno, LimitKind, MapFlags, ResourceLimit};

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

    /// The `deny` role body: with no `CAP_MEM_PIN` the pin must be refused
    /// by the dispatcher gate, while the ungated unpin still answers
    /// success (it only narrows the caller's own state).
    fn deny() -> i32 {
        if rustos_rt::mem_pin() != neg(Errno::PermissionDenied) {
            return 20;
        }
        if rustos_rt::mem_unpin() != 0 {
            return 21;
        }
        0
    }

    /// The `pin` role body: the full bound/pin/map/unpin dance under a
    /// self-lowered `pinned-memory-bytes` budget.
    fn pin() -> i32 {
        let Some(bound) = rustos_rt::arg(2).and_then(parse_u64) else {
            return 30;
        };
        let Some(within) = rustos_rt::arg(3).and_then(parse_u64) else {
            return 31;
        };
        let Some(over) = rustos_rt::arg(4).and_then(parse_u64) else {
            return 32;
        };
        let Ok(limit) = ResourceLimit::new(bound, bound) else {
            return 33;
        };
        // Lowering one's own bound needs no capability.
        if rustos_rt::rlimit_set(LimitKind::PinnedMemoryBytes, limit) != 0 {
            return 34;
        }
        if rustos_rt::mem_pin() != 0 {
            return 35;
        }
        // Already pinned is success: the caller is in the requested state.
        if rustos_rt::mem_pin() != 0 {
            return 36;
        }
        // Past the budget: refused closed by the bound, before the
        // producer is reached.
        if rustos_rt::mem_map(over as usize, MapFlags::empty(), 0) != neg(Errno::OutOfRange) {
            return 37;
        }
        // Inside the budget: a genuine mapping.
        if rustos_rt::mem_map(within as usize, MapFlags::empty(), 0) < 0 {
            return 38;
        }
        if rustos_rt::mem_unpin() != 0 {
            return 39;
        }
        // Unpinned, the same request must now reach the producer and
        // succeed — the bound binds exactly while pinned.
        if rustos_rt::mem_map(over as usize, MapFlags::empty(), 0) < 0 {
            return 40;
        }
        0
    }

    /// The `child` role body: spawned by the *pinned* parent, this process
    /// starts unpinned (the mark is never inherited), so a map past the
    /// parent's pinned budget must succeed.
    fn child() -> i32 {
        let Some(over) = rustos_rt::arg(2).and_then(parse_u64) else {
            return 50;
        };
        if rustos_rt::mem_map(over as usize, MapFlags::empty(), 0) < 0 {
            return 51;
        }
        0
    }

    /// Spawn the child registered at `path` and reap it, asserting it
    /// exited with `0`. Returns `0` on success or `fail_code` (+1/+2) on a
    /// spawn, wait, or exit-code failure.
    fn run_child(path: &[u8], fail_code: i32) -> i32 {
        let pid = rustos_rt::spawn(path);
        if pid <= 0 {
            return fail_code;
        }
        #[allow(clippy::cast_possible_truncation)]
        let pid = pid as i32;
        let mut code = 0i32;
        if rustos_rt::wait_exit(pid, &mut code) < 0 {
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
        let Some(bound) = rustos_rt::arg(2).and_then(parse_u64) else {
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
        if rustos_rt::rlimit_set(LimitKind::PinnedMemoryBytes, limit) != 0 {
            return 8;
        }
        if rustos_rt::mem_pin() != 0 {
            return 9;
        }
        let failed = run_child(b"/bin/mp-child", 16);
        if failed != 0 {
            return failed;
        }
        if rustos_rt::mem_unpin() != 0 {
            return 19;
        }
        0
    }

    /// Program entry point: dispatch on the role argument the registry entry
    /// pinned (`arg(1)`). An absent or unknown role is a wiring defect and a
    /// distinct failure code (fail closed, never a default role).
    fn main() -> i32 {
        match rustos_rt::arg(1) {
            Some(b"parent") => parent(),
            Some(b"deny") => deny(),
            Some(b"pin") => pin(),
            Some(b"child") => child(),
            _ => 5,
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `rustos-rt` entry path is not compiled, so this inert `main` keeps the
// crate building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
