//! The window the game is looked at through: which of the three size
//! states it is in, whether it is on screen, and whether it still has a
//! seat.
//!
//! The client *asks* for a size state and the compositor *answers* with
//! one. Nothing here assumes a request took effect: the state is adopted
//! from the resize event the compositor sends, which carries the state
//! alongside the extent precisely so the two cannot be believed
//! separately. A client that set its own state optimistically would lay
//! out edge-to-edge in a window that had stayed where it was.

use tairix_abi::window_ipc::WindowSizeState;

/// What the window is doing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Shell {
    state: WindowSizeState,
    restore_to: WindowSizeState,
    extent: Option<(u32, u32)>,
    focused: bool,
    seated: bool,
    shown: bool,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    /// A window not yet opened.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: WindowSizeState::Restored,
            restore_to: WindowSizeState::Restored,
            extent: None,
            focused: false,
            seated: true,
            shown: true,
        }
    }

    /// The state the compositor last said the window was in.
    #[must_use]
    pub const fn state(&self) -> WindowSizeState {
        self.state
    }

    /// The window's extent, or `None` before the first resize.
    #[must_use]
    pub const fn extent(&self) -> Option<(u32, u32)> {
        self.extent
    }

    /// Whether the window holds keyboard focus.
    #[must_use]
    pub const fn focused(&self) -> bool {
        self.focused
    }

    /// Whether the session is showing this client's seat.
    #[must_use]
    pub const fn seated(&self) -> bool {
        self.seated
    }

    /// Whether the window is on screen rather than minimized.
    #[must_use]
    pub const fn shown(&self) -> bool {
        self.shown
    }

    /// Whether the game should be running its clock and drawing frames.
    ///
    /// A window without a seat, or minimized off the screen, is a picture
    /// nobody is looking at: drawing it would spend a core for nothing, and
    /// simulating for it would advance past the moment the player left.
    #[must_use]
    pub const fn running(&self) -> bool {
        self.seated && self.shown
    }

    /// Record the window being minimized: off the screen until the
    /// compositor gives it focus or a size again.
    pub fn minimized(&mut self) {
        self.shown = false;
    }

    /// Adopt the extent and state the compositor answered with.
    ///
    /// Returns whether anything moved, so a caller repaints on a change
    /// and not on the echo of its own request.
    pub fn resized(&mut self, width: u32, height: u32, state: WindowSizeState) -> bool {
        let moved = self.extent != Some((width, height)) || self.state != state;
        // Where a *later* exit from fullscreen should land. Captured on
        // the way in rather than on the way out, because by then the
        // state it would be read from is the one being left.
        if !state.is_fullscreen() {
            self.restore_to = state;
        }
        self.extent = Some((width, height));
        self.state = state;
        self.shown = true;
        moved
    }

    /// Record a change of keyboard focus.
    ///
    /// Returns whether the client should let go of everything it was
    /// holding, which losing focus always means.
    pub fn focus(&mut self, focused: bool) -> bool {
        let lost = self.focused && !focused;
        self.focused = focused;
        if focused {
            self.shown = true;
        }
        lost
    }

    /// Record the seat going away or coming back.
    ///
    /// Returns whether the running state changed, which is the edge the
    /// caller pauses and resumes its clock on.
    pub fn seat(&mut self, seated: bool) -> bool {
        let moved = self.seated != seated;
        self.seated = seated;
        if !seated {
            self.focused = false;
        }
        moved
    }

    /// The state to ask for when the player asks for fullscreen and back.
    ///
    /// Leaving fullscreen goes to the state the window was in before it,
    /// so a maximised window that went fullscreen comes back maximised
    /// rather than to an arbitrary restored size.
    #[must_use]
    pub const fn fullscreen_toggle(&self) -> WindowSizeState {
        if self.state.is_fullscreen() {
            self.restore_to
        } else {
            WindowSizeState::Fullscreen
        }
    }

    /// The request to send for `want`, or `None` when the window is
    /// already in that state.
    ///
    /// Asking for the state the window is already in would cost a round
    /// trip and a resize event to be told nothing changed.
    #[must_use]
    pub fn request(&self, want: WindowSizeState) -> Option<WindowSizeState> {
        (self.state != want).then_some(want)
    }
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
