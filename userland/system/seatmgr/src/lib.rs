//! RustOS seat-manager service (`seatmgr`) — `plans/DISPLAY.md` D3.
//!
//! The seat manager is the sole holder of `CAP_SEAT_ADMIN`, the
//! seat-multiplexing authority: the `chvt`/`logind`-class power to switch
//! which session is foreground across every seat and to forcibly revoke a
//! wedged owner's lease. It binds the reserved
//! [`rustos_abi::seat::SEATMGR_ENDPOINT`] rendezvous and serves the typed
//! [`rustos_abi::seat::SeatAdminRequest`] operations over the kernel's
//! `seat_switch` / `seat_revoke` syscalls; the installed binary lives at
//! `/System/Services/seatmgr.app/Run`. It is headless-safe: nothing here
//! depends on a graphical session.
//!
//! # What this crate is
//!
//! This crate is the **dispatcher**: the policy layer that turns a raw
//! request buffer into a typed, capability-checked, audited seat operation.
//! It owns exactly three responsibilities and no state of its own:
//!
//! 1. Decode the [`SeatAdminRequest`](rustos_abi::seat::SeatAdminRequest),
//!    failing closed on any malformed input.
//! 2. Require the requester's kernel-attested `Origin` to carry
//!    `CAP_SEAT_ADMIN` **before** touching any state — the broker never
//!    launders its own authority onto an unprivileged caller, and the
//!    kernel re-checks the capability and every index when the syscall is
//!    issued, so the service adds policy without widening reach.
//! 3. Emit a [`rustos_log`] audit record for every applied operation,
//!    every refusal, and every malformed request.
//!
//! The kernel is reached through the [`SeatAdmin`] seam so the
//! security-relevant dispatch code is exhaustively testable with an
//! in-memory fixture.
//!
//! # Module map
//!
//! * [`events`] — stable [`rustos_log::EventId`] constants (`14000` range).
//! * [`service`] — the [`serve`] entry point, its pipeline, and the
//!   [`SeatAdmin`] kernel seam.
//!
//! # Layering
//!
//! The crate is `no_std` and depends only on the audited `lib/*` crates
//! `rustos-abi` and `rustos-log`, so a userland service never links a
//! kernel or driver crate.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod events;
pub mod service;

pub use service::{serve, SeatAdmin};
