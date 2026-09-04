//! Unit tests for the wallpaper desk's policy.
//!
//! The handshake, the staleness rule, the deduplication that stops one picture
//! being prepared twice at once, and the colour-only shortcut — all with no
//! thread, no lock, and no sandbox.

use super::*;

use alloc::string::String;

use tairix_raster::Color;
use tairix_wallpaper::WallpaperPath;

fn image(path: &str) -> WallpaperSource {
    WallpaperSource {
        choice: WallpaperChoice::Image(WallpaperPath::new(path).expect("a valid wallpaper path")),
        fit: WallpaperFit::default(),
        width: 800,
        height: 600,
    }
}

fn colour_only() -> WallpaperSource {
    WallpaperSource {
        choice: WallpaperChoice::None,
        fit: WallpaperFit::default(),
        width: 800,
        height: 600,
    }
}

fn screen(source: &WallpaperSource) -> Surface {
    Surface::filled(
        source.width,
        source.height,
        Color::rgba(1, 2, 3, 255).premultiply(),
    )
    .expect("a screen-sized surface")
}

#[test]
fn a_colour_only_choice_never_reaches_a_preparer() {
    let mut desk = WallpaperDesk::new();
    assert!(matches!(
        desk.take(&colour_only()),
        Prepared::Ready {
            surface: None,
            refusal: None
        }
    ));
    assert!(!desk.has_work(), "a colour-only backdrop asked for work");
    assert!(desk.next_job().is_none());
}

#[test]
fn a_first_ask_records_the_request_and_answers_pending() {
    let mut desk = WallpaperDesk::new();
    let wanted = image("/System/Graphics/Wallpapers/Space/low-orbit.jpg");
    assert!(matches!(desk.take(&wanted), Prepared::Pending));
    assert!(desk.has_work());
    assert_eq!(desk.next_job(), Some(wanted));
}

#[test]
fn asking_again_while_a_preparer_holds_it_starts_no_second_preparation() {
    let mut desk = WallpaperDesk::new();
    let wanted = image("/a.png");
    assert!(matches!(desk.take(&wanted), Prepared::Pending));
    assert!(desk.next_job().is_some());
    assert!(matches!(desk.take(&wanted), Prepared::Pending));
    assert!(
        desk.next_job().is_none(),
        "a preparation in progress was handed out twice"
    );
}

#[test]
fn a_prepared_surface_is_served_once_and_installed() {
    let mut desk = WallpaperDesk::new();
    let wanted = image("/a.png");
    assert!(matches!(desk.take(&wanted), Prepared::Pending));
    let job = desk.next_job().expect("a job");
    let painted = screen(&job);
    let pixels = painted.pixels().to_vec();
    assert!(desk.deliver(job, Ok(painted)));

    let Prepared::Ready {
        surface: Some(surface),
        refusal: None,
    } = desk.take(&wanted)
    else {
        panic!("the prepared surface was not served");
    };
    assert_eq!(surface.pixels(), pixels.as_slice());
    // Consumed: the desktop has installed it, so asking again means it wants
    // the picture prepared afresh.
    assert!(matches!(desk.take(&wanted), Prepared::Pending));
}

#[test]
fn a_refusal_is_served_as_the_backdrop_colour() {
    let mut desk = WallpaperDesk::new();
    let wanted = image("/missing.png");
    assert!(matches!(desk.take(&wanted), Prepared::Pending));
    let job = desk.next_job().expect("a job");
    assert!(desk.deliver(job, Err(String::from("unreadable"))));
    let Prepared::Ready {
        surface: None,
        refusal: Some(reason),
    } = desk.take(&wanted)
    else {
        panic!("a refusal must be served with its reason");
    };
    assert_eq!(reason, "unreadable");
}

#[test]
fn a_surface_prepared_for_a_screen_the_desktop_left_is_never_painted() {
    let mut desk = WallpaperDesk::new();
    let small = image("/a.png");
    let mut large = small.clone();
    large.width = 1920;
    large.height = 1080;

    assert!(matches!(desk.take(&small), Prepared::Pending));
    let job = desk.next_job().expect("a job");
    // The screen mode changes while the picture is being fitted to the old one.
    assert!(matches!(desk.take(&large), Prepared::Pending));
    assert!(
        !desk.deliver(job.clone(), Ok(screen(&job))),
        "an abandoned preparation must report that nobody wants it"
    );
    assert!(matches!(desk.take(&large), Prepared::Pending));
    assert_eq!(
        desk.next_job(),
        Some(large),
        "the new screen was not queued"
    );
}

#[test]
fn switching_to_a_colour_only_backdrop_discards_a_prepared_picture() {
    let mut desk = WallpaperDesk::new();
    let wanted = image("/a.png");
    assert!(matches!(desk.take(&wanted), Prepared::Pending));
    let job = desk.next_job().expect("a job");
    assert!(desk.deliver(job.clone(), Ok(screen(&job))));

    // The user turns the wallpaper off before the answer is collected: the
    // prepared pixels must not survive to be painted later.
    assert!(matches!(
        desk.take(&colour_only()),
        Prepared::Ready {
            surface: None,
            refusal: None
        }
    ));
    assert!(matches!(desk.take(&wanted), Prepared::Pending));
}

#[test]
fn stopping_hands_out_no_more_work() {
    let mut desk = WallpaperDesk::new();
    assert!(matches!(desk.take(&image("/a.png")), Prepared::Pending));
    assert!(desk.has_work());
    desk.stop();
    assert!(desk.stopping());
    assert!(!desk.has_work());
    assert!(desk.next_job().is_none());
}

#[test]
fn the_wanted_source_is_derived_from_the_settings_and_the_screen() {
    let settings = PinboardSettings::default();
    let wanted = WallpaperSource::wanted(&settings, Rect::new(0, 0, 1280, 720));
    assert_eq!(wanted.width, 1280);
    assert_eq!(wanted.height, 720);
    assert_eq!(wanted.fit, settings.fit);
    assert_eq!(wanted.choice, settings.wallpaper);
    // The shipped default is an image, so a fresh account really does prepare
    // one — this is the path a first login takes.
    assert!(wanted.image_path().is_some());
}

/// The defect the listing desk had in the same shape: a preparer that hands
/// itself the same picture for ever.
///
/// A hand-out clones the source rather than taking it, so the request outlived
/// its own answer and the desk became workable again the instant it was
/// answered. Here each turn round that loop is a whole-screen read, decode and
/// resample, so it is the more expensive of the two.
#[test]
fn an_answered_preparation_is_never_handed_out_again() {
    let mut desk = WallpaperDesk::new();
    let wanted = image("/a.png");
    assert!(matches!(desk.take(&wanted), Prepared::Pending));
    let job = desk.next_job().expect("a job");
    let painted = screen(&job);
    assert!(desk.deliver(job, Ok(painted)));

    assert!(
        !desk.has_work(),
        "the answered preparation must not make the desk workable again"
    );
    assert!(
        desk.next_job().is_none(),
        "a preparer looking for work after answering must find none and park"
    );

    // And the surface is still there to be installed.
    assert!(matches!(
        desk.take(&wanted),
        Prepared::Ready {
            surface: Some(_),
            refusal: None
        }
    ));
}

/// A preparation the desktop has moved on from leaves its *newer* request
/// standing, so the abandoned picture costs one wasted decode and not a stall.
#[test]
fn a_stale_preparation_does_not_clear_the_newer_request() {
    let mut desk = WallpaperDesk::new();
    let first = image("/a.png");
    let second = image("/b.png");
    assert!(matches!(desk.take(&first), Prepared::Pending));
    let job = desk.next_job().expect("a job");
    let painted = screen(&job);

    assert!(matches!(desk.take(&second), Prepared::Pending));
    assert!(
        !desk.deliver(job, Ok(painted)),
        "an abandoned preparation owes no wake"
    );

    assert!(desk.has_work(), "the newer request is still owed a decode");
    assert_eq!(desk.next_job(), Some(second));
}
