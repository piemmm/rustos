//! The write-back expiry timer: publishing a volume whose batched
//! filesystem transaction has aged out (`plans/ARXFS-WRITEBACK.md` §10).
//!
//! A filesystem that batches commits keeps one transaction open for the next
//! operation to join, so the commit's barrier, root, and rewritten metadata
//! blocks cost once per burst instead of once per operation. The driver
//! bounds how long it will do that — a window from the device's class — but
//! it can only *check* that window from inside an operation. A volume whose
//! last write is followed by silence would therefore hold its transaction
//! until the next operation, an explicit `fs_sync`, or unmount: bounded in
//! content, unbounded in time.
//!
//! So the expiry lives here, above the driver, where Linux also puts it: one
//! kernel task parks on [`WRITEBACK_WAITQ`](crate::waitq::WRITEBACK_WAITQ)
//! until the soonest deadline any mounted volume has published, then calls
//! the ordinary [`FilesystemWrite::flush`] on each volume that is due.
//!
//! # Event-driven, not a sweep
//!
//! Nothing polls the mounts. Each driver *reports* its deadline as its
//! transaction opens and reports its absence as the transaction closes
//! ([`tairix_abi::driver::filesystem::WritebackHost`]), so the timer is armed
//! exactly once per batch and a machine with no dirty volume arms nothing and
//! takes no wakeup. A deadline
//! is **consumed** when it fires, exactly as a wait-queue deadline is: a
//! fired deadline left in place would keep the one-shot armed in the past and
//! spin the dispatch loop.
//!
//! # One flusher, in deadline order
//!
//! One task serves every mount rather than one per volume: a per-mount task
//! would cost a kernel stack per attached volume and a spawn/teardown per
//! hotplug, for a saving only a machine with several simultaneously-dirty
//! volumes would notice. It publishes in deadline order and **blocks** on
//! each mount's lock rather than skipping a busy one, because skipping means
//! dropping a consumed deadline: an operation in flight usually publishes the
//! aged-out transaction itself, but one that *fails* rolls back and leaves it
//! open, so a skipped volume could keep its transaction with nothing left to
//! fire.

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::sync::Arc;

use tairix_abi::driver::filesystem::FilesystemWrite;
use tairix_abi::driver::DriverHandle;
use tairix_log::{Field, FieldValue, Level, Sink};

use crate::audit::{self, AuditEvent};
use crate::sleeplock::SleepLock;
use crate::waitq::NO_DEADLINE;

use super::mounted::LateFilesystem;

/// One mounted volume's published write-back deadline: the absolute
/// monotonic instant its open transaction must be published by, or
/// [`NO_DEADLINE`] when it holds none.
///
/// Written by the driver from inside its own operation (under the mount
/// lock) and read by the flusher without taking that lock, so a volume being
/// operated on never delays the deadline scan. One `u64` per mount, with
/// [`NO_DEADLINE`] standing for "nothing open" — a real deadline can never
/// be `u64::MAX`, which is nanoseconds enough for five centuries of uptime.
#[derive(Debug, Default)]
pub(super) struct WritebackDue(AtomicU64);

impl WritebackDue {
    /// A slot for a volume with nothing to publish.
    pub(super) const fn empty() -> Self {
        Self(AtomicU64::new(NO_DEADLINE))
    }

    /// Record the volume's deadline, or its absence.
    ///
    /// A deadline is clamped one nanosecond below the sentinel, so a driver
    /// reporting an absurd instant can never spell "nothing open".
    pub(super) fn store(&self, deadline_ns: Option<u64>) {
        let encoded = deadline_ns.map_or(NO_DEADLINE, |at| at.min(NO_DEADLINE - 1));
        self.0.store(encoded, Ordering::Release);
    }

    /// The recorded deadline, or `None` when the volume holds nothing.
    pub(super) fn load(&self) -> Option<u64> {
        match self.0.load(Ordering::Acquire) {
            NO_DEADLINE => None,
            deadline => Some(deadline),
        }
    }

    /// Take the deadline if it has arrived by `now_ns`, leaving the slot
    /// empty and returning what was taken.
    ///
    /// Consuming it is what makes the firing single-shot: a deadline left in
    /// place after it fired would re-arm the timer in the past forever. The
    /// driver publishes a fresh one the next time it opens a transaction. The
    /// exchange is what the caller acts on, so a deadline the driver moved
    /// concurrently is either taken whole or left whole, never reported as one
    /// value and cleared as another.
    pub(super) fn take_if_due(&self, now_ns: u64) -> Option<u64> {
        let deadline = self.load()?;
        if deadline > now_ns {
            return None;
        }
        self.0
            .compare_exchange(deadline, NO_DEADLINE, Ordering::AcqRel, Ordering::Acquire)
            .ok()
    }
}

/// The `through_ns` that makes every published deadline due at once, for the
/// flusher's own teardown: it is disarming deferral, so nothing may be left
/// deferred behind it.
pub const EVERYTHING_DUE: u64 = NO_DEADLINE - 1;

/// Publish every registered volume whose write-back deadline is at or before
/// `through_ns`, returning the soonest deadline still pending — the instant
/// the flusher parks until, or `None` for "nothing is dirty, arm nothing".
///
/// A failed publish is not retried here: the driver's own commit failure
/// path abandons the transaction and reports that it holds nothing, so the
/// volume leaves the due set either way. The failure is logged, because a
/// background publish is the one durability failure no caller is waiting on,
/// so the log is the only place its reason can land.
pub fn publish_due<F>(
    mounts: &LateFilesystem<F>,
    audit_sink: &dyn Sink,
    through_ns: u64,
) -> Option<u64>
where
    F: FilesystemWrite + Send + 'static,
{
    for (volume, driver) in mounts.take_writeback_due(through_ns) {
        publish_one(volume, &driver, audit_sink);
    }
    mounts.earliest_writeback_due()
}

/// Publish one volume, logging a failure against the volume's handle.
fn publish_one<F>(volume: DriverHandle, driver: &Arc<SleepLock<F>>, audit_sink: &dyn Sink)
where
    F: FilesystemWrite + Send + 'static,
{
    let Err(err) = driver.lock().flush() else {
        return;
    };
    audit::emit(
        audit_sink,
        Level::Error,
        AuditEvent::VolumeWritebackFailed,
        &[
            Field {
                key: "volume",
                value: FieldValue::UnsignedInt(volume.as_u64()),
            },
            Field {
                key: "error",
                value: FieldValue::SignedInt(i64::from(err.as_errno().as_i32())),
            },
        ],
    );
}

#[cfg(test)]
#[path = "writeback_tests.rs"]
mod tests;
