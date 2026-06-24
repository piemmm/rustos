//! The crate's fallible-operation error type.
//!
//! Every operation that can be asked to do something impossible — address a
//! cell outside a window, allocate a colour pair past the table, read or write
//! a closed tty — returns a [`CursesError`] rather than panicking. There is no `unwrap` / `expect` / `panic!` anywhere in
//! the crate.

/// Why a curses operation could not be completed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CursesError {
    /// A cell position fell outside the target window or screen.
    OutOfBounds,
    /// A requested size was zero in a dimension that must be positive.
    EmptySize,
    /// A colour-pair id was zero (reserved for the default pair) or past the
    /// table's capacity.
    BadColorPair,
    /// The underlying tty channel could not be read or written (for example
    /// because the far end has closed).
    Io,
}

/// The crate's `Result`, fixed to [`CursesError`].
pub type Result<T> = core::result::Result<T, CursesError>;
