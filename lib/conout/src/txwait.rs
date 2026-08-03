//! The bounded, wedged-aware transmitter-readiness wait every port's console
//! transmit path shares.
//!
//! A character transmitter is orders of magnitude slower than the CPU, so
//! handing it a byte means waiting for room. That wait must be *bounded*: a
//! transmitter whose flow control never opens, or that is not wired to
//! anything, would otherwise stall the kernel forever on its first log line —
//! and an unbounded readiness spin is exactly the kind of hang a kernel must
//! not contain. So a wait that expires declares the transmitter wedged and the
//! byte is dropped, best-effort, rather than blocking the machine.
//!
//! The verdict is sticky. Once wedged, a transmitter is polled exactly *once*
//! per byte: it recovers the instant it accepts one, but a permanently blocked
//! transmitter costs the budget once rather than once per byte.

/// Transmit-readiness polls one byte may consume before the transmitter is
/// declared wedged and the byte dropped.
///
/// Sized for the slowest healthy drain a console supports: a 16-deep transmit
/// FIFO at 115200 baud empties in about 1.4 ms, and an MMIO status poll costs
/// well over 100 ns, so the budget covers a full FIFO drain with generous
/// headroom. A transmitter still not ready after it is not draining at all.
pub const TX_POLL_BUDGET: u32 = 200_000;

/// Verdict of one bounded transmit-readiness wait ([`tx_wait`]).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TxOutcome {
    /// The transmitter can accept the byte: write it.
    Send,
    /// The transmitter never became ready: drop the byte rather than stall the
    /// kernel on a device that is not draining.
    Drop,
}

/// Wait — boundedly — for the transmitter to accept a byte.
///
/// `tx_ready` polls the device's readiness bit; `wedged` is the sticky verdict
/// of the previous wait. A non-wedged transmitter is polled up to `budget`
/// times and expiry declares it wedged; a wedged one is polled exactly once.
/// Returns the verdict and the new wedged state, which the caller retains.
///
/// Pure over the `tx_ready` closure, so the policy is host-tested once and
/// every port supplies only its own device poll.
pub fn tx_wait(mut tx_ready: impl FnMut() -> bool, wedged: bool, budget: u32) -> (TxOutcome, bool) {
    if wedged {
        return if tx_ready() {
            (TxOutcome::Send, false)
        } else {
            (TxOutcome::Drop, true)
        };
    }
    let mut remaining = budget;
    while remaining != 0 {
        if tx_ready() {
            return (TxOutcome::Send, false);
        }
        remaining -= 1;
        core::hint::spin_loop();
    }
    (TxOutcome::Drop, true)
}

#[cfg(test)]
mod tests {
    use super::{tx_wait, TxOutcome, TX_POLL_BUDGET};

    #[test]
    fn sends_immediately_when_ready() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                true
            },
            false,
            TX_POLL_BUDGET,
        );
        assert_eq!(outcome, TxOutcome::Send);
        assert!(!wedged);
        assert_eq!(polls, 1);
    }

    #[test]
    fn sends_after_a_slow_drain_within_budget() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                polls == 7
            },
            false,
            16,
        );
        assert_eq!(outcome, TxOutcome::Send);
        assert!(!wedged);
        assert_eq!(polls, 7);
    }

    #[test]
    fn declares_a_never_ready_transmitter_wedged() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                false
            },
            false,
            16,
        );
        assert_eq!(outcome, TxOutcome::Drop);
        assert!(wedged);
        assert_eq!(polls, 16, "the budget is spent exactly once");
    }

    #[test]
    fn a_wedged_transmitter_costs_one_poll_per_byte() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                false
            },
            true,
            TX_POLL_BUDGET,
        );
        assert_eq!(outcome, TxOutcome::Drop);
        assert!(wedged, "it stays wedged");
        assert_eq!(polls, 1, "no second budget is spent");
    }

    #[test]
    fn a_wedged_transmitter_recovers_the_moment_it_accepts_a_byte() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                true
            },
            true,
            TX_POLL_BUDGET,
        );
        assert_eq!(outcome, TxOutcome::Send);
        assert!(!wedged, "readiness clears the sticky verdict");
        assert_eq!(polls, 1);
    }

    #[test]
    fn a_zero_budget_drops_without_polling() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                true
            },
            false,
            0,
        );
        assert_eq!(outcome, TxOutcome::Drop);
        assert!(wedged);
        assert_eq!(polls, 0);
    }
}
