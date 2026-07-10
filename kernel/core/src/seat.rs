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
//!   drains it with `keyboard_read`. Pointer events take the same shape
//!   through the seat's pointer channel (`pointer_inject` → `pointer_read`);
//!   while the seat is unowned they are consumed and discarded, because the
//!   text console has no pointer consumer.
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

use core::ops::Deref;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::sync::Arc;
use alloc::vec::Vec;

use rustos_abi::driver::display::SeatGate;
use rustos_abi::input::{KeyInput, PointerInput};
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

/// Capacity, in [`PointerInput`] records, of the desktop pointer channel's
/// ring.
///
/// The same **fixed bound** rationale as [`KEYBOARD_CHANNEL_CAPACITY`], sized
/// larger because a pointing device emits far more events than a keyboard —
/// a drag produces a motion record per hardware report, hundreds per second —
/// so the ring must absorb a realistic burst between the compositor's
/// per-frame drains. Overflow still drops the *oldest* record: a stale
/// motion is worthless once a fresher one exists, and the producer never
/// blocks and never grows kernel memory.
pub const POINTER_CHANNEL_CAPACITY: usize = 256;

/// The fixed-capacity record ring behind a desktop input channel:
/// `CAP` records of `REC` bytes each.
struct ChannelRing<const CAP: usize, const REC: usize> {
    buf: [[u8; REC]; CAP],
    /// Index of the next record to drain.
    head: usize,
    /// Number of records currently queued.
    len: usize,
}

impl<const CAP: usize, const REC: usize> ChannelRing<CAP, REC> {
    const fn new() -> Self {
        Self {
            buf: [[0u8; REC]; CAP],
            head: 0,
            len: 0,
        }
    }
}

impl<const CAP: usize, const REC: usize> Drop for ChannelRing<CAP, REC> {
    fn drop(&mut self) {
        // A destroyed seat's ring may still hold undrained records (a typed
        // character — possibly a password keystroke — transits the keyboard
        // ring), so the whole backing store is wiped before the memory is
        // freed: zero-on-free for memory that held a credential. The
        // pointer ring shares the one ring definition, so it inherits the
        // wipe at no extra code.
        self.buf.zeroize();
    }
}

/// A bounded, lock-protected channel of fixed-width input records the seat
/// routes to the desktop while it is held, drained one record at a time by
/// the seat owner (`keyboard_read` / `pointer_read`).
///
/// The one ring definition behind both desktop input channels; only the
/// capacity and record width differ. Each drained record is **zeroed in
/// place** as it leaves the ring: a key event can carry a typed character
/// (a password keystroke transits the keyboard channel between the driver
/// and the desktop), so the buffer must not retain it after the consumer
/// has taken it (zero-on-free for memory that held a credential — secret
/// hygiene).
struct InputChannel<const CAP: usize, const REC: usize> {
    ring: SpinLock<ChannelRing<CAP, REC>>,
}

/// The desktop keyboard channel: [`KeyInput`] records, drained by
/// `keyboard_read`.
type KeyboardChannel = InputChannel<KEYBOARD_CHANNEL_CAPACITY, { KeyInput::WIRE_LEN }>;

/// The desktop pointer channel: [`PointerInput`] records, drained by
/// `pointer_read`.
type PointerChannel = InputChannel<POINTER_CHANNEL_CAPACITY, { PointerInput::WIRE_LEN }>;

impl<const CAP: usize, const REC: usize> InputChannel<CAP, REC> {
    const fn new() -> Self {
        Self {
            ring: SpinLock::new(ChannelRing::new()),
        }
    }

    /// Enqueue one record, dropping the oldest if the ring is full (the
    /// producer never blocks).
    fn push(&self, record: &[u8; REC]) {
        let mut ring = self.ring.lock();
        if ring.len == CAP {
            // Drop the oldest record to make room — a stale record is
            // preferable to unbounded growth or refusing the live one.
            let head = ring.head;
            ring.buf[head].zeroize();
            ring.head = (head + 1) % CAP;
            ring.len -= 1;
        }
        let idx = (ring.head + ring.len) % CAP;
        ring.buf[idx] = *record;
        ring.len += 1;
    }

    /// Drain one record into `out`, zeroing the drained slot, and return the
    /// number of bytes written (`REC`, or `0` when empty).
    ///
    /// `out` is assumed to be at least `REC` bytes (the caller checks the
    /// bound first).
    fn drain_one(&self, out: &mut [u8]) -> usize {
        let mut ring = self.ring.lock();
        if ring.len == 0 {
            return 0;
        }
        let idx = ring.head;
        out[..REC].copy_from_slice(&ring.buf[idx]);
        ring.buf[idx].zeroize();
        ring.head = (ring.head + 1) % CAP;
        ring.len -= 1;
        REC
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

/// One seat's kernel-side backing: the shared [`SeatState`] owner/lease
/// state machine under its own lock, the seat's text sink, and its desktop
/// keyboard and pointer channels.
///
/// Each seat's state and channel are independent — one seat's input, owner,
/// and revocations never touch another's — and each slot carries its own
/// lock so operating one seat never serialises the others.
struct SeatSlot {
    /// The seat's owner/lease/routing state machine — the one shared
    /// definition (`lib/seat`), never re-derived here.
    state: SpinLock<SeatState>,
    /// The text console's injectable input queue — the seat's text sink.
    /// A seat with no attached text console (a hotplugged display) uses
    /// [`NULL_CONSOLE_INPUT`], which fails closed.
    text_sink: &'static (dyn ConsoleInput + 'static),
    /// The desktop keyboard channel — the seat's desktop keyboard sink.
    channel: KeyboardChannel,
    /// The desktop pointer channel — the seat's desktop pointer sink.
    pointer: PointerChannel,
}

impl SeatSlot {
    const fn new(text_sink: &'static (dyn ConsoleInput + 'static)) -> Self {
        Self {
            state: SpinLock::new(SeatState::new(ConsoleIndex(0))),
            text_sink,
            channel: KeyboardChannel::new(),
            pointer: PointerChannel::new(),
        }
    }

    /// One wire-encodable snapshot of this seat, taken under its state lock
    /// so the owner, generation, and foreground are one consistent
    /// observation.
    fn record(&self, seat_id: u64) -> SeatRecord {
        let state = self.state.lock();
        let (owner_task, flags) = match state.owner() {
            Some(SeatOwner(task)) => (task, SEAT_FLAG_OWNED),
            None => (0, 0),
        };
        SeatRecord {
            seat_id,
            owner_task,
            generation: state.generation(),
            foreground_console: state.foreground_console().0,
            flags,
        }
    }
}

/// One discovery-created seat: the display node it was minted for and its
/// shared backing. The `Arc` lets an operation resolve the slot under the
/// table lock and then run against the seat without holding that lock, so
/// a concurrent destroy is safe (the in-flight operation completes against
/// the still-alive slot; every later resolve fails closed `NotFound`).
struct HotplugSeat {
    seat_id: u64,
    node_id: u32,
    slot: Arc<SeatSlot>,
}

/// A resolved seat: either the inline boot seat or a shared hotplug slot.
enum SlotRef<'r> {
    Primary(&'r SeatSlot),
    Hotplug(Arc<SeatSlot>),
}

impl Deref for SlotRef<'_> {
    type Target = SeatSlot;

    fn deref(&self) -> &SeatSlot {
        match self {
            Self::Primary(slot) => slot,
            Self::Hotplug(slot) => slot,
        }
    }
}

/// The kernel seat registry: every seat on the machine, each with its own
/// owner, lease, foreground console, and input routing (`plans/DISPLAY.md`
/// D6).
///
/// Seat [`SEAT_PRIMARY`] (id 0) is the **boot seat**: it always exists —
/// even on a headless build, where it is a text-only seat — and its text
/// sink is the console that owns the directly attached keyboard (a platform
/// with no injectable text console points it at [`NULL_CONSOLE_INPUT`],
/// which fails closed). Every further seat is minted by hardware discovery:
/// a display-class node published into the live hardware tree creates one
/// ([`Self::attach_display`], driven by the `hw_emit_node` handler) and the
/// node's removal destroys it again ([`Self::detach_display`], driven by
/// `hw_remove_node`) — hotplug needs no reboot. Seat ids are minted
/// monotonically and **never reused**, so a stale lease or record can never
/// alias a later seat.
pub struct SeatRegistry {
    /// The boot seat (id [`SEAT_PRIMARY`]), inline so the registry is
    /// `const`-constructible for the boot path's `'static`.
    primary: SeatSlot,
    /// The discovery-created seats, in creation order.
    hotplug: SpinLock<Vec<HotplugSeat>>,
    /// The next seat id to mint; starts past [`SEAT_PRIMARY`] and only
    /// ever increases.
    next_seat_id: AtomicU64,
    /// One-shot latches, one per input kind: `false` until the first key
    /// edge (respectively pointer record) is delivered to any seat, then
    /// `true` forever. They let the `key_inject` / `pointer_inject`
    /// syscall handlers emit one audit witness per input kind the first
    /// time a (typically autoloaded) driver of that kind delivers input —
    /// proof each input path is live and attributable per kind — without
    /// logging one record per event, which would leak typed secrets and
    /// their timing (no input-content/timing noise — secret hygiene).
    first_key_delivery: AtomicBool,
    first_pointer_delivery: AtomicBool,
}

/// The input kind a first-delivery witness attributes
/// ([`SeatRegistry::note_first_delivery`]): which class of input driver
/// proved itself live.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DeliveredInputKind {
    /// A keyboard key edge (`key_inject`).
    Key,
    /// A pointer motion or button record (`pointer_inject`).
    Pointer,
}

impl DeliveredInputKind {
    /// The stable `kind` field value the audit witness carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Pointer => "pointer",
        }
    }
}

impl SeatRegistry {
    /// Build a registry whose boot seat's text sink is `text_sink` and
    /// whose seats all start unowned (a freshly booted system is a text
    /// login until a desktop acquires a seat). Discovery-created seats are
    /// added later through [`Self::attach_display`].
    ///
    /// `const` so the boot path can place it in a `'static`.
    #[must_use]
    pub const fn new(text_sink: &'static (dyn ConsoleInput + 'static)) -> Self {
        Self {
            primary: SeatSlot::new(text_sink),
            hotplug: SpinLock::new(Vec::new()),
            next_seat_id: AtomicU64::new(SEAT_PRIMARY + 1),
            first_key_delivery: AtomicBool::new(false),
            first_pointer_delivery: AtomicBool::new(false),
        }
    }

    /// Resolve `seat_id` to its live slot.
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id (it never existed,
    ///   or its display node was hot-removed); fail closed, never guess.
    fn resolve(&self, seat_id: u64) -> Result<SlotRef<'_>, Errno> {
        if seat_id == SEAT_PRIMARY {
            return Ok(SlotRef::Primary(&self.primary));
        }
        self.hotplug
            .lock()
            .iter()
            .find(|seat| seat.seat_id == seat_id)
            .map(|seat| SlotRef::Hotplug(Arc::clone(&seat.slot)))
            .ok_or(Errno::NotFound)
    }

    /// Grant seat `seat_id` to the kernel-attested `owner`
    /// (`display_acquire`), returning the minted [`Lease`]: subsequently
    /// injected key edges route to that seat's keyboard channel, and the
    /// lease's generation is the handle the present path is later checked
    /// against ([`Self::present_gate`]).
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id.
    /// - [`Errno::SeatBusy`] — another task holds the seat; ownership
    ///   is never displaced.
    /// - [`Errno::AlreadyExists`] — `owner` already holds it; a double
    ///   acquire is a caller bug, surfaced rather than silently succeeding.
    pub fn acquire(&self, seat_id: u64, owner: SeatOwner) -> Result<Lease, Errno> {
        let slot = self.resolve(seat_id)?;
        let lease = slot.state.lock().acquire(owner).map_err(seat_errno)?;
        Ok(lease)
    }

    /// Release seat `seat_id` held by `owner` (`display_release`),
    /// returning its input to the text foreground.
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id.
    /// - [`Errno::SeatNotOwner`] — `owner` does not hold the seat; a
    ///   release is owner-checked, never a global "flip it back" switch.
    /// - [`Errno::SeatRevoked`] — `owner`'s lease was revoked; the
    ///   refusal acknowledges the pending revocation.
    pub fn release(&self, seat_id: u64, owner: SeatOwner) -> Result<(), Errno> {
        let slot = self.resolve(seat_id)?;
        let outcome = slot.state.lock().release(owner);
        outcome.map_err(seat_errno)
    }

    /// Route one decoded key edge to seat `seat_id`'s current foreground
    /// sink, returning the number of bytes consumed from the record
    /// ([`KeyInput::WIRE_LEN`]).
    ///
    /// A **held** seat routes the whole record to its keyboard channel. An
    /// **unowned** seat encodes a key *press* to console bytes and enqueues
    /// them on its text sink — a release, a modifier, or a key with no
    /// terminal encoding produces no bytes (`Ok(0)` from the encoder) and
    /// nothing is enqueued. A short push to a bounded sink is best-effort
    /// and does not change the consumed count, but a text sink that accepts
    /// *no* injected input (a console with no keyboard, or a hotplugged
    /// display seat with no text console) fails closed and the error is
    /// surfaced to the driver.
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id.
    /// - The text sink's [`Errno`] (for example [`Errno::NotImplemented`]
    ///   for a console with no injectable input queue) when a press would
    ///   be enqueued there but the sink refuses it.
    pub fn inject(&self, seat_id: u64, record: KeyInput) -> Result<usize, Errno> {
        let slot = self.resolve(seat_id)?;
        let route = slot.state.lock().route();
        match route {
            Route::Desktop(_) => {
                let bytes = record.to_le_bytes();
                slot.channel.push(&bytes);
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
                    slot.text_sink.push(&out[..n])?;
                }
            }
        }
        Ok(KeyInput::WIRE_LEN)
    }

    /// Route one decoded pointer event to seat `seat_id`'s current
    /// foreground sink, returning the number of bytes consumed from the
    /// record ([`PointerInput::WIRE_LEN`]).
    ///
    /// A **held** seat routes the whole record to its pointer channel. An
    /// **unowned** seat consumes and discards the record: the text console
    /// has no pointer consumer, and dropping at the arbiter keeps the
    /// routing policy out of the device driver exactly as for key edges —
    /// the driver never learns (and never needs to learn) who holds the
    /// seat.
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id.
    pub fn inject_pointer(&self, seat_id: u64, record: PointerInput) -> Result<usize, Errno> {
        let slot = self.resolve(seat_id)?;
        let route = slot.state.lock().route();
        if let Route::Desktop(_) = route {
            let bytes = record.to_le_bytes();
            slot.pointer.push(&bytes);
        }
        Ok(PointerInput::WIRE_LEN)
    }

    /// Record that an input record of `kind` has been delivered to the
    /// seat and report whether this was the **first** delivery of that
    /// kind since boot.
    ///
    /// Returns `true` exactly once per input kind over the registry's
    /// lifetime — on the kind's first call — and `false` on every later
    /// call, through a one-shot compare-and-set on that kind's latch. The
    /// `key_inject` / `pointer_inject` handlers call this after a
    /// successful [`Self::inject`] / [`Self::inject_pointer`] and emit one
    /// audit witness ([`crate::audit::AuditEvent::InputDelivered`], with a
    /// `kind` field) per `true`, so the log records that an (autoloaded)
    /// driver of each input class is live — at most two records over the
    /// kernel's lifetime, never one per event (no input-content/timing
    /// noise — secret hygiene). It carries no event content; only the fact
    /// of the kind's first delivery.
    #[must_use]
    pub fn note_first_delivery(&self, kind: DeliveredInputKind) -> bool {
        let latch = match kind {
            DeliveredInputKind::Key => &self.first_key_delivery,
            DeliveredInputKind::Pointer => &self.first_pointer_delivery,
        };
        latch
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Drain one decoded key event from seat `seat_id`'s keyboard channel
    /// into `out` for the kernel-attested `owner`, returning the bytes
    /// written — one [`KeyInput`] record, or `0` when the channel is
    /// drained (`keyboard_read`).
    ///
    /// The drain is owner-gated through the seat's live lease
    /// ([`SeatState::access`]): only the task that acquired the seat may
    /// take records off its desktop channel, so a second holder of
    /// `CAP_INPUT_READ` — even the owner of *another* seat — can never
    /// siphon this session's keystrokes.
    ///
    /// # Errors
    ///
    /// - [`Errno::BufferTooSmall`] — `out` cannot hold a whole record
    ///   ([`KeyInput::WIRE_LEN`] bytes); the kernel never writes a partial
    ///   record.
    /// - [`Errno::NotFound`] — no live seat has that id.
    /// - [`Errno::SeatNotOwner`] — `owner` does not hold the seat.
    /// - [`Errno::SeatRevoked`] — `owner`'s lease was revoked.
    pub fn read_key(&self, seat_id: u64, owner: SeatOwner, out: &mut [u8]) -> Result<usize, Errno> {
        if out.len() < KeyInput::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let slot = self.resolve(seat_id)?;
        slot.state.lock().access(owner).map_err(seat_errno)?;
        Ok(slot.channel.drain_one(out))
    }

    /// Drain one decoded pointer event from seat `seat_id`'s pointer
    /// channel into `out` for the kernel-attested `owner`, returning the
    /// bytes written — one [`PointerInput`] record, or `0` when the channel
    /// is drained (`pointer_read`).
    ///
    /// The drain is owner-gated through the seat's live lease
    /// ([`SeatState::access`]) exactly like [`Self::read_key`]: only the
    /// task that acquired the seat may take records off its pointer
    /// channel, so a second holder of `CAP_INPUT_READ` — even the owner of
    /// *another* seat — can never observe this session's pointer stream.
    ///
    /// # Errors
    ///
    /// - [`Errno::BufferTooSmall`] — `out` cannot hold a whole record
    ///   ([`PointerInput::WIRE_LEN`] bytes); the kernel never writes a
    ///   partial record.
    /// - [`Errno::NotFound`] — no live seat has that id.
    /// - [`Errno::SeatNotOwner`] — `owner` does not hold the seat.
    /// - [`Errno::SeatRevoked`] — `owner`'s lease was revoked.
    pub fn read_pointer(
        &self,
        seat_id: u64,
        owner: SeatOwner,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        if out.len() < PointerInput::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let slot = self.resolve(seat_id)?;
        slot.state.lock().access(owner).map_err(seat_errno)?;
        Ok(slot.pointer.drain_one(out))
    }

    /// The task currently holding seat `seat_id`, if any
    /// (test/introspection aid; the routing itself always consults the
    /// live lease). `None` for an unowned *or* unknown seat.
    #[must_use]
    pub fn owner(&self, seat_id: u64) -> Option<SeatOwner> {
        let slot = self.resolve(seat_id).ok()?;
        let owner = slot.state.lock().owner();
        owner
    }

    /// One wire-encodable snapshot of seat `seat_id` for the seat
    /// inventory, taken under the seat's state lock so the owner,
    /// generation, and foreground are one consistent observation, or
    /// `None` for a seat that does not (or no longer) exist.
    #[must_use]
    pub fn record(&self, seat_id: u64) -> Option<SeatRecord> {
        let slot = self.resolve(seat_id).ok()?;
        Some(slot.record(seat_id))
    }

    /// The wire-encoded seat-inventory page starting at record offset
    /// `first`, at most `max_records` whole [`SeatRecord`]s
    /// (`IntrospectDomain::Seats` paging: an offset past the end returns
    /// the empty terminator).
    ///
    /// The boot seat pages first, then the discovery-created seats in
    /// creation order.
    #[must_use]
    pub fn records(&self, first: u64, max_records: usize) -> Vec<u8> {
        let mut records = Vec::new();
        records.push(self.primary.record(SEAT_PRIMARY));
        {
            let hotplug = self.hotplug.lock();
            for seat in hotplug.iter() {
                records.push(seat.slot.record(seat.seat_id));
            }
        }
        let skip = usize::try_from(first).unwrap_or(usize::MAX);
        let mut out = Vec::new();
        for record in records.iter().skip(skip).take(max_records) {
            out.extend_from_slice(&record.to_le_bytes());
        }
        out
    }

    /// Create (or find) the seat for the display-class hardware-tree node
    /// `node_id`, returning its seat id (`plans/DISPLAY.md` D6).
    ///
    /// Driven by the `hw_emit_node` handler when a display-class node is
    /// published into the live tree: hotplugging a display creates its seat
    /// with no reboot. Idempotent — a node that already has a live seat
    /// keeps it (and its id) rather than minting a duplicate. The new seat
    /// starts unowned with no text console attached (its text sink fails
    /// closed until a desktop acquires it), and its id is minted from the
    /// never-reused counter.
    pub fn attach_display(&self, node_id: u32) -> u64 {
        let mut hotplug = self.hotplug.lock();
        if let Some(existing) = hotplug.iter().find(|seat| seat.node_id == node_id) {
            return existing.seat_id;
        }
        let seat_id = self.next_seat_id.fetch_add(1, Ordering::Relaxed);
        hotplug.push(HotplugSeat {
            seat_id,
            node_id,
            slot: Arc::new(SeatSlot::new(&NULL_CONSOLE_INPUT)),
        });
        seat_id
    }

    /// Destroy the seat created for the display-class node `node_id`,
    /// returning its seat id — or `None` when no seat was attached to that
    /// node (`plans/DISPLAY.md` D6).
    ///
    /// Driven by the `hw_remove_node` handler when the node leaves the live
    /// tree: unplugging a display destroys its seat with no reboot. Every
    /// subsequent operation naming the dead seat — an acquire, drain,
    /// present, switch, or revoke — fails closed `NotFound`, and the seat's
    /// keyboard channel is zeroed as it is freed (a ring that held typed
    /// characters never outlives its seat). The boot seat has no display
    /// node and can never be destroyed.
    pub fn detach_display(&self, node_id: u32) -> Option<u64> {
        let mut hotplug = self.hotplug.lock();
        let index = hotplug.iter().position(|seat| seat.node_id == node_id)?;
        let seat = hotplug.remove(index);
        Some(seat.seat_id)
    }

    /// Retarget seat `seat_id`'s foreground text console (`seat_switch`,
    /// `plans/DISPLAY.md` D3).
    ///
    /// Takes effect immediately for an unowned seat; a held seat keeps
    /// routing to its owner until the lease ends. The syscall handler
    /// validates the console index against the installed console list and
    /// checks `CAP_SEAT_ADMIN` *before* calling this.
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id.
    pub fn switch_foreground(&self, seat_id: u64, console: ConsoleIndex) -> Result<(), Errno> {
        let slot = self.resolve(seat_id)?;
        slot.state.lock().set_foreground_console(console);
        Ok(())
    }

    /// Forcibly revoke seat `seat_id`'s current lease (`seat_revoke`,
    /// `plans/DISPLAY.md` D3), returning the evicted owner for the audit
    /// record.
    ///
    /// The seat becomes acquirable immediately and its input returns to
    /// the text foreground; the evicted owner's next owner-gated call is
    /// refused with [`Errno::SeatRevoked`], so the loss is observable.
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id.
    /// - [`Errno::SeatNotOwner`] — no lease is held, so there is nothing
    ///   to revoke.
    pub fn revoke(&self, seat_id: u64) -> Result<SeatOwner, Errno> {
        let slot = self.resolve(seat_id)?;
        let outcome = slot.state.lock().revoke();
        outcome.map_err(seat_errno)
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
}

/// A [`SeatGate`] bound to one client's [`SeatLease`] over the kernel seat
/// registry: the present-path check a display driver's host exposes
/// (`plans/DISPLAY.md` D4).
///
/// Every call re-resolves the handle's seat and re-reads its live lease
/// under the seat's lock — the gate caches nothing — so a `seat_revoke`
/// between two frames refuses the very next present, and a hot-removed
/// seat's handle is refused the instant the seat is destroyed. The bound
/// handle carries the mint-time generation, which is what makes a stale
/// pre-revoke handle refusable even after its owner reacquired the seat
/// ([`rustos_seat::SeatState::verify`], the one definition of the check).
pub struct PresentGate<'r> {
    registry: &'r SeatRegistry,
    lease: SeatLease,
}

impl SeatGate for PresentGate<'_> {
    fn check_present(&self) -> Result<(), DriverError> {
        // Resolve the handle's seat against the live registry on every
        // call: a seat that does not (or no longer) exist — a hot-removed
        // display — refuses exactly like a dead lease (fail closed, never
        // guess).
        let Ok(slot) = self.registry.resolve(self.lease.seat_id) else {
            return Err(DriverError::PermissionDenied);
        };
        let lease = Lease {
            owner: SeatOwner(self.lease.owner_task),
            generation: self.lease.generation,
        };
        let verdict = slot.state.lock().verify(lease);
        verdict.map_err(|err| match err {
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
    fn a_fresh_registry_has_one_unowned_boot_seat() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        assert_eq!(seat.owner(SEAT_PRIMARY), None);
        // The inventory holds exactly the boot seat.
        let page = seat.records(0, 8);
        assert_eq!(page.len(), SeatRecord::WIRE_LEN);
        let record = SeatRecord::from_bytes(&page).expect("record decodes");
        assert_eq!(record.seat_id, SEAT_PRIMARY);
        assert!(!record.owned());
    }

    #[test]
    fn an_unowned_seat_encodes_a_press_to_the_text_sink() {
        // A leaked queue stands in for the video console's input queue.
        let queue = text_queue();
        let seat = SeatRegistry::new(queue);
        assert_eq!(
            seat.inject(SEAT_PRIMARY, press_char('a')),
            Ok(KeyInput::WIRE_LEN)
        );
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
        assert_eq!(seat.inject(SEAT_PRIMARY, release), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; 8];
        // A release produces no tty bytes, so nothing reached the text sink.
        assert_eq!(queue.read(&mut buf).expect("queue read"), 0);
    }

    #[test]
    fn an_unowned_seat_with_no_injectable_sink_fails_closed() {
        // The NULL sink accepts no injected input: a press that would be
        // enqueued there surfaces `NotImplemented` rather than dropping it.
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        assert_eq!(
            seat.inject(SEAT_PRIMARY, press_char('a')),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn a_held_seat_routes_records_to_the_owner_drain() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let lease = seat
            .acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        assert_eq!(lease.owner, WM);
        assert_eq!(lease.generation, 1);
        assert_eq!(seat.owner(SEAT_PRIMARY), Some(WM));
        let record = KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::Enter),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        };
        assert_eq!(seat.inject(SEAT_PRIMARY, record), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        let n = seat
            .read_key(SEAT_PRIMARY, WM, &mut buf)
            .expect("owner drains");
        assert_eq!(n, KeyInput::WIRE_LEN);
        assert_eq!(KeyInput::from_bytes(&buf), Ok(record));
        // Drained: the channel is now empty.
        assert_eq!(seat.read_key(SEAT_PRIMARY, WM, &mut buf), Ok(0));
    }

    #[test]
    fn a_non_owner_cannot_drain_the_desktop_channel() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        assert_eq!(
            seat.inject(SEAT_PRIMARY, press_char('s')),
            Ok(KeyInput::WIRE_LEN)
        );
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        // Neither another task nor a reader of an unowned channel may
        // siphon the owner's keystrokes; the record stays queued.
        assert_eq!(
            seat.read_key(SEAT_PRIMARY, INTRUDER, &mut buf),
            Err(Errno::SeatNotOwner)
        );
        assert_eq!(
            seat.read_key(SEAT_PRIMARY, WM, &mut buf)
                .expect("owner drains"),
            KeyInput::WIRE_LEN
        );
    }

    #[test]
    fn reading_an_unowned_seat_is_refused() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        assert_eq!(
            seat.read_key(SEAT_PRIMARY, WM, &mut buf),
            Err(Errno::SeatNotOwner)
        );
    }

    #[test]
    fn a_second_task_cannot_steal_a_held_seat() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        assert_eq!(seat.acquire(SEAT_PRIMARY, INTRUDER), Err(Errno::SeatBusy));
        assert_eq!(seat.owner(SEAT_PRIMARY), Some(WM));
    }

    #[test]
    fn a_non_owner_cannot_release_a_held_seat() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        assert_eq!(
            seat.release(SEAT_PRIMARY, INTRUDER),
            Err(Errno::SeatNotOwner)
        );
        assert_eq!(seat.owner(SEAT_PRIMARY), Some(WM));
    }

    #[test]
    fn release_returns_input_to_the_text_sink() {
        let queue = text_queue();
        let seat = SeatRegistry::new(queue);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        // A press routed to the desktop channel while held.
        assert_eq!(
            seat.inject(SEAT_PRIMARY, press_char('x')),
            Ok(KeyInput::WIRE_LEN)
        );
        assert_eq!(seat.release(SEAT_PRIMARY, WM), Ok(()));
        assert_eq!(seat.owner(SEAT_PRIMARY), None);
        // Now the press routes to the text sink instead.
        assert_eq!(
            seat.inject(SEAT_PRIMARY, press_char('y')),
            Ok(KeyInput::WIRE_LEN)
        );
        let mut buf = [0u8; 8];
        let n = queue.read(&mut buf).expect("queue read");
        assert_eq!(&buf[..n], b"y");
    }

    #[test]
    fn first_delivery_latch_fires_exactly_once_per_kind() {
        // Each input kind's one-shot witness latch returns `true` on its
        // first call and `false` forever after, regardless of routing or
        // ownership — so the `key_inject` / `pointer_inject` handlers emit
        // one audit witness per kind and never one per event. The two
        // kinds latch independently: a delivered keystroke says nothing
        // about the pointer path.
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        assert!(seat.note_first_delivery(DeliveredInputKind::Key));
        assert!(!seat.note_first_delivery(DeliveredInputKind::Key));
        assert!(seat.note_first_delivery(DeliveredInputKind::Pointer));
        assert!(!seat.note_first_delivery(DeliveredInputKind::Pointer));
        assert!(!seat.note_first_delivery(DeliveredInputKind::Key));
    }

    #[test]
    fn read_key_rejects_a_short_buffer() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        let mut buf = [0u8; KeyInput::WIRE_LEN - 1];
        assert_eq!(
            seat.read_key(SEAT_PRIMARY, WM, &mut buf),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn a_held_seat_routes_pointer_records_to_the_owner_drain() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        let record = PointerInput::MovedBy { dx: 40, dy: -8 };
        assert_eq!(
            seat.inject_pointer(SEAT_PRIMARY, record),
            Ok(PointerInput::WIRE_LEN)
        );
        let mut buf = [0u8; PointerInput::WIRE_LEN];
        let n = seat
            .read_pointer(SEAT_PRIMARY, WM, &mut buf)
            .expect("owner drains");
        assert_eq!(n, PointerInput::WIRE_LEN);
        assert_eq!(PointerInput::from_bytes(&buf), Ok(record));
        // Drained: the channel is now empty.
        assert_eq!(seat.read_pointer(SEAT_PRIMARY, WM, &mut buf), Ok(0));
    }

    #[test]
    fn an_unowned_seat_discards_pointer_records() {
        // With no desktop owner there is no pointer consumer: the record is
        // consumed and dropped — never routed to the text sink, never an
        // error the driver must special-case per event.
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let record = PointerInput::MovedBy { dx: 1, dy: 2 };
        assert_eq!(
            seat.inject_pointer(SEAT_PRIMARY, record),
            Ok(PointerInput::WIRE_LEN)
        );
        // Acquiring afterwards finds an empty channel: the pre-ownership
        // record was not retained.
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        let mut buf = [0u8; PointerInput::WIRE_LEN];
        assert_eq!(seat.read_pointer(SEAT_PRIMARY, WM, &mut buf), Ok(0));
    }

    #[test]
    fn a_non_owner_cannot_drain_the_pointer_channel() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        let record = PointerInput::MovedBy { dx: 3, dy: 4 };
        assert_eq!(
            seat.inject_pointer(SEAT_PRIMARY, record),
            Ok(PointerInput::WIRE_LEN)
        );
        let mut buf = [0u8; PointerInput::WIRE_LEN];
        // Holding CAP_INPUT_READ alone is not enough: the drain is gated on
        // the live lease, so the record stays queued for the owner.
        assert_eq!(
            seat.read_pointer(SEAT_PRIMARY, INTRUDER, &mut buf),
            Err(Errno::SeatNotOwner)
        );
        assert_eq!(
            seat.read_pointer(SEAT_PRIMARY, WM, &mut buf)
                .expect("owner drains"),
            PointerInput::WIRE_LEN
        );
    }

    #[test]
    fn reading_pointer_of_an_unowned_seat_is_refused() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let mut buf = [0u8; PointerInput::WIRE_LEN];
        assert_eq!(
            seat.read_pointer(SEAT_PRIMARY, WM, &mut buf),
            Err(Errno::SeatNotOwner)
        );
    }

    #[test]
    fn read_pointer_rejects_a_short_buffer() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        let mut buf = [0u8; PointerInput::WIRE_LEN - 1];
        assert_eq!(
            seat.read_pointer(SEAT_PRIMARY, WM, &mut buf),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn pointer_channel_drops_the_oldest_record_on_overflow() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        // Fill the ring plus one: the first record is dropped.
        for i in 0..=POINTER_CHANNEL_CAPACITY {
            let record = PointerInput::MovedBy {
                dx: i32::try_from(i).unwrap(),
                dy: 0,
            };
            assert_eq!(
                seat.inject_pointer(SEAT_PRIMARY, record),
                Ok(PointerInput::WIRE_LEN)
            );
        }
        // The very first record (dx == 0) was evicted, so the oldest
        // surviving record is the second pushed (dx == 1).
        let mut buf = [0u8; PointerInput::WIRE_LEN];
        assert_eq!(
            seat.read_pointer(SEAT_PRIMARY, WM, &mut buf),
            Ok(PointerInput::WIRE_LEN)
        );
        assert_eq!(
            PointerInput::from_bytes(&buf),
            Ok(PointerInput::MovedBy { dx: 1, dy: 0 })
        );
    }

    #[test]
    fn pointer_and_keyboard_channels_are_independent() {
        // One seat's two channels never bleed into each other: a key record
        // is only ever drained by `read_key`, a pointer record only by
        // `read_pointer`.
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        assert_eq!(
            seat.inject(SEAT_PRIMARY, press_char('k')),
            Ok(KeyInput::WIRE_LEN)
        );
        let mut buf = [0u8; PointerInput::WIRE_LEN];
        assert_eq!(seat.read_pointer(SEAT_PRIMARY, WM, &mut buf), Ok(0));
        assert_eq!(
            seat.read_key(SEAT_PRIMARY, WM, &mut buf),
            Ok(KeyInput::WIRE_LEN)
        );
    }

    #[test]
    fn channel_drops_the_oldest_record_on_overflow() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        // Fill the ring plus one: the first record is dropped.
        for i in 0..=KEYBOARD_CHANNEL_CAPACITY {
            let c = char::from(b'a' + u8::try_from(i % 26).unwrap());
            assert_eq!(
                seat.inject(SEAT_PRIMARY, press_char(c)),
                Ok(KeyInput::WIRE_LEN)
            );
        }
        // The channel holds exactly CAPACITY records; the very first ('a')
        // was evicted, so the oldest surviving record is the second pushed.
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        let n = seat
            .read_key(SEAT_PRIMARY, WM, &mut buf)
            .expect("owner drains");
        assert_eq!(n, KeyInput::WIRE_LEN);
        let first = KeyInput::from_bytes(&buf).expect("valid record");
        assert_eq!(first, press_char('b'));
    }

    #[test]
    fn revoke_evicts_the_owner_and_returns_input_to_text() {
        let queue = text_queue();
        let seat = SeatRegistry::new(queue);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        assert_eq!(seat.revoke(SEAT_PRIMARY), Ok(WM));
        assert_eq!(seat.owner(SEAT_PRIMARY), None);
        // The evicted owner's next drain observes the distinct refusal, and
        // only once; afterwards it is a plain non-owner.
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        assert_eq!(
            seat.read_key(SEAT_PRIMARY, WM, &mut buf),
            Err(Errno::SeatRevoked)
        );
        // Input routes to the text foreground, never a stale desktop channel.
        assert_eq!(
            seat.inject(SEAT_PRIMARY, press_char('z')),
            Ok(KeyInput::WIRE_LEN)
        );
        let mut text = [0u8; 8];
        let n = queue.read(&mut text).expect("queue read");
        assert_eq!(&text[..n], b"z");
    }

    #[test]
    fn revoking_an_unowned_seat_is_refused() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        assert_eq!(seat.revoke(SEAT_PRIMARY), Err(Errno::SeatNotOwner));
    }

    #[test]
    fn switch_foreground_retargets_the_text_sink_route() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        assert_eq!(
            seat.switch_foreground(SEAT_PRIMARY, ConsoleIndex(2)),
            Ok(())
        );
        let record = seat.record(SEAT_PRIMARY).expect("boot seat exists");
        assert_eq!(record.foreground_console, 2);
    }

    #[test]
    fn record_reports_the_live_lease_and_generation() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let fresh = seat.record(SEAT_PRIMARY).expect("boot seat exists");
        assert_eq!(fresh.seat_id, 0);
        assert!(!fresh.owned());
        assert_eq!(fresh.owner(), None);
        assert_eq!(fresh.generation, 0);

        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        let held = seat.record(SEAT_PRIMARY).expect("boot seat exists");
        assert!(held.owned());
        assert_eq!(held.owner(), Some(WM.0));
        assert_eq!(held.generation, 1);

        seat.revoke(SEAT_PRIMARY).expect("held seat revokes");
        let revoked = seat.record(SEAT_PRIMARY).expect("boot seat exists");
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
        let lease = seat
            .acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        assert_eq!(
            seat.present_gate(handle(WM, lease.generation))
                .check_present(),
            Ok(())
        );
        // A handle naming another task, a stale generation, or a seat that
        // does not exist is refused before any scanout.
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
        let lease = seat
            .acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        let gate_handle = handle(WM, lease.generation);
        seat.revoke(SEAT_PRIMARY).expect("held seat revokes");
        // The gate re-reads the live lease on every call: the very next
        // present after the revoke is refused, and the evicted client sees
        // the distinct refusal so it learns it lost the seat.
        assert_eq!(
            seat.present_gate(gate_handle).check_present(),
            Err(DriverError::SeatRevoked)
        );
        // The new foreground's fresh lease presents; the stale pre-revoke
        // handle stays dead even though its owner may reacquire later.
        let fresh = seat
            .acquire(SEAT_PRIMARY, INTRUDER)
            .expect("revoked seat is acquirable");
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
    fn every_seat_operation_fails_closed_for_an_unknown_seat() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        assert_eq!(seat.acquire(42, WM), Err(Errno::NotFound));
        assert_eq!(seat.release(42, WM), Err(Errno::NotFound));
        assert_eq!(seat.inject(42, press_char('a')), Err(Errno::NotFound));
        assert_eq!(seat.read_key(42, WM, &mut buf), Err(Errno::NotFound));
        assert_eq!(
            seat.switch_foreground(42, ConsoleIndex(0)),
            Err(Errno::NotFound)
        );
        assert_eq!(seat.revoke(42), Err(Errno::NotFound));
        assert_eq!(seat.owner(42), None);
        assert_eq!(seat.record(42), None);
    }

    #[test]
    fn attaching_displays_mints_new_independent_seats_idempotently() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let first = seat.attach_display(5);
        let second = seat.attach_display(9);
        assert_eq!(first, 1, "ids are minted past the boot seat");
        assert_eq!(second, 2, "ids are monotonic");
        // A re-publish of the same node keeps its seat, never a duplicate.
        assert_eq!(seat.attach_display(5), first);
        // The inventory pages all three, boot seat first.
        let page = seat.records(0, 8);
        assert_eq!(page.len(), 3 * SeatRecord::WIRE_LEN);
        let ids: alloc::vec::Vec<u64> = page
            .chunks(SeatRecord::WIRE_LEN)
            .map(|chunk| {
                SeatRecord::from_bytes(chunk)
                    .expect("record decodes")
                    .seat_id
            })
            .collect();
        assert_eq!(ids, alloc::vec![SEAT_PRIMARY, first, second]);
    }

    #[test]
    fn records_page_by_whole_record_offset() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let first = seat.attach_display(5);
        // A window smaller than the set returns whole leading records; an
        // offset addresses the tail; past-the-end is the empty terminator.
        assert_eq!(seat.records(0, 1).len(), SeatRecord::WIRE_LEN);
        let tail = seat.records(1, 8);
        assert_eq!(tail.len(), SeatRecord::WIRE_LEN);
        let record = SeatRecord::from_bytes(&tail).expect("record decodes");
        assert_eq!(record.seat_id, first);
        assert!(seat.records(2, 8).is_empty());
        assert!(seat.records(0, 0).is_empty());
        assert!(seat.records(u64::MAX, 8).is_empty());
    }

    #[test]
    fn two_seats_route_input_to_their_own_owners_independently() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let second = seat.attach_display(5);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("boot seat is acquirable");
        seat.acquire(second, INTRUDER)
            .expect("hotplug seat is acquirable by another owner");
        // Each seat's records land on its own channel…
        assert_eq!(
            seat.inject(SEAT_PRIMARY, press_char('a')),
            Ok(KeyInput::WIRE_LEN)
        );
        assert_eq!(seat.inject(second, press_char('b')), Ok(KeyInput::WIRE_LEN));
        // …and only that seat's owner drains it: one seat's owner is a
        // plain non-owner on the other, so no cross-seat siphoning.
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        assert_eq!(
            seat.read_key(SEAT_PRIMARY, INTRUDER, &mut buf),
            Err(Errno::SeatNotOwner)
        );
        assert_eq!(
            seat.read_key(second, WM, &mut buf),
            Err(Errno::SeatNotOwner)
        );
        seat.read_key(SEAT_PRIMARY, WM, &mut buf)
            .expect("boot-seat owner drains");
        assert_eq!(KeyInput::from_bytes(&buf), Ok(press_char('a')));
        seat.read_key(second, INTRUDER, &mut buf)
            .expect("hotplug-seat owner drains");
        assert_eq!(KeyInput::from_bytes(&buf), Ok(press_char('b')));
        // Each drain emptied only its own seat's channel.
        assert_eq!(seat.read_key(SEAT_PRIMARY, WM, &mut buf), Ok(0));
        assert_eq!(seat.read_key(second, INTRUDER, &mut buf), Ok(0));
    }

    #[test]
    fn revoking_one_seat_leaves_the_other_held() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let second = seat.attach_display(5);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("boot seat is acquirable");
        seat.acquire(second, INTRUDER)
            .expect("hotplug seat is acquirable");
        assert_eq!(seat.revoke(second), Ok(INTRUDER));
        // The revoked seat's evicted owner observes the loss; the boot
        // seat's lease is untouched.
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        assert_eq!(
            seat.read_key(second, INTRUDER, &mut buf),
            Err(Errno::SeatRevoked)
        );
        assert_eq!(seat.owner(SEAT_PRIMARY), Some(WM));
        assert_eq!(seat.read_key(SEAT_PRIMARY, WM, &mut buf), Ok(0));
    }

    #[test]
    fn detaching_a_display_destroys_its_seat() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let second = seat.attach_display(5);
        let lease = seat
            .acquire(second, WM)
            .expect("hotplug seat is acquirable");
        let mut gate_handle = handle(WM, lease.generation);
        gate_handle.seat_id = second;
        assert_eq!(seat.present_gate(gate_handle).check_present(), Ok(()));

        assert_eq!(seat.detach_display(5), Some(second));
        // Every operation naming the dead seat fails closed, including the
        // still-held present handle — unplugging the display ends the
        // lease's authority immediately.
        assert_eq!(seat.acquire(second, WM), Err(Errno::NotFound));
        assert_eq!(seat.owner(second), None);
        assert_eq!(
            seat.present_gate(gate_handle).check_present(),
            Err(DriverError::PermissionDenied)
        );
        // The inventory shrinks back to the boot seat; a second detach of
        // the vanished node reports nothing to destroy.
        assert_eq!(seat.records(0, 8).len(), SeatRecord::WIRE_LEN);
        assert_eq!(seat.detach_display(5), None);
    }

    #[test]
    fn seat_ids_are_never_reused_after_a_destroy() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        let first = seat.attach_display(5);
        assert_eq!(seat.detach_display(5), Some(first));
        // A later display — even the same node replugged — mints a fresh
        // id, so a stale lease or record can never alias the new seat.
        let replugged = seat.attach_display(5);
        assert!(replugged > first);
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
