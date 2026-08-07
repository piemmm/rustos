//! Shared ANSI / VT / xterm escape and attribute vocabulary (`lib/vt`).
//!
//! Every real terminal, terminal multiplexer, and remote Linux host speaks the
//! ANSI / xterm escape vocabulary, so TAIRiX speaks it too — and it speaks it
//! from **one** definition. This crate is that definition:
//! the control set, the SGR attributes and colour models, and the
//! screen-control operations, expressed once as the [`Op`] vocabulary and
//! shared by the *consumer* (the terminal emulator) and the *emitter* (the
//! curses renderer).
//!
//! # Emitter and parser agree by construction
//!
//! [`encode_into`] turns an [`Op`] into bytes; [`Parser`] turns bytes back into
//! [`Op`] events. Both are built over the same tables, so they provably agree:
//! every operation the emitter writes parses back to the identical operation
//! (the `emit_parse_round_trip*` tests assert exactly this). There is no second
//! escape-sequence definition anywhere in the tree.
//!
//! # Untrusted input
//!
//! A terminal consumes bytes it did not produce — local shell output, and (in
//! the remote stages of `plans/CURSES.md`) the output of a foreign host. The
//! [`Parser`] is therefore total: it consumes any byte stream without panic or
//! out-of-bounds access, saturating oversized parameters and
//! dropping unrecognised or malformed sequences rather than corrupting state.
//! There is no `unwrap` / `expect` / `panic!` in this crate, and nothing here
//! ever writes to fd 3 (`stdinfo` is reserved).
//!
//! ```
//! use tairix_vt::{encode_into, Op, Parser, Sgr, Color};
//!
//! // Emit one operation into a byte sink, then parse the bytes back: identity
//! // holds. The emitter writes into any `Extend<u8>` (here a `Vec`), so it
//! // needs no allocator of its own.
//! let op = Op::Sgr(Sgr::Foreground(Color::Rgb(0x30, 0x70, 0xf0)));
//! let mut bytes = alloc::vec::Vec::new();
//! encode_into(&op, &mut bytes);
//!
//! let mut parser = Parser::new();
//! let mut seen = alloc::vec::Vec::new();
//! parser.feed(&bytes, |parsed| seen.push(parsed));
//! assert_eq!(seen, alloc::vec![op]);
//! # extern crate alloc;
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

// The vocabulary is allocation-free: the `Parser` uses fixed inline buffers,
// `Op` owns no heap (the OSC title is a bounded inline `Title`), and the
// emitter writes into a caller-provided `Extend<u8>` sink. `alloc` is pulled in
// only for the unit tests, which build `Vec`s to assert against.
#[cfg(test)]
extern crate alloc;

pub mod attr;
pub mod cell;
pub mod color;
pub mod conformance;
pub mod control;
pub mod emit;
pub mod key;
pub mod line;
pub mod mouse;
pub mod op;
pub mod parse;
pub mod scheme;
pub mod secret;
pub mod width;

#[cfg(test)]
mod tests;

pub use attr::{Attributes, Sgr};
pub use cell::Cell;
pub use color::{BasicColor, Color};
pub use emit::{encode_all_into, encode_into};
pub use key::Key;
pub use line::{EraseSeq, LineEditor, LineFeed};
pub use mouse::{MouseButton, MouseMode, MouseReport};
pub use op::{EraseMode, Op, Title, MAX_TITLE};
pub use parse::Parser;
pub use scheme::{Role, Style};
pub use secret::{SecretIndicator, SecretInput, SECRET_TICK_NS};
pub use width::{char_width, is_wide, str_width, truncate_to_width, CONTINUATION};
