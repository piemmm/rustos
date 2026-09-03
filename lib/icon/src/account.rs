//! An account's identity picture: the circular disc that stands for a user.
//!
//! One account is drawn the same wherever it appears — the login screen's
//! tiles and prompt, and the desktop's own account capsule on the icon bar —
//! so the mark a person picks at login is the mark they then live with. The
//! disc is the tier beneath a picture the account itself carries: nothing
//! sets one yet, so today every account resolves to its monogram.
//!
//! The picture is always a circle, at exactly the side the slot asked for, so
//! whatever draws it neither scales nor crops what it is handed.

use tairix_font::BitmapFont;
use tairix_raster::{Color, Surface};

/// The mark drawn for an account whose name yields no character.
pub const FALLBACK_MONOGRAM: char = '?';

/// The disc mark for `name`: its first character uppercased, or
/// [`FALLBACK_MONOGRAM`] when there is nothing to take one from.
///
/// A scalar whose uppercase form is several characters (`ß`) contributes the
/// first of them, since only one is drawn.
#[must_use]
pub fn monogram_of(name: &str) -> char {
    name.chars().next().map_or(FALLBACK_MONOGRAM, |ch| {
        ch.to_uppercase().next().unwrap_or(ch)
    })
}

/// A `side`×`side` disc bearing `monogram`, in the `(fill, ink)` colours the
/// caller chose.
///
/// The caller chooses the text role `font` comes from, since a login tile's
/// disc, the prompt's larger one, and the icon bar's smaller one carry the
/// mark at different sizes. `None` when there is no room for a picture at
/// all, which leaves a slot drawing its fallback glyph.
#[must_use]
pub fn monogram_disc(
    monogram: char,
    side: u32,
    font: BitmapFont,
    (fill, ink): (Color, Color),
) -> Option<Surface> {
    if side == 0 {
        return None;
    }
    let mut disc = Surface::new(side, side)?;
    disc.fill_round_rect(0, 0, side, side, side / 2, fill);

    let mut encoded = [0u8; 4];
    let text = &*monogram.encode_utf8(&mut encoded);
    let width = font.text_width(text).min(side);
    let height = font.line_height().min(side);
    font.draw_text(
        &mut disc,
        i32::try_from((side - width) / 2).unwrap_or(0),
        i32::try_from((side - height) / 2).unwrap_or(0),
        text,
        ink,
    );
    Some(disc)
}

#[cfg(test)]
#[path = "account_tests.rs"]
mod tests;
