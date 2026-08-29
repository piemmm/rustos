//! The `Run` entry-point binary of the `framestats` desktop frame-accounting
//! fixture (`plans/FIX-DESKTOP-SPEEDUP.md` A.4).
//!
//! This is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt`, which provides `_start`, the per-process stack canary, the
//! panic handler, the `mem_map`-backed global allocator, the syscall wrappers,
//! and the `log_emit`-backed log sink this program reports through.
//!
//! `main` does one thing: read the desktop's published frame accounting
//! through the System Information API and re-emit the counters the hover gate
//! reads as one structured system-log record.
//!
//! Exactly **one** publishing session is required. A desktop-hover run has a
//! single compositing session, so two records would mean the reading is
//! ambiguous about whose frames it describes — and none would mean the
//! desktop has published nothing yet. Both are refused rather than reduced to
//! a figure that looks plausible.
//!
//! A run that cannot sample **says so and exits non-zero**: the reason goes to
//! `stderr` and to the system log under
//! [`SAMPLE_FAILED_EVENT`](tairix_test_framestats::SAMPLE_FAILED_EVENT), which
//! the consuming vertical's guest sink fails the run on sight of — so a
//! missing sample is a loud failure, never a run that quietly waits out its
//! budget.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_log::{log, Event, Field, FieldValue, Level, Sink};
    use tairix_procinfo::{for_each_desktop_frame_report, IpcTransport, Transport, WalkStep};
    use tairix_rt::io::write_stderr_line;
    use tairix_rt::LogSink;
    use tairix_test_framestats::{
        Sample, SAMPLE_FAILED_EVENT, SAMPLE_FAILED_MESSAGE, SAMPLE_FAILED_STATUS,
    };

    /// Read the one publishing session's accounting.
    ///
    /// The walk is `sysinfod`'s own paging client, gated service-side on this
    /// process's kernel-attested `CAP_SYSINFO_GLOBAL`; a refusal or a
    /// malformed reply propagates as the walk's error rather than a
    /// substituted zero.
    fn sample(transport: &dyn Transport) -> Result<Sample, &'static str> {
        let mut found: Option<Sample> = None;
        let mut extra = false;
        for_each_desktop_frame_report(transport, |record| {
            if found.is_some() {
                extra = true;
                return Ok(WalkStep::Stop);
            }
            found = Some(Sample::from_totals(&record.totals));
            Ok(WalkStep::Continue)
        })
        .map_err(|_| "framestats: the desktop frame-stats query was refused")?;
        if extra {
            return Err("framestats: more than one session publishes frame accounting");
        }
        found.ok_or("framestats: no session has published frame accounting")
    }

    /// Report `reason` on `stderr` and on the system log, then exit non-zero.
    ///
    /// Both channels are best-effort and the exit status is the contract: the
    /// consuming vertical's sink watches the log, and a developer running the
    /// command by hand reads `stderr`.
    fn fail(sink: &LogSink, reason: &'static str) -> i32 {
        write_stderr_line(reason);
        log(
            sink,
            &Event {
                level: Level::Error,
                id: SAMPLE_FAILED_EVENT,
                message: SAMPLE_FAILED_MESSAGE,
                fields: &[Field {
                    key: "reason",
                    value: FieldValue::Str(reason),
                }],
            },
        );
        SAMPLE_FAILED_STATUS
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        let sink = LogSink;
        match sample(&IpcTransport) {
            Ok(sample) => {
                let fields = sample.fields();
                sink.write_event(&Sample::event(&fields));
                0
            }
            Err(reason) => fail(&sink, reason),
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
