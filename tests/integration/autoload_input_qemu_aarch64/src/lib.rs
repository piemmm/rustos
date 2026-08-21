//! The autoload desktop vertical's **interaction contract**: the shared
//! readiness markers each stage of the scripted click-through waits on,
//! which the freestanding test kernel's PASS gate (`src/main.rs`) and the
//! host-side runner's screendump keys and step gates (`tools/xtask`
//! `qemu_tests`) both import, so the script and its observers can never
//! drift (`plans/APPWIN.md` AW3 + AW4).
//!
//! **Every stage gates on a witness only its own subject can produce** — a
//! guest-emitted readiness marker, a named bundle load, or a window's own
//! frame map — never on a cumulative count of an event the whole system
//! emits, and never on traffic to a rendezvous many clients share. Both
//! rules are paid for: the FONT-SERVICE cadence change shifted the old
//! cumulative `MessageDelivered` thresholds so they fired during the
//! terminal stage and stalled the run (`plans/OPEN-DEFECTS.md` D20), and
//! the desktop-info query the Switchboard issues at start-up later fired an
//! "the files window was created" gate that had been keyed on *any* reply
//! over the shared `WINDOW_ENDPOINT`, clicking empty desktop half a second
//! before that window existed. A gate that another component can satisfy is
//! the defect; a stage waits for its own subject or it does not wait at all.
//!
//! The file-manager stages (FM9/FM10/FM11) are deliberately not driven here
//! — they are host-tested in `lib/browse`; this vertical proves driver
//! autoload, unlock, display bind, and the keyboard → session → terminal →
//! pty → shell round trip.
//!
//! The desktop session's window engine is the only port sender in this
//! image, so each `MessageDelivered` is exactly one delivered
//! `WindowEvent`, and the record's `port` field names the window it went
//! to — which is what makes a per-window delivery ordinal attributable
//! where a system-wide total is not:
//!
//! 1. Clicking the served files window (opened by the session at desktop
//!    reveal) delivers `Focus { focused: true }` (the window was
//!    unfocused) …
//! 2. … then the activating `Pressed`. Both landed on the files window's
//!    own port, so the guest emits [`FILES_WINDOW_ACTIVATED_MARKER`]: the
//!    served window demonstrably exists and is active on the composited
//!    desktop, keying the second screendump.
//! 3. A handshake click on the still-focused window — injected only after
//!    that marker appeared and held while the second dump is pending —
//!    delivers one further `Pressed` to the same port, on which the guest
//!    emits [`FILES_HANDSHAKE_MARKER`]: the wake boundary the terminal
//!    stage waits on (the runner holds its library-popup clicks behind it).
//! 4. The terminal stage (`plans/APPWIN.md` AW4): the taskbar's Library
//!    button opens the program-library popup (a session-owned surface —
//!    no app-ward delivery), its `Terminal` entry spawns the terminal
//!    bundle through the catalog the session merged from the planted
//!    machine store (`plans/NEW-TASKBAR.md` T5), and clicking the
//!    terminal's served window delivers `Focus { focused: false }` to the
//!    files window (delivery 4), `Focus { focused: true }` to the
//!    terminal (5), and the activating `Pressed` (6) — the gate after
//!    which the runner types the shell command.
//! 5. Once the terminal is focused, the runner types [`TERMINAL_COMMAND`]
//!    (`sleep 3600` + Enter); the terminal writes the line to the shell,
//!    which resolves and loads the store bundle
//!    [`TERMINAL_ROUND_TRIP_BUNDLE`] (`sleep.app`). That load — attributed
//!    by the bundle's own name — is the AW4 round-trip witness; on it the
//!    guest emits [`CTRL_C_ARM_MARKER`].
//! 6. The pty job-control (`Ctrl-C`) witness (`plans/PTY.md`): the
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

/// The label the file manager's icon-bar slot carries: the name its own
/// signed `AppInfo` states, which is what the session resolves the slot's
/// identity from.
///
/// The host-side script reconstructs the bar with this slot to compute where
/// to click, so naming it once here is what stops the script and the guest
/// from drifting.
pub const FILES_BAR_APP_NAME: &str = "files";

/// Guest marker: the files window received the `Focus` + `Pressed` pair of
/// the first in-window click — it exists, is active, and the compositor has
/// the frame the second screendump reads.
///
/// Counted **on that window's own port**, not system-wide, so no other
/// app's, service's or session surface's traffic can advance it; test-only.
pub const FILES_WINDOW_ACTIVATED_MARKER: &str = "AUTOLOAD files window activated";

/// Deliveries to the files window's own port that
/// [`FILES_WINDOW_ACTIVATED_MARKER`] reports: the activating click's
/// `Focus` then `Pressed`.
pub const FILES_ACTIVATION_DELIVERIES: u32 = 2;

/// Guest marker: the handshake click's `Pressed` reached the still-focused
/// files window. The terminal stage's library-popup clicks gate on it, so
/// they fire in a wake strictly after the verified second dump's frame.
pub const FILES_HANDSHAKE_MARKER: &str = "AUTOLOAD files handshake delivered";

/// Deliveries to the files window's own port that [`FILES_HANDSHAKE_MARKER`]
/// reports: the activating click's two, then the handshake's `Pressed`.
pub const FILES_HANDSHAKE_DELIVERIES: u32 = 3;

/// Shared-frame **map** operations (`sc=shm_map`) that have occurred by the
/// time the *files* window exists and can be clicked: the boot scan-out map,
/// then that window's own create map.
///
/// This is the gate for the first in-window click, and it is attributable
/// where a reply over the shared `WINDOW_ENDPOINT` is not — every client of
/// that rendezvous replies on it (the Switchboard's start-up desktop query
/// did so half a second before this window was created, which is what once
/// clicked empty desktop), whereas only a window **create** maps a frame.
/// See [`TERMINAL_WINDOW_FRAME_MAPS`] for why a map counts creations and
/// never repaints.
pub const FILES_WINDOW_FRAME_MAPS: u32 = 2;

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
pub const TERMINAL_ROUND_TRIP_BUNDLE: &str = "/System/Commands/sleep.app";

/// The on-disk bundle path the shell loads for the recovered `true` of
/// [`TERMINAL_CTRL_C_RECOVERY`]. The guest attributes the pty `Ctrl-C`
/// job-control witness to *this exact bundle load*: the shell is parked in
/// `wait` on `sleep` and can load and run `true` only once `Ctrl-C`
/// interrupted `sleep`, so `true`'s load is the end-to-end job-control
/// witness (the last of the vertical's six PASS witnesses).
pub const CTRL_C_RECOVERY_BUNDLE: &str = "/System/Commands/true.app";

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
