//! The icon-bar vertical's shared interaction contract
//! (`plans/NEW-TASKBAR.md`).
//!
//! The freestanding guest kernel (`src/main.rs`) and the host runner's
//! enrolment (`tools/xtask/src/commands/qemu_tests.rs`) both read these
//! definitions, so the gesture the host injects and the witness the guest
//! latches can never drift apart.
//!
//! Every stage is sequenced on a **uniquely attributable** witness — the
//! exact bundle a load names, a reply only one request shape produces, an
//! announcement only the desktop makes — never on a cumulative event count
//! that an unrelated subsystem's cadence can shift under it
//! (`plans/OPEN-DEFECTS.md` D19/D20).
//!
//! # Who states what
//!
//! The two sides observe different streams, and each gates on the one that
//! can answer its question honestly:
//!
//! - The **host** reads the serial transcript, so it gates on the *desktop
//!   session's* own announcements. Only the session knows when a served
//!   window's pixels reached the display, so
//!   `tairix_desktop_session::WINDOW_SHOWN_MESSAGE` is what says "there is
//!   now something on screen worth reading" — a screendump taken on anything
//!   earlier is a race against the frame.
//! - The **guest** kernel's audit sink sees kernel audit records only, so it
//!   gates on those: the bundle a load names, and the window channel's
//!   create replies, which are the one reply shape on that endpoint with a
//!   distinctive wire length.
//!
//! Neither side infers the other's facts. In particular the guest does not
//! try to recognise a *present* in the audit trail: on the window channel a
//! present, a backdrop-blur change, a retitle and an icon-bar declaration all
//! answer with the same four-byte status reply, so "the reply after the
//! create is the first present" is not a fact — it is a guess about how many
//! requests an application happens to make, and a shared rendezvous makes it
//! a guess about the other clients too.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Bare name of the application whose icon-bar slot the vertical drives —
/// the bundle is `<system application store>/<name>.app`, composed from the
/// shared `lib/abi` spellings on both sides rather than written out here.
///
/// The terminal, because it is the application whose declaration has rows
/// with a *visible consequence a machine can attest*: both its *New window*
/// row and its primary-click default action make it open another window in
/// the **same** process, which no other gesture in this vertical can
/// produce.
pub const BAR_APP_NAME: &str = "terminal";

/// Number of windows the application has opened once the vertical's whole
/// script has been delivered: the one it opens on launch, the one the chosen
/// *New window* menu row opens, and the one the slot's primary click opens
/// through the declared default action.
///
/// Each is a create reply on the reserved window endpoint, and in this world
/// the launched application is the only client that creates a window — the
/// desktop's own surfaces (the bar, the launcher popup, the menu, the hover
/// picker, the desktop icons) are session-painted compositor windows that
/// never call the window channel. So the count is attributable act by act,
/// and reaching it is the guest's PASS.
pub const WINDOWS_OPENED: u32 = 3;
