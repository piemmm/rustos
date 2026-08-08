//! Host tests for the window's relationship to the directory it shows: the
//! title it carries and what leaving the directory means.

use alloc::string::ToString;
use alloc::vec;

use tairix_abi::window_ipc::WindowTitle;
use tairix_browse::Browser;

use super::{leave_directory, location_title, retitle, Leave};
use crate::test_fs::{browser, FakeFs};

/// A browser showing `/Users/ann/Documents`, three levels down the fixture.
fn deep() -> Browser<FakeFs> {
    let mut browser = browser();
    browser
        .navigate_to(vec![
            "Users".to_string(),
            "ann".to_string(),
            "Documents".to_string(),
        ])
        .expect("the fixture lists");
    browser
}

#[test]
fn leaving_a_subdirectory_climbs_to_its_parent() {
    let mut browser = deep();

    assert_eq!(leave_directory(&mut browser), Leave::Climbed);
    assert_eq!(browser.path(), "/Users/ann");
    assert_eq!(leave_directory(&mut browser), Leave::Climbed);
    assert_eq!(browser.path(), "/Users");
}

/// The gesture closes the window only where there is nothing left to leave.
#[test]
fn leaving_the_root_closes_the_window() {
    let mut browser = browser();
    assert!(browser.is_root());

    assert_eq!(leave_directory(&mut browser), Leave::Closed);
    assert!(browser.is_root(), "a closing gesture navigates nowhere");
}

/// An unreadable ancestor must not destroy the window: the climb is refused
/// and the browser stays exactly where it was.
#[test]
fn a_parent_that_cannot_be_listed_is_refused_not_treated_as_the_top() {
    let mut browser = browser();
    browser
        .navigate_to(vec!["Storage".to_string(), "Backup".to_string()])
        .expect("the fixture lists the volume");

    assert_eq!(
        leave_directory(&mut browser),
        Leave::Refused("could not open /Storage".to_string()),
        "the user is told which place was refused"
    );
    assert_eq!(
        browser.path(),
        "/Storage/Backup",
        "a refused climb leaves the listing where it was"
    );
}

#[test]
fn the_title_is_the_location_and_only_changes_when_it_moves() {
    let mut browser = browser();

    assert_eq!(retitle(&browser, ""), Some("/".to_string()));
    assert_eq!(retitle(&browser, "/"), None, "a repaint that did not move");

    browser
        .navigate_to(vec!["Users".to_string(), "ann".to_string()])
        .expect("the fixture lists");
    assert_eq!(retitle(&browser, "/"), Some("/Users/ann".to_string()));
    assert_eq!(retitle(&browser, "/Users/ann"), None);
}

/// Whatever the location, the text handed to the window channel is text the
/// channel accepts: the window is retitled, never refused. (That a location
/// too long for the field is shortened rather than rejected is the shared
/// spelling's own contract, tested in the engine.)
#[test]
fn every_location_yields_a_title_the_channel_accepts() {
    let mut browser = deep();
    loop {
        let title = location_title(&browser);
        assert!(
            WindowTitle::new(&title).is_ok(),
            "the channel must accept {title:?}"
        );
        if leave_directory(&mut browser) != Leave::Climbed {
            break;
        }
    }
}
