//! Unit tests for the account chooser: its focus model, the `Other…` tile
//! that is always there, picking a tile with the keyboard or the pointer,
//! stepping back, and the badge on an account that is already signed in.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_controls::IconTile;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey};

use crate::chooser::{AccountTile, Chooser, Step, FALLBACK_MONOGRAM, OTHER_LABEL};
use crate::surface::{
    AuthSurface, Backdrop, EventContext, Verdict, CHOOSE_HINT, HINT, NAME_HINT, NAME_REQUIRED,
};
use crate::testkit::{
    centre, feed, key, key_with, named, painted, render, submit, theme, Scripted, PRESS, RELEASE,
    SCREEN,
};

fn accounts(names: &[&str]) -> Vec<AccountTile> {
    names
        .iter()
        .map(|name| AccountTile::new(name, name))
        .collect()
}

/// A chooser over the same accounts the surface was given, for asking where
/// its tiles are.
fn grid(names: &[&str]) -> Chooser {
    Chooser::new(accounts(names))
}

/// An account tile is tall enough for a two-word display name with a line to
/// spare: "System Administrator" wraps rather than being cut short, and a face
/// wider than the reference one can push a long word onto a third line instead
/// of breaking it mid-word. Pinned at the reference density and at a doubled
/// one, because the band and the line height scale by different roundings.
#[test]
fn an_account_tile_holds_a_two_word_name_at_every_density() {
    let theme = theme();
    for percent in [100, 200] {
        let scale = Scale::from_percent(percent).expect("a supported density");
        let tile = grid(&["ann"]).tile_rect(0, SCREEN, scale).expect("a tile");
        assert!(
            IconTile::label_lines(tile, scale, &theme) >= 3,
            "at {percent}% a tile holds {} label lines",
            IconTile::label_lines(tile, scale, &theme)
        );
    }
}

fn shift() -> Modifiers {
    Modifiers {
        shift: true,
        ..Modifiers::default()
    }
}

/// Press `event` `times` on a fresh chooser over `names`, then Return, and
/// report what the surface then asks about: an account's login name, or
/// [`OTHER_LABEL`] when the trailing tile was the one picked.
fn pick_after(names: &[&str], event: &InputEvent, times: usize) -> String {
    let mut surface = AuthSurface::with_accounts(accounts(names));
    let mut verifier = Scripted::refusing();
    for _ in 0..times {
        feed(&mut surface, event, &mut verifier);
    }
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    surface
        .selected_account()
        .map_or_else(|| String::from(OTHER_LABEL), String::from)
}

#[test]
fn a_tile_shows_the_display_name_and_names_the_account_to_log_in_as() {
    let tile = AccountTile::new("Ann Example", "ann");

    assert_eq!(tile.display_name(), "Ann Example");
    assert_eq!(tile.login_name(), "ann");
    assert!(!tile.has_live_session());
    assert_eq!(tile.monogram(), 'A');
}

/// A display name the authority could not give is not a reason to draw a
/// blank tile: the login name stands in for it.
#[test]
fn a_tile_with_no_display_name_falls_back_to_the_login_name() {
    let tile = AccountTile::new("", "ann");

    assert_eq!(tile.display_name(), "ann");
    assert_eq!(tile.monogram(), 'A');
}

#[test]
fn a_monogram_is_the_first_character_uppercased() {
    assert_eq!(AccountTile::new("ann", "ann").monogram(), 'A');
    assert_eq!(AccountTile::new("Édith", "edith").monogram(), 'É');
    assert_eq!(AccountTile::new("", "").monogram(), FALLBACK_MONOGRAM);
}

#[test]
fn a_live_session_is_carried_on_the_tile() {
    let tile = AccountTile::new("Ann", "ann").with_live_session(true);

    assert!(tile.has_live_session());
    assert!(!tile.with_live_session(false).has_live_session());
}

/// The `Other…` tile is the last slot, whatever the account list holds.
#[test]
fn the_other_tile_is_always_present_and_always_last() {
    for names in [
        &[][..],
        &["ann"][..],
        &["ann", "bob", "cai", "dee", "eve"][..],
    ] {
        let chooser = grid(names);

        assert_eq!(chooser.slots(), names.len() + 1);
        assert!(chooser.account(names.len()).is_none(), "{names:?}");
        for slot in 0..names.len() {
            assert!(chooser.account(slot).is_some(), "{names:?} slot {slot}");
        }
    }
}

/// With no accounts at all the chooser is not empty: `Other…` is still there
/// and still reachable, so a machine whose authority listed nobody can still
/// be logged in to.
#[test]
fn with_no_accounts_the_chooser_still_offers_other() {
    let mut surface = AuthSurface::with_accounts(Vec::new());
    let mut verifier = Scripted::refusing();

    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

    assert_eq!(surface.notice(), NAME_HINT);
    assert_eq!(surface.selected_account(), None);
    assert_eq!(OTHER_LABEL, "Other…");
}

/// The focus model itself: on and back, wrapping at both ends so a
/// keyboard-only user never falls off the row.
#[test]
fn the_focus_wraps_at_both_ends() {
    let mut chooser = grid(&["ann", "bob"]);
    assert_eq!(chooser.focus(), 0);

    let mut forward = Vec::new();
    for _ in 0..4 {
        chooser.move_focus(Step::Next, 0, 0);
        forward.push(chooser.focus());
    }
    assert_eq!(forward, vec![1, 2, 0, 1]);

    let mut chooser = grid(&["ann", "bob"]);
    let mut backward = Vec::new();
    for _ in 0..4 {
        chooser.move_focus(Step::Previous, 0, 0);
        backward.push(chooser.focus());
    }
    assert_eq!(backward, vec![2, 1, 0, 2]);
}

/// A chooser with only the `Other…` tile has nowhere to move to, and says so
/// rather than reporting a repaint nobody needs.
#[test]
fn a_single_tile_chooser_never_moves() {
    let mut chooser = grid(&[]);

    assert!(!chooser.move_focus(Step::Next, 0, 0));
    assert!(!chooser.move_focus(Step::Previous, 0, 0));
    assert_eq!(chooser.focus(), 0);
}

#[test]
fn tab_and_the_forward_arrow_keys_move_the_focus_on_and_wrap_at_the_end() {
    let names = ["ann", "bob"];
    for event in [
        named(NamedKey::Tab),
        named(NamedKey::Right),
        named(NamedKey::Down),
    ] {
        assert_eq!(pick_after(&names, &event, 0), "ann", "{event:?}");
        assert_eq!(pick_after(&names, &event, 1), "bob", "{event:?}");
        assert_eq!(pick_after(&names, &event, 2), OTHER_LABEL, "{event:?}");
        assert_eq!(pick_after(&names, &event, 3), "ann", "{event:?}");
    }
}

#[test]
fn shift_tab_and_the_back_arrow_keys_move_the_focus_back_and_wrap_at_the_start() {
    let names = ["ann", "bob"];
    for event in [
        key_with(Key::Named(NamedKey::Tab), shift()),
        named(NamedKey::Left),
        named(NamedKey::Up),
    ] {
        assert_eq!(pick_after(&names, &event, 1), OTHER_LABEL, "{event:?}");
        assert_eq!(pick_after(&names, &event, 2), "bob", "{event:?}");
        assert_eq!(pick_after(&names, &event, 3), "ann", "{event:?}");
        assert_eq!(pick_after(&names, &event, 4), OTHER_LABEL, "{event:?}");
    }
}

/// The whole chooser is reachable from the keyboard alone, because a machine
/// without a pointer must still be able to log in.
#[test]
fn a_tile_is_picked_with_the_keyboard_alone() {
    let mut surface = AuthSurface::with_accounts(accounts(&["ann", "bob"]));
    let mut verifier = Scripted::refusing();

    feed(&mut surface, &named(NamedKey::Tab), &mut verifier);
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

    assert_eq!(surface.selected_account(), Some("bob"));
    assert_eq!(surface.notice(), HINT);
}

/// The pointer picks the tile it is released over, tested against the very
/// rectangles the tiles are drawn into.
#[test]
fn the_pointer_picks_the_tile_it_is_released_over() {
    let mut surface = AuthSurface::with_accounts(accounts(&["ann", "bob"]));
    let mut verifier = Scripted::refusing();
    let bob = grid(&["ann", "bob"])
        .tile_rect(1, SCREEN, Scale::ONE)
        .expect("a second tile");

    feed(&mut surface, &centre(bob), &mut verifier);
    feed(&mut surface, &PRESS, &mut verifier);
    feed(&mut surface, &RELEASE, &mut verifier);

    assert_eq!(surface.selected_account(), Some("bob"));
}

/// A press that slides off its tile before it is let go picks nothing: the
/// pointer's answer is where it *finished*, exactly as every other control
/// reads a click.
#[test]
fn a_press_that_leaves_its_tile_picks_nothing() {
    let mut surface = AuthSurface::with_accounts(accounts(&["ann", "bob"]));
    let mut verifier = Scripted::refusing();
    let chooser = grid(&["ann", "bob"]);
    let ann = chooser.tile_rect(0, SCREEN, Scale::ONE).expect("a tile");
    let bob = chooser
        .tile_rect(1, SCREEN, Scale::ONE)
        .expect("a second tile");

    feed(&mut surface, &centre(ann), &mut verifier);
    feed(&mut surface, &PRESS, &mut verifier);
    feed(&mut surface, &centre(bob), &mut verifier);
    feed(&mut surface, &RELEASE, &mut verifier);

    assert_eq!(surface.selected_account(), None);
}

/// Picking `Other…` leads to a typed login name, and that name is what the
/// authority is then asked about.
#[test]
fn the_other_tile_leads_to_a_typed_login_name() {
    let mut surface = AuthSurface::with_accounts(accounts(&["ann"]));
    let mut verifier = Scripted::refusing();

    feed(&mut surface, &named(NamedKey::Tab), &mut verifier);
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    assert_eq!(surface.notice(), NAME_HINT);
    assert_eq!(surface.selected_account(), None);

    for ch in "zoe".chars() {
        feed(&mut surface, &key(Key::Char(ch)), &mut verifier);
    }
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

    assert_eq!(surface.selected_account(), Some("zoe"));
    submit(&mut surface, "secret", &mut verifier);
    assert_eq!(verifier.accounts, vec![String::from("zoe")]);
}

/// A name is what the next question is asked about, so an empty one is not
/// an answer and the surface keeps asking.
#[test]
fn an_empty_login_name_does_not_move_on() {
    let mut surface = AuthSurface::with_accounts(Vec::new());
    let mut verifier = Scripted::new(vec![Verdict::Verified]);

    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

    assert_eq!(surface.notice(), NAME_REQUIRED);
    assert_eq!(surface.selected_account(), None);
    assert!(verifier.offered.is_empty(), "nothing was ever offered");
}

/// Escape steps back to the chooser and takes the half-typed secret with it,
/// so the next account's prompt cannot inherit it.
#[test]
fn escape_returns_to_the_chooser_and_wipes_the_secret() {
    let mut surface = AuthSurface::with_accounts(accounts(&["ann", "bob"]));
    let mut verifier = Scripted::refusing();

    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    for ch in "half-typed".chars() {
        feed(&mut surface, &key(Key::Char(ch)), &mut verifier);
    }
    feed(&mut surface, &named(NamedKey::Escape), &mut verifier);

    assert_eq!(surface.selected_account(), None);
    assert_eq!(surface.notice(), CHOOSE_HINT);

    // Pick the same account again and submit without typing: what comes back
    // is empty, so nothing survived the step back.
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    assert_eq!(verifier.offered, vec![String::new()]);
}

/// The same holds for the typed login name: stepping back from `Other…`
/// leaves nothing behind either.
#[test]
fn escape_from_the_name_field_wipes_what_was_typed() {
    let mut surface = AuthSurface::with_accounts(Vec::new());
    let mut verifier = Scripted::refusing();

    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    for ch in "zoe".chars() {
        feed(&mut surface, &key(Key::Char(ch)), &mut verifier);
    }
    feed(&mut surface, &named(NamedKey::Escape), &mut verifier);
    assert_eq!(surface.notice(), CHOOSE_HINT);

    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);
    feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

    assert_eq!(surface.notice(), NAME_REQUIRED);
    assert_eq!(surface.selected_account(), None);
}

/// Nothing on the chooser lets anybody in: it names an account, and the
/// secret is still asked for afterwards.
#[test]
fn no_chooser_event_concludes_the_surface() {
    let mut surface = AuthSurface::with_accounts(accounts(&["ann"]));
    let mut verifier = Scripted::new(vec![Verdict::Verified]);
    let ann = grid(&["ann"])
        .tile_rect(0, SCREEN, Scale::ONE)
        .expect("a tile");

    for event in [
        named(NamedKey::Tab),
        named(NamedKey::Escape),
        named(NamedKey::Enter),
        centre(ann),
        PRESS,
        RELEASE,
    ] {
        assert!(
            !feed(&mut surface, &event, &mut verifier).verified(),
            "{event:?} concluded the surface from the chooser"
        );
    }
}

/// A live session is visible on the tile, so the switch-user affordance can
/// be seen rather than guessed at.
#[test]
fn a_live_session_badges_its_tile() {
    let quiet = AuthSurface::with_accounts(vec![AccountTile::new("Ann", "ann")]);
    let live =
        AuthSurface::with_accounts(vec![AccountTile::new("Ann", "ann").with_live_session(true)]);

    assert_ne!(render(&quiet), render(&live));
}

/// Every tile lands inside the grid the damage report names, so a focus move
/// that repaints the chooser rectangle repaints all of it.
#[test]
fn every_tile_sits_inside_the_chooser_bounds() {
    let chooser = grid(&["ann", "bob", "cai", "dee", "eve"]);
    let bounds = chooser.bounds(SCREEN, Scale::ONE);

    for slot in 0..chooser.slots() {
        let tile = chooser.tile_rect(slot, SCREEN, Scale::ONE).expect("a tile");
        assert!(tile.origin.x >= bounds.origin.x, "slot {slot}");
        assert!(tile.origin.y >= bounds.origin.y, "slot {slot}");
        assert!(tile.right() <= bounds.right(), "slot {slot}");
        assert!(tile.bottom() <= bounds.bottom(), "slot {slot}");
    }
}

/// The rectangle a tile is painted into is the rectangle it is hit-tested
/// against: a point inside one resolves to that slot and no other.
#[test]
fn the_painted_tile_is_the_rectangle_the_pointer_is_tested_against() {
    let chooser = grid(&["ann", "bob", "cai"]);

    for slot in 0..chooser.slots() {
        let tile = chooser.tile_rect(slot, SCREEN, Scale::ONE).expect("a tile");
        let middle = Point::new(
            tile.origin.x + i32::try_from(tile.width / 2).expect("a small tile"),
            tile.origin.y + i32::try_from(tile.height / 2).expect("a small tile"),
        );
        assert_eq!(chooser.hit(middle, SCREEN, Scale::ONE), Some(slot));
    }
    assert_eq!(chooser.hit(Point::new(1, 1), SCREEN, Scale::ONE), None);
}

/// The whole round trip on a grid that has wrapped into two rows: every
/// slot's rectangle has something drawn in it, and a press released there
/// picks the very account drawn in it. Paint and hit test are asserted
/// against the one geometry both read.
#[test]
fn a_press_over_any_tile_picks_the_account_drawn_in_it() {
    let names = ["ann", "bob", "cai", "dee", "eve", "fay", "gus"];
    let chooser = grid(&names);
    let frame = render(&AuthSurface::with_accounts(accounts(&names)));

    for slot in 0..chooser.slots() {
        let tile = chooser.tile_rect(slot, SCREEN, Scale::ONE).expect("a tile");
        assert!(painted(&frame, tile), "slot {slot} drew nothing");

        let mut surface = AuthSurface::with_accounts(accounts(&names));
        let mut verifier = Scripted::refusing();
        feed(&mut surface, &centre(tile), &mut verifier);
        feed(&mut surface, &PRESS, &mut verifier);
        feed(&mut surface, &RELEASE, &mut verifier);

        assert_eq!(surface.selected_account(), names.get(slot).copied());
    }
}

/// The accounts stay one centred row for as long as the screen holds them
/// and wrap only when it cannot: a grid where a row would do reads as a list
/// rather than as a choice.
#[test]
fn the_tiles_stay_one_row_until_the_screen_cannot_hold_them() {
    let chooser = grid(&["ann", "bob", "cai", "dee", "eve", "fay", "gus"]);
    let last = chooser.slots() - 1;
    let wide = Rect::new(0, 0, 2000, 900);
    let narrow = Rect::new(0, 0, 700, 900);
    let top_of = |slot: usize, screen: Rect| {
        chooser
            .tile_rect(slot, screen, Scale::ONE)
            .expect("a tile")
            .origin
            .y
    };

    assert_eq!(top_of(0, wide), top_of(last, wide), "a wide screen wrapped");
    assert!(
        top_of(last, narrow) > top_of(0, narrow),
        "a narrow screen did not wrap"
    );
}

/// A screen with no pixels is answered, not faulted on: the chooser hits
/// nothing, moves nothing, and produces no frame.
#[test]
fn a_screen_with_no_pixels_leaves_the_chooser_standing() {
    let mut surface = AuthSurface::with_accounts(accounts(&["ann", "bob"]));
    let mut verifier = Scripted::refusing();
    let nothing = Rect::new(0, 0, 0, 0);
    let bare = theme();

    for event in [named(NamedKey::Tab), centre(nothing), PRESS, RELEASE] {
        let outcome = surface.on_event(
            &event,
            &mut EventContext {
                screen: nothing,
                scale: Scale::ONE,
                theme: &bare,
                verifier: &mut verifier,
                now_ns: 0,
            },
        );
        assert!(!outcome.verified(), "{event:?} concluded a blank screen");
    }
    assert!(surface
        .render(nothing, Scale::ONE, &bare, Backdrop::Desktop)
        .is_none());
}

/// A frame is produced at every density, and the chooser draws something
/// rather than an empty backdrop.
#[test]
fn the_chooser_draws_at_every_supported_density() {
    let surface = AuthSurface::with_accounts(accounts(&["ann", "bob"]));
    let bare = AuthSurface::with_accounts(Vec::new());

    for percent in [Scale::MIN_PERCENT, 100, 200, Scale::MAX_PERCENT] {
        let scale = Scale::from_percent(percent).expect("a supported density");
        let frame = surface
            .render(SCREEN, scale, &theme(), Backdrop::Desktop)
            .expect("a frame at every density");
        let empty = bare
            .render(SCREEN, scale, &theme(), Backdrop::Desktop)
            .expect("a frame at every density");

        assert_eq!(frame.width(), SCREEN.width);
        assert_ne!(frame, empty, "at {percent}%");
    }
}

/// A fresh chooser is settled: slot 0 fully marked, nothing animating.
#[test]
fn a_fresh_chooser_is_settled() {
    let chooser = grid(&["ann", "bob"]);
    assert_eq!(chooser.focus(), 0);
    assert_eq!(chooser.selection_fade(0), u8::MAX);
    assert_eq!(chooser.selection_fade(1), 0);
    assert_eq!(chooser.next_frame_in(0), None);
}

/// Moving the focus starts a cross-fade: leaving full, arriving empty at the
/// start; complementary mid-way; settled full on the new slot at the end.
#[test]
fn moving_focus_cross_fades_the_selection_mark() {
    let mut chooser = grid(&["ann", "bob", "cy"]);
    let millis = theme()
        .motion()
        .duration(tairix_theme::MotionInteraction::SelectionChange);
    assert!(millis > 0, "built-in themes animate selection");
    let span_ns = u64::from(millis) * 1_000_000;

    assert!(chooser.focus_on(1, 1_000, millis));
    assert!(chooser.next_frame_in(1_000).is_some());
    assert_eq!(chooser.selection_fade(0), u8::MAX);
    assert_eq!(chooser.selection_fade(1), 0);
    // The one frame cadence every animation shares, not a step this fade
    // works out for itself.
    assert_eq!(
        chooser.next_frame_in(1_000),
        Some(tairix_theme::Timeline::FRAME_NS)
    );

    let mid = 1_000 + span_ns / 2;
    assert!(chooser.advance(mid));
    let arriving = chooser.selection_fade(1);
    let leaving = chooser.selection_fade(0);
    assert!(
        arriving > 0 && arriving < u8::MAX,
        "mid-fade arriving={arriving}"
    );
    assert_eq!(
        u16::from(arriving) + u16::from(leaving),
        u16::from(u8::MAX),
        "strengths are complementary"
    );
    assert_eq!(chooser.selection_fade(2), 0);

    assert!(chooser.advance(1_000 + span_ns));
    assert_eq!(chooser.next_frame_in(0), None);
    assert_eq!(chooser.selection_fade(1), u8::MAX);
    assert_eq!(chooser.selection_fade(0), 0);
    assert_eq!(chooser.next_frame_in(1_000 + span_ns), None);
}

/// A zero duration (reduced motion) settles instantly and never asks for a frame.
#[test]
fn a_zero_duration_settles_instantly() {
    let mut chooser = grid(&["ann", "bob"]);
    assert!(chooser.focus_on(1, 50, 0));
    assert_eq!(chooser.next_frame_in(0), None);
    assert_eq!(chooser.selection_fade(1), u8::MAX);
    assert_eq!(chooser.selection_fade(0), 0);
    assert_eq!(chooser.next_frame_in(50), None);
    assert!(!chooser.advance(100));
}

/// A second focus change while one is running does not strand a mark.
#[test]
fn a_second_focus_change_does_not_strand_a_mark() {
    let mut chooser = grid(&["ann", "bob", "cy"]);
    let millis = 100u16;
    let span_ns = u64::from(millis) * 1_000_000;

    assert!(chooser.focus_on(1, 0, millis));
    assert!(chooser.advance(span_ns / 2));
    assert!(chooser.selection_fade(0) > 0);
    assert!(chooser.selection_fade(1) > 0);

    // Jump to a third tile mid-fade.
    assert!(chooser.focus_on(2, span_ns / 2, millis));
    assert_eq!(chooser.selection_fade(0), 0, "old leaving is gone");
    assert_eq!(
        chooser.selection_fade(1),
        u8::MAX,
        "previous arriving is leaving at full"
    );
    assert_eq!(chooser.selection_fade(2), 0);

    assert!(chooser.advance(span_ns / 2 + span_ns));
    assert_eq!(chooser.next_frame_in(0), None);
    assert_eq!(chooser.selection_fade(2), u8::MAX);
    assert_eq!(chooser.selection_fade(0), 0);
    assert_eq!(chooser.selection_fade(1), 0);
}

/// A clock that jumps backwards settles rather than hanging or panicking.
#[test]
fn a_backwards_clock_settles_the_fade() {
    let mut chooser = grid(&["ann", "bob"]);
    assert!(chooser.focus_on(1, 1_000, 100));
    assert!(chooser.advance(500));
    assert_eq!(chooser.next_frame_in(0), None);
    assert_eq!(chooser.selection_fade(1), u8::MAX);
    assert_eq!(chooser.selection_fade(0), 0);
}

/// The settled render is pixel-identical to a chooser that never animated,
/// and two mid-fade strengths paint different pixels.
#[test]
fn the_fade_only_affects_the_transition_pixels() {
    use crate::testkit::changed_pixels;
    use tairix_raster::{Color, Surface};

    let theme = theme();
    let names = ["ann", "bob"];

    // Direct settle on slot 1 with no animation.
    let mut direct = Chooser::new(accounts(&names));
    direct.focus_on(1, 0, 0);
    let mut direct_frame = Surface::new(SCREEN.width, SCREEN.height).expect("surface");
    direct_frame.fill(Color::from(theme.palette().desktop));
    direct.render(&mut direct_frame, SCREEN, Scale::ONE, &theme, u8::MAX);

    // Animate to the same slot and settle.
    let mut faded = Chooser::new(accounts(&names));
    let millis = theme
        .motion()
        .duration(tairix_theme::MotionInteraction::SelectionChange);
    let span_ns = u64::from(millis) * 1_000_000;
    faded.focus_on(1, 0, millis);

    let mut start_frame = Surface::new(SCREEN.width, SCREEN.height).expect("surface");
    start_frame.fill(Color::from(theme.palette().desktop));
    faded.render(&mut start_frame, SCREEN, Scale::ONE, &theme, u8::MAX);

    faded.advance(span_ns / 2);
    let mut mid_frame = Surface::new(SCREEN.width, SCREEN.height).expect("surface");
    mid_frame.fill(Color::from(theme.palette().desktop));
    faded.render(&mut mid_frame, SCREEN, Scale::ONE, &theme, u8::MAX);

    assert!(
        !changed_pixels(&start_frame, &mid_frame).is_empty(),
        "mid-fade pixels differ from the start of the fade"
    );

    faded.advance(span_ns);
    let mut end_frame = Surface::new(SCREEN.width, SCREEN.height).expect("surface");
    end_frame.fill(Color::from(theme.palette().desktop));
    faded.render(&mut end_frame, SCREEN, Scale::ONE, &theme, u8::MAX);

    assert_eq!(
        end_frame, direct_frame,
        "a settled fade matches a chooser that selected without animating"
    );
}
