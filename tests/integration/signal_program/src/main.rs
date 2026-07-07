//! SPAWN stage `SP7b` fixture program: a minimal, separately-linked pure-Rust
//! EL0 program built in two roles from one source.
//!
//! The consuming vertical (`tests/integration/signal_qemu_aarch64`) compiles
//! this program twice — once as the **child** and once as the **parent** —
//! into two separate, hardware-isolated EL0 address spaces and drives them
//! under the live scheduler (`plans/SPAWN.md` `SP7b`):
//!
//! * the **child** runs forever, giving up the CPU with the `yield` syscall on
//!   each iteration (never exiting on its own), so it only ever ends when its
//!   parent terminates it with a signal;
//! * the **parent** reads the child's PID from its inherited startup argument
//!   (`arg(1)`, which the vertical fills in), then drives the full job-control
//!   sequence through the real syscalls (`plans/SPAWN.md` `SP7b`/`SP9`):
//!   `Signal::Stop` → a `WaitFlags::STOPPED` wait observes the stop without
//!   reaping → `Signal::Continue` resumes → `Signal::Terminate` ends the
//!   child → a blocking wait reaps it and verifies the POSIX-familiar
//!   termination status (143) — returning 0 on success and a distinct
//!   non-zero diagnostic otherwise.
//!
//! The vertical asserts the parent stopped, resumed, terminated, and reaped
//! the child and exited 0, proving signal delivery, the stop overlay, the
//! stopped wait report, and the signalled reap end to end.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `rustos-rt` (which provides `_start`, the stack canary, the panic handler,
//! and the `signal`/`wait`/`yield`/`exit` syscall wrappers), never the C ABI
//! (`crt0` + `abi-sys`), which exists solely for non-Rust programs. It is built
//! position-independent and converted to an `rxe` blob by the consuming test's
//! build script. On the host it is an inert stub so `cargo build --workspace`,
//! clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::{Signal, WaitFlags, WaitStatus};

    /// `true` when this build is the parent role, selected by
    /// `RUSTOS_SIGNAL_ROLE == "parent"`; any other value (including the child
    /// role and an absent variable) builds the child.
    const IS_PARENT: bool = match option_env!("RUSTOS_SIGNAL_ROLE") {
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

    /// Parse `bytes` as a non-negative decimal `i32`, or `None` on an empty
    /// string, a non-digit byte, or overflow. Fail closed — the parent turns
    /// `None` into a distinct diagnostic rather than signalling a guessed PID.
    fn parse_pid(bytes: &[u8]) -> Option<i32> {
        if bytes.is_empty() {
            return None;
        }
        let mut acc: i32 = 0;
        for &b in bytes {
            if !b.is_ascii_digit() {
                return None;
            }
            let digit = i32::from(b - b'0');
            acc = acc.checked_mul(10)?.checked_add(digit)?;
        }
        Some(acc)
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    ///
    /// The child yields forever (it is terminated by the parent's signal, so
    /// this never returns). The parent reads the child's PID, signals it, reaps
    /// it, and verifies the reaped termination status.
    fn main() -> i32 {
        if !IS_PARENT {
            // Child: never exit on our own — give up the CPU each iteration and
            // wait to be terminated by the parent's signal. A tight spin would
            // starve the parent on the single cooperative CPU, so yield.
            loop {
                rustos_rt::yield_now();
            }
        }

        // Parent: the child's PID is the second inherited argument (arg 0 is
        // the program name the vertical chose). Fail closed if it is missing or
        // malformed rather than signalling an arbitrary PID.
        let Some(child_pid) = rustos_rt::arg(1).and_then(parse_pid) else {
            return 20;
        };

        // Stop the running child (`plans/SPAWN.md` SP9): it is parked and
        // held by the stop overlay, not terminated.
        if rustos_rt::signal(child_pid, Signal::Stop) != 0 {
            return 21;
        }

        // A `STOPPED` wait observes the stop — without reaping the child.
        let mut status = WaitStatus::Exited(-1);
        let ret = rustos_rt::wait(child_pid, &mut status, WaitFlags::STOPPED);
        if ret < 0 {
            return 22;
        }
        if ret != i64::from(child_pid) {
            return 23;
        }
        if status != WaitStatus::Stopped(Signal::Stop) {
            return 24;
        }

        // Resume it: a stopped child is still live and signallable.
        if rustos_rt::signal(child_pid, Signal::Continue) != 0 {
            return 25;
        }

        // Deliver a graceful terminate to our (resumed) child. `signal`
        // returns 0 on success, `-errno` otherwise.
        if rustos_rt::signal(child_pid, Signal::Terminate) != 0 {
            return 26;
        }

        // Reap the child and read back the status the kernel recorded for it.
        let mut status: i32 = -1;
        if rustos_rt::wait_exit(child_pid, &mut status) < 0 {
            return 27;
        }

        // A signalled child is reaped with the POSIX-familiar status
        // (`Terminate` reports SIGTERM's 143).
        let Some(expected) = Signal::Terminate.termination_status() else {
            return 28;
        };
        if status != expected {
            return 29;
        }
        // Stopped, resumed, terminated, and reaped our child exactly.
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
