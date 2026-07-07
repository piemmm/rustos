//! Kernel seat registry (`plans/DISPLAY.md` D2 — fold the input-focus
//! arbiter into a per-seat sink; `plans/PI.md` P11 — input follows the
//! surface owner).
//!
//! A **seat** is one physical display plus the keyboard and pointer attached
//! to it. This module hosts the kernel's seat: the [`rustos_seat::SeatState`]
//! owner/lease/routing state machine (the one definition shared with the
//! future user-space seat manager) under the registry's own lock, plus the
//! two input sinks that state machine routes between:
//!
//! * **Text foreground** (the default, an unowned seat): a key *press* is
//!   encoded to the console (tty) bytes a terminal sends — through the one
//!   shared [`rustos_keymap::encode_key_input`] map, never a second copy —
//!   and enqueued on the seat's text sink, where a login/shell `stream_read`
//!   drains it.
//! * **Desktop foreground** (a held seat): the whole record is routed to the
//!   seat's keyboard channel, where the seat owner (the window manager)
//!   drains it with `keyboard_read`.
//!
//! Ownership is a kernel fact, not a capability side effect: `display_acquire`
//! records the kernel-attested caller as the seat owner ([`SeatOwner`]), a
//! second task's acquire is refused (`SeatBusy`) rather than displacing the
//! holder, `display_release` and the desktop keyboard drain are owner-checked
//! (`SeatNotOwner`), and every refusal is a typed error, never a silent flip.
//! The keyboard follows the surface owner automatically — the desktop
//! analogue of "input follows the foreground tty". Routing is
//! kernel-arbitrated and capability-gated (the syscalls carry
//! `CAP_INPUT_INJECT` / `CAP_DISPLAY` / `CAP_INPUT_READ` *before* the owner
//! check); an unattached channel denies rather than leaking to a device.

use core::sync::atomic::{AtomicBool, Ordering};

use rustos_abi::driver::display::SeatGate;
use rustos_abi::input::KeyInput;
use rustos_abi::seat::{SeatLease, SEAT_PRIMARY};
use rustos_abi::sysinfo::{SeatRecord, SEAT_FLAG_OWNED};
use rustos_abi::{DriverError, Errno};
use rustos_keymap::{encode_key_input, MAX_KEY_BYTES};
use rustos_seat::{ConsoleIndex, Lease, Route, SeatError, SeatOwner, SeatState};
use rustos_sync::SpinLock;
use zeroize::Zeroize;

use crate::console::{ConsoleInput, NULL_CONSOLE_INPUT};

/// Capacity, in [`KeyInput`] records, of the desktop keyboard channel's ring.
///
/// A **fixed bound**, not a scaling capacity: the channel
/// is the desktop analogue of a console's type-ahead FIFO, and a human types a
/// handful of keys per second, so a small ring absorbs realistic type-ahead
/// between `keyboard_read` drains. A bound rather than an unbounded queue means
/// a wedged or absent window manager can never make the keyboard driver's
/// pushes grow kernel memory without limit. Overflow drops the
/// oldest record (the producer never blocks).
pub const KEYBOARD_CHANNEL_CAPACITY: usize = 64;

/// The fixed-capacity record ring behind the desktop keyboard channel.
struct ChannelRing {
    buf: [[u8; KeyInput::WIRE_LEN]; KEYBOARD_CHANNEL_CAPACITY],
    /// Index of the next record to drain.
    head: usize,
    /// Number of records currently queued.
    len: usize,
}

impl ChannelRing {
    const fn new() -> Self {
        Self {
            buf: [[0u8; KeyInput::WIRE_LEN]; KEYBOARD_CHANNEL_CAPACITY],
            head: 0,
            len: 0,
        }
    }
}

/// A bounded, lock-protected channel of decoded [`KeyInput`] records the
/// seat routes to the desktop while it is held, drained one record at a
/// time by the seat owner's `keyboard_read`.
///
/// Each drained record is **zeroed in place** as it leaves the ring: a key
/// event can carry a typed character (a password keystroke transits this
/// channel between the keyboard driver and the desktop), so the buffer must
/// not retain it after the consumer has taken it (zero-on-free
/// for memory that held a credential — secret hygiene).
struct KeyboardChannel {
    ring: SpinLock<ChannelRing>,
}

impl KeyboardChannel {
    const fn new() -> Self {
        Self {
            ring: SpinLock::new(ChannelRing::new()),
        }
    }

    /// Enqueue one record, dropping the oldest if the ring is full (the
    /// producer never blocks).
    fn push(&self, record: &[u8; KeyInput::WIRE_LEN]) {
        let mut ring = self.ring.lock();
        if ring.len == KEYBOARD_CHANNEL_CAPACITY {
            // Drop the oldest record to make room — a stale keystroke is
            // preferable to unbounded growth or refusing the live one.
            let head = ring.head;
            ring.buf[head].zeroize();
            ring.head = (head + 1) % KEYBOARD_CHANNEL_CAPACITY;
            ring.len -= 1;
        }
        let idx = (ring.head + ring.len) % KEYBOARD_CHANNEL_CAPACITY;
        ring.buf[idx] = *record;
        ring.len += 1;
    }

    /// Drain one record into `out`, zeroing the drained slot, and return the
    /// number of bytes written ([`KeyInput::WIRE_LEN`], or `0` when empty).
    ///
    /// `out` is assumed to be at least [`KeyInput::WIRE_LEN`] bytes (the caller
    /// checks the bound first).
    fn drain_one(&self, out: &mut [u8]) -> usize {
        let mut ring = self.ring.lock();
        if ring.len == 0 {
            return 0;
        }
        let idx = ring.head;
        out[..KeyInput::WIRE_LEN].copy_from_slice(&ring.buf[idx]);
        ring.buf[idx].zeroize();
        ring.head = (ring.head + 1) % KEYBOARD_CHANNEL_CAPACITY;
        ring.len -= 1;
        KeyInput::WIRE_LEN
    }
}

/// Map a typed seat refusal onto its stable ABI error code.
///
/// The one place [`SeatError`] meets [`Errno`], so the syscall handlers and
/// the owner-gated drain can never diverge. `SeatUnowned` (a `seat_revoke`
/// of a seat nobody holds) maps to the same "you do not hold it" refusal a
/// non-owner sees: there is no lease to revoke, and the mapping is total so
/// no call site can hit an unmapped variant.
#[must_use]
pub fn seat_errno(err: SeatError) -> Errno {
    match err {
        SeatError::SeatBusy => Errno::SeatBusy,
        SeatError::AlreadyOwner => Errno::AlreadyExists,
        SeatError::NotOwner | SeatError::SeatUnowned => Errno::SeatNotOwner,
        SeatError::SeatRevoked => Errno::SeatRevoked,
    }
}

/// The kernel seat registry: one seat, one keyboard stream, one owner-tracked
/// foreground sink.
///
/// Hosts the seat's [`SeatState`] (lease + foreground console) under the
/// registry's own lock, the text console's injectable input queue (the
/// seat's text sink), and the desktop keyboard channel. The boot path
/// installs one per running kernel and points the text sink at the console
/// that owns the directly attached keyboard; a platform with no injectable
/// text console points it at [`NULL_CONSOLE_INPUT`], which fails closed.
/// A machine with several displays becomes several seats in a later stage
/// (`plans/DISPLAY.md` D6); today the registry hosts the one seat every
/// Tier-1 image has (a text-only seat when no display node was discovered).
pub struct SeatRegistry {
    /// The seat's owner/lease/routing state machine — the one shared
    /// definition (`lib/seat`), never re-derived here.
    seat: SpinLock<SeatState>,
    /// The text console's injectable input queue — the seat's text sink.
    text_sink: &'static (dyn ConsoleInput + 'static),
    /// The desktop keyboard channel — the seat's desktop sink.
    channel: KeyboardChannel,
    /// One-shot latch: `false` until the first key edge is delivered to
    /// the seat, then `true` forever. It lets the `key_inject` syscall
    /// handler emit a single audit witness the first time a (typically
    /// autoloaded) keyboard driver delivers input — proof the input path
    /// is live — without logging one record per keystroke, which would
    /// leak typed secrets and their timing (no
    /// input-content/timing noise — secret hygiene).
    first_delivery: AtomicBool,
}

impl SeatRegistry {
    /// Build a registry whose seat's text sink is `text_sink` and whose seat
    /// starts unowned (a freshly booted system is a text login until a
    /// desktop acquires the seat). The foreground console is the primary
    /// console (index 0); retargeting it is the D5 handoff work.
    ///
    /// `const` so the boot path can place it in a `'static`.
    #[must_use]
    pub const fn new(text_sink: &'static (dyn ConsoleInput + 'static)) -> Self {
        Self {
            seat: SpinLock::new(SeatState::new(ConsoleIndex(0))),
            text_sink,
            channel: KeyboardChannel::new(),
            first_delivery: AtomicBool::new(false),
        }
    }

    /// Grant the seat to the kernel-attested `owner` (`display_acquire`),
    /// returning the minted [`Lease`]: subsequently injected key edges
    /// route to the keyboard channel, and the lease's generation is the
    /// handle the present path is later checked against
    /// ([`Self::present_gate`]).
    ///
    /// # Errors
    ///
    /// - [`SeatError::SeatBusy`] — another task holds the seat; ownership
    ///   is never displaced.
    /// - [`SeatError::AlreadyOwner`] — `owner` already holds it; a double
    ///   acquire is a caller bug, surfaced rather than silently succeeding.
    pub fn acquire(&self, owner: SeatOwner) -> Result<Lease, SeatError> {
        self.seat.lock().acquire(owner)
    }

    /// Release the seat held by `owner` (`display_release`), returning
    /// input to the text foreground.
    ///
    /// # Errors
    ///
    /// - [`SeatError::NotOwner`] — `owner` does not hold the seat; a
    ///   release is owner-checked, never a global "flip it back" switch.
    /// - [`SeatError::SeatRevoked`] — `owner`'s lease was revoked; the
    ///   refusal acknowledges the pending revocation.
    pub fn release(&self, owner: SeatOwner) -> Result<(), SeatError> {
        self.seat.lock().release(owner)
    }

    /// Route one decoded key edge to the seat's current foreground sink,
    /// returning the number of bytes consumed from the record
    /// ([`KeyInput::WIRE_LEN`]).
    ///
    /// A **held** seat routes the whole record to the keyboard channel. An
    /// **unowned** seat encodes a key *press* to console bytes and enqueues
    /// them on the text sink — a release, a modifier, or a key with no
    /// terminal encoding produces no bytes (`Ok(0)` from the encoder) and
    /// nothing is enqueued. A short push to a bounded sink is best-effort
    /// and does not change the consumed count, but a text sink that accepts
    /// *no* injected input (a console with no keyboard) fails closed and the
    /// error is surfaced to the driver.
    ///
    /// # Errors
    ///
    /// Returns the text sink's [`Errno`] (for example [`Errno::NotImplemented`]
    /// for a console with no injectable input queue) when a press would be
    /// enqueued there but the sink refuses it.
    pub fn inject(&self, record: KeyInput) -> Result<usize, Errno> {
        let route = self.seat.lock().route();
        match route {
            Route::Desktop(_) => {
                let bytes = record.to_le_bytes();
                self.channel.push(&bytes);
            }
            Route::Text(_) => {
                let mut out = [0u8; MAX_KEY_BYTES];
                // The shared map; an over-long sequence cannot occur for a
                // `MAX_KEY_BYTES` buffer, so a `BufferTooSmall` here would be
                // a map bug, surfaced rather than hidden.
                let n = encode_key_input(&record, &mut out).map_err(|_| Errno::BufferTooSmall)?;
                if n > 0 {
                    // A short push (the bounded type-ahead queue is near
                    // full) is best-effort; a sink that accepts no input
                    // fails closed.
                    self.text_sink.push(&out[..n])?;
                }
            }
        }
        Ok(KeyInput::WIRE_LEN)
    }

    /// Record that a key edge has been delivered to the seat and report
    /// whether this was the **first** delivery since boot.
    ///
    /// Returns `true` exactly once over the registry's lifetime — on the
    /// first call — and `false` on every later call, through a one-shot
    /// compare-and-set on the `first_delivery` latch. The `key_inject`
    /// handler calls this after a successful [`Self::inject`] and emits a
    /// single audit witness ([`crate::audit::AuditEvent::InputDelivered`])
    /// on the `true`, so the log records that an (autoloaded) input driver
    /// is live without a per-keystroke record (no
    /// input-content/timing noise — secret hygiene). It carries no
    /// key content; only the fact of first delivery.
    #[must_use]
    pub fn note_first_delivery(&self) -> bool {
        self.first_delivery
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Drain one decoded key event from the keyboard channel into `out` for
    /// the kernel-attested `owner`, returning the bytes written — one
    /// [`KeyInput`] record, or `0` when the channel is drained
    /// (`keyboard_read`).
    ///
    /// The drain is owner-gated through the seat's live lease
    /// ([`SeatState::access`]): only the task that acquired the seat may
    /// take records off the desktop channel, so a second holder of
    /// `CAP_INPUT_READ` can never siphon another session's keystrokes.
    ///
    /// # Errors
    ///
    /// - [`Errno::BufferTooSmall`] — `out` cannot hold a whole record
    ///   ([`KeyInput::WIRE_LEN`] bytes); the kernel never writes a partial
    ///   record.
    /// - [`Errno::SeatNotOwner`] — `owner` does not hold the seat.
    /// - [`Errno::SeatRevoked`] — `owner`'s lease was revoked.
    pub fn read_key(&self, owner: SeatOwner, out: &mut [u8]) -> Result<usize, Errno> {
        if out.len() < KeyInput::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        self.seat.lock().access(owner).map_err(seat_errno)?;
        Ok(self.channel.drain_one(out))
    }

    /// The task currently holding the seat, if any (test/introspection aid;
    /// the routing itself always consults the live lease).
    #[must_use]
    pub fn owner(&self) -> Option<SeatOwner> {
        self.seat.lock().owner()
    }

    /// Retarget the seat's foreground text console (`seat_switch`,
    /// `plans/DISPLAY.md` D3).
    ///
    /// Takes effect immediately for an unowned seat; a held seat keeps
    /// routing to its owner until the lease ends. The syscall handler
    /// validates the console index against the installed console list and
    /// checks `CAP_SEAT_ADMIN` *before* calling this.
    pub fn switch_foreground(&self, console: ConsoleIndex) {
        self.seat.lock().set_foreground_console(console);
    }

    /// Forcibly revoke the current lease (`seat_revoke`, `plans/DISPLAY.md`
    /// D3), returning the evicted owner for the audit record.
    ///
    /// The seat becomes acquirable immediately and input returns to the
    /// text foreground; the evicted owner's next owner-gated call is
    /// refused with [`SeatError::SeatRevoked`], so the loss is observable.
    ///
    /// # Errors
    ///
    /// - [`SeatError::SeatUnowned`] — no lease is held, so there is nothing
    ///   to revoke.
    pub fn revoke(&self) -> Result<SeatOwner, SeatError> {
        self.seat.lock().revoke()
    }

    /// The live seat-lease gate for the client holding `lease` — the one
    /// place the present right is derived from the seat registry
    /// (`plans/DISPLAY.md` D4). The returned gate is handed to a display
    /// driver's host as its `DriverHost::seat_gate`; the driver consults it
    /// at the top of every present/flip, so a revoked client cannot scan
    /// out even though its framebuffer mapping still exists.
    #[must_use]
    pub const fn present_gate(&self, lease: SeatLease) -> PresentGate<'_> {
        PresentGate {
            registry: self,
            lease,
        }
    }

    /// One wire-encodable snapshot of the seat for the seat inventory
    /// (`IntrospectDomain::Seats`), taken under the registry lock so the
    /// owner, generation, and foreground are one consistent observation.
    #[must_use]
    pub fn record(&self, seat_id: u64) -> SeatRecord {
        let seat = self.seat.lock();
        let (owner_task, flags) = match seat.owner() {
            Some(SeatOwner(task)) => (task, SEAT_FLAG_OWNED),
            None => (0, 0),
        };
        SeatRecord {
            seat_id,
            owner_task,
            generation: seat.generation(),
            foreground_console: seat.foreground_console().0,
            flags,
        }
    }
}

/// A [`SeatGate`] bound to one client's [`SeatLease`] over the kernel seat
/// registry: the present-path check a display driver's host exposes
/// (`plans/DISPLAY.md` D4).
///
/// Every call re-reads the registry's live lease under its lock — the gate
/// caches nothing — so a `seat_revoke` between two frames refuses the very
/// next present. The bound handle carries the mint-time generation, which
/// is what makes a stale pre-revoke handle refusable even after its owner
/// reacquired the seat ([`rustos_seat::SeatState::verify`], the one
/// definition of the check).
pub struct PresentGate<'r> {
    registry: &'r SeatRegistry,
    lease: SeatLease,
}

impl SeatGate for PresentGate<'_> {
    fn check_present(&self) -> Result<(), DriverError> {
        // The registry hosts the primary seat; a handle naming any other
        // seat cannot be live here (fail closed, never guess).
        if self.lease.seat_id != SEAT_PRIMARY {
            return Err(DriverError::PermissionDenied);
        }
        let lease = Lease {
            owner: SeatOwner(self.lease.owner_task),
            generation: self.lease.generation,
        };
        self.registry
            .seat
            .lock()
            .verify(lease)
            .map_err(|err| match err {
                SeatError::SeatRevoked => DriverError::SeatRevoked,
                SeatError::SeatBusy
                | SeatError::AlreadyOwner
                | SeatError::NotOwner
                | SeatError::SeatUnowned => DriverError::PermissionDenied,
            })
    }
}

/// The shared fail-closed registry a kernel build with no seat wiring
/// holds: its text sink is [`NULL_CONSOLE_INPUT`], so a `key_inject` on the
/// unowned seat fails closed with [`Errno::NotImplemented`] and a
/// `keyboard_read` denies for want of ownership (never fabricate a
/// destination).
pub static NULL_SEAT_REGISTRY: SeatRegistry = SeatRegistry::new(&NULL_CONSOLE_INPUT);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::{ConsoleInputQueue, ConsoleRead};
    use alloc::boxed::Box;
    use rustos_abi::input::{KeyValue, Modifiers, NamedKeyCode};

    const WM: SeatOwner = SeatOwner(7);
    const INTRUDER: SeatOwner = SeatOwner(9);

    fn press_char(c: char) -> KeyInput {
        KeyInput::Pressed {
            key: KeyValue::Char(c),
            modifiers: Modifiers::default(),
        }
    }

    fn text_queue() -> &'static ConsoleInputQueue {
        Box::leak(Box::new(ConsoleInputQueue::new()))
    }

    #[test]
    fn a_fresh_seat_is_unowned() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        assert_eq!(seat.owner(), None);
    }

    #[test]
    fn an_unowned_seat_encodes_a_press_to_the_text_sink() {
        // A leaked queue stands in for the video console's input queue.
        let queue = text_queue();
        let seat = SeatRegistry::new(queue);
        assert_eq!(seat.inject(press_char('a')), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; 8];
        let n = queue.read(&mut buf).expect("queue read");
        assert_eq!(&buf[..n], b"a");
    }

    #[test]
    fn an_unowned_seat_drops_releases_and_modifiers() {
        let queue = text_queue();
        let seat = SeatRegistry::new(queue);
        let release = KeyInput::Released {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        };
        assert_eq!(seat.inject(release), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; 8];
        // A release produces no tty bytes, so nothing reached the text sink.
        assert_eq!(queue.read(&mut buf).expect("queue read"), 0);
    }

    #[test]
    fn an_unowned_seat_with_no_injectable_sink_fails_closed() {
        // The NULL sink accepts no injected input: a press that would be
        // enqueued there surfaces `NotImplemented` rather than dropping it.
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        assert_eq!(seat.inject(press_char('a')), Err(Errno::NotImplemented));
    }

    #[test]
    fn a_held_seat_routes_records_to_the_owner_drain() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let lease = seat.acquire(WM).expect("fresh seat is acquirable");
        assert_eq!(lease.owner, WM);
        assert_eq!(lease.generation, 1);
        assert_eq!(seat.owner(), Some(WM));
        let record = KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::Enter),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        };
        assert_eq!(seat.inject(record), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        let n = seat.read_key(WM, &mut buf).expect("owner drains");
        assert_eq!(n, KeyInput::WIRE_LEN);
        assert_eq!(KeyInput::from_bytes(&buf), Ok(record));
        // Drained: the channel is now empty.
        assert_eq!(seat.read_key(WM, &mut buf), Ok(0));
    }

    #[test]
    fn a_non_owner_cannot_drain_the_desktop_channel() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(WM).expect("fresh seat is acquirable");
        assert_eq!(seat.inject(press_char('s')), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        // Neither another task nor a reader of an unowned channel may
        // siphon the owner's keystrokes; the record stays queued.
        assert_eq!(seat.read_key(INTRUDER, &mut buf), Err(Errno::SeatNotOwner));
        assert_eq!(
            seat.read_key(WM, &mut buf).expect("owner drains"),
            KeyInput::WIRE_LEN
        );
    }

    #[test]
    fn reading_an_unowned_seat_is_refused() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        assert_eq!(seat.read_key(WM, &mut buf), Err(Errno::SeatNotOwner));
    }

    #[test]
    fn a_second_task_cannot_steal_a_held_seat() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(WM).expect("fresh seat is acquirable");
        assert_eq!(seat.acquire(INTRUDER), Err(SeatError::SeatBusy));
        assert_eq!(seat.owner(), Some(WM));
    }

    #[test]
    fn a_non_owner_cannot_release_a_held_seat() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(WM).expect("fresh seat is acquirable");
        assert_eq!(seat.release(INTRUDER), Err(SeatError::NotOwner));
        assert_eq!(seat.owner(), Some(WM));
    }

    #[test]
    fn release_returns_input_to_the_text_sink() {
        let queue = text_queue();
        let seat = SeatRegistry::new(queue);
        seat.acquire(WM).expect("fresh seat is acquirable");
        // A press routed to the desktop channel while held.
        assert_eq!(seat.inject(press_char('x')), Ok(KeyInput::WIRE_LEN));
        assert_eq!(seat.release(WM), Ok(()));
        assert_eq!(seat.owner(), None);
        // Now the press routes to the text sink instead.
        assert_eq!(seat.inject(press_char('y')), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; 8];
        let n = queue.read(&mut buf).expect("queue read");
        assert_eq!(&buf[..n], b"y");
    }

    #[test]
    fn first_delivery_latch_fires_exactly_once() {
        // The one-shot witness latch returns `true` on the first call and
        // `false` forever after, regardless of routing or ownership — so the
        // `key_inject` handler emits a single audit witness and never one
        // per keystroke.
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        assert!(seat.note_first_delivery());
        assert!(!seat.note_first_delivery());
        assert!(!seat.note_first_delivery());
    }

    #[test]
    fn read_key_rejects_a_short_buffer() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(WM).expect("fresh seat is acquirable");
        let mut buf = [0u8; KeyInput::WIRE_LEN - 1];
        assert_eq!(seat.read_key(WM, &mut buf), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn channel_drops_the_oldest_record_on_overflow() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(WM).expect("fresh seat is acquirable");
        // Fill the ring plus one: the first record is dropped.
        for i in 0..=KEYBOARD_CHANNEL_CAPACITY {
            let c = char::from(b'a' + u8::try_from(i % 26).unwrap());
            assert_eq!(seat.inject(press_char(c)), Ok(KeyInput::WIRE_LEN));
        }
        // The channel holds exactly CAPACITY records; the very first ('a')
        // was evicted, so the oldest surviving record is the second pushed.
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        let n = seat.read_key(WM, &mut buf).expect("owner drains");
        assert_eq!(n, KeyInput::WIRE_LEN);
        let first = KeyInput::from_bytes(&buf).expect("valid record");
        assert_eq!(first, press_char('b'));
    }

    #[test]
    fn revoke_evicts_the_owner_and_returns_input_to_text() {
        let queue = text_queue();
        let seat = SeatRegistry::new(queue);
        seat.acquire(WM).expect("fresh seat is acquirable");
        assert_eq!(seat.revoke(), Ok(WM));
        assert_eq!(seat.owner(), None);
        // The evicted owner's next drain observes the distinct refusal, and
        // only once; afterwards it is a plain non-owner.
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        assert_eq!(seat.read_key(WM, &mut buf), Err(Errno::SeatRevoked));
        // Input routes to the text foreground, never a stale desktop channel.
        assert_eq!(seat.inject(press_char('z')), Ok(KeyInput::WIRE_LEN));
        let mut text = [0u8; 8];
        let n = queue.read(&mut text).expect("queue read");
        assert_eq!(&text[..n], b"z");
    }

    #[test]
    fn revoking_an_unowned_seat_is_refused() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        assert_eq!(seat.revoke(), Err(SeatError::SeatUnowned));
    }

    #[test]
    fn switch_foreground_retargets_the_text_sink_route() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.switch_foreground(ConsoleIndex(2));
        assert_eq!(seat.record(0).foreground_console, 2);
    }

    #[test]
    fn record_reports_the_live_lease_and_generation() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let fresh = seat.record(0);
        assert_eq!(fresh.seat_id, 0);
        assert!(!fresh.owned());
        assert_eq!(fresh.owner(), None);
        assert_eq!(fresh.generation, 0);

        seat.acquire(WM).expect("fresh seat is acquirable");
        let held = seat.record(0);
        assert!(held.owned());
        assert_eq!(held.owner(), Some(WM.0));
        assert_eq!(held.generation, 1);

        seat.revoke().expect("held seat revokes");
        let revoked = seat.record(0);
        assert!(!revoked.owned());
        assert_eq!(revoked.owner_task, 0);
        assert_eq!(revoked.generation, 1);
    }

    /// The abi-facing lease handle for `owner` under `generation` on the
    /// primary seat.
    fn handle(owner: SeatOwner, generation: u64) -> SeatLease {
        SeatLease {
            seat_id: SEAT_PRIMARY,
            owner_task: owner.0,
            generation,
        }
    }

    #[test]
    fn present_gate_admits_only_the_live_lease() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let lease = seat.acquire(WM).expect("fresh seat is acquirable");
        assert_eq!(
            seat.present_gate(handle(WM, lease.generation))
                .check_present(),
            Ok(())
        );
        // A handle naming another task, a stale generation, or a foreign
        // seat id is refused before any scanout.
        assert_eq!(
            seat.present_gate(handle(INTRUDER, lease.generation))
                .check_present(),
            Err(DriverError::PermissionDenied)
        );
        assert_eq!(
            seat.present_gate(handle(WM, lease.generation + 1))
                .check_present(),
            Err(DriverError::PermissionDenied)
        );
        let mut foreign = handle(WM, lease.generation);
        foreign.seat_id = 7;
        assert_eq!(
            seat.present_gate(foreign).check_present(),
            Err(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn present_gate_refuses_a_revoked_lease_distinctly() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let lease = seat.acquire(WM).expect("fresh seat is acquirable");
        let gate_handle = handle(WM, lease.generation);
        seat.revoke().expect("held seat revokes");
        // The gate re-reads the live lease on every call: the very next
        // present after the revoke is refused, and the evicted client sees
        // the distinct refusal so it learns it lost the seat.
        assert_eq!(
            seat.present_gate(gate_handle).check_present(),
            Err(DriverError::SeatRevoked)
        );
        // The new foreground's fresh lease presents; the stale pre-revoke
        // handle stays dead even though its owner may reacquire later.
        let fresh = seat.acquire(INTRUDER).expect("revoked seat is acquirable");
        assert_eq!(
            seat.present_gate(handle(INTRUDER, fresh.generation))
                .check_present(),
            Ok(())
        );
        assert_eq!(
            seat.present_gate(gate_handle).check_present(),
            Err(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn seat_errno_maps_every_refusal_onto_its_abi_code() {
        assert_eq!(seat_errno(SeatError::SeatBusy), Errno::SeatBusy);
        assert_eq!(seat_errno(SeatError::AlreadyOwner), Errno::AlreadyExists);
        assert_eq!(seat_errno(SeatError::NotOwner), Errno::SeatNotOwner);
        assert_eq!(seat_errno(SeatError::SeatUnowned), Errno::SeatNotOwner);
        assert_eq!(seat_errno(SeatError::SeatRevoked), Errno::SeatRevoked);
    }
}
