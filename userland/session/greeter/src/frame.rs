//! Walking the painted surface into the screen's scan-out frame.
//!
//! The shared half of this — how long a frame is, which byte order a format
//! wants, and whether a rectangle is a sub-region or the whole screen — lives
//! in `lib/display`. What is here is only the loop that copies one surface
//! into one frame at the mode's stride, which is genuinely different work
//! from a compositor blending many windows.

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::display::{DamageRect, DisplayMode};
use tairix_cursor::PlacedCursor;
use tairix_display::{scanout_len, sub_screen_damage, ChannelOrder};
use tairix_geometry::Rect;
use tairix_raster::Surface;

/// Bytes the channel encoder writes per pixel.
const PIXEL_BYTES: usize = 4;

/// What a composition asks the display to present.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Present {
    /// Nothing changed, so nothing is presented.
    Nothing,
    /// Present this sub-region of the frame.
    Region(DamageRect),
    /// Present the whole frame.
    Whole,
}

impl Present {
    /// One present covering both this and `other`, so a drain that composed
    /// several changes hands the display one call rather than one per
    /// record.
    ///
    /// Two regions become the rectangle containing both, which `mode`
    /// resolves back to a whole present once it covers the screen — the one
    /// definition of that question. A region whose origin will not convert
    /// becomes the whole frame: presenting more than changed is wasteful,
    /// presenting less shows stale pixels.
    #[must_use]
    pub fn merged(self, other: Self, mode: &DisplayMode) -> Self {
        match (self, other) {
            (Self::Nothing, present) | (present, Self::Nothing) => present,
            (Self::Whole, _) | (_, Self::Whole) => Self::Whole,
            (Self::Region(mine), Self::Region(theirs)) => {
                let (Some(mine), Some(theirs)) = (rect_of(mine), rect_of(theirs)) else {
                    return Self::Whole;
                };
                match sub_screen_damage(&mine.union(&theirs), mode) {
                    Some(region) => Self::Region(region),
                    None => Self::Whole,
                }
            }
        }
    }
}

/// `damage` as a geometry rectangle, or `None` when its origin lies beyond
/// the signed coordinate space a rectangle is expressed in.
fn rect_of(damage: DamageRect) -> Option<Rect> {
    Some(Rect::new(
        i32::try_from(damage.x).ok()?,
        i32::try_from(damage.y).ok()?,
        damage.width_px,
        damage.height_px,
    ))
}

/// One screen-shaped scan-out frame and the order its pixels go out in.
///
/// The frame is kept across repaints, so a redraw confined to the field or
/// the clock copies only those pixels and the rest of the screen stands as
/// it was.
pub struct Scanout {
    mode: DisplayMode,
    order: ChannelOrder,
    frame: Vec<u8>,
}

impl Scanout {
    /// A frame for `mode`, or `None` when the mode cannot be scanned out.
    ///
    /// A zero extent, an impossible stride, or a pixel format with no
    /// software encoding here are all refused rather than guessed: a wrong
    /// guess renders the whole screen in false colour, and there is nothing
    /// to draw on at all without a frame.
    #[must_use]
    pub fn new(mode: DisplayMode) -> Option<Self> {
        let order = ChannelOrder::for_format(mode.format)?;
        if usize::try_from(mode.format.bytes_per_pixel()).ok() != Some(PIXEL_BYTES) {
            return None;
        }
        let len = scanout_len(&mode)?;
        Some(Self {
            mode,
            order,
            frame: vec![0u8; len],
        })
    }

    /// The mode this frame is shaped for.
    #[must_use]
    pub const fn mode(&self) -> &DisplayMode {
        &self.mode
    }

    /// The rectangle the frame covers, in the surface's own coordinates.
    #[must_use]
    pub const fn screen(&self) -> Rect {
        Rect::new(0, 0, self.mode.width_px, self.mode.height_px)
    }

    /// The frame's bytes, for the present call.
    #[must_use]
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    /// Copy `surface` into the frame within `damage`, `cursor` over the top,
    /// and say what to present.
    ///
    /// `damage` is `None` for "the whole screen changed". A rectangle is
    /// clipped to the screen first, so one that lies partly or wholly outside
    /// copies what overlaps and asks for nothing when nothing does.
    ///
    /// The cursor is sampled here rather than drawn into `surface`, so the
    /// pixels behind it are never overwritten and one painted surface serves
    /// every position the pointer takes over it.
    pub fn compose(
        &mut self,
        surface: &Surface,
        cursor: Option<&PlacedCursor>,
        damage: Option<Rect>,
    ) -> Present {
        let screen = self.screen();
        let clip = match damage {
            Some(damage) => damage.intersection(&screen),
            None => screen,
        };
        if clip.is_empty() {
            return Present::Nothing;
        }
        self.blit(surface, cursor, clip);
        match sub_screen_damage(&clip, &self.mode) {
            Some(region) => Present::Region(region),
            None => Present::Whole,
        }
    }

    /// Encode `clip`'s pixels from `surface`, blended under `cursor`, into
    /// the frame.
    ///
    /// `clip` is already inside the screen, and the frame is stride-shaped
    /// for that screen, so a pixel the surface does not have is skipped
    /// rather than faulted: a surface smaller than the screen leaves those
    /// bytes as they were.
    ///
    /// Which image row the cursor draws from is resolved once per scanline,
    /// and a scanline it does not reach walks the plain copy, so the blend
    /// costs only the rows the pointer actually covers.
    fn blit(&mut self, surface: &Surface, cursor: Option<&PlacedCursor>, clip: Rect) {
        let Ok(stride) = usize::try_from(self.mode.stride_bytes) else {
            return;
        };
        let Ok(surface_width) = usize::try_from(surface.width()) else {
            return;
        };
        let Ok(surface_height) = usize::try_from(surface.height()) else {
            return;
        };
        let Ok(left) = usize::try_from(clip.left()) else {
            return;
        };
        let Ok(top) = usize::try_from(clip.top()) else {
            return;
        };
        let columns = usize::try_from(clip.width)
            .unwrap_or(0)
            .min(surface_width.saturating_sub(left));
        let rows = usize::try_from(clip.height)
            .unwrap_or(0)
            .min(surface_height.saturating_sub(top));
        let order = self.order;
        let pixels = surface.pixels();
        for row in 0..rows {
            let Some(y) = top.checked_add(row) else {
                break;
            };
            let Some(source) = y
                .checked_mul(surface_width)
                .and_then(|start| start.checked_add(left))
                .and_then(|start| pixels.get(start..start.checked_add(columns)?))
            else {
                continue;
            };
            let Some(target) = y
                .checked_mul(stride)
                .and_then(|line| line.checked_add(left.checked_mul(PIXEL_BYTES)?))
                .and_then(|start| {
                    let span = columns.checked_mul(PIXEL_BYTES)?;
                    self.frame.get_mut(start..start.checked_add(span)?)
                })
            else {
                continue;
            };
            let (slots, _) = target.as_chunks_mut::<PIXEL_BYTES>();
            let cursor_row = cursor
                .zip(i32::try_from(y).ok())
                .and_then(|(cursor, y)| cursor.local_row(y).map(|ly| (cursor, ly)));
            let Some((cursor, ly)) = cursor_row else {
                for (slot, pixel) in slots.iter_mut().zip(source) {
                    *slot = order.encode(*pixel);
                }
                continue;
            };
            for ((slot, pixel), x) in slots.iter_mut().zip(source).zip(clip.left()..) {
                let painted = cursor
                    .sample_row(x, ly)
                    .map_or(*pixel, |sprite| sprite.over(*pixel));
                *slot = order.encode(painted);
            }
        }
    }
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
