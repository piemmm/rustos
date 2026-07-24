//! The TCP connection state machine (RFC 9293), pure and event-driven.
//!
//! This is the transport control block (TCB) built on the [`crate::tcp`]
//! wire codec and [`crate::tcp::SeqNumber`] arithmetic. Like every engine
//! in this crate it does no I/O and names no syscall: the caller feeds it
//! parsed inbound segments and explicit monotonic time, drives the
//! application side (`connect`/`send`/`recv`/`close`/`abort`), drains
//! outbound segments through an `emit` closure, and arms one timer from
//! [`Tcb::next_deadline`]. Randomness (the initial sequence number) is a
//! caller-supplied value drawn from the kernel CSPRNG, so the engine is
//! deterministic and replayable — the property tests and the fuzz
//! state-machine driver exercise the exact code the live service runs.
//!
//! What is implemented here (N5b + N6a congestion control + N6b-1 SACK loss
//! recovery): the full RFC 9293 state machine
//! (active and passive open, simultaneous open/close, teardown), send and
//! receive windows over [`SeqNumber`], RFC 7323 window scaling and
//! timestamps with PAWS, RFC 2018 SACK generation, RFC 6298 retransmission
//! timeout with Karn's algorithm, RFC 6675 SACK-based selective loss
//! recovery (a bounded scoreboard drives `IsLost`/`SetPipe`/`NextSeg`,
//! replacing go-back-N when the peer negotiated SACK; go-back-N remains the
//! fallback after a timeout and when SACK is absent), fast retransmit on
//! duplicate ACKs, zero-window (persist) probing, RFC 5961 in-window
//! RST/SYN/ACK checks with rate-limited challenge ACKs, delayed ACKs, the
//! RFC 9293 user timeout, and pluggable congestion control
//! ([`crate::tcp::cc`]): the send path is bounded by both the peer's
//! advertised window and the congestion window.
//! Listeners with an accept queue and stateless SYN cookies are the next
//! increment (N6b-2).
//!
//! Every table is bounded: the send and receive buffers and the
//! out-of-order reassembly set are capped at construction (fail closed on
//! overflow, never an attacker-sized allocation).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use tairix_abi::time::Duration64;

use crate::tcp::cc::{CongestionAlgorithm, CongestionControl};
use crate::tcp::{SackBlock, SeqNumber, TcpFlags, TcpOptions, TcpSegment, TcpSegmentMeta};
use crate::timeutil::{from_nanos, nanos, NEVER};

/// The RFC 9293 §3.3.2 connection states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    /// No connection.
    Closed,
    /// Waiting for a connection request from a remote peer (passive open).
    Listen,
    /// A connection request was sent; waiting for a matching request/ack.
    SynSent,
    /// A connection request was received; waiting for the confirming ack.
    SynReceived,
    /// The connection is open; data may flow both ways.
    Established,
    /// Our FIN was sent; waiting for its ack or the peer's FIN.
    FinWait1,
    /// Our FIN was acked; waiting for the peer's FIN.
    FinWait2,
    /// The peer's FIN was received; the local application may still send.
    CloseWait,
    /// Both sides sent FIN simultaneously; waiting for the final ack.
    Closing,
    /// Our FIN (after the peer's) was sent; waiting for its ack.
    LastAck,
    /// Waiting out 2·MSL so the peer's retransmitted FIN is absorbed.
    TimeWait,
}

impl State {
    /// Whether the local application may still enqueue data to send.
    #[must_use]
    const fn can_send(self) -> bool {
        matches!(self, Self::Established | Self::CloseWait)
    }
}

/// The largest window-scale shift RFC 7323 §2.3 permits (a window of at
/// most 2³⁰). A peer advertising more is clamped to this.
pub const MAX_WINDOW_SCALE: u8 = 14;

/// A [`Duration64`] of `ms` milliseconds (there is no `Duration64::from_millis`).
fn millis(ms: u64) -> Duration64 {
    Duration64::from_nanos(ms.saturating_mul(1_000_000))
}

/// Narrow a byte count to a 32-bit sequence-space offset. Every buffer here
/// is bounded far below 2³² (a TCP window cannot exceed 2³⁰), so no bits are
/// ever lost; the saturating conversion keeps the arithmetic total for a
/// pathological configuration rather than wrapping.
fn as_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The RFC 9293 §3.7.1 default send MSS when the peer offers no MSS
/// option: the conservative IPv4 value.
pub const DEFAULT_SEND_MSS: u16 = 536;

/// Why a connection ended other than by an orderly, fully-acknowledged
/// close. Surfaced to the application through [`Tcb::reset_reason`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetReason {
    /// The peer sent (or we sent, in response to an unacceptable segment)
    /// a RST: the connection was aborted.
    ConnectionReset,
    /// A connection-establishment attempt was refused (RST in reply to our
    /// SYN).
    ConnectionRefused,
    /// The retransmission budget or the RFC 9293 user timeout elapsed with
    /// data still unacknowledged: the peer is unreachable.
    TimedOut,
    /// The local application aborted the connection ([`Tcb::abort`]).
    Aborted,
}

/// Errors from the application-facing [`Tcb`] operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpError {
    /// The operation is not valid in the connection's current [`State`]
    /// (e.g. `connect` on an already-open TCB, `send` after `close`).
    InvalidState,
    /// The connection has been reset or timed out; see
    /// [`Tcb::reset_reason`].
    ConnectionClosed,
}

/// Tuning for a [`Tcb`]. Every capacity is a bounded, caller-chosen value
/// (the stack sizes them from its per-principal budget, §24); nothing here
/// is an attacker-influenced allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpConfig {
    /// The MSS this side advertises in its SYN (the largest segment it is
    /// willing to *receive*). Clamped to at least [`DEFAULT_SEND_MSS`].
    pub local_mss: u16,
    /// The window-scale shift this side advertises (`0..=MAX_WINDOW_SCALE`).
    /// Zero disables scaling on our side.
    pub window_scale: u8,
    /// Offer RFC 7323 timestamps (enables PAWS when the peer agrees).
    pub enable_timestamps: bool,
    /// Offer RFC 2018 selective acknowledgement.
    pub enable_sack: bool,
    /// Send-buffer capacity in bytes (bounds unacked + unsent data).
    pub send_buffer: usize,
    /// Receive-buffer capacity in bytes (bounds the advertised window and
    /// the in-order delivered-but-unread data).
    pub receive_buffer: usize,
    /// The maximum number of out-of-order segments held for reassembly
    /// before the oldest is dropped (fail closed, never unbounded).
    pub max_reassembly_segments: usize,
    /// Initial RTO before any RTT sample (RFC 6298 §2.1 recommends 1 s).
    pub rto_initial: Duration64,
    /// RTO floor (RFC 6298 §2.4 recommends ≥ 1 s; a lower value is
    /// permitted for low-latency links).
    pub rto_min: Duration64,
    /// RTO ceiling; the exponential backoff saturates here.
    pub rto_max: Duration64,
    /// The RFC 9293 §3.8.3 user timeout: unacknowledged data older than
    /// this aborts the connection.
    pub user_timeout: Duration64,
    /// Maximum consecutive retransmissions of the oldest unacked segment
    /// before the connection is declared timed out.
    pub max_retransmits: u32,
    /// Delay before a standalone ACK is sent (RFC 9293 §3.8.6.3; ≤ 500 ms).
    pub delayed_ack: Duration64,
    /// Maximum segment lifetime; TIME-WAIT lasts 2·MSL (RFC 9293 §3.4.2).
    pub maximum_segment_lifetime: Duration64,
    /// Minimum spacing between challenge ACKs (RFC 5961 §10 rate limit),
    /// so a hostile peer cannot induce an ACK storm.
    pub challenge_ack_interval: Duration64,
    /// The congestion-control algorithm this connection uses (RFC 9438
    /// CUBIC by default; see [`crate::tcp::cc`]).
    pub congestion: CongestionAlgorithm,
    /// Largest TCP payload the connection may hand to the egress device
    /// as one over-size *super-segment* for TCP segmentation offload
    /// (`plans/NETWORK.md` §2.3), or `0` to disable TSO (the default).
    ///
    /// When non-zero and larger than the effective per-segment MSS, the
    /// segmentizer batches up to this many fresh, never-retransmitted
    /// payload bytes from the send frontier into a single segment the
    /// device splits on the wire; retransmissions and SACK recovery stay
    /// per-MSS. The stack sets it from the egress interface's negotiated
    /// segmentation offload and transmit-ring capacity, bounded so the
    /// one IP packet the super-segment forms stays within the 16-bit IP
    /// length field.
    pub tso_max_payload: u16,
}

impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            local_mss: 1460,
            window_scale: 7,
            enable_timestamps: true,
            enable_sack: true,
            send_buffer: 64 * 1024,
            receive_buffer: 64 * 1024,
            max_reassembly_segments: 32,
            rto_initial: millis(1000),
            rto_min: millis(200),
            rto_max: Duration64::from_secs(60),
            user_timeout: Duration64::from_secs(120),
            max_retransmits: 8,
            delayed_ack: millis(100),
            maximum_segment_lifetime: Duration64::from_secs(30),
            challenge_ack_interval: millis(500),
            congestion: CongestionAlgorithm::Cubic,
            tso_max_payload: 0,
        }
    }
}

/// The RFC 6298 retransmission-timeout estimator (SRTT / RTTVAR / RTO),
/// kept in nanoseconds so the arithmetic avoids [`Duration64`] ordering.
#[derive(Clone, Copy, Debug)]
struct RtoEstimator {
    srtt: u128,
    rttvar: u128,
    rto: u128,
    min: u128,
    max: u128,
    /// Whether a first sample has been taken (RFC 6298 §2.2 vs §2.3).
    seeded: bool,
}

impl RtoEstimator {
    fn new(config: &TcpConfig) -> Self {
        Self {
            srtt: 0,
            rttvar: 0,
            rto: nanos(config.rto_initial),
            min: nanos(config.rto_min),
            max: nanos(config.rto_max),
            seeded: false,
        }
    }

    /// Clamp the RTO into `[min, max]`.
    fn clamp(&self, rto: u128) -> u128 {
        rto.clamp(self.min, self.max)
    }

    /// Fold one round-trip-time measurement in (RFC 6298 §2.2/§2.3).
    fn sample(&mut self, rtt: u128) {
        if self.seeded {
            // RTTVAR = 3/4·RTTVAR + 1/4·|SRTT − R'|
            let delta = self.srtt.abs_diff(rtt);
            self.rttvar = (self.rttvar * 3 + delta) / 4;
            // SRTT = 7/8·SRTT + 1/8·R'
            self.srtt = (self.srtt * 7 + rtt) / 8;
        } else {
            self.srtt = rtt;
            self.rttvar = rtt / 2;
            self.seeded = true;
        }
        // RTO = SRTT + max(G, 4·RTTVAR); the clock granularity G folds into
        // the min clamp.
        self.rto = self.clamp(self.srtt + 4 * self.rttvar);
    }

    /// Double the RTO on a timeout (RFC 6298 §5.5), saturating at `max`.
    fn backoff(&mut self) {
        self.rto = self.clamp(self.rto.saturating_mul(2));
    }

    fn current(&self) -> u128 {
        self.rto
    }
}

/// A bounded out-of-order reassembly set: segments received above
/// `rcv_nxt`, held until the gap fills. Overlaps are trimmed on insert;
/// the set is capped so a peer cannot force unbounded state (fail closed —
/// the oldest hole is dropped, not merged, matching the fragment engine's
/// overlap-is-drop posture).
struct Reassembly {
    /// `(start, bytes)` segments, kept sorted by `start` and non-adjacent.
    segments: Vec<(SeqNumber, Vec<u8>)>,
    max_segments: usize,
}

impl Reassembly {
    fn new(max_segments: usize) -> Self {
        Self {
            segments: Vec::new(),
            max_segments: max_segments.max(1),
        }
    }

    fn clear(&mut self) {
        self.segments.clear();
    }

    fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Insert `data` starting at `start`, trimming any part at or below
    /// `rcv_nxt` (already delivered) and coalescing with neighbours.
    /// Returns `false` (dropping the insert) when the set is full and the
    /// segment does not merge — bounded, fail closed.
    fn insert(&mut self, mut start: SeqNumber, mut data: Vec<u8>, rcv_nxt: SeqNumber) {
        // Drop the portion already in order.
        if start.lt(rcv_nxt) {
            let drop = rcv_nxt.distance_from(start) as usize;
            if drop >= data.len() {
                return;
            }
            data.drain(..drop);
            start = rcv_nxt;
        }
        if data.is_empty() {
            return;
        }
        // Every held segment starts at or after `rcv_nxt` (the contiguous
        // front is delivered and removed, never held), so the forward
        // distance from `rcv_nxt` is a valid ascending sort key within the
        // receive window.
        self.segments.push((start, data));
        self.segments.sort_by_key(|(s, _)| s.distance_from(rcv_nxt));
        self.coalesce();
        // Enforce the bound: drop the highest (newest hole) segments first,
        // so the segment nearest rcv_nxt (the one that will fill the gap) is
        // never the victim.
        while self.segments.len() > self.max_segments {
            self.segments.pop();
        }
    }

    /// Merge adjacent/overlapping segments so no two touch. Overlapping
    /// bytes from the later segment are discarded (first writer wins).
    fn coalesce(&mut self) {
        let mut merged: Vec<(SeqNumber, Vec<u8>)> = Vec::with_capacity(self.segments.len());
        for (start, data) in self.segments.drain(..) {
            match merged.last_mut() {
                Some((prev_start, prev_data)) => {
                    let prev_end = prev_start.add(as_u32(prev_data.len()));
                    if start.lt(prev_end) || start == prev_end {
                        // Overlap or adjacency: extend prev with the tail of
                        // this segment that lies past prev_end.
                        let end = start.add(as_u32(data.len()));
                        if end.lt(prev_end) || end == prev_end {
                            // Fully covered; drop.
                        } else {
                            let skip = prev_end.distance_from(start) as usize;
                            prev_data.extend_from_slice(&data[skip.min(data.len())..]);
                        }
                    } else {
                        merged.push((start, data));
                    }
                }
                None => merged.push((start, data)),
            }
        }
        self.segments = merged;
    }

    /// Pop the segment contiguous with `rcv_nxt`, if any, for delivery.
    fn pop_contiguous(&mut self, rcv_nxt: SeqNumber) -> Option<Vec<u8>> {
        let first = self.segments.first()?;
        if first.0 == rcv_nxt {
            Some(self.segments.remove(0).1)
        } else {
            None
        }
    }

    /// The RFC 2018 SACK blocks describing the held segments, most recent
    /// first, capped at [`crate::tcp::MAX_SACK_BLOCKS`].
    fn sack_blocks(&self) -> Vec<SackBlock> {
        self.segments
            .iter()
            .rev()
            .take(crate::tcp::MAX_SACK_BLOCKS)
            .map(|(start, data)| SackBlock {
                left: *start,
                right: start.add(as_u32(data.len())),
            })
            .collect()
    }
}

/// The RFC 6675 duplicate-acknowledgement threshold: three discontiguous
/// SACK blocks above a byte (or the equivalent SACKed volume) mark it lost.
const DUP_THRESH: u32 = 3;

/// The maximum number of distinct SACKed ranges the loss-recovery
/// scoreboard tracks. A coalesced scoreboard needs at most one range per
/// hole in one window of in-flight data, but a reordering or hostile peer
/// can fragment its selective acknowledgements; the count is capped so the
/// state can never grow with attacker input (fail closed — extra ranges
/// are dropped, which only makes recovery more conservative, never unsafe).
const MAX_SACK_RANGES: usize = 64;

/// The RFC 6675 SACK scoreboard: the set of send-sequence ranges the peer
/// has selectively acknowledged, held as sorted, non-overlapping,
/// non-adjacent `[left, right)` intervals within `(snd_una, snd_max]`.
///
/// This is the state RFC 6675 loss recovery reasons over: `is_lost`
/// classifies a byte as lost from the SACK pattern above it, `pipe`
/// estimates the bytes actually in flight, and `next_hole` walks the gaps
/// the sender must fill. The scoreboard holds no payload — only sequence
/// extents — so its footprint is a bounded list of range pairs, never a
/// copy of the data.
struct Scoreboard {
    /// `(left, right)` SACKed ranges, sorted ascending by distance from a
    /// reference (`snd_una`) and kept non-overlapping / non-adjacent.
    ranges: Vec<(SeqNumber, SeqNumber)>,
}

impl Scoreboard {
    fn new() -> Self {
        Self { ranges: Vec::new() }
    }

    fn clear(&mut self) {
        self.ranges.clear();
    }

    /// Fold the peer's SACK blocks into the board, clamped to the currently
    /// outstanding window `(snd_una, snd_max]`, then coalesce and bound.
    /// A block outside the window, empty, or with `right <= left` (modular)
    /// is ignored — a hostile SACK can never inject state outside the data
    /// actually in flight.
    fn record(&mut self, blocks: &[SackBlock], snd_una: SeqNumber, snd_max: SeqNumber) {
        // Drop everything at or below the cumulative ACK first: those bytes
        // are delivered and no longer part of the board.
        self.retain_above(snd_una);
        for block in blocks {
            let mut left = block.left;
            let right = block.right;
            // A well-formed block covers a non-empty forward range that lies
            // within the bytes we have sent but not yet had cumulatively
            // acknowledged.
            if !left.lt(right) {
                continue;
            }
            if left.lt(snd_una) {
                left = snd_una;
            }
            if !left.lt(right) || right.gt(snd_max) || left.lt(snd_una) {
                continue;
            }
            self.insert(left, right);
        }
        self.coalesce();
        while self.ranges.len() > MAX_SACK_RANGES {
            // Drop the highest range: the lowest holes drive retransmission,
            // so keeping the ranges nearest `snd_una` preserves the
            // decisions that matter (fail closed — a dropped range is simply
            // treated as not-yet-SACKed).
            self.ranges.pop();
        }
    }

    /// Remove any SACKed range wholly at or below `snd_una`, trimming a
    /// range that straddles it.
    fn retain_above(&mut self, snd_una: SeqNumber) {
        self.ranges.retain_mut(|(l, r)| {
            if r.le(snd_una) {
                return false;
            }
            if l.lt(snd_una) {
                *l = snd_una;
            }
            true
        });
    }

    fn insert(&mut self, left: SeqNumber, right: SeqNumber) {
        self.ranges.push((left, right));
    }

    /// Sort and merge overlapping/adjacent ranges so the board is a set of
    /// disjoint, non-touching intervals. Sorted by forward distance from the
    /// lowest left edge, which is a valid ascending key within one window.
    fn coalesce(&mut self) {
        if self.ranges.is_empty() {
            return;
        }
        let base = self
            .ranges
            .iter()
            .map(|(l, _)| *l)
            .reduce(|a, b| if a.lt(b) { a } else { b })
            .unwrap_or_else(|| SeqNumber::new(0));
        self.ranges.sort_by_key(|(l, _)| l.distance_from(base));
        let mut merged: Vec<(SeqNumber, SeqNumber)> = Vec::with_capacity(self.ranges.len());
        for (l, r) in self.ranges.drain(..) {
            match merged.last_mut() {
                Some((_, pr)) if l.le(*pr) => {
                    if pr.lt(r) {
                        *pr = r;
                    }
                }
                _ => merged.push((l, r)),
            }
        }
        self.ranges = merged;
    }

    /// Whether `seq` lies inside a SACKed range.
    #[cfg(test)]
    fn is_sacked(&self, seq: SeqNumber) -> bool {
        self.ranges.iter().any(|(l, r)| seq.ge(*l) && seq.lt(*r))
    }

    /// The highest sequence number the peer has selectively acknowledged
    /// (the right edge of the last range), or `None` when the board is empty.
    fn high_sack(&self) -> Option<SeqNumber> {
        self.ranges.last().map(|(_, r)| *r)
    }

    /// The number of SACKed bytes strictly above `seq` (RFC 6675 rule (2)).
    fn sacked_bytes_above(&self, seq: SeqNumber) -> u32 {
        let mut total = 0u32;
        for (l, r) in &self.ranges {
            let left = if l.gt(seq) { *l } else { seq };
            if left.lt(*r) {
                total = total.saturating_add(r.distance_from(left));
            }
        }
        total
    }

    /// The number of discontiguous SACKed ranges that lie entirely above
    /// `seq` (RFC 6675 rule (1) — the reordering-robust duplicate count).
    fn ranges_above(&self, seq: SeqNumber) -> u32 {
        u32::try_from(self.ranges.iter().filter(|(l, _)| l.gt(seq)).count()).unwrap_or(u32::MAX)
    }

    /// RFC 6675 `IsLost`: `seq` is lost when at least [`DUP_THRESH`]
    /// discontiguous SACK ranges lie above it, or more than
    /// `(DUP_THRESH − 1)·SMSS` bytes above it have been SACKed. Both terms
    /// are computed from the ranges strictly above `seq`, so — because a
    /// hole contains no SACK edges — the verdict is constant across a hole.
    fn is_lost(&self, seq: SeqNumber, smss: u32) -> bool {
        self.ranges_above(seq) >= DUP_THRESH
            || self.sacked_bytes_above(seq) > (DUP_THRESH - 1).saturating_mul(smss)
    }

    /// Visit the unSACKed holes within `[from, to)` in ascending order,
    /// calling `f(hole_left, hole_right)`. Stops early if `f` returns
    /// `false`. Ranges are disjoint and sorted, so this is one linear walk.
    fn for_each_hole<F: FnMut(SeqNumber, SeqNumber) -> bool>(
        &self,
        from: SeqNumber,
        to: SeqNumber,
        mut f: F,
    ) {
        if !from.lt(to) {
            return;
        }
        let mut cursor = from;
        for (l, r) in &self.ranges {
            if r.le(cursor) {
                continue;
            }
            if l.ge(to) {
                break;
            }
            if l.gt(cursor) {
                let hole_right = if l.lt(to) { *l } else { to };
                if !f(cursor, hole_right) {
                    return;
                }
            }
            if r.gt(cursor) {
                cursor = *r;
            }
            if cursor.ge(to) {
                return;
            }
        }
        if cursor.lt(to) {
            f(cursor, to);
        }
    }
}

/// One segment the engine wants transmitted: the header metadata and the
/// payload bytes (borrowed from the caller-visible `emit` scratch). The
/// caller frames it (IP + Ethernet) and folds the checksum via
/// [`crate::tcp::write`], which is the only place addresses enter.
pub struct OutSegment<'a> {
    /// The header to serialise.
    pub meta: TcpSegmentMeta,
    /// The payload (may be empty for a pure control/ACK segment).
    pub payload: &'a [u8],
    /// When `Some(mss)`, `payload` is an over-size super-segment the
    /// egress device must split into `mss`-byte segments (TCP
    /// segmentation offload); the caller frames it as one packet and
    /// tags it accordingly. `None` is an ordinary single segment.
    pub gso_size: Option<u16>,
}

/// The transmission control block: one TCP connection's state machine.
///
/// A TCB legitimately carries several independent boolean condition flags
/// (options negotiated, an ACK owed, a FIN queued, …); grouping them into a
/// sub-struct purely to satisfy the heuristic would obscure, not clarify.
#[allow(clippy::struct_excessive_bools)]
pub struct Tcb {
    config: TcpConfig,
    state: State,
    local_port: u16,
    remote_port: u16,

    // Send sequence space (RFC 9293 §3.3.1).
    iss: SeqNumber,
    snd_una: SeqNumber,
    snd_nxt: SeqNumber,
    /// The highest sequence number ever sent (RFC 6298 / Karn's algorithm:
    /// only a segment that pushes this forward carries new bytes and may be
    /// timed for an RTT sample).
    snd_max: SeqNumber,
    /// Sequence number of `tx.front()` (the first buffered data byte).
    send_data_start: SeqNumber,
    snd_wnd: u32,
    snd_wl1: SeqNumber,
    snd_wl2: SeqNumber,
    /// Shift applied to a *received* window field (the peer's scale).
    snd_wnd_shift: u8,

    // Receive sequence space.
    irs: SeqNumber,
    rcv_nxt: SeqNumber,
    /// Shift applied to the window field we *send* (our advertised scale).
    rcv_wnd_shift: u8,

    // Buffers (bounded).
    tx: VecDeque<u8>,
    rx: VecDeque<u8>,
    ooo: Reassembly,

    // Negotiated options.
    peer_mss: u16,
    send_mss: u16,
    sack_permitted: bool,
    ts_enabled: bool,
    ts_recent: u32,
    /// `TSval` to echo in `TSecr` on our next ACK (RFC 7323 §4.3).
    last_ts_echo: u32,

    // Control-bit bookkeeping.
    /// Our FIN has been queued by the application.
    fin_queued: bool,
    /// Sequence number our FIN occupies, once assigned.
    fin_seq: Option<SeqNumber>,
    /// A RST is pending emission: its sequence number and, when the reset
    /// acknowledges the peer (RFC 9293 §3.10.7.1 for an unsynchronised
    /// segment), the acknowledgement number.
    rst_pending: Option<(SeqNumber, Option<SeqNumber>)>,

    // Retransmission (RFC 6298).
    rto: RtoEstimator,
    rtx_deadline: u128,
    rtx_count: u32,
    /// The in-flight RTT sample: `(sequence just past the timed byte, send
    /// time)`. Cancelled on retransmission (Karn's algorithm), so only a
    /// segment sent exactly once is ever measured.
    rtt_sample: Option<(SeqNumber, u128)>,

    // Fast retransmit (RFC 5681 §3.2): three duplicate ACKs trigger an
    // early retransmit and a congestion-control loss event.
    last_ack: SeqNumber,
    dup_ack_count: u32,

    // Congestion control (RFC 5681 / RFC 9438). The policy owns `cwnd`; the
    // TCB bounds every send by `min(snd_wnd, cwnd)` and feeds it the
    // ACK/loss/timeout signals. `recover` is the RFC 6582 high-water mark:
    // one multiplicative decrease per loss episode, cleared once the
    // cumulative ACK passes it, so a burst of losses in one window cannot
    // halve the window repeatedly.
    cc: Box<dyn CongestionControl>,
    recover: SeqNumber,
    in_recovery: bool,

    // RFC 6675 SACK-based loss recovery. Active only when the peer agreed to
    // SACK; otherwise the connection uses the RFC 6582 NewReno go-back-N
    // fast-retransmit path above. `scoreboard` is the set of selectively
    // acknowledged send ranges; `high_rxt` is the highest sequence a
    // rule-(1) retransmission has covered; `rescue_rxt` guards the single
    // rule-(3) rescue retransmission per recovery episode.
    scoreboard: Scoreboard,
    sack_recovery: bool,
    high_rxt: SeqNumber,
    rescued: bool,

    // Zero-window persist (RFC 9293 §3.8.6.1).
    persist_deadline: u128,
    persist_shift: u32,

    // Delayed / owed ACK (RFC 9293 §3.8.6.3).
    ack_pending: bool,
    /// Whether the owed ACK must be sent at once (out-of-order data, a
    /// challenge, a FIN) rather than after the delayed-ACK timer.
    ack_immediate: bool,
    delayed_ack_deadline: u128,

    // TIME-WAIT / user timeout.
    time_wait_deadline: u128,
    user_timeout_deadline: u128,

    // RFC 5961 challenge-ACK rate limit: the last time a challenge ACK was
    // emitted, so a hostile in-window segment cannot induce an ACK storm.
    last_challenge: u128,
    /// Whether at least one challenge ACK has been emitted (so `last_challenge`
    /// is meaningful and the first challenge is always allowed).
    challenged: bool,

    /// Set once the three-way handshake completes (for the app/`accept`).
    became_established: bool,
    /// Why the connection aborted, if it did.
    reset_reason: Option<ResetReason>,
}

/// What the segmentizer decided to put on the wire next.
enum Plan {
    /// A SYN (with ACK in SYN-RECEIVED); carries the SYN options.
    Syn { with_ack: bool },
    /// A data and/or FIN segment: `len` payload bytes from `snd_nxt`, plus
    /// FIN when `fin` is set. `gso_size` is non-zero only for a
    /// segmentation-offload super-segment (`len` exceeds one MSS), and
    /// then names the per-segment MSS the device splits against.
    Data {
        len: usize,
        fin: bool,
        probe: bool,
        gso_size: u16,
    },
    /// An RFC 6675 selective retransmission: `len` payload bytes from the
    /// explicit `seq` (a hole in the SACK scoreboard), which — unlike
    /// [`Plan::Data`] — does not advance `snd_nxt`/`snd_max`.
    Retransmit { seq: SeqNumber, len: usize },
    /// A pure acknowledgement (no sequence-space consumption).
    Ack,
}

impl Tcb {
    /// Build a blank TCB in the `Closed` state.
    fn blank(config: TcpConfig, local_port: u16, remote_port: u16) -> Self {
        let rcv_wnd_shift = config.window_scale.min(MAX_WINDOW_SCALE);
        let zero = SeqNumber::new(0);
        let rto = RtoEstimator::new(&config);
        let ooo = Reassembly::new(config.max_reassembly_segments);
        let cc = config.congestion.build(u32::from(DEFAULT_SEND_MSS));
        Self {
            config,
            state: State::Closed,
            local_port,
            remote_port,
            cc,
            recover: zero,
            in_recovery: false,
            scoreboard: Scoreboard::new(),
            sack_recovery: false,
            high_rxt: zero,
            rescued: false,
            iss: zero,
            snd_una: zero,
            snd_nxt: zero,
            snd_max: zero,
            send_data_start: zero,
            snd_wnd: 0,
            snd_wl1: zero,
            snd_wl2: zero,
            snd_wnd_shift: 0,
            irs: zero,
            rcv_nxt: zero,
            rcv_wnd_shift,
            tx: VecDeque::new(),
            rx: VecDeque::new(),
            ooo,
            peer_mss: DEFAULT_SEND_MSS,
            send_mss: DEFAULT_SEND_MSS,
            sack_permitted: false,
            ts_enabled: false,
            ts_recent: 0,
            last_ts_echo: 0,
            fin_queued: false,
            fin_seq: None,
            rst_pending: None,
            rto,
            rtx_deadline: NEVER,
            rtx_count: 0,
            rtt_sample: None,
            last_ack: zero,
            dup_ack_count: 0,
            persist_deadline: NEVER,
            persist_shift: 0,
            ack_pending: false,
            ack_immediate: false,
            delayed_ack_deadline: NEVER,
            time_wait_deadline: NEVER,
            user_timeout_deadline: NEVER,
            last_challenge: 0,
            challenged: false,
            became_established: false,
            reset_reason: None,
        }
    }

    /// Open a connection actively (RFC 9293 §3.10.1): queue a SYN and enter
    /// `SynSent`. `iss` is the initial send sequence number the caller drew
    /// from the kernel CSPRNG (§22); the engine never generates randomness
    /// itself.
    #[must_use]
    pub fn connect(
        config: TcpConfig,
        local_port: u16,
        remote_port: u16,
        iss: u32,
        now: Duration64,
    ) -> Self {
        let mut tcb = Self::blank(config, local_port, remote_port);
        let iss = SeqNumber::new(iss);
        tcb.iss = iss;
        tcb.snd_una = iss;
        tcb.snd_nxt = iss;
        tcb.snd_max = iss;
        tcb.send_data_start = iss.add(1);
        tcb.state = State::SynSent;
        tcb.arm_rtx(nanos(now));
        tcb
    }

    /// Open passively (RFC 9293 §3.10.1): wait in `Listen` for a peer's SYN,
    /// which drives the connection to `SynReceived`. `iss` seeds our SYN-ACK
    /// once a SYN arrives. `remote_port` of `0` accepts a SYN from any port
    /// (the single-connection form; the demultiplexing listener is N6).
    #[must_use]
    pub fn listen(config: TcpConfig, local_port: u16, remote_port: u16, iss: u32) -> Self {
        let mut tcb = Self::blank(config, local_port, remote_port);
        tcb.iss = SeqNumber::new(iss);
        tcb.state = State::Listen;
        tcb
    }

    /// Set (or clear, with `0`) the largest TCP payload this connection may
    /// batch into one segmentation-offload super-segment
    /// ([`TcpConfig::tso_max_payload`]).
    ///
    /// A connection opened with [`connect`](Self::connect) is configured up
    /// front, but a connection produced passively by a listener is built
    /// from a template that does not know the egress link, so the service
    /// enables the offload here once the accepted child is bound to its
    /// interface. Changing it never affects data already in flight — the
    /// per-segment path and the super-segment path produce the same
    /// on-wire bytes.
    pub fn set_tso_max_payload(&mut self, max_payload: u16) {
        self.config.tso_max_payload = max_payload;
    }

    /// The current connection state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// The local port.
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// The remote port (learned from the SYN for a passive open).
    #[must_use]
    pub fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// Whether the three-way handshake has completed at least once.
    #[must_use]
    pub fn is_established(&self) -> bool {
        self.became_established
    }

    /// Why the connection aborted, if it did (`None` while live or after an
    /// orderly close).
    #[must_use]
    pub fn reset_reason(&self) -> Option<ResetReason> {
        self.reset_reason
    }

    /// Bytes of in-order received data available to read.
    #[must_use]
    pub fn recv_len(&self) -> usize {
        self.rx.len()
    }

    /// Bytes buffered for transmission not yet acknowledged (queued by the
    /// application, whether or not yet sent). The `Send-Q` a socket-listing
    /// diagnostic reports.
    #[must_use]
    pub fn send_queued(&self) -> usize {
        self.tx.len()
    }

    /// Free space in the send buffer (bytes the application may enqueue).
    #[must_use]
    pub fn send_available(&self) -> usize {
        self.config.send_buffer.saturating_sub(self.tx.len())
    }

    /// Enqueue `data` for transmission, returning how many bytes were
    /// accepted (bounded by the send buffer). Fails closed if the
    /// connection is not in a state that may send, or has been reset.
    pub fn send(&mut self, data: &[u8]) -> Result<usize, TcpError> {
        if let Some(_reason) = self.reset_reason {
            return Err(TcpError::ConnectionClosed);
        }
        if !self.state.can_send() || self.fin_queued {
            return Err(TcpError::InvalidState);
        }
        let room = self.send_available();
        let take = data.len().min(room);
        self.tx.extend(&data[..take]);
        Ok(take)
    }

    /// Copy up to `out.len()` bytes of in-order received data into `out`,
    /// returning the number copied and freeing that much receive window.
    pub fn recv(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.rx.len());
        for slot in out.iter_mut().take(n) {
            *slot = self.rx.pop_front().expect("rx has at least n bytes");
        }
        // Freeing receive buffer may reopen a shrunk window; advertise the
        // reopened window promptly with an immediate ACK.
        if n > 0 {
            self.ack_pending = true;
            self.ack_immediate = true;
        }
        n
    }

    /// Half-close the connection (RFC 9293 §3.10.4 CLOSE): queue a FIN after
    /// all buffered data. Fails closed if the connection cannot send.
    pub fn close(&mut self, _now: Duration64) -> Result<(), TcpError> {
        if self.reset_reason.is_some() {
            return Err(TcpError::ConnectionClosed);
        }
        match self.state {
            State::Established | State::CloseWait if !self.fin_queued => {
                self.fin_queued = true;
                self.fin_seq = Some(self.send_data_start.add(as_u32(self.tx.len())));
                Ok(())
            }
            State::SynSent | State::Listen => {
                // Nothing sent yet: just close.
                self.state = State::Closed;
                Ok(())
            }
            _ => Err(TcpError::InvalidState),
        }
    }

    /// Abort the connection (RFC 9293 §3.10.5 ABORT): send a RST and drop
    /// all state.
    pub fn abort(&mut self, _now: Duration64) {
        if matches!(self.state, State::Closed | State::Listen | State::TimeWait) {
            self.state = State::Closed;
            return;
        }
        let ack = if matches!(self.state, State::SynSent) {
            None
        } else {
            Some(self.rcv_nxt)
        };
        self.rst_pending = Some((self.snd_nxt, ack));
        self.abort_with(ResetReason::Aborted);
    }
}

impl Tcb {
    /// The RFC 7323 timestamp clock: a millisecond count from monotonic time.
    fn ts_now(now_ns: u128) -> u32 {
        // The RFC 7323 timestamp clock is intentionally a wrapping 32-bit
        // value; mask to 32 bits so the conversion is exact, not a truncation.
        u32::try_from((now_ns / 1_000_000) & 0xFFFF_FFFF).unwrap_or(0)
    }

    /// The unscaled receive window: free space in the receive buffer.
    fn receive_window(&self) -> u32 {
        u32::try_from(self.config.receive_buffer.saturating_sub(self.rx.len())).unwrap_or(u32::MAX)
    }

    /// The window value to advertise, scaled by our negotiated shift and
    /// clamped to the 16-bit header field.
    fn advertised_window(&self) -> u16 {
        let scaled = self.receive_window() >> self.rcv_wnd_shift;
        u16::try_from(scaled.min(u32::from(u16::MAX))).unwrap_or(u16::MAX)
    }

    /// Start the retransmission timer if it is not already running and there
    /// is unacknowledged sequence space.
    fn arm_rtx(&mut self, now_ns: u128) {
        if self.rtx_deadline == NEVER {
            self.rtx_deadline = now_ns.saturating_add(self.rto.current());
        }
    }

    /// Restart the retransmission timer from `now` (new data acknowledged
    /// with data still outstanding, RFC 6298 §5.3).
    fn restart_rtx(&mut self, now_ns: u128) {
        self.rtx_deadline = now_ns.saturating_add(self.rto.current());
    }

    /// The sequence number just past our FIN's control byte, if a FIN has
    /// been queued.
    fn fin_end(&self) -> Option<SeqNumber> {
        self.fin_seq.map(|s| s.add(1))
    }

    /// Whether our FIN has been fully acknowledged.
    fn fin_acked(&self) -> bool {
        matches!(self.fin_end(), Some(end) if self.snd_una == end)
    }

    /// Record that a challenge ACK is owed, rate-limited per RFC 5961 §10.
    fn challenge_ack(&mut self, now_ns: u128) {
        let interval = nanos(self.config.challenge_ack_interval);
        if self.challenged && now_ns.saturating_sub(self.last_challenge) < interval {
            return;
        }
        self.challenged = true;
        self.last_challenge = now_ns;
        self.ack_pending = true;
        self.ack_immediate = true;
    }

    /// Queue a bare ACK, deferring it by the delayed-ACK timer unless
    /// `immediate`.
    fn owe_ack(&mut self, now_ns: u128, immediate: bool) {
        self.ack_pending = true;
        if immediate {
            self.ack_immediate = true;
        } else if self.delayed_ack_deadline == NEVER {
            self.delayed_ack_deadline = now_ns.saturating_add(nanos(self.config.delayed_ack));
        }
    }

    /// Abort: record the reason and drop to the closed state (any RST was
    /// queued by the caller).
    fn abort_with(&mut self, reason: ResetReason) {
        self.reset_reason = Some(reason);
        self.state = State::Closed;
        self.tx.clear();
        self.ooo.clear();
        self.rtx_deadline = NEVER;
        self.persist_deadline = NEVER;
        self.delayed_ack_deadline = NEVER;
        self.user_timeout_deadline = NEVER;
        self.time_wait_deadline = NEVER;
        self.ack_pending = false;
    }

    /// Enter TIME-WAIT, arming the 2·MSL timer.
    fn enter_time_wait(&mut self, now_ns: u128) {
        self.state = State::TimeWait;
        let msl = nanos(self.config.maximum_segment_lifetime);
        self.time_wait_deadline = now_ns.saturating_add(msl.saturating_mul(2));
        self.rtx_deadline = NEVER;
        self.persist_deadline = NEVER;
        self.user_timeout_deadline = NEVER;
    }

    /// Adopt the peer's SYN options (MSS, window scale, SACK, timestamps).
    fn negotiate_from_syn(&mut self, seg: &TcpSegment<'_>) {
        self.peer_mss = seg.options.mss.unwrap_or(DEFAULT_SEND_MSS).max(1);
        // A segment we send must fit both the peer's advertised MSS *and*
        // our own link: the peer's value is what it will accept, but our
        // egress link's MTU (folded into `local_mss` by the stack for this
        // connection's family) bounds what we can put on the wire. Absent
        // the clamp, a peer advertising the IPv4 1460 over an IPv6 link
        // would have us build 1460-byte payloads that overflow the 1500
        // MTU once the 40-byte IPv6 header is added, and every full-size
        // segment would be silently dropped by the network layer.
        self.send_mss = self.peer_mss.min(self.config.local_mss.max(1));
        self.cc.set_mss(u32::from(self.send_mss));
        if let Some(ws) = seg.options.window_scale {
            self.snd_wnd_shift = ws.min(MAX_WINDOW_SCALE);
        } else {
            self.snd_wnd_shift = 0;
            self.rcv_wnd_shift = 0;
        }
        self.sack_permitted = self.config.enable_sack && seg.options.sack_permitted;
        if self.config.enable_timestamps && seg.options.timestamps.is_some() {
            self.ts_enabled = true;
            if let Some(ts) = seg.options.timestamps {
                self.ts_recent = ts.value;
                self.last_ts_echo = ts.value;
            }
        } else {
            self.ts_enabled = false;
        }
    }

    /// Queue a RST in reply to a segment that arrives at a state which
    /// cannot accept it (RFC 9293 §3.10.7.1). Never sent in reply to a RST.
    fn reset_for(&mut self, seg: &TcpSegment<'_>) {
        if seg.flags.rst() {
            return;
        }
        if seg.flags.ack() {
            self.rst_pending = Some((seg.ack, None));
        } else {
            let seg_len = segment_seq_len(seg);
            self.rst_pending = Some((SeqNumber::new(0), Some(seg.seq.add(seg_len))));
        }
    }
}

/// The sequence space a received segment occupies: its payload plus one for
/// each of SYN and FIN.
fn segment_seq_len(seg: &TcpSegment<'_>) -> u32 {
    as_u32(seg.payload.len()) + u32::from(seg.flags.syn()) + u32::from(seg.flags.fin())
}

/// Whether a segment occupying `[seq, seq + seg_len)` is acceptable against
/// the receive window `[rcv_nxt, rcv_nxt + rcv_wnd)` (RFC 9293 §3.4 Table 6).
fn segment_acceptable(seq: SeqNumber, seg_len: u32, rcv_nxt: SeqNumber, rcv_wnd: u32) -> bool {
    match (seg_len == 0, rcv_wnd == 0) {
        (true, true) => seq == rcv_nxt,
        (true, false) => seq.in_window(rcv_nxt, rcv_wnd),
        (false, true) => false,
        (false, false) => {
            seq.in_window(rcv_nxt, rcv_wnd) || seq.add(seg_len - 1).in_window(rcv_nxt, rcv_wnd)
        }
    }
}

impl Tcb {
    /// Process one inbound segment at monotonic time `now` (RFC 9293
    /// §3.10.7 SEGMENT ARRIVES). The segment is already parsed and
    /// checksum-verified by [`TcpSegment::parse`]; this drives the state
    /// machine and queues any response for [`Tcb::poll_transmit`].
    pub fn on_segment(&mut self, seg: &TcpSegment<'_>, now: Duration64) {
        let now_ns = nanos(now);
        match self.state {
            State::Closed => {
                self.reset_for(seg);
            }
            State::Listen => self.recv_in_listen(seg, now_ns),
            State::SynSent => self.recv_in_syn_sent(seg, now_ns),
            _ => self.recv_synchronized(seg, now_ns),
        }
    }

    /// RFC 9293 §3.10.7.2: a segment arriving in LISTEN.
    fn recv_in_listen(&mut self, seg: &TcpSegment<'_>, now_ns: u128) {
        if seg.flags.rst() {
            return;
        }
        if seg.flags.ack() {
            // An ACK in LISTEN is illegal: RST it.
            self.reset_for(seg);
            return;
        }
        if !seg.flags.syn() {
            return;
        }
        if self.remote_port == 0 {
            self.remote_port = seg.source_port;
        }
        self.irs = seg.seq;
        self.rcv_nxt = seg.seq.add(1);
        self.negotiate_from_syn(seg);
        self.snd_una = self.iss;
        self.snd_nxt = self.iss;
        self.snd_max = self.iss;
        self.send_data_start = self.iss.add(1);
        self.snd_wnd = u32::from(seg.window);
        self.snd_wl1 = seg.seq;
        self.snd_wl2 = self.iss;
        self.state = State::SynReceived;
        self.arm_rtx(now_ns);
    }

    /// RFC 9293 §3.10.7.3: a segment arriving in SYN-SENT.
    fn recv_in_syn_sent(&mut self, seg: &TcpSegment<'_>, now_ns: u128) {
        let ack_acceptable = if seg.flags.ack() {
            if seg.ack.le(self.iss) || seg.ack.gt(self.snd_nxt) {
                if !seg.flags.rst() {
                    self.reset_for(seg);
                }
                return;
            }
            self.snd_una.le(seg.ack) && seg.ack.le(self.snd_nxt)
        } else {
            false
        };
        if seg.flags.rst() {
            if ack_acceptable {
                self.abort_with(ResetReason::ConnectionRefused);
            }
            return;
        }
        if !seg.flags.syn() {
            return;
        }
        self.irs = seg.seq;
        self.rcv_nxt = seg.seq.add(1);
        self.negotiate_from_syn(seg);
        self.snd_wnd = u32::from(seg.window);
        self.snd_wl1 = seg.seq;
        if seg.flags.ack() {
            self.snd_una = seg.ack;
            self.drop_acked_data();
            self.snd_wl2 = seg.ack;
            self.snd_wnd = u32::from(seg.window) << self.snd_wnd_shift;
            self.state = State::Established;
            self.became_established = true;
            self.cc.init(u32::from(self.send_mss));
            if self.snd_una == self.snd_nxt {
                self.rtx_deadline = NEVER;
            } else {
                self.restart_rtx(now_ns);
            }
            self.owe_ack(now_ns, true);
        } else {
            // Simultaneous open: re-send our SYN, now carrying the ACK.
            self.state = State::SynReceived;
            self.snd_nxt = self.iss;
            self.snd_wl2 = self.iss;
            self.rtx_deadline = NEVER;
            self.arm_rtx(now_ns);
            self.owe_ack(now_ns, true);
        }
    }

    /// RFC 9293 §3.10.7.4: a segment arriving in a synchronised state.
    fn recv_synchronized(&mut self, seg: &TcpSegment<'_>, now_ns: u128) {
        let seg_len = as_u32(seg.payload.len()) + u32::from(seg.flags.fin());
        let rcv_wnd = self.receive_window();

        // PAWS (RFC 7323 §5.3): drop a segment whose timestamp is older than
        // the recent one, but acknowledge it so the peer resynchronises.
        if self.ts_enabled {
            if let Some(ts) = seg.options.timestamps {
                if SeqNumber::new(ts.value).lt(SeqNumber::new(self.ts_recent)) && !seg.flags.rst() {
                    self.owe_ack(now_ns, true);
                    return;
                }
            }
        }

        // First: acceptability (RFC 9293 §3.4).
        if !segment_acceptable(seg.seq, seg_len, self.rcv_nxt, rcv_wnd) {
            if !seg.flags.rst() {
                self.owe_ack(now_ns, true);
            }
            return;
        }

        // RST (RFC 5961 §3.2): only an exact-`rcv_nxt` RST resets; an
        // in-window-but-not-exact RST earns a challenge ACK.
        if seg.flags.rst() {
            if seg.seq == self.rcv_nxt {
                if matches!(self.state, State::SynReceived) {
                    self.abort_with(ResetReason::ConnectionRefused);
                } else {
                    self.abort_with(ResetReason::ConnectionReset);
                }
            } else {
                self.challenge_ack(now_ns);
            }
            return;
        }

        // SYN in window (RFC 5961 §4): never reset; challenge ACK.
        if seg.flags.syn() {
            self.challenge_ack(now_ns);
            return;
        }

        // A synchronised segment must carry ACK.
        if !seg.flags.ack() {
            return;
        }

        // Update the recent timestamp for an in-order segment (RFC 7323 §4.3).
        if self.ts_enabled {
            if let Some(ts) = seg.options.timestamps {
                if seg.seq.le(self.rcv_nxt)
                    && !SeqNumber::new(ts.value).lt(SeqNumber::new(self.ts_recent))
                {
                    self.ts_recent = ts.value;
                }
            }
        }

        if self.state == State::SynReceived {
            if !(self.snd_una.lt(seg.ack) && seg.ack.le(self.snd_nxt)) {
                self.reset_for(seg);
                return;
            }
            self.snd_una = seg.ack;
            self.drop_acked_data();
            self.state = State::Established;
            self.became_established = true;
            self.cc.init(u32::from(self.send_mss));
            self.snd_wnd = u32::from(seg.window) << self.snd_wnd_shift;
            self.snd_wl1 = seg.seq;
            self.snd_wl2 = seg.ack;
            self.dup_ack_count = 0;
            self.rtx_count = 0;
            if self.snd_una == self.snd_nxt {
                self.rtx_deadline = NEVER;
            } else {
                self.restart_rtx(now_ns);
            }
        } else if !self.process_ack(seg, now_ns) {
            return;
        }

        // Our-FIN acknowledgement transitions.
        match self.state {
            State::FinWait1 if self.fin_acked() => self.state = State::FinWait2,
            State::Closing if self.fin_acked() => self.enter_time_wait(now_ns),
            State::LastAck if self.fin_acked() => {
                self.state = State::Closed;
                return;
            }
            _ => {}
        }

        // TIME-WAIT: any acceptable segment restarts the 2·MSL wait and is
        // acknowledged (absorbs a retransmitted FIN).
        if self.state == State::TimeWait {
            self.enter_time_wait(now_ns);
            self.owe_ack(now_ns, true);
            return;
        }

        // Segment text and FIN.
        let ack_now = self.accept_segment_text(seg, now_ns);
        if !seg.payload.is_empty() || seg.flags.fin() || ack_now {
            self.owe_ack(now_ns, ack_now);
        }
    }

    /// Process the ACK field of a synchronised segment (RFC 9293 §3.10.7.4
    /// fifth check, with the RFC 5961 §5 blind-ack window). Returns `false`
    /// when the segment must be dropped without further processing.
    fn process_ack(&mut self, seg: &TcpSegment<'_>, now_ns: u128) -> bool {
        let ack = seg.ack;
        let prev_una = self.snd_una;
        let flight_before = self.snd_max.distance_from(prev_una);
        // ACK for something not yet sent: challenge and drop (RFC 5961 §5).
        // The bound is the highest sequence ever transmitted (`snd_max`), not
        // the next-to-send cursor `snd_nxt`: loss recovery rewinds `snd_nxt`
        // back to `snd_una` (go-back-N on RTO, fast retransmit) so it can sit
        // far below data the peer already holds and is acknowledging. Gating
        // on `snd_nxt` would reject a valid cumulative ACK covering
        // `(snd_nxt, snd_max]`, leaving `snd_una` frozen and the sender
        // retransmitting already-acknowledged data until the connection
        // times out.
        if ack.gt(self.snd_max) {
            self.challenge_ack(now_ns);
            return false;
        }
        // Fold the peer's selective acknowledgements into the scoreboard,
        // clamped to the bytes actually in flight (RFC 6675). Only when SACK
        // was negotiated; otherwise the connection uses NewReno go-back-N.
        let use_sack = self.sack_permitted && self.config.enable_sack;
        if use_sack {
            self.scoreboard
                .record(seg.options.sack(), prev_una, self.snd_max);
        }
        if ack.le(self.snd_una) {
            // Duplicate or stale ACK. Count pure duplicates (RFC 5681 §3.2):
            // same ack, no data, no window move, data still outstanding.
            let outstanding = self.snd_una.lt(self.snd_max);
            let pure_dup = ack == self.snd_una
                && seg.payload.is_empty()
                && !seg.flags.fin()
                && outstanding
                && u32::from(seg.window) << self.snd_wnd_shift == self.snd_wnd;
            if pure_dup {
                self.dup_ack_count = self.dup_ack_count.saturating_add(1);
            }
            if outstanding {
                if use_sack {
                    // RFC 6675 §5: enter recovery on the duplicate-ACK
                    // threshold or as soon as the scoreboard marks the oldest
                    // unacknowledged byte lost.
                    self.maybe_enter_sack_recovery(now_ns);
                } else if pure_dup && self.dup_ack_count == DUP_THRESH {
                    self.fast_retransmit(now_ns);
                }
            }
        } else {
            // New data acknowledged.
            self.take_rtt_sample(ack, now_ns);
            self.snd_una = ack;
            // Preserve `snd_una <= snd_nxt`: a cumulative ACK can acknowledge
            // data past the next-to-send cursor when recovery has rewound
            // `snd_nxt` below `snd_max` (go-back-N on RTO / fast retransmit).
            // The acknowledged bytes are no longer to be retransmitted, so the
            // cursor moves forward to the acknowledgement; any still-unacked
            // tail (`snd_una..snd_max`) resumes from here.
            if self.snd_nxt.lt(self.snd_una) {
                self.snd_nxt = self.snd_una;
            }
            self.drop_acked_data();
            if use_sack {
                self.scoreboard.retain_above(self.snd_una);
                // Keep HighRxt at least at the highest acknowledged octet.
                let acked_octet = self.snd_una.sub(1);
                if self.high_rxt.lt(acked_octet) {
                    self.high_rxt = acked_octet;
                }
            }
            self.dup_ack_count = 0;
            self.rtx_count = 0;
            // Congestion control: a loss episode ends once the cumulative
            // ACK passes the high-water mark recorded at the loss (RFC 6582
            // NewReno / RFC 6675). Grow the window only outside an episode;
            // during recovery the window stays at the reduced value.
            let acked = ack.distance_from(prev_una).min(flight_before);
            if self.in_recovery {
                if self.recover.le(ack) {
                    self.in_recovery = false;
                    self.sack_recovery = false;
                    self.rescued = false;
                    self.scoreboard.clear();
                }
            } else {
                self.cc.on_ack(acked, flight_before, now_ns);
                // A cumulative ACK that still leaves the scoreboard marking a
                // hole lost starts SACK recovery even without three bare
                // duplicates (RFC 6675 §5's IsLost trigger).
                if use_sack {
                    self.maybe_enter_sack_recovery(now_ns);
                }
            }
            if self.snd_una == self.snd_max {
                self.rtx_deadline = NEVER;
                self.user_timeout_deadline = NEVER;
            } else {
                self.restart_rtx(now_ns);
            }
        }
        // Send-window update (RFC 9293 §3.10.7.4).
        if self.snd_wl1.lt(seg.seq)
            || (self.snd_wl1 == seg.seq && (self.snd_wl2.lt(ack) || self.snd_wl2 == ack))
        {
            self.snd_wnd = u32::from(seg.window) << self.snd_wnd_shift;
            self.snd_wl1 = seg.seq;
            self.snd_wl2 = ack;
            if self.snd_wnd > 0 {
                self.persist_deadline = NEVER;
                self.persist_shift = 0;
            }
        }
        self.last_ack = ack;
        true
    }

    /// Fold in a round-trip-time measurement if the just-acked range covers
    /// the timed segment and Karn's algorithm has not voided it.
    fn take_rtt_sample(&mut self, ack: SeqNumber, now_ns: u128) {
        if let Some((end, sent)) = self.rtt_sample {
            if ack.gt(end) || ack == end {
                self.rto.sample(now_ns.saturating_sub(sent));
                self.rtt_sample = None;
            }
        }
    }

    /// Drop acknowledged bytes from the front of the send buffer.
    fn drop_acked_data(&mut self) {
        if self.snd_una.gt(self.send_data_start) {
            let drop =
                (self.snd_una.distance_from(self.send_data_start) as usize).min(self.tx.len());
            self.tx.drain(..drop);
            self.send_data_start = self.send_data_start.add(as_u32(drop));
        }
    }

    /// Retransmit the oldest unacknowledged segment now (RFC 5681 fast
    /// retransmit) and, on the first entry per loss window, drive the
    /// congestion-control multiplicative decrease (RFC 6582 fast recovery).
    fn fast_retransmit(&mut self, now_ns: u128) {
        // Enter fast recovery at most once per loss window (RFC 6582): apply
        // the multiplicative decrease and remember the highest sequence sent
        // so the episode ends when it is cumulatively acknowledged.
        if !self.in_recovery {
            let flight = self.snd_max.distance_from(self.snd_una);
            self.cc.on_loss(flight, now_ns);
            self.recover = self.snd_max;
            self.in_recovery = true;
        }
        self.snd_nxt = self.snd_una;
        // Karn: cancel any in-flight sample so the retransmitted range is
        // never measured.
        self.rtt_sample = None;
        self.restart_rtx(now_ns);
    }

    /// The effective send MSS in bytes (at least one), used as `SMSS`
    /// throughout the RFC 6675 loss-recovery arithmetic.
    fn smss(&self) -> u32 {
        u32::from(self.send_mss).max(1)
    }

    /// Enter RFC 6675 SACK-based fast recovery if the loss condition holds
    /// and we are not already recovering: at least [`DUP_THRESH`] duplicate
    /// acknowledgements, or the scoreboard marking the oldest unacknowledged
    /// byte lost. Applies the one multiplicative decrease per episode and
    /// records the RFC 6582 recovery high-water mark.
    fn maybe_enter_sack_recovery(&mut self, now_ns: u128) {
        if self.sack_recovery || self.in_recovery {
            return;
        }
        if !self.snd_una.lt(self.snd_max) {
            return;
        }
        let trigger =
            self.dup_ack_count >= DUP_THRESH || self.scoreboard.is_lost(self.snd_una, self.smss());
        if !trigger {
            return;
        }
        let flight = self.snd_max.distance_from(self.snd_una);
        self.cc.on_loss(flight, now_ns);
        self.recover = self.snd_max;
        self.in_recovery = true;
        self.sack_recovery = true;
        // HighRxt is the highest octet retransmitted this episode; before any
        // retransmission that is the highest *acknowledged* octet
        // (`snd_una - 1`), so NextSeg's `S2 > HighRxt` makes `snd_una` itself
        // — the first hole byte — eligible to retransmit.
        self.high_rxt = self.snd_una.sub(1);
        self.rescued = false;
        // Karn: a retransmission is about to cover this window, so no
        // in-flight sample may be measured from it.
        self.rtt_sample = None;
        self.restart_rtx(now_ns);
    }

    /// RFC 6675 `SetPipe`: the estimate of bytes currently in flight. Walks
    /// the unSACKed holes in `[snd_una, snd_max)`; a hole's octets count
    /// once if not lost (still expected in the network) and once if they
    /// have been retransmitted (`<= high_rxt`). SACKed octets are not in
    /// flight and are skipped by construction.
    fn set_pipe(&self) -> u32 {
        let smss = self.smss();
        let high_rxt = self.high_rxt;
        let mut pipe = 0u32;
        self.scoreboard
            .for_each_hole(self.snd_una, self.snd_max, |hl, hr| {
                if !self.scoreboard.is_lost(hl, smss) {
                    pipe = pipe.saturating_add(hr.distance_from(hl));
                }
                if hl.le(high_rxt) {
                    let rtx_end = high_rxt.add(1);
                    let end = if rtx_end.lt(hr) { rtx_end } else { hr };
                    if end.gt(hl) {
                        pipe = pipe.saturating_add(end.distance_from(hl));
                    }
                }
                true
            });
        pipe
    }

    /// The number of buffered bytes available to (re)send starting at `seq`,
    /// capped by the negotiated payload size and the exclusive upper bound
    /// `limit` (a hole edge or a flow-control edge).
    fn segment_len(&self, seq: SeqNumber, limit: SeqNumber, max_payload: usize) -> usize {
        if !seq.lt(limit) {
            return 0;
        }
        let offset = seq.distance_from(self.send_data_start) as usize;
        let buffered = self.tx.len().saturating_sub(offset);
        buffered
            .min(limit.distance_from(seq) as usize)
            .min(max_payload)
    }

    /// RFC 6675 §5 transmission during SACK recovery: bounded by the
    /// congestion window through the `pipe` estimate, choose the next
    /// segment via `NextSeg` — a lost hole to retransmit (rule 1), fresh
    /// data (rule 2), or a single rescue retransmission (rule 3).
    fn plan_sack(&self, now_ns: u128) -> Option<Plan> {
        let cwnd = self.cc.cwnd();
        let pipe = self.set_pipe();
        if pipe >= cwnd {
            return None;
        }
        let max_payload = (self.send_mss as usize)
            .saturating_sub(self.data_option_len())
            .max(1);
        let smss = self.smss();

        // NextSeg rule (1): the lowest lost hole that extends above HighRxt,
        // below the highest SACKed byte (there must be SACKed data above it).
        if let Some(high_sack) = self.scoreboard.high_sack() {
            let mut chosen: Option<(SeqNumber, SeqNumber)> = None;
            self.scoreboard
                .for_each_hole(self.snd_una, high_sack, |hl, hr| {
                    if !self.scoreboard.is_lost(hl, smss) {
                        return true;
                    }
                    let start = if hl.gt(self.high_rxt) {
                        hl
                    } else {
                        self.high_rxt.add(1)
                    };
                    if start.lt(hr) {
                        chosen = Some((start, hr));
                        return false;
                    }
                    true
                });
            if let Some((start, hole_end)) = chosen {
                let len = self.segment_len(start, hole_end, max_payload);
                if len > 0 {
                    return Some(Plan::Retransmit { seq: start, len });
                }
            }
        }

        // NextSeg rule (2): send fresh data from the frontier, bounded by the
        // flow-control window and the room the congestion window still has.
        if let Some(plan) = self.plan_new_data(pipe, cwnd, max_payload) {
            return Some(plan);
        }

        // NextSeg rule (3): a single rescue retransmission per episode of the
        // lowest not-yet-retransmitted hole below the highest SACKed byte,
        // so a hole the `is_lost` test has not (yet) condemned need not wait
        // for the retransmission timeout. Searching from `high_rxt + 1` (not
        // `snd_una`) guarantees it never re-sends data rule (1) already
        // retransmitted.
        let _ = now_ns;
        if !self.rescued {
            if let Some(high_sack) = self.scoreboard.high_sack() {
                let from = self.high_rxt.add(1);
                if from.lt(high_sack) {
                    let mut rescue: Option<(SeqNumber, SeqNumber)> = None;
                    self.scoreboard.for_each_hole(from, high_sack, |hl, hr| {
                        rescue = Some((hl, hr));
                        false
                    });
                    if let Some((start, hole_end)) = rescue {
                        let len = self.segment_len(start, hole_end, max_payload);
                        if len > 0 {
                            return Some(Plan::Retransmit { seq: start, len });
                        }
                    }
                }
            }
        }
        None
    }

    /// Plan a fresh-data (or FIN) segment from the send frontier while in
    /// SACK recovery, bounded by both the peer's advertised window and the
    /// congestion room `cwnd - pipe`.
    fn plan_new_data(&self, pipe: u32, cwnd: u32, max_payload: usize) -> Option<Plan> {
        let offset = self.snd_nxt.distance_from(self.send_data_start) as usize;
        let avail = self.tx.len().saturating_sub(offset);
        let fc_edge = self.snd_una.add(self.snd_wnd);
        let fc_room = if fc_edge.gt(self.snd_nxt) {
            fc_edge.distance_from(self.snd_nxt) as usize
        } else {
            0
        };
        let cc_room = cwnd.saturating_sub(pipe) as usize;
        let len = avail.min(fc_room).min(cc_room).min(max_payload);
        let mut fin = false;
        if let Some(fseq) = self.fin_seq {
            if self.snd_nxt.add(as_u32(len)) == fseq && self.snd_nxt.le(fseq) {
                fin = true;
            }
        }
        if len > 0 || fin {
            // Fresh data sent during SACK recovery stays per-MSS (no TSO).
            Some(Plan::Data {
                len,
                fin,
                probe: false,
                gso_size: 0,
            })
        } else {
            None
        }
    }

    /// Accept the segment's payload and/or FIN (RFC 9293 §3.10.7.4 seventh
    /// and eighth checks). Returns `true` when an immediate ACK is owed (an
    /// out-of-order or duplicate segment, or a FIN).
    fn accept_segment_text(&mut self, seg: &TcpSegment<'_>, now_ns: u128) -> bool {
        let receives_data = matches!(
            self.state,
            State::Established | State::FinWait1 | State::FinWait2
        );
        let mut immediate = false;
        if receives_data && !seg.payload.is_empty() {
            if seg.seq == self.rcv_nxt {
                let room = self.receive_window() as usize;
                let take = seg.payload.len().min(room);
                self.rx.extend(&seg.payload[..take]);
                self.rcv_nxt = self.rcv_nxt.add(as_u32(take));
                self.drain_ooo();
            } else if seg.seq.gt(self.rcv_nxt) {
                self.ooo.insert(seg.seq, seg.payload.to_vec(), self.rcv_nxt);
                immediate = true;
            } else {
                // Wholly below rcv_nxt: an old duplicate. Acknowledge at once.
                immediate = true;
            }
        }
        if seg.flags.fin() {
            let fin_seq = seg.seq.add(as_u32(seg.payload.len()));
            if fin_seq == self.rcv_nxt {
                self.rcv_nxt = self.rcv_nxt.add(1);
                self.on_peer_fin(now_ns);
                immediate = true;
            }
        }
        immediate
    }

    /// Deliver any reassembled segments now contiguous with `rcv_nxt` into
    /// the in-order receive buffer, respecting the receive window.
    fn drain_ooo(&mut self) {
        loop {
            let room = self.receive_window() as usize;
            if room == 0 {
                break;
            }
            let Some(chunk) = self.ooo.pop_contiguous(self.rcv_nxt) else {
                break;
            };
            let take = chunk.len().min(room);
            self.rx.extend(&chunk[..take]);
            self.rcv_nxt = self.rcv_nxt.add(as_u32(take));
            if take < chunk.len() {
                self.ooo
                    .insert(self.rcv_nxt, chunk[take..].to_vec(), self.rcv_nxt);
                break;
            }
        }
    }

    /// Handle the peer's in-order FIN (RFC 9293 §3.10.7.4 eighth check).
    fn on_peer_fin(&mut self, now_ns: u128) {
        match self.state {
            State::Established => self.state = State::CloseWait,
            State::FinWait1 => self.state = State::Closing,
            State::FinWait2 => self.enter_time_wait(now_ns),
            _ => {}
        }
    }
}

impl Tcb {
    /// Drain outbound segments, calling `emit` for each. `emit` returns
    /// `false` to apply back-pressure (a busy device): the un-emitted
    /// segment is not committed and transmission resumes on the next poll.
    /// Returns the number of segments emitted.
    pub fn poll_transmit<F>(&mut self, now: Duration64, mut emit: F) -> usize
    where
        F: FnMut(OutSegment<'_>) -> bool,
    {
        let now_ns = nanos(now);
        let mut count = 0usize;

        if let Some((seq, ack)) = self.rst_pending.take() {
            let mut flags = TcpFlags::RST;
            let ack_num = match ack {
                Some(a) => {
                    flags = flags | TcpFlags::ACK;
                    a
                }
                None => SeqNumber::new(0),
            };
            let meta = TcpSegmentMeta {
                source_port: self.local_port,
                destination_port: self.remote_port,
                seq,
                ack: ack_num,
                flags,
                window: 0,
                urgent: 0,
                options: TcpOptions::new(),
            };
            if emit(OutSegment {
                meta,
                payload: &[],
                gso_size: None,
            }) {
                count += 1;
            }
            return count;
        }

        while let Some(plan) = self.plan_segment(now_ns) {
            let (meta, payload) = self.build_segment(&plan, now_ns);
            // A super-segment carries its per-segment MSS to the framer.
            let gso_size = match &plan {
                Plan::Data { gso_size, .. } if *gso_size > 0 => Some(*gso_size),
                _ => None,
            };
            if !emit(OutSegment {
                meta,
                payload: &payload,
                gso_size,
            }) {
                break;
            }
            self.commit_segment(&plan, now_ns);
            count += 1;
        }
        count
    }

    /// Decide the next segment to emit from the current send state, or
    /// `None` when there is nothing to send.
    fn plan_segment(&self, now_ns: u128) -> Option<Plan> {
        // A SYN is owed while its sequence number has not advanced.
        if matches!(self.state, State::SynSent | State::SynReceived) && self.snd_nxt == self.iss {
            return Some(Plan::Syn {
                with_ack: matches!(self.state, State::SynReceived),
            });
        }

        // During RFC 6675 SACK recovery the segmentizer is scoreboard-driven
        // (retransmit lost holes, then fresh data, then a rescue) rather than
        // the linear-from-`snd_nxt` path below.
        if self.sack_recovery {
            if let Some(plan) = self.plan_sack(now_ns) {
                return Some(plan);
            }
            if self.ack_pending
                && (self.ack_immediate
                    || (self.delayed_ack_deadline != NEVER && now_ns >= self.delayed_ack_deadline))
            {
                return Some(Plan::Ack);
            }
            return None;
        }

        // Data and/or FIN.
        let offset = self.snd_nxt.distance_from(self.send_data_start) as usize;
        let avail = self.tx.len().saturating_sub(offset);
        let in_flight = self.snd_nxt.distance_from(self.snd_una);
        // The sender is bounded by both the peer's advertised (flow-control)
        // window and the congestion window (RFC 5681 §4.1): it may keep at
        // most `min(snd_wnd, cwnd)` bytes in flight.
        let window = self.snd_wnd.min(self.cc.cwnd());
        let usable = window.saturating_sub(in_flight) as usize;
        // A data segment's payload is the negotiated MSS *minus* the TCP
        // options it carries (RFC 6691): the MSS counts data only, so
        // timestamps/SACK bytes must come out of the payload or the whole
        // segment overflows the path MTU. `send_mss` is already clamped to
        // our egress link, so header + options + payload then fits.
        let per_seg = (self.send_mss as usize)
            .saturating_sub(self.data_option_len())
            .max(1);
        // TCP segmentation offload: when the device negotiated it and we
        // are at the send frontier (fresh, never-retransmitted data), batch
        // up to the configured cap into one super-segment the device splits
        // on the wire. Retransmissions and SACK recovery (handled above)
        // always stay per-MSS, so a lost super-segment recovers as ordinary
        // segments. Off the frontier (`snd_nxt != snd_max`, i.e. a go-back-N
        // retransmit is in progress) TSO is disabled and the cap is one MSS.
        let tso_cap = usize::from(self.config.tso_max_payload);
        let at_frontier = self.snd_nxt == self.snd_max;
        let send_cap = if tso_cap > per_seg && at_frontier {
            tso_cap
        } else {
            per_seg
        };
        let mut len = avail.min(usable).min(send_cap);
        let mut probe = false;
        if len == 0
            && avail > 0
            && usable == 0
            && self.persist_deadline != NEVER
            && now_ns >= self.persist_deadline
        {
            // Zero-window probe: send exactly one byte past the window.
            len = 1;
            probe = true;
        }
        let mut fin = false;
        if let Some(fseq) = self.fin_seq {
            if self.snd_nxt.add(as_u32(len)) == fseq && self.snd_nxt.le(fseq) {
                fin = true;
            }
        }
        if len > 0 || fin {
            // A segment spanning more than one MSS is a super-segment the
            // device splits against `gso_size`; one MSS or less is ordinary.
            let gso_size = if len > per_seg {
                u16::try_from(per_seg).unwrap_or(u16::MAX)
            } else {
                0
            };
            return Some(Plan::Data {
                len,
                fin,
                probe,
                gso_size,
            });
        }

        // A pure acknowledgement, if one is owed and due.
        if self.ack_pending
            && (self.ack_immediate
                || (self.delayed_ack_deadline != NEVER && now_ns >= self.delayed_ack_deadline))
        {
            return Some(Plan::Ack);
        }
        None
    }

    /// Materialise the header metadata and payload bytes for `plan`.
    fn build_segment(&self, plan: &Plan, now_ns: u128) -> (TcpSegmentMeta, alloc::vec::Vec<u8>) {
        let ts = Self::ts_now(now_ns);
        match plan {
            Plan::Syn { with_ack } => {
                let mut options = TcpOptions::new();
                options.mss = Some(self.config.local_mss.max(1));
                options.window_scale = Some(self.config.window_scale.min(MAX_WINDOW_SCALE));
                options.sack_permitted = self.config.enable_sack;
                if self.config.enable_timestamps {
                    options.timestamps = Some(crate::tcp::Timestamps {
                        value: ts,
                        echo: if *with_ack { self.ts_recent } else { 0 },
                    });
                }
                let mut flags = TcpFlags::SYN;
                let ack = if *with_ack {
                    flags = flags | TcpFlags::ACK;
                    self.rcv_nxt
                } else {
                    SeqNumber::new(0)
                };
                // The SYN's window field is always unscaled (scaling only
                // takes effect once both SYNs are exchanged).
                let window = u16::try_from(self.receive_window().min(u32::from(u16::MAX)))
                    .unwrap_or(u16::MAX);
                let meta = TcpSegmentMeta {
                    source_port: self.local_port,
                    destination_port: self.remote_port,
                    seq: self.iss,
                    ack,
                    flags,
                    window,
                    urgent: 0,
                    options,
                };
                (meta, alloc::vec::Vec::new())
            }
            Plan::Data { len, fin, .. } => {
                let offset = self.snd_nxt.distance_from(self.send_data_start) as usize;
                let payload: alloc::vec::Vec<u8> =
                    self.tx.iter().skip(offset).take(*len).copied().collect();
                let mut flags = TcpFlags::ACK;
                if *fin {
                    flags = flags | TcpFlags::FIN;
                }
                // Push when this segment sends the last currently-buffered byte.
                if *len > 0 && offset + *len >= self.tx.len() {
                    flags = flags | TcpFlags::PSH;
                }
                let meta = TcpSegmentMeta {
                    source_port: self.local_port,
                    destination_port: self.remote_port,
                    seq: self.snd_nxt,
                    ack: self.rcv_nxt,
                    flags,
                    window: self.advertised_window(),
                    urgent: 0,
                    options: self.data_options(ts),
                };
                (meta, payload)
            }
            Plan::Retransmit { seq, len } => {
                let offset = seq.distance_from(self.send_data_start) as usize;
                let payload: alloc::vec::Vec<u8> =
                    self.tx.iter().skip(offset).take(*len).copied().collect();
                let mut flags = TcpFlags::ACK;
                if *len > 0 && offset + *len >= self.tx.len() {
                    flags = flags | TcpFlags::PSH;
                }
                let meta = TcpSegmentMeta {
                    source_port: self.local_port,
                    destination_port: self.remote_port,
                    seq: *seq,
                    ack: self.rcv_nxt,
                    flags,
                    window: self.advertised_window(),
                    urgent: 0,
                    options: self.data_options(ts),
                };
                (meta, payload)
            }
            Plan::Ack => {
                let meta = TcpSegmentMeta {
                    source_port: self.local_port,
                    destination_port: self.remote_port,
                    seq: self.snd_nxt,
                    ack: self.rcv_nxt,
                    flags: TcpFlags::ACK,
                    window: self.advertised_window(),
                    urgent: 0,
                    options: self.data_options(ts),
                };
                (meta, alloc::vec::Vec::new())
            }
        }
    }

    /// Wire bytes a data segment's TCP options occupy (timestamps and any
    /// SACK blocks) — the overhead a full-size payload must leave room for
    /// so header + options + payload stays within the path MTU (RFC 6691).
    /// The length is independent of the timestamp *value*, so any `ts_now`
    /// gives the right size.
    fn data_option_len(&self) -> usize {
        self.data_options(0).wire_len()
    }

    /// Options for a non-SYN segment: timestamps (if negotiated) and the
    /// SACK blocks describing our reassembly holes. When both are present
    /// only three SACK blocks fit the 40-byte region alongside timestamps
    /// (RFC 7323 §3), so the set is capped accordingly.
    fn data_options(&self, ts_now: u32) -> TcpOptions {
        let mut options = TcpOptions::new();
        if self.ts_enabled {
            options.timestamps = Some(crate::tcp::Timestamps {
                value: ts_now,
                echo: self.ts_recent,
            });
        }
        if self.sack_permitted && !self.ooo.is_empty() {
            let mut blocks = self.ooo.sack_blocks();
            let max = if self.ts_enabled {
                3
            } else {
                crate::tcp::MAX_SACK_BLOCKS
            };
            blocks.truncate(max);
            let _ = options.set_sack(&blocks);
        }
        options
    }

    /// Commit the sequence-space and timer effects of an emitted segment.
    fn commit_segment(&mut self, plan: &Plan, now_ns: u128) {
        match plan {
            Plan::Syn { .. } => {
                let new_nxt = self.iss.add(1);
                self.snd_nxt = new_nxt;
                if new_nxt.gt(self.snd_max) {
                    if self.rtt_sample.is_none() {
                        self.rtt_sample = Some((new_nxt, now_ns));
                    }
                    self.snd_max = new_nxt;
                }
                self.arm_rtx(now_ns);
                self.clear_ack_owed();
            }
            Plan::Data {
                len, fin, probe, ..
            } => {
                let seqlen = as_u32(*len) + u32::from(*fin);
                let new_nxt = self.snd_nxt.add(seqlen);
                if *fin {
                    match self.state {
                        State::Established => self.state = State::FinWait1,
                        State::CloseWait => self.state = State::LastAck,
                        _ => {}
                    }
                }
                if new_nxt.gt(self.snd_max) {
                    if !*probe && seqlen > 0 && self.rtt_sample.is_none() {
                        self.rtt_sample = Some((new_nxt, now_ns));
                    }
                    self.snd_max = new_nxt;
                }
                self.snd_nxt = new_nxt;
                self.arm_rtx(now_ns);
                if self.user_timeout_deadline == NEVER {
                    self.user_timeout_deadline =
                        now_ns.saturating_add(nanos(self.config.user_timeout));
                }
                if *probe {
                    self.persist_shift = (self.persist_shift + 1).min(6);
                    self.persist_deadline =
                        now_ns.saturating_add(self.rto.current() << self.persist_shift);
                }
                self.clear_ack_owed();
            }
            Plan::Retransmit { seq, len } => {
                // A selective retransmission never advances the send frontier
                // (`snd_nxt`/`snd_max`) and — by Karn's algorithm — is never
                // timed for an RTT sample. It advances `high_rxt` (the highest
                // octet a rule-(1) retransmit has covered); a retransmission
                // of already-covered data (`seq <= high_rxt`) is the one
                // rule-(3) rescue this episode allows.
                if !seq.gt(self.high_rxt) {
                    self.rescued = true;
                }
                let last = seq.add(as_u32(*len)).sub(1);
                if last.gt(self.high_rxt) {
                    self.high_rxt = last;
                }
                self.arm_rtx(now_ns);
                if self.user_timeout_deadline == NEVER {
                    self.user_timeout_deadline =
                        now_ns.saturating_add(nanos(self.config.user_timeout));
                }
                self.clear_ack_owed();
            }
            Plan::Ack => self.clear_ack_owed(),
        }
    }

    /// An outbound segment carried our acknowledgement, so no standalone ACK
    /// is owed any longer.
    fn clear_ack_owed(&mut self) {
        self.ack_pending = false;
        self.ack_immediate = false;
        self.delayed_ack_deadline = NEVER;
    }

    /// Arm the zero-window persist timer when the window is closed and we
    /// have data waiting behind it.
    fn maybe_arm_persist(&mut self, now_ns: u128) {
        let offset = self.snd_nxt.distance_from(self.send_data_start) as usize;
        let avail = self.tx.len().saturating_sub(offset);
        if self.snd_wnd == 0 && avail > 0 && self.persist_deadline == NEVER {
            self.persist_deadline = now_ns.saturating_add(self.rto.current());
        }
    }

    /// Fire every timed transition due at `now` (RFC 6298 retransmission,
    /// zero-window persist, the RFC 9293 user timeout, and TIME-WAIT
    /// expiry). Emit the resulting segments with a following
    /// [`Tcb::poll_transmit`].
    pub fn advance(&mut self, now: Duration64) {
        let now_ns = nanos(now);

        if self.state == State::TimeWait
            && self.time_wait_deadline != NEVER
            && now_ns >= self.time_wait_deadline
        {
            self.state = State::Closed;
            self.time_wait_deadline = NEVER;
            return;
        }

        let outstanding = self.snd_una.lt(self.snd_max);

        if outstanding
            && self.user_timeout_deadline != NEVER
            && now_ns >= self.user_timeout_deadline
        {
            self.rst_pending = Some((self.snd_nxt, Some(self.rcv_nxt)));
            self.abort_with(ResetReason::TimedOut);
            return;
        }

        if outstanding && self.rtx_deadline != NEVER && now_ns >= self.rtx_deadline {
            self.rtx_count = self.rtx_count.saturating_add(1);
            if self.rtx_count > self.config.max_retransmits {
                self.rst_pending = Some((self.snd_nxt, Some(self.rcv_nxt)));
                self.abort_with(ResetReason::TimedOut);
                return;
            }
            self.rto.backoff();
            // RFC 5681 §3.1: a timeout collapses the congestion window to
            // one segment and restarts slow start, ending any fast recovery.
            let flight = self.snd_max.distance_from(self.snd_una);
            self.cc.on_rto(flight, now_ns);
            self.in_recovery = false;
            // A timeout abandons SACK recovery: the scoreboard is cleared and
            // the sender falls back to go-back-N from the oldest
            // unacknowledged byte (RFC 6675 §5.1). Future selective
            // acknowledgements rebuild the board.
            self.sack_recovery = false;
            self.scoreboard.clear();
            self.high_rxt = self.snd_una.sub(1);
            self.rescued = false;
            self.snd_nxt = self.snd_una;
            self.rtt_sample = None;
            self.dup_ack_count = 0;
            self.rtx_deadline = now_ns.saturating_add(self.rto.current());
        }

        self.maybe_arm_persist(now_ns);
    }

    /// The earliest instant a timed transition or a deferred ACK is due, for
    /// the caller's one-shot timer. `None` when nothing is pending.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let mut earliest = NEVER;
        let outstanding = self.snd_una.lt(self.snd_max);
        if outstanding {
            earliest = earliest.min(self.rtx_deadline);
            earliest = earliest.min(self.user_timeout_deadline);
        }
        earliest = earliest.min(self.persist_deadline);
        earliest = earliest.min(self.time_wait_deadline);
        if self.ack_pending && !self.ack_immediate {
            earliest = earliest.min(self.delayed_ack_deadline);
        }
        if earliest == NEVER {
            None
        } else {
            Some(from_nanos(earliest))
        }
    }
}

#[cfg(test)]
#[path = "tcp_conn_tests.rs"]
mod tests;
