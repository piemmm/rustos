//! Unit tests for the authentication surface.
//!
//! These cover the state machine (typing, submission, the erasure of the
//! secret on every verdict, the wordings), the modality property that only a
//! verified secret concludes the surface, the one geometry definition that
//! paint and hit test share, and the render's degraded paths.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::surface::{
    panel_rect, AuthSurface, Backdrop, EventContext, Outcome, Verdict, Verifier, HINT,
    MAX_PASSWORD, PANEL_HEIGHT, PANEL_WIDTH, REFUSED, UNNAMED_ACCOUNT, UNREACHABLE,
};

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};

const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

/// A screen comfortably larger than the panel, so the centring is visible
/// rather than clamped.
const SCREEN: Rect = Rect::new(0, 0, 1000, 600);

fn theme() -> Theme {
    Theme::dark()
}

fn key(key: Key) -> InputEvent {
    InputEvent::KeyPressed {
        key,
        modifiers: Modifiers::default(),
    }
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

/// The centre of `rect`, as a pointer position.
fn centre(rect: Rect) -> InputEvent {
    moved(
        rect.origin.x + i32::try_from(rect.width / 2).expect("test rectangles are small"),
        rect.origin.y + i32::try_from(rect.height / 2).expect("test rectangles are small"),
    )
}

/// A [`Verifier`] answering from a scripted list of verdicts, oldest first,
/// and recording every secret it was offered — so a test can assert both
/// what was typed and what came back for it. Offered past the end of the
/// script, it refuses.
#[derive(Default)]
struct Scripted {
    answers: Vec<Verdict>,
    offered: Vec<String>,
}

impl Scripted {
    fn new(mut answers: Vec<Verdict>) -> Self {
        answers.reverse();
        Self {
            answers,
            offered: Vec::new(),
        }
    }

    fn refusing() -> Self {
        Self::new(Vec::new())
    }
}

impl Verifier for Scripted {
    fn verify(&mut self, secret: &str) -> Verdict {
        self.offered.push(String::from(secret));
        self.answers.pop().unwrap_or(Verdict::Refused)
    }
}

/// Apply one event on [`SCREEN`] at the unscaled density.
fn feed(surface: &mut AuthSurface, event: &InputEvent, verifier: &mut dyn Verifier) -> Outcome {
    let theme = theme();
    surface.on_event(
        event,
        &mut EventContext {
            screen: SCREEN,
            scale: Scale::ONE,
            theme: &theme,
            verifier,
        },
    )
}

/// Type `secret` one key at a time, then press Enter, returning the outcome
/// of that final, submitting event.
fn submit(surface: &mut AuthSurface, secret: &str, verifier: &mut dyn Verifier) -> Outcome {
    for ch in secret.chars() {
        feed(surface, &key(Key::Char(ch)), verifier);
    }
    feed(surface, &key(Key::Named(NamedKey::Enter)), verifier)
}

fn render(surface: &AuthSurface) -> Surface {
    surface
        .render(SCREEN, Scale::ONE, &theme(), Backdrop::Desktop)
        .expect("a 1000x600 frame")
}

#[test]
fn typing_builds_the_secret_and_enter_offers_it_once_exactly_as_typed() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Verified]);

    let outcome = submit(&mut surface, "Hunter2!", &mut verifier);

    assert!(outcome.verified());
    assert_eq!(verifier.offered, vec![String::from("Hunter2!")]);
}

/// Editing keys reach the field, so the secret offered is the one on screen
/// at the moment Enter is pressed rather than everything ever typed.
#[test]
fn backspace_edits_the_secret_before_it_is_offered() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::refusing();

    for ch in "Hunterx".chars() {
        feed(&mut surface, &key(Key::Char(ch)), &mut verifier);
    }
    feed(
        &mut surface,
        &key(Key::Named(NamedKey::Backspace)),
        &mut verifier,
    );
    submit(&mut surface, "2", &mut verifier);

    assert_eq!(verifier.offered, vec![String::from("Hunter2")]);
}

/// Every key is worth a frame: one arrives at human typing rate, and can
/// move the caret without reporting an edit.
#[test]
fn every_key_asks_for_a_repaint() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::refusing();

    assert!(feed(&mut surface, &key(Key::Char('a')), &mut verifier).redraw());
    assert!(feed(
        &mut surface,
        &key(Key::Named(NamedKey::Home)),
        &mut verifier
    )
    .redraw());
}

/// The field is bounded so its buffer is reserved once and never grown; a
/// caller leaning on the keyboard is truncated, not reallocated.
#[test]
fn the_secret_is_bounded_at_the_documented_maximum() {
    let mut surface = AuthSurface::new("ann");
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
        let mut surface = AuthSurface::new("ann");
        let mut verifier = Scripted::new(vec![verdict]);

        submit(&mut surface, "secret", &mut verifier);
        feed(
            &mut surface,
            &key(Key::Named(NamedKey::Enter)),
            &mut verifier,
        );

        assert_eq!(
            verifier.offered,
            vec![String::from("secret"), String::new()],
            "the secret survived a {verdict:?} verdict"
        );
    }
}

#[test]
fn the_resting_notice_is_the_hint() {
    assert_eq!(AuthSurface::new("ann").notice(), HINT);
}

/// "Wrong password" and "I could not ask" call for different reactions from
/// the person at the keyboard, so they never read the same.
#[test]
fn a_refusal_and_an_unreachable_authority_read_differently() {
    let mut refused = AuthSurface::new("ann");
    let mut unreachable = AuthSurface::new("ann");

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
    let mut surface = AuthSurface::new("ann");
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
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::refusing();

    for event in [
        key(Key::Named(NamedKey::Escape)),
        key(Key::Named(NamedKey::Enter)),
        key(Key::Named(NamedKey::Tab)),
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

/// Only a submission asks the authority at all, so an authority that would
/// say yes cannot be reached by typing, hovering, or clicking.
#[test]
fn only_a_submission_reaches_the_authority() {
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::new(vec![Verdict::Verified, Verdict::Verified]);

    for event in [key(Key::Char('a')), moved(500, 300), PRESS, RELEASE] {
        assert!(!feed(&mut surface, &event, &mut verifier).verified());
    }

    assert!(verifier.offered.is_empty(), "nothing was ever offered");
    assert!(feed(
        &mut surface,
        &key(Key::Named(NamedKey::Enter)),
        &mut verifier
    )
    .verified());
}

#[test]
fn the_panel_is_centred_horizontally_and_a_third_of_the_way_down() {
    let rect = panel_rect(SCREEN, Scale::ONE);

    assert_eq!(
        rect,
        Rect::new(
            i32::try_from((SCREEN.width - PANEL_WIDTH) / 2).expect("a small screen"),
            i32::try_from((SCREEN.height - PANEL_HEIGHT) / 3).expect("a small screen"),
            PANEL_WIDTH,
            PANEL_HEIGHT,
        )
    );
}

/// A screen smaller than the panel gets the whole screen: a prompt that
/// refused to draw on a small display would be one that did not ask.
#[test]
fn the_panel_clamps_to_a_screen_smaller_than_itself() {
    let small = Rect::new(0, 0, 100, 50);

    assert_eq!(panel_rect(small, Scale::ONE), small);
}

/// The panel is authored in logical pixels and converted through the one
/// shared scale, so a denser output gets a proportionally larger prompt.
#[test]
fn the_panel_grows_with_the_desktop_scale() {
    let double = Scale::from_percent(200).expect("200% is in range");
    let screen = Rect::new(0, 0, 2000, 1200);

    let rect = panel_rect(screen, double);

    assert_eq!(rect.width, PANEL_WIDTH * 2);
    assert_eq!(rect.height, PANEL_HEIGHT * 2);
    assert_eq!(
        rect.origin.x,
        i32::try_from((2000 - rect.width) / 2).expect("a small screen")
    );
    assert_eq!(
        rect.origin.y,
        i32::try_from((1200 - rect.height) / 3).expect("a small screen")
    );
}

#[test]
fn the_field_sits_inside_the_panel() {
    let surface = AuthSurface::new("ann");
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
    let mut surface = AuthSurface::new("ann");
    let mut verifier = Scripted::refusing();
    let field = surface.field_rect(SCREEN, Scale::ONE, &theme());
    let before = render(&surface);

    feed(&mut surface, &key(Key::Char('a')), &mut verifier);
    let after = render(&surface);

    let mut changed = 0_u32;
    for y in 0..SCREEN.height {
        for x in 0..SCREEN.width {
            if before.get(x, y) == after.get(x, y) {
                continue;
            }
            changed += 1;
            assert!(
                field.contains(Point::new(
                    i32::try_from(x).expect("a small screen"),
                    i32::try_from(y).expect("a small screen")
                )),
                "the keystroke painted ({x}, {y}), outside the hit-tested field"
            );
        }
    }
    assert!(changed > 0, "the keystroke painted nothing at all");
}

/// A pointer is only inside the field where the hit test says it is: a
/// sample on the backdrop leaves the field untouched, and one at its centre
/// wakes it.
#[test]
fn only_a_pointer_within_the_field_rect_reaches_the_field() {
    let mut surface = AuthSurface::new("ann");
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
    let empty = render(&AuthSurface::new(""));
    let placeholder = render(&AuthSurface::new(UNNAMED_ACCOUNT));
    let named = render(&AuthSurface::new("ann"));

    assert_eq!(empty, placeholder);
    assert_ne!(empty, named);
}

#[test]
fn a_frame_is_produced_at_every_supported_density() {
    let surface = AuthSurface::new("ann");

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
    let surface = AuthSurface::new("ann");

    for screen in [Rect::new(0, 0, 0, 600), Rect::new(0, 0, 1000, 0)] {
        assert!(surface
            .render(screen, Scale::ONE, &theme(), Backdrop::Desktop)
            .is_none());
    }
}
