//! The `Run` entry-point binary of the `sleep` tool — the program a shell
//! spawns to pause for a sum of time intervals.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment (plans/APPS.md §5 — the shell
//! exports it; the tool invents no second source), and runs the parsed
//! command against the production seams: the clock-backed `RtSleeper`,
//! which parks the task off-CPU for the requested interval, the shared
//! `tairix_help::BundleHelp`, which reads the tool's own bundle's `Help/`
//! tree for the short-help switches, and `RtOutput`, which writes the
//! short help to the inherited standard output (fd 1). The tool binds only
//! to its inherited descriptors, never a console device, and holds no
//! ambient authority.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::format;

    use tairix_abi::{Delay, Errno};
    use tairix_help::BundleHelp;
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_rt::ClockDelay;
    use tairix_sleep::{parse, run, Output, SleepError, Sleeper, USAGE};

    /// Microseconds in one second — the seconds → park-unit factor.
    const MICROS_PER_SECOND: f64 = 1_000_000.0;

    /// The production [`Sleeper`]: a clock-backed, off-CPU timed park.
    ///
    /// Every wait goes through [`ClockDelay::delay_us`], which parks the
    /// task on the runtime's timed wait-set — the CPU sleeps between the
    /// deadline checks, it never spins (`AGENTS.md` §2.23). A finite
    /// interval is parked in `u32`-microsecond chunks (each a real park);
    /// an infinite interval (`sleep inf`) re-parks in maximal chunks
    /// forever, so the process pauses until it is killed without ever
    /// busy-looping.
    struct RtSleeper;

    impl Sleeper for RtSleeper {
        fn sleep(&self, seconds: f64) {
            let delay = ClockDelay::new();
            if seconds.is_infinite() {
                loop {
                    delay.delay_us(u32::MAX);
                }
            }
            // `seconds` is finite and non-negative here (the parser rejects
            // negatives and `nan`). Convert to microseconds, saturating a
            // value larger than `u64` can hold to the longest park we can
            // express, then park in `u32` chunks so each is a real timed
            // wait.
            let micros = seconds * MICROS_PER_SECOND;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            // The bound guards the cast; a fractional microsecond truncates
            // (the runtime park is microsecond-resolution), never wraps.
            let mut remaining: u64 = if micros >= (u64::MAX as f64) {
                u64::MAX
            } else {
                micros as u64
            };
            while remaining > 0 {
                let chunk = remaining.min(u64::from(u32::MAX));
                #[allow(clippy::cast_possible_truncation)]
                // `chunk <= u32::MAX` by the `min` above.
                delay.delay_us(chunk as u32);
                remaining -= chunk;
            }
        }
    }

    /// The production [`Output`]: the inherited standard output (fd 1).
    ///
    /// A single line plus its terminating newline, over the shared
    /// `tairix_rt::io` short-write loop; a stream that stops accepting bytes
    /// fails closed rather than spinning, and the failure is surfaced so a
    /// short-help write error becomes a non-zero exit.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_line(&self, line: &str) -> Result<(), Errno> {
            let mut out = Stdout;
            out.write_all(line.as_bytes())
                .map_err(|_| Errno::BrokenPipe)?;
            out.write_all(b"\n").map_err(|_| Errno::BrokenPipe)
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` when the interval elapsed (or a requested short help
    /// was written); `1` when the short-help write failed; `2` on a usage
    /// error (a malformed argument vector, an unrecognised option, a missing
    /// operand, or an invalid time interval).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line("sleep: invalid time interval");
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("sleep: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &RtSleeper,
            &BundleHelp::new("sleep"),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(SleepError::Output(_)) => {
                write_stderr_line("sleep: write error");
                1
            }
            // The parser produced the command, so `run` cannot raise a usage
            // error here; the arm stays total and fails loud rather than
            // guessing an exit code.
            Err(err) => {
                write_stderr_line(&format!("sleep: {err}"));
                2
            }
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
