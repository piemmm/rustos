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

// --- FM9-a: the file-manager New-Folder + inline-rename stage
// (`plans/NEW-FILEMANAGER.md` FM9-a), appended after the AW4 terminal
// round trip. The same window-event delivery counter is the sequencing
// clock: after the terminal command's five keys (both edges) the counter
// stands at [`FM9_TYPING_DONE_DELIVERIES`], and each appended input action
// advances it deterministically (a focus-changing click delivers three
// events — the old window's unfocus, the new window's focus, and the
// press — a click on the already-focused window delivers one press, and a
// typed key delivers both of its edges). The guest applies the injected
// events strictly in order, so these counts are exact, not racy.

/// The delivery counter once the terminal command has been fully typed:
/// [`TERMINAL_TYPE_DELIVERIES`] (7) plus both edges of the five
/// [`TERMINAL_COMMAND`] keys (`t r u e` + Enter = 10) — the base the
/// file-manager stage's gates are measured from.
pub const FM9_TYPING_DONE_DELIVERIES: u32 = 17;

/// Gate for the click that refocuses the files window **and** selects the
/// **`Users`** row in one action — the first file-manager click, fired once
/// the terminal command is fully typed. It lands on the `Users` row at a
/// left-biased column that is clear of the overlapping (raised) terminal
/// window, so the desktop routes it to the still-background files window:
/// it raises files to the front, and delivers three events (terminal
/// unfocus, files focus, files press) — the press selecting the row. Every
/// later file-manager click lands on the now-frontmost files window.
pub const FM9_USERS_CLICK_DELIVERIES: u32 = FM9_TYPING_DONE_DELIVERIES;

/// Gate for the first **`Enter`** (descend into `/Users`): after the
/// `Users`-row click's three deliveries. `Enter` delivers both its edges;
/// the press activates the selected directory.
pub const FM9_DESCEND_USERS_DELIVERIES: u32 = FM9_USERS_CLICK_DELIVERIES + 3;

/// Gate for the click that selects the **`root`** row in `/Users`: after
/// the first `Enter`'s two edge deliveries.
pub const FM9_ROOT_CLICK_DELIVERIES: u32 = FM9_DESCEND_USERS_DELIVERIES + 2;

/// Gate for the second **`Enter`** (descend into `/Users/root`): after the
/// `root`-row click's one delivery.
pub const FM9_DESCEND_ROOT_DELIVERIES: u32 = FM9_ROOT_CLICK_DELIVERIES + 1;

/// Gate for the **New Folder** toolbar click: after the second `Enter`'s
/// two edge deliveries. The click creates `New Folder` in `/Users/root`
/// (`fs_mkdir`) and opens the inline rename on it.
pub const FM9_NEW_FOLDER_DELIVERIES: u32 = FM9_DESCEND_ROOT_DELIVERIES + 2;

/// Gate for typing the rename **suffix + Enter**: after the New Folder
/// click's one delivery. The inline editor is pre-filled with the
/// placeholder name and focused with the caret at the end, so the typed
/// characters append to make a distinct name, and `Enter` commits it
/// (`fs_rename`).
pub const FM9_RENAME_DELIVERIES: u32 = FM9_NEW_FOLDER_DELIVERIES + 1;

/// The text the runner appends to the placeholder folder name before
/// committing the rename: printable letters plus the committing `Enter`.
/// It must be non-empty so the committed name differs from the placeholder
/// (a rename to the same name is a no-op that performs no `fs_rename` and
/// emits no audit record), and every character must be seat-keyboard
/// typable (lowercase letters qualify).
pub const FM9_RENAME_SUFFIX: &str = "x\n";

/// Gate for the file-manager "named folder" screendump: after the rename
/// suffix + Enter has been fully typed. [`FM9_RENAME_SUFFIX`] is two keys
/// (`x` then Enter), each delivering both edges, so the counter advances by
/// four from [`FM9_RENAME_DELIVERIES`]. By then the inline editor has
/// committed and closed and the named folder is on screen.
pub const FM9_FOLDER_DUMP_DELIVERIES: u32 = FM9_RENAME_DELIVERIES + 4;
