//! The `view.app` engine: the document model, the viewport, and the
//! request/answer desk a picture is drawn from.
//!
//! Host-tested and free of both windows and I/O. The engine never decodes a
//! document and never reads one: it *records* what it wants drawn and
//! *collects* what came back, so a paint draws only from state it already
//! holds. `Run` is what carries a request out, on a worker thread, against
//! the capability-empty sandbox worker that does the decoding.
//!
//! # What the viewer holds, and in which space
//!
//! A page has a natural pixel size — its own, or a drawing's rounded source
//! extent. The user may turn or mirror it, which gives the **displayed**
//! size; the zoom scales that; and the canvas shows a window of the result.
//! Three spaces therefore exist, and the engine is careful about which one a
//! value is in:
//!
//! * **page space** — the page's own axes. The render request is in this
//!   space, because the worker holds the page and knows nothing of the user's
//!   turn.
//! * **display space** — page space set down through [`Reorient`]. What the
//!   canvas shows a window of, and what a pointer position means.
//! * **window space** — the window's own pixels, which [`Layout`] divides.
//!
//! A turn is a permutation of pixels the app already holds, so it is applied
//! here rather than in the worker: the wire is straight alpha and the app
//! holds premultiplied pixels, so turning in the worker would be a lossy
//! round trip for no gain.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_geometry::Rect;
use tairix_raster::{Reorient, Surface};
use tairix_sandbox::imagerender::{ViewDocument, ViewFormat, ViewPage};

pub mod layout;
pub mod paint;
pub mod view;

pub use layout::Layout;
pub use view::{Command, Outcome, Refusal, View};

/// The window extent the viewer asks the desktop for, in logical pixels.
///
/// Large enough that a photograph fitted into the canvas is worth looking at
/// and the toolbar's tools all fit across the top; the desktop caps it to the
/// real screen.
pub const WIN_WIDTH: u32 = 900;
/// The window height the viewer asks for, in logical pixels.
pub const WIN_HEIGHT: u32 = 640;

/// The smallest window the viewer asks the window manager to hold, in
/// logical pixels: below this the toolbar's tools would not fit and the
/// canvas would be too small to read a picture in.
pub const MIN_WIN_WIDTH: u32 = 420;
/// The smallest window height the viewer asks for, in logical pixels.
pub const MIN_WIN_HEIGHT: u32 = 320;

/// Largest document, in bytes, the viewer will hold resident to hand to its
/// worker.
///
/// A fixed containment bound rather than a capacity: it does not scale with
/// the machine, because what it bounds is how much of an untrusted file one
/// window may be made to hold at once. Positional reads mean it bounds what
/// is *resident*, never what is addressable. Set to the sandbox's own
/// document ceiling, so a file this viewer accepts is one the seam accepts
/// and there is one figure rather than two that can disagree.
pub const MAX_DOCUMENT_BYTES: usize = tairix_sandbox::imagerender::MAX_DOCUMENT_BYTES;

/// The zoom ladder, in parts per thousand of the displayed picture's natural
/// size, ascending.
///
/// What the zoom-in and zoom-out tools step between, and what the slider
/// interpolates along. A ladder rather than a linear range because zoom is
/// perceived multiplicatively: a slider linear in scale would put actual size
/// in the first fiftieth of its track.
pub const ZOOM_RUNGS: [u32; ZOOM_RUNG_COUNT as usize] = [
    10, 50, 100, 250, 500, 750, 1_000, 1_500, 2_000, 3_000, 4_000, 8_000, 16_000, 32_000, 64_000,
];

/// How many rungs the ladder has, named so the slider's own arithmetic is
/// derived from it rather than from a cast of the array's length.
const ZOOM_RUNG_COUNT: u32 = 15;

/// The gaps between [`ZOOM_RUNGS`] the slider's own travel is divided into.
const ZOOM_GAPS: u32 = ZOOM_RUNG_COUNT - 1;

/// The slider control's own full-scale value: it reads its position in parts
/// per thousand of its track.
const SLIDER_FULL: u32 = 1_000;

/// The slider's line step, in parts per thousand of its track: a percent of
/// the travel, which is a fraction of a rung and is what makes the control
/// read as continuous rather than notched.
pub const ZOOM_SLIDER_LINE_STEP: u16 = 10;

/// The slider's page step: one whole rung of the ladder.
///
/// Rounded **up**, because the travel does not divide evenly by the gap
/// count: rounding down would leave each page step a hair short of a rung, so
/// a step from the bottom would land just below the second rung rather than
/// on it, and the whole travel would never quite reach the top. Rounding up
/// overshoots into the next rung's own span instead, which the control's own
/// clamp holds at full scale.
pub const ZOOM_SLIDER_PAGE_STEP: u16 = {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a thousandth part over fourteen gaps, rounded up, is 72 — far inside `u16`"
    )]
    let step = (SLIDER_FULL.div_ceil(ZOOM_GAPS)) as u16;
    step
};

/// The smallest zoom the viewer offers, in parts per thousand.
pub const ZOOM_MIN_PER_MILLE: u32 = ZOOM_RUNGS[0];
/// The largest zoom the viewer offers, in parts per thousand.
pub const ZOOM_MAX_PER_MILLE: u32 = ZOOM_RUNGS[ZOOM_RUNGS.len() - 1];
/// Actual size, in parts per thousand.
pub const ZOOM_ACTUAL_PER_MILLE: u32 = 1_000;

/// How much of one canvas a wheel notch or an arrow key pans, as a
/// percentage: a step small enough to keep one's place and large enough to
/// cross a picture in a few presses.
const PAN_STEP_PERCENT: u32 = 12;

/// What decides the zoom.
///
/// The three fitted modes are recomputed whenever the canvas or the picture
/// changes, because the whole point of a fit is that it holds; [`Free`] is
/// the user's own factor and survives a resize.
///
/// [`Free`]: Fit::Free
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Fit {
    /// The whole picture inside the canvas.
    #[default]
    Window,
    /// The picture's full width across the canvas, however tall that leaves
    /// it.
    Width,
    /// One picture pixel per screen pixel.
    Actual,
    /// Whatever the user set, by slider, wheel, or the zoom tools.
    Free,
}

/// The zoom in parts per thousand a picture of `natural` displayed pixels
/// takes to sit inside `canvas` under `fit`, or `None` for a mode that does
/// not depend on the canvas.
///
/// Held to the ladder's own ends, so a picture beside a one-pixel canvas
/// still has a zoom a render may name.
#[must_use]
pub fn fitted_zoom(fit: Fit, natural: (u32, u32), canvas: (u32, u32)) -> Option<u32> {
    let (width, height) = natural;
    if width == 0 || height == 0 {
        return None;
    }
    let across = |extent: u32, of: u32| -> u32 {
        u64::from(extent)
            .saturating_mul(u64::from(ZOOM_ACTUAL_PER_MILLE))
            .checked_div(u64::from(of))
            .map_or(ZOOM_MAX_PER_MILLE, |per_mille| {
                u32::try_from(per_mille).unwrap_or(ZOOM_MAX_PER_MILLE)
            })
    };
    let per_mille = match fit {
        Fit::Window => across(canvas.0, width).min(across(canvas.1, height)),
        Fit::Width => across(canvas.0, width),
        Fit::Actual => ZOOM_ACTUAL_PER_MILLE,
        Fit::Free => return None,
    };
    Some(per_mille.clamp(ZOOM_MIN_PER_MILLE, ZOOM_MAX_PER_MILLE))
}

/// The zoom in parts per thousand that slider position `at` — itself in
/// parts per thousand of the slider's travel — names.
///
/// Linear between the two rungs the position falls between, so the control is
/// continuous while the ladder keeps it geometric overall. Integer
/// throughout: a viewer's zoom must be reproducible, and no rounding here
/// depends on a float.
#[must_use]
pub fn zoom_at_slider(at: u16) -> u32 {
    let along_travel = u32::from(at).min(SLIDER_FULL) * ZOOM_GAPS;
    let rung = (along_travel / SLIDER_FULL) as usize;
    let Some(&low) = ZOOM_RUNGS.get(rung) else {
        return ZOOM_MAX_PER_MILLE;
    };
    let Some(&high) = ZOOM_RUNGS.get(rung + 1) else {
        return low;
    };
    let along = along_travel % SLIDER_FULL;
    let span = u64::from(high - low).saturating_mul(u64::from(along)) / u64::from(SLIDER_FULL);
    low.saturating_add(u32::try_from(span).unwrap_or(0))
}

/// The slider position that names a zoom nearest `per_mille`.
///
/// The inverse of [`zoom_at_slider`] to within one step of the slider's own
/// resolution, which is what lets a zoom set by the tools, the wheel, or a
/// fit show on the slider without the slider then dragging the zoom
/// somewhere else.
#[must_use]
pub fn slider_at_zoom(per_mille: u32) -> u16 {
    let per_mille = per_mille.clamp(ZOOM_MIN_PER_MILLE, ZOOM_MAX_PER_MILLE);
    for rung in 0..ZOOM_RUNGS.len() - 1 {
        let low = ZOOM_RUNGS[rung];
        let high = ZOOM_RUNGS[rung + 1];
        if per_mille > high {
            continue;
        }
        let along = u64::from(per_mille - low).saturating_mul(u64::from(SLIDER_FULL))
            / u64::from(high - low);
        let position = (u32::try_from(along).unwrap_or(0)
            + u32::try_from(rung).unwrap_or(0) * SLIDER_FULL)
            / ZOOM_GAPS;
        return u16::try_from(position.min(SLIDER_FULL)).unwrap_or(0);
    }
    u16::try_from(SLIDER_FULL).unwrap_or(u16::MAX)
}

/// The rung of [`ZOOM_RUNGS`] immediately above `per_mille`, or the top.
#[must_use]
pub fn zoom_rung_above(per_mille: u32) -> u32 {
    ZOOM_RUNGS
        .iter()
        .copied()
        .find(|rung| *rung > per_mille)
        .unwrap_or(ZOOM_MAX_PER_MILLE)
}

/// The rung of [`ZOOM_RUNGS`] immediately below `per_mille`, or the bottom.
#[must_use]
pub fn zoom_rung_below(per_mille: u32) -> u32 {
    ZOOM_RUNGS
        .iter()
        .copied()
        .rev()
        .find(|rung| *rung < per_mille)
        .unwrap_or(ZOOM_MIN_PER_MILLE)
}

/// Where the viewer is looking, and how.
///
/// Holds no picture and no layout: everything here is the user's intent, so
/// it is the same value whatever the window is doing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Viewport {
    /// What decides the zoom.
    pub fit: Fit,
    /// The zoom in parts per thousand of the displayed picture's natural
    /// size, always inside the ladder's ends.
    zoom: u32,
    /// The top-left of the canvas within the scaled displayed picture, in
    /// display-space pixels. Zero on an axis the picture does not overflow.
    pan: (u32, u32),
    /// How the user has turned or mirrored the picture.
    pub reorient: Reorient,
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new()
    }
}

impl Viewport {
    /// A viewport fitting the whole picture, unturned.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fit: Fit::Window,
            zoom: ZOOM_ACTUAL_PER_MILLE,
            pan: (0, 0),
            reorient: Reorient::None,
        }
    }

    /// The zoom in parts per thousand.
    #[must_use]
    pub const fn zoom(&self) -> u32 {
        self.zoom
    }

    /// The pan offset, in display-space pixels.
    #[must_use]
    pub const fn pan(&self) -> (u32, u32) {
        self.pan
    }

    /// Set the zoom, which makes it the user's own factor.
    ///
    /// The pan is *not* clamped here: it is clamped against the canvas by
    /// [`clamp_pan`](Self::clamp_pan), which is what knows the geometry.
    pub fn set_zoom(&mut self, per_mille: u32) {
        self.fit = Fit::Free;
        self.zoom = per_mille.clamp(ZOOM_MIN_PER_MILLE, ZOOM_MAX_PER_MILLE);
    }

    /// Adopt `fit`, recomputing the factor from the picture and the canvas.
    pub fn set_fit(&mut self, fit: Fit, natural: (u32, u32), canvas: (u32, u32)) {
        self.fit = fit;
        if let Some(per_mille) = fitted_zoom(fit, natural, canvas) {
            self.zoom = per_mille;
        }
    }

    /// Recompute a fitted factor after the picture or the canvas changed.
    ///
    /// A free factor is the user's and is left alone, which is the whole
    /// difference between the two.
    pub fn refit(&mut self, natural: (u32, u32), canvas: (u32, u32)) {
        if let Some(per_mille) = fitted_zoom(self.fit, natural, canvas) {
            self.zoom = per_mille;
        }
    }

    /// The displayed picture's natural size: `natural` set down through the
    /// user's turn.
    #[must_use]
    pub const fn displayed(&self, natural: (u32, u32)) -> (u32, u32) {
        self.reorient.applied_size(natural.0, natural.1)
    }

    /// The scaled displayed picture's extent, in display-space pixels.
    ///
    /// At least one pixel on each axis for a picture that has any, because a
    /// render's extent must be a size something can be a rectangle of.
    #[must_use]
    pub fn scaled(&self, natural: (u32, u32)) -> (u32, u32) {
        let (width, height) = self.displayed(natural);
        (scale_axis(width, self.zoom), scale_axis(height, self.zoom))
    }

    /// The page-space extent a render is asked for: the scaled displayed
    /// extent read back through the user's turn.
    ///
    /// The worker holds the page and knows nothing of the turn, so the
    /// request is in the page's own axes; setting *this* down through the
    /// turn gives exactly [`scaled`](Self::scaled) back, which is what makes
    /// the two spaces agree to the pixel.
    #[must_use]
    pub fn page_extent(&self, natural: (u32, u32)) -> (u32, u32) {
        let (width, height) = self.scaled(natural);
        self.reorient.applied_size(width, height)
    }

    /// The largest zoom, in parts per thousand, at which a picture of
    /// `natural` pixels still has an extent a render may name.
    ///
    /// Held here rather than by clamping each axis on its own, because
    /// clamping the axes independently would change the picture's aspect
    /// ratio — the viewer would silently show a stretched photograph instead
    /// of refusing to magnify further.
    #[must_use]
    pub fn zoom_ceiling(&self, natural: (u32, u32)) -> u32 {
        let (width, height) = self.displayed(natural);
        let ceiling = |extent: u32| -> u32 {
            if extent == 0 {
                return ZOOM_MAX_PER_MILLE;
            }
            let per_mille = u64::from(tairix_raster::MAX_DRAWING_EXTENT)
                .saturating_mul(u64::from(ZOOM_ACTUAL_PER_MILLE))
                / u64::from(extent);
            u32::try_from(per_mille).unwrap_or(ZOOM_MAX_PER_MILLE)
        };
        ceiling(width)
            .min(ceiling(height))
            .clamp(ZOOM_MIN_PER_MILLE, ZOOM_MAX_PER_MILLE)
    }

    /// Hold the zoom to what this picture's extent allows, after a zoom, a
    /// turn, or a page change.
    ///
    /// Answers whether the factor moved, so a caller can state that a
    /// magnification was refused rather than silently not happening.
    pub fn cap_zoom(&mut self, natural: (u32, u32)) -> bool {
        let ceiling = self.zoom_ceiling(natural);
        let capped = self.zoom.min(ceiling);
        let moved = capped != self.zoom;
        self.zoom = capped;
        moved
    }

    /// Move the view by `(dx, dy)` display-space pixels, clamped.
    pub fn pan_by(&mut self, dx: i64, dy: i64, natural: (u32, u32), canvas: (u32, u32)) {
        let scaled = self.scaled(natural);
        self.pan = (
            shift(self.pan.0, dx, pan_limit(scaled.0, canvas.0)),
            shift(self.pan.1, dy, pan_limit(scaled.1, canvas.1)),
        );
    }

    /// Pan by one step of `canvas` in each named direction, as a wheel notch
    /// or an arrow key does.
    pub fn pan_steps(&mut self, dx: i32, dy: i32, natural: (u32, u32), canvas: (u32, u32)) {
        let step =
            |extent: u32| (i64::from(extent.max(1)) * i64::from(PAN_STEP_PERCENT) / 100).max(1);
        self.pan_by(
            i64::from(dx) * step(canvas.0),
            i64::from(dy) * step(canvas.1),
            natural,
            canvas,
        );
    }

    /// Put the view back at the picture's own origin.
    ///
    /// What a page change and a turn do, because an offset into one picture
    /// names nothing in another.
    pub const fn reset_pan(&mut self) {
        self.pan = (0, 0);
    }

    /// Bring the pan back inside what the canvas can reach, after a zoom, a
    /// turn, a resize, or a page change.
    pub fn clamp_pan(&mut self, natural: (u32, u32), canvas: (u32, u32)) {
        let scaled = self.scaled(natural);
        self.pan = (
            self.pan.0.min(pan_limit(scaled.0, canvas.0)),
            self.pan.1.min(pan_limit(scaled.1, canvas.1)),
        );
    }

    /// Whether the scaled picture overflows `canvas` on either axis, which is
    /// what decides whether a scrollbar is drawn and whether a drag pans.
    #[must_use]
    pub fn overflows(&self, natural: (u32, u32), canvas: (u32, u32)) -> (bool, bool) {
        let scaled = self.scaled(natural);
        (scaled.0 > canvas.0, scaled.1 > canvas.1)
    }

    /// Turn the picture a quarter turn, composing onto whatever the user has
    /// already done to it.
    ///
    /// The pan is dropped rather than turned with it: a rotation moves every
    /// part of the picture, so the rectangle the user was looking at is not
    /// somewhere the same offset still names.
    pub fn turn(&mut self, by: Reorient, natural: (u32, u32), canvas: (u32, u32)) {
        self.reorient = self.reorient.then(by);
        self.reset_pan();
        self.refit(natural, canvas);
    }

    /// The rectangle of the scaled displayed picture the canvas shows.
    ///
    /// Centred on an axis the picture does not fill, which is where a viewer
    /// puts a picture smaller than its window.
    #[must_use]
    pub fn visible(&self, natural: (u32, u32), canvas: (u32, u32)) -> Rect {
        let scaled = self.scaled(natural);
        Rect::new(
            tairix_geometry::to_i32(self.pan.0),
            tairix_geometry::to_i32(self.pan.1),
            scaled.0.min(canvas.0),
            scaled.1.min(canvas.1),
        )
    }

    /// Where in the canvas the visible rectangle is drawn: centred on an axis
    /// the picture does not fill.
    #[must_use]
    pub fn placement(&self, natural: (u32, u32), canvas: Rect) -> Rect {
        let visible = self.visible(natural, (canvas.width, canvas.height));
        Rect::new(
            canvas.origin.x
                + tairix_geometry::to_i32(canvas.width.saturating_sub(visible.width) / 2),
            canvas.origin.y
                + tairix_geometry::to_i32(canvas.height.saturating_sub(visible.height) / 2),
            visible.width,
            visible.height,
        )
    }
}

/// One axis scaled by a zoom in parts per thousand, held to at least one
/// pixel for an axis that has any and to what a render may name.
fn scale_axis(extent: u32, zoom: u32) -> u32 {
    if extent == 0 {
        return 0;
    }
    let scaled =
        u64::from(extent).saturating_mul(u64::from(zoom)) / u64::from(ZOOM_ACTUAL_PER_MILLE);
    u32::try_from(scaled)
        .unwrap_or(tairix_raster::MAX_DRAWING_EXTENT)
        .clamp(1, tairix_raster::MAX_DRAWING_EXTENT)
}

/// How far the pan may reach on one axis: what the scaled picture has that
/// the canvas cannot show at once.
fn pan_limit(scaled: u32, canvas: u32) -> u32 {
    scaled.saturating_sub(canvas)
}

/// `from` moved by `delta`, held to `0..=limit` without wrapping either end.
fn shift(from: u32, delta: i64, limit: u32) -> u32 {
    let moved = i64::from(from).saturating_add(delta);
    u32::try_from(moved.clamp(0, i64::from(limit))).unwrap_or(limit)
}

/// One document the viewer has open: what the container declares, which entry
/// the viewer is showing, which entry the worker holds decoded, and what the
/// file itself is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    /// What the container declares about its entries as a whole.
    pub info: ViewDocument,
    /// The entry the viewer is showing.
    ///
    /// Held apart from [`page`](Self::page) because the two are genuinely
    /// different facts: the moment the user turns a page, this is the new
    /// entry and that is still the old one. Deriving the selection *from* the
    /// decoded entry would lose it the instant the old picture was dropped,
    /// and the render asked for would be the page the user just left.
    selected: u32,
    /// The entry the worker currently holds decoded, once one does.
    pub page: Option<ViewPage>,
    /// The document's own file name, for the title and the info panel.
    pub name: String,
    /// The document's length in bytes, for the info panel.
    pub bytes: u64,
}

impl Document {
    /// A document just opened, showing its first entry, with nothing decoded.
    #[must_use]
    pub const fn new(info: ViewDocument, name: String, bytes: u64) -> Self {
        Self {
            info,
            selected: 0,
            page: None,
            name,
            bytes,
        }
    }

    /// The entry the viewer is showing.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.selected
    }

    /// Show the entry at `index`, reporting whether the selection moved.
    ///
    /// The decoded entry is *not* dropped: a viewer never blanks, so what is
    /// on screen stays there until the new entry arrives. What tells the two
    /// apart is [`decoded`](Self::decoded).
    pub fn select(&mut self, index: u32) -> bool {
        if index >= self.info.count || index == self.selected {
            return false;
        }
        self.selected = index;
        true
    }

    /// Whether the entry the worker holds decoded is the one being shown.
    #[must_use]
    pub fn decoded(&self) -> bool {
        self.page.is_some_and(|page| page.index == self.selected)
    }

    /// The shown entry's natural pixel size, or the container's declared
    /// canvas until that entry is the one decoded.
    ///
    /// A page container's pages differ in size, so the container's figure is
    /// its largest — right for reserving a canvas, wrong for the page on
    /// screen, which is why the decoded entry wins as soon as it is the one
    /// being shown.
    #[must_use]
    pub fn natural(&self) -> (u32, u32) {
        self.page
            .filter(|page| page.index == self.selected)
            .map_or((self.info.width, self.info.height), |page| {
                (page.width, page.height)
            })
    }

    /// The format the container's own header identified.
    #[must_use]
    pub const fn format(&self) -> ViewFormat {
        self.info.format
    }
}

/// What the engine wants carried out, and `Run` performs on its worker.
///
/// Latest-wins on the shared desk, which is exactly right for both: a render
/// superseded by a newer zoom or pan is one nobody wants drawn, and a
/// [`Show`](Request::Show) is never submitted before an
/// [`Open`](Request::Open) has been answered, so an open is never displaced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Read the document the program was given and open it.
    Open,
    /// Bring the session to `page`, then draw `window` of the page scaled to
    /// `extent` — both in **page** space.
    ///
    /// `pixels` is the buffer the answer comes back in, handed over so the
    /// worker draws into a buffer the loop already owns: a window's worth of
    /// pixels is megabytes, and allocating that per pointer sample is what
    /// recycling it removes.
    Show {
        /// The entry to hold decoded.
        page: u32,
        /// The extent the whole page is scaled to, in page space.
        extent: (u32, u32),
        /// The rectangle of that scaling to draw, in page space.
        window: Rect,
        /// The buffer to draw into, resized by the worker as needed.
        pixels: Vec<u8>,
    },
}

/// What came back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Answer {
    /// The document opened, or would not.
    Opened {
        /// What the container declares, the file's name (empty when the
        /// embedder was never told one) and its length.
        opened: Result<(ViewDocument, String, u64), view::Refusal>,
    },
    /// A draw was carried out, or refused. The buffer comes back either way,
    /// so a refusal costs the next render no allocation.
    Shown {
        /// The entry the worker was asked to hold.
        page: u32,
        /// The extent asked for, echoed so a superseded answer is spotted.
        extent: (u32, u32),
        /// The window asked for, echoed for the same reason.
        window: Rect,
        /// The entry the worker actually decoded, when it did.
        decoded: Option<ViewPage>,
        /// The pixels, straight-alpha RGBA8, when the draw succeeded.
        pixels: Vec<u8>,
        /// Whether the draw happened, and why not when it did not.
        outcome: Result<(), view::Refusal>,
    },
}

/// The display-space picture the canvas draws, and the buffer it was
/// collected in.
///
/// Two surfaces rather than one because a turn that swaps the axes cannot be
/// done in place: `staging` is what the worker's pixels are written into, in
/// page space, and `shown` is that set down for display. An unturned picture
/// needs no second surface at all, which is the common case and pays nothing.
#[derive(Debug, Default)]
pub(crate) struct Picture {
    /// The window drawn, in page space, matching `staging`'s geometry.
    pub(crate) window: Rect,
    /// The extent the page was scaled to when it was drawn.
    pub(crate) extent: (u32, u32),
    /// The turn `shown` was produced under.
    pub(crate) reorient: Reorient,
    /// The worker's pixels, in page space.
    pub(crate) staging: Option<Surface>,
    /// `staging` set down for display; absent when the turn is the identity.
    pub(crate) shown: Option<Surface>,
}

impl Picture {
    /// The surface the canvas draws, or `None` before anything has arrived.
    pub(crate) fn surface(&self) -> Option<&Surface> {
        self.shown.as_ref().or(self.staging.as_ref())
    }

    /// Adopt `pixels` as the picture of `window` of the page scaled to
    /// `extent`, set down `reorient`.
    ///
    /// Answers `false` — changing nothing — when the pixels do not describe
    /// the window, or a surface could not be allocated. A viewer then keeps
    /// showing what it had and states the refusal, rather than blanking.
    pub(crate) fn adopt(
        &mut self,
        window: Rect,
        extent: (u32, u32),
        reorient: Reorient,
        pixels: &[u8],
    ) -> bool {
        if !fits(window, reorient, pixels.len()) {
            return false;
        }
        let Some(staging) = held_or_fresh(&mut self.staging, window.width, window.height) else {
            return false;
        };
        if !staging.write_rgba8(pixels) {
            return false;
        }
        if reorient == Reorient::None {
            self.shown = None;
        } else {
            let (width, height) = reorient.applied_size(window.width, window.height);
            // Two surfaces of the same geometry, so the borrow of one cannot
            // be the borrow of the other: taking `staging` out for the turn
            // is what lets both be `&mut` at once, and it goes straight back.
            let Some(source) = self.staging.take() else {
                return false;
            };
            let turned = held_or_fresh(&mut self.shown, width, height)
                .is_some_and(|shown| source.reorient_into(shown, reorient));
            self.staging = Some(source);
            if !turned {
                return false;
            }
        }
        self.window = window;
        self.extent = extent;
        self.reorient = reorient;
        true
    }
}

/// The surface `slot` holds when it is already `width`×`height`, or a fresh
/// one of that geometry put there.
///
/// A viewer panning at a fixed zoom asks for the same geometry every sample,
/// so the common case allocates nothing.
fn held_or_fresh(slot: &mut Option<Surface>, width: u32, height: u32) -> Option<&mut Surface> {
    let reusable = slot
        .as_ref()
        .is_some_and(|held| held.width() == width && held.height() == height);
    if !reusable {
        *slot = Some(Surface::new(width, height)?);
    }
    slot.as_mut()
}

/// Whether `len` bytes are exactly the straight-alpha pixels of `window`,
/// and the turn's own destination is a size a surface can hold.
fn fits(window: Rect, reorient: Reorient, len: usize) -> bool {
    let turned = reorient.applied_size(window.width, window.height);
    usize::try_from(window.width)
        .ok()
        .and_then(|w| w.checked_mul(usize::try_from(window.height).ok()?))
        .and_then(|pixels| pixels.checked_mul(4))
        .is_some_and(|expected| expected == len)
        && turned.0 != 0
        && turned.1 != 0
}

/// The page-space rectangle that `display` of a `grid`-pixel **display**
/// grid names, under `reorient`.
///
/// `grid` is the scaled *displayed* extent — [`Viewport::scaled`] — because
/// the undoing turn reads the display grid as its source. Reached through the
/// one position map [`Reorient`] already carries rather than a second piece
/// of orientation arithmetic, so the rectangle the worker is asked for and
/// the pixels the turn then produces cannot disagree.
#[must_use]
pub fn window_in_page_space(display: Rect, grid: (u32, u32), reorient: Reorient) -> Rect {
    if display.width == 0 || display.height == 0 {
        return Rect::EMPTY;
    }
    let back = reorient.inverse();
    let far = (
        display.width.saturating_sub(1),
        display.height.saturating_sub(1),
    );
    let left = u32::try_from(display.origin.x.max(0)).unwrap_or(0);
    let top = u32::try_from(display.origin.y.max(0)).unwrap_or(0);
    let near = back.place(left, top, grid.0, grid.1);
    let away = back.place(
        left.saturating_add(far.0),
        top.saturating_add(far.1),
        grid.0,
        grid.1,
    );
    let (x0, x1) = (near.0.min(away.0), near.0.max(away.0));
    let (y0, y1) = (near.1.min(away.1), near.1.max(away.1));
    Rect::new(
        tairix_geometry::to_i32(x0),
        tairix_geometry::to_i32(y0),
        x1 - x0 + 1,
        y1 - y0 + 1,
    )
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
