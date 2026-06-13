//! Kernel input-focus arbiter (`AGENTS.md` §10 / §17.3 / §20; `plans/PI.md`
//! P11 — input follows the surface owner).
//!
//! A directly attached keyboard produces a single stream of decoded *key
//! edges*. Where that stream is delivered — and how it is encoded — is
//! **policy**, and policy lives above the device (`AGENTS.md` §17.4): the
//! keyboard driver emits only the device-resolved [`KeyInput`] record (a
//! pressed or released key plus the held modifiers) through the `key_inject`
//! syscall, and this arbiter decides the rest by who currently holds input
//! focus, exactly the way `stream_read` / `stream_write` deliver a tty's bytes
//! to whoever owns the terminal:
//!
//! * **Text foreground** (the default): a key *press* is encoded to the
//!   console (tty) bytes a terminal sends — through the one shared
//!   [`rustos_keymap::encode_key_input`] map, never a second copy
//!   (`AGENTS.md` §2.2) — and enqueued on the focused text console's input
//!   queue, where a login/shell `stream_read` drains it.
//! * **Desktop foreground**: the whole record is routed to the kernel
//!   keyboard channel, where the display owner (the window manager) drains it
//!   with `keyboard_read`.
//!
//! Acquiring the display ([`InputFocus::acquire_display`]) switches the
//! foreground to the desktop, and releasing it ([`InputFocus::release_display`])
//! returns it to the text console, so the keyboard follows the surface owner
//! automatically — the desktop analogue of "input follows the foreground tty"
//! (`AGENTS.md` §20). Routing is kernel-arbitrated and capability-gated (the
//! syscalls carry `CAP_INPUT_INJECT` / `CAP_DISPLAY` / `CAP_INPUT_READ`); an
//! unattached channel denies rather than leaking to a device (`AGENTS.md` §4 /
//! §5.4 / §20).

use core::sync::atomic::{AtomicBool, Ordering};

use rustos_abi::input::KeyInput;
use rustos_abi::Errno;
use rustos_keymap::{encode_key_input, MAX_KEY_BYTES};
use rustos_sync::SpinLock;
use zeroize::Zeroize;

use crate::console::{ConsoleInput, NULL_CONSOLE_INPUT};

/// Capacity, in [`KeyInput`] records, of the desktop keyboard channel's ring.
///
/// A **fixed bound**, not a scaling capacity (`AGENTS.md` §24.4): the channel
/// is the desktop analogue of a console's type-ahead FIFO, and a human types a
/// handful of keys per second, so a small ring absorbs realistic type-ahead
/// between `keyboard_read` drains. A bound rather than an unbounded queue means
/// a wedged or absent window manager can never make the keyboard driver's
/// pushes grow kernel memory without limit (`AGENTS.md` §4). Overflow drops the
/// oldest record (the producer never blocks, `AGENTS.md` §2.1).
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
/// arbiter routes to the desktop while it holds focus, drained one record at a
/// time by the display owner's `keyboard_read`.
///
/// Each drained record is **zeroed in place** as it leaves the ring: a key
/// event can carry a typed character (a password keystroke transits this
/// channel between the keyboard driver and the desktop), so the buffer must
/// not retain it after the consumer has taken it (`AGENTS.md` §4 — zero-on-free
/// for memory that held a credential; §23.1 — secret hygiene).
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
    /// producer never blocks, `AGENTS.md` §2.1).
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

/// The kernel input-focus arbiter: one keyboard stream, one foreground sink.
///
/// Holds the current foreground (text console versus desktop), the text
/// console's injectable input queue (the arbiter's text sink), and the desktop
/// keyboard channel. The boot path installs one per running kernel and points
/// the text sink at the console that owns the directly attached keyboard (on
/// the Pi, the video console's queue); a platform with no injectable text
/// console points it at [`NULL_CONSOLE_INPUT`], which fails closed
/// (`AGENTS.md` §2.9).
pub struct InputFocus {
    /// `true` while the desktop (window manager) holds focus; `false` (the
    /// default) routes to the text console.
    desktop: AtomicBool,
    /// The text console's injectable input queue — the arbiter's text sink.
    text_sink: &'static (dyn ConsoleInput + 'static),
    /// The desktop keyboard channel — the arbiter's desktop sink.
    channel: KeyboardChannel,
}

impl InputFocus {
    /// Build an arbiter whose text sink is `text_sink` and whose foreground
    /// starts at the text console (`AGENTS.md` §20 — a freshly booted system
    /// is a text login until a desktop takes the display).
    ///
    /// `const` so the boot path can place it in a `'static`.
    #[must_use]
    pub const fn new(text_sink: &'static (dyn ConsoleInput + 'static)) -> Self {
        Self {
            desktop: AtomicBool::new(false),
            text_sink,
            channel: KeyboardChannel::new(),
        }
    }

    /// `true` while the desktop holds input focus.
    #[must_use]
    pub fn desktop_focused(&self) -> bool {
        self.desktop.load(Ordering::Acquire)
    }

    /// Claim input focus for the desktop: subsequently injected key edges are
    /// routed to the keyboard channel rather than the text console
    /// (`display_acquire`).
    pub fn acquire_display(&self) {
        self.desktop.store(true, Ordering::Release);
    }

    /// Return input focus to the text console (`display_release`).
    pub fn release_display(&self) {
        self.desktop.store(false, Ordering::Release);
    }

    /// Route one decoded key edge to the current foreground sink, returning
    /// the number of bytes consumed from the record ([`KeyInput::WIRE_LEN`]).
    ///
    /// With the **desktop** foreground the whole record is enqueued on the
    /// keyboard channel. With the **text** console foreground a key *press* is
    /// encoded to console bytes and enqueued on the text sink — a release, a
    /// modifier, or a key with no terminal encoding produces no bytes
    /// (`Ok(0)` from the encoder) and nothing is enqueued. A short push to a
    /// bounded sink is best-effort (`AGENTS.md` §2.1) and does not change the
    /// consumed count, but a text sink that accepts *no* injected input (a
    /// console with no keyboard) fails closed and the error is surfaced to the
    /// driver (`AGENTS.md` §2.9).
    ///
    /// # Errors
    ///
    /// Returns the text sink's [`Errno`] (for example [`Errno::NotImplemented`]
    /// for a console with no injectable input queue) when a press would be
    /// enqueued there but the sink refuses it.
    pub fn inject(&self, record: KeyInput) -> Result<usize, Errno> {
        if self.desktop.load(Ordering::Acquire) {
            let bytes = record.to_le_bytes();
            self.channel.push(&bytes);
        } else {
            let mut out = [0u8; MAX_KEY_BYTES];
            // The shared map; an over-long sequence cannot occur for a
            // `MAX_KEY_BYTES` buffer, so a `BufferTooSmall` here would be a
            // map bug, surfaced rather than hidden (`AGENTS.md` §2.9).
            let n = encode_key_input(&record, &mut out).map_err(|_| Errno::BufferTooSmall)?;
            if n > 0 {
                // A short push (the bounded type-ahead queue is near full) is
                // best-effort; a sink that accepts no input fails closed.
                self.text_sink.push(&out[..n])?;
            }
        }
        Ok(KeyInput::WIRE_LEN)
    }

    /// Drain one decoded key event from the keyboard channel into `out`,
    /// returning the bytes written — one [`KeyInput`] record, or `0` when the
    /// channel is drained (`keyboard_read`).
    ///
    /// # Errors
    ///
    /// Returns [`Errno::BufferTooSmall`] if `out` cannot hold a whole record
    /// ([`KeyInput::WIRE_LEN`] bytes); the kernel never writes a partial
    /// record (`AGENTS.md` §2.9).
    pub fn read_key(&self, out: &mut [u8]) -> Result<usize, Errno> {
        if out.len() < KeyInput::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(self.channel.drain_one(out))
    }
}

/// The shared fail-closed arbiter a kernel build with no input-focus wiring
/// holds: its text sink is [`NULL_CONSOLE_INPUT`], so a `key_inject` in the
/// default text focus fails closed with [`Errno::NotImplemented`] and a
/// `keyboard_read` of the empty channel returns no input (`AGENTS.md` §2.9 /
/// §5.4 — never fabricate a destination).
pub static NULL_INPUT_FOCUS: InputFocus = InputFocus::new(&NULL_CONSOLE_INPUT);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::{ConsoleInputQueue, ConsoleRead};
    use alloc::boxed::Box;
    use rustos_abi::input::{KeyValue, Modifiers, NamedKeyCode};

    fn press_char(c: char) -> KeyInput {
        KeyInput::Pressed {
            key: KeyValue::Char(c),
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn defaults_to_text_focus() {
        let focus = InputFocus::new(&NULL_CONSOLE_INPUT);
        assert!(!focus.desktop_focused());
    }

    #[test]
    fn text_focus_encodes_a_press_to_the_text_sink() {
        // A leaked queue stands in for the video console's input queue.
        let queue: &'static ConsoleInputQueue = Box::leak(Box::new(ConsoleInputQueue::new()));
        let focus = InputFocus::new(queue);
        assert_eq!(focus.inject(press_char('a')), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; 8];
        let n = queue.read(&mut buf).expect("queue read");
        assert_eq!(&buf[..n], b"a");
    }

    #[test]
    fn text_focus_drops_releases_and_modifiers() {
        let queue: &'static ConsoleInputQueue = Box::leak(Box::new(ConsoleInputQueue::new()));
        let focus = InputFocus::new(queue);
        let release = KeyInput::Released {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        };
        assert_eq!(focus.inject(release), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; 8];
        // A release produces no tty bytes, so nothing reached the text sink.
        assert_eq!(queue.read(&mut buf).expect("queue read"), 0);
    }

    #[test]
    fn text_focus_with_no_injectable_sink_fails_closed() {
        // The NULL sink accepts no injected input: a press that would be
        // enqueued there surfaces `NotImplemented` rather than dropping it
        // (`AGENTS.md` §2.9).
        let focus = InputFocus::new(&NULL_CONSOLE_INPUT);
        assert_eq!(focus.inject(press_char('a')), Err(Errno::NotImplemented));
    }

    #[test]
    fn desktop_focus_routes_records_to_the_channel() {
        let focus = InputFocus::new(&NULL_CONSOLE_INPUT);
        focus.acquire_display();
        assert!(focus.desktop_focused());
        let record = KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::Enter),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        };
        assert_eq!(focus.inject(record), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        let n = focus.read_key(&mut buf).expect("read");
        assert_eq!(n, KeyInput::WIRE_LEN);
        assert_eq!(KeyInput::from_bytes(&buf), Ok(record));
        // Drained: the channel is now empty.
        assert_eq!(focus.read_key(&mut buf), Ok(0));
    }

    #[test]
    fn release_returns_focus_to_text() {
        let queue: &'static ConsoleInputQueue = Box::leak(Box::new(ConsoleInputQueue::new()));
        let focus = InputFocus::new(queue);
        focus.acquire_display();
        // A press routed to the desktop channel while focused.
        assert_eq!(focus.inject(press_char('x')), Ok(KeyInput::WIRE_LEN));
        focus.release_display();
        assert!(!focus.desktop_focused());
        // Now the press routes to the text sink instead.
        assert_eq!(focus.inject(press_char('y')), Ok(KeyInput::WIRE_LEN));
        let mut buf = [0u8; 8];
        let n = queue.read(&mut buf).expect("queue read");
        assert_eq!(&buf[..n], b"y");
    }

    #[test]
    fn read_key_rejects_a_short_buffer() {
        let focus = InputFocus::new(&NULL_CONSOLE_INPUT);
        let mut buf = [0u8; KeyInput::WIRE_LEN - 1];
        assert_eq!(focus.read_key(&mut buf), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn channel_drops_the_oldest_record_on_overflow() {
        let focus = InputFocus::new(&NULL_CONSOLE_INPUT);
        focus.acquire_display();
        // Fill the ring plus one: the first record is dropped.
        for i in 0..=KEYBOARD_CHANNEL_CAPACITY {
            let c = char::from(b'a' + u8::try_from(i % 26).unwrap());
            assert_eq!(focus.inject(press_char(c)), Ok(KeyInput::WIRE_LEN));
        }
        // The channel holds exactly CAPACITY records; the very first ('a')
        // was evicted, so the oldest surviving record is the second pushed.
        let mut buf = [0u8; KeyInput::WIRE_LEN];
        let n = focus.read_key(&mut buf).expect("read");
        assert_eq!(n, KeyInput::WIRE_LEN);
        let first = KeyInput::from_bytes(&buf).expect("valid record");
        assert_eq!(first, press_char('b'));
    }
}
