//! Composed-game tests: input routing, the clock, the wake deadline, and the
//! damage every path reports.

use super::*;

use tairix_controls::damage;
use tairix_geometry::Region;
use tairix_rng::NonCryptoRng;
use tairix_theme::Theme;

use crate::board::{Cover, Dimensions, Phase};
use crate::scores::MAX_TIME_SECS;

fn rng(seed: u64) -> NonCryptoRng {
    NonCryptoRng::seed_from_u64(seed)
}

/// A beginner game in the window it opens at.
fn game() -> Game {
    game_of(Difficulty::Beginner)
}

fn game_of(difficulty: Difficulty) -> Game {
    let (width, height) = Layout::preferred(difficulty.dimensions(), Scale::ONE);
    Game::new(
        difficulty,
        BestTimes::default(),
        true,
        false,
        Rect::new(0, 0, width, height),
        Scale::ONE,
    )
}

/// The centre of `at` in the game's own window coordinates.
fn centre(game: &Game, at: Coord) -> Point {
    game.layout.cell_rect(at).center()
}

/// Move the pointer to `to` and click `button`, answering the release's
/// reaction and what the whole sequence reported as damage.
fn click_at(game: &mut Game, to: Point, button: PointerButton, now_ns: u64) -> (Reaction, Region) {
    let mut region = damage::sink();
    let mut generator = rng(1);
    for event in [
        InputEvent::PointerMoved { to },
        InputEvent::PointerPressed { button },
    ] {
        game.on_pointer(&event, now_ns, &mut generator, &mut region);
    }
    let reaction = game.on_pointer(
        &InputEvent::PointerReleased { button },
        now_ns,
        &mut generator,
        &mut region,
    );
    (reaction, region)
}

fn click(game: &mut Game, at: Coord, button: PointerButton, now_ns: u64) -> Reaction {
    let to = centre(game, at);
    click_at(game, to, button, now_ns).0
}

fn press_key(game: &mut Game, key: Key, now_ns: u64) -> Reaction {
    let mut region = damage::sink();
    game.on_key(key, Modifiers::default(), now_ns, &mut rng(1), &mut region)
}

/// Whether `region` covers `rect` — the test's own containment check, since a
/// budgeted region may have merged rectangles.
fn covers(region: &Region, rect: Rect) -> bool {
    region.rects().iter().any(|r| {
        r.left() <= rect.left()
            && r.top() <= rect.top()
            && r.right() >= rect.right()
            && r.bottom() >= rect.bottom()
    })
}

// --- Opening a game -----------------------------------------------------

#[test]
fn a_new_game_is_ready_with_the_clock_at_zero() {
    let game = game();
    assert_eq!(game.board().phase(), Phase::Ready);
    assert_eq!(game.elapsed_secs(0), 0);
    assert_eq!(game.elapsed_secs(60 * SECOND_NS), 0, "not started yet");
    assert_eq!(game.deadline_ns(0), None, "an untouched game owes no wake");
}

#[test]
fn the_first_click_starts_the_clock_and_the_game() {
    let mut game = game();
    let start = 5 * SECOND_NS;
    let reaction = click(&mut game, Coord::new(4, 4), PointerButton::Primary, start);
    assert!(reaction.changed);
    assert_eq!(game.board().phase(), Phase::Playing);
    assert_eq!(game.elapsed_secs(start), 0);
    assert_eq!(game.elapsed_secs(start + 3 * SECOND_NS), 3);
}

#[test]
fn a_mark_alone_does_not_start_the_clock() {
    // The board does not exist until a cell is revealed, so there is nothing
    // yet to be timed.
    let mut game = game();
    click(&mut game, Coord::new(4, 4), PointerButton::Secondary, 0);
    assert_eq!(game.board().phase(), Phase::Ready);
    assert_eq!(game.elapsed_secs(9 * SECOND_NS), 0);
}

// --- The wake deadline --------------------------------------------------

#[test]
fn a_running_game_asks_only_for_its_next_whole_second() {
    let mut game = game_of(Difficulty::Expert);
    let start = 1_000;
    click(&mut game, Coord::new(15, 8), PointerButton::Primary, start);
    // Past the reveal cascade, the clock is the only thing left to wake for.
    let quiet = start + 10 * SECOND_NS;
    let mut region = damage::sink();
    game.tick(quiet, &mut region);
    assert_eq!(game.deadline_ns(quiet), Some(start + 11 * SECOND_NS));
}

#[test]
fn an_animating_game_asks_for_a_frame() {
    let mut game = game();
    let start = 0;
    click(&mut game, Coord::new(4, 4), PointerButton::Primary, start);
    let deadline = game.deadline_ns(start).expect("a wave is in flight");
    assert!(
        deadline <= start + crate::anim::FRAME_NS,
        "a frame is due before the clock's next second"
    );
}

#[test]
fn a_finished_game_owes_no_wake_once_its_animation_ends() {
    let mut game = game();
    let mine = first_mine(&mut game, 4);
    click(&mut game, mine, PointerButton::Primary, 0);
    assert_eq!(game.board().phase(), Phase::Lost);
    let settled = 10 * SECOND_NS;
    let mut region = damage::sink();
    game.tick(settled, &mut region);
    assert_eq!(
        game.deadline_ns(settled),
        None,
        "a finished, still board must not wake at all"
    );
}

#[test]
fn a_tick_reports_the_clock_only_when_the_reading_moves() {
    let mut game = game();
    let start = 0;
    click(&mut game, Coord::new(4, 4), PointerButton::Primary, start);
    // Past the cascade so the animation is not what reports.
    let mut region = damage::sink();
    game.tick(start + 5 * SECOND_NS, &mut region);

    let mut quiet = damage::sink();
    assert!(
        !game.tick(start + 5 * SECOND_NS + 1, &mut quiet),
        "the same second reports nothing"
    );
    assert!(quiet.is_empty());

    let mut moved = damage::sink();
    assert!(game.tick(start + 6 * SECOND_NS, &mut moved));
    assert!(covers(&moved, game.layout.clock));
}

#[test]
fn a_clock_at_its_last_reading_stops_asking_to_be_woken() {
    // Past the readout's last reading the clock's "next second" is an instant
    // already gone, and parking to one of those returns at once — a spin.
    let mut game = game();
    let start = 0;
    click(&mut game, Coord::new(4, 4), PointerButton::Primary, start);
    let capped = u64::from(MAX_TIME_SECS) * SECOND_NS;
    let mut region = damage::sink();
    game.tick(capped, &mut region);
    assert_eq!(game.elapsed_secs(capped), MAX_TIME_SECS);
    assert_eq!(
        game.deadline_ns(capped),
        None,
        "a capped clock owes no further wake"
    );
    assert_eq!(game.deadline_ns(capped * 2), None);

    // One second short of the cap it is still ticking.
    let short = capped - SECOND_NS;
    assert!(game.deadline_ns(short).is_some());
}

// --- Revealing ----------------------------------------------------------

/// Play the opening move, then find a cell that still holds a mine.
fn first_mine(game: &mut Game, seed: u64) -> Coord {
    let mut region = damage::sink();
    let mut generator = rng(seed);
    game.board.reveal(Coord::new(0, 0), &mut generator);
    game.started_ns = Some(0);
    region.add(game.client);
    game.board()
        .iter()
        .map(|(at, _, _)| at)
        .find(|&at| {
            game.board().is_mine(at) == Some(true) && game.board().cover(at) == Some(Cover::Covered)
        })
        .expect("a covered mine remains after the opening move")
}

#[test]
fn a_reveal_reports_only_the_cells_it_opened() {
    let mut game = game_of(Difficulty::Expert);
    let mut region = damage::sink();
    let at = Coord::new(15, 8);
    let to = centre(&game, at);
    let mut generator = rng(1);
    for event in [
        InputEvent::PointerMoved { to },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        game.on_pointer(&event, 0, &mut generator, &mut region);
    }
    assert!(!region.is_empty());
    assert!(
        !covers(&region, game.client),
        "a local reveal must not repaint the whole window"
    );
}

#[test]
fn a_flagged_cell_is_not_revealed_by_a_click() {
    let mut game = game();
    let at = Coord::new(2, 2);
    click(&mut game, at, PointerButton::Secondary, 0);
    assert_eq!(game.board().cover(at), Some(Cover::Flagged));
    click(&mut game, at, PointerButton::Primary, 0);
    assert_eq!(game.board().cover(at), Some(Cover::Flagged));
}

#[test]
fn a_press_dragged_off_its_cell_is_abandoned() {
    let mut game = game();
    let from = Coord::new(2, 2);
    let mut region = damage::sink();
    let mut generator = rng(1);
    for event in [
        InputEvent::PointerMoved {
            to: centre(&game, from),
        },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        // Off the grid entirely.
        InputEvent::PointerMoved {
            to: Point::new(-50, -50),
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        game.on_pointer(&event, 0, &mut generator, &mut region);
    }
    assert_eq!(game.board().cover(from), Some(Cover::Covered));
    assert_eq!(game.board().phase(), Phase::Ready);
}

#[test]
fn a_press_dragged_to_another_cell_acts_on_that_one() {
    let mut game = game();
    let from = Coord::new(2, 2);
    let to = Coord::new(5, 5);
    let mut region = damage::sink();
    let mut generator = rng(1);
    for event in [
        InputEvent::PointerMoved {
            to: centre(&game, from),
        },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerMoved {
            to: centre(&game, to),
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        game.on_pointer(&event, 0, &mut generator, &mut region);
    }
    // The mines are laid around whichever cell was actually revealed, so the
    // safe region proves the release acted on `to` rather than on `from` —
    // which a cascade may well have opened along the way.
    assert_eq!(game.board().cover(to), Some(Cover::Open));
    assert_eq!(game.board().is_mine(to), Some(false));
    for (dc, dr) in [(-1_i32, -1_i32), (1, 1), (-1, 1), (1, -1)] {
        let near = Coord::new(
            u16::try_from(i32::from(to.col) + dc).expect("in bounds"),
            u16::try_from(i32::from(to.row) + dr).expect("in bounds"),
        );
        assert_eq!(game.board().is_mine(near), Some(false), "{near:?}");
    }
    let _ = from;
}

// --- The new-game button ------------------------------------------------

#[test]
fn clicking_the_new_game_button_starts_a_fresh_board() {
    let mut game = game();
    click(&mut game, Coord::new(4, 4), PointerButton::Primary, 0);
    assert_eq!(game.board().phase(), Phase::Playing);
    let face = game.layout.face.center();
    let (reaction, _) = click_at(&mut game, face, PointerButton::Primary, 0);
    assert!(reaction.changed);
    assert_eq!(game.board().phase(), Phase::Ready);
    assert_eq!(game.elapsed_secs(9 * SECOND_NS), 0);
}

#[test]
fn a_click_on_the_header_beside_the_button_does_nothing() {
    let mut game = game();
    click(&mut game, Coord::new(4, 4), PointerButton::Primary, 0);
    let beside = Point::new(game.layout.header.left() + 1, game.layout.header.top() + 1);
    assert!(!game.layout.face.contains(beside));
    let (reaction, _) = click_at(&mut game, beside, PointerButton::Primary, 0);
    assert_eq!(reaction, Reaction::IDLE);
    assert_eq!(
        game.board().phase(),
        Phase::Playing,
        "the game must not restart"
    );
}

#[test]
fn a_press_on_the_button_dragged_off_it_is_abandoned() {
    let mut game = game();
    click(&mut game, Coord::new(4, 4), PointerButton::Primary, 0);
    let mut region = damage::sink();
    let mut generator = rng(1);
    for event in [
        InputEvent::PointerMoved {
            to: game.layout.face.center(),
        },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerMoved {
            to: Point::new(game.layout.header.left() + 1, game.layout.header.top() + 1),
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        game.on_pointer(&event, 0, &mut generator, &mut region);
    }
    assert_eq!(game.board().phase(), Phase::Playing);
}

// --- Chording -----------------------------------------------------------

#[test]
fn a_click_on_an_open_number_chords_it() {
    let mut game = game();
    let mut generator = rng(2);
    // Reach a board with an open number whose one mine is flagged.
    game.board.reveal(Coord::new(0, 0), &mut generator);
    let anchor = game
        .board()
        .iter()
        .find(|&(at, cover, adjacent)| {
            cover == Cover::Open
                && adjacent == 1
                && game
                    .board()
                    .iter()
                    .filter(|&(other, c, _)| {
                        c == Cover::Covered
                            && other.col.abs_diff(at.col) <= 1
                            && other.row.abs_diff(at.row) <= 1
                    })
                    .count()
                    > 1
        })
        .map(|(at, _, _)| at);
    let Some(anchor) = anchor else {
        return;
    };
    let mine = (0..9)
        .find_map(|n| {
            let col = i32::from(anchor.col) + (n % 3) - 1;
            let row = i32::from(anchor.row) + (n / 3) - 1;
            let at = Coord::new(
                u16::try_from(col.max(0)).ok()?,
                u16::try_from(row.max(0)).ok()?,
            );
            (game.board().is_mine(at) == Some(true)).then_some(at)
        })
        .expect("a `1` has a mine beside it");
    click(&mut game, mine, PointerButton::Secondary, 0);
    let before = open_count(&game);
    click(&mut game, anchor, PointerButton::Primary, 0);
    assert!(open_count(&game) > before, "the chord opened something");
}

fn open_count(game: &Game) -> usize {
    game.board()
        .iter()
        .filter(|&(_, cover, _)| cover == Cover::Open)
        .count()
}

#[test]
fn a_secondary_click_on_an_open_number_flag_chords_it() {
    let mut game = game_of(Difficulty::Beginner);
    let mut generator = rng(3);
    game.board.reveal(Coord::new(0, 0), &mut generator);
    // A number whose covered neighbours are exactly its count is what a flag
    // chord acts on; find one or leave the rule to the board's own tests.
    let anchor = game.board().iter().find(|&(at, cover, adjacent)| {
        cover == Cover::Open && adjacent > 0 && {
            let covered = game
                .board()
                .iter()
                .filter(|&(other, c, _)| {
                    c != Cover::Open
                        && other != at
                        && other.col.abs_diff(at.col) <= 1
                        && other.row.abs_diff(at.row) <= 1
                })
                .count();
            covered == usize::from(adjacent)
        }
    });
    let Some((anchor, _, adjacent)) = anchor else {
        return;
    };
    click(&mut game, anchor, PointerButton::Secondary, 0);
    let flagged = game
        .board()
        .iter()
        .filter(|&(_, cover, _)| cover == Cover::Flagged)
        .count();
    assert_eq!(flagged, usize::from(adjacent));
}

#[test]
fn a_middle_press_previews_the_chord_and_the_release_commits_it() {
    let mut game = game();
    let mut generator = rng(1);
    game.board.reveal(Coord::new(4, 4), &mut generator);
    let anchor = Coord::new(4, 4);
    let mut region = damage::sink();
    game.on_pointer(
        &InputEvent::PointerMoved {
            to: centre(&game, anchor),
        },
        0,
        &mut generator,
        &mut region,
    );
    game.on_pointer(
        &InputEvent::PointerPressed {
            button: PointerButton::Middle,
        },
        0,
        &mut generator,
        &mut region,
    );
    assert_eq!(game.focus.chording, Some(anchor));
    game.on_pointer(
        &InputEvent::PointerReleased {
            button: PointerButton::Middle,
        },
        0,
        &mut generator,
        &mut region,
    );
    assert_eq!(game.focus.chording, None, "the preview is released");
}

// --- Refusals -----------------------------------------------------------

#[test]
fn a_refused_action_shakes_the_cell_rather_than_vanishing() {
    let mut game = game();
    let mut generator = rng(1);
    game.board.reveal(Coord::new(4, 4), &mut generator);
    // A chord on a number with no flags around it: the rules refuse it.
    let anchor = game
        .board()
        .iter()
        .find(|&(_, cover, adjacent)| cover == Cover::Open && adjacent > 0)
        .map(|(at, _, _)| at);
    let Some(anchor) = anchor else {
        return;
    };
    let reaction = click(&mut game, anchor, PointerButton::Primary, 0);
    assert!(reaction.changed, "the refusal is shown, not swallowed");
    assert!(
        game.motion.cell(anchor, 0).is_some(),
        "the cell is animating its refusal"
    );
}

#[test]
fn a_refusal_on_a_finished_board_stays_silent() {
    let mut game = game();
    let mine = first_mine(&mut game, 4);
    click(&mut game, mine, PointerButton::Primary, 0);
    assert_eq!(game.board().phase(), Phase::Lost);
    game.motion.clear();
    let reaction = click(&mut game, Coord::new(0, 8), PointerButton::Primary, 0);
    assert!(!reaction.record);
    assert!(
        game.motion.is_idle(),
        "a finished board does not shake at every click"
    );
}

// --- Winning ------------------------------------------------------------

/// Open every safe cell in order, answering the reaction of the move that won.
fn play_to_win(game: &mut Game, seed: u64, now_ns: u64) -> Reaction {
    let mut generator = rng(seed);
    let mut region = damage::sink();
    let cells: Vec<Coord> = game.board().iter().map(|(at, _, _)| at).collect();
    let mut last = Reaction::IDLE;
    for at in cells {
        if game.board().phase().is_over() {
            break;
        }
        if game.board().is_mine(at) == Some(false) || game.board().phase() == Phase::Ready {
            if game.board().phase() == Phase::Ready {
                game.started_ns = Some(0);
            }
            let acted = game.board.reveal(at, &mut generator);
            last = game.apply(&acted, at, now_ns, &mut region);
        }
    }
    last
}

#[test]
fn winning_records_a_best_time_and_freezes_the_clock() {
    let mut game = game();
    let finish = 42 * SECOND_NS;
    let reaction = play_to_win(&mut game, 7, finish);
    assert_eq!(game.board().phase(), Phase::Won);
    assert!(reaction.record, "a first win is always a best time");
    assert_eq!(game.best_times().best(Difficulty::Beginner), Some(42));
    assert_eq!(game.elapsed_secs(finish + 60 * SECOND_NS), 42, "frozen");
}

#[test]
fn a_slower_second_win_is_not_recorded() {
    let mut game = game();
    play_to_win(&mut game, 7, 20 * SECOND_NS);
    let best = game.best_times();
    let mut region = damage::sink();
    game.restart(&mut region);
    let reaction = play_to_win(&mut game, 8, 90 * SECOND_NS);
    assert_eq!(game.board().phase(), Phase::Won);
    assert!(!reaction.record, "slower than the standing best");
    assert_eq!(game.best_times(), best);
}

#[test]
fn a_win_on_a_custom_board_records_nothing() {
    let custom = Difficulty::Custom(Dimensions::new(5, 5, 1).expect("legal"));
    let mut game = game_of(custom);
    let reaction = play_to_win(&mut game, 7, 3 * SECOND_NS);
    assert_eq!(game.board().phase(), Phase::Won);
    assert!(!reaction.record);
    assert!(game.best_times().is_empty());
}

#[test]
fn winning_sweeps_the_whole_board() {
    let mut game = game();
    play_to_win(&mut game, 7, 0);
    // Two waves are in flight — the winning reveal and the sweep — so the same
    // cell may appear twice; what matters is that every cell is in one.
    let swept: alloc::collections::BTreeSet<Coord> = game.motion.animating().collect();
    assert_eq!(
        u32::try_from(swept.len()).expect("fits"),
        game.board().dimensions().cells(),
        "the sweep covers every cell"
    );
}

// --- Difficulty and layout ----------------------------------------------

#[test]
fn changing_difficulty_starts_a_fresh_board_and_asks_for_a_resize() {
    let mut game = game();
    click(&mut game, Coord::new(4, 4), PointerButton::Primary, 0);
    let mut region = damage::sink();
    let reaction = game.set_difficulty(Difficulty::Expert, &mut region);
    assert!(reaction.changed && reaction.resized);
    assert_eq!(game.difficulty(), Difficulty::Expert);
    assert_eq!(game.board().phase(), Phase::Ready);
    assert_eq!(game.board().dimensions().cols(), 30);
    assert!(covers(&region, game.client), "the whole window is redrawn");
}

#[test]
fn restarting_at_the_same_difficulty_asks_for_no_resize() {
    let mut game = game();
    let mut region = damage::sink();
    let reaction = game.set_difficulty(Difficulty::Beginner, &mut region);
    assert!(reaction.changed);
    assert!(!reaction.resized);
}

#[test]
fn a_relayout_that_changes_nothing_reports_nothing() {
    let mut game = game();
    let mut region = damage::sink();
    game.relayout(game.client, Scale::ONE, &mut region);
    assert!(region.is_empty());
}

#[test]
fn a_resize_redraws_the_whole_window() {
    let mut game = game();
    let mut region = damage::sink();
    let bigger = Rect::new(0, 0, game.client.width * 2, game.client.height * 2);
    game.relayout(bigger, Scale::ONE, &mut region);
    assert!(covers(&region, bigger));
}

#[test]
fn the_preferred_size_fits_the_board_and_the_floor_is_no_larger() {
    for difficulty in Difficulty::PRESETS {
        let game = game_of(difficulty);
        let (pref_w, pref_h) = game.preferred_size();
        let (min_w, min_h) = game.minimum_size();
        assert!(min_w <= pref_w && min_h <= pref_h);
    }
}

// --- Keyboard -----------------------------------------------------------

#[test]
fn the_arrow_keys_move_a_cursor_that_starts_in_the_middle() {
    let mut game = game();
    assert_eq!(game.focus.cursor, None);
    press_key(&mut game, Key::Named(NamedKey::Right), 0);
    assert_eq!(game.focus.cursor, Some(Coord::new(5, 4)));
    press_key(&mut game, Key::Named(NamedKey::Down), 0);
    assert_eq!(game.focus.cursor, Some(Coord::new(5, 5)));
    press_key(&mut game, Key::Named(NamedKey::Left), 0);
    press_key(&mut game, Key::Named(NamedKey::Up), 0);
    assert_eq!(game.focus.cursor, Some(Coord::new(4, 4)));
}

#[test]
fn the_cursor_stops_at_the_edge_and_reports_nothing() {
    let mut game = game();
    for _ in 0..20 {
        press_key(&mut game, Key::Named(NamedKey::Left), 0);
    }
    assert_eq!(game.focus.cursor, Some(Coord::new(0, 4)));
    let reaction = press_key(&mut game, Key::Named(NamedKey::Left), 0);
    assert_eq!(reaction, Reaction::IDLE, "already against the edge");
}

#[test]
fn space_reveals_the_cursor_cell() {
    let mut game = game();
    press_key(&mut game, Key::Char(' '), 0);
    assert_eq!(game.board().cover(Coord::new(4, 4)), Some(Cover::Open));
    assert_eq!(game.board().phase(), Phase::Playing);
}

#[test]
fn f_marks_the_cursor_cell() {
    let mut game = game();
    press_key(&mut game, Key::Char('f'), 0);
    assert_eq!(game.board().cover(Coord::new(4, 4)), Some(Cover::Flagged));
    press_key(&mut game, Key::Char('F'), 0);
    assert_eq!(
        game.board().cover(Coord::new(4, 4)),
        Some(Cover::Questioned)
    );
}

#[test]
fn n_starts_a_new_game_and_the_number_keys_pick_a_difficulty() {
    let mut game = game();
    press_key(&mut game, Key::Char(' '), 0);
    assert_eq!(game.board().phase(), Phase::Playing);
    press_key(&mut game, Key::Char('n'), 0);
    assert_eq!(game.board().phase(), Phase::Ready);
    assert!(press_key(&mut game, Key::Char('2'), 0).resized);
    assert_eq!(game.difficulty(), Difficulty::Intermediate);
    press_key(&mut game, Key::Char('3'), 0);
    assert_eq!(game.difficulty(), Difficulty::Expert);
    press_key(&mut game, Key::Char('1'), 0);
    assert_eq!(game.difficulty(), Difficulty::Beginner);
}

#[test]
fn an_unbound_key_does_nothing() {
    let mut game = game();
    for key in [
        Key::Char('z'),
        Key::Char('9'),
        Key::Named(NamedKey::Escape),
        Key::Named(NamedKey::Tab),
        Key::Named(NamedKey::Function { number: 1 }),
    ] {
        assert_eq!(press_key(&mut game, key, 0), Reaction::IDLE, "{key:?}");
    }
    assert_eq!(game.board().phase(), Phase::Ready);
}

// --- Settings -----------------------------------------------------------

#[test]
fn turning_reduced_motion_on_ends_the_animation_and_redraws() {
    let mut game = game();
    click(&mut game, Coord::new(4, 4), PointerButton::Primary, 0);
    assert!(!game.motion.is_idle());
    let mut region = damage::sink();
    game.set_reduced_motion(true, &mut region);
    assert!(game.motion.is_idle());
    assert!(covers(&region, game.client));

    let mut again = damage::sink();
    game.set_reduced_motion(true, &mut again);
    assert!(again.is_empty(), "no change, no repaint");
}

#[test]
fn turning_the_question_mark_off_reaches_the_board() {
    let mut game = game();
    game.set_questions(false);
    assert!(!game.questions());
    assert!(!game.board().questions());
    let at = Coord::new(2, 2);
    click(&mut game, at, PointerButton::Secondary, 0);
    click(&mut game, at, PointerButton::Secondary, 0);
    assert_eq!(game.board().cover(at), Some(Cover::Covered));
}

// --- Rendering ----------------------------------------------------------

#[test]
fn a_game_renders_at_every_stage_without_panicking() {
    for theme in [Theme::dark(), Theme::light()] {
        let mut game = game();
        let (width, height) = game.preferred_size();
        let mut surface = Surface::new(width, height).expect("a surface");
        let font = BitmapFont::monospace(14);
        game.render(&mut surface, &theme, font, 0);
        click(&mut game, Coord::new(4, 4), PointerButton::Primary, 0);
        for now in [0, 50_000_000, 400_000_000, 5 * SECOND_NS] {
            game.render(&mut surface, &theme, font, now);
        }
        let mine = first_mine(&mut game, 4);
        click(&mut game, mine, PointerButton::Primary, 0);
        for now in [0, 200_000_000, 2 * SECOND_NS] {
            game.render(&mut surface, &theme, font, now);
        }
    }
}
