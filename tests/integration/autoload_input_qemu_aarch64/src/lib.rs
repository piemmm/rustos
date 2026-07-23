//! The autoload desktop vertical's **interaction contract**: how many
//! app-ward window-event deliveries (the kernel/ipc `MessageDelivered`
//! audit records) each stage of the scripted click-through produces —
//! the one definition the freestanding test kernel's PASS gate
//! (`src/main.rs`) and the host-side runner's screendump keys and step
//! gates (`tools/xtask` `qemu_tests`) both import, so the script and its
//! observers can never drift (`plans/APPWIN.md` AW3 + AW4).
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
//!    strictly after the wake that presented the light frame …
//! 4. … and the handshake click (injected only after delivery 3
//!    appeared on serial, so the guest processes it in a *later* wake,
//!    strictly after the light-theme frame reached the scan-out) keys
//!    the third screendump.
//! 5. The terminal stage (`plans/APPWIN.md` AW4): the start menu's
//!    `Terminal` row spawns the terminal bundle, and clicking its served
//!    window delivers `Focus { focused: false }` to the files window
//!    (delivery 5), `Focus { focused: true }` to the terminal (6), and
//!    the activating `Pressed` (7) — the gate after which the runner
//!    types the shell command.
//! 6. The typed [`TERMINAL_COMMAND`] (five characters, Enter last) is
//!    deliveries 8–17: the session delivers both edges of every key to
//!    the focused terminal, one `Key` event per edge. The Enter *press*
//!    — the edge that makes the terminal write the completed line to
//!    the shell — is delivery 16.

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
/// marker-occurrence key, and the gate behind which the terminal stage's
/// menu clicks queue (they additionally wait for that dump to verify —
/// the runner holds pointer steps behind pending dumps).
pub const APPEARANCE_DUMP_DELIVERIES: u32 = 4;

/// Shared-frame **map** operations (`sc=shm_map`) that have occurred by
/// the time the terminal window exists and can be clicked — the robust,
/// present-count-independent gate for the terminal-window click.
///
/// A window's shared frame region is mapped **exactly once, when the
/// window is created** (`WindowServer::create` → the session's
/// `ShmMapper`); a *present* re-uses that mapping and maps nothing. So the
/// count of `shm_map` operations tracks window **creation**, never the
/// (timing-variable) number of repaints — which is why gating on it is
/// immune to the flaky-repaint race that a `CallReplied`-count gate
/// suffered (a files-window click that happened to repaint would inflate
/// the reply count and fire the terminal click before the terminal
/// existed).
///
/// Exactly three frame maps precede the terminal-window click, in order:
/// 1. the framebuffer display service maps the desktop's granted scan-out
///    frame region (boot);
/// 2. the session maps the **files** window's frame region (its create);
/// 3. the session maps the **terminal** window's frame region (its
///    create) — the occurrence this gate keys on.
///
/// After occurrence 3 the terminal window demonstrably exists in the
/// compositor at its cascade slot, so the click focuses it. Neither the
/// files window's repaints nor the terminal's own later presents (the
/// shell prompt/output) add `shm_map` operations, so the gate can never
/// race them.
pub const TERMINAL_WINDOW_FRAME_MAPS: u32 = 3;

/// Deliveries after the terminal-window click completed (the files
/// window's unfocus, the terminal's focus, and the activating press):
/// the gate after which the runner types [`TERMINAL_COMMAND`] at the
/// seat keyboard — the terminal is provably the focused key recipient.
pub const TERMINAL_TYPE_DELIVERIES: u32 = 7;

/// The command the runner types into the focused terminal: a real store
/// bundle the shell resolves and spawns. `true` is the smallest such
/// program — it starts, does nothing, and exits `0` — so the spawn is
/// the whole observable effect.
pub const TERMINAL_COMMAND: &str = "true\n";

/// Deliveries at (or beyond) which a kernel `ProcessSpawned` audit
/// record witnesses the shell round trip — the guest PASS gate. The
/// Enter **press** is delivery 16 (7 from [`TERMINAL_TYPE_DELIVERIES`]
/// plus both edges of `t`/`r`/`u`/`e` and the Enter press), and only
/// that press makes the terminal write the completed command line to
/// the shell's pipe. Every other spawn in the image (services, login,
/// the session, the two menu-launched apps, the terminal's own shell)
/// happens strictly before the typing gate, so a spawn observed with
/// the delivery counter at 16 or more can only be the shell executing
/// the typed command: keyboard → session → terminal → pipe → shell →
/// `spawn`, every hop kernel-attested.
pub const TERMINAL_ROUND_TRIP_DELIVERIES: u32 = 16;
