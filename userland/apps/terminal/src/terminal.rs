//! The terminal model: the screen [`Grid`] glued to a [`ShellSource`].
//!
//! [`Terminal`] owns the screen [`Grid`], the [`Parser`] that drives it, and
//! the [`ShellSource`] channel to the hosted shell. It is the analogue of the
//! file browser's `Browser`: the navigation/parsing logic lives here and the
//! outside world is reached only through the injected seam, so the whole model
//! is testable without a kernel.
//!
//! Two operations cross the seam:
//!
//! * [`Terminal::pump`] reads whatever the shell has produced and feeds it to
//!   the grid, returning how many bytes were applied.
//! * [`Terminal::send`] / [`Terminal::send_str`] forward the user's input to
//!   the shell.
//!
//! The terminal never echoes input to the screen itself: echo (and all line
//! editing and job control) is the shell's responsibility, exactly as on a
//! real tty. A failing seam call surfaces the boundary [`Errno`] and leaves
//! the screen unchanged.

use tairix_abi::Errno;

use crate::grid::Grid;
use crate::parser::Parser;
use crate::shell::ShellSource;

/// A character-cell terminal hosting a shell over a [`ShellSource`] seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Terminal<S: ShellSource> {
    grid: Grid,
    parser: Parser,
    shell: S,
}

impl<S: ShellSource> Terminal<S> {
    /// Create a `cols`×`rows` terminal over `shell`, with a blank screen and
    /// the cursor at the home position.
    ///
    /// Returns `None` for a screen size [`Grid::new`] rejects, so an unusable
    /// geometry fails closed rather than allocating something degenerate.
    #[must_use]
    pub fn new(cols: u16, rows: u16, shell: S) -> Option<Self> {
        Some(Self {
            grid: Grid::new(cols, rows)?,
            parser: Parser::new(),
            shell,
        })
    }

    /// The screen grid, for rendering and inspection.
    #[must_use]
    pub const fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Reshape the screen to `cols`×`rows` (a window resize), preserving the
    /// top-left overlap of the current contents. Returns `false`, changing
    /// nothing, for a zero/oversized dimension or a no-op resize (fail closed).
    ///
    /// The shell learns the new geometry through the pty window size the caller
    /// sets alongside this (`pty_set_size`), so its prompt and any full-screen
    /// program re-lay-out on their next output; this only reshapes the local
    /// screen model so the next render fills the resized window.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        self.grid.resize(cols, rows)
    }

    /// Read the bytes the shell has produced and apply them to the screen,
    /// returning how many bytes were applied.
    ///
    /// A read of no new bytes applies nothing and returns `0`; it is not an
    /// error.
    ///
    /// # Errors
    ///
    /// Propagates the [`Errno`] from the underlying [`ShellSource::read`],
    /// leaving the screen unchanged.
    pub fn pump(&mut self) -> Result<usize, Errno> {
        let bytes = self.shell.read()?;
        self.parser.feed(&mut self.grid, &bytes);
        Ok(bytes.len())
    }

    /// Forward raw input `bytes` to the shell.
    ///
    /// # Errors
    ///
    /// Propagates the [`Errno`] from the underlying [`ShellSource::write`].
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        self.shell.write(bytes)
    }

    /// Forward `text` to the shell as UTF-8 bytes.
    ///
    /// # Errors
    ///
    /// Propagates the [`Errno`] from the underlying [`ShellSource::write`].
    pub fn send_str(&mut self, text: &str) -> Result<(), Errno> {
        self.shell.write(text.as_bytes())
    }

    /// Feed raw `bytes` directly into the screen model without touching the
    /// shell.
    ///
    /// This is the in-process half of [`pump`](Self::pump): the binary uses
    /// `pump`, while a caller that already holds the bytes (or a test) can
    /// apply them directly.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.feed(&mut self.grid, bytes);
    }

    /// Blank the screen and return the cursor home, leaving the shell
    /// untouched.
    ///
    /// The *Clear screen* command: it clears what the emulator is showing, in
    /// the same way a `clear` would, without writing anything to the shell —
    /// so a half-typed command line is not disturbed and no program sees
    /// input it did not receive from the user.
    pub fn clear(&mut self) {
        self.grid.clear();
    }
}
