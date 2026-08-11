//! The window-channel protocol engine (`plans/APPWIN.md` AW2).
//!
//! One crate hosts **both halves** of the `WINDOW_ENDPOINT` protocol
//! over injected seams, so the semantics — what a request means, what is
//! validated, what is refused — have exactly one definition:
//!
//! * [`server::WindowServer`] — the engine the desktop session composes:
//!   decode → caller attestation ([`server::CallerIdentity`]) →
//!   owner/bounds validation → the [`server::WindowHost`] compositor
//!   bridge, plus [`deliver_event`](server::WindowServer::deliver_event)
//!   for the session's app-ward input routing.
//! * [`client::WindowClient`] / [`client::WindowEvents`] — the app-side
//!   half over a [`client::WindowTransport`] and a parked
//!   [`client::EventSource`], so an app creates, presents, closes, and
//!   waits for events without ever polling, plus
//!   [`pointer_input_events`] and [`key_input_event`] — the one
//!   translation from a delivered wire pointer or key event into the input
//!   vocabulary the shared controls consume, so no app carries a private
//!   copy of it.
//!
//! The wire format itself lives in `tairix_abi::window_ipc`; this crate
//! adds the behaviour. Window frames travel through one `shm_grant`ed
//! region mapped once at create time — presents carry a frame index and
//! a damage rectangle, never pixels — and every window is keyed to the
//! kernel-attested `ProcId` of the task that created it, so one app can
//! never touch another's window.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod desktop;
pub mod server;

pub use client::{
    damage_in, event_endpoint_for, key_input_event, pointer_input_events, present_damage,
    EventSource, Repaint, WindowClient, WindowEvents, WindowTransport, EVENT_MAILBOX_CAPACITY,
};
pub use desktop::Desktop;
pub use server::{
    CallerIdentity, EventSink, PinDecision, PopupSpec, WindowHost, WindowServer, WindowSizing,
    WINDOWS_PER_CLIENT_MAX, WINDOW_REPLY_MAX,
};

#[cfg(test)]
mod tests;
