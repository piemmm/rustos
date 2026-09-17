//! Saved-state tests: the round trip, and what a damaged document produces.

use super::Saved;
use crate::mind::Needs;
use tairix_appconf::Document;

#[test]
fn the_state_round_trips_through_the_settings_grammar() {
    let saved = Saved {
        needs: Needs {
            energy: 0.42,
            play: 0.7,
            affection: 0.13,
        },
        was_loose: true,
    };
    let document = saved.to_document().expect("a well-formed document");
    let read = Saved::from_document(&document);
    assert!(near(read.needs.energy, saved.needs.energy));
    assert!(near(read.needs.play, saved.needs.play));
    assert!(near(read.needs.affection, saved.needs.affection));
    assert_eq!(read.was_loose, saved.was_loose);
}

#[test]
fn the_document_is_the_key_equals_value_text_a_user_can_read() {
    let rendered = Saved::default()
        .to_document()
        .expect("a well-formed document")
        .render();
    assert!(rendered.contains("energy"));
    assert!(rendered.contains("loose"));
}

#[test]
fn an_empty_document_yields_a_usable_companion() {
    let read = Saved::from_document(&Document::new());
    assert_eq!(read, Saved::default());
}

#[test]
fn a_missing_line_falls_back_on_its_own_rather_than_losing_the_rest() {
    let mut document = Saved {
        needs: Needs {
            energy: 0.5,
            play: 0.5,
            affection: 0.5,
        },
        was_loose: true,
    }
    .to_document()
    .expect("a well-formed document");
    document.unset("play");
    let read = Saved::from_document(&document);
    assert!(near(read.needs.energy, 0.5), "the rest survives");
    assert!(
        near(read.needs.play, Saved::default().needs.play),
        "and the lost line falls back on its own"
    );
    assert!(read.was_loose);
}

#[test]
fn a_value_this_never_wrote_is_treated_as_absent_rather_than_repaired() {
    let mut document = Document::new();
    document.set("energy", "not a number").expect("settable");
    document.set("play", "50000").expect("settable");
    let read = Saved::from_document(&document);
    assert!(near(read.needs.energy, Saved::default().needs.energy));
    assert!(
        near(read.needs.play, Saved::default().needs.play),
        "a level this did not write is not one to believe, even clamped"
    );
}

#[test]
fn a_level_outside_the_range_is_bounded_on_the_way_out() {
    let document = Saved {
        needs: Needs {
            energy: 9.0,
            play: -3.0,
            affection: 0.5,
        },
        was_loose: false,
    }
    .to_document()
    .expect("bounded on the way out rather than refused");
    let read = Saved::from_document(&document);
    assert!(near(read.needs.energy, 1.0));
    assert!(near(read.needs.play, 0.0));
}

fn near(a: f64, b: f64) -> bool {
    tairix_util::mathf::fabs(a - b) < 0.002
}
