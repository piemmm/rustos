//! How much of the theme's own desktop colour a wallpaper needs behind the
//! panel for the panel's text to stay legible.

use tairix_geometry::Rect;
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

/// The lightest scrim ever returned.
///
/// A wallpaper always gets some, so panel text never sits directly on
/// photographic detail even where the picture happens to match the theme.
pub(crate) const MIN_SCRIM: u8 = 64;

/// The heaviest scrim ever returned, leaving the wallpaper visible: a login
/// screen that hid the picture entirely would have no reason to carry one.
pub(crate) const MAX_SCRIM: u8 = 224;

/// How far the scrimmed wallpaper may sit from the theme's desktop
/// brightness, in luminance steps, before the scrim is thickened.
pub(crate) const SCRIM_TOLERANCE: u32 = 48;

/// Samples taken along each axis of the panel: a fixed grid, so the cost is
/// the same for a 4K master as for a thumbnail.
pub(crate) const SAMPLES_PER_AXIS: u32 = 16;

/// The alpha the theme's scrim needs behind `panel` for text on the panel to
/// stay legible over `image`.
///
/// `image` is the already-decoded, already-fitted wallpaper in the same
/// coordinates the surface is painted in, and `panel` is the rectangle whose
/// legibility matters. The answer depends only on those and the theme, so an
/// embedder computes it once per wallpaper, screen size, and theme and keeps
/// it — it is not a per-frame call.
///
/// The picture is sampled on a bounded grid, and the scrim is sized for the
/// sample that sits *furthest* from the theme's own desktop brightness rather
/// than for the average: text has to stay readable over the worst patch under
/// it, not over the mean. A panel that no part of the picture reaches needs
/// only the resting minimum, since what shows there is the desktop colour
/// already.
#[must_use]
pub fn scrim_alpha(image: &Surface, panel: Rect, theme: &Theme) -> u8 {
    let base = Color::from(theme.palette().desktop);
    let backdrop = base.premultiply();
    let target = luminance(base);
    let Some(region) = sampled_region(image, panel) else {
        return MIN_SCRIM;
    };

    let mut worst = 0;
    for (x, y) in region.samples() {
        let Some(pixel) = image.get(x, y) else {
            continue;
        };
        let shown = luminance(pixel.over(backdrop).unpremultiply());
        worst = worst.max(target.abs_diff(shown));
    }
    if worst <= SCRIM_TOLERANCE {
        return MIN_SCRIM;
    }
    let needed = 255_u32.saturating_sub(255 * SCRIM_TOLERANCE / worst);
    u8::try_from(needed.clamp(u32::from(MIN_SCRIM), u32::from(MAX_SCRIM))).unwrap_or(MAX_SCRIM)
}

/// Perceived brightness of `color`, ignoring its alpha (it has already been
/// composited by the time this is asked).
///
/// The Rec. 601 luma weights in integer form, which is what the eye reads a
/// contrast by; a plain channel average would call saturated blue as bright
/// as green.
fn luminance(color: Color) -> u32 {
    let (r, g, b) = (u32::from(color.r), u32::from(color.g), u32::from(color.b));
    (77 * r + 150 * g + 29 * b) >> 8
}

/// The part of `panel` that lies over `image`, and the sample stride across
/// it.
struct Region {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Region {
    /// The sample positions: a fixed grid, so a large region is sampled no
    /// more often than a small one.
    fn samples(&self) -> impl Iterator<Item = (u32, u32)> {
        let Self {
            x,
            y,
            width,
            height,
        } = *self;
        let step = |extent: u32| {
            usize::try_from(extent.div_ceil(SAMPLES_PER_AXIS))
                .unwrap_or(1)
                .max(1)
        };
        let (dx, dy) = (step(width), step(height));
        (0..height).step_by(dy).flat_map(move |row| {
            (0..width)
                .step_by(dx)
                .map(move |column| (x + column, y + row))
        })
    }
}

/// Where `panel` overlaps `image`, or `None` when it does not overlap at all.
fn sampled_region(image: &Surface, panel: Rect) -> Option<Region> {
    let overlap = panel.intersection(&Rect::new(0, 0, image.width(), image.height()));
    if overlap.is_empty() {
        return None;
    }
    Some(Region {
        x: u32::try_from(overlap.origin.x).ok()?,
        y: u32::try_from(overlap.origin.y).ok()?,
        width: overlap.width,
        height: overlap.height,
    })
}
