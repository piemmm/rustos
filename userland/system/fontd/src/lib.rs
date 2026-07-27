//! TAIRiX font service (`fontd`) — `plans/FONT-SERVICE.md` FS-3.
//!
//! Text rendering is a single, sandboxed OS resource (`AGENTS.md` §16.4,
//! §19.5). `fontd` is the only process that holds a font face or runs the
//! outline rasteriser: it loads the committed TrueType faces from
//! `/System/Fonts` once at startup, binds the reserved
//! [`FONT_ENDPOINT`](tairix_abi::font_ipc::FONT_ENDPOINT), and answers each
//! [`FontRequest`](tairix_abi::font_ipc::FontRequest) with the 8-bit glyph
//! coverage the client blits — never a font byte. The installed binary lives
//! at `/System/Services/fontd.app/Run`.
//!
//! # What this crate is
//!
//! This crate is the **rasterising dispatcher** ([`FontService`]): it owns the
//! parsed faces and a bounded `(face, glyph, cell height)` coverage cache, and
//! turns one decoded request into a framed reply. It holds no ambient
//! authority and adds no capability — drawing text is not a security boundary
//! (§5.2); the *reply path* nonetheless validates every field and fails closed
//! on a corrupt frame (§5.4). The privileged thing the service holds is the
//! one-shot `/System/Fonts` read at startup and the reserved-endpoint bind,
//! both declared in its manifest.
//!
//! The face bytes are injected (borrowed) rather than embedded, so the
//! security-relevant rasterise + cache logic is exhaustively host-testable
//! against the committed repository faces without any on-disk `/System/Fonts`.
//!
//! # Module map
//!
//! * [`events`] — stable [`tairix_log::EventId`] constants (`17000` range).
//! * [`service`] — the [`FontService`] rasteriser, its bounded cache, and the
//!   [`FontService::handle`] request pipeline.
//!
//! # Layering
//!
//! The crate is `no_std` and depends only on the audited `lib/*` crates
//! `tairix-abi`, `tairix-fontface`, `tairix-vt`, and `tairix-log`, so a
//! userland service never links a kernel or driver crate.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod events;
pub mod service;

pub use service::{FontService, FACE_REPERTOIRES};
