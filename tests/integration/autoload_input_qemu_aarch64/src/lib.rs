//! The autoload desktop vertical's **interaction contract**: the shared
//! constants — the readiness markers each stage of the scripted
//! click-through waits on, and the few remaining AW3 window-event
//! delivery counts — that the freestanding test kernel's PASS gate
//! (`src/main.rs`) and the host-side runner's screendump keys and step
//! gates (`tools/xtask` `qemu_tests`) both import, so the script and its
//! observers can never drift (`plans/APPWIN.md` AW3 + AW4).
//!
//! The terminal + pty `Ctrl-C` recovery stage sequences on **guest-emitted
//! readiness markers and uniquely-attributable witnesses** (a loaded
//! bundle's own name), never on cumulative `MessageDelivered` counts: the
//! FONT-SERVICE cadence change shifted those counts so the old absolute
//! thresholds fired during the terminal stage and stalled the run
//! (`plans/OPEN-DEFECTS.md` D20). Only the AW3 desktop stage still counts a
//! handful of early deliveries. The file-manager stages (FM9/FM10/FM11) are
//! deliberately not driven here — they are host-tested in `lib/browse`; this
//! vertical proves driver autoload, unlock, display bind, and the
//! keyboard → session → terminal → pty → shell round trip.
//!
//! The desktop session's window engine is the only port sender in this
//! image, so each `MessageDelivered` is exactly one delivered
//! `WindowEvent`:
//!
//! 1. Clicking the served files window delivers `Focus { focused: true }`
//!    (the window was unfocused) …
//! 2. … then the activating `Pressed` — the served window demonstrably
//!    exists on the composited desktop, keying the second screendump.
//! 3. The start menu's appearance-toggle row is clicked, then the (still
//!    focused) window is clicked once more (delivery 3) …
//! 4. … and a handshake click (delivery 4) — injected only after delivery 3
//!    appeared on serial — is the gate the terminal stage waits on (the
//!    runner holds its menu clicks behind it). The toggle exercises the
//!    taskbar menu + appearance-row hit-test; the theme-toggle *pixels* are
//!    not asserted here (that WM behaviour is host-tested).
//! 5. The terminal stage (`plans/APPWIN.md` AW4): the start menu's
//!    `Terminal` row spawns the terminal bundle, and clicking its served
//!    window delivers `Focus { focused: false }` to the files window
//!    (delivery 5), `Focus { focused: true }` to the terminal (6), and
//!    the activating `Pressed` (7) — the gate after which the runner
//!    types the shell command.
//! 6. Once the terminal is focused, the runner types [`TERMINAL_COMMAND`]
//!    (`sleep 3600` + Enter); the terminal writes the line to the shell,
//!    which resolves and loads the store bundle
//!    [`TERMINAL_ROUND_TRIP_BUNDLE`] (`sleep.app`). That load — attributed
//!    by the bundle's own name — is the AW4 round-trip witness; on it the
//!    guest emits [`CTRL_C_ARM_MARKER`].
//! 7. The pty job-control (`Ctrl-C`) witness (`plans/PTY.md`): the
//!    [`CTRL_C_ARM_MARKER`] gates the runner's [`TERMINAL_CTRL_C_RECOVERY`]
//!    step — a `Ctrl-C` (which the terminal encodes as the `0x03` interrupt
//!    byte) whose cooked-mode line discipline signals the foreground `sleep`
//!    dead, then `true` + Enter. Because the shell is parked in `wait` on
//!    `sleep` until it dies, loading [`CTRL_C_RECOVERY_BUNDLE`] (`true.app`)
//!    is reachable **only** if `Ctrl-C` interrupted `sleep`: keyboard →
//!    session → terminal → pty cooked `^C` → foreground signal → job death →
//!    shell recovery, every hop kernel-attested — the vertical's sixth and
//!    final witness. A failed interrupt leaves `sleep` blocking past the run
//!    budget, so the witness never latches and the run times out (fail
//!    loud).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Deliveries after the first in-window click completed (`Focus` +
/// `Pressed`): the second screendump's marker-occurrence key.
pub const WINDOW_DUMP_DELIVERIES: u32 = 2;

/// The window-event delivery count reached once the appearance-toggle burst
/// has run: the post-toggle in-window click (delivery 3) then the handshake
/// click (delivery 4). The terminal-stage menu clicks gate on this count, so
/// they fire only after the appearance toggle was exercised. (The former
/// light-theme screendump keyed on the same count was dropped — the
/// theme-toggle pixels are host-tested, not asserted in this vertical.)
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

/// Guest marker: the terminal window first becomes the focused key
/// recipient (first app-ward delivery to the second distinct window port;
/// files is first, terminal second). [`TERMINAL_COMMAND`] typing gates on
/// this, not a delivery count, since only a delivery to the terminal's own
/// port proves focus routed to it. Mirrors the [`CTRL_C_ARM_MARKER`]
/// readiness handshake; test-only.
pub const TERMINAL_FOCUSED_MARKER: &str = "AUTOLOAD terminal focused";

/// The command the runner types into the focused terminal: a real store
/// bundle the shell resolves and spawns as a **blocking foreground job**.
/// `sleep 3600` parks off-CPU far past the run budget, so its spawn is the
/// round-trip witness *and* it stays alive as the foreground job the
/// [`TERMINAL_CTRL_C_RECOVERY`] step then interrupts — proving the pty
/// cooked-mode `Ctrl-C` line discipline end to end (`plans/PTY.md`).
pub const TERMINAL_COMMAND: &str = "sleep 3600\n";

/// The on-disk bundle path the shell loads for [`TERMINAL_COMMAND`]. The
/// guest attributes the AW4 round-trip witness to *this exact bundle load*
/// (the `bundle` field of the `appmgr` load record), not to a fragile
/// cumulative delivery count: `sleep` is loaded only by the shell running
/// the typed command, so the witness is uniquely and unambiguously that
/// event. Its load is also the readiness point at which the guest emits
/// [`CTRL_C_ARM_MARKER`].
pub const TERMINAL_ROUND_TRIP_BUNDLE: &str = "/System/Apps/sleep.app";

/// The on-disk bundle path the shell loads for the recovered `true` of
/// [`TERMINAL_CTRL_C_RECOVERY`]. The guest attributes the pty `Ctrl-C`
/// job-control witness to *this exact bundle load*: the shell is parked in
/// `wait` on `sleep` and can load and run `true` only once `Ctrl-C`
/// interrupted `sleep`, so `true`'s load is the end-to-end job-control
/// witness (the last of the vertical's six PASS witnesses).
pub const CTRL_C_RECOVERY_BUNDLE: &str = "/System/Apps/true.app";

/// The follow-on the runner types once the `sleep` spawn has latched (the
/// [`CTRL_C_ARM_MARKER`] gate): a `Ctrl-C` (the `\u{3}` ETX byte the
/// runner injects as the QEMU `ctrl-c` chord, which the terminal encodes
/// through the shared `lib/keymap` rule as the `0x03` interrupt byte) then
/// `true` + Enter. The `Ctrl-C` drives the pty cooked-mode line discipline
/// to signal the foreground `sleep` dead; the shell, unblocked from its
/// `wait`, then reads and spawns `true` — whose bundle load
/// ([`CTRL_C_RECOVERY_BUNDLE`]) is the guest's job-control witness.
pub const TERMINAL_CTRL_C_RECOVERY: &str = "\u{3}true\n";

/// The deterministic serial marker the guest kernel emits once it has
/// witnessed the foreground `sleep` spawn (the [`TERMINAL_COMMAND`] round
/// trip), so the host runner injects [`TERMINAL_CTRL_C_RECOVERY`] in a
/// later wake — strictly after `sleep` is the live, parked foreground job.
/// A guest-observed readiness point the script cannot otherwise time.
pub const CTRL_C_ARM_MARKER: &str = "PTY ctrl-c armed";
