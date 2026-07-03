//! The secret-entry **activity indicator**: the `[input active...]` marker a
//! terminal shows while a password is being typed with echo suppressed.
//!
//! A suppressed (no-echo) secret read renders nothing at all, so the operator
//! gets no feedback that keystrokes are landing. This module is the one
//! definition of the visual feedback every text/terminal password prompt
//! shows instead: after the first typed character the prompt gains
//! `[input active.]`, whose trailing dots cycle `.` → `..` → `...` on a
//! one-second cadence for as long as the marker is shown. The whole marker
//! is removed when the line is submitted (Enter) or when every typed
//! character has been erased again.
//!
//! [`SecretIndicator`] is the pure state machine: it performs no I/O and
//! reads no clock — the caller feeds it input events and tick wake-ups with
//! the current monotonic time and writes the returned [`Render`] bytes to the
//! terminal. Timing is **one-shot**: [`SecretIndicator::deadline_ns`] names
//! the single next animation frame while the marker is shown, or `None`
//! while it is hidden — the caller arms exactly that deadline and nothing
//! else, so a prompt with nothing typed yet takes no timer wake-ups at all,
//! and the animation's wake-ups span only the bounded window from the first
//! typed character to the line's submission (or full erasure).
//!
//! The rendering is plain printable text plus the shared
//! [`control::ERASE_ECHO`] rub-out — no escape sequences — so it draws
//! correctly on every console backing (UART, framebuffer text console, or a
//! remote terminal). Nothing about the secret itself is ever rendered: the
//! marker is the same fixed text regardless of what, or how much, was typed.

use crate::control;

/// The animation cadence, in nanoseconds: the dots advance every second
/// while the marker is shown.
pub const SECRET_TICK_NS: u64 = 1_000_000_000;

/// The marker's fixed head, up to the animated dots.
const HEAD: &[u8] = b"[input active";

/// The most dots the animation shows before wrapping back to one.
const MAX_DOTS: u8 = 3;

/// The rendered marker width for a given dot count: head + dots + `]`.
const fn width(dots: u8) -> usize {
    HEAD.len() + dots as usize + 1
}

/// The bytes one indicator transition asks the caller to write.
///
/// Sized for the largest transition (rubbing out the fully-dotted marker,
/// [`control::ERASE_ECHO`] per character); a transition that renders nothing
/// is the empty slice.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Render {
    buf: [u8; width(MAX_DOTS) * control::ERASE_ECHO.len()],
    len: usize,
}

impl Render {
    /// An empty render (nothing to write).
    const fn empty() -> Self {
        Self {
            buf: [0; width(MAX_DOTS) * control::ERASE_ECHO.len()],
            len: 0,
        }
    }

    /// The bytes to write to the terminal, in order.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Append `bytes`; the buffer is sized for every transition this module
    /// emits, so an overflow is a defect the debug assertion surfaces — the
    /// release path truncates rather than writing out of bounds.
    fn push(&mut self, bytes: &[u8]) {
        let take = bytes.len().min(self.buf.len() - self.len);
        debug_assert_eq!(take, bytes.len());
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
    }

    /// Append `count` repetitions of `byte`.
    fn push_repeat(&mut self, byte: u8, count: usize) {
        for _ in 0..count {
            self.push(&[byte]);
        }
    }
}

/// An input event the secret reader feeds to the indicator, derived from the
/// read line discipline's view of the consumed bytes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SecretInput {
    /// A character was accepted into the (hidden) line.
    Typed,
    /// An erase rubbed one character out; `line_empty` reports whether the
    /// line is now empty again.
    Erased {
        /// Whether the erase left the line with no characters.
        line_empty: bool,
    },
    /// A line terminator submitted the line.
    Submitted,
}

/// The `[input active...]` activity indicator for one suppressed secret
/// read. See the module docs for the behaviour it renders.
#[derive(Debug)]
pub struct SecretIndicator {
    /// Whether the marker is currently on screen.
    shown: bool,
    /// Dots currently rendered: `1..=MAX_DOTS` while shown, `0` while
    /// hidden.
    dots: u8,
    /// The armed one-shot deadline, while the marker is shown.
    next_tick_ns: Option<u64>,
}

impl Default for SecretIndicator {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretIndicator {
    /// A fresh, hidden indicator.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            shown: false,
            dots: 0,
            next_tick_ns: None,
        }
    }

    /// The single one-shot wake-up the animation currently needs, as an
    /// absolute monotonic deadline, or `None` while the marker is hidden.
    /// The caller arms exactly this deadline and calls
    /// [`SecretIndicator::tick`] when it passes.
    #[must_use]
    pub const fn deadline_ns(&self) -> Option<u64> {
        self.next_tick_ns
    }

    /// Feed one input event at monotonic time `now_ns`, returning the bytes
    /// to write.
    pub fn input(&mut self, input: SecretInput, now_ns: u64) -> Render {
        match input {
            SecretInput::Typed => self.activity(now_ns),
            SecretInput::Erased { line_empty } => {
                if line_empty {
                    self.hide()
                } else {
                    self.activity(now_ns)
                }
            }
            SecretInput::Submitted => self.hide(),
        }
    }

    /// The animation deadline passed: advance the dots one frame (wrapping
    /// `...` back to `.`) and arm the next frame's wake-up. A stale tick
    /// after the marker was hidden renders nothing and arms nothing.
    pub fn tick(&mut self, now_ns: u64) -> Render {
        if !self.shown {
            self.next_tick_ns = None;
            return Render::empty();
        }
        let from = self.dots;
        self.dots = if self.dots == MAX_DOTS {
            1
        } else {
            self.dots + 1
        };
        self.next_tick_ns = Some(now_ns.saturating_add(SECRET_TICK_NS));
        redraw_dots(from, self.dots)
    }

    /// Input activity: show the marker on the first character; further
    /// activity renders nothing (the animation is already running) and
    /// leaves the armed cadence undisturbed.
    fn activity(&mut self, now_ns: u64) -> Render {
        if self.shown {
            return Render::empty();
        }
        self.shown = true;
        self.dots = 1;
        self.next_tick_ns = Some(now_ns.saturating_add(SECRET_TICK_NS));
        let mut render = Render::empty();
        render.push(HEAD);
        render.push(b".]");
        render
    }

    /// Remove the whole marker from the screen (submitted, or the line was
    /// erased back to empty) and arm no further wake-up.
    fn hide(&mut self) -> Render {
        let mut render = Render::empty();
        if self.shown {
            for _ in 0..width(self.dots) {
                render.push(&control::ERASE_ECHO);
            }
        }
        self.shown = false;
        self.dots = 0;
        self.next_tick_ns = None;
        render
    }
}

/// Redraw the marker's animated tail from `from` dots to `to` dots: step
/// back over the old tail (`]` plus dots), write the new dots and `]`, and
/// blank any leftover columns when the tail shrank.
fn redraw_dots(from: u8, to: u8) -> Render {
    let mut render = Render::empty();
    render.push_repeat(control::BS, usize::from(from) + 1);
    render.push_repeat(b'.', usize::from(to));
    render.push(b"]");
    if from > to {
        let shrink = usize::from(from - to);
        render.push_repeat(b' ', shrink);
        render.push_repeat(control::BS, shrink);
    }
    render
}

#[cfg(test)]
mod tests {
    use super::{Render, SecretIndicator, SecretInput, MAX_DOTS, SECRET_TICK_NS};

    /// The rendered bytes as a `Vec` for readable assertions.
    fn bytes(render: &Render) -> alloc::vec::Vec<u8> {
        render.bytes().to_vec()
    }

    #[test]
    fn hidden_until_the_first_character() {
        let indicator = SecretIndicator::new();
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn the_first_character_shows_the_marker_with_one_dot() {
        let mut indicator = SecretIndicator::new();
        let render = indicator.input(SecretInput::Typed, 10);
        assert_eq!(bytes(&render), b"[input active.]");
        // One one-shot wake-up is armed, a second after the render.
        assert_eq!(indicator.deadline_ns(), Some(10 + SECRET_TICK_NS));
    }

    #[test]
    fn further_typing_renders_nothing_and_keeps_the_cadence() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let render = indicator.input(SecretInput::Typed, 100);
        assert!(render.bytes().is_empty());
        // The armed frame is undisturbed: the cadence stays steady.
        assert_eq!(indicator.deadline_ns(), Some(SECRET_TICK_NS));
    }

    #[test]
    fn ticks_cycle_the_dots_while_the_marker_is_shown() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        // First frame: back over `.]`, then `..]`.
        let render = indicator.tick(SECRET_TICK_NS);
        assert_eq!(bytes(&render), b"\x08\x08..]");
        assert_eq!(indicator.deadline_ns(), Some(2 * SECRET_TICK_NS));
        // Second frame: three dots — no further typing required.
        let render = indicator.tick(2 * SECRET_TICK_NS);
        assert_eq!(bytes(&render), b"\x08\x08\x08...]");
        assert_eq!(indicator.deadline_ns(), Some(3 * SECRET_TICK_NS));
        // Third frame: the wrap back to one dot, blanking the two leftover
        // columns.
        let render = indicator.tick(3 * SECRET_TICK_NS);
        assert_eq!(bytes(&render), b"\x08\x08\x08\x08.]  \x08\x08");
        assert_eq!(indicator.deadline_ns(), Some(4 * SECRET_TICK_NS));
    }

    #[test]
    fn submitting_the_line_removes_the_whole_marker() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let render = indicator.input(SecretInput::Submitted, 100);
        // `[input active.]` is 15 characters; each is rubbed out with the
        // shared `BS SP BS`.
        assert_eq!(render.bytes().len(), 15 * 3);
        assert!(render.bytes().chunks(3).all(|c| c == b"\x08 \x08"));
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn erasing_the_last_character_removes_the_whole_marker() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let render = indicator.input(SecretInput::Erased { line_empty: true }, 100);
        assert_eq!(render.bytes().len(), 15 * 3);
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn an_erase_with_characters_left_keeps_the_marker_animating() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let render = indicator.input(SecretInput::Erased { line_empty: false }, 100);
        assert!(render.bytes().is_empty());
        // Characters remain, so the marker stays and the next frame still
        // advances the dots.
        let render = indicator.tick(SECRET_TICK_NS);
        assert_eq!(bytes(&render), b"\x08\x08..]");
        assert_eq!(indicator.deadline_ns(), Some(2 * SECRET_TICK_NS));
    }

    #[test]
    fn submitting_while_hidden_renders_nothing() {
        let mut indicator = SecretIndicator::new();
        let render = indicator.input(SecretInput::Submitted, 0);
        assert!(render.bytes().is_empty());
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn a_stale_tick_after_hiding_renders_nothing() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let _ = indicator.input(SecretInput::Submitted, 100);
        let render = indicator.tick(SECRET_TICK_NS);
        assert!(render.bytes().is_empty());
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn the_dot_count_never_exceeds_the_maximum() {
        let mut indicator = SecretIndicator::new();
        let mut now = 0;
        let _ = indicator.input(SecretInput::Typed, now);
        for _ in 0..10 {
            now += SECRET_TICK_NS;
            let _ = indicator.tick(now);
            assert!(indicator.dots <= MAX_DOTS && indicator.dots >= 1);
        }
    }
}
