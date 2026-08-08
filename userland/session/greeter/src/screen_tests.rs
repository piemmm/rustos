use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
use tairix_abi::input::{PointerButtonCode, PointerInput};
use tairix_abi::session_ipc::{SessionRequest, SessionVerdict};
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::Errno;
use tairix_cursor::{CursorImage, PlacedCursor};
use tairix_display::ChannelOrder;
use tairix_geometry::{Point, Rect, Scale};
use tairix_greeter::{AccountTile, Verdict};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey};
use tairix_raster::{Pixel, Surface};
use tairix_theme::{MotionInteraction, Theme, Timeline};

use super::LoginScreen;
use crate::accounts::SessionTransport;
use crate::cursor::pointer_image;
use crate::frame::{rect_of, Present, Scanout};
use crate::wait::FOREVER;

const SECRET: &str = "open-sesame";

/// A screen large enough for the panel and a row of tiles.
fn mode() -> DisplayMode {
    DisplayMode {
        width_px: 1000,
        height_px: 600,
        stride_bytes: 1000 * 4,
        format: DisplayFormat::Bgra8888,
    }
}

/// An authority accepting exactly one account's one secret and refusing
/// everything else with a fixed lockout.
struct Authority {
    account: &'static str,
    secret: &'static str,
    lockout: Duration64,
    reachable: bool,
}

impl Authority {
    const fn accepting(account: &'static str, secret: &'static str) -> Self {
        Self {
            account,
            secret,
            lockout: Duration64::from_secs(20),
            reachable: true,
        }
    }

    const fn unreachable() -> Self {
        Self {
            account: "",
            secret: "",
            lockout: Duration64::ZERO,
            reachable: false,
        }
    }
}

impl SessionTransport for Authority {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        if !self.reachable {
            return Err(Errno::TimedOut);
        }
        let Ok(SessionRequest::Authenticate { username, password }) =
            SessionRequest::decode(request)
        else {
            return Err(Errno::OutOfRange);
        };
        if username == self.account && password == self.secret {
            SessionVerdict::Accepted.encode(reply)
        } else {
            SessionVerdict::Refused {
                retry_after: self.lockout,
            }
            .encode(reply)
        }
    }
}

fn screen(accounts: Vec<AccountTile>, authority: Authority) -> LoginScreen<Authority> {
    screen_in(accounts, authority, Theme::dark())
}

fn screen_in(
    accounts: Vec<AccountTile>,
    authority: Authority,
    theme: Theme,
) -> LoginScreen<Authority> {
    LoginScreen::new(
        Scanout::new(mode()).expect("a valid mode"),
        theme,
        Scale::ONE,
        "tairix".to_string(),
        accounts,
        authority,
    )
}

/// The shipped theme with reduced motion: every animation lands at once.
fn still() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        base.name(),
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        *base.fonts(),
        base.cursors().clone(),
        base.motion().with_reduced_motion(true),
        base.density(),
        base.contrast(),
    )
}

/// A moment past every animation a round can have started.
///
/// A deadline assertion about the screen *at rest* is made here, so that
/// picking an account or being refused — both of which animate — is over
/// rather than still asking for frames.
fn settled_ns() -> u64 {
    let motion = Theme::dark().motion();
    let longest = MotionInteraction::ALL
        .iter()
        .map(|interaction| u64::from(motion.duration(*interaction)))
        .max()
        .unwrap_or(0);
    longest * 1_000_000 + 1
}

fn key(named: NamedKey) -> InputEvent {
    InputEvent::KeyPressed {
        key: Key::Named(named),
        modifiers: Modifiers::default(),
    }
}

fn typed(ch: char) -> InputEvent {
    InputEvent::KeyPressed {
        key: Key::Char(ch),
        modifiers: Modifiers::default(),
    }
}

/// The arrow the login screen draws, at the scale every test runs at.
fn arrow() -> CursorImage {
    pointer_image(Scale::ONE).expect("the built-in arrow renders")
}

/// Where the pointer starts: the middle of the screen.
fn centre() -> (i32, i32) {
    (
        i32::try_from(mode().width_px / 2).expect("a small screen"),
        i32::try_from(mode().height_px / 2).expect("a small screen"),
    )
}

/// The screen rectangle `arrow` covers with its hotspot at `(x, y)`.
fn cursor_rect(image: &CursorImage, x: i32, y: i32) -> Rect {
    let hotspot = image.hotspot();
    Rect::new(x - hotspot.x, y - hotspot.y, image.width(), image.height())
}

/// Relative motion carrying the pointer from `(fx, fy)` to `(tx, ty)`.
fn moved_from(from: (i32, i32), to: (i32, i32)) -> PointerInput {
    PointerInput::MovedBy {
        dx: to.0 - from.0,
        dy: to.1 - from.1,
    }
}

/// How bright the frame's pixel at `(x, y)` is, summed over its three
/// colour channels. The fourth byte is alpha, which is not colour.
fn brightness(frame: &[u8], x: u32, y: u32) -> u32 {
    pixel_at(frame, x, y)
        .iter()
        .take(3)
        .map(|channel| u32::from(*channel))
        .sum()
}

/// The four scan-out bytes of the frame at `(x, y)`.
fn pixel_at(frame: &[u8], x: u32, y: u32) -> &[u8] {
    let stride = usize::try_from(mode().stride_bytes).expect("a small stride");
    let at = usize::try_from(y).expect("a small screen") * stride
        + usize::try_from(x).expect("a small screen") * 4;
    &frame[at..at + 4]
}

/// Every screen position where the frame differs from the kept surface's
/// own pixel — everywhere the composer drew something over it.
///
/// The frame is that surface encoded for scan-out with the cursor sampled
/// on top, so this is exactly the arrow's ink, and empty when no pointer is
/// drawn. Only meaningful straight after a whole-screen composition: a
/// frame composed within a damage rectangle is deliberately older than the
/// surface outside it.
fn drawn_over(login: &LoginScreen<Authority>) -> Vec<(i32, i32)> {
    let order = ChannelOrder::for_format(mode().format).expect("a format the frame encodes");
    let surface = login.painted.as_ref().expect("a surface is kept");
    let frame = login.frame();
    let mut found = Vec::new();
    for y in 0..mode().height_px {
        for x in 0..mode().width_px {
            let Some(pixel) = surface.get(x, y) else {
                continue;
            };
            if pixel_at(frame, x, y) != order.encode(pixel).as_slice() {
                found.push((
                    i32::try_from(x).expect("a small screen"),
                    i32::try_from(y).expect("a small screen"),
                ));
            }
        }
    }
    found
}

/// Whether `present` hands the display every pixel of `rect`.
fn covers(present: Present, rect: Rect) -> bool {
    match present {
        Present::Nothing => false,
        Present::Whole => true,
        Present::Region(region) => {
            let Some(region) = rect_of(region) else {
                return false;
            };
            region.union(&rect) == region
        }
    }
}

/// Every screen position where `left` and `right` differ.
fn differing(left: &[u8], right: &[u8]) -> Vec<(i32, i32)> {
    let mut found = Vec::new();
    for y in 0..mode().height_px {
        for x in 0..mode().width_px {
            if pixel_at(left, x, y) != pixel_at(right, x, y) {
                found.push((
                    i32::try_from(x).expect("a small screen"),
                    i32::try_from(y).expect("a small screen"),
                ));
            }
        }
    }
    found
}

/// A colour no render of this screen produces.
const MARK: Pixel = Pixel {
    r: 1,
    g: 2,
    b: 3,
    a: 255,
};

/// A screen with its pointer and its first frame already up.
fn ready() -> LoginScreen<Authority> {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.set_pointer(arrow());
    login.repaint();
    login
}

/// Stamp [`MARK`] into the kept surface at `at`.
///
/// A render cannot produce that colour, so a surface still carrying the
/// stamp is demonstrably the one that was rendered before it — which is how
/// these tests count renders without the screen counting them for them.
fn stamp(login: &mut LoginScreen<Authority>, at: (i32, i32)) {
    let surface = login.painted.as_mut().expect("a surface is kept");
    surface.set(on_screen(at.0), on_screen(at.1), MARK);
}

/// The kept surface's own pixel at `at`, or `None` when no surface is kept.
fn kept(login: &LoginScreen<Authority>, at: (i32, i32)) -> Option<Pixel> {
    let surface = login.painted.as_ref()?;
    Some(
        surface
            .get(on_screen(at.0), on_screen(at.1))
            .expect("a pixel on the screen"),
    )
}

fn on_screen(value: i32) -> u32 {
    u32::try_from(value).expect("a coordinate on the screen")
}

/// Pick the focused tile, then type `secret` and submit it. Returns the step
/// the submitting event produced.
fn offer<T: SessionTransport>(
    screen: &mut LoginScreen<T>,
    secret: &str,
    now_ns: u64,
) -> crate::Step {
    screen.on_input(&key(NamedKey::Enter), now_ns);
    for ch in secret.chars() {
        screen.on_input(&typed(ch), now_ns);
    }
    screen.on_input(&key(NamedKey::Enter), now_ns)
}

#[test]
fn the_first_frame_covers_the_whole_screen() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    assert_eq!(login.repaint(), Present::Whole);
    assert!(
        login.frame().iter().any(|byte| *byte != 0),
        "the first frame drew something"
    );
}

#[test]
fn a_verified_secret_finishes_the_screen() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    let step = offer(&mut login, SECRET, 0);
    assert!(step.verified);
    assert_eq!(
        step.answer.map(|answer| answer.verdict),
        Some(Verdict::Verified)
    );
}

/// A verified secret takes the screen to black before the process leaves,
/// so the desktop coming up out of the same black reads as one movement.
#[test]
fn a_verified_secret_fades_the_screen_to_black() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    assert!(offer(&mut login, SECRET, 0).verified);

    // Typing a secret takes longer than picking the account animates, so the
    // screen the fade covers is a settled one.
    let start = settled_ns();
    login.refresh(start, None);

    let opening = login.begin_session_fade(start);
    assert_ne!(opening, Present::Nothing, "the fade has a first frame");
    assert!(!login.session_fade_finished());

    let (x, y) = (mode().width_px / 2, mode().height_px / 2);
    let mut darkest = 3 * 255u32;
    let mut now = start;
    let mut frames = 0u32;
    while let Some(due) = login.session_fade_due(now) {
        frames += 1;
        assert!(
            frames <= login.session_fade_budget(),
            "the fade asked for more frames than it can need"
        );
        now += due;
        login.session_fade_step(now);
        let sample = brightness(login.frame(), x, y);
        assert!(sample <= darkest, "the veil lightened at frame {frames}");
        darkest = sample;
    }

    assert!(login.session_fade_finished(), "the screen is black");
    assert_eq!(darkest, 0, "and every channel of it is");
    assert!(frames > 1, "it faded rather than cut, in {frames} frames");
}

/// The fade ends on the clock and its own budget alone.
///
/// Nothing about it reads the display's answer, so a present the display
/// refuses cannot keep a successful login from leaving; and a clock that
/// stopped, or a seat that reads ready forever, runs the budget out instead
/// of spinning.
#[test]
fn the_fade_ends_on_the_clock_and_the_budget_alone() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    assert!(offer(&mut login, SECRET, 0).verified);

    // Every frame's present dropped on the floor, as a refusing display
    // would leave it.
    let _ = login.begin_session_fade(0);
    let span = settled_ns();
    let _ = login.session_fade_step(span);
    assert!(
        login.session_fade_finished(),
        "the clock alone took it to black"
    );
    assert_eq!(
        login.session_fade_due(span),
        None,
        "and it asks for no more"
    );

    let mut stuck = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    stuck.repaint();
    let _ = stuck.begin_session_fade(0);
    for _ in 0..stuck.session_fade_budget() {
        let _ = stuck.session_fade_step(0);
    }
    assert!(
        !stuck.session_fade_finished(),
        "a clock that never advances never finishes the fade — the budget is\n         what lets the login leave anyway"
    );
}

/// Once the screen has begun leaving, the decision is made: input is not
/// answered, and nothing it would have changed is drawn.
#[test]
fn input_during_the_fade_is_ignored() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    assert!(offer(&mut login, SECRET, 0).verified);
    let _ = login.begin_session_fade(0);

    for event in [key(NamedKey::Escape), typed('x'), key(NamedKey::Enter)] {
        let step = login.on_input(&event, 0);
        assert_eq!(
            step.present,
            Present::Nothing,
            "{event:?} painted something"
        );
        assert!(!step.verified);
        assert!(step.answer.is_none(), "{event:?} reached the authority");
    }
}

/// A reduced-motion theme has nothing to fade: the screen leaves at once,
/// with no extra frame presented.
#[test]
fn a_reduced_motion_fade_leaves_at_once() {
    let mut login = screen_in(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
        still(),
    );
    login.repaint();
    assert!(offer(&mut login, SECRET, 0).verified);

    assert_eq!(login.begin_session_fade(0), Present::Nothing);
    assert!(login.session_fade_finished());
    assert_eq!(login.session_fade_due(0), None);
}

/// A verified screen, its pointer drawn and its whole frame composed, at
/// the moment before it begins leaving.
fn about_to_leave() -> (LoginScreen<Authority>, u64) {
    let mut login = ready();
    assert!(offer(&mut login, SECRET, 0).verified);
    // Typing a secret takes longer than picking the account animates, so
    // what the fade covers is a settled screen.
    let start = settled_ns();
    login.refresh(start, None);
    login.repaint();
    (login, start)
}

/// The pointer leaves with the screen it belonged to. From the first veiled
/// frame nothing is drawn for it: the verdict is given, input is no longer
/// answered, and a bright arrow over the black would point at nothing.
#[test]
fn the_pointer_is_gone_from_the_first_veiled_frame() {
    let (mut login, start) = about_to_leave();
    assert!(
        !drawn_over(&login).is_empty(),
        "the arrow is on the frame to begin with"
    );

    login.begin_session_fade(start);

    assert_eq!(
        drawn_over(&login),
        Vec::new(),
        "the veiled frame is the veiled surface and nothing over it"
    );
}

/// The frame the pointer leaves on repaints where it sat, so no arrow can
/// be left burned into the presented bytes.
#[test]
fn the_frame_the_pointer_leaves_on_repaints_where_it_sat() {
    let (mut login, start) = about_to_leave();
    let sat = cursor_rect(&arrow(), centre().0, centre().1).intersection(&login.screen());
    assert!(!sat.is_empty(), "the arrow is on the screen");
    let ink = drawn_over(&login);
    let before = login.frame().to_vec();

    let opening = login.begin_session_fade(start);

    assert!(
        covers(opening, sat),
        "{opening:?} does not present the {sat:?} the arrow sat on"
    );
    // The veil opens fully transparent, so the only pixels this frame can
    // change are the ones the arrow was inking — and it changes all of them.
    assert_eq!(
        differing(&before, login.frame()),
        ink,
        "the frame changed somewhere other than where the arrow was"
    );
}

/// The pointer still tracks while the screen leaves; it is only not drawn.
/// Nothing is presented for a move nobody can see.
#[test]
fn a_move_during_the_fade_presents_nothing_and_still_tracks_the_pointer() {
    let (mut login, start) = about_to_leave();
    login.begin_session_fade(start);
    let veiled = login.frame().to_vec();

    let corner = (30, 30);
    let step = login.on_pointer(&moved_from(centre(), corner), start);

    assert_eq!(step.present, Present::Nothing, "a move nobody can see");
    assert_eq!(login.frame(), veiled.as_slice(), "and nothing was drawn");
    assert_eq!(login.cursor.at(), Point::new(corner.0, corner.1));
    assert_eq!(
        login.pointer.as_ref().map(PlacedCursor::bounds),
        Some(cursor_rect(&arrow(), corner.0, corner.1)),
        "the artwork followed the position it is not drawn at"
    );
}

/// Only the screen leaving hides the pointer. A screen animating for any
/// other reason — a refused attempt shaking, a lockout counting down —
/// draws it exactly where it sits, as it always did.
#[test]
fn an_unveiled_screen_still_draws_its_pointer() {
    let mut login = ready();
    let sits = cursor_rect(&arrow(), centre().0, centre().1);
    let confined = |ink: &[(i32, i32)]| {
        assert!(!ink.is_empty(), "the arrow is drawn");
        for (x, y) in ink {
            assert!(
                sits.contains(Point::new(*x, *y)),
                "({x}, {y}) is drawn outside the cursor at {sits:?}"
            );
        }
    };
    confined(&drawn_over(&login));

    assert!(!offer(&mut login, "wrong", 0).verified);
    login.refresh(Timeline::FRAME_NS, None);
    login.repaint();
    confined(&drawn_over(&login));
}

/// A reduced-motion theme has no veil to present, so the screen leaves on
/// the frame it was already showing — pointer and all. There is no veiled
/// frame for the arrow to be absent from.
#[test]
fn a_reduced_motion_fade_leaves_the_frame_as_it_was() {
    let mut login = screen_in(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
        still(),
    );
    login.set_pointer(arrow());
    login.repaint();
    assert!(offer(&mut login, SECRET, 0).verified);
    login.repaint();
    let showing = login.frame().to_vec();

    assert_eq!(login.begin_session_fade(0), Present::Nothing);
    assert_eq!(login.frame(), showing.as_slice());
    assert!(
        !drawn_over(&login).is_empty(),
        "the arrow is still on the frame the screen leaves on"
    );
}

#[test]
fn a_refusal_puts_its_lockout_on_the_screen_and_keeps_asking() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    let step = offer(&mut login, "wrong", 0);
    assert!(!step.verified);
    assert_eq!(
        step.answer.map(|answer| answer.verdict),
        Some(Verdict::Refused)
    );
    assert_eq!(
        step.answer.map(|answer| answer.retry_after),
        Some(Duration64::from_secs(20))
    );
    assert!(
        login.notice().contains("20"),
        "the notice presents the lockout, got {:?}",
        login.notice()
    );
}

#[test]
fn a_lockout_counts_down_on_the_screen_and_clears() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    offer(&mut login, "wrong", 0);

    let after_ten = 10 * 1_000_000_000;
    let step = login.refresh(after_ten, None);
    assert_ne!(step.present, Present::Nothing, "the countdown repainted");
    assert!(login.notice().contains("10"));

    let expired = 21 * 1_000_000_000;
    login.refresh(expired, None);
    assert!(
        !login.notice().contains("10"),
        "the lockout cleared, got {:?}",
        login.notice()
    );
}

#[test]
fn an_unreachable_authority_keeps_the_surface_alive() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::unreachable(),
    );
    login.repaint();
    let step = offer(&mut login, SECRET, 0);
    assert!(!step.verified, "no answer is never a pass");
    assert_eq!(
        step.answer.map(|answer| answer.verdict),
        Some(Verdict::Unreachable)
    );
    assert_eq!(
        step.answer.map(|answer| answer.retry_after),
        Some(Duration64::ZERO),
        "an unanswerable attempt is not a lockout"
    );
    assert_eq!(
        login.park_timeout(settled_ns(), None),
        FOREVER,
        "nothing is counting down, so nothing is armed"
    );

    let again = offer(&mut login, SECRET, 0);
    assert!(!again.verified, "the surface is still asking");
}

#[test]
fn no_accounts_still_reaches_the_authority_by_name() {
    let mut login = screen(Vec::new(), Authority::accepting("ann", SECRET));
    login.repaint();

    // The lone tile leads to a typed login name, then to the secret.
    login.on_input(&key(NamedKey::Enter), 0);
    for ch in "ann".chars() {
        login.on_input(&typed(ch), 0);
    }
    login.on_input(&key(NamedKey::Enter), 0);
    for ch in SECRET.chars() {
        login.on_input(&typed(ch), 0);
    }
    let step = login.on_input(&key(NamedKey::Enter), 0);
    assert!(step.verified);
}

#[test]
fn an_idle_screen_arms_no_timer_and_a_clocked_one_wakes_at_the_minute() {
    let login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    assert_eq!(login.park_timeout(0, None), FOREVER);

    // Twenty seconds past a minute boundary, so forty seconds to the next.
    let twenty_past = Time64::from_secs(1_700_000_060);
    assert_eq!(login.park_timeout(0, Some(twenty_past)), 40_000_000_000);
}

#[test]
fn a_running_lockout_is_the_nearer_deadline() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    offer(&mut login, "wrong", 0);
    let twenty_past = Time64::from_secs(1_700_000_060);
    assert_eq!(
        login.park_timeout(0, Some(twenty_past)),
        Timeline::FRAME_NS,
        "the refusal's shake is the nearest thing owing a frame"
    );
    assert_eq!(
        login.park_timeout(settled_ns(), Some(twenty_past)),
        1_000_000_000,
        "once it has settled, the lockout ticks before the minute turns"
    );
}

#[test]
fn a_keystroke_presents_only_what_it_changed() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    login.on_input(&key(NamedKey::Enter), 0);

    let step = login.on_input(&typed('x'), 0);
    let Present::Region(region) = step.present else {
        panic!("a keystroke touches the field, not the screen: {step:?}");
    };
    assert!(region.width_px < mode().width_px);
    assert!(region.height_px < mode().height_px);
}

#[test]
fn a_wake_with_nothing_to_do_presents_nothing() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    let noon = Time64::from_secs(1_700_000_060);
    assert_ne!(login.refresh(0, Some(noon)).present, Present::Nothing);
    assert_eq!(
        login.refresh(0, Some(noon)).present,
        Present::Nothing,
        "the same minute a second time changes nothing"
    );
}

#[test]
fn a_wallpaper_is_drawn_behind_the_panel() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    let plain = login.frame().to_vec();

    let paper = Surface::filled(
        mode().width_px,
        mode().height_px,
        Pixel {
            r: 200,
            g: 40,
            b: 40,
            a: 255,
        },
    )
    .expect("a screen-sized image");
    login.set_wallpaper(paper);
    assert_eq!(login.repaint(), Present::Whole);
    assert_ne!(login.frame(), plain.as_slice());
}

#[test]
fn the_pointer_is_drawn_over_the_surface_and_only_where_it_sits() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    let bare = login.frame().to_vec();

    let image = arrow();
    let sits = cursor_rect(&image, centre().0, centre().1);
    login.set_pointer(image);
    assert_eq!(login.repaint(), Present::Whole);

    let changed = differing(&bare, login.frame());
    assert!(
        !changed.is_empty(),
        "a whole-screen repaint draws the pointer"
    );
    for (x, y) in changed {
        assert!(
            sits.contains(Point::new(x, y)),
            "({x}, {y}) changed outside the cursor at {sits:?}"
        );
    }
}

/// The region `step` presented, or a failure naming what came instead.
fn presented(step: crate::Step) -> Rect {
    let Present::Region(region) = step.present else {
        panic!("a sub-screen change is a region present, got {step:?}");
    };
    rect_of(region).expect("a region on the screen")
}

#[test]
fn a_move_presents_the_old_and_new_cursor_rectangles_and_nothing_larger() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    let image = arrow();
    login.set_pointer(image.clone());
    login.repaint();

    // Onto the backdrop first, so the only thing the next move changes is
    // the pointer itself.
    let corner = (30, 30);
    login.on_pointer(&moved_from(centre(), corner), 0);

    let along = (70, 30);
    let step = login.on_pointer(&moved_from(corner, along), 0);
    let expected = cursor_rect(&image, corner.0, corner.1)
        .union(&cursor_rect(&image, along.0, along.1))
        .intersection(&login.screen());
    assert_eq!(presented(step), expected);

    // And into the corner, where the union runs off the screen: what is
    // presented is the part that is on it.
    let step = login.on_pointer(&moved_from(along, (0, 0)), 0);
    let clipped = cursor_rect(&image, along.0, along.1)
        .union(&cursor_rect(&image, 0, 0))
        .intersection(&login.screen());
    assert_eq!(presented(step), clipped);
    assert_eq!(
        login.screen().union(&clipped),
        login.screen(),
        "the clipped union reaches past the screen"
    );
}

#[test]
fn a_move_that_changes_no_control_state_renders_the_surface_once_in_total() {
    let mut login = ready();
    // Away from the centred panel, so nothing the sweep passes over is a
    // tile whose focus would honestly change.
    let start = (30, 30);
    login.on_pointer(&moved_from(centre(), start), 0);
    stamp(&mut login, start);

    // The stream a hand resting on the mouse produces.
    let mut from = start;
    for step in 1..=50 {
        let to = (start.0 + step, start.1);
        assert_ne!(
            login.on_pointer(&moved_from(from, to), 0).present,
            Present::Nothing
        );
        from = to;
    }

    assert_eq!(
        kept(&login, start),
        Some(MARK),
        "the surface was rendered again for a move that changed nothing"
    );
}

#[test]
fn the_frame_after_a_move_is_what_a_full_repaint_would_have_drawn() {
    let spot = (240, 180);
    let mut moved = ready();
    moved.on_pointer(&moved_from(centre(), spot), 0);

    let mut fresh = ready();
    fresh.on_pointer(&moved_from(centre(), spot), 0);
    assert_eq!(fresh.repaint(), Present::Whole);

    let differences = differing(moved.frame(), fresh.frame());
    assert!(
        differences.is_empty(),
        "{} pixels differ from a full repaint, first at {:?}",
        differences.len(),
        differences.first()
    );
}

/// What a drain does with a burst: apply every record, merge what each one
/// changed, and hand the display that one present.
#[test]
fn a_run_of_moves_merges_into_one_present_covering_every_one_of_them() {
    let mut login = ready();
    let image = arrow();
    let start = (100, 100);
    login.on_pointer(&moved_from(centre(), start), 0);

    let mut merged = Present::Nothing;
    let mut from = start;
    for step in 1..=8 {
        let to = (start.0 + step * 3, start.1);
        let present = login.on_pointer(&moved_from(from, to), 0).present;
        merged = merged.merged(present, login.scanout.mode());
        from = to;
    }

    let expected = cursor_rect(&image, start.0, start.1)
        .union(&cursor_rect(&image, from.0, from.1))
        .intersection(&login.screen());
    let Present::Region(region) = merged else {
        panic!("a run across the backdrop is one sub-screen region, got {merged:?}");
    };
    assert_eq!(
        Rect::new(
            i32::try_from(region.x).expect("on screen"),
            i32::try_from(region.y).expect("on screen"),
            region.width_px,
            region.height_px,
        ),
        expected
    );
}

#[test]
fn a_move_onto_the_field_presents_the_field_and_the_pointer_together() {
    let mut login = ready();
    let image = arrow();
    login.on_input(&key(NamedKey::Enter), 0);

    let away = (5, 5);
    login.on_pointer(&moved_from(centre(), away), 0);
    let field = login
        .surface
        .field_rect(login.screen(), login.scale, &login.theme);
    let onto = (field.origin.x + 2, field.origin.y + 2);

    let step = login.on_pointer(&moved_from(away, onto), 0);
    let expected = cursor_rect(&image, away.0, away.1)
        .union(&cursor_rect(&image, onto.0, onto.1))
        .union(&field)
        .intersection(&login.screen());
    assert_eq!(presented(step), expected);
}

#[test]
fn a_keystroke_and_a_clock_tick_each_rebuild_the_surface() {
    let mut login = ready();
    login.on_input(&key(NamedKey::Enter), 0);
    stamp(&mut login, centre());
    assert_ne!(login.on_input(&typed('x'), 0).present, Present::Nothing);
    assert_ne!(kept(&login, centre()), Some(MARK), "a keystroke");

    let mut login = ready();
    stamp(&mut login, centre());
    let noon = Time64::from_secs(1_700_000_060);
    assert_ne!(login.refresh(0, Some(noon)).present, Present::Nothing);
    assert_ne!(kept(&login, centre()), Some(MARK), "a clock tick");
}

#[test]
fn a_verdict_and_the_countdown_it_starts_each_rebuild_the_surface() {
    let mut login = ready();
    login.on_input(&key(NamedKey::Enter), 0);
    for ch in "wrong".chars() {
        login.on_input(&typed(ch), 0);
    }

    stamp(&mut login, centre());
    let verdict = login.on_input(&key(NamedKey::Enter), 0);
    assert_eq!(
        verdict.answer.map(|answer| answer.verdict),
        Some(Verdict::Refused)
    );
    assert_ne!(verdict.present, Present::Nothing);
    assert_ne!(kept(&login, centre()), Some(MARK), "a verdict");

    stamp(&mut login, centre());
    let counted = login.refresh(10 * 1_000_000_000, None);
    assert_ne!(counted.present, Present::Nothing);
    assert_ne!(
        kept(&login, centre()),
        Some(MARK),
        "a lockout counting down"
    );
}

#[test]
fn an_installed_wallpaper_drops_the_kept_surface() {
    let mut login = ready();
    stamp(&mut login, centre());

    login.set_wallpaper(
        Surface::filled(
            mode().width_px,
            mode().height_px,
            Pixel {
                r: 200,
                g: 40,
                b: 40,
                a: 255,
            },
        )
        .expect("a screen-sized image"),
    );
    assert_eq!(kept(&login, centre()), None, "the wallpaper is behind it");
    assert_eq!(login.repaint(), Present::Whole);
    assert_ne!(kept(&login, centre()), Some(MARK));
}

/// Installed pointer artwork is sampled over the kept surface rather than
/// painted into it, so it appears on the next frame without the screen being
/// rendered again — and the pixels behind it are still there to be restored
/// when it moves off them.
#[test]
fn an_installed_pointer_draws_over_the_kept_surface_without_rebuilding_it() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    let bare = login.frame().to_vec();
    stamp(&mut login, centre());

    let image = arrow();
    login.set_pointer(image.clone());
    assert_eq!(
        kept(&login, centre()),
        Some(MARK),
        "the pointer is not part of the surface"
    );

    assert_eq!(login.repaint(), Present::Whole);
    let sits = cursor_rect(&image, centre().0, centre().1);
    let changed = differing(&bare, login.frame());
    assert!(!changed.is_empty(), "the new pointer is on the frame");
    for (x, y) in changed {
        assert!(
            sits.contains(Point::new(x, y)) || (x, y) == centre(),
            "({x}, {y}) changed outside the cursor at {sits:?}"
        );
    }
}

#[test]
fn a_moved_pointer_leaves_nothing_painted_behind_it() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    let bare = login.frame().to_vec();

    let image = arrow();
    login.set_pointer(image.clone());
    login.repaint();
    let corner = (30, 30);
    login.on_pointer(&moved_from(centre(), corner), 0);

    let sits = cursor_rect(&image, corner.0, corner.1);
    let changed = differing(&bare, login.frame());
    assert!(!changed.is_empty(), "the pointer is drawn where it landed");
    for (x, y) in changed {
        assert!(
            sits.contains(Point::new(x, y)),
            "({x}, {y}) still differs after the pointer left it"
        );
    }
}

#[test]
fn motion_that_moves_nothing_presents_nothing() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.set_pointer(arrow());
    login.repaint();
    login.on_pointer(&moved_from(centre(), (30, 30)), 0);

    let step = login.on_pointer(&PointerInput::MovedBy { dx: 0, dy: 0 }, 0);
    assert_eq!(step.present, Present::Nothing);

    // Nor does motion the screen edge swallows.
    let step = login.on_pointer(&PointerInput::MovedBy { dx: -1000, dy: 0 }, 0);
    assert_ne!(
        step.present,
        Present::Nothing,
        "the pointer reached the edge"
    );
    let step = login.on_pointer(&PointerInput::MovedBy { dx: -1000, dy: 0 }, 0);
    assert_eq!(step.present, Present::Nothing, "and stayed there");
}

#[test]
fn a_pointer_that_would_not_rasterise_leaves_a_working_screen_with_none_drawn() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    let bare = login.frame().to_vec();

    // No `set_pointer`: the screen still tracks and hit-tests, draws no
    // cursor, and asks for no paint it does not owe.
    let step = login.on_pointer(&moved_from(centre(), (30, 30)), 0);
    assert_eq!(step.present, Present::Nothing);
    assert_eq!(login.frame(), bare.as_slice(), "nothing was drawn for it");
    assert_eq!(login.repaint(), Present::Whole);
    assert_eq!(login.frame(), bare.as_slice());

    assert!(
        offer(&mut login, SECRET, 0).verified,
        "and it still logs in"
    );
}

/// A button report expands to a move *and* a transition, and the two are
/// merged into one frame. On the backdrop neither changes a pixel, so the
/// merge must not invent damage of its own.
#[test]
fn a_button_where_nothing_answers_it_presents_nothing() {
    let mut login = screen(
        vec![AccountTile::new("Ann Example", "ann")],
        Authority::accepting("ann", SECRET),
    );
    login.set_pointer(arrow());
    login.repaint();
    login.on_pointer(&moved_from(centre(), (30, 30)), 0);
    let bare = login.frame().to_vec();

    for report in [
        PointerInput::Pressed(PointerButtonCode::Primary),
        PointerInput::Released(PointerButtonCode::Primary),
        PointerInput::Scrolled { dx: 0, dy: 3 },
    ] {
        let step = login.on_pointer(&report, 0);
        assert_eq!(step.present, Present::Nothing, "{report:?} changed nothing");
        assert!(!step.verified);
    }
    assert_eq!(login.frame(), bare.as_slice());
}

#[test]
fn the_account_the_authority_was_asked_about_is_the_one_picked() {
    let mut login = screen(
        vec![
            AccountTile::new("Ann Example", "ann"),
            AccountTile::new("Bo Example", "bo"),
        ],
        Authority::accepting("bo", SECRET),
    );
    login.repaint();
    login.on_input(&key(NamedKey::Tab), 0);
    login.on_input(&key(NamedKey::Enter), 0);
    for ch in SECRET.chars() {
        login.on_input(&typed(ch), 0);
    }
    let step = login.on_input(&key(NamedKey::Enter), 0);
    assert!(step.verified);
}

/// An idle screen's park timeout is unchanged by motion: no timer where there
/// was none.
#[test]
fn an_idle_screen_park_timeout_is_still_forever() {
    let login = screen(
        vec![
            AccountTile::new("Ann Example", "ann"),
            AccountTile::new("Bo Example", "bo"),
        ],
        Authority::accepting("ann", SECRET),
    );
    assert_eq!(login.park_timeout(0, None), FOREVER);
    // After a first paint, still idle.
    let mut login = login;
    login.repaint();
    assert_eq!(login.park_timeout(0, None), FOREVER);
}

/// A focus change arms a short timeout; successive refreshes present
/// shrinking-or-equal damage covering the tiles; once settled the timeout
/// returns to idle.
#[test]
fn a_focus_change_arms_motion_and_settles_cleanly() {
    let mut login = screen(
        vec![
            AccountTile::new("Ann Example", "ann"),
            AccountTile::new("Bo Example", "bo"),
        ],
        Authority::accepting("ann", SECRET),
    );
    login.repaint();
    assert_eq!(login.park_timeout(0, None), FOREVER);

    let step = login.on_input(&key(NamedKey::Tab), 0);
    assert_ne!(step.present, Present::Nothing, "focus move presents");

    let millis = Theme::dark()
        .motion()
        .duration(tairix_theme::MotionInteraction::SelectionChange);
    let span_ns = u64::from(millis) * 1_000_000;
    assert!(millis > 0);

    let timeout = login.park_timeout(0, None);
    assert!(
        timeout < FOREVER && timeout <= span_ns,
        "motion arms a short timeout, got {timeout}"
    );

    let mut now = 0u64;
    let step_ns = (span_ns / 8).max(1);
    let mut saw_present = false;
    loop {
        now = now.saturating_add(step_ns);
        let step = login.refresh(now, None);
        match step.present {
            Present::Nothing => {}
            Present::Region(_) | Present::Whole => saw_present = true,
        }
        if login.park_timeout(now, None) == FOREVER {
            break;
        }
        assert!(now <= span_ns.saturating_mul(2), "fade did not settle");
    }
    assert!(saw_present, "at least one refresh presented fade damage");
    assert_eq!(login.park_timeout(now, None), FOREVER);

    // A refresh that finds nothing to do still presents nothing.
    let quiet = login.refresh(now.saturating_add(1), None);
    assert_eq!(quiet.present, Present::Nothing);
}
