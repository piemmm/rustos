//! `cargo xtask bench` implementation: host microbenchmarks for the raster
//! and compositor families.
//!
//! The desktop's redraw cost is per-pixel work — a blend, a blur tap, a
//! resample tap, a scan-out encode — so the numbers that matter are
//! nanoseconds per pixel and nanoseconds per frame. This command produces
//! them by running the *production* entry points (`lib/raster`,
//! `lib/display`'s scan-out encode, and a real `tairix_wm::Compositor` over a
//! window stack), never a re-implementation of them.
//!
//! # Not a gate
//!
//! Wall-clock timings are load-dependent, so nothing here is a pass/fail
//! threshold and no test asserts an elapsed time. The output is evidence for
//! a completion report; the regression gates are the deterministic work
//! counters (pixels blended, rectangles presented) elsewhere. CI may run this
//! only as a smoke check that the harness itself still produces a number for
//! every family.
//!
//! # One timing loop
//!
//! The measurement is `lib/cpuops`'s existing `BenchHarness` — bounded,
//! one-shot, median-of-rounds, `black_box`ed — with a host clock injected
//! through its `CycleCounter` seam. There is deliberately no second timing
//! loop here.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::time::Instant;

use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
use tairix_abi::seat::SEAT_PRIMARY;
use tairix_cpuops::{BenchHarness, CycleCounter};
use tairix_display::ChannelOrder;
use tairix_font::{glyph_cache_budget, glyph_cache_candidate, set_glyph_cache, BitmapFont};
use tairix_geometry::Scale;
use tairix_log::DiscardSink;
use tairix_raster::{box_blur, BlurScratch, Color, Pixel, Rgba8Image, Surface};
use tairix_reclaim::{PressureBand, ReclaimCache, ReclaimOwner, ReportedPressure};
use tairix_theme::{TextRole, Theme};
use tairix_wm::{chrome_cache, frost_cache, Compositor, Point, Rect, Region, WindowId};

/// The default timed calls per round and rounds per case.
///
/// Deliberately far below the harness's own boot-time defaults: a megapixel
/// case resolves at this budget, and a large one would cost a developer
/// minutes for nothing. A *small* case does not: on a ten-thousand-pixel one
/// the run-to-run spread here is around ±15%, so comparing two builds of
/// something that cheap needs `--iters 400 --rounds 25` (≈1%) before a
/// difference of a few per cent means anything.
const DEFAULT_ITERS: u32 = 16;
const DEFAULT_ROUNDS: u32 = 5;

/// The screen the composite family composes for: a laptop-class desktop.
const SCREEN_W: u32 = 1280;
const SCREEN_H: u32 = 800;

/// The small update a control-sized repaint damages — a label or a hovered
/// button, the case the desktop pays for on every pointer sample.
const SMALL_DAMAGE: Rect = Rect {
    origin: Point { x: 16, y: 16 },
    width: 64,
    height: 24,
};

/// Parsed `bench` arguments.
#[derive(Debug)]
pub struct Options {
    /// Timed calls per round.
    pub iters: u32,
    /// Independent rounds reduced by the median.
    pub rounds: u32,
    /// Run only the families whose name contains this substring.
    pub filter: Option<String>,
}

/// Parse `cargo xtask bench` arguments.
///
/// A missing or non-numeric budget, an unknown flag, and a filter matching no
/// family are all reported as errors; none of them panics.
pub fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut iters = DEFAULT_ITERS;
    let mut rounds = DEFAULT_ROUNDS;
    let mut filter = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let Some(flag) = arg.to_str() else {
            return Err(format!(
                "bench: argument is not valid UTF-8: {}",
                arg.display()
            ));
        };
        match flag {
            "--iters" => iters = number(&mut rest, flag)?,
            "--rounds" => rounds = number(&mut rest, flag)?,
            "--filter" => {
                let value = rest
                    .next()
                    .ok_or_else(|| "bench: `--filter` requires a substring".to_string())?;
                let value = value.to_str().ok_or_else(|| {
                    format!("bench: `--filter` is not valid UTF-8: {}", value.display())
                })?;
                filter = Some(value.to_string());
            }
            _ => {
                return Err(format!(
                    "bench: unknown argument `{flag}`\n\n\
                     usage: cargo xtask bench [--iters N] [--rounds N] [--filter SUBSTRING]\n\
                     families: {}",
                    family_names()
                ))
            }
        }
    }
    Ok(Options {
        iters,
        rounds,
        filter,
    })
}

/// Measure every selected family and print the table.
pub fn run(opts: &Options) -> Result<(), String> {
    let families = select(opts.filter.as_deref())?;
    let clock = NanoClock::new();
    let harness = BenchHarness::with_budget(&clock, opts.iters, opts.rounds);
    // The harness clamps its budget, so report what it will actually run
    // rather than what was asked for.
    let iters = harness.iters();
    println!(
        "bench: {iters} iterations x {} rounds per case, median round, host clock in ns",
        harness.rounds()
    );
    println!("bench: evidence for a completion report, never a pass/fail gate");
    println!(
        "\n{:<10} {:<34} {:>10} {:>14} {:>9}",
        "family", "case", "px", "ns/frame", "ns/px"
    );
    for family in families {
        eprintln!("xtask: [bench] {} — {}", family.name, family.what);
        for row in (family.measure)(&harness)? {
            println!(
                "{:<10} {:<34} {:>10} {:>14.1} {:>9.3}",
                family.name,
                row.case,
                row.pixels,
                row.ns_per_call(iters),
                row.ns_per_pixel(iters)
            );
        }
    }
    Ok(())
}

/// The families whose name contains `filter`, or all of them for `None`.
fn select(filter: Option<&str>) -> Result<Vec<&'static Family>, String> {
    let Some(filter) = filter else {
        return Ok(FAMILIES.iter().collect());
    };
    let picked: Vec<&Family> = FAMILIES
        .iter()
        .filter(|family| family.name.contains(filter))
        .collect();
    if picked.is_empty() {
        return Err(format!(
            "bench: `--filter {filter}` matches no family (have: {})",
            family_names()
        ));
    }
    Ok(picked)
}

fn family_names() -> String {
    FAMILIES
        .iter()
        .map(|family| family.name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn number<'a>(rest: &mut impl Iterator<Item = &'a OsString>, flag: &str) -> Result<u32, String> {
    let value = rest
        .next()
        .ok_or_else(|| format!("bench: `{flag}` requires a number"))?;
    value
        .to_str()
        .and_then(|text| text.parse::<u32>().ok())
        .ok_or_else(|| {
            format!(
                "bench: `{flag}` expects a positive integer, got {}",
                value.display()
            )
        })
}

/// A host time source for the harness, counting **nanoseconds** — not CPU
/// cycles — since its construction.
///
/// The harness is unit-agnostic: it only ever subtracts two readings and
/// takes a median, so a nanosecond counter reports exactly as validly as the
/// cycle counter the kernel injects. Every number this command prints is
/// therefore a nanosecond figure.
struct NanoClock {
    start: Instant,
}

impl NanoClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl CycleCounter for NanoClock {
    fn cycles(&self) -> u64 {
        // A run outliving 584 years would saturate rather than wrap.
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// `Instant` is monotonic and its rate does not follow core frequency.
    fn cycles_monotonic_hint(&self) -> bool {
        true
    }
}

/// One measured case: the median cost of a whole timed round, and the pixels
/// that round's *single* call touches.
struct Measurement {
    case: String,
    pixels: u64,
    round_ns: u64,
}

impl Measurement {
    fn new(case: String, pixels: u64, round_ns: u64) -> Self {
        Self {
            case,
            pixels,
            round_ns,
        }
    }

    /// Nanoseconds for one call — one blit, one blur, one composited frame.
    fn ns_per_call(&self, iters: u32) -> f64 {
        as_f64(self.round_ns) / f64::from(iters.max(1))
    }

    fn ns_per_pixel(&self, iters: u32) -> f64 {
        self.ns_per_call(iters) / as_f64(self.pixels.max(1))
    }
}

#[allow(clippy::cast_precision_loss)] // Nanosecond and pixel counts here are far below f64's exactly-representable range.
fn as_f64(value: u64) -> f64 {
    value as f64
}

/// One benchmark family: a group of cases over the same production entry
/// point.
#[derive(Debug)]
struct Family {
    name: &'static str,
    what: &'static str,
    measure: fn(&BenchHarness<'_>) -> Result<Vec<Measurement>, String>,
}

/// The families `bench` measures. Adding a case means extending one of these
/// functions; adding an entry point means one more row here.
static FAMILIES: &[Family] = &[
    Family {
        name: "blit",
        what: "Surface::blit, opaque and translucent sources",
        measure: blit,
    },
    Family {
        name: "round-rect",
        what: "Surface::fill_round_rect",
        measure: round_rect,
    },
    Family {
        name: "text",
        what: "BitmapFont::draw_text and text_width over a label and a row",
        measure: text,
    },
    Family {
        name: "blur",
        what: "box_blur and Surface::frost_region over several radii",
        measure: blur,
    },
    Family {
        name: "resample",
        what: "resample to a new size",
        measure: resample,
    },
    Family {
        name: "encode",
        what: "ChannelOrder::encode over a scan-out row",
        measure: encode,
    },
    Family {
        name: "composite",
        what: "Compositor::composite over a window stack",
        measure: composite,
    },
];

fn surface(width: u32, height: u32, color: Color) -> Result<Surface, String> {
    Surface::filled(width, height, color.premultiply())
        .ok_or_else(|| format!("bench: a {width}x{height} surface could not be allocated"))
}

/// An extent as a host count. Every extent here is a screen dimension, far
/// inside `usize` on any host this tool runs on; saturating keeps that fact
/// from ever becoming a panic.
fn count_of(extent: u32) -> usize {
    usize::try_from(extent).unwrap_or(usize::MAX)
}

/// The bytes a `width`x`height` RGBA8 buffer holds.
fn pixel_bytes(width: u32, height: u32) -> usize {
    count_of(width)
        .saturating_mul(count_of(height))
        .saturating_mul(4)
}

fn blit(harness: &BenchHarness<'_>) -> Result<Vec<Measurement>, String> {
    struct Warm {
        dst: RefCell<Surface>,
        src: Surface,
    }

    fn run(_: (), warm: &Warm) -> Option<Pixel> {
        let mut dst = warm.dst.borrow_mut();
        dst.blit(0, 0, &warm.src);
        dst.get(0, 0)
    }

    let mut rows = Vec::new();
    for (width, height) in [(SCREEN_W, SCREEN_H), (320, 200)] {
        for alpha in [255u8, 160] {
            let warm = Warm {
                dst: RefCell::new(surface(SCREEN_W, SCREEN_H, Color::rgb(18, 20, 26))?),
                src: surface(width, height, Color::rgba(90, 140, 220, alpha))?,
            };
            rows.push(Measurement::new(
                format!("{} src {width}x{height}", alpha_label(alpha)),
                u64::from(width) * u64::from(height),
                harness.median_cycles((), run, &warm),
            ));
        }
    }
    Ok(rows)
}

fn alpha_label(alpha: u8) -> &'static str {
    if alpha == u8::MAX {
        "opaque"
    } else {
        "translucent"
    }
}

fn round_rect(harness: &BenchHarness<'_>) -> Result<Vec<Measurement>, String> {
    struct Warm {
        dst: RefCell<Surface>,
        color: Color,
    }

    #[derive(Copy, Clone)]
    struct Case {
        width: u32,
        height: u32,
        radius: u32,
    }

    fn run(case: Case, warm: &Warm) -> Option<Pixel> {
        let mut dst = warm.dst.borrow_mut();
        dst.fill_round_rect(0, 0, case.width, case.height, case.radius, warm.color);
        dst.get(0, 0)
    }

    let warm = Warm {
        dst: RefCell::new(surface(SCREEN_W, SCREEN_H, Color::rgb(18, 20, 26))?),
        color: Color::rgba(200, 210, 230, 230),
    };
    let cases = [
        // A button, a panel, and a full-width taskbar strip.
        Case {
            width: 200,
            height: 40,
            radius: 8,
        },
        Case {
            width: 400,
            height: 240,
            radius: 16,
        },
        Case {
            width: SCREEN_W,
            height: 64,
            radius: 24,
        },
    ];
    Ok(cases
        .into_iter()
        .map(|case| {
            Measurement::new(
                format!("{}x{} r{}", case.width, case.height, case.radius),
                u64::from(case.width) * u64::from(case.height),
                harness.median_cycles(case, run, &warm),
            )
        })
        .collect())
}

/// The label a control draws: a file name, a button caption, a row title.
const LABEL: &str = "Documents";

/// A list row's or terminal line's worth of text.
const ROW: &str = "The compositor copies an opaque run and blends only what shows through it.";

/// The colour text is drawn in: the dark theme's body ink.
const INK: Color = Color::rgb(230, 232, 238);

/// Text drawing and measurement, through the production entry points.
///
/// Text is the desktop's other per-pixel family: a control-rich window draws
/// hundreds of glyphs a frame, and each one costs a pen advance, a glyph-cache
/// lookup, and a coverage composite. The pixel figure is the run's own
/// measured extent, so it is comparable with the blit and blend families.
///
/// The proportional cases are the ones a desktop pays — a monospace family
/// shares one advance and takes an arithmetic path with nothing to look up.
fn text(harness: &BenchHarness<'_>) -> Result<Vec<Measurement>, String> {
    struct Warm {
        dst: RefCell<Surface>,
        font: BitmapFont,
        text: &'static str,
    }

    fn draw(_: (), warm: &Warm) -> i32 {
        let mut dst = warm.dst.borrow_mut();
        warm.font.draw_text(&mut dst, 0, 0, warm.text, INK)
    }

    fn measure(_: (), warm: &Warm) -> u32 {
        warm.font.text_width(warm.text)
    }

    warm_font_client();
    let theme = Theme::dark();
    let proportional = BitmapFont::for_role(theme.fonts(), TextRole::Body, Scale::ONE);
    let monospace = BitmapFont::monospace(proportional.glyph_height());

    let mut rows = Vec::new();
    for (label, font, body) in [
        ("proportional label", proportional, LABEL),
        ("proportional row", proportional, ROW),
        ("monospace row", monospace, ROW),
    ] {
        let warm = Warm {
            dst: RefCell::new(surface(SCREEN_W, 64, Color::rgb(18, 20, 26))?),
            font,
            text: body,
        };
        // The first draw fetches this run's glyphs; every timed one then
        // measures the steady state a repainting desktop is in.
        draw((), &warm);
        let pixels = text_pixels(font, body);
        rows.push(Measurement::new(
            format!("draw {label}, {} chars", body.chars().count()),
            pixels,
            harness.median_cycles((), draw, &warm),
        ));
        if label == "proportional row" {
            rows.push(Measurement::new(
                format!("width {label}, {} chars", body.chars().count()),
                pixels,
                harness.median_cycles((), measure, &warm),
            ));
        }
    }
    Ok(rows)
}

/// The pixels a run of `text` covers at `font`: its measured extent.
fn text_pixels(font: BitmapFont, text: &str) -> u64 {
    u64::from(font.text_width(text)) * u64::from(font.glyph_height())
}

/// Install the glyph cache the text cases draw through.
///
/// A desktop draws with a cache installed, so a figure taken without one
/// would describe the host font's reply encoding rather than the drawing path.
/// The transport itself defaults in on first draw.
fn warm_font_client() {
    PRESSURE.report(PressureBand::Normal);
    set_glyph_cache(ReclaimCache::new(
        "bench.font.glyphs",
        glyph_cache_candidate(ReclaimOwner::UserlandProcess("xtask.bench")),
        glyph_cache_budget(1 << 30),
        &PRESSURE,
        &SINK,
    ));
}

fn blur(harness: &BenchHarness<'_>) -> Result<Vec<Measurement>, String> {
    struct BoxWarm {
        region: RefCell<Vec<Pixel>>,
        aux: RefCell<Vec<Pixel>>,
        width: usize,
        height: usize,
    }

    struct FrostWarm {
        surface: RefCell<Surface>,
        scratch: RefCell<BlurScratch>,
        width: u32,
        height: u32,
    }

    fn run_box(radius: usize, warm: &BoxWarm) -> Option<Pixel> {
        let mut region = warm.region.borrow_mut();
        let mut aux = warm.aux.borrow_mut();
        box_blur(&mut region, warm.width, warm.height, radius, &mut aux);
        region.first().copied()
    }

    fn run_frost(radius: u32, warm: &FrostWarm) -> Option<Pixel> {
        let mut surface = warm.surface.borrow_mut();
        let mut scratch = warm.scratch.borrow_mut();
        surface.frost_region(
            0,
            0,
            warm.width,
            warm.height,
            radius,
            &mut scratch,
            |_, _| u8::MAX,
        );
        surface.get(0, 0)
    }

    // A window-backdrop-sized rectangle: what a frosted panel actually costs.
    let (width, height) = (640u32, 360u32);
    let pixels = u64::from(width) * u64::from(height);
    let count = count_of(width).saturating_mul(count_of(height));
    let box_warm = BoxWarm {
        region: RefCell::new(vec![Color::rgb(70, 90, 140).premultiply(); count]),
        aux: RefCell::new(vec![Pixel::TRANSPARENT; count]),
        width: count_of(width),
        height: count_of(height),
    };
    let frost_warm = FrostWarm {
        surface: RefCell::new(surface(width, height, Color::rgb(70, 90, 140))?),
        scratch: RefCell::new(BlurScratch::new()),
        width,
        height,
    };

    let mut rows = Vec::new();
    for radius in [4u32, 12, 24] {
        rows.push(Measurement::new(
            format!("box_blur {width}x{height} r{radius}"),
            pixels,
            harness.median_cycles(radius as usize, run_box, &box_warm),
        ));
    }
    for radius in [4u32, 12, 24] {
        rows.push(Measurement::new(
            format!("frost_region {width}x{height} r{radius}"),
            pixels,
            harness.median_cycles(radius, run_frost, &frost_warm),
        ));
    }
    Ok(rows)
}

// One signature serves the whole family table; a family that allocates a
// surface genuinely fails, this one cannot.
#[allow(clippy::unnecessary_wraps)]
fn resample(harness: &BenchHarness<'_>) -> Result<Vec<Measurement>, String> {
    struct Warm {
        pixels: Vec<u8>,
        width: u32,
        height: u32,
    }

    #[derive(Copy, Clone)]
    struct Case {
        dest_width: u32,
        dest_height: u32,
    }

    fn run(case: Case, warm: &Warm) -> Option<usize> {
        let src = Rgba8Image::new(warm.width, warm.height, &warm.pixels).ok()?;
        let out =
            tairix_raster::resample(&src, src.whole(), case.dest_width, case.dest_height).ok()?;
        Some(out.len())
    }

    // The wallpaper fit (a photographic master down to the screen) and the two
    // icon directions a desktop slot asks for.
    let cases = [
        (1920u32, 1080u32, 1280u32, 800u32),
        (256, 256, 64, 64),
        (64, 64, 256, 256),
    ];
    let mut rows = Vec::new();
    for (width, height, dest_width, dest_height) in cases {
        let warm = Warm {
            pixels: gradient_rgba8(width, height),
            width,
            height,
        };
        let case = Case {
            dest_width,
            dest_height,
        };
        rows.push(Measurement::new(
            format!("{width}x{height} -> {dest_width}x{dest_height}"),
            u64::from(dest_width) * u64::from(dest_height),
            harness.median_cycles(case, run, &warm),
        ));
    }
    Ok(rows)
}

/// A straight-alpha RGBA8 image whose every pixel differs, so a resample's
/// taps do real work instead of walking one flat colour.
fn gradient_rgba8(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(pixel_bytes(width, height));
    for y in 0..height {
        let row = shade(y);
        for x in 0..width {
            let column = shade(x);
            pixels.extend_from_slice(&[column, row, column ^ row, u8::MAX]);
        }
    }
    pixels
}

/// A repeating per-coordinate shade. The mask makes the conversion total, so
/// the fallback is unreachable and nothing here can panic.
fn shade(coordinate: u32) -> u8 {
    u8::try_from(coordinate & 0xFF).unwrap_or_default()
}

#[allow(clippy::unnecessary_wraps)] // As above: the family table's one signature.
fn encode(harness: &BenchHarness<'_>) -> Result<Vec<Measurement>, String> {
    struct Warm {
        row: Vec<Pixel>,
        out: RefCell<Vec<u8>>,
    }

    fn run(order: ChannelOrder, warm: &Warm) -> Option<u8> {
        let mut out = warm.out.borrow_mut();
        let (bytes, _) = out.as_chunks_mut::<4>();
        for (pixel, target) in warm.row.iter().zip(bytes) {
            *target = order.encode(*pixel);
        }
        out.first().copied()
    }

    let width = SCREEN_W;
    let warm = Warm {
        row: (0..width)
            .map(|x| Color::rgb(shade(x), 128, 200).premultiply())
            .collect(),
        out: RefCell::new(vec![0u8; pixel_bytes(width, 1)]),
    };
    Ok([ChannelOrder::Rgba, ChannelOrder::Bgra]
        .into_iter()
        .map(|order| {
            Measurement::new(
                format!("{order:?} row of {width}"),
                u64::from(width),
                harness.median_cycles(order, run, &warm),
            )
        })
        .collect())
}

/// The gauge and audit sink the compositor's furniture cache answers to.
///
/// A gauge that has never been told a band answers critical, and a cache at
/// critical retains nothing — which would measure a cache that never hits.
/// Reporting the desktop's ordinary band is what makes the composite numbers
/// describe production behaviour.
static PRESSURE: ReportedPressure = ReportedPressure::unknown();
static SINK: DiscardSink = DiscardSink;

/// How a composite case dirties the screen before each timed frame.
#[derive(Copy, Clone)]
enum Damage {
    /// The whole screen, as a theme switch or a wallpaper change does.
    FullScreen,
    /// One control-sized rectangle inside the top window, as a hover flip
    /// does. A blurred window widens this to its own bounds.
    SmallRect,
    /// The top window moves one pointer sample's worth, as dragging it by
    /// its title bar does. Its own pixels are untouched — only its origin
    /// moves — so this is the case a retained frost has to survive.
    Drag,
}

/// How far a dragged window travels between two pointer samples. A hand
/// moving a window across the screen at interactive rates delivers a few
/// pixels per sample, not one and not a hundred.
const DRAG_STEP: i32 = 6;

/// What the window stack under test is made of.
#[derive(Copy, Clone)]
enum Stack {
    Opaque,
    Translucent,
    BackdropBlur,
}

struct CompositeWarm {
    compositor: RefCell<Compositor>,
    top: WindowId,
    client: (u32, u32),
    /// Where the top window sits when it is not being dragged.
    origin: Point,
    flip: Cell<bool>,
}

fn composite(harness: &BenchHarness<'_>) -> Result<Vec<Measurement>, String> {
    fn run(damage: Damage, warm: &CompositeWarm) -> u64 {
        let mut compositor = warm.compositor.borrow_mut();
        match damage {
            Damage::FullScreen => {
                // Alternate so every call genuinely re-dirties the screen.
                let flip = warm.flip.get();
                warm.flip.set(!flip);
                let shade = if flip { 18 } else { 19 };
                compositor.set_background(Color::rgb(shade, 20, 26));
            }
            Damage::SmallRect => {
                let (width, height) = warm.client;
                compositor.present_window_content(warm.top, width, height, |_| ((), SMALL_DAMAGE));
            }
            Damage::Drag => {
                // Two origins a sample apart, alternated, so a long run of
                // timed frames each moves the window without walking it off
                // the screen.
                let flip = warm.flip.get();
                warm.flip.set(!flip);
                let step = if flip { DRAG_STEP } else { 0 };
                let origin = warm.origin;
                compositor.move_window(warm.top, Point::new(origin.x + step, origin.y + step));
            }
        }
        region_pixels(&compositor.composite())
    }

    let cases = [
        (
            "full screen, opaque stack",
            Stack::Opaque,
            Damage::FullScreen,
        ),
        (
            "full screen, translucent stack",
            Stack::Translucent,
            Damage::FullScreen,
        ),
        (
            "full screen, backdrop blur",
            Stack::BackdropBlur,
            Damage::FullScreen,
        ),
        ("64x24 rect, opaque stack", Stack::Opaque, Damage::SmallRect),
        (
            "64x24 rect, backdrop blur",
            Stack::BackdropBlur,
            Damage::SmallRect,
        ),
        ("drag, opaque stack", Stack::Opaque, Damage::Drag),
        ("drag, translucent stack", Stack::Translucent, Damage::Drag),
        ("drag, backdrop blur", Stack::BackdropBlur, Damage::Drag),
    ];
    let mut rows = Vec::new();
    for (case, stack, damage) in cases {
        let warm = scene(stack)?;
        // The first present establishes the window's content buffer and
        // dirties its whole client area; the second reports the steady-state
        // damage this case actually recomposites per frame.
        run(damage, &warm);
        let pixels = run(damage, &warm);
        rows.push(Measurement::new(
            case.to_string(),
            pixels,
            harness.median_cycles(damage, run, &warm),
        ));
    }
    Ok(rows)
}

fn region_pixels(region: &Region) -> u64 {
    region
        .rects()
        .iter()
        .map(|rect| u64::from(rect.width) * u64::from(rect.height))
        .sum()
}

/// A compositor holding a representative three-window stack.
fn scene(stack: Stack) -> Result<CompositeWarm, String> {
    PRESSURE.report(PressureBand::Normal);
    let mode = DisplayMode {
        width_px: SCREEN_W,
        height_px: SCREEN_H,
        stride_bytes: SCREEN_W * 4,
        format: DisplayFormat::Rgba8888,
    };
    let frame_bytes = count_of(mode.stride_bytes).saturating_mul(count_of(mode.height_px));
    let chrome = chrome_cache(SEAT_PRIMARY, frame_bytes, &PRESSURE, &SINK);
    let frost = frost_cache(SEAT_PRIMARY, frame_bytes, &PRESSURE, &SINK);
    let mut compositor = Compositor::new(mode, Color::rgb(18, 20, 26), chrome, frost, &PRESSURE)
        .ok_or_else(|| format!("bench: no compositor for {SCREEN_W}x{SCREEN_H}"))?;

    let layout = [
        (Point::new(40, 60), 900u32, 560u32),
        (Point::new(200, 180), 700, 420),
        (Point::new(420, 300), 560, 360),
    ];
    let mut top = None;
    for (origin, width, height) in layout {
        let id = compositor.add_window(origin, surface(width, height, Color::rgb(48, 54, 72))?);
        match stack {
            Stack::Opaque => {}
            Stack::Translucent => {
                compositor.set_opacity(id, 190);
            }
            Stack::BackdropBlur => {
                compositor.set_opacity(id, 200);
                compositor.set_backdrop_blur(id, 12);
            }
        }
        top = Some((id, origin, width, height));
    }
    let (top, origin, width, height) =
        top.ok_or_else(|| "bench: the composite scene has no windows".to_string())?;
    // Compose once so the measured frames start from a settled screen.
    compositor.composite();
    Ok(CompositeWarm {
        compositor: RefCell::new(compositor),
        top,
        client: (width, height),
        origin,
        flip: Cell::new(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest budget the harness will run: enough to prove every family
    /// still measures its cases, without spending a test's time on resolution
    /// no assertion here reads.
    fn tiny(clock: &NanoClock) -> BenchHarness<'_> {
        BenchHarness::with_budget(clock, 1, 1)
    }

    #[test]
    fn every_family_reports_a_number_for_every_case() {
        let clock = NanoClock::new();
        let harness = tiny(&clock);
        for family in FAMILIES {
            let rows = (family.measure)(&harness)
                .unwrap_or_else(|err| panic!("{} did not measure: {err}", family.name));
            assert!(!rows.is_empty(), "{} measured no case", family.name);
            for row in rows {
                // Work, not wall-clock: a pixel count the case genuinely
                // touches is what makes the derived figure meaningful.
                assert!(
                    row.pixels > 0,
                    "{}/{} touches no pixels",
                    family.name,
                    row.case
                );
                assert!(
                    row.ns_per_call(harness.iters()).is_finite()
                        && row.ns_per_pixel(harness.iters()).is_finite(),
                    "{}/{} reported no finite figure",
                    family.name,
                    row.case
                );
            }
        }
    }

    #[test]
    fn a_non_numeric_budget_is_a_clean_error() {
        for flag in ["--iters", "--rounds"] {
            let args = [OsString::from(flag), OsString::from("lots")];
            let err = parse(&args).expect_err("a non-numeric budget is refused");
            assert!(err.contains(flag), "{err}");
            let missing = [OsString::from(flag)];
            let err = parse(&missing).expect_err("a budget with no value is refused");
            assert!(err.contains(flag), "{err}");
        }
        let err = parse(&[OsString::from("--rehearse")]).expect_err("an unknown flag is refused");
        assert!(err.contains("--rehearse"), "{err}");
    }

    #[test]
    fn a_budget_is_parsed_and_defaults_apply() {
        let opts = parse(&[]).expect("no arguments is the default budget");
        assert_eq!((opts.iters, opts.rounds), (DEFAULT_ITERS, DEFAULT_ROUNDS));
        assert!(opts.filter.is_none());
        let args = [
            OsString::from("--iters"),
            OsString::from("3"),
            OsString::from("--rounds"),
            OsString::from("7"),
            OsString::from("--filter"),
            OsString::from("blur"),
        ];
        let opts = parse(&args).expect("an explicit budget parses");
        assert_eq!((opts.iters, opts.rounds), (3, 7));
        assert_eq!(opts.filter.as_deref(), Some("blur"));
    }

    #[test]
    fn filter_selects_the_expected_subset() {
        let picked: Vec<&str> = select(Some("composite"))
            .expect("a matching filter selects")
            .iter()
            .map(|family| family.name)
            .collect();
        assert_eq!(picked, ["composite"]);
        assert_eq!(
            select(None).expect("no filter selects every family").len(),
            FAMILIES.len()
        );
        let err = select(Some("no-such-family")).expect_err("an unmatched filter is refused");
        assert!(err.contains("no-such-family"), "{err}");
    }
}
