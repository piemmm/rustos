//! TAIRiX font service (`fontd`) — `plans/FONT-SERVICE.md` FS-3.
//!
//! Text rendering is a single OS resource, served from one sandbox holding
//! almost no authority, so a malformed face can fault nothing but this
//! process. `fontd` is the only process that holds a font face or runs the
//! outline rasteriser: it discovers `/System/Fonts` at startup — one
//! subdirectory per family, each carrying a `FontFamily` manifest — binds the
//! reserved [`FONT_ENDPOINT`](tairix_abi::font_ipc::FONT_ENDPOINT), and
//! answers each [`FontRequest`](tairix_abi::font_ipc::FontRequest) with the
//! 8-bit glyph coverage the client blits — never a font byte. The installed
//! binary lives at `/System/Services/fontd.app/Run`.
//!
//! # What this crate is
//!
//! This crate is the **rasterising dispatcher** ([`FontService`]): it owns the
//! discovered families and their lazily-loaded faces, and a byte-budgeted
//! `(requesting family, resolved family, face, glyph, pixel height, weight)`
//! coverage cache, turning one decoded request into a framed reply. It holds
//! no ambient authority and adds no capability — drawing text is not a
//! security boundary; the *reply path* nonetheless validates every field and
//! fails closed on a corrupt frame. The privileged thing the service holds is
//! the `/System/Fonts` reads and the reserved-endpoint bind, both declared in
//! its manifest.
//!
//! # Discovery, not a hardcoded list
//!
//! The store is a curated OS set of family directories, discovered at
//! startup rather than named in this crate: [`discovery::discover`] lists
//! `/System/Fonts`, reads each subdirectory's `FontFamily` manifest
//! ([`tairix_fontface::FamilyManifest`]), and skips — with a logged
//! warning, never fatally on its own — a directory that carries no readable
//! or valid manifest. A store with not one usable family is a fatal startup
//! error: the service cannot serve text at all. A face's bytes are read on
//! first use, not at startup, so a session that never draws a script never
//! pays for the (possibly large) face that covers it.
//!
//! # What a caller can make this service hold
//!
//! The requested pixel height is caller-supplied, so what a client can make
//! the service *retain* is caller-influenced. The cache is therefore bounded
//! in bytes by a budget derived from the machine's RAM — through the same
//! shared reclaimable-memory model, and the same single cached-glyph
//! declaration in [`tairix_font::glyph_cache`], that the render-path client on
//! the other side of the endpoint uses — so a client walking the permitted
//! size range evicts old rasters instead of growing the service without
//! bound. [`glyph_cache`] builds one; the `Run` binary supplies the RAM
//! figure, the process pressure gauge, and the audit sink.
//!
//! The discovery and resolution logic is exhaustively host-testable against
//! an in-memory store fixture holding the committed repository faces,
//! without any on-disk `/System/Fonts` and without the multi-megabyte CJK
//! companion faces the real store ships.
//!
//! # Module map
//!
//! * [`events`] — stable [`tairix_log::EventId`] constants (`17000` range).
//! * `embolden` — the synthetic-weight coverage transform a face with no
//!   `wght` axis is thickened with when a client asks for a heavier weight.
//! * [`discovery`] — the [`discovery::FontStore`]/[`discovery::FaceLoad`]
//!   seam and [`discovery::discover`], which turn a store into a
//!   [`FontService`].
//! * [`service`] — the [`FontService`] rasteriser, its byte-budgeted
//!   [`GlyphCache`], and the [`FontService::handle`] request pipeline.
//!
//! # Layering
//!
//! The crate is `no_std` and depends only on the audited `lib/*` crates
//! `tairix-abi`, `tairix-fontface`, `tairix-log`, `tairix-reclaim`, and
//! `tairix-font`'s shared cached-glyph declaration, so a userland service
//! never links a kernel or driver crate.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod discovery;
mod embolden;
pub mod events;
pub mod service;

pub use service::{glyph_cache, FontService, GlyphCache, GlyphKey};
