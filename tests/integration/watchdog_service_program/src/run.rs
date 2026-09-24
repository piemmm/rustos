//! The watchdog fixture: a supervised service that renews its liveness
//! heartbeat for a few intervals and then stops renewing for good
//! (`plans/NEW-SERVICEMANAGER.md` SVC-8, `plans/WATCHDOG.md`).
//!
//! The consuming vertical (`tests/integration/watchdog_qemu_aarch64`) boots
//! the production pipeline against a disk whose `netstack` service bundle
//! is this fixture instead of the real stack, so PID 1 registers, spawns,
//! watches, kills, reaps and relaunches it through exactly the production
//! paths — only the supervised program differs. It is a test double for
//! that one service and is planted on that one disk; no production image
//! ships it.
//!
//! A wedge cannot be provoked from outside: it is the absence of a call,
//! and only the supervised process can stop making one. So the fixture
//! makes the absence deliberate and observable, in three steps:
//!
//! 1. **Announce.** Announce readiness, as the stack it stands in for does,
//!    and adopt the interval the manager answers with. An unwatched run
//!    proves nothing, so it says so and exits rather than passing quietly.
//! 2. **Renew.** Renew through the same `tairix_rt::servicenotice::Watchdog`
//!    a real service uses — the code under test — recording each accepted
//!    renewal. Three of them carry the service past one whole interval,
//!    which is the line an un-renewed watchdog would already have crossed.
//! 3. **Wedge.** Stop calling and park on a pipe nothing will ever write.
//!    The process stays alive and scheduled-out, which is precisely what
//!    the manager cannot distinguish from a service stuck in a handler —
//!    and precisely what it must recover from.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

#[cfg(freestanding)]
mod program {
    use tairix_abi::{WaitSetOp, WaitSourceKind, WAITSET_TIMEOUT_NONE};
    use tairix_log::{Event, EventId, Field, FieldValue, Level};
    use tairix_rt::servicenotice::Watchdog;
    use tairix_rt::LogSink;
    use tairix_test_watchdog_service::{RENEWALS_BEFORE_WEDGE, RENEWED, UNWATCHED, WEDGED};

    /// The audit sink every record goes through. The vertical's witness sink
    /// observes these, so they are the fixture's whole output — the console
    /// belongs to the login session, not to a service.
    static LOG_SINK: LogSink = LogSink;

    /// The manager reported no watchdog, so there is nothing to renew.
    const FAIL_UNWATCHED: i32 = 11;
    /// The kernel refused the pipe or the wait-set the wedge parks on, so
    /// the fixture cannot wedge and must not pretend it did.
    const FAIL_NO_PARK: i32 = 12;

    /// The wedge park's member token. One member means one possible answer.
    const PARK_TOKEN: u64 = 1;

    /// Monotonic nanoseconds, the clock the renewal deadline is kept in.
    fn now_ns() -> u64 {
        tairix_rt::clock_get()
    }

    /// Record one event, optionally carrying the renewal count.
    fn record(id: EventId, level: Level, message: &str, count: Option<u32>) {
        let fields = [Field {
            key: "renewals",
            value: FieldValue::UnsignedInt(u64::from(count.unwrap_or(0))),
        }];
        let _ = tairix_log::log(
            &LOG_SINK,
            &Event {
                level,
                id,
                message,
                fields: if count.is_some() { &fields } else { &[] },
            },
        );
    }

    /// Park for ever on a pipe whose write end this process holds and never
    /// writes.
    ///
    /// The wedge has to be a real indefinite park rather than a loop: a loop
    /// would keep the process runnable and burn a core, which is a different
    /// (and forbidden) shape from the wedge under test. Holding the write
    /// end open is what keeps the read end from reporting end-of-file and
    /// waking the park.
    fn wedge_forever() -> i32 {
        let Ok((read, _write)) = tairix_rt::pipe_create() else {
            return FAIL_NO_PARK;
        };
        let set = tairix_rt::waitset_create();
        let Ok(set) = u64::try_from(set) else {
            return FAIL_NO_PARK;
        };
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Stream,
            u64::from(read),
            PARK_TOKEN,
        ) != 0
        {
            return FAIL_NO_PARK;
        }
        record(
            WEDGED,
            Level::Warn,
            "watchdog fixture: renewals stopped; parking for good",
            Some(RENEWALS_BEFORE_WEDGE),
        );
        let mut token = 0u64;
        loop {
            // Nothing can make the member ready, so this never returns. The
            // loop is here only so a spurious wake cannot turn the wedge
            // back into a running service and silently pass the run.
            let _ = tairix_rt::waitset_wait(set, WAITSET_TIMEOUT_NONE, &mut token);
        }
    }

    fn main() -> i32 {
        let mut watchdog = Watchdog::announce_ready(now_ns());
        if !watchdog.is_watched() {
            // Fail loud: a run where the manager is not watching this
            // service can neither kill it nor restart it, so passing
            // quietly would certify nothing.
            record(
                UNWATCHED,
                Level::Error,
                "watchdog fixture: the service manager reports no watchdog",
                None,
            );
            return FAIL_UNWATCHED;
        }

        // One wait-set for the whole renewal phase: sleeping to the
        // deadline through the runtime's own park exercises the same
        // timeout the real service folds into its wait-set rather than a
        // private timer, and creating it once keeps the loop from leaking a
        // handle per renewal.
        let set = tairix_rt::waitset_create();
        let Ok(set) = u64::try_from(set) else {
            return FAIL_NO_PARK;
        };
        let mut renewals = 0u32;
        while renewals < RENEWALS_BEFORE_WEDGE {
            let mut token = 0u64;
            let _ = tairix_rt::waitset_wait(set, watchdog.timeout_ns(now_ns()), &mut token);
            let before = watchdog;
            watchdog.renew_if_due(now_ns());
            if watchdog == before {
                // The deadline had not lapsed, so no renewal was owed yet.
                continue;
            }
            if !watchdog.is_watched() {
                record(
                    UNWATCHED,
                    Level::Error,
                    "watchdog fixture: the manager refused a renewal",
                    Some(renewals),
                );
                return FAIL_UNWATCHED;
            }
            renewals += 1;
            record(
                RENEWED,
                Level::Info,
                "watchdog fixture: liveness renewed",
                Some(renewals),
            );
        }

        wedge_forever()
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// program is not compiled, so this inert `main` keeps the crate building
// under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
