//! The running long operation's modal input routing: what a polled event means
//! while a recursive delete, a move to Trash, or a paste is running.
//!
//! # Why this is its own module
//!
//! The `Run` binary around it is a freestanding program — it only exists when
//! the crate is built for a bare-metal target — so nothing inside it can be
//! reached by a host test. Which press cancels a running operation is worth
//! testing: it is a pure function of the event, the window, and the drawn rail,
//! so it lives here, compiles on the host, and is covered by the tests beside
//! it.
//!
//! # No I/O, and no authority
//!
//! Nothing here touches the operation, the filesystem, or the window. It
//! classifies one event and the program acts on the answer.

use tairix_abi::input::{KeyInput, KeyValue, NamedKeyCode};
use tairix_abi::window_ipc::WindowEvent;
use tairix_browse::render::{content_area, progress_cancel_at};
use tairix_browse::Places;
use tairix_geometry::{Rect, Scale};
use tairix_theme::Theme;

use crate::sidebar::press_point;

/// The disposition of a poll taken while a long operation runs.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OperationControl {
    /// The user asked to cancel (Escape, or a click on the Cancel button).
    Cancel,
    /// The desktop asked the window to close.
    Close,
    /// Nothing that affects the running operation.
    Ignore,
}

/// Classify a polled `event` while a long operation runs: a close request, a
/// cancel (Escape, or a primary press on the progress panel's Cancel button),
/// or nothing that affects the run. A press anywhere but the Cancel button is
/// ignored, so nothing navigates behind the modal panel (fail closed).
///
/// `window` is the whole window and `places` the drawn rail. The panel is
/// painted in what the rail leaves, so the press is resolved against that
/// same shared inset: testing the window itself misses the drawn button by
/// the rail's width. Taking the two values the frame is drawn from — and
/// insetting here rather than at the call site — is what keeps the painted
/// button and the press that must match it from ever disagreeing.
#[must_use]
pub fn operation_control(
    places: &Places,
    scale: Scale,
    theme: &Theme,
    window: Rect,
    event: &WindowEvent,
) -> OperationControl {
    let panel = content_area(window, scale, theme, Some(places));
    match event {
        WindowEvent::CloseRequested { .. } => OperationControl::Close,
        WindowEvent::Key {
            key:
                KeyInput::Pressed {
                    key: KeyValue::Named(NamedKeyCode::Escape),
                    ..
                },
            ..
        } => OperationControl::Cancel,
        WindowEvent::Pointer { x, y, action, .. } => match press_point(*action, *x, *y) {
            Some(point) if progress_cancel_at(panel, scale, theme, point) => {
                OperationControl::Cancel
            }
            _ => OperationControl::Ignore,
        },
        // Nothing else reaches the running operation. A redraw request needs no
        // arm of its own: the modal loop that polls this re-presents the
        // progress panel in full on every pass, so the released pixels are back
        // on the next step — and a released frame region is re-attached by the
        // same present. A desktop change is already adopted by the caller
        // before this is reached. The alternate close means "leave this folder",
        // which would move the listing the running operation is walking, so it
        // is ignored while the panel is up rather than deferred. An icon-bar
        // click or menu row names the whole application, and acting on it
        // would take the running operation somewhere the user cannot see, so
        // it too waits for the panel to go. The rest is input that must not
        // navigate behind the modal panel.
        WindowEvent::AlternateCloseRequested { .. }
        | WindowEvent::AppBarDefault
        | WindowEvent::AppBarMenu { .. }
        | WindowEvent::Key { .. }
        | WindowEvent::Focus { .. }
        | WindowEvent::Minimized { .. }
        | WindowEvent::RedrawRequested { .. }
        | WindowEvent::ContentReleased { .. }
        | WindowEvent::Resized { .. }
        | WindowEvent::Scrolled { .. }
        | WindowEvent::FilePicked { .. }
        | WindowEvent::PickCancelled { .. }
        | WindowEvent::DesktopChanged { .. } => OperationControl::Ignore,
    }
}

#[cfg(test)]
#[path = "operation_tests.rs"]
mod tests;
