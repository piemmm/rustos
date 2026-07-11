//! The autoload desktop vertical's **interaction contract**: how many
//! app-ward window-event deliveries (the kernel/ipc `MessageDelivered`
//! audit records) each stage of the scripted click-through produces —
//! the one definition the freestanding test kernel's PASS gate
//! (`src/main.rs`) and the host-side runner's screendump keys
//! (`tools/xtask` `qemu_tests`) both import, so the script and its
//! observers can never drift (`plans/APPWIN.md` AW3).
//!
//! The desktop session's window engine is the only port sender in this
//! image, so each `MessageDelivered` is exactly one delivered
//! `WindowEvent`:
//!
//! 1. Clicking the served files window delivers `Focus { focused: true }`
//!    (the window was unfocused) …
//! 2. … then the activating `Pressed` — the served window demonstrably
//!    exists on the composited desktop, keying the second screendump.
//! 3. After the start menu's appearance toggle re-themed the desktop,
//!    clicking the (still focused) window delivers one more `Pressed`,
//!    strictly after the wake that presented the light frame — keying
//!    the third screendump and the guest PASS gate.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Deliveries after the first in-window click completed (`Focus` +
/// `Pressed`): the second screendump's marker-occurrence key.
pub const WINDOW_DUMP_DELIVERIES: u32 = 2;

/// Deliveries after the first post-toggle handshake click. The
/// post-toggle in-window click (delivery 3) can share its wake with the
/// toggle press itself, and deliveries are emitted *before* that wake's
/// tail present — so a dump keyed on 3 can race the re-themed frame.
/// This handshake click is injected only after delivery 3 appeared on
/// serial, so the guest processes it in a *later* wake, strictly after
/// the light-theme frame reached the scan-out: the third screendump's
/// marker-occurrence key.
pub const APPEARANCE_DUMP_DELIVERIES: u32 = 4;

/// Deliveries after the second handshake click: the guest PASS gate's
/// threshold. The runner holds this click back until the third
/// screendump has been read back and verified, so the guest — which
/// exits the instant its witnesses are complete — can never tear QEMU
/// down under a pending dump. Reaching it requires the entire
/// click-through (menu → launch → served window → theme toggle → dumps)
/// to have happened.
pub const PASS_DELIVERIES: u32 = 5;
