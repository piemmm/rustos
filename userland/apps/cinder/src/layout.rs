//! The playpen window's geometry, and the companion surface's.
//!
//! Every length is authored in *logical* pixels at the reference density and
//! resolved through the desktop's one scale, so the pen is the same size on
//! every screen.

use tairix_abi::window_ipc::DESKTOP_LAYER_MAX_SIDE_LOGICAL;
use tairix_controls::Button;
use tairix_geometry::{Rect, Scale};
use tairix_theme::Theme;

/// The playpen's client width in logical pixels.
pub const PEN_WIDTH: u32 = 320;

/// The playpen's client height in logical pixels.
pub const PEN_HEIGHT: u32 = 240;

/// The smallest client the pen can be dragged down to, in logical pixels.
///
/// Below this the floor has no room for the furniture and Cinder, so the pen
/// stops rather than drawing a scene that does not fit.
pub const PEN_MIN_WIDTH: u32 = 220;

/// The smallest client height, for the same reason.
pub const PEN_MIN_HEIGHT: u32 = 170;

/// The companion surface's side in logical pixels.
///
/// Square, and sized to hold the creature with room for the tail at full
/// swing and the contact shadow at full spread. It is deliberately at the
/// layer bound's scale rather than at it: the surface the desktop hands out
/// is capped, and a companion that needed every pixel of the cap would have
/// nothing left when the tail swung.
pub const COMPANION_SIDE: u32 = 176;

/// That the companion fits inside the desktop's own bound with room to spare.
///
/// A build-time fact rather than a test's: asking for a surface the desktop
/// would refuse could not be a working companion, and a companion that needed
/// every pixel of the cap would have nothing left when the tail swung.
const _: () = assert!(COMPANION_SIDE < DESKTOP_LAYER_MAX_SIDE_LOGICAL);

/// That the default pen is at least its own stated minimum.
const _: () = assert!(PEN_WIDTH >= PEN_MIN_WIDTH && PEN_HEIGHT >= PEN_MIN_HEIGHT);

/// Where Cinder's feet rest inside the companion surface, as a fraction of
/// its side.
///
/// Low, because the creature stands on the floor at the bottom of the surface
/// and the tail rises behind it; centring him would waste the upper half and
/// clip the tail.
pub const COMPANION_FEET: (f64, f64) = (0.5, 0.78);

/// How wide the pen's floor band is, as a fraction of what is left once the
/// control strip has taken its share.
const FLOOR_SHARE: f64 = 0.62;

/// How far the button sits from the strip's edges, in logical pixels.
const STRIP_INSET: u32 = 6;

/// The pen's parts at `client` size and `scale`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PenLayout {
    /// The whole client rectangle.
    pub client: Rect,
    /// The wall behind the floor, where the mood readout is drawn.
    pub wall: Rect,
    /// The floor Cinder walks on.
    pub floor: Rect,
    /// The bed, at the floor's left.
    pub bed: Rect,
    /// The bowl, at the floor's right.
    pub bowl: Rect,
    /// The toy he bats about.
    pub toy: Rect,
    /// The control strip along the bottom.
    pub strip: Rect,
    /// The button in the strip that lets Cinder out and brings him home.
    pub button: Rect,
}

/// How tall the pen's control strip is at `scale` under `theme`, in physical
/// pixels.
///
/// Derived from the button it carries rather than hand-picked: the theme
/// authors its own control height and type ladder, and a strip sized by a
/// constant that happened to look right can be shorter than a line of the
/// theme's own body text — which draws a plate with no label on it. Asking the
/// control is the only way the two cannot disagree.
#[must_use]
pub fn strip_height(scale: Scale, theme: &Theme) -> u32 {
    Button::height(scale, theme).saturating_add(scale.scale_length(STRIP_INSET).saturating_mul(2))
}

/// Lay the pen out for a `client` of physical pixels at `scale` under `theme`.
#[must_use]
pub fn pen(client: Rect, scale: Scale, theme: &Theme) -> PenLayout {
    // The strip is taken off the bottom first, so the room above it is what
    // the wall and floor divide between them.
    let strip_height = strip_height(scale, theme).min(client.height);
    let room = client.height.saturating_sub(strip_height);
    let floor_height = scaled_share(room, FLOOR_SHARE);
    let wall_height = room.saturating_sub(floor_height);
    let wall = Rect::new(client.left(), client.top(), client.width, wall_height);
    let floor = Rect::new(
        client.left(),
        client
            .top()
            .saturating_add(i32::try_from(wall_height).unwrap_or(0)),
        client.width,
        floor_height,
    );
    let strip = Rect::new(client.left(), floor.bottom(), client.width, strip_height);
    let button_inset = scale.scale_length(STRIP_INSET);
    let button = Rect::new(
        strip
            .left()
            .saturating_add(i32::try_from(button_inset).unwrap_or(0)),
        strip
            .top()
            .saturating_add(i32::try_from(button_inset).unwrap_or(0)),
        strip.width.saturating_sub(button_inset.saturating_mul(2)),
        strip.height.saturating_sub(button_inset.saturating_mul(2)),
    );
    let inset = scale.scale_length(FURNITURE_INSET);
    let bed_w = scale.scale_length(BED_WIDTH);
    let bed_h = scale.scale_length(BED_HEIGHT);
    let bowl_side = scale.scale_length(BOWL_SIDE);
    let toy_side = scale.scale_length(TOY_SIDE);
    let bed = Rect::new(
        floor
            .left()
            .saturating_add(i32::try_from(inset).unwrap_or(0)),
        floor
            .bottom()
            .saturating_sub(i32::try_from(bed_h.saturating_add(inset)).unwrap_or(0)),
        bed_w,
        bed_h,
    );
    let bowl = Rect::new(
        floor
            .right()
            .saturating_sub(i32::try_from(bowl_side.saturating_add(inset)).unwrap_or(0)),
        floor
            .bottom()
            .saturating_sub(i32::try_from(bowl_side.saturating_add(inset)).unwrap_or(0)),
        bowl_side,
        bowl_side,
    );
    let toy = Rect::new(
        floor.left() + i32::try_from(floor.width / 2).unwrap_or(0),
        floor.top() + i32::try_from(floor.height / 3).unwrap_or(0),
        toy_side,
        toy_side,
    );
    PenLayout {
        client,
        wall,
        floor,
        bed,
        bowl,
        toy,
        strip,
        button,
    }
}

/// How far the furniture sits from the floor's edges, in logical pixels.
const FURNITURE_INSET: u32 = 10;

/// The bed's logical width.
const BED_WIDTH: u32 = 62;

/// The bed's logical height.
const BED_HEIGHT: u32 = 26;

/// The bowl's logical side.
const BOWL_SIDE: u32 = 24;

/// The toy's logical side.
const TOY_SIDE: u32 = 14;

/// `share` of `total`, rounded down and never zero when `total` is not.
fn scaled_share(total: u32, share: f64) -> u32 {
    let scaled = f64::from(total) * share;
    // `share` is a fraction of a `u32`, so the product is well inside the
    // range; the max keeps a one-pixel client from producing a zero band.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rounded = scaled as u32;
    rounded.min(total).max(u32::from(total > 0))
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
