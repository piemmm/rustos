//! The symbols the settings categories are drawn with.
//!
//! A symbol is authored as geometry, not pixels: SVG path data and circles on
//! a [`SYMBOL_GRID`]-unit square, filled or stroked. Nothing here flattens a
//! curve or outlines a stroke itself — `lib/svg`'s one flattener and one
//! stroker do, and `lib/svg`'s own placement puts the result on the integer
//! design grid a layer holds — so a symbol and a decoded SVG asset are the
//! same kind of drawing to the rasteriser.
//!
//! The same marks serve two pictures: a category's colour badge, where the
//! symbol stands small in white on its plate ([`crate::badge`]), and the
//! category's tintable glyph, where it fills the grid on its own.

use alloc::vec::Vec;

use tairix_raster::{Affine, Color, FillRule, Layer, Paint};
use tairix_svg::geom::{place, LineCap, LineJoin, StrokeStyle, SubPath};
use tairix_svg::pathdata::{flatten_ellipse_arc, parse_path_data};
use tairix_svg::stroke::stroke_outline;

use crate::glyph::IconKind;
use crate::vector::VectorIcon;

/// The side of the square every symbol is authored on, in symbol units.
pub(crate) const SYMBOL_GRID: f64 = 24.0;

/// Design units per symbol unit: fine enough that rounding a flattened point
/// onto the grid moves it a hundredth of a pixel at the largest slot side.
pub(crate) const UNIT: f64 = 128.0;

/// The design grid a symbol, and the badge it stands on, is drawn over.
pub(crate) const DESIGN: u32 = 3072;

const _: () = assert!(DESIGN as f64 == SYMBOL_GRID * UNIT);

/// The furthest a glyph's flattened curve departs from the true one, in symbol
/// units: a tenth of a pixel at a 480-pixel slot, so one glyph serves every
/// side.
const GLYPH_FLATNESS: f64 = 0.005;

/// The furthest a curve drawn at a known side departs from the true one, in
/// pixels of that side.
const FLATNESS_PX: f64 = 0.05;

/// The coarsest any curve is flattened, in symbol units, however small the
/// side it is drawn at.
const MAX_FLATNESS: f64 = 0.25;

/// The most points one mark may flatten to.
///
/// A containment bound, not a capacity: the authored marks flatten to a few
/// hundred points, so this only stops a defect in the table from allocating
/// without end.
const MAX_MARK_POINTS: usize = 16_384;

/// One closed or open contour of a mark.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Outline {
    /// SVG path data, in symbol units.
    Path(&'static str),
    /// A whole circle.
    Circle {
        /// Its centre, in symbol units.
        centre: (f64, f64),
        /// Its radius, in symbol units.
        radius: f64,
    },
}

/// One painted region of a symbol.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Mark {
    /// The area `outlines` enclose under `rule`: even-odd is how a mark cuts
    /// a hole (a keyhole, a key, a lamp) out of itself.
    Fill {
        /// The contours, filled together.
        outlines: &'static [Outline],
        /// Which points they enclose.
        rule: FillRule,
    },
    /// The area a stroke `width` symbol units wide along `outlines` covers.
    Stroke {
        /// The contours stroked.
        outlines: &'static [Outline],
        /// The stroke width, in symbol units.
        width: f64,
        /// How an open contour ends.
        cap: LineCap,
        /// How a contour turns a corner.
        join: LineJoin,
    },
}

/// Where a symbol lands: `scale` design units per symbol unit, offset by
/// `origin` design units.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Placement {
    /// Design units per symbol unit.
    pub scale: f64,
    /// Where the symbol grid's origin lands, in design units.
    pub origin: (f64, f64),
}

impl Placement {
    fn to_design(self) -> Affine {
        Affine::scale(self.scale, self.scale).then(Affine::translate(self.origin.0, self.origin.1))
    }

    /// A symbol filling the whole grid, as a glyph does.
    pub(crate) const WHOLE: Self = Self {
        scale: UNIT,
        origin: (0.0, 0.0),
    };

    /// A symbol drawn `fraction` of its own size, centred on the grid.
    pub(crate) fn centred(fraction: f64) -> Self {
        let inset = SYMBOL_GRID * (1.0 - fraction) / 2.0 * UNIT;
        Self {
            scale: UNIT * fraction,
            origin: (inset, inset),
        }
    }
}

/// `kind`'s symbol as a glyph filling the grid in `color`, or `None` for a
/// kind drawn with a glyph of its own.
pub(crate) fn glyph(kind: IconKind, color: Color) -> Option<VectorIcon> {
    let layers = layers(
        marks(kind)?,
        Placement::WHOLE,
        &Paint::Solid(color),
        GLYPH_FLATNESS,
    )?;
    Some(VectorIcon::new(DESIGN, layers))
}

/// The flatness, in symbol units, a picture `px_per_unit` pixels to the symbol
/// unit needs: finer as it is drawn larger, so a small icon costs few vertices
/// and a large one shows no facets.
pub(crate) fn flatness_at(px_per_unit: f64) -> f64 {
    (FLATNESS_PX / px_per_unit).clamp(GLYPH_FLATNESS, MAX_FLATNESS)
}

/// `marks` placed through `placement` and painted with `paint`, their curves
/// flattened to within `flatness` symbol units.
///
/// `None` when a mark cannot be built. The table is compiled in and tested
/// whole, so that is a defect in it, and the caller falls back rather than
/// drawing part of a symbol.
pub(crate) fn layers(
    marks: &[Mark],
    placement: Placement,
    paint: &Paint,
    flatness: f64,
) -> Option<Vec<Layer>> {
    let to_design = placement.to_design();
    marks
        .iter()
        .map(|mark| {
            let (subpaths, rule) = match *mark {
                Mark::Fill { outlines, rule } => (flatten(outlines, flatness)?, rule),
                Mark::Stroke {
                    outlines,
                    width,
                    cap,
                    join,
                } => {
                    let style = StrokeStyle {
                        width,
                        cap,
                        join,
                        ..StrokeStyle::default()
                    };
                    let area = stroke_outline(
                        &flatten(outlines, flatness)?,
                        &style,
                        flatness,
                        MAX_MARK_POINTS,
                    )
                    .ok()?;
                    (area, FillRule::NonZero)
                }
            };
            Some(Layer::filled(
                paint.clone(),
                rule,
                place(&subpaths, to_design),
            ))
        })
        .collect()
}

/// Every contour of `outlines`, flattened to within `flatness` symbol units.
pub(crate) fn flatten(outlines: &[Outline], flatness: f64) -> Option<Vec<SubPath>> {
    let mut subpaths = Vec::new();
    for outline in outlines {
        match *outline {
            Outline::Path(data) => {
                subpaths.extend(parse_path_data(data, flatness, MAX_MARK_POINTS, None).ok()?);
            }
            Outline::Circle { centre, radius } => {
                // Four equal quarters, so the polygon is as symmetric as the
                // circle at any flatness: one sweep from angle zero can end on
                // an odd count, which leans a centred symbol to one side.
                let quarter = core::f64::consts::FRAC_PI_2;
                let mut points = Vec::new();
                points.push((centre.0 + radius, centre.1));
                for turn in [0.0, 1.0, 2.0, 3.0] {
                    flatten_ellipse_arc(
                        centre,
                        (radius, radius),
                        0.0,
                        turn * quarter,
                        quarter,
                        flatness,
                        &mut points,
                    );
                }
                subpaths.push(SubPath::closed(points));
            }
        }
    }
    Some(subpaths)
}

/// The symbol `kind` is drawn with, or `None` for a kind drawn with a glyph
/// of its own.
pub(crate) fn marks(kind: IconKind) -> Option<&'static [Mark]> {
    Some(match kind {
        IconKind::Settings => GEAR,
        IconKind::Appearance => APPEARANCE,
        IconKind::Wallpaper => WALLPAPER,
        IconKind::Display => DISPLAY,
        IconKind::LockScreen => LOCK,
        IconKind::Screensaver => SCREENSAVER,
        IconKind::Power => POWER,
        IconKind::Networking => GLOBE,
        IconKind::Bluetooth => BLUETOOTH,
        IconKind::Sound => SPEAKER,
        IconKind::Notifications => BELL,
        IconKind::Keyboard => KEYBOARD,
        IconKind::Mouse => MOUSE,
        IconKind::Trackpad => TRACKPAD,
        IconKind::Touchscreen => TOUCHSCREEN,
        IconKind::Printer => PRINTER,
        IconKind::Accessibility => ACCESSIBILITY,
        IconKind::Language => LANGUAGE,
        IconKind::Sharing => SHARING,
        IconKind::Users => USERS,
        IconKind::Storage => STORAGE,
        _ => return None,
    })
}

const fn fill(outlines: &'static [Outline]) -> Mark {
    Mark::Fill {
        outlines,
        rule: FillRule::NonZero,
    }
}

/// A fill whose inner contours are holes.
const fn cut(outlines: &'static [Outline]) -> Mark {
    Mark::Fill {
        outlines,
        rule: FillRule::EvenOdd,
    }
}

const fn stroke(outlines: &'static [Outline], width: f64) -> Mark {
    Mark::Stroke {
        outlines,
        width,
        cap: LineCap::Round,
        join: LineJoin::Round,
    }
}

const fn circle(x: f64, y: f64, radius: f64) -> Outline {
    Outline::Circle {
        centre: (x, y),
        radius,
    }
}

/// An eight-toothed cog around a bored hub.
const GEAR: &[Mark] = &[cut(&[
    Outline::Path(
        "M9.685 4.656L10.105 1.774A10.4 10.4 0 0 1 13.895 1.774L14.315 4.656\
         A7.7 7.7 0 0 1 15.555 5.17L17.891 3.429A10.4 10.4 0 0 1 20.571 6.109\
         L18.83 8.445A7.7 7.7 0 0 1 19.344 9.685L22.226 10.105\
         A10.4 10.4 0 0 1 22.226 13.895L19.344 14.315A7.7 7.7 0 0 1 18.83 15.555\
         L20.571 17.891A10.4 10.4 0 0 1 17.891 20.571L15.555 18.83\
         A7.7 7.7 0 0 1 14.315 19.344L13.895 22.226A10.4 10.4 0 0 1 10.105 22.226\
         L9.685 19.344A7.7 7.7 0 0 1 8.445 18.83L6.109 20.571\
         A10.4 10.4 0 0 1 3.429 17.891L5.17 15.555A7.7 7.7 0 0 1 4.656 14.315\
         L1.774 13.895A10.4 10.4 0 0 1 1.774 10.105L4.656 9.685\
         A7.7 7.7 0 0 1 5.17 8.445L3.429 6.109A10.4 10.4 0 0 1 6.109 3.429\
         L8.445 5.17A7.7 7.7 0 0 1 9.685 4.656Z",
    ),
    circle(12.0, 12.0, 3.5),
])];

/// A ring with its leading half filled: light against dark.
const APPEARANCE: &[Mark] = &[
    cut(&[circle(12.0, 12.0, 9.4), circle(12.0, 12.0, 7.4)]),
    fill(&[Outline::Path("M12 4.4A7.6 7.6 0 0 0 12 19.6Z")]),
];

/// A framed landscape under a sun.
const WALLPAPER: &[Mark] = &[
    stroke(
        &[Outline::Path(
            "M5.6 4.6H18.4A2.6 2.6 0 0 1 21 7.2V16.8A2.6 2.6 0 0 1 18.4 19.4H5.6\
             A2.6 2.6 0 0 1 3 16.8V7.2A2.6 2.6 0 0 1 5.6 4.6Z",
        )],
        2.2,
    ),
    fill(&[
        Outline::Path("M5.2 17.4L9.6 12.3L12.6 15.4L14.6 13.4L18.8 17.4Z"),
        circle(15.6, 9.2, 1.8),
    ]),
];

/// A screen on a pedestal.
const DISPLAY: &[Mark] = &[
    stroke(
        &[Outline::Path(
            "M5.2 3.8H18.8A2.2 2.2 0 0 1 21 6V14.2A2.2 2.2 0 0 1 18.8 16.4H5.2\
             A2.2 2.2 0 0 1 3 14.2V6A2.2 2.2 0 0 1 5.2 3.8Z",
        )],
        2.2,
    ),
    fill(&[Outline::Path(
        "M10.9 16.4H13.1L13.5 19.2H15.8A0.9 0.9 0 0 1 15.8 21H8.2\
         A0.9 0.9 0 0 1 8.2 19.2H10.5Z",
    )]),
];

/// A padlock with a keyhole.
const LOCK: &[Mark] = &[
    Mark::Stroke {
        outlines: &[Outline::Path("M8.2 10.6V8.2A3.8 3.8 0 0 1 15.8 8.2V10.6")],
        width: 2.2,
        cap: LineCap::Butt,
        join: LineJoin::Round,
    },
    cut(&[
        Outline::Path(
            "M7.4 10.2H16.6A2.2 2.2 0 0 1 18.8 12.4V18.6A2.2 2.2 0 0 1 16.6 20.8H7.4\
             A2.2 2.2 0 0 1 5.2 18.6V12.4A2.2 2.2 0 0 1 7.4 10.2Z",
        ),
        Outline::Path(
            "M12 13.2A1.5 1.5 0 0 1 12.8 15.96L13.1 17.9H10.9L11.2 15.96\
             A1.5 1.5 0 0 1 12 13.2Z",
        ),
    ]),
];

/// A crescent moon beside two stars.
const SCREENSAVER: &[Mark] = &[fill(&[
    Outline::Path("M9.613 4.918A8.2 8.2 0 1 0 18.931 15.084A7.2 7.2 0 0 1 9.613 4.918Z"),
    Outline::Path("M17.6 2.9Q17.95 4.85 19.9 5.2Q17.95 5.55 17.6 7.5Q17.25 5.55 15.3 5.2Q17.25 4.85 17.6 2.9Z"),
    Outline::Path("M20.4 8.5Q20.62 9.78 21.9 10Q20.62 10.22 20.4 11.5Q20.18 10.22 18.9 10Q20.18 9.78 20.4 8.5Z"),
])];

/// The power mark: a broken ring and the bar through its gap.
const POWER: &[Mark] = &[stroke(
    &[
        Outline::Path("M16.952 7.501A7.4 7.4 0 1 1 7.048 7.501"),
        Outline::Path("M12 3.4V11.6"),
    ],
    2.3,
)];

/// A globe: its rim, a meridian and the equator.
const GLOBE: &[Mark] = &[stroke(
    &[
        circle(12.0, 12.0, 9.1),
        Outline::Path(
            "M12 2.9C9.7 5.3 8.4 8.5 8.4 12C8.4 15.5 9.7 18.7 12 21.1\
             C14.3 18.7 15.6 15.5 15.6 12C15.6 8.5 14.3 5.3 12 2.9Z",
        ),
        Outline::Path("M2.9 12H21.1"),
    ],
    1.9,
)];

/// The Bluetooth rune.
const BLUETOOTH: &[Mark] = &[stroke(
    &[Outline::Path(
        "M7 7.8L16.6 16.1L12 20.4V3.6L16.6 7.9L7 16.2",
    )],
    2.0,
)];

/// A speaker sounding.
const SPEAKER: &[Mark] = &[
    fill(&[Outline::Path(
        "M4.6 9.2H6.6L10.4 5.8C10.9 5.4 11.6 5.7 11.6 6.4V17.6C11.6 18.3 10.9 18.6 \
         10.4 18.2L6.6 14.8H4.6C3.9 14.8 3.4 14.3 3.4 13.6V10.4C3.4 9.7 3.9 9.2 4.6 9.2Z",
    )]),
    stroke(
        &[
            Outline::Path("M14.028 9.172A4 4 0 0 1 14.028 14.828"),
            Outline::Path("M16.085 6.178A7.6 7.6 0 0 1 16.085 17.822"),
        ],
        2.0,
    ),
];

/// A bell over its clapper.
const BELL: &[Mark] = &[fill(&[
    Outline::Path(
        "M12 2.8C8.7 2.8 6.6 5.4 6.6 8.8V12.9L4.9 15.7C4.5 16.4 5 17.2 5.8 17.2H18.2\
         C19 17.2 19.5 16.4 19.1 15.7L17.4 12.9V8.8C17.4 5.4 15.3 2.8 12 2.8Z",
    ),
    Outline::Path("M9.5 18.4H14.5A2.5 2.5 0 0 1 9.5 18.4Z"),
])];

/// A keyboard: two rows of keys over a space bar.
const KEYBOARD: &[Mark] = &[cut(&[
    Outline::Path(
        "M4.2 5.8H19.8A2.2 2.2 0 0 1 22 8V16A2.2 2.2 0 0 1 19.8 18.2H4.2\
         A2.2 2.2 0 0 1 2 16V8A2.2 2.2 0 0 1 4.2 5.8Z",
    ),
    Outline::Path(
        "M3.8 8h3.2v2.4h-3.2zM8.2 8h3.2v2.4h-3.2zM12.6 8h3.2v2.4h-3.2zM17 8h3.2v2.4h-3.2z",
    ),
    Outline::Path(
        "M3.8 11.4h3.2v2.4h-3.2zM8.2 11.4h3.2v2.4h-3.2zM12.6 11.4h3.2v2.4h-3.2z\
         M17 11.4h3.2v2.4h-3.2z",
    ),
    Outline::Path("M7.6 14.8h8.8v1.6h-8.8z"),
])];

/// A mouse and its wheel.
const MOUSE: &[Mark] = &[cut(&[
    Outline::Path(
        "M12 2.6C15.9 2.6 18.2 5.2 18.2 9.4V14.6C18.2 18.8 15.9 21.4 12 21.4\
         C8.1 21.4 5.8 18.8 5.8 14.6V9.4C5.8 5.2 8.1 2.6 12 2.6Z",
    ),
    Outline::Path("M12 5.6A1 1 0 0 1 13 6.6V9.4A1 1 0 0 1 11 9.4V6.6A1 1 0 0 1 12 5.6Z"),
])];

/// A trackpad with its click band.
const TRACKPAD: &[Mark] = &[
    stroke(
        &[Outline::Path(
            "M5.4 4.8H18.6A2.4 2.4 0 0 1 21 7.2V16.8A2.4 2.4 0 0 1 18.6 19.2H5.4\
             A2.4 2.4 0 0 1 3 16.8V7.2A2.4 2.4 0 0 1 5.4 4.8Z",
        )],
        2.1,
    ),
    Mark::Stroke {
        outlines: &[Outline::Path("M3.8 15.3H20.2")],
        width: 1.5,
        cap: LineCap::Butt,
        join: LineJoin::Round,
    },
];

/// A touch on an upright screen.
const TOUCHSCREEN: &[Mark] = &[
    stroke(
        &[Outline::Path(
            "M7.6 2.4H16.4A2.6 2.6 0 0 1 19 5V19A2.6 2.6 0 0 1 16.4 21.6H7.6\
             A2.6 2.6 0 0 1 5 19V5A2.6 2.6 0 0 1 7.6 2.4Z",
        )],
        2.0,
    ),
    fill(&[circle(12.0, 12.0, 1.9)]),
    stroke(&[circle(12.0, 12.0, 4.1)], 1.4),
];

/// A printer feeding a sheet.
const PRINTER: &[Mark] = &[
    cut(&[
        Outline::Path(
            "M5 7.6H19A2.2 2.2 0 0 1 21.2 9.8V14.8A2.2 2.2 0 0 1 19 17H18.2V13.2H5.8V17H5\
             A2.2 2.2 0 0 1 2.8 14.8V9.8A2.2 2.2 0 0 1 5 7.6Z",
        ),
        circle(17.9, 10.4, 0.9),
    ]),
    fill(&[
        Outline::Path("M7.4 3H16.6V6.8H7.4Z"),
        Outline::Path("M7 14.4H17V20.6A0.6 0.6 0 0 1 16.4 21.2H7.6A0.6 0.6 0 0 1 7 20.6Z"),
    ]),
];

/// A figure with arms open.
const ACCESSIBILITY: &[Mark] = &[
    fill(&[circle(12.0, 5.2, 2.1)]),
    stroke(
        &[
            Outline::Path("M5.6 9.2L12 10.6L18.4 9.2"),
            Outline::Path("M12 10.6V14.4"),
            Outline::Path("M12 14.4L9.2 20.2M12 14.4L14.8 20.2"),
        ],
        2.2,
    ),
];

/// A speech bubble carrying a letter.
const LANGUAGE: &[Mark] = &[cut(&[
    Outline::Path(
        "M5.8 3H18.2C20.4 3 21.8 4.4 21.8 6.6V14C21.8 16.2 20.4 17.6 18.2 17.6H11.4\
         L6.9 21.2C6.5 21.5 5.9 21.2 5.9 20.7V17.6H5.8C3.6 17.6 2.2 16.2 2.2 14V6.6\
         C2.2 4.4 3.6 3 5.8 3Z",
    ),
    Outline::Path("M12 4.9L16.9 15.8H14.4L13.5 13.6H10.5L9.6 15.8H7.1Z"),
    Outline::Path("M12 8.6L12.85 11.5H11.15Z"),
])];

/// Three joined nodes: one thing reaching two others.
const SHARING: &[Mark] = &[
    fill(&[
        circle(17.4, 5.8, 2.7),
        circle(6.6, 12.0, 2.7),
        circle(17.4, 18.2, 2.7),
    ]),
    stroke(&[Outline::Path("M6.6 12L17.4 5.8M6.6 12L17.4 18.2")], 1.8),
];

/// Two people, one before the other.
const USERS: &[Mark] = &[fill(&[
    circle(9.2, 8.4, 3.3),
    Outline::Path(
        "M2.8 19.4C2.8 15.5 5.4 13.1 9.2 13.1C13 13.1 15.6 15.5 15.6 19.4\
         C15.6 20 15.2 20.4 14.6 20.4H3.8C3.2 20.4 2.8 20 2.8 19.4Z",
    ),
    circle(16.4, 7.6, 2.7),
    Outline::Path(
        "M17.1 11.7C19.7 11.9 21.4 14 21.4 17.2C21.4 17.7 21 18.1 20.5 18.1H17.3\
         C17 15.6 15.9 13.6 14.1 12.4C15 11.9 16 11.6 17.1 11.7Z",
    ),
])];

/// Two stacked drives, each with its lamp.
const STORAGE: &[Mark] = &[cut(&[
    Outline::Path(
        "M5 4H19A2 2 0 0 1 21 6V9.6A2 2 0 0 1 19 11.6H5A2 2 0 0 1 3 9.6V6A2 2 0 0 1 5 4Z",
    ),
    circle(17.6, 7.8, 1.15),
    Outline::Path(
        "M5 12.8H19A2 2 0 0 1 21 14.8V18.4A2 2 0 0 1 19 20.4H5A2 2 0 0 1 3 18.4V14.8\
         A2 2 0 0 1 5 12.8Z",
    ),
    circle(17.6, 16.6, 1.15),
])];
