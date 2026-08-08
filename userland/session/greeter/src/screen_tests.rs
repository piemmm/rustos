use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
use tairix_abi::input::{PointerButtonCode, PointerInput};
use tairix_abi::session_ipc::{SessionRequest, SessionVerdict};
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::Errno;
use tairix_cursor::CursorImage;
use tairix_geometry::{Point, Rect, Scale};
use tairix_greeter::{AccountTile, Verdict};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey};
use tairix_raster::{Pixel, Surface};
use tairix_theme::Theme;

use super::LoginScreen;
use crate::accounts::SessionTransport;
use crate::cursor::pointer_image;
use crate::frame::{Present, Scanout};
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
    LoginScreen::new(
        Scanout::new(mode()).expect("a valid mode"),
        Theme::dark(),
        Scale::ONE,
        "tairix".to_string(),
        accounts,
        authority,
    )
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

/// The four scan-out bytes of the frame at `(x, y)`.
fn pixel_at(frame: &[u8], x: u32, y: u32) -> &[u8] {
    let stride = usize::try_from(mode().stride_bytes).expect("a small stride");
    let at = usize::try_from(y).expect("a small screen") * stride
        + usize::try_from(x).expect("a small screen") * 4;
    &frame[at..at + 4]
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
        login.park_timeout(0, None),
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
        1_000_000_000,
        "the lockout ticks before the minute turns"
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
    Rect::new(
        i32::try_from(region.x).expect("on screen"),
        i32::try_from(region.y).expect("on screen"),
        region.width_px,
        region.height_px,
    )
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
