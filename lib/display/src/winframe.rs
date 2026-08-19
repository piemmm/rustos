//! Turning an application's pixels into the bytes a **window** frame holds,
//! and back.
//!
//! A window channel carries an app's pixels in one `shm`-granted frame laid out
//! by a [`DisplayMode`], exactly as the display service's scan-out frame is —
//! but with one difference that matters: the screen is opaque, so
//! [`ChannelOrder::encode`] writes a premultiplied channel unchanged, whereas a
//! window frame carries the app's own alpha and therefore holds **straight**
//! alpha. Both directions of that conversion live here, once, over the same
//! [`ChannelOrder`] the scan-out path resolves, so nothing anywhere holds a
//! second opinion about which byte is red.
//!
//! * [`encode`] — the app's side: a [`Surface`] of premultiplied pixels out
//!   into the frame it granted the session.
//! * [`decode`] — the desktop's side: a presented frame in to the compositor's
//!   own window surface, reporting the sub-rectangle whose pixels the
//!   conversion actually *changed*.
//!
//! # Why only one side is given a real [`JobRunner`]
//!
//! Both directions are row-independent by construction — a row reads and writes
//! only its own pixels — so both are expressed over a runner. In practice the
//! desktop passes a real pool and an app passes [`tairix_parallel::SERIAL`], and
//! the asymmetry is principled rather than an oversight: an app decides how much
//! it presents and should present only what it changed, while the session must
//! accept whatever damage the app declares and cannot make it smaller.
//! Spreading the pass it cannot bound is what keeps one app's whole-window
//! repaint off the desktop's critical path.
//!
//! # Fail closed
//!
//! Every index either direction will use is validated **before** the first
//! write, so a malformed or hostile geometry refuses the whole conversion
//! rather than leaving half a window converted. A format with no channel order
//! here is refused, never guessed: guessing renders the window in false colour.
//!
//! A conversion covers the surface's full extent; an active clip
//! ([`Surface::with_clip`]) narrows which pixels it reaches, which is the
//! caller's own choice rather than something this module second-guesses.

use alloc::vec::Vec;
use core::ops::Range;

use tairix_abi::driver::display::{DamageRect, DisplayMode};
use tairix_abi::Errno;
use tairix_geometry::Rect;
use tairix_parallel::JobRunner;
use tairix_raster::{RowBand, Surface};

use crate::scanout::ChannelOrder;

/// Bytes one pixel occupies in a window frame. The two orders this crate knows
/// are both four-byte, and a mode claiming otherwise is refused.
const PIXEL_BYTES: usize = 4;

/// Pixels below which a conversion is not worth splitting across cores.
///
/// A band carrying fewer than this pays more in dispatch than it saves, so a
/// small repaint — the common case once an app clips its damage — runs on the
/// calling thread with no atomics and no syscall, exactly as it did before a
/// pool existed. The same budget the compositor splits a composite by, for the
/// same reason.
const MIN_PARALLEL_BAND_PX: usize = 64 * 1024;

/// The validated shape of one conversion: the resolved channel order, the row
/// stride, and the damage rectangle's own bounds.
///
/// Resolved once per call so neither direction repeats the arithmetic per row,
/// and so the whole request is refused before a pixel moves.
struct Shape {
    order: ChannelOrder,
    stride: usize,
    /// First surface row of the rectangle.
    top: u32,
    /// One past the last surface row.
    bottom: u32,
    /// First surface column of the rectangle.
    left: u32,
    /// Columns the rectangle spans.
    columns: usize,
    /// Byte offset of the rectangle's first column within a frame row.
    first_byte: usize,
    /// Bytes the rectangle spans within a frame row.
    row_bytes: usize,
}

impl Shape {
    /// Validate `damage` against `mode` and `surface`, and against `frame_len`
    /// bytes of frame.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a format with no channel order, a pixel size
    /// other than four bytes, a rectangle reaching outside the surface, a row
    /// span wider than the stride, a frame too short for the rows the rectangle
    /// names, or any arithmetic that would overflow.
    fn resolve(
        mode: &DisplayMode,
        surface: &Surface,
        damage: DamageRect,
        frame_len: usize,
    ) -> Result<Self, Errno> {
        let order = ChannelOrder::for_format(mode.format).ok_or(Errno::OutOfRange)?;
        if mode.format.bytes_per_pixel() as usize != PIXEL_BYTES {
            return Err(Errno::OutOfRange);
        }
        let right = damage
            .x
            .checked_add(damage.width_px)
            .ok_or(Errno::OutOfRange)?;
        let bottom = damage
            .y
            .checked_add(damage.height_px)
            .ok_or(Errno::OutOfRange)?;
        if right > surface.width() || bottom > surface.height() {
            return Err(Errno::OutOfRange);
        }
        let stride = mode.stride_bytes as usize;
        let columns = damage.width_px as usize;
        let first_byte = (damage.x as usize)
            .checked_mul(PIXEL_BYTES)
            .ok_or(Errno::OutOfRange)?;
        let row_bytes = columns.checked_mul(PIXEL_BYTES).ok_or(Errno::OutOfRange)?;
        if first_byte.checked_add(row_bytes).ok_or(Errno::OutOfRange)? > stride {
            return Err(Errno::OutOfRange);
        }
        // The band split chunks whole frame rows, so every row the rectangle
        // names has to be there. A window frame is `stride × height` and the
        // rectangle is inside the surface's height, so this holds for every
        // well-formed present and refuses every other.
        if (bottom as usize)
            .checked_mul(stride)
            .ok_or(Errno::OutOfRange)?
            > frame_len
        {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            order,
            stride,
            top: damage.y,
            bottom,
            left: damage.x,
            columns,
            first_byte,
            row_bytes,
        })
    }

    /// Rows the rectangle spans.
    fn rows(&self) -> usize {
        usize::try_from(self.bottom.saturating_sub(self.top)).unwrap_or(0)
    }

    /// Columns as the extent a row borrow is asked for.
    fn width(&self) -> u32 {
        u32::try_from(self.columns).unwrap_or(u32::MAX)
    }

    /// How many rows each band carries, and whether there is more than one.
    ///
    /// The grain is a pixel budget expressed in this rectangle's own rows, so a
    /// narrow rectangle needs many rows to reach it and a wide one needs few —
    /// the rule the compositor splits a composite by.
    fn split(&self, runner: &dyn JobRunner) -> (usize, u32) {
        let rows = self.rows();
        let count = tairix_parallel::bands(
            runner,
            rows,
            MIN_PARALLEL_BAND_PX.div_ceil(self.columns.max(1)),
        );
        let per_band = u32::try_from(rows.div_ceil(count.max(1)))
            .unwrap_or(u32::MAX)
            .max(1);
        (count, per_band)
    }

    /// Byte range of surface row `y`'s span, within a frame slice whose first
    /// row is `base_row`.
    fn frame_span(&self, base_row: u32, y: u32) -> Option<Range<usize>> {
        let local = usize::try_from(y.checked_sub(base_row)?).ok()?;
        let start = local
            .checked_mul(self.stride)?
            .checked_add(self.first_byte)?;
        Some(start..start.checked_add(self.row_bytes)?)
    }

    /// The frame bytes of the rows the rectangle names.
    fn region<'a>(&self, frame: &'a mut [u8]) -> Option<&'a mut [u8]> {
        let start = (self.top as usize).checked_mul(self.stride)?;
        let end = (self.bottom as usize).checked_mul(self.stride)?;
        frame.get_mut(start..end)
    }
}

/// Convert `damage`'s pixels of `surface` into `frame` (laid out per `mode`) as
/// the straight-alpha bytes a window frame holds.
///
/// This is what an app calls before it presents: it converts exactly the
/// rectangle it is about to declare, because that is the only rectangle the
/// session copies — everything outside it stays as the last frame left it.
///
/// # Errors
///
/// Any [`Errno`] the shape validation reports. Nothing is written on a refusal.
pub fn encode(
    surface: &Surface,
    frame: &mut [u8],
    mode: &DisplayMode,
    damage: DamageRect,
    runner: &dyn JobRunner,
) -> Result<(), Errno> {
    let shape = Shape::resolve(mode, surface, damage, frame.len())?;
    if shape.rows() == 0 || shape.columns == 0 {
        return Ok(());
    }
    let (count, per_band) = shape.split(runner);
    let band_bytes = usize::try_from(per_band)
        .ok()
        .and_then(|rows| rows.checked_mul(shape.stride))
        .ok_or(Errno::OutOfRange)?;
    let top = shape.top;
    let region = shape.region(frame).ok_or(Errno::OutOfRange)?;
    let bands = region
        .chunks_mut(band_bytes)
        .enumerate()
        .map(|(index, bytes)| EncodeBand {
            base_row: top.saturating_add(
                u32::try_from(index)
                    .unwrap_or(u32::MAX)
                    .saturating_mul(per_band),
            ),
            bytes,
        });
    if count <= 1 {
        // One band is the whole rectangle: no vector, no dispatch, and the same
        // per-row body a wide conversion runs.
        for mut only in bands {
            encode_band(&shape, surface, &mut only);
        }
        return Ok(());
    }
    let mut split: Vec<EncodeBand<'_>> = bands.collect();
    tairix_parallel::for_each(runner, &mut split, &|band| {
        encode_band(&shape, surface, band);
    });
    Ok(())
}

/// One band of an [`encode`]: the frame bytes of a contiguous run of rows, and
/// the first surface row they belong to.
struct EncodeBand<'a> {
    base_row: u32,
    bytes: &'a mut [u8],
}

/// Encode every row of one band.
fn encode_band(shape: &Shape, surface: &Surface, band: &mut EncodeBand<'_>) {
    let held = u32::try_from(band.bytes.len() / shape.stride.max(1)).unwrap_or(u32::MAX);
    let last = shape.bottom.min(band.base_row.saturating_add(held));
    for y in band.base_row..last {
        let (Some(span), Some((_, source))) = (
            shape.frame_span(band.base_row, y),
            surface.row_span(y, shape.left, shape.width()),
        ) else {
            continue;
        };
        let Some(target) = band.bytes.get_mut(span) else {
            continue;
        };
        // The span is a whole number of pixels by construction, so the ragged
        // tail this splits off is always empty.
        let (slots, _tail) = target.as_chunks_mut::<PIXEL_BYTES>();
        for (slot, pixel) in slots.iter_mut().zip(source) {
            *slot = shape.order.encode_straight(*pixel);
        }
    }
}

/// Convert `damage`'s pixels of `frame` (laid out per `mode`) into `surface`,
/// returning the sub-rectangle of `damage` whose pixels the conversion actually
/// *changed* — [`Rect::EMPTY`] when the presented pixels were identical to the
/// ones already there.
///
/// The returned rectangle is the window's real damage, and it is what keeps a
/// repaint proportional to the change. An app generally cannot say which pixels
/// its own toolkit touched, so it presents whole-window damage; taking that
/// claim at face value would recomposite every pixel of the window for a hover
/// highlight a few rows tall. The comparison is exact — a pixel reported
/// unchanged carries the byte-identical value it already had — and costs one
/// extra read on a loop that already reads the frame and writes the surface.
///
/// # Errors
///
/// Any [`Errno`] the shape validation reports. Nothing is written on a refusal.
pub fn decode(
    frame: &[u8],
    surface: &mut Surface,
    mode: &DisplayMode,
    damage: DamageRect,
    runner: &dyn JobRunner,
) -> Result<Rect, Errno> {
    let shape = Shape::resolve(mode, surface, damage, frame.len())?;
    if shape.rows() == 0 || shape.columns == 0 {
        return Ok(Rect::EMPTY);
    }
    let (count, per_band) = shape.split(runner);
    let bands = surface
        .row_bands_mut(shape.top..shape.bottom, per_band)
        .map(|rows| DecodeBand {
            rows,
            changed: Bounds::default(),
        });
    let mut changed = Bounds::default();
    if count <= 1 {
        for mut only in bands {
            decode_band(&shape, frame, &mut only);
            changed.merge(&only.changed);
        }
        return Ok(changed.rect());
    }
    let mut split: Vec<DecodeBand<'_>> = bands.collect();
    tairix_parallel::for_each(runner, &mut split, &|band| {
        decode_band(&shape, frame, band);
    });
    // The union of the bands' boxes: a merge that cannot depend on the order the
    // bands ran in, which is what makes the split invisible in the result.
    for band in &split {
        changed.merge(&band.changed);
    }
    Ok(changed.rect())
}

/// One band of a [`decode`]: the surface rows it owns, and the box of pixels it
/// changed within them.
struct DecodeBand<'a> {
    rows: RowBand<'a>,
    changed: Bounds,
}

/// Decode every row of one band, accumulating the pixels it changed.
fn decode_band(shape: &Shape, frame: &[u8], band: &mut DecodeBand<'_>) {
    let rows = band.rows.rows();
    for y in rows.start.max(shape.top)..rows.end.min(shape.bottom) {
        // Addressed from the frame's own first row: the frame is shared
        // read-only, so a band indexes it absolutely rather than through a
        // slice of its own.
        let (Some(span), Some((_, target))) = (
            shape.frame_span(0, y),
            band.rows.row_span_mut(y, shape.left, shape.width()),
        ) else {
            continue;
        };
        let Some(source) = frame.get(span) else {
            continue;
        };
        // One row address and one bounds check per row, not per pixel: this
        // loop runs over every damaged pixel of every application repaint.
        let (groups, _tail) = source.as_chunks::<PIXEL_BYTES>();
        for (index, (bytes, slot)) in groups.iter().zip(target).enumerate() {
            let pixel = shape.order.decode_straight(*bytes);
            if *slot == pixel {
                continue;
            }
            *slot = pixel;
            if let Ok(step) = u32::try_from(index) {
                band.changed.include(shape.left.saturating_add(step), y);
            }
        }
    }
}

/// The bounding box of the pixels a conversion changed, accumulated as
/// inclusive edges so an untouched conversion stays distinguishable from one
/// that changed the single pixel at the origin.
#[derive(Default)]
struct Bounds {
    edges: Option<(u32, u32, u32, u32)>,
}

impl Bounds {
    /// Grow the box to cover the changed pixel `(x, y)`.
    fn include(&mut self, x: u32, y: u32) {
        self.edges = Some(match self.edges {
            None => (x, y, x, y),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
        });
    }

    /// Grow the box to cover everything `other` covers.
    fn merge(&mut self, other: &Self) {
        let Some((x0, y0, x1, y1)) = other.edges else {
            return;
        };
        self.include(x0, y0);
        self.include(x1, y1);
    }

    /// The box as a rectangle, or [`Rect::EMPTY`] when nothing was changed.
    fn rect(&self) -> Rect {
        let Some((x0, y0, x1, y1)) = self.edges else {
            return Rect::EMPTY;
        };
        let (Ok(left), Ok(top)) = (i32::try_from(x0), i32::try_from(y0)) else {
            return Rect::EMPTY;
        };
        Rect::new(
            left,
            top,
            x1.saturating_sub(x0).saturating_add(1),
            y1.saturating_sub(y0).saturating_add(1),
        )
    }
}

#[cfg(test)]
#[path = "winframe_tests.rs"]
mod tests;
