//! The [`Chart`] control: one bounded history series — optionally two
//! opposing ones — plotted as a line over the box it is given
//! (`plans/GUI-CONTROLS-DESIGN.md` §11.35).
//!
//! A chart is a read-only instrument like [`Progress`](crate::Progress) and a
//! [`MetricTile`](crate::MetricTile)'s embedded proportional track: it carries
//! a series of readings and never accepts pointer or keyboard input. What
//! distinguishes it from a track is *what it says*. A track answers "how much
//! of this resource is in use right now" and draws one thin proportional
//! line; a chart answers "what has this resource been doing", and a trend
//! needs vertical room to be legible — a series squeezed into a track's
//! instrument groove cannot rise more than a pixel or two whatever its values
//! are, which is a graph that cannot report its own data. The chart therefore
//! claims the whole rectangle its owner lays out for it, and its readings map
//! across that full height.
//!
//! A read/write or receive/send rate is *one* reading with two directions, so
//! a chart takes an optional opposing series
//! ([`with_opposing`](Chart::with_opposing)): the box splits at a drawn axis,
//! the primary series rises above it and the opposing one mirrors below,
//! tinted by its own role. Two stacked charts would lose the comparison that
//! matters.
//!
//! A series is tinted by a [`SignalRole`] rather than the accent, so a CPU
//! trace reads as the compute colour whether it is showing 5% or 95%, and the
//! accent stays reserved for a chosen action. The role — not a resource
//! pressure — is what a series carries, because the two halves of a duplex
//! trace are *directions*: reads against writes, receive against send. Giving
//! both the device's own hue drew one reading in one colour and said nothing
//! about which way the bytes went. A resource-identity chart names
//! `kind.signal_role()` and reads exactly as before.
//!
//! Every colour, radius, and thickness resolves from the active [`Theme`] and
//! [`Scale`] through the shared accessors (`crate::paint`), and the line itself
//! is drawn by the one shared stroke path in `lib/raster` — the same primitive
//! a window-furniture diagonal uses — so no second stroke geometry exists here.

use alloc::vec::Vec;

use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface, SUBPIXEL};
use tairix_theme::{SignalRole, Theme};

use crate::paint::{plate_border, role_color, seam_thickness, surface_rect, withheld, FULL};

/// The most samples a [`Chart`] may hold.
///
/// A chart plots one line segment per adjacent pair, so a series far beyond a
/// small window would spend segments on detail finer than a pixel. Bounding it
/// here keeps the history an owner-controlled window rather than an unbounded
/// log the render path would have to walk past, and keeps the series small
/// enough to hold inline in a model that must not allocate.
pub const MAX_CHART_SAMPLES: usize = 64;

/// How much of the trace's own colour the area beneath it carries *at the
/// band's full-scale edge*, fading to nothing at the zero line.
///
/// The filled area gives the trace a body, so a low-amplitude series still
/// reads as a shape rather than a wandering hairline. Filling it flat instead
/// draws the zero line as a second hard edge across the box, which reads as a
/// measurement the chart never took; ramping it out means the only edges the
/// eye finds are the trace and the axis.
///
/// The ramp's mean is half its peak, so this is twice the weight a flat fill
/// would carry: the same ink, redistributed toward the trace rather than
/// spread evenly down to the floor. It stays below the line's own weight so
/// the line remains the thing being read, and so anything drawn behind the
/// chart still shows through.
const AREA_ALPHA: u8 = 128;

/// Full strength of the area fill's vertical ramp.
const AREA_RAMP_FULL: u32 = 255;

/// A chart's second, mirrored series and the role it reads as.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Opposing {
    role: SignalRole,
    samples: Vec<u16>,
}

/// One bounded, oldest-to-newest history series, plotted as a line with a
/// quiet filled area beneath it, tinted by its semantic signal colour
/// (spec §11.35).
///
/// Readings are permille of the resource's capacity by default; a series with
/// no such ceiling — a count — states its own with
/// [`with_full_scale`](Self::with_full_scale).
///
/// The owner supplies every visible fact — the signal role and the series —
/// and re-renders when either changes. A chart with no samples draws *nothing*:
/// an honest "nothing recorded yet" leaving the plate it sits on untouched,
/// never a fabricated flat line along the floor, which would read as a measured
/// idle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chart {
    role: SignalRole,
    samples: Vec<u16>,
    opposing: Option<Opposing>,
    full_scale: u16,
}

impl Chart {
    /// An empty chart whose series reads as `role`. Add readings with
    /// [`with_samples`](Self::with_samples).
    ///
    /// A chart of a resource's own activity passes
    /// [`PressureKind::signal_role`](crate::PressureKind::signal_role); one of
    /// something that is not a resource pressure — a task census, a transfer
    /// direction — names its role directly.
    #[must_use]
    pub fn new(role: SignalRole) -> Self {
        Self {
            role,
            samples: Vec::new(),
            opposing: None,
            full_scale: FULL,
        }
    }

    /// This chart reading its series against `full_scale` rather than against
    /// a permille capacity.
    ///
    /// A count has no permille ceiling of its own, so a caller plotting one
    /// states the denominator it means: the box's top edge is `full_scale`,
    /// and a sample at or above it fills the box. A zero scale would divide by
    /// nothing, so it falls back to the permille default rather than failing
    /// the draw.
    #[must_use]
    pub fn with_full_scale(mut self, full_scale: u16) -> Self {
        self.full_scale = if full_scale == 0 { FULL } else { full_scale };
        self
    }

    /// This chart with an oldest-to-newest series, read against the chart's
    /// scale — a permille fraction of the resource's capacity unless
    /// [`with_full_scale`](Self::with_full_scale) states another ceiling. A
    /// reading at or above that ceiling fills the box, clamped fail closed;
    /// the series is capped to the most recent [`MAX_CHART_SAMPLES`], dropping
    /// the oldest first.
    #[must_use]
    pub fn with_samples(mut self, samples: impl IntoIterator<Item = u16>) -> Self {
        self.samples = bounded(samples);
        self
    }

    /// This chart with a second series reading as `role`, plotted mirrored
    /// below a drawn axis and tinted by that role's own colour; the primary
    /// series then rises above the axis instead of filling the box.
    ///
    /// The two roles are what make a duplex trace readable: pass the
    /// *direction* each half measures — read against write, receive against
    /// send — so a glance says which way the bytes went. Passing one role
    /// twice draws one reading in one colour and says nothing.
    ///
    /// The series is bounded and clamped exactly as
    /// [`with_samples`](Self::with_samples)'s is. Adding one asserts that the
    /// opposing direction is *measured*: a direction with no reading behind it
    /// is left off, so the chart stays a single-series trend over the whole box
    /// rather than showing an empty half as a quiet nothing.
    #[must_use]
    pub fn with_opposing(
        mut self,
        role: SignalRole,
        samples: impl IntoIterator<Item = u16>,
    ) -> Self {
        self.opposing = Some(Opposing {
            role,
            samples: bounded(samples),
        });
        self
    }

    /// Whether the chart holds no readings in either series, and so plots
    /// nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
            && self
                .opposing
                .as_ref()
                .is_none_or(|opposing| opposing.samples.is_empty())
    }

    /// Paint the chart into `surface` at `bounds` for the active theme.
    ///
    /// A single-series trace occupies the full height of `bounds`: a reading of
    /// zero sits on the floor and a reading of full capacity on the ceiling,
    /// both inset by half the line's weight so the stroke stays inside its own
    /// box. A chart with an opposing series splits that height at its axis and
    /// each series claims one half, mirrored.
    ///
    /// The chart lays down no ground of its own: it draws its trace onto
    /// whatever surface it was given, so the box reads as part of the plate it
    /// sits on rather than as a panel cut into it. An empty chart therefore
    /// draws nothing at all, which is the honest picture of a reading with no
    /// history behind it — an axis on a ground of its own would read as a
    /// measured nought.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        if withheld(surface, bounds) {
            return;
        }
        let Some(plot_box) = surface_rect(bounds) else {
            return;
        };
        let (_, _, width, height) = plot_box;
        if width == 0 || height == 0 {
            return;
        }

        let Some(opposing) = &self.opposing else {
            let weight = trace_weight(theme, scale, height);
            let band = Band {
                box_px: plot_box,
                rising_up: true,
            };
            paint_series(
                surface,
                &self.samples,
                &band,
                weight,
                self.role,
                theme,
                self.full_scale,
            );
            return;
        };

        // Nothing recorded in either direction: no axis, so a rule across the
        // box is never mistaken for a measured nought.
        if self.is_empty() {
            return;
        }
        let Some(split) = Split::resolve(plot_box, scale, theme) else {
            return;
        };
        let (ax, ay, aw, ah) = split.axis;
        surface.fill_rect(ax, ay, aw, ah, Color::from(theme.palette().rim));
        paint_series(
            surface,
            &self.samples,
            &split.upper,
            split.weight,
            self.role,
            theme,
            self.full_scale,
        );
        paint_series(
            surface,
            &opposing.samples,
            &split.lower,
            split.weight,
            opposing.role,
            theme,
            self.full_scale,
        );
    }
}

/// `samples` capped to the most recent [`MAX_CHART_SAMPLES`], oldest dropped
/// first — the one admission rule both of a chart's series pass through.
///
/// Readings are kept as the caller stated them; the chart's own `full_scale`
/// is what maps them into the box, so a count survives ingest intact.
fn bounded(samples: impl IntoIterator<Item = u16>) -> Vec<u16> {
    let mut buf: Vec<u16> = samples.into_iter().collect();
    if buf.len() > MAX_CHART_SAMPLES {
        let drop = buf.len() - MAX_CHART_SAMPLES;
        buf.drain(..drop);
    }
    buf
}

/// One series' own half of a chart: the pixel box its readings map into, and
/// which way a rising reading grows.
///
/// A single-series chart is the whole box rising up; an opposing series is the
/// lower half growing down. One band definition serves both, so the mirrored
/// series is the same plot arithmetic rather than a second recipe.
struct Band {
    box_px: (u32, u32, u32, u32),
    rising_up: bool,
}

/// The two bands and the axis rule a duplex chart's box splits into.
struct Split {
    upper: Band,
    lower: Band,
    /// The axis's pixel rect, drawn between the bands as the zero line both
    /// series read against.
    axis: (u32, u32, u32, u32),
    /// One trace weight for both bands, so the pair reads as one instrument
    /// even when the rounding remainder makes the halves differ by a pixel.
    weight: i32,
}

impl Split {
    /// Split `box_px` into an upper band, an axis, and a lower band, or `None`
    /// when the box is too short to seat all three — where the honest outcome
    /// is to draw nothing rather than a half-drawn pair.
    fn resolve(box_px: (u32, u32, u32, u32), scale: Scale, theme: &Theme) -> Option<Self> {
        let (left, top, width, height) = box_px;
        let axis_h = plate_border(theme, scale);
        let bands_h = height.checked_sub(axis_h)?;
        if bands_h < 2 {
            return None;
        }
        let upper_h = bands_h / 2;
        let lower_h = bands_h - upper_h;
        let axis_top = top.saturating_add(upper_h);
        Some(Self {
            upper: Band {
                box_px: (left, top, width, upper_h),
                rising_up: true,
            },
            lower: Band {
                box_px: (left, axis_top.saturating_add(axis_h), width, lower_h),
                rising_up: false,
            },
            axis: (left, axis_top, width, axis_h),
            weight: trace_weight(theme, scale, upper_h.min(lower_h)),
        })
    }
}

/// Plot `samples` into `band` — the filled area first, then the line over its
/// own body — tinted by `role`'s colour. An empty series draws nothing.
fn paint_series(
    surface: &mut Surface,
    samples: &[u16],
    band: &Band,
    weight: i32,
    role: SignalRole,
    theme: &Theme,
    full_scale: u16,
) {
    let Some(poly) = plot(samples, band, weight, full_scale) else {
        return;
    };
    // The filled area is the whole polygon and the trace is its interior, so
    // the stroke reads the same vertices rather than a second copy of them.
    let Some(trace) = poly.len().checked_sub(1).and_then(|end| poly.get(1..end)) else {
        return;
    };
    let color = role_color(theme, role);
    let (_, top, _, height) = band.box_px;
    let rising_up = band.rising_up;
    surface.wash_polygon_subpixel(
        &poly,
        Color {
            a: AREA_ALPHA,
            ..color
        },
        |_, y| area_ramp(y, top, height, rising_up),
    );
    surface.stroke_polyline(trace, weight, color);
}

/// How much of the area fill's opacity surface row `y` carries, for a band
/// `height` rows tall starting at `band_top`: full at the edge a rising
/// reading grows toward, nothing at the zero line it is read against.
///
/// The ramp is the band's, not the trace's, so the fill's weight at a given
/// height means the same thing whatever the reading happens to be there —
/// which is what lets a reader compare two columns of one chart, or the same
/// row of two charts, by eye. A mirrored opposing band grows the other way and
/// so ramps the other way.
fn area_ramp(y: u32, band_top: u32, height: u32, rising_up: bool) -> u8 {
    let Some(last) = height.checked_sub(1) else {
        return 0;
    };
    if last == 0 {
        return u8::try_from(AREA_RAMP_FULL).unwrap_or(u8::MAX);
    }
    let row = y.saturating_sub(band_top).min(last);
    let reach = if rising_up { last - row } else { row };
    u8::try_from(AREA_RAMP_FULL.saturating_mul(reach) / last).unwrap_or(u8::MAX)
}

/// `samples` as one closed polygon in device sub-pixel units: the band's own
/// edge against the zero line, then the trace vertices, then that edge again,
/// so the fill is the whole vector and the trace is `[1 .. len - 1]`. One
/// vector serves both, because a chart draws the same shape twice.
/// `None` when there is nothing to plot.
///
/// **The box is a fixed window and the newest reading is pinned to its
/// trailing edge**, one slot per sample whatever the series holds. So a series
/// shorter than the window draws a trace that reaches back as far as the
/// readings genuinely go and no further, and each new sample slides the shape
/// left by exactly one slot. Spreading `count` readings across the whole box
/// instead made the trace rewrite its own shape on every sample — the same
/// history redrawn at a different scale — and claimed a minute's span for
/// three seconds of readings.
fn plot(samples: &[u16], band: &Band, weight: i32, full_scale: u16) -> Option<Vec<(i32, i32)>> {
    let count = samples.len();
    if count == 0 {
        return None;
    }
    let (left_px, top_px, width, height) = band.box_px;
    if width == 0 || height == 0 {
        return None;
    }
    let inset = weight / 2;
    // The band the trace's own centre may occupy: the box less the room the
    // stroke needs on each side, so a full-scale reading draws inside its
    // rectangle instead of half outside it.
    let span_x = sub(width).saturating_sub(weight);
    let span_y = sub(height).saturating_sub(weight);
    let left = sub(left_px).saturating_add(inset);
    let top = sub(top_px).saturating_add(inset);
    let (zero, rise_sign, close) = if band.rising_up {
        (
            top.saturating_add(span_y),
            -1,
            sub(top_px.saturating_add(height)),
        )
    } else {
        (top, 1, sub(top_px))
    };

    // The window is `MAX_CHART_SAMPLES` slots wide however few readings there
    // are, so `reach` is the share of the box this series genuinely covers and
    // the newest reading sits at the trailing edge. One slot is the floor: a
    // reading's mark is never thinner than the line drawing it, so a box too
    // narrow to resolve one slot still shows its newest reading rather than
    // collapsing it to a zero-width segment and dropping it.
    let slots = i32::try_from(MAX_CHART_SAMPLES.saturating_sub(1))
        .unwrap_or(1)
        .max(1);
    let covered = i32::try_from(count.saturating_sub(1)).unwrap_or(slots);
    let reach = (span_x.saturating_mul(covered) / slots).max(weight.min(span_x));
    let right = left.saturating_add(span_x);
    let start = right.saturating_sub(reach);
    let at = |i: i32| match covered {
        0 => start,
        span => start.saturating_add(reach.saturating_mul(i) / span),
    };
    // The box's top edge is `full_scale`, so a reading at or above it fills the
    // band: the permille default is just the case where that ceiling is FULL.
    let ceiling = i32::from(full_scale).max(1);
    let rise_at = |reading: u16| {
        let rise = span_y.saturating_mul(i32::from(reading).min(ceiling)) / ceiling;
        zero.saturating_add(rise.saturating_mul(rise_sign))
    };
    // Closed at both ends in place, and a lone reading holds its slot flat:
    // two vertices at the same height rather than an invisible point.
    let mut points = Vec::with_capacity(count + 3);
    points.push((start, close));
    for (i, &reading) in samples.iter().enumerate() {
        points.push((at(i32::try_from(i).unwrap_or(covered)), rise_at(reading)));
    }
    if covered == 0 {
        if let Some(&only) = samples.first() {
            points.push((right, rise_at(only)));
        }
    }
    points.push((right, close));
    Some(points)
}

/// The trace's line weight in sub-pixel units: the theme's instrument-seam
/// thickness, never thicker than a third of the band (so a short chart keeps
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
