//! Window-owner responsiveness tracking — the desktop's honest
//! "not responding" detector.
//!
//! The desktop session delivers every app-ward window event as one
//! non-blocking send to the owning app's event mailbox (the
//! [`EventSink`](tairix_window::EventSink) seam). A healthy app drains that
//! mailbox promptly, so a send only comes back refused with the kernel's
//! transient `WouldBlock` back-pressure signal — the same one an empty
//! mailbox already answers a non-blocking receive with — when the app has
//! stopped consuming its events; that is the one observable, evidence-based
//! hang signal the desktop has. [`HangTracker`] folds those per-delivery
//! outcomes into a per-owner verdict: an owner whose deliveries have been
//! refused as back-pressure continuously for at least
//! [`UNRESPONSIVE_AFTER_NS`] is *unresponsive*; one accepted delivery clears
//! it, because acceptance proves the mailbox drained.
//!
//! An event the session's hold-back ([`crate::holdback`]) takes rather than
//! sends counts as one of those refusals: it is undeliverable for exactly
//! the same reason, and only a delivery the owner accepts ends the debt. Not
//! counting it would mean a wedged app — which never drains, so never frees
//! the room that would prompt another send — produced evidence only once and
//! was never flagged, which is the case this detector exists for.
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
    /// Only the kernel's transient `WouldBlock` refusal (the documented
    /// mailbox-full back-pressure signal of the port send, whether this send
    /// met it or the hold-back is still carrying an earlier one) is hang
    /// evidence:
    /// it proves the mailbox exists and the owner is not draining it. A send
    /// to a torn-down port (`NotFound`) means the owner is gone — that is the
    /// reap path's business, and any standing suspicion is dropped here so a
    /// recycled task id can never inherit stale evidence. Every other
    /// refusal — including a malformed-call error such as
    /// `LengthOutOfRange` — is no evidence either way and leaves the verdict
    /// unchanged, so a bug in the sender can never be miscounted as the
    /// receiver hanging.
    ///
    /// Returns `true` when the unresponsive set changed.
    pub fn note_refused(&mut self, owner: u64, error: Errno, now_ns: u64) -> bool {
        match error {
            Errno::WouldBlock => {
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

    /// The currently-flagged owners' ids, in ascending order — an
    /// allocation-free walk of the live set, so a caller that must bound
    /// how many it keeps (the seat report's own cap) can `take` from it
    /// rather than this tracker growing an unbounded collection itself.
    ///
    /// Pairs with [`unresponsive_count`](Self::unresponsive_count): that is
    /// the truthful total, this the (possibly partial) named few.
    pub fn unresponsive_owners(&self) -> impl Iterator<Item = u64> + '_ {
        self.suspects
            .iter()
            .filter(|(_, suspect)| suspect.flagged)
            .map(|(&owner, _)| owner)
    }
}
