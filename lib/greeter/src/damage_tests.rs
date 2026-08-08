//! Unit tests for the changed-rectangle report: what each transition damages,
//! and that the report is never smaller than what the next paint actually
//! repaints.

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::Duration64;
use tairix_geometry::{Rect, Scale};
use tairix_input::{Key, NamedKey};

use crate::chooser::{AccountTile, Chooser};
use crate::surface::{panel_rect, AuthSurface, Chrome, Verdict};
use crate::testkit::{changed_pixels, feed, key, named, render, submit, theme, Scripted, SCREEN};

fn accounts() -> Vec<AccountTile> {
    vec![
        AccountTile::new("Ann", "ann"),
        AccountTile::new("Bob", "bob"),
    ]
}

fn chrome() -> Chrome {
    Chrome {
        clock: "09:41".into(),
        date: "Friday 7 August".into(),
        host: "tairix".into(),
    }
}

/// Every pixel a keystroke changes lies inside the rectangle the outcome
/// reported, so an embedder that repaints only that rectangle repaints
/// everything that moved.
#[test]
fn a_keystroke_damages_no_more_than_it_reports() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::refusing();
    let before = render(&surface);

    let outcome = feed(&mut surface, &key(Key::Char('a')), &mut verifier);
    let after = render(&surface);

    let damage = outcome.damage().expect("a placed surface reports a rect");
    let changed = changed_pixels(&before, &after);
    assert!(!changed.is_empty(), "the keystroke painted nothing at all");
    for pixel in changed {
        assert!(
            damage.contains(pixel),
            "the keystroke painted {pixel:?}, outside the reported {damage:?}"
        );
    }
}

/// The same holds for a verdict, which repaints the notice under the field.
#[test]
fn a_verdict_damages_no_more_than_it_reports() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Refused]);
    for ch in "wrong".chars() {
        feed(&mut surface, &key(Key::Char(ch)), &mut verifier);
    }
    let before = render(&surface);

    let outcome = feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    let after = render(&surface);

    let damage = outcome.damage().expect("a placed surface reports a rect");
    let changed = changed_pixels(&before, &after);
    assert!(!changed.is_empty(), "the verdict painted nothing at all");
    for pixel in changed {
        assert!(
            damage.contains(pixel),
            "the verdict painted {pixel:?}, outside the reported {damage:?}"
        );
    }
}

/// And for a chooser focus move, which repaints two tiles.
#[test]
fn a_focus_move_damages_no_more_than_it_reports() {
    let mut surface = AuthSurface::with_accounts(accounts());
    let mut verifier = Scripted::refusing();
    // One event first, so the surface knows where it is before the move
    // under test.
    feed(&mut surface, &named(NamedKey::Tab), &mut verifier);
    let before = render(&surface);

    let outcome = feed(&mut surface, &named(NamedKey::Tab), &mut verifier);
    let after = render(&surface);

    let damage = outcome.damage().expect("a placed surface reports a rect");
    let changed = changed_pixels(&before, &after);
    assert!(!changed.is_empty(), "the focus move painted nothing at all");
    for pixel in changed {
        assert!(
            damage.contains(pixel),
            "the focus move painted {pixel:?}, outside the reported {damage:?}"
        );
    }
}

/// And for the clock, which repaints only its own band.
#[test]
fn a_chrome_change_damages_no_more_than_it_reports() {
    let mut surface = AuthSurface::new("ann");
    surface.set_chrome(chrome());
    let before = render(&surface);

    let outcome = surface.set_chrome(Chrome {
        clock: "09:42".into(),
        ..chrome()
    });
    let after = render(&surface);

    let damage = outcome.damage().expect("a rendered surface reports a rect");
    let changed = changed_pixels(&before, &after);
    assert!(!changed.is_empty(), "the clock painted nothing at all");
    for pixel in changed {
        assert!(
            damage.contains(pixel),
            "the clock painted {pixel:?}, outside the reported {damage:?}"
        );
    }
}

/// A keystroke reports the field it typed into; a verdict reports the whole
/// panel, because the notice under the field changes with it.
#[test]
fn a_keystroke_reports_the_field_and_a_verdict_reports_the_panel() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Refused]);
    let field = surface.field_rect(SCREEN, Scale::ONE, &theme());

    let typed = feed(&mut surface, &key(Key::Char('a')), &mut verifier);
    let answered = feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

    assert_eq!(typed.damage(), Some(field));
    assert_eq!(answered.damage(), Some(panel_rect(SCREEN, Scale::ONE)));
}

/// A focus move reports the union of the two tiles the mark is leaving and
/// arriving at, so every pixel the selection fade repaints is inside it.
#[test]
fn a_focus_move_reports_the_animating_tiles() {
    let mut surface = AuthSurface::with_accounts(accounts());
    let mut verifier = Scripted::refusing();
    let grid = Chooser::new(accounts());
    let expected = grid
        .tile_bounds_of(&[0, 1], SCREEN, Scale::ONE)
        .expect("two tiles");

    // Place the surface so damage is a rectangle rather than the whole screen.
    let _ = render(&surface);
    let moved = feed(&mut surface, &named(NamedKey::Tab), &mut verifier);

    assert_eq!(moved.damage(), Some(expected));
    assert!(expected.width <= grid.bounds(SCREEN, Scale::ONE).width);
}

/// A clock tick reports its own band and nothing else, so an idle login
/// screen does not repaint the whole display once a minute.
#[test]
fn a_clock_tick_reports_only_the_chrome_band() {
    let mut surface = AuthSurface::new("ann");
    let _ = render(&surface);

    let ticked = surface.set_chrome(chrome());

    let band = ticked.damage().expect("a rendered surface reports a rect");
    assert!(band.width > 0 && band.height > 0);
    assert!(
        band.bottom() <= panel_rect(SCREEN, Scale::ONE).top(),
        "the chrome band reached the panel"
    );
}

/// A mode change redraws everything, so it reports the whole screen rather
/// than a rectangle that would leave the previous mode's pixels behind.
#[test]
fn every_mode_change_reports_the_whole_screen() {
    let mut surface = AuthSurface::with_accounts(accounts());
    let mut verifier = Scripted::refusing();

    let picked = feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    let stepped_back = feed(&mut surface, &named(NamedKey::Escape), &mut verifier);
    feed(&mut surface, &named(NamedKey::Left), &mut verifier);
    let other = feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

    assert_eq!(picked.damage(), None);
    assert_eq!(stepped_back.damage(), None);
    assert_eq!(other.damage(), None);
}

/// A verified secret hands the screen back, so there is nothing left to
/// repaint in part.
#[test]
fn a_verified_secret_reports_the_whole_screen() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Verified]);

    let outcome = submit(&mut surface, "hunter2", &mut verifier);

    assert!(outcome.verified());
    assert_eq!(outcome.damage(), None);
}

/// An event that changes nothing reports an empty rectangle rather than a
/// repaint nobody needs.
#[test]
fn an_event_that_changes_nothing_reports_an_empty_rectangle() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::refusing();

    let outcome = feed(&mut surface, &crate::testkit::moved(1, 1), &mut verifier);

    assert!(!outcome.redraw());
    assert_eq!(outcome.damage(), Some(Rect::EMPTY));
}

/// Before the surface has ever been painted or asked about an event it does
/// not know where it is, and says so rather than guessing a rectangle.
#[test]
fn an_unplaced_surface_reports_the_whole_screen() {
    let mut surface = AuthSurface::new("ann");

    assert_eq!(surface.set_chrome(chrome()).damage(), None);
    assert_eq!(
        surface.set_cooldown(Duration64::from_secs(5)).damage(),
        None
    );
}

/// Advancing a running fade reports damage that contains both animating tiles,
/// and reports no change when nothing is animating.
#[test]
fn advance_reports_animating_tile_damage_or_nothing() {
    use crate::testkit::feed_at;
    use tairix_theme::MotionInteraction;

    let mut surface = AuthSurface::with_accounts(accounts());
    let mut verifier = Scripted::refusing();
    let _ = render(&surface);

    assert!(!surface.advance(0).redraw(), "idle advance changes nothing");
    assert_eq!(surface.motion_due(0), None);

    let millis = theme()
        .motion()
        .duration(MotionInteraction::SelectionChange);
    let span_ns = u64::from(millis) * 1_000_000;
    feed_at(&mut surface, &named(NamedKey::Tab), &mut verifier, 0);

    let tiles = Chooser::new(accounts())
        .tile_bounds_of(&[0, 1], SCREEN, Scale::ONE)
        .expect("tiles");
    let mid = surface.advance(span_ns / 2);
    assert!(mid.redraw());
    let damage = mid.damage().expect("damage");
    assert_eq!(damage, tiles);

    assert!(surface.advance(span_ns).redraw());
    assert!(!surface.advance(span_ns + 1).redraw());
    assert_eq!(surface.motion_due(span_ns + 1), None);
}

/// A question at rest asks for nothing: the seam folds every mode, so what
/// makes it quiet here is that nothing is running, not the mode it is in.
#[test]
fn advancing_a_resting_question_is_quiet() {
    let mut surface = AuthSurface::new("ann");
    let _ = render(&surface);
    assert!(!surface.advance(1_000).redraw());
    assert_eq!(surface.motion_due(1_000), None);
}
