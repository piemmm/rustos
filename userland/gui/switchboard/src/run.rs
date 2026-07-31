//! The `Run` entry-point binary of the Switchboard monitor service,
//! installed at `/System/Services/switchboard.app/Run`
//! (`plans/NEW-TASKBAR.md` T10) — spawned by the desktop session as the
//! logged-in user (never PID 1), so the tray-overview authority
//! (`CAP_SYSINFO_GLOBAL`/`CAP_SYSINFO_KERNEL`) never has to grow the
//! session's own manifest.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the
//! Rust userland runtime `tairix-rt` — never the C ABI. `tairix-rt`
//! provides `_start`, the panic handler, the `#[global_allocator]`,
//! `ipc_call`, the wait-set syscalls, `clock_get`, `signal_intake`, and
//! `stderr`; `tairix-procinfo`'s `IpcTransport` (enabled through its own
//! `program` feature) is the production `tairix_procinfo::Transport` the
//! sampler queries through.
//!
//! # What this service does
//!
//! At startup it enables signal observation and builds a wait-set with one
//! member — the process's own termination-request signal — which doubles
//! as this loop's sole parking source: the service ticklessly samples the
//! live system every `tairix_switchboard::SAMPLE_PERIOD_NS`, derives a
//! compact tray summary, and posts it to the desktop session's
//! `SWITCHBOARD_ENDPOINT` only when it changed or the keepalive interval
//! elapsed, then parks on the wait-set until the next sample is due or a
//! termination signal arrives. It never busy-polls: the one `waitset_wait`
//! call per iteration is always given exactly the time remaining until the
//! next thing that must happen.
//!
//! The optional system-wide process scope and memory-pressure gauge are
//! probed once at startup (`tairix_switchboard::probe_scopes`) — capability
//! sets are fixed at spawn, so re-probing per sample could only rediscover
//! the same answer while spamming the audited memory-pressure query.
//!
//! A refusal from the session (`NotFound` — no session bound the
//! endpoint, or it exited; `PermissionDenied` — the session refused this
//! instance's identity, e.g. after a session restart left it orphaned) is
//! a **clean** exit: the service has no purpose without a session to
//! report to. Any other publish failure is retried on the next cycle, up
//! to a small bounded count in a row, after which the service gives up
//! rather than retrying forever. Every abnormal exit states its reason on
//! `stderr` first.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the
// optional `tairix-rt` runtime through the default `program` feature. The
// host tooling builds only this crate's *library*, so this module (and
// `tairix-rt`) never enter those builds.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_abi::reply::decode_status_reply;
    use tairix_abi::switchboard_ipc::{SwitchboardRequest, SWITCHBOARD_ENDPOINT};
    use tairix_abi::{Errno, Signal, SignalIntakeOp, WaitSetOp, WaitSourceKind};
    use tairix_procinfo::IpcTransport;
    use tairix_switchboard::{
        advance_deadline, derive_summary, probe_scopes, wait_timeout_ns, DegradedField, Hysteresis,
        Publisher, Sampler,
    };

    /// Wait-set token of the termination-signal member (the set's only
    /// member).
    const TOKEN_SIGNAL: u64 = 1;

    /// Consecutive non-clean publish failures after which the service
    /// gives up rather than retrying forever.
    const MAX_CONSECUTIVE_PUBLISH_FAILURES: u32 = 5;

    /// Exit code when enabling signal observation, or building and arming
    /// the termination wait-set, fails: with no parking source the service
    /// cannot run its tickless loop at all.
    const EXIT_NO_WAIT_SOURCE: i32 = 1;

    /// Exit code after [`MAX_CONSECUTIVE_PUBLISH_FAILURES`] consecutive
    /// non-clean publish failures.
    const EXIT_PUBLISH_FAILURES: i32 = 2;

    /// Exit code when `waitset_wait` itself fails for a reason other than
    /// the ordinary sample-due timeout — continuing would either busy-loop
    /// (no real park occurred) or hang forever, so the service exits.
    const EXIT_WAIT_FAILED: i32 = 3;

    /// State the abnormal-exit reason on `stderr` (fail loud: an exit code
    /// alone is not a diagnosis) and hand back `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = tairix_rt::stderr(b"switchboard: ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
        code
    }

    /// State a clean-exit reason on `stderr` and return `0`: the service
    /// has no purpose without a session to report to, so this is not a
    /// failure, merely a stated reason for stopping.
    fn clean_exit(reason: &str) -> i32 {
        let _ = tairix_rt::stderr(b"switchboard: ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
        0
    }

    /// Write a one-time notice for a field that just degraded to its
    /// honest empty value, naming which measurement is affected.
    fn note_degradation(field: DegradedField) {
        let reason = match field {
            DegradedField::ProcessList => {
                "notice: the process list is unavailable; recovery count and top task are degraded"
            }
            DegradedField::CpuTime => {
                "notice: CPU-time totals are unavailable; overall CPU load is degraded"
            }
            DegradedField::MemoryPressure => {
                "notice: the memory-pressure gauge is unavailable; memory pressure is degraded"
            }
        };
        let _ = tairix_rt::stderr(b"switchboard: ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
    }

    /// Sample, derive, and — when warranted — publish one cycle's tray
    /// summary. Returns `Some(exit_code)` when the loop should stop.
    fn run_cycle(
        transport: &IpcTransport,
        sampler: &mut Sampler,
        hysteresis: &mut Hysteresis,
        publisher: &mut Publisher,
        now_ns: u64,
    ) -> Option<i32> {
        let sample = sampler.sample(transport, now_ns);
        for field in &sample.degradations {
            note_degradation(*field);
        }
        let summary = derive_summary(&sample, hysteresis);
        let Some(offered) = publisher.offer(summary, now_ns) else {
            return None;
        };

        let request = SwitchboardRequest::PublishSummary { summary: offered }.to_le_bytes();
        let mut reply = [0u8; tairix_abi::reply::STATUS_REPLY_LEN];
        let outcome = match tairix_rt::ipc_call(SWITCHBOARD_ENDPOINT, &request, &mut reply) {
            Ok(len) => decode_status_reply(&reply[..len]),
            Err(ret) => Err(Errno::from_syscall(ret)),
        };

        match outcome {
            Ok(()) => {
                publisher.record_ack(offered);
                None
            }
            Err(Errno::NotFound) => Some(clean_exit(
                "the desktop session's Switchboard endpoint is not bound; exiting",
            )),
            Err(Errno::PermissionDenied) => Some(clean_exit(
                "the desktop session refused this instance; exiting",
            )),
            Err(_) => {
                if publisher.record_failure() >= MAX_CONSECUTIVE_PUBLISH_FAILURES {
                    Some(fail(
                        EXIT_PUBLISH_FAILURES,
                        "too many consecutive publish failures",
                    ))
                } else {
                    None
                }
            }
        }
    }

    /// Probe the granted scopes, then sample/derive/publish on a tickless
    /// schedule until a termination signal arrives or a fatal condition is
    /// reached.
    fn main() -> i32 {
        if tairix_rt::signal_intake(SignalIntakeOp::Enable) != 0 {
            return fail(EXIT_NO_WAIT_SOURCE, "cannot enable signal observation");
        }
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return fail(
                EXIT_NO_WAIT_SOURCE,
                "cannot create the termination wait-set",
            );
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` checked above; it is a kernel-minted handle.
        let set = set as u64;
        if tairix_rt::waitset_ctl(set, WaitSetOp::Add, WaitSourceKind::Signal, 0, TOKEN_SIGNAL) != 0
        {
            return fail(
                EXIT_NO_WAIT_SOURCE,
                "cannot arm the termination signal wait-set member",
            );
        }

        let transport = IpcTransport;
        let scopes = probe_scopes(&transport);
        let mut sampler = Sampler::new(scopes);
        let mut hysteresis = Hysteresis::new();
        let mut publisher = Publisher::new();

        let mut next_deadline = tairix_rt::clock_get();
        loop {
            let now = tairix_rt::clock_get();
            if let Some(code) = run_cycle(
                &transport,
                &mut sampler,
                &mut hysteresis,
                &mut publisher,
                now,
            ) {
                return code;
            }

            next_deadline = advance_deadline(next_deadline, tairix_rt::clock_get());
            let timeout = wait_timeout_ns(next_deadline, tairix_rt::clock_get());
            let mut token = 0u64;
            let wait_ret = tairix_rt::waitset_wait(set, timeout, &mut token);
            if wait_ret == 0 {
                if token == TOKEN_SIGNAL {
                    let taken = tairix_rt::signal_intake(SignalIntakeOp::Take);
                    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                    // A non-negative take result is the drained signal's u32 wire discriminant.
                    let name = if taken >= 0 {
                        match Signal::from_u32(taken as u32) {
                            Ok(Signal::Terminate) => "terminate",
                            Ok(Signal::Interrupt) => "interrupt",
                            Ok(Signal::Kill) => "kill",
                            Ok(Signal::Continue) => "continue",
                            Ok(Signal::Stop) => "stop",
                            Err(_) => "unknown",
                        }
                    } else {
                        "unknown"
                    };
                    let _ = tairix_rt::stderr(b"switchboard: received a ");
                    let _ = tairix_rt::stderr(name.as_bytes());
                    let _ = tairix_rt::stderr(b" signal; exiting\n");
                    return 0;
                }
                // A ready member other than the signal is unreachable (the
                // set has exactly one member); treat it as a spurious wake
                // and re-sample on the next loop iteration.
                continue;
            }
            if Errno::from_syscall(wait_ret) == Errno::TimedOut {
                continue;
            }
            // Any other wait failure means the loop is no longer actually
            // parking: continuing would busy-loop rather than wait, so exit
            // fail-loud instead.
            return fail(
                EXIT_WAIT_FAILED,
                "the termination wait-set failed unexpectedly",
            );
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ------------------------------------------------------------
//
// Whenever the real freestanding `tairix-rt` `_start` path is not compiled —
// on the host (`cargo build --workspace`, clippy, fmt), or for a
// `program`-less build of this crate — this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
