//! The [`Chart`] control: one bounded history series, plotted as a line over
//! the box it is given (`plans/GUI-CONTROLS-DESIGN.md` §11.35).
//!
//! A chart is a read-only instrument like [`Meter`](crate::Meter) and
//! [`Progress`](crate::Progress): it carries a series of readings and never
//! accepts pointer or keyboard input. What distinguishes it from a meter is
//! *what it says*. A meter answers "how much of this resource is in use right
//! now" and draws one thin proportional track; a chart answers "what has this
//! resource been doing", and a trend needs vertical room to be legible — a
//! series squeezed into a meter's instrument groove cannot rise more than a
//! pixel or two whatever its values are, which is a graph that cannot report
//! its own data. The chart therefore claims the whole rectangle its owner lays
//! out for it, and its readings map across that full height.
//!
//! Like a meter's track, the trace is tinted by the resource's own semantic
//! rail colour rather than the accent, so a CPU trace reads as the compute
//! colour whether it is showing 5% or 95%, and the accent stays reserved for a
//! chosen action.
//!
//! Every colour, radius, and thickness resolves from the active [`Theme`] and
//! [`Scale`] through the shared accessors (`crate::paint`), and the line itself
//! is drawn by the one shared stroke path in `lib/raster` — the same primitive
//! a window-furniture diagonal uses — so no second stroke geometry exists here.

use alloc::vec::Vec;

use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface, SUBPIXEL};
use tairix_theme::Theme;

use crate::paint::{clamp_permille, seam_thickness, signal_color, surface_rect, FULL};
use crate::state::PressureKind;

/// The most samples a [`Chart`] may hold.
///
/// A chart plots one line segment per adjacent pair, so a series far beyond a
/// small window would spend segments on detail finer than a pixel. Bounding it
/// here keeps the history an owner-controlled window rather than an unbounded
/// log the render path would have to walk past, and keeps the series small
/// enough to hold inline in a model that must not allocate.
pub const MAX_CHART_SAMPLES: usize = 64;

/// How much of the trace's own colour the area beneath it carries.
///
/// The filled area gives the trace a body, so a low-amplitude series still
/// reads as a shape rather than a wandering hairline. It stays well below the
/// line's own weight so the line remains the thing being read, and so anything
/// drawn behind the chart still shows through.
const AREA_ALPHA: u8 = 64;

/// One bounded, oldest-to-newest history series, plotted as a line with a
/// quiet filled area beneath it, tinted by its resource's semantic rail colour
/// (spec §11.35).
///
/// The owner supplies every visible fact — the resource kind and the series —
/// and re-renders when either changes. A chart with no samples draws only its
/// quiet plate: an honest "nothing recorded yet", never a fabricated flat line
/// along the floor, which would read as a measured idle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chart {
    kind: PressureKind,
    samples: Vec<u16>,
}

impl Chart {
    /// An empty chart for the resource `kind`. Add readings with
    /// [`with_samples`](Self::with_samples).
    #[must_use]
    pub fn new(kind: PressureKind) -> Self {
        Self {
            kind,
            samples: Vec::new(),
        }
    }

    /// This chart with an oldest-to-newest series. Each sample is a permille
    /// fraction of the resource's capacity, clamped fail closed; the series is
    /// capped to the most recent [`MAX_CHART_SAMPLES`], dropping the oldest
    /// first.
    #[must_use]
    pub fn with_samples(mut self, samples: impl IntoIterator<Item = u16>) -> Self {
        let mut buf: Vec<u16> = samples.into_iter().map(clamp_permille).collect();
        if buf.len() > MAX_CHART_SAMPLES {
            let drop = buf.len() - MAX_CHART_SAMPLES;
            buf.drain(..drop);
        }
        self.samples = buf;
        self
    }

    /// Whether the chart holds no readings, and so plots nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Paint the chart into `surface` at `bounds` for the active theme.
    ///
    /// The trace occupies the full height of `bounds`: a reading of zero sits
    /// on the floor and a reading of full capacity on the ceiling, both inset
    /// by half the line's weight so the stroke stays inside its own box.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let Some(plot_box) = surface_rect(bounds) else {
            return;
        };
        let (left, top, width, height) = plot_box;
        if width == 0 || height == 0 {
            return;
        }
        // The plot's floor and ceiling are straight: a rounded plate would cut
        // the corners off the very readings the chart exists to show.
        surface.fill_rect(
            left,
            top,
            width,
            height,
            Color::from(theme.palette().scroll_track),
        );

        let weight = trace_weight(theme, scale, height);
        let Some(points) = self.plot(plot_box, weight) else {
            // No readings: the quiet plate alone, never a fabricated floor.
            return;
        };
        let color = signal_color(theme, self.kind);

        // The area first, so the line reads over its own body rather than
        // under it.
        let floor = sub(top.saturating_add(height));
        let mut area: Vec<(i32, i32)> = Vec::with_capacity(points.len() + 2);
        area.push((points[0].0, floor));
        area.extend_from_slice(&points);
        area.push((points[points.len() - 1].0, floor));
        surface.fill_polygon_subpixel(
            &area,
            Color {
                a: AREA_ALPHA,
                ..color
            },
        );

        surface.stroke_polyline(&points, weight, color);
    }

    /// The trace's vertices, in device sub-pixel units, for the pixel box
    /// `plot_box` and a trace `weight` sub-pixel units wide; `None` when there
    /// is nothing to plot.
    ///
    /// A single reading is a real measurement, so it plots as a flat line
    /// across the box at its own height rather than as an invisible point.
    fn plot(&self, plot_box: (u32, u32, u32, u32), weight: i32) -> Option<Vec<(i32, i32)>> {
        let count = self.samples.len();
        if count == 0 {
            return None;
        }
        let (left_px, top_px, width, height) = plot_box;
        let inset = weight / 2;
        // The band the trace's own centre may occupy: the box less the room the
        // stroke needs on each side, so a full-scale reading draws inside its
        // rectangle instead of half outside it.
        let span_x = sub(width).saturating_sub(weight);
        let span_y = sub(height).saturating_sub(weight);
        let left = sub(left_px).saturating_add(inset);
        let top = sub(top_px).saturating_add(inset);

        let last = count.saturating_sub(1);
        let mut points = Vec::with_capacity(count.max(2));
        for (i, &permille) in self.samples.iter().enumerate() {
            let along = if last == 0 {
                0
            } else {
                let i = i32::try_from(i).unwrap_or(i32::MAX);
                let last = i32::try_from(last).unwrap_or(1);
                span_x.saturating_mul(i) / last
            };
            let rise =
                span_y.saturating_mul(i32::from(clamp_permille(permille))) / i32::from(FULL).max(1);
            points.push((
                left.saturating_add(along),
                top.saturating_add(span_y).saturating_sub(rise),
            ));
        }
        if last == 0 {
            // One reading: hold it across the whole box.
            let (_, only) = points[0];
            points.push((left.saturating_add(span_x), only));
        }
        Some(points)
    }
}

/// The trace's line weight in sub-pixel units: the theme's instrument-seam
/// thickness, never thicker than a third of the box (so a short chart keeps
/// room to show a shape) and never thinner than one whole pixel.
fn trace_weight(theme: &Theme, scale: Scale, h: u32) -> i32 {
    let seam = seam_thickness(theme, scale).min(h / 3).max(1);
    sub(seam)
}

/// `px` whole pixels as device sub-pixel units.
fn sub(px: u32) -> i32 {
    i32::try_from(px)
        .unwrap_or(i32::MAX / SUBPIXEL)
        .saturating_mul(SUBPIXEL)
}
