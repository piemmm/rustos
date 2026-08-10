//! A complete, replaceable set of pointer cursors — one [`VectorCursor`] per
//! [`CursorKind`] — and the built-in default set.
//!
//! A [`CursorTheme`] is the cursor analogue of `lib/theme`'s palette: a fixed
//! record with one cursor per kind, so a lookup can never miss. The theme names a cursor by [`CursorKind`]; this crate turns that
//! name into actual scalable, colourful artwork. Because a `CursorTheme` is
//! plain data built from [`VectorCursor`]s, an entirely different cursor set
//! is just a different `CursorTheme` — that is the "replaceable with other
//! cursor sets" requirement (`PLAN.md` Stage 7), realised without any change
//! to the window manager.
//!
//! The built-in set ([`CursorTheme::builtin`]) draws each cursor as a light
//! body over a darker outline so it stays legible on any background, and the
//! busy cursor is genuinely two-tone, exercising the colour capability the
//! representation provides.

use alloc::vec::Vec;

use tairix_raster::Color;
use tairix_theme::CursorKind;

use crate::vector::{Shape, VectorCursor, Vertex};

/// The design-grid side every built-in cursor is authored on.
const DESIGN: u32 = 32;

/// Near-black outline drawn behind each body for contrast on light backgrounds.
const OUTLINE: Color = Color::rgb(24, 24, 32);
/// Near-white body, the legible foreground on dark backgrounds.
const BODY: Color = Color::rgb(245, 246, 250);
/// The busy cursor's primary tone (a calm blue).
const BUSY_PRIMARY: Color = Color::rgb(64, 140, 250);
/// The busy cursor's secondary tone (a warm amber) — proof the format is
/// colourful, not a one-bit mask.
const BUSY_SECONDARY: Color = Color::rgb(250, 198, 64);

/// One [`VectorCursor`] per [`CursorKind`].
///
/// Stored as fixed fields rather than a map so every kind is always defined
/// and [`cursor`](Self::cursor) is total.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorTheme {
    arrow: VectorCursor,
    text: VectorCursor,
    pointer: VectorCursor,
    move_: VectorCursor,
    busy: VectorCursor,
}

impl CursorTheme {
    /// Construct a cursor theme from one cursor per kind.
    #[must_use]
    pub fn new(
        arrow: VectorCursor,
        text: VectorCursor,
        pointer: VectorCursor,
        move_: VectorCursor,
        busy: VectorCursor,
    ) -> Self {
        Self {
            arrow,
            text,
            pointer,
            move_,
            busy,
        }
    }

    /// The cursor for `kind`. Total — every kind always resolves.
    #[must_use]
    pub fn cursor(&self, kind: CursorKind) -> &VectorCursor {
        match kind {
            CursorKind::Arrow => &self.arrow,
            CursorKind::Text => &self.text,
            CursorKind::Pointer => &self.pointer,
            CursorKind::Move => &self.move_,
            CursorKind::Busy => &self.busy,
        }
    }

    /// The built-in default cursor set: a light body over a dark outline for
    /// every kind, with a two-tone busy spinner.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            arrow: builtin_arrow(),
            text: builtin_text(),
            pointer: builtin_pointer(),
            move_: builtin_move(),
            busy: builtin_busy(),
        }
    }
}

/// The classic top-left arrow. Hotspot at the tip `(0, 0)`.
fn builtin_arrow() -> VectorCursor {
    const SILHOUETTE: &[(i32, i32)] = &[
        (1, 1),
        (1, 23),
        (7, 18),
        (11, 27),
        (14, 26),
        (10, 17),
        (18, 17),
    ];
    outlined(0, 0, SILHOUETTE)
}

/// The I-beam shown over editable text. Hotspot at the centre.
fn builtin_text() -> VectorCursor {
    const SILHOUETTE: &[(i32, i32)] = &[
        (11, 6),
        (21, 6),
        (21, 9),
        (18, 9),
        (18, 23),
        (21, 23),
        (21, 26),
        (11, 26),
        (11, 23),
        (14, 23),
        (14, 9),
        (11, 9),
    ];
    outlined(16, 16, SILHOUETTE)
}

/// A pointing hand for clickable controls. Hotspot at the fingertip.
fn builtin_pointer() -> VectorCursor {
    const SILHOUETTE: &[(i32, i32)] = &[
        (10, 3),
        (13, 3),
        (13, 13),
        (16, 13),
        (16, 15),
        (19, 15),
        (19, 16),
        (22, 16),
        (22, 27),
        (10, 27),
    ];
    outlined(11, 3, SILHOUETTE)
}

/// The four-way move cursor shown while dragging. Hotspot at the centre.
fn builtin_move() -> VectorCursor {
    const SILHOUETTE: &[(i32, i32)] = &[
        (16, 1),
        (21, 6),
        (18, 6),
        (18, 14),
        (26, 14),
        (26, 11),
        (31, 16),
        (26, 21),
        (26, 18),
        (18, 18),
        (18, 26),
        (21, 26),
        (16, 31),
        (11, 26),
        (14, 26),
        (14, 18),
        (6, 18),
        (6, 21),
        (1, 16),
        (6, 11),
        (6, 14),
        (14, 14),
        (14, 6),
        (11, 6),
    ];
    outlined(16, 16, SILHOUETTE)
}

/// The busy/wait cursor: a two-tone disc. Hotspot at the centre.
fn builtin_busy() -> VectorCursor {
    const RING: &[(i32, i32)] = &[
        (30, 16),
        (28, 9),
        (23, 4),
        (16, 2),
        (9, 4),
        (4, 9),
        (2, 16),
        (4, 23),
        (9, 28),
        (16, 30),
        (23, 28),
        (28, 23),
    ];
    let outer = Shape::from_points(BUSY_PRIMARY, RING);
    let inner = Shape::new(BUSY_SECONDARY, scaled_about(RING, 16, 16, 1, 2));
    let shapes = alloc::vec![outer, inner];
    VectorCursor::new(DESIGN, 16, 16, shapes)
}

/// Build a two-layer cursor: a dark [`OUTLINE`] enlarged about the
/// silhouette's centroid, then the light [`BODY`] at its authored size. The
/// enlarged layer shows through as a uniform border (one
/// outline mechanism, not a per-cursor hack).
fn outlined(hotspot_x: i32, hotspot_y: i32, silhouette: &[(i32, i32)]) -> VectorCursor {
    let (cx, cy) = centroid(silhouette);
    let outline = Shape::new(OUTLINE, scaled_about(silhouette, cx, cy, 6, 5));
    let body = Shape::from_points(BODY, silhouette);
    let shapes = alloc::vec![outline, body];
    VectorCursor::new(DESIGN, hotspot_x, hotspot_y, shapes)
}

/// The integer centroid (mean vertex) of a polygon, used as the scaling
/// centre for the outline layer. An empty polygon centres on the origin.
fn centroid(points: &[(i32, i32)]) -> (i32, i32) {
    let count = i32::try_from(points.len()).unwrap_or(1).max(1);
    let sum = points.iter().fold((0_i64, 0_i64), |(sx, sy), &(x, y)| {
        (sx + i64::from(x), sy + i64::from(y))
    });
    let cx = i32::try_from(sum.0 / i64::from(count)).unwrap_or(0);
    let cy = i32::try_from(sum.1 / i64::from(count)).unwrap_or(0);
    (cx, cy)
}

/// Scale `points` about `(cx, cy)` by the rational factor `num/den`,
/// returning design-grid [`Vertex`]es. A zero denominator leaves the points
/// unscaled rather than dividing by zero.
fn scaled_about(points: &[(i32, i32)], cx: i32, cy: i32, num: i32, den: i32) -> Vec<Vertex> {
    let den = if den == 0 { 1 } else { den };
    let scale = |c: i32, centre: i32| -> i32 {
        let offset = i64::from(c - centre) * i64::from(num) / i64::from(den);
        let value = i64::from(centre) + offset;
        i32::try_from(value).unwrap_or(centre)
    };
    points
        .iter()
        .map(|&(x, y)| Vertex::new(scale(x, cx), scale(y, cy)))
        .collect()
}
