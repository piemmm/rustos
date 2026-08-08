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
    let Present::Region(region) = step.present else {
        panic!("a pointer move repaints the pointer: {step:?}");
    };
    let expected = cursor_rect(&image, corner.0, corner.1)
        .union(&cursor_rect(&image, along.0, along.1))
        .intersection(&login.screen());
    assert_eq!(region.x, u32::try_from(expected.left()).expect("on screen"));
    assert_eq!(region.y, u32::try_from(expected.top()).expect("on screen"));
    assert_eq!(region.width_px, expected.width);
    assert_eq!(region.height_px, expected.height);
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
