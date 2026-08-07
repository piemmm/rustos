//! Shared test support for the control-family unit tests.
//!
//! Every family exercises the same two axes — the built-in themes and the
//! heavier-contrast path — so the fixtures they need in common live here once
//! rather than being restated in each family's test module.
//!
//! Compiled in for this crate's own tests and, behind the `test-support`
//! feature, for a downstream crate's tests too — a composition built from
//! these controls exercises the same two axes and must reach for this one
//! fixture rather than growing its own copy.

use tairix_font::BitmapFont;
use tairix_geometry::Scale;
use tairix_theme::{Contrast, Fonts, TextRole, Theme};

/// The face a control resolves for its own text under `theme` at `scale`.
///
/// A control is never handed a face: it asks the theme for the one the
/// interface-text role names. A test that predicts where a glyph lands, how
/// wide a label measures, or which pixels a row covers must therefore measure
/// with that same face — a hand-picked one would answer a question about a
/// face nothing draws with.
#[must_use]
pub fn control_font(theme: &Theme, scale: Scale) -> BitmapFont {
    crate::paint::role_font(theme, scale, TextRole::Body)
}

/// A theme identical to [`Theme::dark`] but whose whole type ladder is
/// authored at `base_size_px`, so a test can vary the theme's typography and
/// nothing else.
///
/// This is the only lever a test has over a control's text, because a control
/// resolves its own face from the theme — and it is the lever that proves the
/// resolution happens at all: a control that measured with a face from
/// somewhere else would size identically under two of these.
#[must_use]
pub fn text_ladder(base_size_px: u16) -> Theme {
    let base = Theme::dark();
    let fonts = Fonts::ladder(
        base.fonts().ui_family(),
        base.fonts().monospace_family(),
        base_size_px,
    );
    Theme::new(
        base.id(),
        base.name(),
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        fonts,
        base.cursors().clone(),
        base.motion(),
        base.density(),
        base.contrast(),
    )
}

/// A theme identical to [`Theme::dark`] but with [`Contrast::High`], so the
/// high-contrast rendering path can be exercised without a second built-in.
///
/// Only the contrast policy differs: the palette, metrics, fonts, cursors,
/// motion, and density are the dark theme's, so a test that compares a
/// high-contrast render against a normal one is comparing the contrast
/// treatment alone.
#[must_use]
pub fn high_contrast() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        "Test High Contrast",
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        *base.fonts(),
        base.cursors().clone(),
        base.motion(),
        base.density(),
        Contrast::High,
    )
}
