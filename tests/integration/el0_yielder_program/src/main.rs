//! SPAWN stage `SP2c` fixture program: a minimal, separately-linked pure-Rust
//! EL0 program that yields the CPU a fixed number of times then exits 0.
//!
//! The consuming vertical (`tests/integration/spawn_el0_timeshare_qemu_aarch64`)
//! builds **two** instances of this program into two separate, hardware-isolated
//! EL0 address spaces and drives them under the live scheduler. Each instance
//! calls `rustos_rt::yield_now` `RUSTOS_EL0_YIELDS` times — every `yield` is a
//! real `svc` trap that the kernel turns into a context switch back to the
//! dispatcher (`plans/SPAWN.md` SP2) — then returns 0, which the runtime routes
//! through the `exit` syscall. The vertical asserts both instances completed
//! their full yield count and exited, proving a real EL0→EL0 context switch
//! under the live scheduler (the `SP2c` "done when").
//!
//! It is a **pure-Rust** program (`AGENTS.md` §1): it links the Rust userland
//! runtime `rustos-rt` (which provides `_start`, the stack canary, the panic
//! handler, and the `yield`/`exit` syscall wrappers), never the C ABI
//! (`crt0` + `abi-sys`), which exists solely for non-Rust programs
//! (`AGENTS.md` §16.4). It is built position-independent and converted to an
//! `rxe` blob by the consuming test's build script (`AGENTS.md` §9, §19.2). On
//! the host it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    /// Number of times the program yields when the consuming build did not
    /// pin a count via the `RUSTOS_EL0_YIELDS` environment variable. Large
    /// enough that a single accidental run cannot satisfy the vertical's PASS
    /// check, small enough to drain well within the harness budget.
    const DEFAULT_YIELDS: u32 = 16;

    /// The yield count, read from the `RUSTOS_EL0_YIELDS` environment variable
    /// the consuming vertical's build script sets when it compiles this
    /// program, falling back to [`DEFAULT_YIELDS`]. The vertical emits the same
    /// number as a Rust constant for its kernel side, so the build script is
    /// the single source of truth for the count (`AGENTS.md` §2.2).
    const fn yield_count() -> u32 {
        match option_env!("RUSTOS_EL0_YIELDS") {
            Some(s) => parse_u32(s.as_bytes()),
            None => DEFAULT_YIELDS,
        }
    }

    /// Parse `bytes` as a non-negative decimal integer at compile time,
    /// falling back to [`DEFAULT_YIELDS`] on an empty string, a non-digit
    /// byte, or overflow of the `u32` range. `const` and panic-free so the
    /// count is fixed into the image with no runtime parsing
    /// (`AGENTS.md` §2.9 — fail closed to the default).
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

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    ///
    /// Yields the CPU [`yield_count`] times then returns 0. Each `yield` is a
    /// cooperative reschedule point, so when run alongside a sibling EL0 task
    /// the scheduler interleaves the two through real context switches.
    fn main() -> i32 {
        let mut remaining = yield_count();
        while remaining > 0 {
            rustos_rt::yield_now();
            remaining -= 1;
        }
        0
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `rustos-rt` entry path is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
