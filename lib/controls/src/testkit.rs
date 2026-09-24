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
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Contrast, Fonts, Rgba, TextRole, Theme};

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

/// A theme identical to [`Theme::dark`] but with [`Contrast::Monochrome`], the
/// policy under which a state must be told apart by shape rather than by hue.
#[must_use]
pub fn monochrome() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        "Test Monochrome",
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        *base.fonts(),
        base.cursors().clone(),
        base.motion(),
        base.density(),
        Contrast::Monochrome,
    )
}

/// `rgba` as the premultiplied pixel an opaque fill of it leaves.
#[must_use]
pub fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

/// Whether `want` appears anywhere on `surface`.
#[must_use]
pub fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

/// Whether `want` appears anywhere in columns `xr` and rows `yr` of `surface`.
#[must_use]
pub fn region_has(surface: &Surface, xr: (u32, u32), yr: (u32, u32), want: Pixel) -> bool {
    (xr.0..xr.1)
        .flat_map(|x| (yr.0..yr.1).map(move |y| (x, y)))
        .any(|(x, y)| surface.get(x, y) == Some(want))
}

/// How many pixels `surface` draws differently from `bare`: the same control
/// with nothing where the text under test goes, so the count is that text's
/// own ink whatever colour the control paints it in.
///
/// # Panics
///
/// When the two are not the same size, which would compare unrelated pixels.
#[must_use]
pub fn ink_over(surface: &Surface, bare: &Surface) -> usize {
    assert_eq!(
        (surface.width(), surface.height()),
        (bare.width(), bare.height()),
        "ink is only counted between renders of one size"
    );
    surface
        .pixels()
        .iter()
        .zip(bare.pixels())
        .filter(|(drawn, plain)| drawn != plain)
        .count()
}

/// Whether the control `render` draws elides a text too long for its room
/// with the shared mark rather than cutting it where the room ran out.
///
/// The text is one word and then only spaces, so a silent cut draws exactly
/// the word's own ink and any more is the mark.
#[must_use]
pub fn marks_elision(render: impl Fn(&str) -> Surface) -> bool {
    let spilling = alloc::format!("W{}", " ".repeat(80));
    let bare = render("");
    ink_over(&render(&spilling), &bare) > ink_over(&render("W"), &bare)
}
