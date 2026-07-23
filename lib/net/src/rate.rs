//! Windowed throughput rate metering (`plans/NETWORK.md` §5, N8b).
//!
//! A [`RateMeter`] turns an interface's monotonic byte/packet counters into
//! the live *rates* (`stats:net/<iface>/rx.pps`, `tx.bps`, …) an operator
//! reads to see a link's load or a denial-of-service in progress. It is a
//! pure, integer-only, `no_std` component in the tradition of the rest of
//! this crate: it holds no clock and does no I/O — the caller feeds it
//! explicit monotonic time and the live counters, and every figure is an
//! honest average over the window that has *actually* elapsed.
//!
//! # Tickless by construction
//!
//! The meter never demands a periodic sample. It keeps a small bounded
//! ring of coalesced counter snapshots that the service records
//! opportunistically whenever it wakes for other work, so a quiet interface
//! costs nothing and no timer is armed merely to measure a rate. A read
//! computes the average from the live counters and the retained snapshot
//! nearest the requested window; when the history does not yet reach back a
//! full window (an interface that just came up, or a long-idle one) the
//! read reports the *actual* shorter window it measured over rather than
//! inventing coverage it does not have.
//!
//! # Fixed resolution, not a scaling capacity
//!
//! [`HISTORY`] and [`MIN_SAMPLE_GAP_NANOS`] fix the meter's sampling
//! *resolution* and the longest window it can report ([`MAX_WINDOW_NANOS`]);
//! they are a measurement fidelity choice, not a per-device capacity that a
//! larger machine outgrows, so they are deliberately constant.

use tairix_abi::time::{Duration64, NANOS_PER_SEC};

use crate::timeutil::nanos;

/// The monotonic per-interface counters a throughput rate is derived from.
///
/// A subset of the stack's full counter set: only the four accumulators a
/// rate is ever taken over (received/transmitted packets and bytes).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct RateCounters {
    /// Frames received from the device.
    pub rx_packets: u64,
    /// Bytes received from the device (whole Ethernet length).
    pub rx_bytes: u64,
    /// Frames emitted for transmission.
    pub tx_packets: u64,
    /// Bytes emitted for transmission (whole Ethernet length).
    pub tx_bytes: u64,
}

/// Which counter a [`RateMeter`] read is taken over.
///
/// The variant fixes both the accumulator and whether the result is a
/// packet rate (packets per second) or a bit rate (bits per second), so the
/// caller never has to remember the `× 8` byte-to-bit conversion.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RateSelector {
    /// Received packets per second (`rx.pps`).
    RxPackets,
    /// Transmitted packets per second (`tx.pps`).
    TxPackets,
    /// Received bits per second (`rx.bps`).
    RxBits,
    /// Transmitted bits per second (`tx.bps`).
    TxBits,
}

impl RateSelector {
    /// The accumulator value and the per-unit multiplier this selector
    /// reads: `1` for a packet rate, `8` for a bit rate.
    fn extract(self, counters: RateCounters) -> (u64, u128) {
        match self {
            Self::RxPackets => (counters.rx_packets, 1),
            Self::TxPackets => (counters.tx_packets, 1),
            Self::RxBits => (counters.rx_bytes, 8),
            Self::TxBits => (counters.tx_bytes, 8),
        }
    }
}

/// One resolved rate: the value in the selector's natural unit (packets or
/// bits per second) and the window it was actually averaged over.
///
/// The window is honest: it is the span between the baseline snapshot and
/// the read, which may be shorter than the caller requested when the
/// history does not yet reach back a full window. A [`Duration64::ZERO`]
/// window means there was no usable baseline yet, and the value is `0`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RateReading {
    /// The rate, in packets per second or bits per second.
    pub value: u64,
    /// The span the value was averaged over.
    pub window: Duration64,
}

/// One retained counter snapshot.
#[derive(Copy, Clone, Debug, Default)]
struct RateSample {
    /// Monotonic time of the snapshot, in nanoseconds.
    at: u128,
    /// The counters at that time.
    counters: RateCounters,
}

/// Number of snapshots the ring retains.
pub const HISTORY: usize = 32;

/// Minimum spacing between retained snapshots, in nanoseconds (250 ms).
/// Sub-gap records coalesce onto the newest snapshot so a busy interface
/// cannot flush the ring with near-instant samples and lose its
/// longer-window baselines.
pub const MIN_SAMPLE_GAP_NANOS: u128 = 250_000_000;

/// The longest window the ring can report over, in nanoseconds:
/// `(HISTORY - 1)` gaps.
pub const MAX_WINDOW_NANOS: u128 = (HISTORY as u128 - 1) * MIN_SAMPLE_GAP_NANOS;

/// A bounded, tickless windowed-rate meter over one interface's counters.
#[derive(Clone, Debug)]
pub struct RateMeter {
    buf: [RateSample; HISTORY],
    head: usize,
    len: usize,
}

impl Default for RateMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateMeter {
    /// An empty meter with no history.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [RateSample {
                at: 0,
                counters: RateCounters {
                    rx_packets: 0,
                    rx_bytes: 0,
                    tx_packets: 0,
                    tx_bytes: 0,
                },
            }; HISTORY],
            head: 0,
            len: 0,
        }
    }

    /// Index of the newest retained snapshot, if any.
    fn newest_index(&self) -> Option<usize> {
        (self.len > 0).then(|| (self.head + self.len - 1) % HISTORY)
    }

    /// Record a counter snapshot at monotonic time `now`.
    ///
    /// Called whenever the service wakes for other work; it is cheap and
    /// self-throttling. A snapshot within [`MIN_SAMPLE_GAP_NANOS`] of the
    /// newest is dropped (keeping the ring's spaced longer-window baselines)
    /// rather than consuming a slot. A snapshot that is not monotonically newer than
    /// the newest is ignored (monotonic time never goes backwards; a stray
    /// non-advancing value must not corrupt the baseline).
    pub fn record(&mut self, now: Duration64, counters: RateCounters) {
        let at = nanos(now);
        if let Some(newest) = self.newest_index() {
            let last_at = self.buf[newest].at;
            if at < last_at {
                return;
            }
            if at - last_at < MIN_SAMPLE_GAP_NANOS {
                // Within the current gap: drop the record so the ring keeps
                // its spaced baselines. The live counters a read averages
                // against are supplied to `rate`, so a slightly stale newest
                // snapshot costs nothing.
                return;
            }
        }
        let slot = (self.head + self.len) % HISTORY;
        self.buf[slot] = RateSample { at, counters };
        if self.len < HISTORY {
            self.len += 1;
        } else {
            self.head = (self.head + 1) % HISTORY;
        }
    }

    /// The average rate of `selector` over roughly `window` ending at
    /// `now`, computed from the live `current` counters and the retained
    /// snapshot nearest one `window` in the past.
    ///
    /// The result is fail-safe, never a panic: with no usable baseline (no
    /// history, or a zero-length span) it reports `0` over a
    /// [`Duration64::ZERO`] window. A counter that appears to go backwards
    /// (it never should) saturates the delta to `0` rather than wrapping.
    #[must_use]
    pub fn rate(
        &self,
        now: Duration64,
        current: RateCounters,
        window: Duration64,
        selector: RateSelector,
    ) -> RateReading {
        let now_ns = nanos(now);
        let want_ns = nanos(window);
        let cutoff = now_ns.saturating_sub(want_ns);
        // Baseline: the newest snapshot at or before the cutoff, so the
        // measured span is as close to `window` as the history allows;
        // failing that (history shorter than the window) the oldest.
        let mut baseline: Option<RateSample> = None;
        for i in 0..self.len {
            let sample = self.buf[(self.head + i) % HISTORY];
            if sample.at > now_ns {
                continue;
            }
            match baseline {
                None => baseline = Some(sample),
                Some(chosen) if sample.at <= cutoff && sample.at >= chosen.at => {
                    baseline = Some(sample);
                }
                _ => {}
            }
        }
        let Some(baseline) = baseline else {
            return RateReading {
                value: 0,
                window: Duration64::ZERO,
            };
        };
        let elapsed = now_ns.saturating_sub(baseline.at);
        if elapsed == 0 {
            return RateReading {
                value: 0,
                window: Duration64::ZERO,
            };
        }
        let (now_value, mult) = selector.extract(current);
        let (base_value, _) = selector.extract(baseline.counters);
        let delta = u128::from(now_value.saturating_sub(base_value));
        let per_sec = delta
            .saturating_mul(mult)
            .saturating_mul(u128::from(NANOS_PER_SEC))
            / elapsed;
        RateReading {
            value: u64::try_from(per_sec).unwrap_or(u64::MAX),
            window: Duration64::from_nanos(u64::try_from(elapsed).unwrap_or(u64::MAX)),
        }
    }
}

#[cfg(test)]
#[path = "rate_tests.rs"]
mod tests;
