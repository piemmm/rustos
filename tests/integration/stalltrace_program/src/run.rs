//! The `stalltrace` fixture: provoke exactly one frame-budget overrun so the
//! kernel's report has a blocking call to name and a live user stack to walk
//! (`plans/FIX-STALLTRACE.md`).
//!
//! The consuming vertical (`tests/integration/stalltrace_qemu_aarch64`) runs
//! the production boot pipeline, types this fixture's command word at the
//! scripted root shell, and gates on the `TaskLatencyOverrun` record the run
//! produces. Everything the fixture does is deliberate: nothing here is a
//! plausible shape for real code, and that is the point — a stall the kernel
//! reports must be one a developer could have caused.
//!
//! The three steps mirror what a real interactive surface does, minus the
//! surface:
//!
//! 1. **Declare a budget.** A budget of zero means the image compiled the
//!    diagnostics out, which is not a failure — the fixture says so and exits
//!    clean, because there is nothing to prove on such an image.
//! 2. **Open a span.** The kernel opens one when a thread returns from an
//!    *event* wait, so the fixture makes a real one: a pipe it writes to
//!    itself, registered as a `Stream` member, is ready the moment the wait
//!    is entered. That is deterministic — no timing, no peer, no race.
//! 3. **Stall.** A single timed park well past the budget. It is a memberless
//!    finite wait, which deliberately does *not* close the span, so the whole
//!    park is charged to the frame it delayed and the kernel reports at that
//!    park's own exit: the culprit call names itself and the frame captured at
//!    its entry is the live stalling stack.
//!
//! Each step that can fail prints its reason and returns a distinct non-zero
//! code (fail loud, never a silent pass), and a clean run prints the marker
//! the vertical's runner keys its script on.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

#[cfg(freestanding)]
mod program {
    use tairix_abi::latency::DEFAULT_FRAME_BUDGET_NS;
    use tairix_abi::{WaitSetOp, WaitSourceKind};
    use tairix_rt::io::{self, Stream, Write as _};
    use tairix_test_stalltrace::{INERT_MARKER, PROVOKED_MARKER};

    /// Clean run: a span was opened and one overrun provoked.
    const EXIT_OK: i32 = 0;
    /// The kernel refused a pipe, so no event wait can be made ready.
    const FAIL_PIPE: i32 = 11;
    /// The kernel refused a wait-set, or the `Stream` membership.
    const FAIL_WAITSET: i32 = 12;
    /// The write that makes the member ready was refused, so the wait would
    /// have parked forever instead of opening a span.
    const FAIL_FEED: i32 = 13;
    /// The event wait itself was refused, so no span was ever opened.
    const FAIL_WAIT: i32 = 14;

    /// The member's token. Any non-zero value; the fixture never reads it
    /// back, since one member means one possible answer.
    const STREAM_TOKEN: u64 = 1;

    /// How long the deliberate stall parks for.
    ///
    /// Comfortably past [`DEFAULT_FRAME_BUDGET_NS`] so the overrun cannot be
    /// missed on a loaded host — the report is keyed on the budget being
    /// crossed, and a margin this wide makes that independent of scheduling
    /// jitter. Overshooting costs the run only the wall-clock difference.
    const STALL_NS: u64 = DEFAULT_FRAME_BUDGET_NS * 2;

    /// Report `line` on stdout. A refused write leaves the run to fail by
    /// timeout with the transcript as the diagnosis, which is the fixture's
    /// only remaining channel.
    fn say(line: &str) {
        let _ = io::Stdout.write_all(line.as_bytes());
        let _ = io::Stdout.write_all(b"\n");
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        // 1. Declare the frame budget. Zero means the diagnostics are not in
        //    this image at all, which the fixture reports rather than
        //    treating as a failure: there is no overrun to provoke.
        if tairix_rt::latency_watch(DEFAULT_FRAME_BUDGET_NS) == 0 {
            say(INERT_MARKER);
            return EXIT_OK;
        }

        // 2. Open a span. The pipe is fed *before* the wait, so the member is
        //    already ready and the wait returns without parking — the span
        //    opens deterministically rather than on a peer's timing.
        let Ok((read_fd, write_fd)) = tairix_rt::pipe_create() else {
            say("stalltrace: pipe refused");
            return FAIL_PIPE;
        };
        let set = tairix_rt::waitset_create();
        if set < 0 {
            say("stalltrace: wait-set refused");
            return FAIL_WAITSET;
        }
        // A negative handle was refused above, so the cast is of a
        // non-negative value.
        #[allow(clippy::cast_sign_loss)]
        let set = set as u64;
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Stream,
            u64::from(read_fd),
            STREAM_TOKEN,
        ) != 0
        {
            say("stalltrace: stream membership refused");
            return FAIL_WAITSET;
        }
        if Stream::new(write_fd).write_all(b"x").is_err() {
            say("stalltrace: pipe feed refused");
            return FAIL_FEED;
        }
        let mut token = 0u64;
        if tairix_rt::waitset_wait(set, u64::MAX, &mut token) != 0 {
            say("stalltrace: event wait refused");
            return FAIL_WAIT;
        }

        // 3. Stall. A memberless finite park does not close the span, so the
        //    whole wait is charged to the frame it delayed and the kernel
        //    reports at this call's own exit — with the frame it captured at
        //    this call's entry, which is the stack that stalled.
        tairix_rt::park_ns(STALL_NS);

        say(PROVOKED_MARKER);
        EXIT_OK
    }

    tairix_rt::entry!(main);
}

// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
