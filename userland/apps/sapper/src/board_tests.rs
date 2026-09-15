//! Board rule tests.
//!
//! A child module of `board`, so a fixture may place mines directly and the
//! production type carries no test-only constructor.

use super::*;

use tairix_rng::NonCryptoRng;

/// A board with its mines already laid from an ASCII map: `*` is a mine, any
/// other character is not. Rows must be equal length.
fn laid(map: &[&str]) -> Board {
    let rows = u16::try_from(map.len()).expect("test map fits");
    let cols = u16::try_from(map[0].len()).expect("test map fits");
    assert!(
        map.iter().all(|row| row.len() == usize::from(cols)),
        "ragged test map"
    );
    let mines = u32::try_from(
        map.iter()
            .flat_map(|r| r.bytes())
            .filter(|&b| b == b'*')
            .count(),
    )
    .expect("test map fits");
    let mut board = Board::new(Dimensions { cols, rows, mines }, true);
    for (row, line) in map.iter().enumerate() {
        for (col, byte) in line.bytes().enumerate() {
            board.cells[row * usize::from(cols) + col].mine = byte == b'*';
        }
    }
    for index in 0..board.cells.len() {
        let at = board.coord(index).expect("index is on the board");
        let count = board
            .neighbours(at)
            .filter(|&n| board.is_mine(n) == Some(true))
            .count();
        board.cells[index].adjacent = u8::try_from(count).expect("at most eight neighbours");
    }
    board.laid = true;
    board.phase = Phase::Playing;
    board
}

/// Four mines in a square, leaving `(1,1)` reading `4` and its four edge
/// neighbours reading `2`. Every cell around the cluster opens alone, so a
/// reveal in it cannot flood the board and end the game by accident.
const CLUSTER: [&str; 8] = [
    "*.*.....", "........", "*.*.....", "........", "........", "........", "........", "........",
];

/// A clear 4×4 top-left region walled off by mines, with the rest of the board
/// beyond the wall: a flood with a known shape that does not win.
const WALLED: [&str; 8] = [
    "....*...", "....*...", "....*...", "....*...", "*****...", "........", "........", "........",
];

fn rng(seed: u64) -> NonCryptoRng {
    NonCryptoRng::seed_from_u64(seed)
}

fn all_covers(board: &Board) -> Vec<Cover> {
    board.iter().map(|(_, cover, _)| cover).collect()
}

// --- Dimensions ---------------------------------------------------------

#[test]
fn a_side_outside_the_bounds_is_refused() {
    assert_eq!(Dimensions::new(MIN_SIDE - 1, 9, 1), None);
    assert_eq!(Dimensions::new(9, MIN_SIDE - 1, 1), None);
    assert_eq!(Dimensions::new(MAX_SIDE + 1, 9, 1), None);
    assert_eq!(Dimensions::new(9, MAX_SIDE + 1, 1), None);
    assert!(Dimensions::new(MIN_SIDE, MIN_SIDE, 1).is_some());
    assert!(Dimensions::new(MAX_SIDE, MAX_SIDE, 1).is_some());
}

#[test]
fn a_board_with_no_mines_is_refused() {
    assert_eq!(Dimensions::new(9, 9, 0), None);
}

#[test]
fn more_mines_than_fit_outside_the_safe_region_are_refused() {
    let most = Dimensions::max_mines(9, 9);
    assert_eq!(most, 81 - 9);
    assert!(Dimensions::new(9, 9, most).is_some());
    assert_eq!(Dimensions::new(9, 9, most + 1), None);
}

#[test]
fn the_smallest_board_still_has_room_for_a_mine() {
    assert!(Dimensions::max_mines(MIN_SIDE, MIN_SIDE) >= 1);
}

#[test]
fn presets_are_valid() {
    for preset in Difficulty::PRESETS {
        let dims = preset.dimensions();
        assert_eq!(
            Dimensions::new(dims.cols(), dims.rows(), dims.mines()),
            Some(dims),
            "{} is not a legal board",
            preset.title()
        );
    }
}

#[test]
fn preset_identifiers_round_trip_and_custom_has_none() {
    for preset in Difficulty::PRESETS {
        let id = preset.preset_id().expect("a preset has an identifier");
        assert_eq!(Difficulty::from_preset_id(id), Some(preset));
    }
    let custom = Difficulty::Custom(Dimensions::new(10, 10, 12).expect("legal"));
    assert_eq!(custom.preset_id(), None);
    assert_eq!(Difficulty::from_preset_id("nonesuch"), None);
}

// --- Laying the mines ---------------------------------------------------

#[test]
fn the_opening_move_never_strikes_a_mine() {
    let dims = Difficulty::Beginner.dimensions();
    for seed in 0..200 {
        for at in [
            Coord::new(0, 0),
            Coord::new(4, 4),
            Coord::new(8, 8),
            Coord::new(0, 8),
        ] {
            let mut board = Board::new(dims, true);
            let outcome = board.reveal(at, &mut rng(seed)).outcome;
            assert_ne!(outcome, Outcome::Detonated, "seed {seed} at {at:?}");
            assert_ne!(board.phase(), Phase::Lost);
        }
    }
}

#[test]
fn the_opening_move_clears_the_whole_safe_region() {
    let dims = Difficulty::Expert.dimensions();
    for seed in 0..100 {
        let mut board = Board::new(dims, true);
        let at = Coord::new(7, 5);
        board.reveal(at, &mut rng(seed));
        assert_eq!(board.is_mine(at), Some(false));
        for n in board.neighbours(at).collect::<Vec<_>>() {
            assert_eq!(board.is_mine(n), Some(false), "seed {seed} neighbour {n:?}");
        }
    }
}

#[test]
fn the_opening_move_always_opens_more_than_one_cell() {
    // The point of clearing the whole safe region rather than only the clicked
    // cell: the first click can never strand the player on a bare number.
    let dims = Difficulty::Expert.dimensions();
    for seed in 0..100 {
        let mut board = Board::new(dims, true);
        let opened = board.reveal(Coord::new(7, 5), &mut rng(seed));
        assert!(
            opened.steps.len() >= 9,
            "seed {seed}: {} cells",
            opened.steps.len()
        );
    }
}

#[test]
fn the_opening_move_lays_exactly_the_requested_mines() {
    for preset in Difficulty::PRESETS {
        let dims = preset.dimensions();
        let mut board = Board::new(dims, true);
        board.reveal(Coord::new(2, 2), &mut rng(7));
        let laid = board.cells.iter().filter(|c| c.mine).count();
        assert_eq!(u32::try_from(laid).expect("fits"), dims.mines());
    }
}

#[test]
fn every_eligible_cell_can_receive_a_mine() {
    let dims = Dimensions::new(5, 5, 1).expect("legal");
    let first = Coord::new(0, 0);
    let mut seen = alloc::collections::BTreeSet::new();
    for seed in 0..400 {
        let mut board = Board::new(dims, true);
        board.reveal(first, &mut rng(seed));
        for (index, cell) in board.cells.iter().enumerate() {
            if cell.mine {
                seen.insert(index);
            }
        }
    }
    // Twenty-five cells less the four-cell safe region a corner click clears.
    assert_eq!(seen.len(), 25 - 4, "some eligible cell never took a mine");
}

#[test]
fn no_mine_lands_before_the_first_reveal() {
    let board = Board::new(Difficulty::Beginner.dimensions(), true);
    assert_eq!(board.phase(), Phase::Ready);
    assert!(board.cells.iter().all(|cell| !cell.mine));
}

// --- Revealing ----------------------------------------------------------

#[test]
fn a_flood_carries_the_ring_it_was_reached_on() {
    let mut board = laid(&WALLED);
    let opened = board.reveal(Coord::new(0, 0), &mut rng(1));
    assert_eq!(opened.outcome, Outcome::Opened);
    assert_eq!(opened.steps.len(), 16, "the walled-off region, and only it");
    let ring_of = |at: Coord| {
        opened
            .steps
            .iter()
            .find(|s| s.at == at)
            .map(|s| s.ring)
            .expect("cell was opened")
    };
    assert_eq!(ring_of(Coord::new(0, 0)), 0);
    assert_eq!(ring_of(Coord::new(1, 1)), 1);
    assert_eq!(ring_of(Coord::new(2, 2)), 2);
    assert_eq!(ring_of(Coord::new(3, 3)), 3);
    let mut previous = 0;
    for step in &opened.steps {
        assert!(step.ring >= previous, "rings must not decrease");
        previous = step.ring;
    }
}

#[test]
fn a_flood_stops_at_the_wall_it_cannot_pass() {
    let mut board = laid(&WALLED);
    board.reveal(Coord::new(0, 0), &mut rng(1));
    assert_eq!(board.cover(Coord::new(3, 3)), Some(Cover::Open));
    assert_eq!(
        board.cover(Coord::new(5, 5)),
        Some(Cover::Covered),
        "the far side of the wall is untouched"
    );
    assert_eq!(board.phase(), Phase::Playing);
}

#[test]
fn a_numbered_cell_opens_alone() {
    let mut board = laid(&CLUSTER);
    let opened = board.reveal(Coord::new(1, 1), &mut rng(1));
    assert_eq!(opened.steps.len(), 1);
    assert_eq!(opened.outcome, Outcome::Opened);
    assert_eq!(board.adjacent(Coord::new(1, 1)), Some(4));
}

#[test]
fn revealing_a_flagged_cell_does_nothing() {
    let mut board = laid(&CLUSTER);
    let at = Coord::new(1, 1);
    board.toggle_mark(at);
    assert!(board.reveal(at, &mut rng(1)).is_nothing());
    assert_eq!(board.cover(at), Some(Cover::Flagged));
}

#[test]
fn revealing_a_questioned_cell_opens_it() {
    let mut board = laid(&CLUSTER);
    let at = Coord::new(1, 1);
    board.toggle_mark(at);
    board.toggle_mark(at);
    assert_eq!(board.cover(at), Some(Cover::Questioned));
    board.reveal(at, &mut rng(1));
    assert_eq!(board.cover(at), Some(Cover::Open));
}

#[test]
fn a_flood_stops_at_a_flag() {
    // A flag is the player's own claim, so a cascade must not sweep it away and
    // silently open what it was protecting.
    let mut board = laid(&WALLED);
    let flagged = Coord::new(2, 2);
    board.toggle_mark(flagged);
    assert_eq!(board.remaining(), 8);
    let opened = board.reveal(Coord::new(0, 0), &mut rng(1));
    assert_eq!(board.cover(flagged), Some(Cover::Flagged));
    assert_eq!(board.remaining(), 8, "the flag is still placed");
    assert!(opened.steps.iter().all(|s| s.at != flagged));
}

// --- Marking ------------------------------------------------------------

#[test]
fn the_mark_cycle_runs_covered_flagged_questioned_covered() {
    let mut board = laid(&CLUSTER);
    let at = Coord::new(5, 5);
    assert_eq!(board.cover(at), Some(Cover::Covered));
    board.toggle_mark(at);
    assert_eq!(board.cover(at), Some(Cover::Flagged));
    assert_eq!(board.remaining(), 3);
    board.toggle_mark(at);
    assert_eq!(board.cover(at), Some(Cover::Questioned));
    assert_eq!(board.remaining(), 4, "a question mark is not a flag");
    board.toggle_mark(at);
    assert_eq!(board.cover(at), Some(Cover::Covered));
}

#[test]
fn the_mark_cycle_skips_the_question_when_it_is_off() {
    let mut board = laid(&CLUSTER);
    board.set_questions(false);
    let at = Coord::new(5, 5);
    board.toggle_mark(at);
    assert_eq!(board.cover(at), Some(Cover::Flagged));
    board.toggle_mark(at);
    assert_eq!(board.cover(at), Some(Cover::Covered));
}

#[test]
fn turning_questions_off_leaves_a_placed_question_alone() {
    let mut board = laid(&CLUSTER);
    let at = Coord::new(5, 5);
    board.toggle_mark(at);
    board.toggle_mark(at);
    board.set_questions(false);
    assert_eq!(board.cover(at), Some(Cover::Questioned));
    board.toggle_mark(at);
    assert_eq!(board.cover(at), Some(Cover::Covered));
}

#[test]
fn the_counter_goes_negative_past_the_mine_count() {
    let mut board = laid(&CLUSTER);
    for col in 0..5 {
        board.toggle_mark(Coord::new(col, 5));
    }
    assert_eq!(board.remaining(), -1);
}

#[test]
fn marking_an_open_cell_does_nothing() {
    let mut board = laid(&CLUSTER);
    let at = Coord::new(1, 1);
    board.reveal(at, &mut rng(1));
    assert_eq!(board.phase(), Phase::Playing, "the game is still running");
    assert!(board.toggle_mark(at).is_nothing());
    assert_eq!(board.cover(at), Some(Cover::Open));
}

// --- Losing -------------------------------------------------------------

#[test]
fn striking_a_mine_exposes_every_other_and_marks_wrong_flags() {
    let mut board = laid(&["*...*", ".....", "..*..", ".....", "*...*"]);
    let wrong = Coord::new(2, 3);
    board.toggle_mark(wrong);
    let correct = Coord::new(0, 0);
    board.toggle_mark(correct);
    let lost = board.reveal(Coord::new(2, 2), &mut rng(1));

    assert_eq!(lost.outcome, Outcome::Detonated);
    assert_eq!(board.phase(), Phase::Lost);
    assert_eq!(
        board.cover(Coord::new(2, 2)),
        Some(Cover::Exposed { struck: true })
    );
    assert_eq!(
        board.cover(Coord::new(4, 0)),
        Some(Cover::Exposed { struck: false })
    );
    assert_eq!(board.cover(wrong), Some(Cover::Misflagged));
    assert_eq!(
        board.cover(correct),
        Some(Cover::Flagged),
        "a flag that was right stays a flag"
    );
}

#[test]
fn the_detonation_chain_is_ordered_by_distance_from_the_struck_mine() {
    let mut board = laid(&["*....", ".....", "..*..", ".....", "....*"]);
    let lost = board.reveal(Coord::new(2, 2), &mut rng(1));
    let rings: Vec<u16> = lost.steps.iter().map(|s| s.ring).collect();
    assert_eq!(rings.first(), Some(&0), "the struck mine leads");
    // Both corners are two rings out from the centre.
    assert_eq!(rings, vec![0, 2, 2]);
}

#[test]
fn a_finished_game_accepts_no_further_action() {
    let mut board = laid(&CLUSTER);
    board.reveal(Coord::new(0, 0), &mut rng(1));
    assert_eq!(board.phase(), Phase::Lost);
    let before = all_covers(&board);
    assert!(board.reveal(Coord::new(5, 5), &mut rng(1)).is_nothing());
    assert!(board.toggle_mark(Coord::new(5, 5)).is_nothing());
    assert!(board.chord(Coord::new(5, 5)).is_nothing());
    assert!(board.flag_chord(Coord::new(5, 5)).is_nothing());
    assert_eq!(before, all_covers(&board));
}

// --- Chording -----------------------------------------------------------

#[test]
fn chording_needs_the_flags_to_match() {
    let mut board = laid(&CLUSTER);
    let anchor = Coord::new(1, 0);
    board.reveal(anchor, &mut rng(1));
    assert_eq!(board.adjacent(anchor), Some(2));
    assert!(board.chord(anchor).is_nothing(), "no flag placed yet");
    board.toggle_mark(Coord::new(0, 0));
    assert!(board.chord(anchor).is_nothing(), "one flag, two mines");
    board.toggle_mark(Coord::new(2, 0));
    assert!(!board.chord(anchor).is_nothing(), "both mines marked");
}

#[test]
fn chording_reveals_the_unflagged_neighbours() {
    let mut board = laid(&CLUSTER);
    let anchor = Coord::new(1, 0);
    board.reveal(anchor, &mut rng(1));
    board.toggle_mark(Coord::new(0, 0));
    board.toggle_mark(Coord::new(2, 0));
    let acted = board.chord(anchor);
    assert_eq!(acted.outcome, Outcome::Opened);
    for at in [Coord::new(0, 1), Coord::new(1, 1), Coord::new(2, 1)] {
        assert_eq!(board.cover(at), Some(Cover::Open), "{at:?}");
    }
    assert_eq!(
        board.cover(Coord::new(0, 0)),
        Some(Cover::Flagged),
        "the flag protected its cell"
    );
    assert_eq!(board.phase(), Phase::Playing);
}

#[test]
fn chording_onto_a_mine_detonates_it() {
    let mut board = laid(&CLUSTER);
    let anchor = Coord::new(1, 1);
    board.reveal(anchor, &mut rng(1));
    assert_eq!(board.adjacent(anchor), Some(4));
    // Four flags, two of them wrong: a chord is a claim, and acting on a wrong
    // claim is how a player loses.
    for at in [
        Coord::new(0, 0),
        Coord::new(2, 0),
        Coord::new(1, 0),
        Coord::new(1, 2),
    ] {
        board.toggle_mark(at);
    }
    let acted = board.chord(anchor);
    assert_eq!(acted.outcome, Outcome::Detonated);
    assert_eq!(board.phase(), Phase::Lost);
    assert_eq!(board.cover(Coord::new(1, 0)), Some(Cover::Misflagged));
}

#[test]
fn chording_a_covered_cell_does_nothing() {
    let mut board = laid(&CLUSTER);
    assert!(board.chord(Coord::new(5, 5)).is_nothing());
    assert_eq!(board.phase(), Phase::Playing);
}

#[test]
fn chording_a_zero_does_nothing() {
    let mut board = laid(&WALLED);
    let zero = Coord::new(1, 1);
    board.reveal(zero, &mut rng(1));
    assert_eq!(board.adjacent(zero), Some(0));
    assert!(board.chord(zero).is_nothing());
}

#[test]
fn flag_chording_flags_the_covered_neighbours() {
    let mut board = laid(&CLUSTER);
    let anchor = Coord::new(1, 1);
    for at in [
        Coord::new(1, 0),
        Coord::new(0, 1),
        Coord::new(2, 1),
        Coord::new(1, 2),
        anchor,
    ] {
        board.reveal(at, &mut rng(1));
    }
    let acted = board.flag_chord(anchor);
    assert_eq!(acted.outcome, Outcome::Marked);
    assert_eq!(acted.steps.len(), 4);
    for at in [
        Coord::new(0, 0),
        Coord::new(2, 0),
        Coord::new(0, 2),
        Coord::new(2, 2),
    ] {
        assert_eq!(board.cover(at), Some(Cover::Flagged), "{at:?}");
    }
    assert_eq!(board.remaining(), 0);
}

#[test]
fn flag_chording_needs_the_count_to_match() {
    let mut board = laid(&CLUSTER);
    let anchor = Coord::new(1, 1);
    board.reveal(anchor, &mut rng(1));
    // Eight covered neighbours around a `4`: the claim does not hold.
    assert!(board.flag_chord(anchor).is_nothing());
}

#[test]
fn flag_chording_a_zero_does_nothing() {
    let mut board = laid(&WALLED);
    let zero = Coord::new(1, 1);
    board.reveal(zero, &mut rng(1));
    assert!(board.flag_chord(zero).is_nothing());
}

// --- Winning ------------------------------------------------------------

#[test]
fn opening_the_last_safe_cell_wins_and_flags_every_mine() {
    let mut board = laid(&["*...*", ".....", ".....", ".....", "*...*"]);
    let acted = board.reveal(Coord::new(2, 2), &mut rng(1));
    assert_eq!(acted.outcome, Outcome::Won);
    assert_eq!(board.phase(), Phase::Won);
    let corners = [
        Coord::new(0, 0),
        Coord::new(4, 0),
        Coord::new(0, 4),
        Coord::new(4, 4),
    ];
    for at in corners {
        assert_eq!(board.cover(at), Some(Cover::Flagged));
    }
    assert_eq!(board.remaining(), 0);
    let flagged = acted
        .steps
        .iter()
        .filter(|s| corners.contains(&s.at))
        .count();
    assert_eq!(flagged, 4, "the auto-flags are in the repaint set");
}

#[test]
fn the_win_does_not_need_every_mine_flagged_first() {
    let mut board = laid(&["*....", ".....", ".....", ".....", "....."]);
    for row in 0..5 {
        for col in 0..5 {
            let at = Coord::new(col, row);
            if board.is_mine(at) == Some(false) {
                board.reveal(at, &mut rng(1));
            }
        }
    }
    assert_eq!(board.phase(), Phase::Won);
}

#[test]
fn a_won_board_reports_no_mines_remaining() {
    let mut board = laid(&["*...*", ".....", ".....", ".....", "*...*"]);
    board.toggle_mark(Coord::new(0, 0));
    board.reveal(Coord::new(2, 2), &mut rng(1));
    assert_eq!(board.phase(), Phase::Won);
    assert_eq!(board.remaining(), 0, "a flag already placed is not doubled");
}

#[test]
fn a_flag_on_a_safe_cell_holds_the_win_back() {
    // The flag blocks the flood, so the cell it covers is never opened and the
    // game is not yet won: the player must take their own claim back first.
    let mut board = laid(&["*...*", ".....", ".....", ".....", "*...*"]);
    let mistake = Coord::new(2, 0);
    board.toggle_mark(mistake);
    board.reveal(Coord::new(2, 2), &mut rng(1));
    assert_eq!(board.phase(), Phase::Playing);
    // Flagged → questioned → covered.
    board.toggle_mark(mistake);
    board.toggle_mark(mistake);
    assert_eq!(board.cover(mistake), Some(Cover::Covered));
    let acted = board.reveal(mistake, &mut rng(1));
    assert_eq!(acted.outcome, Outcome::Won);
}

// --- Traversal ----------------------------------------------------------

#[test]
fn iter_visits_every_cell_in_row_major_order() {
    let board = laid(&["*...", "....", "....", "...."]);
    let seen: Vec<Coord> = board.iter().map(|(at, _, _)| at).collect();
    assert_eq!(seen.len(), 16);
    assert_eq!(seen[0], Coord::new(0, 0));
    assert_eq!(seen[1], Coord::new(1, 0));
    assert_eq!(seen[4], Coord::new(0, 1));
    assert_eq!(seen[15], Coord::new(3, 3));
    let (_, _, adjacent) = board.iter().nth(1).expect("second cell");
    assert_eq!(adjacent, 1);
}

#[test]
fn a_coordinate_off_the_board_reads_as_nothing() {
    let board = laid(&["*...", "....", "....", "...."]);
    assert!(!board.contains(Coord::new(4, 0)));
    assert_eq!(board.cover(Coord::new(4, 0)), None);
    assert_eq!(board.adjacent(Coord::new(0, 4)), None);
    assert_eq!(board.is_mine(Coord::new(9, 9)), None);
    let mut board = board;
    assert!(board.reveal(Coord::new(9, 9), &mut rng(1)).is_nothing());
    assert!(board.toggle_mark(Coord::new(9, 9)).is_nothing());
}
