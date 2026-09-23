//! The front: the process holding the multicast DNS sockets and every
//! authority, which never parses a byte a peer sent.
//!
//! It owns one supervised decoder ([`crate::decoder`]) and drives it:
//!
//! * **Relay.** A datagram the stack delivered is relayed only when the stack
//!   found its sender on-link and the sender is within its relay budget; the
//!   payload crosses unread. A sender past its budget is refused before the
//!   budget every sender shares is charged, so one flooding peer cannot starve
//!   the rest, and the shared budget bounds a sender rotating its address.
//! * **Back-pressure.** A datagram the decoder's queue has no room for is held,
//!   and the front stops draining the delivery port until the queue has room:
//!   the stack's own mailbox then fills and drops, which costs the front
//!   nothing per datagram while its decoder is busy.
//! * **Time.** The decoder has no clock. It reports the one instant its
//!   engines next need time, and the front answers with a tick when that
//!   instant comes — never before the decoder has reported since the last
//!   one, and never closer together than [`MIN_TICK_INTERVAL_NS`]. A tick the
//!   decoder's queue has no room for waits on the queue draining rather than
//!   on a timer, so a hostile decoder cannot turn the front into a spinning
//!   clock.
//! * **Containment.** A decoder that dies, breaks the framing, or sends a
//!   frame the front cannot believe is reaped and logged, everything derived
//!   from it is dropped, and its replacement — keyed afresh — starts after the
//!   supervisor's paced delay.
//!
//! Every instant here is monotonic nanoseconds, the unit the owner's clock and
//! wait-set speak.

use alloc::vec::Vec;

use tairix_abi::net::SocketDatagram;
use tairix_abi::net_ipc::ip_from_parts;
use tairix_abi::time::Duration64;
use tairix_abi::{Errno, FieldValue};
use tairix_log::{Event, Field, Level, Sink};
use tairix_net::rate::PeerBudgets;
use tairix_sandbox::proto::FRAME_HEADER_LEN;
use tairix_sandbox::session::{SessionBounds, SessionDescriptors, SessionError};
use tairix_sandbox::supervise::{SessionLauncher, SupervisedSession};
use tairix_util::fallible;
use tairix_util::secret::Wiped;

use crate::events::DECODER_STARTED;
use crate::wire::{
    FromDecoder, ToDecoder, CACHE_KEY_LEN, CONFIGURE_LEN, MAX_FROM_DECODER, MAX_TO_DECODER,
    RNG_KEY_LEN, TICK_LEN,
};

/// Bytes of frames the front may hold queued for its decoder.
pub const OUTBOUND_QUEUE: usize = 64 * 1024;

/// Bytes of the decoder's frames the front accumulates before taking them.
pub const INBOUND_QUEUE: usize = 256;

/// The least time between two ticks, however soon the decoder asks.
pub const MIN_TICK_INTERVAL_NS: u64 = 10_000_000;

/// Senders whose relay budget is tracked at once.
const RELAY_TRACKED_SOURCES: usize = 32;

/// Datagrams one sender may have relayed in a burst, and per second after.
const RELAY_SOURCE_BURST: u32 = 64;
const RELAY_SOURCE_RATE: u32 = 32;

/// The same for every sender together.
const RELAY_SHARED_BURST: u32 = 2048;
const RELAY_SHARED_RATE: u32 = 1024;

// Every frame either side can send fits the queue it crosses, so a refusal
// is back-pressure and never a frame that can never be sent.
const _: () = assert!(OUTBOUND_QUEUE - FRAME_HEADER_LEN >= MAX_TO_DECODER);
const _: () = assert!(INBOUND_QUEUE - FRAME_HEADER_LEN >= MAX_FROM_DECODER);

/// The CSPRNG the front keys each decoder from.
pub trait Entropy {
    /// Fill `out` with unpredictable bytes.
    ///
    /// # Errors
    ///
    /// The source's typed refusal.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), Errno>;
}

/// Why the front cannot go on at all.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Fatal {
    /// The random source refused the keys a decoder needs: without them no
    /// decoder can be started, so the service stops rather than run one
    /// under predictable keys.
    Entropy(Errno),
}

/// Where the decoder's time stands.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Clock {
    /// The decoder has reported since the last tick, or has had none.
    Idle,
    /// A tick fell due while the decoder's queue had no room for it.
    Owed,
    /// A tick was queued and the decoder has not reported since.
    Sent,
}

/// The front's state: one supervised decoder and what it has been told.
pub struct Front<L: SessionLauncher, S: Sink + Clone, E: Entropy> {
    decoder: SupervisedSession<L, S>,
    sink: S,
    entropy: E,
    admission: PeerBudgets<RELAY_TRACKED_SOURCES>,
    /// The relay's encoding buffer, holding at most one datagram frame.
    frame: Vec<u8>,
    /// The length of a frame in `frame` the decoder's queue had no room for.
    held: Option<usize>,
    /// The instant the current decoder last reported needing time.
    deadline: Option<u64>,
    clock: Clock,
    last_tick: u64,
}

impl<L: SessionLauncher, S: Sink + Clone, E: Entropy> Front<L, S, E> {
    /// A front whose decoders `launcher` starts, logging to `sink` and keying
    /// each decoder from `entropy`. The first decoder is due at once.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] when the relay's frame buffer cannot be
    /// committed.
    pub fn new(launcher: L, sink: S, entropy: E) -> Result<Self, Errno> {
        let bounds =
            SessionBounds::new(OUTBOUND_QUEUE, INBOUND_QUEUE).map_err(|_| Errno::OutOfRange)?;
        let frame = fallible::filled(MAX_TO_DECODER, 0u8).ok_or(Errno::OutOfMemory)?;
        Ok(Self {
            decoder: SupervisedSession::new(launcher, bounds, sink.clone()),
            sink,
            entropy,
            admission: PeerBudgets::new(
                RELAY_SOURCE_BURST,
                RELAY_SOURCE_RATE,
                RELAY_SHARED_BURST,
                RELAY_SHARED_RATE,
            ),
            frame,
            held: None,
            deadline: None,
            clock: Clock::Idle,
            last_tick: 0,
        })
    }

    /// The instant the owner must wake by, or `None` when only a delivery or
    /// the decoder can wake it. After [`Self::on_wake`] it is always later
    /// than the instant that call was given, so the owner never spins.
    #[must_use]
    pub fn wake_at(&self) -> Option<u64> {
        let tick = self
            .deadline
            .filter(|_| self.may_tick())
            .map(|at| at.max(self.last_tick.saturating_add(MIN_TICK_INTERVAL_NS)));
        match (self.decoder.restart_deadline(), tick) {
            (Some(restart), Some(tick)) => Some(restart.min(tick)),
            (restart, tick) => restart.or(tick),
        }
    }

    /// Do whatever `now` has reached: start and key a decoder that is due,
    /// and give a live one the tick its deadline asks for.
    ///
    /// # Errors
    ///
    /// [`Fatal`] when the keys a due decoder needs cannot be drawn.
    pub fn on_wake(&mut self, now: u64) -> Result<(), Fatal> {
        if self.decoder.restart_deadline().is_some_and(|at| now >= at) {
            self.start(now)?;
        }
        let due = self.deadline.is_some_and(|at| {
            now >= at && now >= self.last_tick.saturating_add(MIN_TICK_INTERVAL_NS)
        });
        if due && self.may_tick() {
            self.tick(now);
        }
        Ok(())
    }

    /// Whether the owner should drain the delivery port: a decoder is live
    /// and no datagram is waiting for room.
    #[must_use]
    pub fn wants_datagrams(&self) -> bool {
        self.decoder.is_live() && self.held.is_none()
    }

    /// Relay one datagram the stack delivered at `now`.
    pub fn on_datagram(&mut self, now: u64, datagram: &SocketDatagram<'_>) {
        if !datagram.source_on_link || !self.wants_datagrams() {
            return;
        }
        let source = ip_from_parts(datagram.source.family, datagram.source.addr);
        if !self.admission.allow(Duration64::from_nanos(now), source) {
            return;
        }
        let relayed = ToDecoder::Datagram {
            now,
            interface: datagram.interface,
            source,
            port: datagram.source.port,
            payload: datagram.payload,
        };
        if let Some(len) = relayed.encode(&mut self.frame) {
            self.queue(len);
        }
    }

    /// The decoder's write end has room. The held datagram goes first: it
    /// arrived before the tick fell due.
    pub fn on_decoder_writable(&mut self, now: u64) {
        if self.decoder.on_writable(now).is_err() {
            self.forget_decoder();
            return;
        }
        if let Some(len) = self.held.take() {
            self.queue(len);
        }
        if self.clock == Clock::Owed {
            self.tick(now);
        }
    }

    /// The decoder's read end has bytes, or has closed.
    pub fn on_decoder_readable(&mut self, now: u64) {
        if self.decoder.on_readable(now).is_err() {
            self.forget_decoder();
            return;
        }
        loop {
            match self.decoder.recv(now, FromDecoder::decode) {
                Ok(Some(Ok(FromDecoder::Deadline(at)))) => {
                    self.deadline = at;
                    self.clock = Clock::Idle;
                }
                Ok(Some(Err(_))) => {
                    self.decoder
                        .condemn(now, "the decoder sent a frame no decoder sends");
                    self.forget_decoder();
                    return;
                }
                Ok(None) => return,
                Err(_) => {
                    self.forget_decoder();
                    return;
                }
            }
        }
    }

    /// The live decoder's descriptors, for the owner's wait-set.
    #[must_use]
    pub fn descriptors(&self) -> Option<SessionDescriptors> {
        self.decoder.descriptors()
    }

    /// Whether the owner should arm read readiness on the decoder.
    #[must_use]
    pub fn wants_read(&self) -> bool {
        self.decoder.wants_read()
    }

    /// Whether the owner should arm write-room readiness on the decoder.
    #[must_use]
    pub fn wants_write(&self) -> bool {
        self.decoder.wants_write()
    }

    /// Whether a tick may fall due: a decoder is live to take it, and no tick
    /// is owed or awaiting the decoder's report.
    fn may_tick(&self) -> bool {
        self.decoder.is_live() && self.clock == Clock::Idle
    }

    /// Draw a fresh decoder's keys, start it, and hand them to it.
    ///
    /// The keys are drawn before the start, so a decoder is never left
    /// running without them.
    fn start(&mut self, now: u64) -> Result<(), Fatal> {
        let mut cache_key = Wiped::<CACHE_KEY_LEN>::new();
        let mut rng_key = Wiped::<RNG_KEY_LEN>::new();
        self.entropy
            .fill(&mut cache_key[..])
            .map_err(Fatal::Entropy)?;
        self.entropy
            .fill(&mut rng_key[..])
            .map_err(Fatal::Entropy)?;
        let Some(generation) = self.decoder.start(now) else {
            return Ok(());
        };
        self.forget_decoder();
        let mut frame = Wiped::<CONFIGURE_LEN>::new();
        let configure = ToDecoder::Configure {
            cache_key: &cache_key,
            rng_key: &rng_key,
        };
        // A fresh worker's queue is empty, and a configuration always fits
        // one; a worker that failed already is paced like any other.
        if let Some(len) = configure.encode(&mut frame[..]) {
            let _ = self.decoder.send(&frame[..len]);
        }
        tairix_log::log(
            &self.sink,
            &Event {
                level: Level::Info,
                id: DECODER_STARTED,
                message: "discoveryd: decoder started",
                fields: &[Field {
                    key: "generation",
                    value: FieldValue::UnsignedInt(generation),
                }],
            },
        );
        Ok(())
    }

    /// Pass the decoder time.
    fn tick(&mut self, now: u64) {
        let mut frame = [0u8; TICK_LEN];
        let Some(len) = (ToDecoder::Tick { now }).encode(&mut frame) else {
            return;
        };
        match self.decoder.send(&frame[..len]) {
            Ok(()) => {
                self.clock = Clock::Sent;
                self.last_tick = now;
            }
            Err(SessionError::WorkerFailed) => self.forget_decoder(),
            // Every refusal but a failed worker is the queue lacking room: a
            // tick always fits an empty one.
            Err(_) => self.clock = Clock::Owed,
        }
    }

    /// Queue the relay frame of `len` bytes, holding it if the queue has no
    /// room.
    fn queue(&mut self, len: usize) {
        match self.decoder.send(&self.frame[..len]) {
            Err(SessionError::OutboundFull) => self.held = Some(len),
            Err(SessionError::WorkerFailed) => self.forget_decoder(),
            // Every relay frame fits the queue, so nothing else can refuse
            // one; if it did, the datagram is dropped.
            Ok(()) | Err(_) => {}
        }
    }

    /// Drop everything learned from the current decoder: a replacement knows
    /// none of it, and a decoder that failed may have lied about all of it.
    fn forget_decoder(&mut self) {
        self.held = None;
        self.deadline = None;
        self.clock = Clock::Idle;
    }
}

#[cfg(test)]
#[path = "front_tests.rs"]
mod tests;
