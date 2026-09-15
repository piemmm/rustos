//! Painter tests.
//!
//! A software rasteriser's output is pixels, so these assert the two things a
//! test can hold the painter to: that every reachable board state and every
//! point of every animation draws without panicking or writing outside its
//! surface, and that the states a player must be able to tell apart actually
//! produce different pixels.

use super::*;

use alloc::vec::Vec;
use tairix_raster::Pixel;
use tairix_rng::NonCryptoRng;

use crate::anim::{Motion, WaveKind};
use crate::board::{Board, Difficulty, Dimensions, Step};

/// A 14px font, as the `Run` binary resolves at the theme's UI size.
fn font() -> BitmapFont {
    BitmapFont::monospace(14)
}

fn rng(seed: u64) -> NonCryptoRng {
    NonCryptoRng::seed_from_u64(seed)
}

fn beginner() -> Dimensions {
    Difficulty::Beginner.dimensions()
}

/// A surface, layout and skin sized to a board, ready to draw into.
fn canvas(dims: Dimensions, theme: &Theme) -> (Surface, Layout, Skin) {
    let (width, height) = Layout::preferred(dims, Scale::ONE);
    let surface = Surface::new(width, height).expect("a surface for the preferred window");
    let layout = Layout::resolve(Rect::new(0, 0, width, height), dims, Scale::ONE);
    let skin = Skin::resolve(theme, Scale::ONE, layout.cell);
    (surface, layout, skin)
}

/// Draw one frame, answering the pixels it produced.
fn frame(
    surface: &mut Surface,
    layout: &Layout,
    game: &Board,
    motion: &Motion,
    skin: &Skin,
    focus: Focus,
    now_ns: u64,
) -> Vec<Pixel> {
    board(
        surface,
        layout,
        game,
        motion,
        skin,
        font(),
        focus,
        0,
        now_ns,
    );
    surface.pixels().to_vec()
}

/// The pixels of one frame of `game`, drawn with no motion and no focus.
fn pixels(game: &Board, theme: &Theme) -> Vec<Pixel> {
    let (mut surface, layout, skin) = canvas(game.dimensions(), theme);
    frame(
        &mut surface,
        &layout,
        game,
        &Motion::new(game.dimensions().cols(), true),
        &skin,
        Focus::default(),
        0,
    )
}

fn steps(cells: &[(u16, u16, u16)]) -> Vec<Step> {
    cells
        .iter()
        .map(|&(col, row, ring)| Step {
            at: Coord::new(col, row),
            ring,
        })
        .collect()
}

// --- Every state draws --------------------------------------------------

#[test]
fn a_fresh_board_draws_on_both_themes() {
    for theme in [Theme::dark(), Theme::light()] {
        let game = Board::new(beginner(), true);
        let drawn = pixels(&game, &theme);
        assert!(drawn.iter().any(|&p| p != drawn[0]), "the frame is blank");
    }
}

#[test]
fn every_reachable_cover_draws() {
    // One board carried all the way to a loss, so every endgame cover — open,
    // flagged, questioned, exposed, struck, misflagged — is on screen at once.
    let theme = Theme::dark();
    let mut game = Board::new(beginner(), true);
    game.reveal(Coord::new(4, 4), &mut rng(3));
    let mut marked = 0;
    for (at, cover, _) in game.iter().collect::<Vec<_>>() {
        if cover == Cover::Covered && marked < 4 {
            game.toggle_mark(at);
            if marked % 2 == 1 {
                game.toggle_mark(at);
            }
            marked += 1;
        }
    }
    let mine = game
        .iter()
        .map(|(at, _, _)| at)
        .find(|&at| game.is_mine(at) == Some(true) && game.cover(at) == Some(Cover::Covered))
        .expect("a covered mine remains");
    game.reveal(mine, &mut rng(3));
    assert_eq!(game.phase(), Phase::Lost);

    let covers: Vec<Cover> = game.iter().map(|(_, cover, _)| cover).collect();
    assert!(covers.iter().any(|c| matches!(c, Cover::Exposed { .. })));
    assert!(covers.contains(&Cover::Open));
    let drawn = pixels(&game, &theme);
    assert!(drawn.iter().any(|&p| p != drawn[0]));
}

#[test]
fn every_face_draws() {
    let theme = Theme::light();
    for phase in [Phase::Ready, Phase::Playing, Phase::Won, Phase::Lost] {
        let dims = beginner();
        let (mut surface, layout, skin) = canvas(dims, &theme);
        // The face reads the phase, so drive it directly rather than playing a
        // whole game to reach each one.
        face(&mut surface, layout.face, phase, Focus::default(), &skin);
        face(
            &mut surface,
            layout.face,
            phase,
            Focus {
                pressed: Some(Coord::new(0, 0)),
                ..Focus::default()
            },
            &skin,
        );
    }
}

#[test]
fn a_readout_draws_every_value_it_can_hold() {
    let theme = Theme::dark();
    let dims = beginner();
    let (mut surface, layout, skin) = canvas(dims, &theme);
    for value in [-999_i64, -1, 0, 7, 99, 100, 999, 1_000, i64::MIN, i64::MAX] {
        readout(&mut surface, layout.counter, value, &skin);
    }
}

#[test]
fn the_counter_shows_a_different_reading_when_the_count_changes() {
    let theme = Theme::dark();
    let dims = beginner();
    let (mut surface, layout, skin) = canvas(dims, &theme);
    let capture = |surface: &Surface| surface.pixels().to_vec();
    readout(&mut surface, layout.counter, 10, &skin);
    let ten = capture(&surface);
    readout(&mut surface, layout.counter, 9, &skin);
    let nine = capture(&surface);
    assert_ne!(ten, nine, "a digit change must be visible");
    readout(&mut surface, layout.counter, -1, &skin);
    let negative = capture(&surface);
    assert_ne!(nine, negative, "the sign must be visible");
}

// --- Animation ----------------------------------------------------------

#[test]
fn every_wave_draws_at_every_point_of_its_span() {
    let theme = Theme::dark();
    let dims = beginner();
    let mut game = Board::new(dims, true);
    game.reveal(Coord::new(4, 4), &mut rng(11));
    let (mut surface, layout, skin) = canvas(dims, &theme);

    for kind in [
        WaveKind::Reveal,
        WaveKind::Mark,
        WaveKind::Detonate,
        WaveKind::Victory,
        WaveKind::Rejected,
    ] {
        let mut motion = Motion::new(dims.cols(), false);
        motion.begin(kind, &steps(&[(0, 0, 0), (4, 4, 1), (8, 8, 2)]), 0);
        // Sixty samples across two seconds covers every wave end to end.
        for step in 0..60_u64 {
            let now = step * 33_000_000;
            frame(
                &mut surface,
                &layout,
                &game,
                &motion,
                &skin,
                Focus::default(),
                now,
            );
        }
    }
}

#[test]
fn a_reveal_in_flight_looks_different_from_one_that_has_finished() {
    let theme = Theme::dark();
    let dims = beginner();
    let mut game = Board::new(dims, true);
    game.reveal(Coord::new(4, 4), &mut rng(5));
    let (mut surface, layout, skin) = canvas(dims, &theme);

    let mut motion = Motion::new(dims.cols(), false);
    motion.begin(WaveKind::Reveal, &steps(&[(4, 4, 0)]), 0);
    let sample_ns = 40_000_000;
    let progress = motion
        .cell(Coord::new(4, 4), sample_ns)
        .map(|m| m.progress)
        .expect("the cell is in the wave");
    assert!(
        progress > 0 && progress < u8::MAX,
        "the sample must land mid-wave, not on either end: {progress}"
    );
    let midway = frame(
        &mut surface,
        &layout,
        &game,
        &motion,
        &skin,
        Focus::default(),
        sample_ns,
    );
    let settled = frame(
        &mut surface,
        &layout,
        &game,
        &Motion::new(dims.cols(), true),
        &skin,
        Focus::default(),
        0,
    );
    assert_ne!(midway, settled, "the lid must be visible mid-reveal");
}

#[test]
fn a_suppressed_animation_draws_exactly_the_settled_board() {
    // Reduced motion must reach the same pixels as a finished animation, which
    // is what makes it one code path rather than two.
    let theme = Theme::light();
    let dims = beginner();
    let mut game = Board::new(dims, true);
    game.reveal(Coord::new(4, 4), &mut rng(9));
    let (mut surface, layout, skin) = canvas(dims, &theme);

    let mut suppressed = Motion::new(dims.cols(), true);
    suppressed.begin(WaveKind::Reveal, &steps(&[(4, 4, 0)]), 0);
    let reduced = frame(
        &mut surface,
        &layout,
        &game,
        &suppressed,
        &skin,
        Focus::default(),
        0,
    );

    let mut finished = Motion::new(dims.cols(), false);
    finished.begin(WaveKind::Reveal, &steps(&[(4, 4, 0)]), 0);
    let settled_ns = 5_000_000_000;
    assert_eq!(
        finished
            .cell(Coord::new(4, 4), settled_ns)
            .map(|m| m.progress),
        Some(u8::MAX),
        "the sample must be past the end of the wave"
    );
    let after = frame(
        &mut surface,
        &layout,
        &game,
        &finished,
        &skin,
        Focus::default(),
        settled_ns,
    );
    assert_eq!(reduced, after);
}

#[test]
fn a_shake_returns_the_cell_to_where_it_started() {
    assert_eq!(shake(None, 30), None);
    let at_rest = shake(
        Some(CellMotion {
            kind: WaveKind::Rejected,
            progress: 255,
        }),
        30,
    );
    assert_eq!(at_rest, Some(0), "the shake must not leave the tile offset");
    let moved = (0..=255_u8)
        .filter_map(|progress| {
            shake(
                Some(CellMotion {
                    kind: WaveKind::Rejected,
                    progress,
                }),
                30,
            )
        })
        .any(|offset| offset != 0);
    assert!(moved, "the shake must actually move the tile");
}

#[test]
fn only_a_rejection_shakes() {
    for kind in [
        WaveKind::Reveal,
        WaveKind::Mark,
        WaveKind::Detonate,
        WaveKind::Victory,
    ] {
        assert_eq!(
            shake(
                Some(CellMotion {
                    kind,
                    progress: 128
                }),
                30
            ),
            None
        );
    }
}

// --- Clipped repaints ---------------------------------------------------

#[test]
fn a_repaint_clipped_to_one_cell_draws_that_cell() {
    // The window is presented by damage rectangle, so an incremental repaint is
    // handed a surface clipped to it. Every cell inside that clip must land
    // exactly what a whole-window paint puts there — a guard that read the clip
    // wrongly skipped them all, so a hover erased the tiles it damaged.
    let theme = Theme::dark();
    let dims = beginner();
    let mut game = Board::new(dims, true);
    game.reveal(Coord::new(4, 4), &mut rng(5));
    let (mut surface, layout, skin) = canvas(dims, &theme);
    let motion = Motion::new(dims.cols(), true);

    let whole = frame(
        &mut surface,
        &layout,
        &game,
        &motion,
        &skin,
        Focus::default(),
        0,
    );

    // Start from a state no paint would produce, so an undrawn pixel is
    // unmistakable.
    let sentinel = Color::rgb(0xFF, 0x00, 0xFF);
    surface.fill(sentinel);
    let damage = layout.cell_damage(Coord::new(2, 3));
    let (x, y) = (
        u32::try_from(damage.left()).expect("on the surface"),
        u32::try_from(damage.top()).expect("on the surface"),
    );
    surface.with_clip(x, y, damage.width, damage.height, |surface| {
        board(
            surface,
            &layout,
            &game,
            &motion,
            &skin,
            font(),
            Focus::default(),
            0,
            0,
        );
    });

    let clipped: Vec<Pixel> = surface.pixels().to_vec();
    let width = surface.width();
    let mut inside = 0_u32;
    for (index, pixel) in clipped.iter().enumerate() {
        let px = u32::try_from(index).expect("fits") % width;
        let py = u32::try_from(index).expect("fits") / width;
        let covered = px >= x && px < x + damage.width && py >= y && py < y + damage.height;
        if covered {
            assert_eq!(
                *pixel, whole[index],
                "({px},{py}) inside the clip differs from a whole repaint"
            );
            inside += 1;
        } else {
            assert_eq!(
                pixel.unpremultiply(),
                sentinel,
                "({px},{py}) outside the clip was written"
            );
        }
    }
    assert_eq!(inside, damage.width * damage.height);
}

#[test]
fn a_clipped_repaint_covers_every_cell_it_reaches() {
    // A damage rectangle spanning several tiles must draw all of them, not the
    // first or the nearest.
    let theme = Theme::light();
    let dims = beginner();
    let mut game = Board::new(dims, true);
    game.reveal(Coord::new(4, 4), &mut rng(8));
    let (mut surface, layout, skin) = canvas(dims, &theme);
    let motion = Motion::new(dims.cols(), true);
    let whole = frame(
        &mut surface,
        &layout,
        &game,
        &motion,
        &skin,
        Focus::default(),
        0,
    );

    // The span a pointer crossing from one tile to another damages.
    let from = layout.cell_damage(Coord::new(1, 1));
    let to = layout.cell_damage(Coord::new(6, 5));
    let span = from.union(&to);
    surface.fill(Color::rgb(0xFF, 0x00, 0xFF));
    let (x, y) = (
        u32::try_from(span.left()).expect("on the surface"),
        u32::try_from(span.top()).expect("on the surface"),
    );
    surface.with_clip(x, y, span.width, span.height, |surface| {
        board(
            surface,
            &layout,
            &game,
            &motion,
            &skin,
            font(),
            Focus::default(),
            0,
            0,
        );
    });

    for at in [Coord::new(1, 1), Coord::new(3, 3), Coord::new(6, 5)] {
        let rect = layout.cell_rect(at);
        let centre = rect.center();
        let index = usize::try_from(
            u32::try_from(centre.y).expect("on the surface") * surface.width()
                + u32::try_from(centre.x).expect("on the surface"),
        )
        .expect("fits");
        assert_eq!(
            surface.pixels()[index],
            whole[index],
            "{at:?} was not drawn inside the clip"
        );
    }
}

// --- Focus --------------------------------------------------------------

#[test]
fn a_pressed_cell_draws_differently_from_a_resting_one() {
    let theme = Theme::dark();
    let dims = beginner();
    let game = Board::new(dims, true);
    let (mut surface, layout, skin) = canvas(dims, &theme);
    let motion = Motion::new(dims.cols(), true);
    let resting = frame(
        &mut surface,
        &layout,
        &game,
        &motion,
        &skin,
        Focus::default(),
        0,
    );
    let pressed = frame(
        &mut surface,
        &layout,
        &game,
        &motion,
        &skin,
        Focus {
            pressed: Some(Coord::new(3, 3)),
            ..Focus::default()
        },
        0,
    );
    assert_ne!(resting, pressed);
}

#[test]
fn a_chord_preview_presses_the_anchor_and_its_covered_neighbours() {
    let dims = beginner();
    let game = Board::new(dims, true);
    let anchor = Coord::new(4, 4);
    let focus = Focus {
        chording: Some(anchor),
        ..Focus::default()
    };
    assert!(focus.pressed(anchor, &game));
    assert!(focus.pressed(Coord::new(3, 3), &game), "a neighbour");
    assert!(focus.pressed(Coord::new(5, 5), &game), "a neighbour");
    assert!(!focus.pressed(Coord::new(6, 6), &game), "two cells away");
}

#[test]
fn the_keyboard_cursor_is_drawn() {
    let theme = Theme::light();
    let dims = beginner();
    let game = Board::new(dims, true);
    let (mut surface, layout, skin) = canvas(dims, &theme);
    let motion = Motion::new(dims.cols(), true);
    let bare = frame(
        &mut surface,
        &layout,
        &game,
        &motion,
        &skin,
        Focus::default(),
        0,
    );
    let ringed = frame(
        &mut surface,
        &layout,
        &game,
        &motion,
        &skin,
        Focus {
            cursor: Some(Coord::new(2, 2)),
            ..Focus::default()
        },
        0,
    );
    assert_ne!(bare, ringed);
}

// --- Robustness ---------------------------------------------------------

#[test]
fn a_surface_smaller_than_the_board_draws_without_overrunning() {
    // The compositor's floor keeps this from happening in practice, but a
    // painter that trusts its surface is one release-mode bug from writing
    // outside it.
    let theme = Theme::dark();
    let dims = Difficulty::Expert.dimensions();
    let mut game = Board::new(dims, true);
    game.reveal(Coord::new(15, 8), &mut rng(2));
    for (width, height) in [(1, 1), (16, 16), (64, 40), (200, 120)] {
        let mut surface = Surface::new(width, height).expect("a small surface");
        let layout = Layout::resolve(Rect::new(0, 0, width, height), dims, Scale::ONE);
        let skin = Skin::resolve(&theme, Scale::ONE, layout.cell);
        let mut motion = Motion::new(dims.cols(), false);
        motion.begin(WaveKind::Detonate, &steps(&[(15, 8, 0), (0, 0, 15)]), 0);
        for now in [0, 100_000_000, 500_000_000] {
            frame(
                &mut surface,
                &layout,
                &game,
                &motion,
                &skin,
                Focus {
                    cursor: Some(Coord::new(29, 15)),
                    ..Focus::default()
                },
                now,
            );
        }
    }
}

#[test]
fn the_smallest_and_largest_boards_both_draw() {
    let theme = Theme::dark();
    for dims in [
        Dimensions::new(crate::board::MIN_SIDE, crate::board::MIN_SIDE, 1).expect("legal"),
        Dimensions::new(crate::board::MAX_SIDE, crate::board::MAX_SIDE, 99).expect("legal"),
    ] {
        let mut game = Board::new(dims, true);
        game.reveal(Coord::new(2, 2), &mut rng(4));
        let drawn = pixels(&game, &theme);
        assert!(drawn.iter().any(|&p| p != drawn[0]));
    }
}

#[test]
fn every_scale_the_desktop_offers_draws() {
    let theme = Theme::dark();
    let dims = beginner();
    let mut game = Board::new(dims, true);
    game.reveal(Coord::new(4, 4), &mut rng(6));
    for percent in [50, 75, 100, 125, 150, 200, 300] {
        let Some(scale) = Scale::from_percent(percent) else {
            continue;
        };
        let (width, height) = Layout::preferred(dims, scale);
        let mut surface = Surface::new(width, height).expect("a surface");
        let layout = Layout::resolve(Rect::new(0, 0, width, height), dims, scale);
        let skin = Skin::resolve(&theme, scale, layout.cell);
        assert!(skin.radius >= 1);
        frame(
            &mut surface,
            &layout,
            &game,
            &Motion::new(dims.cols(), false),
            &skin,
            Focus::default(),
            0,
        );
    }
}

#[test]
fn both_shipped_themes_resolve_a_usable_skin() {
    for theme in [Theme::dark(), Theme::light()] {
        for cell in [1_u32, 4, 18, 26, 46, 400] {
            let skin = Skin::resolve(&theme, Scale::ONE, cell);
            assert!(skin.radius >= 1, "a radius of zero draws square corners");
            assert!(skin.radius <= cell.max(1));
        }
    }
}

#[test]
fn the_number_palettes_are_all_distinct_within_themselves() {
    for palette in [NUMBERS_DARK, NUMBERS_LIGHT] {
        for (index, colour) in palette.iter().enumerate() {
            for other in palette.iter().skip(index + 1) {
                assert_ne!(colour, other, "two adjacency counts share a colour");
            }
        }
    }
}

#[test]
fn a_shaded_colour_stays_in_range() {
    for level in [0_u8, 1, 128, 254, 255] {
        for by in [i16::MIN, -300, -1, 0, 1, 300, i16::MAX] {
            let shaded = shade(Color::rgba(level, level, level, 200), by);
            assert_eq!(shaded.a, 200, "the alpha is never touched");
        }
    }
}

#[test]
fn the_pulse_rises_and_falls() {
    assert_eq!(arch(0), 0);
    assert_eq!(arch(255), 0);
    assert!(arch(128) > arch(64));
    assert!(arch(128) > arch(200));
}

/// The whole point of following the desktop's appearance: it reaches the
/// pixels. The same board, drawn under the light and the dark theme, must not
/// produce the same frame — otherwise an adopted light/dark switch would be
/// bookkeeping the user cannot see.
#[test]
fn the_same_board_renders_differently_under_light_and_dark() {
    let game = Board::new(beginner(), true);
    let dark = pixels(&game, &Theme::dark());
    let light = pixels(&game, &Theme::light());
    assert_eq!(dark.len(), light.len(), "the same board is the same size");
    assert_ne!(
        dark, light,
        "the desktop's appearance must reach the board's pixels"
    );
}

/// And the registry is what an adopted appearance is applied to, so the theme
/// it then hands the painter is the one the switch asked for.
#[test]
fn a_registry_switched_to_light_hands_the_painter_the_light_theme() {
    let mut themes = tairix_theme::ThemeRegistry::with_builtins();
    themes.set_appearance(Appearance::Dark);
    let game = Board::new(beginner(), true);
    let under_dark = pixels(&game, themes.active());

    themes.set_appearance(Appearance::Light);
    assert_eq!(themes.active().appearance(), Appearance::Light);
    assert_ne!(
        under_dark,
        pixels(&game, themes.active()),
        "the switch the app adopts is the one the painter draws with"
    );
}
