//! Unit tests for the [`Chart`] history instrument (spec §11.35).
//!
//! These cover the property the control exists for — a reading maps across the
//! *whole* height of the box, so a full-scale series reaches its ceiling and a
//! zero series its floor — plus the bounded series (empty, one, many, capped,
//! clamped), oldest-to-newest ordering, each [`PressureKind`]'s own rail
//! colour, dark/light coverage, and fail-closed behaviour in a degenerate box.
//!
//! The opposing series has the same coverage on its own terms: the axis splits
//! the box, each direction stays in its own half, the mirrored series grows the
//! other way, the two carry their own tints, and a box too short to seat both
//! halves degrades to the quiet plate.

use alloc::vec::Vec;

use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::Theme;

use crate::chart::{Chart, MAX_CHART_SAMPLES};
use crate::state::PressureKind;

const W: u32 = 96;
const H: u32 = 40;

fn premul(rgba: tairix_theme::Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn chart_surface_of(chart: &Chart, theme: &Theme, w: u32, h: u32) -> Surface {
    let mut surface = Surface::new(w, h).expect("surface");
    chart.render(&mut surface, Rect::new(0, 0, w, h), Scale::ONE, theme);
    surface
}

fn chart_surface(chart: &Chart, theme: &Theme) -> Surface {
    chart_surface_of(chart, theme, W, H)
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

/// Whether row `y` carries `want` anywhere across its width.
fn row_has(surface: &Surface, y: u32, want: Pixel) -> bool {
    (0..surface.width()).any(|x| surface.get(x, y) == Some(want))
}

/// The topmost row carrying `want` within columns `[from, to)`, if any.
fn topmost_in(surface: &Surface, from: u32, to: u32, want: Pixel) -> Option<u32> {
    (0..surface.height()).find(|&y| (from..to).any(|x| surface.get(x, y) == Some(want)))
}

/// The topmost row carrying `want` anywhere, if any.
fn topmost(surface: &Surface, want: Pixel) -> Option<u32> {
    topmost_in(surface, 0, surface.width(), want)
}

fn cpu(theme: &Theme) -> Pixel {
    premul(theme.palette().cpu_pressure)
}

// --- The reading maps across the whole box --------------------------------

#[test]
fn a_full_scale_series_reaches_the_ceiling_and_a_zero_series_the_floor() {
    // The defect this control replaced confined the plot to a progress
    // track's own thickness, so a series could never rise above a pixel or
    // two whatever it read. A chart owns its box: full capacity is the top
    // row and nothing at all is the bottom one.
    let theme = Theme::dark();
    let ink = cpu(&theme);

    let full = chart_surface(
        &Chart::new(PressureKind::Cpu).with_samples([1000; 8]),
        &theme,
    );
    assert!(
        row_has(&full, 0, ink),
        "a saturated series must reach the top"
    );

    let empty = chart_surface(&Chart::new(PressureKind::Cpu).with_samples([0; 8]), &theme);
    assert!(
        !row_has(&empty, 0, ink),
        "a zero series must not reach the top"
    );
    assert!(
        row_has(&empty, H - 1, ink),
        "a zero series must sit on the floor"
    );
}

#[test]
fn the_plot_scales_to_the_height_it_is_given() {
    // A mid-scale reading lands halfway up whatever box it is drawn in, so a
    // taller chart shows a taller swing rather than the same few pixels.
    let theme = Theme::dark();
    let ink = cpu(&theme);
    for height in [24_u32, 40, 100, 160] {
        let surface = chart_surface_of(
            &Chart::new(PressureKind::Cpu).with_samples([500; 8]),
            &theme,
            W,
            height,
        );
        let top = topmost(&surface, ink).expect("the trace is drawn");
        let middle = height / 2;
        // Within a line weight of the half-height mark, at every size.
        assert!(
            top + 4 >= middle && top <= middle + 4,
            "half scale in a {height}-tall box plotted at row {top}, not near {middle}"
        );
    }
}

#[test]
fn readings_run_oldest_to_newest_left_to_right() {
    let theme = Theme::dark();
    let ink = cpu(&theme);
    let surface = chart_surface(
        &Chart::new(PressureKind::Cpu).with_samples([0, 1000]),
        &theme,
    );
    let mid = W / 2;
    let left = topmost_in(&surface, 0, mid, ink).expect("the trace crosses the left half");
    let right = topmost_in(&surface, mid, W, ink).expect("the trace crosses the right half");
    // The newest reading is the high one, so the trace rises to the right.
    assert!(
        right < left,
        "oldest to newest runs left to right: {left} then {right}"
    );
}

// --- The bounded series ---------------------------------------------------

#[test]
fn an_empty_chart_plots_nothing_at_all() {
    let theme = Theme::dark();
    let chart = Chart::new(PressureKind::Cpu);
    assert!(chart.is_empty());
    let surface = chart_surface(&chart, &theme);
    // The quiet plate alone: no fabricated floor line, which would read as a
    // measured idle rather than as no history.
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
    assert!(!has_pixel(&surface, cpu(&theme)));
}

#[test]
fn no_reading_and_a_measured_nought_do_not_look_alike() {
    // The distinction the whole honest-reading model rests on: a resource
    // measured at nought is a fact and draws its floor line, while a resource
    // nobody could measure draws nothing. Rendering them the same would let a
    // missing reading pass for an idle one.
    let theme = Theme::dark();
    let nothing = chart_surface(&Chart::new(PressureKind::Cpu), &theme);
    let nought = chart_surface(&Chart::new(PressureKind::Cpu).with_samples([0, 0]), &theme);
    assert_ne!(nothing.pixels(), nought.pixels());
    assert!(
        has_pixel(&nought, cpu(&theme)),
        "a measured nought is drawn"
    );
    assert!(
        !has_pixel(&nothing, cpu(&theme)),
        "an absent reading is not"
    );
}

#[test]
fn a_single_reading_holds_across_the_whole_box() {
    // One sample is a real measurement, so it draws as a flat line at its own
    // height rather than as an invisible point.
    let theme = Theme::dark();
    let ink = cpu(&theme);
    let surface = chart_surface(&Chart::new(PressureKind::Cpu).with_samples([1000]), &theme);
    let marked: Vec<u32> = (0..W).filter(|&x| surface.get(x, 1) == Some(ink)).collect();
    let first = *marked.first().expect("the lone reading is drawn");
    let last = *marked.last().expect("the lone reading is drawn");
    // Held right across, inset only by the room the stroke needs at each end.
    assert!(first <= 2, "the trace starts at column {first}");
    assert!(last + 3 >= W, "the trace ends at column {last} of {W}");
}

#[test]
fn a_series_is_capped_and_keeps_the_most_recent() {
    let theme = Theme::dark();
    let long: Vec<u16> = (0..MAX_CHART_SAMPLES + 10)
        .map(|i| u16::try_from(i).expect("test index fits in u16"))
        .collect();
    let tail: Vec<u16> = (10..MAX_CHART_SAMPLES + 10)
        .map(|i| u16::try_from(i).expect("test index fits in u16"))
        .collect();
    let capped = Chart::new(PressureKind::Cpu).with_samples(long);
    let bounded = Chart::new(PressureKind::Cpu).with_samples(tail);
    assert_eq!(
        chart_surface(&capped, &theme).pixels(),
        chart_surface(&bounded, &theme).pixels()
    );
}

#[test]
fn out_of_range_readings_are_clamped_fail_closed() {
    let theme = Theme::dark();
    let over = Chart::new(PressureKind::Cpu).with_samples([5000, 5000]);
    let clamped = Chart::new(PressureKind::Cpu).with_samples([1000, 1000]);
    assert_eq!(
        chart_surface(&over, &theme).pixels(),
        chart_surface(&clamped, &theme).pixels()
    );
}

// --- Identity and theme ---------------------------------------------------

#[test]
fn each_pressure_kind_traces_in_its_own_rail_colour() {
    let theme = Theme::dark();
    let p = theme.palette();
    let cases = [
        (PressureKind::Cpu, p.cpu_pressure),
        (PressureKind::Memory, p.memory_pressure),
        (PressureKind::Disk, p.disk_pressure),
        (PressureKind::Network, p.network_activity),
        (PressureKind::Power, p.power_pressure),
        (PressureKind::Thermal, p.thermal_pressure),
    ];
    for (kind, expected) in cases {
        let surface = chart_surface(&Chart::new(kind).with_samples([600; 6]), &theme);
        assert!(
            has_pixel(&surface, premul(expected)),
            "{kind:?} must trace in its own rail colour"
        );
    }
}

#[test]
fn a_chart_renders_in_both_themes() {
    let chart = Chart::new(PressureKind::Memory).with_samples([200, 800, 400, 900]);
    assert_ne!(
        chart_surface(&chart, &Theme::dark()).pixels(),
        chart_surface(&chart, &Theme::light()).pixels()
    );
}

/// Every reading a live series can take plots, at every width the instrument
/// is laid out at.
///
/// The regression this covers is the one a monitor actually hit: a chart is
/// fed fresh measurements every sample, so its trace steps by a different
/// amount each time, and the shared stroke path used to compute a segment's
/// length by an iteration that never converged for certain lengths. One
/// unlucky reading wedged the drawing process in a loop it never left — the
/// window simply stopped repainting and a core sat at full tilt.
#[test]
fn every_reading_plots_at_every_width() {
    let theme = Theme::dark();
    let ink = cpu(&theme);
    for width in 9..=20 {
        for reading in 0..=1000_u16 {
            let surface = chart_surface_of(
                &Chart::new(PressureKind::Cpu).with_samples([reading, 0]),
                &theme,
                width,
                H,
            );
            assert!(
                has_pixel(&surface, ink),
                "a {width}-wide chart dropped the reading {reading}"
            );
        }
    }
}

// --- The opposing series --------------------------------------------------

fn network(theme: &Theme) -> Pixel {
    premul(theme.palette().network_activity)
}

/// The bottom-most row carrying `want` anywhere, if any.
fn bottommost(surface: &Surface, want: Pixel) -> Option<u32> {
    (0..surface.height())
        .rev()
        .find(|&y| row_has(surface, y, want))
}

#[test]
fn each_direction_keeps_to_its_own_half_of_the_box() {
    // The comparison the control exists for: receive above the axis, send
    // below it, so one glance says which way the traffic is going.
    let theme = Theme::dark();
    let duplex = Chart::new(PressureKind::Network)
        .with_samples([1000; 8])
        .with_opposing(PressureKind::Disk, [1000; 8]);
    let surface = chart_surface(&duplex, &theme);

    let up = network(&theme);
    let down = premul(theme.palette().disk_pressure);
    let up_low = bottommost(&surface, up).expect("the primary series must plot");
    let down_high = topmost(&surface, down).expect("the opposing series must plot");
    assert!(
        up_low < down_high,
        "the two directions overlapped: primary reached row {up_low}, opposing row {down_high}"
    );
}

#[test]
fn the_opposing_series_grows_the_other_way() {
    // Mirrored, not merely relocated: a rising opposing reading moves *down*
    // the surface, away from the axis.
    let theme = Theme::dark();
    let ink = premul(theme.palette().disk_pressure);
    let quiet = Chart::new(PressureKind::Network)
        .with_samples([0; 6])
        .with_opposing(PressureKind::Disk, [100; 6]);
    let busy = Chart::new(PressureKind::Network)
        .with_samples([0; 6])
        .with_opposing(PressureKind::Disk, [1000; 6]);

    let quiet_reach = bottommost(&chart_surface(&quiet, &theme), ink).expect("quiet plots");
    let busy_reach = bottommost(&chart_surface(&busy, &theme), ink).expect("busy plots");
    assert!(
        busy_reach > quiet_reach,
        "a larger opposing reading must reach further down: {busy_reach} vs {quiet_reach}"
    );
}

#[test]
fn a_duplex_chart_draws_its_axis() {
    let theme = Theme::dark();
    let rim = premul(theme.palette().rim);
    let duplex = Chart::new(PressureKind::Network)
        .with_samples([400; 4])
        .with_opposing(PressureKind::Disk, [400; 4]);
    assert!(
        has_pixel(&chart_surface(&duplex, &theme), rim),
        "the zero line both series read against must be drawn"
    );
    // A single-series chart has no second direction, so no axis.
    let single = Chart::new(PressureKind::Network).with_samples([400; 4]);
    assert!(!has_pixel(&chart_surface(&single, &theme), rim));
}

#[test]
fn an_axis_is_never_drawn_where_there_is_no_reading() {
    // An empty duplex chart is the quiet plate alone: a rule across the middle
    // of an empty box would read as a measured nought.
    let theme = Theme::dark();
    let empty = Chart::new(PressureKind::Network).with_opposing(PressureKind::Disk, []);
    assert!(empty.is_empty());
    assert!(!has_pixel(
        &chart_surface(&empty, &theme),
        premul(theme.palette().rim)
    ));
}

#[test]
fn one_measured_direction_still_plots_and_states_its_axis() {
    // An interface that receives and never sends has a measured zero send,
    // which is a reading: the axis and the primary trace both draw.
    let theme = Theme::dark();
    let half = Chart::new(PressureKind::Network)
        .with_samples([700; 5])
        .with_opposing(PressureKind::Disk, []);
    assert!(!half.is_empty());
    let surface = chart_surface(&half, &theme);
    assert!(has_pixel(&surface, network(&theme)));
    assert!(has_pixel(&surface, premul(theme.palette().rim)));
    assert!(!has_pixel(&surface, premul(theme.palette().disk_pressure)));
}

#[test]
fn the_opposing_series_is_bounded_and_clamped_like_the_primary() {
    let theme = Theme::dark();
    let over = Chart::new(PressureKind::Network)
        .with_samples([500])
        .with_opposing(PressureKind::Disk, [5000, 5000]);
    let clamped = Chart::new(PressureKind::Network)
        .with_samples([500])
        .with_opposing(PressureKind::Disk, [1000, 1000]);
    assert_eq!(
        chart_surface(&over, &theme).pixels(),
        chart_surface(&clamped, &theme).pixels()
    );

    let capped = Chart::new(PressureKind::Network)
        .with_samples([500])
        .with_opposing(
            PressureKind::Disk,
            (0..MAX_CHART_SAMPLES + 4).map(|i| u16::try_from(i % 1000).unwrap_or(0)),
        );
    let recent = Chart::new(PressureKind::Network)
        .with_samples([500])
        .with_opposing(
            PressureKind::Disk,
            (4..MAX_CHART_SAMPLES + 4).map(|i| u16::try_from(i % 1000).unwrap_or(0)),
        );
    assert_eq!(
        chart_surface(&capped, &theme).pixels(),
        chart_surface(&recent, &theme).pixels()
    );
}

#[test]
fn a_box_too_short_to_seat_both_halves_keeps_the_quiet_plate() {
    // Fail closed: half a duplex reading is worse than none, so a box that
    // cannot hold an axis and two bands draws no trace at all.
    let theme = Theme::dark();
    let duplex = Chart::new(PressureKind::Network)
        .with_samples([1000; 4])
        .with_opposing(PressureKind::Disk, [1000; 4]);
    let surface = chart_surface_of(&duplex, &theme, W, 2);
    assert!(!has_pixel(&surface, network(&theme)));
    assert!(!has_pixel(&surface, premul(theme.palette().disk_pressure)));
    // The groove is still there: the instrument is present, just unreadable
    // at that height.
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
}

#[test]
fn a_duplex_chart_renders_in_both_themes() {
    let duplex = Chart::new(PressureKind::Network)
        .with_samples([200, 800, 400])
        .with_opposing(PressureKind::Disk, [600, 100, 900]);
    assert_ne!(
        chart_surface(&duplex, &Theme::dark()).pixels(),
        chart_surface(&duplex, &Theme::light()).pixels()
    );
}

#[test]
fn a_duplex_chart_draws_a_heavier_axis_under_heavy_contrast() {
    let duplex = Chart::new(PressureKind::Network)
        .with_samples([300; 4])
        .with_opposing(PressureKind::Disk, [300; 4]);
    let normal = chart_surface(&duplex, &Theme::dark());
    let heavy = chart_surface(&duplex, &crate::testkit::high_contrast());
    let rim = premul(Theme::dark().palette().rim);
    let rows = |s: &Surface| (0..s.height()).filter(|&y| row_has(s, y, rim)).count();
    assert!(
        rows(&heavy) > rows(&normal),
        "the heavier-contrast axis must be thicker"
    );
}

// --- Fail-closed ----------------------------------------------------------

#[test]
fn a_degenerate_box_draws_nothing_outside_itself() {
    let theme = Theme::dark();
    let chart = Chart::new(PressureKind::Cpu).with_samples([100, 900, 500]);
    let mut surface = Surface::new(1, 1).expect("surface");
    chart.render(&mut surface, Rect::new(0, 0, 1, 1), Scale::ONE, &theme);
    assert_eq!(surface.pixels().len(), 1);

    // A zero-extent box is not a plot at all.
    let mut none = Surface::new(4, 4).expect("surface");
    chart.render(&mut none, Rect::new(0, 0, 0, 0), Scale::ONE, &theme);
    assert!(none.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}
