//! Unit tests for the surface's three animations: the chooser and the prompt
//! trading places, the shake that answers a rejected attempt, and the veil
//! the screen arrives from and leaves through.
//!
//! Each is asserted at both ends as well as in the middle, because the whole
//! point of an animation here is that it changes nothing about the screens it
//! joins: a finished travel must be the settled stage pixel for pixel.

use alloc::vec;
use alloc::vec::Vec;

use tairix_geometry::{Rect, Scale};
use tairix_input::{Key, NamedKey};
use tairix_raster::{div255, Color, Surface};
use tairix_theme::{MotionInteraction, Timeline};

use crate::chooser::{AccountTile, Chooser};
use crate::layout::Prompt;
use crate::motion::{between_rects, Shake, Stage, Toward, Veil};
use crate::surface::{AuthSurface, Verdict, CHOOSE_HINT, REFUSED};
use crate::testkit::{
    changed_pixels, contrast_in, feed_at, feed_in, key, named, painted, render, render_in, still,
    theme, Scripted, SCREEN,
};

/// Nanoseconds in one millisecond.
const MS: u64 = 1_000_000;

/// A monotonic instant a person could plausibly log in at.
///
/// Not zero: turning a travel round back-dates the return's start, which a
/// clock sitting on zero cannot represent.
const BOOT: u64 = 5_000 * MS;

fn accounts() -> Vec<AccountTile> {
    vec![
        AccountTile::new("Ann", "ann"),
        AccountTile::new("Bob", "bob"),
    ]
}

/// A chooser over the same accounts the surface was given, for asking where
/// its tiles and their discs are.
fn grid() -> Chooser {
    Chooser::new(accounts())
}

/// How long `interaction` runs in the shipped theme, in nanoseconds.
fn span_of(interaction: MotionInteraction) -> u64 {
    u64::from(theme().motion().duration(interaction)) * MS
}

/// The same duration for the veil in the milliseconds it is started from.
fn veil_ms() -> u16 {
    theme().motion().duration(MotionInteraction::SessionFade)
}

/// A surface on the chooser that has been painted once, so it knows where it
/// is and reports rectangles rather than the whole screen.
fn placed() -> AuthSurface {
    let surface = AuthSurface::with_accounts(accounts());
    let _ = render(&surface);
    surface
}

/// The prompt as a theme that animates nothing draws it: the settled stage a
/// completed travel has to land on, pixel for pixel.
fn cut_to_prompt() -> AuthSurface {
    let mut cut = placed();
    feed_in(
        &mut cut,
        &named(NamedKey::Enter),
        &mut Scripted::refusing(),
        0,
        &still(),
    );
    cut
}

#[test]
fn a_zero_duration_starts_nothing() {
    assert!(Stage::start(0, Toward::Prompt, 0, 0).is_none());
    assert!(Shake::start(0, 0).is_none());
}

/// An untouched login screen asks for no wake at all, before and after every
/// animation this crate can run.
#[test]
fn an_idle_surface_asks_for_no_frame() {
    let chooser = placed();
    assert_eq!(chooser.motion_due(0), None);
    assert_eq!(chooser.motion_due(u64::MAX), None);

    let mut lock = AuthSurface::new("ann");
    assert_eq!(lock.motion_due(0), None);
    assert!(!lock.advance(1_000).redraw());
}

/// The desktop's lock composes a surface that is only ever the question — no
/// chooser, no travel — so the motion seam must fold what is running there
/// too. Stepping only the chooser would leave the lock's refusal and its
/// departure frozen with nothing failing to say so.
#[test]
fn a_chooserless_question_shakes_and_fades() {
    let theme = theme();
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Refused]);
    let _ = render(&surface);

    feed_at(&mut surface, &key(Key::Char('x')), &mut verifier, BOOT);
    feed_at(&mut surface, &named(NamedKey::Enter), &mut verifier, BOOT);
    let shake = span_of(MotionInteraction::AttemptRejected);
    assert_eq!(surface.motion_due(BOOT), Some(Timeline::FRAME_NS));
    assert!(surface.advance(BOOT + shake / 4).redraw(), "it shakes");
    surface.advance(BOOT + shake);
    assert_eq!(surface.motion_due(BOOT + shake), None, "and comes to rest");

    let fade = span_of(MotionInteraction::SessionFade);
    surface.begin_session_fade(BOOT, &theme);
    assert_eq!(surface.motion_due(BOOT), Some(Timeline::FRAME_NS));
    assert!(surface.advance(BOOT + fade / 4).redraw(), "it darkens");
    surface.advance(BOOT + fade);
    assert!(surface.session_fade_finished(), "and reaches black");
}

/// Picking an account starts the travel; part-way through, the disc is
/// between the tile it left and the prompt's place, and both stages have ink
/// on screen.
#[test]
fn picking_an_account_travels_the_disc_between_the_two_stages() {
    let span = span_of(MotionInteraction::StageTransition);
    assert!(span > 0, "the shipped theme animates a stage transition");
    let theme = theme();
    let mut surface = placed();
    let mut verifier = Scripted::refusing();

    let chooser = render(&surface);
    feed_at(&mut surface, &named(NamedKey::Enter), &mut verifier, 0);
    assert!(
        surface.motion_due(0).is_some(),
        "a started transition asks for frames"
    );

    surface.advance(span / 2);
    let mid = render(&surface);

    let from = grid()
        .tile_disc_rect(0, SCREEN, Scale::ONE, &theme)
        .expect("the picked tile has a disc");
    let to = Prompt::new(SCREEN, Scale::ONE).disc;
    let travelling = between_rects(from, to, u8::MAX / 2);
    assert!(
        between(travelling.origin.x, from.origin.x, to.origin.x)
            && between(travelling.origin.y, from.origin.y, to.origin.y),
        "the disc is at neither end: {travelling:?} between {from:?} and {to:?}"
    );
    assert!(travelling.width > from.width && travelling.width < to.width);

    assert!(
        painted(&mid, travelling),
        "the disc is drawn on its way between the two"
    );
    assert!(
        contrast_in(&mid, travelling) > contrast_in(&chooser, travelling),
        "the travelling disc is ink the settled chooser does not have there"
    );
    let other_tile = grid()
        .tile_rect(1, SCREEN, Scale::ONE)
        .expect("a second tile");
    assert!(painted(&mid, other_tile), "the chooser is still on screen");
    assert!(
        painted(&mid, surface.field_rect(SCREEN, Scale::ONE, &theme)),
        "the prompt's pill is already arriving"
    );
}

/// The travel ends on the settled prompt exactly: a completed transition is
/// indistinguishable from a theme that never animated one.
#[test]
fn a_completed_travel_is_the_settled_prompt() {
    let span = span_of(MotionInteraction::StageTransition);
    let mut verifier = Scripted::refusing();

    let mut animated = placed();
    feed_at(&mut animated, &named(NamedKey::Enter), &mut verifier, 0);
    animated.advance(span);
    assert_eq!(animated.motion_due(span), None, "nothing is left running");

    assert_eq!(render(&animated), render(&cut_to_prompt()));
}

/// The stall this guards: an embedder steps the travel, spends real time
/// presenting that frame, and only then asks when to wake. A span that ended
/// in between still owes the frame that lands on the prompt, or the transition
/// freezes part-way until something unrelated wakes the screen.
#[test]
fn a_travel_that_ended_while_its_frame_was_presented_still_asks_for_the_last() {
    let span = span_of(MotionInteraction::StageTransition);
    let mut verifier = Scripted::refusing();
    let mut surface = placed();

    feed_at(&mut surface, &named(NamedKey::Enter), &mut verifier, BOOT);
    let stepped = BOOT + span - MS;
    surface.advance(stepped);
    assert!(surface.motion_due(stepped).is_some(), "still travelling");
    assert_ne!(
        render(&surface),
        render(&cut_to_prompt()),
        "and not yet the prompt"
    );

    // Presenting that frame outlasted the millisecond the span had left.
    let asked = BOOT + span + MS;
    assert_eq!(surface.motion_due(asked), Some(0), "the last frame is owed");

    surface.advance(asked);
    assert_eq!(render(&surface), render(&cut_to_prompt()));
    assert_eq!(surface.motion_due(asked), None, "only then is it over");
}

/// And the return runs the same travel backwards, ending on the settled
/// chooser exactly. A one-way transition would be half a transition.
#[test]
fn the_return_ends_on_the_settled_chooser() {
    let span = span_of(MotionInteraction::StageTransition);
    let mut verifier = Scripted::refusing();
    let mut surface = placed();

    feed_at(&mut surface, &named(NamedKey::Enter), &mut verifier, 0);
    surface.advance(span);
    feed_at(&mut surface, &named(NamedKey::Escape), &mut verifier, span);
    assert!(
        surface.motion_due(span).is_some(),
        "stepping back animates too"
    );

    let midway = render(&surface);
    surface.advance(span + span / 2);
    assert_ne!(
        changed_pixels(&midway, &render(&surface)).len(),
        0,
        "the return moves"
    );

    surface.advance(span * 2);
    assert_eq!(surface.motion_due(span * 2), None);
    assert_eq!(surface.notice(), CHOOSE_HINT);
    assert_eq!(render(&surface), render(&placed()));
}

/// Reduced motion lands on the destination at once and arms no timer: the
/// theme's zero duration is the whole of the reduced-motion branch.
#[test]
fn reduced_motion_steps_straight_to_the_prompt() {
    let still = still();
    let mut verifier = Scripted::refusing();
    let mut surface = placed();

    feed_in(
        &mut surface,
        &named(NamedKey::Enter),
        &mut verifier,
        0,
        &still,
    );

    assert_eq!(surface.selected_account(), Some("ann"));
    assert_eq!(surface.motion_due(0), None);
    assert!(!surface.advance(0).redraw());
    assert_eq!(render_in(&surface, &still), render(&surface));
}

/// Turning back part-way through a travel picks the disc up where it is
/// rather than teleporting it to the far end and starting again.
#[test]
fn a_travel_interrupted_half_way_turns_round_where_it_is() {
    let span = span_of(MotionInteraction::StageTransition);
    let theme = theme();
    let mut verifier = Scripted::refusing();
    let mut surface = placed();

    feed_at(&mut surface, &named(NamedKey::Enter), &mut verifier, BOOT);
    surface.advance(BOOT + span / 2);
    let outgoing = render(&surface);

    feed_at(
        &mut surface,
        &named(NamedKey::Escape),
        &mut verifier,
        BOOT + span / 2,
    );
    let turned = render(&surface);

    let from = grid()
        .tile_disc_rect(0, SCREEN, Scale::ONE, &theme)
        .expect("a disc");
    let to = Prompt::new(SCREEN, Scale::ONE).disc;
    let half = between_rects(from, to, u8::MAX / 2);
    assert!(
        painted(&turned, half),
        "the disc is still where the outgoing travel left it"
    );
    // The travel is read a step at a time, so the turn can shift both stages
    // by one step of strength; what it may not do is jump, which is a change
    // of a different order.
    let turn = largest_gap(&outgoing, &turned);
    let jump = largest_gap(&render(&placed()), &turned);
    assert!(
        turn * 8 < jump,
        "turning round changed a pixel by {turn} where a jump changes one by {jump}"
    );

    surface.advance(BOOT + span * 2);
    assert_eq!(surface.motion_due(BOOT + span * 2), None);
    assert_eq!(render(&surface), render(&placed()));
}

/// A selection fade and a stage transition legitimately overlap when someone
/// arrows and confirms quickly. Both must settle, and neither may strand the
/// other's mark.
#[test]
fn an_overlapping_selection_fade_and_travel_both_settle() {
    let selection = span_of(MotionInteraction::SelectionChange);
    let stage = span_of(MotionInteraction::StageTransition);
    let still = still();
    let mut verifier = Scripted::refusing();

    let mut animated = placed();
    feed_at(&mut animated, &named(NamedKey::Tab), &mut verifier, 0);
    feed_at(&mut animated, &named(NamedKey::Enter), &mut verifier, MS);
    assert!(animated.motion_due(MS).is_some());

    let end = MS + selection.max(stage) + 1;
    animated.advance(end);
    assert_eq!(animated.motion_due(end), None, "nothing is left animating");
    feed_at(&mut animated, &named(NamedKey::Escape), &mut verifier, end);
    animated.advance(end + stage);
    assert_eq!(animated.motion_due(end + stage), None);

    let mut cut = placed();
    feed_in(&mut cut, &named(NamedKey::Tab), &mut verifier, 0, &still);
    feed_in(&mut cut, &named(NamedKey::Enter), &mut verifier, 0, &still);
    feed_in(&mut cut, &named(NamedKey::Escape), &mut verifier, 0, &still);

    assert_eq!(
        render(&animated),
        render(&cut),
        "the mark landed on the tile the keyboard is on"
    );
}

/// Turning a travel round re-enters it where the outgoing one had reached,
/// rather than at its own beginning or at the mirror of where it was.
#[test]
fn a_reversed_travel_re_enters_where_the_outgoing_one_reached() {
    let ms = theme()
        .motion()
        .duration(MotionInteraction::StageTransition);
    let span = u64::from(ms) * MS;

    for step in 0..=16u64 {
        let now = BOOT + span * step / 16;
        let mut outgoing = Stage::start(0, Toward::Prompt, BOOT, ms).expect("a travel");
        outgoing.advance(now);
        let reached = outgoing.prompt_strength();

        let Some(mut returning) = outgoing.reverse(now, ms) else {
            continue;
        };
        returning.advance(now);

        assert_eq!(returning.toward(), Toward::Chooser);
        assert!(
            returning.prompt_strength().abs_diff(reached) <= 1,
            "the return entered at {} rather than {reached}",
            returning.prompt_strength()
        );
    }
}

/// The oscillation itself: it goes both ways about zero, decays, and comes to
/// rest at exactly zero.
#[test]
fn the_shake_swings_both_ways_decays_and_rests_at_zero() {
    let span = span_of(MotionInteraction::AttemptRejected);
    assert!(span > 0, "the shipped theme animates a refusal");
    let mut shake = Shake::start(
        0,
        theme()
            .motion()
            .duration(MotionInteraction::AttemptRejected),
    )
    .expect("a running shake");
    let room = (1_000, 1_000);

    let steps = 64u64;
    let mut offsets = Vec::new();
    for step in 0..=steps {
        shake.advance(span * step / steps);
        offsets.push(shake.offset(Scale::ONE, room));
    }

    assert_eq!(offsets.first().copied(), Some(0), "it starts at rest");
    assert_eq!(offsets.last().copied(), Some(0), "and ends at exactly zero");
    assert!(shake.finished(span));
    assert!(offsets.iter().any(|offset| *offset > 0), "it swings out");
    assert!(offsets.iter().any(|offset| *offset < 0), "and back");

    let half = offsets.len() / 2;
    let peak = |window: &[i32]| window.iter().map(|offset| offset.abs()).max().unwrap_or(0);
    assert!(
        peak(&offsets[..half]) > peak(&offsets[half..]),
        "the swing decays: {:?} then {:?}",
        peak(&offsets[..half]),
        peak(&offsets[half..])
    );
}

/// A refusal shakes the question and reports its damage as the band the
/// question moves in — never the whole screen, and never less than every
/// position it takes.
#[test]
fn a_refusal_shakes_the_question_within_the_band_it_reports() {
    let span = span_of(MotionInteraction::AttemptRejected);
    let theme = theme();
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Refused]);
    let _ = render(&surface);

    feed_at(&mut surface, &key(Key::Char('x')), &mut verifier, 0);
    feed_at(&mut surface, &named(NamedKey::Enter), &mut verifier, 0);
    assert_eq!(surface.notice(), REFUSED);
    assert!(surface.motion_due(0).is_some(), "a refusal shakes");

    let resting = render(&surface);
    let mut union: Option<Rect> = None;
    let mut moved = false;
    let steps = 48u64;
    for step in 1..=steps {
        let now = span * step / steps;
        let outcome = surface.advance(now);
        let frame = render(&surface);
        assert!(
            painted(&frame, surface.field_rect(SCREEN, Scale::ONE, &theme)),
            "the pill is still drawn at step {step}"
        );
        let Some(damage) = outcome.damage() else {
            continue;
        };
        union = Some(union.map_or(damage, |acc: Rect| acc.union(&damage)));
        for pixel in changed_pixels(&resting, &frame) {
            moved = true;
            assert!(
                damage.contains(pixel),
                "the shake painted {pixel:?} outside the reported {damage:?}"
            );
        }
    }

    assert!(moved, "the shake displaced something");
    let band = union.expect("a shaking surface reports a band");
    let prompt = Prompt::new(SCREEN, Scale::ONE);
    let pill = surface.field_rect(SCREEN, Scale::ONE, &theme);
    assert_eq!(band.origin.y, prompt.disc.origin.y);
    assert_eq!(band.bottom(), pill.bottom());
    assert!(band.height < SCREEN.height, "it is not the whole screen");

    surface.advance(span);
    assert_eq!(surface.motion_due(span), None);
    assert_eq!(render(&surface), resting, "it comes to rest where it began");
}

/// Under reduced motion a refusal is reported by its notice alone: nothing
/// moves, and nothing asks for a frame.
#[test]
fn a_reduced_motion_refusal_says_so_without_moving() {
    let still = still();
    let mut shaken = AuthSurface::new("ann");
    let mut quiet = AuthSurface::new("ann");

    for (surface, theme) in [(&mut shaken, theme()), (&mut quiet, still)] {
        let mut verifier = Scripted::new(vec![Verdict::Refused]);
        feed_in(surface, &key(Key::Char('x')), &mut verifier, 0, &theme);
        feed_in(surface, &named(NamedKey::Enter), &mut verifier, 0, &theme);
    }

    assert_eq!(quiet.notice(), REFUSED);
    assert_eq!(quiet.motion_due(0), None, "reduced motion arms no timer");
    assert!(!quiet.advance(1_000).redraw());
    assert_eq!(
        render(&quiet),
        render(&shaken),
        "a refusal at rest draws what an unshaken one draws"
    );
}

/// The veil darkens the whole composed screen toward black, monotonically,
/// and reaches full black.
#[test]
fn the_veil_darkens_to_black() {
    let span = span_of(MotionInteraction::SessionFade);
    assert!(span > 0, "the shipped theme animates the session fade");
    let theme = theme();
    let mut surface = AuthSurface::new("ann");
    let clear = render(&surface);

    surface.begin_session_fade(0, &theme);
    assert!(!surface.session_fade_finished());

    let sample = brightest(&clear);
    let mut last = clear.get(sample.0, sample.1).expect("a pixel");
    for step in 1..=8u64 {
        surface.advance(span * step / 8);
        let frame = render(&surface);
        let pixel = frame.get(sample.0, sample.1).expect("a pixel");
        assert!(
            u32::from(pixel.r) + u32::from(pixel.g) + u32::from(pixel.b)
                <= u32::from(last.r) + u32::from(last.g) + u32::from(last.b),
            "the veil lightened at step {step}"
        );
        last = pixel;
    }

    assert!(surface.session_fade_finished());
    assert_eq!(surface.motion_due(span), None);
    let black = render(&surface);
    for y in 0..black.height() {
        for x in 0..black.width() {
            assert_eq!(black.get(x, y), Some(Color::rgb(0, 0, 0).premultiply()));
        }
    }
}

/// A pixel part-way through the fade is the composed pixel scaled toward
/// black by exactly the veil's own strength.
#[test]
fn a_mid_fade_pixel_is_the_composed_pixel_scaled_toward_black() {
    let span = span_of(MotionInteraction::SessionFade);
    let theme = theme();
    let mut surface = AuthSurface::new("ann");
    let clear = render(&surface);

    surface.begin_session_fade(0, &theme);
    let half = span / 2;
    surface.advance(half);
    let faded = render(&surface);

    // The veil takes the linear progress, and composites over what is
    // already there: every channel keeps `1 - strength` of itself.
    let strength = u8::try_from(half * u64::from(u8::MAX) / span).unwrap_or(u8::MAX);
    let keep = u8::MAX - strength;
    let (x, y) = brightest(&clear);
    let under = clear.get(x, y).expect("a pixel");
    let over = faded.get(x, y).expect("a pixel");

    assert!(strength > 0 && strength < u8::MAX, "part-way through");
    assert_eq!(over.r, div255(u32::from(under.r) * u32::from(keep)));
    assert_eq!(over.g, div255(u32::from(under.g) * u32::from(keep)));
    assert_eq!(over.b, div255(u32::from(under.b) * u32::from(keep)));
    assert_eq!(over.a, u8::MAX, "the screen stays opaque");
}

/// Once the screen has begun leaving, the decision is made: a keystroke may
/// not re-enter the prompt, and nothing it would have changed is drawn.
#[test]
fn input_during_the_fade_changes_nothing() {
    let theme = theme();
    let mut verifier = Scripted::refusing();
    let mut surface = placed();
    feed_in(
        &mut surface,
        &named(NamedKey::Enter),
        &mut verifier,
        0,
        &still(),
    );
    surface.begin_session_fade(0, &theme);
    let during = render(&surface);

    for event in [
        named(NamedKey::Escape),
        key(Key::Char('x')),
        named(NamedKey::Enter),
    ] {
        let outcome = feed_at(&mut surface, &event, &mut verifier, 0);
        assert!(!outcome.redraw(), "{event:?} asked for a repaint");
        assert!(!outcome.verified(), "{event:?} concluded the surface again");
    }

    assert_eq!(surface.selected_account(), Some("ann"));
    assert_eq!(render(&surface), during);
    assert!(verifier.offered.is_empty(), "nothing was offered again");
}

/// Beginning a fade already begun changes nothing, so an owner that asks
/// twice does not restart the screen's departure.
#[test]
fn a_second_begin_does_not_restart_the_fade() {
    let span = span_of(MotionInteraction::SessionFade);
    let theme = theme();
    let mut surface = AuthSurface::new("ann");
    surface.begin_session_fade(0, &theme);
    surface.advance(span / 2);
    let half = render(&surface);

    assert!(!surface.begin_session_fade(span / 2, &theme).redraw());
    assert_eq!(render(&surface), half);
}

/// A veil that has arrived is done with the clock: the owner holds it for as
/// long as it holds the screen, so a fully black screen must ask for no
/// further frame however long that is.
#[test]
fn a_veil_at_full_black_asks_for_no_further_frame() {
    let fade = span_of(MotionInteraction::SessionFade);
    let mut surface = AuthSurface::new("ann");

    surface.begin_session_fade(BOOT, &theme());
    surface.advance(BOOT + fade);

    assert!(surface.session_fade_finished());
    assert_eq!(surface.motion_due(BOOT + fade), None);
    assert_eq!(surface.motion_due(BOOT + fade * 2), None, "nor later");
    assert!(!surface.advance(BOOT + fade * 2).redraw());
}

/// A reduced-motion theme's fade is over the moment it begins, so an owner
/// leaves without presenting a frame for it.
#[test]
fn a_reduced_motion_fade_is_finished_at_once() {
    let mut surface = AuthSurface::new("ann");
    surface.begin_session_fade(0, &still());

    assert!(surface.session_fade_finished());
    assert_eq!(surface.motion_due(0), None);
}

/// The surface says it has begun leaving from the first veiled frame, which
/// is what an owner drawing a pointer over it reads to stop drawing one.
#[test]
fn a_surface_that_has_begun_leaving_says_so() {
    let span = span_of(MotionInteraction::SessionFade);
    let theme = theme();
    let mut surface = AuthSurface::new("ann");
    assert!(!surface.session_fade_begun(), "nothing has begun");

    surface.begin_session_fade(0, &theme);
    assert!(surface.session_fade_begun(), "from the first veiled frame");
    assert!(!surface.session_fade_finished());

    surface.advance(span);
    assert!(
        surface.session_fade_begun(),
        "and for as long as it is black"
    );

    // A theme with nothing to fade is still a screen that is leaving, even
    // though no frame is ever presented for it.
    let mut instant = AuthSurface::new("ann");
    instant.begin_session_fade(0, &still());
    assert!(instant.session_fade_begun());
}

/// The veil the screen arrives out of runs the other way: opaque on the frame
/// it begins on, and gone by the end of its span, so a screen appears out of
/// the black rather than snapping onto it.
#[test]
fn an_arriving_veil_starts_black_and_ends_fully_transparent() {
    let span = span_of(MotionInteraction::SessionFade);
    let mut veil = Veil::arriving(BOOT, veil_ms());
    assert_eq!(veil.strength(), u8::MAX, "the first frame is all of it");
    assert!(!veil.finished());
    assert!(!veil.is_leaving(), "it is the screen arriving");

    let mut last = veil.strength();
    for step in 1..=8u64 {
        veil.advance(BOOT + span * step / 8);
        assert!(veil.strength() <= last, "the veil darkened at step {step}");
        last = veil.strength();
    }

    assert_eq!(veil.strength(), 0, "and the last frame is none of it");
    assert!(veil.finished());
}

/// A screen asked to leave while it is still arriving goes on to black from
/// the strength it had reached: an accepted secret may not brighten the
/// screen before darkening it.
#[test]
fn a_leaving_veil_begins_at_the_strength_the_arriving_one_reached() {
    let span = span_of(MotionInteraction::SessionFade);
    let mut arriving = Veil::arriving(BOOT, veil_ms());
    arriving.advance(BOOT + span / 2);
    let reached = arriving.strength();
    assert!(reached > 0 && reached < u8::MAX, "part of the way out");

    let turned = BOOT + span / 2;
    let mut leaving = Veil::leaving(reached, turned, veil_ms());
    assert_eq!(leaving.strength(), reached, "it picks up where it was");
    assert!(leaving.is_leaving());

    leaving.advance(turned + span);
    assert_eq!(leaving.strength(), u8::MAX, "and still ends on black");
    assert!(leaving.finished());
}

/// A veil that has reached the strength it runs to is done with the clock
/// whichever way it was going: a screen that has arrived and one that has
/// gone both ask for no further frame.
#[test]
fn a_finished_veil_of_either_direction_asks_for_no_further_frame() {
    let span = span_of(MotionInteraction::SessionFade);
    for mut veil in [
        Veil::arriving(BOOT, veil_ms()),
        Veil::leaving(0, BOOT, veil_ms()),
    ] {
        assert!(veil.next_frame_in(BOOT).is_some(), "it is running");
        veil.advance(BOOT + span);
        assert!(veil.finished());
        assert_eq!(veil.next_frame_in(BOOT + span), None);
        assert_eq!(veil.next_frame_in(BOOT + span * 2), None, "nor later");
        assert!(!veil.advance(BOOT + span * 2), "and nothing moves");
    }
}

/// The largest colour difference any one pixel shows between two frames,
/// summed over the three channels.
fn largest_gap(before: &Surface, after: &Surface) -> u32 {
    let mut largest = 0;
    for y in 0..before.height() {
        for x in 0..before.width() {
            let (Some(one), Some(other)) = (before.get(x, y), after.get(x, y)) else {
                continue;
            };
            let apart = u32::from(one.r.abs_diff(other.r))
                + u32::from(one.g.abs_diff(other.g))
                + u32::from(one.b.abs_diff(other.b));
            largest = largest.max(apart);
        }
    }
    largest
}

/// Whether `value` is strictly inside the span the two ends mark out,
/// whichever way round they are.
fn between(value: i32, one: i32, other: i32) -> bool {
    value > one.min(other) && value < one.max(other)
}

/// The brightest pixel of `frame`, which is the one a fade toward black
/// changes most.
fn brightest(frame: &Surface) -> (u32, u32) {
    let mut best = (0, 0);
    let mut strongest = 0;
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let Some(pixel) = frame.get(x, y) else {
                continue;
            };
            let sum = u32::from(pixel.r) + u32::from(pixel.g) + u32::from(pixel.b);
            if sum > strongest {
                strongest = sum;
                best = (x, y);
            }
        }
    }
    best
}
