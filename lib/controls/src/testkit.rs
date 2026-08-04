//! Shared test support for the control-family unit tests.
//!
//! Every family exercises the same two axes — the built-in themes and the
//! heavier-contrast path — so the fixtures they need in common live here once
//! rather than being restated in each family's test module.

use tairix_theme::{Contrast, Theme};

/// A theme identical to [`Theme::dark`] but with [`Contrast::High`], so the
/// high-contrast rendering path can be exercised without a second built-in.
///
/// Only the contrast policy differs: the palette, metrics, fonts, cursors,
/// motion, and density are the dark theme's, so a test that compares a
/// high-contrast render against a normal one is comparing the contrast
/// treatment alone.
#[must_use]
pub(crate) fn high_contrast() -> Theme {
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
