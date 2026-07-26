//! Pluggable TCP congestion control (`crate::tcp::cc`).
//!
//! Congestion control is a *policy*, exactly as the kernel scheduler is
//! (`kernel/sched`): the connection state machine ([`crate::tcp::conn::Tcb`])
//! owns the sequence space and loss detection and consults a
//! [`CongestionControl`] object for the one number it does not own — how
//! many bytes it may keep in flight (the congestion window, `cwnd`). Adding
//! an algorithm is implementing this trait and adding a
//! [`CongestionAlgorithm`] variant; nothing else in the stack changes.
//!
//! Two algorithms ship, and the shared conformance suite (`cc::tests`)
//! holds both to the RFC-mandated invariants:
//!
//! - [`NewReno`] (RFC 6582 / RFC 5681): AIMD — slow-start doubling below
//!   `ssthresh`, one-MSS-per-RTT additive increase above it, halve on loss.
//! - [`Cubic`] (RFC 9438), the default: the window is a cubic function of
//!   the time since the last congestion event, with a Reno-friendly floor
//!   so it is never *less* aggressive than `NewReno` on a short-RTT path. The
//!   arithmetic is exact integer fixed-point (the crate is `no_std` and
//!   forbids floating point / libm, per the charter's roll-your-own rule),
//!   including a bounded integer cube root.
//!
//! Every method is pure: state in, state out, no I/O and no time source of
//! its own (`now_ns` is passed in). Windows are byte counts, never segment
//! counts, so the connection layer needs no unit conversion. `cwnd` never
//! drops below one MSS (RFC 5681 §3.1), so the sender can always make
//! forward progress.

use alloc::boxed::Box;

/// The congestion-control policy contract. Implementors decide `cwnd` (the
/// number of bytes that may be in flight) from the ACK/loss/timeout signals
/// the connection feeds them.
///
/// The connection separately tracks the peer's advertised (flow-control)
/// window and the transient fast-recovery inflation; a policy sees only the
/// *congestion* window and its own state, so the two concerns never tangle.
pub trait CongestionControl: core::fmt::Debug + Send {
    /// A short, stable name for logs and the `info:`/`stats:` surface.
    fn name(&self) -> &'static str;

    /// (Re)initialise for a connection whose effective send MSS is `mss`
    /// bytes. Sets the RFC 6928 initial window and `ssthresh` to "infinity"
    /// (RFC 5681 §3.1 permits an arbitrarily high initial threshold).
    fn init(&mut self, mss: u32);

    /// The effective send MSS changed (post-negotiation). Windows are byte
    /// counts and are unaffected; only the per-MSS arithmetic adopts it.
    fn set_mss(&mut self, mss: u32);

    /// `acked` new bytes were cumulatively acknowledged while *not* in loss
    /// recovery, with `flight` bytes in flight before this ACK. Grows the
    /// window: slow start below `ssthresh`, congestion avoidance at or above
    /// it.
    fn on_ack(&mut self, acked: u32, flight: u32, now_ns: u128);

    /// Loss detected by duplicate/selective ACKs (fast retransmit). Applies
    /// the multiplicative decrease and records the congestion epoch;
    /// `flight` is the current `FlightSize`. The connection calls this once
    /// per recovery episode.
    fn on_loss(&mut self, flight: u32, now_ns: u128);

    /// An explicit congestion notification (RFC 3168 §6.1.2): a peer echoed
    /// ECE, so a router marked one of our ECN-capable packets as Congestion
    /// Experienced. The window response is identical to a single dropped
    /// packet — the multiplicative decrease — but no retransmission follows,
    /// because nothing was lost. The connection applies this at most once
    /// per window of data, so the default deferring to [`Self::on_loss`]
    /// gives every policy the correct decrease without a second code path.
    fn on_ecn(&mut self, flight: u32, now_ns: u128) {
        self.on_loss(flight, now_ns);
    }

    /// A retransmission timeout fired: collapse `cwnd` to the loss window
    /// (one MSS) and drop `ssthresh` (RFC 5681 §3.1). Slow start restarts.
    fn on_rto(&mut self, flight: u32, now_ns: u128);

    /// The current congestion window, in bytes (always ≥ one MSS).
    fn cwnd(&self) -> u32;

    /// The current slow-start threshold, in bytes.
    fn ssthresh(&self) -> u32;
}

/// The RFC 6928 initial window: `min(10·MSS, max(2·MSS, 14600))` bytes.
fn initial_window(mss: u32) -> u32 {
    let mss = mss.max(1);
    let ten = mss.saturating_mul(10);
    let two = mss.saturating_mul(2);
    ten.min(two.max(14_600))
}

/// The multiplicative-decrease target on loss: `max(flight/2, 2·MSS)`
/// (RFC 5681 §3.2 step 2). Shared by both policies' Reno component.
fn reno_ssthresh(flight: u32, mss: u32) -> u32 {
    (flight / 2).max(mss.saturating_mul(2))
}

/// The selectable congestion-control algorithm, chosen per connection in
/// [`crate::tcp::conn::TcpConfig`]. This is the closed, versioned set of
/// policies the stack ships; it is not attacker-influenced.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum CongestionAlgorithm {
    /// RFC 9438 CUBIC — the default (best on high bandwidth-delay paths).
    #[default]
    Cubic,
    /// RFC 6582 `NewReno` — the classic AIMD sibling.
    NewReno,
}

impl CongestionAlgorithm {
    /// Build the policy object for this algorithm, initialised for `mss`.
    #[must_use]
    pub fn build(self, mss: u32) -> Box<dyn CongestionControl> {
        match self {
            Self::Cubic => Box::new(Cubic::new(mss)),
            Self::NewReno => Box::new(NewReno::new(mss)),
        }
    }
}

/// RFC 6582 / RFC 5681 `NewReno`: additive-increase / multiplicative-decrease.
///
/// Below `ssthresh` the window doubles each round trip (slow start); at or
/// above it, it grows by one MSS per round trip (congestion avoidance,
/// byte-counted so delayed ACKs do not halve the growth rate). A loss halves
/// `ssthresh` and sets `cwnd` to it; a timeout collapses `cwnd` to one MSS.
#[derive(Clone, Copy, Debug)]
pub struct NewReno {
    mss: u32,
    cwnd: u32,
    ssthresh: u32,
    /// Bytes acknowledged toward the next one-MSS congestion-avoidance step.
    ca_acked: u32,
}

impl NewReno {
    /// A `NewReno` policy initialised for a connection with send MSS `mss`.
    #[must_use]
    pub fn new(mss: u32) -> Self {
        let mss = mss.max(1);
        Self {
            mss,
            cwnd: initial_window(mss),
            ssthresh: u32::MAX,
            ca_acked: 0,
        }
    }
}

impl CongestionControl for NewReno {
    fn name(&self) -> &'static str {
        "newreno"
    }

    fn init(&mut self, mss: u32) {
        *self = Self::new(mss);
    }

    fn set_mss(&mut self, mss: u32) {
        self.mss = mss.max(1);
    }

    fn on_ack(&mut self, acked: u32, _flight: u32, _now_ns: u128) {
        if acked == 0 {
            return;
        }
        if self.cwnd < self.ssthresh {
            // Slow start: at most one MSS of growth per ACK (RFC 5681 §3.1).
            self.cwnd = self.cwnd.saturating_add(acked.min(self.mss));
        } else {
            // Congestion avoidance: one MSS per cwnd of acknowledged data.
            self.ca_acked = self.ca_acked.saturating_add(acked);
            while self.ca_acked >= self.cwnd {
                self.ca_acked -= self.cwnd;
                self.cwnd = self.cwnd.saturating_add(self.mss);
            }
        }
    }

    fn on_loss(&mut self, flight: u32, _now_ns: u128) {
        self.ssthresh = reno_ssthresh(flight, self.mss);
        self.cwnd = self.ssthresh;
        self.ca_acked = 0;
    }

    fn on_rto(&mut self, flight: u32, _now_ns: u128) {
        self.ssthresh = reno_ssthresh(flight, self.mss);
        self.cwnd = self.mss;
        self.ca_acked = 0;
    }

    fn cwnd(&self) -> u32 {
        self.cwnd.max(self.mss)
    }

    fn ssthresh(&self) -> u32 {
        self.ssthresh
    }
}

/// Floor of the real cube root of `v` (exact for every `u64`).
///
/// A monotone bit-by-bit extraction: it builds the answer one bit at a time
/// from the most significant, keeping the running cube `≤ v`, so it is total
/// and bounded (at most 22 iterations for a 64-bit input) and needs no
/// floating point. CUBIC's `K` and window target are derived from it.
fn icbrt(v: u64) -> u64 {
    let mut y: u64 = 0;
    // 64-bit values have a cube root below 2^22; walk bits 21..=0.
    for shift in (0..22).rev() {
        let candidate = y | (1u64 << shift);
        // candidate³ without overflow: candidate ≤ 2^22, cube ≤ 2^66 — use
        // u128 for the intermediate.
        let c = u128::from(candidate);
        if c * c * c <= u128::from(v) {
            y = candidate;
        }
    }
    y
}

/// CUBIC's multiplicative-decrease factor `beta = 0.7`, as a 1/1000 ratio.
const CUBIC_BETA_PERMILLE: u64 = 700;

/// CUBIC (RFC 9438).
///
/// After a congestion event the window grows as a cubic function of the time
/// since that event: concave (cautious) as it approaches the pre-loss window
/// `W_max`, then convex (probing) beyond it. A parallel Reno estimate
/// `w_est` is a floor, so on a short-RTT path where plain Reno would be
/// faster CUBIC never falls behind it (RFC 9438 §4.3, the "TCP-friendly
/// region"). All arithmetic is exact integer fixed-point: the cubic term is
/// evaluated in milliseconds and segments, and `K` comes from an integer
/// cube root.
#[derive(Clone, Copy, Debug)]
pub struct Cubic {
    mss: u32,
    cwnd: u32,
    ssthresh: u32,
    /// Window just before the last reduction (the cubic inflection point).
    w_max: u32,
    /// The Reno-friendly estimate that floors `cwnd`.
    w_est: u32,
    /// Bytes acked toward the next `w_est` one-MSS step.
    w_est_acked: u32,
    /// Time (ns) of the last congestion event; `0` until the first CA ACK
    /// after a reduction re-anchors the epoch.
    epoch_start_ns: u128,
    /// `K` in milliseconds: the time for the cubic to climb back to `w_max`.
    k_ms: u64,
    /// `w_max` captured in segments at the epoch, for the cubic term.
    w_max_seg_at_epoch: u64,
}

impl Cubic {
    /// A CUBIC policy initialised for a connection with send MSS `mss`.
    #[must_use]
    pub fn new(mss: u32) -> Self {
        let mss = mss.max(1);
        Self {
            mss,
            cwnd: initial_window(mss),
            ssthresh: u32::MAX,
            w_max: 0,
            w_est: 0,
            w_est_acked: 0,
            epoch_start_ns: 0,
            k_ms: 0,
            w_max_seg_at_epoch: 0,
        }
    }

    /// (Re)anchor the cubic epoch at `now_ns` from the current `w_max`.
    fn start_epoch(&mut self, now_ns: u128) {
        self.epoch_start_ns = now_ns.max(1);
        self.w_max_seg_at_epoch =
            (u64::from(self.w_max) + u64::from(self.mss) - 1) / u64::from(self.mss).max(1);
        // K = cbrt(W_max · (1 − beta) / C) seconds, C = 0.4, (1−beta) = 0.3.
        // (1−beta)/C = 0.75 = 3/4; scale by 1e9 = 1000³ so the cube root is
        // directly in milliseconds.
        let arg = self
            .w_max_seg_at_epoch
            .saturating_mul(3)
            .saturating_mul(1_000_000_000)
            / 4;
        self.k_ms = icbrt(arg);
        self.w_est = self.cwnd;
        self.w_est_acked = 0;
    }

    /// The cubic target window in bytes at `t_ms` past the epoch.
    fn cubic_target_bytes(&self, t_ms: u64) -> u32 {
        // W_cubic(t) = C·(t − K)³ + W_max, in segments, with C = 0.4 = 2/5
        // and t, K in seconds. In milliseconds: C·(Δms/1000)³ = 2·Δms³/5e9.
        let delta = i128::from(t_ms) - i128::from(self.k_ms);
        let cubic = (2 * delta * delta * delta) / 5_000_000_000;
        let seg = i128::from(self.w_max_seg_at_epoch) + cubic;
        let seg = seg.clamp(0, i128::from(u32::MAX / self.mss.max(1)));
        let bytes = u64::try_from(seg)
            .unwrap_or(0)
            .saturating_mul(u64::from(self.mss));
        u32::try_from(bytes).unwrap_or(u32::MAX)
    }
}

impl CongestionControl for Cubic {
    fn name(&self) -> &'static str {
        "cubic"
    }

    fn init(&mut self, mss: u32) {
        *self = Self::new(mss);
    }

    fn set_mss(&mut self, mss: u32) {
        self.mss = mss.max(1);
    }

    fn on_ack(&mut self, acked: u32, _flight: u32, now_ns: u128) {
        if acked == 0 {
            return;
        }
        if self.cwnd < self.ssthresh {
            // Slow start, identical to Reno (RFC 9438 §4.7).
            self.cwnd = self.cwnd.saturating_add(acked.min(self.mss));
            return;
        }
        if self.epoch_start_ns == 0 {
            self.start_epoch(now_ns);
        }
        // Reno-friendly estimate: a plain AIMD floor (RFC 9438 §4.3), so
        // CUBIC is never slower than NewReno on a short-RTT path.
        self.w_est_acked = self.w_est_acked.saturating_add(acked);
        while self.w_est_acked >= self.w_est.max(self.mss) {
            self.w_est_acked -= self.w_est.max(self.mss);
            self.w_est = self.w_est.saturating_add(self.mss);
        }
        // Cubic term: climb toward the target over roughly one RTT of ACKs.
        let t_ms = u64::try_from((now_ns.saturating_sub(self.epoch_start_ns)) / 1_000_000)
            .unwrap_or(u64::MAX);
        let target = self.cubic_target_bytes(t_ms);
        if target > self.cwnd {
            let inc = u64::from(target - self.cwnd).saturating_mul(u64::from(acked))
                / u64::from(self.cwnd.max(self.mss));
            self.cwnd = self
                .cwnd
                .saturating_add(u32::try_from(inc).unwrap_or(u32::MAX));
        }
        // Never below the Reno floor.
        self.cwnd = self.cwnd.max(self.w_est);
    }

    fn on_loss(&mut self, _flight: u32, _now_ns: u128) {
        // Fast convergence (RFC 9438 §4.6): if we lose before reaching the
        // previous peak, lower the peak so competing flows get room.
        if self.cwnd < self.w_max {
            self.w_max = (u64::from(self.cwnd) * (1000 + CUBIC_BETA_PERMILLE) / 2000)
                .try_into()
                .unwrap_or(self.cwnd);
        } else {
            self.w_max = self.cwnd;
        }
        let reduced = (u64::from(self.cwnd) * CUBIC_BETA_PERMILLE / 1000)
            .try_into()
            .unwrap_or(self.cwnd);
        self.ssthresh = reduced.max(self.mss.saturating_mul(2));
        self.cwnd = self.ssthresh;
        self.epoch_start_ns = 0;
    }

    fn on_rto(&mut self, _flight: u32, _now_ns: u128) {
        self.w_max = self.cwnd;
        self.ssthresh = (u64::from(self.cwnd) * CUBIC_BETA_PERMILLE / 1000)
            .try_into()
            .map_or(self.mss.saturating_mul(2), |s: u32| {
                s.max(self.mss.saturating_mul(2))
            });
        self.cwnd = self.mss;
        self.w_est = self.mss;
        self.w_est_acked = 0;
        self.epoch_start_ns = 0;
    }

    fn cwnd(&self) -> u32 {
        self.cwnd.max(self.mss)
    }

    fn ssthresh(&self) -> u32 {
        self.ssthresh
    }
}

#[cfg(test)]
#[path = "tcp_cc_tests.rs"]
mod tests;
