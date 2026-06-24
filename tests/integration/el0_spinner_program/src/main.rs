//! P-1 preemption fixture program: a minimal, separately-linked pure-Rust EL0
//! program that busy-spins a fixed compile-time number of iterations **without
//! issuing any syscall**, then exits 0.
//!
//! The consuming vertical (`tests/integration/preempt_el0_qemu_aarch64`) spawns
//! this program as a single EL0 task and arms the production timer-IRQ
//! preemption path. Because the spin loop never traps to the kernel (no
//! `yield`, no other `svc`), the *only* way the running task can leave EL0
//! before its final `exit` is an **involuntary** timer-driven preemption
//! (`plans/PI.md` D2b-2b-A P-1). The vertical proves preemption fired (its
//! preempt callback ran while this task was current) *and* that the preempted
//! task was correctly resumed mid-loop (it goes on to complete the spin and
//! exit). A broken preemption path would either never preempt the runaway loop
//! (the vertical's `step` never returns → harness timeout) or resume it wrongly
//! (it never reaches `exit` → harness timeout), so the run fails loudly.
//!
//! The loop body is funnelled through [`core::hint::black_box`] so the compiler
//! cannot fold the spin away to nothing — the program must genuinely execute
//! across multiple timer ticks for the preemption to be observable.
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `rustos-rt` (which provides `_start`, the stack canary, the panic
//! handler, and the `exit` syscall wrapper that routes `main`'s return), never
//! the C ABI (`crt0` + `abi-sys`), which exists solely for non-Rust programs. It is built position-independent and converted to an
//! `rxe` blob by the consuming test's build script. On
//! the host it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    /// Busy-loop iteration count when the consuming build did not pin one via
    /// the `RUSTOS_EL0_SPINS` environment variable. Large enough that, even on
    /// fast QEMU TCG, the loop spans many generic-timer ticks (so at least one
    /// involuntary preemption is guaranteed), yet small enough to drain well
    /// within the vertical's wall-clock budget.
    const DEFAULT_SPINS: u64 = 200_000_000;

    /// The spin count, read from the `RUSTOS_EL0_SPINS` environment variable
    /// the consuming vertical's build script sets when it compiles this
    /// program, falling back to [`DEFAULT_SPINS`]. The build script is the
    /// single source of truth for the count.
    const fn spin_count() -> u64 {
        match option_env!("RUSTOS_EL0_SPINS") {
            Some(s) => parse_u64(s.as_bytes()),
            None => DEFAULT_SPINS,
        }
    }

    /// Parse `bytes` as a non-negative decimal integer at compile time,
    /// falling back to [`DEFAULT_SPINS`] on an empty string, a non-digit byte,
    /// or overflow of the `u64` range. `const` and panic-free so the count is
    /// fixed into the image with no runtime parsing (fail
    /// closed to the default).
    const fn parse_u64(bytes: &[u8]) -> u64 {
        let mut acc: u64 = 0;
        let mut i = 0usize;
        let mut seen = false;
        while i < bytes.len() {
            let b = bytes[i];
            if b < b'0' || b > b'9' {
                return DEFAULT_SPINS;
            }
            let digit = (b - b'0') as u64;
            acc = match acc.checked_mul(10) {
                Some(v) => match v.checked_add(digit) {
                    Some(v) => v,
                    None => return DEFAULT_SPINS,
                },
                None => return DEFAULT_SPINS,
            };
            seen = true;
            i += 1;
        }
        if seen {
            acc
        } else {
            DEFAULT_SPINS
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    ///
    /// Busy-spins [`spin_count`] iterations issuing **no** syscall, then
    /// returns 0. Each iteration is laundered through `black_box` so the loop
    /// is not optimised away and genuinely consumes CPU across timer ticks; the
    /// only way out of EL0 before `exit` is an involuntary preemption.
    fn main() -> i32 {
        let mut remaining = spin_count();
        while remaining > 0 {
            remaining = core::hint::black_box(remaining - 1);
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
