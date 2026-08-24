//! The demultiplexing TCP listener and stateless SYN-cookie defence
//! (`plans/NETWORK.md` N6b-2).
//!
//! A [`Tcb`] models one connection; a [`Listener`] sits above it and owns
//! the *server* side of connection establishment for one local port. It
//! demultiplexes inbound segments by peer identity, keeps a bounded backlog
//! of half-open (SYN-RECEIVED) connections, moves completed ones onto a
//! bounded accept queue, and — when the half-open backlog is full, exactly
//! the SYN-flood condition — falls back to **stateless RFC 4987 SYN
//! cookies**, so a flood of spoofed SYNs can consume no per-connection
//! memory at all.
//!
//! # Purity
//!
//! Like the rest of `lib/net`, the listener is pure: no I/O, no syscalls, no
//! capability checks, and no randomness or cryptography of its own. Time is the
//! caller's explicit [`Duration64`]; the keyed MAC that authenticates a cookie
//! is an injected [`CookieSecret`] seam the service backs with `lib/crypto`
//! over a per-boot secret (the engine never hand-rolls crypto). Every buffer is
//! bounded and every decision fails closed, so the same code the live
//! `netstack` service runs is the code the unit, property, and fuzz tests
//! exercise.
//!
//! # The SYN-cookie trade-off (RFC 4987)
//!
//! A cookie encodes the server's initial sequence number as a keyed MAC over
//! the connection 4-tuple and a slowly-rotating counter, so the handshake
//! can be reconstructed from the client's returning ACK with no state held
//! between the SYN and the ACK. The cost is the documented option loss: a
//! cookie carries only a 3-bit MSS index, so a connection accepted via a
//! cookie negotiates **no** window scaling, SACK, or timestamps. Cookies are
//! therefore the overflow path, not the default: while the backlog has room
//! a full-state half-open connection (with options) is kept instead.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use tairix_abi::time::Duration64;

use crate::addr::{Ecn, IpAddr};
use crate::tcp::conn::{OutSegment, Tcb, TcpConfig};
use crate::tcp::{SeqNumber, TcpFlags, TcpOptions, TcpSegment, TcpSegmentMeta};
use crate::timeutil::{nanos, NEVER};

/// The peer (remote) endpoint of a connection: address and port.
///
/// The [`Tcb`] itself is address-agnostic (the caller folds the
/// pseudo-header checksum), but a listener must demultiplex by peer, so the
/// peer identity lives here, one level up.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Peer {
    /// The remote address.
    pub addr: IpAddr,
    /// The remote port.
    pub port: u16,
}

/// An accepted connection handed to the application by [`Listener::accept`]:
/// the peer it came from and the established [`Tcb`] the caller now owns and
/// drives (`send`/`recv`/`poll_transmit`/`advance`).
pub struct Connection {
    /// The peer this connection is with.
    pub peer: Peer,
    /// The established transmission control block.
    pub tcb: Tcb,
}

/// An injected keyed-MAC seam for stateless SYN cookies.
///
/// The engine never hand-rolls cryptography: it asks the caller for a keyed MAC
/// over the connection identity `tuple` and the rotating `counter`. `netstack`
/// backs this with `lib/crypto` over a per-boot secret drawn from the platform
/// RNG; tests inject a deterministic MAC. Only the low 24 bits of the result
/// are used.
pub trait CookieSecret {
    /// A keyed MAC over `tuple` and `counter`. Must be deterministic for a
    /// given `(tuple, counter)` within the secret's lifetime.
    fn mac(&self, tuple: &[u8], counter: u32) -> u32;
}

/// Tuning for a [`Listener`]. Every capacity is a fixed, caller-chosen bound
/// sized from the service's per-principal budget; nothing here is an
/// attacker-influenced allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListenConfig {
    /// Maximum half-open (SYN-RECEIVED) connections held with full state.
    /// Once this many are outstanding, further SYNs are answered with
    /// stateless cookies instead of allocating a TCB — the SYN-flood brake.
    pub max_half_open: usize,
    /// Maximum established connections awaiting [`Listener::accept`]. When
    /// full, a newly completed handshake is refused (RST) rather than
    /// growing without bound.
    pub max_accept: usize,
    /// How long a half-open connection may wait for the client's ACK before
    /// it is expired and dropped, freeing its backlog slot.
    pub half_open_timeout: Duration64,
    /// The [`TcpConfig`] template applied to accepted connections.
    pub template: TcpConfig,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            max_half_open: 256,
            max_accept: 128,
            half_open_timeout: Duration64::from_secs(10),
            template: TcpConfig::default(),
        }
    }
}

/// Observability counters for a [`Listener`], exposed for the System
/// Information API and asserted by the adversarial tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListenerStats {
    /// SYNs that opened a full-state half-open connection.
    pub half_open_started: u64,
    /// SYNs answered with a stateless cookie (backlog was full).
    pub cookies_sent: u64,
    /// Returning ACKs whose cookie validated and were accepted.
    pub cookies_accepted: u64,
    /// Returning ACKs whose cookie failed validation (RST sent).
    pub cookies_rejected: u64,
    /// Handshakes completed and moved onto the accept queue.
    pub accepted: u64,
    /// Completed handshakes refused because the accept queue was full.
    pub accept_overflow: u64,
    /// Half-open connections expired before the client's ACK arrived.
    pub half_open_expired: u64,
    /// RST segments emitted (illegal ACKs, refused connections).
    pub resets_sent: u64,
}

/// One half-open (SYN-RECEIVED) connection held with full state.
struct HalfOpen {
    peer: Peer,
    tcb: Tcb,
    /// Absolute expiry deadline, in nanoseconds (`crate::timeutil`).
    expires: u128,
}

/// A demultiplexing TCP listener for one local port with SYN-flood defence.
pub struct Listener {
    local_port: u16,
    cfg: ListenConfig,
    half_open: Vec<HalfOpen>,
    accept_queue: VecDeque<Connection>,
    stats: ListenerStats,
}

impl Listener {
    /// Create a listener bound to `local_port` with policy `cfg`.
    #[must_use]
    pub fn new(local_port: u16, cfg: ListenConfig) -> Self {
        Self {
            local_port,
            cfg,
            half_open: Vec::new(),
            accept_queue: VecDeque::new(),
            stats: ListenerStats::default(),
        }
    }

    /// The local port this listener serves.
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// The listener's running counters.
    #[must_use]
    pub fn stats(&self) -> ListenerStats {
        self.stats
    }

    /// Number of established connections awaiting [`accept`](Self::accept).
    #[must_use]
    pub fn pending(&self) -> usize {
        self.accept_queue.len()
    }

    /// Number of half-open connections currently held with full state.
    #[must_use]
    pub fn half_open_len(&self) -> usize {
        self.half_open.len()
    }

    /// Take the next established connection, or [`None`] if none are ready.
    pub fn accept(&mut self) -> Option<Connection> {
        self.accept_queue.pop_front()
    }

    /// Process one inbound segment addressed to this listener's port.
    ///
    /// `local` is the local address the segment arrived on and `peer` the
    /// remote endpoint (both supplied by the caller, which owns the IP
    /// layer). `secret` authenticates SYN cookies; `emit` serialises each
    /// reply for the given peer (returning `false` applies back-pressure and
    /// the reply is retried on the next poll). Completed connections are
    /// enqueued for [`accept`](Self::accept).
    ///
    /// Every path is total and fails closed: an illegal or unrecognised
    /// segment is answered with a RST or dropped, never partially applied.
    pub fn on_segment<F>(
        &mut self,
        local: IpAddr,
        peer: Peer,
        seg: &TcpSegment<'_>,
        now: Duration64,
        secret: &dyn CookieSecret,
        mut emit: F,
    ) where
        F: FnMut(Peer, OutSegment<'_>) -> bool,
    {
        // A segment matching a full-state half-open connection drives it.
        if let Some(index) = self.find_half_open(peer) {
            self.drive_half_open(index, peer, seg, now, &mut emit);
            return;
        }

        // A bare SYN (no ACK, no RST) opens a new connection.
        if seg.flags.syn() && !seg.flags.ack() && !seg.flags.rst() {
            self.on_syn(local, peer, seg, now, secret, &mut emit);
            return;
        }

        // An ACK with no matching half-open connection is either a returning
        // SYN cookie (reconstruct) or illegal (RST). A RST with no state is
        // silently dropped; anything else is dropped too (RFC 9293 §3.10.7.2).
        if seg.flags.ack() && !seg.flags.syn() && !seg.flags.rst() {
            self.on_bare_ack(local, peer, seg, now, secret, &mut emit);
        }
    }

    /// Advance every half-open connection's timers: retransmit an owed
    /// SYN-ACK and expire connections whose client never completed the
    /// handshake, freeing their backlog slot. Drives no accepted connection
    /// (the caller owns those once dequeued).
    pub fn advance<F>(&mut self, now: Duration64, mut emit: F)
    where
        F: FnMut(Peer, OutSegment<'_>) -> bool,
    {
        let now_ns = nanos(now);
        let mut index = 0;
        while index < self.half_open.len() {
            if now_ns >= self.half_open[index].expires {
                self.half_open.swap_remove(index);
                self.stats.half_open_expired += 1;
                continue;
            }
            let peer = self.half_open[index].peer;
            self.half_open[index].tcb.advance(now);
            self.half_open[index]
                .tcb
                .poll_transmit(now, |out| emit(peer, out));
            index += 1;
        }
    }

    /// The earliest time [`advance`](Self::advance) has work to do: the
    /// nearest half-open retransmission or expiry deadline, or [`None`] when
    /// no half-open connection is outstanding.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let mut earliest = NEVER;
        for ho in &self.half_open {
            earliest = earliest.min(ho.expires);
            if let Some(d) = ho.tcb.next_deadline() {
                earliest = earliest.min(nanos(d));
            }
        }
        if earliest == NEVER {
            None
        } else {
            Some(crate::timeutil::from_nanos(earliest))
        }
    }

    fn find_half_open(&self, peer: Peer) -> Option<usize> {
        self.half_open.iter().position(|ho| ho.peer == peer)
    }

    /// Drive a matched half-open connection with `seg`, promoting it to the
    /// accept queue once established (or refusing it if the queue is full),
    /// and reaping it if it resets or closes.
    fn drive_half_open<F>(
        &mut self,
        index: usize,
        peer: Peer,
        seg: &TcpSegment<'_>,
        now: Duration64,
        emit: &mut F,
    ) where
        F: FnMut(Peer, OutSegment<'_>) -> bool,
    {
        // Handshake segments are never ECN-Capable Transport (RFC 3168
        // §6.1.1), and the accepted connection's data ECN is handled by the
        // socket layer, so the listener drives the handshake as Not-ECT.
        self.half_open[index].tcb.on_segment(seg, Ecn::NotEct, now);
        if self.half_open[index].tcb.is_established() {
            let ho = self.half_open.swap_remove(index);
            if self.accept_queue.len() < self.cfg.max_accept {
                self.accept_queue
                    .push_back(Connection { peer, tcb: ho.tcb });
                self.stats.accepted += 1;
            } else {
                // Fail closed: refuse the completed connection with a RST
                // rather than growing the accept queue without bound.
                self.emit_rst(peer, seg.ack, emit);
                self.stats.accept_overflow += 1;
            }
            return;
        }
        // A reset or otherwise dead half-open frees its slot; otherwise flush
        // any owed SYN-ACK retransmission the segment provoked.
        if self.half_open[index].tcb.reset_reason().is_some() {
            self.half_open.swap_remove(index);
            return;
        }
        self.half_open[index]
            .tcb
            .poll_transmit(now, |out| emit(peer, out));
    }

    /// Handle a bare SYN: open a full-state half-open connection while the
    /// backlog has room, else answer with a stateless SYN cookie.
    fn on_syn<F>(
        &mut self,
        local: IpAddr,
        peer: Peer,
        seg: &TcpSegment<'_>,
        now: Duration64,
        secret: &dyn CookieSecret,
        emit: &mut F,
    ) where
        F: FnMut(Peer, OutSegment<'_>) -> bool,
    {
        if self.half_open.len() < self.cfg.max_half_open {
            // Full-state half-open: the ISN is derived from the keyed MAC —
            // the same value a cookie would carry — because the engine takes
            // no RNG, and a MAC over the 4-tuple is unpredictable to an
            // off-path attacker who cannot guess our sequence space. A
            // full-state slot is only ever completed or expired, never
            // evicted-then-reconstructed, so its full option set is kept.
            let counter = cookie_counter(now);
            let tuple = cookie_tuple(local, self.local_port, peer);
            let mss_idx = mss_index(seg.options.mss.unwrap_or(DEFAULT_COOKIE_MSS));
            let iss = encode_cookie(secret.mac(&tuple, counter), counter, mss_idx);
            let mut tcb = Tcb::listen(self.cfg.template, self.local_port, peer.port, iss);
            tcb.on_segment(seg, Ecn::NotEct, now);
            tcb.poll_transmit(now, |out| emit(peer, out));
            self.half_open.push(HalfOpen {
                peer,
                tcb,
                expires: nanos(now).saturating_add(nanos(self.cfg.half_open_timeout)),
            });
            self.stats.half_open_started += 1;
        } else {
            // Backlog full — the SYN-flood condition. Answer statelessly.
            let counter = cookie_counter(now);
            let tuple = cookie_tuple(local, self.local_port, peer);
            let mss_idx = mss_index(seg.options.mss.unwrap_or(DEFAULT_COOKIE_MSS));
            let cookie = encode_cookie(secret.mac(&tuple, counter), counter, mss_idx);
            self.emit_cookie_synack(peer, seg.seq, cookie, emit);
            self.stats.cookies_sent += 1;
        }
    }

    /// Handle a bare ACK with no matching half-open connection: validate it
    /// as a returning SYN cookie and reconstruct the connection, or RST.
    fn on_bare_ack<F>(
        &mut self,
        local: IpAddr,
        peer: Peer,
        seg: &TcpSegment<'_>,
        now: Duration64,
        secret: &dyn CookieSecret,
        emit: &mut F,
    ) where
        F: FnMut(Peer, OutSegment<'_>) -> bool,
    {
        let cookie = seg.ack.value().wrapping_sub(1);
        let tuple = cookie_tuple(local, self.local_port, peer);
        let Some(mss) = validate_cookie(cookie, &tuple, cookie_counter(now), secret) else {
            // Not a cookie we minted: refuse the phantom connection.
            self.emit_rst(peer, seg.ack, emit);
            self.stats.cookies_rejected += 1;
            return;
        };
        if self.accept_queue.len() >= self.cfg.max_accept {
            // Valid cookie but no room: drop, so the client retransmits its
            // ACK once a slot frees (fail closed, never unbounded).
            self.stats.accept_overflow += 1;
            return;
        }
        if let Some(tcb) = self.reconstruct(peer, seg, now, mss) {
            self.accept_queue.push_back(Connection { peer, tcb });
            self.stats.cookies_accepted += 1;
            self.stats.accepted += 1;
        } else {
            self.emit_rst(peer, seg.ack, emit);
        }
    }

    /// Reconstruct an established [`Tcb`] from a validated SYN-cookie ACK by
    /// replaying the handshake through the existing state machine with
    /// options disabled (the RFC 4987 trade-off). Returns [`None`] if the
    /// replay does not reach ESTABLISHED (a malformed or hostile ACK).
    fn reconstruct(
        &self,
        peer: Peer,
        seg: &TcpSegment<'_>,
        now: Duration64,
        mss: u16,
    ) -> Option<Tcb> {
        // A cookie carries no option state, so the reconstructed connection
        // negotiates none: disable window scaling, SACK, and timestamps.
        let mut cfg = self.cfg.template;
        cfg.window_scale = 0;
        cfg.enable_sack = false;
        cfg.enable_timestamps = false;
        let iss = seg.ack.value().wrapping_sub(1);
        let mut tcb = Tcb::listen(cfg, self.local_port, peer.port, iss);

        // Synthesize the client's original SYN from the returning ACK: its
        // sequence was one before the ACK's, and it advertised the window the
        // ACK now carries (scaling is off) and the MSS the cookie preserved.
        let mut options = TcpOptions::new();
        options.mss = Some(mss);
        let syn = TcpSegment {
            source_port: peer.port,
            destination_port: self.local_port,
            seq: seg.seq.sub(1),
            ack: SeqNumber::new(0),
            flags: TcpFlags::SYN,
            window: seg.window,
            urgent: 0,
            options,
            payload: &[],
        };
        tcb.on_segment(&syn, Ecn::NotEct, now);
        // Commit the resulting SYN-ACK (discarded — the cookie SYN-ACK was
        // already sent) so `snd_nxt` advances to `iss + 1` and the returning
        // ACK is acceptable.
        tcb.poll_transmit(now, |_| true);
        tcb.on_segment(seg, Ecn::NotEct, now);
        if tcb.is_established() {
            Some(tcb)
        } else {
            None
        }
    }

    /// Emit a stateless SYN-ACK carrying `cookie` as its sequence number, in
    /// reply to a client SYN whose sequence was `client_seq`.
    fn emit_cookie_synack<F>(&self, peer: Peer, client_seq: SeqNumber, cookie: u32, emit: &mut F)
    where
        F: FnMut(Peer, OutSegment<'_>) -> bool,
    {
        let mut options = TcpOptions::new();
        options.mss = Some(self.cfg.template.local_mss.max(1));
        let meta = TcpSegmentMeta {
            source_port: self.local_port,
            destination_port: peer.port,
            seq: SeqNumber::new(cookie),
            ack: client_seq.add(1),
            flags: TcpFlags::SYN | TcpFlags::ACK,
            window: self.cookie_window(),
            urgent: 0,
            options,
        };
        emit(
            peer,
            OutSegment {
                meta,
                payload: &[],
                gso_size: None,
                ecn: Ecn::NotEct,
            },
        );
    }

    /// Emit a RST refusing a segment. `seq` is the RST's sequence number (the
    /// acknowledgement field of the segment being refused, RFC 9293
    /// §3.10.7.1), so a client that sent an unexpected ACK tears the phantom
    /// connection down.
    fn emit_rst<F>(&mut self, peer: Peer, seq: SeqNumber, emit: &mut F)
    where
        F: FnMut(Peer, OutSegment<'_>) -> bool,
    {
        let meta = TcpSegmentMeta {
            source_port: self.local_port,
            destination_port: peer.port,
            seq,
            ack: SeqNumber::new(0),
            flags: TcpFlags::RST,
            window: 0,
            urgent: 0,
            options: TcpOptions::new(),
        };
        if emit(
            peer,
            OutSegment {
                meta,
                payload: &[],
                gso_size: None,
                ecn: Ecn::NotEct,
            },
        ) {
            self.stats.resets_sent += 1;
        }
    }

    /// The unscaled receive window a cookie SYN-ACK advertises: the
    /// template's receive buffer clamped to the 16-bit header field.
    fn cookie_window(&self) -> u16 {
        u16::try_from(self.cfg.template.receive_buffer).unwrap_or(u16::MAX)
    }
}

/// Default MSS assumed for a SYN that carried no MSS option, per RFC 9293
/// §3.7.1 (the IPv4 default). Only ever used to pick a cookie MSS index.
const DEFAULT_COOKIE_MSS: u16 = 536;

/// The RFC 4987 cookie MSS table: eight common MSS values a 3-bit index
/// selects, so the client's MSS survives the stateless round-trip. The
/// encoded index is the largest entry not exceeding the client's MSS.
const COOKIE_MSS_TABLE: [u16; 8] = [256, 512, 536, 1024, 1220, 1440, 1460, 8960];

/// Seconds per tick of the rotating cookie counter. A cookie is valid for
/// the current tick and the immediately previous one, i.e. up to
/// `2 × COOKIE_COUNTER_SECS` after issue — long enough to survive an RTT and
/// the client's ACK, short enough that a captured cookie expires quickly.
const COOKIE_COUNTER_SECS: u64 = 60;

/// How many prior counter values a returning cookie may still validate
/// against (in addition to the current one), tolerating a counter tick
/// between our SYN-ACK and the client's ACK.
const COOKIE_COUNTER_WINDOW: u32 = 1;

/// The rotating cookie counter derived from monotonic time.
fn cookie_counter(now: Duration64) -> u32 {
    let secs = u64::try_from(now.secs()).unwrap_or(0);
    u32::try_from((secs / COOKIE_COUNTER_SECS) & u64::from(u32::MAX)).unwrap_or(0)
}

/// The 16 canonical octets of an address (IPv6 as-is, IPv4 in the first
/// four), for the cookie MAC tuple.
fn ip_octets(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(a) => {
            let mut b = [0u8; 16];
            b[..4].copy_from_slice(&a.octets());
            b
        }
        IpAddr::V6(a) => a.octets(),
    }
}

/// The connection-identity bytes the cookie MAC is computed over: both
/// addresses and both ports. Distinct 4-tuples yield distinct tuples, so a
/// cookie minted for one connection never validates for another.
fn cookie_tuple(local: IpAddr, local_port: u16, peer: Peer) -> [u8; 36] {
    let mut t = [0u8; 36];
    t[..16].copy_from_slice(&ip_octets(local));
    t[16..32].copy_from_slice(&ip_octets(peer.addr));
    t[32..34].copy_from_slice(&local_port.to_be_bytes());
    t[34..36].copy_from_slice(&peer.port.to_be_bytes());
    t
}

/// The cookie MSS index for a client MSS: the largest table entry that does
/// not exceed it (index 0 if the client MSS is below every entry).
fn mss_index(mss: u16) -> u8 {
    let mut index = 0u8;
    for (i, &value) in COOKIE_MSS_TABLE.iter().enumerate() {
        if value <= mss {
            // `i` is bounded by the 8-entry table, so the conversion never
            // fails; keep the prior index on the impossible overflow.
            index = u8::try_from(i).unwrap_or(index);
        }
    }
    index
}

/// Assemble a cookie ISN: the 5-bit counter tick, the 3-bit MSS index, and
/// the low 24 bits of the keyed MAC.
fn encode_cookie(mac: u32, counter: u32, mss_idx: u8) -> u32 {
    let tick = (counter & 0x1f) << 27;
    let mss = (u32::from(mss_idx) & 0x7) << 24;
    tick | mss | (mac & 0x00ff_ffff)
}

/// Validate a returning cookie against the recent counter window, returning
/// the reconstructed client MSS on success or [`None`] on any mismatch.
fn validate_cookie(
    cookie: u32,
    tuple: &[u8],
    current_counter: u32,
    secret: &dyn CookieSecret,
) -> Option<u16> {
    let tick = cookie >> 27;
    let mss_idx = ((cookie >> 24) & 0x7) as usize;
    let mac_field = cookie & 0x00ff_ffff;
    // Accept the current counter and up to COOKIE_COUNTER_WINDOW prior ones,
    // tolerating a tick between our SYN-ACK and the client's ACK.
    for back in 0..=COOKIE_COUNTER_WINDOW {
        let counter = current_counter.wrapping_sub(back);
        if counter & 0x1f != tick {
            continue;
        }
        if secret.mac(tuple, counter) & 0x00ff_ffff == mac_field {
            return Some(COOKIE_MSS_TABLE[mss_idx]);
        }
    }
    None
}

#[cfg(test)]
#[path = "tcp_listen_tests.rs"]
mod tests;
