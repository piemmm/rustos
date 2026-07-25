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

// --- FM9-b: open a file into the viewer via the trusted picker + CU6
// one-shot delegation (`plans/NEW-FILEMANAGER.md` FM9-b), appended after
// FM9-a. The Viewer is launched from the start menu with no document, so it
// asks the session's trusted picker (`plans/APPWIN.md` AW5); the picker opens
// at the user's home (`/Users/root`, where the fixture plants a document),
// the user clicks the document row, and the session `fd_grant`s the chosen
// file to the viewer, which `fd_redeem`s and reads it.
//
// The start-menu button and the Viewer row are the taskbar's own session-
// internal surfaces (not app-served windows), and the trusted picker is a
// session-owned compositor window — none of them deliver app-ward window
// events, so this stage's clicks do not advance the [`MessageDelivered`]
// counter. They are therefore gated on the FM9-a folder-dump delivery count
// (the last app-ward delivery) and, for the pick itself, on a serial marker
// that appears only once the picker is composited and ready.

/// Gate for the start-menu-open + Viewer-launch clicks: the FM9-a folder
/// dump's delivery count (the last app-ward delivery). These clicks are
/// session-internal (taskbar), so they add no deliveries; the runner also
/// holds them behind the pending folder dump, so the Viewer is launched only
/// once FM9-a is provably complete.
pub const FM9B_VIEWER_LAUNCH_DELIVERIES: u32 = FM9_FOLDER_DUMP_DELIVERIES;

/// Serial marker the freestanding test kernel prints once it observes the
/// desktop session read a directory *after* the FM9-a rename — the trusted
/// picker's `open_at` listing of the user's home, done synchronously inside
/// the `PickFile` serve, so the picker window is composited in that same wake.
///
/// There is no kernel-audited event at picker-open (the picker is a
/// session-owned compositor surface, and the user-authority session cannot
/// emit to the diagnostic log), and no app-ward `MessageDelivered` fires. But
/// the session's `read_dir_all` for the picker's home listing is a
/// `SyscallInvoked` (`sc=fs_open`) attributed to `comm=desktop`, and after
/// FM9-a the session performs no other `fs_open` before this one — so the
/// test's audit sink turns that unique event into this deterministic marker.
/// The runner gates its pick-click on it, so the click is processed in a
/// *later* wake, strictly after the picker is open (the FM9-a "later wake"
/// discipline). This is a test-only observation (`src/main.rs`), never a
/// production log line.
pub const FM9B_PICKER_OPEN_MARKER: &str = "FM9B trusted picker open";

/// The session process's `comm` (its bundle leaf `desktop.app` → `desktop`),
/// the `SyscallInvoked` `comm` field value the test sink matches to attribute
/// the picker directory-read to the session and nothing else.
pub const SESSION_COMM: &str = "desktop";

// --- FM9-c: delete with confirm through the right-click context menu
// (`plans/NEW-FILEMANAGER.md` FM9-c), appended after FM9-b. The files window
// still shows `/Users/root`, which now holds the FM9-a folder and the planted
// document; a secondary-button press on the folder row raises+focuses the
// files window (over the frontmost Viewer) and opens the context menu on it, a
// primary click on the drawn **Delete** row opens the confirmation dialog, and
// a primary click on the dialog's Delete button hands the removal to the app's
// operation runner — a real permission-checked `rmdir` under the user's own
// identity (the guest PASS's `FsNodeMutated op=rmdir` witness).
//
// The whole burst is gated on [`FM9C_DELETE_GATE_MARKER`] (the Viewer's
// `fd_redeem`, the last FM9-b serial event), so it runs strictly after the CU6
// delegation. It does not use the app-ward delivery counter: the Viewer window
// that FM9-b launches delivers its own focus event(s), so the counter's value
// after FM9-b is not statically known — the `fd_redeem` serial marker is the
// robust ordering point. Within the burst the guest applies the queued pointer
// events strictly in order and each overlay (menu, then dialog) is handled
// synchronously on its press, so no finer gate is needed.

/// Serial marker the whole FM9-c delete click-through is gated on: the
/// `SyscallInvoked` (`sc=fd_redeem`) trace line the Viewer emits when it
/// redeems the FM9-b one-shot delegation — the last serial event of FM9-b, and
/// (per the FM9-b witness) the only `fd_redeem` in the image, so its first
/// occurrence sequences FM9-c strictly after the delegation. Rendered by the
/// same `sc=<name>` syscall trace the input-arming and frame-map gates key on.
pub const FM9C_DELETE_GATE_MARKER: &str = "sc=fd_redeem";
