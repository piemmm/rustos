//! Pixel-exact coverage for the Box Drawing and Block Elements ranges.
//!
//! These two ranges exist to tile. A window border has to join its neighbours
//! into one unbroken rule, and a filled block has to abut the next one with no
//! seam. An outline face delivers that only where its hairlines happen to land
//! on pixel boundaries, and at a console cell they do not: the stems fall
//! between pixels and antialias to grey, so a border renders dim and blurred
//! and a filled area shows a lighter band at every cell edge.
//!
//! Terminals therefore draw these characters geometrically rather than from
//! the face, and so does this generator. Every glyph here is whole covered
//! pixels computed from the cell, which keeps it crisp at any cell size and —
//! because the console magnifies by whole factors — at every glyph scale too.
//! Only characters whose whole purpose is to tile are synthesised; the face
//! still supplies every other scalar, including the geometric shapes either
//! side of these ranges.

use core::ops::Range;

/// Full coverage of one pixel, in the atlas's 4-bit alpha.
const FULL: u8 = 15;

/// The Box Drawing block: rules, corners, tees, crosses, arcs and diagonals.
const BOX_DRAWING: Range<u32> = 0x2500..0x2580;

/// The Block Elements block: halves, eighths, quadrants and shades.
const BLOCK_ELEMENTS: Range<u32> = 0x2580..0x25A0;

/// The coverage for `code` in a `width`×`height` cell, or `None` where the
/// face supplies the glyph.
pub fn coverage(code: u32, width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    if BOX_DRAWING.contains(&code) {
        return Some(draw_box(code, width, height));
    }
    if BLOCK_ELEMENTS.contains(&code) {
        return Some(draw_block(code, width, height));
    }
    None
}

// --- The cell -----------------------------------------------------------

/// A cell of coverage being drawn into.
struct Pen {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Pen {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; (width * height) as usize],
        }
    }

    fn fill(&mut self, x: Range<u32>, y: Range<u32>) {
        self.shade(x, y, FULL);
    }

    fn shade(&mut self, x: Range<u32>, y: Range<u32>, coverage: u8) {
        for row in y.start..y.end.min(self.height) {
            for column in x.start..x.end.min(self.width) {
                self.pixels[(row * self.width + column) as usize] = coverage;
            }
        }
    }
}

// --- Box drawing --------------------------------------------------------

/// How heavily one arm of a box-drawing character is drawn.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Weight {
    None,
    Light,
    Heavy,
    Double,
}

use Weight::{Double as D, Heavy as H, Light as L, None as N};

/// A box-drawing character as its four arms, read clockwise from the top,
/// plus the number of pieces a dashed rule is broken into.
#[derive(Clone, Copy)]
struct Arms {
    up: Weight,
    right: Weight,
    down: Weight,
    left: Weight,
    dashes: u32,
}

const fn arms(up: Weight, right: Weight, down: Weight, left: Weight) -> Shape {
    Shape::Arms(Arms {
        up,
        right,
        down,
        left,
        dashes: 0,
    })
}

const fn dashed(up: Weight, right: Weight, down: Weight, left: Weight, dashes: u32) -> Shape {
    Shape::Arms(Arms {
        up,
        right,
        down,
        left,
        dashes,
    })
}

const fn arc(up: Weight, right: Weight, down: Weight, left: Weight) -> Shape {
    Shape::Arc(Arms {
        up,
        right,
        down,
        left,
        dashes: 0,
    })
}

/// What a box-drawing character draws.
#[derive(Clone, Copy)]
enum Shape {
    Arms(Arms),
    /// A corner rounded by a quarter turn between its two arms.
    Arc(Arms),
    /// A corner-to-corner diagonal, falling (`╲`) and/or rising (`╱`).
    Diagonal {
        falling: bool,
        rising: bool,
    },
}

/// Where strokes sit in the cell.
///
/// Light, heavy and double rules all share one centre, so any two of them
/// meet cleanly where they cross and a rule joins its neighbour across a cell
/// boundary.
struct Strokes {
    width: u32,
    height: u32,
    light: u32,
    heavy: u32,
    /// Left edge of a vertical light stroke; top edge of a horizontal one.
    vertical: u32,
    horizontal: u32,
    /// Extent of a double rule, both rails and the gap between them.
    double: u32,
}

impl Strokes {
    fn for_cell(width: u32, height: u32) -> Self {
        // One pixel at the conventional 8×16 cell, thickening in proportion,
        // so line-art keeps the weight of the text beside it as the cell grows.
        let light = (height / 16).max(1);
        // Two rails and a gap, so a double rule still reads as double at the
        // smallest cell instead of closing up into a heavy one.
        let double = light * 3;
        Self {
            width,
            height,
            light,
            heavy: light * 2,
            vertical: centred(width, light),
            horizontal: centred(height, light),
            double,
        }
    }

    fn thickness(&self, weight: Weight) -> u32 {
        match weight {
            Weight::None => 0,
            Weight::Light | Weight::Double => self.light,
            Weight::Heavy => self.heavy,
        }
    }

    /// Columns a vertical stroke of `weight` occupies, centred on the light
    /// rule so a mixed-weight junction still joins.
    fn column(&self, weight: Weight) -> Range<u32> {
        let thickness = self.thickness(weight);
        let start = match weight {
            Weight::Heavy => centred(self.width, thickness),
            _ => self.vertical,
        };
        start..start + thickness
    }

    fn row(&self, weight: Weight) -> Range<u32> {
        let thickness = self.thickness(weight);
        let start = match weight {
            Weight::Heavy => centred(self.height, thickness),
            _ => self.horizontal,
        };
        start..start + thickness
    }
}

fn centred(extent: u32, thickness: u32) -> u32 {
    extent.saturating_sub(thickness) / 2
}

fn draw_box(code: u32, width: u32, height: u32) -> Vec<u8> {
    let strokes = Strokes::for_cell(width, height);
    let mut pen = Pen::new(width, height);
    match BOX_SHAPES[(code - BOX_DRAWING.start) as usize] {
        Shape::Arms(spec) => {
            draw_solid_arms(&mut pen, &strokes, spec);
            draw_double_rails(&mut pen, &strokes, spec);
        }
        Shape::Arc(spec) => draw_arc(&mut pen, &strokes, spec),
        Shape::Diagonal { falling, rising } => draw_diagonals(&mut pen, &strokes, falling, rising),
    }
    pen.pixels
}

/// Draw the light and heavy arms, each running from the cell centre out to
/// its own edge — reaching the edge is what joins one cell's rule to the next.
fn draw_solid_arms(pen: &mut Pen, strokes: &Strokes, spec: Arms) {
    let mid_column = strokes.column(Weight::Light);
    let mid_row = strokes.row(Weight::Light);
    for (weight, side) in [
        (spec.up, Side::Up),
        (spec.right, Side::Right),
        (spec.down, Side::Down),
        (spec.left, Side::Left),
    ] {
        if matches!(weight, Weight::None | Weight::Double) {
            continue;
        }
        match side {
            Side::Up | Side::Down => {
                let span = match side {
                    Side::Up => 0..mid_row.end,
                    _ => mid_row.start..strokes.height,
                };
                for piece in pieces(span, spec.dashes, strokes.height) {
                    pen.fill(strokes.column(weight), piece);
                }
            }
            Side::Left | Side::Right => {
                let span = match side {
                    Side::Left => 0..mid_column.end,
                    _ => mid_column.start..strokes.width,
                };
                for piece in pieces(span, spec.dashes, strokes.width) {
                    pen.fill(piece, strokes.row(weight));
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Side {
    Up,
    Right,
    Down,
    Left,
}

/// Break `span` into the parts of a rule dashed into `dashes` pieces across
/// `extent`, or leave it whole when the rule is unbroken.
///
/// The slots are laid out over the whole cell rather than per arm, so the two
/// halves of a dashed rule line up with each other and the pattern continues
/// across cells. A dash keeps roughly two thirds of its slot, and never less
/// than one pixel: a cell too small to show the gaps draws a solid rule rather
/// than a row of nothing.
fn pieces(span: Range<u32>, dashes: u32, extent: u32) -> Vec<Range<u32>> {
    if dashes == 0 {
        return vec![span];
    }
    (0..dashes)
        .filter_map(|piece| {
            let slot = (piece * extent / dashes)..((piece + 1) * extent / dashes);
            let gap = ((slot.end - slot.start) / 3).max(1);
            let start = slot.start.max(span.start);
            let end = (slot.end.saturating_sub(gap))
                .max(slot.start + 1)
                .min(span.end);
            (start < end).then_some(start..end)
        })
        .collect()
}

/// Draw the double-rule arms as the two rails that outline them.
///
/// A double rule is the boundary of the region its arms sweep: two parallel
/// rails along a straight run, and at a junction exactly the turns and notches
/// that outline makes. Deriving it that way — rather than case-splitting the
/// twenty-nine junctions — is what makes every corner, tee and cross agree.
fn draw_double_rails(pen: &mut Pen, strokes: &Strokes, spec: Arms) {
    if ![spec.up, spec.right, spec.down, spec.left].contains(&Weight::Double) {
        return;
    }
    let rail = strokes.light;
    // The region is measured on a grid grown by one rail all round, so an arm
    // running off the cell is continuous with its neighbour and only genuine
    // ends are outlined.
    let pad = rail;
    let (width, height) = (strokes.width + 2 * pad, strokes.height + 2 * pad);
    let column = centred(strokes.width, strokes.double) + pad;
    let row = centred(strokes.height, strokes.double) + pad;
    let band_x = column..column + strokes.double;
    let band_y = row..row + strokes.double;
    // How far back towards the middle an arm sweeps. Where the rule it crosses
    // is also double, it sweeps the whole of that rule's width, so the two
    // overlap completely and the junction's notches fall out of the outline.
    // Where it crosses a single rule instead, it stops on that rule, which
    // hides the end of the sweep under the line already drawn there.
    let doubled =
        |first: Weight, second: Weight| first == Weight::Double || second == Weight::Double;
    let back_x = if doubled(spec.up, spec.down) {
        band_x.clone()
    } else {
        strokes.vertical + pad..strokes.vertical + pad + rail
    };
    let back_y = if doubled(spec.left, spec.right) {
        band_y.clone()
    } else {
        strokes.horizontal + pad..strokes.horizontal + pad + rail
    };
    let mut region = vec![false; (width * height) as usize];
    let mut sweep = |x: Range<u32>, y: Range<u32>| {
        for py in y.clone() {
            for px in x.clone() {
                region[(py * width + px) as usize] = true;
            }
        }
    };
    if spec.up == Weight::Double {
        sweep(band_x.clone(), 0..back_y.end);
    }
    if spec.down == Weight::Double {
        sweep(band_x.clone(), back_y.start..height);
    }
    if spec.left == Weight::Double {
        sweep(0..back_x.end, band_y.clone());
    }
    if spec.right == Weight::Double {
        sweep(back_x.start..width, band_y.clone());
    }
    let inside = |x: u32, y: u32| region[(y * width + x) as usize];
    for y in pad..height - pad {
        for x in pad..width - pad {
            if !inside(x, y) {
                continue;
            }
            // A rail pixel is one the region does not surround to the depth of
            // a rail: the outline, whatever shape the junction happens to be.
            // The padding guarantees the whole neighbourhood is on the grid.
            let surrounded =
                (y - rail..=y + rail).all(|ny| (x - rail..=x + rail).all(|nx| inside(nx, ny)));
            if !surrounded {
                pen.fill(x - pad..x - pad + 1, y - pad..y - pad + 1);
            }
        }
    }
}

/// Draw a rounded corner: the two arms, joined by a quarter turn.
///
/// The turn is a quarter circle centred a radius along each arm, so it leaves
/// the arms tangentially and the corner reads as rounded rather than mitred.
fn draw_arc(pen: &mut Pen, strokes: &Strokes, spec: Arms) {
    let column = strokes.column(Weight::Light);
    let row = strokes.row(Weight::Light);
    let radius = i64::from(strokes.vertical.min(strokes.horizontal).max(1));
    let upward = spec.up != Weight::None;
    let leftward = spec.left != Weight::None;
    let centre_x = i64::from(column.start) + if leftward { -radius } else { radius };
    let centre_y = i64::from(row.start) + if upward { -radius } else { radius };
    // The straight parts run from their own edge up to where the turn begins.
    let arm_start = |centre: i64| u32::try_from(centre.max(0)).unwrap_or(u32::MAX);
    if upward {
        pen.fill(column.clone(), 0..arm_start(centre_y));
    } else {
        pen.fill(column.clone(), arm_start(centre_y + 1)..strokes.height);
    }
    if leftward {
        pen.fill(0..arm_start(centre_x), row.clone());
    } else {
        pen.fill(arm_start(centre_x + 1)..strokes.width, row.clone());
    }
    // The turn itself: the band of pixels a radius from that centre, in the
    // one quadrant facing back towards the two arms.
    let outer_radius = radius + i64::from(strokes.light);
    let band = (radius * radius)..(outer_radius * outer_radius);
    for y in 0..strokes.height {
        for x in 0..strokes.width {
            let dx = i64::from(x) - centre_x;
            let dy = i64::from(y) - centre_y;
            // The quadrant test includes the two axes through the centre:
            // those pixels are where the turn meets its arms, and dropping
            // them would break the curve at both ends.
            let facing_arms =
                if leftward { dx >= 0 } else { dx <= 0 } && if upward { dy >= 0 } else { dy <= 0 };
            if facing_arms && band.contains(&(dx * dx + dy * dy)) {
                pen.fill(x..x + 1, y..y + 1);
            }
        }
    }
}

/// Draw the corner-to-corner diagonals.
fn draw_diagonals(pen: &mut Pen, strokes: &Strokes, falling: bool, rising: bool) {
    let thickness = strokes.light;
    for y in 0..strokes.height {
        let along = (y * strokes.width) / strokes.height;
        if falling {
            pen.fill(along..along + thickness, y..y + 1);
        }
        if rising {
            let mirrored = strokes.width.saturating_sub(along + thickness);
            pen.fill(mirrored..mirrored + thickness, y..y + 1);
        }
    }
}

// --- Block elements -----------------------------------------------------

/// `eighths` eighths of `extent`, measured from the near edge.
///
/// A block that grows from the top or the left keeps the whole pixel it does
/// not quite reach; its opposite number ([`far_eighths`]) claims that pixel
/// instead, so a pair of complementary blocks partitions a cell of any size
/// exactly — no overlap, and no uncovered line between them. Never nothing,
/// so all eight steps stay distinguishable in a very small cell.
fn near_eighths(eighths: u32, extent: u32) -> u32 {
    ((extent * eighths) / 8).clamp(1, extent)
}

/// `eighths` eighths of `extent`, measured from the far edge.
fn far_eighths(eighths: u32, extent: u32) -> u32 {
    (extent * eighths).div_ceil(8).clamp(1, extent)
}

fn draw_block(code: u32, width: u32, height: u32) -> Vec<u8> {
    let mut pen = Pen::new(width, height);
    let (mid_x, mid_y) = (near_eighths(4, width), near_eighths(4, height));
    match code {
        0x2580 => pen.fill(0..width, 0..mid_y),
        // Lower one-eighth block through lower seven-eighths block. The half
        // step of this run is `▄`, and it meets `▀` exactly.
        0x2581..=0x2587 => {
            let eighths = code - 0x2580;
            pen.fill(0..width, height - far_eighths(eighths, height)..height);
        }
        0x2588 => pen.fill(0..width, 0..height),
        // Left seven-eighths block through left one-eighth block.
        0x2589..=0x258F => {
            let eighths = 8 - (code - 0x2588);
            pen.fill(0..near_eighths(eighths, width), 0..height);
        }
        0x2590 => pen.fill(mid_x..width, 0..height),
        // A shade is a uniform partial coverage, not a stipple: it is the same
        // tone, and unlike a dither pattern it tiles exactly at any cell size
        // and under any magnification.
        0x2591 => pen.shade(0..width, 0..height, FULL / 4),
        0x2592 => pen.shade(0..width, 0..height, FULL / 2),
        0x2593 => pen.shade(0..width, 0..height, FULL * 3 / 4),
        0x2594 => pen.fill(0..width, 0..near_eighths(1, height)),
        0x2595 => pen.fill(width - far_eighths(1, width)..width, 0..height),
        0x2596..=0x259F => {
            for (index, present) in QUADRANTS[(code - 0x2596) as usize].iter().enumerate() {
                if !present {
                    continue;
                }
                let x = if index % 2 == 0 {
                    0..mid_x
                } else {
                    mid_x..width
                };
                let y = if index < 2 { 0..mid_y } else { mid_y..height };
                pen.fill(x, y);
            }
        }
        _ => {}
    }
    pen.pixels
}

/// Which corners each quadrant character covers: top-left, top-right,
/// bottom-left, bottom-right.
const QUADRANTS: [[bool; 4]; 10] = [
    [false, false, true, false], // ▖
    [false, false, false, true], // ▗
    [true, false, false, false], // ▘
    [true, false, true, true],   // ▙
    [true, false, false, true],  // ▚
    [true, true, true, false],   // ▛
    [true, true, false, true],   // ▜
    [false, true, false, false], // ▝
    [false, true, true, false],  // ▞
    [false, true, true, true],   // ▟
];

// --- The repertoire -----------------------------------------------------

/// Every Box Drawing scalar as the shape it draws, in codepoint order from
/// U+2500. The names are the Unicode ones, shortened.
#[rustfmt::skip]
const BOX_SHAPES: [Shape; 128] = [
    arms(N, L, N, L),           // ─ light horizontal
    arms(N, H, N, H),           // ━ heavy horizontal
    arms(L, N, L, N),           // │ light vertical
    arms(H, N, H, N),           // ┃ heavy vertical
    dashed(N, L, N, L, 3),      // ┄ light triple dash horizontal
    dashed(N, H, N, H, 3),      // ┅ heavy triple dash horizontal
    dashed(L, N, L, N, 3),      // ┆ light triple dash vertical
    dashed(H, N, H, N, 3),      // ┇ heavy triple dash vertical
    dashed(N, L, N, L, 4),      // ┈ light quadruple dash horizontal
    dashed(N, H, N, H, 4),      // ┉ heavy quadruple dash horizontal
    dashed(L, N, L, N, 4),      // ┊ light quadruple dash vertical
    dashed(H, N, H, N, 4),      // ┋ heavy quadruple dash vertical
    arms(N, L, L, N),           // ┌ light down and right
    arms(N, H, L, N),           // ┍ down light and right heavy
    arms(N, L, H, N),           // ┎ down heavy and right light
    arms(N, H, H, N),           // ┏ heavy down and right
    arms(N, N, L, L),           // ┐ light down and left
    arms(N, N, L, H),           // ┑ down light and left heavy
    arms(N, N, H, L),           // ┒ down heavy and left light
    arms(N, N, H, H),           // ┓ heavy down and left
    arms(L, L, N, N),           // └ light up and right
    arms(L, H, N, N),           // ┕ up light and right heavy
    arms(H, L, N, N),           // ┖ up heavy and right light
    arms(H, H, N, N),           // ┗ heavy up and right
    arms(L, N, N, L),           // ┘ light up and left
    arms(L, N, N, H),           // ┙ up light and left heavy
    arms(H, N, N, L),           // ┚ up heavy and left light
    arms(H, N, N, H),           // ┛ heavy up and left
    arms(L, L, L, N),           // ├ light vertical and right
    arms(L, H, L, N),           // ┝ vertical light and right heavy
    arms(H, L, L, N),           // ┞ up heavy and right down light
    arms(L, L, H, N),           // ┟ down heavy and right up light
    arms(H, L, H, N),           // ┠ vertical heavy and right light
    arms(H, H, L, N),           // ┡ down light and right up heavy
    arms(L, H, H, N),           // ┢ up light and right down heavy
    arms(H, H, H, N),           // ┣ heavy vertical and right
    arms(L, N, L, L),           // ┤ light vertical and left
    arms(L, N, L, H),           // ┥ vertical light and left heavy
    arms(H, N, L, L),           // ┦ up heavy and left down light
    arms(L, N, H, L),           // ┧ down heavy and left up light
    arms(H, N, H, L),           // ┨ vertical heavy and left light
    arms(H, N, L, H),           // ┩ down light and left up heavy
    arms(L, N, H, H),           // ┪ up light and left down heavy
    arms(H, N, H, H),           // ┫ heavy vertical and left
    arms(N, L, L, L),           // ┬ light down and horizontal
    arms(N, L, L, H),           // ┭ left heavy and right down light
    arms(N, H, L, L),           // ┮ right heavy and left down light
    arms(N, H, L, H),           // ┯ down light and horizontal heavy
    arms(N, L, H, L),           // ┰ down heavy and horizontal light
    arms(N, L, H, H),           // ┱ right light and left down heavy
    arms(N, H, H, L),           // ┲ left light and right down heavy
    arms(N, H, H, H),           // ┳ heavy down and horizontal
    arms(L, L, N, L),           // ┴ light up and horizontal
    arms(L, L, N, H),           // ┵ left heavy and right up light
    arms(L, H, N, L),           // ┶ right heavy and left up light
    arms(L, H, N, H),           // ┷ up light and horizontal heavy
    arms(H, L, N, L),           // ┸ up heavy and horizontal light
    arms(H, L, N, H),           // ┹ right light and left up heavy
    arms(H, H, N, L),           // ┺ left light and right up heavy
    arms(H, H, N, H),           // ┻ heavy up and horizontal
    arms(L, L, L, L),           // ┼ light vertical and horizontal
    arms(L, L, L, H),           // ┽ left heavy and right vertical light
    arms(L, H, L, L),           // ┾ right heavy and left vertical light
    arms(L, H, L, H),           // ┿ vertical light and horizontal heavy
    arms(H, L, L, L),           // ╀ up heavy and down horizontal light
    arms(L, L, H, L),           // ╁ down heavy and up horizontal light
    arms(H, L, H, L),           // ╂ vertical heavy and horizontal light
    arms(H, L, L, H),           // ╃ left up heavy and right down light
    arms(H, H, L, L),           // ╄ right up heavy and left down light
    arms(L, L, H, H),           // ╅ left down heavy and right up light
    arms(L, H, H, L),           // ╆ right down heavy and left up light
    arms(H, H, L, H),           // ╇ down light and up horizontal heavy
    arms(L, H, H, H),           // ╈ up light and down horizontal heavy
    arms(H, L, H, H),           // ╉ right light and left vertical heavy
    arms(H, H, H, L),           // ╊ left light and right vertical heavy
    arms(H, H, H, H),           // ╋ heavy vertical and horizontal
    dashed(N, L, N, L, 2),      // ╌ light double dash horizontal
    dashed(N, H, N, H, 2),      // ╍ heavy double dash horizontal
    dashed(L, N, L, N, 2),      // ╎ light double dash vertical
    dashed(H, N, H, N, 2),      // ╏ heavy double dash vertical
    arms(N, D, N, D),           // ═ double horizontal
    arms(D, N, D, N),           // ║ double vertical
    arms(N, D, L, N),           // ╒ down single and right double
    arms(N, L, D, N),           // ╓ down double and right single
    arms(N, D, D, N),           // ╔ double down and right
    arms(N, N, L, D),           // ╕ down single and left double
    arms(N, N, D, L),           // ╖ down double and left single
    arms(N, N, D, D),           // ╗ double down and left
    arms(L, D, N, N),           // ╘ up single and right double
    arms(D, L, N, N),           // ╙ up double and right single
    arms(D, D, N, N),           // ╚ double up and right
    arms(L, N, N, D),           // ╛ up single and left double
    arms(D, N, N, L),           // ╜ up double and left single
    arms(D, N, N, D),           // ╝ double up and left
    arms(L, D, L, N),           // ╞ vertical single and right double
    arms(D, L, D, N),           // ╟ vertical double and right single
    arms(D, D, D, N),           // ╠ double vertical and right
    arms(L, N, L, D),           // ╡ vertical single and left double
    arms(D, N, D, L),           // ╢ vertical double and left single
    arms(D, N, D, D),           // ╣ double vertical and left
    arms(N, D, L, D),           // ╤ down single and horizontal double
    arms(N, L, D, L),           // ╥ down double and horizontal single
    arms(N, D, D, D),           // ╦ double down and horizontal
    arms(L, D, N, D),           // ╧ up single and horizontal double
    arms(D, L, N, L),           // ╨ up double and horizontal single
    arms(D, D, N, D),           // ╩ double up and horizontal
    arms(L, D, L, D),           // ╪ vertical single and horizontal double
    arms(D, L, D, L),           // ╫ vertical double and horizontal single
    arms(D, D, D, D),           // ╬ double vertical and horizontal
    arc(N, L, L, N),            // ╭ light arc down and right
    arc(N, N, L, L),            // ╮ light arc down and left
    arc(L, N, N, L),            // ╯ light arc up and left
    arc(L, L, N, N),            // ╰ light arc up and right
    Shape::Diagonal { falling: false, rising: true },  // ╱
    Shape::Diagonal { falling: true, rising: false },  // ╲
    Shape::Diagonal { falling: true, rising: true },   // ╳
    arms(N, N, N, L),           // ╴ light left
    arms(L, N, N, N),           // ╵ light up
    arms(N, L, N, N),           // ╶ light right
    arms(N, N, L, N),           // ╷ light down
    arms(N, N, N, H),           // ╸ heavy left
    arms(H, N, N, N),           // ╹ heavy up
    arms(N, H, N, N),           // ╺ heavy right
    arms(N, N, H, N),           // ╻ heavy down
    arms(N, H, N, L),           // ╼ light left and heavy right
    arms(L, N, H, N),           // ╽ light up and heavy down
    arms(N, L, N, H),           // ╾ heavy left and light right
    arms(H, N, L, N),           // ╿ heavy up and light down
];

#[cfg(test)]
#[path = "font_lineart_tests.rs"]
mod tests;
