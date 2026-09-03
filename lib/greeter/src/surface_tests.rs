//! Unit tests for the authentication surface's secret prompt.
//!
//! These cover the state machine (typing, submission, the erasure of the
//! secret on every verdict, the wordings), the modality property that only a
//! verified secret concludes the surface, which of the veil's two directions
//! stops the surface answering at all, the one geometry definition that paint
//! and hit test share, and the render's degraded paths. The veil's own
//! arithmetic has its file, as do the chooser, the cooldown and chrome
//! updates, the backdrop, and the damage report.

use alloc::string::String;
use alloc::vec;

use tairix_geometry::{Rect, Scale};
use tairix_input::{Key, NamedKey};
use tairix_theme::Theme;

use crate::chooser::AccountTile;
use crate::layout::{chrome_band, chrome_bands, notice_band, Prompt};
use crate::surface::{
    panel_rect, AuthSurface, Backdrop, Chrome, Verdict, HINT, MAX_PASSWORD, REFUSED,
    UNNAMED_ACCOUNT, UNREACHABLE,
};
use crate::testkit::{
    centre, changed_pixels, contrast_in, feed, key, moved, named, painted, render, render_in,
    separation, still, submit, theme, Scripted, PRESS, RELEASE, SCREEN,
};

/// A dressed clock block, so a test that cares where the chrome lands has
/// something for it to draw.
fn chrome() -> Chrome {
    Chrome {
        clock: "09:41".into(),
        date: "Friday 7 August".into(),
        host: "tairix".into(),
    }
}

#[test]
fn typing_builds_the_secret_and_enter_offers_it_once_exactly_as_typed() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::new(vec![Verdict::Verified]);

    let outcome = submit(&mut surface, "Hunter2!", &mut verifier);

    assert!(outcome.verified());
    assert_eq!(verifier.offered, vec![String::from("Hunter2!")]);
}

/// The account the surface is asking about reaches the authority with the
/// secret, so a verifier that checks a named account checks the right one.
#[test]
fn the_account_being_asked_for_reaches_the_verifier() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::refusing();

    submit(&mut surface, "x", &mut verifier);

    assert_eq!(verifier.accounts, vec![String::from("ann")]);
    assert_eq!(surface.selected_account(), Some("ann"));
}

/// A name the embedder could not resolve is headed by the placeholder, and
/// the authority is offered *nothing* — the placeholder is a line of display
/// text, and offering it as a login name would ask about an account called
/// "Locked". A name nobody has cannot be mistaken for one somebody does.
#[test]
fn an_unnamed_account_offers_no_login_name_to_the_authority() {
    let mut surface = AuthSurface::new("", "");
    let mut verifier = Scripted::refusing();

    submit(&mut surface, "x", &mut verifier);

    assert_eq!(verifier.accounts, vec![String::new()]);
    assert_ne!(
        verifier.accounts,
        vec![String::from(UNNAMED_ACCOUNT)],
        "the heading's placeholder is never offered as a credential"
    );
}

/// Editing keys reach the field, so the secret offered is the one on screen
/// at the moment Enter is pressed rather than everything ever typed.
#[test]
fn backspace_edits_the_secret_before_it_is_offered() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::refusing();

    for ch in "Hunterx".chars() {
        feed(&mut surface, &key(Key::Char(ch)), &mut verifier);
    }
    feed(&mut surface, &named(NamedKey::Backspace), &mut verifier);
    submit(&mut surface, "2", &mut verifier);

    assert_eq!(verifier.offered, vec![String::from("Hunter2")]);
}

/// Every key is worth a frame: one arrives at human typing rate, and can
/// move the caret without reporting an edit.
#[test]
fn every_key_asks_for_a_repaint() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::refusing();

    assert!(feed(&mut surface, &key(Key::Char('a')), &mut verifier).redraw());
    assert!(feed(&mut surface, &named(NamedKey::Home), &mut verifier).redraw());
}

/// The field is bounded so its buffer is reserved once and never grown; a
/// caller leaning on the keyboard is truncated, not reallocated.
#[test]
fn the_secret_is_bounded_at_the_documented_maximum() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::refusing();
    let long = "x".repeat(MAX_PASSWORD + 10);

    submit(&mut surface, &long, &mut verifier);

    assert_eq!(verifier.offered.len(), 1);
    assert_eq!(verifier.offered[0].chars().count(), MAX_PASSWORD);
}

/// The erasure is unconditional: a second submission with nothing typed
/// offers an empty secret, whatever the first attempt was answered with.
#[test]
fn the_secret_is_erased_after_every_verdict() {
    for verdict in [Verdict::Verified, Verdict::Refused, Verdict::Unreachable] {
        let mut surface = AuthSurface::new("ann", "ann");
        let mut verifier = Scripted::new(vec![verdict]);

        submit(&mut surface, "secret", &mut verifier);
        feed(&mut surface, &named(NamedKey::Enter), &mut verifier);

        assert_eq!(
            verifier.offered,
            vec![String::from("secret"), String::new()],
            "the secret survived a {verdict:?} verdict"
        );
    }
}

#[test]
fn the_resting_notice_is_the_hint() {
    assert_eq!(AuthSurface::new("ann", "ann").notice(), HINT);
}

/// "Wrong password" and "I could not ask" call for different reactions from
/// the person at the keyboard, so they never read the same.
#[test]
fn a_refusal_and_an_unreachable_authority_read_differently() {
    let mut refused = AuthSurface::new("ann", "ann");
    let mut unreachable = AuthSurface::new("ann", "ann");

    submit(
        &mut refused,
        "wrong",
        &mut Scripted::new(vec![Verdict::Refused]),
    );
    submit(
        &mut unreachable,
        "wrong",
        &mut Scripted::new(vec![Verdict::Unreachable]),
    );

    assert_eq!(refused.notice(), REFUSED);
    assert_eq!(unreachable.notice(), UNREACHABLE);
    assert_ne!(REFUSED, UNREACHABLE);
}

/// Typing asks the question again, so the verdict on the previous secret
/// stops standing over the new one.
#[test]
fn typing_again_clears_the_previous_verdict() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::new(vec![Verdict::Refused]);
    submit(&mut surface, "wrong", &mut verifier);
    assert_eq!(surface.notice(), REFUSED);

    feed(&mut surface, &key(Key::Char('r')), &mut verifier);

    assert_eq!(surface.notice(), HINT);
}

/// The modality property. Escape, Tab, Enter on an empty field, a printable
/// key, and every pointer event are all harmless against an authority that
/// refuses: none of them is a second way out of the surface. Escape
/// especially must not be mistaken for a cancel-and-dismiss.
#[test]
fn no_event_concludes_the_surface_without_a_verified_verdict() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::refusing();

    for event in [
        named(NamedKey::Escape),
        named(NamedKey::Enter),
        named(NamedKey::Tab),
        key(Key::Char('x')),
        centre(surface.field_rect(SCREEN, Scale::ONE, &theme())),
        PRESS,
        RELEASE,
    ] {
        assert!(
            !feed(&mut surface, &event, &mut verifier).verified(),
            "{event:?} concluded the surface without a verified secret"
        );
    }
}

/// A screen that is still arriving is still asking: somebody may pick an
/// account and start typing while it comes up. Only the screen *leaving*
/// stops answering, because by then the decision has been made.
#[test]
fn only_the_screen_leaving_stops_the_surface_answering_input() {
    let theme = theme();
    let mut arriving = AuthSurface::new("ann", "ann");
    let mut asked = Scripted::refusing();
    arriving.begin_entry_fade(0, &theme);

    submit(&mut arriving, "x", &mut asked);
    assert_eq!(
        asked.offered,
        vec![String::from("x")],
        "a surface that is still arriving is still asking"
    );

    let mut leaving = AuthSurface::new("ann", "ann");
    let mut unasked = Scripted::refusing();
    leaving.begin_session_fade(0, &theme);

    assert!(!submit(&mut leaving, "x", &mut unasked).redraw());
    assert!(unasked.offered.is_empty(), "nothing was offered");
}

/// A screen arriving and a screen leaving are opposite ends of the same
/// black, so an owner waiting for the screen to *go* must never be told a
/// screen that has just come up has gone.
#[test]
fn a_finished_entry_fade_is_not_a_finished_session_fade() {
    let mut surface = AuthSurface::new("ann", "ann");
    surface.begin_entry_fade(0, &theme());

    assert!(!surface.session_fade_begun(), "arriving is not leaving");
    assert!(!surface.session_fade_finished());

    surface.advance(u64::MAX);
    assert!(!surface.session_fade_begun());
    assert!(!surface.session_fade_finished(), "nor once it has arrived");
}

/// The veil the screen arrived out of is let go once it is transparent: a
/// screen-wide fill of nothing must not be painted, or repainted for, on
/// every frame after.
#[test]
fn an_arrived_screen_owes_no_further_frame_and_holds_no_veil() {
    let mut surface = AuthSurface::new("ann", "ann");
    surface.begin_entry_fade(0, &theme());
    assert!(surface.veiled(), "it comes up under the black");

    let arrived = surface.advance(u64::MAX);
    assert!(arrived.redraw(), "the frame it arrives on is drawn");
    assert_eq!(arrived.damage(), None, "and it is the whole screen");

    assert!(!surface.veiled(), "and the veil is gone with it");
    assert!(!surface.advance(u64::MAX).redraw(), "nothing after it");
    assert_eq!(surface.motion_due(u64::MAX), None);
    assert_eq!(
        render(&surface),
        render(&AuthSurface::new("ann", "ann")),
        "an arrived screen is the screen, with nothing over it"
    );
}

/// A secret accepted while the screen is still coming up takes it on to black
/// from where it is. Restarting the veil at nothing would flash the screen
/// bright and then fade it, which is two movements where there is one.
#[test]
fn leaving_part_way_through_the_entry_fade_does_not_brighten_the_screen() {
    let theme = theme();
    let mut surface = AuthSurface::new("ann", "ann");
    surface.begin_entry_fade(0, &theme);
    let frame = surface.motion_due(0).expect("an arriving screen asks");
    surface.advance(frame);
    let arriving = render(&surface);

    assert!(surface.begin_session_fade(frame, &theme).redraw());
    assert!(surface.session_fade_begun());
    assert_eq!(render(&surface), arriving, "the screen did not brighten");

    surface.advance(u64::MAX);
    assert!(surface.session_fade_finished(), "and it still ends black");
    assert!(surface.veiled(), "a screen that has gone stays black");
}

/// A reduced-motion theme has nothing to arrive out of: the screen is simply
/// there, with no veil drawn and no frame owed for one.
#[test]
fn a_reduced_motion_entry_fade_is_over_before_it_begins() {
    let mut surface = AuthSurface::new("ann", "ann");

    assert!(!surface.begin_entry_fade(0, &still()).redraw());

    assert!(!surface.veiled(), "there is nothing to uncover");
    assert_eq!(surface.motion_due(0), None, "nothing is armed");
    assert!(!surface.session_fade_begun());
    assert_eq!(render(&surface), render(&AuthSurface::new("ann", "ann")));
}

/// A screen lock has no chooser to step back to, so Escape is inert: it
/// leaves the surface where it is, still asking about the same account, with
/// what was typed still in the field.
#[test]
fn a_surface_with_no_chooser_ignores_escape() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::refusing();
    for ch in "half".chars() {
        feed(&mut surface, &key(Key::Char(ch)), &mut verifier);
    }

    let outcome = feed(&mut surface, &named(NamedKey::Escape), &mut verifier);

    assert!(!outcome.verified());
    assert_eq!(surface.selected_account(), Some("ann"));
    submit(&mut surface, "", &mut verifier);
    assert_eq!(verifier.offered, vec![String::from("half")]);
}

/// Only a submission asks the authority at all, so an authority that would
/// say yes cannot be reached by typing, hovering, or clicking.
#[test]
fn only_a_submission_reaches_the_authority() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::new(vec![Verdict::Verified, Verdict::Verified]);

    for event in [key(Key::Char('a')), moved(500, 300), PRESS, RELEASE] {
        assert!(!feed(&mut surface, &event, &mut verifier).verified());
    }

    assert!(verifier.offered.is_empty(), "nothing was ever offered");
    assert!(feed(&mut surface, &named(NamedKey::Enter), &mut verifier).verified());
}

/// The composition is one centred column: chrome, then the disc, the name,
/// and the prompt block, each below the last and none overlapping its
/// neighbour, at the reference density and at twice it alike.
#[test]
fn the_column_is_a_centred_stack_at_every_density() {
    let screen = Rect::new(0, 0, 1400, 1100);
    let surface = AuthSurface::new("ann", "ann");

    for percent in [100, 200] {
        let scale = Scale::from_percent(percent).expect("a supported density");
        let prompt = Prompt::new(screen, scale);
        let chrome = chrome_band(screen, scale);
        let block = panel_rect(screen, scale);
        let field = surface.field_rect(screen, scale, &theme());

        for part in [prompt.disc, block, field] {
            let left = part.origin.x - screen.origin.x;
            let right = screen.right() - part.right();
            assert!(
                (left - right).abs() <= 1,
                "{part:?} is off centre at {percent}%"
            );
        }
        assert!(
            chrome.bottom() <= prompt.disc.origin.y,
            "chrome at {percent}%"
        );
        assert!(
            prompt.disc.bottom() <= prompt.name.origin.y,
            "disc at {percent}%"
        );
        assert!(prompt.name.bottom() <= block.origin.y, "name at {percent}%");
        assert!(field.bottom() <= block.bottom(), "field at {percent}%");
        assert!(block.bottom() <= screen.bottom(), "block at {percent}%");
    }
}

/// Every length is authored in logical pixels and converted through the one
/// shared scale, so a denser output gets a proportionally larger column.
#[test]
fn the_column_grows_with_the_desktop_scale() {
    let screen = Rect::new(0, 0, 2000, 1200);
    let double = Scale::from_percent(200).expect("200% is in range");

    let single = panel_rect(screen, Scale::ONE);
    let doubled = panel_rect(screen, double);

    assert_eq!(doubled.width, single.width * 2);
    assert_eq!(doubled.height, single.height * 2);
    assert_eq!(
        Prompt::new(screen, double).disc.width,
        Prompt::new(screen, Scale::ONE).disc.width * 2
    );
}

/// A screen too small for the whole column still gets a prompt that fits on
/// it: one that refused to draw, or ran off the edge, would be one that did
/// not ask.
#[test]
fn a_small_screen_still_gets_a_usable_prompt() {
    let small = Rect::new(0, 0, 640, 480);
    let surface = AuthSurface::new("ann", "ann");
    let block = panel_rect(small, Scale::ONE);
    let field = surface.field_rect(small, Scale::ONE, &theme());

    assert!(block.right() <= small.right() && block.bottom() <= small.bottom());
    assert!(field.right() <= small.right() && field.bottom() <= small.bottom());
    assert!(field.width > 0 && field.height > 0);

    let frame = surface
        .render(small, Scale::ONE, &theme(), Backdrop::Desktop)
        .expect("a frame on a small screen");
    assert!(painted(&frame, field), "the field drew nothing");
}

/// The clock is present, or absent, for a given screen and density alone —
/// never because of which body is up. A screen that gains or loses its clock
/// when an account is picked is one that appears to jump.
#[test]
fn the_chrome_is_the_same_whichever_body_is_up() {
    let tiles = vec![
        AccountTile::new("Ann", "ann"),
        AccountTile::new("Bob", "bob"),
        AccountTile::new("Cai", "cai"),
    ];
    let short = Rect::new(0, 0, 640, 400);

    for screen in [SCREEN, short] {
        for percent in [100, 200] {
            let scale = Scale::from_percent(percent).expect("a supported density");
            let band = chrome_band(screen, scale);
            let mut chooser = AuthSurface::with_accounts(tiles.clone());
            chooser.set_chrome(chrome());
            let mut prompt = AuthSurface::new("ann", "ann");
            prompt.set_chrome(chrome());

            let shown = |surface: &AuthSurface| {
                let frame = surface
                    .render(screen, scale, &theme(), Backdrop::Desktop)
                    .expect("a frame");
                painted(&frame, band)
            };
            assert_eq!(
                shown(&chooser),
                shown(&prompt),
                "{screen:?} at {percent}% disagreed about the chrome"
            );
            assert_eq!(band.height > 0, shown(&prompt), "{screen:?} at {percent}%");
        }
    }
}

/// A screen narrower than the block clamps it rather than letting it run off
/// the edge.
#[test]
fn the_prompt_block_never_grows_past_the_screen() {
    let tiny = Rect::new(0, 0, 100, 50);

    let block = panel_rect(tiny, Scale::ONE);

    assert!(block.width <= tiny.width && block.height <= tiny.height);
}

#[test]
fn the_field_sits_inside_the_panel() {
    let surface = AuthSurface::new("ann", "ann");
    let panel = panel_rect(SCREEN, Scale::ONE);

    let field = surface.field_rect(SCREEN, Scale::ONE, &theme());

    assert!(field.origin.x >= panel.origin.x);
    assert!(field.origin.y >= panel.origin.y);
    assert!(field.right() <= panel.right());
    assert!(field.bottom() <= panel.bottom());
}

/// The rectangle the paint uses is the rectangle the pointer is tested
/// against: everything a keystroke changes on screen falls inside the field
/// rect the hit test reads, so the two cannot drift apart.
#[test]
fn the_painted_field_is_the_rectangle_the_pointer_is_tested_against() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::refusing();
    let field = surface.field_rect(SCREEN, Scale::ONE, &theme());
    let before = render(&surface);

    feed(&mut surface, &key(Key::Char('a')), &mut verifier);
    let after = render(&surface);

    let changed = changed_pixels(&before, &after);
    assert!(!changed.is_empty(), "the keystroke painted nothing at all");
    for pixel in changed {
        assert!(
            field.contains(pixel),
            "the keystroke painted {pixel:?}, outside the hit-tested field"
        );
    }
}

/// A pointer is only inside the field where the hit test says it is: a
/// sample on the backdrop leaves the field untouched, and one at its centre
/// wakes it.
#[test]
fn only_a_pointer_within_the_field_rect_reaches_the_field() {
    let mut surface = AuthSurface::new("ann", "ann");
    let mut verifier = Scripted::refusing();
    let field = surface.field_rect(SCREEN, Scale::ONE, &theme());

    assert!(
        !feed(&mut surface, &moved(1, 1), &mut verifier).redraw(),
        "a sample on the backdrop changes nothing"
    );
    assert!(
        feed(&mut surface, &centre(field), &mut verifier).redraw(),
        "a sample inside the field wakes it"
    );
}

/// The account heads the panel, and a name the embedder could not resolve
/// is not a reason to refuse to ask: it falls back to the placeholder.
#[test]
fn an_empty_account_heads_the_surface_with_the_placeholder() {
    let empty = render(&AuthSurface::new("", ""));
    let placeholder = render(&AuthSurface::new(UNNAMED_ACCOUNT, UNNAMED_ACCOUNT));
    let named_account = render(&AuthSurface::new("ann", "ann"));

    assert_eq!(empty, placeholder);
    assert_ne!(empty, named_account);
}

/// Every part of the column actually puts marks inside its own band. A
/// geometry the paint does not honour reads correctly in the arithmetic and
/// draws a blank screen.
#[test]
fn every_part_of_the_column_paints_inside_its_own_band() {
    let mut surface = AuthSurface::new("Ann Example", "Ann Example");
    surface.set_chrome(chrome());
    let prompt = Prompt::new(SCREEN, Scale::ONE);
    let field = surface.field_rect(SCREEN, Scale::ONE, &theme());
    let notice = notice_band(prompt.block, field, Scale::ONE).expect("room for the notice");
    let [clock, date, host] = chrome_bands(chrome_band(SCREEN, Scale::ONE), Scale::ONE);
    let frame = render(&surface);

    for (band, part) in [
        (clock, "clock"),
        (date, "date"),
        (host, "host"),
        (prompt.disc, "disc"),
        (prompt.name, "name"),
        (field, "field"),
        (notice, "notice"),
    ] {
        assert!(painted(&frame, band), "the {part} drew nothing in {band:?}");
    }
}

/// The column reads against its own backdrop on both themes: the ink reaches
/// at least half the separation the theme itself promises against the ground
/// behind it. A build that draws no glyphs fails here rather than passing
/// quietly with an empty screen.
#[test]
fn the_column_reads_against_its_backdrop_on_both_themes() {
    for active in [Theme::dark(), Theme::light()] {
        let mut surface = AuthSurface::new("Ann Example", "Ann Example");
        surface.set_chrome(chrome());
        let prompt = Prompt::new(SCREEN, Scale::ONE);
        let field = surface.field_rect(SCREEN, Scale::ONE, &active);
        let notice = notice_band(prompt.block, field, Scale::ONE).expect("room for the notice");
        let clock = chrome_bands(chrome_band(SCREEN, Scale::ONE), Scale::ONE)[0];
        let frame = render_in(&surface, &active);
        let palette = active.palette();

        for (band, ink, part) in [
            (clock, palette.on_surface, "clock"),
            (prompt.name, palette.on_surface, "name"),
            (notice, palette.on_surface_muted, "notice"),
            (field, palette.rim_active, "field"),
        ] {
            let promised = separation(ink, palette.desktop);
            let reached = contrast_in(&frame, band);
            assert!(
                reached * 2 >= promised,
                "the {part} reached {reached} of the {promised} {} promises",
                active.name()
            );
        }
    }
}

#[test]
fn a_frame_is_produced_at_every_supported_density() {
    let surface = AuthSurface::new("ann", "ann");

    for percent in [Scale::MIN_PERCENT, 50, 100, 150, 200, Scale::MAX_PERCENT] {
        let scale = Scale::from_percent(percent).expect("a supported density");
        let frame = surface
            .render(SCREEN, scale, &theme(), Backdrop::Desktop)
            .expect("a frame at every density");

        assert_eq!(frame.width(), SCREEN.width);
        assert_eq!(frame.height(), SCREEN.height);
    }
}

/// A frame with no pixels covers nothing, so it is refused rather than
/// handed back as if it had: an embedder that must cover the screen fails
/// closed on it.
#[test]
fn a_screen_with_no_pixels_yields_no_frame() {
    let surface = AuthSurface::new("ann", "ann");

    for screen in [Rect::new(0, 0, 0, 600), Rect::new(0, 0, 1000, 0)] {
        assert!(surface
            .render(screen, Scale::ONE, &theme(), Backdrop::Desktop)
            .is_none());
    }
}
