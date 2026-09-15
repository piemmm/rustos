//! Drawing the board.
//!
//! Everything is composed from the shared raster primitives against the active
//! theme, so the game re-themes and re-densifies with the rest of the desktop
//! and owns no second rounded-rect, gradient, or text path. The one thing it
//! *does* own is its number palette: the eight adjacency colours are the game's
//! own visual identity, tuned per appearance, exactly as the terminal owns its
//! ANSI scheme rather than asking the desktop theme for sixteen colours it has
//! no opinion about.
//!
//! A frame resolves the theme into a [`Skin`] once and then draws cells from
//! it, so the per-cell path performs no palette lookups and no allocation.
//!
//! Motion is applied here rather than baked into the board: a covered cell
//! being revealed draws its finished face with the *lid* still over it, sliding
//! and fading off. That is why a suppressed animation needs no second code
//! path — with no motion the lid is simply already gone.

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface, SUBPIXEL};
use tairix_theme::{Appearance, Rgba, Theme};

use crate::anim::{CellMotion, Motion, WaveKind};
use crate::board::{Board, Coord, Cover, Phase};
use crate::layout::Layout;

/// The eight adjacency colours on a dark surface.
///
/// The classic assignment — blue, green, red, navy, maroon, teal, black, grey —
/// lifted off a light background onto a dark one, so `7` and `8` stay legible
/// where the original's black-on-grey would vanish.
const NUMBERS_DARK: [Color; 8] = [
    Color::rgb(0x6E, 0xA8, 0xFE),
    Color::rgb(0x5F, 0xD1, 0x8A),
    Color::rgb(0xFF, 0x7B, 0x72),
    Color::rgb(0xB4, 0x9C, 0xFF),
    Color::rgb(0xFF, 0xB0, 0x6B),
    Color::rgb(0x59, 0xD5, 0xD5),
    Color::rgb(0xF0, 0xC0, 0xE8),
    Color::rgb(0xC9, 0xD1, 0xD9),
];

/// The eight adjacency colours on a light surface: the classic assignment,
/// darkened enough to hold contrast against a pale tile.
const NUMBERS_LIGHT: [Color; 8] = [
    Color::rgb(0x17, 0x50, 0xC4),
    Color::rgb(0x1B, 0x74, 0x3C),
    Color::rgb(0xC0, 0x27, 0x1B),
    Color::rgb(0x50, 0x2C, 0xA8),
    Color::rgb(0x9A, 0x4B, 0x06),
    Color::rgb(0x0D, 0x70, 0x77),
    Color::rgb(0x8A, 0x2B, 0x74),
    Color::rgb(0x30, 0x36, 0x3D),
];

/// Half the difference between the top and bottom of a covered tile's bevel,
/// in colour levels. Positive, on both appearances: the light falls from above
/// whatever the theme.
const BEVEL: i16 = 15;
/// How far a covered tile stands clear of the surface it sits on, in colour
/// levels. Without it the board reads as a flat grid rather than as tiles.
///
/// A light theme needs the larger step: its surfaces are already close to white,
/// so a lift *upward* has nowhere to go and the tile is cut downward instead.
const LIFT_DARK: i16 = 30;
/// The same step on a light appearance, taken downward.
const LIFT_LIGHT: i16 = 38;
/// The shockwave's furthest reach past a struck cell, as a per-mille of the
/// cell side.
const SHOCKWAVE_REACH: u32 = 900;
/// How much of the lit colour an unlit segment carries, per mille.
///
/// An unlit segment is the same lamp with nothing behind it: faintly warm, and
/// far below the plate's own contrast with the lit one. Enough that a figure
/// reads as a figure rather than as disconnected strokes, and no more — at any
/// weight worth noticing, every digit starts to read as an eight.
const UNLIT_PERMILLE: u16 = 55;

/// Vertices used to draw a disc. Enough that a mine reads round at every cell
/// size the layout allows.
const DISC_STEPS: usize = 24;
/// The rejection shake's travel, as a per-mille of the cell side.
const SHAKE_REACH: u32 = 180;

/// Every colour and length one frame draws with, resolved once.
#[derive(Copy, Clone, Debug)]
pub struct Skin {
    /// The window's background.
    pub ground: Color,
    /// The header band.
    pub header: Color,
    /// A readout's recessed plate.
    pub readout: Color,
    /// A lit readout segment.
    pub lit: Color,
    /// An unlit readout segment, which is what makes a digit readable as a
    /// digit rather than a floating fragment.
    pub unlit: Color,
    /// The top of a covered tile.
    pub tile_top: Color,
    /// The bottom of a covered tile.
    pub tile_bottom: Color,
    /// A covered tile under the pointer.
    pub tile_hover: Color,
    /// A covered tile being pressed.
    pub tile_press: Color,
    /// An opened cell.
    pub opened: Color,
    /// The keyboard cursor's ring.
    pub cursor: Color,
    /// A mine's body.
    pub mine: Color,
    /// The plate under the mine that ended the game.
    pub struck: Color,
    /// A flag's pennant.
    pub flag: Color,
    /// A flag's pole, and the cross over one that was wrong.
    pub pole: Color,
    /// Text on the window's surfaces.
    pub text: Color,
    /// Quieter text.
    pub muted: Color,
    /// The victory sweep's light.
    pub victory: Color,
    /// The eight adjacency colours.
    pub numbers: [Color; 8],
    /// A tile's corner radius, in physical pixels.
    pub radius: u32,
}

impl Skin {
    /// Resolve `theme` at `scale` into the colours and lengths a frame draws
    /// with.
    #[must_use]
    pub fn resolve(theme: &Theme, scale: Scale, cell: u32) -> Self {
        let palette = theme.palette();
        let dark = theme.appearance() == Appearance::Dark;
        let raised = Color::from(palette.surface_raised);
        let surface = Color::from(palette.surface);
        // A covered tile stands clear of the board, and the board clear of the
        // window. Which side of the surface it stands on is the appearance's;
        // the bevel is lit from above on both.
        let tile_face = if dark { LIFT_DARK } else { -LIFT_LIGHT };
        // The two readouts are instruments, so they are drawn as instruments:
        // a dark plate on *either* appearance, because a segment display needs
        // its unlit segments to read as unlit and a pale plate cannot show one.
        let readout = if dark {
            Rgba::new(0, 0, 0, 255)
        } else {
            palette.on_surface.mix(Rgba::new(0, 0, 0, 255), 250)
        };
        Self {
            ground: surface,
            header: raised,
            readout: Color::from(readout),
            lit: Color::from(palette.accent),
            unlit: Color::from(readout.mix(palette.accent, UNLIT_PERMILLE)),
            tile_top: shade(raised, tile_face + BEVEL),
            tile_bottom: shade(raised, tile_face - BEVEL),
            tile_hover: shade(raised, tile_face + BEVEL * 2),
            tile_press: Color::from(palette.surface_pressed),
            opened: shade(surface, if dark { 14 } else { -18 }),
            cursor: Color::from(palette.rim_active),
            mine: Color::from(palette.on_surface),
            struck: Color::from(palette.danger),
            flag: Color::from(palette.danger),
            pole: Color::from(palette.on_surface),
            text: Color::from(palette.on_surface),
            muted: Color::from(palette.on_surface_muted),
            victory: Color::from(palette.success),
            numbers: if dark { NUMBERS_DARK } else { NUMBERS_LIGHT },
            // A tile is small, so the shared control radius would round it into
            // a lozenge; a proportion of the cell keeps the same softness at
            // every size the layout produces.
            radius: (cell / 6).clamp(1, scale.scale_length(6).max(1)),
        }
    }
}

/// What the pointer and keyboard are doing to the grid, so a tile can react.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Focus {
    /// The cell under the pointer.
    pub hovered: Option<Coord>,
    /// The cell being pressed, drawn compressed.
    pub pressed: Option<Coord>,
    /// The cell a chord is previewing, drawn compressed along with the
    /// neighbours it would open.
    pub chording: Option<Coord>,
    /// Where the keyboard is, drawn as a ring so it reads at a glance.
    pub cursor: Option<Coord>,
}

impl Focus {
    /// Whether `at` is drawn compressed.
    fn pressed(self, at: Coord, board: &Board) -> bool {
        if self.pressed == Some(at) {
            return true;
        }
        self.chording.is_some_and(|anchor| {
            anchor == at
                || (chebyshev_touches(anchor, at)
                    && matches!(board.cover(at), Some(Cover::Covered | Cover::Questioned)))
        })
    }
}

fn chebyshev_touches(a: Coord, b: Coord) -> bool {
    a != b && a.col.abs_diff(b.col) <= 1 && a.row.abs_diff(b.row) <= 1
}

/// Draw the whole window: the background, the header, and every cell.
///
/// The caller clips to the damage it is presenting, so drawing every cell costs
/// only the cells inside that clip.
#[allow(clippy::too_many_arguments)] // One frame's whole surround, threaded explicitly.
pub fn board(
    surface: &mut Surface,
    layout: &Layout,
    board: &Board,
    motion: &Motion,
    skin: &Skin,
    font: BitmapFont,
    focus: Focus,
    elapsed_secs: u32,
    now_ns: u64,
) {
    surface.fill(skin.ground);
    header(surface, layout, board, skin, focus, elapsed_secs);
    for (at, cover, adjacent) in board.iter() {
        cell(
            surface,
            layout,
            at,
            cover,
            adjacent,
            motion.cell(at, now_ns),
            skin,
            font,
            focus,
            board,
        );
    }
}

/// Draw the header band: the counter, the new-game button, and the clock.
fn header(
    surface: &mut Surface,
    layout: &Layout,
    board: &Board,
    skin: &Skin,
    focus: Focus,
    elapsed_secs: u32,
) {
    fill(surface, layout.header, skin.header);
    readout(surface, layout.counter, board.remaining(), skin);
    readout(surface, layout.clock, i64::from(elapsed_secs), skin);
    face(surface, layout.face, board.phase(), focus, skin);
}

/// Draw one readout: a recessed plate carrying three seven-segment digits and,
/// where the value is negative, a leading bar.
fn readout(surface: &mut Surface, area: Rect, value: i64, skin: &Skin) {
    round_fill(surface, area, skin.radius, skin.readout);
    let inner = shrink(area, area.height / 5);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // Four slots: a sign and three digits, which covers every value either
    // readout can hold once the magnitude is clamped.
    let slots = 4_u32;
    let gap = (inner.width / (slots * 6)).max(1);
    let slot_width = (inner.width.saturating_sub(gap * (slots - 1))) / slots;
    let magnitude = u32::try_from(value.unsigned_abs().min(999)).unwrap_or(999);
    let digits = [
        if value < 0 {
            Glyph::Minus
        } else {
            Glyph::Blank
        },
        Glyph::Digit(u8::try_from(magnitude / 100 % 10).unwrap_or(0)),
        Glyph::Digit(u8::try_from(magnitude / 10 % 10).unwrap_or(0)),
        Glyph::Digit(u8::try_from(magnitude % 10).unwrap_or(0)),
    ];
    debug_assert!(magnitude <= 999, "the readout holds three digits");
    for (index, glyph) in digits.into_iter().enumerate() {
        let step =
            i32::try_from(u32::try_from(index).unwrap_or(0) * (slot_width + gap)).unwrap_or(0);
        let slot = Rect::new(inner.left() + step, inner.top(), slot_width, inner.height);
        seven_segment(surface, slot, glyph, skin);
    }
}

/// What one readout slot shows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Glyph {
    /// A decimal digit, `0..=9`.
    Digit(u8),
    /// The minus a negative count carries.
    Minus,
    /// Nothing lit at all.
    Blank,
}

impl Glyph {
    /// Which of the seven segments are lit, in the order `a`–`g`: top,
    /// top-right, bottom-right, bottom, bottom-left, top-left, middle.
    const fn segments(self) -> [bool; 7] {
        match self {
            Self::Digit(0) => [true, true, true, true, true, true, false],
            Self::Digit(1) => [false, true, true, false, false, false, false],
            Self::Digit(2) => [true, true, false, true, true, false, true],
            Self::Digit(3) => [true, true, true, true, false, false, true],
            Self::Digit(4) => [false, true, true, false, false, true, true],
            Self::Digit(5) => [true, false, true, true, false, true, true],
            Self::Digit(6) => [true, false, true, true, true, true, true],
            Self::Digit(7) => [true, true, true, false, false, false, false],
            Self::Digit(8) => [true, true, true, true, true, true, true],
            Self::Digit(9) => [true, true, true, true, false, true, true],
            Self::Minus => [false, false, false, false, false, false, true],
            // A digit outside `0..=9` cannot be produced by the caller's
            // modulo, and a blank slot is the honest drawing for one.
            Self::Digit(_) | Self::Blank => [false; 7],
        }
    }
}

/// Draw one seven-segment glyph filling `slot`.
///
/// Both the lit and the unlit segments are drawn: a display that shows only the
/// lit ones reads as disconnected strokes rather than a digit. A wholly blank
/// slot draws nothing at all, so an unused sign position is empty rather than a
/// ghost figure beside the number.
fn seven_segment(surface: &mut Surface, slot: Rect, glyph: Glyph, skin: &Skin) {
    if glyph == Glyph::Blank {
        return;
    }
    let lit = glyph.segments();
    let thick = (slot.width / 5).max(1);
    let half = slot.height / 2;
    // The bars are inset by one thickness at each corner, which is what makes
    // seven strokes read as a figure instead of a filled block.
    let span = slot.width.saturating_sub(thick * 2);
    let arm = half.saturating_sub(thick).max(1);
    let bars = [
        (thick, 0, span, thick),
        (slot.width.saturating_sub(thick), thick, thick, arm),
        (slot.width.saturating_sub(thick), half, thick, arm),
        (thick, slot.height.saturating_sub(thick), span, thick),
        (0, half, thick, arm),
        (0, thick, thick, arm),
        (thick, half.saturating_sub(thick / 2), span, thick),
    ];
    for (on, (x, y, w, h)) in lit.into_iter().zip(bars) {
        let bar = Rect::new(
            slot.left() + i32::try_from(x).unwrap_or(0),
            slot.top() + i32::try_from(y).unwrap_or(0),
            w,
            h,
        );
        fill(surface, bar, if on { skin.lit } else { skin.unlit });
    }
}

/// Draw the new-game button: a face that reads the game's state at a glance.
fn face(surface: &mut Surface, area: Rect, phase: Phase, focus: Focus, skin: &Skin) {
    let plate = if focus.pressed.is_some() || focus.chording.is_some() {
        skin.tile_press
    } else {
        skin.tile_top
    };
    disc(surface, area, plate);
    disc(
        surface,
        shrink(area, area.width / 12),
        face_colour(phase, skin),
    );

    let side = area.width.min(area.height);
    if side < 8 {
        return;
    }
    let unit = i32::try_from(side).unwrap_or(1);
    let centre = area.center();
    let eye = |dx: i32| (centre.x + dx * unit / 5, centre.y - unit / 7);
    let ink = skin.readout;
    let eye_radius = (side / 10).max(1);

    match phase {
        // A struck mine: crossed-out eyes.
        Phase::Lost => {
            let reach = unit / 9;
            for dx in [-1, 1] {
                let (ex, ey) = eye(dx);
                cross(surface, ex, ey, reach, (side / 14).max(1), ink);
            }
        }
        // A finished board: a visor, because the player was never in danger.
        Phase::Won => {
            let visor = Rect::new(
                centre.x - unit * 3 / 10,
                centre.y - unit / 5,
                side * 3 / 5,
                (side / 5).max(1),
            );
            round_fill(surface, visor, (visor.height / 2).max(1), ink);
        }
        Phase::Ready | Phase::Playing => {
            for dx in [-1, 1] {
                let (ex, ey) = eye(dx);
                disc(
                    surface,
                    Rect::new(
                        ex - i32::try_from(eye_radius).unwrap_or(0),
                        ey - i32::try_from(eye_radius).unwrap_or(0),
                        eye_radius * 2,
                        eye_radius * 2,
                    ),
                    ink,
                );
            }
        }
    }

    // The mouth: an arc through three points, turned down only while a cell is
    // held, which is the moment the outcome is still unknown.
    let anxious = focus.pressed.is_some() || focus.chording.is_some();
    let curve = if anxious || phase == Phase::Lost {
        -unit / 8
    } else {
        unit / 6
    };
    let mouth_y = centre.y + unit / 6;
    let stroke = weight(side / 12);
    let arc = [
        sub(centre.x - unit / 4, mouth_y),
        sub(centre.x, mouth_y + curve),
        sub(centre.x + unit / 4, mouth_y),
    ];
    surface.stroke_polyline(&arc, stroke, ink);
}

fn face_colour(phase: Phase, skin: &Skin) -> Color {
    match phase {
        Phase::Won => skin.victory,
        Phase::Lost => skin.struck,
        Phase::Ready | Phase::Playing => skin.lit,
    }
}

/// Draw one cell: whatever it shows, plus whatever it is doing.
#[allow(clippy::too_many_arguments)] // One cell's whole surround, threaded explicitly.
fn cell(
    surface: &mut Surface,
    layout: &Layout,
    at: Coord,
    cover: Cover,
    adjacent: u8,
    motion: Option<CellMotion>,
    skin: &Skin,
    font: BitmapFont,
    focus: Focus,
    board: &Board,
) {
    let mut rect = layout.cell_rect(at);
    if rect.is_empty() || !surface.admits(0, 0, 1, 1) {
        return;
    }
    // A refused action shakes the cell it was refused on, which is the only
    // motion here that moves a whole tile rather than what is drawn in it.
    if let Some(shift) = shake(motion, rect.width) {
        rect = Rect::new(rect.left() + shift, rect.top(), rect.width, rect.height);
    }

    // The finished face first; the lid, if any, goes over it.
    match cover {
        Cover::Open => opened(surface, rect, adjacent, skin, font),
        Cover::Exposed { struck } => {
            exposed(surface, layout, rect, struck, motion, skin);
        }
        Cover::Misflagged => {
            opened(surface, rect, 0, skin, font);
            flag(surface, rect, skin, None);
            let reach = i32::try_from(rect.width / 4).unwrap_or(1);
            cross(
                surface,
                rect.center().x,
                rect.center().y,
                reach,
                (rect.width / 12).max(1),
                skin.struck,
            );
        }
        Cover::Covered | Cover::Flagged | Cover::Questioned => {
            tile(
                surface,
                rect,
                skin,
                focus.pressed(at, board),
                focus.hovered == Some(at),
            );
            match cover {
                Cover::Flagged => flag(surface, rect, skin, motion),
                Cover::Questioned => question(surface, rect, skin, font),
                _ => {}
            }
        }
    }

    // The lid: a covered tile lifting off the face that was under it.
    if let Some(motion) = motion {
        if motion.kind == WaveKind::Reveal {
            lid(surface, rect, motion, skin);
        }
        if motion.kind == WaveKind::Victory {
            let strength = arch(motion.eased());
            wash(surface, rect, skin.victory, strength);
        }
    }

    if focus.cursor == Some(at) {
        ring(surface, rect, skin.cursor, (rect.width / 12).max(1));
    }
}

/// The raised plate of a covered cell.
fn tile(surface: &mut Surface, rect: Rect, skin: &Skin, pressed: bool, hovered: bool) {
    if pressed {
        round_fill(
            surface,
            shrink(rect, rect.width / 12),
            skin.radius,
            skin.tile_press,
        );
        return;
    }
    let top = if hovered {
        skin.tile_hover
    } else {
        skin.tile_top
    };
    round_fill(surface, rect, skin.radius, top);
    // The gradient is laid inside the rounded plate, so the corners keep the
    // rounding the plate already established.
    let inner = shrink(rect, 1);
    if let Some((x, y)) = origin(inner) {
        surface.fill_vertical_gradient(x, y, inner.width, inner.height, top, skin.tile_bottom);
    }
}

/// The recessed plate of an opened cell, with its number.
fn opened(surface: &mut Surface, rect: Rect, adjacent: u8, skin: &Skin, font: BitmapFont) {
    round_fill(surface, rect, skin.radius, skin.opened);
    if adjacent == 0 {
        return;
    }
    let index = usize::from(adjacent).saturating_sub(1);
    let Some(&colour) = skin.numbers.get(index) else {
        return;
    };
    let digit = [b'0' + adjacent.min(9)];
    let Ok(text) = core::str::from_utf8(&digit) else {
        return;
    };
    let width = font.text_width(text);
    let height = font.line_height();
    let x = rect.left() + i32::try_from(rect.width.saturating_sub(width) / 2).unwrap_or(0);
    let y = rect.top() + i32::try_from(rect.height.saturating_sub(height) / 2).unwrap_or(0);
    font.draw_text(surface, x, y, text, colour);
}

/// A mine the end of the game uncovered, and — for the one that was struck —
/// the shockwave going out from it.
fn exposed(
    surface: &mut Surface,
    layout: &Layout,
    rect: Rect,
    struck: bool,
    motion: Option<CellMotion>,
    skin: &Skin,
) {
    if struck {
        round_fill(surface, rect, skin.radius, skin.struck);
    } else {
        round_fill(surface, rect, skin.radius, skin.opened);
    }
    let scale = motion
        .filter(|m| m.kind == WaveKind::Detonate)
        .map_or(1000, |m| u32::from(m.overshoot()));
    let body = scaled(shrink(rect, rect.width / 4), scale);
    disc(surface, body, skin.mine);
    spikes(surface, body, skin.mine);
    // A highlight, so the mine reads as a sphere rather than a blob.
    let gleam = Rect::new(
        body.left() + i32::try_from(body.width / 5).unwrap_or(0),
        body.top() + i32::try_from(body.height / 5).unwrap_or(0),
        (body.width / 4).max(1),
        (body.height / 4).max(1),
    );
    disc(surface, gleam, skin.opened);

    if !struck {
        return;
    }
    let Some(motion) = motion.filter(|m| m.kind == WaveKind::Detonate) else {
        return;
    };
    shockwave(surface, layout, rect, motion, skin);
}

/// The ring that expands out of the struck mine and fades as it goes.
fn shockwave(surface: &mut Surface, layout: &Layout, rect: Rect, motion: CellMotion, skin: &Skin) {
    let progress = u32::from(motion.eased());
    if progress == 0 || progress >= 255 {
        return;
    }
    let reach = rect.width.saturating_mul(SHOCKWAVE_REACH) / 1000;
    let grown = reach.saturating_mul(progress) / 255;
    let ring_rect = Rect::new(
        rect.left() - i32::try_from(grown).unwrap_or(0),
        rect.top() - i32::try_from(grown).unwrap_or(0),
        rect.width.saturating_add(grown * 2),
        rect.height.saturating_add(grown * 2),
    );
    // The wave outlives its own cell, so it is clipped to the grid rather than
    // to the tile: it washes over the neighbours it reaches.
    let fade = u8::try_from(255 - progress).unwrap_or(0);
    let thickness = (rect.width / 8).max(1);
    let colour = Color::rgba(skin.struck.r, skin.struck.g, skin.struck.b, fade / 2);
    // The wave reaches past its own cell, so it is bounded by the grid rather
    // than by the tile: it must not wash over the header.
    let Some((x, y)) = origin(layout.grid) else {
        return;
    };
    surface.with_clip(x, y, layout.grid.width, layout.grid.height, |surface| {
        ring(surface, ring_rect, colour, thickness);
    });
}

/// The covered tile still sitting over a cell that has just been opened,
/// sliding up and fading as it goes.
fn lid(surface: &mut Surface, rect: Rect, motion: CellMotion, skin: &Skin) {
    let progress = u32::from(motion.eased());
    if progress >= 255 {
        return;
    }
    let remaining = 255 - progress;
    // Shrinks towards its own centre and rises, so it reads as a lid coming
    // off rather than a square fading out.
    let shrunk = scaled(rect, 400 + remaining * 600 / 255);
    let lift = i32::try_from(rect.height.saturating_mul(progress) / 900).unwrap_or(0);
    let placed = Rect::new(
        shrunk.left(),
        shrunk.top() - lift,
        shrunk.width,
        shrunk.height,
    );
    let alpha = u8::try_from(remaining).unwrap_or(0);
    round_fill(
        surface,
        placed,
        skin.radius,
        Color::rgba(skin.tile_top.r, skin.tile_top.g, skin.tile_top.b, alpha),
    );
}

/// A flag, planted with an overshoot so it lands rather than fades in.
fn flag(surface: &mut Surface, rect: Rect, skin: &Skin, motion: Option<CellMotion>) {
    let scale = motion
        .filter(|m| m.kind == WaveKind::Mark)
        .map_or(1000, |m| u32::from(m.overshoot()));
    let area = scaled(shrink(rect, rect.width / 8), scale);
    if area.width < 3 || area.height < 3 {
        return;
    }
    let left = area.left();
    let top = area.top();
    let width = i32::try_from(area.width).unwrap_or(1);
    let height = i32::try_from(area.height).unwrap_or(1);
    let pole_x = left + width * 11 / 16;

    // The base, so the flag stands on something.
    let base = Rect::new(
        left + width / 6,
        top + height * 5 / 6,
        area.width * 2 / 3,
        (area.height / 6).max(1),
    );
    fill(surface, base, skin.pole);
    let pole_weight = weight(area.width / 9);
    surface.stroke_polyline(
        &[sub(pole_x, top), sub(pole_x, top + height * 5 / 6)],
        pole_weight,
        skin.pole,
    );
    // The pennant.
    surface.fill_polygon_subpixel(
        &[
            sub(pole_x, top),
            sub(left, top + height * 5 / 16),
            sub(pole_x, top + height * 5 / 8),
        ],
        skin.flag,
    );
}

/// The question mark a player leaves on a cell they are unsure of.
fn question(surface: &mut Surface, rect: Rect, skin: &Skin, font: BitmapFont) {
    let width = font.text_width("?");
    let height = font.line_height();
    let x = rect.left() + i32::try_from(rect.width.saturating_sub(width) / 2).unwrap_or(0);
    let y = rect.top() + i32::try_from(rect.height.saturating_sub(height) / 2).unwrap_or(0);
    font.draw_text(surface, x, y, "?", skin.muted);
}

// --- Primitives ---------------------------------------------------------

/// How far a shaken cell is displaced this frame, or `None` when it is not
/// being shaken.
///
/// Two full swings that decay to nothing, so the tile ends exactly where it
/// started however the frame timing falls.
fn shake(motion: Option<CellMotion>, width: u32) -> Option<i32> {
    let motion = motion.filter(|m| m.kind == WaveKind::Rejected)?;
    let reach = i64::from(width.saturating_mul(SHAKE_REACH) / 1000);
    let progress = i64::from(motion.progress);
    let decay = 255 - progress;
    // A triangle wave of four half-swings across the span, so the tile crosses
    // its rest position an even number of times.
    let phase = (progress * 4) % 255;
    let swing = if phase < 128 { phase } else { 255 - phase } - 64;
    let travel = swing * reach * decay / (64 * 255);
    let direction = if (progress * 4) / 255 % 2 == 0 { 1 } else { -1 };
    i32::try_from(travel * direction).ok()
}

/// A rectangle scaled about its own centre, in per-mille.
fn scaled(rect: Rect, permille: u32) -> Rect {
    let scale = permille;
    let width = rect.width.saturating_mul(scale) / 1000;
    let height = rect.height.saturating_mul(scale) / 1000;
    Rect::new(
        rect.left()
            + i32::try_from(rect.width.abs_diff(width) / 2).unwrap_or(0) * sign(rect.width, width),
        rect.top()
            + i32::try_from(rect.height.abs_diff(height) / 2).unwrap_or(0)
                * sign(rect.height, height),
        width,
        height,
    )
}

/// `1` when the scaled extent shrank (so the origin moves in), `-1` when it
/// grew.
fn sign(original: u32, scaled: u32) -> i32 {
    if scaled <= original {
        1
    } else {
        -1
    }
}

/// `rect` inset by `by` on every side, never inverted.
fn shrink(rect: Rect, by: u32) -> Rect {
    let by = by.min(rect.width / 2).min(rect.height / 2);
    Rect::new(
        rect.left() + i32::try_from(by).unwrap_or(0),
        rect.top() + i32::try_from(by).unwrap_or(0),
        rect.width.saturating_sub(by * 2),
        rect.height.saturating_sub(by * 2),
    )
}

/// A rectangle's origin as surface coordinates, or `None` when it starts off
/// the surface — in which case the shape is clipped away anyway.
fn origin(rect: Rect) -> Option<(u32, u32)> {
    Some((
        u32::try_from(rect.left()).ok()?,
        u32::try_from(rect.top()).ok()?,
    ))
}

fn fill(surface: &mut Surface, rect: Rect, colour: Color) {
    if let Some((x, y)) = origin(rect) {
        surface.fill_rect(x, y, rect.width, rect.height, colour);
    }
}

fn round_fill(surface: &mut Surface, rect: Rect, radius: u32, colour: Color) {
    if let Some((x, y)) = origin(rect) {
        surface.fill_round_rect(x, y, rect.width, rect.height, radius, colour);
    }
}

/// A hollow rectangle `thickness` pixels wide.
///
/// Four bars rather than a rectangle with its middle punched out: compositing a
/// transparent inner rectangle changes nothing, so a punch-out would draw a
/// solid block over whatever the ring is meant to surround.
fn ring(surface: &mut Surface, rect: Rect, colour: Color, thickness: u32) {
    let thickness = thickness.min(rect.width / 2).min(rect.height / 2).max(1);
    let side = rect.height.saturating_sub(thickness * 2);
    let inner_x = rect.left() + to_i32(rect.width.saturating_sub(thickness));
    let inner_y = rect.top() + to_i32(thickness);
    for bar in [
        Rect::new(rect.left(), rect.top(), rect.width, thickness),
        Rect::new(
            rect.left(),
            rect.top() + to_i32(rect.height.saturating_sub(thickness)),
            rect.width,
            thickness,
        ),
        Rect::new(rect.left(), inner_y, thickness, side),
        Rect::new(inner_x, inner_y, thickness, side),
    ] {
        fill(surface, bar, colour);
    }
}

/// A pixel length as a signed coordinate, saturating rather than wrapping.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Composite `colour` over `rect` at `strength`/255.
fn wash(surface: &mut Surface, rect: Rect, colour: Color, strength: u8) {
    if strength == 0 {
        return;
    }
    if let Some((x, y)) = origin(rect) {
        surface.fill_rect(
            x,
            y,
            rect.width,
            rect.height,
            Color::rgba(colour.r, colour.g, colour.b, strength),
        );
    }
}

/// A filled disc inscribed in `rect`.
fn disc(surface: &mut Surface, rect: Rect, colour: Color) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    surface.fill_polygon_subpixel(&ellipse(rect, 1000), colour);
}

/// The eight radial spikes that make a mine read as a mine.
fn spikes(surface: &mut Surface, rect: Rect, colour: Color) {
    let centre = rect.center();
    // Past the disc's own radius, or the spikes are drawn inside the body and
    // never show at all.
    let reach = i32::try_from(rect.width.max(rect.height) * 7 / 10).unwrap_or(1);
    let stroke = weight(rect.width / 9);
    if reach < 3 {
        return;
    }
    // Axis-aligned and diagonal, the diagonals pulled in so all eight reach
    // equally far from the centre.
    let diagonal = reach * 7 / 10;
    for (dx, dy) in [
        (reach, 0),
        (-reach, 0),
        (0, reach),
        (0, -reach),
        (diagonal, diagonal),
        (-diagonal, diagonal),
        (diagonal, -diagonal),
        (-diagonal, -diagonal),
    ] {
        surface.stroke_polyline(
            &[sub(centre.x, centre.y), sub(centre.x + dx, centre.y + dy)],
            stroke,
            colour,
        );
    }
}

/// An `X` centred on a point.
fn cross(surface: &mut Surface, x: i32, y: i32, reach: i32, thickness: u32, colour: Color) {
    if reach < 1 {
        return;
    }
    let stroke = weight(thickness);
    for (dx, dy) in [(reach, reach), (reach, -reach)] {
        surface.stroke_polyline(&[sub(x - dx, y - dy), sub(x + dx, y + dy)], stroke, colour);
    }
}

/// A closed polygon approximating the ellipse inscribed in `rect`, scaled about
/// its centre by `permille`, in device sub-pixel units.
fn ellipse(rect: Rect, permille: u32) -> [(i32, i32); DISC_STEPS] {
    let centre = rect.center();
    let rx = i64::from(rect.width) * i64::from(permille) / 2000;
    let ry = i64::from(rect.height) * i64::from(permille) / 2000;
    let mut points = [(0, 0); DISC_STEPS];
    for (index, point) in points.iter_mut().enumerate() {
        let (cos, sin) = unit_circle(index);
        let x = i64::from(centre.x) * i64::from(SUBPIXEL) + rx * cos * i64::from(SUBPIXEL) / 1000;
        let y = i64::from(centre.y) * i64::from(SUBPIXEL) + ry * sin * i64::from(SUBPIXEL) / 1000;
        *point = (
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        );
    }
    points
}

/// The cosine and sine of the `index`th of [`DISC_STEPS`] equal steps around a
/// circle, in per-mille.
///
/// A compiled table rather than a trigonometric call: the crate is `no_std`, the
/// step count is fixed, and a table is exact at every call site.
const fn unit_circle(index: usize) -> (i64, i64) {
    const COS: [i64; DISC_STEPS] = [
        1000, 966, 866, 707, 500, 259, 0, -259, -500, -707, -866, -966, -1000, -966, -866, -707,
        -500, -259, 0, 259, 500, 707, 866, 966,
    ];
    const SIN: [i64; DISC_STEPS] = [
        0, 259, 500, 707, 866, 966, 1000, 966, 866, 707, 500, 259, 0, -259, -500, -707, -866, -966,
        -1000, -966, -866, -707, -500, -259,
    ];
    (COS[index % DISC_STEPS], SIN[index % DISC_STEPS])
}

/// A value that rises to full at the half-way point and falls back — a pulse
/// rather than a fade, which is what a sweep of light over a cell is.
fn arch(progress: u8) -> u8 {
    /// The strongest the sweep washes a cell, per 255.
    const PEAK: u32 = 200;
    let triangle = 255 - (2 * u32::from(progress)).abs_diff(255);
    u8::try_from(triangle * PEAK / 255).unwrap_or(u8::MAX)
}

/// A stroke weight in device sub-pixel units, from a width in whole pixels.
fn weight(pixels: u32) -> i32 {
    i32::try_from(pixels.max(1))
        .unwrap_or(i32::MAX)
        .saturating_mul(SUBPIXEL)
}

/// A pixel coordinate in device sub-pixel units.
fn sub(x: i32, y: i32) -> (i32, i32) {
    (x.saturating_mul(SUBPIXEL), y.saturating_mul(SUBPIXEL))
}

/// `colour` lightened (positive) or darkened (negative) by `by` levels.
fn shade(colour: Color, by: i16) -> Color {
    let adjust = |channel: u8| {
        let value = i16::from(channel).saturating_add(by);
        u8::try_from(value.clamp(0, 255)).unwrap_or(channel)
    };
    Color::rgba(
        adjust(colour.r),
        adjust(colour.g),
        adjust(colour.b),
        colour.a,
    )
}

#[cfg(test)]
#[path = "paint_tests.rs"]
mod tests;
