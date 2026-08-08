//! Shared scaffolding for the surface's own tests: the screen they run on,
//! the scripted authority they ask, and the shorthand for feeding events.
//!
//! One place, so no test file grows its own idea of what a refusing verifier
//! or a key press is.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::{Rgba, Theme};

use crate::surface::{AuthSurface, Backdrop, EventContext, Outcome, Verdict, Verifier};

pub(crate) const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};

pub(crate) const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

/// A screen comfortably larger than the whole column and a full row of
/// tiles, so the centring is visible rather than clamped.
pub(crate) const SCREEN: Rect = Rect::new(0, 0, 1000, 600);

pub(crate) fn theme() -> Theme {
    Theme::dark()
}

pub(crate) fn key(key: Key) -> InputEvent {
    InputEvent::KeyPressed {
        key,
        modifiers: Modifiers::default(),
    }
}

/// A key press with `modifiers` held.
pub(crate) fn key_with(key: Key, modifiers: Modifiers) -> InputEvent {
    InputEvent::KeyPressed { key, modifiers }
}

pub(crate) fn named(name: NamedKey) -> InputEvent {
    key(Key::Named(name))
}

pub(crate) fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

/// The centre of `rect`, as a pointer position.
pub(crate) fn centre(rect: Rect) -> InputEvent {
    moved(
        rect.origin.x + i32::try_from(rect.width / 2).expect("test rectangles are small"),
        rect.origin.y + i32::try_from(rect.height / 2).expect("test rectangles are small"),
    )
}

/// A [`Verifier`] answering from a scripted list of verdicts, oldest first,
/// and recording every `(account, secret)` it was offered — so a test can
/// assert both what was asked and what came back for it. Offered past the end
/// of the script, it refuses.
#[derive(Default)]
pub(crate) struct Scripted {
    answers: Vec<Verdict>,
    pub(crate) offered: Vec<String>,
    pub(crate) accounts: Vec<String>,
}

impl Scripted {
    pub(crate) fn new(mut answers: Vec<Verdict>) -> Self {
        answers.reverse();
        Self {
            answers,
            offered: Vec::new(),
            accounts: Vec::new(),
        }
    }

    pub(crate) fn refusing() -> Self {
        Self::new(Vec::new())
    }
}

impl Verifier for Scripted {
    fn verify(&mut self, account: &str, secret: &str) -> Verdict {
        self.accounts.push(String::from(account));
        self.offered.push(String::from(secret));
        self.answers.pop().unwrap_or(Verdict::Refused)
    }
}

/// Apply one event on [`SCREEN`] at the unscaled density.
pub(crate) fn feed(
    surface: &mut AuthSurface,
    event: &InputEvent,
    verifier: &mut dyn Verifier,
) -> Outcome {
    let theme = theme();
    surface.on_event(
        event,
        &mut EventContext {
            screen: SCREEN,
            scale: Scale::ONE,
            theme: &theme,
            verifier,
        },
    )
}

/// Type `secret` one key at a time, then press Enter, returning the outcome
/// of that final, submitting event.
pub(crate) fn submit(
    surface: &mut AuthSurface,
    secret: &str,
    verifier: &mut dyn Verifier,
) -> Outcome {
    for ch in secret.chars() {
        feed(surface, &key(Key::Char(ch)), verifier);
    }
    feed(surface, &named(NamedKey::Enter), verifier)
}

pub(crate) fn render(surface: &AuthSurface) -> Surface {
    render_in(surface, &theme())
}

/// Paint `surface` on [`SCREEN`] at the unscaled density in `theme`.
pub(crate) fn render_in(surface: &AuthSurface, theme: &Theme) -> Surface {
    surface
        .render(SCREEN, Scale::ONE, theme, Backdrop::Desktop)
        .expect("a 1000x600 frame")
}

/// The strongest difference from the frame's own backdrop colour anywhere in
/// `rect`, summed over the three colour channels.
///
/// The reference is the frame's top-left pixel, which no part of the column
/// reaches, so this is "how far what was drawn here stands out from what is
/// behind it" — zero when nothing was drawn at all.
pub(crate) fn contrast_in(frame: &Surface, rect: Rect) -> u32 {
    let Some(backdrop) = frame.get(0, 0) else {
        return 0;
    };
    let mut strongest = 0;
    for y in rows(rect, frame.height()) {
        for x in columns(rect, frame.width()) {
            let Some(pixel) = frame.get(x, y) else {
                continue;
            };
            let apart = channel_gap(pixel.r, backdrop.r)
                + channel_gap(pixel.g, backdrop.g)
                + channel_gap(pixel.b, backdrop.b);
            strongest = strongest.max(apart);
        }
    }
    strongest
}

/// Whether anything at all was drawn inside `rect`.
pub(crate) fn painted(frame: &Surface, rect: Rect) -> bool {
    contrast_in(frame, rect) > 0
}

/// How far apart two theme colours are, summed over the three channels.
///
/// The bar a legibility assertion measures against: what the theme itself
/// promises between an ink and the ground under it, rather than a number
/// somebody picked.
pub(crate) fn separation(ink: Rgba, behind: Rgba) -> u32 {
    channel_gap(ink.r, behind.r) + channel_gap(ink.g, behind.g) + channel_gap(ink.b, behind.b)
}

/// How far apart two channel values are.
fn channel_gap(a: u8, b: u8) -> u32 {
    u32::from(a.abs_diff(b))
}

/// The surface rows `rect` covers, clamped to a `height`-tall frame.
fn rows(rect: Rect, height: u32) -> core::ops::Range<u32> {
    let top = rect.origin.y.max(0).unsigned_abs().min(height);
    top..top.saturating_add(rect.height).min(height)
}

/// The surface columns `rect` covers, clamped to a `width`-wide frame.
fn columns(rect: Rect, width: u32) -> core::ops::Range<u32> {
    let left = rect.origin.x.max(0).unsigned_abs().min(width);
    left..left.saturating_add(rect.width).min(width)
}

/// Every pixel that differs between `before` and `after`.
pub(crate) fn changed_pixels(before: &Surface, after: &Surface) -> Vec<Point> {
    let mut changed = Vec::new();
    for y in 0..before.height() {
        for x in 0..before.width() {
            if before.get(x, y) != after.get(x, y) {
                changed.push(Point::new(
                    i32::try_from(x).expect("a small screen"),
                    i32::try_from(y).expect("a small screen"),
                ));
            }
        }
    }
    changed
}
