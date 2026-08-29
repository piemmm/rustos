//! The write-back flusher kthread: the kernel task that publishes a volume
//! whose batched filesystem transaction has aged out
//! (`plans/ARXFS-WRITEBACK.md` §10).
//!
//! The policy — which volume is due, in what order, and what a failed
//! publish means — is arch-neutral and lives in
//! [`tairix_kernel_core::fs::writeback`]. This module is the wiring: it hands
//! the mount registry to every driver as its write-back timer and admits one
//! long-lived kthread that parks on
//! [`WRITEBACK_WAITQ`] between
//! deadlines.
//!
//! # Deferral is enabled only while the flusher can fire it
//!
//! The flusher arms deferral itself, from its own body, once it has proved it
//! can register on the queue and be woken — and disarms it if it ever stops.
//! Until then, and ever after, the host declines to read its clock and every
//! driver publishes at each operation exactly as it did before batching
//! existed. So a transaction can never be held against a timer that will not
//! come back for it, whatever fails: a port with no storage floor, a service
//! that was not admitted, a scheduler hook that is not wired.

use alloc::boxed::Box;

use tairix_kernel_core::fs::writeback;
use tairix_kernel_core::kthread::YieldHandle;
use tairix_kernel_core::waitq::{
    rearm_timed_wakeup, wait_arch, wait_now_ns, WaitQueueArch, NO_DEADLINE, WRITEBACK_WAITQ,
};
use tairix_kernel_core::InitSpawnCtx;
use tairix_log::{Event, Field, FieldValue, Level, Sink};

use crate::system_mount::LATE_FILESYSTEM;

/// Stable event id: the write-back flusher is not running, so no volume's
/// dirty-age window is enforced from above and every driver publishes
/// eagerly.
const WRITEBACK_FLUSHER_UNAVAILABLE: tairix_log::EventId = tairix_log::EventId(4187);

/// Install the mount registry as every driver's write-back timer and admit
/// the flusher kthread.
///
/// Installing the host is harmless before the flusher runs — a hostless or
/// unarmed driver reports nothing and defers nothing — so the two are wired
/// here and the *arming* is left to the flusher's own first step.
pub fn start(ctx: &'static (dyn InitSpawnCtx + Sync), audit: &'static (dyn Sink + Sync)) {
    if LATE_FILESYSTEM
        .install_writeback_host(&LATE_FILESYSTEM)
        .is_err()
    {
        // A flusher is already wired for this boot; a second would publish
        // the same volumes twice.
        return;
    }
    let body = move |yielder: &mut dyn YieldHandle| flusher(yielder, audit);
    if ctx.spawn_kernel_service(Box::new(body)).is_none() {
        unavailable(audit, "flusher_not_admitted");
    }
}

/// Park until the soonest deadline any mounted volume published, publish
/// every volume that is due, and repeat — for the life of the system.
///
/// The park is registered *before* it is taken, so a deadline published in
/// the window between the scan and the park is not slept through: the
/// scheduler's wake-pending token re-readies a task whose wake arrived before
/// it committed. With nothing dirty the flusher registers [`NO_DEADLINE`],
/// which arms no timer at all, so an idle machine takes no wakeups.
fn flusher(yielder: &mut dyn YieldHandle, audit: &'static (dyn Sink + Sync)) {
    // Prove the park works before any driver is allowed to defer against it.
    if !arm(None) {
        unavailable(audit, "flusher_cannot_park");
        return;
    }
    LATE_FILESYSTEM.set_writeback_armed(true);
    loop {
        yielder.park();
        let due = match wait_now_ns() {
            Some(now) => writeback::publish_due(&LATE_FILESYSTEM, audit, now),
            // No monotonic clock means the host read none either, so nothing
            // can have been deferred; there is nothing due to publish.
            None => LATE_FILESYSTEM.earliest_writeback_due(),
        };
        if !arm(due) {
            break;
        }
    }
    // Leaving means nothing will fire a window again: stop every driver
    // deferring, then publish what is still held so the exit costs recency
    // only up to this moment.
    LATE_FILESYSTEM.set_writeback_armed(false);
    let _ = writeback::publish_due(&LATE_FILESYSTEM, audit, writeback::EVERYTHING_DUE);
    unavailable(audit, "flusher_cannot_park");
}

/// Register this task on the write-back queue for `due` (or with no
/// deadline) and re-point the timed one-shot, returning whether the
/// registration succeeded.
fn arm(due: Option<u64>) -> bool {
    let Some(arch) = wait_arch() else {
        return false;
    };
    let Some(task) = arch
        .current_cpu()
        .and_then(|cpu| WaitQueueArch::current_task(arch, cpu))
    else {
        return false;
    };
    WRITEBACK_WAITQ.register(task, due.unwrap_or(NO_DEADLINE));
    rearm_timed_wakeup();
    true
}

/// Log that the dirty-age window is not being enforced from above, naming a
/// stable, secret-free `cause`.
fn unavailable(audit: &dyn Sink, cause: &'static str) {
    tairix_log::log(
        audit,
        &Event {
            level: Level::Error,
            id: WRITEBACK_FLUSHER_UNAVAILABLE,
            message: "writeback: flusher not running; volumes publish at every operation",
            fields: &[Field {
                key: "cause",
                value: FieldValue::Str(cause),
            }],
        },
    );
}

#[cfg(test)]
#[path = "writeback_service_tests.rs"]
mod tests;
