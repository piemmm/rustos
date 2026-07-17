//! `plans/PI.md` stage RV-X3 fixture program: a minimal, separately-linked
//! pure-Rust EL0 program built in two roles from one source.
//!
//! The consuming vertical (`tests/integration/spawn_session_qemu_riscv64`)
//! compiles this program twice — once as the **parent** and once as the
//! **child** (session) — into two separate, hardware-isolated U-mode address
//! spaces and drives them under the live scheduler (`plans/PI.md` RV-X3, the
//! riscv64 sibling of `spawn_session_qemu_aarch64` / `_x86_64`):
//!
//! * the **parent** issues a real `CAP_PROC_SPAWN`-gated `spawn` syscall
//!   (`tairix_rt::spawn`) for the embedded session program, checks the
//!   returned PID is non-negative (a negative return is `-errno`), yields the
//!   CPU a build-pinned number of times so the freshly admitted child
//!   interleaves with it under the cooperative scheduler, then returns 0 — the
//!   spawning caller keeps running (a true concurrent spawn, not an
//!   `exec`-style hand-off);
//! * the **child** (session) yields the CPU the same build-pinned number of
//!   times — every `yield` is a real `ecall` the kernel turns into a context
//!   switch back to the dispatcher — then returns 0.
//!
//! `tairix-rt` routes each role's `main` return value through the `exit`
//! syscall. The vertical asserts the parent's `spawn` built the child a fresh
//! isolated address space and that both the parent and the child ran to `exit`,
//! proving the riscv64 runtime `spawn` concurrent producer end to end (the
//! RV-X3 "done when").
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `tairix-rt` (which provides `_start`, the stack canary, the panic
//! handler, and the `spawn`/`yield`/`exit` syscall wrappers), never the C ABI
//! (`crt0` + `abi-sys`), which exists solely for non-Rust programs. It is built position-independent and converted to an
//! `rxe` blob by the consuming test's build script. On
//! the host it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    /// Absolute path the parent role spawns. It is the registry key the
    /// consuming vertical's runtime `spawn` producer resolves to the child
    /// (session) `rxe`; both halves agree on this byte string (bundle layout single definition).
    const SESSION_PATH: &[u8] = b"/Apps/Session.app/Run";

    /// Yield count when the consuming build did not pin one via
    /// `TAIRIX_SPAWN_YIELDS`. Large enough that a single accidental run cannot
    /// satisfy the vertical's PASS check, small enough to drain well within the
    /// harness budget.
    const DEFAULT_YIELDS: u32 = 8;

    /// The yield count, read from `TAIRIX_SPAWN_YIELDS` (the value the consuming
    /// vertical's build script pins when it compiles both roles), falling back
    /// to [`DEFAULT_YIELDS`]. The vertical is the single source of truth for the
    /// count.
    const YIELDS: u32 = match option_env!("TAIRIX_SPAWN_YIELDS") {
        Some(s) => parse_u32(s.as_bytes()),
        None => DEFAULT_YIELDS,
    };

    /// `true` when this build is the parent role, selected by
    /// `TAIRIX_SPAWN_ROLE == "parent"`; any other value (including the child
    /// role and an absent variable) builds the child (session).
    const IS_PARENT: bool = match option_env!("TAIRIX_SPAWN_ROLE") {
        Some(s) => bytes_eq(s.as_bytes(), b"parent"),
        None => false,
    };

    /// Compile-time byte-string equality (no `core::cmp` in `const`).
    const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Parse `bytes` as a non-negative decimal integer at compile time,
    /// falling back to [`DEFAULT_YIELDS`] on an empty string, a non-digit byte,
    /// or overflow of the `u32` range. `const` and panic-free so the count is
    /// fixed into the image with no runtime parsing (fail
    /// closed to the default).
    const fn parse_u32(bytes: &[u8]) -> u32 {
        let mut acc: u32 = 0;
        let mut i = 0usize;
        let mut seen = false;
        while i < bytes.len() {
            let b = bytes[i];
            if b < b'0' || b > b'9' {
                return DEFAULT_YIELDS;
            }
            let digit = (b - b'0') as u32;
            acc = match acc.checked_mul(10) {
                Some(v) => match v.checked_add(digit) {
                    Some(v) => v,
                    None => return DEFAULT_YIELDS,
                },
                None => return DEFAULT_YIELDS,
            };
            seen = true;
            i += 1;
        }
        if seen {
            acc
        } else {
            DEFAULT_YIELDS
        }
    }

    /// Yield the CPU [`YIELDS`] times. Each `yield` is a cooperative reschedule
    /// point the kernel turns into a context switch back to the dispatcher, so
    /// a sibling task interleaves with this one.
    fn yield_loop() {
        let mut remaining = YIELDS;
        while remaining > 0 {
            tairix_rt::yield_now();
            remaining -= 1;
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    ///
    /// The child simply yields then returns 0. The parent spawns the session,
    /// verifies the spawn succeeded, yields to let the child interleave, and
    /// returns 0 — or a distinct non-zero diagnostic on a failed spawn.
    fn main() -> i32 {
        if !IS_PARENT {
            // Child (session): yield a while so it visibly interleaves with the
            // parent under the cooperative scheduler, then exit cleanly.
            yield_loop();
            return 0;
        }

        // Parent: spawn the session concurrently. A negative return is
        // `-errno`; surface a distinct diagnostic so the vertical fails loudly
        // rather than silently passing on a failed spawn.
        let pid = tairix_rt::spawn(SESSION_PATH);
        if pid < 0 {
            return 12;
        }
        // Keep running alongside the freshly admitted child so the two
        // interleave through real context switches, then exit cleanly.
        yield_loop();
        0
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
