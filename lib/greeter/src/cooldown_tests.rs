//! Unit tests for the two things the embedder pushes into the surface: the
//! authority's remaining lockout, and the clock, date, and host name on the
//! backdrop.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::Duration64;
use tairix_geometry::Scale;
use tairix_input::NamedKey;

use crate::chooser::AccountTile;
use crate::surface::{AuthSurface, Chrome, Verdict, CHOOSE_HINT, HINT, MAX_CHROME};
use crate::testkit::{changed_pixels, feed, named, render, submit, theme, Scripted, SCREEN};

fn chrome(clock: &str) -> Chrome {
    Chrome {
        clock: String::from(clock),
        date: String::from("Friday 7 August"),
        host: String::from("tairix"),
    }
}

/// A live lockout is shown, so the person at the keyboard knows why nothing
/// is happening rather than believing the machine is broken.
#[test]
fn a_live_cooldown_is_shown_under_the_field() {
    let mut surface = AuthSurface::new("ann");

    let outcome = surface.set_cooldown(Duration64::from_secs(42));

    assert!(outcome.redraw());
    assert!(surface.notice().contains("42"));
    assert_ne!(surface.notice(), HINT);
}

/// While it stands, a submission never reaches the authority: the surface
/// re-shows the lockout instead of asking.
#[test]
fn a_submission_during_a_cooldown_never_reaches_the_verifier() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Verified]);
    surface.set_cooldown(Duration64::from_secs(30));
    let locked = String::from(surface.notice());

    let outcome = submit(&mut surface, "hunter2", &mut verifier);

    assert!(!outcome.verified());
    assert!(
        verifier.offered.is_empty(),
        "the authority was asked anyway"
    );
    assert_eq!(surface.notice(), locked);
}

/// The secret does not sit in memory through a lockout that may last
/// minutes: a refused-for-cooldown submission erases it like any other.
#[test]
fn a_submission_during_a_cooldown_still_erases_the_secret() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::refusing();
    surface.set_cooldown(Duration64::from_secs(30));
    submit(&mut surface, "hunter2", &mut verifier);

    surface.set_cooldown(Duration64::ZERO);
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

    assert_eq!(verifier.offered, vec![String::new()]);
}

/// Typing during a lockout does not clear the notice: the lockout is not a
/// verdict on what is being typed now, and stands until the authority says
/// otherwise.
#[test]
fn typing_does_not_clear_a_live_cooldown() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::refusing();
    surface.set_cooldown(Duration64::from_secs(9));
    let locked = String::from(surface.notice());

    feed(&mut surface, &named(NamedKey::Backspace), &mut verifier);

    assert_eq!(surface.notice(), locked);
}

/// Zero clears it, and the surface goes back to asking.
#[test]
fn zero_clears_the_cooldown_and_submitting_works_again() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Verified]);
    surface.set_cooldown(Duration64::from_secs(5));

    let cleared = surface.set_cooldown(Duration64::ZERO);

    assert!(cleared.redraw());
    assert_eq!(surface.notice(), HINT);
    assert!(submit(&mut surface, "hunter2", &mut verifier).verified());
}

/// A negative span is not a lockout; treating it as one would leave a
/// surface that could never be submitted again.
#[test]
fn a_negative_cooldown_is_not_a_lockout() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Verified]);

    let outcome = surface.set_cooldown(Duration64::from_secs(-5));

    assert!(!outcome.redraw());
    assert_eq!(surface.notice(), HINT);
    assert!(submit(&mut surface, "hunter2", &mut verifier).verified());
}

/// A sub-second remainder still reads as a whole second left, so a lockout
/// with time on it never reads as over.
#[test]
fn a_part_second_remainder_rounds_up() {
    let mut surface = AuthSurface::new("ann");

    surface.set_cooldown(Duration64::new(0, 1).expect("a canonical span"));

    assert!(surface.notice().contains('1'), "{}", surface.notice());
}

/// The same lockout again changes nothing, so an embedder that re-reports it
/// every tick does not force a repaint.
#[test]
fn an_unchanged_cooldown_asks_for_no_repaint() {
    let mut surface = AuthSurface::new("ann");
    surface.set_cooldown(Duration64::from_secs(5));

    let again = surface.set_cooldown(Duration64::from_secs(5));

    assert!(!again.redraw());
    assert_eq!(again.damage().map(|rect| rect.is_empty()), Some(true));
}

/// A lockout is the authority's answer about *one* account, so stepping back
/// to the chooser drops it rather than carrying it to the next.
#[test]
fn stepping_back_to_the_chooser_drops_the_previous_account_s_cooldown() {
    let mut surface = AuthSurface::with_accounts(vec![
        AccountTile::new("Ann", "ann"),
        AccountTile::new("Bob", "bob"),
    ]);
    let mut verifier = Scripted::new(vec![Verdict::Verified]);

    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    surface.set_cooldown(Duration64::from_secs(60));
    feed(&mut surface, &named(NamedKey::Escape), &mut verifier);
    assert_eq!(surface.notice(), CHOOSE_HINT);

    feed(&mut surface, &named(NamedKey::Tab), &mut verifier);
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

    assert_eq!(surface.selected_account(), Some("bob"));
    assert_eq!(surface.notice(), HINT);
    assert!(submit(&mut surface, "hunter2", &mut verifier).verified());
}

#[test]
fn chrome_is_drawn_on_the_backdrop() {
    let bare = AuthSurface::new("ann");
    let mut dressed = AuthSurface::new("ann");

    let outcome = dressed.set_chrome(chrome("09:41"));

    assert!(outcome.redraw());
    assert_ne!(render(&bare), render(&dressed));
}

/// Chrome that is already on screen changes nothing, so a clock re-reported
/// within the same minute costs no frame.
#[test]
fn unchanged_chrome_asks_for_no_repaint() {
    let mut surface = AuthSurface::new("ann");
    surface.set_chrome(chrome("09:41"));

    let again = surface.set_chrome(chrome("09:41"));

    assert!(!again.redraw());
    assert_eq!(again.damage().map(|rect| rect.is_empty()), Some(true));
}

/// The strings are display text and are bounded, so a host name from a
/// hostile or broken source cannot run off the screen.
#[test]
fn chrome_strings_are_cut_to_the_documented_bound() {
    let mut surface = AuthSurface::new("ann");
    let long = "x".repeat(MAX_CHROME * 3);

    let first = surface.set_chrome(Chrome {
        clock: long.clone(),
        date: long.clone(),
        host: long,
    });
    let cut = "x".repeat(MAX_CHROME);
    let again = surface.set_chrome(Chrome {
        clock: cut.clone(),
        date: cut.clone(),
        host: cut,
    });

    assert!(first.redraw());
    assert!(
        !again.redraw(),
        "the over-long chrome was not cut to the bound"
    );
}

/// Chrome is drawn above the panel and never inside it, so it cannot cover
/// the thing the user is typing into.
#[test]
fn chrome_stays_clear_of_the_panel() {
    let mut surface = AuthSurface::new("ann");
    let before = render(&surface);
    surface.set_chrome(chrome("09:41"));
    let after = render(&surface);

    let field = surface.field_rect(SCREEN, Scale::ONE, &theme());
    let touched: Vec<_> = changed_pixels(&before, &after)
        .into_iter()
        .filter(|pixel| field.contains(*pixel))
        .collect();

    assert!(touched.is_empty(), "chrome painted inside the field");
}
