//! The desktop pinned-shortcut store engine.
//!
//! TAIRiX keeps the user's ordered list of pinned taskbar shortcuts in a
//! text document on the volume: the per-user store at [`user_pins_path`].
//! This crate is the **single definition** of that document: the ordered
//! pin list ([`PinList`]), its on-disk line grammar, the closed key registry
//! ([`PinKey`]), the bounded fail-closed parser, the canonical render, and
//! the pin/unpin/reorder operations every producer and consumer shares. The
//! taskbar that draws the pins and the session settings that edit them both
//! go through this engine, so a writer and a reader can never disagree about
//! what the shortcut list says.
//!
//! # Grammar
//!
//! The text is a sequence of lines in display order. A `#` begins a comment
//! that runs to the end of the line; blank and comment-only lines carry no
//! setting. Every other line is one pin: a key drawn from the closed
//! [`PinKey`] registry (`entry` or `bundle`), whitespace, and the target's
//! identifier or path.
//!
//! # Security
//!
//! A pin store is **untrusted input** to every consumer: the parser is
//! bounded ([`MAX_PINS_LEN`], [`MAX_PINS`]), validates every target through
//! the model's own validators, and fails closed ([`PinsError`]) on anything
//! it does not fully understand — an unknown key, an over-long line, a
//! duplicate pin, or an over-long document. A reader that cannot fully
//! parse a store runs on the empty list ([`PinList::default`]) rather than
//! guessing at a partial intent, and a writer refuses the edit outright.
//!
//! The engine performs no I/O and holds no authority: reading and writing
//! the document goes through the secured VFS under the caller's own
//! kernel-attested identity. Pins are per-user state only; there is no
//! machine-wide store.
//!
//! # Bounds
//!
//! The store enforces fixed security and format bounds, not growable
//! capacities: a taskbar strip is a bounded surface, so the list is capped
//! at [`MAX_PINS`] entries.

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;

pub mod pin;
pub mod store;

pub use pin::{PinError, PinList, PinTarget};
pub use store::{parse, render, PinKey, PinsError, MAX_PINS, MAX_PINS_LEN};
pub use tairix_proglib::{BundlePath, EntryError, EntryId};

/// The pin store's own directory name under a `Settings/` tree, spelled once
/// so every path here derives from it.
macro_rules! pins_component {
    () => {
        "Taskbar"
    };
}

/// The pin store's directory name inside a `Settings/` tree — the one
/// component a settings browser creates — shared with [`user_pins_path`]
/// so the spellings cannot drift.
pub const PINS_SETTINGS_SUBDIR: &str = pins_component!();

/// The pin store's document file name.
pub const PINS_FILE: &str = "pins.conf";

/// The per-user pin store path inside `home`, the account's home directory
/// exactly as the session inherited it (`HOME`). A trailing `/` is
/// normalised away; an empty or root home yields `None` rather than a
/// guessed rootward path, so a caller with no home fails closed.
#[must_use]
pub fn user_pins_path(home: &str) -> Option<String> {
    let home = home.strip_suffix('/').unwrap_or(home);
    if home.is_empty() || home == "/" {
        return None;
    }
    Some(format!(
        "{home}/Settings/{PINS_SETTINGS_SUBDIR}/{PINS_FILE}"
    ))
}
