//! Shared ANSI / VT / xterm escape and attribute vocabulary (`lib/vt`).
//!
//! Every real terminal, terminal multiplexer, and remote Linux host speaks the
//! ANSI / xterm escape vocabulary, so RustOS speaks it too — and it speaks it
//! from **one** definition (`AGENTS.md` §2.2). This crate is that definition:
//! the control set, the SGR attributes and colour models, and the
//! screen-control operations, expressed once as the [`Op`] vocabulary and
//! shared by the *consumer* (the terminal emulator) and the *emitter* (the
//! curses renderer).
//!
//! # Emitter and parser agree by construction
//!
//! [`encode`] turns an [`Op`] into bytes; [`Parser`] turns bytes back into
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
//! out-of-bounds access (`AGENTS.md` §2.9), saturating oversized parameters and
//! dropping unrecognised or malformed sequences rather than corrupting state.
//! There is no `unwrap` / `expect` / `panic!` in this crate, and nothing here
//! ever writes to fd 3 (`stdinfo` is reserved, §20).
//!
//! ```
//! use rustos_vt::{encode, Op, Parser, Sgr, Color};
//!
//! // Emit one operation, then parse the bytes back: identity holds.
//! let op = Op::Sgr(Sgr::Foreground(Color::Rgb(0x30, 0x70, 0xf0)));
//! let bytes = encode(&op);
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

extern crate alloc;

pub mod attr;
pub mod cell;
pub mod color;
pub mod control;
pub mod emit;
pub mod op;
pub mod parse;

#[cfg(test)]
mod tests;

pub use attr::{Attributes, Sgr};
pub use cell::Cell;
pub use color::{BasicColor, Color};
pub use emit::{encode, encode_all, encode_into};
pub use op::{EraseMode, Op};
pub use parse::Parser;
