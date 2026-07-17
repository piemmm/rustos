//! The compiled-in terminal capability database (`lib/termcap`).
//!
//! A terminal's `TERM` value names a profile of what that terminal can do —
//! how deep its colour is, whether it can address the cursor, switch to an
//! alternate screen, report the mouse, and so on. On a POSIX system this lives
//! in a terminfo / termcap database read from `/usr/share` or `/etc`. TAIRiX
//! has no such paths, so the database is **compiled in**:
//! a closed, versioned [`TermType`] set and a constant
//! [`Capabilities`] record per terminal.
//!
//! # One vocabulary
//!
//! Every escape sequence a capability record describes is a [`tairix_vt::Op`]
//! — the one shared vocabulary. This crate never defines a
//! second escape-sequence table: the output capabilities are expressed as the
//! `Op`s the terminal accepts, the recognised colours are the
//! [`tairix_vt::Color`] models the terminal renders, and arrow-key input is the
//! `Op` the terminal's bytes parse back to through [`tairix_vt::Parser`]. The
//! [`Capabilities::referenced_ops`] method exposes exactly that set, and the
//! `no_record_emits_a_sequence_absent_from_vt` test round-trips every one of
//! them through `lib/vt` to prove the database invents nothing.
//!
//! # Fail closed
//!
//! [`from_term`] parses an untrusted `TERM` string. An unknown or empty value
//! degrades to the safe [`TermType::Dumb`] baseline
//! rather than guessing — and it never reads a file derived from `TERM`. There
//! is no `unwrap` / `expect` / `panic!` in this crate, and nothing here ever
//! writes to fd 3 (`stdinfo` is reserved).
//!
//! ```
//! use tairix_termcap::{from_term, ColorDepth, TermType};
//!
//! assert_eq!(from_term("xterm-256color"), TermType::Xterm256Color);
//! assert_eq!(from_term("no-such-terminal"), TermType::Dumb);
//!
//! let caps = TermType::Xterm256Color.capabilities();
//! assert_eq!(caps.color, ColorDepth::Indexed256);
//! assert!(caps.alt_screen);
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod capabilities;
pub mod term_type;

#[cfg(test)]
mod tests;

pub use capabilities::{
    ArrowKeys, Capabilities, ColorDepth, KeyInput, MouseReporting, MouseSupport,
};
pub use term_type::{from_term, TermType};
