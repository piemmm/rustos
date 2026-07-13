//! The `Run` entry-point binary of the `stress` tool — the load generator
//! a shell spawns (`plans/STRESSTEST.md` ST5).
//!
//! This is a **pure-Rust** program linking the Rust userland runtime
//! `rustos-rt` (`_start`, canary, panic handler, `mem_map` allocator, and
//! the syscall wrappers); `rustos_rt::entry!` names this program's `main`.
//!
//! One binary, two roles decided by argv:
//!
//! * **Controller** (a user's `stress …` command): pins itself
//!   (`mem_pin`, incidental — a refusal is reported and the run continues
//!   unpinned), opts into `signal_intake` so `^C`/`Terminate` are
//!   observed and the workers are torn down before exit, prepares the
//!   scratch directory, sizes the byte targets from discovered RAM and
//!   free space, spawns the workers (and `--monitor`'s `sysmon`), and
//!   drives the [`rustos_stress::Controller`] state machine off one
//!   wait-set (child exits + the signal intake + the timeout/grace
//!   deadline). Teardown removes every scratch file on every exit path.
//! * **Worker** (`stress --worker …`, spawned by the controller through
//!   the kernel's attested `@self` token): runs its load unit in a loop
//!   until terminated; a typed refusal is reported once and exits
//!   [`rustos_stress::REFUSED_EXIT`]; any other failure exits 1.
//!
//! The program binds only to its inherited descriptors (fd 0/1/2/3) and
//! holds no ambient authority. On the host it is an inert stub so `cargo
//! build --workspace`, clippy, and fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

#[cfg(all(freestanding, feature = "program"))]
mod worker_main;

#[cfg(all(freestanding, feature = "program"))]
mod controller_main;

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use rustos_help::{own_short_help, BundleHelp};
    use rustos_rt::io::{write_stderr_line, Stdout, Write};
    use rustos_stress::{parse, Command, USAGE};

    /// Render `stress`'s own short help through the one shared engine;
    /// the usage banner stands in when no document can be served.
    fn short_help() -> i32 {
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("stress"), locale, "stress")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point.
    ///
    /// Exit codes: `0` on a clean run (or served help/version), `1` on a
    /// failed run or terminal failure, `2` on a usage error, `130`/`143`
    /// when a signal ended the run, and — in a worker —
    /// [`rustos_stress::REFUSED_EXIT`] for a typed resource refusal.
    fn main() -> i32 {
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        match parse(&arguments) {
            Ok(Command::Help) => short_help(),
            Ok(Command::Version) => {
                let line = alloc::format!("stress (RustOS) {}\n", env!("CARGO_PKG_VERSION"));
                match Stdout.write_all(line.as_bytes()) {
                    Ok(()) => 0,
                    Err(_) => 1,
                }
            }
            Ok(Command::Worker(spec)) => crate::worker_main::run(&spec),
            Ok(Command::Run(spec)) => crate::controller_main::run(&spec),
            Err(_) => {
                write_stderr_line(USAGE);
                2
            }
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
