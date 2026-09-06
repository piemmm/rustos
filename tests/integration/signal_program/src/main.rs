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
//! * the **intake** role (`plans/STRESSTEST.md` ST3) opts into signal
//!   observation (`signal_intake(Enable)`), drains its intake with a
//!   `Take`/`yield` loop until the first observed `Interrupt` arrives —
//!   proving a foreground `^C` delivered through the real console line
//!   discipline is *observed, not fatal* — then deliberately stops draining
//!   and yields forever, so the vertical can prove the second-pending
//!   `Interrupt` escalates to the default terminate path (the 130 reap);
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
//! `tairix-rt` (which provides `_start`, the stack canary, the panic handler,
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
    use tairix_abi::{Signal, WaitFlags, WaitStatus};

    /// `true` when this build is the parent role, selected by
    /// `TAIRIX_SIGNAL_ROLE == "parent"`; any other value (including the child
    /// role and an absent variable) builds the child.
    const IS_PARENT: bool = match option_env!("TAIRIX_SIGNAL_ROLE") {
        Some(s) => bytes_eq(s.as_bytes(), b"parent"),
        None => false,
    };

    /// `true` when this build is the intake role (`plans/STRESSTEST.md` ST3),
    /// selected by `TAIRIX_SIGNAL_ROLE == "intake"`.
    const IS_INTAKE: bool = match option_env!("TAIRIX_SIGNAL_ROLE") {
        Some(s) => bytes_eq(s.as_bytes(), b"intake"),
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
    fn parse_pid(bytes: &[u8]) -> Option<i64> {
        if bytes.is_empty() {
            return None;
        }
        let mut acc: i64 = 0;
        for &b in bytes {
            if !b.is_ascii_digit() {
                return None;
            }
            let digit = i64::from(b - b'0');
            acc = acc.checked_mul(10)?.checked_add(digit)?;
        }
        Some(acc)
    }

    /// The intake role's body: opt in, observe the first `Interrupt`, then
    /// stop draining so the vertical can prove the escalation rule. Returns
    /// a diagnostic only on a failure — the success path never returns (the
    /// program ends terminated by the escalated second `^C`).
    fn run_intake() -> i32 {
        use tairix_abi::{Errno, SignalIntakeOp};
        // Opt in: with the intake enabled a foreground `^C` is recorded as
        // an observable event instead of terminating this process.
        if tairix_rt::signal_intake(SignalIntakeOp::Enable) != 0 {
            return 30;
        }
        // Drain until the first observed Interrupt arrives. `WouldBlock`
        // means nothing is pending yet — give up the CPU and re-check (the
        // fixture chassis is a cooperative single-CPU slice; production
        // callers park on a wait-set `Signal` member instead).
        loop {
            let ret = tairix_rt::signal_intake(SignalIntakeOp::Take);
            if ret == i64::from(tairix_abi::Signal::Interrupt.as_u32()) {
                break;
            }
            if ret == -i64::from(Errno::WouldBlock.as_i32()) {
                tairix_rt::yield_now();
                continue;
            }
            // Any other outcome (a different signal, a decode error) is a
            // distinct failure diagnostic.
            return 31;
        }
        // Observed, not fatal: this program is still running after the
        // `^C`. Now deliberately stop draining — the next `^C` is recorded
        // pending, and the one after that finds the slot occupied and
        // escalates to the default terminate (the vertical reaps 130).
        loop {
            tairix_rt::yield_now();
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    ///
    /// The child yields forever (it is terminated by the parent's signal, so
    /// this never returns). The intake role runs [`run_intake`]. The parent
    /// reads the child's PID, signals it, reaps it, and verifies the reaped
    /// termination status.
    fn main() -> i32 {
        if IS_INTAKE {
            return run_intake();
        }
        if !IS_PARENT {
            // Child: never exit on our own — give up the CPU each iteration and
            // wait to be terminated by the parent's signal. A tight spin would
            // starve the parent on the single cooperative CPU, so yield.
            loop {
                tairix_rt::yield_now();
            }
        }

        // Parent: the child's PID is the second inherited argument (arg 0 is
        // the program name the vertical chose). Fail closed if it is missing or
        // malformed rather than signalling an arbitrary PID.
        let Some(child_pid) = tairix_rt::arg(1).and_then(parse_pid) else {
            return 20;
        };

        // Stop the running child (`plans/SPAWN.md` SP9): it is parked and
        // held by the stop overlay, not terminated.
        if tairix_rt::signal(child_pid, Signal::Stop) != 0 {
            return 21;
        }

        // A `STOPPED` wait observes the stop — without reaping the child.
        let mut status = WaitStatus::Exited(-1);
        let ret = tairix_rt::wait(child_pid, &mut status, WaitFlags::STOPPED);
        if ret < 0 {
            return 22;
        }
        if ret != child_pid {
            return 23;
        }
        if status != WaitStatus::Stopped(Signal::Stop) {
            return 24;
        }

        // Resume it: a stopped child is still live and signallable.
        if tairix_rt::signal(child_pid, Signal::Continue) != 0 {
            return 25;
        }

        // Deliver a graceful terminate to our (resumed) child. `signal`
        // returns 0 on success, `-errno` otherwise.
        if tairix_rt::signal(child_pid, Signal::Terminate) != 0 {
            return 26;
        }

        // Reap the child and read back the status the kernel recorded for it.
        let mut status: i32 = -1;
        if tairix_rt::wait_exit(child_pid, &mut status) < 0 {
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

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
