//! RustOS shared command-help engine (`lib/help`, `plans/APPS.md`).
//!
//! Every application bundle may ship a `Help/` tree (`rustos_abi::appinfo`,
//! `AGENTS.md` §16.5): one structured-Markdown document per command or topic,
//! under one directory per BCP-47 locale plus the mandatory `default/`
//! (en-US) canonical source. Three consumers read that tree — the `man`
//! command, every command's short `-h`/`-?` help, and any graphical help
//! viewer — and they must not each grow a private locale walker, Markdown
//! parser, or escape vocabulary. This crate is the one engine they share.
//!
//! # What it does
//!
//! * **Locate** a document through an injected, capability-scoped read seam
//!   ([`HelpSource`]). The engine performs no ambient I/O: the caller hands
//!   it a reader already scoped to one bundle's `Help/` tree, mirroring how
//!   `appmgr` injects its bundle store.
//! * **Select** the locale by the deterministic fallback chain ([`load`]):
//!   the exact requested tag, then the lexicographically first directory of
//!   the same language that holds the document, then `default/`. The served
//!   locale and how it relates to the request are reported ([`Selection`])
//!   so a caller such as `man` can surface a locale fallback on `stdinfo`.
//!   A document is always rendered whole from a single file — falling back
//!   never mixes languages within a page.
//! * **Parse** the document into the fixed section model ([`HelpDoc`],
//!   [`SectionKind`]): the closed, ordered set of `## NAME` … `## SEE ALSO`
//!   sections whose keys are language-neutral while their prose is
//!   localised. Parsing is total and bounded: hard, fixed security limits on
//!   document size, line length, line count, block/item/table dimensions,
//!   and heading depth, every violation a typed [`HelpError`] — these are
//!   validation bounds, never growable capacities. Help content is signed,
//!   but it is still parsed as hostile input: a malformed document degrades
//!   to a clean error, never a panic and never invented text.
//! * **Render** the two help surfaces as `lib/vt` operations —
//!   [`render_short`] (the `-h`/`-?` view: `NAME`, `SYNOPSIS`, compact
//!   `OPTIONS`) and [`render_full`] (the whole `man` page) — so the escape
//!   vocabulary stays the one `lib/vt` definition and the output inherits
//!   its control-byte discipline.
//!
//! # What it deliberately does not do
//!
//! * No paging, terminal probing, or locale *discovery*: the pager is the
//!   `man` app's concern and the active locale is resolved once by the
//!   session/shell and passed in.
//! * No second path/reference parser and no filesystem access: the seam
//!   receives only spellings this crate has validated ([`Locale`],
//!   [`DocumentName`]), so a hostile name can never traverse outside the
//!   tree it was scoped to.
//!
//! The crate is `no_std` + `alloc`, forbids `unsafe`, and has no
//! `unwrap`/`expect`/`panic!` on any path: every failure is a typed error
//! the caller handles as a value.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod doc;
mod locale;
mod render;

pub use doc::{
    Align, Block, HelpDoc, HelpError, ListItem, Section, SectionKind, Span, Table,
    MAX_BLOCKS_PER_SECTION, MAX_DOC_LEN, MAX_LINES, MAX_LINE_LEN, MAX_LIST_ITEMS,
    MAX_TABLE_COLUMNS, MAX_TABLE_ROWS,
};
pub use locale::{
    load, DocumentName, Fallback, HelpSource, LoadError, Loaded, Locale, NameError, Selection,
    SourceError, TagError, DEFAULT_LOCALE, MAX_DOCUMENT_NAME_LEN, MAX_LOCALE_DIRS, MAX_LOCALE_LEN,
};
pub use render::{render_full, render_short};

#[cfg(test)]
mod tests;
