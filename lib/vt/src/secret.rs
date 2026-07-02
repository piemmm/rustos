//! The secret-entry **activity indicator**: the `[input active...]` marker a
//! terminal shows while a password is being typed with echo suppressed.
//!
//! A suppressed (no-echo) secret read renders nothing at all, so the operator
//! gets no feedback that keystrokes are landing. This module is the one
//! definition of the visual feedback every text/terminal password prompt
//! shows instead: after the first typed character the prompt gains
//! `[input active.]`, whose trailing dots animate `.` → `..` → `...` on a
//! one-second cadence while typing continues, pause (the dots are removed,
//! the marker stays) once the operator has stopped typing for a second, and
//! resume on the next keystroke. The whole marker is removed when the line is
//! submitted (Enter) or when every typed character has been erased again.
//!
//! [`SecretIndicator`] is the pure state machine: it performs no I/O and
//! reads no clock — the caller feeds it input events and tick wake-ups with
//! the current monotonic time and writes the returned [`Render`] bytes to the
//! terminal. Timing is **one-shot**: [`SecretIndicator::deadline_ns`] names
//! the single next wake-up the animation needs, or `None` when it needs none
//! (hidden, or paused after idle) — the caller arms exactly that deadline and
//! nothing else, so an idle prompt takes no timer wake-ups at all.
//!
//! The rendering is plain printable text plus the shared
//! [`control::ERASE_ECHO`] rub-out — no escape sequences — so it draws
//! correctly on every console backing (UART, framebuffer text console, or a
//! remote terminal). Nothing about the secret itself is ever rendered: the
//! marker is the same fixed text regardless of what, or how much, was typed.

use crate::control;

/// The animation cadence and the idle threshold, in nanoseconds: dots
/// advance every second while typing continues, and pause once a full
/// second passes with no input.
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
    /// Whether the marker is currently on screen (with or without dots).
    shown: bool,
    /// Dots currently rendered: `1..=MAX_DOTS` while animating, `0` while
    /// paused after idle (marker shown as `[input active]`).
    dots: u8,
    /// Whether input arrived since the last animation render, so the next
    /// tick advances the dots instead of pausing.
    active_since_render: bool,
    /// The armed one-shot deadline, while the animation is running.
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
            active_since_render: false,
            next_tick_ns: None,
        }
    }

    /// The single one-shot wake-up the animation currently needs, as an
    /// absolute monotonic deadline, or `None` when it needs none (hidden,
    /// or paused after idle). The caller arms exactly this deadline and
    /// calls [`SecretIndicator::tick`] when it passes.
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

    /// The animation deadline passed: advance the dots if input arrived
    /// since the last render, else pause (remove the dots, keep the
    /// marker) and arm no further wake-up.
    pub fn tick(&mut self, now_ns: u64) -> Render {
        if !self.shown || self.dots == 0 {
            self.next_tick_ns = None;
            return Render::empty();
        }
        if self.active_since_render {
            let from = self.dots;
            self.dots = if self.dots == MAX_DOTS {
                1
            } else {
                self.dots + 1
            };
            self.active_since_render = false;
            self.next_tick_ns = Some(now_ns.saturating_add(SECRET_TICK_NS));
            redraw_dots(from, self.dots)
        } else {
            let from = self.dots;
            self.dots = 0;
            self.next_tick_ns = None;
            redraw_dots(from, 0)
        }
    }

    /// Input activity: show the marker (first character), resume the dots
    /// (paused marker), or just note the activity for the next tick.
    fn activity(&mut self, now_ns: u64) -> Render {
        if !self.shown {
            self.shown = true;
            self.dots = 1;
            self.active_since_render = false;
            self.next_tick_ns = Some(now_ns.saturating_add(SECRET_TICK_NS));
            let mut render = Render::empty();
            render.push(HEAD);
            render.push(b".]");
            return render;
        }
        if self.dots == 0 {
            let from = self.dots;
            self.dots = 1;
            self.active_since_render = false;
            self.next_tick_ns = Some(now_ns.saturating_add(SECRET_TICK_NS));
            return redraw_dots(from, 1);
        }
        self.active_since_render = true;
        Render::empty()
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
        self.active_since_render = false;
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
    fn further_typing_renders_nothing_until_the_tick() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let render = indicator.input(SecretInput::Typed, 100);
        assert!(render.bytes().is_empty());
    }

    #[test]
    fn ticks_advance_the_dots_while_typing_continues() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        // Activity before the tick: the tick advances one dot to two.
        let _ = indicator.input(SecretInput::Typed, 500);
        let render = indicator.tick(SECRET_TICK_NS);
        // Back over `.]`, then `..]`.
        assert_eq!(bytes(&render), b"\x08\x08..]");
        assert_eq!(indicator.deadline_ns(), Some(2 * SECRET_TICK_NS));
        // Continued activity: three dots.
        let _ = indicator.input(SecretInput::Typed, SECRET_TICK_NS + 1);
        let render = indicator.tick(2 * SECRET_TICK_NS);
        assert_eq!(bytes(&render), b"\x08\x08\x08...]");
        // And the wrap back to one dot, blanking the two leftover columns.
        let _ = indicator.input(SecretInput::Typed, 2 * SECRET_TICK_NS + 1);
        let render = indicator.tick(3 * SECRET_TICK_NS);
        assert_eq!(bytes(&render), b"\x08\x08\x08\x08.]  \x08\x08");
    }

    #[test]
    fn a_second_without_typing_pauses_the_dots_and_arms_nothing() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        // No activity before the tick: the dot is removed, the marker
        // stays, and no further wake-up is armed (a paused prompt costs no
        // timer interrupts).
        let render = indicator.tick(SECRET_TICK_NS);
        assert_eq!(bytes(&render), b"\x08\x08] \x08");
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn typing_after_the_pause_resumes_the_dots() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let _ = indicator.tick(SECRET_TICK_NS);
        let render = indicator.input(SecretInput::Typed, 5 * SECRET_TICK_NS);
        // Back over `]`, then `.]` — the animation restarts at one dot.
        assert_eq!(bytes(&render), b"\x08.]");
        assert_eq!(indicator.deadline_ns(), Some(6 * SECRET_TICK_NS));
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
    fn an_erase_with_characters_left_counts_as_activity() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let render = indicator.input(SecretInput::Erased { line_empty: false }, 100);
        assert!(render.bytes().is_empty());
        // The erase was activity, so the next tick advances rather than
        // pausing.
        let render = indicator.tick(SECRET_TICK_NS);
        assert_eq!(bytes(&render), b"\x08\x08..]");
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
            let _ = indicator.input(SecretInput::Typed, now - 1);
            let _ = indicator.tick(now);
            assert!(indicator.dots <= MAX_DOTS && indicator.dots >= 1);
        }
    }
}
