//! The `Run` entry-point binary of the `memsoak` memory-stability fixture —
//! the program the aarch64 memory-stability QEMU vertical runs from the
//! scripted root shell (`plans/APPS.md` "Immediate work" I2/I3).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` soaks the kernel's per-cycle memory behaviour on the live system:
//!
//! 1. **Warmup** — `WARMUP_CYCLES` full cycles, paying every once-per-boot
//!    cost (the app store's verification cache admitting `true.app`,
//!    sysinfod's heap reaching its query working set) off the measured
//!    window.
//! 2. **Baseline** — one `KERNEL_MEMORY_STATS` query through the System
//!    Information API (`sysinfod` gates it on this process's own
//!    `CAP_SYSINFO_KERNEL`, kernel-attested — no ambient authority).
//! 3. **Soak** — `MEASURED_CYCLES` cycles, each the I2 login/logout
//!    equivalent plus the I3 `top -d0` refresh shape: spawn and reap a
//!    `true.app` child off the mounted store (the full spawn → exit → reap →
//!    teardown path, including user-frame and page-table reclamation), a
//!    timed `stream_read` whose bound elapses (the refresh park), a
//!    self-scoped process-list walk, and a memory query round trip over the
//!    live `sysinfod` IPC endpoint.
//! 4. **Verdict** — one more sample; the strict comparison lives in the
//!    host-tested library (`lib.rs`). On `Stable` it prints the
//!    `MEMSOAK PASS …` line and exits `0`; the consuming vertical's script
//!    keys its PASS chain on that marker.
//!
//! **A failed soak never exits.** The vertical's guest-side sink arms its
//! PASS chain on this program's audited `exit`, so the failure path prints
//! the `MEMSOAK FAIL …` line (or the failing step's reason) to standard
//! error and parks forever off the run queue — the run then times out and
//! the harness reports the failure loudly, with the diagnosis in the serial
//! transcript (fail loud, and no CPU is burned while failing).
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_abi::sysinfo::{KernelMemoryStats, SysinfoQueryId};
    use tairix_procinfo::{call, for_each_process, IpcTransport, Transport};
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_test_memsoak::{
        report_line, verdict, Verdict, CHILD_PATH, CYCLE_PARK_NANOS, MEASURED_CYCLES, WARMUP_CYCLES,
    };

    /// Sample the kernel's free-memory figure through the System
    /// Information API: one `KERNEL_MEMORY_STATS` round trip, decoded
    /// through the frozen `sysinfo-v1` wire type. Fails closed on a refusal
    /// or a malformed reply — the soak never fabricates a sample.
    fn sample_free_bytes(transport: &dyn Transport) -> Result<u64, &'static str> {
        let bytes = call(transport, SysinfoQueryId::KERNEL_MEMORY_STATS, &[])
            .map_err(|_| "memsoak: KERNEL_MEMORY_STATS query refused")?;
        KernelMemoryStats::from_bytes(&bytes)
            .map(|stats| stats.free_bytes)
            .map_err(|_| "memsoak: KERNEL_MEMORY_STATS reply malformed")
    }

    /// One soak cycle: spawn and reap a `true.app` child (the full
    /// teardown path), park on a timed read whose bound elapses (the
    /// `top -d0` refresh shape — the elapsed bound is the expected outcome,
    /// not an error), then walk the self-scoped process list. Any step that
    /// refuses outright fails the cycle with its reason.
    fn cycle(transport: &dyn Transport) -> Result<(), &'static str> {
        let pid = tairix_rt::spawn(CHILD_PATH);
        if pid < 0 {
            return Err("memsoak: child spawn refused");
        }
        let Ok(pid) = i32::try_from(pid) else {
            return Err("memsoak: child PID out of range");
        };
        let mut status = 0i32;
        if tairix_rt::wait_exit(pid, &mut status) < 0 {
            return Err("memsoak: child wait refused");
        }
        if status != 0 {
            return Err("memsoak: child exited non-zero");
        }
        // The timed park: the bound elapsing with no input is the expected
        // outcome (nobody types during the soak), so the elapsed-bound
        // error is absorbed; the park itself — arming the one-shot and
        // being woken by it — is the behaviour under test.
        let mut scratch = [0u8; 1];
        let _ = tairix_rt::stdin_timeout(&mut scratch, CYCLE_PARK_NANOS);
        for_each_process(transport, false, |_| Ok(()))
            .map_err(|_| "memsoak: self process-list walk failed")?;
        Ok(())
    }

    /// Terminal failure: report the reason on standard error, then park
    /// forever off the run queue. This program must **never exit** on a
    /// failure — the consuming vertical arms its PASS chain on this
    /// process's audited `exit`, so failing loudly means parking until the
    /// harness times the run out with the reason in the transcript. The
    /// spin fallback runs only if even the park is refused (nothing better
    /// remains, and exiting would still be wrong).
    fn fail(reason: &str) -> ! {
        write_stderr_line(reason);
        let _ = tairix_rt::park_forever();
        loop {
            core::hint::spin_loop();
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall. Returns `0` only for a proven-stable soak; every other
    /// outcome diverges into [`fail`].
    fn main() -> i32 {
        let transport = IpcTransport;
        for _ in 0..WARMUP_CYCLES {
            if let Err(reason) = cycle(&transport) {
                fail(reason);
            }
        }
        let baseline = match sample_free_bytes(&transport) {
            Ok(value) => value,
            Err(reason) => fail(reason),
        };
        for _ in 0..MEASURED_CYCLES {
            if let Err(reason) = cycle(&transport) {
                fail(reason);
            }
        }
        let final_free = match sample_free_bytes(&transport) {
            Ok(value) => value,
            Err(reason) => fail(reason),
        };
        let outcome = verdict(baseline, final_free);
        let line = report_line(outcome, baseline, final_free);
        match outcome {
            Verdict::Stable => {
                if Stdout.write_all(line.as_bytes()).is_err() {
                    // A PASS the transcript never carried is not a PASS the
                    // vertical may act on: park rather than exit.
                    fail("memsoak: report write failed");
                }
                0
            }
            Verdict::Drifted => fail(line.trim_end()),
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
