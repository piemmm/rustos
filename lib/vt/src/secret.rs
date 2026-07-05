//! The secret-entry **activity indicator**: the `[input active...]` marker a
//! terminal shows while a password is being typed with echo suppressed.
//!
//! A suppressed (no-echo) secret read renders nothing at all, so the operator
//! gets no feedback that keystrokes are landing. This module is the one
//! definition of the visual feedback every text/terminal password prompt
//! shows instead: after the first typed character the prompt gains
//! `[input active.]`, whose trailing dots cycle `.` → `..` → `...` on a
//! one-second cadence. The animation is **bounded**: it runs for at least
//! [`SECRET_ANIMATE_NS`] after the most recent keystroke and then freezes
//! (the marker stays on screen, showing that input is still open, but the
//! dots stop moving and no further timer wake-ups are taken). A later
//! keystroke restarts the bounded animation. When the line is submitted
//! (Enter) the marker is replaced in place with `[input complete]`; when the
//! line is erased back to empty the marker is removed entirely, and an
//! aborted read clears it with [`SecretIndicator::abort`]. One indicator can
//! be reused across several prompt lines (a wrong passphrase asked again):
//! after a line completes, the next keystroke begins a fresh marker at the
//! new prompt's cursor rather than disturbing the completed one.
//!
//! [`SecretIndicator`] is the pure state machine: it performs no I/O and
//! reads no clock — the caller feeds it input events and tick wake-ups with
//! the current monotonic time and writes the returned [`Render`] bytes to the
//! terminal. Timing is **one-shot**: [`SecretIndicator::deadline_ns`] names
//! the single next animation frame while the animation is running, or `None`
//! while the marker is hidden, frozen, or complete — the caller arms exactly
//! that deadline and nothing else, so a prompt with nothing typed yet takes no
//! timer wake-ups at all, and the animation's wake-ups span only the bounded
//! window from a keystroke to [`SECRET_ANIMATE_NS`] after the last one.
//!
//! The rendering is plain printable text plus backspace/space rub-out — no
//! escape sequences — so it draws correctly on every console backing (UART,
//! framebuffer text console, or a remote terminal). Nothing about the secret
//! itself is ever rendered: the marker is the same fixed text regardless of
//! what, or how much, was typed.

use crate::control;

/// The animation cadence, in nanoseconds: the dots advance every second
/// while the animation is running.
pub const SECRET_TICK_NS: u64 = 1_000_000_000;

/// The minimum time, in nanoseconds, the dots keep animating after the most
/// recent keystroke before the animation freezes. A later keystroke restarts
/// this window. Kept a whole number of [`SECRET_TICK_NS`] frames so the
/// animation freezes cleanly on a frame boundary.
pub const SECRET_ANIMATE_NS: u64 = 3 * SECRET_TICK_NS;

/// The active marker's fixed head, up to the animated dots.
const HEAD: &[u8] = b"[input active";

/// The completed marker, shown once the line is submitted (Enter).
const COMPLETE: &[u8] = b"[input complete]";

/// The most dots the animation shows before wrapping back to one.
pub const MAX_DOTS: u8 = 3;

/// The active marker's rendered width for a given dot count: head + dots + `]`.
const fn active_width(dots: u8) -> usize {
    HEAD.len() + dots as usize + 1
}

/// The widest marker any transition draws or rubs out.
const MAX_WIDTH: usize = {
    let active = active_width(MAX_DOTS);
    if active > COMPLETE.len() {
        active
    } else {
        COMPLETE.len()
    }
};

/// The bytes one indicator transition asks the caller to write.
///
/// Sized for the largest transition: rub the widest marker out entirely
/// (backspace + space + backspace per column). A transition that renders
/// nothing is the empty slice.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Render {
    buf: [u8; MAX_WIDTH * control::ERASE_ECHO.len()],
    len: usize,
}

impl Render {
    /// An empty render (nothing to write).
    const fn empty() -> Self {
        Self {
            buf: [0; MAX_WIDTH * control::ERASE_ECHO.len()],
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

/// Replace the marker currently on screen (`old_width` columns wide, the
/// cursor sitting just past its last column) with `new`: step the cursor back
/// over the old marker, draw the new one, and blank any columns the new
/// marker no longer covers when it is shorter. `new` empty rubs the marker
/// out entirely.
fn retarget(old_width: usize, new: &[u8]) -> Render {
    let mut render = Render::empty();
    render.push_repeat(control::BS, old_width);
    render.push(new);
    if old_width > new.len() {
        let shrink = old_width - new.len();
        render.push_repeat(b' ', shrink);
        render.push_repeat(control::BS, shrink);
    }
    render
}

/// The active marker for `dots`: `[input active` + dots + `]`.
///
/// This is the one definition of the marker's text. Byte-stream consoles
/// consume it through [`SecretIndicator`], which also emits the rub-out
/// bytes to redraw in place; a cell-composited screen (a curses view that
/// repaints its field each keystroke) renders these bytes directly instead.
/// `dots` outside `1..=`[`MAX_DOTS`] is clamped, so a caller-driven cycle
/// can never draw a malformed marker.
#[must_use]
pub fn active_marker(dots: u8) -> Render {
    let mut marker = Render::empty();
    marker.push(HEAD);
    marker.push_repeat(b'.', usize::from(dots.clamp(1, MAX_DOTS)));
    marker.push(b"]");
    marker
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

/// What the indicator is currently showing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Phase {
    /// Nothing on screen.
    Hidden,
    /// The active marker is on screen with `dots` dots. `animate_until_ns`
    /// is the time the dots stop moving; `Some` while they are still moving
    /// (a one-shot frame is armed), `None` once the animation has frozen.
    Active {
        /// Dots currently rendered, `1..=MAX_DOTS`.
        dots: u8,
        /// The frozen-at time while the animation is running, else `None`.
        animate_until_ns: Option<u64>,
    },
    /// `[input complete]` is on screen; the line was submitted.
    Complete,
}

/// The `[input active...]` activity indicator for one suppressed secret
/// read. See the module docs for the behaviour it renders.
#[derive(Debug)]
pub struct SecretIndicator {
    phase: Phase,
    /// The armed one-shot deadline, while the dots are still moving.
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
            phase: Phase::Hidden,
            next_tick_ns: None,
        }
    }

    /// The single one-shot wake-up the animation currently needs, as an
    /// absolute monotonic deadline, or `None` while the marker is hidden,
    /// frozen, or complete. The caller arms exactly this deadline and calls
    /// [`SecretIndicator::tick`] when it passes.
    #[must_use]
    pub const fn deadline_ns(&self) -> Option<u64> {
        self.next_tick_ns
    }

    /// The width of the marker currently on screen, `0` when hidden.
    fn shown_width(&self) -> usize {
        match self.phase {
            Phase::Hidden => 0,
            Phase::Active { dots, .. } => active_width(dots),
            Phase::Complete => COMPLETE.len(),
        }
    }

    /// Feed one input event at monotonic time `now_ns`, returning the bytes
    /// to write.
    pub fn input(&mut self, input: SecretInput, now_ns: u64) -> Render {
        match input {
            SecretInput::Typed => self.activity(now_ns),
            SecretInput::Erased { line_empty } => {
                if line_empty {
                    self.abort()
                } else {
                    self.activity(now_ns)
                }
            }
            SecretInput::Submitted => self.complete(),
        }
    }

    /// The animation deadline passed: advance the dots one frame (wrapping
    /// `...` back to `.`) and arm the next frame, unless the bounded
    /// animation window has elapsed, in which case freeze the dots where
    /// they are and arm nothing further. A stale tick after the marker was
    /// hidden or completed renders nothing and arms nothing.
    pub fn tick(&mut self, now_ns: u64) -> Render {
        let Phase::Active {
            dots,
            animate_until_ns: Some(animate_until_ns),
        } = self.phase
        else {
            self.next_tick_ns = None;
            return Render::empty();
        };
        if now_ns >= animate_until_ns {
            self.phase = Phase::Active {
                dots,
                animate_until_ns: None,
            };
            self.next_tick_ns = None;
            return Render::empty();
        }
        let from = dots;
        let to = if dots == MAX_DOTS { 1 } else { dots + 1 };
        self.phase = Phase::Active {
            dots: to,
            animate_until_ns: Some(animate_until_ns),
        };
        self.next_tick_ns = Some(now_ns.saturating_add(SECRET_TICK_NS));
        redraw_dots(from, to)
    }

    /// Input activity (a character typed, or an erase that left characters):
    /// show the marker on the first character, and (re)start the bounded
    /// animation window from `now_ns`. Activity while the animation is
    /// already running renders nothing and leaves the armed cadence
    /// undisturbed; activity after the animation has frozen re-arms the next
    /// frame so the dots resume moving.
    ///
    /// A keystroke *after* a line was completed begins a **fresh** marker at
    /// the cursor's current position, not a redraw of the old one: the
    /// completed `[input complete]` belongs to the previous prompt line, and
    /// the next character is the first of a new secret line (a re-prompt
    /// after a wrong passphrase reuses one indicator across attempts). Drawing
    /// a fresh marker forward — never stepping the cursor back over the new
    /// prompt — is what keeps the marker appearing, and later
    /// `[input complete]` landing, in the right place on every attempt.
    fn activity(&mut self, now_ns: u64) -> Render {
        let until = now_ns.saturating_add(SECRET_ANIMATE_NS);
        match self.phase {
            Phase::Hidden | Phase::Complete => {
                self.phase = Phase::Active {
                    dots: 1,
                    animate_until_ns: Some(until),
                };
                self.next_tick_ns = Some(now_ns.saturating_add(SECRET_TICK_NS));
                active_marker(1)
            }
            Phase::Active {
                dots,
                animate_until_ns,
            } => {
                let resume = animate_until_ns.is_none();
                self.phase = Phase::Active {
                    dots,
                    animate_until_ns: Some(until),
                };
                if resume {
                    self.next_tick_ns = Some(now_ns.saturating_add(SECRET_TICK_NS));
                }
                Render::empty()
            }
        }
    }

    /// The line was submitted (Enter): replace whatever marker is on screen
    /// with `[input complete]` and arm no further wake-up. Submitting while
    /// the marker is hidden (nothing typed) renders nothing.
    fn complete(&mut self) -> Render {
        self.next_tick_ns = None;
        if self.phase == Phase::Hidden {
            return Render::empty();
        }
        let render = retarget(self.shown_width(), COMPLETE);
        self.phase = Phase::Complete;
        render
    }

    /// Remove an in-progress marker from the screen (the line was erased back
    /// to empty, or the secret read was aborted before submission) and arm no
    /// further wake-up.
    ///
    /// A *completed* marker is left untouched: once the line was submitted the
    /// `[input complete]` marker is the final, deliberate feedback and must
    /// survive the read being torn down (e.g. echo being restored), so it is
    /// never rubbed out from under the operator.
    pub fn abort(&mut self) -> Render {
        self.next_tick_ns = None;
        match self.phase {
            Phase::Active { dots, .. } => {
                let render = retarget(active_width(dots), b"");
                self.phase = Phase::Hidden;
                render
            }
            Phase::Hidden | Phase::Complete => Render::empty(),
        }
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
    use super::{
        active_marker, Render, SecretIndicator, SecretInput, MAX_DOTS, SECRET_ANIMATE_NS,
        SECRET_TICK_NS,
    };

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
    fn active_marker_renders_plain_text_and_clamps_the_dots() {
        // The one marker definition a cell-composited consumer renders
        // directly: fixed text, no control bytes.
        assert_eq!(active_marker(1).bytes(), b"[input active.]");
        assert_eq!(active_marker(2).bytes(), b"[input active..]");
        assert_eq!(active_marker(3).bytes(), b"[input active...]");
        // Out-of-range dot counts clamp rather than draw a malformed
        // marker.
        assert_eq!(active_marker(0).bytes(), b"[input active.]");
        assert_eq!(active_marker(200).bytes(), b"[input active...]");
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
    fn ticks_cycle_the_dots_while_the_animation_runs() {
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
    }

    #[test]
    fn the_animation_freezes_after_the_bounded_window() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        // Frames advance until the window (3s) elapses.
        let _ = indicator.tick(SECRET_TICK_NS);
        let _ = indicator.tick(2 * SECRET_TICK_NS);
        assert_eq!(indicator.deadline_ns(), Some(3 * SECRET_TICK_NS));
        // The frame at the window boundary freezes the dots: nothing drawn,
        // no further wake-up armed.
        let render = indicator.tick(SECRET_ANIMATE_NS);
        assert!(render.bytes().is_empty());
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn typing_after_the_freeze_resumes_the_animation() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let _ = indicator.tick(SECRET_TICK_NS);
        let _ = indicator.tick(2 * SECRET_TICK_NS);
        let _ = indicator.tick(SECRET_ANIMATE_NS);
        assert_eq!(indicator.deadline_ns(), None);
        // A later keystroke re-arms the cadence without redrawing anything.
        let now = SECRET_ANIMATE_NS + 5;
        let render = indicator.input(SecretInput::Typed, now);
        assert!(render.bytes().is_empty());
        assert_eq!(indicator.deadline_ns(), Some(now + SECRET_TICK_NS));
    }

    #[test]
    fn typing_extends_the_animation_window() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        // A keystroke at 2s pushes the freeze out to 2s + 3s = 5s.
        let _ = indicator.input(SecretInput::Typed, 2 * SECRET_TICK_NS);
        // The frame that would have frozen the old window still advances.
        let render = indicator.tick(3 * SECRET_TICK_NS);
        assert!(!render.bytes().is_empty());
        assert_eq!(indicator.deadline_ns(), Some(4 * SECRET_TICK_NS));
    }

    #[test]
    fn submitting_the_line_shows_the_complete_marker() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let render = indicator.input(SecretInput::Submitted, 100);
        // Step back over `[input active.]` (15 columns), draw
        // `[input complete]` (16); the new marker is wider, so nothing is
        // blanked.
        assert_eq!(
            bytes(&render),
            [b"\x08".repeat(15), b"[input complete]".to_vec()].concat()
        );
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn submitting_a_three_dot_marker_blanks_the_leftover_column() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let _ = indicator.tick(SECRET_TICK_NS);
        let _ = indicator.tick(2 * SECRET_TICK_NS);
        // `[input active...]` is 17 columns; `[input complete]` is 16, so one
        // trailing column is blanked.
        let render = indicator.input(SecretInput::Submitted, 100);
        assert_eq!(
            bytes(&render),
            [
                b"\x08".repeat(17),
                b"[input complete]".to_vec(),
                b" ".to_vec(),
                b"\x08".to_vec(),
            ]
            .concat()
        );
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn erasing_the_last_character_removes_the_whole_marker() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let render = indicator.input(SecretInput::Erased { line_empty: true }, 100);
        // `[input active.]` is 15 columns, each blanked with `BS … SP … BS`.
        assert_eq!(render.bytes().len(), 15 * 3);
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn aborting_removes_the_whole_marker() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let render = indicator.abort();
        assert_eq!(render.bytes().len(), 15 * 3);
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn aborting_a_completed_marker_leaves_it_on_screen() {
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let _ = indicator.input(SecretInput::Submitted, 100);
        // Tearing the read down after Enter must not rub out the deliberate
        // `[input complete]` feedback.
        let render = indicator.abort();
        assert!(render.bytes().is_empty());
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
    fn typing_after_completion_starts_a_fresh_marker() {
        // A re-prompt (a wrong passphrase asked again) reuses one indicator:
        // the first line is submitted, then the next attempt's first
        // keystroke must draw a brand-new `[input active.]` forward at the
        // new prompt's cursor — never render nothing (leaving the operator
        // with no feedback), and never step the cursor back over the prompt.
        let mut indicator = SecretIndicator::new();
        let _ = indicator.input(SecretInput::Typed, 0);
        let _ = indicator.input(SecretInput::Submitted, 10);
        let render = indicator.input(SecretInput::Typed, 20);
        assert_eq!(bytes(&render), b"[input active.]");
        assert_eq!(indicator.deadline_ns(), Some(20 + SECRET_TICK_NS));
        // Submitting the new line then lands `[input complete]` in place,
        // stepping back over exactly the fresh marker's 15 columns.
        let render = indicator.input(SecretInput::Submitted, 30);
        assert_eq!(
            bytes(&render),
            [b"\x08".repeat(15), b"[input complete]".to_vec()].concat()
        );
    }

    #[test]
    fn submitting_while_hidden_renders_nothing() {
        let mut indicator = SecretIndicator::new();
        let render = indicator.input(SecretInput::Submitted, 0);
        assert!(render.bytes().is_empty());
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn aborting_while_hidden_renders_nothing() {
        let mut indicator = SecretIndicator::new();
        let render = indicator.abort();
        assert!(render.bytes().is_empty());
        assert_eq!(indicator.deadline_ns(), None);
    }

    #[test]
    fn a_stale_tick_after_completion_renders_nothing() {
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
            // Keep typing so the window never freezes; the dots must stay
            // within range regardless of how long the animation runs.
            let _ = indicator.input(SecretInput::Typed, now);
            let _ = indicator.tick(now);
            if let super::Phase::Active { dots, .. } = indicator.phase {
                assert!((1..=MAX_DOTS).contains(&dots));
            } else {
                panic!("marker should still be active");
            }
        }
    }
}
