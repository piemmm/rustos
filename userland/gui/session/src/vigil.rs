//! Window-owner responsiveness tracking — the desktop's honest
//! "not responding" detector.
//!
//! The desktop session delivers every app-ward window event as one
//! non-blocking send to the owning app's event mailbox (the
//! [`EventSink`](tairix_window::EventSink) seam). A healthy app drains that
//! mailbox promptly, so a send only comes back refused with the kernel's
//! mailbox-full backpressure signal when the app has stopped consuming its
//! events — the one observable, evidence-based hang signal the desktop has.
//! [`HangTracker`] folds those per-delivery outcomes into a per-owner
//! verdict: an owner whose deliveries have been refused as backpressure
//! continuously for at least [`UNRESPONSIVE_AFTER_NS`] is *unresponsive*;
//! one accepted delivery clears it, because acceptance proves the mailbox
//! drained.
//!
//! The tracker is pure bookkeeping: time is supplied by the caller as
//! monotonic nanoseconds (the session stamps `clock_get` only on the
//! delivery paths, so an idle desktop takes no clock reads), and nothing
//! here fabricates a verdict — no deliveries means no new evidence, and the
//! standing verdict holds until the app proves recovery by draining or
//! exits and is [`forget`](HangTracker::forget)-ten by the reap path.

use alloc::collections::BTreeMap;

use tairix_abi::Errno;

/// How long an owner's event deliveries must be continuously refused as
/// backpressure before the owner is declared unresponsive, in monotonic
/// nanoseconds.
///
/// The mailbox only fills when the app has already ignored a full queue of
/// events, so the first refusal is itself late news; four further seconds
/// rides out a machine under heavy load (a contended CPU or a paging storm
/// legitimately delays a healthy app's drain for a while) without leaving
/// the user staring at a wedged window wondering why nothing is flagged.
pub const UNRESPONSIVE_AFTER_NS: u64 = 4_000_000_000;

/// One suspect owner: deliveries have been refused since `since_ns` with no
/// accepted delivery in between.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Suspect {
    /// When the current unbroken run of refusals began.
    since_ns: u64,
    /// Whether the run has already crossed the threshold and the owner is
    /// flagged unresponsive.
    flagged: bool,
}

/// Folds per-delivery outcomes into per-owner responsiveness verdicts.
///
/// Keyed by the owning app's kernel task id (the id embedded in its event
/// mailbox endpoint). Entries exist only for owners with an unbroken run of
/// refused deliveries, so the map is bounded by the number of live window
/// clients; a drained mailbox or a reaped owner removes its entry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HangTracker {
    suspects: BTreeMap<u64, Suspect>,
}

impl HangTracker {
    /// A tracker with no suspects.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            suspects: BTreeMap::new(),
        }
    }

    /// Record one refused delivery to `owner`'s event mailbox at `now_ns`.
    ///
    /// Only the kernel's mailbox-full refusal (`LengthOutOfRange`, the
    /// documented backpressure signal of the port send) is hang evidence: it
    /// proves the mailbox exists and the owner is not draining it. A send to
    /// a torn-down port (`NotFound`) means the owner is gone — that is the
    /// reap path's business, and any standing suspicion is dropped here so a
    /// recycled task id can never inherit stale evidence. Every other
    /// refusal is no evidence either way and leaves the verdict unchanged.
    ///
    /// Returns `true` when the unresponsive set changed.
    pub fn note_refused(&mut self, owner: u64, error: Errno, now_ns: u64) -> bool {
        match error {
            Errno::LengthOutOfRange => {
                let suspect = self.suspects.entry(owner).or_insert(Suspect {
                    since_ns: now_ns,
                    flagged: false,
                });
                if suspect.flagged {
                    return false;
                }
                if now_ns.saturating_sub(suspect.since_ns) >= UNRESPONSIVE_AFTER_NS {
                    suspect.flagged = true;
                    return true;
                }
                false
            }
            Errno::NotFound => self.forget(owner),
            _ => false,
        }
    }

    /// Record one accepted delivery to `owner`'s event mailbox.
    ///
    /// Acceptance proves the mailbox had room — the owner drained it — so
    /// any standing suspicion or verdict is cleared. Returns `true` when the
    /// unresponsive set changed.
    pub fn note_delivered(&mut self, owner: u64) -> bool {
        self.forget(owner)
    }

    /// Drop every record of `owner` (it exited and was reaped, or its
    /// mailbox was torn down). Returns `true` when the unresponsive set
    /// changed.
    pub fn forget(&mut self, owner: u64) -> bool {
        self.suspects
            .remove(&owner)
            .is_some_and(|suspect| suspect.flagged)
    }

    /// Whether `owner` is currently flagged unresponsive.
    #[must_use]
    pub fn is_unresponsive(&self, owner: u64) -> bool {
        self.suspects
            .get(&owner)
            .is_some_and(|suspect| suspect.flagged)
    }

    /// How many owners are currently flagged unresponsive, saturating at
    /// `u16::MAX` (the tray summary's count width).
    #[must_use]
    pub fn unresponsive_count(&self) -> u16 {
        let flagged = self
            .suspects
            .values()
            .filter(|suspect| suspect.flagged)
            .count();
        u16::try_from(flagged).unwrap_or(u16::MAX)
    }
}
