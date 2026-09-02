//! Host tests for the running long operation's modal input routing.
//!
//! The panel is drawn in what the places rail leaves, so the tests locate the
//! drawn Cancel button through the renderer's own hit-test — never a second
//! copy of its geometry — and press it exactly where the user would.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::input::{KeyInput, KeyValue, Modifiers, NamedKeyCode, PointerButtonCode};
use tairix_abi::window_ipc::{PointerAction, WindowEvent};
use tairix_browse::render::{content_area, progress_cancel_at, progress_dialog_rect, sidebar_view};
use tairix_browse::{Places, ToolbarBand, Volume};
use tairix_geometry::{Point, Rect, Scale};
use tairix_theme::Theme;

use super::{operation_control, OperationControl};

/// The window the operation's panel is laid out in for these tests.
const WINDOW: Rect = Rect::new(0, 0, 480, 480);

/// The chrome these tests measure against: the command band shown.
const BAND: ToolbarBand = ToolbarBand::Shown;

/// The window this app is given, addressed by every synthesised event.
const WINDOW_ID: u64 = 7;

/// The rail the tests draw beside the panel: the user's places plus one
/// mounted volume.
fn places() -> Places {
    let home: Vec<String> = vec!["Users".to_string(), "ann".to_string()];
    Places::new(
        &home,
        &[Volume {
            label: "Backup".to_string(),
            target: "/Storage/Backup".to_string(),
            medium: Some(BlkDeviceClass::Rotational),
        }],
    )
}

/// The rail's drawn width in this test's window.
fn rail_width(places: &Places) -> u32 {
    sidebar_view(WINDOW, Scale::ONE, &Theme::dark(), Some(places), BAND)
        .expect("the rail has rows")
        .width()
}

/// The trailing-bottom pixel of the Cancel button the progress panel draws
/// when centred in `area`.
///
/// Found by asking the renderer's own hit-test where it put the button, so the
/// test carries no copy of the panel's geometry. The trailing corner is the
/// telling pixel: a panel centred in a *narrower* area sits further right, so
/// its trailing edge is exactly what a hit-test against the whole window
/// misses.
fn cancel_corner(area: Rect, theme: &Theme) -> Point {
    let panel = progress_dialog_rect(area, Scale::ONE, theme);
    let right = panel.left() + i32::try_from(panel.width).unwrap_or(0);
    let bottom = panel.top() + i32::try_from(panel.height).unwrap_or(0);
    for y in (panel.top()..bottom).rev() {
        for x in (panel.left()..right).rev() {
            let point = Point::new(x, y);
            if progress_cancel_at(area, Scale::ONE, theme, point) {
                return point;
            }
        }
    }
    panic!("the progress panel draws a Cancel button in a 480px window")
}

/// A primary press at `point`.
fn press(point: Point) -> WindowEvent {
    WindowEvent::Pointer {
        window_id: WINDOW_ID,
        x: u32::try_from(point.x).unwrap_or(0),
        y: u32::try_from(point.y).unwrap_or(0),
        action: PointerAction::Pressed(PointerButtonCode::Primary),
        modifiers: Modifiers::default(),
    }
}

/// A pointer motion to `point`.
fn motion(point: Point) -> WindowEvent {
    WindowEvent::Pointer {
        window_id: WINDOW_ID,
        x: u32::try_from(point.x).unwrap_or(0),
        y: u32::try_from(point.y).unwrap_or(0),
        action: PointerAction::Moved,
        modifiers: Modifiers::default(),
    }
}

/// A named key press with no modifiers held.
fn named(code: NamedKeyCode) -> WindowEvent {
    WindowEvent::Key {
        window_id: WINDOW_ID,
        key: KeyInput::Pressed {
            key: KeyValue::Named(code),
            modifiers: Modifiers::default(),
        },
    }
}

/// Route `event` to the running operation with this test's window and rail.
fn route(places: &Places, event: &WindowEvent) -> OperationControl {
    operation_control(
        Some(places),
        Scale::ONE,
        &Theme::dark(),
        WINDOW,
        BAND,
        event,
    )
}

#[test]
fn a_press_on_the_drawn_cancel_button_cancels_the_operation() {
    let places = places();
    let theme = Theme::dark();
    let panel_area = content_area(WINDOW, Scale::ONE, &theme, Some(&places), BAND);
    let drawn = cancel_corner(panel_area, &theme);

    assert_eq!(route(&places, &press(drawn)), OperationControl::Cancel);
    // The button the whole window would place is further left, so this very
    // press falls outside it: resolving the press against the window rather
    // than the area the panel is painted in swallows the user's cancel.
    assert!(!progress_cancel_at(WINDOW, Scale::ONE, &theme, drawn));
    // A motion over the button is not a press, and changes nothing.
    assert_eq!(route(&places, &motion(drawn)), OperationControl::Ignore);
}

#[test]
fn a_press_over_the_rail_beside_the_button_leaves_the_operation_running() {
    let places = places();
    let theme = Theme::dark();
    let panel_area = content_area(WINDOW, Scale::ONE, &theme, Some(&places), BAND);
    let drawn = cancel_corner(panel_area, &theme);
    let rail = rail_width(&places);
    assert!(rail > 0, "this window draws a rail");

    // Level with the drawn button, but over the rail the panel is inset past.
    let on_rail = Point::new(i32::try_from(rail / 2).unwrap_or(0), drawn.y);
    assert_eq!(route(&places, &press(on_rail)), OperationControl::Ignore);
    // So is a press inside the panel but off the button (the panel is modal:
    // nothing behind it acts).
    let panel = progress_dialog_rect(panel_area, Scale::ONE, &theme);
    let top_left = Point::new(panel.left(), panel.top());
    assert_eq!(route(&places, &press(top_left)), OperationControl::Ignore);
}

#[test]
fn escape_cancels_a_close_request_closes_and_nothing_else_reaches_the_run() {
    let places = places();

    assert_eq!(
        route(&places, &named(NamedKeyCode::Escape)),
        OperationControl::Cancel
    );
    assert_eq!(
        route(&places, &WindowEvent::CloseRequested { window_id: 7 }),
        OperationControl::Close
    );
    // Every other key is swallowed, so nothing navigates behind the panel.
    assert_eq!(
        route(&places, &named(NamedKeyCode::Enter)),
        OperationControl::Ignore
    );
    assert_eq!(
        route(
            &places,
            &WindowEvent::Focus {
                window_id: WINDOW_ID,
                focused: true,
            }
        ),
        OperationControl::Ignore
    );
}

#[test]
fn a_window_too_small_for_the_panel_resolves_no_cancel() {
    let places = places();
    let theme = Theme::dark();
    let tiny = Rect::new(0, 0, 1, 1);

    // Total on a degenerate window: no button is placed, so no press cancels,
    // and nothing panics.
    for point in [Point::new(0, 0), Point::new(400, 400)] {
        assert_eq!(
            operation_control(Some(&places), Scale::ONE, &theme, tiny, BAND, &press(point)),
            OperationControl::Ignore
        );
    }
    // Escape still stops the run whatever the window's size.
    assert_eq!(
        operation_control(
            Some(&places),
            Scale::ONE,
            &theme,
            tiny,
            BAND,
            &named(NamedKeyCode::Escape)
        ),
        OperationControl::Cancel
    );
}
