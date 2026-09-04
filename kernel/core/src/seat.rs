//! Kernel seat registry (`plans/DISPLAY.md` D2 — fold the input-focus
//! arbiter into a per-seat sink; `plans/PI.md` P11 — input follows the
//! surface owner).
//!
//! A **seat** is one physical display plus the keyboard and pointer attached
//! to it. This module hosts the kernel's seat: the [`tairix_seat::SeatState`]
//! owner/lease/routing state machine (the one definition shared with the
//! future user-space seat manager) under the registry's own lock, plus the
//! two input sinks that state machine routes between:
//!
//! * **Text foreground** (the default, an unowned seat): a key *press* is
//!   encoded to the console (tty) bytes a terminal sends — through the one
//!   shared [`tairix_keymap::encode_key_input`] map, never a second copy —
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

use tairix_abi::driver::display::SeatGate;
use tairix_abi::input::{ClickDebounce, KeyInput, PointerInput};
use tairix_abi::seat::{ReleaseSurface, SeatLease, SEAT_PRIMARY};
use tairix_abi::sysinfo::{SeatRecord, SEAT_FLAG_OWNED};
use tairix_abi::time::NANOS_PER_MILLI;
use tairix_abi::{DriverError, Errno};
use tairix_collections::SecretRing;
use tairix_fbcon::Surface;
use tairix_keymap::{encode_key_input, MAX_KEY_BYTES};
use tairix_log::Field;
use tairix_seat::{ConsoleIndex, Lease, Route, SeatError, SeatOwner, SeatState};
use tairix_sync::SpinLock;
use tairix_sysconfig::DEFAULT_CLICK_DEBOUNCE_MS;
use zeroize::Zeroize;

use crate::console::{ConsoleDevice, ConsoleInput, NULL_CONSOLE_INPUT};

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

/// A bounded, lock-protected channel of fixed-width input records the seat
/// routes to the desktop while it is held, drained one record at a time by
/// the seat owner (`keyboard_read` / `pointer_read`).
///
/// Behind both desktop input channels; only the capacity and record width
/// differ. The queue is a [`SecretRing`] because a key event can carry a typed
/// character — a password keystroke transits the keyboard channel between the
/// driver and the desktop — so every slot it vacates is blanked as the record
/// leaves and the buffer retains no copy once the consumer has taken it. That
/// also covers a destroyed seat: an undrained record is gone before the memory
/// is freed.
struct InputChannel<const CAP: usize, const REC: usize> {
    ring: SpinLock<SecretRing<[u8; REC], CAP>>,
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
            ring: SpinLock::new(SecretRing::new([0u8; REC])),
        }
    }

    /// Enqueue one record, dropping the oldest if the ring is full (the
    /// producer never blocks).
    fn push(&self, record: &[u8; REC]) {
        let mut ring = self.ring.lock();
        if ring.is_full() {
            // Drop the oldest record to make room — a stale record is
            // preferable to unbounded growth or refusing the live one. It is
            // discarded rather than handed back, so no copy of it reaches a
            // caller's stack.
            ring.discard_front(1);
        }
        // Room was just made, so the push cannot be refused; a refusal is
        // dropped rather than asserted so no path here can panic.
        let _ = ring.try_push_back(*record);
    }

    /// Whether the channel currently holds at least one undrained record
    /// (the `SeatInput` wait-set readiness probe; a peek — nothing is
    /// consumed).
    fn pending(&self) -> bool {
        !self.ring.lock().is_empty()
    }

    /// Discard every queued record, zeroing the whole backing store.
    ///
    /// Run at both ends of a lease. At the **end**, because an undrained
    /// record may hold a typed character — a passphrase keystroke transits
    /// this ring — and memory that held a credential is zeroed once it is no
    /// longer the holder's (zero-on-free). At the **acquire**, because an
    /// incoming owner must never be able to read a record it did not
    /// produce, whatever path ended the previous lease.
    fn purge(&self) {
        self.ring.lock().purge();
    }

    /// Drain one record into `out`, zeroing the drained slot, and return the
    /// number of bytes written (`REC`, or `0` when empty).
    ///
    /// `out` is assumed to be at least `REC` bytes (the caller checks the
    /// bound first).
    fn drain_one(&self, out: &mut [u8]) -> usize {
        let Some(mut record) = self.ring.lock().pop_front() else {
            return 0;
        };
        out[..REC].copy_from_slice(&record);
        // The record left the ring by value, so the copy the pop handed back
        // is itself a place a typed character can sit; it is wiped before the
        // frame goes, leaving the cleartext only in the caller's buffer.
        record.zeroize();
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
    /// The seat's pointer-button chatter filter. The seat is the one funnel
    /// every pointer injector passes through, so the window is applied here
    /// rather than in each driver — and a driver holds no configuration
    /// authority to read the operator's window with.
    debounce: SpinLock<ClickDebounce>,
}

impl SeatSlot {
    const fn new(text_sink: &'static (dyn ConsoleInput + 'static)) -> Self {
        Self {
            state: SpinLock::new(SeatState::new(ConsoleIndex(0))),
            text_sink,
            channel: KeyboardChannel::new(),
            pointer: PointerChannel::new(),
            debounce: SpinLock::new(ClickDebounce::new()),
        }
    }

    /// Discard both desktop input channels' queued records, zeroing them.
    ///
    /// One call for the pair, so no lease transition can remember to purge
    /// the keystrokes and forget the pointer trail.
    fn purge_channels(&self) {
        self.channel.purge();
        self.pointer.purge();
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

/// The operator's pointer-button chatter window, in nanoseconds.
///
/// A process-global control on the same pattern as the cache switches: the
/// seat reads it on every button edge, so an operator's value takes effect on
/// the next click without a seat needing to be rebuilt. Zero disables the
/// filter.
pub static CLICK_DEBOUNCE: ClickDebounceControl = ClickDebounceControl::new();

/// The live pointer-button chatter window every seat consults.
pub struct ClickDebounceControl {
    window_ns: AtomicU64,
}

impl ClickDebounceControl {
    /// A control at the shipped default window.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            window_ns: AtomicU64::new(DEFAULT_CLICK_DEBOUNCE_MS as u64 * NANOS_PER_MILLI),
        }
    }

    /// The window in force, in nanoseconds.
    #[must_use]
    pub fn window_ns(&self) -> u64 {
        self.window_ns.load(Ordering::Relaxed)
    }

    /// Apply the operator's window in whole milliseconds; `0` disables the
    /// filter. Bounded by the configuration parser, so no clamp is needed here.
    pub fn set_ms(&self, ms: u16) {
        self.window_ns
            .store(u64::from(ms) * NANOS_PER_MILLI, Ordering::Relaxed);
    }
}

impl Default for ClickDebounceControl {
    fn default() -> Self {
        Self::new()
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
    /// The installed system consoles, so a lease transition can hand the
    /// boot seat's display surface between its text console and the
    /// graphical session that holds it.
    ///
    /// Defaults to [`crate::console::NO_CONSOLES`]: a build with no console
    /// wiring (and every host test that constructs a bare registry) simply
    /// has no surface to hand over.
    consoles: &'static [ConsoleDevice],
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
            consoles: &crate::console::NO_CONSOLES,
            hotplug: SpinLock::new(Vec::new()),
            next_seat_id: AtomicU64::new(SEAT_PRIMARY + 1),
            first_key_delivery: AtomicBool::new(false),
            first_pointer_delivery: AtomicBool::new(false),
        }
    }

    /// Install the system console list whose display surface the boot seat's
    /// lease hands over.
    ///
    /// `const` so the boot path can chain it in the registry's `'static`
    /// initialiser, and the boot path passes the *same* list it installs on
    /// the syscall handlers, so the seat's `ConsoleIndex` and a descriptor's
    /// console index mean the same thing.
    #[must_use]
    pub const fn with_consoles(mut self, consoles: &'static [ConsoleDevice]) -> Self {
        self.consoles = consoles;
        self
    }

    /// Hand the boot seat's display surface to whoever its lease says holds
    /// it: the foreground text console while the seat is unowned, the
    /// graphical owner while it is held.
    ///
    /// `unowned` is what that foreground console does when the seat has no
    /// owner. Every path but one passes [`Surface::Shown`] — take the screen
    /// back and repaint the retained text. A release that handed the seat to
    /// another graphical presenter passes [`Surface::Blank`] instead, so the
    /// gap before that presenter's first frame shows neither the outgoing
    /// session's pixels nor a replay of a text screen nobody is returning to.
    ///
    /// The kernel's framebuffer text console paints the surface the
    /// architecture port brought up at boot, which is the **boot seat's**; a
    /// discovery-created seat's display carries no kernel text console, so a
    /// desktop that takes a second head leaves the boot console painting its
    /// own screen. A non-boot seat therefore hands nothing over, and
    /// `seat_id` is checked rather than assumed.
    ///
    /// `state` is the caller's **live guard** on the boot seat, so the
    /// decision and its application are one critical section: two CPUs
    /// transitioning the same seat cannot interleave and leave the surface
    /// with the loser's answer. The console's own render lock is a leaf
    /// (nothing it guards takes another lock), so the nesting cannot
    /// deadlock.
    fn apply_boot_surface(&self, seat_id: u64, state: &SeatState, unowned: Surface) {
        if seat_id != SEAT_PRIMARY {
            return;
        }
        let route = state.route();
        for (index, device) in self.consoles.iter().enumerate() {
            let surface = match route {
                Route::Text(console) if console.0 as usize == index => unowned,
                _ => Surface::Hidden,
            };
            device.set_surface(surface);
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
        let mut state = slot.state.lock();
        let lease = state.acquire(owner).map_err(seat_errno)?;
        // Under the same guard as the grant: the incoming owner can never
        // read a record it did not produce, and the text console gives the
        // display surface up before the new owner presents its first frame.
        slot.purge_channels();
        self.apply_boot_surface(seat_id, &state, Surface::Shown);
        Ok(lease)
    }

    /// Release seat `seat_id` held by `owner` (`display_release`),
    /// returning its input to the text foreground.
    ///
    /// `next` is the outgoing owner's statement of what its screen becomes:
    /// the text console takes it back ([`ReleaseSurface::Text`]), or it is
    /// held cleared for the graphical presenter taking over
    /// ([`ReleaseSurface::Handover`]). Only a clean, owner-initiated release
    /// may ask for the latter — a revocation or a dead owner's reclaim
    /// always restores the text console, so a wedged or hostile presenter
    /// cannot leave the screen dark.
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id.
    /// - [`Errno::SeatNotOwner`] — `owner` does not hold the seat; a
    ///   release is owner-checked, never a global "flip it back" switch.
    /// - [`Errno::SeatRevoked`] — `owner`'s lease was revoked; the
    ///   refusal acknowledges the pending revocation.
    pub fn release(
        &self,
        seat_id: u64,
        owner: SeatOwner,
        next: ReleaseSurface,
    ) -> Result<(), Errno> {
        let slot = self.resolve(seat_id)?;
        let released = {
            let mut state = slot.state.lock();
            let outcome = state.release(owner);
            // A `SeatRevoked` release *did* end the lease (it acknowledges a
            // pending eviction and returns the seat to unowned), so the
            // surface and the channels follow it exactly as a plain release;
            // only a `NotOwner` refusal changed nothing.
            if !matches!(outcome, Err(SeatError::NotOwner)) {
                slot.purge_channels();
                let unowned = match next {
                    ReleaseSurface::Text => Surface::Shown,
                    ReleaseSurface::Handover => Surface::Blank,
                };
                self.apply_boot_surface(seat_id, &state, unowned);
            }
            outcome.map_err(seat_errno)
        };
        if released.is_ok() {
            // A lease ending is a `SeatInput` readiness edge: wake any
            // parked observer so losing the seat is observable rather than
            // an eternal park.
            crate::waitq::seat_input_wake();
        }
        released
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
        let state = slot.state.lock();
        let route = state.route();
        match route {
            Route::Desktop(_) => {
                let bytes = record.to_le_bytes();
                // Decide and deliver under one guard: a record routed to
                // the *outgoing* owner must never land in the channel after
                // the next owner has acquired it, which is the one way a
                // keystroke could cross principals despite the purge.
                slot.channel.push(&bytes);
                drop(state);
                // Wake the seat owner parked on a `SeatInput` wait-set
                // member; it drains the channel and parks again when empty.
                crate::waitq::seat_input_wake();
            }
            Route::Text(_) => {
                // The text sink is a console input queue an interrupt
                // handler also pushes to, so the seat guard is dropped
                // first and only the queue's own lock is taken here.
                drop(state);
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
    pub fn inject_pointer(
        &self,
        seat_id: u64,
        record: PointerInput,
        now_ns: u64,
    ) -> Result<usize, Errno> {
        let slot = self.resolve(seat_id)?;
        // Chatter is filtered before the lease is consulted, so the filter's
        // per-button history stays continuous across a lease change: a press
        // dropped as chatter must still suppress the release that closes it,
        // whoever holds the seat by then.
        if !slot
            .debounce
            .lock()
            .admits(record, now_ns, CLICK_DEBOUNCE.window_ns())
        {
            return Ok(PointerInput::WIRE_LEN);
        }
        let state = slot.state.lock();
        if let Route::Desktop(_) = state.route() {
            let bytes = record.to_le_bytes();
            // Decided and delivered under one guard, exactly as for a key
            // edge: a pointer trail never crosses a lease boundary.
            slot.pointer.push(&bytes);
            drop(state);
            // Wake the seat owner parked on a `SeatInput` wait-set member.
            crate::waitq::seat_input_wake();
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
        let seat_id = {
            let mut hotplug = self.hotplug.lock();
            let index = hotplug.iter().position(|seat| seat.node_id == node_id)?;
            let seat = hotplug.remove(index);
            seat.seat_id
        };
        // A destroyed seat is a `SeatInput` readiness edge for its (former)
        // owner, exactly like a revoke; wake outside the hotplug lock.
        crate::waitq::seat_input_wake();
        Some(seat_id)
    }

    /// Retarget seat `seat_id`'s foreground text console (`seat_switch`,
    /// `plans/DISPLAY.md` D3).
    ///
    /// Takes effect immediately for an unowned seat — including its display
    /// surface, which moves to the new foreground console — while a held seat
    /// keeps routing to its owner until the lease ends. The syscall handler
    /// checks `CAP_SEAT_ADMIN` *before* calling this.
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id, or `console` does
    ///   not name an installed console; a typo can never strand a seat's
    ///   input or pixels on a console that does not exist.
    pub fn switch_foreground(&self, seat_id: u64, console: ConsoleIndex) -> Result<(), Errno> {
        // The registry owns the console list, so it owns the one definition
        // of "is this a live console" — validated before any state changes.
        let index = usize::try_from(console.0).map_err(|_| Errno::NotFound)?;
        if self.consoles.get(index).is_none() {
            return Err(Errno::NotFound);
        }
        let slot = self.resolve(seat_id)?;
        let mut state = slot.state.lock();
        state.set_foreground_console(console);
        self.apply_boot_surface(seat_id, &state, Surface::Shown);
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
        let evicted = {
            let mut state = slot.state.lock();
            let outcome = state.revoke();
            if outcome.is_ok() {
                // The lease ended, so the display surface returns to the
                // text console and the evicted owner's undrained records
                // are wiped — an eviction must not hand its keystrokes to
                // whoever takes the seat next.
                slot.purge_channels();
                self.apply_boot_surface(seat_id, &state, Surface::Shown);
            }
            outcome.map_err(seat_errno)
        };
        if evicted.is_ok() {
            // Wake the evicted owner parked on a `SeatInput` wait-set
            // member: its next drain fails closed `SeatRevoked`, so the
            // eviction is observed instead of parked through.
            crate::waitq::seat_input_wake();
        }
        evicted
    }

    /// Reclaim every seat held by the **gone** task `owner`, returning the
    /// screen and the keyboard to the text console (`plans/DISPLAY.md` D8).
    ///
    /// Driven from the task-exit reclaim, so it covers every way a graphical
    /// session can end: a clean `exit` that forgot (or never reached) its
    /// `display_release`, a fault kill, a signal kill, a force-quit. Without
    /// it a dead task's lease is immortal — its input keeps routing to a
    /// channel nobody drains, every later `display_acquire` is refused
    /// `SeatBusy`, and the last composited frame is frozen on the display
    /// with no way back to text.
    ///
    /// Per seat it releases the lease, wipes both input channels (undrained
    /// keystrokes never outlive their session), and hands the display surface
    /// back — all under the seat's state guard, so a task racing to acquire
    /// the freed seat either sees it still held or gets it fully reset. A
    /// pending revocation naming the dead task is cleared too: an eviction
    /// record must not outlive its subject and be inherited by a later task
    /// that reuses the id.
    ///
    /// One `SeatLeaseReclaimed` record per reclaimed seat: a lease ending
    /// because its holder died is an ownership change, and it is attributable.
    pub fn release_owned_by(&self, owner: SeatOwner, audit: &(dyn tairix_log::Sink + Sync)) {
        let mut reclaimed = false;
        let mut reclaim = |slot: &SeatSlot, seat_id: u64| {
            let mut state = slot.state.lock();
            // `release` returns `SeatRevoked` when it clears a pending
            // eviction naming this task: the lease still ended, which is
            // exactly what has to be cleaned up.
            if matches!(state.release(owner), Err(SeatError::NotOwner)) {
                return;
            }
            slot.purge_channels();
            self.apply_boot_surface(seat_id, &state, Surface::Shown);
            drop(state);
            reclaimed = true;
            crate::audit::emit(
                audit,
                tairix_log::Level::Warn,
                crate::audit::AuditEvent::SeatLeaseReclaimed,
                &[
                    Field {
                        key: "seat",
                        value: tairix_log::FieldValue::UnsignedInt(seat_id),
                    },
                    Field {
                        key: "owner",
                        value: tairix_log::FieldValue::UnsignedInt(owner.0),
                    },
                ],
            );
        };
        reclaim(&self.primary, SEAT_PRIMARY);
        {
            let hotplug = self.hotplug.lock();
            for seat in hotplug.iter() {
                reclaim(&seat.slot, seat.seat_id);
            }
        }
        if reclaimed {
            // The lease ended, so wake anything parked on a `SeatInput`
            // member of a seat this task held — a sibling observer learns
            // the session is over instead of parking forever.
            crate::waitq::seat_input_wake();
        }
    }

    /// The live lease `owner` currently holds on seat `seat_id`
    /// (`plans/DISPLAY.md` D7a): the owner-check behind adding a
    /// `SeatInput` wait-set member and behind `call_peer_seat`'s
    /// per-present answer.
    ///
    /// # Errors
    ///
    /// - [`Errno::NotFound`] — no live seat has that id.
    /// - [`Errno::SeatNotOwner`] — `owner` does not hold the seat (it is
    ///   unowned or another task holds it).
    /// - [`Errno::SeatRevoked`] — `owner`'s lease was revoked and the
    ///   revocation is unacknowledged.
    pub fn live_lease(&self, seat_id: u64, owner: SeatOwner) -> Result<Lease, Errno> {
        let slot = self.resolve(seat_id)?;
        let lease = slot.state.lock().access(owner);
        lease.map_err(seat_errno)
    }

    /// Whether `owner` holds the live lease of **any** registered seat —
    /// the kernel-attested fact behind the seat-scoped reserved-endpoint
    /// bind (`plans/APPWIN.md` AW3): the desktop session that owns a seat
    /// may bind the window, notification, and Switchboard tray-summary
    /// rendezvous without `CAP_IPC_BIND_PRIVILEGED`, and nothing else may.
    /// A revoked or released lease answers `false` (fail closed).
    #[must_use]
    pub fn holds_live_lease(&self, owner: SeatOwner) -> bool {
        if self.primary.state.lock().access(owner).is_ok() {
            return true;
        }
        self.hotplug
            .lock()
            .iter()
            .any(|seat| seat.slot.state.lock().access(owner).is_ok())
    }

    /// Whether a `SeatInput` wait-set member observing seat `seat_id` for
    /// `owner` is ready (`plans/DISPLAY.md` D7a). A non-consuming peek:
    ///
    /// - a record is queued on the seat's keyboard **or** pointer channel
    ///   while `owner` holds the live lease — there is input to drain; or
    /// - `owner` no longer holds the live lease (released, revoked, seat
    ///   destroyed) — the loss itself is the event: the woken owner's next
    ///   drain returns the typed refusal and the session tears down.
    ///
    /// Only "the lease is live and both channels are empty" parks.
    #[must_use]
    pub fn input_ready(&self, seat_id: u64, owner: SeatOwner) -> bool {
        let Ok(slot) = self.resolve(seat_id) else {
            return true;
        };
        if slot.state.lock().access(owner).is_err() {
            return true;
        }
        slot.channel.pending() || slot.pointer.pending()
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
/// ([`tairix_seat::SeatState::verify`], the one definition of the check).
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
    /// Monotonic time the seat tests inject at. Well past any configured
    /// chatter window, so no test edge is judged against a stale release.
    const TEST_NOW_NS: u64 = 1_000_000_000;

    use tairix_abi::input::PointerButtonCode;

    #[test]
    fn a_chattering_repress_never_reaches_the_pointer_channel() {
        // The on-metal fault: a worn switch emits a second press 16 ms after
        // the release. The seat drops it and the release that closes the same
        // pulse, so the desktop sees one click and no unpaired edge.
        const MS: u64 = 1_000_000;
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, SeatOwner(7)).expect("acquired");
        CLICK_DEBOUNCE.set_ms(25);

        let press = PointerInput::Pressed(PointerButtonCode::Primary);
        let release = PointerInput::Released(PointerButtonCode::Primary);
        for (record, at) in [
            (press, 0),
            (release, 80 * MS),
            (press, 96 * MS),
            (release, 128 * MS),
        ] {
            seat.inject_pointer(SEAT_PRIMARY, record, at)
                .expect("injected");
        }

        let mut buf = [0u8; PointerInput::WIRE_LEN];
        let mut drained = alloc::vec::Vec::new();
        while seat
            .read_pointer(SEAT_PRIMARY, SeatOwner(7), &mut buf)
            .expect("drained")
            != 0
        {
            drained.push(PointerInput::from_bytes(&buf).expect("decodes"));
        }
        assert_eq!(
            drained,
            alloc::vec![press, release],
            "the chatter pulse is dropped whole, not half"
        );
    }

    #[test]
    fn a_zero_window_delivers_a_rapid_fire_pair() {
        // A device whose rapid-fire mode emits click pairs at ~10 ms is
        // reporting real intent; zero must deliver every edge.
        const MS: u64 = 1_000_000;
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, SeatOwner(9)).expect("acquired");
        CLICK_DEBOUNCE.set_ms(0);

        let press = PointerInput::Pressed(PointerButtonCode::Primary);
        let release = PointerInput::Released(PointerButtonCode::Primary);
        for step in 0..3u64 {
            seat.inject_pointer(SEAT_PRIMARY, press, step * 10 * MS)
                .expect("injected");
            seat.inject_pointer(SEAT_PRIMARY, release, step * 10 * MS + 5 * MS)
                .expect("injected");
        }
        CLICK_DEBOUNCE.set_ms(DEFAULT_CLICK_DEBOUNCE_MS);

        let mut buf = [0u8; PointerInput::WIRE_LEN];
        let mut count = 0;
        while seat
            .read_pointer(SEAT_PRIMARY, SeatOwner(9), &mut buf)
            .expect("drained")
            != 0
        {
            count += 1;
        }
        assert_eq!(count, 6, "every edge of a deliberate rapid-fire run");
    }

    use super::*;
    use crate::console::{ConsoleInputQueue, ConsoleRead, ConsoleWrite, NULL_CONSOLE_READ};
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;
    use tairix_abi::input::{KeyValue, Modifiers, NamedKeyCode};

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

    /// A console whose write half records only the disposition of its
    /// display surface — the observable half of the D8 handover.
    struct SurfaceConsole {
        surface: SpinLock<Surface>,
    }

    impl ConsoleWrite for SurfaceConsole {
        fn write(&self, bytes: &[u8]) -> Result<usize, Errno> {
            Ok(bytes.len())
        }

        fn set_surface(&self, surface: Surface) {
            *self.surface.lock() = surface;
        }
    }

    /// A registry over one surface-bearing console, plus the handle the
    /// test reads the surface state back through.
    fn registry_with_surface() -> (SeatRegistry, &'static SurfaceConsole) {
        let console: &'static SurfaceConsole = Box::leak(Box::new(SurfaceConsole {
            // A fresh system is a text login, so the console starts shown.
            surface: SpinLock::new(Surface::Shown),
        }));
        let devices: &'static [ConsoleDevice] =
            Box::leak(Box::new([ConsoleDevice::new(console, &NULL_CONSOLE_READ)]));
        (
            SeatRegistry::new(&NULL_CONSOLE_INPUT).with_consoles(devices),
            console,
        )
    }

    fn surface_of(console: &SurfaceConsole) -> Surface {
        *console.surface.lock()
    }

    fn shown(console: &SurfaceConsole) -> bool {
        surface_of(console) == Surface::Shown
    }

    fn reclaimed_count(audit: &TestSink) -> usize {
        let id = crate::audit::AuditEvent::SeatLeaseReclaimed.id().0;
        audit.event_ids().iter().filter(|&&got| got == id).count()
    }

    /// The text console and a display client share one scan-out surface,
    /// so the lease decides which of them paints it: acquiring takes it,
    /// and every way the lease can end gives it back.
    #[test]
    fn the_lease_hands_the_display_surface_over_and_back() {
        let (seat, console) = registry_with_surface();
        assert!(shown(console), "a text login owns the screen at boot");

        seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");
        assert!(!shown(console), "the session owns the screen while held");

        seat.release(SEAT_PRIMARY, WM, ReleaseSurface::Text)
            .expect("seat released");
        assert!(shown(console), "a clean exit returns the screen");

        // A revoked lease ends the same way.
        seat.acquire(SEAT_PRIMARY, WM).expect("reacquired");
        assert!(!shown(console));
        assert_eq!(seat.revoke(SEAT_PRIMARY), Ok(WM));
        assert!(shown(console), "an eviction returns the screen");

        // The evicted owner's acknowledging release is refused but must
        // not disturb the surface it no longer owns.
        assert_eq!(
            seat.release(SEAT_PRIMARY, WM, ReleaseSurface::Text),
            Err(Errno::SeatRevoked),
            "the eviction is acknowledged"
        );
        assert!(shown(console));
    }

    /// A release that hands the seat to another graphical presenter leaves
    /// the screen cleared instead of replaying the text console into the
    /// gap, and the next acquire finds it that way.
    #[test]
    fn a_handover_release_leaves_the_screen_cleared() {
        let (seat, console) = registry_with_surface();
        seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");

        seat.release(SEAT_PRIMARY, WM, ReleaseSurface::Handover)
            .expect("seat released");
        assert_eq!(
            surface_of(console),
            Surface::Blank,
            "the gap before the next presenter shows nothing"
        );

        seat.acquire(SEAT_PRIMARY, INTRUDER).expect("reacquired");
        assert_eq!(surface_of(console), Surface::Hidden);
    }

    /// A handover is the outgoing owner's own clean exit and nothing else:
    /// an eviction and a dead owner's reclaim always hand the screen back
    /// to the text console, so a wedged or hostile presenter cannot leave
    /// the machine dark.
    #[test]
    fn only_a_clean_release_may_hand_the_screen_over_cleared() {
        let (seat, console) = registry_with_surface();
        let audit = TestSink::new();

        seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");
        assert_eq!(seat.revoke(SEAT_PRIMARY), Ok(WM));
        assert!(shown(console), "an eviction returns the screen");

        seat.acquire(SEAT_PRIMARY, WM).expect("reacquired");
        seat.release_owned_by(WM, &audit);
        assert!(shown(console), "a dead owner's seat returns the screen");
    }

    /// A refused acquire changes nothing: the text console keeps the
    /// screen, so a second desktop failing to start cannot blank the one
    /// that is running (fail closed).
    #[test]
    fn a_refused_acquire_leaves_the_surface_where_it_was() {
        let (seat, console) = registry_with_surface();
        seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");
        assert_eq!(seat.acquire(SEAT_PRIMARY, INTRUDER), Err(Errno::SeatBusy));
        assert!(!shown(console), "the live owner still holds the screen");

        seat.release(SEAT_PRIMARY, WM, ReleaseSurface::Text)
            .expect("seat released");
        assert_eq!(
            seat.release(SEAT_PRIMARY, INTRUDER, ReleaseSurface::Text),
            Err(Errno::SeatNotOwner)
        );
        assert!(
            shown(console),
            "a non-owner's refused release changes nothing"
        );
    }

    /// The dead-owner reclaim is what returns the user to the terminal
    /// they started the desktop from: a session killed, faulted, or
    /// force-quit never runs its own release, so task exit must free the
    /// seat and repaint the text screen.
    #[test]
    fn a_dead_owner_loses_the_seat_and_the_screen() {
        let (seat, console) = registry_with_surface();
        let audit = TestSink::new();
        seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");
        assert!(!shown(console));

        seat.release_owned_by(WM, &audit);

        assert!(shown(console), "the terminal's screen comes back");
        assert!(
            seat.acquire(SEAT_PRIMARY, INTRUDER).is_ok(),
            "the seat is acquirable again, not wedged SeatBusy"
        );
        assert_eq!(reclaimed_count(&audit), 1);
    }

    /// Reclaiming a task that holds nothing is inert — task exit runs it
    /// for every dying task, so it must not disturb a live session's
    /// screen or emit a record.
    #[test]
    fn reclaiming_a_task_that_holds_nothing_is_inert() {
        let (seat, console) = registry_with_surface();
        let audit = TestSink::new();
        seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");

        seat.release_owned_by(INTRUDER, &audit);

        assert!(!shown(console), "the live owner keeps the screen");
        assert_eq!(reclaimed_count(&audit), 0);
        assert_eq!(seat.acquire(SEAT_PRIMARY, INTRUDER), Err(Errno::SeatBusy));
    }

    /// A pending eviction naming the dead task is cleared with it, so a
    /// later task that reuses the id cannot inherit a refusal it never
    /// earned.
    #[test]
    fn the_reclaim_clears_a_pending_eviction_naming_the_dead_task() {
        let (seat, _console) = registry_with_surface();
        let audit = TestSink::new();
        seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");
        assert_eq!(seat.revoke(SEAT_PRIMARY), Ok(WM));

        seat.release_owned_by(WM, &audit);

        // The same task id acquires cleanly rather than meeting the stale
        // `SeatRevoked` marker its predecessor left, and the reclaim of a
        // revoked-but-unacknowledged lease is still recorded.
        assert_eq!(reclaimed_count(&audit), 1);
        assert!(seat.acquire(SEAT_PRIMARY, WM).is_ok());
    }

    /// Undrained records never outlive their lease: a keystroke typed at
    /// one session (a passphrase at a lock screen) must be unreadable by
    /// whoever takes the seat next, however the lease ended.
    #[test]
    fn a_lease_boundary_purges_the_input_channels() {
        let ends: [&dyn Fn(&SeatRegistry); 3] = [
            &|seat| {
                seat.release(SEAT_PRIMARY, WM, ReleaseSurface::Text)
                    .expect("released");
            },
            &|seat| {
                assert_eq!(seat.revoke(SEAT_PRIMARY), Ok(WM));
            },
            &|seat| {
                seat.release_owned_by(WM, &TestSink::new());
            },
        ];
        for end_lease in ends {
            let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
            seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");
            seat.inject(SEAT_PRIMARY, press_char('s')).expect("keyed");
            seat.inject_pointer(
                SEAT_PRIMARY,
                PointerInput::MovedBy { dx: 3, dy: 4 },
                TEST_NOW_NS,
            )
            .expect("moved");

            end_lease(&seat);
            seat.acquire(SEAT_PRIMARY, INTRUDER).expect("reacquired");

            let mut key = [0u8; KeyInput::WIRE_LEN];
            assert_eq!(
                seat.read_key(SEAT_PRIMARY, INTRUDER, &mut key),
                Ok(0),
                "the previous session's keystroke is gone"
            );
            let mut ptr = [0u8; PointerInput::WIRE_LEN];
            assert_eq!(
                seat.read_pointer(SEAT_PRIMARY, INTRUDER, &mut ptr),
                Ok(0),
                "the previous session's pointer trail is gone"
            );
            assert!(!seat.input_ready(SEAT_PRIMARY, INTRUDER));
        }
    }

    /// The foreground switch validates its target against the installed
    /// console list and moves the surface with the input, so a typo can
    /// never strand a seat's pixels on a console that does not exist.
    #[test]
    fn switching_the_foreground_validates_and_moves_the_surface() {
        let (seat, console) = registry_with_surface();
        assert_eq!(
            seat.switch_foreground(SEAT_PRIMARY, ConsoleIndex(1)),
            Err(Errno::NotFound),
            "an unknown console fails closed"
        );
        assert!(shown(console));

        assert_eq!(
            seat.switch_foreground(SEAT_PRIMARY, ConsoleIndex(0)),
            Ok(())
        );
        assert!(shown(console));

        // A held seat keeps routing to its owner, so a switch underneath
        // it must not take the screen back from the live session.
        seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");
        assert_eq!(
            seat.switch_foreground(SEAT_PRIMARY, ConsoleIndex(0)),
            Ok(())
        );
        assert!(!shown(console), "the owner keeps the screen");
    }

    /// The kernel's text console paints the **boot** seat's surface, so a
    /// session that takes a discovery-created second head leaves the boot
    /// console painting its own screen.
    #[test]
    fn a_hotplug_seats_lease_leaves_the_boot_console_alone() {
        let (seat, console) = registry_with_surface();
        let second = seat.attach_display(42);
        assert_ne!(second, SEAT_PRIMARY);

        seat.acquire(second, WM).expect("second head acquired");
        assert!(shown(console), "the boot console keeps its own screen");

        seat.release(second, WM, ReleaseSurface::Text)
            .expect("released");
        assert!(shown(console));
    }

    /// The `SeatInput` readiness probe: only "the lease is live and both
    /// desktop channels are empty" parks. Queued input, a released or
    /// revoked lease, a foreign observer, and a destroyed seat are all
    /// ready — the loss of the seat is itself the observable event.
    #[test]
    fn input_ready_reports_queued_records_and_lease_loss() {
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM).expect("seat acquired");
        // Held, both channels empty: the owner parks.
        assert!(!seat.input_ready(SEAT_PRIMARY, WM));
        // A key record queues: ready until drained.
        assert_eq!(
            seat.inject(SEAT_PRIMARY, press_char('k')),
            Ok(KeyInput::WIRE_LEN)
        );
        assert!(seat.input_ready(SEAT_PRIMARY, WM));
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        assert_eq!(
            seat.read_key(SEAT_PRIMARY, WM, &mut buf),
            Ok(KeyInput::WIRE_LEN)
        );
        assert!(!seat.input_ready(SEAT_PRIMARY, WM));
        // A pointer record queues through the sibling channel: also ready.
        assert_eq!(
            seat.inject_pointer(
                SEAT_PRIMARY,
                PointerInput::MovedBy { dx: 1, dy: 2 },
                TEST_NOW_NS
            ),
            Ok(PointerInput::WIRE_LEN)
        );
        assert!(seat.input_ready(SEAT_PRIMARY, WM));
        let mut pbuf = [0u8; PointerInput::WIRE_LEN];
        assert_eq!(
            seat.read_pointer(SEAT_PRIMARY, WM, &mut pbuf),
            Ok(PointerInput::WIRE_LEN)
        );
        assert!(!seat.input_ready(SEAT_PRIMARY, WM));
        // A task that is not the live owner never parks unwoken: it is
        // "ready" and its drain returns the typed refusal.
        assert!(seat.input_ready(SEAT_PRIMARY, INTRUDER));
        // Revocation is a readiness edge for the evicted owner.
        assert_eq!(seat.revoke(SEAT_PRIMARY), Ok(WM));
        assert!(seat.input_ready(SEAT_PRIMARY, WM));
        // An unknown seat is ready (the wait must not park forever on a
        // hot-removed display).
        assert!(seat.input_ready(999, WM));
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
    fn a_modifier_change_types_nothing_in_text_focus() {
        // A modifier names no key, so the text console must receive no bytes
        // for it — and the record must not be refused either: the arbiter
        // consumes it and the seat's sink stays untouched.
        let queue = text_queue();
        let seat = SeatRegistry::new(queue);
        let record = KeyInput::ModifiersChanged {
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        };
        assert_eq!(seat.inject(SEAT_PRIMARY, record), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; 8];
        assert_eq!(queue.read(&mut buf).expect("queue read"), 0);
    }

    #[test]
    fn a_held_seat_relays_a_modifier_change_to_the_owner() {
        // The desktop is the consumer that needs it: it holds the seat's
        // modifier state and cannot reconstruct it from keys alone.
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT);
        seat.acquire(SEAT_PRIMARY, WM)
            .expect("fresh seat is acquirable");
        let record = KeyInput::ModifiersChanged {
            modifiers: Modifiers {
                shift: true,
                meta: true,
                ..Modifiers::default()
            },
        };
        assert_eq!(seat.inject(SEAT_PRIMARY, record), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        assert_eq!(
            seat.read_key(SEAT_PRIMARY, WM, &mut buf),
            Ok(KeyInput::WIRE_LEN)
        );
        assert_eq!(KeyInput::from_bytes(&buf), Ok(record));
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
            seat.release(SEAT_PRIMARY, INTRUDER, ReleaseSurface::Text),
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
        assert_eq!(seat.release(SEAT_PRIMARY, WM, ReleaseSurface::Text), Ok(()));
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
            seat.inject_pointer(SEAT_PRIMARY, record, TEST_NOW_NS),
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
            seat.inject_pointer(SEAT_PRIMARY, record, TEST_NOW_NS),
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
            seat.inject_pointer(SEAT_PRIMARY, record, TEST_NOW_NS),
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
                seat.inject_pointer(SEAT_PRIMARY, record, TEST_NOW_NS),
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
        // Three installed consoles, because the registry validates the
        // target against the list whose surface it hands over.
        let devices: &'static [ConsoleDevice] =
            Box::leak(Box::new([(); 3].map(|()| {
                ConsoleDevice::new(&crate::console::NULL_CONSOLE, &NULL_CONSOLE_READ)
            })));
        let seat = SeatRegistry::new(&NULL_CONSOLE_INPUT).with_consoles(devices);
        assert_eq!(
            seat.switch_foreground(SEAT_PRIMARY, ConsoleIndex(2)),
            Ok(())
        );
        let record = seat.record(SEAT_PRIMARY).expect("boot seat exists");
        assert_eq!(record.foreground_console, 2);

        // Past the end of the list fails closed and leaves the foreground
        // where it was — never a seat routed at a console that is not there.
        assert_eq!(
            seat.switch_foreground(SEAT_PRIMARY, ConsoleIndex(3)),
            Err(Errno::NotFound)
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
        assert_eq!(
            seat.release(42, WM, ReleaseSurface::Text),
            Err(Errno::NotFound)
        );
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
